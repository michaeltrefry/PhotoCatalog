//! Configured C/G/F/N preview routes. No export/import command is dispatched.
use super::super::DesktopBridge;
use super::{Call, Parent};
use crate::application::{
    Config, Limits, PreviewState, PreviewStatus, PreviewTier, Reply, Request, Response, U64,
};
use crate::catalog_edits::VariantKey;
use crate::catalog_session::{RootCapability, native as n};
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
    metadata: crate::preview::ByteBudget,
    temporary: Arc<tempfile::TempDir>,
    token: Option<String>,
    cleanup: CleanupState,
}
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
        ensure!((1..=2).contains(&workers), "fixture worker count");
        let client = Arc::new(Client::spawn(
            executable,
            vec![NativePath::from_path(originals)],
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
                128 * 1024 * 1024 * workers as u64
            },
            per_worker_bytes: if small { 64 * 1024 } else { 64 * 1024 * 1024 },
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
            original_roots: vec![originals.to_owned()],
            preview_policy: policy,
            preview_limits: limits.clone(),
            limits: Limits::default(),
            import_checkpoint: None,
        };
        let native = crate::preview::ByteBudget::new(limits.working_bytes)?;
        parent.configure_native(executable.to_owned(), limits, &native)?;
        let observed: Arc<Mutex<Vec<Observation>>> = Default::default();
        let weak = Arc::downgrade(&parent);
        let observations = observed.clone();
        *parent.observer.lock().unwrap() = Some(Arc::new(move |call, after| {
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
        // All pre-C fallibility passed while guarded. Once called, spawn_inner
        // owns C creation and its Unstarted error retains F when required.
        before_catalog.parent.take();
        let bridge = match DesktopBridge::spawn_inner(config, Some(parent.clone()), Some(&metadata))
        {
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
            metadata,
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
        let mut filesystem_retired = false;
        if c_reaped {
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
