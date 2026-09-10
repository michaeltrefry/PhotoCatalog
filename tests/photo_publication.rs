use photocatalog::metadata_export::*;
use std::{
    fs,
    io::{self, Seek, SeekFrom, Write},
    path::Path,
};

fn authority() -> String {
    blake3::hash(b"immutable recipe/metadata/source/destination job")
        .to_hex()
        .to_string()
}
fn cancel() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "canceled job")
}
fn sealed(root: &Path, destination: &Path) -> anyhow::Result<SealedPhotoExport> {
    let input = root.join("completed-encoder-file");
    fs::write(&input, b"final pixels")?;
    let snapshot = snapshot_photo_destination(destination, 1024)?;
    seal_photo_export(&snapshot, &input, 1024, &authority(), |_| Ok(()))
}

#[test]
fn queue_is_read_only_and_photo_extensions_do_not_widen_xmp() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    for extension in ["jpg", "JPEG", "pNg", "TIF", "tiff"] {
        let path = root.path().join(format!("output.{extension}"));
        let snapshot = snapshot_photo_destination(&path, 0)?;
        assert!(snapshot.expected.is_none());
        assert!(plan_export(&path, b"XMP").is_err());
    }
    assert_eq!(fs::read_dir(root.path())?.count(), 0);
    assert!(snapshot_photo_destination(&root.path().join("original.CR2"), 1024).is_err());
    assert!(snapshot_photo_destination(&root.path().join("metadata.xmp"), 1024).is_err());
    assert_eq!(
        plan_export(&root.path().join("metadata.XmP"), b"packet")?.version,
        1
    );
    Ok(())
}

#[test]
fn final_seekback_bytes_are_sealed_with_fixed_buffer_and_separate_old_file_limit()
-> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("encoder");
    let destination = root.path().join("output.TIFF");
    let mut encoder = fs::File::create(&input)?;
    let block = [0x5a; 64 * 1024];
    let mut expected = blake3::Hasher::new();
    // 2 MiB streamed, then a TIFF-like header rewrite without changing extent.
    for _ in 0..32 {
        encoder.write_all(&block)?;
    }
    encoder.seek(SeekFrom::Start(0))?;
    encoder.write_all(b"finalhdr")?;
    encoder.sync_all()?;
    drop(encoder);
    expected.update(b"finalhdr");
    expected.update(&block[8..]);
    for _ in 1..32 {
        expected.update(&block);
    }
    // Prior destination can exceed the new payload ceiling, with its own bound.
    let old = fs::File::create(&destination)?;
    old.set_len(3 * 1024 * 1024)?;
    drop(old);
    assert!(snapshot_photo_destination(&destination, 2 * 1024 * 1024).is_err());
    let snapshot = snapshot_photo_destination(&destination, 3 * 1024 * 1024)?;
    let input_before = inspect_file_revision(&input, 2 * 1024 * 1024)?;
    let seal = seal_photo_export(&snapshot, &input, 2 * 1024 * 1024, &authority(), |_| Ok(()))?;
    assert_eq!(seal.payload.bytes, 2 * 1024 * 1024);
    assert_eq!(
        seal.payload.digest,
        expected.finalize().to_hex().to_string()
    );
    assert_eq!(read_photo_seal(&snapshot, &authority())?, seal);
    let receipt = publish_photo_export(&seal)?;
    assert_eq!(receipt.state, ExportState::Published);
    assert_eq!(
        inspect_file_revision(&destination, 2 * 1024 * 1024)?,
        seal.payload
    );
    assert_eq!(
        fs::metadata(receipt.captured_original.unwrap())?.len(),
        3 * 1024 * 1024
    );
    assert_eq!(
        inspect_file_revision(&input, 2 * 1024 * 1024)?,
        input_before
    );
    assert_eq!(recover_photo_export(&seal)?.state, ExportState::Published);
    assert!(recover_export(&seal.recovery_directory()).is_err());
    let internal: ExportPlan =
        serde_json::from_slice(&fs::read(seal.recovery_directory().join("plan.json"))?)?;
    assert!(apply_export(&internal, b"final pixels").is_err());
    assert!(restore_planned_export(&internal).is_err());
    Ok(())
}

