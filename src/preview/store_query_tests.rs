use super::*;

fn fixture() -> Result<(tempfile::TempDir, PreviewStore)> {
    let root = tempfile::tempdir()?;
    let store = PreviewStore::open(
        StoreConfig {
            manifest_root: root.path().join("manifest"),
            layout: Layout::HashPrefix,
            thumbnail_root: root.path().join("thumb"),
            large_root: root.path().join("large"),
            thumbnail_bytes: 100,
            large_bytes: 100,
        },
        &[],
    )?;
    Ok((root, store))
}

#[test]
fn saved_job_query_guards_borrowed_storage_before_copy_and_recovers() -> Result<()> {
    let (_root, store) = fixture()?;
    store.save_job(&"a".repeat(64), "{}", 100)?;
    for (update, expected) in [
        (
            "descriptor=replace(hex(zeroblob(32769)),'0','x')",
            "saved job descriptor size limit",
        ),
        ("descriptor=zeroblob(8)", "not TEXT"),
        ("descriptor=CAST(x'ff' AS TEXT)", "invalid utf-8"),
        ("id='short'", "invalid saved job identity"),
        ("id=zeroblob(64)", "not TEXT"),
        (
            "id=replace(hex(zeroblob(32)),'0','g')",
            "invalid saved job identity",
        ),
        (
            "id=replace(hex(zeroblob(32)),'0','é')",
            "invalid saved job identity",
        ),
    ] {
        store.db.execute_batch("SAVEPOINT malformed")?;
        store
            .db
            .execute_batch(&format!("UPDATE render_jobs SET {update}"))?;
        let error = store.saved_jobs(0, 100).unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "{update}: {error:#}"
        );
        store
            .db
            .execute_batch("ROLLBACK TO malformed; RELEASE malformed")?;
        let good = store.saved_jobs(0, 100)?;
        assert_eq!(good.len(), 1);
        assert_eq!(good[0].1, "a".repeat(64));
        assert_eq!(good[0].2, "{}");
    }
    Ok(())
}

#[test]
fn saved_job_query_preserves_maximum_descriptor_and_page_order() -> Result<()> {
    let (_root, store) = fixture()?;
    let maximum = format!("{}{{}}", " ".repeat(64 * 1024 - 2));
    store.save_job(&"a".repeat(64), &maximum, 100)?;
    store.save_job(&"b".repeat(64), "{}", 100)?;
    let first = store.saved_jobs(0, 1)?;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].2.len(), 64 * 1024);
    let next = store.saved_jobs(first[0].0, 1)?;
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].1, "b".repeat(64));
    assert!(store.saved_jobs(next[0].0, 1)?.is_empty());
    assert!(store.saved_jobs(0, 0).is_err());
    assert!(store.saved_jobs(0, 1001).is_err());
    Ok(())
}

#[test]
fn image_scope_upgrade_checks_descriptor_storage_before_json_allocation() -> Result<()> {
    let db = Connection::open_in_memory()?;
    db.execute_batch("CREATE TABLE objects(key TEXT,descriptor TEXT);
        CREATE TABLE wanted(desired TEXT); CREATE TABLE render_jobs(id TEXT,created INTEGER,descriptor TEXT);
        INSERT INTO wanted VALUES('key')")?;
    let key = PreviewKey {
        image_pixel_generation: None,
        asset_id: "asset".into(),
        variant_id: "master".into(),
        generation: 1,
        fingerprint: "a".repeat(64),
        edit_revision: 0,
        renderer_version: "test-1".into(),
        preparation_version: PREPARATION_VERSION.into(),
        tier: Tier::Thumbnail,
        edge: 256,
        encoding: CodecSettings {
            codec: super::super::Codec::Jpeg,
            quality: 65,
        },
    };
    db.execute(
        "INSERT INTO objects VALUES('key',?1)",
        [serde_json::to_string(&key)?],
    )?;
    db.execute(
        "INSERT INTO render_jobs VALUES('job',1,?1)",
        [serde_json::json!({"request":{"keys":[key]}}).to_string()],
    )?;
    restore_image_scopes(&db)?;
    for (table, bound) in [
        ("objects", "cached descriptor migration bound"),
        ("render_jobs", "cached job migration bound"),
    ] {
        for (expression, expected) in [
            ("replace(hex(zeroblob(32769)),'0','x')", bound),
            ("zeroblob(8)", "not TEXT"),
        ] {
            db.execute_batch("SAVEPOINT malformed")?;
            db.execute_batch(&format!("UPDATE {table} SET descriptor={expression}"))?;
            let error = restore_image_scopes(&db).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "{table}: {error:#}"
            );
            db.execute_batch("ROLLBACK TO malformed; RELEASE malformed")?;
            restore_image_scopes(&db)?;
        }
    }
    Ok(())
}
