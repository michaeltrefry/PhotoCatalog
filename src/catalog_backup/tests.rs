use super::*;
use rusqlite::params;
fn limits() -> Limits {
    Limits {
        min_free_bytes: 0,
        pages_per_step: 1,
        ..Limits::default()
    }
}
fn source(root: &Path) -> Result<Catalog> {
    let c = Catalog::open(root)?;
    c.db.execute_batch(
        "CREATE TABLE snapshot_pair(id INTEGER PRIMARY KEY,a INTEGER NOT NULL,b BLOB NOT NULL);",
    )?;
    c.db.execute(
        "INSERT INTO snapshot_pair VALUES(1,7,?1)",
        [vec![19u8; 2 * 1024 * 1024]],
    )?;
    Ok(c)
}
#[test]
fn pinned_snapshot_allows_writers_and_excludes_later_commits() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source_root = temp.path().join("live");
    let original = source(&source_root)?;
    let writer = Connection::open(source_root.join(DB))?;
    writer.busy_timeout(Duration::ZERO)?;
    let mut wrote_snapshot = false;
    let mut wrote_copy = false;
    let mut snapshot_released = false;
    let bundle = temp.path().join("backup");
    let r = backup_catalog(&source_root, &bundle, &limits(), |p| {
        if p.phase == Phase::Snapshot {
            writer.execute(
                "UPDATE snapshot_pair SET a=8,b=?1 WHERE id=1",
                [vec![20u8; 2 * 1024 * 1024]],
            )?;
            wrote_snapshot = true;
        }
        if p.phase == Phase::Copy && !wrote_copy {
            writer.execute("INSERT INTO snapshot_pair VALUES(2,9,?1)", [vec![3u8; 20]])?;
            wrote_copy = true;
        }
        if p.phase == Phase::Verify && !snapshot_released {
            let busy: i64 =
                writer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
            assert_eq!(busy, 0, "source snapshot must release before verification");
            snapshot_released = true;
        }
        Ok(())
    })?;
    assert!(wrote_snapshot && wrote_copy && snapshot_released);
    assert_eq!(
        original
            .db
            .query_row("SELECT a FROM snapshot_pair WHERE id=1", [], |r| r
                .get::<_, i64>(0))?,
        8
    );
    assert_eq!(inspect_backup(&bundle, &limits(), |_| Ok(()))?, r);
    let copy = readonly(&bundle.join(DB))?;
    assert_eq!(
        copy.query_row("SELECT count(*) FROM snapshot_pair", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    let (a, b): (i64, Vec<u8>) = copy.query_row("SELECT a,b FROM snapshot_pair", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?;
    assert_eq!(a, 7);
    assert_eq!(b, vec![19; 2 * 1024 * 1024]);
    // The snapshot lease is released before verification/publication.
    writer.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(())
}
#[test]
fn incomplete_and_backup_roots_cannot_be_opened_or_promoted() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let src = temp.path().join("live");
    let _live = source(&src)?;
    let failed = temp.path().join("failed");
    let err = backup_catalog(&src, &failed, &limits(), |p| {
        if p.phase == Phase::Copy {
            bail!("injected interruption")
        }
        Ok(())
    })
    .unwrap_err();
    assert!(format!("{err:#}").contains("injected interruption"));
    assert!(failed.join(PENDING).exists());
    assert!(!failed.join(MANIFEST).exists());
    assert!(Catalog::open(&failed).is_err());
    assert!(!failed.join("previews").exists());
    assert!(inspect_backup(&failed, &limits(), |_| Ok(())).is_err());
    let good = temp.path().join("good");
    backup_catalog(&src, &good, &limits(), |_| Ok(()))?;
    assert!(Catalog::open(&good).is_err());
    assert!(!good.join("previews").exists());
    let dst = temp.path().join("restoring");
    assert!(
        restore_catalog(&good, &dst, &limits(), |p| {
            if p.phase == Phase::Copy {
                bail!("restore interrupted")
            }
            Ok(())
        })
        .is_err()
    );
    assert!(Catalog::open(&dst).is_err());
    assert!(!dst.join(RESTORE).exists());
    assert!(backup_catalog(&src, &good, &limits(), |_| Ok(())).is_err());
    assert!(restore_catalog(&good, &src, &limits(), |_| Ok(())).is_err());
    assert!(Catalog::open(&src).is_ok());
    inspect_backup(&good, &limits(), |_| Ok(()))?;
    Ok(())
}
#[test]
fn low_disk_wal_pressure_corrupt_bundle_and_bad_upgrade_fail_closed() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let src = temp.path().join("live");
    let live = source(&src)?;
    let low = Limits {
        min_free_bytes: u64::MAX / 2,
        ..limits()
    };
    let dst = temp.path().join("low");
    assert!(
        backup_catalog(&src, &dst, &low, |_| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("free space")
    );
    assert!(Catalog::open(&dst).is_err());
    let pressure = Limits {
        max_source_wal_bytes: 0,
        ..limits()
    };
    assert!(
        backup_catalog(&src, temp.path().join("wal"), &pressure, |_| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("WAL pressure")
    );
    // Deliberately malformed old schema remains an inspectable backup; upgrading the copy fails.
    live.db.execute_batch("DROP INDEX migration_keyword_repair_order; DROP TABLE migration_keyword_repair_items; CREATE TABLE migration_keyword_repair_items(bad INTEGER); PRAGMA user_version=9;")?;
    let good = temp.path().join("old");
    let r = backup_catalog(&src, &good, &limits(), |_| Ok(()))?;
    assert_eq!(r.schema_version, 9);
    let broken = temp.path().join("upgrade");
    assert!(
        restore_catalog(&good, &broken, &limits(), |_| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("schema upgrade failed")
    );
    assert!(Catalog::open(&broken).is_err());
    assert!(!broken.join(RESTORE).exists());
    assert_eq!(
        readonly(&broken.join(DB))?.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        9
    );
    assert_eq!(inspect_backup(&good, &limits(), |_| Ok(()))?, r);
    let before = fs::read(good.join(DB))?;
    let mut file = OpenOptions::new().write(true).open(good.join(DB))?;
    file.write_all(b"broken")?;
    drop(file);
    assert!(
        inspect_backup(&good, &limits(), |_| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("digest mismatch")
    );
    assert!(restore_catalog(&good, temp.path().join("no"), &limits(), |_| Ok(())).is_err());
    assert!(!temp.path().join("no").exists());
    fs::write(good.join(DB), before)?;
    assert_eq!(inspect_backup(&good, &limits(), |_| Ok(()))?, r);
    Ok(())
}
#[test]
fn restored_execution_is_receipt_bound_and_release_executes_nothing() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let src = temp.path().join("live");
    let mut live = source(&src)?;
    let job = live.begin_photo_export()?;
    let backup = temp.path().join("backup");
    backup_catalog(&src, &backup, &limits(), |_| Ok(()))?;
    let dst = temp.path().join("new");
    let r = restore_catalog(&backup, &dst, &limits(), |_| Ok(()))?;
    let mut c = Catalog::open(&dst)?;
    assert!(restore_status(&dst)?.unwrap().jobs_held);
    for e in [
        c.claim_photo_export(&job.id).map(|_| ()),
        c.publish_photo_export_item(&job.id, 1).map(|_| ()),
        c.recover_metadata_export(Path::new("/not-read"))
            .map(|_| ()),
        c.apply_metadata_export("missing").map(|_| ()),
    ] {
        assert!(
            e.unwrap_err()
                .to_string()
                .contains("restored external jobs are held")
        );
    }
    let raw: String = c.db.query_row(
        "SELECT state FROM photo_export_jobs WHERE id=?1",
        [&job.id],
        |r| r.get(0),
    )?;
    assert!(resume_restored_jobs(&dst, &r.restore_id, false).is_err());
    assert!(resume_restored_jobs(&dst, "wrong", true).is_err());
    let status = resume_restored_jobs(&dst, &r.restore_id, true)?;
    assert!(!status.jobs_held);
    assert_eq!(status.receipt, r);
    assert_eq!(
        c.db.query_row(
            "SELECT state FROM photo_export_jobs WHERE id=?1",
            [&job.id],
            |r| r.get::<_, String>(0)
        )?,
        raw
    );
    assert!(!restore_status(&dst)?.unwrap().jobs_held);
    assert!(c.begin_photo_export().is_ok());
    assert!(resume_restored_jobs(&dst, &r.restore_id, true).is_ok());
    drop(c);
    assert!(!restore_status(&dst)?.unwrap().jobs_held);
    assert!(Catalog::open(&dst).is_ok());
    fs::write(dst.join(RESUMED), b"{}")?;
    assert!(Catalog::open(&dst)?.begin_photo_export().is_err());
    Ok(())
}
#[test]
fn completed_restore_with_lost_or_corrupt_receipt_cannot_bypass_job_hold() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let src = temp.path().join("source");
    let mut legacy = source(&src)?;
    assert_eq!(restore_status(&src)?, None);
    assert!(legacy.begin_photo_export().is_ok());
    let bundle = temp.path().join("bundle");
    backup_catalog(&src, &bundle, &limits(), |_| Ok(()))?;
    let dst = temp.path().join("restored");
    let receipt = restore_catalog(&bundle, &dst, &limits(), |_| Ok(()))?;
    let mut restored = Catalog::open(&dst)?;
    let receipt_bytes = fs::read(dst.join(RESTORE))?;
    assert!(dst.join(COMPLETED).is_file());
    fs::remove_file(dst.join(RESTORE))?;
    assert!(restore_status(&dst).is_err());
    assert!(Catalog::open(&dst).is_err());
    assert!(restored.begin_photo_export().is_err());
    assert!(resume_restored_jobs(&dst, &receipt.restore_id, true).is_err());
    fs::write(dst.join(RESTORE), b"{}")?;
    assert!(restore_status(&dst).is_err());
    assert!(restored.begin_photo_export().is_err());
    fs::write(dst.join(RESTORE), receipt_bytes)?;
    assert!(restore_status(&dst)?.unwrap().jobs_held);
    assert!(restored.begin_photo_export().is_err());
    assert!(!resume_restored_jobs(&dst, &receipt.restore_id, true)?.jobs_held);
    assert!(restored.begin_photo_export().is_ok());
    Ok(())
}
#[test]
fn sqlite_verification_observes_cancel_and_vm_budget() -> Result<()> {
    let db = Connection::open_in_memory()?;
    db.execute_batch("CREATE TABLE data(x); WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000) INSERT INTO data SELECT x FROM n;")?;
    let cancel = CancellationToken::default();
    let l = limits();
    let op = Operation::new(&l, &cancel, |_| Ok(()))?;
    op.guard(&db)?;
    let ticks = Arc::clone(&op.vm);
    let c = cancel.clone();
    let done = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&done);
    let thread = std::thread::spawn(move || {
        while ticks.load(Ordering::Relaxed) == 0 && !finished.load(Ordering::Relaxed) {
            std::thread::yield_now();
        }
        if !finished.load(Ordering::Relaxed) {
            c.cancel();
        }
    });
    let result = db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0));
    done.store(true, Ordering::Relaxed);
    thread.join().unwrap();
    assert!(cancel.is_cancelled());
    assert!(result.is_err());
    let l = Limits {
        verification_vm_steps: 1000,
        ..limits()
    };
    let op = Operation::new(&l, &CancellationToken::default(), |_| Ok(()))?;
    op.guard(&db)?;
    assert!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .is_err()
    );
    Ok(())
}
#[test]
fn missing_source_invalid_limits_and_prepublication_cancellation_do_not_promote() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let missing = tmp.path().join("missing");
    assert!(backup_catalog(&missing, tmp.path().join("out"), &limits(), |_| Ok(())).is_err());
    assert!(!missing.exists());
    let invalid = Limits {
        pages_per_step: 0,
        ..limits()
    };
    assert!(backup_catalog(&missing, tmp.path().join("out"), &invalid, |_| Ok(())).is_err());
    let src = tmp.path().join("source");
    let _c = source(&src)?;
    let dest = tmp.path().join("cancel");
    let token = CancellationToken::default();
    assert!(
        backup_catalog_with_control(&src, &dest, &limits(), &token, |p| {
            if p.phase == Phase::Publish {
                token.cancel();
            }
            Ok(())
        })
        .is_err()
    );
    assert!(dest.join(PENDING).exists());
    assert!(!dest.join(MANIFEST).exists());
    assert!(Catalog::open(&dest).is_err());
    // No source table content was rebuilt or normalized.
    assert_eq!(
        readonly(&src.join(DB))?.query_row(
            "SELECT length(b) FROM snapshot_pair WHERE a=?1",
            params![7],
            |r| r.get::<_, i64>(0)
        )?,
        2 * 1024 * 1024
    );
    Ok(())
}

