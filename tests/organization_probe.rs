use anyhow::{Result, ensure};
use std::{fs, path::Path, process::Command};
fn run(root: &Path, name: &str, args: &[&str]) -> Result<(bool, serde_json::Value)> {
    let output = root.join(format!("{name}.json"));
    let result = Command::new(env!("CARGO_BIN_EXE_organization_probe"))
        .arg("--catalog")
        .arg(root.join("catalog"))
        .arg("--output")
        .arg(&output)
        .args(args)
        .output()?;
    let receipt = serde_json::from_slice(&fs::read(output)?)?;
    Ok((result.status.success(), receipt))
}
#[test]
fn all_frozen_queries_prove_small_fixture_and_corrupt_rows_fail_closed() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    ensure!(run(root, "prepare", &["prepare", "--count", "1000"])?.0);
    let before = blake3::hash(&fs::read(root.join("catalog/catalog.sqlite3"))?);
    for case in [
        "browse",
        "rating",
        "rating-sort",
        "capture-rating",
        "filename-reverse",
        "keyword",
        "wide-keyword",
        "collection",
        "mixed",
        "text",
        "text-capture",
        "camera-lens",
        "date-camera",
        "label-flag",
        "folder",
        "folder-recursive",
        "conflicted",
    ] {
        let (okay, receipt) = run(
            root,
            case,
            &["query", case, "--repetitions", "2", "--warmups", "0"],
        )?;
        ensure!(okay && receipt["complete"] == true, "{case}: {receipt}");
        let samples = receipt["samples"].as_array().unwrap();
        ensure!(samples.len() == 2);
        for sample in samples {
            ensure!(
                sample["rows"].as_array().unwrap().len()
                    == sample["oracle_sequences"].as_array().unwrap().len()
            );
            ensure!(
                sample["chunks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|c| c["sorts"] == 0)
            );
        }
    }
    ensure!(before == blake3::hash(&fs::read(root.join("catalog/catalog.sqlite3"))?));
    let copy = root.join("mutation-copy");
    fs::create_dir(&copy)?;
    let copy_catalog = copy.join("catalog");
    fs::create_dir(&copy_catalog)?;
    fs::copy(
        root.join("catalog/catalog.sqlite3"),
        copy_catalog.join("catalog.sqlite3"),
    )?;
    let (okay, transition) = run(&copy, "transition", &["transitions", "--repetitions", "2"])?;
    ensure!(
        okay && transition["complete"] == true,
        "transition errors: {}",
        transition["errors"]
    );
    ensure!(
        transition["writes"].as_array().unwrap().len() == 2
            && transition["source_updates"].as_array().unwrap().len() == 2
    );
    ensure!(transition["source_hashes_before"] == transition["source_hashes_after"]);
    ensure!(before == blake3::hash(&fs::read(root.join("catalog/catalog.sqlite3"))?));
    let db = rusqlite::Connection::open(root.join("catalog/catalog.sqlite3"))?;
    db.execute(
        "UPDATE organization_assets SET label='corrupted' WHERE sequence IN(501,546)",
        [],
    )?;
    drop(db);
    let (okay, receipt) = run(
        root,
        "corrupt",
        &["query", "browse", "--repetitions", "2", "--warmups", "0"],
    )?;
    ensure!(
        !okay && receipt["complete"] == false && receipt["errors"].as_array().unwrap().len() == 2
    );
    ensure!(
        receipt["samples"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["rows"].as_array().unwrap().len() == 200 && s["error"].as_str().is_some())
    );
    let retained = fs::read(root.join("corrupt.json"))?;
    let result = Command::new(env!("CARGO_BIN_EXE_organization_probe"))
        .arg("--catalog")
        .arg(root.join("catalog"))
        .arg("--output")
        .arg(root.join("corrupt.json"))
        .args(["query", "browse", "--repetitions", "1"])
        .output()?;
    ensure!(!result.status.success() && fs::read(root.join("corrupt.json"))? == retained);
    Ok(())
}

