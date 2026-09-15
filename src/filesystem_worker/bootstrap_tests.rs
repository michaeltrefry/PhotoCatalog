use super::*;
use crate::{
    application::U64,
    catalog_session::{
        BootstrapMode, ConfirmSqlAdmission, EXPORT_PROFILE_BYTES, ExportOriginalAction,
        ExportOriginalRequest, ExportOriginalValue, ExportProfileAction, ExportProfileRequest,
        ExportProfileValue, InspectExportOriginal, LeaseId, PrepareCatalog, PrepareExportDirectory,
        RootCapability, SQL_ROLES, SqlRole, SqlRoleObservation,
    },
};
use std::{io::Write, sync::atomic::AtomicBool};
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

fn profile_request(
    root: &RootCapability,
    path: &Path,
    transfer: &LeaseId,
    step: u64,
    action: ExportProfileAction,
) -> ExportProfileRequest {
    ExportProfileRequest {
        root: root.clone(),
        requested: NativePath::from_path(path),
        transfer: transfer.clone(),
        step: U64(step),
        allowance: U64(EXPORT_PROFILE_BYTES as u64),
        action,
    }
}

fn original_request(
    root: &RootCapability,
    path: &Path,
    transfer: &LeaseId,
    step: u64,
    allowance: u64,
    action: ExportOriginalAction,
) -> ExportOriginalRequest {
    ExportOriginalRequest {
        root: root.clone(),
        requested: NativePath::from_path(path),
        transfer: transfer.clone(),
        step: U64(step),
        allowance: U64(allowance),
        action,
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

#[test]
fn export_profile_transfer_is_stable_bounded_bound_and_explicitly_retired() -> Result<()> {
    let temp = TempDir::new()?;
    let catalog_request = request(&temp);
    let mut owner = owner();
    let bootstrap = owner.prepare(&catalog_request, &AtomicBool::new(false), |_| Ok(()))?;
    owner.confirm(&confirmation(&bootstrap), &AtomicBool::new(false))?;
    let root = bootstrap.root_capability();
    let profile = temp.path().join("selected.icc");
    let original = vec![0xa5; crate::catalog_session::preview_io::CHUNK_BYTES + 17];
    fs::write(&profile, &original)?;

    let transfer = LeaseId::new();
    let begin = profile_request(&root, &profile, &transfer, 0, ExportProfileAction::Begin);
    let reply = owner.export_profile_call(&begin, &AtomicBool::new(false))?;
    reply.validate(&begin)?;
    assert!(
        matches!(reply.value, ExportProfileValue::Begun { bytes } if bytes == U64(original.len() as u64))
    );
    assert!(owner.release(&root).is_err());

    let mut foreign = profile_request(
        &root,
        &profile,
        &transfer,
        1,
        ExportProfileAction::Read { offset: U64(0) },
    );
    foreign.root.session = LeaseId::new();
    assert!(
        owner
            .export_profile_call(&foreign, &AtomicBool::new(false))
            .is_err()
    );
    let first = profile_request(
        &root,
        &profile,
        &transfer,
        1,
        ExportProfileAction::Read { offset: U64(0) },
    );
    let first_reply = owner.export_profile_call(&first, &AtomicBool::new(false))?;
    first_reply.validate(&first)?;
    let ExportProfileValue::Chunk { bytes, .. } = first_reply.value else {
        panic!("profile chunk")
    };
    assert_eq!(bytes, original[..bytes.len()]);
    let second = profile_request(
        &root,
        &profile,
        &transfer,
        2,
        ExportProfileAction::Read {
            offset: U64(bytes.len() as u64),
        },
    );
    let second_reply = owner.export_profile_call(&second, &AtomicBool::new(false))?;
    second_reply.validate(&second)?;
    let finish = profile_request(&root, &profile, &transfer, 3, ExportProfileAction::Finish);
    let finish_reply = owner.export_profile_call(&finish, &AtomicBool::new(false))?;
    finish_reply.validate(&finish)?;
    assert_eq!(fs::read(&profile)?, original);
    owner
        .export_profile_call(&finish, &AtomicBool::new(false))?
        .validate(&finish)?;
    let foreign_finish = profile_request(
        &root,
        &profile,
        &LeaseId::new(),
        3,
        ExportProfileAction::Finish,
    );
    assert!(
        owner
            .export_profile_call(&foreign_finish, &AtomicBool::new(false))
            .is_err()
    );
    let foreign_terminal_abort = profile_request(
        &root,
        &profile,
        &LeaseId::new(),
        4,
        ExportProfileAction::Abort,
    );
    assert!(
        owner
            .export_profile_call(&foreign_terminal_abort, &AtomicBool::new(false))
            .is_err()
    );
    let finish_abort = profile_request(&root, &profile, &transfer, 4, ExportProfileAction::Abort);
    owner.export_profile_call(&finish_abort, &AtomicBool::new(false))?;
    owner.export_profile_call(&finish_abort, &AtomicBool::new(false))?;

    let canceled_transfer = LeaseId::new();
    let begin = profile_request(
        &root,
        &profile,
        &canceled_transfer,
        0,
        ExportProfileAction::Begin,
    );
    owner.export_profile_call(&begin, &AtomicBool::new(false))?;
    let canceled = profile_request(
        &root,
        &profile,
        &canceled_transfer,
        1,
        ExportProfileAction::Read { offset: U64(0) },
    );
    let error = owner
        .export_profile_call(&canceled, &AtomicBool::new(true))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::Canceled
    );
    assert!(owner.release(&root).is_err());
    let foreign_abort = profile_request(
        &root,
        &profile,
        &LeaseId::new(),
        99,
        ExportProfileAction::Abort,
    );
    assert!(
        owner
            .export_profile_call(&foreign_abort, &AtomicBool::new(true))
            .is_err()
    );
    assert!(owner.release(&root).is_err());
    let abort = profile_request(
        &root,
        &profile,
        &canceled_transfer,
        2,
        ExportProfileAction::Abort,
    );
    owner.export_profile_call(&abort, &AtomicBool::new(true))?;
    owner.export_profile_call(&abort, &AtomicBool::new(true))?;
    assert!(
        owner
            .export_profile_call(&foreign_abort, &AtomicBool::new(true))
            .is_err()
    );

    let offset_transfer = LeaseId::new();
    let begin = profile_request(
        &root,
        &profile,
        &offset_transfer,
        0,
        ExportProfileAction::Begin,
    );
    owner.export_profile_call(&begin, &AtomicBool::new(false))?;
    let wrong_offset = profile_request(
        &root,
        &profile,
        &offset_transfer,
        1,
        ExportProfileAction::Read { offset: U64(1) },
    );
    assert!(
        owner
            .export_profile_call(&wrong_offset, &AtomicBool::new(false))
            .is_err()
    );
    let abort = profile_request(
        &root,
        &profile,
        &offset_transfer,
        2,
        ExportProfileAction::Abort,
    );
    owner.export_profile_call(&abort, &AtomicBool::new(false))?;

    let growing_transfer = LeaseId::new();
    let begin = profile_request(
        &root,
        &profile,
        &growing_transfer,
        0,
        ExportProfileAction::Begin,
    );
    owner.export_profile_call(&begin, &AtomicBool::new(false))?;
    fs::OpenOptions::new()
        .append(true)
        .open(&profile)?
        .write_all(b"changed")?;
    let mut offset = 0usize;
    let mut step = 1u64;
    while offset < original.len() {
        let read = profile_request(
            &root,
            &profile,
            &growing_transfer,
            step,
            ExportProfileAction::Read {
                offset: U64(offset as u64),
            },
        );
        let reply = owner.export_profile_call(&read, &AtomicBool::new(false))?;
        let ExportProfileValue::Chunk { bytes, .. } = reply.value else {
            panic!("profile chunk")
        };
        offset += bytes.len();
        step += 1;
    }
    let finish = profile_request(
        &root,
        &profile,
        &growing_transfer,
        step,
        ExportProfileAction::Finish,
    );
    assert!(
        owner
            .export_profile_call(&finish, &AtomicBool::new(false))
            .is_err()
    );
    let abort = profile_request(
        &root,
        &profile,
        &growing_transfer,
        step + 1,
        ExportProfileAction::Abort,
    );
    owner.export_profile_call(&abort, &AtomicBool::new(false))?;

    let oversized = temp.path().join("oversized.icc");
    fs::File::create(&oversized)?.set_len(EXPORT_PROFILE_BYTES as u64 + 1)?;
    let begin = profile_request(
        &root,
        &oversized,
        &LeaseId::new(),
        0,
        ExportProfileAction::Begin,
    );
    let error = owner
        .export_profile_call(&begin, &AtomicBool::new(false))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::ResourceLimit
    );
    #[cfg(unix)]
    {
        let link = temp.path().join("alias.icc");
        std::os::unix::fs::symlink(&profile, &link)?;
        let begin = profile_request(&root, &link, &LeaseId::new(), 0, ExportProfileAction::Begin);
        assert!(
            owner
                .export_profile_call(&begin, &AtomicBool::new(false))
                .is_err()
        );
    }
    owner.release(&root)?;
    owner.shutdown()?;
    Ok(())
}

