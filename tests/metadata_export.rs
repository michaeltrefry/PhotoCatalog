#[path = "../src/metadata_export.rs"]
mod metadata_export;
use metadata_export::*;
use std::{fs, io, path::Path};

fn bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap()
}
fn injected() -> io::Error {
    io::Error::new(
        io::ErrorKind::StorageFull,
        "injected full-disk/publication failure",
    )
}

#[test]
fn plan_is_read_only_payload_bound_and_success_retains_previous_bytes() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().join("photo.xmp");
    let original = root.path().join("photo.CR2");
    fs::write(&original, b"private original")?;
    fs::write(&destination, b"old opaque XMP")?;
    let plan = plan_export(&destination, b"new opaque XMP")?;
    assert_eq!(fs::read_dir(root.path())?.count(), 2);
    assert!(apply_export(&plan, b"wrong payload").is_err());
    assert_eq!(bytes(&destination), b"old opaque XMP");
    let receipt = apply_export(&plan, b"new opaque XMP")?;
    assert_eq!(receipt.state, ExportState::Published);
    assert_eq!(
        bytes(receipt.captured_original.as_ref().unwrap()),
        b"old opaque XMP"
    );
    assert_eq!(bytes(&destination), b"new opaque XMP");
    assert_eq!(bytes(&original), b"private original");
    assert_eq!(
        apply_export(&plan, b"new opaque XMP")?.state,
        ExportState::Published
    );
    assert_eq!(
        recover_export(&receipt.recovery_directory)?.state,
        ExportState::Published
    );
    assert_eq!(
        discover_exports(root.path())?,
        vec![receipt.recovery_directory]
    );
    Ok(())
}

#[test]
fn absent_plan_never_clobbers_new_destination_even_with_identical_bytes() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let fresh = root.path().join("fresh.xmp");
    let fresh_plan = plan_export(&fresh, b"created")?;
    let created = apply_export(&fresh_plan, b"created")?;
    assert_eq!(created.state, ExportState::Published);
    assert!(created.captured_original.is_none());
    assert_eq!(bytes(&fresh), b"created");
    assert_eq!(
        recover_export(&created.recovery_directory)?.state,
        ExportState::Published
    );
    let path = root.path().join("new.xmp");
    let plan = plan_export(&path, b"wanted")?;
    let receipt = apply_export_with_hook(&plan, b"wanted", |phase| {
        if phase == ExportBoundary::BeforePublish {
            fs::write(&path, b"wanted")?;
        }
        Ok(())
    })?;
    assert_eq!(receipt.state, ExportState::Conflict);
    assert!(receipt.captured_original.is_none());
    assert_eq!(bytes(&path), b"wanted");
    assert_eq!(
        recover_export(&receipt.recovery_directory)?.state,
        ExportState::Conflict
    );
    Ok(())
}

#[test]
fn preexisting_change_is_untouched_and_capture_race_restores_changed_bytes() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("photo.xmp");
    fs::write(&path, b"old")?;
    let plan = plan_export(&path, b"new")?;
    fs::write(&path, b"external")?;
    assert_eq!(apply_export(&plan, b"new")?.state, ExportState::Conflict);
    assert_eq!(bytes(&path), b"external");
    let plan = plan_export(&path, b"new")?;
    let receipt = apply_export_with_hook(&plan, b"new", |phase| {
        if phase == ExportBoundary::BeforeCapture {
            fs::write(&path, b"raced external edit")?;
        }
        Ok(())
    })?;
    assert_eq!(receipt.state, ExportState::Restored);
    assert_eq!(bytes(&path), b"raced external edit");
    assert_eq!(
        bytes(receipt.captured_original.as_ref().unwrap()),
        b"raced external edit"
    );
    Ok(())
}

