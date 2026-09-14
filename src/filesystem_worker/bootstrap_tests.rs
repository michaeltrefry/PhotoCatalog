use super::*;
use crate::{
    application::U64,
    catalog_session::{
        BootstrapMode, ConfirmSqlAdmission, LeaseId, PrepareCatalog, PrepareExportDirectory,
        SQL_ROLES, SqlRole, SqlRoleObservation,
    },
};
use std::sync::atomic::AtomicBool;
use tempfile::TempDir;

fn request(temp: &TempDir) -> PrepareCatalog {
    PrepareCatalog {
        operation: U64(1),
        session: LeaseId::new(),
        mode: BootstrapMode::DesktopCreate,
        root: NativePath::from_path(&temp.path().join("catalog")),
        manifest_root: NativePath::from_path(&temp.path().join("manifest")),
        import_source: None,
    }
}
fn owner() -> BootstrapOwner {
    BootstrapOwner::new(LeaseId::new(), vec![])
}
fn confirmation(value: &crate::catalog_session::CatalogBootstrap) -> ConfirmSqlAdmission {
    ConfirmSqlAdmission {
        operation: value.operation,
        root: value.root_capability(),
        roles: SQL_ROLES.map(|role| SqlRoleObservation {
            role,
            physical: if role == SqlRole::Manifest {
                value.manifest.physical
            } else {
                value.catalog.physical
            },
        }),
    }
}

#[test]
fn fresh_preparation_retains_pins_past_lost_reply_and_never_replays_create() -> Result<()> {
    let temp = TempDir::new()?;
    let request = request(&temp);
    let mut owner = owner();
    let cancel = AtomicBool::new(false);
    let error = owner
        .prepare(&request, &cancel, |progress| {
            if progress.bootstrap.is_some() {
                anyhow::bail!("injected lost preparation reply");
            }
            Ok(())
        })
        .unwrap_err();
    assert!(error.to_string().contains("lost preparation"));
    let progress = owner.progress().unwrap();
    assert!(progress.directory_created && progress.catalog_created && progress.manifest_created);
    let bootstrap = progress.bootstrap.clone().unwrap();
    assert!(owner.shutdown().is_err());
    assert!(owner.prepare(&request, &cancel, |_| Ok(())).is_err());
    // Existing empty objects are evidence of completed creation, not permission
    // to overwrite/adopt them through a second Create operation.
    assert_eq!(fs::metadata(bootstrap.catalog.path.to_path()?)?.len(), 0);
    owner.abandon(request.operation, &request.session)?;
    owner.shutdown()?;
    assert!(bootstrap.catalog.path.to_path()?.exists());
    assert!(owner.prepare(&request, &cancel, |_| Ok(())).is_err());
    Ok(())
}

#[test]
fn incomplete_role_confirmation_retains_ownership_and_same_epoch_confirmation_is_single_use()
-> Result<()> {
    let temp = TempDir::new()?;
    let mut owner = owner();
    let value = owner.prepare(&request(&temp), &AtomicBool::new(false), |_| Ok(()))?;
    let mut wrong = confirmation(&value);
    wrong.roles[7] = wrong.roles[0];
    assert!(owner.confirm(&wrong, &AtomicBool::new(false)).is_err());
    assert!(owner.shutdown().is_err());
    // This unit-level trusted observation exercises F admission logic only.
    // Actual SQL overlap is a separate cross-process integration test.
    owner.confirm(&confirmation(&value), &AtomicBool::new(false))?;
    assert!(
        owner
            .confirm(&confirmation(&value), &AtomicBool::new(false))
            .is_err()
    );
    assert!(owner.abandon(value.operation, &value.session).is_err());
    owner.release(&value.root_capability())?;
    owner.release(&value.root_capability())?;
    owner.shutdown()?;
    Ok(())
}

#[test]
fn replaced_catalog_rejects_confirmation_and_managed_marker_reads() -> Result<()> {
    let temp = TempDir::new()?;
    let mut owner = owner();
    let value = owner.prepare(&request(&temp), &AtomicBool::new(false), |_| Ok(()))?;
    let path = value.catalog.path.to_path()?;
    fs::rename(&path, path.with_extension("retained"))?;
    fs::write(&path, b"replacement")?;
    assert!(
        owner
            .confirm(&confirmation(&value), &AtomicBool::new(false))
            .is_err()
    );
    assert!(owner.restore_status(&value.root_capability()).is_err());
    owner.abandon(value.operation, &value.session)?;
    assert_eq!(fs::read(&path)?, b"replacement");
    Ok(())
}

#[test]
fn existing_mode_never_creates_missing_database_and_preserves_created_provenance() -> Result<()> {
    let temp = TempDir::new()?;
    let mut request = request(&temp);
    request.mode = BootstrapMode::DesktopExisting;
    fs::create_dir(request.root.to_path()?)?;
    let mut owner = owner();
    assert!(
        owner
            .prepare(&request, &AtomicBool::new(false), |_| Ok(()))
            .is_err()
    );
    assert!(!request.root.to_path()?.join("catalog.sqlite3").exists());
    owner.abandon(request.operation, &request.session)?;
    fs::write(request.root.to_path()?.join("catalog.sqlite3"), [])?;
    fs::create_dir(request.manifest_root.to_path()?)?;
    fs::write(
        request.manifest_root.to_path()?.join("previews.sqlite3"),
        [],
    )?;
    let value = owner.prepare(&request, &AtomicBool::new(false), |_| Ok(()))?;
    assert!(!value.catalog.created && !value.manifest.created);
    owner.abandon(request.operation, &request.session)?;
    Ok(())
}

