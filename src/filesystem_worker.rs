//! Private filesystem sibling for managed catalog admission and restore control.
//! It never opens SQLite or launches a decoder. Production selection is gated
//! on the remaining managed filesystem and SQL routes.
mod bootstrap;
pub(crate) use bootstrap::open_directory;
pub mod client;
mod export_executor;
mod export_stage;
mod lightroom_artifacts;
mod lightroom_sealed;
mod metadata_files;
mod import;
mod preview_io;
mod preview_stage;
pub mod process;
mod store;
pub mod wire;

use crate::{catalog_backup, catalog_storage::physical_object_id, storage_volume::NativePath};
use anyhow::{Context, Result, ensure};
use bootstrap::{BootstrapOwner, PreparationProgress, PreparationState};
use process::{Handler, OperationContext};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use wire::{
    AdmissionSnapshot, AdmissionState, Empty, Failure, FailureKind, Operation, Outcome, Response,
    Startup,
};

struct FilesystemHandler {
    owner: BootstrapOwner,
    lightroom_sealed: lightroom_sealed::Owner,
    lightroom_artifacts: lightroom_artifacts::Owner,
}
pub(crate) fn export_profile_transfer_layout() -> (usize, usize) {
    bootstrap::export_profile_transfer_layout()
}
pub(crate) fn export_original_transfer_layout() -> (usize, usize) {
    bootstrap::export_original_transfer_layout()
}
pub(crate) fn export_publication_transfer_layout() -> (usize, usize) {
    bootstrap::export_publication_transfer_layout()
}
pub(crate) fn export_stage_owner_layout() -> (usize, usize) {
    export_stage::owner_layout()
}
pub(crate) fn export_executor_owner_layout() -> (usize, usize) {
    export_executor::owner_layout()
}
pub(crate) fn import_owner_layout() -> (usize, usize) {
    import::owner_layout()
}
impl FilesystemHandler {
    fn new(startup: Startup) -> Result<Self> {
        startup.validate()?;
        Ok(Self {
            owner: BootstrapOwner::new(startup.epoch, startup.original_roots),
            lightroom_sealed: Default::default(),
            lightroom_artifacts: Default::default(),
        })
    }
    fn execute_inner(
        &mut self,
        operation: Operation,
        context: &OperationContext,
    ) -> Result<Response> {
        operation.validate()?;
        let cancel = context.cancellation();
        match operation {
            Operation::Storage(request) => self
                .owner
                .storage_call(&request, cancel)
                .map(Response::Storage),
            Operation::Import(request) => self
                .owner
                .import_call(&request, cancel)
                .map(Response::Import),
            Operation::ExportExecutor(request) => self
                .owner
                .export_executor_call(&request, cancel)
                .map_err(|error| export_executor_failure(error, &request))
                .map(Response::ExportExecutor),
            Operation::ExportStage(request) => self
                .owner
                .export_stage_call(&request, cancel)
                .map(Response::ExportStage),
            Operation::PreviewStage(request) => self
                .owner
                .stage_call(&request, cancel)
                .map(Response::PreviewStage),
            Operation::PreviewIo(request) => self
                .owner
                .preview_io_call(&request, cancel, |snapshot| {
                    context.publish_objects(snapshot)
                })
                .map(Response::PreviewIo),
            Operation::PreviewStore(request) => self
                .owner
                .store_call(&request, cancel, |snapshot| context.publish_store(snapshot))
                .map(Response::PreviewStore),
            Operation::ReadPreviewConfiguration(path) => {
                store::read_configuration(&path, cancel).map(Response::PreviewConfiguration)
            }
            Operation::LightroomSealedRead(request) => self
                .lightroom_sealed
                .execute(request, cancel)
                .map(Response::LightroomSealedDocument),
            Operation::LightroomArtifactPreparation(request) => self
                .lightroom_artifacts
                .execute(request, cancel)
                .map(Response::LightroomArtifactPreparation),
            Operation::PrepareExportDirectory(request) => self
                .owner
                .prepare_export_directory(&request, cancel)
                .map_err(export_directory_failure)
                .map(Response::ExportDirectory),
            Operation::ExportDestinationSnapshot(request) => self
                .owner
                .export_destination_snapshot(&request, cancel)
                .map_err(export_directory_failure)
                .map(Response::ExportDestinationSnapshot),
            Operation::MigrationIdentity(request) => self
                .owner
                .migration_identity(&request, cancel)
                .map_err(export_directory_failure)
                .map(Response::MigrationIdentity),
            Operation::ExportAliasFact(request) => self
                .owner
                .export_alias_fact(&request, cancel)
                .map_err(export_directory_failure)
                .map(Response::ExportAliasFact),
            Operation::InspectExportOriginal(request) => self
                .owner
                .inspect_export_original(&request, cancel)
                .map_err(export_directory_failure)
                .map(Response::InspectedExportOriginal),
            Operation::ExportOriginal(request) => self
                .owner
                .export_original_call(&request, cancel)
                .map_err(export_directory_failure)
                .map(Response::ExportOriginal),
            Operation::ExportPublication(request) => self
                .owner
                .export_publication_call(&request, cancel)
                .map_err(export_directory_failure)
                .map(Response::ExportPublication),
            Operation::MetadataFiles(request) => self
                .owner
                .metadata_files_call(&request, cancel)
                .map_err(export_directory_failure)
                .map(Response::MetadataFiles),
            Operation::ExportProfile(request) => self
                .owner
                .export_profile_call(&request, cancel)
                .map_err(export_profile_failure)
                .map(Response::ExportProfile),
            Operation::PrepareCatalog(request) => self
                .owner
                .prepare(&request, cancel, |progress| {
                    context.publish_admission(snapshot(progress))
                })
                .map(Response::Bootstrap),
            Operation::ConfirmSqlAdmission(request) => {
                let result = self.owner.confirm(&request, cancel)?;
                self.publish(context)?;
                Ok(Response::Confirmed(result))
            }
            Operation::AbandonPrepare { operation, session } => {
                self.owner.abandon(operation, &session)?;
                self.publish(context)?;
                Ok(Response::Unit(Empty {}))
            }
            Operation::ReleaseRoot { root } => {
                self.owner.release(&root)?;
                context.clear_store(&root)?;
                self.publish(context)?;
                Ok(Response::Released(Empty {}))
            }
            Operation::RestoreStatus { root } => {
                check_cancel(cancel)?;
                self.owner
                    .restore_status(&root)
                    .map(Response::RestoreStatus)
            }
            Operation::RequireJobsReleased { root } => {
                check_cancel(cancel)?;
                self.owner.require_jobs_released(&root)?;
                Ok(Response::Unit(Empty {}))
            }
            Operation::ResumeRestoredJobs {
                root,
                restore_id,
                acknowledge_pending_jobs,
            } => self
                .owner
                .resume(&root, &restore_id, acknowledge_pending_jobs, cancel)
                .map(|value| Response::RestoreStatus(Some(value))),
            Operation::GlobalRestoreStatus { root } => {
                check_cancel(cancel)?;
                let observed = GlobalRoot::observe(&root)?;
                let result = catalog_backup::restore_status(&observed.path);
                observed.verify()?;
                result.map(Response::RestoreStatus)
            }
            Operation::GlobalResumeRestoredJobs {
                root,
                restore_id,
                acknowledge_pending_jobs,
            } => {
                check_cancel(cancel)?;
                let observed = GlobalRoot::observe(&root)?;
                let result = catalog_backup::resume_restored_jobs_controlled(
                    &observed.path,
                    &restore_id,
                    acknowledge_pending_jobs,
                    &mut || {
                        check_cancel(cancel)?;
                        observed.verify()
                    },
                );
                observed.verify()?;
                result.map(|value| Response::RestoreStatus(Some(value)))
            }
        }
    }
    fn publish(&self, context: &OperationContext) -> Result<()> {
        context.publish_admission(snapshot(
            self.owner
                .progress()
                .context("missing admission progress")?,
        ))
    }
}
impl Handler for FilesystemHandler {
    fn execute(&mut self, operation: Operation, context: &OperationContext) -> Outcome {
        // A filesystem failure can follow a successful creation or publication.
        // Preserve uncertainty and the separate admission record for reconciliation.
        let export_profile = matches!(&operation, Operation::ExportProfile(_));
        self.execute_inner(operation, context).map_err(|error| {
            if export_profile {
                filesystem_failure(export_profile_failure(error))
            } else {
                filesystem_failure(error)
            }
        })
    }
    fn shutdown(&mut self) -> std::result::Result<(), Failure> {
        self.owner
            .shutdown()
            .map_err(|error| Failure::new(FailureKind::Unknown, error))
    }
}
fn filesystem_failure(error: anyhow::Error) -> Failure {
    let kind = if let Some(failure) = error.downcast_ref::<Failure>() {
        failure.kind
    } else if error.is::<crate::catalog_session::store::ResourceLimit>() {
        FailureKind::ResourceLimit
    } else {
        FailureKind::Unknown
    };
    let object_receipt = error
        .downcast_ref::<Failure>()
        .and_then(|f| f.object_receipt);
    let mut failure = Failure::new(kind, error);
    failure.object_receipt = object_receipt;
    failure
}
fn export_executor_failure(
    error: anyhow::Error,
    request: &crate::catalog_session::export_executor::Request,
) -> anyhow::Error {
    let mut failure = error
        .downcast_ref::<Failure>()
        .cloned()
        .unwrap_or_else(|| Failure::new(FailureKind::Unknown, error));
    failure.object_receipt = Some(crate::catalog_session::preview_io::FailureReceipt {
        operation: request.operation,
        step: crate::application::U64(0),
        request_digest: request
            .receipt_digest()
            .expect("validated export executor request digest"),
    });
    anyhow::Error::new(failure)
}
fn export_directory_failure(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<Failure>().is_some() {
        error
    } else if error.is::<crate::metadata_export::FileByteLimit>() {
        Failure::new(FailureKind::ResourceLimit, error).into()
    } else {
        Failure::new(FailureKind::Rejected, error).into()
    }
}
fn export_profile_failure(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<Failure>().is_some() {
        error
    } else {
        Failure::new(FailureKind::Rejected, error).into()
    }
}
fn snapshot(value: &PreparationProgress) -> AdmissionSnapshot {
    AdmissionSnapshot {
        operation: value.request.operation,
        session: value.request.session.clone(),
        directory_created: value.directory_created,
        catalog_created: value.catalog_created,
        manifest_created: value.manifest_created,
        bootstrap: value.bootstrap.clone(),
        state: match value.state {
            PreparationState::Preparing => AdmissionState::Preparing,
            PreparationState::Prepared => AdmissionState::Prepared,
            PreparationState::Confirmed => AdmissionState::Confirmed,
            PreparationState::Abandoned => AdmissionState::Abandoned,
            PreparationState::Failed => AdmissionState::Failed,
        },
        failure: value
            .error
            .as_ref()
            .map(|error| Failure::new(FailureKind::Unknown, error)),
    }
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "filesystem operation canceled"
    );
    Ok(())
}