#[test]
fn limits_cancellation_and_low_disk_never_seal_partial_bytes() -> anyhow::Result<()> {
    for failure in ["limit", "cancel", "disk"] {
        let root = tempfile::tempdir()?;
        let input = root.path().join("encoder");
        let mut file = fs::File::create(&input)?;
        file.write_all(&[7; 128 * 1024])?;
        drop(file);
        let original = inspect_file_revision(&input, 128 * 1024)?;
        let destination = root.path().join("out.png");
        fs::write(&destination, b"old")?;
        let snapshot = snapshot_photo_destination(&destination, 1024)?;
        let result = seal_photo_export(
            &snapshot,
            &input,
            if failure == "limit" {
                64 * 1024
            } else {
                128 * 1024
            },
            &authority(),
            |bytes| {
                if bytes > 0 {
                    if failure == "cancel" {
                        return Err(cancel());
                    }
                    if failure == "disk" {
                        return Err(io::Error::new(
                            io::ErrorKind::StorageFull,
                            "injected write failure",
                        ));
                    }
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(read_photo_seal(&snapshot, &authority()).is_err());
        assert_eq!(fs::read(&destination)?, b"old");
        assert_eq!(inspect_file_revision(&input, 128 * 1024)?, original);
        let paths = discover_photo_exports(root.path(), 20)?;
        assert_eq!(paths.len(), 1);
        assert!(
            paths[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("-preparing-")
        );
        assert!(recover_export(&paths[0]).is_err());
    }
    Ok(())
}

#[test]
fn canceled_lost_return_requires_explicit_seal_reconciliation_not_discovery_publication()
-> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().join("out.jpg");
    let input = root.path().join("encoder");
    fs::write(&input, b"complete")?;
    let snapshot = snapshot_photo_destination(&destination, 0)?;
    let directory = root
        .path()
        .canonicalize()?
        .join(format!(".photocatalog-photo-export-{}", snapshot.operation));
    assert!(
        seal_photo_export(&snapshot, &input, 100, &authority(), |_| {
            if directory.exists() {
                Err(cancel())
            } else {
                Ok(())
            }
        })
        .is_err()
    );
    assert!(!destination.exists());
    assert!(recover_export(&directory).is_err());
    let seal = read_photo_seal(&snapshot, &authority())?;
    assert!(!destination.exists());
    assert_ne!(restore_photo_export(&seal)?.state, ExportState::Published);
    assert!(!destination.exists());
    // Only the parent may decide to accept a recovered seal into its durable job
    // lifecycle. This test explicitly grants publication after that reconciliation.
    assert_eq!(publish_photo_export(&seal)?.state, ExportState::Published);
    Ok(())
}

#[test]
fn changed_destination_and_absent_races_preserve_external_bytes() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().join("out.jpg");
    fs::write(&destination, b"old")?;
    let seal = sealed(root.path(), &destination)?;
    fs::write(&destination, b"changed externally")?;
    assert_eq!(publish_photo_export(&seal)?.state, ExportState::Conflict);
    assert_eq!(fs::read(&destination)?, b"changed externally");
    let destination = root.path().join("absent.png");
    let seal = sealed(root.path(), &destination)?;
    let receipt = publish_photo_export_with_hook(&seal, |phase| {
        if phase == ExportBoundary::BeforePublish {
            fs::write(&destination, b"third party")?;
        }
        Ok(())
    })?;
    assert_eq!(receipt.state, ExportState::Conflict);
    assert_eq!(fs::read(&destination)?, b"third party");
    Ok(())
}

#[test]
fn capture_race_restores_large_changed_bytes_without_unbounded_hashing() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().join("out.tif");
    fs::write(&destination, b"old")?;
    let seal = sealed(root.path(), &destination)?;
    let receipt = publish_photo_export_with_hook(&seal, |phase| {
        if phase == ExportBoundary::BeforeCapture {
            fs::write(&destination, [9; 4096])?;
        }
        Ok(())
    })?;
    assert_eq!(receipt.state, ExportState::Restored);
    assert_eq!(fs::read(&destination)?, [9; 4096]);
    assert_eq!(fs::read(receipt.captured_original.unwrap())?, [9; 4096]);
    Ok(())
}

#[test]
fn interrupted_capture_and_failed_restore_keep_both_revisions() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().join("out.png");
    fs::write(&destination, b"old")?;
    let seal = sealed(root.path(), &destination)?;
    let receipt = publish_photo_export_with_hook(&seal, |phase| {
        if phase == ExportBoundary::Captured {
            fs::write(&destination, b"external")?;
            return Err(cancel());
        }
        Ok(())
    })?;
    assert_eq!(receipt.state, ExportState::Conflict);
    assert_eq!(fs::read(&destination)?, b"external");
    assert_eq!(fs::read(receipt.captured_original.unwrap())?, b"old");
    assert_eq!(restore_photo_export(&seal)?.state, ExportState::Conflict);
    assert_eq!(fs::read(&destination)?, b"external");
    Ok(())
}