#[test]
fn capture_gap_new_destination_and_original_are_both_retained() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("photo.xmp");
    fs::write(&path, b"old")?;
    let plan = plan_export(&path, b"new")?;
    let receipt = apply_export_with_hook(&plan, b"new", |phase| {
        if phase == ExportBoundary::Captured {
            assert!(!path.exists());
            fs::write(&path, b"third party")?;
        }
        Ok(())
    })?;
    assert_eq!(receipt.state, ExportState::Conflict);
    assert_eq!(bytes(&path), b"third party");
    assert_eq!(bytes(receipt.captured_original.as_ref().unwrap()), b"old");
    assert_eq!(
        recover_export(&receipt.recovery_directory)?.state,
        ExportState::Conflict
    );
    assert_eq!(bytes(&path), b"third party");
    Ok(())
}

#[test]
fn failures_before_and_after_capture_preserve_recovery_and_external_bytes() -> anyhow::Result<()> {
    for failure in [
        ExportBoundary::Prepared,
        ExportBoundary::Captured,
        ExportBoundary::BeforePublish,
        ExportBoundary::Published,
    ] {
        let root = tempfile::tempdir()?;
        let path = root.path().join("photo.xmp");
        fs::write(&path, b"old")?;
        let plan = plan_export(&path, b"new")?;
        let receipt = apply_export_with_hook(&plan, b"new", |phase| {
            if phase == failure {
                Err(injected())
            } else {
                Ok(())
            }
        })?;
        assert_ne!(receipt.state, ExportState::Published);
        assert!(receipt.recovery_directory.join("payload").is_file());
        if failure == ExportBoundary::Published {
            assert_eq!(bytes(&path), b"new");
            assert_eq!(
                recover_export(&receipt.recovery_directory)?.state,
                ExportState::Published
            );
        } else {
            assert_eq!(bytes(&path), b"old");
        }
    }
    Ok(())
}

#[test]
fn low_disk_during_preparation_never_captures_destination() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("photo.xmp");
    fs::write(&path, b"external original")?;
    let plan = plan_export(&path, b"new")?;
    let receipt = apply_export_with_hook(&plan, b"new", |phase| {
        if phase == ExportBoundary::BeforePayload {
            Err(injected())
        } else {
            Ok(())
        }
    })?;
    assert_eq!(receipt.state, ExportState::Recoverable);
    assert!(receipt.detail.contains("preparation failed before capture"));
    assert!(receipt.captured_original.is_none());
    assert_eq!(bytes(&path), b"external original");
    assert_eq!(
        discover_exports(root.path())?,
        vec![receipt.recovery_directory]
    );
    Ok(())
}

#[test]
fn rollback_failure_exposes_missing_destination_and_can_resume() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("photo.xmp");
    fs::write(&path, b"old")?;
    let plan = plan_export(&path, b"new")?;
    let receipt = apply_export_with_hook(&plan, b"new", |phase| {
        if matches!(
            phase,
            ExportBoundary::Captured | ExportBoundary::BeforeRestore
        ) {
            Err(injected())
        } else {
            Ok(())
        }
    })?;
    assert_eq!(receipt.state, ExportState::Recoverable);
    assert!(!path.exists());
    assert_eq!(bytes(receipt.captured_original.as_ref().unwrap()), b"old");
    assert_eq!(
        recover_export(&receipt.recovery_directory)?.state,
        ExportState::Published
    );
    assert_eq!(bytes(&path), b"new");
    Ok(())
}

#[test]
fn simultaneous_recovery_is_refused_and_late_original_handle_writes_are_retained()
-> anyhow::Result<()> {
    use std::io::Write;
    let root = tempfile::tempdir()?;
    let path = root.path().join("photo.xmp");
    fs::write(&path, b"old")?;
    let mut writer = fs::OpenOptions::new().append(true).open(&path)?;
    let plan = plan_export(&path, b"new")?;
    let receipt = apply_export_with_hook(&plan, b"new", |phase| {
        if phase == ExportBoundary::Captured {
            let dirs = discover_exports(root.path()).unwrap();
            assert!(recover_export(&dirs[0]).is_err());
        }
        Ok(())
    })?;
    assert_eq!(receipt.state, ExportState::Published);
    writer.write_all(b" + late external write")?;
    writer.sync_all()?;
    assert_eq!(bytes(&path), b"new");
    assert_eq!(
        bytes(receipt.captured_original.as_ref().unwrap()),
        b"old + late external write"
    );
    Ok(())
}

