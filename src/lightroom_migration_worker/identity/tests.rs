use super::*;
use crate::lightroom::source::Source;
#[test]
fn duplicate_source_poison_precedes_another_read_or_writer_grant() -> Result<()> {
    let t = tempfile::tempdir()?;
    let path = t.path().canonicalize()?.join("sealed.sqlite3");
    std::fs::write(&path, b"synthetic immutable source")?;
    let cancel = Arc::new(AtomicBool::new(false));
    let audit = Audit::new(cancel.clone(), vec![])?;
    let _scope = audit.install()?;
    let source = Source::open(&path, 1024)?;
    source.verify()?;
    assert!(Source::open(&path, 1024).is_err());
    assert!(audit.is_poisoned());
    assert!(cancel.load(Ordering::Acquire));
    assert!(source.verify().is_err());
    assert!(audit.writing(true).is_err());
    Ok(())
}
#[test]
fn source_destination_alias_and_unrelated_close_are_distinct() -> Result<()> {
    let t = tempfile::tempdir()?;
    let path = t.path().canonicalize()?.join("source");
    std::fs::write(&path, b"source")?;
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let _scope = audit.install()?;
    let source = Source::open(&path, 1024)?;
    let other = t.path().canonicalize()?.join("unrelated");
    std::fs::write(&other, b"other")?;
    drop(Source::open(&other, 1024)?);
    source.verify()?;
    assert!(!audit.is_poisoned());
    assert!(audit.open(&path, Role::Destination, false).is_err());
    assert!(audit.is_poisoned());
    assert!(source.verify().is_err());
    Ok(())
}
#[test]
fn protected_gui_object_never_becomes_destination_or_import_lock() -> Result<()> {
    let t = tempfile::tempdir()?;
    let path = t.path().canonicalize()?.join("inspection.sqlite3");
    std::fs::write(&path, b"inspection")?;
    let gui = File::open(&path)?;
    let key = FileKey::of(&gui)?;
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![key])?;
    assert!(audit.open(&path, Role::ImportLock, false).is_err());
    assert!(audit.is_poisoned());
    assert_eq!(std::fs::read(&path)?, b"inspection");
    Ok(())
}
#[test]
fn source_open_under_writer_is_rejected_before_path_open() -> Result<()> {
    let t = tempfile::tempdir()?;
    let nonexistent = t.path().canonicalize()?.join("does-not-exist");
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let _scope = audit.install()?;
    audit.writing(true)?;
    let error = match Source::open(&nonexistent, 1024) {
        Ok(_) => panic!("source opened under writer"),
        Err(e) => e,
    };
    assert!(error.to_string().contains("inside a granted writer"));
    assert!(audit.is_poisoned());
    audit.writing(false)?;
    Ok(())
}
