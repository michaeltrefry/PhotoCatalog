use super::*;
use crate::catalog_exports::ExportWork;
use anyhow::Context;
use std::{
    fs::{self, OpenOptions},
    path::PathBuf,
    sync::{Barrier, atomic::AtomicUsize},
};

/// Synthetic facts provider only. Real F process/path custody is independently
/// qualified by the filesystem worker. These pins are retained until SQL closes.
struct Facts {
    bootstrap: CatalogBootstrap,
    _pins: Vec<File>,
    confirms: AtomicUsize,
    releases: AtomicUsize,
    abandons: AtomicUsize,
    fail_release: AtomicBool,
    fail_prepare: AtomicBool,
    fail_confirmation: AtomicBool,
    empty_restore_status: AtomicBool,
    fresh: bool,
    fatal_drop_sentry: bool,
    export_directory: NativePath,
    export_requests: Arc<Mutex<Vec<PrepareExportDirectory>>>,
    export_snapshot_requests: Arc<Mutex<Vec<ExportDestinationSnapshotRequest>>>,
    export_alias_requests: Arc<Mutex<Vec<ExportAliasFactRequest>>>,
    export_profile: Mutex<Vec<u8>>,
    export_profile_requests: Arc<Mutex<Vec<ExportProfileRequest>>>,
    cancel_profile_after_begin: AtomicBool,
    fail_profile_begin_reply: AtomicBool,
    fail_profile_finish_reply: AtomicBool,
    fail_profile_abort: AtomicBool,
    export_original_revision: Mutex<Option<crate::metadata_export::FileRevision>>,
    export_original_requests: Arc<Mutex<Vec<ExportOriginalRequest>>>,
    inspect_original_requests: Arc<Mutex<Vec<InspectExportOriginal>>>,
    fail_original_begin_reply: AtomicBool,
    fail_original_finish_reply: Arc<AtomicBool>,
    fail_original_abort: Arc<AtomicBool>,
    original_cleanup_probe: Arc<Mutex<Option<crate::metadata_export::SealedPhotoExport>>>,
    original_cleanup_probe_results: Arc<Mutex<Vec<(ExportOriginalAction, bool)>>>,
    export_publication: Mutex<TestPublicationState>,
    export_publication_requests: Arc<Mutex<Vec<ExportPublicationRequest>>>,
    reject_publication: AtomicBool,
    lose_publication_action: Arc<Mutex<Option<ExportPublicationAction>>>,
    cancel_lost_publication_reply: Arc<AtomicBool>,
    fail_publication_begin_reply: AtomicBool,
    fail_publication_step_reply: AtomicBool,
    fail_publication_abort: Arc<AtomicBool>,
    export_executor_requests: Arc<Mutex<Vec<export_executor::Request>>>,
    real_export_executor:
        Mutex<Option<crate::filesystem_worker::export_executor_test_support::Owner>>,
    lose_export_executor_action: Mutex<Option<String>>,
    cancel_after_discard: Mutex<Option<Arc<AtomicBool>>>,
    export_executor_enter: Mutex<Option<Arc<Barrier>>>,
    export_executor_release: Mutex<Option<Arc<Barrier>>>,
    bad_export_executor_digest: AtomicBool,
    wrong_export_executor_value: AtomicBool,
    export_native_status: Mutex<Option<export_native::Status>>,
    export_native_requests: Arc<Mutex<Vec<export_native::Request>>>,
    foreign_export_native_status_binding: AtomicBool,
    export_stage_requests: Arc<Mutex<Vec<export_stage::Request>>>,
    export_stage_completed: Mutex<Option<([u8; 32], export_stage::Reply)>>,
    lose_export_stage_reply: AtomicBool,
    reject_export_stage_reply: AtomicBool,
    export_stage_work: Mutex<Option<ExportWork>>,
    drain_export_native_on_status: AtomicBool,
    lose_export_native_action: Mutex<Option<String>>,
    lose_export_stage_action: Mutex<Option<String>>,
    reject_export_register: AtomicBool,
    fail_export_spawn: AtomicBool,
    lose_export_status: AtomicBool,
    reject_export_release: AtomicBool,
}

#[expect(
    clippy::large_enum_variant,
    reason = "the test double mirrors inline publication custody and observes its exact drop order"
)]
enum TestPublicationState {
    Empty,
    Active {
        transfer: LeaseId,
        source: ExportPublicationSource,
        next_step: u64,
        seal: crate::metadata_export::SealedPhotoExport,
        publication: crate::metadata_export::PhotoPublication,
        last: (
            ExportPublicationRequest,
            std::result::Result<ExportPublicationReply, crate::filesystem_worker::wire::Failure>,
        ),
    },
    Terminal {
        request: ExportPublicationRequest,
        reply: ExportPublicationReply,
    },
}

fn test_publication_failure(error: anyhow::Error) -> anyhow::Error {
    if error
        .downcast_ref::<crate::filesystem_worker::wire::Failure>()
        .is_some()
    {
        error
    } else {
        crate::filesystem_worker::wire::Failure::new(
            crate::filesystem_worker::wire::FailureKind::Rejected,
            error,
        )
        .into()
    }
}
impl Facts {
    fn create(base: &Path) -> Result<(Arc<Self>, PrepareCatalog)> {
        let root = base.join("catalog");
        let cache = base.join("cache");
        let export_directory = base.join("exports");
        fs::create_dir_all(&root)?;
        fs::create_dir_all(&cache)?;
        let root = root.canonicalize()?;
        let cache = cache.canonicalize()?;
        let export_directory = if export_directory.exists() {
            export_directory.canonicalize()?
        } else {
            export_directory
        };
        let main = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.join("catalog.sqlite3"))?;
        let manifest = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(cache.join("previews.sqlite3"))?;
        let mut root_options = OpenOptions::new();
        root_options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            root_options.custom_flags(0x02000000); // FILE_FLAG_BACKUP_SEMANTICS
        }
        let root_file = root_options.open(&root)?;
        let request = PrepareCatalog {
            operation: U64(1),
            session: LeaseId::new(),
            mode: BootstrapMode::DesktopCreate,
            root: NativePath::from_path(&root),
            manifest_root: NativePath::from_path(&cache),
            import_source: None,
        };
        let bootstrap = CatalogBootstrap {
            version: 1,
            operation: request.operation,
            epoch: LeaseId::new(),
            token: LeaseId::new(),
            session: request.session.clone(),
            canonical_root: request.root.clone(),
            root_physical: crate::catalog_storage::physical_object_id(&root_file)?,
            catalog: PinnedDatabase {
                path: NativePath::from_path(&root.join("catalog.sqlite3")),
                physical: crate::catalog_storage::physical_object_id(&main)?,
                created: true,
            },
            manifest: PinnedDatabase {
                path: NativePath::from_path(&cache.join("previews.sqlite3")),
                physical: crate::catalog_storage::physical_object_id(&manifest)?,
                created: true,
            },
        };
        Ok((
            Arc::new(Self {
                bootstrap,
                _pins: vec![main, manifest, root_file],
                confirms: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
                abandons: AtomicUsize::new(0),
                fail_release: AtomicBool::new(false),
                fail_prepare: AtomicBool::new(false),
                fail_confirmation: AtomicBool::new(false),
                empty_restore_status: AtomicBool::new(false),
                fresh: true,
                fatal_drop_sentry: false,
                export_directory: NativePath::from_path(&export_directory),
                export_requests: Arc::new(Mutex::new(Vec::new())),
                export_snapshot_requests: Arc::new(Mutex::new(Vec::new())),
                export_alias_requests: Arc::new(Mutex::new(Vec::new())),
                export_profile: Mutex::new(Vec::new()),
                export_profile_requests: Arc::new(Mutex::new(Vec::new())),
                cancel_profile_after_begin: AtomicBool::new(false),
                fail_profile_begin_reply: AtomicBool::new(false),
                fail_profile_finish_reply: AtomicBool::new(false),
                fail_profile_abort: AtomicBool::new(false),
                export_original_revision: Mutex::new(None),
                export_original_requests: Arc::new(Mutex::new(Vec::new())),
                inspect_original_requests: Arc::new(Mutex::new(Vec::new())),
                fail_original_begin_reply: AtomicBool::new(false),
                fail_original_finish_reply: Arc::new(AtomicBool::new(false)),
                fail_original_abort: Arc::new(AtomicBool::new(false)),
                original_cleanup_probe: Arc::new(Mutex::new(None)),
                original_cleanup_probe_results: Arc::new(Mutex::new(Vec::new())),
                export_publication: Mutex::new(TestPublicationState::Empty),
                export_publication_requests: Arc::new(Mutex::new(Vec::new())),
                reject_publication: AtomicBool::new(false),
                lose_publication_action: Arc::new(Mutex::new(None)),
                cancel_lost_publication_reply: Arc::new(AtomicBool::new(false)),
                fail_publication_begin_reply: AtomicBool::new(false),
                fail_publication_step_reply: AtomicBool::new(false),
                fail_publication_abort: Arc::new(AtomicBool::new(false)),
                export_executor_requests: Arc::new(Mutex::new(Vec::new())),
                real_export_executor: Mutex::new(None),
                lose_export_executor_action: Mutex::new(None),
                cancel_after_discard: Mutex::new(None),
                export_executor_enter: Mutex::new(None),
                export_executor_release: Mutex::new(None),
                bad_export_executor_digest: AtomicBool::new(false),
                wrong_export_executor_value: AtomicBool::new(false),
                export_native_status: Mutex::new(None),
                export_native_requests: Arc::new(Mutex::new(Vec::new())),
                foreign_export_native_status_binding: AtomicBool::new(false),
                export_stage_requests: Arc::new(Mutex::new(Vec::new())),
                export_stage_completed: Mutex::new(None),
                lose_export_stage_reply: AtomicBool::new(false),
                reject_export_stage_reply: AtomicBool::new(false),
                export_stage_work: Mutex::new(None),
                drain_export_native_on_status: AtomicBool::new(false),
                lose_export_native_action: Mutex::new(None),
                lose_export_stage_action: Mutex::new(None),
                reject_export_register: AtomicBool::new(false),
                fail_export_spawn: AtomicBool::new(false),
                lose_export_status: AtomicBool::new(false),
                reject_export_release: AtomicBool::new(false),
            }),
            request,
        ))
    }
}
impl Drop for Facts {
    fn drop(&mut self) {
        if self.fatal_drop_sentry {
            std::process::exit(98);
        }
    }
}
impl CatalogFilesystem for Facts {
    fn export_native(&self) -> Option<&dyn export_native::CatalogExportNative> {
        Some(self)
    }

