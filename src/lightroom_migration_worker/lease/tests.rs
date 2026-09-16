use super::*;
use std::sync::atomic::AtomicBool;
fn fixture() -> Result<(tempfile::TempDir, NativePath)> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("catalog");
    drop(Catalog::open(&root)?);
    let native = NativePath::from_path(&root.canonicalize()?);
    Ok((temp, native))
}
#[test]
fn existing_read_admission_does_not_create_import_lock() -> Result<()> {
    let (_temp, root) = fixture()?;
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let review = DestinationReview::existing(&root, None, &audit)?;
    assert!(!root.to_path()?.join(".lightroom-import.lock").exists());
    let db = review.read()?;
    assert!(db.is_autocommit());
    drop(db);
    assert!(
        DestinationLease::acquire(review, None, Instant::now() + Duration::from_secs(1)).is_err()
    );
    assert!(!root.to_path()?.join(".lightroom-import.lock").exists());
    Ok(())
}
#[test]
fn existing_lock_owner_outlives_borrowed_executor_connection() -> Result<()> {
    let (_temp, root) = fixture()?;
    let path = root.to_path()?;
    std::fs::write(path.join(".lightroom-import.lock"), b"")?;
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let review = DestinationReview::existing(&root, None, &audit)?;
    let lease = DestinationLease::acquire(review, None, Instant::now() + Duration::from_secs(1))?;
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.join(".lightroom-import.lock"))?;
    {
        let catalog = lease.open_current(Arc::new(Writers::default()))?;
        catalog.verify()?;
        assert!(contender.try_lock_exclusive().is_err());
    }
    assert!(contender.try_lock_exclusive().is_err());
    drop(lease);
    contender.try_lock_exclusive()?;
    Ok(())
}
#[test]
fn stale_database_pin_fails_before_lock_or_configuration() -> Result<()> {
    let (_temp, root) = fixture()?;
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let review = DestinationReview::existing(&root, None, &audit)?;
    let mut stale = review.pin.clone();
    stale.database_key.index.0 ^= 1;
    drop(review);
    assert!(DestinationReview::existing(&root, Some(&stale), &audit).is_err());
    assert!(audit.is_poisoned());
    assert!(!root.to_path()?.join(".lightroom-import.lock").exists());
    Ok(())
}
