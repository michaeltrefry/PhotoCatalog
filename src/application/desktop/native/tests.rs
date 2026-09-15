use super::*;
use crate::{
    catalog_session::PhysicalObjectId,
    preview::{ByteBudget, Codec, CodecSettings, PreviewKey, RenderWork, Tier},
    storage_volume::NativePath,
};
fn root() -> RootCapability {
    // Synthetic identities only; these tests do not admit or open a catalog.
    #[cfg(unix)]
    let physical = PhysicalObjectId::Unix {
        device: U64(1),
        inode: U64(2),
    };
    #[cfg(windows)]
    let physical = PhysicalObjectId::Windows {
        volume_serial: U64(1),
        file_index: U64(2),
    };
    #[cfg(unix)]
    let path = std::path::Path::new("/synthetic");
    #[cfg(windows)]
    let path = std::path::Path::new(r"C:\synthetic");
    RootCapability {
        epoch: LeaseId::new(),
        token: LeaseId::new(),
        session: LeaseId::new(),
        canonical_root: NativePath::from_path(path),
        root_physical: physical,
        catalog_physical: physical,
    }
}

fn render() -> RenderWork {
    let key = |tier, edge, codec| PreviewKey {
        asset_id: "native-cost-fixture".into(),
        variant_id: "master".into(),
        generation: 1,
        image_pixel_generation: Some(1),
        fingerprint: "a".repeat(64),
        edit_revision: 0,
        renderer_version: crate::preview::renderer_identity().into(),
        preparation_version: crate::preview::PREPARATION_VERSION.into(),
        tier,
        edge,
        encoding: CodecSettings { codec, quality: 80 },
    };
    #[cfg(unix)]
    let path = std::path::Path::new("/synthetic/photo.raw");
    #[cfg(windows)]
    let path = std::path::Path::new(r"C:\synthetic\photo.raw");
    RenderWork {
        source: NativePath::from_path(path),
        keys: vec![
            key(Tier::Thumbnail, 256, Codec::Jpeg),
            key(Tier::Large, 512, Codec::Avif),
        ],
        encoded_limit: 1024 * 1024,
        decode_limits: crate::media::DecodeLimits::default(),
        edit: None,
    }
}

struct NoSpawn;
impl Stages for NoSpawn {
    fn arm(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<PathBuf> {
        anyhow::bail!("injected stage admission failure before spawn")
    }
    fn drained(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<()> {
        Ok(())
    }
    fn header(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<Header> {
        anyhow::bail!("unused header")
    }
}
#[test]
fn errors_stream_into_fixed_capacity_without_materializing_display() {
    struct Long;
    impl std::fmt::Display for Long {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for _ in 0..100_000 {
                f.write_str("é")?;
            }
            Ok(())
        }
    }
    let error = bounded(Long);
    assert_eq!(error.len(), ERROR_BYTES);
    assert_eq!(error.capacity(), ERROR_BYTES);
    assert!(error.is_char_boundary(error.len()));
}
#[test]
fn configured_native_allowance_must_match_the_supplied_pool() -> Result<()> {
    let limits = crate::preview::ServiceLimits::default();
    let budget = ByteBudget::new(limits.working_bytes - 1)?;
    let error = Owner::new(
        PathBuf::from("never-spawned"),
        Arc::new(NoSpawn),
        limits,
        &budget,
    )
    .err()
    .context("mismatched native allowance unexpectedly configured")?;
    assert!(
        error
            .to_string()
            .contains("native budget does not match configured working allowance")
    );
    Ok(())
}
#[test]
fn root_switch_cannot_pass_registered_or_registering_native_owner() -> Result<()> {
    let limits = crate::preview::ServiceLimits::default();
    let budget = ByteBudget::new(limits.working_bytes)?;
    let owner = Arc::new(Owner::new(
        PathBuf::from("never-spawned"),
        Arc::new(NoSpawn),
        limits,
        &budget,
    )?);
    let root = root();
    owner.bind(&root)?;
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = Mutex::new(release_rx);
    *owner.before_register.lock().unwrap() = Some(Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv().unwrap();
    }));
    let request = render();
    let cost = render_cost(
        &request,
        owner.limits.per_worker_bytes,
        owner.limits.cache_codec_scratch_bytes,
    )?;
    let r = Request {
        root: root.clone(),
        operation: U64(1),
        action: Action::Spawn {
            stage: LeaseId::new(),
            work: Work::Render(Box::new(request)),
            workers: owner.limits.workers as u8,
            working_bytes: U64(cost),
        },
    };
    let work = owner.clone();
    let spawn = std::thread::spawn(move || work.call(&r));
    entered_rx.recv()?;
    let mut other = root.clone();
    other.token = LeaseId::new();
    let work = owner.clone();
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let bind = std::thread::spawn(move || {
        done_tx.send(work.bind(&other)).unwrap();
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(10)).is_err());
    release_tx.send(())?;
    let status = spawn.join().unwrap()?;
    assert_eq!(status.phase, Phase::WaitFailed);
    assert!(status.pid.is_none());
    assert!(done_rx.recv()?.is_err());
    bind.join().unwrap();
    owner.call(&Request {
        root: root.clone(),
        operation: U64(1),
        action: Action::Drain,
    })?;
    while owner.status(&root, U64(1))?.phase != Phase::Drained {
        std::thread::yield_now();
    }
    owner.retire_root(&root)?;
    assert!(owner.slots.lock().unwrap().is_empty());
    Ok(())
}