    fn export_executor_call(
        &self,
        request: &export_executor::Request,
        _cancel: &AtomicBool,
    ) -> Result<export_executor::Reply> {
        request.validate()?;
        self.export_executor_requests
            .lock()
            .unwrap()
            .push(request.clone());
        if matches!(request.action, export_executor::Action::Acquire) {
            let enter = self.export_executor_enter.lock().unwrap().clone();
            let release = self.export_executor_release.lock().unwrap().clone();
            if let (Some(enter), Some(release)) = (enter, release) {
                enter.wait();
                release.wait();
            }
        }
        if let Some(owner) = self.real_export_executor.lock().unwrap().as_mut() {
            let catalog = self.bootstrap.canonical_root.to_path()?;
            let manifest = self.bootstrap.manifest.path.to_path()?;
            let reply = owner.call(&catalog, manifest.parent().unwrap(), request, _cancel, true)?;
            let label = match request.action {
                export_executor::Action::Acquire => "acquire",
                export_executor::Action::Recover { .. } => "recover",
                export_executor::Action::Discard { .. } => "discard",
                export_executor::Action::Release => "close",
            };
            if label == "discard"
                && let Some(cancel) = self.cancel_after_discard.lock().unwrap().take()
            {
                cancel.store(true, Ordering::Release);
            }
            let mut lost = self.lose_export_executor_action.lock().unwrap();
            if lost.as_deref() == Some(label) {
                lost.take();
                anyhow::bail!("injected lost real executor {label} acknowledgement");
            }
            return Ok(reply);
        }
        let mut value = match &request.action {
            export_executor::Action::Acquire => export_executor::Value::Acquired,
            export_executor::Action::Recover { .. } => export_executor::Value::Recovery {
                scanned: U64(0),
                cleaned: U64(0),
                retained: U64(0),
                retained_example: None,
                candidate: None,
            },
            export_executor::Action::Discard { .. } => {
                export_executor::Value::Discarded { candidate: None }
            }
            export_executor::Action::Release => export_executor::Value::Released,
        };
        if self
            .wrong_export_executor_value
            .swap(false, Ordering::AcqRel)
        {
            value = export_executor::Value::Released;
        }
        Ok(export_executor::Reply {
            root: request.root.clone(),
            executor: request.executor.clone(),
            operation: request.operation,
            request_digest: if self
                .bad_export_executor_digest
                .swap(false, Ordering::AcqRel)
            {
                "0".repeat(64)
            } else {
                request.digest()?
            },
            value,
        })
    }