#[test]
fn original_overlap_and_missing_create_parent_reject_before_side_effects() -> Result<()> {
    let temp = TempDir::new()?;
    let request = request(&temp);
    let mut protected =
        BootstrapOwner::new(LeaseId::new(), vec![NativePath::from_path(temp.path())]);
    assert!(
        protected
            .prepare(&request, &AtomicBool::new(false), |_| Ok(()))
            .is_err()
    );
    assert!(!request.root.to_path()?.exists());
    let mut request = request;
    request.root = NativePath::from_path(&temp.path().join("absent/catalog"));
    let mut owner = owner();
    assert!(
        owner
            .prepare(&request, &AtomicBool::new(false), |_| Ok(()))
            .is_err()
    );
    assert!(!temp.path().join("absent").exists());
    Ok(())
}

#[test]
fn cancellation_records_created_directory_and_explicit_abandon_preserves_it() -> Result<()> {
    let temp = TempDir::new()?;
    let request = request(&temp);
    let mut owner = owner();
    let cancel = AtomicBool::new(false);
    assert!(
        owner
            .prepare(&request, &cancel, |progress| {
                if progress.directory_created {
                    cancel.store(true, Ordering::Release);
                }
                Ok(())
            })
            .is_err()
    );
    let progress = owner.progress().unwrap();
    assert!(progress.directory_created && !progress.catalog_created);
    owner.abandon(request.operation, &request.session)?;
    assert!(request.root.to_path()?.is_dir());
    assert!(!request.root.to_path()?.join("catalog.sqlite3").exists());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn unix_non_unicode_catalog_and_manifest_paths_preserve_native_units() -> Result<()> {
    use std::os::unix::ffi::OsStringExt;
    let temp = TempDir::new()?;
    let mut request = request(&temp);
    request.root = NativePath::from_path(
        &temp
            .path()
            .join(std::ffi::OsString::from_vec(b"catalog-\xff".to_vec())),
    );
    request.manifest_root = NativePath::from_path(
        &temp
            .path()
            .join(std::ffi::OsString::from_vec(b"manifest-\xfe".to_vec())),
    );
    let mut owner = owner();
    let value = owner.prepare(&request, &AtomicBool::new(false), |_| Ok(()))?;
    assert_eq!(value.canonical_root, request.root);
    assert_eq!(
        value.manifest.path.to_path()?.parent(),
        Some(request.manifest_root.to_path()?.as_path())
    );
    owner.abandon(value.operation, &value.session)?;
    Ok(())
}

#[test]
fn global_restore_observation_rejects_replaced_directory() -> Result<()> {
    let temp = TempDir::new()?;
    let root = temp.path().join("restore");
    fs::create_dir(&root)?;
    let observed = GlobalRoot::observe(&NativePath::from_path(&root))?;
    fs::rename(&root, temp.path().join("original"))?;
    fs::create_dir(&root)?;
    assert!(observed.verify().is_err());
    Ok(())
}

#[test]
fn export_directory_preparation_is_read_only_bound_and_cancellable() -> Result<()> {
    let temp = TempDir::new()?;
    let catalog_request = request(&temp);
    let mut owner = owner();
    let bootstrap = owner.prepare(&catalog_request, &AtomicBool::new(false), |_| Ok(()))?;
    let output = temp.path().join("output");
    fs::create_dir(&output)?;
    fs::write(output.join("sentinel"), b"unchanged")?;
    let request = PrepareExportDirectory {
        root: bootstrap.root_capability(),
        directory: NativePath::from_path(&output),
    };
    let prepared = owner.prepare_export_directory(&request, &AtomicBool::new(false))?;
    prepared.validate_for(&request)?;
    assert_eq!(
        prepared.directory,
        NativePath::from_path(&output.canonicalize()?)
    );
    assert_eq!(fs::read(output.join("sentinel"))?, b"unchanged");
    assert_eq!(fs::read_dir(&output)?.count(), 1);

    let error = owner
        .prepare_export_directory(&request, &AtomicBool::new(true))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::Canceled
    );
    let mut foreign = request.clone();
    foreign.root.token = LeaseId::new();
    assert!(
        owner
            .prepare_export_directory(&foreign, &AtomicBool::new(false))
            .is_err()
    );
    fs::write(temp.path().join("not-directory"), b"file")?;
    let mut not_directory = request.clone();
    not_directory.directory = NativePath::from_path(&temp.path().join("not-directory"));
    let error = owner
        .prepare_export_directory(&not_directory, &AtomicBool::new(false))
        .unwrap_err();
    let error = export_directory_failure(error);
    assert_eq!(
        error.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::Rejected
    );
    assert_eq!(fs::read(output.join("sentinel"))?, b"unchanged");
    owner.abandon(bootstrap.operation, &bootstrap.session)?;
    owner.shutdown()?;
    Ok(())
}
