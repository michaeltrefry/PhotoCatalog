use super::*;
use crate::{
    catalog_edits::{EditRenderIdentity, VariantKey},
    catalog_exports::{
        ExportWork, MetadataSelection, PhotoExportPlan, StoredOutput, StoredProfile,
    },
    catalog_images::ImageMetadataIdentity,
    catalog_metadata::RenderIdentity,
    catalog_session::PhysicalObjectId,
    edit::Recipe,
    image_export::{AlphaPolicy, IntegerDepth, OutputFormat, OutputSize},
    metadata_export::{DestinationSnapshot, FileRevision},
    storage_volume::NativePath,
};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

pub(crate) struct FakeStages {
    path: PathBuf,
    calls: Mutex<Vec<export_stage::Request>>,
    hold_ready: AtomicBool,
    hold_begin: AtomicBool,
    hold_arm: AtomicBool,
    ready_entered: AtomicBool,
    hold_wake: Condvar,
    hold: Mutex<()>,
    drain_unknown: AtomicBool,
    begin_rejected: AtomicBool,
    seal_attempts: AtomicUsize,
}
impl FakeStages {
    pub(crate) fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            calls: Mutex::new(Vec::new()),
            hold_ready: AtomicBool::new(false),
            hold_begin: AtomicBool::new(false),
            hold_arm: AtomicBool::new(false),
            ready_entered: AtomicBool::new(false),
            hold_wake: Condvar::new(),
            hold: Mutex::new(()),
            drain_unknown: AtomicBool::new(false),
            begin_rejected: AtomicBool::new(false),
            seal_attempts: AtomicUsize::new(0),
        })
    }
    pub(crate) fn pause_arm(&self) {
        self.ready_entered.store(false, Ordering::Release);
        self.hold_arm.store(true, Ordering::Release);
    }
    pub(crate) fn resume_arm(&self) {
        self.hold_arm.store(false, Ordering::Release);
        self.hold_wake.notify_all();
    }
    pub(crate) fn pause_begin(&self) {
        self.hold_begin.store(true, Ordering::Release);
    }
    pub(crate) fn resume_begin(&self) {
        self.hold_begin.store(false, Ordering::Release);
        self.hold_wake.notify_all();
    }
    fn actions(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|request| format!("{:?}", std::mem::discriminant(&request.action)))
            .collect()
    }
    fn count(&self, predicate: impl Fn(&export_stage::Action) -> bool) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|request| predicate(&request.action))
            .count()
    }
    pub(crate) fn wait_ready_entered(&self) {
        let mut guard = self.hold.lock().unwrap();
        while !self.ready_entered.load(AtomicOrdering::Acquire) {
            let waited = self
                .hold_wake
                .wait_timeout(guard, Duration::from_secs(5))
                .unwrap();
            assert!(!waited.1.timed_out(), "held F request deadline");
            guard = waited.0;
        }
    }
    fn release_ready(&self) {
        self.hold_ready.store(false, AtomicOrdering::Release);
        self.hold_wake.notify_all();
    }
}
impl Stages for FakeStages {
    fn call(
        &self,
        request: &export_stage::Request,
        _cancel: &AtomicBool,
    ) -> Result<export_stage::Reply> {
        request.validate()?;
        self.calls.lock().unwrap().push(request.clone());
        if matches!(request.action, export_stage::Action::Begin { .. })
            && self.begin_rejected.load(AtomicOrdering::Acquire)
        {
            return Err(Failure::new(
                FailureKind::Rejected,
                "injected generic Begin failure after unknown F custody",
            )
            .into());
        }
        if (matches!(request.action, export_stage::Action::Ready { .. })
            && self.hold_ready.load(AtomicOrdering::Acquire))
            || (matches!(request.action, export_stage::Action::Begin { .. })
                && self.hold_begin.load(AtomicOrdering::Acquire))
            || (matches!(request.action, export_stage::Action::Arm { .. })
                && self.hold_arm.load(AtomicOrdering::Acquire))
        {
            let mut guard = self.hold.lock().unwrap();
            self.ready_entered.store(true, AtomicOrdering::Release);
            self.hold_wake.notify_all();
            while self.hold_ready.load(AtomicOrdering::Acquire)
                || self.hold_begin.load(AtomicOrdering::Acquire)
                || self.hold_arm.load(AtomicOrdering::Acquire)
            {
                let waited = self
                    .hold_wake
                    .wait_timeout(guard, Duration::from_secs(5))
                    .unwrap();
                assert!(!waited.1.timed_out(), "held F request deadline");
                guard = waited.0;
            }
        }
        if matches!(request.action, export_stage::Action::NativeDrained { .. })
            && self.drain_unknown.load(AtomicOrdering::Acquire)
        {
            return Err(Failure::new(FailureKind::Unknown, "lost drain acknowledgement").into());
        }
        if matches!(request.action, export_stage::Action::ResultAndSeal) {
            let attempt = self.seal_attempts.fetch_add(1, AtomicOrdering::AcqRel);
            let mut value = Failure::new(FailureKind::Unknown, "injected terminal seal failure");
            if attempt > 0 {
                value.object_receipt = Some(crate::catalog_session::preview_io::FailureReceipt {
                    operation: request.operation,
                    step: U64(0),
                    request_digest: request.digest()?,
                });
            }
            return Err(value.into());
        }
        let value = match request.action {
            export_stage::Action::Begin { .. } => export_stage::Value::Begun,
            export_stage::Action::Ready { .. } => export_stage::Value::Ready {
                path: NativePath::from_path(&self.path),
            },
            _ => export_stage::Value::Unit,
        };
        Ok(export_stage::Reply {
            epoch: request.root.epoch.clone(),
            session: request.root.session.clone(),
            stage: request.stage.clone(),
            operation: request.operation,
            binding: request.binding.clone(),
            value,
        })
    }
}

