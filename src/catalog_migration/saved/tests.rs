use super::*;
use std::{io::Write, sync::atomic::Ordering};
fn stop() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
fn fixture() -> Result<Connection> {
    let db = Connection::open_in_memory()?;
    super::super::retention::install(&db)?;
    super::super::importer::install(&db)?;
    super::super::current_repair::install(&db)?;
    super::super::keyword_repair::install(&db)?;
    Ok(db)
}
fn key(n: u64) -> String {
    format!("{n:064x}")
}
fn run(db: &Connection, n: u64, raw: &[u8]) -> Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO migration_retention(id,seal,approval) VALUES(?1,?2,?2)",
        params![key(0), b"{}".as_slice()],
    )?;
    db.execute(
        "INSERT INTO migration_runs VALUES(?1,?2,?3,?3)",
        params![key(n), key(0), raw],
    )?;
    Ok(())
}
#[test]
fn live_keyset_pages_are_bounded_and_seek_without_poll_scans() -> Result<()> {
    let mut db = fixture()?;
    let tx = db.transaction()?;
    for n in 1..=10_000 {
        run(&tx, n, b"{}")?;
    }
    tx.commit()?;
    let limits = Limits {
        rows: 10,
        page_bytes: 1024,
        vm_steps: 1000,
        ..Default::default()
    };
    let page = list(&db, Kind::Run, Some(&key(9900)), limits, stop())?;
    assert!(!page.rows.is_empty() && page.rows.len() < 10);
    assert_eq!(page.rows[0].id, key(9901));
    assert_eq!(page.next.as_ref(), page.rows.last().map(|r| &r.id));
    assert!(bounded_json(&page, 1024)?.len() <= 1024);
    let next = list(&db, Kind::Run, page.next.as_deref(), limits, stop())?;
    assert_eq!(next.rows[0].id, key(9901 + page.rows.len() as u64));
    let tail = list(&db, Kind::Run, Some(&key(9999)), limits, stop())?;
    assert_eq!(tail.rows.len(), 1);
    assert!(tail.next.is_none());
    let canceled = stop();
    canceled.store(true, Ordering::Release);
    assert!(list(&db, Kind::Run, None, limits, canceled).is_err());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM migration_runs", [], |r| r
            .get::<_, i64>(0))?,
        10_000
    );
    Ok(())
}
#[test]
fn exact_documents_preserve_legacy_whitespace_numbers_and_duplicate_keys() -> Result<()> {
    let db = fixture()?;
    let raw = b" \n{\"n\":9007199254740993,\"n\":1e+000,\"__proto__\":{\"x\":true}}\t ";
    run(&db, 1, raw)?;
    db.execute(
        "UPDATE migration_retention SET seal=?2,approval=?2 WHERE id=?1",
        params![key(0), raw],
    )?;
    for field in [
        Document::Progress,
        Document::Policy,
        Document::Seal,
        Document::Approval,
    ] {
        let out = document(
            &db,
            Kind::Run,
            &key(1),
            field,
            None,
            Limits::default(),
            stop(),
        )?;
        assert_eq!(out.bytes, raw);
        assert_eq!(out.blake3, blake3::hash(raw).to_hex().as_str());
        assert!(
            document(
                &db,
                Kind::Run,
                &key(1),
                field,
                Some(&key(0)),
                Limits::default(),
                stop()
            )
            .is_err()
        );
        document(
            &db,
            Kind::Run,
            &key(1),
            field,
            Some(&out.blake3),
            Limits::default(),
            stop(),
        )?;
    }
    assert!(
        document(
            &db,
            Kind::Run,
            &key(1),
            Document::Binding,
            None,
            Limits::default(),
            stop()
        )
        .is_err()
    );
    let out = list(&db, Kind::Run, None, Limits::default(), stop())?;
    assert_eq!(out.rows[0].progress_bytes, raw.len() as u64);
    Ok(())
}
fn repair(db: &Connection, kind: Kind, raw: &[u8]) -> Result<Vec<u8>> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(raw)?;
    let encoded = z.finish()?;
    db.execute(
        &format!("INSERT INTO {} VALUES(?1,?2,?3,?3,?4,?5,?6)", table(kind)),
        params![
            key(2),
            key(1),
            raw,
            encoded,
            i64::try_from(raw.len())?,
            blake3::hash(raw).to_hex().as_str()
        ],
    )?;
    Ok(encoded)
}
#[test]
fn both_repairs_keep_exact_predecessor_bytes_and_reject_corruption() -> Result<()> {
    let db = fixture()?;
    run(&db, 1, b"{}")?;
    let raw = " \n{\"saved\":\"é\",\"cursor\":18446744073709551615} \t".repeat(2000);
    for kind in [Kind::CurrentRepair, Kind::KeywordRepair] {
        let mut compressed = repair(&db, kind, raw.as_bytes())?;
        for field in [
            Document::Progress,
            Document::Binding,
            Document::OriginalProgress,
        ] {
            let out = document(&db, kind, &key(2), field, None, Limits::default(), stop())?;
            assert_eq!(out.bytes, raw.as_bytes());
        }
        let page = list(&db, kind, None, Limits::default(), stop())?;
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].owner, key(1));
        assert!(
            document(
                &db,
                kind,
                &key(2),
                Document::OriginalProgress,
                None,
                Limits {
                    document_bytes: 1024,
                    ..Default::default()
                },
                stop()
            )
            .is_err()
        );
        compressed.push(0);
        db.execute(
            &format!("UPDATE {} SET original_progress=?1", table(kind)),
            [compressed],
        )?;
        assert!(
            document(
                &db,
                kind,
                &key(2),
                Document::OriginalProgress,
                None,
                Limits::default(),
                stop()
            )
            .is_err()
        );
        db.execute(
            &format!("UPDATE {} SET original_digest=?1", table(kind)),
            [key(0)],
        )?;
        assert!(
            document(
                &db,
                kind,
                &key(2),
                Document::OriginalProgress,
                None,
                Limits::default(),
                stop()
            )
            .is_err()
        );
    }
    Ok(())
}
#[test]
fn oversized_or_changed_documents_fail_without_replacing_saved_authority() -> Result<()> {
    let db = fixture()?;
    let raw = "é".repeat(1024);
    run(&db, 1, raw.as_bytes())?;
    let limits = Limits {
        document_bytes: 1024,
        ..Default::default()
    };
    assert!(
        document(
            &db,
            Kind::Run,
            &key(1),
            Document::Progress,
            None,
            limits,
            stop()
        )
        .is_err()
    );
    let admitted = document(
        &db,
        Kind::Run,
        &key(1),
        Document::Progress,
        None,
        Limits::default(),
        stop(),
    )?;
    db.execute(
        "UPDATE migration_runs SET progress=?1",
        [b"new progress".as_slice()],
    )?;
    assert!(
        document(
            &db,
            Kind::Run,
            &key(1),
            Document::Progress,
            Some(&admitted.blake3),
            Limits::default(),
            stop()
        )
        .is_err()
    );
    let canceled = stop();
    canceled.store(true, Ordering::Release);
    assert!(
        document(
            &db,
            Kind::Run,
            &key(1),
            Document::Policy,
            None,
            Limits::default(),
            canceled
        )
        .is_err()
    );
    let policy = document(
        &db,
        Kind::Run,
        &key(1),
        Document::Policy,
        None,
        Limits::default(),
        stop(),
    )?;
    assert_eq!(policy.bytes, raw.as_bytes());
    Ok(())
}
