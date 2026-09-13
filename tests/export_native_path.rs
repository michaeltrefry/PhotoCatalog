use photocatalog::{metadata_export::*, storage_volume::NativePath};
use std::{fs, path::PathBuf};

fn unusual_name(suffix: &str) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = vec![b'p', 255];
        bytes.extend_from_slice(suffix.as_bytes());
        std::ffi::OsString::from_vec(bytes).into()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let mut units = vec![112, 0xd800];
        units.extend(suffix.encode_utf16());
        std::ffi::OsString::from_wide(&units).into()
    }
}

#[test]
fn wire_preserves_native_units_and_rejects_mixed_future_and_foreign_execution() -> anyhow::Result<()>
{
    for native in [
        NativePath::UnixBytes(vec![47, 255]),
        NativePath::WindowsWide(vec![67, 58, 92, 0xd800]),
    ] {
        let encoded = serde_json::to_string(&native)?;
        let stored: wire::StoredPath = serde_json::from_str(&encoded)?;
        assert_eq!(serde_json::to_string(&stored)?, encoded);
        let local = native.to_path();
        assert_eq!(stored.local(true).is_ok(), local.is_ok());
    }
    let temp = tempfile::tempdir()?;
    let plan = plan_export(&temp.path().join("native.xmp"), b"new")?;
    assert_eq!(plan.version, 3);
    let json = serde_json::to_value(&plan)?;
    assert!(json["destination"].is_object());
    for version in [0, 1, 2, 5, 999] {
        let mut invalid = json.clone();
        invalid["version"] = version.into();
        assert!(serde_json::from_value::<ExportPlan>(invalid).is_err());
    }
    let mut nul = json.clone();
    nul["destination"] = serde_json::to_value(NativePath::UnixBytes(vec![0]))?;
    assert!(serde_json::from_value::<ExportPlan>(nul).is_err());
    let mut foreign = json.clone();
    foreign["destination"] = serde_json::to_value(if cfg!(unix) {
        NativePath::WindowsWide(vec![67, 58, 92, 0xd800])
    } else {
        NativePath::UnixBytes(vec![47, 255])
    })?;
    let inspected: wire::Plan = serde_json::from_value(foreign.clone())?;
    assert!(ExportPlan::try_from(inspected).is_err());
    assert!(serde_json::from_value::<ExportPlan>(foreign).is_err());
    let mut empty = plan.clone();
    empty.destination = PathBuf::new();
    assert!(apply_export(&empty, b"new").is_err());
    let mut future = plan.clone();
    future.version = 99;
    assert!(apply_export(&future, b"new").is_err());
    assert_eq!(fs::read_dir(temp.path())?.count(), 0);
    Ok(())
}

#[test]
fn every_receipt_state_roundtrips_all_native_paths_and_reads_legacy() -> anyhow::Result<()> {
    for state in [
        ExportState::Published,
        ExportState::Restored,
        ExportState::Conflict,
        ExportState::Recoverable,
    ] {
        let receipt = ExportReceipt {
            state,
            destination: unusual_name(".xmp"),
            recovery_directory: unusual_name("-recovery"),
            captured_original: Some(unusual_name("-captured")),
            detail: "retained".into(),
        };
        let encoded = serde_json::to_string(&receipt)?;
        let decoded: ExportReceipt = serde_json::from_str(&encoded)?;
        assert_eq!(decoded.destination, receipt.destination);
        assert_eq!(decoded.recovery_directory, receipt.recovery_directory);
        assert_eq!(decoded.captured_original, receipt.captured_original);
        assert_eq!(decoded.state, state);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded)?["version"],
            2
        );
    }
    let legacy = r#"{"state":"Published","destination":"old.xmp","recovery_directory":"old-recovery","captured_original":"old-capture","detail":"legacy"}"#;
    let decoded: ExportReceipt = serde_json::from_str(legacy)?;
    assert_eq!(decoded.destination, PathBuf::from("old.xmp"));
    let mut future = serde_json::to_value(decoded)?;
    future["version"] = 3.into();
    assert!(serde_json::from_value::<ExportReceipt>(future).is_err());
    Ok(())
}

#[test]
fn legacy_xmp_plan_and_journal_replay_without_rewriting_authority() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let destination = temp.path().join("old.xmp");
    fs::write(&destination, b"old")?;
    let mut plan = plan_export(&destination, b"new")?;
    plan.version = 1;
    let encoded = serde_json::to_string_pretty(&plan)?.replace("old.xmp", "old\\u002exmp");
    let plan: ExportPlan = serde_json::from_str(&encoded)?;
    let receipt = apply_export(&plan, b"new")?;
    let journal = receipt.recovery_directory.join("plan.json");
    // Existing journals may use any equivalent lexical encoding; recovery never rewrites them.
    fs::write(&journal, &encoded)?;
    for _ in 0..2 {
        assert_eq!(
            recover_export(&receipt.recovery_directory)?.state,
            ExportState::Published
        );
        assert_eq!(fs::read_to_string(&journal)?, encoded);
    }
    assert_eq!(fs::read(receipt.captured_original.unwrap())?, b"old");
    Ok(())
}

