use anyhow::Result;
use photocatalog::metadata_export::*;
use std::fs;

#[test]
fn held_proof_rejects_restored_mtime_replacement_and_bounded_scan_mutation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("ordinary-file");
    fs::write(&path, b"original")?;
    assert!(VerifiedFile::read(&path, 7).is_err());
    let proof = VerifiedFile::read(&path, 8)?;
    let mtime = fs::metadata(&path)?.modified()?;
    fs::write(&path, b"modified")?;
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_times(fs::FileTimes::new().set_modified(mtime))?;
    assert!(proof.recheck().is_err());
    drop(proof);
    let proof = VerifiedFile::read(&path, 8)?;
    fs::rename(&path, root.path().join("held-old-object"))?;
    fs::write(&path, b"modified")?;
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_times(fs::FileTimes::new().set_modified(mtime))?;
    assert!(proof.recheck().is_err());
    drop(proof);
    let mut changed = false;
    assert!(
        VerifiedFile::read_with_checkpoint(&path, 8, &mut |_| {
            if !changed {
                fs::write(&path, b"external")?;
                changed = true;
            }
            Ok(())
        })
        .is_err()
    );
    assert_eq!(fs::read(path)?, b"external");
    Ok(())
}

#[test]
fn phased_lease_and_destination_proof_preserve_a_concurrent_replacement() -> Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().join("output.jpg");
    let input = root.path().join("encoded");
    fs::write(&destination, b"old destination")?;
    fs::write(&input, b"encoded bytes")?;
    let snapshot = snapshot_photo_destination(&destination, 1024)?;
    let authority = blake3::hash(b"frozen-job-plan").to_hex().to_string();
    let seal = seal_photo_export(&snapshot, &input, 1024, &authority, |_| Ok(()))?;
    let mut publication = PhotoPublication::prepare(&seal)?;
    assert!(PhotoPublication::prepare(&seal).is_err());
    assert!(publish_photo_export(&seal).is_err());
    fs::rename(&destination, root.path().join("old-retained"))?;
    fs::write(&destination, b"external replacement")?;
    assert!(publication.capture().is_err());
    assert_eq!(fs::read(&destination)?, b"external replacement");
    assert!(!seal.recovery_directory().join("original").exists());
    assert_eq!(fs::read(input)?, b"encoded bytes");
    Ok(())
}
