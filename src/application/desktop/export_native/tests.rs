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
use fs2::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

fn fake_executor_reply(
    request: &crate::catalog_session::export_executor::Request,
) -> Result<crate::catalog_session::export_executor::Reply> {
    use crate::catalog_session::export_executor as e;
    let value = match &request.action {
        e::Action::Acquire => e::Value::Acquired,
        e::Action::Recover { .. } => e::Value::Recovery {
            scanned: U64(0),
            cleaned: U64(0),
            retained: U64(0),
            retained_example: None,
            candidate: None,
        },
        e::Action::Discard { .. } => e::Value::Discarded { candidate: None },
        e::Action::Release => e::Value::Released,
    };
    let reply = e::Reply {
        root: request.root.clone(),
        executor: request.executor.clone(),
        operation: request.operation,
        request_digest: request.digest()?,
        value,
    };
    reply.validate(request)?;
    Ok(reply)
}

pub(crate) struct FakeStages {
    path: PathBuf,
    calls: Mutex<Vec<export_stage::Request>>,
    executor_calls: Mutex<Vec<crate::catalog_session::export_executor::Request>>,
    hold_ready: AtomicBool,
    hold_begin: AtomicBool,
    hold_arm: AtomicBool,
    ready_entered: AtomicBool,
    hold_wake: Condvar,
    hold: Mutex<()>,
    drain_unknown: AtomicBool,
    begin_rejected: AtomicBool,
    executor_unknown: AtomicBool,
    seal_attempts: AtomicUsize,
}
impl FakeStages {
    pub(crate) fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            calls: Mutex::new(Vec::new()),
            executor_calls: Mutex::new(Vec::new()),
            hold_ready: AtomicBool::new(false),
            hold_begin: AtomicBool::new(false),
            hold_arm: AtomicBool::new(false),
            ready_entered: AtomicBool::new(false),
            hold_wake: Condvar::new(),
            hold: Mutex::new(()),
            drain_unknown: AtomicBool::new(false),
            begin_rejected: AtomicBool::new(false),
            executor_unknown: AtomicBool::new(false),
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
    fn executor_call(
        &self,
        request: &crate::catalog_session::export_executor::Request,
        _cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::export_executor::Reply> {
        self.executor_calls.lock().unwrap().push(request.clone());
        let reply = fake_executor_reply(request)?;
        if self.executor_unknown.swap(false, Ordering::AcqRel) {
            return Err(Failure::new(FailureKind::Unknown, "lost executor reply").into());
        }
        Ok(reply)
    }
}

pub(crate) struct Fixture {
    pub(crate) _temp: tempfile::TempDir,
    pub(crate) root: RootCapability,
    pub(crate) executor: LeaseId,
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
    let executor = export_executor::executor_id(&root, 1)?;
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
        executor: executor.clone(),
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
        executor,
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
pub(crate) fn acquire(owner: &Owner, f: &Fixture) -> Result<()> {
    owner.executor_call(
        &crate::catalog_session::export_executor::Request {
            root: f.root.clone(),
            executor: f.executor.clone(),
            operation: U64(1),
            action: crate::catalog_session::export_executor::Action::Acquire,
        },
        &AtomicBool::new(false),
    )?;
    Ok(())
}
fn owner(f: &Fixture, stages: Arc<FakeStages>, pool: &ByteBudget) -> Result<Arc<Owner>> {
    let owner = Arc::new(Owner::new(
        f._temp.path().join("missing-export-worker"),
        stages,
        1,
        pool,
    )?);
    owner.bind(&f.root)?;
    acquire(&owner, f)?;
    Ok(owner)
}
pub(crate) fn ordinary(
    f: &Fixture,
    operation: u64,
    action: export_stage::Action,
) -> export_stage::Request {
    export_stage::Request {
        root: f.root.clone(),
        executor: f.executor.clone(),
        stage: f.stage.clone(),
        operation: U64(operation),
        supervisor: false,
        binding: f.binding.clone(),
        action,
    }
}

fn executor_request(
    f: &Fixture,
    executor: LeaseId,
    operation: u64,
    action: crate::catalog_session::export_executor::Action,
) -> crate::catalog_session::export_executor::Request {
    crate::catalog_session::export_executor::Request {
        root: f.root.clone(),
        executor,
        operation: U64(operation),
        action,
    }
}

fn persisted_transport(f: &Fixture, root: &Path, name: &str) -> Result<PathBuf> {
    let path = root.join(name);
    std::fs::create_dir_all(&path)?;
    std::fs::write(path.join("active.lock"), [])?;
    let export_stage::Action::Begin { work, limits } = &f.begin.action else {
        unreachable!()
    };
    let request = crate::export_worker::prepare_managed_request(work, *limits, &path)?;
    std::fs::write(path.join("request.json"), request)?;
    Ok(path)
}

#[test]
fn f_executor_replays_acquire_and_close_while_excluding_local_owner() -> Result<()> {
    use fs2::FileExt;
    let f = fixture()?;
    let catalog = f.root.canonical_root.to_path()?;
    let manifest = catalog.join("manifest");
    std::fs::create_dir_all(&manifest)?;
    let mut owner = crate::filesystem_worker::export_executor_test_support::Owner::default();
    let acquire = executor_request(
        &f,
        f.executor.clone(),
        1,
        crate::catalog_session::export_executor::Action::Acquire,
    );
    let first = owner.call(&catalog, &manifest, &acquire, &AtomicBool::new(false), true)?;
    assert_eq!(
        first,
        owner.call(&catalog, &manifest, &acquire, &AtomicBool::new(false), true,)?
    );
    let competitor = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(catalog.join("photo-export.lock"))?;
    assert!(competitor.try_lock_exclusive().is_err());
    let close = executor_request(
        &f,
        f.executor.clone(),
        2,
        crate::catalog_session::export_executor::Action::Release,
    );
    let closed = owner.call(&catalog, &manifest, &close, &AtomicBool::new(false), true)?;
    assert!(owner.empty());
    competitor.try_lock_exclusive()?;
    fs2::FileExt::unlock(&competitor)?;
    let successor = export_executor::executor_id(&f.root, 2)?;
    owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            successor.clone(),
            1,
            crate::catalog_session::export_executor::Action::Acquire,
        ),
        &AtomicBool::new(false),
        true,
    )?;
    assert_eq!(
        owner.call(&catalog, &manifest, &close, &AtomicBool::new(false), true,)?,
        closed
    );
    owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            successor,
            2,
            crate::catalog_session::export_executor::Action::Release,
        ),
        &AtomicBool::new(false),
        true,
    )?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn f_failed_staging_setup_retains_lock_until_exact_release() -> Result<()> {
    use std::os::unix::fs::symlink;
    let f = fixture()?;
    let catalog = f.root.canonical_root.to_path()?;
    let manifest = catalog.join("manifest");
    let target = catalog.join("staging-target");
    std::fs::create_dir_all(&manifest)?;
    std::fs::create_dir_all(&target)?;
    symlink(&target, catalog.join("photo-export-workers"))?;
    let mut owner = crate::filesystem_worker::export_executor_test_support::Owner::default();
    let acquire = executor_request(
        &f,
        f.executor.clone(),
        1,
        crate::catalog_session::export_executor::Action::Acquire,
    );
    assert!(
        owner
            .call(&catalog, &manifest, &acquire, &AtomicBool::new(false), true,)
            .is_err()
    );
    assert!(!owner.empty());
    let competitor = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(catalog.join("photo-export.lock"))?;
    assert!(fs2::FileExt::try_lock_exclusive(&competitor).is_err());
    owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            f.executor.clone(),
            2,
            crate::catalog_session::export_executor::Action::Release,
        ),
        &AtomicBool::new(false),
        true,
    )?;
    assert!(owner.empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn f_executor_revalidates_retained_lock_and_staging_on_exact_retry() -> Result<()> {
    let f = fixture()?;
    let catalog = f.root.canonical_root.to_path()?;
    let manifest = catalog.join("manifest");
    std::fs::create_dir_all(&manifest)?;
    let mut owner = crate::filesystem_worker::export_executor_test_support::Owner::default();
    owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            f.executor.clone(),
            1,
            crate::catalog_session::export_executor::Action::Acquire,
        ),
        &AtomicBool::new(false),
        true,
    )?;
    let recover = executor_request(
        &f,
        f.executor.clone(),
        2,
        crate::catalog_session::export_executor::Action::Recover {
            max_directories: U64(1024),
        },
    );
    let canceled = AtomicBool::new(true);
    assert!(
        owner
            .call(&catalog, &manifest, &recover, &canceled, true)
            .is_err()
    );
    owner.call(&catalog, &manifest, &recover, &AtomicBool::new(false), true)?;

    let lock = catalog.join("photo-export.lock");
    let original_lock = catalog.join("photo-export.lock.retained");
    std::fs::rename(&lock, &original_lock)?;
    std::fs::write(&lock, [])?;
    let recover_lock = executor_request(
        &f,
        f.executor.clone(),
        3,
        crate::catalog_session::export_executor::Action::Recover {
            max_directories: U64(1024),
        },
    );
    assert!(
        owner
            .call(
                &catalog,
                &manifest,
                &recover_lock,
                &AtomicBool::new(false),
                true,
            )
            .is_err()
    );
    assert!(
        owner
            .call(
                &catalog,
                &manifest,
                &executor_request(
                    &f,
                    f.executor.clone(),
                    4,
                    crate::catalog_session::export_executor::Action::Release,
                ),
                &AtomicBool::new(false),
                true,
            )
            .is_err()
    );
    std::fs::remove_file(&lock)?;
    std::fs::rename(&original_lock, &lock)?;
    owner.call(
        &catalog,
        &manifest,
        &recover_lock,
        &AtomicBool::new(false),
        true,
    )?;

    let staging = catalog.join("photo-export-workers");
    let original_staging = catalog.join("photo-export-workers.retained");
    std::fs::rename(&staging, &original_staging)?;
    std::fs::create_dir(&staging)?;
    let recover_staging = executor_request(
        &f,
        f.executor.clone(),
        4,
        crate::catalog_session::export_executor::Action::Recover {
            max_directories: U64(1024),
        },
    );
    assert!(
        owner
            .call(
                &catalog,
                &manifest,
                &recover_staging,
                &AtomicBool::new(false),
                true,
            )
            .is_err()
    );
    std::fs::remove_dir(&staging)?;
    std::fs::rename(&original_staging, &staging)?;
    owner.call(
        &catalog,
        &manifest,
        &recover_staging,
        &AtomicBool::new(false),
        true,
    )?;
    owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            f.executor.clone(),
            5,
            crate::catalog_session::export_executor::Action::Release,
        ),
        &AtomicBool::new(false),
        true,
    )?;
    Ok(())
}

