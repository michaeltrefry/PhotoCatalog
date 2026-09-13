//! Small synthetic-fixture oracle: preserve SQLite types, exact text/BLOB bytes,
//! schema objects and sequences. This is not a production serialization format.
use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use std::{collections::BTreeMap, path::Path};

pub fn logical_snapshot(root: &Path) -> Result<BTreeMap<String, Vec<Vec<u8>>>> {
    let db = Connection::open_with_flags(
        root.join("catalog.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    db.execute_batch("BEGIN;")?;
    let tables = db
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result = BTreeMap::new();
    for table in std::iter::once("sqlite_schema".to_owned()).chain(tables) {
        let mut statement =
            db.prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            let mut encoded = Vec::new();
            for column in 0..columns {
                let (tag, bytes) = match row.get_ref(column)? {
                    ValueRef::Null => (0, Vec::new()),
                    ValueRef::Integer(value) => (1, value.to_le_bytes().to_vec()),
                    ValueRef::Real(value) => (2, value.to_bits().to_le_bytes().to_vec()),
                    ValueRef::Text(value) => (3, value.to_vec()),
                    ValueRef::Blob(value) => (4, value.to_vec()),
                };
                encoded.push(tag);
                encoded.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                encoded.extend_from_slice(&bytes);
            }
            values.push(encoded);
        }
        values.sort();
        result.insert(table, values);
    }
    db.execute_batch("ROLLBACK;")?;
    ensure!(
        result.contains_key("sqlite_sequence"),
        "fixture must exercise persistent sequences"
    );
    Ok(result)
}