// Global operations retain their own directory observation. They cannot provide
// confirmation for a managed session, nor substitute a caller path for its cap.
struct GlobalRoot {
    requested: PathBuf,
    path: PathBuf,
    directory: Option<File>,
}
impl GlobalRoot {
    fn observe(path: &NativePath) -> Result<Self> {
        crate::catalog_session::validate_path(path)?;
        let requested = path.to_path()?;
        let path = crate::prospective_directory(&requested)?;
        crate::catalog_session::validate_path(&NativePath::from_path(&path))?;
        let directory = Self::open_if_present(&path)?;
        Ok(Self {
            requested,
            path,
            directory,
        })
    }
    fn open_if_present(path: &Path) -> Result<Option<File>> {
        match fs::symlink_metadata(path) {
            Ok(_) => bootstrap::open_directory(path).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    fn verify(&self) -> Result<()> {
        ensure!(
            crate::prospective_directory(&self.requested)? == self.path,
            "global restore path resolution changed"
        );
        let current = Self::open_if_present(&self.path)?;
        match (&self.directory, current) {
            (None, None) => Ok(()),
            (Some(held), Some(current)) => {
                ensure!(
                    physical_object_id(held)? == physical_object_id(&current)?,
                    "global restore root moved or was replaced"
                );
                Ok(())
            }
            _ => anyhow::bail!("global restore root changed"),
        }
    }
}

/// Invoked only by the configured executable's hidden dispatch. stderr belongs
/// exclusively to framed controls; callers must not print unframed diagnostics.
pub fn worker_main() -> Result<()> {
    // This entry point runs only in the dedicated child. Caught panics are
    // reported by the framed owner; the default hook must not corrupt stderr.
    std::panic::set_hook(Box::new(|_| {}));
    process::serve(
        std::io::stdin().lock(),
        std::io::stdout(),
        std::io::stderr(),
        FilesystemHandler::new,
    )
}

#[cfg(test)]
mod bootstrap_tests;

/// In-process access to the real F state machine for G custody tests.
#[cfg(test)]
pub(crate) mod export_stage_test_support {
    use super::*;
    #[derive(Default)]
    pub struct Owner(super::export_stage::Owner);
    impl Owner {
        pub fn call(
            &mut self,
            manifest: &Path,
            request: &crate::catalog_session::export_stage::Request,
            cancel: &AtomicBool,
        ) -> Result<crate::catalog_session::export_stage::Reply> {
            self.0.call(manifest, request, cancel)
        }
        pub fn empty(&self) -> bool {
            self.0.empty()
        }
    }
    #[cfg(unix)]
    pub fn maximum_receipt(work: &crate::catalog_exports::ExportWork, path: &Path) -> Result<()> {
        super::export_stage::test_maximum_receipt(work, path)
    }
    pub fn captured_begin_failure() {
        super::export_stage::test_captured_begin_failure();
    }
}

/// In-process access to the real F executor state machine for exact lifecycle
/// and persisted-recovery tests.
#[cfg(test)]
pub(crate) mod export_executor_test_support {
    use super::*;
    #[derive(Default)]
    pub struct Owner(super::export_executor::Owner);
    impl Owner {
        pub fn call(
            &mut self,
            catalog: &Path,
            manifest: &Path,
            request: &crate::catalog_session::export_executor::Request,
            cancel: &AtomicBool,
            export_stage_empty: bool,
        ) -> Result<crate::catalog_session::export_executor::Reply> {
            self.0
                .call(catalog, manifest, request, cancel, export_stage_empty)
        }
        pub fn empty(&self) -> bool {
            self.0.empty()
        }
    }
}

/// In-process access to the real F import owner for exact replay/custody tests.
#[cfg(test)]
pub(crate) mod import_test_support {
    use super::*;
    #[derive(Default)]
    pub struct Owner(super::import::Owner);
    impl Owner {
        pub fn call(
            &mut self,
            catalog: &Path,
            original_roots: &[NativePath],
            request: &crate::catalog_session::import::Request,
            cancel: &AtomicBool,
        ) -> Result<crate::catalog_session::import::Reply> {
            self.0.call(catalog, original_roots, request, cancel)
        }
        pub fn empty(&self) -> bool {
            self.0.empty()
        }
    }
}

#[cfg(windows)]
pub(crate) fn delete_export_held(file: &File) -> Result<()> {
    export_stage::delete_held(file)
}

/// Combined Bootstrap owner, including token admission before the stage cache.
#[cfg(test)]
pub(crate) mod export_bootstrap_test_support {
    use super::*;
    pub struct Owner(BootstrapOwner);
    impl Owner {
        pub fn new(base: &Path) -> Result<(Self, crate::catalog_session::RootCapability)> {
            use crate::catalog_session::{
                BootstrapMode, ConfirmSqlAdmission, LeaseId, PrepareCatalog, SQL_ROLES, SqlRole,
                SqlRoleObservation,
            };
            let mut owner = BootstrapOwner::new(LeaseId::new(), vec![]);
            let prepared = owner.prepare(
                &PrepareCatalog {
                    operation: crate::application::U64(1),
                    session: LeaseId::new(),
                    mode: BootstrapMode::DesktopCreate,
                    root: NativePath::from_path(&base.join("combined-catalog")),
                    manifest_root: NativePath::from_path(&base.join("combined-manifest")),
                    import_source: None,
                },
                &AtomicBool::new(false),
                |_| Ok(()),
            )?;
            let root = prepared.root_capability();
            owner.confirm(
                &ConfirmSqlAdmission {
                    operation: prepared.operation,
                    root: root.clone(),
                    roles: SQL_ROLES.map(|role| SqlRoleObservation {
                        role,
                        physical: if role == SqlRole::Manifest {
                            prepared.manifest.physical
                        } else {
                            prepared.catalog.physical
                        },
                    }),
                },
                &AtomicBool::new(false),
            )?;
            Ok((Self(owner), root))
        }
        pub fn stage(
            &mut self,
            request: &crate::catalog_session::export_stage::Request,
            cancel: &AtomicBool,
        ) -> Result<crate::catalog_session::export_stage::Reply> {
            self.0.export_stage_call(request, cancel)
        }
        pub fn executor(
            &mut self,
            request: &crate::catalog_session::export_executor::Request,
            cancel: &AtomicBool,
        ) -> Result<crate::catalog_session::export_executor::Reply> {
            self.0.export_executor_call(request, cancel)
        }
    }
}
