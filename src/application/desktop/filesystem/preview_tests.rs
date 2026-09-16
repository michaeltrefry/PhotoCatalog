//! Configured C/G/F/N preview and export routes on owned temporary catalogs.
use super::super::DesktopBridge;
use super::{Call, Parent};
use crate::application::{
    Config, Limits, PreviewState, PreviewStatus, PreviewTier, Reply, Request, Response, U64,
};
use crate::catalog_edits::VariantKey;
use crate::catalog_session::{CatalogFilesystem, RootCapability, native as n};
use crate::filesystem_worker::client::Client;
use crate::storage_volume::NativePath;
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

struct Observation {
    root: RootCapability,
    operation: U64,
    pid: u32,
    render: bool,
}
// Only held before C construction; no catalog or native request can exist yet.
struct BeforeCatalog {
    parent: Option<Arc<Parent>>,
    temporary: Arc<tempfile::TempDir>,
}
impl Drop for BeforeCatalog {
    fn drop(&mut self) {
        if let Some(parent) = self.parent.take()
            && let Err(orderly) = parent.finish_after_dependents(false)
            && let Err(retained) = parent.finish_after_dependents(true)
        {
            // No C/N was admitted, so forced F retirement is legal here only.
            eprintln!(
                "pre-catalog fixture cleanup retained: orderly={orderly:#}; forced={retained:#}"
            );
            std::mem::forget(parent);
            std::mem::forget(self.temporary.clone());
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum CleanupState {
    Active,
    Attempted,
    Retired,
}
struct Running {
    bridge: DesktopBridge,
    parent: Arc<Parent>,
    client: Arc<Client>,
    observed: Arc<Mutex<Vec<Observation>>>,
    restored_roots: Arc<Mutex<Vec<RootCapability>>>,
    metadata: crate::preview::ByteBudget,
    native: crate::preview::ByteBudget,
    temporary: Arc<tempfile::TempDir>,
    token: Option<String>,
    cleanup: CleanupState,
}
struct ExportFixtureOptions<'a> {
    temporary: Arc<tempfile::TempDir>,
    executable: &'a Path,
    root: &'a Path,
    originals: &'a Path,
    small: bool,
    codec: crate::preview::Codec,
    workers: usize,
    export_executable: &'a Path,
    worker_bytes: u64,
    configured_originals: bool,
}
type ManagedExportMetadataJob = (String, PathBuf, &'static str, Vec<u8>);
type ExportObserverRecord = (String, [u8; 32]);

fn command(bridge: &DesktopBridge, request: Request) -> Result<Response> {
    let result = bridge
        .submit(request)
        .with_context(|| format!("configured command admission: {:?}", bridge.status()))?
        .receiver
        .recv_timeout(Duration::from_secs(30))?;
    match result {
        Reply::Ok { value } => Ok(value),
        Reply::Error { error } => Err(error.into()),
    }
}
impl Running {
    fn start(
        temporary: Arc<tempfile::TempDir>,
        executable: &Path,
        root: &Path,
        originals: &Path,
        small: bool,
    ) -> Result<(Self, String)> {
        Self::start_codec(
            temporary,
            executable,
            root,
            originals,
            small,
            crate::preview::Codec::Jpeg,
        )
    }
    fn start_codec(
        temporary: Arc<tempfile::TempDir>,
        executable: &Path,
        root: &Path,
        originals: &Path,
        small: bool,
        codec: crate::preview::Codec,
    ) -> Result<(Self, String)> {
        Self::start_options(temporary, executable, root, originals, small, codec, 1)
    }
    fn start_options(
        temporary: Arc<tempfile::TempDir>,
        executable: &Path,
        root: &Path,
        originals: &Path,
        small: bool,
        codec: crate::preview::Codec,
        workers: usize,
    ) -> Result<(Self, String)> {
        Self::start_export_options(ExportFixtureOptions {
            temporary,
            executable,
            root,
            originals,
            small,
            codec,
            workers,
            export_executable: executable,
            worker_bytes: 64 * 1024 * 1024,
            configured_originals: true,
        })
    }
    fn start_export_options(options: ExportFixtureOptions<'_>) -> Result<(Self, String)> {
        let ExportFixtureOptions {
            temporary,
            executable,
            root,
            originals,
            small,
            codec,
            workers,
            export_executable,
            worker_bytes,
            configured_originals,
        } = options;
        ensure!((1..=2).contains(&workers), "fixture worker count");
        let client = Arc::new(Client::spawn(
            executable,
            configured_originals
                .then(|| NativePath::from_path(originals))
                .into_iter()
                .collect(),
        )?);
        let parent = Parent::new(client.clone());
        let mut before_catalog = BeforeCatalog {
            parent: Some(parent.clone()),
            temporary: temporary.clone(),
        };
        let limits = crate::preview::ServiceLimits {
            workers,
            working_bytes: if small {
                2 * 1024 * 1024
            } else {
                2 * worker_bytes * workers as u64
            },
            per_worker_bytes: if small { 64 * 1024 } else { worker_bytes },
            cache_header_scratch_bytes: 4096,
            cache_codec_scratch_bytes: 64 * 1024,
            encoded_staging_bytes: 8 * 1024 * 1024 * workers as u64,
            per_worker_encoded_bytes: 4 * 1024 * 1024,
            ..Default::default()
        };
        let mut policy = crate::preview::PreviewPolicy::default();
        policy.thumbnail.edge = 256;
        policy.large.edge = 512;
        policy.thumbnail.encoding.codec = codec;
        policy.large.encoding.codec = codec;
        let config = Config {
            worker_executable: executable.to_owned(),
            cache_root: None,
            original_roots: configured_originals
                .then(|| originals.to_owned())
                .into_iter()
                .collect(),
            preview_policy: policy,
            preview_limits: limits.clone(),
            limits: Limits::default(),
            import_checkpoint: None,
        };
        let native = crate::preview::ByteBudget::new(limits.working_bytes)?;
        parent.configure_native(executable.to_owned(), limits, &native)?;
        parent.configure_export_native(export_executable.to_owned(), workers, &native)?;
        let observed: Arc<Mutex<Vec<Observation>>> = Default::default();
        let restored_roots: Arc<Mutex<Vec<RootCapability>>> = Default::default();
        let weak = Arc::downgrade(&parent);
        let observations = observed.clone();
        let restored = restored_roots.clone();
        *parent.observer.lock().unwrap() = Some(Arc::new(move |call, after| {
            if after && let Call::RestoreOriginalRoot(request) = call {
                restored.lock().unwrap().push(request.root.clone());
            }
            if after
                && let Call::Native(request) = call
                && let n::Action::Spawn { work, .. } = &request.action
            {
                let parent = weak.upgrade().context("fixture parent dropped")?;
                let status = parent
                    .native_owner()?
                    .status(&request.root, request.operation)?;
                if let Some(pid) = status.pid {
                    let mut rows = observations.lock().unwrap();
                    ensure!(rows.len() < 32, "fixture observation cap");
                    rows.push(Observation {
                        root: request.root.clone(),
                        operation: request.operation,
                        pid,
                        render: matches!(work, n::Work::Render(_)),
                    });
                    eprintln!(
                        "configured preview N pid={pid} operation={} render={}",
                        request.operation.0,
                        matches!(work, n::Work::Render(_))
                    );
                }
            }
            Ok(())
        }));
        config.validate()?;
        let metadata = crate::preview::ByteBudget::new(config.requested_preview_metadata_bytes()?)?;
        let migration_source = crate::preview::ByteBudget::new(1)?;
        let migration_result = crate::preview::ByteBudget::new(1)?;
        // All pre-C fallibility passed while guarded. Once called, spawn_inner
        // owns C creation and its Unstarted error retains F when required.
        before_catalog.parent.take();
        let bridge = match DesktopBridge::spawn_inner(
            config,
            Some(parent.clone()),
            Some(&metadata),
            Some(&migration_source),
            Some(&migration_result),
        ) {
            Ok(bridge) => bridge,
            Err(error) => {
                if let Some(unstarted) = error.downcast_ref::<super::Unstarted>()
                    && let Err(retirement) = unstarted.retire()
                {
                    let retained_owner = unstarted.owner.clone();
                    eprintln!(
                        "configured preview pre-C cleanup failed: {retirement:#}; fixture retained at {:?}",
                        temporary.path()
                    );
                    std::mem::forget(retained_owner);
                    std::mem::forget(temporary);
                }
                return Err(error);
            }
        };
        eprintln!(
            "configured preview G={} C={} F={}",
            std::process::id(),
            bridge.status().pid,
            client.pid()
        );
        let mut running = Self {
            bridge,
            parent,
            client,
            observed,
            restored_roots,
            metadata,
            native,
            temporary,
            token: None,
            cleanup: CleanupState::Active,
        };
        let Response::Status(status) = command(
            &running.bridge,
            Request::OpenExisting {
                path: NativePath::from_path(root),
            },
        )?
        else {
            anyhow::bail!("wrong open reply")
        };
        let token = status.catalog.context("missing catalog token")?;
        running.token = Some(token.clone());
        Ok((running, token))
    }
    fn ready(&self, token: &str, key: &VariantKey, generation: u64) -> Result<PreviewStatus> {
        let Response::Preview(mut status) = command(
            &self.bridge,
            Request::Preview {
                catalog: token.into(),
                key: key.clone(),
                tier: PreviewTier::Thumbnail,
                interactive: false,
                viewport: "fixture".into(),
                generation: U64(generation),
                foreground: true,
            },
        )?
        else {
            anyhow::bail!("wrong preview reply")
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(status.state, PreviewState::Ready) {
            ensure!(
                matches!(status.state, PreviewState::Queued),
                "preview failed: {:?} {:?}",
                status.state,
                status.message
            );
            ensure!(Instant::now() < deadline, "preview ready timeout");
            thread::sleep(Duration::from_millis(5));
            let Response::Preview(next) = command(
                &self.bridge,
                Request::PreviewStatus {
                    catalog: token.into(),
                    ticket: status.ticket,
                },
            )?
            else {
                anyhow::bail!("wrong status reply")
            };
            status = next;
        }
        Ok(status)
    }
    fn bytes(&self, token: &str, ticket: &str) -> Result<crate::application::PreviewBytes> {
        Ok(self
            .bridge
            .preview_bytes(token.into(), ticket.into(), true)?
            .receiver
            .recv_timeout(Duration::from_secs(30))??)
    }
    fn mark_externally_retired(&mut self) {
        self.token = None;
        self.cleanup = CleanupState::Retired;
    }
    fn finish(mut self, token: String) -> Result<()> {
        ensure!(
            self.token.as_deref() == Some(token.as_str()),
            "configured preview catalog token changed before cleanup"
        );
        let metadata = self.metadata.clone();
        let result = self.cleanup();
        drop(self);
        result?;
        ensure!(
            metadata.used() == 0,
            "metadata retained after final C/F owners dropped"
        );
        Ok(())
    }
    fn cleanup(&mut self) -> Result<()> {
        ensure!(
            self.cleanup == CleanupState::Active,
            "configured preview cleanup already attempted"
        );
        self.cleanup = CleanupState::Attempted;
        let mut first = None;
        if let Some(token) = self.token.take()
            && let Err(error) = command(&self.bridge, Request::Close { catalog: token })
        {
            first = Some(error.context("configured preview catalog close"));
        }
        if let Err(error) = self.bridge.try_shutdown() {
            if first.is_none() {
                first = Some(anyhow::Error::new(error).context("configured preview C shutdown"));
            } else {
                eprintln!("configured preview C shutdown also failed: {error:#}");
            }
        }
        let c_reaped = {
            let state = self.bridge.0.shared.state.lock().unwrap();
            state.reaped && state.child_finished
        };
        if !c_reaped {
            let error = anyhow::anyhow!("configured preview C wait/pipe retirement unverified");
            if first.is_none() {
                first = Some(error);
            } else {
                eprintln!("{error:#}");
            }
        }
        // DesktopBridge may already have checked native dependents, reaped F,
        // and joined the relay. In particular, forced retirement preserves an
        // Unknown operation diagnostic; invoking normal F work again is invalid.
        let mut filesystem_retired = self
            .bridge
            .0
            .shared
            .state
            .lock()
            .unwrap()
            .filesystem_verified;
        if filesystem_retired {
            ensure!(
                self.parent.threads.lock().unwrap().is_empty(),
                "F retirement proof retained relay threads"
            );
        }
        if c_reaped && !filesystem_retired {
            // This fixture admits only the preview helper allowlist. Once C is
            // reaped, forced F cleanup cannot race an untracked descendant.
            match self.parent.finish_after_dependents(false) {
                Ok(()) => filesystem_retired = true,
                Err(orderly) => match self.parent.finish_after_dependents(true) {
                    Ok(()) => filesystem_retired = true,
                    Err(forced) => {
                        let error = anyhow::anyhow!(
                            "configured preview F retirement failed: orderly={orderly:#}; forced={forced:#}"
                        );
                        if first.is_none() {
                            first = Some(error);
                        } else {
                            eprintln!("{error:#}");
                        }
                    }
                },
            }
        }
        let mut native_retired = false;
        if filesystem_retired {
            match self.parent.native_owner() {
                Ok(owner) => {
                    native_retired = true;
                    for row in self.observed.lock().unwrap().iter() {
                        if owner.status(&row.root, row.operation).is_ok() {
                            native_retired = false;
                            let error = anyhow::anyhow!(
                                "native operation {} retained after checked close",
                                row.operation.0
                            );
                            if first.is_none() {
                                first = Some(error);
                            } else {
                                eprintln!("{error:#}");
                            }
                        } else {
                            eprintln!(
                                "configured preview verified N pid={} retired after wait/pipe joins",
                                row.pid
                            );
                        }
                    }
                }
                Err(error) => {
                    if first.is_none() {
                        first = Some(error.context("configured preview native owner"));
                    } else {
                        eprintln!("configured preview native owner also failed: {error:#}");
                    }
                }
            }
        }
        if c_reaped && filesystem_retired && native_retired {
            self.cleanup = CleanupState::Retired;
            eprintln!(
                "configured preview verified C={} F={} checked retirement",
                self.bridge.status().pid,
                self.client.pid()
            );
        }
        match first {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if self.cleanup == CleanupState::Active
            && let Err(error) = self.cleanup()
        {
            eprintln!(
                "configured preview early-error cleanup failed: {error:#}; retaining custody"
            );
        }
        if self.cleanup != CleanupState::Retired {
            eprintln!(
                "configured preview unresolved custody retained C={} F={} at {:?}",
                self.bridge.status().pid,
                self.client.pid(),
                self.temporary.path()
            );
            std::mem::forget(self.bridge.clone());
            std::mem::forget(self.parent.clone());
            std::mem::forget(self.client.clone());
            std::mem::forget(self.metadata.clone());
            std::mem::forget(self.temporary.clone());
        }
    }
}
fn fixture() -> Result<(Arc<tempfile::TempDir>, PathBuf, PathBuf, VariantKey, String)> {
    let temp = Arc::new(tempfile::tempdir()?);
    let base = temp.path().canonicalize()?;
    let originals = base.join("originals");
    std::fs::create_dir(&originals)?;
    let path = originals.join("original.png");
    let mut image = image::RgbImage::new(256, 256);
    let mut state = 7u32;
    for pixel in image.pixels_mut() {
        for v in &mut pixel.0 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = (state >> 24) as u8;
        }
    }
    image.save(&path)?;
    let checksum = blake3::hash(&std::fs::read(&path)?).to_hex().to_string();
    let root = base.join("catalog");
    let mut catalog = crate::Catalog::open(&root)?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let key = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
    // Import retains a legacy preview. Managed tickets request current output
    // with allow_stale=false, so the empty current manifest forces Render N.
    drop(catalog);
    Ok((temp, root, originals, key, checksum))
}

#[cfg(unix)]
#[derive(Default)]
struct BackupProcessProbe {
    state: Mutex<(Vec<crate::catalog_backup::managed::ProcessEvent>, bool)>,
    wake: std::sync::Condvar,
}
#[cfg(unix)]
struct BackupProbePause(Arc<BackupProcessProbe>);
#[cfg(unix)]
impl Drop for BackupProbePause {
    fn drop(&mut self) {
        // Release the injected pause before Running's checked cleanup, even
        // when an assertion or fallible setup returns early.
        self.0.release();
    }
}
#[cfg(unix)]
impl BackupProcessProbe {
    fn observe(&self, event: crate::catalog_backup::managed::ProcessEvent) {
        let mut state = self.state.lock().unwrap();
        state.0.push(event);
        self.wake.notify_all();
        if matches!(
            event,
            crate::catalog_backup::managed::ProcessEvent::Spawned { .. }
        ) {
            while state.1 {
                state = self.wake.wait(state).unwrap();
            }
        }
    }
    fn pause(self: &Arc<Self>) -> BackupProbePause {
        self.state.lock().unwrap().1 = true;
        BackupProbePause(self.clone())
    }
    fn spawned(&self) -> Result<(u32, u32)> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(pids) = state.0.iter().find_map(|event| match event {
                crate::catalog_backup::managed::ProcessEvent::Spawned { backup, filesystem } => {
                    Some((*backup, *filesystem))
                }
                _ => None,
            }) {
                return Ok(pids);
            }
            let now = Instant::now();
            ensure!(now < deadline, "managed backup process spawn timeout");
            state = self.wake.wait_timeout(state, deadline - now).unwrap().0;
        }
    }
    fn release(&self) {
        self.state.lock().unwrap().1 = false;
        self.wake.notify_all();
    }
    fn reaped(&self, backup: u32, filesystem: u32) -> bool {
        let state = self.state.lock().unwrap();
        state
            .0
            .contains(&crate::catalog_backup::managed::ProcessEvent::BackupReaped { backup })
            && state.0.contains(
                &crate::catalog_backup::managed::ProcessEvent::FilesystemReaped { filesystem },
            )
    }
}

#[cfg(unix)]
fn process_parent(pid: u32) -> Result<u32> {
    let pid_text = pid.to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", pid_text.as_str()])
        .output()?;
    ensure!(output.status.success(), "ps failed for process {pid}");
    Ok(std::str::from_utf8(&output.stdout)?.trim().parse()?)
}

#[cfg(unix)]
fn backup_snapshot(bridge: &DesktopBridge) -> Result<Option<crate::application::backup::Snapshot>> {
    let Response::Backup(snapshot) = command(bridge, Request::BackupStatus)? else {
        anyhow::bail!("wrong backup status reply")
    };
    Ok(snapshot)
}

#[cfg(unix)]
fn wait_backup_terminal(bridge: &DesktopBridge) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if backup_snapshot(bridge)?.is_some_and(|snapshot| {
            matches!(
                snapshot.state,
                crate::application::backup::State::Complete
                    | crate::application::backup::State::Failed
            )
        }) {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "managed backup terminal timeout");
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
#[test]
#[ignore = "requires explicitly configured built CLI; actual G/C/B/F fixture"]
fn actual_g_owns_backup_siblings_and_close_waits_for_checked_drain() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let (temporary, root, originals, _, _) = fixture()?;
    let (mut running, token) =
        Running::start(temporary.clone(), &executable, &root, &originals, false)?;
    let probe = Arc::new(BackupProcessProbe::default());
    let _pause = probe.pause();
    let observer = probe.clone();
    running
        .bridge
        .set_backup_process_probe(Arc::new(move |event| observer.observe(event)))?;
    let bundle = temporary.path().join("close-backup");
    let Response::Backup(Some(started)) = command(
        &running.bridge,
        Request::BackupCreate {
            catalog: token.clone(),
            bundle: NativePath::from_path(&bundle),
        },
    )?
    else {
        anyhow::bail!("wrong backup start reply")
    };
    let (backup_pid, filesystem_pid) = probe.spawned()?;
    ensure!(
        process_parent(backup_pid)? == std::process::id(),
        "B is not a direct G child"
    );
    ensure!(
        process_parent(filesystem_pid)? == std::process::id(),
        "backup F is not a direct G child"
    );
    ensure!(
        command(
            &running.bridge,
            Request::Export {
                catalog: token.clone(),
                request: Box::new(crate::application::exports::Request::Begin),
            },
        )
        .is_err(),
        "G admitted a new export job while backup custody was live"
    );
    ensure!(
        command(
            &running.bridge,
            Request::Close {
                catalog: "stale-close-token".into(),
            },
        )
        .is_err(),
        "stale Close unexpectedly reached G retirement"
    );
    ensure!(
        backup_snapshot(&running.bridge)?.is_some_and(|snapshot| {
            snapshot.operation == started.operation
                && snapshot.state == crate::application::backup::State::Running
                && !snapshot.cancellation_requested
        }),
        "stale Close changed the active backup"
    );
    ensure!(
        unsafe { libc::kill(backup_pid as libc::pid_t, 0) } == 0
            && unsafe { libc::kill(filesystem_pid as libc::pid_t, 0) } == 0,
        "stale Close retired a G backup child"
    );
    let oversized = "x".repeat(running.bridge.0.shared.limits.request_bytes + 1);
    ensure!(
        running
            .bridge
            .submit(Request::Close { catalog: oversized })
            .is_err(),
        "oversized Close passed the shared public request boundary"
    );
    ensure!(
        backup_snapshot(&running.bridge)?.is_some_and(|snapshot| {
            snapshot.operation == started.operation
                && snapshot.state == crate::application::backup::State::Running
                && !snapshot.cancellation_requested
        }),
        "oversized Close changed the active backup"
    );
    let close = running.bridge.submit(Request::Close {
        catalog: token.clone(),
    })?;
    ensure!(
        close
            .receiver
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "Close acknowledged before the paused B/F drain"
    );
    probe.release();
    match close.receiver.recv_timeout(Duration::from_secs(30))? {
        Reply::Ok {
            value: Response::Status(status),
        } => ensure!(status.catalog.is_none(), "Close retained catalog token"),
        Reply::Error { error } => return Err(error.into()),
        _ => anyhow::bail!("wrong Close reply"),
    }
    wait_backup_terminal(&running.bridge)?;
    ensure!(
        probe.reaped(backup_pid, filesystem_pid),
        "Close returned before checked B/F reap"
    );
    ensure!(
        backup_snapshot(&running.bridge)?.is_some_and(|snapshot| {
            snapshot.operation == started.operation && snapshot.cancellation_requested
        }),
        "Close did not preserve the canceled operation identity"
    );
    running.token = None;
    running.cleanup()?;
    ensure!(
        running.bridge.status().phase == super::super::TransportPhase::Closed,
        "desktop reported Closed before complete control-task drain"
    );
    let tasks = running.bridge.0.control_tasks.lock().unwrap();
    ensure!(
        tasks.backup_admission.is_none() && tasks.close.is_none(),
        "Closed retained a backup control task"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
#[ignore = "requires explicitly configured built CLI; actual G/C/B/F fixture"]
fn actual_c_death_cancels_and_reaps_g_owned_backup_siblings() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let (temporary, root, originals, _, _) = fixture()?;
    let (mut running, token) =
        Running::start(temporary.clone(), &executable, &root, &originals, false)?;
    let probe = Arc::new(BackupProcessProbe::default());
    let _pause = probe.pause();
    let observer = probe.clone();
    running
        .bridge
        .set_backup_process_probe(Arc::new(move |event| observer.observe(event)))?;
    let Response::Backup(Some(_)) = command(
        &running.bridge,
        Request::BackupCreate {
            catalog: token,
            bundle: NativePath::from_path(&temporary.path().join("c-death-backup")),
        },
    )?
    else {
        anyhow::bail!("wrong backup start reply")
    };
    let (backup_pid, filesystem_pid) = probe.spawned()?;
    let c_pid = running.bridge.status().pid;
    ensure!(
        unsafe { libc::kill(c_pid as libc::pid_t, libc::SIGKILL) } == 0,
        "kill C"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !matches!(
        running.bridge.status().phase,
        super::super::TransportPhase::Failed | super::super::TransportPhase::Draining
    ) {
        ensure!(Instant::now() < deadline, "C death was not observed by G");
        thread::sleep(Duration::from_millis(5));
    }
    probe.release();
    wait_backup_terminal(&running.bridge)?;
    running.token = None;
    running.cleanup()?;
    ensure!(
        probe.reaped(backup_pid, filesystem_pid),
        "C death cleanup returned before checked B/F reap"
    );
    let tasks = running.bridge.0.control_tasks.lock().unwrap();
    ensure!(
        tasks.backup_admission.is_none() && tasks.close.is_none(),
        "C death cleanup retained a backup control task"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
#[ignore = "requires explicitly configured built CLI; actual G/C/B/F fixture"]
fn actual_public_backup_cancel_reaps_both_g_children_before_terminal_status() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let (temporary, root, originals, _, _) = fixture()?;
    let (running, token) =
        Running::start(temporary.clone(), &executable, &root, &originals, false)?;
    let probe = Arc::new(BackupProcessProbe::default());
    let _pause = probe.pause();
    let observer = probe.clone();
    running
        .bridge
        .set_backup_process_probe(Arc::new(move |event| observer.observe(event)))?;
    let Response::Backup(Some(started)) = command(
        &running.bridge,
        Request::BackupCreate {
            catalog: token.clone(),
            bundle: NativePath::from_path(&temporary.path().join("cancel-backup")),
        },
    )?
    else {
        anyhow::bail!("wrong backup start reply")
    };
    let (backup_pid, filesystem_pid) = probe.spawned()?;
    let Response::Backup(Some(canceling)) = command(
        &running.bridge,
        Request::BackupCancel {
            operation: started.operation.clone(),
        },
    )?
    else {
        anyhow::bail!("wrong backup cancel reply")
    };
    ensure!(
        canceling.cancellation_requested
            && canceling.state == crate::application::backup::State::CancelRequested,
        "public cancel did not retain the running operation"
    );
    probe.release();
    wait_backup_terminal(&running.bridge)?;
    ensure!(
        probe.reaped(backup_pid, filesystem_pid),
        "terminal cancel status preceded checked B/F reap"
    );
    running.finish(token)
}

#[cfg(unix)]
#[test]
#[ignore = "requires explicitly configured built CLI; actual G/C/B/F fixture"]
fn actual_global_inspect_and_restore_remain_legal_after_catalog_close() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let (temporary, root, originals, _, _) = fixture()?;
    let (mut running, token) =
        Running::start(temporary.clone(), &executable, &root, &originals, false)?;
    let bundle = temporary.path().join("closed-inspection");
    let rejected = temporary.path().join("stale-token");
    ensure!(
        command(
            &running.bridge,
            Request::BackupCreate {
                catalog: "stale-catalog-token".into(),
                bundle: NativePath::from_path(&rejected),
            },
        )
        .is_err(),
        "C admitted a stale catalog token for backup"
    );
    ensure!(
        !rejected.exists(),
        "stale token reached G backup process creation"
    );
    let Response::Backup(Some(_)) = command(
        &running.bridge,
        Request::BackupCreate {
            catalog: token.clone(),
            bundle: NativePath::from_path(&bundle),
        },
    )?
    else {
        anyhow::bail!("wrong backup start reply")
    };
    wait_backup_terminal(&running.bridge)?;
    ensure!(
        backup_snapshot(&running.bridge)?.is_some_and(|snapshot| {
            snapshot.state == crate::application::backup::State::Complete
                && matches!(
                    snapshot.receipt,
                    Some(crate::application::backup::Receipt::Backup(_))
                )
        }),
        "managed create did not publish a backup receipt"
    );
    let Response::Status(closed) = command(
        &running.bridge,
        Request::Close {
            catalog: token.clone(),
        },
    )?
    else {
        anyhow::bail!("wrong Close reply")
    };
    ensure!(closed.catalog.is_none(), "catalog remained open");
    running.token = None;
    let Response::Backup(Some(_)) = command(
        &running.bridge,
        Request::BackupInspect {
            bundle: NativePath::from_path(&bundle),
        },
    )?
    else {
        anyhow::bail!("wrong inspect start reply")
    };
    wait_backup_terminal(&running.bridge)?;
    ensure!(
        backup_snapshot(&running.bridge)?.is_some_and(|snapshot| {
            snapshot.state == crate::application::backup::State::Complete
                && matches!(
                    snapshot.receipt,
                    Some(crate::application::backup::Receipt::Backup(_))
                )
        }),
        "managed inspect did not retain its receipt"
    );
    let destination = temporary.path().join("restored");
    let Response::Backup(Some(_)) = command(
        &running.bridge,
        Request::BackupRestore {
            bundle: NativePath::from_path(&bundle),
            destination: NativePath::from_path(&destination),
        },
    )?
    else {
        anyhow::bail!("wrong restore start reply")
    };
    wait_backup_terminal(&running.bridge)?;
    ensure!(
        backup_snapshot(&running.bridge)?.is_some_and(|snapshot| {
            snapshot.state == crate::application::backup::State::Complete
                && matches!(
                    snapshot.receipt,
                    Some(crate::application::backup::Receipt::Restore(_))
                )
        }),
        "managed restore did not retain its receipt"
    );
    let restored = crate::Catalog::open(&destination)?;
    ensure!(
        restored
            .restore_status()?
            .is_some_and(|status| status.jobs_held),
        "restored catalog lost its external-job hold"
    );
    drop(restored);
    running.cleanup()
}
#[test]
#[ignore = "requires explicitly configured built CLI; actual C/G/F/N fixture"]
fn actual_managed_render_cold_cache_decode_and_warm_delivery() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let (temporary, root, originals, key, original) = fixture()?;
    let (running, token) =
        Running::start(temporary.clone(), &executable, &root, &originals, false)?;
    let status = running.ready(&token, &key, 1)?;
    let bytes = running.bytes(&token, &status.ticket)?;
    ensure!(
        bytes.bytes().len() > 16 * 1024,
        "fixture must span multiple F chunks"
    );
    let digest = blake3::hash(bytes.bytes());
    drop(bytes);
    ensure!(
        running.observed.lock().unwrap().iter().any(|r| r.render),
        "cold request did not render in N"
    );
    running.finish(token)?;
    let (running, token) = Running::start(temporary.clone(), &executable, &root, &originals, true)?;
    let cold = Instant::now();
    let status = running.ready(&token, &key, 1)?;
    let bytes = running.bytes(&token, &status.ticket)?;
    ensure!(
        blake3::hash(bytes.bytes()) == digest,
        "cached encoded bytes changed"
    );
    drop(bytes);
    let cold_elapsed = cold.elapsed();
    let initial = running.observed.lock().unwrap().len();
    ensure!(
        initial > 0 && running.observed.lock().unwrap().iter().all(|r| !r.render),
        "cold cache did not use DecodeEncoded only"
    );
    let warm = Instant::now();
    for generation in 2..=9 {
        let status = running.ready(&token, &key, generation)?;
        let bytes = running.bytes(&token, &status.ticket)?;
        ensure!(
            blake3::hash(bytes.bytes()) == digest,
            "warm encoded bytes changed"
        );
        drop(bytes);
    }
    ensure!(
        running.observed.lock().unwrap().len() == initial,
        "warm decoded hits launched N"
    );
    eprintln!(
        "bounded cache batch cold_ms={} warm8_ms={} (not 200-preview performance acceptance)",
        cold_elapsed.as_millis(),
        warm.elapsed().as_millis()
    );
    running.finish(token)?;
    ensure!(
        blake3::hash(&std::fs::read(originals.join("original.png"))?)
            .to_hex()
            .as_str()
            == original,
        "original bytes changed"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
#[ignore = "configured actual C/F early-error fixture cleanup"]
fn actual_managed_second_session_error_reaps_c_f_and_releases_budget() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let (temporary, root, originals, _, original) = fixture()?;
    let (first, token) = Running::start(temporary.clone(), &executable, &root, &originals, true)?;
    first.finish(token)?;

    let (second, _) = Running::start(temporary.clone(), &executable, &root, &originals, true)?;
    let c_pid = second.bridge.status().pid;
    let f_pid = second.client.pid();
    let budget = second.metadata.clone();
    let original_error = (|| -> Result<()> {
        let _running = second;
        anyhow::bail!("injected second-session error")
    })();
    ensure!(
        original_error
            .unwrap_err()
            .to_string()
            .contains("injected second-session error"),
        "early assertion error was not preserved"
    );
    ensure!(budget.used() == 0, "early-error metadata retained");
    process_is_gone(c_pid)?;
    process_is_gone(f_pid)?;
    match Running::start(
        temporary.clone(),
        &executable,
        &root.join("missing-catalog"),
        &originals,
        true,
    ) {
        Err(_) => {}
        Ok((running, token)) => {
            running.finish(token)?;
            anyhow::bail!("post-spawn startup failure was not reached")
        }
    }
    ensure!(
        blake3::hash(&std::fs::read(originals.join("original.png"))?)
            .to_hex()
            .as_str()
            == original,
        "early-error cleanup changed the synthetic original"
    );
    Ok(())
}

#[cfg(unix)]
fn process_is_gone(pid: u32) -> Result<()> {
    let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
    ensure!(
        status == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
        "configured fixture process {pid} was not reaped"
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum HeldRead {
    Object(u64),
    Stage(u64),
}
struct Hold {
    entered: std::sync::mpsc::Receiver<()>,
    release: Option<std::sync::mpsc::SyncSender<()>>,
}
impl Hold {
    fn install(running: &Running, kind: HeldRead) -> Self {
        let original = running.parent.observer.lock().unwrap().clone();
        let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::sync_channel(1);
        let gate = Mutex::new(Some((entered_tx, release_rx)));
        *running.parent.observer.lock().unwrap() = Some(Arc::new(move |call, after| {
            if let Some(original) = &original {
                original(call, after)?;
            }
            let matching = !after
                && match (kind, call) {
                    (HeldRead::Object(wanted), Call::PreviewIo(r)) => {
                        matches!(&r.action,crate::catalog_session::preview_io::Action::Read{offset} if offset.0==wanted)
                    }
                    (HeldRead::Stage(wanted), Call::PreviewStage(r)) => {
                        matches!(&r.action,crate::catalog_session::preview_stage::Action::Read{offset,..} if offset.0==wanted)
                    }
                    _ => false,
                };
            if matching && let Some((entered, release)) = gate.lock().unwrap().take() {
                entered.send(())?;
                release.recv()?;
            }
            Ok(())
        }));
        Self {
            entered,
            release: Some(release),
        }
    }
    fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}
impl Drop for Hold {
    fn drop(&mut self) {
        self.release();
    }
}
fn request(
    running: &Running,
    token: &str,
    key: &VariantKey,
    generation: u64,
) -> Result<PreviewStatus> {
    match command(
        &running.bridge,
        Request::Preview {
            catalog: token.into(),
            key: key.clone(),
            tier: PreviewTier::Thumbnail,
            interactive: false,
            viewport: "fixture".into(),
            generation: U64(generation),
            foreground: true,
        },
    )? {
        Response::Preview(status) => Ok(status),
        _ => anyhow::bail!("wrong preview request reply"),
    }
}
fn held_status(running: &Running, token: &str, ticket: &str) -> Result<()> {
    let pending = running.bridge.submit(Request::PreviewStatus {
        catalog: token.into(),
        ticket: ticket.into(),
    })?;
    let before = Instant::now();
    let response = pending.receiver.recv_timeout(Duration::from_secs(1))?;
    ensure!(
        matches!(
            response,
            Reply::Ok {
                value: Response::Preview(_)
            }
        ),
        "status failed while F held"
    );
    eprintln!(
        "configured held F C-status latency_ms={}",
        before.elapsed().as_millis()
    );
    Ok(())
}
#[test]
#[ignore = "requires explicitly configured built CLI; held C/G/F/N fixture"]
fn actual_managed_first_and_middle_transfers_keep_actor_cancel_responsive() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    let (temporary, root, originals, key, original) = fixture()?;
    let (warm, token) = Running::start(temporary.clone(), &executable, &root, &originals, false)?;
    let status = warm.ready(&token, &key, 1)?;
    let bytes = warm.bytes(&token, &status.ticket)?;
    let digest = blake3::hash(bytes.bytes());
    ensure!(bytes.bytes().len() > 16 * 1024, "multi-chunk fixture");
    drop(bytes);
    warm.finish(token)?;
    let (running, token) = Running::start(temporary.clone(), &executable, &root, &originals, true)?;
    let result = (|| -> Result<()> {
        // First cache-object chunk, before native admission.
        {
            let mut hold = Hold::install(&running, HeldRead::Object(0));
            let ticket = request(&running, &token, &key, 1)?;
            hold.entered.recv_timeout(Duration::from_secs(30))?;
            held_status(&running, &token, &ticket.ticket)?;
            ensure!(
                running.observed.lock().unwrap().is_empty(),
                "N spawned before encoded input transfer"
            );
            command(
                &running.bridge,
                Request::CancelPreview {
                    catalog: token.clone(),
                    ticket: ticket.ticket,
                },
            )?;
            hold.release();
        }
        // Middle verified RGB chunk after N has completed; C must still admit cancellation.
        {
            let mut hold = Hold::install(&running, HeldRead::Stage(16 * 1024));
            let ticket = request(&running, &token, &key, 2)?;
            hold.entered.recv_timeout(Duration::from_secs(30))?;
            held_status(&running, &token, &ticket.ticket)?;
            command(
                &running.bridge,
                Request::CancelPreview {
                    catalog: token.clone(),
                    ticket: ticket.ticket,
                },
            )?;
            hold.release();
        }
        let ticket = running.ready(&token, &key, 3)?;
        // Ready-ticket encoded delivery is a distinct task and binary envelope.
        {
            let mut hold = Hold::install(&running, HeldRead::Object(16 * 1024));
            let pending =
                running
                    .bridge
                    .preview_bytes(token.clone(), ticket.ticket.clone(), true)?;
            hold.entered.recv_timeout(Duration::from_secs(30))?;
            held_status(&running, &token, &ticket.ticket)?;
            pending.cancellation().cancel();
            hold.release();
            let canceled = pending.receiver.recv_timeout(Duration::from_secs(30))?;
            ensure!(
                matches!(
                    canceled,
                    Err(crate::application::BridgeError {
                        code: crate::application::ErrorCode::Canceled,
                        ..
                    })
                ),
                "byte cancellation category"
            );
        }
        let bytes = running.bytes(&token, &ticket.ticket)?;
        ensure!(
            blake3::hash(bytes.bytes()) == digest,
            "cancellation invalidated or changed cache"
        );
        drop(bytes);
        Ok(())
    })();
    let retirement = running.finish(token);
    result?;
    retirement?;
    ensure!(
        blake3::hash(&std::fs::read(originals.join("original.png"))?)
            .to_hex()
            .as_str()
            == original,
        "original bytes changed"
    );
    Ok(())
}

#[test]
#[ignore = "requires explicitly configured built CLI; actual C/G/F/N codec fixture"]
fn actual_managed_webp_avif_render_and_cold_cache_delivery() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    for (codec, mime) in [
        (crate::preview::Codec::Webp, "image/webp"),
        (crate::preview::Codec::Avif, "image/avif"),
    ] {
        let (temporary, root, originals, key, original) = fixture()?;
        let (running, token) = Running::start_codec(
            temporary.clone(),
            &executable,
            &root,
            &originals,
            false,
            codec,
        )?;
        let status = running.ready(&token, &key, 1)?;
        let bytes = running.bytes(&token, &status.ticket)?;
        ensure!(
            bytes.mime == mime && !bytes.bytes().is_empty(),
            "wrong codec delivery"
        );
        let digest = blake3::hash(bytes.bytes());
        drop(bytes);
        ensure!(
            running
                .observed
                .lock()
                .unwrap()
                .iter()
                .any(|row| row.render),
            "codec cold request did not render in N"
        );
        running.finish(token)?;
        let (running, token) = Running::start_codec(
            temporary.clone(),
            &executable,
            &root,
            &originals,
            true,
            codec,
        )?;
        let status = running.ready(&token, &key, 1)?;
        let bytes = running.bytes(&token, &status.ticket)?;
        ensure!(
            bytes.mime == mime && blake3::hash(bytes.bytes()) == digest,
            "cold codec bytes changed"
        );
        drop(bytes);
        let observed = running.observed.lock().unwrap();
        ensure!(
            !observed.is_empty() && observed.iter().all(|row| !row.render),
            "codec cache did not use DecodeEncoded only"
        );
        drop(observed);
        running.finish(token)?;
        ensure!(
            blake3::hash(&std::fs::read(originals.join("original.png"))?)
                .to_hex()
                .as_str()
                == original,
            "original bytes changed"
        );
        eprintln!(
            "configured codec {codec:?}: Render, N header/decode, encoded delivery and checked C/F/N retirement verified"
        );
    }
    Ok(())
}

#[derive(Clone)]
struct CachedFixtureObject {
    digest: String,
    key: crate::preview::PreviewKey,
    path: PathBuf,
    bytes: i64,
    checksum: String,
    record: String,
}
fn cached_fixture_object(
    root: &Path,
    originals: &Path,
    variant: &VariantKey,
) -> Result<CachedFixtureObject> {
    let cache = root.join("application-previews");
    let manifest = cache.join("manifest");
    let (digest, descriptor, bytes, checksum, record): (String, String, i64, String, String) = {
        let db = rusqlite::Connection::open_with_flags(
            manifest.join("previews.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        db.query_row(
            "SELECT o.key,o.descriptor,o.bytes,o.checksum,r.record FROM wanted w JOIN objects o ON o.key=w.current JOIN render_records r ON r.key=o.key WHERE w.asset=?1 AND w.variant=?2 AND w.tier='thumbnail' AND w.channel='refined' AND o.status='ready'",
            rusqlite::params![variant.asset_id, variant.variant_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?
    };
    let key: crate::preview::PreviewKey = serde_json::from_str(&descriptor)?;
    ensure!(
        key.digest()? == digest,
        "fixture manifest key digest mismatch"
    );
    ensure!(
        key.asset_id == variant.asset_id
            && key.variant_id == variant.variant_id
            && key.tier == crate::preview::Tier::Thumbnail,
        "fixture manifest selected the wrong preview"
    );
    let config =
        crate::preview::PreviewStore::current_configuration(crate::preview::StoreConfig {
            manifest_root: manifest,
            layout: crate::preview::Layout::HashPrefix,
            thumbnail_root: cache.join("thumbnail"),
            large_root: cache.join("large"),
            thumbnail_bytes: 2 * 1024 * 1024 * 1024,
            large_bytes: 8 * 1024 * 1024 * 1024,
        })?;
    let store = crate::preview::PreviewStore::open(config, &[originals.to_owned()])?;
    let path = store.test_object_path(&key)?;
    ensure!(
        path.starts_with(&store.configuration().thumbnail_root),
        "fixture object escaped saved thumbnail root"
    );
    drop(store);
    Ok(CachedFixtureObject {
        digest,
        key,
        path,
        bytes,
        checksum,
        record,
    })
}
fn replace_cached_fixture_object(
    root: &Path,
    object: &CachedFixtureObject,
) -> Result<(i64, String)> {
    let original = std::fs::read(&object.path)?;
    ensure!(
        i64::try_from(original.len())? == object.bytes
            && blake3::hash(&original).to_hex().as_str() == object.checksum,
        "fixture object changed before corruption"
    );
    let codec = object.key.encoding.codec;
    ensure!(original.len() >= 32, "fixture object too small to truncate");
    // Use explicit malformed syntax for JPEG/WebP. Short reads may be reported
    // as an unclassified I/O error and cannot authorize cache invalidation.
    let mut malformed = original.clone();
    match codec {
        crate::preview::Codec::Jpeg => {
            let dqt = original
                .windows(2)
                .position(|bytes| bytes == [0xff, 0xdb])
                .context("fixture JPEG has no quantization table")?;
            let info = malformed
                .get_mut(dqt + 4)
                .context("fixture DQT is incomplete")?;
            *info = (*info & 0xf0) | 0x0f; // Invalid table index: only 0..=3 exist.
        }
        crate::preview::Codec::Webp => {
            let vp8 = original
                .windows(4)
                .position(|bytes| bytes == b"VP8 ")
                .context("fixture WebP has no lossy VP8 chunk")?;
            ensure!(
                original.get(vp8 + 11..vp8 + 14) == Some(&[0x9d, 0x01, 0x2a]),
                "fixture VP8 key-frame magic missing"
            );
            malformed[vp8 + 11] ^= 1;
        }
        crate::preview::Codec::Avif => malformed.truncate(original.len() / 2),
    }
    match codec {
        crate::preview::Codec::Jpeg => ensure!(
            malformed.starts_with(&[0xff, 0xd8]),
            "malformed JPEG lost its codec marker"
        ),
        crate::preview::Codec::Webp => ensure!(
            malformed.starts_with(b"RIFF") && malformed.get(8..12) == Some(b"WEBP"),
            "malformed WebP lost its codec marker"
        ),
        crate::preview::Codec::Avif => ensure!(
            malformed.get(4..8) == Some(b"ftyp"),
            "truncated AVIF lost its codec marker"
        ),
    }
    let bytes = i64::try_from(malformed.len())?;
    let checksum = blake3::hash(&malformed).to_hex().to_string();
    std::fs::write(&object.path, &malformed)?;
    let mut db =
        rusqlite::Connection::open(root.join("application-previews/manifest/previews.sqlite3"))?;
    let transaction = db.transaction()?;
    ensure!(
        transaction.execute(
            "UPDATE objects SET bytes=?1,checksum=?2 WHERE key=?3 AND status='ready' AND bytes=?4 AND checksum=?5",
            rusqlite::params![bytes, checksum, object.digest, object.bytes, object.checksum],
        )? == 1,
        "fixture object manifest changed before corruption"
    );
    ensure!(
        transaction.execute(
            "UPDATE usage SET bytes=bytes-?1+?2 WHERE tier='thumbnail'",
            rusqlite::params![object.bytes, bytes],
        )? == 1,
        "fixture thumbnail usage row missing"
    );
    let retained: String = transaction.query_row(
        "SELECT record FROM render_records WHERE key=?1",
        [&object.digest],
        |row| row.get(0),
    )?;
    ensure!(retained == object.record, "fixture render record changed");
    transaction.commit()?;
    ensure!(
        std::fs::metadata(&object.path)?.len() == u64::try_from(bytes)?
            && blake3::hash(&std::fs::read(&object.path)?)
                .to_hex()
                .as_str()
                == checksum,
        "fixture malformed object read-back mismatch"
    );
    Ok((bytes, checksum))
}
fn failed_status(
    running: &Running,
    token: &str,
    key: &VariantKey,
    generation: u64,
) -> Result<PreviewStatus> {
    let mut status = request(running, token, key, generation)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while matches!(status.state, PreviewState::Queued) {
        ensure!(Instant::now() < deadline, "corrupt preview failure timeout");
        thread::sleep(Duration::from_millis(5));
        let Response::Preview(next) = command(
            &running.bridge,
            Request::PreviewStatus {
                catalog: token.into(),
                ticket: status.ticket,
            },
        )?
        else {
            anyhow::bail!("wrong corrupt status reply")
        };
        status = next;
    }
    ensure!(
        matches!(status.state, PreviewState::Failed) && status.message.is_some(),
        "malformed cached preview was not an authoritative failure: {:?} {:?}",
        status.state,
        status.message
    );
    Ok(status)
}
fn verify_exact_corrupt_invalidation(
    root: &Path,
    bad: &CachedFixtureObject,
    healthy: &CachedFixtureObject,
    malformed_bytes: i64,
    malformed_checksum: &str,
) -> Result<()> {
    let db = rusqlite::Connection::open_with_flags(
        root.join("application-previews/manifest/previews.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (bad_status, bad_bytes, bad_checksum): (String, i64, String) = db.query_row(
        "SELECT status,bytes,checksum FROM objects WHERE key=?1",
        [&bad.digest],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    ensure!(
        bad_status == "orphan"
            && bad_bytes == malformed_bytes
            && bad_checksum == malformed_checksum,
        "typed corruption did not orphan the exact malformed object: status={bad_status}, bytes={bad_bytes}, checksum={bad_checksum}"
    );
    let bad_published: i64 = db.query_row(
        "SELECT count(*) FROM render_records r JOIN objects o ON o.key=r.key WHERE r.key=?1 AND o.status='ready'",
        [&bad.digest],
        |row| row.get(0),
    )?;
    ensure!(
        bad_published == 0,
        "corrupt render record remained published"
    );
    let (healthy_status, healthy_bytes, healthy_checksum, healthy_record):
        (String, i64, String, String) = db.query_row(
            "SELECT o.status,o.bytes,o.checksum,r.record FROM objects o JOIN render_records r ON r.key=o.key WHERE o.key=?1",
            [&healthy.digest],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    ensure!(
        healthy_status == "ready"
            && healthy_bytes == healthy.bytes
            && healthy_checksum == healthy.checksum
            && healthy_record == healthy.record,
        "healthy sibling object or record changed"
    );
    ensure!(
        std::fs::metadata(&healthy.path)?.len() == u64::try_from(healthy.bytes)?
            && blake3::hash(&std::fs::read(&healthy.path)?)
                .to_hex()
                .as_str()
                == healthy.checksum,
        "healthy sibling payload changed"
    );
    Ok(())
}

#[test]
#[ignore = "requires explicitly configured built CLI; actual corrupt-cache G/C/F/N fixture"]
fn actual_managed_jpeg_webp_avif_corruption_is_typed_and_exactly_invalidated() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    for codec in [
        crate::preview::Codec::Jpeg,
        crate::preview::Codec::Webp,
        crate::preview::Codec::Avif,
    ] {
        let (temporary, root, originals, bad_variant, original_checksum) = fixture()?;
        let healthy_variant = {
            let mut catalog = crate::Catalog::open(&root)?;
            let revision = catalog.edit_variant(&bad_variant)?.revision;
            let created = catalog.create_edit_variant(
                &bad_variant,
                revision,
                &format!("healthy-{codec:?}"),
            )?;
            catalog
                .save_edit_recipe(
                    &created.key,
                    created.revision,
                    &crate::edit::Recipe::V1(crate::edit::RecipeV1 {
                        exposure_ev: 0.25,
                        ..Default::default()
                    }),
                )?
                .key
        };
        let (warm, token) = Running::start_codec(
            temporary.clone(),
            &executable,
            &root,
            &originals,
            false,
            codec,
        )?;
        let bad_ready = warm.ready(&token, &bad_variant, 1)?;
        let bad_bytes = warm.bytes(&token, &bad_ready.ticket)?;
        ensure!(!bad_bytes.bytes().is_empty(), "bad fixture preview empty");
        drop(bad_bytes);
        let healthy_ready = warm.ready(&token, &healthy_variant, 2)?;
        let healthy_bytes = warm.bytes(&token, &healthy_ready.ticket)?;
        let healthy_delivery = blake3::hash(healthy_bytes.bytes());
        drop(healthy_bytes);
        let warm_observed = warm.observed.lock().unwrap();
        ensure!(
            warm_observed.iter().filter(|row| row.render).count() == 2,
            "fixture generation did not render both cache objects in N"
        );
        drop(warm_observed);
        warm.finish(token)?;

        let bad = cached_fixture_object(&root, &originals, &bad_variant)?;
        let healthy = cached_fixture_object(&root, &originals, &healthy_variant)?;
        ensure!(bad.digest != healthy.digest, "fixture cache keys collided");
        ensure!(
            bad.key.encoding.codec == codec && healthy.key.encoding.codec == codec,
            "fixture codec descriptor mismatch"
        );
        let (malformed_bytes, malformed_checksum) = replace_cached_fixture_object(&root, &bad)?;

        let (cold, token) = Running::start_codec(
            temporary.clone(),
            &executable,
            &root,
            &originals,
            true,
            codec,
        )?;
        let failed = failed_status(&cold, &token, &bad_variant, 10)?;
        eprintln!(
            "configured {codec:?} malformed-object failure: {:?}",
            failed.message
        );
        {
            let observed = cold.observed.lock().unwrap();
            ensure!(
                observed.len() == 1 && !observed[0].render,
                "malformed cache did not fail in exactly one DecodeEncoded N"
            );
        }
        verify_exact_corrupt_invalidation(
            &root,
            &bad,
            &healthy,
            malformed_bytes,
            &malformed_checksum,
        )?;
        let sibling = cold.ready(&token, &healthy_variant, 11)?;
        let sibling_bytes = cold.bytes(&token, &sibling.ticket)?;
        ensure!(
            blake3::hash(sibling_bytes.bytes()) == healthy_delivery,
            "healthy sibling delivery changed after corruption"
        );
        drop(sibling_bytes);
        ensure!(
            cold.observed.lock().unwrap().iter().all(|row| !row.render),
            "corruption route unexpectedly rendered from original"
        );
        eprintln!(
            "configured codec {codec:?}: malformed bytes={} passed F checksum, DecodeEncoded N returned authoritative Corrupt ({:?}), exact object/record invalidated, healthy sibling preserved",
            malformed_bytes, failed.message
        );
        cold.finish(token)?;
        ensure!(
            blake3::hash(&std::fs::read(originals.join("original.png"))?)
                .to_hex()
                .as_str()
                == original_checksum,
            "codec corruption fixture changed original content"
        );
    }
    Ok(())
}

#[cfg(unix)]
mod abnormal_tests;

#[cfg(unix)]
#[test]
#[ignore = "configured actual C/F metadata allowance lifetime"]
fn actual_managed_metadata_is_held_through_close_until_checked_wait() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    let (temporary, root, originals, _, checksum) = fixture()?;
    let (started, gate) = crate::application::desktop::process::test_reap::armed(|| {
        Running::start(temporary.clone(), &executable, &root, &originals, true)
    });
    let (mut running, token) = match started {
        Ok(value) => value,
        Err(error) => {
            drop(gate);
            eprintln!("metadata fixture retained at {:?}", temporary.path());
            std::mem::forget(temporary.clone());
            return Err(error);
        }
    };
    let pool = running.metadata.clone();
    let charge = pool.used();
    let mut shutdown = None;
    let result = (|| -> Result<()> {
        ensure!(charge > 0, "managed startup did not reserve metadata");
        command(&running.bridge, Request::Close { catalog: token })?;
        ensure!(
            pool.used() == charge,
            "catalog close released live C metadata"
        );
        let bridge = running.bridge.clone();
        shutdown = Some(thread::spawn(move || bridge.try_shutdown()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !running.bridge.0.shared.state.lock().unwrap().stopping {
            ensure!(Instant::now() < deadline, "shutdown never started");
            thread::sleep(Duration::from_millis(2));
        }
        ensure!(
            !running.bridge.0.shared.state.lock().unwrap().reaped,
            "wait gate bypassed"
        );
        ensure!(
            pool.used() == charge && pool.try_reserve(1).is_none(),
            "unwaited C released admission"
        );
        Ok(())
    })();
    // Always release the test gate and join shutdown before touching fixture files.
    drop(gate);
    let retired = (|| -> Result<()> {
        if let Some(shutdown) = shutdown {
            shutdown
                .join()
                .map_err(|_| anyhow::anyhow!("shutdown thread panicked"))??;
        } else {
            running.bridge.try_shutdown()?;
        }
        let state = running.bridge.0.shared.state.lock().unwrap();
        ensure!(
            state.reaped && state.child_finished && state.filesystem_verified,
            "C/F retirement unverified"
        );
        drop(state);
        ensure!(
            pool.used() == charge && pool.try_reserve(1).is_none(),
            "live parent relay backing released after wait"
        );
        running.parent.finish_after_dependents(false)?;
        ensure!(
            blake3::hash(&std::fs::read(originals.join("original.png"))?)
                .to_hex()
                .to_string()
                == checksum,
            "synthetic original changed"
        );
        running.mark_externally_retired();
        Ok(())
    })();
    if result.is_err() || retired.is_err() {
        eprintln!(
            "metadata fixture retained at {:?}; assertions={result:?}; retirement={retired:?}",
            temporary.path()
        );
        std::mem::forget(temporary.clone());
        return result.and(retired);
    }
    drop(running);
    ensure!(
        pool.used() == 0,
        "final parent owners did not release allowance"
    );
    let retry = pool.try_reserve(charge).context("same-pool retry denied")?;
    drop(retry);
    eprintln!(
        "metadata charge={charge}; held through catalog close, blocked OS wait and surviving parent; same-pool retry and C/F retirement passed"
    );
    Ok(())
}

fn managed_export_metadata_jobs(
    temporary: &Path,
    root: &Path,
    key: &VariantKey,
) -> Result<Vec<ManagedExportMetadataJob>> {
    use crate::catalog_session::export_stage as s;
    let mut catalog = crate::Catalog::open(root)?;
    let metadata_revision = catalog.image_metadata_identity(key)?.metadata_revision;
    let mut max_icc = lcms2::Profile::new_srgb().icc()?;
    max_icc.resize(s::BLOB_BYTES as usize, 0);
    max_icc[..4].copy_from_slice(&(s::BLOB_BYTES as u32).to_be_bytes());
    // A valid profile with unused trailing storage, admitted by LittleCMS.
    lcms2::Profile::new_icc(&max_icc)?;
    let packet = String::from_utf8(crate::xmp::empty_packet()?)?;
    let split = packet.rfind("<?xpacket end").unwrap_or(packet.len());
    let mut max_xmp = packet.as_bytes()[..split].to_vec();
    max_xmp.resize(s::BLOB_BYTES as usize - (packet.len() - split), b' ');
    max_xmp.extend_from_slice(&packet.as_bytes()[split..]);
    crate::xmp::parse(&max_xmp)
        .with_context(|| format!("both-max selected XMP parse: input bytes {}", max_xmp.len()))?;
    let fields = |icc: Option<&[u8]>, linear: bool| crate::xmp::DerivativeFields {
        width: 256,
        height: 256,
        channels: 4,
        bits_per_sample: 8,
        mime_type: "image/png".into(),
        profile_name: if let Some(icc) = icc {
            format!("ICC BLAKE3 {}", blake3::hash(icc).to_hex())
        } else if linear {
            "linear sRGB".into()
        } else {
            "sRGB IEC61966-2.1".into()
        },
        is_srgb: icc.is_none() && !linear,
    };
    // RDF literals survive derivative policy; packet padding does not.
    // Size the retained literal against the exact PNG/sRGB technical fields.
    const PAYLOAD_NS: &str = "urn:lensworks:managed-export-fixture";
    let packet_with_literal = |literal: &str| {
        format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:fixture=\"{PAYLOAD_NS}\"><fixture:payload>{literal}</fixture:payload></rdf:Description></rdf:RDF></x:xmpmeta>"
    ).into_bytes()
    };
    // The SDK's compact form writes this simple property as an attribute:
    // each quote becomes &quot;. Its canonical element form keeps one byte.
    // This gives the bounded per-property assembly 80 KiB of headroom before
    // compact output reaches 16 MiB, without changing the retained RDF value.
    let quoted_prefix = "\"".repeat(16 * 1024);
    let seed = packet_with_literal(&format!("{quoted_prefix}A"));
    let seed_derivative = crate::xmp::rendered_derivative(&seed, &fields(None, false))
        .with_context(|| {
            format!(
                "maximum derivative seed: source bytes {}, quote bytes {}",
                seed.len(),
                quoted_prefix.len()
            )
        })?;
    let ascii_bytes = (s::BLOB_BYTES as usize)
        .checked_sub(seed_derivative.len())
        .context("maximum derivative fixture overhead exceeds blob limit")?
        + 1;
    let literal = format!("{quoted_prefix}{}", "A".repeat(ascii_bytes));
    let max_output_xmp = packet_with_literal(&literal);
    ensure!(
        max_output_xmp.len() <= s::BLOB_BYTES as usize,
        "maximum derivative source is {} bytes; input limit {}",
        max_output_xmp.len(),
        s::BLOB_BYTES
    );
    let source_model = crate::xmp::parse(&max_output_xmp)
        .with_context(|| format!("maximum derivative source parse: input bytes {}, literal bytes {}, seed output bytes {}", max_output_xmp.len(), literal.len(), seed_derivative.len()))?;
    let canonical_source_bytes = crate::xmp::canonical(&source_model)
        .with_context(|| {
            format!(
                "maximum derivative source canonicalization: input bytes {}",
                max_output_xmp.len()
            )
        })?
        .len();
    eprintln!(
        "maximum XMP fixture: source bytes {}, canonical source bytes {}, quote bytes {}, literal bytes {}, seed output bytes {}, expected compact derivative bytes {}",
        max_output_xmp.len(),
        canonical_source_bytes,
        quoted_prefix.len(),
        literal.len(),
        seed_derivative.len(),
        s::BLOB_BYTES
    );
    let maximum_derivative =
        crate::xmp::rendered_derivative(&max_output_xmp, &fields(None, false))
            .with_context(|| format!("maximum derivative including bounded fragment assembly: source bytes {}, canonical source bytes {}, literal bytes {}, quote bytes {}, expected compact output bytes {}", max_output_xmp.len(), canonical_source_bytes, literal.len(), quoted_prefix.len(), s::BLOB_BYTES))?;
    ensure!(
        maximum_derivative.len() == s::BLOB_BYTES as usize,
        "maximum derivative sizing: actual {} expected {}",
        maximum_derivative.len(),
        s::BLOB_BYTES
    );
    let derivative_xml = std::str::from_utf8(&maximum_derivative)?;
    let derivative_doc = roxmltree::Document::parse(derivative_xml)?;
    let retained = derivative_doc
        .descendants()
        .find_map(|node| node.attribute((PAYLOAD_NS, "payload")))
        .or_else(|| {
            derivative_doc
                .descendants()
                .find(|node| node.has_tag_name((PAYLOAD_NS, "payload")))
                .and_then(|node| node.text())
        })
        .context("maximum derivative lost unknown RDF literal")?;
    ensure!(
        retained.as_bytes() == literal.as_bytes(),
        "retained RDF literal: actual bytes {} hash {}; expected bytes {} hash {}",
        retained.len(),
        blake3::hash(retained.as_bytes()),
        literal.len(),
        blake3::hash(literal.as_bytes())
    );
    let small_icc = lcms2::Profile::new_srgb().icc()?;
    let small_xmp = crate::xmp::empty_packet()?;
    let mut jobs = Vec::new();
    for (name, icc, xmp, linear) in [
        (
            "replay",
            Some(small_icc.as_slice()),
            Some(small_xmp.as_slice()),
            false,
        ),
        ("srgb", None, None, false),
        ("linear", None, None, true),
        ("max-icc", Some(max_icc.as_slice()), None, false),
        ("max-xmp", None, Some(max_output_xmp.as_slice()), false),
        (
            "both-max",
            Some(max_icc.as_slice()),
            Some(max_xmp.as_slice()),
            false,
        ),
    ] {
        let destination = temporary.join(format!("paired-{name}.png"));
        let job = catalog.begin_photo_export()?;
        catalog.append_photo_export(
            &job.id,
            0,
            &crate::catalog_exports::ExportTarget {
                key: key.clone(),
                expected_revision: 0,
                destination: destination.clone(),
                overwrite: false,
                metadata: if xmp.is_some() {
                    crate::catalog_exports::MetadataSelection::Resolved {
                        expected_revision: metadata_revision,
                        base_model: None,
                    }
                } else {
                    crate::catalog_exports::MetadataSelection::Omit
                },
            },
            &crate::image_export::OutputSpec {
                size: crate::image_export::OutputSize::Original,
                format: crate::image_export::OutputFormat::Png {
                    depth: crate::image_export::IntegerDepth::Eight,
                },
                profile: crate::image_export::OutputProfile::Srgb,
                alpha: crate::image_export::AlphaPolicy::Preserve,
            },
            4 * 1024 * 1024,
            64 * 1024 * 1024,
        )?;
        // Preserve exact transport inputs; rendering subsequently applies
        // the specified derivative metadata policy.
        crate::export_service::managed_test_set_blobs(&mut catalog, &job.id, icc, xmp, linear)?;
        catalog.seal_photo_export_job(&job.id, 1)?;
        let selected = xmp.unwrap_or(&small_xmp);
        let expected_xmp = crate::xmp::rendered_derivative(selected, &fields(icc, linear))
            .with_context(|| {
                format!(
                    "{name}: expected PNG derivative from {} selected XMP bytes",
                    selected.len()
                )
            })?;
        // These packets have no safe EXIF source fields. The existing
        // aggregate metadata limit therefore admits a full 16 MiB XMP.
        crate::image_export::ResolvedExportMetadata {
            xmp: Some(String::from_utf8(expected_xmp.clone())?),
            exif: crate::image_export::SafeExif::default(),
        }
        .validate(s::BLOB_BYTES)
        .with_context(|| {
            format!(
                "{name}: derived metadata admission: XMP bytes {}, safe EXIF bytes 0, cap {}",
                expected_xmp.len(),
                s::BLOB_BYTES
            )
        })?;
        jobs.push((job.id, destination, name, expected_xmp));
    }
    Ok(jobs)
}

#[test]
fn managed_export_max_metadata_fixture_matches_derivative_limits() -> Result<()> {
    let (temporary, root, _originals, key, _digest) = fixture()?;
    // Exercise the same codec-valid packets, derivative sizing, metadata-cap
    // admission and catalog plans that the actual paired fixture consumes.
    managed_export_metadata_jobs(temporary.path(), &root, &key)?;
    Ok(())
}

#[test]
#[ignore = "requires explicitly configured built CLI; actual paired C/G/F import and reopen fixture"]
fn actual_empty_config_actor_import_close_reopen_restores_export_authority() -> Result<()> {
    use crate::application::ImportPhase;
    use crate::catalog_session::InspectExportOriginal;

    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let temporary = Arc::new(tempfile::tempdir()?);
    let base = temporary.path().canonicalize()?;
    let root = base.join("catalog");
    let originals = base.join("selected-originals");
    std::fs::create_dir(&originals)?;
    let original = originals.join("selected.png");
    image::RgbImage::from_pixel(16, 12, image::Rgb([30u8, 60, 90])).save(&original)?;
    drop(crate::Catalog::open(&root)?);

    let options = |temporary: Arc<tempfile::TempDir>| ExportFixtureOptions {
        temporary,
        executable: &executable,
        root: &root,
        originals: &originals,
        small: false,
        codec: crate::preview::Codec::Jpeg,
        workers: 1,
        export_executable: &executable,
        worker_bytes: 64 * 1024 * 1024,
        configured_originals: false,
    };
    let (first, token) = Running::start_export_options(options(temporary.clone()))?;
    let Response::Import(Some(started)) = command(
        &first.bridge,
        Request::ImportStart {
            catalog: token.clone(),
            source: NativePath::from_path(&originals),
        },
    )?
    else {
        anyhow::bail!("managed import did not start")
    };
    ensure!(
        started.source == NativePath::from_path(&originals),
        "F did not return the selected canonical root"
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let completed = loop {
        let Response::Import(Some(status)) = command(
            &first.bridge,
            Request::ImportStatus {
                catalog: token.clone(),
            },
        )?
        else {
            anyhow::bail!("managed import status disappeared")
        };
        if status.phase == ImportPhase::Complete {
            break status;
        }
        ensure!(
            !matches!(status.phase, ImportPhase::Failed | ImportPhase::Canceled),
            "managed import ended {:?}: {:?}",
            status.phase,
            status.error
        );
        ensure!(Instant::now() < deadline, "managed import timed out");
        std::thread::sleep(Duration::from_millis(2));
    };
    ensure!(completed.imported.0 == 1, "managed import count changed");
    first.finish(token)?;
    ensure!(
        crate::Catalog::open(&root)?.browse(0, 10)?.len() == 1,
        "managed actor did not commit the selected image"
    );

    let parked = base.join("selected-originals-offline");
    std::fs::rename(&originals, &parked)?;
    let (second, token) = Running::start_export_options(options(temporary.clone()))?;
    let restored = second.restored_roots.lock().unwrap().clone();
    ensure!(
        restored.len() == 1,
        "managed reopen did not restore exactly one persisted original root"
    );
    std::fs::rename(&parked, &originals)?;
    second.client.inspect_export_original(
        &InspectExportOriginal {
            root: restored[0].clone(),
            requested: NativePath::from_path(&original),
            allowance: U64(std::fs::metadata(&original)?.len()),
        },
        &std::sync::atomic::AtomicBool::new(false),
    )?;
    second.finish(token)?;
    Ok(())
}

#[test]
#[ignore = "requires explicitly configured built CLI; actual paired C/G/F/N export fixture"]
fn actual_managed_export_replays_stage_and_native_acknowledgements_and_reuses_preview_pool()
-> Result<()> {
    use crate::application::exports as x;
    use crate::catalog_session::{export_executor as e, export_native as n, export_stage as s};
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let (temporary, root, originals, key, original_digest) = fixture()?;
    let jobs = managed_export_metadata_jobs(temporary.path(), &root, &key)?;
    let (running, token) = Running::start_export_options(ExportFixtureOptions {
        temporary: temporary.clone(),
        executable: &executable,
        root: &root,
        originals: &originals,
        small: false,
        codec: crate::preview::Codec::Jpeg,
        workers: 1,
        export_executable: &executable,
        worker_bytes: 512 * 1024 * 1024,
        configured_originals: true,
    })?;
    let preview = running.ready(&token, &key, 1)?;
    let preview_bytes = running.bytes(&token, &preview.ticket)?;
    ensure!(
        !preview_bytes.bytes().is_empty(),
        "real paired preview bytes"
    );
    drop(preview_bytes);
    let observed: Arc<Mutex<Vec<ExportObserverRecord>>> = Default::default();
    let dropped: Arc<Mutex<std::collections::BTreeSet<String>>> = Default::default();
    let records = observed.clone();
    let losses = dropped.clone();
    let native_ids: Arc<Mutex<Vec<u64>>> = Default::default();
    let identities = native_ids.clone();
    let native_pool = running.native.clone();
    let shared_pool_probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probes = shared_pool_probes.clone();
    let prior = running.parent.observer.lock().unwrap().clone();
    *running.parent.observer.lock().unwrap() = Some(Arc::new(move |call, after| {
        if let Some(prior) = &prior {
            prior(call, after)?;
        }
        if !after {
            return Ok(());
        }
        if let Call::ExportNative(request) = call
            && matches!(request.action, n::Action::Register { .. })
        {
            identities.lock().unwrap().push(request.operation.0);
            let (limit, used) = native_pool.snapshot();
            ensure!(
                used > 0 && native_pool.try_reserve(limit).is_none(),
                "registered export must occupy the actual shared pool"
            );
            probes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let action = match call {
            Call::ExportNative(request) => match request.action {
                n::Action::Register { .. } => Some(("register", request.digest()?)),
                n::Action::Spawn => Some(("spawn", request.digest()?)),
                n::Action::Start => Some(("start", request.digest()?)),
                _ => None,
            },
            Call::ExportStage(request) => {
                let label = match request.action {
                    s::Action::Begin { .. } => "begin",
                    s::Action::UploadIcc { .. } => "icc",
                    s::Action::UploadXmp { .. } => "xmp",
                    s::Action::Ready { .. } => "ready",
                    s::Action::ResultAndSeal => "seal",
                    s::Action::Release => "release",
                    _ => "other",
                };
                Some((label, request.digest()?))
            }
            Call::ExportExecutor(request) if matches!(request.action, e::Action::Release) => {
                Some((
                    "close",
                    *blake3::hash(&serde_json::to_vec(request)?).as_bytes(),
                ))
            }
            _ => None,
        };
        if let Some((label, digest)) = action {
            records.lock().unwrap().push((label.to_owned(), digest));
            if losses.lock().unwrap().insert(label.to_owned()) {
                return Err(crate::filesystem_worker::wire::Failure::new(
                    crate::filesystem_worker::wire::FailureKind::Unknown,
                    format!("injected lost paired {label} acknowledgement"),
                )
                .into());
            }
        }
        Ok(())
    }));
    let export = |request: x::Request| -> Result<x::Response> {
        match command(
            &running.bridge,
            Request::Export {
                catalog: token.clone(),
                request: Box::new(request),
            },
        )? {
            Response::Export(value) => Ok(*value),
            _ => anyhow::bail!("wrong paired export response"),
        }
    };
    let x::Response::Options(options) = export(x::Request::Options)? else {
        anyhow::bail!("missing export options")
    };
    let mut limits = options.execution;
    limits.render.decode.max_allocation_bytes = U64(32 * 1024 * 1024);
    limits.render.render.max_allocation_bytes = U64(32 * 1024 * 1024);
    limits.render.encode.render.max_allocation_bytes = U64(32 * 1024 * 1024);
    let wait = |id: String| -> Result<x::Operation> {
        let deadline = Instant::now() + Duration::from_secs(240);
        loop {
            let x::Response::Operation(Some(operation)) = export(x::Request::Status {
                operation: Some(id.clone()),
            })?
            else {
                anyhow::bail!("missing paired export operation")
            };
            if ["complete", "failed", "canceled", "paused"].contains(&operation.phase.as_str()) {
                return Ok(operation);
            }
            ensure!(
                Instant::now() < deadline,
                "paired export deadline: {operation:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    };
    let x::Response::Operation(Some(recover)) = export(x::Request::Recover {
        directories: U64(32),
        limits: Some(limits.clone()),
    })?
    else {
        anyhow::bail!("missing recovery operation")
    };
    let recovered = wait(recover.id)?;
    ensure!(
        recovered.phase == "complete",
        "paired recovery: {recovered:?}"
    );
    for (job, destination, name, expected_xmp) in &jobs {
        let x::Response::Operation(Some(run)) = export(x::Request::Run {
            job: job.clone(),
            limits: Some(limits.clone()),
            max_items: U64(1),
            max_seconds: U64(240),
        })?
        else {
            anyhow::bail!("missing Run operation")
        };
        let completed = wait(run.id)?;
        ensure!(
            completed.phase == "complete" && completed.processed == U64(1),
            "paired {name}: {completed:?}"
        );
        ensure!(
            image::open(destination)?.width() == 256,
            "{name}: actual encoded export must decode"
        );
        let decoder = png::Decoder::new_with_limits(
            std::io::BufReader::new(std::fs::File::open(destination)?),
            png::Limits {
                bytes: 128 * 1024 * 1024,
            },
        );
        let reader = decoder.read_info()?;
        if matches!(*name, "max-icc" | "both-max") {
            ensure!(
                reader
                    .info()
                    .icc_profile
                    .as_ref()
                    .is_some_and(|icc| icc.len() == s::BLOB_BYTES as usize),
                "{name}: full ICC retained in PNG"
            );
        }
        if matches!(*name, "max-xmp" | "both-max") {
            let xmp = reader
                .info()
                .utf8_text
                .iter()
                .find(|text| text.keyword == "XML:com.adobe.xmp")
                .context("PNG XMP missing")?
                .get_text()?;
            ensure!(
                xmp.as_bytes() == expected_xmp.as_slice(),
                "{name}: PNG derivative XMP actual bytes {} hash {}; expected bytes {} hash {}",
                xmp.len(),
                blake3::hash(xmp.as_bytes()),
                expected_xmp.len(),
                blake3::hash(expected_xmp)
            );
            if *name == "max-xmp" {
                ensure!(
                    xmp.len() == s::BLOB_BYTES as usize,
                    "{name}: encoded maximum derivative actual {} expected {}",
                    xmp.len(),
                    s::BLOB_BYTES
                );
            }
            crate::xmp::parse(xmp.as_bytes())?;
        }
        drop(reader);
        ensure!(
            running.native.used() == 0,
            "{name}: Retire releases shared pool"
        );
        let full = running
            .native
            .try_reserve(running.native.snapshot().0)
            .context("full shared pool unavailable after checked Retire")?;
        drop(full);
    }
    ensure!(
        shared_pool_probes.load(std::sync::atomic::Ordering::Relaxed) >= jobs.len(),
        "every actual Register charged shared pool"
    );
    ensure!(
        blake3::hash(&std::fs::read(originals.join("original.png"))?)
            .to_hex()
            .as_str()
            == original_digest,
        "original changed"
    );
    ensure!(
        running.native.used() == 0,
        "export retirement must release shared native pool"
    );
    let following = running.ready(&token, &key, 2)?;
    ensure!(
        matches!(following.state, PreviewState::Ready),
        "preview reuses pool after export"
    );
    let mut ids = native_ids.lock().unwrap().clone();
    ensure!(
        ids.windows(2).all(|pair| pair[0] <= pair[1]),
        "native identity regressed across jobs"
    );
    ids.dedup();
    ensure!(
        ids.len() == jobs.len() && ids.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "one contiguous native identity per successful export"
    );
    running.finish(token)?;
    let rows = observed.lock().unwrap();
    for label in [
        "register", "begin", "icc", "xmp", "ready", "spawn", "start", "seal", "release", "close",
    ] {
        let calls: Vec<_> = rows
            .iter()
            .filter(|(name, _)| name == label)
            .map(|(_, digest)| digest)
            .collect();
        let mut counts = std::collections::HashMap::new();
        for digest in calls {
            *counts.entry(*digest).or_insert(0usize) += 1;
        }
        ensure!(
            counts.values().filter(|n| **n == 2).count() == 1 && counts.values().all(|n| *n <= 2),
            "{label}: one lost acknowledgement must yield one exact duplicate, with all other operations occurring once"
        );
    }
    ensure!(
        dropped.lock().unwrap().len() == 10,
        "all ten acknowledgement faults exercised"
    );
    Ok(())
}

#[test]
#[ignore = "requires configured built CLI; actual paired C/G/F failed native launch"]
fn actual_managed_export_failed_launch_drains_releases_and_restores_preview_capacity() -> Result<()>
{
    use crate::application::exports as x;
    use crate::catalog_session::export_native as n;
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    let (temporary, root, originals, key, _) = fixture()?;
    let destination = temporary.path().join("failed-launch.png");
    let job = {
        let mut catalog = crate::Catalog::open(&root)?;
        let job = catalog.begin_photo_export()?;
        catalog.append_photo_export(
            &job.id,
            0,
            &crate::catalog_exports::ExportTarget {
                key: key.clone(),
                expected_revision: 0,
                destination: destination.clone(),
                overwrite: false,
                metadata: crate::catalog_exports::MetadataSelection::Omit,
            },
            &crate::image_export::OutputSpec {
                size: crate::image_export::OutputSize::Original,
                format: crate::image_export::OutputFormat::Png {
                    depth: crate::image_export::IntegerDepth::Eight,
                },
                profile: crate::image_export::OutputProfile::Srgb,
                alpha: crate::image_export::AlphaPolicy::Preserve,
            },
            4 * 1024 * 1024,
            4 * 1024 * 1024,
        )?;
        catalog.seal_photo_export_job(&job.id, 1)?;
        job.id
    };
    let export_executable = temporary.path().join("absent-export-native");
    let (running, token) = Running::start_export_options(ExportFixtureOptions {
        temporary: temporary.clone(),
        executable: &executable,
        root: &root,
        originals: &originals,
        small: false,
        codec: crate::preview::Codec::Jpeg,
        workers: 1,
        export_executable: &export_executable,
        worker_bytes: 64 * 1024 * 1024,
        configured_originals: true,
    })?;
    running.ready(&token, &key, 1)?;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    let prior = running.parent.observer.lock().unwrap().clone();
    *running.parent.observer.lock().unwrap() = Some(Arc::new(move |call, after| {
        if let Some(prior) = &prior {
            prior(call, after)?;
        }
        if after && let Call::ExportNative(request) = call {
            observed.lock().unwrap().push(request.action.clone());
        }
        Ok(())
    }));
    let export = |request| -> Result<x::Response> {
        match command(
            &running.bridge,
            Request::Export {
                catalog: token.clone(),
                request: Box::new(request),
            },
        )? {
            Response::Export(value) => Ok(*value),
            _ => anyhow::bail!("wrong export response"),
        }
    };
    let wait = |id: String| -> Result<x::Operation> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let x::Response::Operation(Some(operation)) = export(x::Request::Status {
                operation: Some(id.clone()),
            })?
            else {
                anyhow::bail!("missing operation")
            };
            if ["complete", "failed", "canceled", "paused"].contains(&operation.phase.as_str()) {
                return Ok(operation);
            }
            ensure!(
                Instant::now() < deadline,
                "failed-launch operation deadline"
            );
            thread::sleep(Duration::from_millis(5));
        }
    };
    let x::Response::Options(options) = export(x::Request::Options)? else {
        anyhow::bail!("options missing")
    };
    let mut limits = options.execution;
    limits.render.decode.max_allocation_bytes = U64(32 * 1024 * 1024);
    limits.render.render.max_allocation_bytes = U64(32 * 1024 * 1024);
    limits.render.encode.render.max_allocation_bytes = U64(32 * 1024 * 1024);
    let x::Response::Operation(Some(recover)) = export(x::Request::Recover {
        directories: U64(32),
        limits: Some(limits.clone()),
    })?
    else {
        anyhow::bail!("recovery missing")
    };
    ensure!(wait(recover.id)?.phase == "complete", "recovery failed");
    let x::Response::Operation(Some(run)) = export(x::Request::Run {
        job: job.clone(),
        limits: Some(limits),
        max_items: U64(1),
        max_seconds: U64(60),
    })?
    else {
        anyhow::bail!("run missing")
    };
    let operation = wait(run.id)?;
    ensure!(
        operation.processed == U64(1),
        "failed launch did not settle: {operation:?}"
    );
    ensure!(
        !destination.exists() && running.native.used() == 0,
        "failed launch must not publish or retain shared reservation"
    );
    let requests = calls.lock().unwrap();
    ensure!(
        requests
            .iter()
            .filter(|r| matches!(r, n::Action::Spawn))
            .count()
            == 1,
        "one attempted native launch"
    );
    ensure!(
        !requests.iter().any(|r| matches!(r, n::Action::Start)),
        "failed launch must not Start"
    );
    drop(requests);
    running.ready(&token, &key, 2)?;
    running.finish(token)?;
    let catalog = crate::Catalog::open(&root)?;
    ensure!(
        catalog.photo_export_items(&job, 0, 2)?[0].state == "failed",
        "failed launch SQL settlement"
    );
    Ok(())
}