#[test]
fn inspect_rejects_same_length_valid_mutation_after_hash() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let src = temp.path().join("source");
    let _live = source(&src)?;
    let bundle = temp.path().join("bundle");
    let r = backup_catalog(&src, &bundle, &limits(), |_| Ok(()))?;
    let mut changed = false;
    let error = inspect_backup(&bundle, &limits(), |p| {
        if p.phase == Phase::Verify && !changed {
            let db = Connection::open(bundle.join(DB))?;
            db.execute("UPDATE snapshot_pair SET a=8 WHERE id=1", [])?;
            drop(db);
            assert_eq!(regular(&bundle.join(DB))?.len(), r.database_bytes);
            changed = true;
        }
        Ok(())
    })
    .unwrap_err();
    assert!(changed);
    assert!(error.to_string().contains("identity or revision changed"));
    assert_eq!(
        readonly(&bundle.join(DB))?
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?,
        "ok"
    );
    assert!(inspect_backup(&bundle, &limits(), |_| Ok(())).is_err());
    Ok(())
}
#[cfg(unix)]
#[test]
fn hash_rejects_same_length_replacement() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let src = temp.path().join("source");
    let _live = source(&src)?;
    let bundle = temp.path().join("bundle");
    backup_catalog(&src, &bundle, &limits(), |_| Ok(()))?;
    let file = bundle.join(DB);
    let bytes = fs::read(&file)?;
    let mut replaced = false;
    let error = inspect_backup(&bundle, &limits(), |p| {
        if p.phase == Phase::Hash && p.bytes_processed > 0 && !replaced {
            let new = bundle.join("replacement");
            fs::write(&new, &bytes)?;
            fs::rename(new, &file)?;
            replaced = true;
        }
        Ok(())
    })
    .unwrap_err();
    assert!(replaced);
    assert!(error.to_string().contains("identity or revision changed"));
    for suffix in ["-wal", "-journal"] {
        let mut name = file.as_os_str().to_os_string();
        name.push(suffix);
        let companion = PathBuf::from(name);
        fs::write(&companion, b"nonempty")?;
        assert!(
            no_journal(&file)
                .unwrap_err()
                .to_string()
                .contains("not self-contained")
        );
        fs::remove_file(companion)?;
    }
    inspect_backup(&bundle, &limits(), |_| Ok(()))?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn companion_suffix_preserves_native_path_bytes() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let path = PathBuf::from(std::ffi::OsString::from_vec(
        b"/unrepresentable-\xff/catalog.sqlite3".to_vec(),
    ));
    assert_eq!(
        companion(&path, "-wal").as_os_str().as_bytes(),
        b"/unrepresentable-\xff/catalog.sqlite3-wal"
    );
    assert_eq!(
        companion(&path, "-journal").as_os_str().as_bytes(),
        b"/unrepresentable-\xff/catalog.sqlite3-journal"
    );
}
// APFS rejects invalid UTF-8 filenames at directory creation; Linux supports them.
#[cfg(target_os = "linux")]
#[test]
fn non_utf8_directory_nonempty_journal_is_rejected() -> Result<()> {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir()?;
    let dir = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"bundle-\xff".to_vec()));
    fs::create_dir(&dir)?;
    let db = dir.join(DB);
    for suffix in ["-wal", "-journal"] {
        let file = companion(&db, suffix);
        fs::write(&file, b"nonempty")?;
        assert!(no_journal(&db).is_err());
        fs::remove_file(file)?;
    }
    Ok(())
}
