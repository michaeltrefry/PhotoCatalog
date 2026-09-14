//! Filesystem-only catalog admission. No SQLite connection is opened here.
use crate::{
    catalog_backup::{self, RestoreStatus},
    catalog_session::{
        BootstrapMode, CatalogBootstrap, ConfirmSqlAdmission, ExportAliasFactKind,
        ExportAliasFactReply, ExportAliasFactRequest, ExportAliasFactValue,
        ExportDestinationSnapshotReply, ExportDestinationSnapshotRequest, ExportObjectKey,
        ExportProfileAction, ExportProfileReply, ExportProfileRequest, ExportProfileValue, LeaseId,
        PinnedDatabase, PrepareCatalog, PrepareExportDirectory, PreparedExportDirectory,
        RootCapability, validate_path,
    },
    catalog_storage::{open_regular, physical_object_id},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[cfg(test)]
pub(super) const EXPORT_DESTINATION_SNAPSHOT_BARRIER: &str =
    "PHOTOCATALOG_F_EXPORT_DESTINATION_SNAPSHOT_BARRIER";

#[cfg(test)]
fn export_destination_snapshot_test_barrier(
    bytes: u64,
    cancel: &AtomicBool,
) -> std::io::Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let Some(marker) = std::env::var_os(EXPORT_DESTINATION_SNAPSHOT_BARRIER) else {
        return Ok(());
    };
    let barrier = PathBuf::from(marker);
    if !barrier.join("armed").exists() {
        return Ok(());
    }
    let entered = barrier.join("entered");
    if !entered.exists() {
        fs::write(&entered, b"snapshot-read-started")?;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cancel.load(Ordering::Acquire) {
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "test snapshot cancellation was not released",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PreparationState {
    Preparing,
    Prepared,
    Confirmed,
    Abandoned,
    Failed,
}

#[derive(Clone, Debug)]
pub(super) struct PreparationProgress {
    pub request: PrepareCatalog,
    pub directory_created: bool,
    pub catalog_created: bool,
    pub manifest_created: bool,
    pub bootstrap: Option<CatalogBootstrap>,
    pub state: PreparationState,
    pub error: Option<String>,
}

struct RootRecord {
    bootstrap: CatalogBootstrap,
    root: File,
    catalog: Option<File>,
    manifest: Option<File>,
    manifest_lock: ManifestLock,
    manifest_directory: File,
    store: super::store::StoreOwner,
    objects: super::preview_io::ObjectOwner,
    stages: super::preview_stage::Owner,
    export_profile: Option<ExportProfileTransfer>,
    export_profile_terminal: Option<ExportProfileTerminal>,
}
struct ExportProfileTransfer {
    requested: NativePath,
    transfer: LeaseId,
    next_step: u64,
    allowance: u64,
    offset: u64,
    source: crate::lightroom::source::Source,
}
struct ExportProfileTerminal {
    requested: NativePath,
    transfer: LeaseId,
    step: u64,
    allowance: u64,
    value: ExportProfileTerminalValue,
}
enum ExportProfileTerminalValue {
    Finished { bytes: u64 },
    Aborted,
}
pub(crate) fn export_profile_transfer_layout() -> (usize, usize) {
    (
        std::mem::size_of::<Option<ExportProfileTransfer>>()
            + std::mem::size_of::<Option<ExportProfileTerminal>>(),
        std::mem::align_of::<Option<ExportProfileTransfer>>()
            .max(std::mem::align_of::<Option<ExportProfileTerminal>>()),
    )
}
struct ManifestLock(File);
impl ManifestLock {
    fn acquire(root: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("preview.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)
            .context("preview service already owns this cache")?;
        Ok(Self(file))
    }
    fn release(&self) -> Result<()> {
        fs2::FileExt::unlock(&self.0).context("release preview manifest ownership")
    }
}
impl Drop for ManifestLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

pub(super) struct BootstrapOwner {
    epoch: LeaseId,
    original_roots: Vec<NativePath>,
    record: Option<RootRecord>,
    progress: Option<PreparationProgress>,
}
impl BootstrapOwner {
    pub fn new(epoch: LeaseId, original_roots: Vec<NativePath>) -> Self {
        Self {
            epoch,
            original_roots,
            record: None,
            progress: None,
        }
    }

    pub fn progress(&self) -> Option<&PreparationProgress> {
        self.progress.as_ref()
    }

    pub fn prepare(
        &mut self,
        request: &PrepareCatalog,
        cancel: &AtomicBool,
        mut publish: impl FnMut(&PreparationProgress) -> Result<()>,
    ) -> Result<CatalogBootstrap> {
        ensure!(
            self.record.is_none(),
            "a catalog filesystem root is already retained"
        );
        ensure!(
            self.progress
                .as_ref()
                .is_none_or(|p| p.state == PreparationState::Abandoned),
            "reconcile or abandon the previous preparation before another admission"
        );
        request.validate()?;
        self.progress = Some(PreparationProgress {
            request: request.clone(),
            directory_created: false,
            catalog_created: false,
            manifest_created: false,
            bootstrap: None,
            state: PreparationState::Preparing,
            error: None,
        });
        let result = self.prepare_inner(request, cancel, &mut publish);
        if let Err(error) = &result {
            let progress = self.progress.as_mut().expect("preparation registered");
            progress.state = PreparationState::Failed;
            let mut message = format!("{error:#}");
            let mut length = message.len().min(4096);
            while !message.is_char_boundary(length) {
                length -= 1;
            }
            message.truncate(length);
            progress.error = Some(message);
            // Preserve the original failure if the transport itself also failed.
            let _ = publish(progress);
        }
        result
    }

    fn prepare_inner(
        &mut self,
        request: &PrepareCatalog,
        cancel: &AtomicBool,
        publish: &mut impl FnMut(&PreparationProgress) -> Result<()>,
    ) -> Result<CatalogBootstrap> {
        check_cancel(cancel)?;
        let path = request.root.to_path()?;
        let manifest_root = request.manifest_root.to_path()?;
        match request.mode {
            BootstrapMode::DesktopCreate => ensure!(
                !path.try_exists()?,
                "new catalog destination already exists"
            ),
            BootstrapMode::DesktopExisting => ensure!(
                fs::symlink_metadata(path.join("catalog.sqlite3"))?
                    .file_type()
                    .is_file(),
                "existing catalog database must be a regular file"
            ),
            BootstrapMode::OpenOrCreate => {}
        }
        let resolved = crate::prospective_directory(&path)?;
        let manifest_resolved = crate::prospective_directory(&manifest_root)?;
        validate_path(&NativePath::from_path(&resolved))?;
        validate_path(&NativePath::from_path(&manifest_resolved))?;
        #[cfg(windows)]
        ensure!(
            manifest_resolved.to_str().is_some(),
            "preview SQLite manifest path must be valid Unicode; choose another manifest directory"
        );
        for original in &self.original_roots {
            let original = crate::prospective_directory(&original.to_path()?)?;
            separate(&resolved, &original)?;
            separate(&manifest_resolved, &original)?;
        }
        if let Some(source) = &request.import_source {
            let source = fs::canonicalize(source.to_path()?).context("resolve import source")?;
            ensure!(source.is_dir(), "import source must be a folder");
            separate(&resolved, &source)?;
            separate(&manifest_resolved, &source)?;
        }
        catalog_backup::check_catalog_root(&path)?;
        check_cancel(cancel)?;
        match request.mode {
            BootstrapMode::DesktopCreate => {
                fs::create_dir(&path).context("create new catalog directory")?;
                self.progress.as_mut().unwrap().directory_created = true;
            }
            BootstrapMode::OpenOrCreate => {
                let existed = path.try_exists()?;
                fs::create_dir_all(&path)?;
                self.progress.as_mut().unwrap().directory_created = !existed;
            }
            BootstrapMode::DesktopExisting => {}
        }
        publish(self.progress.as_ref().unwrap())?;
        check_cancel(cancel)?;
        let canonical_root = fs::canonicalize(&path)?;
        validate_path(&NativePath::from_path(&canonical_root))?;
        ensure!(
            canonical_root == resolved,
            "catalog root changed during preparation"
        );
        let root = open_directory(&canonical_root)?;
        let root_physical = physical_object_id(&root)?;
        fs::create_dir_all(canonical_root.join("previews"))?;
        check_cancel(cancel)?;
        let (catalog, catalog_created) = open_database(
            &canonical_root.join("catalog.sqlite3"),
            request.mode != BootstrapMode::DesktopExisting,
            request.mode == BootstrapMode::DesktopCreate,
        )?;
        self.progress.as_mut().unwrap().catalog_created = catalog_created;
        publish(self.progress.as_ref().unwrap())?;
        check_cancel(cancel)?;
        fs::create_dir_all(&manifest_root)?;
        let manifest_root = fs::canonicalize(manifest_root)?;
        ensure!(
            manifest_root == manifest_resolved,
            "preview manifest root changed during preparation"
        );
        let manifest_lock = ManifestLock::acquire(&manifest_root)?;
        check_cancel(cancel)?;
        let (manifest, manifest_created) =
            open_database(&manifest_root.join("previews.sqlite3"), true, false)?;
        self.progress.as_mut().unwrap().manifest_created = manifest_created;
        publish(self.progress.as_ref().unwrap())?;
        check_cancel(cancel)?;
        let bootstrap = CatalogBootstrap {
            version: 1,
            operation: request.operation,
            epoch: self.epoch.clone(),
            token: LeaseId::new(),
            session: request.session.clone(),
            canonical_root: NativePath::from_path(&canonical_root),
            root_physical,
            catalog: PinnedDatabase {
                path: NativePath::from_path(&canonical_root.join("catalog.sqlite3")),
                physical: physical_object_id(&catalog)?,
                created: catalog_created,
            },
            manifest: PinnedDatabase {
                path: NativePath::from_path(&manifest_root.join("previews.sqlite3")),
                physical: physical_object_id(&manifest)?,
                created: manifest_created,
            },
        };
        bootstrap.validate()?;
        let record = RootRecord {
            bootstrap: bootstrap.clone(),
            root,
            catalog: Some(catalog),
            manifest: Some(manifest),
            manifest_lock,
            manifest_directory: open_directory(&manifest_root)?,
            store: super::store::StoreOwner::default(),
            objects: super::preview_io::ObjectOwner::default(),
            stages: super::preview_stage::Owner::default(),
            export_profile: None,
            export_profile_terminal: None,
        };
        record.verify_root_binding()?;
        self.record = Some(record);
        let progress = self.progress.as_mut().unwrap();
        progress.bootstrap = Some(bootstrap.clone());
        progress.state = PreparationState::Prepared;
        publish(progress)?;
        Ok(bootstrap)
    }

    pub fn confirm(
        &mut self,
        request: &ConfirmSqlAdmission,
        cancel: &AtomicBool,
    ) -> Result<ConfirmSqlAdmission> {
        check_cancel(cancel)?;
        let record = self
            .record
            .as_mut()
            .context("catalog admission is not retained")?;
        request.validate_for(&record.bootstrap)?;
        record.verify_root_binding()?;
        for (pin, expected) in [
            (&record.catalog, record.bootstrap.catalog.physical),
            (&record.manifest, record.bootstrap.manifest.physical),
        ] {
            ensure!(
                physical_object_id(pin.as_ref().context("admission was already confirmed")?)?
                    == expected,
                "retained database admission identity changed"
            );
        }
        ensure!(
            physical_object_id(&open_regular(&record.bootstrap.manifest.path.to_path()?)?)?
                == record.bootstrap.manifest.physical,
            "preview manifest moved or was replaced before admission"
        );
        check_cancel(cancel)?;
        // The trusted SQL owner has observed all eight actual handles. The
        // response acknowledges this exact overlap; it is never synthesized
        // from the later cached historical bootstrap.
        record.catalog.take();
        record.manifest.take();
        self.progress.as_mut().unwrap().state = PreparationState::Confirmed;
        Ok(request.clone())
    }

    pub fn abandon(&mut self, operation: crate::application::U64, session: &LeaseId) -> Result<()> {
        let progress = self.progress.as_ref().context("unknown preparation")?;
        ensure!(
            progress.request.operation == operation && &progress.request.session == session,
            "preparation belongs to another operation or session"
        );
        ensure!(
            progress.state != PreparationState::Confirmed,
            "confirmed SQL ownership requires verified drain before root release"
        );
        if let Some(record) = &self.record {
            record.manifest_lock.release()?;
        }
        self.record.take();
        self.progress.as_mut().unwrap().state = PreparationState::Abandoned;
        Ok(())
    }

    pub fn release(&mut self, root: &RootCapability) -> Result<()> {
        if let Some(record) = &mut self.record {
            ensure!(
                &record.bootstrap.root_capability() == root,
                "root belongs to another session"
            );
            ensure!(
                record.stages.empty(),
                "worker stages/native/output owners have not drained"
            );
            ensure!(
                record.export_profile.is_none(),
                "export profile transfer has not drained"
            );
            record.objects.drain();
            record.store.release()?;
            record.manifest_lock.release()?;
            self.record.take();
            self.progress.as_mut().unwrap().state = PreparationState::Abandoned;
            return Ok(());
        }
        ensure!(
            self.progress
                .as_ref()
                .is_some_and(|p| p.state == PreparationState::Abandoned
                    && p.bootstrap
                        .as_ref()
                        .is_some_and(|b| &b.root_capability() == root)),
            "root ownership is not retained by this session"
        );
        Ok(())
    }

    pub fn restore_status(&self, root: &RootCapability) -> Result<Option<RestoreStatus>> {
        self.with_root(root, |path| catalog_backup::restore_status(path))
    }

    pub fn stage_call(
        &mut self,
        request: &crate::catalog_session::preview_stage::Request,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::preview_stage::Reply> {
        ensure!(
            self.progress
                .as_ref()
                .is_some_and(|p| p.state == PreparationState::Confirmed),
            "stage custody requires confirmed SQL admission"
        );
        let record = self
            .record
            .as_mut()
            .context("stage catalog root is not retained")?;
        ensure!(
            request.root == record.bootstrap.root_capability(),
            "stage session mismatch"
        );
        record.verify_root_binding()?;
        let database = record.bootstrap.manifest.path.to_path()?;
        let manifest = database.parent().context("manifest parent")?;
        ensure!(
            physical_object_id(&record.manifest_directory)?
                == physical_object_id(&open_directory(manifest)?)?,
            "stage manifest directory changed"
        );
        let result = record.stages.call(manifest, request, cancel);
        record.verify_root_binding()?;
        result
    }
    pub fn preview_io_call(
        &mut self,
        request: &crate::catalog_session::preview_io::Request,
        cancel: &AtomicBool,
        publish: impl FnMut(super::preview_io::Snapshot) -> Result<()>,
    ) -> Result<crate::catalog_session::preview_io::Reply> {
        ensure!(
            self.progress
                .as_ref()
                .is_some_and(|p| p.state == PreparationState::Confirmed),
            "cache IO requires confirmed SQL admission"
        );
        let record = self.record.as_mut().context("cache root is not retained")?;
        ensure!(
            request.root == record.bootstrap.root_capability(),
            "cache IO session mismatch"
        );
        record.verify_root_binding()?;
        let result = record
            .objects
            .execute(&record.store, request, cancel, publish);
        record.verify_root_binding()?;
        result
    }

    pub fn store_call(
        &mut self,
        request: &crate::catalog_session::store::Request,
        cancel: &AtomicBool,
        publish: impl FnMut(super::store::Snapshot) -> Result<()>,
    ) -> Result<crate::catalog_session::store::Reply> {
        ensure!(
            self.progress
                .as_ref()
                .is_some_and(|p| p.state == PreparationState::Confirmed),
            "preview tier acquisition requires confirmed SQL admission"
        );
        let record = self
            .record
            .as_mut()
            .context("catalog filesystem root is not retained")?;
        ensure!(
            request.root == record.bootstrap.root_capability(),
            "preview store belongs to another session"
        );
        record.verify_root_binding()?;
        let manifest = record.bootstrap.manifest.path.to_path()?;
        ensure!(
            physical_object_id(&record.manifest_directory)?
                == physical_object_id(&open_directory(
                    manifest.parent().context("manifest parent")?
                )?)?,
            "preview manifest directory was moved or replaced"
        );
        let result = record.store.execute(
            &record.bootstrap,
            &self.original_roots,
            request,
            cancel,
            publish,
        );
        record.verify_root_binding()?;
        result
    }

    pub fn resume(
        &self,
        root: &RootCapability,
        restore_id: &str,
        acknowledge: bool,
        cancel: &AtomicBool,
    ) -> Result<RestoreStatus> {
        self.with_root(root, |path| {
            catalog_backup::resume_restored_jobs_controlled(
                path,
                restore_id,
                acknowledge,
                &mut || {
                    check_cancel(cancel)?;
                    self.with_root(root, |_| Ok(()))
                },
            )
        })
    }
    pub fn require_jobs_released(&self, root: &RootCapability) -> Result<()> {
        self.with_root(root, catalog_backup::require_jobs_released)
    }
    pub fn prepare_export_directory(
        &self,
        request: &PrepareExportDirectory,
        cancel: &AtomicBool,
    ) -> Result<PreparedExportDirectory> {
        request.validate()?;
        export_cancel(cancel)?;
        self.with_root(&request.root, |_| {
            export_cancel(cancel)?;
            let directory = fs::canonicalize(request.directory.to_path()?)?;
            ensure!(directory.is_dir(), "existing output directory required");
            let directory = NativePath::from_path(&directory);
            validate_path(&directory)?;
            export_cancel(cancel)?;
            Ok(PreparedExportDirectory {
                root: request.root.clone(),
                requested: request.directory.clone(),
                directory,
            })
        })
    }
    pub fn export_destination_snapshot(
        &self,
        request: &ExportDestinationSnapshotRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportDestinationSnapshotReply> {
        request.validate()?;
        export_fact_cancel(cancel)?;
        self.with_root(&request.root, |_| {
            let path = request.destination.to_path()?;
            let mut canceled_while_reading = false;
            let snapshot = crate::metadata_export::snapshot_photo_destination_with_checkpoint(
                &path,
                request.max_existing_bytes.0,
                &mut |_bytes| {
                    #[cfg(test)]
                    export_destination_snapshot_test_barrier(_bytes, cancel)?;
                    if cancel.load(Ordering::Acquire) {
                        canceled_while_reading = true;
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "export destination inspection canceled",
                        ))
                    } else {
                        Ok(())
                    }
                },
            );
            if canceled_while_reading {
                return Err(anyhow::Error::new(super::wire::Failure::new(
                    super::wire::FailureKind::Canceled,
                    "export destination inspection canceled",
                )));
            }
            export_fact_cancel(cancel)?;
            let snapshot = snapshot?;
            Ok(ExportDestinationSnapshotReply {
                root: request.root.clone(),
                requested: request.destination.clone(),
                snapshot,
            })
        })
    }
    pub fn export_alias_fact(
        &self,
        request: &ExportAliasFactRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportAliasFactReply> {
        request.validate()?;
        export_fact_cancel(cancel)?;
        self.with_root(&request.root, |_| {
            let path = request.path.to_path()?;
            let value = match request.kind {
                ExportAliasFactKind::Destination => match fs::symlink_metadata(&path) {
                    Ok(metadata) => {
                        ensure!(
                            metadata.is_file() && !metadata.file_type().is_symlink(),
                            "export destination is not an ordinary file"
                        );
                        ExportAliasFactValue::File {
                            object: ExportObjectKey::from_native(
                                crate::storage_volume::object_key(&path, &metadata)?,
                            ),
                            canonical: None,
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        ExportAliasFactValue::Missing
                    }
                    Err(error) => return Err(error.into()),
                },
                ExportAliasFactKind::File | ExportAliasFactKind::CanonicalFile => {
                    match fs::metadata(&path) {
                        Ok(metadata) => {
                            ensure!(
                                metadata.is_file(),
                                "catalog original changed file type; reconcile before overwrite"
                            );
                            ExportAliasFactValue::File {
                                object: ExportObjectKey::from_native(
                                    crate::storage_volume::object_key(&path, &metadata)?,
                                ),
                                canonical: matches!(
                                    request.kind,
                                    ExportAliasFactKind::CanonicalFile
                                )
                                .then(|| fs::canonicalize(&path))
                                .transpose()?
                                .map(|path| NativePath::from_path(&path)),
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            ExportAliasFactValue::Missing
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                ExportAliasFactKind::Directory => match fs::metadata(&path) {
                    Ok(metadata) => {
                        ensure!(
                            metadata.is_dir(),
                            "original directory changed type; reconcile before export"
                        );
                        ExportAliasFactValue::Directory {
                            object: ExportObjectKey::from_native(
                                crate::storage_volume::object_key(&path, &metadata)?,
                            ),
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        ExportAliasFactValue::Missing
                    }
                    Err(error) => return Err(error.into()),
                },
            };
            export_fact_cancel(cancel)?;
            let reply = ExportAliasFactReply {
                root: request.root.clone(),
                path: request.path.clone(),
                kind: request.kind,
                value,
            };
            reply.validate_for(request)?;
            Ok(reply)
        })
    }
    pub fn export_profile_call(
        &mut self,
        request: &ExportProfileRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportProfileReply> {
        request.validate()?;
        ensure!(
            self.progress
                .as_ref()
                .is_some_and(|p| p.state == PreparationState::Confirmed),
            "export profile read requires confirmed SQL admission"
        );
        let record = self
            .record
            .as_mut()
            .context("export profile catalog root is not retained")?;
        ensure!(
            request.root == record.bootstrap.root_capability(),
            "export profile session mismatch"
        );
        record.verify_root_binding()?;
        let result = (|| -> Result<ExportProfileValue> {
            let value = match &request.action {
                ExportProfileAction::Begin => {
                    profile_cancel(cancel)?;
                    ensure!(
                        record.export_profile.is_none(),
                        "export profile transfer already active"
                    );
                    let path = request.requested.to_path()?;
                    let parent = path
                        .parent()
                        .context("profile parent required")?
                        .canonicalize()?;
                    let normalized =
                        parent.join(path.file_name().context("profile filename required")?);
                    let source = crate::lightroom::source::Source::open(&normalized, u64::MAX)?;
                    if source.before.bytes > request.allowance.0 {
                        return Err(anyhow::Error::new(super::wire::Failure::new(
                            super::wire::FailureKind::ResourceLimit,
                            "ICC source exceeds the available profile byte allowance",
                        )));
                    }
                    let bytes = source.before.bytes;
                    record.export_profile_terminal = None;
                    record.export_profile = Some(ExportProfileTransfer {
                        requested: request.requested.clone(),
                        transfer: request.transfer.clone(),
                        next_step: 1,
                        allowance: request.allowance.0,
                        offset: 0,
                        source,
                    });
                    ExportProfileValue::Begun {
                        bytes: crate::application::U64(bytes),
                    }
                }
                action => {
                    if record.export_profile.is_none() {
                        let terminal = record
                            .export_profile_terminal
                            .as_mut()
                            .context("export profile transfer is not retained")?;
                        ensure!(
                            terminal.requested == request.requested
                                && terminal.transfer == request.transfer
                                && terminal.allowance == request.allowance.0,
                            "export profile terminal provenance mismatch"
                        );
                        return match (&mut terminal.value, action) {
                            (
                                ExportProfileTerminalValue::Finished { bytes },
                                ExportProfileAction::Finish,
                            ) => {
                                ensure!(
                                    terminal.step == request.step.0,
                                    "export profile terminal step mismatch"
                                );
                                Ok(ExportProfileValue::Finished {
                                    bytes: crate::application::U64(*bytes),
                                })
                            }
                            (
                                ExportProfileTerminalValue::Finished { .. },
                                ExportProfileAction::Abort,
                            ) => {
                                ensure!(
                                    terminal.step.checked_add(1) == Some(request.step.0),
                                    "export profile terminal abort step mismatch"
                                );
                                terminal.step = request.step.0;
                                terminal.value = ExportProfileTerminalValue::Aborted;
                                Ok(ExportProfileValue::Aborted)
                            }
                            (ExportProfileTerminalValue::Aborted, ExportProfileAction::Abort) => {
                                ensure!(
                                    terminal.step == request.step.0,
                                    "export profile terminal step mismatch"
                                );
                                Ok(ExportProfileValue::Aborted)
                            }
                            _ => anyhow::bail!("export profile terminal action mismatch"),
                        };
                    }
                    let transfer = record
                        .export_profile
                        .as_mut()
                        .context("export profile transfer is not retained")?;
                    ensure!(
                        transfer.requested == request.requested
                            && transfer.transfer == request.transfer
                            && transfer.allowance == request.allowance.0,
                        "export profile transfer provenance mismatch"
                    );
                    if !matches!(action, ExportProfileAction::Abort) {
                        ensure!(
                            transfer.next_step == request.step.0,
                            "export profile step mismatch"
                        );
                        transfer.next_step = transfer
                            .next_step
                            .checked_add(1)
                            .context("export profile step exhausted")?;
                    }
                    match action {
                        ExportProfileAction::Read { offset } => {
                            profile_cancel(cancel)?;
                            ensure!(
                                offset.0 == transfer.offset
                                    && transfer.offset < transfer.source.before.bytes,
                                "export profile read offset mismatch"
                            );
                            let remaining = transfer.source.before.bytes - transfer.offset;
                            let length = usize::try_from(
                                remaining
                                    .min(crate::catalog_session::preview_io::CHUNK_BYTES as u64),
                            )?;
                            let mut bytes = vec![0; length];
                            transfer.source.file.read_exact(&mut bytes)?;
                            profile_cancel(cancel)?;
                            let value = ExportProfileValue::Chunk {
                                offset: *offset,
                                checksum: blake3::hash(&bytes).to_hex().to_string(),
                                bytes,
                            };
                            transfer.offset += length as u64;
                            value
                        }
                        ExportProfileAction::Finish => {
                            profile_cancel(cancel)?;
                            ensure!(
                                transfer.offset == transfer.source.before.bytes,
                                "export profile finished before all bytes were read"
                            );
                            let mut extra = [0];
                            ensure!(
                                transfer.source.file.read(&mut extra)? == 0,
                                "ICC changed size"
                            );
                            transfer.source.verify()?;
                            profile_cancel(cancel)?;
                            let bytes = transfer.source.before.bytes;
                            let transfer = record.export_profile.take().expect("retained transfer");
                            record.export_profile_terminal = Some(ExportProfileTerminal {
                                requested: transfer.requested,
                                transfer: transfer.transfer,
                                step: request.step.0,
                                allowance: transfer.allowance,
                                value: ExportProfileTerminalValue::Finished { bytes },
                            });
                            ExportProfileValue::Finished {
                                bytes: crate::application::U64(bytes),
                            }
                        }
                        ExportProfileAction::Abort => {
                            // Cancellation can be observed before the preceding
                            // request reaches F or after F advances its step.
                            // The serial caller's cleanup is therefore either
                            // the retained next step or exactly one beyond it.
                            ensure!(
                                request.step.0 == transfer.next_step
                                    || transfer.next_step.checked_add(1) == Some(request.step.0),
                                "export profile abort step mismatch"
                            );
                            let transfer = record.export_profile.take().expect("retained transfer");
                            record.export_profile_terminal = Some(ExportProfileTerminal {
                                requested: transfer.requested,
                                transfer: transfer.transfer,
                                step: request.step.0,
                                allowance: transfer.allowance,
                                value: ExportProfileTerminalValue::Aborted,
                            });
                            ExportProfileValue::Aborted
                        }
                        ExportProfileAction::Begin => unreachable!(),
                    }
                }
            };
            Ok(value)
        })();
        record.verify_root_binding()?;
        let value = result?;
        Ok(ExportProfileReply {
            root: request.root.clone(),
            requested: request.requested.clone(),
            transfer: request.transfer.clone(),
            step: request.step,
            value,
        })
    }
    fn with_root<T>(
        &self,
        root: &RootCapability,
        operation: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let record = self
            .record
            .as_ref()
            .context("catalog filesystem root is not retained")?;
        ensure!(
            &record.bootstrap.root_capability() == root,
            "root belongs to another session"
        );
        let path = record.verify_root_binding()?;
        let result = operation(&path);
        record.verify_root_binding()?;
        result
    }
    pub fn shutdown(&self) -> Result<()> {
        ensure!(
            self.record.is_none(),
            "catalog SQL/native ownership has not released its filesystem root"
        );
        Ok(())
    }
}

impl RootRecord {
    fn verify_root_binding(&self) -> Result<PathBuf> {
        let path = self.bootstrap.canonical_root.to_path()?;
        ensure!(
            physical_object_id(&self.root)? == self.bootstrap.root_physical
                && physical_object_id(&open_directory(&path)?)? == self.bootstrap.root_physical,
            "admitted catalog directory moved or was replaced"
        );
        ensure!(
            physical_object_id(&open_regular(&path.join("catalog.sqlite3"))?)?
                == self.bootstrap.catalog.physical,
            "admitted catalog database moved or was replaced"
        );
        Ok(path)
    }
}
fn separate(root: &Path, source: &Path) -> Result<()> {
    ensure!(
        !root.starts_with(source) && !source.starts_with(root),
        "catalog and original roots must be separate"
    );
    Ok(())
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "filesystem operation canceled"
    );
    Ok(())
}
fn export_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Err(anyhow::Error::new(super::wire::Failure::new(
            super::wire::FailureKind::Canceled,
            "export directory preparation canceled",
        )));
    }
    Ok(())
}
fn export_fact_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Err(anyhow::Error::new(super::wire::Failure::new(
            super::wire::FailureKind::Canceled,
            "export destination inspection canceled",
        )));
    }
    Ok(())
}
fn profile_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Err(anyhow::Error::new(super::wire::Failure::new(
            super::wire::FailureKind::Canceled,
            "export profile read canceled",
        )));
    }
    Ok(())
}
fn open_database(path: &Path, may_create: bool, must_create: bool) -> Result<(File, bool)> {
    if must_create {
        return Ok((
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)?,
            true,
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(_) => Ok((open_regular(path)?, false)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && may_create => {
            // No fallback after an exclusive-create race. The caller must
            // reconcile the original preparation; it cannot adopt the winner.
            Ok((
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(path)?,
                true,
            ))
        }
        Err(error) => Err(error.into()),
    }
}
pub(super) fn open_directory(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x02000000 | 0x00200000);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_dir(),
        "admitted catalog root is not a directory"
    );
    physical_object_id(&file)?;
    Ok(file)
}