pub(crate) struct Fixture {
    pub(crate) _temp: tempfile::TempDir,
    pub(crate) root: RootCapability,
    pub(crate) stage: LeaseId,
    pub(crate) binding: export_stage::Binding,
    pub(crate) begin: export_stage::Request,
    pub(crate) worker: u64,
}
pub(crate) fn fixture() -> Result<Fixture> {
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let recipe = Recipe::default();
    let fingerprint = "a".repeat(64);
    let plan = PhotoExportPlan {
        version: 3,
        renderer_identity: crate::photo_render::output_renderer_identity().into(),
        identity: EditRenderIdentity {
            image_identity: Some(ImageMetadataIdentity {
                image_id: "asset".into(),
                key: VariantKey::master("asset"),
                metadata_revision: 0,
                pixel_generation: 1,
                shared_source_epoch: 0,
                physical_generation: 1,
            }),
            source: RenderIdentity {
                asset_id: "asset".into(),
                generation: 1,
                fingerprint: Some(fingerprint.clone()),
                state: "ready".into(),
                metadata_revision: 0,
            },
            key: VariantKey::master("asset"),
            revision: 1,
            recipe_digest: recipe.validate()?.digest().into(),
        },
        original: NativePath::from_path(&base.join("original.raw")),
        original_revision: FileRevision {
            bytes: 64,
            digest: fingerprint,
            modified_ns: 1,
            identity: (1, 2),
        },
        recipe,
        output: StoredOutput {
            size: OutputSize::Original,
            format: OutputFormat::Png {
                depth: IntegerDepth::Eight,
            },
            profile: StoredProfile::Srgb,
            alpha: AlphaPolicy::Preserve,
        },
        metadata: MetadataSelection::Omit,
        xmp_blob: None,
        destination: DestinationSnapshot {
            version: 2,
            operation: uuid::Uuid::new_v4().to_string(),
            destination: base.join("destination.png"),
            expected: None,
            max_existing_bytes: 1024,
        },
        max_original_bytes: 1024,
        max_payload_bytes: 1024,
        alias_limits: Default::default(),
    };
    let raw = serde_json::to_string(&plan)?;
    let authority = blake3::hash(raw.as_bytes()).to_hex().to_string();
    let work = ExportWork {
        job: uuid::Uuid::new_v4().to_string(),
        sequence: 1,
        attempt: uuid::Uuid::new_v4().to_string(),
        authority: authority.clone(),
        plan: crate::catalog_exports::checked_plan(&raw, &authority)?,
    };
    let root = RootCapability {
        epoch: LeaseId::new(),
        token: LeaseId::new(),
        session: LeaseId::new(),
        canonical_root: NativePath::from_path(&base),
        root_physical: PhysicalObjectId::Unix {
            device: U64(1),
            inode: U64(2),
        },
        catalog_physical: PhysicalObjectId::Unix {
            device: U64(1),
            inode: U64(3),
        },
    };
    let stage = LeaseId::new();
    let binding = export_stage::Binding::from_work(&work);
    let limits = crate::photo_render::PhotoRenderLimits {
        decode: crate::media::DecodeLimits {
            max_encoded_bytes: work.plan.max_original_bytes,
            ..Default::default()
        },
        render: Default::default(),
        encode: Default::default(),
        max_encoded_extent: 1024,
    };
    crate::export_worker::prepare_managed_request(&work, limits, &base.join("fixture-stage"))?;
    let worker = limits
        .decode
        .max_allocation_bytes
        .max(limits.render.max_live_bytes)
        .max(limits.encode.render.max_live_bytes);
    let begin = export_stage::Request {
        root: root.clone(),
        stage: stage.clone(),
        operation: U64(1),
        supervisor: false,
        binding: binding.clone(),
        action: export_stage::Action::Begin {
            work: Box::new(work),
            limits,
        },
    };
    Ok(Fixture {
        _temp: temp,
        root,
        stage,
        binding,
        begin,
        worker,
    })
}
pub(crate) fn lifecycle(f: &Fixture, action: Action) -> Request {
    Request {
        root: f.root.clone(),
        operation: U64(9),
        stage: f.stage.clone(),
        binding: f.binding.clone(),
        action,
    }
}
pub(crate) fn register(f: &Fixture) -> Request {
    lifecycle(
        f,
        Action::Register {
            begin: Box::new(f.begin.clone()),
            worker_bytes: U64(f.worker),
            working_bytes: U64(f.worker),
        },
    )
}
fn owner(f: &Fixture, stages: Arc<FakeStages>, pool: &ByteBudget) -> Result<Arc<Owner>> {
    let owner = Arc::new(Owner::new(
        f._temp.path().join("missing-export-worker"),
        stages,
        1,
        pool,
    )?);
    owner.bind(&f.root)?;
    Ok(owner)
}
pub(crate) fn ordinary(
    f: &Fixture,
    operation: u64,
    action: export_stage::Action,
) -> export_stage::Request {
    export_stage::Request {
        root: f.root.clone(),
        stage: f.stage.clone(),
        operation: U64(operation),
        supervisor: false,
        binding: f.binding.clone(),
        action,
    }
}

#[test]
fn shared_pool_refuses_atomically_and_reuses_only_after_explicit_retirement() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let competitor = pool.reserve_exact(1)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages.clone(), &pool)?;
    let error = owner.call(&register(&f)).unwrap_err();
    let limit = error
        .downcast_ref::<crate::preview::ByteLimit>()
        .context("typed shared-pool refusal")?;
    assert_eq!((limit.required, limit.available), (f.worker, f.worker - 1));
    drop(competitor);
    owner.call(&register(&f))?;
    owner.stage_call(&f.begin, &AtomicBool::new(false))?;
    owner.stage_call(
        &ordinary(&f, 2, export_stage::Action::Abort),
        &AtomicBool::new(false),
    )?;
    owner.call(&lifecycle(&f, Action::Retire))?;
    let reused = pool.reserve_exact(f.worker)?;
    assert_eq!(reused.bytes(), f.worker);
    assert_eq!(
        stages.count(|action| matches!(action, export_stage::Action::Begin { .. })),
        1
    );
    Ok(())
}

#[test]
fn ordinary_stage_replay_is_exact_and_unregistered_or_foreign_work_is_rejected() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages.clone(), &pool)?;
    assert!(owner.stage_call(&f.begin, &AtomicBool::new(false)).is_err());
    owner.call(&register(&f))?;
    owner.stage_call(&f.begin, &AtomicBool::new(false))?;
    owner.stage_call(&f.begin, &AtomicBool::new(false))?;
    assert_eq!(
        stages.count(|action| matches!(action, export_stage::Action::Begin { .. })),
        1
    );
    assert!(
        owner
            .stage_call(
                &ordinary(&f, 1, export_stage::Action::Abort),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    let mut foreign_root = f.root.clone();
    foreign_root.token = LeaseId::new();
    let mut foreign_begin = f.begin.clone();
    foreign_begin.root = foreign_root.clone();
    let mut foreign = register(&f);
    foreign.root = foreign_root;
    let Action::Register { begin, .. } = &mut foreign.action else {
        unreachable!()
    };
    *begin = Box::new(foreign_begin);
    assert!(owner.call(&foreign).is_err());
    owner.finish_after_catalog()?;
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn reserved_stop_overtakes_a_held_ordinary_stage_call_and_close_before_seal_never_seals()
-> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages.clone(), &pool)?;
    owner.call(&register(&f))?;
    owner.stage_call(&f.begin, &AtomicBool::new(false))?;
    stages.hold_ready.store(true, AtomicOrdering::Release);
    let request = ordinary(
        &f,
        2,
        export_stage::Action::Ready {
            icc: None,
            xmp: None,
        },
    );
    let worker = {
        let owner = owner.clone();
        thread::spawn(move || owner.stage_call(&request, &AtomicBool::new(false)))
    };
    stages.wait_ready_entered();
    owner.stop_key(&Key::new(&f.root, U64(9), &f.stage))?;
    stages.release_ready();
    worker.join().unwrap()?;
    owner.finish_after_catalog()?;
    assert_eq!(
        stages.count(|action| matches!(action, export_stage::Action::ResultAndSeal)),
        0
    );
    assert_eq!(
        stages.count(|action| matches!(action, export_stage::Action::Abort)),
        1
    );
    assert!(!stages.actions().is_empty());
    Ok(())
}

