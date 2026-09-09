//! Rebuildable preview manifest. Main-catalog generations remain authoritative.
#[path = "relocation.rs"]
mod relocation;
use super::{CodecSettings, PREPARATION_VERSION};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
pub use relocation::RelocationProgress;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Thumbnail,
    Large,
}
impl Tier {
    fn name(self) -> &'static str {
        match self {
            Self::Thumbnail => "thumbnail",
            Self::Large => "large",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PreviewKey {
    pub asset_id: String,
    pub variant_id: String,
    /// Obtained from Catalog::render_identity, never advanced by this manifest.
    pub generation: u64,
    pub fingerprint: String,
    pub edit_revision: u64,
    pub renderer_version: String,
    pub preparation_version: String,
    pub tier: Tier,
    pub edge: u32,
    pub encoding: CodecSettings,
}
impl PreviewKey {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.asset_id.is_empty() && self.asset_id.len() <= 256 && self.variant_id.len() <= 256,
            "invalid stable identity"
        );
        ensure!(
            self.generation <= i64::MAX as u64 && self.edit_revision <= i64::MAX as u64,
            "revision overflow"
        );
        ensure!(
            self.fingerprint.len() == 64 && self.fingerprint.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid source fingerprint"
        );
        ensure!(
            !self.renderer_version.is_empty()
                && self.renderer_version.len() <= 128
                && self.preparation_version == PREPARATION_VERSION,
            "render/preparation identity mismatch"
        );
        ensure!((1..=8192).contains(&self.edge), "preview edge limit");
        self.encoding.validate()
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(blake3::hash(&serde_json::to_vec(self)?)
            .to_hex()
            .to_string())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    Flat,
    HashPrefix,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    pub manifest_root: PathBuf,
    pub layout: Layout,
    pub thumbnail_root: PathBuf,
    pub large_root: PathBuf,
    /// Encoded object bytes, including pending writes; database/filesystem overhead is reported separately.
    pub thumbnail_bytes: u64,
    pub large_bytes: u64,
}
/// The metadata/provenance belong to these pixels, including an explicitly stale
/// retained fallback. General catalog metadata can independently be newer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderRecord {
    pub width: u32,
    pub height: u32,
    pub metadata: crate::media::Metadata,
    pub provenance: crate::media::RenderProvenance,
}
impl RenderRecord {
    fn encoded(&self, key: &PreviewKey) -> Result<String> {
        ensure!(
            self.width > 0 && self.height > 0 && self.width <= key.edge && self.height <= key.edge,
            "render record dimensions exceed tier"
        );
        let value = serde_json::to_string(self)?;
        ensure!(value.len() <= 64 * 1024, "render record size limit");
        Ok(value)
    }
}
#[derive(Debug)]
pub struct CachedPreview {
    pub key: PreviewKey,
    pub bytes: Vec<u8>,
    pub stale: bool,
    pub record: Option<RenderRecord>,
}
#[derive(Debug, Clone, Copy, Serialize)]
pub struct StoreUsage {
    pub thumbnail_bytes: u64,
    pub large_bytes: u64,
    pub pending_objects: u64,
    pub objects: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    Attached,
    AlreadyPresent,
    Stale,
}
#[derive(Debug)]
pub struct CacheQuotaExceeded(pub &'static str);
impl std::fmt::Display for CacheQuotaExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for CacheQuotaExceeded {}

/// One owner serializes filesystem/manifest mutations; workers return encoded
/// data to that owner. The process lock is released automatically on crash.
pub struct PreviewStore {
    db: Connection,
    config: StoreConfig,
    _lock: File,
    _tier_locks: [File; 2],
    _relocation_lock: Option<File>,
    identity: String,
    clock: Cell<i64>,
    touches: RefCell<HashMap<String, i64>>,
}
impl PreviewStore {
    pub fn open(mut config: StoreConfig, original_roots: &[PathBuf]) -> Result<Self> {
        ensure!(
            config.thumbnail_bytes > 0
                && config.large_bytes > 0
                && config.thumbnail_bytes <= i64::MAX as u64
                && config.large_bytes <= i64::MAX as u64,
            "invalid cache quotas"
        );
        let roots = [
            prospective(&config.manifest_root)?,
            prospective(&config.thumbnail_root)?,
            prospective(&config.large_root)?,
        ];
        for source in original_roots {
            let source = prospective(source)?;
            for root in &roots {
                ensure!(
                    !root.starts_with(&source) && !source.starts_with(root),
                    "preview storage overlaps originals"
                );
            }
        }
        for (index, left) in roots.iter().enumerate() {
            for right in &roots[index + 1..] {
                ensure!(
                    !left.starts_with(right) && !right.starts_with(left),
                    "manifest, thumbnail and large storage must be separate"
                );
            }
        }
        for root in &roots {
            fs::create_dir_all(root)?;
        }
        config.manifest_root = fs::canonicalize(&roots[0])?;
        config.thumbnail_root = fs::canonicalize(&roots[1])?;
        config.large_root = fs::canonicalize(&roots[2])?;
        let lock = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(config.manifest_root.join("preview.lock"))?;
        lock.try_lock_exclusive()
            .context("preview service already owns this cache")?;
        let db = Connection::open(config.manifest_root.join("previews.sqlite3"))?;
        let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            let count: i64 = db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )?;
            ensure!(count == 0 && app == 0, "unrelated preview manifest");
        } else {
            ensure!(
                (1..=2).contains(&version) && app == 0x50435056,
                "unsupported preview manifest"
            );
        }
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA fullfsync=ON; PRAGMA cache_size=-8192; PRAGMA mmap_size=0; BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS usage(tier TEXT PRIMARY KEY,bytes INTEGER NOT NULL CHECK(bytes>=0),objects INTEGER NOT NULL CHECK(objects>=0),pending INTEGER NOT NULL CHECK(pending>=0));
            INSERT OR IGNORE INTO usage VALUES('thumbnail',0,0,0),('large',0,0,0);
            CREATE TABLE IF NOT EXISTS objects(key TEXT PRIMARY KEY,descriptor TEXT NOT NULL,tier TEXT NOT NULL,bytes INTEGER NOT NULL CHECK(bytes>0),checksum TEXT NOT NULL,status TEXT NOT NULL CHECK(status IN ('pending','ready','orphan')),temporary TEXT NOT NULL,touched INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS render_jobs(id TEXT PRIMARY KEY,descriptor TEXT NOT NULL,created INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS render_jobs_created ON render_jobs(created,id);
            CREATE TABLE IF NOT EXISTS store_identity(id INTEGER PRIMARY KEY CHECK(id=1),value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS budgets(tier TEXT PRIMARY KEY,bytes INTEGER NOT NULL CHECK(bytes>0));
            CREATE TABLE IF NOT EXISTS relocations(tier TEXT PRIMARY KEY,id TEXT NOT NULL,source TEXT NOT NULL,target TEXT NOT NULL,phase TEXT NOT NULL,cursor TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS render_records(key TEXT PRIMARY KEY REFERENCES objects(key) ON DELETE CASCADE,record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS objects_eviction ON objects(tier,status,touched,key);
            CREATE INDEX IF NOT EXISTS objects_recovery ON objects(status,key);
            CREATE TABLE IF NOT EXISTS counter(id INTEGER PRIMARY KEY CHECK(id=1),value INTEGER NOT NULL);
            INSERT OR IGNORE INTO counter VALUES(1,0);
            CREATE TABLE IF NOT EXISTS locations(tier TEXT PRIMARY KEY,path TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS layout(name TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS wanted(asset TEXT NOT NULL,variant TEXT NOT NULL,tier TEXT NOT NULL,generation INTEGER NOT NULL,desired TEXT NOT NULL,current TEXT,PRIMARY KEY(asset,variant,tier));
            CREATE INDEX IF NOT EXISTS wanted_current ON wanted(current);
            CREATE TRIGGER IF NOT EXISTS render_job_added AFTER INSERT ON render_jobs BEGIN UPDATE counter SET value=max(value,NEW.created) WHERE id=1; END;
            CREATE TRIGGER IF NOT EXISTS object_added AFTER INSERT ON objects BEGIN UPDATE usage SET bytes=bytes+NEW.bytes,objects=objects+1,pending=pending+(NEW.status='pending') WHERE tier=NEW.tier; UPDATE counter SET value=max(value,NEW.touched) WHERE id=1; END;
            CREATE TRIGGER IF NOT EXISTS object_touched AFTER UPDATE OF touched ON objects BEGIN UPDATE counter SET value=max(value,NEW.touched) WHERE id=1; END;
            CREATE TRIGGER IF NOT EXISTS object_removed AFTER DELETE ON objects BEGIN UPDATE usage SET bytes=bytes-OLD.bytes,objects=objects-1,pending=pending-(OLD.status='pending') WHERE tier=OLD.tier; END;
            CREATE TRIGGER IF NOT EXISTS object_status AFTER UPDATE OF status ON objects BEGIN UPDATE usage SET pending=pending+(NEW.status='pending')-(OLD.status='pending') WHERE tier=NEW.tier; END;
            PRAGMA application_id=1346588758; PRAGMA user_version=2; COMMIT;")?;
        let clock = db.query_row("SELECT value FROM counter WHERE id=1", [], |r| r.get(0))?;
        db.execute(
            "INSERT OR IGNORE INTO store_identity VALUES(1,?1)",
            [uuid::Uuid::new_v4().to_string()],
        )?;
        let identity: String =
            db.query_row("SELECT value FROM store_identity WHERE id=1", [], |r| {
                r.get(0)
            })?;
        let tier_locks = [
            relocation::lock_root(
                &config.thumbnail_root,
                &identity,
                Tier::Thumbnail,
                config.layout,
            )?,
            relocation::lock_root(&config.large_root, &identity, Tier::Large, config.layout)?,
        ];
        let mut store = Self {
            db,
            config,
            _lock: lock,
            _tier_locks: tier_locks,
            _relocation_lock: None,
            identity,
            clock: Cell::new(clock),
            touches: RefCell::new(HashMap::new()),
        };
        let layout = serde_json::to_string(&store.config.layout)?;
        let existing: Option<String> = store
            .db
            .query_row("SELECT name FROM layout", [], |r| r.get(0))
            .optional()?;
        if let Some(existing) = existing {
            ensure!(
                existing == layout,
                "cache layout changed; explicit migration required"
            );
        } else {
            store
                .db
                .execute("INSERT INTO layout VALUES(?1)", [layout])?;
        }
        for tier in [Tier::Thumbnail, Tier::Large] {
            let path = store
                .root(tier)
                .to_str()
                .context("cache location must be Unicode")?;
            let saved: Option<String> = store
                .db
                .query_row(
                    "SELECT path FROM locations WHERE tier=?1",
                    [tier.name()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(saved) = saved {
                ensure!(
                    saved == path,
                    "cache location changed; relocate the retained objects explicitly before reopening"
                );
            } else {
                store.db.execute(
                    "INSERT INTO locations VALUES(?1,?2)",
                    params![tier.name(), path],
                )?;
            }
        }
        store.db.execute(
            "INSERT OR IGNORE INTO budgets VALUES('thumbnail',?1)",
            [store.config.thumbnail_bytes as i64],
        )?;
        store.db.execute(
            "INSERT OR IGNORE INTO budgets VALUES('large',?1)",
            [store.config.large_bytes as i64],
        )?;
        store.recover_relocation_lock()?;
        store.recover(128)?;
        Ok(store)
    }
    pub fn configuration(&self) -> &StoreConfig {
        &self.config
    }
    fn root(&self, tier: Tier) -> &Path {
        match tier {
            Tier::Thumbnail => &self.config.thumbnail_root,
            Tier::Large => &self.config.large_root,
        }
    }
    fn path(&self, key: &str, tier: Tier) -> Result<PathBuf> {
        ensure!(
            key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid manifest object key"
        );
        Ok(match self.config.layout {
            Layout::Flat => self.root(tier).join(key),
            Layout::HashPrefix => self.root(tier).join(&key[..2]).join(&key[2..4]).join(key),
        })
    }
    fn ensure_parent(&self, tier: Tier, key: &str) -> Result<()> {
        if self.config.layout == Layout::Flat {
            return Ok(());
        }
        let mut parent = self.root(tier).to_owned();
        for part in [&key[..2], &key[2..4]] {
            let next = parent.join(part);
            match fs::create_dir(&next) {
                Ok(()) => {
                    #[cfg(unix)]
                    File::open(&parent)?.sync_all()?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => ensure!(
                    fs::symlink_metadata(&next)?.file_type().is_dir(),
                    "cache shard is not a real directory"
                ),
                Err(error) => return Err(error.into()),
            }
            parent = next;
        }
        Ok(())
    }
    fn clock(&self) -> Result<i64> {
        let next = self
            .clock
            .get()
            .checked_add(1)
            .context("cache clock overflow")?;
        self.clock.set(next);
        Ok(next)
    }
    /// Recency is auxiliary and batched, never one durable write per thumbnail.
    /// Service maintenance flushes at page boundaries; eviction flushes first.
    pub fn flush_touches(&self) -> Result<()> {
        if self.touches.borrow().is_empty() {
            return Ok(());
        }
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            let mut update = self
                .db
                .prepare("UPDATE objects SET touched=?2 WHERE key=?1")?;
            for (key, touched) in self.touches.borrow().iter() {
                update.execute(params![key, touched])?;
            }
            Ok(())
        })();
        finish(&self.db, result)?;
        self.touches.borrow_mut().clear();
        Ok(())
    }
    /// The service owner registers a fresh catalog identity and its currently
    /// selected preview policy. Workers may publish, but must never register keys.
    /// Policy changes can replace the desired key without inventing a photo edit.
    pub fn desire(&self, key: &PreviewKey, authority: impl Fn() -> Result<bool>) -> Result<()> {
        let digest = key.digest()?;
        ensure!(authority()?, "stale catalog identity");
        let old: Option<(i64, String)> = self
            .db
            .query_row(
                "SELECT generation,desired FROM wanted WHERE asset=?1 AND variant=?2 AND tier=?3",
                params![key.asset_id, key.variant_id, key.tier.name()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((generation, _)) = old {
            ensure!(
                key.generation >= generation as u64,
                "stale catalog generation"
            );
        }
        self.db.execute("INSERT INTO wanted VALUES(?1,?2,?3,?4,?5,NULL) ON CONFLICT(asset,variant,tier) DO UPDATE SET generation=excluded.generation,desired=excluded.desired",params![key.asset_id,key.variant_id,key.tier.name(),key.generation as i64,digest])?;
        Ok(())
    }
    fn desired(&self, key: &PreviewKey, digest: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT desired=?4 AND generation=?5 FROM wanted WHERE asset=?1 AND variant=?2 AND tier=?3",params![key.asset_id,key.variant_id,key.tier.name(),digest,key.generation as i64],|r|r.get::<_,bool>(0)).optional()?.unwrap_or(false))
    }
    pub fn usage(&self) -> Result<StoreUsage> {
        let used = |tier: &str| {
            self.db
                .query_row("SELECT bytes FROM usage WHERE tier=?1", [tier], |r| {
                    unsigned(r, 0)
                })
        };
        Ok(StoreUsage {
            thumbnail_bytes: used("thumbnail")?,
            large_bytes: used("large")?,
            pending_objects: self.db.query_row(
                "SELECT coalesce(sum(pending),0) FROM usage",
                [],
                |r| unsigned(r, 0),
            )?,
            objects: self
                .db
                .query_row("SELECT coalesce(sum(objects),0) FROM usage", [], |r| {
                    unsigned(r, 0)
                })?,
        })
    }
    fn make_room(&self, tier: Tier, bytes: u64) -> Result<()> {
        if tier == Tier::Large {
            self.flush_touches()?;
        }
        let budget = match tier {
            Tier::Thumbnail => self.config.thumbnail_bytes,
            Tier::Large => self.config.large_bytes,
        };
        if bytes > budget {
            return Err(CacheQuotaExceeded("preview exceeds tier quota").into());
        }
        for _ in 0..128 {
            let used: u64 = self.db.query_row(
                "SELECT bytes FROM usage WHERE tier=?1",
                [tier.name()],
                |r| unsigned(r, 0),
            )?;
            if used <= budget - bytes {
                return Ok(());
            }
            if tier != Tier::Large {
                return Err(CacheQuotaExceeded(
                    "retained thumbnail quota exhausted; previous thumbnails preserved",
                )
                .into());
            }
            let oldest:Option<String>=self.db.query_row("SELECT key FROM objects WHERE tier='large' AND status='ready' ORDER BY touched,key LIMIT 1",[],|r|r.get(0)).optional()?;
            let Some(oldest) = oldest else {
                return Err(
                    CacheQuotaExceeded("large-preview quota reserved by pending work").into(),
                );
            };
            self.remove(&oldest, tier)?;
        }
        Err(CacheQuotaExceeded(
            "large-cache eviction batch exhausted; retry on a later service tick",
        )
        .into())
    }
    fn remove(&self, digest: &str, tier: Tier) -> Result<()> {
        self.touches.borrow_mut().remove(digest);
        let temporary: Option<String> = self
            .db
            .query_row(
                "SELECT temporary FROM objects WHERE key=?1",
                [digest],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(temporary) = temporary {
            ensure!(
                temporary.starts_with(digest)
                    && temporary.ends_with(".pending")
                    && !temporary.contains('/')
                    && !temporary.contains('\\'),
                "invalid temporary object name"
            );
            let destination = self.path(digest, tier)?;
            match fs::remove_file(destination.parent().unwrap().join(temporary)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        match fs::remove_file(self.path(digest, tier)?) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            self.db
                .execute("UPDATE wanted SET current=NULL WHERE current=?1", [digest])?;
            self.db
                .execute("DELETE FROM objects WHERE key=?1", [digest])?;
            Ok(())
        })();
        finish(&self.db, result)
    }
    /// Stage outside catalog locks, then ask the catalog authority to run a short
    /// attachment closure while holding its revision transaction. The authority
    /// must return Stale without calling the closure if the identity changed.
    /// This avoids a check-then-attach race between independent SQLite stores.
    pub fn publish(
        &self,
        key: &PreviewKey,
        bytes: &[u8],
        authorize: impl FnOnce(&mut dyn FnMut() -> Result<Publication>) -> Result<Publication>,
    ) -> Result<Publication> {
        self.publish_controlled(key, bytes, authorize, || Ok(()))
    }
    pub fn publish_record(
        &self,
        key: &PreviewKey,
        bytes: &[u8],
        record: &RenderRecord,
        authorize: impl FnOnce(&mut dyn FnMut() -> Result<Publication>) -> Result<Publication>,
    ) -> Result<Publication> {
        self.publish_inner(key, bytes, Some(record), authorize, || Ok(()))
    }
    fn publish_controlled(
        &self,
        key: &PreviewKey,
        bytes: &[u8],
        authorize: impl FnOnce(&mut dyn FnMut() -> Result<Publication>) -> Result<Publication>,
        before_write: impl FnOnce() -> Result<()>,
    ) -> Result<Publication> {
        self.publish_inner(key, bytes, None, authorize, before_write)
    }
    fn publish_inner(
        &self,
        key: &PreviewKey,
        bytes: &[u8],
        record: Option<&RenderRecord>,
        authorize: impl FnOnce(&mut dyn FnMut() -> Result<Publication>) -> Result<Publication>,
        before_write: impl FnOnce() -> Result<()>,
    ) -> Result<Publication> {
        ensure!(!self.relocation_pending()?, "cache relocation in progress");
        let digest = key.digest()?;
        let record = record.map(|record| record.encoded(key)).transpose()?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= 256 * 1024 * 1024,
            "encoded preview size limit"
        );
        if !self.desired(key, &digest)? {
            return Ok(Publication::Stale);
        }
        if self.current_is_intact(key)? && (record.is_none() || self.render_record(key)?.is_some())
        {
            return authorize(&mut || {
                if self.desired(key, &digest)? {
                    Ok(Publication::AlreadyPresent)
                } else {
                    Ok(Publication::Stale)
                }
            });
        }
        let status: Option<String> = self
            .db
            .query_row("SELECT status FROM objects WHERE key=?1", [&digest], |r| {
                r.get(0)
            })
            .optional()?;
        if status.is_some() {
            self.remove(&digest, key.tier)?;
        }
        let destination = self.path(&digest, key.tier)?;
        let parent = destination.parent().unwrap();
        self.ensure_parent(key.tier, &digest)?;
        ensure!(
            !destination.exists(),
            "untracked immutable destination requires recovery"
        );
        self.make_room(key.tier, bytes.len() as u64)?;
        let temporary = format!("{}.{}.pending", digest, uuid::Uuid::new_v4());
        let temporary_path = parent.join(&temporary);
        let checksum = blake3::hash(bytes).to_hex().to_string();
        self.db.execute(
            "INSERT INTO objects VALUES(?1,?2,?3,?4,?5,'pending',?6,?7)",
            params![
                digest,
                serde_json::to_string(key)?,
                key.tier.name(),
                bytes.len() as i64,
                checksum,
                temporary,
                self.clock()?
            ],
        )?;
        let written = (|| -> Result<()> {
            let mut file = File::options()
                .write(true)
                .create_new(true)
                .open(&temporary_path)?;
            before_write()?;
            file.write_all(bytes)?;
            file.sync_all()?;
            ensure!(
                !destination.exists(),
                "immutable preview destination already exists"
            );
            fs::rename(&temporary_path, &destination)?;
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary_path);
            self.remove(&digest, key.tier)?;
            return Err(error.context("preview staging failed; prior current thumbnail retained"));
        }
        let mut previous = None;
        let mut called = false;
        let mut attachment = || -> Result<Publication> {
            ensure!(!called, "attachment may run only once");
            called = true;
            if !self.desired(key, &digest)? {
                return Ok(Publication::Stale);
            }
            previous = self.db.query_row(
                "SELECT current FROM wanted WHERE asset=?1 AND variant=?2 AND tier=?3",
                params![key.asset_id, key.variant_id, key.tier.name()],
                |r| r.get::<_, Option<String>>(0),
            )?;
            self.db.execute_batch("BEGIN IMMEDIATE")?;
            let result = (|| -> Result<()> {
                if let Some(old) = &previous
                    && old != &digest
                {
                    self.db
                        .execute("UPDATE objects SET status='orphan' WHERE key=?1", [old])?;
                }
                if let Some(record) = &record {
                    self.db.execute(
                        "INSERT INTO render_records VALUES(?1,?2)",
                        params![digest, record],
                    )?;
                }
                self.db
                    .execute("UPDATE objects SET status='ready' WHERE key=?1", [&digest])?;
                let changed=self.db.execute("UPDATE wanted SET current=?4 WHERE asset=?1 AND variant=?2 AND tier=?3 AND desired=?4 AND generation=?5",params![key.asset_id,key.variant_id,key.tier.name(),digest,key.generation as i64])?;
                ensure!(changed == 1, "stale preview attachment");
                Ok(())
            })();
            finish(&self.db, result)?;
            Ok(Publication::Attached)
        };
        let outcome = authorize(&mut attachment);
        match outcome {
            Ok(Publication::Attached) => {
                ensure!(called, "authority did not run attachment");
                // Cleanup is outside the short catalog authority transaction.
                if let Some(old) = previous
                    && old != digest
                {
                    let _ = self.remove(&old, key.tier);
                }
                Ok(Publication::Attached)
            }
            other => {
                // A catalog commit failure after attachment must not delete the
                // newly attached file: current-key validation remains mandatory.
                let ready: bool = self.db.query_row(
                    "SELECT status='ready' FROM objects WHERE key=?1",
                    [&digest],
                    |r| r.get(0),
                )?;
                if !ready {
                    self.remove(&digest, key.tier)?;
                }
                other
            }
        }
    }
    /// Durable queue records are bounded by the service's request admission. An
    /// import job is removed only after its catalog ready transaction commits.
    pub(crate) fn save_job(&self, id: &str, descriptor: &str, limit: usize) -> Result<()> {
        ensure!(
            id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid job identity"
        );
        ensure!(
            descriptor.len() <= 64 * 1024 && (1..=100_000).contains(&limit),
            "job journal limit"
        );
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM render_jobs WHERE id=?1)",
            [id],
            |r| r.get(0),
        )?;
        if !exists {
            let count: usize = self
                .db
                .query_row("SELECT count(*) FROM render_jobs", [], |r| r.get(0))?;
            ensure!(count < limit, "durable preview queue full");
        }
        self.db.execute("INSERT INTO render_jobs VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET descriptor=excluded.descriptor",params![id,descriptor,self.clock()?])?;
        Ok(())
    }
    pub(crate) fn saved_jobs(
        &self,
        after: i64,
        limit: usize,
    ) -> Result<Vec<(i64, String, String)>> {
        ensure!((1..=1000).contains(&limit), "job recovery page limit");
        let mut statement=self.db.prepare("SELECT created,id,descriptor FROM render_jobs WHERE created>?1 ORDER BY created,id LIMIT ?2")?;
        Ok(statement
            .query_map(params![after, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub(crate) fn finish_job(&self, id: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM render_jobs WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn render_record(&self, key: &PreviewKey) -> Result<Option<RenderRecord>> {
        let value: Option<String> = self.db.query_row(
            "SELECT r.record FROM render_records r JOIN objects o ON o.key=r.key WHERE r.key=?1 AND o.status='ready'",
            [key.digest()?], |r| r.get(0),
        ).optional()?;
        value
            .map(|value| {
                ensure!(value.len() <= 64 * 1024, "render record size limit");
                let record: RenderRecord = serde_json::from_str(&value)?;
                record.encoded(key)?;
                Ok(record)
            })
            .transpose()
    }
    /// Invalid image data cannot remain a permanent cache hit. The service passes
    /// the actual returned key, including when an old fallback failed decoding.
    pub fn invalidate(&self, key: &PreviewKey) -> Result<()> {
        self.remove(&key.digest()?, key.tier)
    }
    /// Check immutable current-object integrity with fixed-size scratch. Duplicate
    /// publication must not allocate a second encoded object beside the incoming one.
    pub fn current_is_intact(&self, expected: &PreviewKey) -> Result<bool> {
        let digest = expected.digest()?;
        let row: Option<(u64, String)> = self.db.query_row(
            "SELECT o.bytes,o.checksum FROM wanted w JOIN objects o ON o.key=w.current WHERE w.asset=?1 AND w.variant=?2 AND w.tier=?3 AND o.key=?4 AND o.status='ready'",
            params![expected.asset_id, expected.variant_id, expected.tier.name(), digest],
            |r| Ok((unsigned(r, 0)?, r.get(1)?)),
        ).optional()?;
        let Some((len, checksum)) = row else {
            return Ok(false);
        };
        let valid = (|| -> Result<bool> {
            let mut file = File::open(self.path(&digest, expected.tier)?)?;
            if len > 256 * 1024 * 1024 || file.metadata()?.len() != len {
                return Ok(false);
            }
            let mut hash = blake3::Hasher::new();
            let mut scratch = [0; 16384];
            let mut total = 0u64;
            loop {
                let count = file.read(&mut scratch)?;
                if count == 0 {
                    break;
                }
                total += count as u64;
                if total > len {
                    return Ok(false);
                }
                hash.update(&scratch[..count]);
            }
            Ok(total == len && hash.finalize().to_hex().as_str() == checksum)
        })();
        match valid {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.remove(&digest, expected.tier)?;
                Ok(false)
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                self.remove(&digest, expected.tier)?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
    /// Only a thumbnail may be returned as an explicitly stale offline fallback.
    /// The caller provides a fresh catalog identity on each visible request.
    pub fn read(&self, expected: &PreviewKey, allow_stale: bool) -> Result<Option<CachedPreview>> {
        self.read_limited(expected, allow_stale, 256 * 1024 * 1024)
    }
    /// Reject over-budget objects before allocating their bytes. A serial service
    /// reader passes its remaining encoded staging allowance here.
    pub fn read_limited(
        &self,
        expected: &PreviewKey,
        allow_stale: bool,
        max_bytes: u64,
    ) -> Result<Option<CachedPreview>> {
        expected.validate()?;
        let row:Option<(String,String,u64,String)>=self.db.query_row("SELECT o.key,o.descriptor,o.bytes,o.checksum FROM wanted w JOIN objects o ON o.key=w.current WHERE w.asset=?1 AND w.variant=?2 AND w.tier=?3 AND o.status='ready'",params![expected.asset_id,expected.variant_id,expected.tier.name()],|r|Ok((r.get(0)?,r.get(1)?,unsigned(r,2)?,r.get(3)?))).optional()?;
        let Some((digest, descriptor, len, checksum)) = row else {
            return Ok(None);
        };
        let key: PreviewKey = serde_json::from_str(&descriptor)?;
        let stale = key != *expected;
        if stale && !(allow_stale && expected.tier == Tier::Thumbnail) {
            return Ok(None);
        }
        ensure!(
            key.digest()? == digest
                && key.asset_id == expected.asset_id
                && key.variant_id == expected.variant_id
                && key.tier == expected.tier,
            "manifest descriptor identity mismatch"
        );
        let mut file = match File::open(self.path(&digest, key.tier)?) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.remove(&digest, key.tier)?;
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        if len > 256 * 1024 * 1024 || file.metadata()?.len() != len {
            drop(file);
            self.remove(&digest, key.tier)?;
            bail!("cached preview length mismatch; entry invalidated");
        }
        ensure!(
            len <= max_bytes,
            "encoded staging allowance exceeded before allocation"
        );
        let mut bytes = vec![0; len as usize];
        let read = file.read_exact(&mut bytes);
        let mut extra = [0];
        if read.is_err()
            || file.read(&mut extra)? != 0
            || blake3::hash(&bytes).to_hex().as_str() != checksum
        {
            self.remove(&digest, key.tier)?;
            bail!("cached preview checksum mismatch; entry invalidated");
        }
        self.touches.borrow_mut().insert(digest, self.clock()?);
        if self.touches.borrow().len() >= 256 {
            self.flush_touches()?;
        }
        let record = self.render_record(&key)?;
        Ok(Some(CachedPreview {
            key,
            bytes,
            stale,
            record,
        }))
    }
    /// Bounded maintenance; startup performs one batch, subsequent service ticks
    /// can finish remaining pending/orphan cleanup without loading the catalog.
    pub fn recover(&self, limit: usize) -> Result<usize> {
        ensure!((1..=1024).contains(&limit), "recovery batch limit");
        let mut query=self.db.prepare("SELECT o.key,o.descriptor,o.temporary FROM objects o WHERE o.status IN ('pending','orphan') ORDER BY o.status,o.key LIMIT ?1")?;
        let rows = query
            .query_map([limit as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        for (digest, descriptor, temporary) in &rows {
            let key: PreviewKey = serde_json::from_str(descriptor)?;
            ensure!(
                !temporary.contains('/') && !temporary.contains('\\'),
                "invalid temporary cache name"
            );
            let parent = self.path(digest, key.tier)?.parent().unwrap().to_owned();
            match fs::remove_file(parent.join(temporary)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            self.remove(digest, key.tier)?;
        }
        Ok(rows.len())
    }
}
fn unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    value.try_into().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn finish(db: &Connection, result: Result<()>) -> Result<()> {
    match result {
        Ok(()) => match db.execute_batch("COMMIT") {
            Ok(()) => Ok(()),
            Err(error) => {
                // Deferred constraints and some I/O failures leave the
                // transaction open. Never leak provisional state into reuse.
                if !db.is_autocommit() {
                    let _ = db.execute_batch("ROLLBACK");
                }
                Err(error.into())
            }
        },
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}
fn prospective(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut base = absolute.as_path();
    let mut missing = Vec::new();
    while !base.exists() {
        missing.push(base.file_name().context("invalid cache path")?.to_owned());
        base = base.parent().context("invalid cache ancestor")?;
    }
    let mut resolved = fs::canonicalize(base)?;
    for part in missing.into_iter().rev() {
        ensure!(
            part != ".." && part != ".",
            "cache path contains unresolved traversal"
        );
        resolved.push(part);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_commit_rolls_back_provisional_state_and_allows_reuse() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON;
            CREATE TABLE parent(id INTEGER PRIMARY KEY);
            CREATE TABLE publication(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED,state TEXT);
            CREATE TABLE job(id INTEGER PRIMARY KEY);
            INSERT INTO job VALUES(1);
            BEGIN IMMEDIATE;
            INSERT INTO publication VALUES(42,'ready');
            DELETE FROM job;").unwrap();
        let error = finish(&db, Ok(())).unwrap_err();
        assert!(error.to_string().contains("FOREIGN KEY"));
        assert!(db.is_autocommit());
        assert_eq!(
            db.query_row("SELECT count(*) FROM publication", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM job", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        db.execute_batch("BEGIN IMMEDIATE; INSERT INTO parent VALUES(42); INSERT INTO publication VALUES(42,'ready'); DELETE FROM job;").unwrap();
        finish(&db, Ok(())).unwrap();
        assert!(db.is_autocommit());
        assert_eq!(
            db.query_row("SELECT count(*) FROM publication", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn every_pair_of_cache_roots_must_be_disjoint_before_creation() {
        for left in 0..3 {
            for right in 0..3 {
                if left == right {
                    continue;
                }
                for nested in [false, true] {
                    let root = tempfile::tempdir().unwrap();
                    let mut roots = [
                        root.path().join("manifest"),
                        root.path().join("thumb"),
                        root.path().join("large"),
                    ];
                    roots[right] = if nested {
                        roots[left].join("nested")
                    } else {
                        roots[left].clone()
                    };
                    let cfg = StoreConfig {
                        manifest_root: roots[0].clone(),
                        thumbnail_root: roots[1].clone(),
                        large_root: roots[2].clone(),
                        layout: Layout::HashPrefix,
                        thumbnail_bytes: 100,
                        large_bytes: 100,
                    };
                    assert!(PreviewStore::open(cfg, &[]).is_err());
                    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
                }
            }
        }
    }
    fn config(root: &Path, thumb: u64, large: u64) -> StoreConfig {
        StoreConfig {
            manifest_root: root.join("manifest"),
            layout: Layout::HashPrefix,
            thumbnail_root: root.join("thumb"),
            large_root: root.join("large"),
            thumbnail_bytes: thumb,
            large_bytes: large,
        }
    }
    fn key(generation: u64, tier: Tier) -> PreviewKey {
        PreviewKey {
            asset_id: "asset".into(),
            variant_id: "master".into(),
            generation,
            fingerprint: "a".repeat(64),
            edit_revision: generation,
            renderer_version: "test-1".into(),
            preparation_version: PREPARATION_VERSION.into(),
            tier,
            edge: 256,
            encoding: CodecSettings {
                codec: super::super::Codec::Jpeg,
                quality: 65,
            },
        }
    }
    fn authority(attach: &mut dyn FnMut() -> Result<Publication>) -> Result<Publication> {
        attach()
    }
    #[test]
    fn offline_fallback_and_quota_preserve_previous_thumbnail() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path(), 10, 10);
        let store = PreviewStore::open(cfg.clone(), &[]).unwrap();
        let old = key(1, Tier::Thumbnail);
        store.desire(&old, || Ok(true)).unwrap();
        assert_eq!(
            store.publish(&old, b"oldbytes", authority).unwrap(),
            Publication::Attached
        );
        let new = key(2, Tier::Thumbnail);
        store.desire(&new, || Ok(true)).unwrap();
        assert!(store.read(&new, false).unwrap().is_none());
        let fallback = store.read(&new, true).unwrap().unwrap();
        assert!(fallback.stale);
        assert_eq!(fallback.bytes, b"oldbytes");
        assert!(store.publish(&new, b"newbytes", authority).is_err());
        assert_eq!(store.read(&new, true).unwrap().unwrap().bytes, b"oldbytes");
        drop(store);
        let store = PreviewStore::open(cfg, &[]).unwrap();
        assert!(store.read(&new, true).unwrap().unwrap().stale);
        assert_eq!(store.usage().unwrap().thumbnail_bytes, 8);
        assert!(store.desire(&old, || Ok(true)).is_err());
    }
    fn record(label: &str) -> RenderRecord {
        RenderRecord {
            width: 2,
            height: 1,
            metadata: crate::media::Metadata {
                format: "test".into(),
                width: 20,
                height: 10,
                orientation: 1,
                camera_make: None,
                camera_model: None,
                captured_at: None,
                lens: None,
                preview_source: label.into(),
            },
            provenance: crate::media::RenderProvenance {
                pipeline_version: label.into(),
                decoder: "test".into(),
                source_bits_per_channel: 16,
                source_color: "linear".into(),
                working_color: "linear sRGB".into(),
                alpha: "straight".into(),
                calibration: None,
                spatial_calibration: None,
                notes: vec![],
            },
        }
    }
    #[test]
    fn fallback_keeps_its_own_render_record_through_interrupted_replacement() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path(), 100, 100);
        let store = PreviewStore::open(cfg.clone(), &[]).unwrap();
        let old = key(1, Tier::Thumbnail);
        store.desire(&old, || Ok(true)).unwrap();
        store
            .publish_record(&old, b"old", &record("old renderer"), authority)
            .unwrap();
        let new = key(2, Tier::Thumbnail);
        store.desire(&new, || Ok(true)).unwrap();
        assert!(
            store
                .publish_record(&new, b"new", &record("new renderer"), |_| bail!(
                    "owner interrupted"
                ))
                .is_err()
        );
        drop(store);
        let store = PreviewStore::open(cfg, &[]).unwrap();
        let fallback = store.read(&new, true).unwrap().unwrap();
        assert!(fallback.stale);
        assert_eq!(
            fallback.record.unwrap().provenance.pipeline_version,
            "old renderer"
        );
        assert!(store.render_record(&new).unwrap().is_none());
        store
            .publish_record(&new, b"new", &record("new renderer"), authority)
            .unwrap();
        assert_eq!(
            store
                .read(&new, false)
                .unwrap()
                .unwrap()
                .record
                .unwrap()
                .metadata
                .preview_source,
            "new renderer"
        );
        assert!(store.render_record(&old).unwrap().is_none());
    }
    #[test]
    fn manifest_record_migration_retains_old_opaque_objects_without_inventing_provenance() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path(), 100, 100);
        let store = PreviewStore::open(cfg.clone(), &[]).unwrap();
        let key = key(1, Tier::Thumbnail);
        store.desire(&key, || Ok(true)).unwrap();
        store.publish(&key, b"old", authority).unwrap();
        store
            .db
            .execute_batch(
                "DROP TABLE render_records; DROP TABLE render_jobs; PRAGMA user_version=1;",
            )
            .unwrap();
        drop(store);
        let store = PreviewStore::open(cfg, &[]).unwrap();
        let old = store.read(&key, false).unwrap().unwrap();
        assert_eq!(old.bytes, b"old");
        assert!(old.record.is_none());
    }
    #[test]
    fn encoded_read_budget_rejects_without_invalidating_retained_thumbnail() {
        let root = tempfile::tempdir().unwrap();
        let store = PreviewStore::open(config(root.path(), 100, 100), &[]).unwrap();
        let key = key(1, Tier::Thumbnail);
        store.desire(&key, || Ok(true)).unwrap();
        store.publish(&key, b"eightbit", authority).unwrap();
        let error = store.read_limited(&key, false, 7).err().unwrap();
        assert!(error.to_string().contains("before allocation"));
        assert_eq!(store.usage().unwrap().thumbnail_bytes, 8);
        assert_eq!(
            store.read_limited(&key, false, 8).unwrap().unwrap().bytes,
            b"eightbit"
        );
    }
    #[test]
    fn failed_write_and_stale_catalog_authority_do_not_attach() {
        let root = tempfile::tempdir().unwrap();
        let store = PreviewStore::open(config(root.path(), 100, 100), &[]).unwrap();
        let old = key(1, Tier::Thumbnail);
        store.desire(&old, || Ok(true)).unwrap();
        store.publish(&old, b"old", authority).unwrap();
        let new = key(2, Tier::Thumbnail);
        store.desire(&new, || Ok(true)).unwrap();
        assert!(
            store
                .publish_controlled(&new, b"new", authority, || Err(
                    std::io::Error::from_raw_os_error(28).into()
                ))
                .is_err()
        );
        assert_eq!(store.usage().unwrap().pending_objects, 0);
        assert_eq!(store.read(&new, true).unwrap().unwrap().bytes, b"old");
        assert_eq!(
            store
                .publish(&new, b"new", |_| Ok(Publication::Stale))
                .unwrap(),
            Publication::Stale
        );
        assert_eq!(store.read(&new, true).unwrap().unwrap().bytes, b"old");
        assert_eq!(
            store.publish(&new, b"new", authority).unwrap(),
            Publication::Attached
        );
        assert_eq!(store.read(&new, false).unwrap().unwrap().bytes, b"new");
        assert_eq!(store.usage().unwrap().thumbnail_bytes, 3);
    }
    #[test]
    fn interrupted_staging_recovers_without_discarding_retained_fallback() {
        for renamed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let cfg = config(root.path(), 100, 100);
            let store = PreviewStore::open(cfg.clone(), &[]).unwrap();
            let old = key(1, Tier::Thumbnail);
            store.desire(&old, || Ok(true)).unwrap();
            store.publish(&old, b"old", authority).unwrap();
            let new = key(2, Tier::Thumbnail);
            store.desire(&new, || Ok(true)).unwrap();
            let digest = new.digest().unwrap();
            let destination = store.path(&digest, new.tier).unwrap();
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            let temporary = format!("{digest}.interrupted.pending");
            store
                .db
                .execute(
                    "INSERT INTO objects VALUES(?1,?2,'thumbnail',3,?3,'pending',?4,?5)",
                    params![
                        digest,
                        serde_json::to_string(&new).unwrap(),
                        blake3::hash(b"new").to_hex().to_string(),
                        temporary,
                        store.clock().unwrap()
                    ],
                )
                .unwrap();
            let path = if renamed {
                destination.clone()
            } else {
                destination.parent().unwrap().join(&temporary)
            };
            fs::write(&path, b"new").unwrap();
            drop(store);
            let store = PreviewStore::open(cfg, &[]).unwrap();
            assert!(!path.exists());
            assert_eq!(store.usage().unwrap().pending_objects, 0);
            assert_eq!(store.read(&new, true).unwrap().unwrap().bytes, b"old");
        }
    }
    #[test]
    fn large_lru_eviction_does_not_evict_retained_tier() {
        let root = tempfile::tempdir().unwrap();
        let store = PreviewStore::open(config(root.path(), 8, 8), &[]).unwrap();
        let thumb = key(1, Tier::Thumbnail);
        store.desire(&thumb, || Ok(true)).unwrap();
        store.publish(&thumb, b"thumb", authority).unwrap();
        let a = key(1, Tier::Large);
        store.desire(&a, || Ok(true)).unwrap();
        store.publish(&a, b"large-a", authority).unwrap();
        let mut b = a.clone();
        b.asset_id = "second".into();
        store.desire(&b, || Ok(true)).unwrap();
        store.publish(&b, b"large-b", authority).unwrap();
        assert!(store.read(&a, false).unwrap().is_none());
        assert!(store.read(&b, false).unwrap().is_some());
        assert!(store.read(&thumb, false).unwrap().is_some());
        assert_eq!(store.usage().unwrap().large_bytes, 7);
    }
    #[test]
    fn corruption_invalidates_and_allows_regeneration() {
        let root = tempfile::tempdir().unwrap();
        let store = PreviewStore::open(config(root.path(), 100, 100), &[]).unwrap();
        let key = key(1, Tier::Thumbnail);
        store.desire(&key, || Ok(true)).unwrap();
        store.publish(&key, b"valid", authority).unwrap();
        let path = store.path(&key.digest().unwrap(), key.tier).unwrap();
        fs::write(path, b"wrong").unwrap();
        assert!(store.read(&key, false).is_err());
        assert_eq!(store.usage().unwrap().thumbnail_bytes, 0);
        store.publish(&key, b"valid", authority).unwrap();
        assert_eq!(store.read(&key, false).unwrap().unwrap().bytes, b"valid");
    }
    #[test]
    fn original_overlap_and_conflicting_process_owner_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path(), 100, 100);
        assert!(PreviewStore::open(cfg.clone(), &[root.path().to_owned()]).is_err());
        assert!(!cfg.manifest_root.exists());
        let store = PreviewStore::open(cfg.clone(), &[]).unwrap();
        assert!(PreviewStore::open(cfg.clone(), &[]).is_err());
        drop(store);
        let mut changed = cfg;
        changed.thumbnail_root = root.path().join("elsewhere");
        assert!(PreviewStore::open(changed, &[]).is_err());
    }
}
