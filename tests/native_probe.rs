use anyhow::Result;
use photocatalog::Catalog;
use rusqlite::{Connection, params};
use serde_json::Value;

#[test]
fn browse_only_uses_real_catalog_without_entering_mixed_writes() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    drop(Catalog::open(temporary.path())?);
    let mut connection = Connection::open(temporary.path().join("catalog.sqlite3"))?;
    let tx = connection.transaction()?;
    let metadata = r#"{"format":"JPEG","width":10,"height":10,"orientation":1,"camera_make":null,"camera_model":null,"captured_at":null,"preview_source":"synthetic fixture"}"#;
    // Only the actual v1 table exists: entering native_mixed would fail because
    // its synthetic annotations/edits tables are deliberately absent.
    for sequence in 1..=10_000_i64 {
        tx.execute("INSERT INTO assets(sequence,id,location,path_display,fingerprint,state,metadata,preview_hash) VALUES(?1,?2,?3,'/synthetic/offline.jpg','fingerprint','ready',?4,?5)",
            params![sequence,format!("00000000-0000-4000-8000-{sequence:012x}"),sequence.to_le_bytes().as_slice(),metadata,"a".repeat(64)])?;
    }
    tx.commit()?;
    drop(connection);
    let output = assert_cmd::cargo::cargo_bin_cmd!("catalog_probe")
        .arg("--catalog")
        .arg(temporary.path())
        .args(["--count", "10000", "--repetitions", "2", "--browse-only"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["browse_only"], true);
    assert!(result["native_mixed"].is_null());
    assert_eq!(result["n"], 2);
    assert_eq!(result["identity_metadata_restart_verified"], true);
    assert_eq!(result["settings"]["cache_size"], -262144);
    assert_eq!(result["settings"]["temp_store"], 1);
    let connection = Connection::open(temporary.path().join("catalog.sqlite3"))?;
    assert_eq!(
        connection.query_row("SELECT count(*) FROM assets", [], |row| row
            .get::<_, i64>(0))?,
        10_000
    );
    Ok(())
}