    fn export_stage_call(
        &self,
        request: &export_stage::Request,
        _cancel: &AtomicBool,
    ) -> Result<export_stage::Reply> {
        request.validate()?;
        self.export_stage_requests
            .lock()
            .unwrap()
            .push(request.clone());
        let digest = request.digest()?;
        if let Some((completed_digest, reply)) =
            self.export_stage_completed.lock().unwrap().as_ref()
            && completed_digest == &digest
        {
            return Ok(reply.clone());
        }
        let value = match &request.action {
            export_stage::Action::Begin { work, limits } => {
                // Use F's real pre-effect persisted-request validator. A fake
                // acknowledgement must not bypass the stored plan allowances.
                crate::export_worker::prepare_managed_request(
                    work,
                    *limits,
                    &request
                        .root
                        .canonical_root
                        .to_path()?
                        .join("synthetic-export-stage"),
                )?;
                *self.export_stage_work.lock().unwrap() = Some((**work).clone());
                export_stage::Value::Begun
            }
            export_stage::Action::UploadIcc { .. }
            | export_stage::Action::UploadXmp { .. }
            | export_stage::Action::Abort
            | export_stage::Action::Release => export_stage::Value::Unit,
            export_stage::Action::Ready { .. } => export_stage::Value::Ready {
                path: request.root.canonical_root.clone(),
            },
            export_stage::Action::ResultAndSeal => {
                let path = request
                    .root
                    .canonical_root
                    .to_path()?
                    .join("synthetic-export-stage");
                fs::create_dir_all(&path)?;
                let completed_file = path.join("completed.bin");
                fs::write(&completed_file, b"x")?;
                let work = self
                    .export_stage_work
                    .lock()
                    .unwrap()
                    .clone()
                    .context("synthetic export stage work missing")?;
                let sealed = crate::metadata_export::seal_photo_export(
                    &work.plan.destination,
                    &completed_file,
                    work.plan.max_payload_bytes,
                    &work.authority,
                    |_| Ok(()),
                )?;
                export_stage::Value::Completed {
                    path: NativePath::from_path(&path),
                    completion: export_stage::Completion {
                        authority: request.binding.authority.clone(),
                        attempt: request.binding.attempt.clone(),
                        sealed: sealed.clone(),
                        rendered: export_stage::Rendered {
                            staging: NativePath::from_path(&path.join("output")),
                            encoding: crate::image_export::EncodingReport {
                                output: crate::image_export::OutputDescriptor {
                                    width: 1,
                                    height: 1,
                                    channels: 4,
                                    bits_per_sample: 8,
                                    floating_point: false,
                                    orientation: 1,
                                    icc_blake3: "c".repeat(64),
                                    integer_clips_to_unit_range: true,
                                    alpha: crate::image_export::AlphaPolicy::Preserve,
                                },
                                encoded_extent: sealed.payload.bytes,
                                source_fingerprint: "d".repeat(64),
                                recipe_digest: "e".repeat(64),
                                metadata_blake3: "f".repeat(64),
                                compression: "fixture".into(),
                            },
                            renderer_identity: request.binding.renderer.clone(),
                            metadata_notes: Vec::new(),
                            timings: crate::photo_render::PhotoRenderTimings {
                                source_verification_before_ms: 0.,
                                staging_setup_ms: 0.,
                                decode_ms: 0.,
                                recipe_ms: 0.,
                                metadata_ms: 0.,
                                encode_ms: 0.,
                                source_verification_after_ms: 0.,
                                sync_ms: 0.,
                                total_ms: 0.,
                            },
                        },
                        seal_ms: 0.,
                        peak_resident_bytes: Some(1),
                        peak_method: "fixture".into(),
                    },
                }
            }
            export_stage::Action::Arm { .. } | export_stage::Action::NativeDrained { .. } => {
                anyhow::bail!("synthetic C stage fixture rejects supervisor operation")
            }
        };
        let reply = export_stage::Reply {
            epoch: request.root.epoch.clone(),
            session: request.root.session.clone(),
            stage: request.stage.clone(),
            operation: request.operation,
            binding: request.binding.clone(),
            value,
        };
        reply.validate(request)?;
        if matches!(request.action, export_stage::Action::ResultAndSeal) {
            *self.export_stage_completed.lock().unwrap() = Some((digest, reply.clone()));
        }
        if let Some(status) = self.export_native_status.lock().unwrap().as_mut() {
            status.stage_high_water = request.operation;
            status.pending_stage_operation = None;
        }
        if self.reject_export_stage_reply.swap(false, Ordering::AcqRel) {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Rejected,
                "synthetic terminal export stage failure",
            )
            .into());
        }
        if matches!(request.action, export_stage::Action::Release)
            && self.reject_export_release.swap(false, Ordering::AcqRel)
        {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Rejected,
                "synthetic terminal Release failure",
            )
            .into());
        }
        let stage_action = match request.action {
            export_stage::Action::Begin { .. } => "begin",
            export_stage::Action::UploadIcc { .. } => "icc",
            export_stage::Action::UploadXmp { .. } => "xmp",
            export_stage::Action::Ready { .. } => "ready",
            export_stage::Action::ResultAndSeal => "seal",
            export_stage::Action::Release => "release",
            export_stage::Action::Abort => "abort",
            _ => "supervisor",
        };
        let lose_action =
            self.lose_export_stage_action.lock().unwrap().as_deref() == Some(stage_action);
        if lose_action {
            self.lose_export_stage_action.lock().unwrap().take();
        }
        if lose_action || self.lose_export_stage_reply.swap(false, Ordering::AcqRel) {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Unknown,
                "synthetic lost export stage acknowledgement",
            )
            .into());
        }
        Ok(reply)
    }

    fn inspect_export_original(
        &self,
        request: &InspectExportOriginal,
        cancel: &AtomicBool,
    ) -> Result<InspectedExportOriginal> {
        request.validate()?;
        self.inspect_original_requests
            .lock()
            .unwrap()
            .push(request.clone());
        if cancel.load(Ordering::Acquire) {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Canceled,
                "synthetic original inspection cancellation",
            )
            .into());
        }
        Ok(InspectedExportOriginal {
            root: request.root.clone(),
            requested: request.requested.clone(),
            allowance: request.allowance,
            revision: match self.export_original_revision.lock().unwrap().clone() {
                Some(value) => value,
                None => crate::metadata_export::inspect_file_revision(
                    &request.requested.to_path()?,
                    request.allowance.0,
                )?,
            },
        })
    }
    fn export_original_call(
        &self,
        request: &ExportOriginalRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportOriginalReply> {
        request.validate()?;
        self.export_original_requests
            .lock()
            .unwrap()
            .push(request.clone());
        if cancel.load(Ordering::Acquire) && !request.cleanup() {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Canceled,
                "synthetic original cancellation",
            )
            .into());
        }
        if request.cleanup()
            && let Some(seal) = self.original_cleanup_probe.lock().unwrap().as_ref()
        {
            self.original_cleanup_probe_results.lock().unwrap().push((
                request.action,
                crate::metadata_export::PhotoPublication::prepare(seal).is_err(),
            ));
        }
        if request.cleanup() && self.fail_original_abort.load(Ordering::Acquire) {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Unknown,
                "synthetic lost original abort",
            )
            .into());
        }
        let value = match request.action {
            ExportOriginalAction::Begin => {
                if self.fail_original_begin_reply.swap(false, Ordering::AcqRel) {
                    return Err(crate::filesystem_worker::wire::Failure::new(
                        crate::filesystem_worker::wire::FailureKind::Unknown,
                        "synthetic lost original begin reply",
                    )
                    .into());
                }
                ExportOriginalValue::Begun {
                    revision: match self.export_original_revision.lock().unwrap().clone() {
                        Some(value) => value,
                        None => crate::metadata_export::inspect_file_revision(
                            &request.requested.to_path()?,
                            request.allowance.0,
                        )?,
                    },
                }
            }
            ExportOriginalAction::Recheck => ExportOriginalValue::Rechecked,
            ExportOriginalAction::Finish => {
                if self
                    .fail_original_finish_reply
                    .swap(false, Ordering::AcqRel)
                {
                    return Err(crate::filesystem_worker::wire::Failure::new(
                        crate::filesystem_worker::wire::FailureKind::Unknown,
                        "synthetic lost original finish reply",
                    )
                    .into());
                }
                ExportOriginalValue::Finished
            }
            ExportOriginalAction::Abort => ExportOriginalValue::Aborted,
        };
        Ok(ExportOriginalReply {
            root: request.root.clone(),
            requested: request.requested.clone(),
            transfer: request.transfer.clone(),
            step: request.step,
            value,
        })
    }
    fn export_publication_call(
        &self,
        request: &ExportPublicationRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportPublicationReply> {
        request.validate()?;
        self.export_publication_requests
            .lock()
            .unwrap()
            .push(request.clone());
        if self.reject_publication.load(Ordering::Acquire) {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Rejected,
                "synthetic pre-admission rejection",
            )
            .into());
        }
        if cancel.load(Ordering::Acquire) && !request.cleanup() {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Canceled,
                "synthetic publication cancellation",
            )
            .into());
        }
        let mut state = self.export_publication.lock().unwrap();
        match &*state {
            TestPublicationState::Active { last, .. } if last.0 == *request => {
                if self
                    .fail_publication_step_reply
                    .swap(false, Ordering::AcqRel)
                {
                    return Err(crate::filesystem_worker::wire::Failure::new(
                        crate::filesystem_worker::wire::FailureKind::Unknown,
                        "synthetic lost publication step reply",
                    )
                    .into());
                }
                return last.1.clone().map_err(anyhow::Error::new);
            }
            TestPublicationState::Terminal {
                request: prior,
                reply,
            } if prior == request => {
                if self.fail_publication_abort.load(Ordering::Acquire) {
                    return Err(crate::filesystem_worker::wire::Failure::new(
                        crate::filesystem_worker::wire::FailureKind::Unknown,
                        "synthetic lost publication cleanup reply",
                    )
                    .into());
                }
                return Ok(reply.clone());
            }
            _ => {}
        }
        if matches!(request.action, ExportPublicationAction::Begin) {
            ensure!(
                matches!(
                    &*state,
                    TestPublicationState::Empty | TestPublicationState::Terminal { .. }
                ),
                "synthetic publication already active"
            );
            let prepared = (|| -> Result<_> {
                let seal = match &request.source {
                    ExportPublicationSource::Sealed(seal) => seal.clone(),
                    ExportPublicationSource::Recovery {
                        snapshot,
                        authority_digest,
                    } => crate::metadata_export::read_photo_seal_for_restore_with_checkpoint(
                        snapshot,
                        authority_digest,
                        &mut |_| Ok(()),
                    )?,
                };
                let publication = match request.mode {
                    ExportPublicationMode::Publish => {
                        crate::metadata_export::PhotoPublication::prepare(&seal)?
                    }
                    ExportPublicationMode::Restore => {
                        crate::metadata_export::PhotoPublication::prepare_restore(&seal)?
                    }
                };
                Ok((seal, publication))
            })()
            .map_err(test_publication_failure)?;
            let (seal, publication) = prepared;
            let reply = ExportPublicationReply {
                mode: request.mode,
                request_digest: request.digest()?,
                root: request.root.clone(),
                transfer: request.transfer.clone(),
                step: request.step,
                seal: seal.clone(),
                value: ExportPublicationValue::Begun {
                    installed: publication.installed(),
                },
                timings: publication.timings().clone(),
                hashed_bytes: U64(0),
            };
            reply.validate(request)?;
            *state = TestPublicationState::Active {
                transfer: request.transfer.clone(),
                source: request.source.clone(),
                next_step: 1,
                seal,
                publication,
                last: (request.clone(), Ok(reply.clone())),
            };
            if self
                .fail_publication_begin_reply
                .swap(false, Ordering::AcqRel)
            {
                return Err(crate::filesystem_worker::wire::Failure::new(
                    crate::filesystem_worker::wire::FailureKind::Unknown,
                    "synthetic lost publication begin reply",
                )
                .into());
            }
            return Ok(reply);
        }
        let TestPublicationState::Active {
            transfer,
            source,
            next_step,
            seal,
            publication,
            last,
        } = &mut *state
        else {
            anyhow::bail!("synthetic publication lease is not active")
        };
        ensure!(
            transfer == &request.transfer
                && source == &request.source
                && last.0.mode == request.mode,
            "synthetic publication provenance mismatch"
        );
        ensure!(
            *next_step == request.step.0,
            "synthetic publication step mismatch"
        );
        let value = (|| -> Result<_> {
            Ok(match &request.action {
                ExportPublicationAction::Begin => unreachable!(),
                ExportPublicationAction::RecheckPayload => {
                    publication.recheck_payload()?;
                    ExportPublicationValue::RecheckedPayload
                }
                ExportPublicationAction::Capture => {
                    publication.capture()?;
                    ExportPublicationValue::Captured
                }
                ExportPublicationAction::VerifyCapture => {
                    publication.verify_capture()?;
                    ExportPublicationValue::CaptureVerified
                }
                ExportPublicationAction::FailureReceipt { detail } => {
                    ExportPublicationValue::Receipt(publication.failure_receipt(detail.clone()))
                }
                ExportPublicationAction::Link => {
                    publication.link()?;
                    ExportPublicationValue::Linked
                }
                ExportPublicationAction::VerifyInstalled => {
                    ExportPublicationValue::Installed(publication.verify_installed()?)
                }
                ExportPublicationAction::RecheckInstalled => {
                    publication.recheck_installed()?;
                    ExportPublicationValue::RecheckedInstalled
                }
                ExportPublicationAction::RestoreLink => {
                    publication.restore_link()?;
                    ExportPublicationValue::RestoredLinked
                }
                ExportPublicationAction::VerifyRestored => {
                    ExportPublicationValue::Restored(publication.verify_restored()?)
                }
                ExportPublicationAction::RecheckRestored => {
                    publication.recheck_restored()?;
                    ExportPublicationValue::RecheckedRestored
                }
                ExportPublicationAction::Finish => ExportPublicationValue::Finished,
                ExportPublicationAction::Abort => ExportPublicationValue::Aborted,
            })
        })();
        let value = match value {
            Ok(value) => value,
            Err(error) => {
                let error = test_publication_failure(error);
                ExportPublicationValue::Failed(
                    error
                        .downcast_ref::<crate::filesystem_worker::wire::Failure>()
                        .unwrap()
                        .clone(),
                )
            }
        };
        let reply = ExportPublicationReply {
            mode: request.mode,
            request_digest: request.digest()?,
            root: request.root.clone(),
            transfer: request.transfer.clone(),
            step: request.step,
            seal: seal.clone(),
            value,
            timings: publication.timings().clone(),
            hashed_bytes: U64(0),
        };
        reply.validate(request)?;
        *next_step = next_step
            .checked_add(1)
            .context("synthetic publication step exhausted")?;
        *last = (request.clone(), Ok(reply.clone()));
        let terminal =
            request.cleanup() && !matches!(reply.value, ExportPublicationValue::Failed(_));
        if terminal {
            *state = TestPublicationState::Terminal {
                request: request.clone(),
                reply: reply.clone(),
            };
        }
        if matches!(request.action, ExportPublicationAction::Abort)
            && self.fail_publication_abort.load(Ordering::Acquire)
        {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Unknown,
                "synthetic lost publication cleanup reply",
            )
            .into());
        }
        let lose_action = {
            let mut action = self.lose_publication_action.lock().unwrap();
            if action.as_ref() == Some(&request.action) {
                action.take();
                true
            } else {
                false
            }
        };
        if lose_action {
            if self.cancel_lost_publication_reply.load(Ordering::Acquire) {
                cancel.store(true, Ordering::Release);
            }
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Unknown,
                "synthetic lost publication mutation reply",
            )
            .into());
        }
        if self
            .fail_publication_step_reply
            .swap(false, Ordering::AcqRel)
        {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Unknown,
                "synthetic lost publication step reply",
            )
            .into());
        }
        Ok(reply)
    }
    fn export_destination_snapshot(
        &self,
        request: &ExportDestinationSnapshotRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportDestinationSnapshotReply> {
        request.validate()?;
        self.export_snapshot_requests
            .lock()
            .unwrap()
            .push(request.clone());
        if cancel.load(Ordering::Acquire) {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Canceled,
                "synthetic export snapshot cancellation",
            )
            .into());
        }
        let snapshot = crate::metadata_export::snapshot_photo_destination(
            &request.destination.to_path()?,
            request.max_existing_bytes.0,
        )?;
        Ok(ExportDestinationSnapshotReply {
            root: request.root.clone(),
            requested: request.destination.clone(),
            snapshot,
        })
    }
    fn export_alias_fact(
        &self,
        request: &ExportAliasFactRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportAliasFactReply> {
        request.validate()?;
        self.export_alias_requests
            .lock()
            .unwrap()
            .push(request.clone());
        if cancel.load(Ordering::Acquire) {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Canceled,
                "synthetic export alias cancellation",
            )
            .into());
        }
        Ok(ExportAliasFactReply {
            root: request.root.clone(),
            path: request.path.clone(),
            kind: request.kind,
            value: crate::catalog_export_alias::local_alias_fact(&request.path, request.kind)?,
        })
    }
    fn export_profile_call(
        &self,
        request: &ExportProfileRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportProfileReply> {
        request.validate()?;
        self.export_profile_requests
            .lock()
            .unwrap()
            .push(request.clone());
        if cancel.load(Ordering::Acquire) && !request.cleanup() {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Canceled,
                "synthetic export profile cancellation",
            )
            .into());
        }
        if request.cleanup() && self.fail_profile_abort.load(Ordering::Acquire) {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Unknown,
                "synthetic lost export profile abort",
            )
            .into());
        }
        let profile = self.export_profile.lock().unwrap();
        let value = match &request.action {
            ExportProfileAction::Begin => {
                if self.cancel_profile_after_begin.load(Ordering::Acquire) {
                    cancel.store(true, Ordering::Release);
                }
                if self.fail_profile_begin_reply.load(Ordering::Acquire) {
                    return Err(crate::filesystem_worker::wire::Failure::new(
                        crate::filesystem_worker::wire::FailureKind::Unknown,
                        "synthetic lost export profile begin reply",
                    )
                    .into());
                }
                ExportProfileValue::Begun {
                    bytes: U64(profile.len() as u64),
                }
            }
            ExportProfileAction::Read { offset } => {
                let start = usize::try_from(offset.0)?;
                let end = profile
                    .len()
                    .min(start + crate::catalog_session::preview_io::CHUNK_BYTES);
                ensure!(start < end, "synthetic profile offset");
                let bytes = profile[start..end].to_vec();
                ExportProfileValue::Chunk {
                    offset: *offset,
                    checksum: blake3::hash(&bytes).to_hex().to_string(),
                    bytes,
                }
            }
            ExportProfileAction::Finish => {
                if self.fail_profile_finish_reply.load(Ordering::Acquire) {
                    return Err(crate::filesystem_worker::wire::Failure::new(
                        crate::filesystem_worker::wire::FailureKind::Unknown,
                        "synthetic lost export profile finish reply",
                    )
                    .into());
                }
                ExportProfileValue::Finished {
                    bytes: U64(profile.len() as u64),
                }
            }
            ExportProfileAction::Abort => ExportProfileValue::Aborted,
        };
        Ok(ExportProfileReply {
            root: request.root.clone(),
            requested: request.requested.clone(),
            transfer: request.transfer.clone(),
            step: request.step,
            value,
        })
    }
    fn prepare_export_directory(
        &self,
        request: &PrepareExportDirectory,
        cancel: &AtomicBool,
    ) -> Result<PreparedExportDirectory> {
        if cancel.load(Ordering::Acquire) {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Canceled,
                "synthetic export directory cancellation",
            )
            .into());
        }
        self.export_requests.lock().unwrap().push(request.clone());
        Ok(PreparedExportDirectory {
            root: request.root.clone(),
            requested: request.directory.clone(),
            directory: self.export_directory.clone(),
        })
    }
    fn prepare_catalog(
        &self,
        request: &PrepareCatalog,
        _: &AtomicBool,
    ) -> Result<CatalogBootstrap> {
        ensure!(
            !self.fail_prepare.load(Ordering::Acquire),
            "injected lost Prepare result"
        );
        ensure!(request.session == self.bootstrap.session);
        Ok(self.bootstrap.clone())
    }
    fn abandon_prepare(&self, operation: U64, session: &LeaseId) -> Result<()> {
        ensure!(operation == self.bootstrap.operation && session == &self.bootstrap.session);
        self.abandons.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn confirm_sql_admission(
        &self,
        request: &ConfirmSqlAdmission,
        _: &AtomicBool,
    ) -> Result<SqlAdmissionConfirmed> {
        request.validate_for(&self.bootstrap)?;
        ensure!(
            !self.fail_confirmation.load(Ordering::Acquire),
            "injected confirmation loss"
        );
        if self.fresh {
            ensure!(
                self._pins[0].metadata()?.len() == 0,
                "application SQL preceded F confirmation"
            );
        }
        self.confirms.fetch_add(1, Ordering::AcqRel);
        Ok(request.clone())
    }
    fn restore_status(&self, root: &RootCapability) -> Result<Option<RestoreStatus>> {
        ensure!(root == &self.bootstrap.root_capability());
        if self.empty_restore_status.load(Ordering::Acquire) {
            return Ok(None);
        }
        anyhow::bail!("synthetic F marker observation: no local fallback")
    }
    fn resume_restored_jobs(
        &self,
        root: &RootCapability,
        _: &str,
        _: bool,
    ) -> Result<RestoreStatus> {
        ensure!(root == &self.bootstrap.root_capability());
        anyhow::bail!("synthetic F resume observation: no local fallback")
    }
    fn release_root(&self, root: &RootCapability) -> Result<()> {
        if self.fatal_drop_sentry {
            std::process::exit(97);
        }
        ensure!(root == &self.bootstrap.root_capability());
        self.releases.fetch_add(1, Ordering::AcqRel);
        ensure!(
            !self.fail_release.load(Ordering::Acquire),
            "injected F release failure"
        );
        Ok(())
    }
}

