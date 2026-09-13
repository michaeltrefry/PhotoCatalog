use super::*;
use crate::lightroom::migration_source::tests::Fixture;
use std::sync::atomic::AtomicUsize;

fn immutable(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        plan::uri(path)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

#[test]
fn wrong_opened_source_is_rejected_before_any_sql_admission() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let statements = Arc::new(AtomicUsize::new(0));
    let observed = statements.clone();
    let result = MigrationSource::open_with_connection(
        fixture.seal.clone(),
        ReadLimits::default(),
        Arc::new(AtomicBool::new(false)),
        |_| {
            let db = immutable(&other.path)?;
            db.authorizer(Some(move |_: rusqlite::hooks::AuthContext<'_>| {
                observed.fetch_add(1, Ordering::SeqCst);
                rusqlite::hooks::Authorization::Allow
            }))?;
            Ok(db)
        },
    );
    let error = result.err().expect("different opened source must reject");
    assert!(format!("{error:#}").contains("sealed inspection opened object"));
    assert_eq!(statements.load(Ordering::SeqCst), 0);
}

#[test]
fn later_opened_source_mismatch_poison_is_permanent() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let mut source = fixture.open();
    assert_eq!(
        source.count(fixture.revision(), Collection::Rows).unwrap(),
        1
    );
    let original = std::mem::replace(&mut source.db, immutable(&other.path).unwrap());
    let error = source
        .count(fixture.revision(), Collection::Rows)
        .unwrap_err();
    assert!(format!("{error:#}").contains("SQLite selected database object changed"));
    source.db = original;
    let error = source
        .count(fixture.revision(), Collection::Rows)
        .unwrap_err();
    assert!(format!("{error:#}").contains("inspection source was previously invalidated"));
}

#[cfg(unix)]
#[test]
fn directory_aba_cannot_substitute_a_byte_identical_sqlite_source() {
    let fixture = Fixture::new();
    let root = fixture.path.parent().unwrap();
    let parked = root.with_file_name("parked");
    let substitute = root.with_file_name("substitute");
    fs::create_dir(&substitute).unwrap();
    fs::copy(&fixture.path, substitute.join("inspection.sqlite3")).unwrap();
    let source_bytes = fs::read(&fixture.path).unwrap();
    let result = MigrationSource::open_with_connection(
        fixture.seal.clone(),
        ReadLimits::default(),
        Arc::new(AtomicBool::new(false)),
        |path| {
            // Directory renames do not change the held database's inode/ctime.
            // Both file bytes and all restored pathname observations still match A.
            fs::rename(root, &parked)?;
            fs::rename(&substitute, root)?;
            let result = immutable(path);
            fs::rename(root, &substitute)?;
            fs::rename(&parked, root)?;
            result
        },
    );
    let error = result
        .err()
        .expect("opened B must reject after restoring A");
    assert!(format!("{error:#}").contains("sealed inspection opened object"));
    let held = Source::open(&fixture.path, u64::MAX).unwrap();
    assert_eq!(held.before, fixture.seal.identity);
    assert_eq!(fs::read(&fixture.path).unwrap(), source_bytes);
}
