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
        None,
        |_, _| {
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
        None,
        |path, _| {
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
            None,
            |path, _| {
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
        &mut |_| Ok(()),
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

#[test]
fn closed_roster_uri_normalizes_existing_deep_parents_after_original_admission() -> Result<()> {
    use crate::storage_volume::NativePath;
    let fixture = Fixture::new();
    let parent = fixture.path.parent().unwrap().to_path_buf();
    #[cfg(windows)]
    let parent = {
        // The fixture lives on a local drive. Exercise ordinary normalization;
        // retained verbatim paths deliberately keep their original spelling.
        let text = parent.to_str().context("representable synthetic path")?;
        std::path::PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))
    };
    let mut deep = parent.to_path_buf();
    let mut depth = 0usize;
    // Bundled SQLite's Unix VFS mxPathname is 512. Exercise the actual
    // overlong intermediate spelling, while the normalized database stays short.
    while deep.as_os_str().len() <= 512 {
        deep.push(format!("existing-original-prefix-{depth}"));
        fs::create_dir(&deep)?;
        depth += 1;
    }
    assert!(deep.as_os_str().len() > 512);
    let intermediate_bytes = deep.as_os_str().len();
    let mut original = deep;
    for _ in 0..depth {
        original.push("..");
    }
    original.push(fixture.path.file_name().unwrap());
    let mut seal = fixture.seal.clone();
    seal.database = NativePath::from_path(&original);
    let before = fs::read(&fixture.path)?;
    let mut charged = 0usize;
    let source = MigrationSource::open_closed_roster(
        seal,
        ReadLimits::default(),
        Arc::new(AtomicBool::new(false)),
        &[],
        &mut |n| {
            charged = charged.checked_add(n).unwrap();
            Ok(())
        },
    )?;
    assert_eq!(source.count(fixture.revision(), Collection::Rows)?, 1);
    assert!(charged > original.as_os_str().len());
    assert_eq!(source.guard.before, fixture.seal.identity);
    println!(
        "SOURCE_URI intermediate_bytes={intermediate_bytes} sqlite_unix_limit=512 charged={charged} exact_identity=true"
    );
    drop(source);
    assert_eq!(fs::read(&fixture.path)?, before);
    // A nonexistent component followed by '..' must not become an accepted
    // normalized source: original Source admission occurs first.
    let invalid = parent
        .join("not-created")
        .join("..")
        .join(fixture.path.file_name().unwrap());
    let mut seal = fixture.seal.clone();
    seal.database = NativePath::from_path(&invalid);
    let mut calls = 0;
    assert!(
        MigrationSource::open_closed_roster(
            seal,
            ReadLimits::default(),
            Arc::new(AtomicBool::new(false)),
            &[],
            &mut |_| {
                calls += 1;
                Ok(())
            }
        )
        .is_err()
    );
    #[cfg(unix)]
    assert_eq!(calls, 0);
    #[cfg(windows)]
    assert!(calls > 0); // per-prefix preparation precedes each original metadata check
    Ok(())
}