#[test]
fn extensions_directories_corrupt_stage_and_foreign_journal_are_refused() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    assert!(plan_export(&root.path().join("original.CR2"), b"new").is_err());
    let dir = root.path().join("directory.xmp");
    fs::create_dir(&dir)?;
    assert!(plan_export(&dir, b"new").is_err());
    let path = root.path().join("photo.xmp");
    fs::write(&path, b"old")?;
    let plan = plan_export(&path, b"new")?;
    let receipt = apply_export_with_hook(&plan, b"new", |phase| {
        if phase == ExportBoundary::Prepared {
            Err(injected())
        } else {
            Ok(())
        }
    })?;
    fs::write(receipt.recovery_directory.join("payload"), b"corrupt")?;
    assert_ne!(
        recover_export(&receipt.recovery_directory)?.state,
        ExportState::Published
    );
    assert_eq!(bytes(&path), b"old");
    let mut foreign = plan.clone();
    foreign.destination = root.path().join("other.xmp");
    fs::write(
        receipt.recovery_directory.join("plan.json"),
        serde_json::to_vec(&foreign)?,
    )?;
    assert!(apply_export(&plan, b"new").is_err());
    assert!(!foreign.destination.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinks_are_never_followed_including_capture_race() -> anyhow::Result<()> {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source.CR2");
    fs::write(&source, b"untouched")?;
    let path = root.path().join("photo.xmp");
    symlink(&source, &path)?;
    assert!(plan_export(&path, b"new").is_err());
    fs::remove_file(&path)?;
    fs::write(&path, b"old")?;
    let plan = plan_export(&path, b"new")?;
    let receipt = apply_export_with_hook(&plan, b"new", |phase| {
        if phase == ExportBoundary::BeforeCapture {
            fs::remove_file(&path)?;
            symlink(&source, &path)?;
        }
        Ok(())
    })?;
    assert_eq!(receipt.state, ExportState::Recoverable);
    assert!(
        fs::symlink_metadata(receipt.captured_original.as_ref().unwrap())?
            .file_type()
            .is_symlink()
    );
    assert_eq!(bytes(&source), b"untouched");
    assert!(!path.exists());
    Ok(())
}

#[test]
fn crash_worker() -> anyhow::Result<()> {
    let Some(root) = std::env::var_os("PHOTOCATALOG_EXPORT_CRASH_ROOT") else {
        return Ok(());
    };
    let root = Path::new(&root);
    let plan: ExportPlan = serde_json::from_slice(&fs::read(root.join("test-plan.json"))?)?;
    let boundary = std::env::var("PHOTOCATALOG_EXPORT_CRASH_BOUNDARY")?;
    apply_export_with_hook(&plan, b"new", |phase| {
        if format!("{phase:?}") == boundary {
            std::process::exit(73);
        }
        Ok(())
    })?;
    anyhow::bail!("crash boundary not reached")
}

#[test]
fn actual_process_exit_at_durable_boundaries_recovers_without_duplicate_operation()
-> anyhow::Result<()> {
    for boundary in ["Prepared", "Captured", "BeforePublish", "Published"] {
        let root = tempfile::tempdir()?;
        let path = root.path().join("photo.xmp");
        fs::write(&path, b"old")?;
        let plan = plan_export(&path, b"new")?;
        fs::write(
            root.path().join("test-plan.json"),
            serde_json::to_vec(&plan)?,
        )?;
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "crash_worker", "--nocapture"])
            .env("PHOTOCATALOG_EXPORT_CRASH_ROOT", root.path())
            .env("PHOTOCATALOG_EXPORT_CRASH_BOUNDARY", boundary)
            .status()?;
        assert_eq!(status.code(), Some(73));
        let dirs = discover_exports(root.path())?;
        assert_eq!(dirs.len(), 1);
        assert_eq!(recover_export(&dirs[0])?.state, ExportState::Published);
        assert_eq!(bytes(&path), b"new");
        assert_eq!(bytes(&dirs[0].join("original")), b"old");
        assert_eq!(apply_export(&plan, b"new")?.state, ExportState::Published);
    }
    Ok(())
}
