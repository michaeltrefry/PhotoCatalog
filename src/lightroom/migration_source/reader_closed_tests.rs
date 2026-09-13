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
fn closed_roster_wrong_connection_rejects_before_pragma_and_prelock_writer_invalidates()
-> Result<()> {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let statements = Arc::new(AtomicUsize::new(0));
    let observed = statements.clone();
    let wrong = MigrationSource::open_ordered(
        fixture.seal.clone(),
        ReadLimits::default(),
        Arc::new(AtomicBool::new(false)),
        Some(&[]),
        |_| {
            let db = immutable(&other.path)?;
            db.authorizer(Some(move |_: rusqlite::hooks::AuthContext<'_>| {
                observed.fetch_add(1, Ordering::SeqCst);
                rusqlite::hooks::Authorization::Allow
            }))?;
            Ok(db)
        },
    );
    assert!(wrong.is_err());
    assert_eq!(statements.load(Ordering::SeqCst), 0);
    let changed = MigrationSource::open_ordered(
        fixture.seal.clone(),
        ReadLimits::default(),
        Arc::new(AtomicBool::new(false)),
        Some(&[]),
        |path| {
            // The admitted Source still holds its original Revision, but has
            // acquired no custom lock yet. This write must not become baseline.
            let writer = Connection::open(path)?;
            writer.execute(
                "UPDATE captures SET evidence_revision=evidence_revision+1",
                [],
            )?;
            drop(writer);
            immutable(path)
        },
    );
    assert!(changed.is_err());
    Ok(())
}
#[test]
fn closed_roster_protected_alias_and_cancel_reject_before_opening_sql() -> Result<()> {
    let fixture = Fixture::new();
    let file = fs::File::open(&fixture.path)?;
    let key = crate::lightroom_migration_worker::identity::FileKey::of(&file)?;
    drop(file);
    let calls = Flag::new(0);
    for cancel in [false, true] {
        let result = MigrationSource::open_ordered(
            fixture.seal.clone(),
            ReadLimits::default(),
            Arc::new(AtomicBool::new(cancel)),
            Some(std::slice::from_ref(&key)),
            |path| {
                calls.set(calls.get() + 1);
                immutable(path)
            },
        );
        assert!(result.is_err());
    }
    assert_eq!(calls.get(), 0);
    let source = MigrationSource::open_closed_roster(
        fixture.seal.clone(),
        ReadLimits::default(),
        Arc::new(AtomicBool::new(false)),
        &[],
    )?;
    assert_eq!(source.count(fixture.revision(), Collection::Rows)?, 1);
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!Path::new(&format!("{}{suffix}", fixture.path.display())).exists());
    }
    Ok(())
}
#[test]
fn resolved_ids_admit_bytes_before_materialization_and_preserve_ambiguity() -> Result<()> {
    let mut fixture = Fixture::new();
    let revision = fixture.revision().to_owned();
    let oversized = "é".repeat(2049);
    fixture.edit(|db| {
        db.execute(
            "INSERT INTO entities VALUES(?1,?2,'AgLibraryFile','key',NULL,'{}')",
            params![revision, oversized],
        )
        .unwrap();
        db.execute(
            "INSERT INTO entities VALUES(?,'image','Adobe_images',NULL,NULL,'{}')",
            [&revision],
        )
        .unwrap();
        db.execute(
            "INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?,'image','rootFile','AgLibraryFile','key')",
            [&revision],
        )
        .unwrap();
    });
    let source = fixture.open();
    let error = source
        .resolve(&revision, "image", "rootFile", "AgLibraryFile")
        .unwrap_err();
    assert!(format!("{error:#}").contains("4096 bytes"));
    drop(source);
    assert!(
        !fixture
            .open()
            .page(&revision, Collection::Entities, None, 10)?
            .records
            .is_empty()
    );
    // Two actual rows remain Ambiguous even if neither can be a bounded ID.
    fixture.edit(|db| {
        db.execute(
            "INSERT INTO entities VALUES(?1,?2,'AgLibraryFile','key',NULL,'{}')",
            params![revision, "界".repeat(2049)],
        )
        .unwrap();
    });
    assert_eq!(
        fixture
            .open()
            .resolve(&revision, "image", "rootFile", "AgLibraryFile")?,
        Resolution::Ambiguous
    );
    fixture.edit(|db| {
        db.execute("DELETE FROM entities WHERE table_name='AgLibraryFile'", [])
            .unwrap();
    });
    assert_eq!(
        fixture
            .open()
            .resolve(&revision, "image", "rootFile", "AgLibraryFile")?,
        Resolution::Missing
    );
    fixture.edit(|db| {
        db.execute(
            "INSERT INTO entities VALUES(?1,'file','AgLibraryFile','key',NULL,'{}')",
            [&revision],
        )
        .unwrap();
    });
    let source = fixture.open();
    assert_eq!(
        source.resolve(&revision, "image", "rootFile", "AgLibraryFile")?,
        Resolution::Unique("file".into())
    );
    assert_eq!(
        source.image_links(&revision, "image")?.file,
        Resolution::Unique("file".into())
    );
    assert!(source.image_links(&revision, &"é".repeat(2049)).is_err());
    assert!(source.image_links(&revision, "").is_err());
    drop(source);
    fixture.edit(|db| {
        db.execute(
            "UPDATE entities SET source_id=x'66696c65' WHERE source_id='file'",
            [],
        )
        .unwrap();
    });
    assert!(
        fixture
            .open()
            .resolve(&revision, "image", "rootFile", "AgLibraryFile")
            .is_err()
    );
    Ok(())
}