#[test]
fn close_after_seal_dispatch_replays_only_the_exact_pending_seal() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages.clone(), &pool)?;
    owner.call(&register(&f))?;
    owner.stage_call(&f.begin, &AtomicBool::new(false))?;
    let seal = ordinary(&f, 2, export_stage::Action::ResultAndSeal);
    assert!(owner.stage_call(&seal, &AtomicBool::new(false)).is_err());
    owner.retire_root(&f.root)?;
    let seals: Vec<_> = stages
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|request| matches!(request.action, export_stage::Action::ResultAndSeal))
        .map(export_stage::Request::digest)
        .collect::<Result<_>>()?;
    assert_eq!(seals.len(), 2);
    assert_eq!(seals[0], seals[1]);
    assert_eq!(pool.used(), 0);
    assert_eq!(
        stages.count(|action| matches!(action, export_stage::Action::Abort)),
        1
    );
    Ok(())
}

#[test]
fn failed_spawn_and_lost_drain_ack_retain_charge_until_exact_retry_and_release() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages.clone(), &pool)?;
    owner.call(&register(&f))?;
    owner.stage_call(&f.begin, &AtomicBool::new(false))?;
    owner.stage_call(
        &ordinary(
            &f,
            2,
            export_stage::Action::Ready {
                icc: None,
                xmp: None,
            },
        ),
        &AtomicBool::new(false),
    )?;
    let status = owner.call(&lifecycle(&f, Action::Spawn))?;
    assert_eq!(status.phase, Phase::WaitFailed);
    stages.drain_unknown.store(true, AtomicOrdering::Release);
    owner.call(&lifecycle(&f, Action::RetryDrain))?;
    wait_until(|| stages.count(|a| matches!(a, export_stage::Action::NativeDrained { .. })) == 1)?;
    let slot = owner.slot(&f.root, U64(9), &f.stage)?;
    wait_until(|| {
        slot.reaper
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| h.is_finished())
    })?;
    assert_eq!(slot.status().phase, Phase::WaitFailed);
    assert_eq!(pool.used(), f.worker);
    assert!(owner.call(&lifecycle(&f, Action::Retire)).is_err());
    stages.drain_unknown.store(false, AtomicOrdering::Release);
    owner.call(&lifecycle(&f, Action::RetryDrain))?;
    wait_until(|| {
        slot.status().phase == Phase::Drained
            && slot
                .reaper
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|h| h.is_finished())
    })?;
    assert_eq!(slot.status().phase, Phase::Drained);
    owner.stage_call(
        &ordinary(&f, 3, export_stage::Action::Release),
        &AtomicBool::new(false),
    )?;
    owner.call(&lifecycle(&f, Action::Retire))?;
    assert_eq!(pool.used(), 0);
    let drain_calls: Vec<_> = stages
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|request| matches!(request.action, export_stage::Action::NativeDrained { .. }))
        .map(|request| (request.operation, request.digest().unwrap()))
        .collect();
    assert_eq!(drain_calls.len(), 2);
    assert_eq!(drain_calls[0], drain_calls[1]);
    Ok(())
}

#[test]
fn owner_drop_does_not_return_an_unretired_reservation() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages, &pool)?;
    owner.call(&register(&f))?;
    drop(owner);
    let error = pool.reserve_exact(1).err().expect("pool remains charged");
    assert_eq!((error.required, error.available), (1, 0));
    Ok(())
}

#[test]
fn generic_begin_rejection_is_not_negative_authority_for_retirement() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    stages.begin_rejected.store(true, AtomicOrdering::Release);
    let owner = owner(&f, stages.clone(), &pool)?;
    owner.call(&register(&f))?;
    assert!(owner.stage_call(&f.begin, &AtomicBool::new(false)).is_err());
    assert!(owner.call(&lifecycle(&f, Action::Retire)).is_err());
    assert!(owner.retire_root(&f.root).is_err());
    assert_eq!(pool.used(), f.worker);
    assert_eq!(
        stages.count(|action| matches!(action, export_stage::Action::Abort)),
        0
    );
    Ok(())
}

