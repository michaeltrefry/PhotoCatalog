//! Actual C loss across live-native and retained-output boundaries.
//! The fixture admits only the preview helper allowlist; it never dispatches an
//! export/import command. That closed scope authorizes its final forced F cleanup.
use super::*;

struct Custody {
    running: Option<Running>,
    temporary: Option<Arc<tempfile::TempDir>>,
    hold: Option<Hold>,
    reap: Option<crate::application::desktop::process::test_reap::Gate>,
}
impl Custody {
    fn cleanup(&mut self) -> Result<()> {
        self.hold.take(); // Release the ordinary relay before joining it.
        self.reap.take(); // Also release the actual Child wait owner on early error.
        let running = self.running.as_mut().context("fixture custody missing")?;
        let _shutdown = running.bridge.try_shutdown();
        {
            let state = running.bridge.0.shared.state.lock().unwrap();
            ensure!(
                state.reaped && state.child_finished,
                "C wait/transport joins unverified"
            );
        }
        // Only this fixture's closed command scope establishes there can be no
        // untracked export descendants. Production abnormal-C behavior is unchanged.
        running.parent.finish_after_dependents(true)?;
        eprintln!(
            "abnormal preview checked fixture cleanup C={} F={}",
            running.bridge.status().pid,
            running.client.pid()
        );
        running.mark_externally_retired();
        self.running.take();
        Ok(())
    }
}
impl Drop for Custody {
    fn drop(&mut self) {
        self.hold.take();
        self.reap.take();
        // Failure to prove retirement preserves owners and their stage directory.
        // The external owned test runner supplies the last-resort process deadline.
        if let Some(running) = self.running.take() {
            std::mem::forget(running);
            if let Some(temporary) = self.temporary.take() {
                eprintln!(
                    "abnormal preview unresolved fixture retained at {:?}",
                    temporary.path()
                );
                std::mem::forget(temporary);
            }
        }
        // If cleanup proved retirement, any remaining TempDir drops normally.
    }
}
#[derive(Clone, Copy, Debug)]
enum Boundary {
    BeforeStart,
    AfterStart,
    RenderEncode,
    CacheHeader,
    NativeExit,
    OutputHeld,
}
impl Boundary {
    fn exited(self) -> bool {
        matches!(self, Self::NativeExit | Self::OutputHeld)
    }
    fn cache(self) -> bool {
        matches!(self, Self::CacheHeader)
    }
    fn matches(self, call: &Call, after: bool) -> bool {
        use crate::catalog_session::preview_stage::{Action as F, Artifact};
        match (self, call) {
            (Self::BeforeStart, Call::Native(r)) => {
                after && matches!(r.action, n::Action::Spawn { .. })
            }
            (Self::AfterStart, Call::Native(r)) => after && matches!(r.action, n::Action::Start),
            (Self::RenderEncode, Call::Native(r)) => {
                !after && matches!(r.action, n::Action::Encode { header: None, .. })
            }
            (Self::CacheHeader, Call::Native(r)) => {
                !after
                    && matches!(
                        r.action,
                        n::Action::Encode {
                            header: Some(_),
                            ..
                        }
                    )
            }
            (Self::NativeExit, Call::PreviewStage(r)) => {
                !after
                    && matches!(
                        r.action,
                        F::Metadata {
                            artifact: Artifact::Receipt,
                            ..
                        }
                    )
            }
            (Self::OutputHeld, Call::PreviewStage(r)) => {
                !after && matches!(&r.action, F::Read { offset, .. } if offset.0 == 16 * 1024)
            }
            _ => false,
        }
    }
}
fn hold_boundary(
    running: &Running,
    boundary: Boundary,
    grant: Arc<Mutex<Option<n::Request>>>,
) -> Hold {
    let original = running.parent.observer.lock().unwrap().clone();
    let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
    let (release, release_rx) = std::sync::mpsc::sync_channel(1);
    let gate = Mutex::new(Some((entered_tx, release_rx)));
    *running.parent.observer.lock().unwrap() = Some(Arc::new(move |call, after| {
        if let Some(original) = &original {
            original(call, after)?;
        }
        if boundary.matches(call, after)
            && let Some((entered, release)) = gate.lock().unwrap().take()
        {
            if let Call::Native(r) = call
                && let n::Action::Encode {
                    header: Some(header),
                    ..
                } = &r.action
            {
                header.validate()?;
                *grant.lock().unwrap() = Some(r.as_ref().clone());
                ensure!(
                    header.operation == r.operation,
                    "held header operation mismatch"
                );
            }
            entered.send(())?;
            release.recv()?;
        }
        Ok(())
    }));
    Hold {
        entered,
        release: Some(release),
    }
}
fn scope(
    temporary: Arc<tempfile::TempDir>,
    executable: &Path,
    root: &Path,
    originals: &Path,
    small: bool,
    abnormal: bool,
) -> Result<(Custody, String)> {
    let (started, reap) = if abnormal {
        let (started, gate) = crate::application::desktop::process::test_reap::armed(|| {
            Running::start(temporary.clone(), executable, root, originals, small)
        });
        (started, Some(gate))
    } else {
        (
            Running::start(temporary.clone(), executable, root, originals, small),
            None,
        )
    };
    match started {
        Ok((running, token)) => Ok((
            Custody {
                running: Some(running),
                temporary: Some(temporary),
                hold: None,
                reap,
            },
            token,
        )),
        Err(error) => {
            drop(reap);
            eprintln!(
                "abnormal preview failed startup retained at {:?}",
                temporary.path()
            );
            std::mem::forget(temporary);
            Err(error)
        }
    }
}
fn run_boundary(boundary: Boundary) -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let (mut temporary, root, originals, key, original) = fixture()?;
    if boundary.cache() {
        let (mut warm, token) = scope(temporary, &executable, &root, &originals, false, false)?;
        let result = (|| -> Result<()> {
            let running = warm.running.as_ref().unwrap();
            let ready = running.ready(&token, &key, 1)?;
            let bytes = running.bytes(&token, &ready.ticket)?;
            ensure!(!bytes.bytes().is_empty(), "warm cache empty");
            ensure!(
                running
                    .observed
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|row| row.render),
                "warm cache never rendered"
            );
            Ok(())
        })();
        let cleanup = warm.cleanup();
        result?;
        cleanup?;
        temporary = warm.temporary.take().context("warm fixture missing")?;
    }
    let (mut custody, token) = scope(
        temporary,
        &executable,
        &root,
        &originals,
        boundary.cache(),
        true,
    )?;
    let grant = Arc::new(Mutex::new(None));
    custody.hold = Some(hold_boundary(
        custody.running.as_ref().unwrap(),
        boundary,
        grant.clone(),
    ));
    let result = (|| -> Result<()> {
        let running = custody.running.as_ref().unwrap();
        let ticket = request(running, &token, &key, 1)?;
        ensure!(
            matches!(ticket.state, PreviewState::Queued),
            "fixture preview was not queued"
        );
        custody
            .hold
            .as_ref()
            .unwrap()
            .entered
            .recv_timeout(Duration::from_secs(30))?;
        let (root, operation, native_pid) = {
            let observed = running.observed.lock().unwrap();
            ensure!(
                observed.len() == 1 && observed[0].render != boundary.cache(),
                "wrong actual N route for {boundary:?}"
            );
            let row = &observed[0];
            (row.root.clone(), row.operation, row.pid)
        };
        let owner = running.parent.native_owner()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let before = loop {
            let status = owner.status(&root, operation)?;
            if boundary.exited() || matches!(boundary, Boundary::BeforeStart) || status.initial_sent
            {
                break status;
            }
            ensure!(
                Instant::now() < deadline,
                "N initial send timed out: {status:?}"
            );
            thread::sleep(Duration::from_millis(5));
        };
        ensure!(
            before.pid == Some(native_pid),
            "native PID identity changed"
        );
        if boundary.exited() {
            ensure!(
                before.phase == n::Phase::Drained && before.success == Some(true),
                "output observed before N checked retirement: {before:?}"
            );
        } else {
            ensure!(
                before.success.is_none(),
                "N exited before injected C loss: {before:?}"
            );
            ensure!(!before.encode_sent, "E crossed held {boundary:?} boundary");
            ensure!(
                before.initial_sent != matches!(boundary, Boundary::BeforeStart),
                "initial send disagrees with boundary"
            );
            ensure!(
                unsafe { libc::kill(native_pid as libc::pid_t, 0) } == 0,
                "actual N is not live"
            );
        }
        if matches!(boundary, Boundary::CacheHeader) {
            let baseline = grant
                .lock()
                .unwrap()
                .clone()
                .context("held immutable grant missing")?;
            for field in 0..10 {
                let mut altered = baseline.clone();
                let n::Action::Encode {
                    header: Some(header),
                    working_bytes,
                    rgb_bytes,
                } = &mut altered.action
                else {
                    anyhow::bail!("held grant is not DecodeEncoded");
                };
                match field {
                    0 => header.stage = crate::catalog_session::LeaseId::new(),
                    1 => {
                        header.operation = U64(header
                            .operation
                            .0
                            .checked_add(1)
                            .context("test operation overflow")?)
                    }
                    2 => header.input_digest.replace_range(
                        ..1,
                        if header.input_digest.starts_with('a') {
                            "b"
                        } else {
                            "a"
                        },
                    ),
                    3 => header.input_bytes = U64(header.input_bytes.0 + 1),
                    4 => {
                        header.width = if header.width == 8192 {
                            8191
                        } else {
                            header.width + 1
                        }
                    }
                    5 => {
                        header.height = if header.height == 8192 {
                            8191
                        } else {
                            header.height + 1
                        }
                    }
                    6 => {
                        header.codec = if header.codec == crate::preview::Codec::Jpeg {
                            crate::preview::Codec::Webp
                        } else {
                            crate::preview::Codec::Jpeg
                        }
                    }
                    7 => *working_bytes = U64(working_bytes.0 + 1),
                    8 => *rgb_bytes = U64(rgb_bytes.0 + 1),
                    9 => altered.root.session = crate::catalog_session::LeaseId::new(),
                    _ => unreachable!(),
                }
                ensure!(
                    owner.call(&altered).is_err(),
                    "altered immutable grant field {field} accepted"
                );
                let status = owner.status(&root, operation)?;
                ensure!(
                    !status.encode_sent && status.success.is_none(),
                    "rejected grant advanced N: {status:?}"
                );
            }
            eprintln!(
                "immutable HeaderReady barrier: 10 independently altered grants rejected before E, actual N={native_pid}"
            );
        }
        let catalog_pid = running.bridge.status().pid;
        custody.reap.as_ref().unwrap().terminate()?;
        let exit = custody.reap.as_ref().unwrap().waited()?;
        use std::os::unix::process::ExitStatusExt;
        ensure!(
            exit.signal() == Some(libc::SIGKILL),
            "C exited before injected kill: {exit}"
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = owner.status(&root, operation)?;
            if status.phase == n::Phase::Drained {
                ensure!(
                    status.success == Some(boundary.exited()),
                    "native exit classification changed: {status:?}"
                );
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "N did not drain with ordinary relay held: {status:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
        ensure!(
            unsafe { libc::kill(native_pid as libc::pid_t, 0) } == -1,
            "N remains after checked wait/writer join"
        );
        eprintln!(
            "abnormal preview boundary={boundary:?} C={catalog_pid} N={native_pid} operation={} drained while ordinary relay held",
            operation.0
        );
        custody.hold.as_mut().unwrap().release();
        let shutdown = running.bridge.try_shutdown();
        ensure!(
            shutdown.is_err(),
            "abnormal C exit was reported as clean shutdown"
        );
        {
            let state = running.bridge.0.shared.state.lock().unwrap();
            ensure!(
                state.reaped && state.child_finished && !state.filesystem_verified,
                "C wait/transport joins or retained-F state not established"
            );
        }
        ensure!(
            unsafe { libc::kill(running.client.pid() as libc::pid_t, 0) } == 0,
            "F prematurely retired"
        );
        ensure!(
            running.client.status().phase == crate::filesystem_worker::wire::Phase::Ready,
            "F no longer ready before explicit cleanup"
        );
        owner.query(&n::Query {
            key: n::Key::new(&root, operation),
            action: n::QueryAction::Retire,
        })?;
        ensure!(
            owner.status(&root, operation).is_err(),
            "N reaper/slot not retired"
        );
        ensure!(
            unsafe { libc::kill(running.client.pid() as libc::pid_t, 0) } == 0,
            "N reaper retirement retired F"
        );
        eprintln!(
            "abnormal preview boundary={boundary:?} verified C wait/transport joins, N wait/writer/reaper joins, F={} retained",
            running.client.pid()
        );
        ensure!(
            blake3::hash(&std::fs::read(originals.join("original.png"))?)
                .to_hex()
                .as_str()
                == original,
            "original bytes changed"
        );
        Ok(())
    })();
    let cleanup = custody.cleanup();
    result?;
    cleanup?;
    Ok(())
}
#[test]
#[ignore = "configured actual C loss before N Start"]
fn actual_catalog_loss_stops_native_while_ordinary_relay_is_held_and_retains_f() -> Result<()> {
    run_boundary(Boundary::BeforeStart)
}
#[test]
#[ignore = "configured actual C loss after N Start"]
fn actual_catalog_loss_after_start_retains_f() -> Result<()> {
    run_boundary(Boundary::AfterStart)
}
#[test]
#[ignore = "configured actual C loss at Render Encode admission"]
fn actual_catalog_loss_at_render_encode_retains_f() -> Result<()> {
    run_boundary(Boundary::RenderEncode)
}
#[test]
#[ignore = "configured actual C loss at cache Header before E"]
fn actual_catalog_loss_at_cache_header_retains_f() -> Result<()> {
    run_boundary(Boundary::CacheHeader)
}
#[test]
#[ignore = "configured actual C loss after native checked exit"]
fn actual_catalog_loss_after_native_exit_retains_f() -> Result<()> {
    run_boundary(Boundary::NativeExit)
}
#[test]
#[ignore = "configured actual C loss with native output transfer held"]
fn actual_catalog_loss_with_output_held_retains_f() -> Result<()> {
    run_boundary(Boundary::OutputHeld)
}

#[test]
#[ignore = "configured staggered previews and actual C loss with two live N"]
fn actual_catalog_loss_with_two_staggered_workers_retains_f() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let (temporary, root, originals, first, first_checksum) = fixture()?;
    let second_path = originals.join("second.png");
    image::RgbImage::from_pixel(256, 256, image::Rgb([11, 97, 193])).save(&second_path)?;
    let second_checksum = blake3::hash(&std::fs::read(&second_path)?);
    let second = {
        let mut catalog = crate::Catalog::open(&root)?;
        catalog.import(&originals, None, |_| Ok(()))?;
        let rows = catalog.browse(0, 10)?;
        ensure!(rows.len() == 2, "two distinct synthetic assets required");
        VariantKey::master(
            rows.into_iter()
                .find(|row| row.id != first.asset_id)
                .context("second asset missing")?
                .id,
        )
    };
    let (started, reap) = crate::application::desktop::process::test_reap::armed(|| {
        Running::start_options(
            temporary.clone(),
            &executable,
            &root,
            &originals,
            false,
            crate::preview::Codec::Jpeg,
            2,
        )
    });
    let (running, token) = match started {
        Ok(value) => value,
        Err(error) => {
            drop(reap);
            eprintln!(
                "two-worker fixture failed startup retained at {:?}",
                temporary.path()
            );
            std::mem::forget(temporary);
            return Err(error);
        }
    };
    let mut custody = Custody {
        running: Some(running),
        temporary: Some(temporary),
        hold: None,
        reap: Some(reap),
    };
    let result = (|| -> Result<()> {
        let running = custody.running.as_ref().unwrap();
        let owner = running.parent.native_owner()?;
        owner.test_hold_encodes()?;
        let first_ticket = request(running, &token, &first, 1)?;
        ensure!(
            matches!(first_ticket.state, PreviewState::Queued),
            "first request not queued"
        );
        let wait_for = |count: usize| -> Result<Vec<(RootCapability, U64, u32)>> {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let rows: Vec<_> = {
                    let observed = running.observed.lock().unwrap();
                    ensure!(
                        observed.len() <= count && observed.iter().all(|row| row.render),
                        "unexpected native route/count"
                    );
                    observed
                        .iter()
                        .map(|row| (row.root.clone(), row.operation, row.pid))
                        .collect()
                };
                let mut all_held = rows.len() == count;
                for (root, operation, pid) in &rows {
                    let status = owner.status(root, *operation)?;
                    ensure!(
                        status.pid == Some(*pid) && status.success.is_none(),
                        "N exited before two-worker C-loss injection"
                    );
                    all_held &= owner.test_encode_is_held(root, *operation)?;
                }
                if all_held {
                    return Ok(rows);
                }
                ensure!(
                    Instant::now() < deadline,
                    "only {} of {count} staggered workers reached live Encode hold",
                    rows.len()
                );
                thread::sleep(Duration::from_millis(5));
            }
        };
        let first_native = wait_for(1)?;
        // B is submitted only after A is actually live with Encode granted.
        // Sharing generation1 keeps the first viewport ticket current.
        let second_ticket = request(running, &token, &second, 1)?;
        ensure!(
            matches!(second_ticket.state, PreviewState::Queued),
            "second request not queued"
        );
        let rows = wait_for(2)?;
        ensure!(
            rows[0] == first_native[0] && rows[0].2 != rows[1].2,
            "first native identity changed or duplicate child"
        );
        held_status(running, &token, &first_ticket.ticket)?;
        held_status(running, &token, &second_ticket.ticket)?;
        custody.reap.as_ref().unwrap().terminate()?;
        let exit = custody.reap.as_ref().unwrap().waited()?;
        use std::os::unix::process::ExitStatusExt;
        ensure!(
            exit.signal() == Some(libc::SIGKILL),
            "C did not exit from injected kill"
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let mut drained = true;
            for (root, operation, _) in &rows {
                let status = owner.status(root, *operation)?;
                if status.phase == n::Phase::Drained {
                    ensure!(
                        status.success == Some(false),
                        "held N unexpectedly succeeded"
                    );
                } else {
                    drained = false;
                }
            }
            if drained {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "two N did not drain through held Encode writers"
            );
            thread::sleep(Duration::from_millis(5));
        }
        ensure!(
            running.bridge.try_shutdown().is_err(),
            "abnormal C exit reported clean"
        );
        {
            let state = running.bridge.0.shared.state.lock().unwrap();
            ensure!(
                state.reaped && state.child_finished && !state.filesystem_verified,
                "C wait/joins and retained-F state missing"
            );
        }
        ensure!(
            running.client.status().phase == crate::filesystem_worker::wire::Phase::Ready,
            "F stopped before all dependents verified"
        );
        for (root, operation, pid) in rows {
            owner.query(&n::Query {
                key: n::Key::new(&root, operation),
                action: n::QueryAction::Retire,
            })?;
            ensure!(
                owner.status(&root, operation).is_err(),
                "N reaper/slot not retired"
            );
            ensure!(
                unsafe { libc::kill(pid as libc::pid_t, 0) } == -1,
                "N PID remains after wait/joins"
            );
            eprintln!(
                "two-worker C loss verified N={pid} operation={} wait/writer/reaper joins",
                operation.0
            );
        }
        ensure!(
            unsafe { libc::kill(running.client.pid() as libc::pid_t, 0) } == 0,
            "F retired before explicit cleanup"
        );
        ensure!(
            blake3::hash(&std::fs::read(originals.join("original.png"))?)
                .to_hex()
                .as_str()
                == first_checksum,
            "first original changed"
        );
        ensure!(
            blake3::hash(&std::fs::read(&second_path)?) == second_checksum,
            "second original changed"
        );
        Ok(())
    })();
    let cleanup = custody.cleanup();
    result?;
    cleanup?;
    Ok(())
}

mod prepared_route_tests;
