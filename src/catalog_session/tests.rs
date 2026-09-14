use super::*;
use anyhow::Context;
use std::{
    fs::{self, OpenOptions},
    path::PathBuf,
    sync::atomic::AtomicUsize,
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

pub(crate) fn export_managed_session(
    base: &Path,
) -> Result<(
    ManagedSession,
    Arc<Mutex<Vec<PrepareExportDirectory>>>,
    NativePath,
)> {
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