#[test]
fn f_combined_inventory_refuses_over_bound_before_first_fence() -> Result<()> {
    let f = fixture()?;
    let catalog = f.root.canonical_root.to_path()?;
    let manifest = catalog.join("manifest");
    std::fs::create_dir_all(&manifest)?;
    let mut owner = crate::filesystem_worker::export_executor_test_support::Owner::default();
    owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            f.executor.clone(),
            1,
            crate::catalog_session::export_executor::Action::Acquire,
        ),
        &AtomicBool::new(false),
        true,
    )?;
    let catalog_workers = catalog.join("photo-export-workers");
    for _ in 0..1024 {
        std::fs::create_dir(
            catalog_workers.join(format!("photo-worker-{}", uuid::Uuid::new_v4())),
        )?;
    }
    let manifest_workers = manifest.join("export-workers");
    std::fs::create_dir_all(&manifest_workers)?;
    let overflow = manifest_workers.join(format!("photo-worker-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&overflow)?;
    let reply = owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            f.executor.clone(),
            2,
            crate::catalog_session::export_executor::Action::Recover {
                max_directories: U64(1024),
            },
        ),
        &AtomicBool::new(false),
        true,
    )?;
    let crate::catalog_session::export_executor::Value::Recovery {
        scanned,
        cleaned,
        retained,
        candidate,
        ..
    } = reply.value
    else {
        unreachable!()
    };
    assert_eq!((scanned, cleaned, retained), (U64(1024), U64(0), U64(1)));
    assert!(candidate.is_none() && !overflow.join("active.lock").exists());
    assert_eq!(
        std::fs::read_dir(&catalog_workers)?.count(),
        1024,
        "no catalog entry was fenced"
    );
    owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            f.executor.clone(),
            3,
            crate::catalog_session::export_executor::Action::Release,
        ),
        &AtomicBool::new(false),
        true,
    )?;
    Ok(())
}