#[test]
fn export_original_inspection_and_lease_are_bounded_bound_rechecked_and_recoverable() -> Result<()>
{
    let temp = TempDir::new()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    let catalog_request = request(&temp);
    let mut owner = BootstrapOwner::new(
        LeaseId::new(),
        vec![NativePath::from_path(&originals.canonicalize()?)],
    );
    let bootstrap = owner.prepare(&catalog_request, &AtomicBool::new(false), |_| Ok(()))?;
    owner.confirm(&confirmation(&bootstrap), &AtomicBool::new(false))?;
    let root = bootstrap.root_capability();
    let path = originals.join("asset.raw");
    fs::write(&path, b"original-bytes")?;
    let allowance = fs::metadata(&path)?.len();
    let inspect = InspectExportOriginal {
        root: root.clone(),
        requested: NativePath::from_path(&path),
        allowance: U64(allowance),
    };
    let inspected = owner.inspect_export_original(&inspect, &AtomicBool::new(false))?;
    inspected.validate_for(&inspect)?;
    assert_eq!(inspected.revision.bytes, allowance);
    let mut too_small = inspect.clone();
    too_small.allowance = U64(allowance - 1);
    let error = owner
        .inspect_export_original(&too_small, &AtomicBool::new(false))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::ResourceLimit
    );
    let outside = temp.path().join("outside.raw");
    fs::write(&outside, b"outside")?;
    let mut outside_request = inspect.clone();
    outside_request.requested = NativePath::from_path(&outside);
    assert!(
        owner
            .inspect_export_original(&outside_request, &AtomicBool::new(false))
            .is_err()
    );
    let mut directory_request = inspect.clone();
    directory_request.requested = NativePath::from_path(&originals);
    assert!(
        owner
            .inspect_export_original(&directory_request, &AtomicBool::new(false))
            .is_err()
    );
    #[cfg(unix)]
    {
        let link = originals.join("asset-link.raw");
        std::os::unix::fs::symlink(&path, &link)?;
        let mut link_request = inspect.clone();
        link_request.requested = NativePath::from_path(&link);
        assert!(
            owner
                .inspect_export_original(&link_request, &AtomicBool::new(false))
                .is_err()
        );
    }

    let transfer = LeaseId::new();
    let begin = original_request(
        &root,
        &path,
        &transfer,
        0,
        allowance,
        ExportOriginalAction::Begin,
    );
    let begun = owner.export_original_call(&begin, &AtomicBool::new(false))?;
    begun.validate(&begin)?;
    assert!(
        matches!(begun.value, ExportOriginalValue::Begun { revision } if revision == inspected.revision)
    );
    // Repeating the same Begin reconciles a lost response without another lease.
    owner
        .export_original_call(&begin, &AtomicBool::new(false))?
        .validate(&begin)?;
    let foreign_abort = original_request(
        &root,
        &path,
        &LeaseId::new(),
        1,
        allowance,
        ExportOriginalAction::Abort,
    );
    assert!(
        owner
            .export_original_call(&foreign_abort, &AtomicBool::new(true))
            .is_err()
    );
    assert!(owner.release(&root).is_err());
    let recheck = original_request(
        &root,
        &path,
        &transfer,
        1,
        allowance,
        ExportOriginalAction::Recheck,
    );
    owner
        .export_original_call(&recheck, &AtomicBool::new(false))?
        .validate(&recheck)?;
    let retained = originals.join("retained.raw");
    fs::rename(&path, &retained)?;
    fs::write(&path, b"replacement!!!")?;
    let changed = original_request(
        &root,
        &path,
        &transfer,
        2,
        allowance,
        ExportOriginalAction::Recheck,
    );
    assert!(
        owner
            .export_original_call(&changed, &AtomicBool::new(false))
            .is_err()
    );
    let abort = original_request(
        &root,
        &path,
        &transfer,
        3,
        allowance,
        ExportOriginalAction::Abort,
    );
    owner.export_original_call(&abort, &AtomicBool::new(true))?;
    owner.export_original_call(&abort, &AtomicBool::new(true))?;
    fs::remove_file(&path)?;
    fs::rename(&retained, &path)?;

    let finished_transfer = LeaseId::new();
    let begin = original_request(
        &root,
        &path,
        &finished_transfer,
        0,
        allowance,
        ExportOriginalAction::Begin,
    );
    owner.export_original_call(&begin, &AtomicBool::new(false))?;
    let finish = original_request(
        &root,
        &path,
        &finished_transfer,
        1,
        allowance,
        ExportOriginalAction::Finish,
    );
    owner.export_original_call(&finish, &AtomicBool::new(false))?;
    owner.export_original_call(&finish, &AtomicBool::new(false))?;
    let foreign_finish = original_request(
        &root,
        &path,
        &LeaseId::new(),
        1,
        allowance,
        ExportOriginalAction::Finish,
    );
    assert!(
        owner
            .export_original_call(&foreign_finish, &AtomicBool::new(false))
            .is_err()
    );
    let foreign_terminal_abort = original_request(
        &root,
        &path,
        &LeaseId::new(),
        2,
        allowance,
        ExportOriginalAction::Abort,
    );
    assert!(
        owner
            .export_original_call(&foreign_terminal_abort, &AtomicBool::new(true))
            .is_err()
    );
    let finish_abort = original_request(
        &root,
        &path,
        &finished_transfer,
        2,
        allowance,
        ExportOriginalAction::Abort,
    );
    owner.export_original_call(&finish_abort, &AtomicBool::new(true))?;
    owner.export_original_call(&finish_abort, &AtomicBool::new(true))?;

    // If Begin never reached F, retrying the exact Begin establishes a
    // recoverable identity; its cleanup cannot act on a foreign transfer.
    let uncertain = LeaseId::new();
    let begin = original_request(
        &root,
        &path,
        &uncertain,
        0,
        allowance,
        ExportOriginalAction::Begin,
    );
    owner.export_original_call(&begin, &AtomicBool::new(false))?;
    let abort = original_request(
        &root,
        &path,
        &uncertain,
        1,
        allowance,
        ExportOriginalAction::Abort,
    );
    owner.export_original_call(&abort, &AtomicBool::new(true))?;
    owner.release(&root)?;
    owner.shutdown()?;
    assert_eq!(fs::read(&path)?, b"original-bytes");
    Ok(())
}