impl export_native::CatalogExportNative for Facts {
    fn call(
        &self,
        request: &export_native::Request,
        _cancel: &AtomicBool,
    ) -> Result<export_native::Status> {
        request.validate()?;
        self.export_native_requests
            .lock()
            .unwrap()
            .push(request.clone());
        if matches!(request.action, export_native::Action::Register { .. })
            && self.reject_export_register.load(Ordering::Acquire)
        {
            let mut failure = crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Rejected,
                "export configured working allowance exceeds shared native pool",
            );
            failure.object_receipt = Some(preview_io::FailureReceipt {
                operation: request.operation,
                step: U64(0),
                request_digest: request.digest()?,
            });
            return Err(failure.into());
        }
        let previous = self.export_native_status.lock().unwrap().clone();
        let action_name = match request.action {
            export_native::Action::Register { .. } => "register",
            export_native::Action::Spawn => "spawn",
            export_native::Action::Start => "start",
            export_native::Action::Stop => "stop",
            export_native::Action::RetryDrain => "retry_drain",
            export_native::Action::Retire => "retire",
        };
        let phase = match request.action {
            export_native::Action::Register { .. } => export_native::Phase::Registered,
            export_native::Action::Spawn if self.fail_export_spawn.load(Ordering::Acquire) => {
                export_native::Phase::WaitFailed
            }
            export_native::Action::Spawn => export_native::Phase::Spawned,
            export_native::Action::Start => export_native::Phase::Running,
            export_native::Action::Stop => export_native::Phase::StopRequested,
            export_native::Action::RetryDrain => export_native::Phase::Drained,
            export_native::Action::Retire => export_native::Phase::Released,
        };
        let status = export_native::Status {
            epoch: request.root.epoch.clone(),
            session: request.root.session.clone(),
            operation: request.operation,
            stage: request.stage.clone(),
            binding: request.binding.clone(),
            pid: if matches!(
                phase,
                export_native::Phase::Spawned | export_native::Phase::Running
            ) {
                Some(42)
            } else {
                previous.as_ref().and_then(|status| status.pid)
            },
            phase,
            started: matches!(
                phase,
                export_native::Phase::Running
                    | export_native::Phase::StopRequested
                    | export_native::Phase::Drained
                    | export_native::Phase::Released
            ),
            exit_code: (phase == export_native::Phase::Drained).then_some(0),
            success: (phase == export_native::Phase::Drained)
                .then_some(!self.fail_export_spawn.load(Ordering::Acquire)),
            stage_high_water: previous
                .as_ref()
                .map_or(U64(0), |status| status.stage_high_water),
            pending_stage_operation: previous.and_then(|status| status.pending_stage_operation),
            pending_dispatch: export_native::DispatchState::None,
            error: self
                .fail_export_spawn
                .load(Ordering::Acquire)
                .then(|| "export native launch failed before Child return".into()),
        };
        *self.export_native_status.lock().unwrap() = Some(status.clone());
        if self.lose_export_native_action.lock().unwrap().as_deref() == Some(action_name) {
            self.lose_export_native_action.lock().unwrap().take();
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Unknown,
                format!("synthetic lost {action_name} acknowledgement"),
            )
            .into());
        }
        Ok(status)
    }

    fn status(&self, key: &export_native::Key) -> Result<export_native::Status> {
        key.validate()?;
        let mut slot = self.export_native_status.lock().unwrap();
        if self.drain_export_native_on_status.load(Ordering::Acquire)
            && slot.as_ref().is_some_and(|status| {
                matches!(
                    status.phase,
                    export_native::Phase::Running | export_native::Phase::StopRequested
                )
            })
        {
            let status = slot.as_mut().unwrap();
            status.phase = export_native::Phase::Drained;
            status.exit_code = Some(0);
            status.success = Some(true);
        }
        let mut status = slot
            .clone()
            .context("synthetic export native status missing")?;
        drop(slot);
        ensure!(
            status.epoch == key.epoch
                && status.session == key.session
                && status.operation == key.operation
                && status.stage == key.stage,
            "synthetic export native key mismatch"
        );
        if self.lose_export_status.swap(false, Ordering::AcqRel) {
            return Err(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::Unknown,
                "lost native Status acknowledgement",
            )
            .into());
        }
        if self
            .foreign_export_native_status_binding
            .swap(false, Ordering::AcqRel)
        {
            status.binding.job.push_str("-foreign");
        }
        Ok(status)
    }
}

#[test]
fn managed_export_directory_uses_exact_session_capability_and_preserves_cancellation() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    fs::create_dir(temp.path().join("exports"))?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    let requested = NativePath::from_path(&temp.path().join("selected-output"));
    assert_eq!(
        session
            .authority
            .prepare_export_directory(&requested, &AtomicBool::new(false))?,
        Some(facts.export_directory.clone())
    );
    let calls = facts.export_requests.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].root, facts.bootstrap.root_capability());
    assert_eq!(calls[0].directory, requested);
    drop(calls);
    let error = session
        .authority
        .prepare_export_directory(&requested, &AtomicBool::new(true))
        .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<crate::filesystem_worker::wire::Failure>()
            .unwrap()
            .kind,
        crate::filesystem_worker::wire::FailureKind::Canceled
    );
    session.close()?;
    Ok(())
}

#[test]
fn admission_exact_roster_and_join_before_reuse() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    assert_eq!(facts.confirms.load(Ordering::Acquire), 1);
    let catalog = session.catalog.as_ref().unwrap();
    let relink = catalog.relink_worker_handle()?;
    let export = catalog.sql_worker_handle(SqlRole::Export)?;
    let worker = relink.clone().open()?;
    let export_worker = export.open()?;
    assert!(Arc::ptr_eq(&catalog.session, &worker.session));
    assert!(relink.clone().open().is_err());
    drop(worker);
    assert!(
        relink.clone().open().is_err(),
        "return alone must not free worker role"
    );
    session.authority.joined(SqlRole::Relink, true)?;
    drop(relink.clone().open()?);
    session.authority.joined(SqlRole::Relink, true)?;
    drop(export_worker);
    session.authority.joined(SqlRole::Export, true)?;
    let pool = session.authority.pool().unwrap();
    let mut searches = Vec::new();
    for _ in 0..4 {
        searches.push(pool.lease_search()?);
    }
    assert!(pool.lease_search().is_err());
    for (role, db) in searches {
        drop(db);
        pool.joined(role, true)?;
    }
    let discovery = pool.lease(DISCOVERY_ROLE)?;
    assert!(pool.lease(DISCOVERY_ROLE).is_err());
    drop(discovery);
    pool.joined(DISCOVERY_ROLE, true)?;
    session.close()?;
    assert_eq!(facts.releases.load(Ordering::Acquire), 1);
    assert!(relink.clone().open().is_err());
    Ok(())
}

#[test]
fn marker_authority_and_failed_release_are_retained_for_explicit_retry() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    let catalog = session.catalog.as_ref().unwrap();
    assert!(
        catalog
            .require_jobs_released()
            .unwrap_err()
            .to_string()
            .contains("no local fallback")
    );
    assert!(
        catalog
            .resume_restored_jobs("exact", true)
            .unwrap_err()
            .to_string()
            .contains("no local fallback")
    );
    facts.fail_release.store(true, Ordering::Release);
    assert!(session.close().is_err());
    assert!(!session.authority.pool().unwrap().is_closed());
    assert_eq!(facts.releases.load(Ordering::Acquire), 1);
    facts.fail_release.store(false, Ordering::Release);
    session.close()?;
    assert_eq!(facts.releases.load(Ordering::Acquire), 2);
    drop(session);
    assert_eq!(facts.releases.load(Ordering::Acquire), 2);
    Ok(())
}