fn wait_until(mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        ensure!(
            std::time::Instant::now() < deadline,
            "fixture event deadline"
        );
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

struct RealStages {
    manifest: PathBuf,
    owner: Mutex<crate::filesystem_worker::export_stage_test_support::Owner>,
    calls: Mutex<Vec<export_stage::Request>>,
    fail_begin: AtomicBool,
    lose_arm: AtomicBool,
    lose_seal: AtomicBool,
    lose_release: AtomicBool,
    lose_drain: AtomicBool,
    hold_drain: AtomicBool,
    drain_entered: AtomicBool,
    drain_gate: Mutex<()>,
    drain_wake: Condvar,
}
impl RealStages {
    fn new(f: &Fixture) -> Arc<Self> {
        Arc::new(Self {
            manifest: f._temp.path().to_owned(),
            owner: Mutex::new(Default::default()),
            calls: Mutex::new(Vec::new()),
            fail_begin: AtomicBool::new(false),
            lose_arm: AtomicBool::new(false),
            lose_seal: AtomicBool::new(false),
            lose_release: AtomicBool::new(false),
            lose_drain: AtomicBool::new(false),
            hold_drain: AtomicBool::new(false),
            drain_entered: AtomicBool::new(false),
            drain_gate: Mutex::new(()),
            drain_wake: Condvar::new(),
        })
    }
}
impl Stages for RealStages {
    fn call(
        &self,
        request: &export_stage::Request,
        cancel: &AtomicBool,
    ) -> Result<export_stage::Reply> {
        assert!(
            !cancel.load(Ordering::Acquire),
            "G admission must survive later C cancellation"
        );
        self.calls.lock().unwrap().push(request.clone());
        if self.fail_begin.swap(false, Ordering::AcqRel) {
            crate::filesystem_worker::export_stage_test_support::captured_begin_failure();
        }
        let result = self
            .owner
            .lock()
            .unwrap()
            .call(&self.manifest, request, cancel);
        if matches!(request.action, export_stage::Action::NativeDrained { .. })
            && self.hold_drain.load(Ordering::Acquire)
        {
            let mut gate = self.drain_gate.lock().unwrap();
            self.drain_entered.store(true, Ordering::Release);
            self.drain_wake.notify_all();
            while self.hold_drain.load(Ordering::Acquire) {
                let waited = self
                    .drain_wake
                    .wait_timeout(gate, Duration::from_secs(5))
                    .unwrap();
                ensure!(!waited.1.timed_out(), "held real F drain deadline");
                gate = waited.0;
            }
        }
        if (matches!(request.action, export_stage::Action::Arm { .. })
            && self.lose_arm.swap(false, Ordering::AcqRel))
            || (matches!(request.action, export_stage::Action::ResultAndSeal)
                && self.lose_seal.swap(false, Ordering::AcqRel))
            || (matches!(request.action, export_stage::Action::Release)
                && self.lose_release.swap(false, Ordering::AcqRel))
            || (matches!(request.action, export_stage::Action::NativeDrained { .. })
                && self.lose_drain.swap(false, Ordering::AcqRel))
        {
            return Err(Failure::new(FailureKind::Unknown, "lost actual F reply").into());
        }
        result
    }
}
fn real_owner(f: &Fixture, stages: Arc<RealStages>, pool: &ByteBudget) -> Result<Arc<Owner>> {
    let owner = Arc::new(Owner::new(
        f._temp.path().join("unused-worker"),
        stages,
        1,
        pool,
    )?);
    owner.bind(&f.root)?;
    owner.call(&register(f))?;
    Ok(owner)
}
fn ready(f: &Fixture, owner: &Owner) -> Result<()> {
    owner.stage_call(&f.begin, &AtomicBool::new(false))?;
    owner.stage_call(
        &ordinary(
            f,
            2,
            export_stage::Action::Ready {
                icc: None,
                xmp: None,
            },
        ),
        &AtomicBool::new(false),
    )?;
    Ok(())
}

// Each potentially blocking concurrency case runs in a checked, deadline-bound
// subprocess. A regression in G locking cannot hang the outer libtest process.
#[cfg(unix)]
fn subprocess_case(case: &str) -> Result<()> {
    let mut child = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "application::desktop::export_native::tests::lifecycle_subprocess_entrypoint",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("PHOTOCATALOG_G_LIFECYCLE_CASE", case)
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(
                status.success(),
                "lifecycle fixture {case} failed: {status}"
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            child.kill()?;
            let status = child.wait()?;
            anyhow::bail!("lifecycle fixture {case} timed out; checked reap {status}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}
#[cfg(unix)]
#[test]
#[ignore = "entrypoint invoked only by deadline-bound lifecycle tests"]
fn lifecycle_subprocess_entrypoint() -> Result<()> {
    match std::env::var("PHOTOCATALOG_G_LIFECYCLE_CASE")?.as_str() {
        "begin_retire" => held_begin_retire_case(),
        "start_stop" => process_case("stop"),
        "success_lost" => process_case("success"),
        "failure_lost" => process_case("failure"),
        "broken_pipe" => process_case("broken"),
        "arm_close" => real_arm_close_case(),
        "partial_begin" => real_partial_begin_case(),
        "failed_seal" => real_failed_seal_case(),
        "max_terminal" => real_maximum_terminal_case(),
        "failed_arm" => real_failed_arm_case(),
        "failed_drain_before" => real_failed_drain_before_case(),
        "failed_drain_after" => real_failed_drain_after_case(),
        "close_inflight_drain" => real_close_inflight_drain_case(),
        "writer_spawn" => process_case("writer_spawn"),
        _ => anyhow::bail!("unknown lifecycle fixture"),
    }
}
#[cfg(unix)]
#[test]
fn held_begin_retire_keeps_charge() -> Result<()> {
    subprocess_case("begin_retire")
}
#[cfg(unix)]
#[test]
fn successful_start_keeps_reserved_stop_and_status_available() -> Result<()> {
    subprocess_case("start_stop")
}
#[cfg(unix)]
#[test]
fn successful_exit_lost_drain_replays_exact_terminal() -> Result<()> {
    subprocess_case("success_lost")
}
#[cfg(unix)]
#[test]
fn concrete_failed_exit_lost_drain_replays_exact_terminal() -> Result<()> {
    subprocess_case("failure_lost")
}
#[cfg(unix)]
#[test]
fn broken_pipe_checked_wait_and_join_allow_release_and_reuse() -> Result<()> {
    subprocess_case("broken_pipe")
}
#[cfg(unix)]
#[test]
fn real_f_lost_arm_then_close_reconciles_without_c_retry() -> Result<()> {
    subprocess_case("arm_close")
}
#[cfg(unix)]
#[test]
fn real_f_cached_partial_begin_allows_captured_identity_abort() -> Result<()> {
    subprocess_case("partial_begin")
}
#[cfg(unix)]
#[test]
fn real_f_cached_failed_seal_allows_release() -> Result<()> {
    subprocess_case("failed_seal")
}

fn held_begin_retire_case() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    stages.hold_begin.store(true, Ordering::Release);
    let owner = owner(&f, stages.clone(), &pool)?;
    owner.call(&register(&f))?;
    let worker = {
        let owner = owner.clone();
        let request = f.begin.clone();
        thread::spawn(move || owner.stage_call(&request, &AtomicBool::new(false)))
    };
    stages.wait_ready_entered();
    assert!(
        owner
            .query(&Query {
                key: Key::new(&f.root, U64(9), &f.stage),
                action: QueryAction::Retire
            })
            .is_err()
    );
    assert_eq!(pool.used(), f.worker);
    assert!(pool.reserve_exact(1).is_err());
    stages.hold_begin.store(false, Ordering::Release);
    stages.hold_wake.notify_all();
    worker.join().unwrap()?;
    owner.retire_root(&f.root)?;
    assert_eq!(pool.used(), 0);
    assert_eq!(
        stages.count(|a| matches!(a, export_stage::Action::Begin { .. })),
        1
    );
    assert_eq!(
        stages.count(|a| matches!(a, export_stage::Action::Abort)),
        1
    );
    Ok(())
}

#[cfg(unix)]
fn process_case(mode: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    stages
        .drain_unknown
        .store(mode == "success" || mode == "failure", Ordering::Release);
    let executable = f._temp.path().join("synthetic-worker");
    let script = match mode {
        "broken" => {
            "#!/bin/bash\nexec 0<&-\nprintf closed > closed\nwhile [[ ! -f finish && $SECONDS -lt 8 ]]; do :; done\nexit 7\n"
        }
        "stop" => {
            "#!/bin/bash\nIFS= read -r -n 1 c\n[[ $c == '!' ]] || exit 8\nprintf started > started\nwhile [[ $SECONDS -lt 8 ]]; do :; done\nexit 9\n"
        }
        "failure" => "#!/bin/bash\nIFS= read -r -n 1 c\n[[ $c == '!' ]] || exit 8\nexit 7\n",
        _ => "#!/bin/bash\nIFS= read -r -n 1 c\n[[ $c == '!' ]] || exit 8\nexit 0\n",
    };
    std::fs::write(&executable, script)?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    let owner = Arc::new(Owner::new(executable, stages.clone(), 1, &pool)?);
    owner.bind(&f.root)?;
    owner.call(&register(&f))?;
    ready(&f, &owner)?;
    let slot = owner.slot(&f.root, U64(9), &f.stage)?;
    if mode == "writer_spawn" {
        slot.fail_writer_spawn.store(true, Ordering::Release);
    }
    let spawned = owner.call(&lifecycle(&f, Action::Spawn))?;
    assert!(spawned.pid.is_some());
    if mode == "broken" {
        wait_until(|| f._temp.path().join("closed").exists())?;
    }
    if mode != "writer_spawn" {
        owner.call(&lifecycle(&f, Action::Start))?;
    }
    if mode == "broken" {
        wait_until(|| slot.send.lock().unwrap().error.is_some())?;
        std::fs::write(f._temp.path().join("finish"), b"finish")?;
    }
    if mode == "stop" {
        wait_until(|| f._temp.path().join("started").exists())?;
        let query = Query {
            key: Key::new(&f.root, U64(9), &f.stage),
            action: QueryAction::Status,
        };
        let observer = {
            let owner = owner.clone();
            thread::spawn(move || owner.query(&query))
        };
        owner.stop_key(&Key::new(&f.root, U64(9), &f.stage))?;
        observer.join().unwrap()?;
    }
    wait_until(|| {
        slot.reaper
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| h.is_finished())
    })?;
    if mode == "success" || mode == "failure" {
        assert_eq!(slot.status().phase, Phase::WaitFailed);
        assert_eq!(
            slot.status().exit_code,
            Some(if mode == "success" { 0 } else { 7 })
        );
        assert_eq!(pool.used(), f.worker);
        stages.drain_unknown.store(false, Ordering::Release);
        owner.call(&lifecycle(&f, Action::RetryDrain))?;
        wait_until(|| {
            slot.status().phase == Phase::Drained
                && slot
                    .reaper
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|h| h.is_finished())
        })?;
        let calls = stages.calls.lock().unwrap();
        let drains: Vec<_> = calls
            .iter()
            .filter(|r| matches!(r.action, export_stage::Action::NativeDrained { .. }))
            .collect();
        assert_eq!(drains.len(), 2);
        assert_eq!(drains[0].digest()?, drains[1].digest()?);
        match &drains[0].action {
            export_stage::Action::NativeDrained {
                terminal: export_stage::NativeTerminal::Succeeded,
                ..
            } => assert_eq!(mode, "success"),
            export_stage::Action::NativeDrained {
                terminal: export_stage::NativeTerminal::Failed { code: Some(7) },
                ..
            } => assert_eq!(mode, "failure"),
            _ => panic!("changed checked exit terminal"),
        }
    }
    assert_eq!(slot.status().phase, Phase::Drained);
    assert!(slot.child.lock().unwrap().is_none());
    assert!(slot.writer.lock().unwrap().is_none());
    if mode == "broken" {
        assert!(slot.status().error.unwrap().contains("stdin failed"));
    }
    if mode == "writer_spawn" {
        assert!(
            slot.status()
                .error
                .unwrap()
                .contains("writer thread creation failure")
        );
        let send = slot.send.lock().unwrap();
        assert!(send.input.is_none() && send.done && !send.join_failed);
    }
    owner.stage_call(
        &ordinary(&f, 3, export_stage::Action::Release),
        &AtomicBool::new(false),
    )?;
    owner.call(&lifecycle(&f, Action::Retire))?;
    assert_eq!(pool.used(), 0);
    assert_eq!(pool.reserve_exact(f.worker)?.bytes(), f.worker);
    Ok(())
}
fn real_arm_close_case() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = RealStages::new(&f);
    let owner = real_owner(&f, stages.clone(), &pool)?;
    ready(&f, &owner)?;
    stages.lose_arm.store(true, Ordering::Release);
    assert!(owner.call(&lifecycle(&f, Action::Spawn)).is_err());
    owner.retire_root(&f.root)?;
    assert!(stages.owner.lock().unwrap().empty());
    assert_eq!(pool.used(), 0);
    let calls = stages.calls.lock().unwrap();
    let arms: Vec<_> = calls
        .iter()
        .filter(|r| matches!(r.action, export_stage::Action::Arm { .. }))
        .collect();
    assert_eq!(arms.len(), 2);
    assert_eq!(arms[0].digest()?, arms[1].digest()?);
    assert!(
        !calls
            .iter()
            .any(|r| matches!(r.action, export_stage::Action::Abort))
    );
    assert_eq!(
        calls
            .iter()
            .filter(|r| matches!(r.action, export_stage::Action::Release))
            .count(),
        1
    );
    Ok(())
}
fn real_partial_begin_case() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = RealStages::new(&f);
    let owner = real_owner(&f, stages.clone(), &pool)?;
    stages.fail_begin.store(true, Ordering::Release);
    let failed = owner
        .stage_call(&f.begin, &AtomicBool::new(false))
        .unwrap_err()
        .downcast::<Failure>()?;
    assert!(bound_failure(&failed, &f.begin));
    assert_eq!(failed.kind, FailureKind::Unknown);
    let replay = stages
        .owner
        .lock()
        .unwrap()
        .call(&stages.manifest, &f.begin, &AtomicBool::new(false))
        .unwrap_err()
        .downcast::<Failure>()?;
    assert_eq!(serde_json::to_vec(&failed)?, serde_json::to_vec(&replay)?);
    assert!(!stages.owner.lock().unwrap().empty());
    assert_eq!(pool.used(), f.worker);
    owner.retire_root(&f.root)?;
    assert!(stages.owner.lock().unwrap().empty());
    assert_eq!(pool.used(), 0);
    let calls = stages.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(matches!(calls[1].action, export_stage::Action::Abort));
    assert_eq!(calls[1].operation, U64(2));
    Ok(())
}
fn real_failed_seal_case() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = RealStages::new(&f);
    let owner = real_owner(&f, stages.clone(), &pool)?;
    ready(&f, &owner)?;
    let slot = owner.slot(&f.root, U64(9), &f.stage)?;
    slot.supervisor_call(export_stage::Action::Arm { native: U64(9) })?;
    // No process is launched by this fixture. F's actual lease reacquisition
    // and missing result.json exercise its cached failed-seal path.
    fixture_checked_drain(&slot, export_stage::NativeTerminal::Succeeded)?;
    slot.set_phase(Phase::Drained);
    let seal = ordinary(&f, 3, export_stage::Action::ResultAndSeal);
    stages.lose_seal.store(true, Ordering::Release);
    assert!(owner.stage_call(&seal, &AtomicBool::new(false)).is_err());
    owner.retire_root(&f.root)?;
    assert!(stages.owner.lock().unwrap().empty());
    assert_eq!(pool.used(), 0);
    let calls = stages.calls.lock().unwrap();
    let seals: Vec<_> = calls
        .iter()
        .filter(|r| matches!(r.action, export_stage::Action::ResultAndSeal))
        .collect();
    assert_eq!(seals.len(), 2);
    assert_eq!(seals[0].digest()?, seals[1].digest()?);
    assert!(matches!(
        calls.last().unwrap().action,
        export_stage::Action::Release
    ));
    assert_eq!(calls.last().unwrap().operation, U64(4));
    Ok(())
}

