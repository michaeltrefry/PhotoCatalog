use super::*;

fn catalog() -> Result<(tempfile::TempDir, Catalog)> {
    let temp = tempfile::tempdir()?;
    let catalog = Catalog::open(temp.path().join("catalog"))?;
    catalog.db.execute(
        "INSERT INTO migration_retention(id,seal,approval) VALUES('input',x'00',x'00')",
        [],
    )?;
    Ok((temp, catalog))
}

fn record(catalog: &Catalog, sequence: i64, raw_bytes: usize, complete: bool) -> Result<()> {
    let record = EvidenceRecord {
        revision: "revision".into(),
        collection: Collection::Captures,
        rowid: sequence,
        key: vec![],
        fields: Default::default(),
    };
    let mut raw = serde_json::to_vec(&record)?;
    raw.resize(raw_bytes.max(raw.len()), b' ');
    catalog.db.execute(
        "INSERT INTO migration_retained_records(sequence,input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete)
         VALUES(?1,'input','revision',0,?1,?2,?3,?4,'',?5)",
        params![sequence, compress(&raw)?, i64::try_from(raw.len())?, blake3::hash(&raw).to_hex().to_string(), complete],
    )?;
    Ok(())
}

fn page(catalog: &Catalog, after: i64, limit: usize) -> Result<Vec<(i64, EvidenceRecord)>> {
    catalog.retained_migration_records("input", "revision", Collection::Captures, after, limit)
}

#[test]
fn retained_record_query_rejects_storage_before_decode_and_recovers() -> Result<()> {
    let (_temp, catalog) = catalog()?;
    record(&catalog, 1, 0, true)?;
    for (update, expected) in [
        (
            "compressed=zeroblob(8421377)",
            "retained record type/size limit",
        ),
        ("compressed='not a blob'", "retained record type/size limit"),
        ("digest=zeroblob(64)", "retained identity must be 64 bytes"),
        (
            "digest=replace(hex(zeroblob(33)),'0','a')",
            "retained identity must be 64 bytes",
        ),
        (
            "digest=replace(hex(zeroblob(32)),'0','é')",
            "retained identity must be 64 bytes",
        ),
        (
            "digest=CAST(x'ff' || zeroblob(63) AS TEXT)",
            "retained identity must be 64 bytes",
        ),
    ] {
        catalog.db.execute_batch("SAVEPOINT malformed")?;
        catalog
            .db
            .execute_batch(&format!("UPDATE migration_retained_records SET {update}"))?;
        let error = page(&catalog, 0, 100).unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "{update}: {error:#}"
        );
        catalog
            .db
            .execute_batch("ROLLBACK TO malformed; RELEASE malformed")?;
        let good = page(&catalog, 0, 100)?;
        assert_eq!(good.len(), 1);
        assert_eq!(good[0].0, 1);
    }
    Ok(())
}

#[test]
fn retained_record_query_keeps_byte_budget_pagination_and_completion_filter() -> Result<()> {
    let (_temp, catalog) = catalog()?;
    record(&catalog, 1, RECORD_LIMIT / 2 + 1, true)?;
    record(&catalog, 2, RECORD_LIMIT / 2 + 1, true)?;
    record(&catalog, 3, 0, false)?;
    record(&catalog, 4, 0, true)?;
    let first = page(&catalog, 0, 100)?;
    assert_eq!(first.iter().map(|r| r.0).collect::<Vec<_>>(), [1]);
    let next = page(&catalog, first[0].0, 100)?;
    assert_eq!(next.iter().map(|r| r.0).collect::<Vec<_>>(), [2, 4]);
    assert_eq!(page(&catalog, 1, 1)?[0].0, 2);
    assert!(page(&catalog, 4, 100)?.is_empty());
    assert!(page(&catalog, -1, 100).is_err());
    assert!(page(&catalog, 0, 0).is_err());
    assert!(page(&catalog, 0, 101).is_err());
    // The maximum valid first record is still admitted, despite the page cap.
    catalog
        .db
        .execute("DELETE FROM migration_retained_records", [])?;
    record(&catalog, 1, RECORD_LIMIT, true)?;
    assert_eq!(page(&catalog, 0, 100)?.len(), 1);
    Ok(())
}

#[test]
fn retained_field_query_checks_borrowed_identity_before_lookup() -> Result<()> {
    let (_temp, catalog) = catalog()?;
    record(&catalog, 1, 0, true)?;
    let state = evidence::begin(&catalog.db, b"descriptor", 0)?;
    catalog.db.execute(
        "INSERT INTO migration_retained_fields VALUES(1,'field',?1)",
        [&state.id],
    )?;
    assert_eq!(catalog.retained_migration_field(1, "field")?, state);
    for expression in [
        "zeroblob(64)",
        "replace(hex(zeroblob(33)),'0','a')",
        "replace(hex(zeroblob(32)),'0','é')",
        "CAST(x'ff' || zeroblob(63) AS TEXT)",
    ] {
        catalog
            .db
            .execute_batch("SAVEPOINT malformed; PRAGMA defer_foreign_keys=ON")?;
        catalog.db.execute_batch(&format!(
            "UPDATE migration_retained_fields SET evidence={expression}"
        ))?;
        let error = catalog.retained_migration_field(1, "field").unwrap_err();
        assert!(
            format!("{error:#}").contains("retained identity must be 64 bytes"),
            "{expression}: {error:#}"
        );
        catalog
            .db
            .execute_batch("ROLLBACK TO malformed; RELEASE malformed")?;
        assert_eq!(catalog.retained_migration_field(1, "field")?, state);
    }
    catalog
        .db
        .execute("UPDATE migration_retained_records SET complete=0", [])?;
    assert!(catalog.retained_migration_field(1, "field").is_err());
    Ok(())
}
