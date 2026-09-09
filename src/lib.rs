//! UI-independent SQLite catalog core. JPEG thumbnails remain provisional.
pub mod catalog_metadata;
pub mod catalog_storage;
mod import_storage;
pub mod media;
pub mod metadata_export;
pub mod storage_volume;
pub mod xmp;
pub mod xmp_packets;
mod xmp_rdf;
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
    pub metadata_updated: u64,
    pub metadata_warnings: u64,
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

/// Apply the measured SQLite settings to an app-owned, already validated connection.
/// This changes journal mode; callers must not pass unrelated or read-only databases.
/// The cache budget is per connection. It is a target, not a process-memory ceiling.
pub fn configure_catalog_connection(db: &Connection) -> Result<()> {
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "FULL")?;
    // Match macOS durable WAL commits to storage flush semantics (ignored elsewhere).
    db.pragma_update(None, "fullfsync", true)?;
    db.pragma_update(None, "foreign_keys", true)?;
    db.pragma_update(None, "cache_size", -262144)?;
    db.pragma_update(None, "mmap_size", 0)?;
    db.pragma_update(None, "temp_store", 1)?;
    db.pragma_update(None, "wal_autocheckpoint", 1000)?;
    Ok(())
}

impl Catalog {
    /// Resolve source and prospective catalog locations before creating any files.
    /// Use this entry point when opening a catalog for an import operation.
    pub fn open_for_import(root: impl AsRef<Path>, source: impl AsRef<Path>) -> Result<Self> {
        let source = fs::canonicalize(source).context("resolve import source")?;
        ensure!(source.is_dir(), "import source must be a folder");
        let root = prospective_directory(root.as_ref())?;
        ensure!(
            !root.starts_with(&source) && !source.starts_with(&root),
            "catalog and originals must be separate directories"
        );
        Self::open(root)
    }
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        let root = fs::canonicalize(root)?;
        fs::create_dir_all(root.join("previews"))?;
        let mut db = Connection::open(root.join("catalog.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version <= 3,
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
        configure_catalog_connection(&db)?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch("
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
            ")?;
        if version < 2 {
            tx.execute_batch(catalog_metadata::SCHEMA)?;
            tx.pragma_update(None, "user_version", 2)?;
        }
        if version < 3 {
            tx.execute_batch(catalog_storage::SCHEMA)?;
            tx.execute_batch(catalog_metadata::FILE_INSTANCE_SCHEMA)?;
            tx.pragma_update(None, "user_version", 3)?;
        }
        tx.commit()?;
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
        let _lock = ImportLock::acquire(&self.root.join("import.lock"))?;
        self.begin_metadata_scan()?;
        let mut report = ImportReport::default();
        let mut processed = 0;
        let mut volumes = import_storage::ImportVolumes::new();
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
            if !media::supported_extension(&ext) {
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
                    self.record_import_path(path)?;
                    let (changed, warnings) = self.refresh_metadata(path, true)?;
                    report.metadata_updated += u64::from(changed);
                    report.metadata_warnings += warnings as u64;
                    self.fail(&location, &error)?;
                    report.failed += 1;
                    continue;
                }
            };
            let observation = volumes.observe(path)?;
            self.reconnect_storage_asset(path, &observation, &fingerprint, volumes.snapshot())?;
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
                self.bind_import_storage(path, &observation)?;
                let (changed, warnings) = self.refresh_metadata(path, false)?;
                report.metadata_updated += u64::from(changed);
                report.metadata_warnings += warnings as u64;
                report.unchanged += 1;
                continue;
            }
            self.reserve(path, &location)?;
            self.bind_import_storage(path, &observation)?;
            observer(ImportEvent::Reserved)?;
            let (changed, warnings) = self.refresh_metadata(path, true)?;
            report.metadata_updated += u64::from(changed);
            report.metadata_warnings += warnings as u64;
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
    fn bind_import_storage(
        &mut self,
        path: &Path,
        observation: &storage_volume::VolumeLocation,
    ) -> Result<()> {
        let asset = self.record_import_path(path)?;
        if observation.state == storage_volume::LocationState::Available {
            self.bind_storage(&asset, observation)?;
        }
        Ok(())
    }
    fn record_import_path(&mut self, path: &Path) -> Result<String> {
        let asset: String = self.db.query_row(
            "SELECT id FROM assets WHERE location=?1",
            [location_bytes(path)],
            |row| row.get(0),
        )?;
        self.record_storage_path(&asset, &storage_volume::NativePath::from_path(path))?;
        Ok(asset)
    }
    fn reserve(&self, path: &Path, location: &[u8]) -> Result<()> {
        self.db.execute("INSERT INTO assets(id,location,path_display,state,render_generation) VALUES(?1,?2,?3,'pending',1) ON CONFLICT(location) DO UPDATE SET state='pending',preview_hash=NULL,error=NULL,render_generation=render_generation+1", params![Uuid::new_v4().to_string(),location,path.to_string_lossy()])?;
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

// Canonicalize the existing ancestor, then normalize the missing suffix without mkdir.
// Opening the resolved result also avoids creating incidental directories in paths
// such as `originals/not-yet-created/../../catalog`.
fn prospective_directory(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    let mut resolved = loop {
        match fs::canonicalize(ancestor) {
            Ok(existing) => {
                ensure!(existing.is_dir(), "catalog ancestor must be a directory");
                break existing;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    ancestor
                        .components()
                        .next_back()
                        .context("catalog has no existing ancestor")?
                        .as_os_str()
                        .to_os_string(),
                );
                ancestor = ancestor
                    .parent()
                    .context("catalog has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("resolve catalog location"),
        }
    };
    for part in suffix.into_iter().rev() {
        if part == ".." {
            resolved.pop();
        } else if part != "." {
            resolved.push(part);
        }
        // A parent component may return from the missing suffix into a different
        // existing branch; resolve any symlink reached there before comparing.
        match fs::canonicalize(&resolved) {
            Ok(existing) => {
                ensure!(existing.is_dir(), "catalog ancestor must be a directory");
                resolved = existing;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("resolve catalog suffix"),
        }
    }
    Ok(resolved)
}

struct ImportLock(File);
impl ImportLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        fs2::FileExt::try_lock_exclusive(&file).context("another catalog import is running")?;
        Ok(Self(file))
    }
}

impl Drop for ImportLock {
    fn drop(&mut self) {
        // Closing only our descriptor can leave flock held by a fork's inherited
        // open-file description until exec. Release at the operation boundary,
        // including early errors and unwinding, before closing this descriptor.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

#[cfg(all(test, unix))]
mod lock_tests {
    use super::*;
    #[test]
    fn finishing_import_releases_lock_even_if_a_descriptor_was_duplicated() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("import.lock");
        let lock = ImportLock::acquire(&path)?;
        assert!(ImportLock::acquire(&path).is_err());
        // A fork can retain the same open-file description until the child's exec.
        // try_clone reproduces that descriptor lifetime deterministically.
        let inherited = lock.0.try_clone()?;
        drop(lock);
        let next_import = ImportLock::acquire(&path)?;
        drop(inherited);
        drop(next_import);
        Ok(())
    }
}

#[cfg(all(test, unix))]
#[test]
fn failed_or_unwinding_import_releases_duplicated_lock() -> Result<()> {
    for unwind in [false, true] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("import.lock");
        let mut inherited = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            let lock = ImportLock::acquire(&path)?;
            inherited = Some(lock.0.try_clone()?);
            if unwind {
                panic!("controlled importer unwind");
            }
            bail!("controlled importer failure")
        }));
        if unwind {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_err());
        }
        let next_import = ImportLock::acquire(&path)?;
        drop(inherited);
        // Closing the old duplicate must not release the new operation's lock.
        assert!(ImportLock::acquire(&path).is_err());
        drop(next_import);
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn catalog_connections_use_full_durable_wal_commits() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let catalog = Catalog::open(temporary.path())?;
    assert_eq!(
        catalog
            .db
            .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?,
        "wal"
    );
    for (name, expected) in [
        ("synchronous", 2),
        ("fullfsync", 1),
        ("foreign_keys", 1),
        ("cache_size", -262144),
        ("mmap_size", 0),
        ("temp_store", 1),
        ("busy_timeout", 5000),
        ("wal_autocheckpoint", 1000),
    ] {
        let actual: i64 = catalog
            .db
            .pragma_query_value(None, name, |row| row.get(0))?;
        assert_eq!(actual, expected, "{name}");
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn measured_settings_preserve_existing_nonempty_v1_catalog() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    fs::create_dir(root.join("previews"))?;
    let preview = b"opaque stored preview fixture";
    let hash = blake3::hash(preview).to_hex().to_string();
    fs::write(root.join("previews").join(format!("{hash}.jpg")), preview)?;
    let db = Connection::open(root.join("catalog.sqlite3"))?;
    // Build the previous v1 schema independently, without Catalog::open or the new helper.
    db.execute_batch("CREATE TABLE assets (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
        id TEXT NOT NULL UNIQUE, location BLOB NOT NULL UNIQUE, path_display TEXT NOT NULL,
        fingerprint TEXT, state TEXT NOT NULL CHECK(state IN ('pending','ready','failed')),
        metadata TEXT, preview_hash TEXT, error TEXT,
        CHECK(state != 'ready' OR (metadata IS NOT NULL AND preview_hash IS NOT NULL AND fingerprint IS NOT NULL))
        ); PRAGMA application_id=1346913089; PRAGMA user_version=1;")?;
    let metadata = r#"{"format":"JPEG","width":120,"height":80,"orientation":1,"camera_make":null,"camera_model":null,"captured_at":null,"preview_source":"fixture","opaque":"preserve this exact JSON"}"#;
    let location = [0_u8, 255, 10, 120];
    db.execute("INSERT INTO assets VALUES(41,'stable-ready',?1,'/offline/日本語.jpg','fingerprint','ready',?2,?3,NULL)", params![location.as_slice(),metadata,hash])?;
    db.execute("INSERT INTO assets(sequence,id,location,path_display,state,error) VALUES(80,'stable-pending',X'00AB','/offline/pending.CR2','pending','retry after interruption')", [])?;
    let schema_before: String = db.query_row(
        "SELECT sql FROM sqlite_master WHERE name='assets'",
        [],
        |row| row.get(0),
    )?;
    drop(db);
    for _ in 0..2 {
        let catalog = Catalog::open(root)?;
        let rows = catalog.browse(0, 200)?;
        assert_eq!(
            rows.iter()
                .map(|asset| (asset.sequence, asset.id.as_str()))
                .collect::<Vec<_>>(),
            [(41, "stable-ready"), (80, "stable-pending")]
        );
        assert_eq!(catalog.get("stable-ready")?.metadata.unwrap().width, 120);
        assert_eq!(catalog.preview("stable-ready")?, preview);
        assert_eq!(
            catalog.get("stable-pending")?.error.as_deref(),
            Some("retry after interruption")
        );
        let stored: (Vec<u8>, String, String) = catalog.db.query_row(
            "SELECT location,metadata,preview_hash FROM assets WHERE id='stable-ready'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(
            stored,
            (location.to_vec(), metadata.to_string(), hash.clone())
        );
        let schema_after: String = catalog.db.query_row(
            "SELECT sql FROM sqlite_master WHERE name='assets'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            schema_after.replace(", render_generation INTEGER NOT NULL DEFAULT 0", ""),
            schema_before
        );
        assert_eq!(
            catalog
                .db
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
            3
        );
        assert_eq!(
            catalog
                .db
                .pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))?,
            1346913089
        );
        assert_eq!(
            catalog
                .db
                .pragma_query_value(None, "cache_size", |row| row.get::<_, i64>(0))?,
            -262144
        );
        assert_eq!(
            catalog.db.query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='assets'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            80
        );
    }
    Ok(())
}