#[test]
fn streaming_error_bound_stops_formatting_and_preserves_utf8() {
    struct Huge;
    impl std::fmt::Display for Huge {
        fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for _ in 0..usize::MAX {
                std::fmt::Write::write_str(out, "界")?;
            }
            Ok(())
        }
    }
    let result = bounded(Huge);
    assert_eq!(result.len(), ERROR_BYTES / 3 * 3);
}
#[test]
fn mismatched_failure_receipt_retains_exact_pending_and_native_charge() -> Result<()> {
    struct BadReceipt(Mutex<Failure>);
    impl Stages for BadReceipt {
        fn call(&self, _: &export_stage::Request, _: &AtomicBool) -> Result<export_stage::Reply> {
            Err(self.0.lock().unwrap().clone().into())
        }
    }
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let mut value = Failure::new(FailureKind::Rejected, "foreign receipt");
    value.object_receipt = Some(crate::catalog_session::preview_io::FailureReceipt {
        operation: U64(2),
        step: U64(0),
        request_digest: f.begin.digest()?,
    });
    let stages = Arc::new(BadReceipt(Mutex::new(value)));
    let owner = Owner::new(f._temp.path().join("unused"), stages.clone(), 1, &pool)?;
    owner.bind(&f.root)?;
    owner.call(&register(&f))?;
    for changed in [false, true] {
        if changed {
            let mut value = stages.0.lock().unwrap();
            let receipt = value.object_receipt.as_mut().unwrap();
            receipt.operation = U64(1);
            receipt.request_digest = [0; 32];
        }
        assert!(owner.stage_call(&f.begin, &AtomicBool::new(false)).is_err());
        let status = owner.query(&Query {
            key: Key::new(&f.root, U64(9), &f.stage),
            action: QueryAction::Status,
        })?;
        assert_eq!(status.stage_high_water, U64(0));
        assert_eq!(status.pending_dispatch, DispatchState::Unknown);
        assert!(owner.call(&lifecycle(&f, Action::Retire)).is_err());
        assert_eq!(pool.used(), f.worker);
    }
    {
        let mut value = stages.0.lock().unwrap();
        value.kind = FailureKind::Canceled;
        value.object_receipt.as_mut().unwrap().request_digest = f.begin.digest()?;
    }
    assert!(owner.stage_call(&f.begin, &AtomicBool::new(false)).is_err());
    owner.retire_root(&f.root)?;
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn maximum_enriched_receipt_and_pending_release_retain_full_graph_until_exact_replay() -> Result<()>
{
    subprocess_case("max_terminal")
}
fn real_maximum_terminal_case() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = RealStages::new(&f);
    let owner = real_owner(&f, stages.clone(), &pool)?;
    ready(&f, &owner)?;
    let slot = owner.slot(&f.root, U64(9), &f.stage)?;
    let path = match &slot
        .stage_state
        .lock()
        .unwrap()
        .last
        .as_ref()
        .unwrap()
        .outcome
        .as_ref()
        .unwrap()
        .value
    {
        export_stage::Value::Ready { path } => path.to_path()?,
        _ => unreachable!(),
    };
    slot.supervisor_call(export_stage::Action::Arm { native: U64(9) })?;
    let export_stage::Action::Begin { work, .. } = &f.begin.action else {
        unreachable!()
    };
    crate::filesystem_worker::export_stage_test_support::maximum_receipt(work, &path)?;
    fixture_checked_drain(&slot, export_stage::NativeTerminal::Succeeded)?;
    slot.set_phase(Phase::Drained);
    let seal = ordinary(&f, 3, export_stage::Action::ResultAndSeal);
    let completed = owner.stage_call(&seal, &AtomicBool::new(false))?;
    assert!(serde_json::to_vec(&completed)?.len() > export_stage::RECEIPT_BYTES);
    crate::application::desktop::admit_export_stage_reply(&completed)?;
    let f_envelope = crate::filesystem_worker::wire::encode_outcome(&Ok(
        crate::filesystem_worker::wire::Response::ExportStage(completed.clone()),
    ))?;
    let replay = owner.stage_call(&seal, &AtomicBool::new(false))?;
    assert_eq!(
        serde_json::to_vec(&completed)?,
        serde_json::to_vec(&replay)?
    );
    stages.lose_release.store(true, Ordering::Release);
    let release = ordinary(&f, 4, export_stage::Action::Release);
    assert!(owner.stage_call(&release, &AtomicBool::new(false)).is_err());
    {
        let state = slot.stage_state.lock().unwrap();
        assert_eq!(
            state.pending.as_ref().unwrap().request.digest()?,
            release.digest()?
        );
        let retained = state.last.as_ref().unwrap().outcome.as_ref().unwrap();
        assert_eq!(
            serde_json::to_vec(retained)?,
            serde_json::to_vec(&completed)?
        );
        let supervisor = state
            .supervisor_last
            .as_ref()
            .context("retained supervisor terminal graph")?;
        assert_eq!(supervisor.operation, 2);
        assert!(supervisor.outcome.is_ok() && state.drain == DrainState::Acknowledged);
    }
    assert_eq!(pool.used(), f.worker);
    assert!(stages.owner.lock().unwrap().empty());
    drop((completed, replay, f_envelope));
    assert_eq!(pool.used(), f.worker);
    owner.retire_root(&f.root)?;
    assert_eq!(pool.used(), 0);
    let calls = stages.calls.lock().unwrap();
    let releases: Vec<_> = calls
        .iter()
        .filter(|r| matches!(r.action, export_stage::Action::Release))
        .collect();
    assert_eq!(releases.len(), 2);
    assert_eq!(releases[0].digest()?, releases[1].digest()?);
    Ok(())
}