#[test]
fn f_discard_replay_returns_the_original_next_candidate_once() -> Result<()> {
    let f = fixture()?;
    let catalog = f.root.canonical_root.to_path()?;
    let manifest = catalog.join("manifest");
    std::fs::create_dir_all(&manifest)?;
    let mut owner = crate::filesystem_worker::export_executor_test_support::Owner::default();
    owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            f.executor.clone(),
            1,
            crate::catalog_session::export_executor::Action::Acquire,
        ),
        &AtomicBool::new(false),
        true,
    )?;
    let catalog_workers = catalog.join("photo-export-workers");
    let manifest_workers = manifest.join("export-workers");
    std::fs::create_dir_all(&manifest_workers)?;
    persisted_transport(
        &f,
        &catalog_workers,
        &format!("photo-worker-{}", uuid::Uuid::new_v4()),
    )?;
    persisted_transport(
        &f,
        &manifest_workers,
        &format!("photo-worker-{}", uuid::Uuid::new_v4()),
    )?;
    let recovered = owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            f.executor.clone(),
            2,
            crate::catalog_session::export_executor::Action::Recover {
                max_directories: U64(1024),
            },
        ),
        &AtomicBool::new(false),
        true,
    )?;
    let crate::catalog_session::export_executor::Value::Recovery {
        candidate: Some(first),
        retained,
        ..
    } = recovered.value
    else {
        unreachable!()
    };
    assert_eq!(retained, U64(0));
    let discard = executor_request(
        &f,
        f.executor.clone(),
        3,
        crate::catalog_session::export_executor::Action::Discard { token: first.token },
    );
    let discarded = owner.call(&catalog, &manifest, &discard, &AtomicBool::new(false), true)?;
    let crate::catalog_session::export_executor::Value::Discarded {
        candidate: Some(next),
    } = &discarded.value
    else {
        unreachable!()
    };
    let remaining = std::fs::read_dir(&catalog_workers)?.count()
        + std::fs::read_dir(&manifest_workers)?.count();
    assert_eq!(remaining, 1);
    assert_eq!(
        owner.call(&catalog, &manifest, &discard, &AtomicBool::new(false), true,)?,
        discarded
    );
    assert_eq!(
        std::fs::read_dir(&catalog_workers)?.count()
            + std::fs::read_dir(&manifest_workers)?.count(),
        remaining
    );
    owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            f.executor.clone(),
            4,
            crate::catalog_session::export_executor::Action::Discard {
                token: next.token.clone(),
            },
        ),
        &AtomicBool::new(false),
        true,
    )?;
    assert_eq!(
        std::fs::read_dir(&catalog_workers)?.count()
            + std::fs::read_dir(&manifest_workers)?.count(),
        0
    );
    owner.call(
        &catalog,
        &manifest,
        &executor_request(
            &f,
            f.executor.clone(),
            5,
            crate::catalog_session::export_executor::Action::Release,
        ),
        &AtomicBool::new(false),
        true,
    )?;
    Ok(())
}

#[test]
fn g_serializes_identical_executor_calls_and_retains_one_predecessor_close() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = Arc::new(Owner::new(
        f._temp.path().join("missing-worker"),
        stages.clone(),
        1,
        &pool,
    )?);
    owner.bind(&f.root)?;
    let acquire = executor_request(
        &f,
        f.executor.clone(),
        1,
        crate::catalog_session::export_executor::Action::Acquire,
    );
    let mut joins = Vec::new();
    for _ in 0..2 {
        let owner = owner.clone();
        let request = acquire.clone();
        joins.push(thread::spawn(move || {
            owner.executor_call(&request, &AtomicBool::new(false))
        }));
    }
    let acquired: Vec<_> = joins
        .into_iter()
        .map(|join| join.join().expect("executor caller"))
        .collect::<Result<_>>()?;
    assert_eq!(acquired[0], acquired[1]);
    assert_eq!(stages.executor_calls.lock().unwrap().len(), 1);

    let close = executor_request(
        &f,
        f.executor.clone(),
        2,
        crate::catalog_session::export_executor::Action::Release,
    );
    let mut joins = Vec::new();
    for _ in 0..2 {
        let owner = owner.clone();
        let request = close.clone();
        joins.push(thread::spawn(move || {
            owner.executor_call(&request, &AtomicBool::new(false))
        }));
    }
    let closed: Vec<_> = joins
        .into_iter()
        .map(|join| join.join().expect("executor caller"))
        .collect::<Result<_>>()?;
    assert_eq!(closed[0], closed[1]);
    assert_eq!(stages.executor_calls.lock().unwrap().len(), 2);

    let successor = export_executor::executor_id(&f.root, 2)?;
    owner.executor_call(
        &executor_request(
            &f,
            successor.clone(),
            1,
            crate::catalog_session::export_executor::Action::Acquire,
        ),
        &AtomicBool::new(false),
    )?;
    assert_eq!(
        owner.executor_call(&close, &AtomicBool::new(false))?,
        closed[0]
    );
    assert_eq!(stages.executor_calls.lock().unwrap().len(), 3);
    owner.executor_call(
        &executor_request(
            &f,
            successor,
            2,
            crate::catalog_session::export_executor::Action::Release,
        ),
        &AtomicBool::new(false),
    )?;
    assert_eq!(stages.executor_calls.lock().unwrap().len(), 4);
    Ok(())
}

