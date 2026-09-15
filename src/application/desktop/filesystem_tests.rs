//! Actual outer G owns both sibling processes. This is a private fixture, not
//! production State selection or an alternate filesystem authority protocol.
use super::*;
use crate::{
    catalog_session::{BootstrapMode, CatalogFilesystem, LeaseId, PrepareCatalog},
    filesystem_worker::{client::Client, wire::AdmissionState},
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fixture {
    request: PrepareCatalog,
    lost_confirm: bool,
    alias: bool,
    lost_prepare: bool,
}
pub(super) struct HarnessOutput<R> {
    inner: R,
    prefix: Vec<u8>,
    ready: bool,
    offset: usize,
}
impl<R: Read> HarnessOutput<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            prefix: vec![],
            ready: false,
            offset: 0,
        }
    }
}
impl<R: Read> Read for HarnessOutput<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while !self.ready {
            let mut byte = [0];
            if self.inner.read(&mut byte)? == 0 {
                return Ok(0);
            }
            self.prefix.push(byte[0]);
            if self.prefix.ends_with(b"PCDT") {
                self.ready = true;
                self.prefix = b"PCDT".to_vec();
            } else if self.prefix.len() > 512 {
                return Err(std::io::Error::other("test harness prefix limit"));
            }
        }
        if self.offset < self.prefix.len() {
            let n = out.len().min(self.prefix.len() - self.offset);
            out[..n].copy_from_slice(&self.prefix[self.offset..self.offset + n]);
            self.offset += n;
            return Ok(n);
        }
        self.inner.read(out)
    }
}
pub(super) struct Driver {
    pub owner: thread::JoinHandle<anyhow::Result<()>>,
    pub release: mpsc::SyncSender<()>,
}
pub(super) fn start_driver(
    fixture: Option<Fixture>,
    proxy: Arc<filesystem::Proxy>,
    tx: mpsc::SyncSender<Message>,
    _: [u8; 16],
) -> anyhow::Result<Option<Driver>> {
    let Some(f) = fixture else { return Ok(None) };
    let (release, rx) = mpsc::sync_channel(1);
    Ok(Some(Driver {
        release,
        owner: thread::Builder::new()
            .name("managed-overlap-fixture".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if f.lost_prepare {
                        ensure!(
                            proxy
                                .prepare_catalog(&f.request, &AtomicBool::new(false))
                                .is_err(),
                            "lost Prepare unexpectedly replied"
                        );
                        let snapshot = proxy
                            .admission_status(f.request.operation, &f.request.session)?
                            .context("lost Prepare record missing")?;
                        ensure!(
                            snapshot.state == AdmissionState::Prepared,
                            "lost Prepare did not retain pins"
                        );
                        proxy.abandon_prepare(f.request.operation, &f.request.session)?;
                        tx.send(Message::new(
                            Kind::Fixture,
                            0,
                            serde_json::to_vec(&std::result::Result::<String, String>::Ok(
                                "ready_to_drain".into(),
                            ))?,
                        ))?;
                        rx.recv()?;
                        return Ok(());
                    }
                    crate::catalog_session::overlap_tests::run(
                        proxy,
                        f.request,
                        f.lost_confirm,
                        f.alias,
                        || {
                            tx.send(Message::new(
                                Kind::Fixture,
                                0,
                                serde_json::to_vec(&std::result::Result::<String, String>::Ok(
                                    "ready_to_drain".into(),
                                ))?,
                            ))?;
                            rx.recv()?;
                            Ok(())
                        },
                    )
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("SQL fixture panicked")));
                let report = result
                    .as_ref()
                    .map(|_| "eight admitted roles and ninth discovery drained".to_owned())
                    .map_err(|e| e.to_string());
                tx.send(Message::new(Kind::Fixture, 0, serde_json::to_vec(&report)?))?;
                result
            })?,
    }))
}
#[test]
#[ignore = "owned CT SQL fixture entrypoint; not an ordinary behavior test"]
fn catalog_child() {
    std::panic::set_hook(Box::new(|_| {}));
    let result = process::worker_main();
    std::process::exit(if result.is_ok() { 0 } else { 75 });
}
#[test]
#[ignore = "owned independent SQLite contender entrypoint"]
fn sqlite_contender() {
    let result = (|| -> anyhow::Result<bool> {
        let mut bytes = Vec::new();
        std::io::stdin().take(65537).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 65536, "contender path limit");
        let path: NativePath = serde_json::from_slice(&bytes)?;
        let db = rusqlite::Connection::open_with_flags(
            path.to_path()?,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        db.busy_timeout(Duration::ZERO)?;
        match db.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => {
                db.execute_batch("ROLLBACK")?;
                Ok(true)
            }
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    })();
    std::process::exit(match result {
        Ok(true) => 0,
        Ok(false) => 20,
        Err(_) => 21,
    });
}
pub(super) fn contender(path: &Path) -> anyhow::Result<bool> {
    let mut child = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "application::desktop::filesystem_tests::sqlite_contender",
            "--ignored",
            "--nocapture",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let written = (|| -> anyhow::Result<()> {
        let mut input = child.stdin.take().context("contender stdin")?;
        input.write_all(&serde_json::to_vec(&NativePath::from_path(path))?)?;
        Ok(())
    })();
    if let Err(e) = written {
        child.kill()?;
        child.wait()?;
        return Err(e);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait()? {
            return match status.code() {
                Some(0) => Ok(true),
                Some(20) => Ok(false),
                _ => Err(anyhow::anyhow!("contender failed with {status}")),
            };
        }
        if Instant::now() > deadline {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("contender timeout")
        }
        thread::sleep(Duration::from_millis(5));
    }
}
struct NoCatalogYet(Option<Arc<filesystem::Parent>>);
impl Drop for NoCatalogYet {
    fn drop(&mut self) {
        if let Some(f) = &self.0 {
            let _ = f.finish_after_dependents(true);
        }
    }
}
struct Pair {
    owner: Option<process::Owner>,
    relay: Arc<filesystem::Parent>,
    shared: Arc<Shared>,
}
impl Pair {
    fn finish(&mut self, fatal: bool) -> anyhow::Result<()> {
        self.shared.stop();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let state = self.shared.state.lock().unwrap();
            if state.child_finished || state.drain_error.is_some() {
                break;
            }
            drop(state);
            ensure!(
                Instant::now() < deadline,
                "C checked drain timed out; fixture cleanup retains F until C wait"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let result = self.owner.as_mut().unwrap().drain();
        let s = self.shared.state.lock().unwrap();
        ensure!(s.child_finished, "C and pipe owners not drained");
        ensure!(
            s.child_exit == Some(if fatal { 74 } else { 0 }),
            "wrong C exit"
        );
        drop(s);
        self.relay.finish_after_dependents(fatal)?;
        if !fatal {
            result.map_err(|e| anyhow::anyhow!(e.message))?;
        }
        Ok(())
    }
}
impl Drop for Pair {
    fn drop(&mut self) {
        // Fixture C has no native descendants. Its own verified reap, not receipt
        // delivery, is required before F termination. No unbounded stdout collection.
        if let Some(owner) = self.owner.as_mut() {
            self.shared.stop();
            if !self
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .child_finished
            {
                let _ = owner.terminate_no_descendant_fixture();
            } else {
                let _ = owner.drain();
            }
        }
        if self
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .child_finished
        {
            let _ = self.relay.finish_after_dependents(true);
        }
    }
}
fn actual(lost_confirm: bool, alias: bool, lost_prepare: bool) -> anyhow::Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE")
            .context("qualified built photocatalog executable required for actual F fixture")?,
    );
    ensure!(
        executable.is_absolute(),
        "actual F executable must be absolute"
    );
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let request = PrepareCatalog {
        operation: crate::application::U64(u64::MAX - 2),
        session: LeaseId::new(),
        mode: BootstrapMode::DesktopCreate,
        root: NativePath::from_path(&base.join("catalog")),
        manifest_root: NativePath::from_path(&base.join("manifest")),
        import_source: None,
    };
    let client = Arc::new(Client::spawn(&executable, vec![])?);
    let fpid = client.pid();
    let relay = filesystem::Parent::new(client.clone());
    let mut initial = NoCatalogYet(Some(relay.clone()));
    relay.healthy()?;
    let root = base.join("catalog");
    let marker = root.join(".photocatalog-restore.json");
    let database = root.join("catalog.sqlite3");
    let observer_client = client.clone();
    let original = request.clone();
    *relay.observer.lock().unwrap() = Some(Arc::new(move |call, after| {
        use filesystem::Call;
        match call {
            Call::Prepare(_) if after => {
                if lost_prepare {
                    anyhow::bail!("injected lost actual Prepare reply")
                }
                if !alias {
                    let receipt = crate::catalog_backup::RestoreReceipt {
                        protocol: 1,
                        restore_id: "e41041e0-6e87-4c0a-882e-6294ed3859e4".into(),
                        schema_version: crate::CURRENT_SCHEMA_VERSION,
                        backup: crate::catalog_backup::BackupReceipt {
                            protocol: 1,
                            backup_id: "e41041e0-6e87-4c0a-882e-6294ed3859e5".into(),
                            application_id: 0x50484341,
                            schema_version: crate::CURRENT_SCHEMA_VERSION,
                            database_bytes: 0,
                            database_blake3: "0".repeat(64),
                        },
                    };
                    std::fs::write(&marker, serde_json::to_vec(&receipt)?)?;
                }
            }
            Call::Confirm(_) if after => {
                ensure!(
                    observer_client
                        .admission_status(original.operation, &original.session)?
                        .unwrap()
                        .state
                        == AdmissionState::Confirmed,
                    "actual F did not confirm"
                );
                if lost_confirm {
                    anyhow::bail!("injected loss of actual F confirmation reply")
                }
            }
            Call::RestoreStatus(_) | Call::Resume { .. } => {
                ensure!(
                    !contender(&database)?,
                    "writer lock escaped F marker operation"
                );
                if alias && matches!(call, Call::RestoreStatus(_)) {
                    if !after {
                        let shm = root.join("catalog.sqlite3-shm");
                        ensure!(
                            std::fs::metadata(&shm)?.len() <= 65536,
                            "shm fixture must reach bounded read"
                        );
                        std::fs::hard_link(&shm, &marker)?;
                    } else {
                        std::fs::remove_file(&marker)?;
                    }
                }
            }
            Call::Release(_) if !after => ensure!(
                contender(&database)?,
                "closed roster still blocks external SQL"
            ),
            _ => {}
        }
        Ok(())
    }));
    let mut shared = tests::shared(1024 * 1024);
    {
        let s = Arc::get_mut(&mut shared).unwrap();
        s.filesystem = Some(relay.clone());
        let state = s.state.get_mut().unwrap();
        state.phase = TransportPhase::Starting;
        state.ready = false;
        state.filesystem_verified = false;
    }
    let config = Config {
        worker_executable: executable,
        cache_root: None,
        original_roots: vec![],
        preview_policy: crate::preview::PreviewPolicy::default(),
        preview_limits: crate::preview::ServiceLimits::default(),
        limits: Limits::default(),
        import_checkpoint: None,
    };
    let mut wire = wire::ConfigWire::from_config(&config);
    wire.filesystem = Some(relay.binding.clone());
    wire.fixture = Some(Fixture {
        request: request.clone(),
        lost_confirm,
        alias,
        lost_prepare,
    });
    let args = [
        "--exact",
        "application::desktop::filesystem_tests::catalog_child",
        "--ignored",
        "--nocapture",
    ]
    .map(std::ffi::OsString::from);
    let owner = process::Owner::spawn_test(
        &std::env::current_exe()?,
        &args,
        shared.clone(),
        serde_json::to_vec(&wire)?,
    )?;
    let cpid = owner.pid();
    initial.0.take();
    let mut pair = Pair {
        owner: Some(owner),
        relay,
        shared: shared.clone(),
    };
    eprintln!("actual relay fixture G owns C={cpid} F={fpid}");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if lost_confirm && shared.state.lock().unwrap().child_finished {
            break;
        }
        if let Some(result) = shared.fixture.lock().unwrap().take() {
            result.map_err(anyhow::Error::msg)?;
            break;
        }
        ensure!(Instant::now() < deadline, "actual paired fixture timed out");
        thread::sleep(Duration::from_millis(5));
    }
    pair.finish(lost_confirm)?;
    if !lost_confirm {
        ensure!(
            client
                .admission_status(request.operation, &request.session)
                .is_err(),
            "stopped F must not grant live admission"
        );
    }
    eprintln!("actual relay fixture verified C={cpid} F={fpid} reaped");
    Ok(())
}
#[test]
#[ignore = "requires the exact built CLI; scripts/test_catalog_filesystem_processes.py runs this"]
fn actual_f_and_eight_sql_roles_preserve_wal_write_lock_through_marker_and_close()
-> anyhow::Result<()> {
    actual(false, false, false)
}
#[test]
#[ignore = "requires the exact built CLI; scripts/test_catalog_filesystem_processes.py runs this"]
fn actual_confirm_loss_reaps_c74_before_f_retirement() -> anyhow::Result<()> {
    actual(true, false, false)
}
#[cfg(unix)]
#[test]
#[ignore = "requires the exact built CLI; scripts/test_catalog_filesystem_processes.py runs this"]
fn actual_f_binary_shm_alias_read_does_not_release_c_posix_lock() -> anyhow::Result<()> {
    actual(false, true, false)
}

#[test]
#[ignore = "requires the exact built CLI; scripts/test_catalog_filesystem_processes.py runs this"]
fn actual_lost_prepare_inspects_original_record_and_abandons_without_replay() -> anyhow::Result<()>
{
    actual(false, false, true)
}