#[test]
fn failed_prepare_reconciles_original_operation_without_sql() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    facts.fail_prepare.store(true, Ordering::Release);
    let failure = ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false))
        .err()
        .unwrap();
    assert!(!failure.is_poisoned());
    assert!(failure.into_cleanup().is_none());
    assert_eq!(facts.abandons.load(Ordering::Acquire), 1);
    assert_eq!(facts.confirms.load(Ordering::Acquire), 0);
    assert_eq!(facts._pins[0].metadata()?.len(), 0);
    Ok(())
}

#[test]
fn managed_authority_is_exact_arc_while_legacy_exports_keep_physical_compatibility() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session = ManagedSession::admit(facts, &request, &AtomicBool::new(false)).unwrap();
    let authority = session.authority.clone();
    let AuthorityMode::Managed {
        filesystem,
        root,
        pool,
    } = &authority.mode
    else {
        unreachable!()
    };
    let other = Arc::new(CatalogSessionAuthority {
        physical: authority.physical,
        mode: AuthorityMode::Managed {
            filesystem: filesystem.clone(),
            root: root.clone(),
            pool: pool.clone(),
        },
        searches: Mutex::new(Vec::new()),
        original: Arc::new(Mutex::new(None)),
        publication: Arc::new(Mutex::new(None)),
        managed_export: export_managed::Registry::default(),
    });
    assert!(CatalogSessionAuthority::export_matches(
        &authority, &authority
    )?);
    assert!(!CatalogSessionAuthority::export_matches(
        &authority, &other
    )?);
    session.close()?;
    let path = temp.path().join("legacy");
    let first = Catalog::open(&path)?;
    let second = Catalog::open(&path)?;
    assert!(!Arc::ptr_eq(&first.session, &second.session));
    assert!(CatalogSessionAuthority::export_matches(
        &first.session,
        &second.session
    )?);
    assert!(!CatalogSessionAuthority::export_matches(
        &authority,
        &first.session
    )?);
    Ok(())
}

#[test]
fn managed_export_open_releases_claim_after_bad_ack_and_replays_exact_acquire() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    facts
        .bad_export_executor_digest
        .store(true, Ordering::Release);
    assert!(
        session
            .authority
            .open_managed_export(&AtomicBool::new(false))
            .is_err()
    );
    facts
        .wrong_export_executor_value
        .store(true, Ordering::Release);
    assert!(
        session
            .authority
            .open_managed_export(&AtomicBool::new(false))
            .is_err()
    );
    let mut executor = session
        .authority
        .open_managed_export(&AtomicBool::new(false))?
        .unwrap();
    let requests = facts.export_executor_requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[1], requests[2]);
    drop(requests);
    executor.close()?;
    session.close()?;
    Ok(())
}

#[test]
fn managed_export_open_claim_serializes_the_entire_acquire_relay() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    *facts.export_executor_enter.lock().unwrap() = Some(entered.clone());
    *facts.export_executor_release.lock().unwrap() = Some(release.clone());
    let authority = session.authority.clone();
    let opened = std::thread::spawn(move || authority.open_managed_export(&AtomicBool::new(false)));
    entered.wait();
    assert!(
        session
            .authority
            .open_managed_export(&AtomicBool::new(false))
            .err()
            .unwrap()
            .to_string()
            .contains("already open")
    );
    release.wait();
    let mut executor = opened.join().unwrap()?.unwrap();
    assert_eq!(facts.export_executor_requests.lock().unwrap().len(), 1);
    executor.close()?;
    session.close()?;
    Ok(())
}

#[test]
fn managed_export_open_finishes_drop_with_lost_recover_before_successor_acquire() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    let mut executor = session
        .authority
        .open_managed_export(&AtomicBool::new(false))?
        .unwrap();
    facts
        .bad_export_executor_digest
        .store(true, Ordering::Release);
    assert!(executor.recover(8, &AtomicBool::new(false)).is_err());
    facts
        .bad_export_executor_digest
        .store(true, Ordering::Release);
    assert!(executor.close().is_err());
    executor.release_claim_after_failed_drop();
    drop(executor);

    let mut successor = session
        .authority
        .open_managed_export(&AtomicBool::new(false))?
        .unwrap();
    let requests = facts.export_executor_requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(matches!(
        requests[0].action,
        export_executor::Action::Acquire
    ));
    assert_eq!(requests[1], requests[2]);
    assert_eq!(requests[2], requests[3]);
    assert!(matches!(
        requests[3].action,
        export_executor::Action::Recover { .. }
    ));
    assert!(matches!(
        requests[4].action,
        export_executor::Action::Release
    ));
    assert!(matches!(
        requests[5].action,
        export_executor::Action::Acquire
    ));
    assert_ne!(requests[0].executor, requests[5].executor);
    drop(requests);
    successor.close()?;
    session.close()?;
    Ok(())
}

#[test]
fn managed_export_status_and_stage_replay_require_exact_bound_evidence() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let original = temp.path().join("managed-status-original.png");
    fs::write(&original, b"managed status original")?;
    let (facts, request) = Facts::create(temp.path())?;
    facts.empty_restore_status.store(true, Ordering::Release);
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    let catalog = session.catalog.as_mut().unwrap();
    let fingerprint = blake3::hash(b"managed status original")
        .to_hex()
        .to_string();
    catalog.db.execute("INSERT INTO assets(id,location,path_display,state,fingerprint,preview_hash,metadata) VALUES('managed-status',?1,?2,'ready',?3,'fixture','{\"format\":\"PNG\",\"width\":32,\"height\":24,\"orientation\":1,\"camera_make\":null,\"camera_model\":null,\"captured_at\":null,\"preview_source\":\"fixture\"}')", rusqlite::params![crate::location_bytes(&original), original.to_string_lossy(), fingerprint])?;
    catalog.record_storage_path("managed-status", &NativePath::from_path(&original))?;
    let job = catalog.begin_photo_export()?;
    catalog.append_photo_export(
        &job.id,
        0,
        &crate::catalog_exports::ExportTarget {
            key: crate::catalog_edits::VariantKey::master("managed-status"),
            expected_revision: 0,
            destination: temp.path().join("managed-status-output.png"),
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
        1024,
        1024,
    )?;
    catalog.seal_photo_export_job(&job.id, 1)?;
    let work = catalog.claim_photo_export(&job.id)?.unwrap();
    let render = crate::edit::RenderLimits {
        max_pixels: 1024,
        max_allocation_bytes: 1024 * 1024,
        max_live_bytes: 1024 * 1024,
    };
    let limits = crate::export_service::ExportServiceLimits {
        worker_bytes: 2 * 1024 * 1024,
        working_bytes: 2 * 1024 * 1024,
        render: crate::photo_render::PhotoRenderLimits {
            decode: crate::media::DecodeLimits {
                max_encoded_bytes: 1024 * 1024,
                max_intermediate_pixels: 1024,
                max_allocation_bytes: 1024 * 1024,
            },
            render,
            encode: crate::image_export::EncodeLimits {
                render,
                ..Default::default()
            },
            max_encoded_extent: 1024 * 1024,
        },
    };
    let mut executor = session
        .authority
        .open_managed_export(&AtomicBool::new(false))?
        .unwrap();
    let mut attempt = executor.prepare_attempt(work, limits)?;
    facts.reject_export_register.store(true, Ordering::Release);
    let error = attempt.register(&AtomicBool::new(false)).unwrap_err();
    assert!(attempt.registration_rejected(&error));
    let mut foreign = error
        .downcast_ref::<crate::filesystem_worker::wire::Failure>()
        .unwrap()
        .clone();
    foreign.object_receipt.as_mut().unwrap().request_digest[0] ^= 1;
    assert!(!attempt.registration_rejected(&foreign.clone().into()));
    foreign.object_receipt = None;
    assert!(!attempt.registration_rejected(&foreign.into()));
    facts.reject_export_register.store(false, Ordering::Release);
    attempt.register(&AtomicBool::new(false))?;
    facts
        .foreign_export_native_status_binding
        .store(true, Ordering::Release);
    assert!(attempt.status().is_err());
    facts.lose_export_stage_reply.store(true, Ordering::Release);
    assert!(attempt.begin(&AtomicBool::new(false)).is_err());
    assert!(attempt.pending_stage());
    attempt.begin(&AtomicBool::new(false))?;
    assert!(!attempt.pending_stage());
    let stage_requests = facts.export_stage_requests.lock().unwrap();
    assert_eq!(stage_requests[0].digest()?, stage_requests[1].digest()?);
    drop(stage_requests);

    facts.lose_export_stage_reply.store(true, Ordering::Release);
    assert!(
        attempt
            .stage(export_stage::Action::ResultAndSeal, &AtomicBool::new(false),)
            .is_err()
    );
    assert!(attempt.pending_stage());
    let reply = attempt.stage(export_stage::Action::ResultAndSeal, &AtomicBool::new(false))?;
    assert!(matches!(reply.value, export_stage::Value::Completed { .. }));
    assert!(!attempt.pending_stage());
    let stage_requests = facts.export_stage_requests.lock().unwrap();
    let last = stage_requests.len() - 1;
    assert_eq!(
        stage_requests[last - 1].digest()?,
        stage_requests[last].digest()?
    );
    drop(stage_requests);

    facts
        .reject_export_stage_reply
        .store(true, Ordering::Release);
    assert!(
        attempt
            .stage(
                export_stage::Action::UploadIcc {
                    offset: U64(0),
                    bytes: vec![1],
                },
                &AtomicBool::new(false),
            )
            .is_err()
    );
    assert!(!attempt.pending_stage());
    assert!(attempt.take_terminal_stage_failure().is_some());
    executor.close()?;
    session.close()?;
    Ok(())
}