#[test]
fn g_replays_lost_stage_release_before_executor_close_and_reacquires() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = RealStages::new(&f);
    let owner = real_owner(&f, stages.clone(), &pool)?;
    ready(&f, &owner)?;
    assert_eq!(
        owner.call(&lifecycle(&f, Action::Spawn))?.phase,
        Phase::WaitFailed
    );
    stages.lose_release.store(true, Ordering::Release);
    let close = executor_request(
        &f,
        f.executor.clone(),
        2,
        crate::catalog_session::export_executor::Action::Release,
    );
    assert!(
        owner
            .executor_call(&close, &AtomicBool::new(false))
            .is_err()
    );
    assert_eq!(pool.used(), f.worker);
    let closed = owner.executor_call(&close, &AtomicBool::new(false))?;
    assert_eq!(pool.used(), 0);
    assert!(stages.owner.lock().unwrap().empty());
    let releases: Vec<_> = stages
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|request| matches!(request.action, export_stage::Action::Release))
        .cloned()
        .collect();
    assert_eq!(releases.len(), 2);
    assert_eq!(releases[0].digest()?, releases[1].digest()?);

    let successor = export_executor::executor_id(&f.root, 2)?;
    owner.executor_call(
        &executor_request(
            &f,
            successor.clone(),
            1,
            crate::catalog_session::export_executor::Action::Acquire,
        ),
        &AtomicBool::new(false),
    )?;
    assert_eq!(
        owner.executor_call(&close, &AtomicBool::new(false))?,
        closed
    );
    let successor_stage = LeaseId::new();
    let mut successor_begin = f.begin.clone();
    successor_begin.executor = successor.clone();
    successor_begin.stage = successor_stage.clone();
    let mut successor_registration = register(&f);
    successor_registration.operation = U64(10);
    successor_registration.stage = successor_stage;
    let Action::Register { begin, .. } = &mut successor_registration.action else {
        unreachable!()
    };
    **begin = successor_begin.clone();
    owner.call(&successor_registration)?;
    owner.stage_call(&successor_begin, &AtomicBool::new(false))?;
    owner.executor_call(
        &executor_request(
            &f,
            successor,
            2,
            crate::catalog_session::export_executor::Action::Release,
        ),
        &AtomicBool::new(false),
    )?;
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn g_close_before_begin_records_replayable_never_dispatched_without_f_receipt() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages.clone(), &pool)?;
    owner.call(&register(&f))?;
    let slot = owner.slot(&f.root, U64(9), &f.stage)?;

    // These three guards form deterministic scheduler barriers. Close cannot
    // enter admission until both callers exist; after it marks the executor
    // closing, cleanup cannot pass lifecycle and Begin cannot pass stage state.
    let lifecycle = slot.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
    let stage = slot.stage_state.lock().unwrap_or_else(|p| p.into_inner());
    let admission = owner.admission.lock().unwrap_or_else(|p| p.into_inner());
    let close_request = executor_request(
        &f,
        f.executor.clone(),
        2,
        crate::catalog_session::export_executor::Action::Release,
    );
    let close = {
        let owner = owner.clone();
        thread::spawn(move || owner.executor_call(&close_request, &AtomicBool::new(false)))
    };
    let begin_request = f.begin.clone();
    let begin = {
        let owner = owner.clone();
        thread::spawn(move || owner.stage_call(&begin_request, &AtomicBool::new(false)))
    };
    drop(admission);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if owner
            .executor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active
            .as_ref()
            .is_some_and(|active| active.closing)
        {
            break;
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "executor Close admission deadline"
        );
        thread::yield_now();
    }
    drop(stage);

    let first = begin
        .join()
        .expect("Begin caller")
        .unwrap_err()
        .downcast::<Failure>()?;
    assert_eq!(first.kind, FailureKind::Canceled);
    assert!(first.object_receipt.is_none());
    let replay = owner
        .stage_call(&f.begin, &AtomicBool::new(false))
        .unwrap_err()
        .downcast::<Failure>()?;
    assert_eq!(replay.kind, first.kind);
    assert_eq!(replay.message, first.message);
    assert!(replay.object_receipt.is_none());

    {
        let stage = slot.stage_state.lock().unwrap_or_else(|p| p.into_inner());
        let completed = stage.last.as_ref().context("G-local Begin result")?;
        assert_eq!(completed.operation, f.begin.operation.0);
        assert_eq!(completed.digest, f.begin.digest()?);
        assert_eq!(completed.dispatch, DispatchState::NeverDispatched);
        assert!(
            completed
                .outcome
                .as_ref()
                .unwrap_err()
                .object_receipt
                .is_none()
        );
        assert!(stage.pending.is_none());
    }
    let status = slot.status();
    assert_eq!(status.stage_high_water, U64(1));
    assert_eq!(status.pending_dispatch, DispatchState::NeverDispatched);
    assert_eq!(
        stages.count(|action| matches!(action, export_stage::Action::Begin { .. })),
        0
    );

    drop(lifecycle);
    close.join().expect("Close caller")?;
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn catalog_root_close_dominates_executor_reacquire() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = Owner::new(f._temp.path().join("missing-worker"), stages, 1, &pool)?;
    owner.bind(&f.root)?;
    acquire(&owner, &f)?;
    owner.closing();
    owner.retire_root(&f.root)?;
    assert!(
        owner
            .executor_call(
                &executor_request(
                    &f,
                    export_executor::executor_id(&f.root, 2)?,
                    1,
                    crate::catalog_session::export_executor::Action::Acquire,
                ),
                &AtomicBool::new(false),
            )
            .is_err()
    );
    owner.forget_released_root(&f.root)?;
    Ok(())
}

#[test]
fn catalog_root_close_replays_unresolved_executor_operation_before_release() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = Owner::new(
        f._temp.path().join("missing-worker"),
        stages.clone(),
        1,
        &pool,
    )?;
    owner.bind(&f.root)?;
    acquire(&owner, &f)?;
    let recover = executor_request(
        &f,
        f.executor.clone(),
        2,
        crate::catalog_session::export_executor::Action::Recover {
            max_directories: U64(1024),
        },
    );
    stages.executor_unknown.store(true, Ordering::Release);
    assert!(
        owner
            .executor_call(&recover, &AtomicBool::new(false))
            .is_err()
    );
    owner.closing();
    owner.retire_root(&f.root)?;
    let calls = stages.executor_calls.lock().unwrap();
    assert_eq!(calls.len(), 4);
    assert!(matches!(
        calls[0].action,
        crate::catalog_session::export_executor::Action::Acquire
    ));
    assert_eq!(calls[1], recover);
    assert_eq!(calls[2], recover);
    assert!(matches!(
        calls[3].action,
        crate::catalog_session::export_executor::Action::Release
    ));
    drop(calls);
    owner.forget_released_root(&f.root)?;
    Ok(())
}

