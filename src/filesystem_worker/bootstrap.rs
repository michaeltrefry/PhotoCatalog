//! Filesystem-only catalog admission. No SQLite connection is opened here.
use crate::{
    catalog_backup::{self, RestoreStatus},
    catalog_session::{
        BootstrapMode, CatalogBootstrap, ConfirmSqlAdmission, ExportAliasFactKind,
        ExportAliasFactReply, ExportAliasFactRequest, ExportAliasFactValue,
        ExportDestinationSnapshotReply, ExportDestinationSnapshotRequest, ExportObjectKey,
        ExportOriginalAction, ExportOriginalReply, ExportOriginalRequest, ExportOriginalValue,
        ExportProfileAction, ExportProfileReply, ExportProfileRequest, ExportProfileValue,
        ExportPublicationAction, ExportPublicationMode, ExportPublicationReply,
        ExportPublicationRequest, ExportPublicationSource, ExportPublicationValue,
        InspectExportOriginal, InspectedExportOriginal, LeaseId, PinnedDatabase, PrepareCatalog,
        PrepareExportDirectory, PreparedExportDirectory, RootCapability, validate_path,
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
pub(super) const EXPORT_ORIGINAL_BARRIER: &str = "PHOTOCATALOG_F_EXPORT_ORIGINAL_BARRIER";
#[cfg(test)]
pub(super) const EXPORT_PUBLICATION_BARRIER: &str = "PHOTOCATALOG_F_EXPORT_PUBLICATION_BARRIER";

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

#[cfg(test)]
fn export_original_test_barrier(bytes: u64, cancel: &AtomicBool) -> std::io::Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let Some(marker) = std::env::var_os(EXPORT_ORIGINAL_BARRIER) else {
        return Ok(());
    };
    let barrier = PathBuf::from(marker);
    if !barrier.join("armed").exists() {
        return Ok(());
    }
    let entered = barrier.join("entered");
    if !entered.exists() {
        fs::write(&entered, b"original-read-started")?;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cancel.load(Ordering::Acquire) {
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "test original cancellation was not released",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Ok(())
}

#[cfg(test)]
fn export_publication_test_barrier(bytes: u64, cancel: &AtomicBool) -> std::io::Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let Some(marker) = std::env::var_os(EXPORT_PUBLICATION_BARRIER) else {
        return Ok(());
    };
    let barrier = PathBuf::from(marker);
    if !barrier.join("armed").exists() {
        return Ok(());
    }
    let entered = barrier.join("entered");
    if !entered.exists() {
        fs::write(&entered, b"publication-read-started")?;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cancel.load(Ordering::Acquire) {
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "test publication cancellation was not released",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Ok(())
}

#[cfg(test)]
fn export_publication_mutation_test_barrier(
    action: &ExportPublicationAction,
    cancel: &AtomicBool,
) -> std::io::Result<()> {
    if !matches!(
        action,
        ExportPublicationAction::Capture
            | ExportPublicationAction::Link
            | ExportPublicationAction::RestoreLink
    ) {
        return Ok(());
    }
    let Some(marker) = std::env::var_os(EXPORT_PUBLICATION_BARRIER) else {
        return Ok(());
    };
    let barrier = PathBuf::from(marker);
    let armed = barrier.join("mutation-armed");
    if !armed.exists() {
        return Ok(());
    }
    fs::remove_file(armed)?;
    fs::write(
        barrier.join("mutation-entered"),
        b"publication-mutation-admitted",
    )?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cancel.load(Ordering::Acquire) {
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "test publication mutation cancellation was not released",
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
    export_original: Option<ExportOriginalTransfer>,
    export_original_terminal: Option<ExportOriginalTerminal>,
    export_publication: Option<ExportPublicationTransfer>,
    export_publication_terminal: Option<ExportPublicationTerminal>,
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
struct ExportOriginalTransfer {
    requested: NativePath,
    transfer: LeaseId,
    next_step: u64,
    allowance: u64,
    verified: crate::metadata_export::VerifiedFile,
}
struct ExportOriginalTerminal {
    requested: NativePath,
    transfer: LeaseId,
    step: u64,
    allowance: u64,
    value: ExportOriginalTerminalValue,
}
enum ExportOriginalTerminalValue {
    Finished,
    Aborted,
}
#[derive(Clone)]
struct CachedPublication {
    step: crate::application::U64,
    request_digest: String,
    reply: ExportPublicationReply,
}
struct ExportPublicationTransfer {
    mode: ExportPublicationMode,
    source: ExportPublicationSource,
    transfer: LeaseId,
    next_step: u64,
    seal: crate::metadata_export::SealedPhotoExport,
    publication: crate::metadata_export::PhotoPublication,
    last: CachedPublication,
}
struct ExportPublicationTerminal {
    mode: ExportPublicationMode,
    source: ExportPublicationSource,
    transfer: LeaseId,
    next_step: u64,
    seal: crate::metadata_export::SealedPhotoExport,
    last: CachedPublication,
}
pub(crate) fn export_profile_transfer_layout() -> (usize, usize) {
    (
        std::mem::size_of::<Option<ExportProfileTransfer>>()
            + std::mem::size_of::<Option<ExportProfileTerminal>>(),
        std::mem::align_of::<Option<ExportProfileTransfer>>()
            .max(std::mem::align_of::<Option<ExportProfileTerminal>>()),
    )
}
pub(crate) fn export_original_transfer_layout() -> (usize, usize) {
    (
        std::mem::size_of::<Option<ExportOriginalTransfer>>()
            + std::mem::size_of::<Option<ExportOriginalTerminal>>(),
        std::mem::align_of::<Option<ExportOriginalTransfer>>()
            .max(std::mem::align_of::<Option<ExportOriginalTerminal>>()),
    )
}
pub(crate) fn export_publication_transfer_layout() -> (usize, usize) {
    (
        std::mem::size_of::<Option<ExportPublicationTransfer>>()
            + std::mem::size_of::<Option<ExportPublicationTerminal>>(),
        std::mem::align_of::<Option<ExportPublicationTransfer>>()
            .max(std::mem::align_of::<Option<ExportPublicationTerminal>>()),
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
            export_original: None,
            export_original_terminal: None,
            export_publication: None,
            export_publication_terminal: None,
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
            ensure!(
                record.export_original.is_none(),
                "export original lease has not drained"
            );
            ensure!(
                record.export_publication.is_none(),
                "export publication lease has not drained"
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
    fn export_original_path(&self, requested: &NativePath) -> Result<PathBuf> {
        let path = requested.to_path()?;
        let parent = path
            .parent()
            .context("original parent required")?
            .canonicalize()?;
        let normalized = parent.join(path.file_name().context("original filename required")?);
        let mut admitted = false;
        for root in &self.original_roots {
            if let Ok(root) = root.to_path()?.canonicalize()
                && normalized.starts_with(root)
            {
                admitted = true;
                break;
            }
        }
        ensure!(
            admitted,
            "export original is outside admitted original roots"
        );
        Ok(normalized)
    }
    pub fn inspect_export_original(
        &self,
        request: &InspectExportOriginal,
        cancel: &AtomicBool,
    ) -> Result<InspectedExportOriginal> {
        request.validate()?;
        original_cancel(cancel)?;
        self.with_root(&request.root, |_| {
            // Authenticate and revalidate the retained catalog root before
            // resolving or opening any caller-supplied original path.
            let path = self.export_original_path(&request.requested)?;
            let mut canceled_while_reading = false;
            let revision = crate::metadata_export::inspect_file_revision_with_checkpoint(
                &path,
                request.allowance.0,
                &mut |_bytes| {
                    #[cfg(test)]
                    export_original_test_barrier(_bytes, cancel)?;
                    if cancel.load(Ordering::Acquire) {
                        canceled_while_reading = true;
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "export original inspection canceled",
                        ))
                    } else {
                        Ok(())
                    }
                },
            );
            if canceled_while_reading {
                return Err(anyhow::Error::new(super::wire::Failure::new(
                    super::wire::FailureKind::Canceled,
                    "export original inspection canceled",
                )));
            }
            let revision = revision.map_err(export_original_error)?;
            original_cancel(cancel)?;
            let reply = InspectedExportOriginal {
                root: request.root.clone(),
                requested: request.requested.clone(),
                allowance: request.allowance,
                revision,
            };
            reply.validate_for(request)?;
            Ok(reply)
        })
    }
    pub fn export_original_call(
        &mut self,
        request: &ExportOriginalRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportOriginalReply> {
        request.validate()?;
        ensure!(
            self.progress
                .as_ref()
                .is_some_and(|p| p.state == PreparationState::Confirmed),
            "export original lease requires confirmed SQL admission"
        );
        {
            let record = self
                .record
                .as_ref()
                .context("export original catalog root is not retained")?;
            ensure!(
                request.root == record.bootstrap.root_capability(),
                "export original session mismatch"
            );
            record.verify_root_binding()?;
            if matches!(request.action, ExportOriginalAction::Begin) {
                if let Some(active) = &record.export_original {
                    ensure!(
                        active.requested == request.requested
                            && active.transfer == request.transfer
                            && active.allowance == request.allowance.0,
                        "export original lease already active"
                    );
                    return Ok(ExportOriginalReply {
                        root: request.root.clone(),
                        requested: request.requested.clone(),
                        transfer: request.transfer.clone(),
                        step: request.step,
                        value: ExportOriginalValue::Begun {
                            revision: active.verified.revision().clone(),
                        },
                    });
                }
            }
        }
        let candidate = if matches!(request.action, ExportOriginalAction::Begin) {
            let path = self.export_original_path(&request.requested)?;
            original_cancel(cancel)?;
            let mut canceled_while_reading = false;
            let verified = crate::metadata_export::VerifiedFile::read_with_checkpoint(
                &path,
                request.allowance.0,
                &mut |_bytes| {
                    #[cfg(test)]
                    export_original_test_barrier(_bytes, cancel)?;
                    if cancel.load(Ordering::Acquire) {
                        canceled_while_reading = true;
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "export original verification canceled",
                        ))
                    } else {
                        Ok(())
                    }
                },
            );
            if canceled_while_reading {
                return Err(anyhow::Error::new(super::wire::Failure::new(
                    super::wire::FailureKind::Canceled,
                    "export original verification canceled",
                )));
            }
            let verified = verified.map_err(export_original_error)?;
            original_cancel(cancel)?;
            Some(verified)
        } else {
            None
        };
        let record = self
            .record
            .as_mut()
            .context("export original catalog root is not retained")?;
        ensure!(
            request.root == record.bootstrap.root_capability(),
            "export original session mismatch"
        );
        record.verify_root_binding()?;
        let result = (|| -> Result<ExportOriginalValue> {
            match request.action {
                ExportOriginalAction::Begin => {
                    ensure!(
                        record.export_original.is_none(),
                        "export original lease already active"
                    );
                    let verified = candidate.expect("begin candidate verified");
                    let revision = verified.revision().clone();
                    record.export_original_terminal = None;
                    record.export_original = Some(ExportOriginalTransfer {
                        requested: request.requested.clone(),
                        transfer: request.transfer.clone(),
                        next_step: 1,
                        allowance: request.allowance.0,
                        verified,
                    });
                    Ok(ExportOriginalValue::Begun { revision })
                }
                action => {
                    if record.export_original.is_none() {
                        if record.export_original_terminal.is_none()
                            && matches!(action, ExportOriginalAction::Abort)
                        {
                            ensure!(
                                request.step.0 == 1,
                                "unknown export original abort step mismatch"
                            );
                            record.export_original_terminal = Some(ExportOriginalTerminal {
                                requested: request.requested.clone(),
                                transfer: request.transfer.clone(),
                                step: request.step.0,
                                allowance: request.allowance.0,
                                value: ExportOriginalTerminalValue::Aborted,
                            });
                            return Ok(ExportOriginalValue::Aborted);
                        }
                        let terminal = record
                            .export_original_terminal
                            .as_mut()
                            .context("export original lease is not retained")?;
                        ensure!(
                            terminal.requested == request.requested
                                && terminal.transfer == request.transfer
                                && terminal.allowance == request.allowance.0,
                            "export original terminal provenance mismatch"
                        );
                        return match (&mut terminal.value, action) {
                            (
                                ExportOriginalTerminalValue::Finished,
                                ExportOriginalAction::Finish,
                            ) => {
                                ensure!(
                                    terminal.step == request.step.0,
                                    "export original terminal step mismatch"
                                );
                                Ok(ExportOriginalValue::Finished)
                            }
                            (
                                ExportOriginalTerminalValue::Finished,
                                ExportOriginalAction::Abort,
                            ) => {
                                ensure!(
                                    terminal.step.checked_add(1) == Some(request.step.0),
                                    "export original terminal abort step mismatch"
                                );
                                terminal.step = request.step.0;
                                terminal.value = ExportOriginalTerminalValue::Aborted;
                                Ok(ExportOriginalValue::Aborted)
                            }
                            (ExportOriginalTerminalValue::Aborted, ExportOriginalAction::Abort) => {
                                ensure!(
                                    terminal.step == request.step.0,
                                    "export original terminal step mismatch"
                                );
                                Ok(ExportOriginalValue::Aborted)
                            }
                            _ => anyhow::bail!("export original terminal action mismatch"),
                        };
                    }
                    let transfer = record
                        .export_original
                        .as_mut()
                        .context("export original lease is not retained")?;
                    ensure!(
                        transfer.requested == request.requested
                            && transfer.transfer == request.transfer
                            && transfer.allowance == request.allowance.0,
                        "export original lease provenance mismatch"
                    );
                    if !matches!(action, ExportOriginalAction::Abort) {
                        ensure!(
                            transfer.next_step == request.step.0,
                            "export original step mismatch"
                        );
                        transfer.next_step = transfer
                            .next_step
                            .checked_add(1)
                            .context("export original step exhausted")?;
                    }
                    match action {
                        ExportOriginalAction::Recheck => {
                            original_cancel(cancel)?;
                            transfer.verified.recheck()?;
                            original_cancel(cancel)?;
                            Ok(ExportOriginalValue::Rechecked)
                        }
                        ExportOriginalAction::Finish => {
                            let transfer =
                                record.export_original.take().expect("retained original");
                            record.export_original_terminal = Some(ExportOriginalTerminal {
                                requested: transfer.requested,
                                transfer: transfer.transfer,
                                step: request.step.0,
                                allowance: transfer.allowance,
                                value: ExportOriginalTerminalValue::Finished,
                            });
                            Ok(ExportOriginalValue::Finished)
                        }
                        ExportOriginalAction::Abort => {
                            ensure!(
                                request.step.0 == transfer.next_step
                                    || transfer.next_step.checked_add(1) == Some(request.step.0),
                                "export original abort step mismatch"
                            );
                            let transfer =
                                record.export_original.take().expect("retained original");
                            record.export_original_terminal = Some(ExportOriginalTerminal {
                                requested: transfer.requested,
                                transfer: transfer.transfer,
                                step: request.step.0,
                                allowance: transfer.allowance,
                                value: ExportOriginalTerminalValue::Aborted,
                            });
                            Ok(ExportOriginalValue::Aborted)
                        }
                        ExportOriginalAction::Begin => unreachable!(),
                    }
                }
            }
        })();
        if let Err(error) = record.verify_root_binding() {
            return Err(anyhow::Error::new(super::wire::Failure::new(
                super::wire::FailureKind::Unknown,
                error,
            )));
        }
        let value = result?;
        Ok(ExportOriginalReply {
            root: request.root.clone(),
            requested: request.requested.clone(),
            transfer: request.transfer.clone(),
            step: request.step,
            value,
        })
    }
    pub fn export_publication_call(
        &mut self,
        request: &ExportPublicationRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportPublicationReply> {
        request.validate()?;
        ensure!(
            self.progress
                .as_ref()
                .is_some_and(|p| p.state == PreparationState::Confirmed),
            "export publication requires confirmed SQL admission"
        );
        let digest = request.digest()?;
        {
            let record = self
                .record
                .as_ref()
                .context("export publication catalog root is not retained")?;
            ensure!(
                request.root == record.bootstrap.root_capability(),
                "export publication session mismatch"
            );
            record.verify_root_binding()?;
            if let Some(active) = &record.export_publication
                && active.transfer == request.transfer
                && active.source == request.source
                && active.last.step == request.step
            {
                ensure!(
                    active.mode == request.mode && active.last.request_digest == digest,
                    "publication replay differs"
                );
                return cached_publication(&active.last);
            }
            if let Some(terminal) = &record.export_publication_terminal
                && terminal.transfer == request.transfer
                && terminal.source == request.source
                && terminal.last.step == request.step
            {
                ensure!(
                    terminal.mode == request.mode && terminal.last.request_digest == digest,
                    "publication replay differs"
                );
                return cached_publication(&terminal.last);
            }
        }
        if matches!(request.action, ExportPublicationAction::Begin) {
            publication_cancel(cancel)?;
            {
                let record = self.record.as_ref().unwrap();
                ensure!(
                    record.export_publication.is_none(),
                    "export publication already active"
                );
            }
            let mut hashed = PublicationHashProgress::default();
            let mut checkpoint = |bytes| hashed.checkpoint(bytes, cancel);
            let prepared = (|| -> Result<_> {
                let seal = match &request.source {
                    ExportPublicationSource::Sealed(seal) => seal.clone(),
                    ExportPublicationSource::Recovery {
                        snapshot,
                        authority_digest,
                    } => {
                        let seal =
                            crate::metadata_export::read_photo_seal_for_restore_with_checkpoint(
                                snapshot,
                                authority_digest,
                                &mut checkpoint,
                            )?;
                        ensure!(
                            seal.snapshot == *snapshot
                                && seal.authority_digest == *authority_digest,
                            "publication recovery authority mismatch"
                        );
                        seal
                    }
                };
                let publication = match request.mode {
                    ExportPublicationMode::Publish => {
                        crate::metadata_export::PhotoPublication::prepare_with_checkpoint(
                            &seal,
                            &mut checkpoint,
                        )?
                    }
                    ExportPublicationMode::Restore => {
                        crate::metadata_export::PhotoPublication::prepare_restore_with_checkpoint(
                            &seal,
                            &mut checkpoint,
                        )?
                    }
                };
                Ok((seal, publication))
            })();
            drop(checkpoint);
            let (seal, publication) = prepared.map_err(|error| {
                anyhow::Error::new(export_publication_failure(error, hashed.canceled))
            })?;
            publication_cancel(cancel)?;
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
                hashed_bytes: crate::application::U64(hashed.total()),
            };
            reply.validate(request)?;
            let record = self.record.as_mut().unwrap();
            record.verify_root_binding()?;
            let last = CachedPublication {
                step: request.step,
                request_digest: digest,
                reply: reply.clone(),
            };
            record.export_publication = Some(ExportPublicationTransfer {
                mode: request.mode,
                source: request.source.clone(),
                transfer: request.transfer.clone(),
                next_step: 1,
                seal,
                publication,
                last,
            });
            record.export_publication_terminal = None;
            return Ok(reply);
        }

        let record = self
            .record
            .as_mut()
            .context("export publication catalog root is not retained")?;
        ensure!(
            request.root == record.bootstrap.root_capability(),
            "export publication session mismatch"
        );
        record.verify_root_binding()?;
        if record.export_publication.is_none() {
            let terminal = record
                .export_publication_terminal
                .as_mut()
                .context("export publication lease is not retained")?;
            ensure!(
                terminal.transfer == request.transfer
                    && terminal.source == request.source
                    && terminal.mode == request.mode,
                "export publication terminal provenance mismatch"
            );
            ensure!(
                matches!(request.action, ExportPublicationAction::Abort)
                    && request.step.0 == terminal.next_step,
                "export publication terminal action mismatch"
            );
            let reply = ExportPublicationReply {
                mode: request.mode,
                request_digest: request.digest()?,
                root: request.root.clone(),
                transfer: request.transfer.clone(),
                step: request.step,
                seal: terminal.seal.clone(),
                value: ExportPublicationValue::Aborted,
                timings: terminal.last.reply.timings.clone(),
                hashed_bytes: crate::application::U64(0),
            };
            reply.validate(request)?;
            terminal.next_step = terminal
                .next_step
                .checked_add(1)
                .context("publication terminal step exhausted")?;
            terminal.last = CachedPublication {
                step: request.step,
                request_digest: digest,
                reply: reply.clone(),
            };
            return Ok(reply);
        }
        let transfer = record.export_publication.as_mut().unwrap();
        ensure!(
            transfer.transfer == request.transfer
                && transfer.source == request.source
                && transfer.mode == request.mode,
            "export publication lease provenance mismatch"
        );
        ensure!(
            transfer.next_step == request.step.0,
            "export publication step mismatch"
        );
        let next_step = request
            .step
            .0
            .checked_add(1)
            .context("export publication step exhausted")?;
        let mut hashed = PublicationHashProgress::default();
        let mut checkpoint = |bytes| hashed.checkpoint(bytes, cancel);
        let value = (|| -> Result<ExportPublicationValue> {
            if !request.cleanup() {
                publication_cancel(cancel)?;
            }
            ensure!(
                !matches!(
                    (request.mode, &request.action),
                    (
                        ExportPublicationMode::Restore,
                        ExportPublicationAction::Capture
                            | ExportPublicationAction::VerifyCapture
                            | ExportPublicationAction::Link
                    ) | (
                        ExportPublicationMode::Publish,
                        ExportPublicationAction::RestoreLink
                            | ExportPublicationAction::VerifyRestored
                            | ExportPublicationAction::RecheckRestored
                    )
                ),
                "publication action differs from admitted mode"
            );
            #[cfg(test)]
            export_publication_mutation_test_barrier(&request.action, cancel)?;
            Ok(match &request.action {
                ExportPublicationAction::Begin => unreachable!(),
                ExportPublicationAction::RecheckPayload => {
                    transfer.publication.recheck_payload()?;
                    ExportPublicationValue::RecheckedPayload
                }
                ExportPublicationAction::Capture => {
                    transfer.publication.capture()?;
                    ExportPublicationValue::Captured
                }
                ExportPublicationAction::VerifyCapture => {
                    transfer
                        .publication
                        .verify_capture_with_checkpoint(&mut checkpoint)?;
                    ExportPublicationValue::CaptureVerified
                }
                ExportPublicationAction::FailureReceipt { detail } => {
                    let receipt = transfer
                        .publication
                        .failure_receipt_with_checkpoint(detail.clone(), &mut checkpoint);
                    publication_cancel(cancel)?;
                    ExportPublicationValue::Receipt(receipt)
                }
                ExportPublicationAction::Link => {
                    transfer.publication.link()?;
                    ExportPublicationValue::Linked
                }
                ExportPublicationAction::VerifyInstalled => ExportPublicationValue::Installed(
                    transfer
                        .publication
                        .verify_installed_with_checkpoint(&mut checkpoint)?,
                ),
                ExportPublicationAction::RecheckInstalled => {
                    transfer.publication.recheck_installed()?;
                    ExportPublicationValue::RecheckedInstalled
                }
                ExportPublicationAction::RestoreLink => {
                    transfer.publication.restore_link()?;
                    ExportPublicationValue::RestoredLinked
                }
                ExportPublicationAction::VerifyRestored => ExportPublicationValue::Restored(
                    transfer
                        .publication
                        .verify_restored_with_checkpoint(&mut checkpoint)?,
                ),
                ExportPublicationAction::RecheckRestored => {
                    transfer.publication.recheck_restored()?;
                    ExportPublicationValue::RecheckedRestored
                }
                ExportPublicationAction::Finish => ExportPublicationValue::Finished,
                ExportPublicationAction::Abort => ExportPublicationValue::Aborted,
            })
        })();
        drop(checkpoint);
        let value = match value {
            Ok(value) => value,
            Err(error) => {
                ExportPublicationValue::Failed(export_publication_failure(error, hashed.canceled))
            }
        };
        let mut reply = ExportPublicationReply {
            mode: request.mode,
            request_digest: request.digest()?,
            root: request.root.clone(),
            transfer: request.transfer.clone(),
            step: request.step,
            seal: transfer.seal.clone(),
            value,
            timings: transfer.publication.timings().clone(),
            hashed_bytes: crate::application::U64(hashed.total()),
        };
        reply.validate(request)?;
        let _ = transfer;
        if let Err(error) = record.verify_root_binding() {
            reply.value = ExportPublicationValue::Failed(super::wire::Failure::new(
                super::wire::FailureKind::Unknown,
                error,
            ));
        }
        if request.cleanup() && !matches!(reply.value, ExportPublicationValue::Failed(_)) {
            let transfer = record.export_publication.take().unwrap();
            record.export_publication_terminal = Some(ExportPublicationTerminal {
                mode: transfer.mode,
                source: transfer.source,
                transfer: transfer.transfer,
                next_step,
                seal: transfer.seal,
                last: CachedPublication {
                    step: request.step,
                    request_digest: digest,
                    reply: reply.clone(),
                },
            });
        } else {
            let transfer = record
                .export_publication
                .as_mut()
                .expect("publication retained after step");
            transfer.next_step = next_step;
            transfer.last = CachedPublication {
                step: request.step,
                request_digest: digest,
                reply: reply.clone(),
            };
        }
        Ok(reply)
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
fn original_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Err(anyhow::Error::new(super::wire::Failure::new(
            super::wire::FailureKind::Canceled,
            "export original verification canceled",
        )));
    }
    Ok(())
}
fn export_original_error(error: anyhow::Error) -> anyhow::Error {
    if error
        .downcast_ref::<crate::metadata_export::FileByteLimit>()
        .is_some()
    {
        anyhow::Error::new(super::wire::Failure::new(
            super::wire::FailureKind::ResourceLimit,
            "export original exceeds its byte allowance",
        ))
    } else {
        error
    }
}
#[derive(Default)]
struct PublicationHashProgress {
    completed: u64,
    current: u64,
    canceled: bool,
}
impl PublicationHashProgress {
    fn checkpoint(&mut self, bytes: u64, cancel: &AtomicBool) -> std::io::Result<()> {
        if bytes < self.current {
            self.completed = self.completed.saturating_add(self.current);
        }
        self.current = bytes;
        #[cfg(test)]
        export_publication_test_barrier(bytes, cancel)?;
        if cancel.load(Ordering::Acquire) {
            self.canceled = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "export publication verification canceled",
            ));
        }
        Ok(())
    }
    fn total(&self) -> u64 {
        self.completed.saturating_add(self.current)
    }
}
fn publication_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Err(anyhow::Error::new(super::wire::Failure::new(
            super::wire::FailureKind::Canceled,
            "export publication canceled before mutation",
        )));
    }
    Ok(())
}
fn export_publication_failure(error: anyhow::Error, canceled: bool) -> super::wire::Failure {
    if canceled {
        super::wire::Failure::new(super::wire::FailureKind::Canceled, error)
    } else if let Some(failure) = error.downcast_ref::<super::wire::Failure>() {
        failure.clone()
    } else if error.is::<crate::metadata_export::FileByteLimit>() {
        super::wire::Failure::new(super::wire::FailureKind::ResourceLimit, error)
    } else if error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::Interrupted)
    {
        super::wire::Failure::new(super::wire::FailureKind::Canceled, error)
    } else {
        super::wire::Failure::new(super::wire::FailureKind::Rejected, error)
    }
}
fn cached_publication(cached: &CachedPublication) -> Result<ExportPublicationReply> {
    Ok(cached.reply.clone())
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