#[test]
fn admission_wire_rejects_wrong_roles_and_noncanonical_identity() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, _) = Facts::create(temp.path())?;
    let bootstrap = &facts.bootstrap;
    let mut confirmation = ConfirmSqlAdmission {
        operation: bootstrap.operation,
        root: bootstrap.root_capability(),
        roles: std::array::from_fn(|i| SqlRoleObservation {
            role: SQL_ROLES[i],
            physical: if i == 7 {
                bootstrap.manifest.physical
            } else {
                bootstrap.catalog.physical
            },
        }),
    };
    confirmation.validate_for(bootstrap)?;
    confirmation.roles[1].role = SqlRole::Actor;
    assert!(confirmation.validate_for(bootstrap).is_err());
    assert!(serde_json::from_str::<LeaseId>("\"00000000-0000-0000-0000-00000000000A\"").is_err());
    let mut future = bootstrap.clone();
    future.version = 2;
    assert!(future.validate().is_err());
    let mut swapped = bootstrap.clone();
    swapped.manifest.physical = swapped.catalog.physical;
    assert!(swapped.validate().is_err());
    #[cfg(unix)]
    {
        assert!(validate_path(&NativePath::UnixBytes(vec![b'/'; PATH_UNITS + 1])).is_err());
        assert!(
            PhysicalObjectId::Windows {
                volume_serial: U64(1),
                file_index: U64(2)
            }
            .validate()
            .is_err()
        );
        let identity = PhysicalObjectId::Unix {
            device: U64(u64::MAX),
            inode: U64(u64::MAX - 1),
        };
        assert_eq!(
            serde_json::from_str::<PhysicalObjectId>(&serde_json::to_string(&identity)?)?,
            identity
        );
    }
    Ok(())
}

#[test]
fn poison_process_child() -> Result<()> {
    let Some(base) = std::env::var_os("PHOTOCATALOG_SQL_POISON_FIXTURE") else {
        return Ok(());
    };
    let (mut facts, request) = Facts::create(&PathBuf::from(base))?;
    Arc::get_mut(&mut facts).unwrap().fatal_drop_sentry = true;
    if std::env::var_os("PHOTOCATALOG_SQL_POISON_WRONG_LAST").is_some() {
        let wrong = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(request.root.to_path()?.join("wrong-object"))?;
        let facts = Arc::get_mut(&mut facts).unwrap();
        facts.bootstrap.manifest.physical = crate::catalog_storage::physical_object_id(&wrong)?;
        facts._pins.push(wrong);
    }
    facts.fail_confirmation.store(true, Ordering::Release);
    unsafe extern "C" fn on_close(
        _: u32,
        _: *mut std::ffi::c_void,
        _: *mut std::ffi::c_void,
        _: *mut std::ffi::c_void,
    ) -> i32 {
        // Any SQLite close before bootstrap process retirement is a test failure.
        std::process::exit(99)
    }
    let failure = ManagedSession::admit_observed(facts, &request, &AtomicBool::new(false), |db| {
        let code = unsafe {
            rusqlite::ffi::sqlite3_trace_v2(
                db.handle(),
                rusqlite::ffi::SQLITE_TRACE_CLOSE,
                Some(on_close),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(code, rusqlite::ffi::SQLITE_OK);
    })
    .err()
    .unwrap();
    assert!(failure.is_poisoned());
    failure.retire_poisoned()
}

#[test]
fn unknown_admission_retires_owned_child_without_sql_drop() -> Result<()> {
    for wrong_last in [false, true] {
        let temp = tempfile::tempdir()?;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                "catalog_session::tests::poison_process_child",
                "--nocapture",
            ])
            .env("PHOTOCATALOG_SQL_POISON_FIXTURE", temp.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if wrong_last {
            command.env("PHOTOCATALOG_SQL_POISON_WRONG_LAST", "1");
        }
        let mut child = command.spawn()?;
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if start.elapsed() > std::time::Duration::from_secs(15) {
                child.kill()?;
                child.wait()?;
                anyhow::bail!("owned bootstrap child did not retire");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(
            status.code(),
            Some(74),
            "99 means SQLite close, 98 filesystem destructor, 97 filesystem release; all must be suppressed until retirement"
        );
        assert_eq!(
            fs::metadata(temp.path().join("catalog/catalog.sqlite3"))?.len(),
            0
        );
        assert_eq!(
            fs::metadata(temp.path().join("cache/previews.sqlite3"))?.len(),
            0
        );
    }
    Ok(())
}

#[test]
fn prepared_variant_edit_uses_inherited_managed_arc_and_rejects_fresh_arc() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session = ManagedSession::admit(facts, &request, &AtomicBool::new(false)).unwrap();
    let catalog = session.catalog.as_mut().unwrap();
    catalog.db.execute("INSERT INTO assets(id,location,path_display,state) VALUES('a',X'61','synthetic unavailable original','pending')",[])?;
    let master = crate::catalog_edits::VariantKey::master("a");
    let copy = catalog.create_edit_variant(&master, 0, "copy")?.key;
    let identity = catalog.image_metadata_identity(&copy)?;
    let edits = [crate::xmp::Edit::Set {
        namespace: crate::xmp::XMP.into(),
        path: "Label".into(),
        value: "green".into(),
    }];
    let prepared = catalog.prepare_metadata_edit(
        &identity.image_id,
        identity.metadata_revision,
        None,
        &edits,
        &[],
    )?;
    let same = catalog.session.clone();
    let AuthorityMode::Managed {
        filesystem,
        root,
        pool,
    } = &same.mode
    else {
        unreachable!()
    };
    catalog.session = Arc::new(CatalogSessionAuthority {
        physical: same.physical,
        mode: AuthorityMode::Managed {
            filesystem: filesystem.clone(),
            root: root.clone(),
            pool: pool.clone(),
        },
        searches: Mutex::new(Vec::new()),
        original: Arc::new(Mutex::new(None)),
        publication: Arc::new(Mutex::new(None)),
        managed_export: export_managed::Registry::default(),
    });
    assert!(
        catalog
            .commit_prepared_metadata_edit(prepared, |_, _| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("another catalog session")
    );
    catalog.session = same;
    let prepared = catalog.prepare_metadata_edit(
        &identity.image_id,
        identity.metadata_revision,
        None,
        &edits,
        &[],
    )?;
    let handle = catalog.relink_worker_handle()?;
    let result = std::thread::spawn(move || {
        let mut worker = handle.open()?;
        worker.commit_prepared_metadata_edit(prepared, |_, _| Ok(()))
    })
    .join()
    .unwrap()?;
    session.authority.joined(SqlRole::Relink, true)?;
    assert_eq!(result.revision, identity.metadata_revision + 1);
    assert_eq!(
        session
            .catalog
            .as_ref()
            .unwrap()
            .image_metadata_identity(&master)?
            .metadata_revision,
        0
    );
    session.close()?;
    Ok(())
}

#[test]
fn managed_snapshot_close_drains_idle_owner_before_pool_release() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    let snapshot = session
        .catalog
        .as_ref()
        .unwrap()
        .search_session(crate::organization_search::Query::default(), 30)?;
    assert_eq!(facts.releases.load(Ordering::Acquire), 0);
    session.close()?;
    assert_eq!(facts.releases.load(Ordering::Acquire), 1);
    // Late explicit close joins the already-drained owner; no old interrupt can
    // touch an admitted role or issue another filesystem release.
    snapshot.close()?;
    assert_eq!(facts.releases.load(Ordering::Acquire), 1);
    Ok(())
}

pub(crate) fn unused_filesystem(base: &Path) -> Result<Arc<dyn CatalogFilesystem>> {
    Ok(Facts::create(base)?.0)
}

pub(crate) type ExportManagedSessionFixture = (
    ManagedSession,
    Arc<Mutex<Vec<PrepareExportDirectory>>>,
    NativePath,
);

pub(crate) fn export_managed_session(base: &Path) -> Result<ExportManagedSessionFixture> {
    let export_directory = base.join("exports");
    fs::create_dir(&export_directory)?;
    let (facts, request) = Facts::create(base)?;
    let requests = facts.export_requests.clone();
    let directory = NativePath::from_path(&export_directory.canonicalize()?);
    let session = ManagedSession::admit(facts, &request, &AtomicBool::new(false)).unwrap();
    Ok((session, requests, directory))
}

pub(crate) type ExportFactRequests = (
    Arc<Mutex<Vec<ExportDestinationSnapshotRequest>>>,
    Arc<Mutex<Vec<ExportAliasFactRequest>>>,
    Arc<Mutex<Vec<InspectExportOriginal>>>,
);
pub(crate) fn export_facts_managed_session(
    base: &Path,
) -> Result<(ManagedSession, ExportFactRequests)> {
    let (facts, request) = Facts::create(base)?;
    facts.empty_restore_status.store(true, Ordering::Release);
    let calls = (
        facts.export_snapshot_requests.clone(),
        facts.export_alias_requests.clone(),
        facts.inspect_original_requests.clone(),
    );
    let session = ManagedSession::admit(facts, &request, &AtomicBool::new(false)).unwrap();
    Ok((session, calls))
}

#[derive(Clone)]
pub(crate) struct ManagedExportTestControl {
    facts: Arc<Facts>,
}
impl ManagedExportTestControl {
    pub(crate) fn real_executor(&self) {
        *self.facts.real_export_executor.lock().unwrap() = Some(Default::default());
    }
    pub(crate) fn lose_executor(&self, action: &str) {
        *self.facts.lose_export_executor_action.lock().unwrap() = Some(action.into());
    }
    pub(crate) fn cancel_discard(&self, cancel: Arc<AtomicBool>) {
        *self.facts.cancel_after_discard.lock().unwrap() = Some(cancel);
    }
    pub(crate) fn lose_stage_action(&self, action: &str) {
        *self.facts.lose_export_stage_action.lock().unwrap() = Some(action.into());
    }
    pub(crate) fn reject_register(&self) {
        self.facts
            .reject_export_register
            .store(true, Ordering::Release);
    }
    pub(crate) fn fail_spawn(&self) {
        self.facts.fail_export_spawn.store(true, Ordering::Release);
    }
    pub(crate) fn native_failure(&self, phase: export_native::Phase) {
        self.facts.fail_export_spawn.store(true, Ordering::Release);
        let mut slot = self.facts.export_native_status.lock().unwrap();
        let status = slot.as_mut().expect("running fixture native slot");
        status.phase = phase;
        status.success = None;
        status.error = Some(format!("injected native {phase:?}"));
    }
    pub(crate) fn lose_status(&self) {
        self.facts.lose_export_status.store(true, Ordering::Release);
    }
    pub(crate) fn reject_release(&self) {
        self.facts
            .reject_export_release
            .store(true, Ordering::Release);
    }
    pub(crate) fn stage_requests(&self) -> Vec<export_stage::Request> {
        self.facts.export_stage_requests.lock().unwrap().clone()
    }
    pub(crate) fn native_requests(&self) -> Vec<export_native::Request> {
        self.facts.export_native_requests.lock().unwrap().clone()
    }
    pub(crate) fn executor_requests(&self) -> Vec<export_executor::Request> {
        self.facts.export_executor_requests.lock().unwrap().clone()
    }
    pub(crate) fn drain_native_on_status(&self, value: bool) {
        self.facts
            .drain_export_native_on_status
            .store(value, Ordering::Release);
    }
    pub(crate) fn lose_native(&self, action: &str) {
        *self.facts.lose_export_native_action.lock().unwrap() = Some(action.into());
    }
    pub(crate) fn lose_stage(&self) {
        self.facts
            .lose_export_stage_reply
            .store(true, Ordering::Release);
    }
    pub(crate) fn native_request_bytes(&self) -> Result<Vec<Vec<u8>>> {
        self.facts
            .export_native_requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| crate::filesystem_worker::wire::encode(request, ENVELOPE_BYTES))
            .collect()
    }
    pub(crate) fn stage_request_digests(&self) -> Result<Vec<[u8; 32]>> {
        self.facts
            .export_stage_requests
            .lock()
            .unwrap()
            .iter()
            .map(export_stage::Request::digest)
            .collect()
    }
}