#[test]
fn shared_pool_refuses_atomically_and_reuses_only_after_explicit_retirement() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let competitor = pool.reserve_exact(1)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages.clone(), &pool)?;
    let request = register(&f);
    let high_water = *owner.high_water.lock().unwrap();
    let error = owner.call(&request).unwrap_err();
    let failure = error
        .downcast_ref::<Failure>()
        .context("typed shared-pool refusal with negative custody receipt")?;
    failure.validate()?;
    assert_eq!(failure.kind, FailureKind::ResourceLimit);
    assert_eq!(
        failure.message,
        crate::preview::ByteLimit {
            required: f.worker,
            available: f.worker - 1,
        }
        .to_string()
    );
    let receipt = failure.object_receipt.context("negative custody receipt")?;
    assert_eq!(receipt.operation, request.operation);
    assert_eq!(receipt.step, U64(0));
    assert_eq!(receipt.request_digest, request.digest()?);
    assert_eq!(pool.used(), 1, "refusal must retain only the competitor");
    assert_eq!(*owner.high_water.lock().unwrap(), high_water);
    assert!(
        owner
            .slot(&request.root, request.operation, &request.stage)
            .is_err()
    );
    drop(competitor);
    owner.call(&request)?;
    assert_eq!(pool.used(), f.worker);
    owner.stage_call(&f.begin, &AtomicBool::new(false))?;
    owner.stage_call(
        &ordinary(&f, 2, export_stage::Action::Abort),
        &AtomicBool::new(false),
    )?;
    assert_eq!(pool.used(), f.worker, "Abort alone cannot release custody");
    assert!(
        pool.try_reserve(1).is_none(),
        "capacity cannot be reused before Retire"
    );
    owner.call(&lifecycle(&f, Action::Retire))?;
    assert_eq!(pool.used(), 0);
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
    **begin = foreign_begin;
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
    fn executor_call(
        &self,
        request: &crate::catalog_session::export_executor::Request,
        _cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::export_executor::Reply> {
        fake_executor_reply(request)
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
    acquire(&owner, f)?;
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
    acquire(&owner, &f)?;
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
        fn executor_call(
            &self,
            request: &crate::catalog_session::export_executor::Request,
            _cancel: &AtomicBool,
        ) -> Result<crate::catalog_session::export_executor::Reply> {
            fake_executor_reply(request)
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
    acquire(&owner, &f)?;
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
    acquire(&owner, f)?;
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

struct CombinedStages {
    owner: Mutex<crate::filesystem_worker::export_bootstrap_test_support::Owner>,
    stages: Mutex<Vec<export_stage::Request>>,
    executors: Mutex<Vec<export_executor::Request>>,
    lose_release: AtomicBool,
    block_release_replay: AtomicBool,
    lose_close: AtomicBool,
}
impl Stages for CombinedStages {
    fn call(
        &self,
        request: &export_stage::Request,
        cancel: &AtomicBool,
    ) -> Result<export_stage::Reply> {
        self.stages.lock().unwrap().push(request.clone());
        if matches!(request.action, export_stage::Action::Release)
            && self.block_release_replay.load(Ordering::Acquire)
        {
            return Err(Failure::new(FailureKind::Unknown, "held Release reconciliation").into());
        }
        let reply = self.owner.lock().unwrap().stage(request, cancel)?;
        if matches!(request.action, export_stage::Action::Release)
            && self.lose_release.swap(false, Ordering::AcqRel)
        {
            self.block_release_replay.store(true, Ordering::Release);
            return Err(
                Failure::new(FailureKind::Unknown, "lost combined F stage Release reply").into(),
            );
        }
        Ok(reply)
    }
    fn executor_call(
        &self,
        request: &export_executor::Request,
        cancel: &AtomicBool,
    ) -> Result<export_executor::Reply> {
        self.executors.lock().unwrap().push(request.clone());
        let reply = self.owner.lock().unwrap().executor(request, cancel)?;
        if matches!(request.action, export_executor::Action::Release)
            && self.lose_close.swap(false, Ordering::AcqRel)
        {
            return Err(
                Failure::new(FailureKind::Unknown, "lost combined F executor Close reply").into(),
            );
        }
        Ok(reply)
    }
}

#[test]
fn combined_bootstrap_lost_stage_and_executor_close_reopen_and_stale_lifecycles() -> Result<()> {
    let mut f = fixture()?;
    let (bootstrap, root) =
        crate::filesystem_worker::export_bootstrap_test_support::Owner::new(f._temp.path())?;
    f.root = root;
    f.executor = export_executor::executor_id(&f.root, 1)?;
    f.begin.root = f.root.clone();
    f.begin.executor = f.executor.clone();
    let stages = Arc::new(CombinedStages {
        owner: Mutex::new(bootstrap),
        stages: Mutex::new(Vec::new()),
        executors: Mutex::new(Vec::new()),
        lose_release: AtomicBool::new(true),
        block_release_replay: AtomicBool::new(false),
        lose_close: AtomicBool::new(true),
    });
    let pool = ByteBudget::new(f.worker)?;
    let owner = Arc::new(Owner::new(
        f._temp.path().join("missing"),
        stages.clone(),
        1,
        &pool,
    )?);
    owner.bind(&f.root)?;
    acquire(&owner, &f)?;
    owner.call(&register(&f))?;
    ready(&f, &owner)?;
    // Checked spawn failure supplies the no-child terminal needed by Release.
    assert_eq!(
        owner.call(&lifecycle(&f, Action::Spawn))?.phase,
        Phase::WaitFailed
    );
    let close = executor_request(&f, f.executor.clone(), 2, export_executor::Action::Release);
    assert!(
        owner
            .executor_call(&close, &AtomicBool::new(false))
            .is_err()
    );
    let acquire2 = executor_request(
        &f,
        export_executor::executor_id(&f.root, 2)?,
        1,
        export_executor::Action::Acquire,
    );
    assert!(
        owner
            .executor_call(&acquire2, &AtomicBool::new(false))
            .is_err()
    );
    assert!(
        stages
            .owner
            .lock()
            .unwrap()
            .executor(&acquire2, &AtomicBool::new(false))
            .is_err()
    );
    assert_eq!(pool.used(), f.worker);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.root.canonical_root.to_path()?.join("photo-export.lock"))?;
    assert!(lock.try_lock_exclusive().is_err());
    // An unresolved replay still prevents G retirement and actual F unlock.
    assert!(
        owner
            .executor_call(&close, &AtomicBool::new(false))
            .is_err()
    );
    assert!(lock.try_lock_exclusive().is_err());
    assert_eq!(stages.executors.lock().unwrap().len(), 1);
    stages.block_release_replay.store(false, Ordering::Release);
    // F now replays the stage Release while the old executor is still live,
    // G retires the reservation, and actual F Close succeeds but loses its ACK.
    assert!(
        owner
            .executor_call(&close, &AtomicBool::new(false))
            .is_err()
    );
    assert_eq!(pool.used(), 0);
    lock.try_lock_exclusive()?;
    FileExt::unlock(&lock)?;
    assert!(
        owner
            .executor_call(&acquire2, &AtomicBool::new(false))
            .is_err()
    );
    let released = owner.executor_call(&close, &AtomicBool::new(false))?;
    let calls = stages.stages.lock().unwrap();
    let releases: Vec<_> = calls
        .iter()
        .filter(|request| matches!(request.action, export_stage::Action::Release))
        .collect();
    assert_eq!(releases.len(), 3);
    assert!(
        releases
            .iter()
            .all(|request| request.digest().unwrap() == releases[0].digest().unwrap())
    );
    drop(calls);
    let count = stages.executors.lock().unwrap().len();
    // Drop/reopen the service-side reference, preserving the root-owned G.
    let reopened = owner.clone();
    drop(owner);
    reopened.bind(&f.root)?;
    assert_eq!(
        reopened.executor_call(&close, &AtomicBool::new(false))?,
        released
    );
    assert_eq!(stages.executors.lock().unwrap().len(), count);
    let stale_acquire =
        executor_request(&f, f.executor.clone(), 1, export_executor::Action::Acquire);
    assert!(
        reopened
            .executor_call(&stale_acquire, &AtomicBool::new(false))
            .is_err()
    );
    assert!(
        stages
            .owner
            .lock()
            .unwrap()
            .executor(&stale_acquire, &AtomicBool::new(false))
            .is_err()
    );
    reopened.executor_call(&acquire2, &AtomicBool::new(false))?;
    assert_eq!(
        reopened.executor_call(&close, &AtomicBool::new(false))?,
        released
    );
    let mut begin2 = f.begin.clone();
    begin2.executor = acquire2.executor.clone();
    begin2.stage = LeaseId::new();
    let mut registration = register(&f);
    registration.operation = U64(10);
    registration.stage = begin2.stage.clone();
    let Action::Register { begin, .. } = &mut registration.action else {
        unreachable!()
    };
    **begin = begin2.clone();
    reopened.call(&registration)?;
    reopened.stage_call(&begin2, &AtomicBool::new(false))?;
    // The real F user cache has now changed. Stale Begin is rejected before it.
    let failure = stages
        .owner
        .lock()
        .unwrap()
        .stage(&f.begin, &AtomicBool::new(false))
        .unwrap_err()
        .downcast::<Failure>()?;
    assert_eq!(failure.kind, FailureKind::Rejected);
    assert!(bound_failure(&failure, &f.begin));
    let close2 = executor_request(
        &f,
        acquire2.executor.clone(),
        2,
        export_executor::Action::Release,
    );
    reopened.executor_call(&close2, &AtomicBool::new(false))?;
    let acquire3 = executor_request(
        &f,
        export_executor::executor_id(&f.root, 3)?,
        1,
        export_executor::Action::Acquire,
    );
    reopened.executor_call(&acquire3, &AtomicBool::new(false))?;
    for stale in [&stale_acquire, &acquire2] {
        assert!(
            reopened
                .executor_call(stale, &AtomicBool::new(false))
                .is_err()
        );
        assert!(
            stages
                .owner
                .lock()
                .unwrap()
                .executor(stale, &AtomicBool::new(false))
                .is_err()
        );
    }
    let failure = stages
        .owner
        .lock()
        .unwrap()
        .stage(&f.begin, &AtomicBool::new(false))
        .unwrap_err()
        .downcast::<Failure>()?;
    assert_eq!(failure.kind, FailureKind::Rejected);
    assert!(bound_failure(&failure, &f.begin));
    reopened.executor_call(
        &executor_request(&f, acquire3.executor, 2, export_executor::Action::Release),
        &AtomicBool::new(false),
    )?;
    assert_eq!(pool.used(), 0);
    // No active identity remains to reject these accidentally: both preceding
    // receipts have advanced through Close3, so only high-water can reject 1/2.
    for stale in [&stale_acquire, &acquire2] {
        assert!(
            reopened
                .executor_call(stale, &AtomicBool::new(false))
                .is_err()
        );
        assert!(
            stages
                .owner
                .lock()
                .unwrap()
                .executor(stale, &AtomicBool::new(false))
                .is_err()
        );
    }
    lock.try_lock_exclusive()?;
    FileExt::unlock(&lock)?;
    assert!(reopened.executor.lock().unwrap().active.is_none());
    Ok(())
}

#[test]
fn executor_generation_is_root_scoped_noncanonical_and_exhaustion_safe() -> Result<()> {
    let f = fixture()?;
    assert!(export_executor::executor_id(&f.root, 0).is_err());
    let token = export_executor::executor_id(&f.root, u64::MAX)?;
    assert_eq!(export_executor::generation(&f.root, &token)?, u64::MAX);
    let mut foreign = f.root.clone();
    foreign.session = LeaseId::new();
    assert!(export_executor::generation(&foreign, &token).is_err());
    assert!(LeaseId::parse(&token.as_str().to_uppercase()).is_err());
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages, &pool)?;
    owner.executor_call(
        &executor_request(&f, f.executor.clone(), 2, export_executor::Action::Release),
        &AtomicBool::new(false),
    )?;
    owner.executor_call(
        &executor_request(&f, token.clone(), 1, export_executor::Action::Acquire),
        &AtomicBool::new(false),
    )?;
    owner.executor_call(
        &executor_request(&f, token.clone(), 2, export_executor::Action::Release),
        &AtomicBool::new(false),
    )?;
    assert!(
        owner
            .executor_call(
                &executor_request(&f, token, 1, export_executor::Action::Acquire),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert!(
        owner
            .executor_call(
                &executor_request(
                    &f,
                    export_executor::executor_id(&f.root, 2)?,
                    1,
                    export_executor::Action::Acquire
                ),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    let catalog = f.root.canonical_root.to_path()?;
    let manifest = catalog.join("manifest");
    std::fs::create_dir_all(&manifest)?;
    let mut f_owner = crate::filesystem_worker::export_executor_test_support::Owner::default();
    let maximum = export_executor::executor_id(&f.root, u64::MAX)?;
    f_owner.call(
        &catalog,
        &manifest,
        &executor_request(&f, maximum.clone(), 1, export_executor::Action::Acquire),
        &AtomicBool::new(false),
        true,
    )?;
    f_owner.call(
        &catalog,
        &manifest,
        &executor_request(&f, maximum.clone(), 2, export_executor::Action::Release),
        &AtomicBool::new(false),
        true,
    )?;
    for old in [maximum, f.executor.clone()] {
        assert!(
            f_owner
                .call(
                    &catalog,
                    &manifest,
                    &executor_request(&f, old, 1, export_executor::Action::Acquire),
                    &AtomicBool::new(false),
                    true
                )
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn f_invalid_first_or_later_attempt_blocks_complete_inventory_admission() -> Result<()> {
    for invalid_namespace in [0, 1] {
        for kind in [
            "oversized-job",
            "near-request-job",
            "empty-attempt",
            "negative-sequence",
        ] {
            let f = fixture()?;
            let catalog = f.root.canonical_root.to_path()?;
            let manifest = catalog.join("manifest");
            std::fs::create_dir_all(&manifest)?;
            let mut owner =
                crate::filesystem_worker::export_executor_test_support::Owner::default();
            owner.call(
                &catalog,
                &manifest,
                &executor_request(&f, f.executor.clone(), 1, export_executor::Action::Acquire),
                &AtomicBool::new(false),
                true,
            )?;
            let roots = [
                catalog.join("photo-export-workers"),
                manifest.join("export-workers"),
            ];
            for (index, root) in roots.iter().enumerate() {
                std::fs::create_dir_all(root)?;
                persisted_transport(&f, root, &format!("photo-worker-{}", uuid::Uuid::new_v4()))?;
                if index == invalid_namespace {
                    let stage = std::fs::read_dir(root)?.next().unwrap()?.path();
                    let path = stage.join("request.json");
                    let mut request: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&path)?)?;
                    match kind {
                        "oversized-job" => {
                            request["work"]["job"] = serde_json::json!("j".repeat(129))
                        }
                        "near-request-job" => {
                            request["work"]["job"] = serde_json::json!("j".repeat(200_000))
                        }
                        "empty-attempt" => request["work"]["attempt"] = serde_json::json!(""),
                        _ => request["work"]["sequence"] = serde_json::json!(-1),
                    }
                    std::fs::write(path, serde_json::to_vec(&request)?)?;
                }
            }
            let recover = executor_request(
                &f,
                f.executor.clone(),
                2,
                export_executor::Action::Recover {
                    max_directories: U64(2),
                },
            );
            let reply = owner.call(&catalog, &manifest, &recover, &AtomicBool::new(false), true)?;
            let export_executor::Value::Recovery {
                scanned,
                retained,
                candidate,
                ..
            } = &reply.value
            else {
                unreachable!()
            };
            assert_eq!(
                (*scanned, *retained),
                (U64(2), U64(1)),
                "{kind} namespace {invalid_namespace}"
            );
            assert!(candidate.is_none());
            assert_eq!(
                reply,
                owner.call(&catalog, &manifest, &recover, &AtomicBool::new(false), true)?
            );
            assert_eq!(
                roots
                    .iter()
                    .map(|root| std::fs::read_dir(root).unwrap().count())
                    .sum::<usize>(),
                2
            );
            owner.call(
                &catalog,
                &manifest,
                &executor_request(&f, f.executor.clone(), 3, export_executor::Action::Release),
                &AtomicBool::new(false),
                true,
            )?;
        }
    }
    Ok(())
}

#[test]
fn combined_root_close_finishes_each_interrupted_discard_phase() -> Result<()> {
    for fault in [
        "after-request",
        "after-active",
        "directory",
        "after-directory",
    ] {
        let mut f = fixture()?;
        let (bootstrap, root) =
            crate::filesystem_worker::export_bootstrap_test_support::Owner::new(f._temp.path())?;
        f.root = root;
        f.executor = export_executor::executor_id(&f.root, 1)?;
        f.begin.root = f.root.clone();
        f.begin.executor = f.executor.clone();
        let stages = Arc::new(CombinedStages {
            owner: Mutex::new(bootstrap),
            stages: Mutex::new(Vec::new()),
            executors: Mutex::new(Vec::new()),
            lose_release: AtomicBool::new(false),
            block_release_replay: AtomicBool::new(false),
            lose_close: AtomicBool::new(false),
        });
        let pool = ByteBudget::new(f.worker)?;
        let owner = Arc::new(Owner::new(
            f._temp.path().join("missing"),
            stages.clone(),
            1,
            &pool,
        )?);
        owner.bind(&f.root)?;
        acquire(&owner, &f)?;
        let root = f
            .root
            .canonical_root
            .to_path()?
            .join("photo-export-workers");
        for _ in 0..2 {
            persisted_transport(&f, &root, &format!("photo-worker-{}", uuid::Uuid::new_v4()))?;
        }
        let recover = executor_request(
            &f,
            f.executor.clone(),
            2,
            export_executor::Action::Recover {
                max_directories: U64(2),
            },
        );
        let export_executor::Value::Recovery {
            candidate: Some(first),
            ..
        } = owner
            .executor_call(&recover, &AtomicBool::new(false))?
            .value
        else {
            unreachable!()
        };
        let discard = executor_request(
            &f,
            f.executor.clone(),
            3,
            export_executor::Action::Discard { token: first.token },
        );
        let mut fired = false;
        crate::export_worker::set_compact_discard_hook(move |phase, _| {
            if phase == fault && !fired {
                fired = true;
                anyhow::bail!("deterministic {fault} interruption");
            }
            Ok(())
        });
        assert!(
            owner
                .executor_call(&discard, &AtomicBool::new(false))
                .is_err(),
            "{fault}"
        );
        let successor = executor_request(
            &f,
            export_executor::executor_id(&f.root, 2)?,
            1,
            export_executor::Action::Acquire,
        );
        assert!(
            owner
                .executor_call(&successor, &AtomicBool::new(false))
                .is_err()
        );
        // Root close must replay the exact pending Discard before its Release.
        owner.retire_root(&f.root)?;
        let calls = stages.executors.lock().unwrap();
        let discards: Vec<_> = calls
            .iter()
            .filter(|request| matches!(request.action, export_executor::Action::Discard { .. }))
            .collect();
        assert_eq!(discards.len(), 2);
        assert_eq!(discards[0], discards[1]);
        assert!(matches!(
            calls.last().unwrap().action,
            export_executor::Action::Release
        ));
        drop(calls);
        assert_eq!(
            std::fs::read_dir(&root)?.count(),
            1,
            "unreconciled candidate survives root Close"
        );
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.root.canonical_root.to_path()?.join("photo-export.lock"))?;
        lock.try_lock_exclusive()?;
        FileExt::unlock(&lock)?;
    }
    crate::export_worker::set_compact_discard_hook(|_, _| Ok(()));
    Ok(())
}

#[test]
fn managed_c_retire_ack_replays_exact_identity_without_second_budget_release() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages, &pool)?;
    owner.call(&register(&f))?;
    assert_eq!(pool.used(), f.worker);
    let mut retire = register(&f);
    retire.action = Action::Retire;
    let first = owner.call(&retire)?;
    assert_eq!(pool.used(), 0);
    let replay = owner.call(&retire)?;
    assert_eq!(serde_json::to_vec(&first)?, serde_json::to_vec(&replay)?);
    let query = Query {
        key: Key::new(&f.root, retire.operation, &f.stage),
        action: QueryAction::Retire,
    };
    assert_eq!(
        serde_json::to_vec(&first)?,
        serde_json::to_vec(&owner.query(&query)?)?
    );
    let mut foreign = retire.clone();
    foreign.binding.job.push_str("-foreign");
    assert!(owner.call(&foreign).is_err());
    foreign = retire.clone();
    foreign.root.token = LeaseId::new();
    assert!(owner.call(&foreign).is_err());
    foreign = retire.clone();
    foreign.stage = LeaseId::new();
    assert!(owner.call(&foreign).is_err());
    let reservation = pool.try_reserve(f.worker).unwrap();
    owner.call(&retire)?;
    assert_eq!(pool.used(), f.worker);
    drop(reservation);
    owner.executor_call(
        &executor_request(&f, f.executor.clone(), 2, export_executor::Action::Release),
        &AtomicBool::new(false),
    )?;
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn managed_c_retire_waits_for_reservation_and_retires_captured_arc_without_deadlock() -> Result<()>
{
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages, &pool)?;
    owner.call(&register(&f))?;
    let captured = owner.slot(&f.root, U64(9), &f.stage)?;
    let reservation = captured.reservation.lock().unwrap();
    let thread_owner = owner.clone();
    let mut request = register(&f);
    request.action = Action::Retire;
    let (tx, rx) = std::sync::mpsc::channel();
    let first = thread::spawn(move || {
        let _ = tx.send(thread_owner.call(&request));
    });
    wait_until(|| captured.stage_state.lock().unwrap().retired)?;
    assert_eq!(pool.used(), f.worker);
    assert!(owner.previous_retire.lock().unwrap().is_none());
    let query = Query {
        key: Key::new(&f.root, U64(9), &f.stage),
        action: QueryAction::Retire,
    };
    assert!(
        owner.query(&query).is_err(),
        "no successful Retire before budget release"
    );
    drop(reservation);
    rx.recv_timeout(Duration::from_secs(5))??;
    first.join().unwrap();
    assert_eq!(pool.used(), 0);
    // Close captures slot Arcs before cleanup. Exercise those same cleanup and
    // retirement calls after the ordinary caller has independently retired it.
    owner.cleanup_slot(&captured)?;
    let thread_owner = owner.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let retry = thread::spawn(move || {
        let _ = tx.send(thread_owner.retire_slot(&captured));
    });
    rx.recv_timeout(Duration::from_secs(5))??;
    retry.join().unwrap();
    assert_eq!(pool.used(), 0);
    owner.executor_call(
        &executor_request(&f, f.executor.clone(), 2, export_executor::Action::Release),
        &AtomicBool::new(false),
    )?;
    Ok(())
}

#[test]
fn managed_c_register_refusal_has_exact_negative_custody_receipt() -> Result<()> {
    let f = fixture()?;
    let pool = ByteBudget::new(f.worker.checked_sub(1).context("fixture pool allowance")?)?;
    let stages = FakeStages::new(f._temp.path().to_owned());
    let owner = owner(&f, stages, &pool)?;
    let request = register(&f);
    let error = owner.call(&request).unwrap_err();
    let failure = error
        .downcast_ref::<Failure>()
        .context("bound negative registration failure")?;
    assert_ne!(failure.kind, FailureKind::Unknown);
    let receipt = failure.object_receipt.as_ref().unwrap();
    assert_eq!(receipt.operation, request.operation);
    assert_eq!(receipt.request_digest, request.digest()?);
    assert!(
        owner
            .slot(&request.root, request.operation, &request.stage)
            .is_err()
    );
    assert_eq!(pool.used(), 0);
    owner.executor_call(
        &executor_request(&f, f.executor.clone(), 2, export_executor::Action::Release),
        &AtomicBool::new(false),
    )?;
    Ok(())
}
