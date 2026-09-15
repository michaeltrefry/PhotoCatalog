//! Cache schema 5 and later write native paths as versioned BLOBs. Legacy TEXT remains
//! literal Unicode path authority; no prefix can collide with the new format.
use super::*;
use crate::storage_volume::NativePath;
use rusqlite::types::ValueRef;

const MAX_PATH_BYTES: usize = 1024 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPath {
    version: u32,
    #[serde(deserialize_with = "strict_native_path")]
    path: NativePath,
}

fn strict_native_path<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<NativePath, D::Error> {
    #[derive(Deserialize)]
    #[serde(tag = "encoding", content = "units", deny_unknown_fields)]
    enum Strict {
        UnixBytes(Vec<u8>),
        WindowsWide(Vec<u16>),
    }
    Ok(match Strict::deserialize(deserializer)? {
        Strict::UnixBytes(bytes) => NativePath::UnixBytes(bytes),
        Strict::WindowsWide(units) => NativePath::WindowsWide(units),
    })
}

pub(super) fn encode_path(path: &Path) -> Result<Vec<u8>> {
    let native = NativePath::from_path(path);
    ensure!(
        native.to_path()?.is_absolute(),
        "cache path must be absolute"
    );
    let bytes = serde_json::to_vec(&StoredPath {
        version: 1,
        path: native,
    })?;
    ensure!(
        bytes.len() <= MAX_PATH_BYTES,
        "cache path exceeds byte bound"
    );
    Ok(bytes)
}
fn decode_path(value: ValueRef<'_>) -> Result<PathBuf> {
    let path = match value {
        ValueRef::Text(bytes) => {
            ensure!(
                bytes.len() <= MAX_PATH_BYTES,
                "cache path exceeds byte bound"
            );
            let path = Path::new(std::str::from_utf8(bytes)?);
            NativePath::from_path(path).to_path()?
        }
        ValueRef::Blob(bytes) => {
            // Inspect SQLite's borrowed bytes before any copy/JSON allocation.
            ensure!(
                bytes.len() <= MAX_PATH_BYTES,
                "cache path exceeds byte bound"
            );
            let value: StoredPath = serde_json::from_slice(bytes)?;
            ensure!(value.version == 1, "unsupported cache path version");
            value.path.to_path()?
        }
        _ => bail!("invalid cache path storage type"),
    };
    ensure!(path.is_absolute(), "cache path must be absolute");
    Ok(path)
}
pub(super) fn read_path(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<PathBuf> {
    let value = row.get_ref(column)?;
    decode_path(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, value.data_type(), error.into())
    })
}
pub(super) fn validate_paths(db: &Connection) -> Result<()> {
    // Only the two cache tiers exist. Reject excess/corrupt rows rather than scan
    // an arbitrarily extended manifest before migration or filesystem recovery.
    for (table, sql, columns) in [
        (
            "locations",
            "SELECT CASE WHEN length(CAST(path AS BLOB))<=1048576 THEN path ELSE NULL END FROM locations LIMIT 3",
            1,
        ),
        (
            "relocations",
            "SELECT CASE WHEN length(CAST(source AS BLOB))<=1048576 THEN source ELSE NULL END,CASE WHEN length(CAST(target AS BLOB))<=1048576 THEN target ELSE NULL END FROM relocations LIMIT 3",
            2,
        ),
    ] {
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if !exists {
            continue;
        }
        let mut statement = db.prepare(sql)?;
        let mut rows = statement.query([])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            ensure!(count <= 2, "invalid cache path row count");
            for column in 0..columns {
                read_path(row, column)?;
            }
        }
    }
    let roots_exist: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='original_roots')",
        [],
        |row| row.get(0),
    )?;
    if roots_exist {
        let mut statement = db.prepare(
            "SELECT CASE WHEN length(CAST(path AS BLOB))<=262144 THEN path ELSE NULL END,length(CAST(path AS BLOB)) FROM original_roots ORDER BY id LIMIT 1025",
        )?;
        let mut rows = statement.query([])?;
        let mut count = 0usize;
        let mut bytes = 0usize;
        while let Some(row) = rows.next()? {
            count += 1;
            ensure!(count <= 1024, "original root count exceeds bound");
            bytes = bytes
                .checked_add(usize::try_from(row.get::<_, i64>(1)?)?)
                .filter(|value| *value <= 2 * 1024 * 1024)
                .context("original root bytes exceed bound")?;
            read_path(row, 0)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "store_paths_tests.rs"]
mod tests;