pub(crate) fn managed_export_runtime_session(
    base: &Path,
) -> Result<(ManagedSession, ManagedExportTestControl)> {
    let (facts, request) = Facts::create(base)?;
    facts.empty_restore_status.store(true, Ordering::Release);
    let control = ManagedExportTestControl {
        facts: facts.clone(),
    };
    let session = ManagedSession::admit(facts, &request, &AtomicBool::new(false)).unwrap();
    Ok((session, control))
}

pub(crate) fn export_profile_managed_session(
    base: &Path,
    profile: Vec<u8>,
    cancel_after_begin: bool,
    fail_begin_reply: bool,
    fail_finish_reply: bool,
    fail_abort: bool,
) -> Result<(ManagedSession, Arc<Mutex<Vec<ExportProfileRequest>>>)> {
    let (facts, request) = Facts::create(base)?;
    *facts.export_profile.lock().unwrap() = profile;
    facts
        .cancel_profile_after_begin
        .store(cancel_after_begin, Ordering::Release);
    facts
        .fail_profile_begin_reply
        .store(fail_begin_reply, Ordering::Release);
    facts
        .fail_profile_finish_reply
        .store(fail_finish_reply, Ordering::Release);
    facts
        .fail_profile_abort
        .store(fail_abort, Ordering::Release);
    let requests = facts.export_profile_requests.clone();
    let session = ManagedSession::admit(facts, &request, &AtomicBool::new(false)).unwrap();
    Ok((session, requests))
}

pub(crate) struct ExportOriginalTestControl {
    pub(crate) inspections: Arc<Mutex<Vec<InspectExportOriginal>>>,
    pub(crate) calls: Arc<Mutex<Vec<ExportOriginalRequest>>>,
    pub(crate) cleanup_probe: Arc<Mutex<Option<crate::metadata_export::SealedPhotoExport>>>,
    pub(crate) cleanup_probe_results: Arc<Mutex<Vec<(ExportOriginalAction, bool)>>>,
    pub(crate) fail_finish_reply: Arc<AtomicBool>,
    pub(crate) fail_abort: Arc<AtomicBool>,
    pub(crate) publication_calls: Arc<Mutex<Vec<ExportPublicationRequest>>>,
    pub(crate) lose_publication_action: Arc<Mutex<Option<ExportPublicationAction>>>,
    pub(crate) cancel_lost_publication_reply: Arc<AtomicBool>,
}

pub(crate) fn export_original_managed_session(
    base: &Path,
) -> Result<(ManagedSession, ExportOriginalTestControl)> {
    let (facts, request) = Facts::create(base)?;
    facts.empty_restore_status.store(true, Ordering::Release);
    let control = ExportOriginalTestControl {
        inspections: facts.inspect_original_requests.clone(),
        calls: facts.export_original_requests.clone(),
        cleanup_probe: facts.original_cleanup_probe.clone(),
        cleanup_probe_results: facts.original_cleanup_probe_results.clone(),
        fail_finish_reply: facts.fail_original_finish_reply.clone(),
        fail_abort: facts.fail_original_abort.clone(),
        publication_calls: facts.export_publication_requests.clone(),
        lose_publication_action: facts.lose_publication_action.clone(),
        cancel_lost_publication_reply: facts.cancel_lost_publication_reply.clone(),
    };
    let session = ManagedSession::admit(facts, &request, &AtomicBool::new(false)).unwrap();
    Ok((session, control))
}

#[test]
fn managed_original_custody_survives_lost_replies_and_close_reconciles_exact_transfer() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let revision = crate::metadata_export::FileRevision {
        bytes: 7,
        digest: "ab".repeat(32),
        modified_ns: 11,
        identity: (12, 13),
    };
    *facts.export_original_revision.lock().unwrap() = Some(revision.clone());
    facts.empty_restore_status.store(true, Ordering::Release);
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    let requested = NativePath::from_path(&temp.path().join("not-visible-to-c.raw"));
    assert_eq!(
        session
            .authority
            .inspect_export_original(&requested, 8, &AtomicBool::new(false),)?,
        Some(revision.clone())
    );
    let mut lease = session
        .authority
        .begin_export_original(&requested, 8, &revision, &AtomicBool::new(false))?
        .unwrap();
    lease.recheck(&AtomicBool::new(false))?;
    lease.complete(Ok(()), &AtomicBool::new(false))?;

    facts
        .fail_original_begin_reply
        .store(true, Ordering::Release);
    facts.fail_original_abort.store(true, Ordering::Release);
    assert!(
        session
            .authority
            .begin_export_original(&requested, 8, &revision, &AtomicBool::new(false),)
            .is_err()
    );
    facts
        .fail_original_begin_reply
        .store(false, Ordering::Release);
    facts.fail_original_abort.store(false, Ordering::Release);
    session
        .authority
        .inspect_export_original(&requested, 8, &AtomicBool::new(false))?;

    let lease = session
        .authority
        .begin_export_original(&requested, 8, &revision, &AtomicBool::new(false))?
        .unwrap();
    let dropped_transfer = lease.transfer.clone();
    drop(lease);
    session
        .authority
        .inspect_export_original(&requested, 8, &AtomicBool::new(false))?;
    assert!(
        facts
            .export_original_requests
            .lock()
            .unwrap()
            .iter()
            .any(
                |request| matches!(request.action, ExportOriginalAction::Abort)
                    && request.transfer == dropped_transfer
            )
    );

    let lease = session
        .authority
        .begin_export_original(&requested, 8, &revision, &AtomicBool::new(false))?
        .unwrap();
    facts
        .fail_original_finish_reply
        .store(true, Ordering::Release);
    facts.fail_original_abort.store(true, Ordering::Release);
    assert!(lease.complete(Ok(()), &AtomicBool::new(false)).is_err());
    assert!(session.close().is_err());
    facts
        .fail_original_finish_reply
        .store(false, Ordering::Release);
    facts.fail_original_abort.store(false, Ordering::Release);
    session.close()?;

    let requests = facts.export_original_requests.lock().unwrap();
    assert!(matches!(requests[0].action, ExportOriginalAction::Begin));
    assert!(matches!(requests[1].action, ExportOriginalAction::Recheck));
    assert!(matches!(requests[2].action, ExportOriginalAction::Finish));
    assert!(requests.windows(2).any(|pair| {
        matches!(pair[0].action, ExportOriginalAction::Begin)
            && matches!(pair[1].action, ExportOriginalAction::Abort)
            && pair[0].transfer == pair[1].transfer
            && pair[1].step == U64(1)
    }));
    assert!(requests.iter().any(|request| {
        matches!(request.action, ExportOriginalAction::Abort) && request.step == U64(2)
    }));
    Ok(())
}

#[test]
fn managed_publication_replays_lost_steps_preserves_errors_and_close_reconciles_cleanup()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    facts.empty_restore_status.store(true, Ordering::Release);
    let destination = temp.path().join("publication-destination.jpg");
    fs::write(&destination, b"existing destination")?;
    let snapshot = crate::metadata_export::snapshot_photo_destination(&destination, 4096)?;
    let payload = temp.path().join("publication-payload.jpg");
    fs::write(&payload, b"new payload")?;
    let seal = crate::metadata_export::seal_photo_export(
        &snapshot,
        &payload,
        4096,
        &"ab".repeat(32),
        |_| Ok(()),
    )?;
    let source = ExportPublicationSource::Sealed(seal);
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();

    facts
        .fail_publication_begin_reply
        .store(true, Ordering::Release);
    let begin_error = session
        .authority
        .begin_export_publication(
            ExportPublicationMode::Publish,
            source.clone(),
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
    assert_eq!(
        begin_error
            .downcast_ref::<crate::filesystem_worker::wire::Failure>()
            .unwrap()
            .kind,
        crate::filesystem_worker::wire::FailureKind::Unknown
    );
    let begin_calls = facts.export_publication_requests.lock().unwrap().clone();
    assert_eq!(begin_calls[0], begin_calls[1]);
    assert!(matches!(
        begin_calls.last().unwrap().action,
        ExportPublicationAction::Abort
    ));

    let mut lease = session
        .authority
        .begin_export_publication(
            ExportPublicationMode::Publish,
            source.clone(),
            &AtomicBool::new(false),
        )?
        .unwrap();
    facts
        .fail_publication_step_reply
        .store(true, Ordering::Release);
    assert!(lease.capture(&AtomicBool::new(false)).is_err());
    let operation_error = lease
        .complete::<()>(Err(anyhow::anyhow!(
            "original publication operation failed"
        )))
        .unwrap_err();
    assert_eq!(
        operation_error.to_string(),
        "original publication operation failed"
    );
    let calls = facts.export_publication_requests.lock().unwrap().clone();
    let captures: Vec<_> = calls
        .iter()
        .filter(|call| matches!(call.action, ExportPublicationAction::Capture))
        .collect();
    assert_eq!(captures.len(), 2);
    assert_eq!(captures[0], captures[1]);
    assert!(matches!(
        calls.last().unwrap().action,
        ExportPublicationAction::Abort
    ));

    // A pre-admission rejection never consumes a step, including on Close.
    let mut lease = session
        .authority
        .begin_export_publication(
            ExportPublicationMode::Publish,
            source.clone(),
            &AtomicBool::new(false),
        )?
        .unwrap();
    facts.reject_publication.store(true, Ordering::Release);
    let error = lease.recheck_payload(&AtomicBool::new(false)).unwrap_err();
    let before = facts.export_publication_requests.lock().unwrap().len();
    assert!(lease.complete::<()>(Err(error)).is_err());
    assert_eq!(
        facts.export_publication_requests.lock().unwrap().len(),
        before + 1
    );
    assert!(session.authority.reconcile_export_publication().is_err());
    assert_eq!(
        facts.export_publication_requests.lock().unwrap().len(),
        before + 2
    );
    facts.reject_publication.store(false, Ordering::Release);
    session.authority.reconcile_export_publication()?;
    let calls = facts.export_publication_requests.lock().unwrap();
    assert_eq!(calls[before - 1].step, calls[before].step);
    assert!(matches!(
        calls[before].action,
        ExportPublicationAction::Abort
    ));
    assert_eq!(calls[before], calls[before + 1]);
    drop(calls);

    // Lost Finish must be replayed exactly, but successful caller work still
    // returns its first Unknown rather than claiming unproved success.
    let lease = session
        .authority
        .begin_export_publication(
            ExportPublicationMode::Publish,
            source.clone(),
            &AtomicBool::new(false),
        )?
        .unwrap();
    *facts.lose_publication_action.lock().unwrap() = Some(ExportPublicationAction::Finish);
    let error = lease.complete(Ok(())).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<crate::filesystem_worker::wire::Failure>()
            .unwrap()
            .kind,
        crate::filesystem_worker::wire::FailureKind::Unknown
    );
    assert!(session.authority.publication.lock().unwrap().is_none());
    let calls = facts.export_publication_requests.lock().unwrap();
    assert_eq!(calls[calls.len() - 2], calls[calls.len() - 1]);
    drop(calls);

    // An admitted failure is also an exact cached result. Reconciliation
    // consumes its Failed reply and can then close the retained owner.
    let mut lease = session
        .authority
        .begin_export_publication(
            ExportPublicationMode::Publish,
            source.clone(),
            &AtomicBool::new(false),
        )?
        .unwrap();
    *facts.lose_publication_action.lock().unwrap() = Some(ExportPublicationAction::VerifyInstalled);
    let error = lease.verify_installed(&AtomicBool::new(false)).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<crate::filesystem_worker::wire::Failure>()
            .unwrap()
            .kind,
        crate::filesystem_worker::wire::FailureKind::Unknown
    );
    assert!(lease.complete::<()>(Err(error)).is_err());
    assert!(session.authority.publication.lock().unwrap().is_none());

    let lease = session
        .authority
        .begin_export_publication(
            ExportPublicationMode::Publish,
            source,
            &AtomicBool::new(false),
        )?
        .unwrap();
    facts.fail_publication_abort.store(true, Ordering::Release);
    let operation_error = lease
        .complete::<()>(Err(anyhow::anyhow!("retained cleanup operation failed")))
        .unwrap_err();
    assert_eq!(
        operation_error.to_string(),
        "retained cleanup operation failed"
    );
    assert!(session.close().is_err());
    facts.fail_publication_abort.store(false, Ordering::Release);
    session.close()?;
    assert!(matches!(
        facts
            .export_publication_requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .action,
        ExportPublicationAction::Abort
    ));
    Ok(())
}