#[test]
fn altered_authority_journal_or_payload_cannot_be_published() -> anyhow::Result<()> {
    for corruption in ["authority", "snapshot", "journal", "payload"] {
        let root = tempfile::tempdir()?;
        let destination = root.path().join("out.png");
        let mut seal = sealed(root.path(), &destination)?;
        match corruption {
            "authority" => {
                seal.authority_digest = blake3::hash(b"another job").to_hex().to_string()
            }
            "snapshot" => seal.snapshot.max_existing_bytes += 1,
            "journal" => fs::write(seal.recovery_directory().join("photo-seal.json"), b"{}")?,
            "payload" => fs::write(seal.recovery_directory().join("payload"), b"corruption")?,
            _ => unreachable!(),
        }
        let result = publish_photo_export(&seal);
        assert!(result.is_err() || result.unwrap().state != ExportState::Published);
        assert!(!destination.exists());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlink_inputs_destinations_and_recovery_directories_are_refused() -> anyhow::Result<()> {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir()?;
    let original = root.path().join("source.CR2");
    fs::write(&original, b"original")?;
    let destination = root.path().join("out.png");
    symlink(&original, &destination)?;
    assert!(snapshot_photo_destination(&destination, 100).is_err());
    let snapshot = snapshot_photo_destination(&root.path().join("other.png"), 0)?;
    assert!(seal_photo_export(&snapshot, &destination, 100, &authority(), |_| Ok(())).is_err());
    assert_eq!(fs::read(&original)?, b"original");
    let seal = sealed(root.path(), &root.path().join("third.png"))?;
    let directory = seal.recovery_directory();
    let retained = root.path().join("retained");
    fs::rename(&directory, &retained)?;
    symlink(&retained, &directory)?;
    assert!(read_photo_seal(&seal.snapshot, &authority()).is_err());
    assert!(publish_photo_export(&seal).is_err());
    Ok(())
}

#[test]
fn bounded_discovery_reports_incomplete_instead_of_silent_truncation() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("a"), b"")?;
    fs::write(root.path().join("b"), b"")?;
    assert!(
        discover_photo_exports(root.path(), 1)
            .unwrap_err()
            .to_string()
            .contains("incomplete")
    );
    assert!(discover_photo_exports(root.path(), 2)?.is_empty());
    Ok(())
}

#[test]
fn photo_crash_worker() -> anyhow::Result<()> {
    let Some(root) = std::env::var_os("PHOTOCATALOG_PHOTO_CRASH_ROOT") else {
        return Ok(());
    };
    let root = Path::new(&root);
    let seal: SealedPhotoExport = serde_json::from_slice(&fs::read(root.join("authority.json"))?)?;
    let boundary = std::env::var("PHOTOCATALOG_PHOTO_CRASH_BOUNDARY")?;
    publish_photo_export_with_hook(&seal, |phase| {
        if format!("{phase:?}") == boundary {
            std::process::exit(73);
        }
        Ok(())
    })?;
    anyhow::bail!("crash boundary not reached")
}