#[test]
fn exhausted_cleanup_sequence_retains_custody_without_dispatch() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages.clone(), &pool)?;
    owner.call(&register(&f))?;
    owner.stage_call(&f.begin, &AtomicBool::new(false))?;
    let slot = owner.slot(&f.root, U64(9), &f.stage)?;
    slot.stage_state.lock().unwrap().high_water = u64::MAX;
    assert!(
        owner
            .retire_root(&f.root)
            .unwrap_err()
            .to_string()
            .contains("sequence exhausted")
    );
    assert_eq!(pool.used(), f.worker);
    assert_eq!(
        stages.count(|a| matches!(a, export_stage::Action::Abort)),
        0
    );
    // Restore only this synthetic sequence fault so the fixture retires its charge.
    slot.stage_state.lock().unwrap().high_water = 1;
    owner.retire_root(&f.root)?;
    assert_eq!(pool.used(), 0);
    Ok(())
}

fn fixture_checked_drain(slot: &Slot, terminal: export_stage::NativeTerminal) -> Result<()> {
    // These in-process F-only fixtures deliberately never create an OS child.
    *slot.terminal.lock().unwrap() = Some(terminal.clone());
    slot.send.lock().unwrap().done = true;
    slot.drain_stage(terminal)
}

#[cfg(unix)]
#[test]
fn real_f_failed_arm_is_terminal_and_permits_unarmed_abort() -> Result<()> {
    subprocess_case("failed_arm")
}
#[cfg(unix)]
#[test]
fn real_f_drain_lease_refusal_retains_charge_then_recovers_after_unlock() -> Result<()> {
    subprocess_case("failed_drain_before")
}
#[cfg(unix)]
#[test]
fn real_f_post_drain_invalidation_and_lost_failure_reply_allow_exact_cleanup() -> Result<()> {
    subprocess_case("failed_drain_after")
}
#[cfg(unix)]
#[test]
fn close_racing_real_f_drain_adopts_completed_identity_without_redispatch() -> Result<()> {
    subprocess_case("close_inflight_drain")
}
#[cfg(unix)]
#[test]
fn writer_thread_creation_failure_closes_pipe_before_drain_and_pool_reuse() -> Result<()> {
    subprocess_case("writer_spawn")
}