#[test]
fn quarantined_return_cannot_close_until_panicking_thread_is_joined() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    let db = session
        .authority
        .pool()
        .unwrap()
        .lease(role_index(SqlRole::Relink))?;
    let (returned, observed) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::sync_channel(1);
    struct UnwindOwner {
        db: Option<SqlConnection>,
        returned: std::sync::mpsc::SyncSender<()>,
        wait: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for UnwindOwner {
        fn drop(&mut self) {
            drop(self.db.take());
            let _ = self.returned.send(());
            let _ = self.wait.recv_timeout(std::time::Duration::from_secs(10));
        }
    }
    let worker = std::thread::spawn(move || {
        let _owner = UnwindOwner {
            db: Some(db),
            returned,
            wait,
        };
        panic!("synthetic role unwind");
    });
    let observed = observed.recv_timeout(std::time::Duration::from_secs(5));
    let close = session.close();
    let released = facts.releases.load(Ordering::Acquire);
    release.send(())?;
    let joined = worker.join();
    observed?;
    assert!(joined.is_err());
    assert!(
        close.is_err(),
        "unjoined quarantine must retain whole SQL roster"
    );
    assert_eq!(released, 0);
    assert!(session.authority.joined(SqlRole::Relink, false).is_err());
    session.close()?;
    assert_eq!(facts.releases.load(Ordering::Acquire), 1);
    Ok(())
}

#[test]
fn exhausted_searches_are_joined_before_reuse_and_late_cancel_is_fenced() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session = ManagedSession::admit(facts, &request, &AtomicBool::new(false)).unwrap();
    let mut old = Vec::new();
    for _ in 0..4 {
        old.push(
            session
                .catalog
                .as_ref()
                .unwrap()
                .search_session(crate::organization_search::Query::default(), 30)?,
        );
    }
    for snapshot in &mut old {
        assert!(snapshot.next_page(1, 1)?.exhausted);
    }
    let start = std::time::Instant::now();
    while session
        .authority
        .searches
        .lock()
        .unwrap()
        .iter()
        .any(|s| !s.is_finished())
    {
        ensure!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "exhausted search thread did not finish"
        );
        std::thread::yield_now();
    }
    let mut next = session
        .catalog
        .as_ref()
        .unwrap()
        .search_session(crate::organization_search::Query::default(), 30)?;
    drop(old); // These owners carry old interrupt handles for the reused role.
    assert!(next.next_page(1, 1)?.exhausted);
    next.close()?;
    session.close()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn resolved_alias_request_binds_canonical_catalog_and_manifest_without_changing_mode() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let (facts, mut request) = Facts::create(temp.path())?;
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(temp.path().canonicalize()?, &alias)?;
    request.root = NativePath::from_path(&alias.join("catalog"));
    request.manifest_root = NativePath::from_path(&alias.join("cache"));
    let resolved = request.resolved()?;
    assert_eq!(resolved.mode, BootstrapMode::DesktopCreate);
    assert_eq!(resolved.root, facts.bootstrap.canonical_root);
    assert_eq!(
        resolved.manifest_root.to_path()?.join("previews.sqlite3"),
        facts.bootstrap.manifest.path.to_path()?
    );
    let mut session = ManagedSession::admit(facts, &request, &AtomicBool::new(false)).unwrap();
    session.close()?;
    Ok(())
}

pub(crate) fn retained_admission(
    base: &Path,
    initialized: bool,
) -> Result<std::result::Result<ManagedSession, AdmissionCleanup>> {
    let (filesystem, request) = Facts::create(base)?;
    if initialized {
        Ok(Ok(ManagedSession::admit(
            filesystem,
            &request,
            &AtomicBool::new(false),
        )
        .unwrap()))
    } else {
        Ok(Err(AdmissionCleanup::Prepare {
            filesystem,
            request,
            complete: false,
        }))
    }
}

#[test]
fn failed_store_recovery_retains_opaque_lease_until_sql_and_root_release() -> Result<()> {
    use crate::preview::{AdmittedStoreFiles, Layout, ManifestOrigin, PreviewStore, StoreConfig};
    struct Lease {
        drops: Arc<AtomicUsize>,
        facts: Arc<Facts>,
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            assert!(self.facts.releases.load(Ordering::Acquire) > 0);
            self.drops.fetch_add(1, Ordering::AcqRel);
        }
    }
    struct Files {
        drops: Arc<AtomicUsize>,
        facts: Arc<Facts>,
    }
    impl AdmittedStoreFiles for Files {
        fn lock_tiers(&self, _: &StoreConfig, _: &str) -> Result<Arc<dyn Send + Sync>> {
            Ok(Arc::new(Lease {
                drops: self.drops.clone(),
                facts: self.facts.clone(),
            }))
        }
    }
    let temp = tempfile::tempdir()?;
    let (mut facts, request) = Facts::create(temp.path())?;
    let config = StoreConfig {
        manifest_root: request.manifest_root.to_path()?,
        layout: Layout::HashPrefix,
        thumbnail_root: temp.path().join("thumb"),
        large_root: temp.path().join("large"),
        thumbnail_bytes: 1024,
        large_bytes: 1024,
    };
    // Prepare a valid existing manifest before any managed SQL admission. A
    // corrupt retained object fails recovery only after the opaque lease exists.
    drop(PreviewStore::open(config.clone(), &[])?);
    let db = Connection::open(config.manifest_root.join("previews.sqlite3"))?;
    db.execute("INSERT INTO objects VALUES('synthetic','not JSON','thumbnail',1,'checksum','pending','synthetic.pending',0)",[])?;
    drop(db);
    Arc::get_mut(&mut facts).unwrap().bootstrap.manifest.created = false;
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let error = PreviewStore::open_admitted(
        config,
        session.manifest()?,
        ManifestOrigin::Existing,
        Arc::new(Files {
            drops: drops.clone(),
            facts: facts.clone(),
        }),
    )
    .err()
    .context("corrupt recovery must fail")?;
    assert!(error.to_string().contains("expected ident"), "{error:#}");
    assert_eq!(
        drops.load(Ordering::Acquire),
        0,
        "store failure must retain the lease on its SQL owner"
    );
    facts.fail_release.store(true, Ordering::Release);
    assert!(session.close().is_err());
    assert_eq!(
        drops.load(Ordering::Acquire),
        0,
        "failed root release must retain opaque lease"
    );
    facts.fail_release.store(false, Ordering::Release);
    session.close()?;
    assert_eq!(drops.load(Ordering::Acquire), 1);
    Ok(())
}

#[test]
fn injected_progress_owner_invariant_failure_retains_whole_session() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (facts, request) = Facts::create(temp.path())?;
    let mut session =
        ManagedSession::admit(facts.clone(), &request, &AtomicBool::new(false)).unwrap();
    let db = session.authority.pool().unwrap().lease(3)?;
    db.execute_batch("BEGIN")?;
    db.install_cancel_progress(Arc::new(AtomicBool::new(true)))?;
    // Managed constructors never produce unowned Connections. Inject only the
    // cleanup-result boundary to prove invariant failure cannot release owners.
    assert!(db.inject_progress_removal_failure().is_err());
    assert!(db.hook_owner_unverifiable());
    assert!(!db.is_autocommit(), "failure must not run rollback");
    drop(db);
    assert!(session.close().is_err());
    assert_eq!(facts.releases.load(Ordering::Acquire), 0);
    assert!(session.authority.joined(SqlRole::Search0, true).is_err());
    assert!(
        session.close().is_err(),
        "join must not turn unknown ownership into SQL cleanup"
    );
    assert_eq!(facts.releases.load(Ordering::Acquire), 0);
    assert!(session.authority.pool().unwrap().lease(3).is_err());
    drop(session); // fail-closed retention; no cleanup retry in Drop
    assert_eq!(facts.releases.load(Ordering::Acquire), 0);
    Ok(())
}