#[test]
fn same_pool_refuses_spawn_retains_unknown_and_reuses_only_after_retirement() -> Result<()> {
    let mut limits = crate::preview::ServiceLimits::default();
    limits.workers = 1;
    let work = render();
    let cost = render_cost(
        &work,
        limits.per_worker_bytes,
        limits.cache_codec_scratch_bytes,
    )?;
    limits.working_bytes = cost.checked_add(64).context("fixture budget overflow")?;
    let budget = ByteBudget::new(limits.working_bytes)?;
    let owner = Owner::new(
        PathBuf::from("never-spawned"),
        Arc::new(NoSpawn),
        limits.clone(),
        &budget,
    )?;
    let root = root();
    owner.bind(&root)?;
    let request = Request {
        root: root.clone(),
        operation: U64(1),
        action: Action::Spawn {
            stage: LeaseId::new(),
            work: Work::Render(Box::new(work)),
            workers: 1,
            working_bytes: U64(cost),
        },
    };
    let competitor = budget.reserve_exact(65).unwrap();
    let error = owner.call(&request).unwrap_err();
    let limit = error
        .downcast_ref::<crate::preview::ByteLimit>()
        .context("native admission did not preserve typed allowance refusal")?;
    assert_eq!((limit.required, limit.available), (cost, cost - 1));
    assert!(owner.slots.lock().unwrap().is_empty());
    assert_eq!(budget.used(), 65);
    drop(competitor);

    let status = owner.call(&request)?;
    assert_eq!(status.phase, Phase::WaitFailed);
    assert_eq!(budget.used(), cost);
    assert!(budget.reserve_exact(65).is_err());
    owner.call(&Request {
        action: Action::Drain,
        ..request.clone()
    })?;
    while owner.status(&root, U64(1))?.phase != Phase::Drained {
        std::thread::yield_now();
    }
    assert_eq!(budget.used(), cost, "checked drain is not retirement");
    owner.call(&Request {
        action: Action::Retire,
        ..request
    })?;
    assert_eq!(budget.used(), 0);
    let reused = budget.reserve_exact(limits.working_bytes).unwrap();
    assert_eq!(budget.used(), limits.working_bytes);
    drop(reused);
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn dropping_drained_unretired_owner_keeps_the_exact_pool_charge() -> Result<()> {
    let mut limits = crate::preview::ServiceLimits::default();
    limits.workers = 1;
    let work = render();
    let cost = render_cost(
        &work,
        limits.per_worker_bytes,
        limits.cache_codec_scratch_bytes,
    )?;
    limits.working_bytes = cost;
    let budget = ByteBudget::new(cost)?;
    let owner = Owner::new(
        PathBuf::from("never-spawned"),
        Arc::new(NoSpawn),
        limits,
        &budget,
    )?;
    let root = root();
    owner.bind(&root)?;
    let request = Request {
        root: root.clone(),
        operation: U64(1),
        action: Action::Spawn {
            stage: LeaseId::new(),
            work: Work::Render(Box::new(work)),
            workers: 1,
            working_bytes: U64(cost),
        },
    };
    assert_eq!(owner.call(&request)?.phase, Phase::WaitFailed);
    owner.call(&Request {
        action: Action::Drain,
        ..request
    })?;
    while owner.status(&root, U64(1))?.phase != Phase::Drained {
        std::thread::yield_now();
    }
    assert_eq!(budget.used(), cost);
    drop(owner);
    assert_eq!(budget.used(), cost);
    let error = budget.reserve_exact(1).err().expect("pool remains charged");
    assert_eq!((error.required, error.available), (1, 0));
    Ok(())
}

#[test]
fn same_pool_growth_refusal_keeps_the_old_slot_charge() -> Result<()> {
    let budget = ByteBudget::new(100)?;
    let root = root();
    let stage = LeaseId::new();
    let slot = Slot {
        root: root.clone(),
        operation: U64(1),
        stage: stage.clone(),
        digest: [0; 32],
        child: Mutex::new(None),
        send: Mutex::new(SendState {
            input: None,
            start: false,
            encode: false,
            stop: false,
            initial_sent: false,
            encode_sent: false,
            #[cfg(all(test, unix))]
            hold_encode: false,
            done: true,
            error: None,
        }),
        wake: Condvar::new(),
        writer: Mutex::new(None),
        reaper: Mutex::new(None),
        status: Mutex::new(Status {
            epoch: root.epoch,
            session: root.session,
            operation: U64(1),
            stage,
            pid: None,
            phase: Phase::Preparing,
            initial_sent: false,
            encode_sent: false,
            exit_code: None,
            success: None,
            error: None,
        }),
        retry: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        stages: Arc::new(NoSpawn),
        grant: Mutex::new(None),
        no_child_terminal: AtomicBool::new(true),
        reservation: Mutex::new(Some(budget.reserve_exact(40).unwrap())),
        work: Work::Render(Box::new(render())),
    };
    let competitor = budget.reserve_exact(50).unwrap();
    let error = slot.resize_reservation(60).unwrap_err();
    let limit = error
        .downcast_ref::<crate::preview::ByteLimit>()
        .context("native upgrade did not preserve typed allowance refusal")?;
    assert_eq!((limit.required, limit.available), (20, 10));
    assert_eq!(budget.used(), 90);
    assert_eq!(
        slot.reservation.lock().unwrap().as_ref().unwrap().bytes(),
        40
    );
    drop(competitor);
    slot.resize_reservation(60)?;
    assert_eq!(budget.used(), 60);
    slot.resize_reservation(20)?;
    assert_eq!(budget.used(), 20);
    drop(slot);
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn encode_growth_refusal_preserves_grant_and_send_state_for_exact_retry() -> Result<()> {
    #[derive(Clone)]
    struct HeaderStages(Header);
    impl Stages for HeaderStages {
        fn arm(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<PathBuf> {
            anyhow::bail!("unused arm")
        }
        fn drained(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<()> {
            Ok(())
        }
        fn header(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<Header> {
            Ok(self.0.clone())
        }
    }

    let mut limits = crate::preview::ServiceLimits::default();
    limits.workers = 1;
    limits.working_bytes = 100;
    limits.cache_header_scratch_bytes = 10;
    limits.cache_codec_scratch_bytes = 10;
    let budget = ByteBudget::new(limits.working_bytes)?;
    let root = root();
    let stage = LeaseId::new();
    let header = Header {
        operation: U64(1),
        stage: stage.clone(),
        input_digest: "a".repeat(64),
        input_bytes: U64(10),
        codec: Codec::Jpeg,
        width: 2,
        height: 2,
    };
    let stages = Arc::new(HeaderStages(header.clone()));
    let owner = Owner::new(
        PathBuf::from("never-spawned"),
        stages.clone(),
        limits.clone(),
        &budget,
    )?;
    owner.bind(&root)?;
    let initial = header_cost(10, limits.cache_header_scratch_bytes)?;
    let required = decode_cost(
        Codec::Jpeg,
        2,
        2,
        10,
        limits.cache_header_scratch_bytes,
        limits.cache_codec_scratch_bytes,
    )?;
    let slot = Arc::new(Slot {
        root: root.clone(),
        operation: U64(1),
        stage: stage.clone(),
        digest: [0; 32],
        child: Mutex::new(None),
        send: Mutex::new(SendState {
            input: None,
            start: true,
            encode: false,
            stop: false,
            initial_sent: true,
            encode_sent: false,
            #[cfg(all(test, unix))]
            hold_encode: false,
            done: false,
            error: None,
        }),
        wake: Condvar::new(),
        writer: Mutex::new(None),
        reaper: Mutex::new(None),
        status: Mutex::new(Status {
            epoch: root.epoch.clone(),
            session: root.session.clone(),
            operation: U64(1),
            stage,
            pid: None,
            phase: Phase::Running,
            initial_sent: true,
            encode_sent: false,
            exit_code: None,
            success: None,
            error: None,
        }),
        retry: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        stages,
        grant: Mutex::new(None),
        no_child_terminal: AtomicBool::new(true),
        reservation: Mutex::new(Some(budget.reserve_exact(initial).unwrap())),
        work: Work::DecodeEncoded {
            codec: Codec::Jpeg,
            encoded_bytes: U64(10),
            encoded_digest: "a".repeat(64),
            expected_dimensions: None,
        },
    });
    owner.slots.lock().unwrap().push(slot.clone());
    let competitor = budget.reserve_exact(70).unwrap();
    let request = Request {
        root: root.clone(),
        operation: U64(1),
        action: Action::Encode {
            header: Some(header),
            working_bytes: U64(required),
            rgb_bytes: U64(rgb_bytes(2, 2)?),
        },
    };
    let error = owner.call(&request).unwrap_err();
    let limit = error
        .downcast_ref::<crate::preview::ByteLimit>()
        .context("integrated Encode did not retain typed growth refusal")?;
    assert_eq!(
        (limit.required, limit.available),
        (required - initial, 100 - initial - 70)
    );
    assert_eq!(budget.used(), initial + 70);
    assert!(slot.grant.lock().unwrap().is_none());
    assert!(!slot.send.lock().unwrap().encode);
    assert_eq!(
        slot.reservation.lock().unwrap().as_ref().unwrap().bytes(),
        initial
    );

    drop(competitor);
    owner.call(&request)?;
    assert!(slot.grant.lock().unwrap().is_some());
    assert!(slot.send.lock().unwrap().encode);
    assert_eq!(
        slot.reservation.lock().unwrap().as_ref().unwrap().bytes(),
        required
    );
    slot.status.lock().unwrap().phase = Phase::Drained;
    owner.call(&Request {
        root,
        operation: U64(1),
        action: Action::Retire,
    })?;
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
#[ignore = "inert native custody subprocess entrypoint"]
fn held_stdin_entrypoint() {
    assert_eq!(
        std::env::var("PHOTOCATALOG_FS8_HELD_STDIN").as_deref(),
        Ok("1")
    );
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
#[ignore = "explicit actual Child/stdin custody fixture"]
fn actual_live_child_with_blocked_stdin_stops_and_joins_before_stage_drain() -> Result<()> {
    let mut child = Command::new(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "application::desktop::native::tests::held_stdin_entrypoint",
            "--nocapture",
        ])
        .env("PHOTOCATALOG_FS8_HELD_STDIN", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let pid = child.id();
    let input = child.stdin.take().context("fixture child stdin")?;
    let root = root();
    let stage = LeaseId::new();
    let budget = ByteBudget::new(1)?;
    let slot = Arc::new(Slot {
        root: root.clone(),
        operation: U64(1),
        stage: stage.clone(),
        digest: [0; 32],
        child: Mutex::new(Some(child)),
        send: Mutex::new(SendState {
            input: Some((input, vec![b'x'; REQUEST_BYTES - 1])),
            start: true,
            encode: false,
            stop: false,
            initial_sent: false,
            encode_sent: false,
            #[cfg(all(test, unix))]
            hold_encode: false,
            done: false,
            error: None,
        }),
        wake: Condvar::new(),
        writer: Mutex::new(None),
        reaper: Mutex::new(None),
        status: Mutex::new(Status {
            epoch: root.epoch.clone(),
            session: root.session.clone(),
            operation: U64(1),
            stage,
            pid: Some(pid),
            phase: Phase::Spawned,
            initial_sent: false,
            encode_sent: false,
            exit_code: None,
            success: None,
            error: None,
        }),
        retry: AtomicBool::new(true),
        stop: AtomicBool::new(false),
        stages: Arc::new(NoSpawn),
        grant: Mutex::new(None),
        no_child_terminal: AtomicBool::new(false),
        reservation: Mutex::new(Some(budget.reserve_exact(1).unwrap())),
        work: Work::Render(Box::new(render())),
    });
    struct Cleanup(Option<Arc<Slot>>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(slot) = &self.0 {
                slot.stop();
                slot.retry.store(true, Ordering::Release);
                let _ = slot.start_reaper();
            }
        }
    }
    let mut cleanup = Cleanup(Some(slot.clone()));
    slot.start_writer()?;
    slot.start_reaper()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while slot.status().phase == Phase::Spawned && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(slot.status().phase, Phase::Sending);
    assert!(
        !slot.status().initial_sent,
        "fixture must hold an actual unfinished stdin write"
    );
    let stopped = std::time::Instant::now();
    slot.stop();
    assert!(
        stopped.elapsed() < Duration::from_secs(1),
        "Stop blocked behind native stdin/live wait"
    );
    while slot.status().phase != Phase::Drained && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(slot.status().phase, Phase::Drained);
    assert!(slot.status().success.is_some());
    assert!(slot.child.lock().unwrap().is_none());
    assert!(slot.writer.lock().unwrap().is_none());
    slot.reaper
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .join()
        .map_err(|_| anyhow::anyhow!("fixture reaper join"))?;
    println!(
        "checked native PID {pid}: Child wait, stdin writer join, stage drain acknowledgement and reaper join complete"
    );
    cleanup.0.take(); // all owned handles explicitly joined above
    Ok(())
}

#[test]
fn failed_f_release_retains_exact_root_for_supervisor_cleanup() -> Result<()> {
    struct Retained(std::sync::atomic::AtomicUsize);
    impl Stages for Retained {
        fn arm(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<PathBuf> {
            anyhow::bail!("unused")
        }
        fn drained(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<()> {
            Ok(())
        }
        fn header(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<Header> {
            anyhow::bail!("unused")
        }
        fn abandon(&self, _: &RootCapability) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    let stages = Arc::new(Retained(std::sync::atomic::AtomicUsize::new(0)));
    let limits = crate::preview::ServiceLimits::default();
    let budget = ByteBudget::new(limits.working_bytes)?;
    let owner = Owner::new(std::env::current_exe()?, stages.clone(), limits, &budget)?;
    let first = root();
    let next = root();
    owner.bind(&first)?;
    owner.retire_root(&first)?;
    // Simulate F refusing ReleaseRoot: do not acknowledge release to G.
    assert_eq!(owner.selected.lock().unwrap().as_ref(), Some(&first));
    assert!(owner.bind(&next).is_err());
    owner.finish_after_catalog()?;
    assert_eq!(stages.0.load(Ordering::SeqCst), 1);
    assert_eq!(owner.selected.lock().unwrap().as_ref(), Some(&first));
    owner.forget_released_root(&first)?;
    owner.bind(&next)?;
    owner.retire_root(&next)?;
    owner.forget_released_root(&next)?;
    Ok(())
}

struct CancelStages {
    directory: PathBuf,
    held: Mutex<
        Option<(
            std::sync::mpsc::SyncSender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
    arms: std::sync::atomic::AtomicUsize,
    drains: std::sync::atomic::AtomicUsize,
}
impl Stages for CancelStages {
    fn arm(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<PathBuf> {
        self.arms.fetch_add(1, Ordering::AcqRel);
        let held = self.held.lock().unwrap().take();
        if let Some((entered, release)) = held {
            entered.send(())?;
            release.recv()?;
        }
        Ok(self.directory.clone())
    }
    fn drained(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<()> {
        self.drains.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn header(&self, _: &RootCapability, _: &LeaseId, _: U64) -> Result<Header> {
        anyhow::bail!("unexpected canceled fixture header read")
    }
}
fn cancel_request(owner: &Owner, root: &RootCapability) -> Result<Request> {
    let work = render();
    let cost = render_cost(
        &work,
        owner.limits.per_worker_bytes,
        owner.limits.cache_codec_scratch_bytes,
    )?;
    Ok(Request {
        root: root.clone(),
        operation: U64(1),
        action: Action::Spawn {
            stage: LeaseId::new(),
            work: Work::Render(Box::new(work)),
            workers: owner.limits.workers as u8,
            working_bytes: U64(cost),
        },
    })
}
fn retire_canceled_attempt(owner: &Owner, request: &Request) -> Result<()> {
    owner.call(&Request {
        action: Action::Drain,
        ..request.clone()
    })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if owner.status(&request.root, request.operation)?.phase == Phase::Drained {
            break;
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "canceled fixture drain timeout"
        );
        thread::sleep(Duration::from_millis(2));
    }
    owner.call(&Request {
        action: Action::Retire,
        ..request.clone()
    })?;
    ensure!(
        owner.slots.lock().unwrap().is_empty(),
        "retired fixture slot remains"
    );
    Ok(())
}
#[test]
fn reserved_stop_overtaking_spawn_prevents_os_child_and_stays_root_bound() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let stages = Arc::new(CancelStages {
        directory: directory.path().to_owned(),
        held: Mutex::new(None),
        arms: std::sync::atomic::AtomicUsize::new(0),
        drains: std::sync::atomic::AtomicUsize::new(0),
    });
    // Intentionally absent executable: a regression must never launch a helper.
    let limits = crate::preview::ServiceLimits::default();
    let budget = ByteBudget::new(limits.working_bytes)?;
    let owner = Owner::new(
        directory.path().join("absent-native"),
        stages.clone(),
        limits,
        &budget,
    )?;
    let selected = root();
    owner.bind(&selected)?;
    let request = cancel_request(&owner, &selected)?;
    let key = Key::new(&selected, request.operation);
    let mut foreign = key.clone();
    foreign.root = LeaseId::new();
    assert!(owner.stop_key(&foreign).is_err());
    assert!(owner.early_stop.lock().unwrap().is_empty());
    owner.stop_key(&key)?;
    owner.stop_key(&key)?;
    assert_eq!(owner.early_stop.lock().unwrap().as_slice(), &[key]);
    assert!(owner.slots.lock().unwrap().is_empty());
    let status = owner.call(&request)?;
    assert!(status.pid.is_none());
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or("")
            .contains("native canceled before OS spawn")
    );
    assert!(owner.early_stop.lock().unwrap().is_empty());
    assert_eq!(stages.arms.load(Ordering::Acquire), 1);
    retire_canceled_attempt(&owner, &request)?;
    assert_eq!(stages.drains.load(Ordering::Acquire), 1);
    Ok(())
}
#[test]
fn reserved_stop_and_status_progress_while_stage_arm_is_held() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let stages = Arc::new(CancelStages {
        directory: directory.path().to_owned(),
        held: Mutex::new(Some((entered_tx, release_rx))),
        arms: std::sync::atomic::AtomicUsize::new(0),
        drains: std::sync::atomic::AtomicUsize::new(0),
    });
    let limits = crate::preview::ServiceLimits::default();
    let budget = ByteBudget::new(limits.working_bytes)?;
    let owner = Arc::new(Owner::new(
        directory.path().join("absent-native"),
        stages.clone(),
        limits,
        &budget,
    )?);
    let selected = root();
    owner.bind(&selected)?;
    let request = cancel_request(&owner, &selected)?;
    let spawning = owner.clone();
    let pending = request.clone();
    let spawn = thread::spawn(move || spawning.call(&pending));
    let entered = entered_rx.recv_timeout(Duration::from_secs(2));
    let controls = owner.clone();
    let key = Key::new(&selected, request.operation);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let control = thread::spawn(move || {
        let result = controls.stop_key(&key).and_then(|()| {
            controls.query(&Query {
                key,
                action: QueryAction::Status,
            })
        });
        let _ = done_tx.send(result);
    });
    let while_held = done_rx.recv_timeout(Duration::from_secs(2));
    let drains_before_release = stages.drains.load(Ordering::Acquire);
    // Release the fake F call and join all test threads before timing asserts.
    let _ = release_tx.send(());
    let spawned = spawn.join().expect("held F arm spawn panicked");
    control.join().expect("reserved G control panicked");
    entered?;
    let observed = while_held??;
    let terminal = spawned?;
    assert_eq!(observed.phase, Phase::StopRequested);
    assert!(observed.pid.is_none());
    assert_eq!(drains_before_release, 0);
    assert!(terminal.pid.is_none());
    assert!(
        terminal
            .error
            .as_deref()
            .unwrap_or("")
            .contains("native canceled before OS spawn")
    );
    retire_canceled_attempt(&owner, &request)?;
    assert_eq!(stages.drains.load(Ordering::Acquire), 1);
    Ok(())
}