#[test]
fn stale_export_executor_token_returns_bound_rejection_before_stage_effects() -> Result<()> {
    use crate::catalog_session::{export_executor, export_stage};
    let temp = TempDir::new()?;
    let catalog_request = request(&temp);
    let mut owner = owner();
    let bootstrap = owner.prepare(&catalog_request, &AtomicBool::new(false), |_| Ok(()))?;
    owner.confirm(&confirmation(&bootstrap), &AtomicBool::new(false))?;
    let root = bootstrap.root_capability();
    let executor = export_executor::executor_id(&root, 1)?;
    let acquire = export_executor::Request {
        root: root.clone(),
        executor: executor.clone(),
        operation: U64(1),
        action: export_executor::Action::Acquire,
    };
    owner.export_executor_call(&acquire, &AtomicBool::new(false))?;
    let release = export_executor::Request {
        root: root.clone(),
        executor: executor.clone(),
        operation: U64(2),
        action: export_executor::Action::Release,
    };
    owner.export_executor_call(&release, &AtomicBool::new(false))?;

    let fixture = crate::application::desktop::export_native_test_fixture()?;
    let mut begin = fixture.begin;
    begin.root = root.clone();
    begin.executor = executor;
    let manifest_root = bootstrap
        .manifest
        .path
        .to_path()?
        .parent()
        .context("manifest root")?
        .to_owned();
    let stage_root = manifest_root.join("export-workers");
    let before = if stage_root.try_exists()? {
        fs::read_dir(&stage_root)?.count()
    } else {
        0
    };
    let failure = owner
        .export_stage_call(&begin, &AtomicBool::new(false))
        .unwrap_err()
        .downcast::<Failure>()?;
    assert_eq!(failure.kind, FailureKind::Rejected);
    let receipt = failure
        .object_receipt
        .context("bound stale-token receipt")?;
    assert_eq!(receipt.operation, begin.operation);
    assert_eq!(receipt.step, U64(0));
    assert_eq!(receipt.request_digest, begin.digest()?);
    assert_eq!(
        if stage_root.try_exists()? {
            fs::read_dir(&stage_root)?.count()
        } else {
            0
        },
        before,
        "stale Begin created no stage object"
    );

    let successor = export_executor::executor_id(&root, 2)?;
    owner.export_executor_call(
        &export_executor::Request {
            root: root.clone(),
            executor: successor.clone(),
            operation: U64(1),
            action: export_executor::Action::Acquire,
        },
        &AtomicBool::new(false),
    )?;
    let failure = owner
        .export_stage_call(&begin, &AtomicBool::new(false))
        .unwrap_err()
        .downcast::<Failure>()?;
    assert_eq!(failure.kind, FailureKind::Rejected);
    assert_eq!(
        failure.object_receipt.unwrap().request_digest,
        begin.digest()?
    );
    let mut admitted = begin.clone();
    admitted.executor = successor.clone();
    owner.export_stage_call(&admitted, &AtomicBool::new(false))?;
    let successor_release = export_executor::Request {
        root: root.clone(),
        executor: successor.clone(),
        operation: U64(2),
        action: export_executor::Action::Release,
    };
    assert!(
        owner
            .export_executor_call(&successor_release, &AtomicBool::new(false))
            .is_err()
    );
    let before_late = fs::read_dir(&stage_root)?.count();
    let mut late = admitted.clone();
    late.stage = LeaseId::new();
    let failure = owner
        .export_stage_call(&late, &AtomicBool::new(false))
        .unwrap_err()
        .downcast::<Failure>()?;
    assert_eq!(failure.kind, FailureKind::Rejected);
    assert_eq!(
        failure.object_receipt.unwrap().request_digest,
        late.digest()?
    );
    assert_eq!(fs::read_dir(&stage_root)?.count(), before_late);
    let mut abort = admitted;
    abort.operation = U64(2);
    abort.action = export_stage::Action::Abort;
    owner.export_stage_call(&abort, &AtomicBool::new(false))?;
    owner.export_executor_call(&successor_release, &AtomicBool::new(false))?;
    owner.release(&root)?;
    owner.shutdown()?;
    Ok(())
}