#[cfg(unix)]
#[test]
fn non_utf_parent_and_filename_publish_capture_and_restore_idempotently() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let parent = temp.path().join(unusual_name("-parent"));
    if let Err(error) = fs::create_dir(&parent) {
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(92) {
            assert!(!parent.exists());
            eprintln!(
                "non-UTF filesystem probe rejected before export: EILSEQ92; byte-wire custody remains tested"
            );
            return Ok(());
        }
        return Err(error.into());
    }
    let destination = parent.join(unusual_name(".xmp"));
    fs::write(&destination, b"old retained packet")?;
    let plan = plan_export(&destination, b"new packet")?;
    let saved = serde_json::to_string(&plan)?;
    let plan: ExportPlan = serde_json::from_str(&saved)?;
    let receipt = apply_export(&plan, b"new packet")?;
    assert_eq!(receipt.state, ExportState::Published);
    assert_eq!(
        recover_export(&receipt.recovery_directory)?.state,
        ExportState::Published
    );
    assert_eq!(
        fs::read(receipt.captured_original.as_ref().unwrap())?,
        b"old retained packet"
    );
    assert_eq!(fs::read(&destination)?, b"new packet");
    let second = parent.join(unusual_name("-restore.xmp"));
    fs::write(&second, b"captured")?;
    let plan = plan_export(&second, b"never publish")?;
    let receipt = apply_export_with_hook(&plan, b"never publish", |boundary| {
        if matches!(
            boundary,
            ExportBoundary::Captured | ExportBoundary::BeforeRestore
        ) {
            Err(std::io::Error::other(
                "retain captured original for explicit restore",
            ))
        } else {
            Ok(())
        }
    })?;
    assert_eq!(receipt.state, ExportState::Recoverable);
    assert!(!second.exists());
    assert_eq!(
        fs::read(receipt.captured_original.as_ref().unwrap())?,
        b"captured"
    );
    let restored = restore_planned_export(&plan)?;
    assert_eq!(restored.state, ExportState::Restored);
    assert_eq!(fs::read(&second)?, b"captured");
    let repeated = restore_planned_export(&plan)?;
    assert!(matches!(
        repeated.state,
        ExportState::Restored | ExportState::Conflict
    ));
    assert_eq!(
        fs::read_to_string(receipt.recovery_directory.join("plan.json"))?,
        serde_json::to_string(&plan)?
    );
    Ok(())
}

#[test]
fn oversized_native_path_receipt_is_rejected_before_filesystem_mutation() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let mut plan = plan_export(&temp.path().join("normal.xmp"), b"new")?;
    plan.destination = temp.path().join("x".repeat(24000)).join("photo.xmp");
    let error = apply_export(&plan, b"new").unwrap_err().to_string();
    assert!(error.contains("receipt budget"), "{error}");
    assert_eq!(fs::read_dir(temp.path())?.count(), 0);
    Ok(())
}

#[test]
fn snapshot_and_seal_versions_preserve_native_and_legacy_shapes() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let snapshot = snapshot_photo_destination(&temp.path().join("photo.png"), 1024)?;
    let native = serde_json::to_value(&snapshot)?;
    assert_eq!(native["version"], 2);
    assert!(native["destination"].is_object());
    for version in [0, 1, 3, 999] {
        let mut bad = native.clone();
        bad["version"] = version.into();
        assert!(serde_json::from_value::<DestinationSnapshot>(bad).is_err());
    }
    let stage = temp.path().join("stage");
    fs::write(&stage, b"encoded")?;
    let seal = seal_photo_export(&snapshot, &stage, 1024, &"a".repeat(64), |_| Ok(()))?;
    let json = serde_json::to_value(&seal)?;
    assert_eq!(json["version"], 2);
    for version in [0, 1, 3] {
        let mut bad = json.clone();
        bad["version"] = version.into();
        assert!(serde_json::from_value::<SealedPhotoExport>(bad).is_err());
    }
    let mut bad = seal.clone();
    bad.version = 99;
    assert!(publish_photo_export(&bad).is_err());
    assert!(!snapshot.destination.exists());
    let mut old = snapshot_photo_destination(&temp.path().join("old.png"), 1024)?;
    old.version = 1;
    let legacy = serde_json::to_value(&old)?;
    assert!(legacy["destination"].is_string());
    let old_seal = seal_photo_export(&old, &stage, 1024, &"b".repeat(64), |_| Ok(()))?;
    assert_eq!(old_seal.version, 1);
    assert_eq!(
        publish_photo_export(&old_seal)?.state,
        ExportState::Published
    );
    assert!(
        fs::read_to_string(old_seal.recovery_directory().join("plan.json"))?
            .contains("\"version\":2")
    );
    Ok(())
}

#[test]
fn explicit_restore_starts_from_retained_capture_not_automatic_rollback() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().join("restore.xmp");
    fs::write(&destination, b"captured")?;
    let plan = plan_export(&destination, b"never publish")?;
    let receipt = apply_export_with_hook(&plan, b"never publish", |boundary| {
        if matches!(
            boundary,
            ExportBoundary::Captured | ExportBoundary::BeforeRestore
        ) {
            Err(std::io::Error::other(
                "retain captured original for explicit restore",
            ))
        } else {
            Ok(())
        }
    })?;
    assert_eq!(receipt.state, ExportState::Recoverable);
    assert!(!destination.exists());
    assert_eq!(
        fs::read(receipt.captured_original.as_ref().unwrap())?,
        b"captured"
    );
    assert_eq!(restore_planned_export(&plan)?.state, ExportState::Restored);
    assert_eq!(fs::read(&destination)?, b"captured");
    Ok(())
}