fn fixture_ready_path(slot: &Slot) -> Result<PathBuf> {
    let stage = slot.stage_state.lock().unwrap();
    match &stage
        .last
        .as_ref()
        .context("Ready result")?
        .outcome
        .as_ref()
        .map_err(failure_error)?
        .value
    {
        export_stage::Value::Ready { path } => Ok(path.to_path()?),
        _ => anyhow::bail!("fixture expected Ready path"),
    }
}
fn real_failed_arm_case() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = RealStages::new(&f);
    let owner = real_owner(&f, stages.clone(), &pool)?;
    ready(&f, &owner)?;
    let slot = owner.slot(&f.root, U64(9), &f.stage)?;
    let path = fixture_ready_path(&slot)?;
    let original = std::fs::read(path.join("request.json"))?;
    std::fs::write(path.join("request.json"), b"changed staged input")?;
    let failure = owner
        .call(&lifecycle(&f, Action::Spawn))
        .unwrap_err()
        .downcast::<Failure>()?;
    let arm = stages.calls.lock().unwrap().last().unwrap().clone();
    assert!(matches!(arm.action, export_stage::Action::Arm { .. }));
    assert!(bound_failure(&failure, &arm));
    {
        let stage = slot.stage_state.lock().unwrap();
        assert!(!stage.armed && stage.supervisor_pending.is_none());
        assert_eq!(stage.supervisor_next, 2);
        assert_eq!(
            stage.supervisor_last.as_ref().unwrap().digest,
            arm.digest()?
        );
    }
    assert_eq!(pool.used(), f.worker);
    assert!(slot.status().pid.is_none());
    std::fs::write(path.join("request.json"), original)?;
    let cached = stages
        .owner
        .lock()
        .unwrap()
        .call(&stages.manifest, &arm, &AtomicBool::new(false))
        .unwrap_err()
        .downcast::<Failure>()?;
    assert_eq!(serde_json::to_vec(&cached)?, serde_json::to_vec(&failure)?);
    owner.retire_root(&f.root)?;
    let calls = stages.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|r| matches!(r.action, export_stage::Action::Arm { .. }))
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|r| matches!(r.action, export_stage::Action::Abort))
            .count(),
        1
    );
    assert!(!calls.iter().any(|r| matches!(
        r.action,
        export_stage::Action::NativeDrained { .. } | export_stage::Action::Release
    )));
    assert!(stages.owner.lock().unwrap().empty());
    assert_eq!(pool.used(), 0);
    Ok(())
}
fn real_failed_drain_before_case() -> Result<()> {
    use fs2::FileExt;
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = RealStages::new(&f);
    let owner = real_owner(&f, stages.clone(), &pool)?;
    ready(&f, &owner)?;
    let slot = owner.slot(&f.root, U64(9), &f.stage)?;
    let path = fixture_ready_path(&slot)?;
    assert_eq!(
        owner.call(&lifecycle(&f, Action::Spawn))?.phase,
        Phase::WaitFailed
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.join("active.lock"))?;
    lock.try_lock_exclusive()?;
    owner.call(&lifecycle(&f, Action::RetryDrain))?;
    wait_until(|| {
        slot.reaper
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| h.is_finished())
    })?;
    {
        let stage = slot.stage_state.lock().unwrap();
        assert!(stage.drain == DrainState::Failed && stage.supervisor_pending.is_none());
        assert!(stage.supervisor_last.as_ref().unwrap().outcome.is_err());
    }
    slot.checked_native_retired()?;
    let failure = owner
        .retire_root(&f.root)
        .unwrap_err()
        .downcast::<Failure>()?;
    let release = stages.calls.lock().unwrap().last().unwrap().clone();
    assert!(matches!(release.action, export_stage::Action::Release));
    assert!(bound_failure(&failure, &release));
    assert!(slot.stage_state.lock().unwrap().drain == DrainState::RetryAfterRelease);
    assert_eq!(pool.used(), f.worker);
    assert!(!stages.owner.lock().unwrap().empty());
    assert!(path.exists());
    FileExt::unlock(&lock)?;
    drop(lock);
    owner.retire_root(&f.root)?;
    assert!(stages.owner.lock().unwrap().empty());
    assert_eq!(pool.used(), 0);
    assert_eq!(pool.reserve_exact(f.worker)?.bytes(), f.worker);
    let calls = stages.calls.lock().unwrap();
    let drains: Vec<_> = calls
        .iter()
        .filter(|r| matches!(r.action, export_stage::Action::NativeDrained { .. }))
        .collect();
    assert_eq!(drains.len(), 2);
    assert_eq!((drains[0].operation, drains[1].operation), (U64(2), U64(3)));
    let releases: Vec<_> = calls
        .iter()
        .filter(|r| matches!(r.action, export_stage::Action::Release))
        .collect();
    assert_eq!(releases.len(), 2);
    assert_eq!(
        (releases[0].operation, releases[1].operation),
        (U64(3), U64(4))
    );
    Ok(())
}
#[cfg(unix)]
fn real_synthetic_owner(
    f: &Fixture,
    stages: Arc<RealStages>,
    pool: &ByteBudget,
) -> Result<Arc<Owner>> {
    use std::os::unix::fs::PermissionsExt;
    let executable = f._temp.path().join("real-f-synthetic-native");
    std::fs::write(
        &executable,
        "#!/bin/bash\nIFS= read -r -n 1 c\n[[ $c == '!' ]] || exit 8\nexit 0\n",
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    let owner = Arc::new(Owner::new(executable, stages, 1, pool)?);
    owner.bind(&f.root)?;
    owner.call(&register(f))?;
    ready(f, &owner)?;
    Ok(owner)
}
#[cfg(unix)]
fn real_failed_drain_after_case() -> Result<()> {
    for lost_reply in [false, true] {
        let f = fixture()?;
        let pool = ByteBudget::new(f.worker)?;
        let stages = RealStages::new(&f);
        let owner = real_synthetic_owner(&f, stages.clone(), &pool)?;
        let slot = owner.slot(&f.root, U64(9), &f.stage)?;
        let path = fixture_ready_path(&slot)?;
        owner.call(&lifecycle(&f, Action::Spawn))?;
        let original = std::fs::read(path.join("request.json"))?;
        std::fs::write(path.join("request.json"), b"changed after Arm")?;
        stages.lose_drain.store(lost_reply, Ordering::Release);
        owner.call(&lifecycle(&f, Action::Start))?;
        wait_until(|| {
            slot.reaper
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|h| h.is_finished())
        })?;
        assert_eq!(slot.status().phase, Phase::WaitFailed);
        assert_eq!(slot.status().exit_code, Some(0));
        slot.checked_native_retired()?;
        let drain = stages.calls.lock().unwrap().last().unwrap().clone();
        {
            let state = slot.stage_state.lock().unwrap();
            if lost_reply {
                assert_eq!(
                    state.supervisor_pending.as_ref().unwrap().digest()?,
                    drain.digest()?
                );
            } else {
                assert!(state.supervisor_pending.is_none() && state.drain == DrainState::Failed);
            }
        }
        assert_eq!(pool.used(), f.worker);
        assert!(!stages.owner.lock().unwrap().empty());
        std::fs::write(path.join("request.json"), original)?;
        // Restoration does not rewrite F's cached failed terminal.
        let cached = stages
            .owner
            .lock()
            .unwrap()
            .call(&stages.manifest, &drain, &AtomicBool::new(false))
            .unwrap_err()
            .downcast::<Failure>()?;
        assert!(bound_failure(&cached, &drain));
        assert_eq!(cached.kind, FailureKind::Unknown);
        owner.retire_root(&f.root)?;
        assert!(stages.owner.lock().unwrap().empty());
        assert_eq!(pool.used(), 0);
        let calls = stages.calls.lock().unwrap();
        let drains: Vec<_> = calls
            .iter()
            .filter(|r| matches!(r.action, export_stage::Action::NativeDrained { .. }))
            .collect();
        assert_eq!(drains.len(), if lost_reply { 2 } else { 1 });
        assert!(
            drains
                .iter()
                .all(|r| r.digest().unwrap() == drain.digest().unwrap())
        );
        assert_eq!(
            calls
                .iter()
                .filter(|r| matches!(r.action, export_stage::Action::Release))
                .count(),
            1
        );
    }
    Ok(())
}
#[cfg(unix)]
fn real_close_inflight_drain_case() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = RealStages::new(&f);
    let owner = real_synthetic_owner(&f, stages.clone(), &pool)?;
    let slot = owner.slot(&f.root, U64(9), &f.stage)?;
    stages.hold_drain.store(true, Ordering::Release);
    owner.call(&lifecycle(&f, Action::Spawn))?;
    owner.call(&lifecycle(&f, Action::Start))?;
    wait_until(|| stages.drain_entered.load(Ordering::Acquire))?;
    let pending = slot
        .stage_state
        .lock()
        .unwrap()
        .supervisor_pending
        .clone()
        .unwrap();
    let close = {
        let owner = owner.clone();
        let root = f.root.clone();
        thread::spawn(move || owner.retire_root(&root))
    };
    wait_until(|| slot.lifecycle.try_lock().is_err())?;
    assert_eq!(pool.used(), f.worker);
    stages.hold_drain.store(false, Ordering::Release);
    stages.drain_wake.notify_all();
    close.join().unwrap()?;
    slot.checked_native_retired()?;
    assert!(slot.reaper.lock().unwrap().is_none());
    assert!(stages.owner.lock().unwrap().empty());
    assert_eq!(pool.used(), 0);
    assert_eq!(pool.reserve_exact(f.worker)?.bytes(), f.worker);
    let calls = stages.calls.lock().unwrap();
    let drains: Vec<_> = calls
        .iter()
        .filter(|r| matches!(r.action, export_stage::Action::NativeDrained { .. }))
        .collect();
    assert_eq!(drains.len(), 1);
    assert_eq!(drains[0].digest()?, pending.digest()?);
    assert_eq!(
        calls
            .iter()
            .filter(|r| matches!(r.action, export_stage::Action::Release))
            .count(),
        1
    );
    Ok(())
}
