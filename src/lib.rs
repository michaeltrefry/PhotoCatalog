//! UI-independent catalog skeleton. SQLite and JPEG thumbnails are provisional.
mod media;
use anyhow::{Context, Result, bail, ensure};
pub use media::Metadata;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct Asset {
    pub sequence: i64,
    pub id: String,
    pub original_path: String,
    pub state: String,
    pub metadata: Option<Metadata>,
    pub error: Option<String>,
}
#[derive(Debug, Default, Serialize)]
pub struct ImportReport {
    pub imported: u64,
    pub unchanged: u64,
    pub failed: u64,
    pub skipped: u64,
    pub stopped: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportEvent {
    Reserved,
    PreviewPublished,
    Committed,
}
pub struct Catalog {
    db: Connection,
    root: PathBuf,
}
impl Catalog {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        let root = fs::canonicalize(root)?;
        fs::create_dir_all(root.join("previews"))?;
        let db = Connection::open(root.join("catalog.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version <= 1,
            "catalog schema {version} is newer than this application supports"
        );
        let application_id: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        if version == 0 {
            let tables: i64 = db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )?;
            ensure!(
                tables == 0 && application_id == 0,
                "refusing to initialize an unrelated database"
            );
        } else {
            ensure!(
                application_id == 0x50484341,
                "database is not a PhotoCatalog catalog"
            );
        }
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS assets (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE,
                location BLOB NOT NULL UNIQUE,
                path_display TEXT NOT NULL,
                fingerprint TEXT,
                state TEXT NOT NULL CHECK(state IN ('pending','ready','failed')),
                metadata TEXT,
                preview_hash TEXT,
                error TEXT,
                CHECK(state != 'ready' OR (metadata IS NOT NULL AND preview_hash IS NOT NULL AND fingerprint IS NOT NULL))
            );
            PRAGMA application_id = 1346913089;
            PRAGMA user_version = 1;
            COMMIT;")?;
        Ok(Self { db, root })
    }
    /// Imports one explicitly selected directory. Repeating a scan resumes pending/failed files.
    /// The observer runs at durability boundaries and can request a controlled interruption.
    pub fn import(
        &mut self,
        folder: impl AsRef<Path>,
        max_files: Option<usize>,
        mut observer: impl FnMut(ImportEvent) -> Result<()>,
    ) -> Result<ImportReport> {
        let folder = fs::canonicalize(folder)?;
        ensure!(folder.is_dir(), "import source must be a folder");
        ensure!(
            !self.root.starts_with(&folder) && !folder.starts_with(&self.root),
            "catalog and originals must be separate directories"
        );
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("import.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("another catalog import is running")?;
        let mut report = ImportReport::default();
        let mut processed = 0;
        for entry in walkdir::WalkDir::new(&folder)
            .follow_links(false)
            .max_open(16)
        {
            if max_files.is_some_and(|limit| processed >= limit) {
                report.stopped = true;
                break;
            }
            let entry = entry.context("discover source folder")?;
            if !entry.file_type().is_file() {
                continue;
            }
            let ext = entry
                .path()
                .extension()
                .and_then(|v| v.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !["cr2", "jpg", "jpeg", "png"].contains(&ext.as_str()) {
                report.skipped += 1;
                continue;
            }
            processed += 1;
            let path = entry.path();
            let location = location_bytes(path);
            let fingerprint = match fingerprint(path) {
                Ok(value) => value,
                Err(error) => {
                    self.reserve(path, &location)?;
                    self.fail(&location, &error)?;
                    report.failed += 1;
                    continue;
                }
            };
            let existing: Option<(String, String, Option<String>)> = self
                .db
                .query_row(
                    "SELECT fingerprint,state,preview_hash FROM assets WHERE location=?1",
                    [&location],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                            r.get(1)?,
                            r.get(2)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((previous, state, Some(hash))) = existing
                && previous == fingerprint
                && state == "ready"
                && self.read_preview_hash(&hash).is_ok()
            {
                report.unchanged += 1;
                continue;
            }
            self.reserve(path, &location)?;
            observer(ImportEvent::Reserved)?;
            let (metadata, preview) = match media::decode(path) {
                Ok(value) => value,
                Err(error) => {
                    self.fail(&location, &error)?;
                    report.failed += 1;
                    continue;
                }
            };
            // Hash again after decoding: changed or replaced originals must not publish mismatched metadata.
            let after = match fingerprint_file(path) {
                Ok(value) => value,
                Err(error) => {
                    self.fail(&location, &error)?;
                    report.failed += 1;
                    continue;
                }
            };
            if after != fingerprint {
                self.fail(
                    &location,
                    &anyhow::anyhow!("source changed during import; retry required"),
                )?;
                report.failed += 1;
                continue;
            }
            let hash = blake3::hash(&preview).to_hex().to_string();
            self.publish_preview(&hash, &preview)?;
            observer(ImportEvent::PreviewPublished)?;
            self.db.execute("UPDATE assets SET fingerprint=?1,state='ready',metadata=?2,preview_hash=?3,error=NULL WHERE location=?4", params![fingerprint,serde_json::to_string(&metadata)?,hash,location])?;
            observer(ImportEvent::Committed)?;
            report.imported += 1;
        }
        Ok(report)
    }
    fn reserve(&self, path: &Path, location: &[u8]) -> Result<()> {
        self.db.execute("INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?3,'pending') ON CONFLICT(location) DO UPDATE SET state='pending',preview_hash=NULL,error=NULL", params![Uuid::new_v4().to_string(),location,path.to_string_lossy()])?;
        Ok(())
    }
    fn fail(&self, location: &[u8], error: &anyhow::Error) -> Result<()> {
        self.db.execute(
            "UPDATE assets SET state='failed',preview_hash=NULL,error=?1 WHERE location=?2",
            params![format!("{error:#}"), location],
        )?;
        Ok(())
    }
    fn publish_preview(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        let folder = self.root.join("previews");
        let destination = folder.join(format!("{hash}.jpg"));
        if destination.exists() {
            if self.read_preview_hash(hash).is_ok() {
                return Ok(());
            }
            fs::remove_file(&destination)?;
        }
        let mut tmp = tempfile::NamedTempFile::new_in(&folder)?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(&destination)
            .context("publish durable preview")?;
        #[cfg(unix)]
        File::open(&folder)?.sync_all()?;
        Ok(())
    }
    fn read_preview_hash(&self, hash: &str) -> Result<Vec<u8>> {
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid preview key"
        );
        let file = File::open(self.root.join("previews").join(format!("{hash}.jpg")))?;
        ensure!(
            file.metadata()?.len() <= 4 * 1024 * 1024,
            "invalid preview length"
        );
        let mut bytes = Vec::new();
        file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            blake3::hash(&bytes).to_hex().as_str() == hash,
            "preview checksum mismatch"
        );
        Ok(bytes)
    }
    pub fn preview(&self, id: &str) -> Result<Vec<u8>> {
        let (state, hash): (String, Option<String>) = self
            .db
            .query_row(
                "SELECT state,preview_hash FROM assets WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .context("asset not found")?;
        ensure!(state == "ready", "asset preview is not ready ({state})");
        self.read_preview_hash(&hash.context("missing preview reference")?)
    }
    pub fn get(&self, id: &str) -> Result<Asset> {
        let row = self.db.query_row(
            "SELECT sequence,id,path_display,state,metadata,error FROM assets WHERE id=?1",
            [id],
            asset_row,
        )?;
        parse_asset(row)
    }
    /// Keyset pagination; pass the last returned sequence as `after`. Maximum 1000 records.
    pub fn browse(&self, after: i64, limit: usize) -> Result<Vec<Asset>> {
        ensure!(
            after >= 0 && (1..=1000).contains(&limit),
            "after must be nonnegative and limit must be 1..=1000"
        );
        let mut stmt = self.db.prepare("SELECT sequence,id,path_display,state,metadata,error FROM assets WHERE sequence>?1 ORDER BY sequence LIMIT ?2")?;
        let rows = stmt.query_map(params![after, limit as i64], asset_row)?;
        rows.map(|r| parse_asset(r?)).collect()
    }
}
type AssetRow = (i64, String, String, String, Option<String>, Option<String>);
fn asset_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}
fn parse_asset((sequence, id, original_path, state, metadata, error): AssetRow) -> Result<Asset> {
    Ok(Asset {
        sequence,
        id,
        original_path,
        state,
        metadata: metadata.map(|s| serde_json::from_str(&s)).transpose()?,
        error,
    })
}
fn fingerprint(path: &Path) -> Result<String> {
    fingerprint_file(path)
}
fn fingerprint_file(path: &Path) -> Result<String> {
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "source became a symbolic link"
    );
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "source is not a regular file");
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 65536];
    let mut total = 0;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        ensure!(total <= before.len(), "source grew during hashing");
        hasher.update(&buffer[..read]);
    }
    let after = fs::metadata(path)?;
    if total != before.len()
        || before.len() != after.len()
        || before.modified()? != after.modified()?
    {
        bail!("source changed during hashing");
    }
    Ok(hasher.finalize().to_hex().to_string())
}
#[cfg(unix)]
fn location_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
#[cfg(windows)]
fn location_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}