#[test]
fn explicit_fixture_migration_preserves_typed_data_and_rejects_wrong_index() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    ensure!(run(root, "prepare-migration", &["prepare", "--count", "1000"])?.0);
    let conn = rusqlite::Connection::open(root.join("catalog/catalog.sqlite3"))?;
    remove_alias_schema(&conn)?;
    conn.execute_batch("PRAGMA foreign_keys=OFF; DROP INDEX storage_export_path; DROP INDEX storage_export_object; DROP TABLE photo_export_items; DROP TABLE photo_export_jobs; DROP TABLE photo_export_blobs; DROP TABLE edit_copy_items; DROP TABLE edit_copy_jobs; DROP TABLE edit_changes; DROP TABLE edit_redo_nodes; DROP TABLE edit_recipe_nodes; DROP TABLE edit_variants; PRAGMA foreign_keys=ON; DROP INDEX organization_lens_capture; PRAGMA user_version=4; PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);
    let (okay, receipt) = run(root, "migration", &["migrate-fixture"])?;
    ensure!(
        okay && receipt["complete"] == true
            && receipt["schema_before"] == 4
            && receipt["schema_after"] == photocatalog::CURRENT_SCHEMA_VERSION
            && receipt["protocol"] == 2
            && receipt["identity_scope"] == "pre_existing_tables"
            && receipt["added_tables"].as_array().unwrap().len() == 13
            && receipt["added_tables"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row[1] == if row[0] == "export_alias_state" { 1 } else { 0 })
    );
    ensure!(
        receipt["logical_before"] == receipt["logical_after"]
            && receipt["table_counts_before"] == receipt["table_counts_after"]
    );
    ensure!(
        receipt["table_counts_before"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r[0] == "organization_text_idx")
    );
    ensure!(
        run(
            root,
            "after-migration",
            &[
                "query",
                "text-capture",
                "--repetitions",
                "2",
                "--warmups",
                "0"
            ]
        )?
        .0
    );
    let conn = rusqlite::Connection::open(root.join("catalog/catalog.sqlite3"))?;
    conn.execute_batch("DROP INDEX organization_lens_capture; CREATE INDEX organization_lens_capture ON organization_assets(lens); PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);
    let (okay, receipt) = run(root, "wrong-index", &["migrate-fixture"])?;
    ensure!(
        !okay
            && receipt["complete"] == false
            && receipt["error"]
                .as_str()
                .unwrap()
                .contains("unexpected capture index")
    );
    Ok(())
}

#[test]
fn schema_five_requires_explicit_migration_and_current_noop_is_truthful() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    ensure!(run(root, "prepare-six", &["prepare", "--count", "1000"])?.0);
    let main = root.join("catalog/catalog.sqlite3");
    let conn = rusqlite::Connection::open(&main)?;
    remove_alias_schema(&conn)?;
    conn.execute_batch("PRAGMA foreign_keys=OFF; DROP INDEX storage_export_path; DROP INDEX storage_export_object; DROP TABLE photo_export_items; DROP TABLE photo_export_jobs; DROP TABLE photo_export_blobs; DROP TABLE edit_copy_items; DROP TABLE edit_copy_jobs; DROP TABLE edit_changes; DROP TABLE edit_redo_nodes; DROP TABLE edit_recipe_nodes; DROP TABLE edit_variants; PRAGMA user_version=5; PRAGMA wal_checkpoint(TRUNCATE)")?;
    // Existing bindings initialize dirty membership even though projections
    // are deliberately not resolved during schema migration.
    conn.execute("INSERT INTO storage_bindings(asset_id,reference,native_path) VALUES('fixture-000000000001','retained-reference','retained-path')", [])?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);
    let before = fs::read(&main)?;
    let (okay, refusal) = run(
        root,
        "old-query",
        &["query", "browse", "--repetitions", "1", "--warmups", "0"],
    )?;
    ensure!(
        !okay
            && refusal["error"]
                .as_str()
                .unwrap()
                .contains("explicit migration")
    );
    ensure!(
        fs::read(&main)? == before,
        "timed preflight silently migrated old fixture"
    );
    let (okay, migrated) = run(root, "five-to-six", &["migrate-fixture"])?;
    ensure!(
        okay && migrated["protocol"] == 2
            && migrated["schema_before"] == 5
            && migrated["schema_after"] == 6
    );
    ensure!(migrated["logical_before"] == migrated["logical_after"]);
    ensure!(migrated["identity_scope"] == "pre_existing_tables");
    ensure!(migrated["alias_initial_state"] == serde_json::json!({"unbound":999,"dirty":1}));
    ensure!(migrated["added_tables"].as_array().unwrap().len() == 13);
    ensure!(
        migrated["added_tables"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row[1]
                == if row[0] == "export_alias_state" || row[0] == "export_alias_dirty" {
                    1
                } else {
                    0
                })
    );
    ensure!(
        migrated["table_counts_before"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| !row[0].as_str().unwrap().starts_with("edit_"))
    );
    let after = fs::read(&main)?;
    let (okay, verified) = run(root, "six-noop", &["migrate-fixture"])?;
    ensure!(okay && verified["schema_before"] == 6 && verified["schema_after"] == 6);
    ensure!(verified["added_tables"] == serde_json::json!([]));
    ensure!(
        verified["table_counts_before"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row[0].as_str().unwrap().starts_with("edit_"))
            .count()
            == 6
    );
    ensure!(verified["logical_before"] == verified["logical_after"] && fs::read(&main)? == after);
    // Same dirty row count with the wrong member is not equivalent evidence.
    let conn = rusqlite::Connection::open(&main)?;
    conn.execute_batch("DELETE FROM export_alias_dirty; INSERT INTO export_alias_dirty VALUES('fixture-000000000002'); PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);
    let corrupt = fs::read(&main)?;
    let (okay, rejected) = run(root, "wrong-dirty-membership", &["migrate-fixture"])?;
    ensure!(
        !okay
            && rejected["error"]
                .as_str()
                .unwrap()
                .contains("dirty membership")
    );
    ensure!(
        fs::read(&main)? == corrupt,
        "failed verification rewrote the fixture"
    );
    Ok(())
}

// Reconstruct a genuine pre-schema6 fixture; dropping only tables would leave
// alias triggers attached to pre-existing assets/storage_bindings.
fn remove_alias_schema(conn: &rusqlite::Connection) -> Result<()> {
    let triggers = conn
        .prepare(
            "SELECT name FROM sqlite_schema WHERE type='trigger' AND name GLOB 'export_alias_*'",
        )?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for trigger in triggers {
        conn.execute_batch(&format!(
            "DROP TRIGGER \"{}\"",
            trigger.replace('"', "\"\"")
        ))?;
    }
    conn.execute_batch("DROP TABLE export_alias_paths; DROP TABLE export_alias_dirty; DROP TABLE export_alias_directories; DROP TABLE export_alias_state")?;
    Ok(())
}