#[test]
fn actual_process_exit_recovers_only_with_original_sealed_authority() -> anyhow::Result<()> {
    for boundary in ["Prepared", "Captured", "BeforePublish", "Published"] {
        let root = tempfile::tempdir()?;
        let destination = root.path().join("out.jpg");
        fs::write(&destination, b"old")?;
        let seal = sealed(root.path(), &destination)?;
        fs::write(
            root.path().join("authority.json"),
            serde_json::to_vec(&seal)?,
        )?;
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "photo_crash_worker", "--nocapture"])
            .env("PHOTOCATALOG_PHOTO_CRASH_ROOT", root.path())
            .env("PHOTOCATALOG_PHOTO_CRASH_BOUNDARY", boundary)
            .status()?;
        assert_eq!(status.code(), Some(73));
        assert!(recover_export(&seal.recovery_directory()).is_err());
        assert_eq!(recover_photo_export(&seal)?.state, ExportState::Published);
        assert_eq!(fs::read(&destination)?, b"final pixels");
        assert_eq!(
            fs::read(seal.recovery_directory().join("original"))?,
            b"old"
        );
    }
    Ok(())
}

#[test]
fn encoder_file_growth_is_detected_and_never_installed() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("encoder");
    fs::write(&input, [3; 128 * 1024])?;
    let destination = root.path().join("out.jpg");
    let snapshot = snapshot_photo_destination(&destination, 0)?;
    let mut grew = false;
    let result = seal_photo_export(&snapshot, &input, 256 * 1024, &authority(), |bytes| {
        if bytes == 64 * 1024 && !grew {
            fs::OpenOptions::new()
                .append(true)
                .open(&input)?
                .write_all(b"concurrent bytes")?;
            grew = true;
        }
        Ok(())
    });
    assert!(grew);
    assert!(result.is_err());
    assert!(!destination.exists());
    assert!(read_photo_seal(&snapshot, &authority()).is_err());
    Ok(())
}

#[test]
fn lost_capture_can_restore_even_when_sealed_payload_is_corrupt() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().join("out.jpg");
    fs::write(&destination, b"old")?;
    let seal = sealed(root.path(), &destination)?;
    let receipt = publish_photo_export_with_hook(&seal, |phase| {
        if matches!(
            phase,
            ExportBoundary::Captured | ExportBoundary::BeforeRestore
        ) {
            return Err(cancel());
        }
        Ok(())
    })?;
    assert_eq!(receipt.state, ExportState::Recoverable);
    assert!(!destination.exists());
    fs::write(seal.recovery_directory().join("payload"), b"bad")?;
    assert_eq!(restore_photo_export(&seal)?.state, ExportState::Restored);
    assert_eq!(fs::read(&destination)?, b"old");
    Ok(())
}

#[test]
fn concurrent_sealers_cannot_replace_the_same_operation() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("encoder");
    let destination = root.path().join("out.jpg");
    fs::write(&input, b"final")?;
    let snapshot = snapshot_photo_destination(&destination, 0)?;
    let barrier = std::sync::Barrier::new(2);
    let mut successes = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    // Both preparations pass the initial final-directory absence check.
                    let mut first = true;
                    seal_photo_export(&snapshot, &input, 100, &authority(), |_| {
                        if first {
                            first = false;
                            barrier.wait();
                        }
                        Ok(())
                    })
                })
            })
            .collect();
        for handle in handles {
            if let Ok(seal) = handle.join().unwrap() {
                successes.push(seal);
            }
        }
    });
    assert_eq!(successes.len(), 1);
    assert_eq!(read_photo_seal(&snapshot, &authority())?, successes[0]);
    assert!(!destination.exists());
    assert_eq!(
        publish_photo_export(&successes[0])?.state,
        ExportState::Published
    );
    assert_eq!(discover_photo_exports(root.path(), 20)?.len(), 2);
    Ok(())
}
