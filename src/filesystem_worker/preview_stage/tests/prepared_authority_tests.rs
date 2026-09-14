use super::*;

fn valid_receipt(bytes: &[u8]) -> crate::edit::PreparedProxyReceipt {
    let mut r = receipt();
    r.bytes = bytes.len() as u64;
    r.blake3 = blake3::hash(bytes).to_hex().to_string();
    r
}
fn adopt(
    f: &mut Fixture,
    stage: &LeaseId,
    key: &str,
    receipt: crate::edit::PreparedProxyReceipt,
) -> Result<Value> {
    f.call(
        Action::PreparedAdopt {
            stage: stage.clone(),
            key: key.into(),
            receipt,
        },
        false,
    )
}

// F validates opaque byte identity. Native container and pixel validation belong
// to the separate actual managed prepared-cache route fixture.
#[test]
fn prepared_adoption_checks_drain_receipt_and_existing_destination_without_overwrite() -> Result<()>
{
    let mut f = Fixture::new()?;
    f.call(Action::PreparedInitialize, false)?;
    let stage = f.admit()?;
    let source = f.path(&stage).join("prepared.linear");
    let bytes = b"opaque prepared output";
    fs::write(&source, bytes)?;
    let r = valid_receipt(bytes);
    let key = "a".repeat(64);
    let destination = f.manifest.join("prepared").join(format!("{key}.linear"));
    f.arm(&stage, 1)?;
    assert!(adopt(&mut f, &stage, &key, r.clone()).is_err());
    assert!(!destination.exists());
    f.drain(&stage, 1)?;

    for wrong in [
        crate::edit::PreparedProxyReceipt {
            bytes: r.bytes + 1,
            ..r.clone()
        },
        crate::edit::PreparedProxyReceipt {
            blake3: "d".repeat(64),
            ..r.clone()
        },
    ] {
        assert!(adopt(&mut f, &stage, &key, wrong).is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read(&source)?, bytes);
    }
    // A matching pre-existing regular file is accepted without replacing it.
    fs::write(&destination, bytes)?;
    let before = crate::catalog_storage::physical_object_id(&File::open(&destination)?)?;
    let value = adopt(&mut f, &stage, &key, r.clone())?;
    assert!(matches!(value, Value::Path(p) if p == NativePath::from_path(&destination)));
    assert_eq!(
        crate::catalog_storage::physical_object_id(&File::open(&destination)?)?,
        before
    );
    assert_eq!(fs::read(&source)?, bytes);

    // A mismatching existing target must never be overwritten or removed.
    let wrong_key = "b".repeat(64);
    let wrong = f
        .manifest
        .join("prepared")
        .join(format!("{wrong_key}.linear"));
    let foreign = b"different prepared file";
    fs::write(&wrong, foreign)?;
    let before = crate::catalog_storage::physical_object_id(&File::open(&wrong)?)?;
    assert!(adopt(&mut f, &stage, &wrong_key, r.clone()).is_err());
    assert_eq!(fs::read(&wrong)?, foreign);
    assert_eq!(
        crate::catalog_storage::physical_object_id(&File::open(&wrong)?)?,
        before
    );
    fs::remove_file(&wrong)?;
    adopt(&mut f, &stage, &wrong_key, r)?;
    assert_eq!(fs::read(&wrong)?, bytes);
    f.call(Action::Release { stage }, false)?;
    assert!(f.owner.empty());
    assert_eq!(fs::read(&destination)?, bytes);
    assert_eq!(fs::read(&wrong)?, bytes);
    f.call(Action::PreparedRemove { key }, false)?;
    f.call(Action::PreparedRemove { key: wrong_key }, false)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn prepared_adoption_rejects_symlink_target_and_source_without_touching_foreign_bytes() -> Result<()>
{
    let mut f = Fixture::new()?;
    f.call(Action::PreparedInitialize, false)?;
    let stage = f.admit()?;
    let source = f.path(&stage).join("prepared.linear");
    let bytes = b"opaque prepared output";
    fs::write(&source, bytes)?;
    let r = valid_receipt(bytes);
    f.arm(&stage, 1)?;
    f.drain(&stage, 1)?;
    let key = "a".repeat(64);
    let destination = f.manifest.join("prepared").join(format!("{key}.linear"));
    let foreign = f.manifest.join("foreign.linear");
    fs::write(&foreign, bytes)?;
    std::os::unix::fs::symlink(&foreign, &destination)?;
    assert!(adopt(&mut f, &stage, &key, r.clone()).is_err());
    assert!(fs::symlink_metadata(&destination)?.file_type().is_symlink());
    assert_eq!(fs::read(&foreign)?, bytes);
    // Removal unlinks only the catalog entry, preserving its foreign target.
    f.call(Action::PreparedRemove { key: key.clone() }, false)?;
    assert_eq!(fs::read(&foreign)?, bytes);
    fs::remove_file(&source)?;
    std::os::unix::fs::symlink(&foreign, &source)?;
    assert!(adopt(&mut f, &stage, &key, r).is_err());
    assert!(!destination.exists());
    assert_eq!(fs::read(&foreign)?, bytes);
    fs::remove_file(&source)?;
    f.call(Action::Release { stage }, false)?;
    assert!(f.owner.empty());
    Ok(())
}

#[test]
fn prepared_recovery_checks_entire_bounded_roster_before_removal() -> Result<()> {
    let mut f = Fixture::new()?;
    let root = f.manifest.join("prepared");
    fs::create_dir(&root)?;
    for i in 0..1025 {
        fs::write(root.join(format!("{i:064x}.linear")), b"x")?;
    }
    assert!(f.call(Action::PreparedInitialize, false).is_err());
    assert_eq!(fs::read_dir(&root)?.count(), 1025);
    fs::remove_file(root.join(format!("{:064x}.linear", 1024)))?;
    // Keep the supported prepared-cache capacity of 1024; worker-stage recovery
    // has a separate 128-entry batch, not the prepared-cache capacity.
    f.call(Action::PreparedInitialize, false)?;
    assert_eq!(fs::read_dir(&root)?.count(), 0);

    let mut unknown = Fixture::new()?;
    let root = unknown.manifest.join("prepared");
    fs::create_dir(&root)?;
    let recognized = root.join(format!("{}.linear", "a".repeat(64)));
    fs::write(&recognized, b"retained")?;
    let foreign = root.join("unknown.keep");
    fs::write(&foreign, b"unknown")?;
    assert!(unknown.call(Action::PreparedInitialize, false).is_err());
    assert_eq!(fs::read(&recognized)?, b"retained");
    assert_eq!(fs::read(&foreign)?, b"unknown");
    fs::remove_file(&foreign)?;
    unknown.call(Action::PreparedInitialize, false)?;
    assert!(!recognized.exists());
    Ok(())
}
