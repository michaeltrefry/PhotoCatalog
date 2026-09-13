//! Whole committed SQLite snapshots. External originals and preview caches are not backups.
//! Incomplete operations remain marked and cannot be opened as ordinary catalogs.
use crate::{CURRENT_SCHEMA_VERSION, Catalog};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const DB: &str = "catalog.sqlite3";
const PENDING: &str = ".photocatalog-pending.json";
const COMPLETED: &str = ".photocatalog-completed-intent.json";
const MANIFEST: &str = "photocatalog-backup.json";
const RESTORE: &str = ".photocatalog-restore.json";
const RESUMED: &str = ".photocatalog-jobs-resumed.json";
const DOCUMENT_BYTES: u64 = 64 * 1024;
const APPLICATION_ID: i64 = 0x50484341;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub pages_per_step: i32,
    pub max_seconds: u64,
    pub max_database_bytes: u64,
    pub min_free_bytes: u64,
    pub max_source_wal_bytes: u64,
    pub max_busy_steps: u32,
    pub verification_vm_steps: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            pages_per_step: 256,
            max_seconds: 3600,
            max_database_bytes: 2 * 1024_u64.pow(4),
            min_free_bytes: 1024_u64.pow(3),
            max_source_wal_bytes: 4 * 1024_u64.pow(3),
            max_busy_steps: 100,
            verification_vm_steps: 1_000_000_000,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=4096).contains(&self.pages_per_step),
            "pages_per_step must be 1..=4096"
        );
        ensure!(
            (1..=86400).contains(&self.max_seconds),
            "max_seconds must be 1..=86400"
        );
        ensure!(
            self.max_database_bytes > 0 && self.max_database_bytes <= i64::MAX as u64,
            "invalid database byte limit"
        );
        ensure!(
            self.verification_vm_steps >= 1000 && self.verification_vm_steps <= i64::MAX as u64,
            "invalid verification VM limit"
        );
        ensure!(
            self.max_busy_steps <= 10000,
            "max_busy_steps must be <=10000"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Snapshot,
    Copy,
    Verify,
    Hash,
    Upgrade,
    Publish,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub phase: Phase,
    pub pages_copied: u64,
    pub total_pages: u64,
    pub bytes_processed: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupReceipt {
    pub protocol: u32,
    pub backup_id: String,
    pub application_id: i64,
    pub schema_version: i64,
    pub database_bytes: u64,
    pub database_blake3: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreReceipt {
    pub protocol: u32,
    pub restore_id: String,
    pub backup: BackupReceipt,
    pub schema_version: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreStatus {
    pub receipt: RestoreReceipt,
    pub jobs_held: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    protocol: u32,
    restore_id: String,
    receipt_blake3: String,
    acknowledge_pending_jobs: bool,
}

/// Shared cancellation checked by SQLite's progress handler as well as copy/hash steps.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

struct Operation<'a, F> {
    limits: &'a Limits,
    started: Instant,
    vm: Arc<AtomicU64>,
    cancel: CancellationToken,
    callback: F,
}
impl<'a, F: FnMut(Progress) -> Result<()>> Operation<'a, F> {
    fn new(limits: &'a Limits, cancel: &CancellationToken, callback: F) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            limits,
            started: Instant::now(),
            vm: Arc::new(AtomicU64::new(0)),
            cancel: cancel.clone(),
            callback,
        })
    }
    fn check(&self) -> Result<()> {
        ensure!(
            !self.cancel.is_cancelled(),
            "backup/restore cancelled; incomplete destination remains blocked"
        );
        ensure!(
            self.started.elapsed() < Duration::from_secs(self.limits.max_seconds),
            "backup/restore deadline exceeded; incomplete destination remains blocked"
        );
        Ok(())
    }
    fn progress(&mut self, phase: Phase, bytes: u64) -> Result<()> {
        self.check()?;
        (self.callback)(Progress {
            phase,
            pages_copied: 0,
            total_pages: 0,
            bytes_processed: bytes,
        })
        .context("backup/restore cancelled by progress callback")?;
        self.check()
    }
    fn disk(&self, path: &Path, incoming: u64) -> Result<()> {
        self.check()?;
        ensure!(
            fs2::available_space(path)?
                >= self
                    .limits
                    .min_free_bytes
                    .checked_add(incoming)
                    .context("free-space limit overflow")?,
            "insufficient free space for backup/restore; preserve or remove the marked incomplete destination before retrying elsewhere"
        );
        Ok(())
    }
    fn guard(&self, db: &Connection) -> Result<()> {
        let cancel = self.cancel.clone();
        let vm = Arc::clone(&self.vm);
        let limit = self.limits.verification_vm_steps;
        let started = self.started;
        let seconds = self.limits.max_seconds;
        db.progress_handler(
            1000,
            Some(move || {
                cancel.is_cancelled()
                    || vm.fetch_add(1000, Ordering::Relaxed) >= limit
                    || started.elapsed() >= Duration::from_secs(seconds)
            }),
        )?;
        Ok(())
    }
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn regular(path: &Path) -> Result<fs::Metadata> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && !m.file_type().is_symlink(),
        "expected a regular file: {}",
        path.display()
    );
    Ok(m)
}
#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    identity: (u64, u64),
    bytes: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}
fn file_stamp(file: &File) -> Result<FileStamp> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(FileStamp {
        identity: crate::metadata_export::held_file_identity(file)?,
        bytes: metadata.len(),
        modified: metadata.modified()?,
        #[cfg(unix)]
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    })
}
struct StableFile {
    file: File,
    stamp: FileStamp,
}
impl StableFile {
    fn open(path: &Path) -> Result<Self> {
        let file = crate::metadata_export::open_regular(path)?;
        let stamp = file_stamp(&file)?;
        Ok(Self { file, stamp })
    }
    fn recheck(&self, path: &Path) -> Result<()> {
        let current = crate::metadata_export::open_regular(path)?;
        ensure!(
            file_stamp(&self.file)? == self.stamp && file_stamp(&current)? == self.stamp,
            "database identity or revision changed during backup verification"
        );
        Ok(())
    }
}

fn read_document<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    ensure!(
        regular(path)?.len() <= DOCUMENT_BYTES,
        "backup control document exceeds 64 KiB"
    );
    let mut b = Vec::new();
    File::open(path)?
        .take(DOCUMENT_BYTES + 1)
        .read_to_end(&mut b)?;
    ensure!(
        b.len() as u64 <= DOCUMENT_BYTES,
        "control document grew beyond limit"
    );
    serde_json::from_slice(&b).context("invalid backup/restore control document")
}
#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}
fn write_document(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() as u64 <= DOCUMENT_BYTES,
        "control document exceeds limit"
    );
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    f.write_all(&bytes)?;
    crate::metadata_export::sync_file(&f)?;
    drop(f);
    crate::metadata_export::move_to_private(&temporary, path)?;
    sync_directory(path.parent().context("control document parent")?)
}
/// Called before Catalog::open creates directories or touches SQLite.
pub(crate) fn check_catalog_root(root: &Path) -> Result<()> {
    ensure!(
        !exists(&root.join(PENDING))?,
        "incomplete backup/restore destination; ordinary catalog open is blocked"
    );
    ensure!(
        !exists(&root.join(MANIFEST))?,
        "backup bundle is not a live catalog; restore it into a new destination"
    );
    ensure!(
        !exists(&root.join(COMPLETED))? || exists(&root.join(RESTORE))?,
        "completed backup/restore destination is missing its restore receipt; ordinary catalog open and job execution are blocked"
    );
    Ok(())
}
fn root(path: &Path) -> Result<PathBuf> {
    let p = fs::canonicalize(path)?;
    ensure!(p.is_dir(), "expected directory");
    Ok(p)
}
fn exclusive_root(path: &Path, other: &Path) -> Result<PathBuf> {
    let p = crate::prospective_directory(path)?;
    ensure!(
        !p.starts_with(other) && !other.starts_with(&p),
        "source and destination must be separate directories"
    );
    fs::create_dir(&p).context("destination must be a new directory with an existing parent")?;
    // Durable intent precedes any database bytes. Failures retain this guard.
    write_document(
        &p.join(PENDING),
        &serde_json::json!({"protocol":1,"operation":"backup_or_restore"}),
    )?;
    sync_directory(p.parent().context("destination parent")?)?;
    Ok(p)
}
fn publish(root: &Path) -> Result<()> {
    crate::metadata_export::move_to_private(&root.join(PENDING), &root.join(COMPLETED))?;
    sync_directory(root)
}
fn readonly(path: &Path) -> Result<Connection> {
    regular(path)?;
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::ZERO)?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; PRAGMA mmap_size=0; PRAGMA cache_size=-32768; PRAGMA temp_store=FILE;")?;
    Ok(db)
}
fn identity(db: &Connection) -> Result<i64> {
    let app: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        app == APPLICATION_ID && (1..=CURRENT_SCHEMA_VERSION).contains(&schema),
        "not a supported PhotoCatalog backup schema (application={app}, schema={schema})"
    );
    Ok(schema)
}
fn companion(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}
fn no_journal(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-journal"] {
        let p = companion(path, suffix);
        if exists(&p)? {
            ensure!(
                regular(&p)?.len() == 0,
                "database is not self-contained: {}",
                p.display()
            );
        }
    }
    Ok(())
}
fn finalize(path: &Path) -> Result<()> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(Duration::ZERO)?;
    let mode: String = db.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
    ensure!(mode == "delete", "cannot finalize self-contained backup");
    db.close().map_err(|(_, e)| e)?;
    no_journal(path)?;
    crate::metadata_export::sync_file(&OpenOptions::new().read(true).write(true).open(path)?)?;
    sync_directory(path.parent().unwrap())
}
fn verify<F: FnMut(Progress) -> Result<()>>(path: &Path, op: &mut Operation<'_, F>) -> Result<i64> {
    op.progress(Phase::Verify, 0)?;
    ensure!(
        regular(path)?.len() <= op.limits.max_database_bytes,
        "database exceeds byte limit"
    );
    no_journal(path)?;
    let db = readonly(path)?;
    op.guard(&db)?;
    let version = identity(&db)?;
    let mut stmt = db.prepare("PRAGMA integrity_check(1)")?;
    let mut rows = stmt.query([])?;
    let first = rows
        .next()
        .context(
            "SQLite integrity verification interrupted or failed (cancellation/deadline/VM limit)",
        )?
        .context("missing SQLite integrity result")?;
    let answer: String = first.get(0)?;
    ensure!(answer == "ok", "backup integrity failure: {answer}");
    ensure!(rows.next()?.is_none(), "extra integrity failures");
    ensure!(
        db.prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()
            .context("foreign-key verification interrupted or failed")?
            .is_none(),
        "backup foreign-key violation"
    );
    op.check()?;
    Ok(version)
}
fn hash<F: FnMut(Progress) -> Result<()>>(
    path: &Path,
    op: &mut Operation<'_, F>,
) -> Result<(u64, String)> {
    let mut held = StableFile::open(path)?;
    let expected = held.stamp.bytes;
    ensure!(
        expected <= op.limits.max_database_bytes,
        "database exceeds byte limit"
    );
    let mut buf = vec![0; 1024 * 1024];
    let mut count = 0;
    let mut h = blake3::Hasher::new();
    loop {
        op.progress(Phase::Hash, count)?;
        let n = held.file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(count <= expected, "database changed during hash");
        h.update(&buf[..n]);
    }
    ensure!(
        count == expected && regular(path)?.len() == expected,
        "database length changed during hash"
    );
    held.recheck(path)?;
    Ok((count, h.finalize().to_hex().to_string()))
}
fn manifest(root: &Path) -> Result<BackupReceipt> {
    ensure!(
        !exists(&root.join(PENDING))?,
        "backup is incomplete; no completion marker admission"
    );
    let r: BackupReceipt = read_document(&root.join(MANIFEST))?;
    ensure!(
        r.protocol == 1
            && uuid::Uuid::parse_str(&r.backup_id).is_ok()
            && r.application_id == APPLICATION_ID
            && (1..=CURRENT_SCHEMA_VERSION).contains(&r.schema_version)
            && r.database_bytes > 0
            && r.database_blake3.len() == 64
            && r.database_blake3
                .bytes()
                .all(|x| x.is_ascii_hexdigit() && !x.is_ascii_uppercase()),
        "invalid backup manifest"
    );
    Ok(r)
}
fn inspect<F: FnMut(Progress) -> Result<()>>(
    bundle: &Path,
    op: &mut Operation<'_, F>,
) -> Result<BackupReceipt> {
    let r = manifest(bundle)?;
    let file = bundle.join(DB);
    let held = StableFile::open(&file)?;
    let (size, digest) = hash(&file, op)?;
    ensure!(
        size == r.database_bytes && digest == r.database_blake3,
        "backup size/digest mismatch; source was not restored"
    );
    ensure!(
        verify(&file, op)? == r.schema_version,
        "backup schema differs from manifest"
    );
    held.recheck(&file)?;
    no_journal(&file)?;
    Ok(r)
}
pub fn backup_catalog<F: FnMut(Progress) -> Result<()>>(
    source_root: impl AsRef<Path>,
    bundle: impl AsRef<Path>,
    limits: &Limits,
    callback: F,
) -> Result<BackupReceipt> {
    backup_catalog_with_control(
        source_root,
        bundle,
        limits,
        &CancellationToken::default(),
        callback,
    )
}
pub fn inspect_backup<F: FnMut(Progress) -> Result<()>>(
    bundle: impl AsRef<Path>,
    limits: &Limits,
    callback: F,
) -> Result<BackupReceipt> {
    inspect_backup_with_control(bundle, limits, &CancellationToken::default(), callback)
}
pub fn restore_catalog<F: FnMut(Progress) -> Result<()>>(
    bundle: impl AsRef<Path>,
    new_root: impl AsRef<Path>,
    limits: &Limits,
    callback: F,
) -> Result<RestoreReceipt> {
    restore_catalog_with_control(
        bundle,
        new_root,
        limits,
        &CancellationToken::default(),
        callback,
    )
}

/// Copy a pinned committed snapshot while other connections may continue writing.
/// Callback errors cancel; a failed attempt remains marked and cannot be reused.
pub fn backup_catalog_with_control<F: FnMut(Progress) -> Result<()>>(
    source_root: impl AsRef<Path>,
    bundle: impl AsRef<Path>,
    limits: &Limits,
    cancel: &CancellationToken,
    callback: F,
) -> Result<BackupReceipt> {
    let mut op = Operation::new(limits, cancel, callback)?;
    let source = root(source_root.as_ref())?;
    check_catalog_root(&source)?;
    let file = source.join(DB);
    let db = readonly(&file)?;
    op.guard(&db)?;
    db.execute_batch("BEGIN")?;
    // PRAGMA header reads alone do not guarantee the intended data snapshot.
    let _: i64 = db.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
    let version = identity(&db)?;
    let pages: i64 = db.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let page_size: i64 = db.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let bytes = u64::try_from(pages)?
        .checked_mul(u64::try_from(page_size)?)
        .context("database size overflow")?;
    ensure!(
        bytes <= limits.max_database_bytes,
        "database exceeds backup byte limit"
    );
    op.progress(Phase::Snapshot, 0)?;
    let target = exclusive_root(bundle.as_ref(), &source)?;
    op.disk(&target, bytes)?;
    let target_file = target.join(DB);
    let mut copied = Connection::open(&target_file)?;
    copied.busy_timeout(Duration::ZERO)?;
    {
        let backup = Backup::new(&db, &mut copied)?;
        let mut busy = 0;
        loop {
            op.disk(&target, 0)?;
            let wal = source.join("catalog.sqlite3-wal");
            if exists(&wal)? {
                ensure!(
                    regular(&wal)?.len() <= limits.max_source_wal_bytes,
                    "source WAL pressure exceeds limit; release snapshot and retry with sufficient budget"
                );
            }
            let result = backup
                .step(limits.pages_per_step)
                .context("SQLite snapshot copy failed")?;
            let p = backup.progress();
            (op.callback)(Progress {
                phase: Phase::Copy,
                pages_copied: u64::try_from(p.pagecount - p.remaining)?,
                total_pages: u64::try_from(p.pagecount)?,
                bytes_processed: u64::try_from(p.pagecount - p.remaining)?
                    .saturating_mul(page_size as u64),
            })
            .context("backup cancelled during copy")?;
            op.check()?;
            match result {
                StepResult::Done => break,
                StepResult::More => {}
                StepResult::Busy | StepResult::Locked => {
                    busy += 1;
                    ensure!(
                        busy <= limits.max_busy_steps,
                        "backup remained busy/locked; retry in a new destination"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => bail!("unknown SQLite backup result"),
            }
        }
    }
    copied.close().map_err(|(_, e)| e)?;
    db.execute_batch("ROLLBACK")?;
    drop(db); // Release pinned WAL before verification.
    finalize(&target_file)?;
    ensure!(
        verify(&target_file, &mut op)? == version,
        "snapshot schema changed"
    );
    let (database_bytes, database_blake3) = hash(&target_file, &mut op)?;
    let receipt = BackupReceipt {
        protocol: 1,
        backup_id: uuid::Uuid::new_v4().to_string(),
        application_id: APPLICATION_ID,
        schema_version: version,
        database_bytes,
        database_blake3,
    };
    op.progress(Phase::Publish, database_bytes)?;
    op.disk(&target, 0)?;
    write_document(&target.join(MANIFEST), &receipt)?;
    publish(&target)?;
    Ok(receipt)
}
pub fn inspect_backup_with_control<F: FnMut(Progress) -> Result<()>>(
    bundle: impl AsRef<Path>,
    limits: &Limits,
    cancel: &CancellationToken,
    callback: F,
) -> Result<BackupReceipt> {
    let mut op = Operation::new(limits, cancel, callback)?;
    inspect(&root(bundle.as_ref())?, &mut op)
}
/// Restore into an exclusive new root. The bundle and any previous destination are never modified.
pub fn restore_catalog_with_control<F: FnMut(Progress) -> Result<()>>(
    bundle: impl AsRef<Path>,
    new_root: impl AsRef<Path>,
    limits: &Limits,
    cancel: &CancellationToken,
    callback: F,
) -> Result<RestoreReceipt> {
    let mut op = Operation::new(limits, cancel, callback)?;
    let bundle = root(bundle.as_ref())?;
    let receipt = inspect(&bundle, &mut op)?;
    let target = exclusive_root(new_root.as_ref(), &bundle)?;
    op.disk(&target, receipt.database_bytes)?;
    let mut from = File::open(bundle.join(DB))?;
    let mut to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target.join(DB))?;
    let mut buf = vec![0; 1024 * 1024];
    let mut count = 0;
    let mut digest = blake3::Hasher::new();
    loop {
        op.progress(Phase::Copy, count)?;
        op.disk(&target, 0)?;
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(
            count <= receipt.database_bytes,
            "backup grew during restore"
        );
        to.write_all(&buf[..n])?;
        digest.update(&buf[..n]);
    }
    ensure!(
        count == receipt.database_bytes
            && digest.finalize().to_hex().as_str() == receipt.database_blake3,
        "backup changed during restore"
    );
    crate::metadata_export::sync_file(&to)?;
    drop(to);
    drop(from);
    op.progress(Phase::Upgrade, count)?;
    {
        let catalog = Catalog::open_restoring(&target, |db| op.guard(db))
            .context("restored copy schema upgrade failed; original and bundle remain untouched")?;
        drop(catalog);
    }
    finalize(&target.join(DB))?;
    let schema_version = verify(&target.join(DB), &mut op)?;
    ensure!(
        schema_version == CURRENT_SCHEMA_VERSION,
        "restored schema did not reach current version"
    );
    let restored = RestoreReceipt {
        protocol: 1,
        restore_id: uuid::Uuid::new_v4().to_string(),
        backup: receipt,
        schema_version,
    };
    op.progress(Phase::Publish, count)?;
    op.disk(&target, 0)?;
    write_document(&target.join(RESTORE), &restored)?;
    publish(&target)?;
    Ok(restored)
}
/// No database is opened and no jobs execute when reading or releasing the restored-job hold.
pub fn restore_status(root: impl AsRef<Path>) -> Result<Option<RestoreStatus>> {
    let root = root.as_ref();
    check_catalog_root(root)?;
    if !exists(&root.join(RESTORE))? {
        ensure!(
            !exists(&root.join(RESUMED))?,
            "orphan restored-job release marker"
        );
        return Ok(None);
    }
    let receipt: RestoreReceipt = read_document(&root.join(RESTORE))?;
    ensure!(
        receipt.protocol == 1 && uuid::Uuid::parse_str(&receipt.restore_id).is_ok(),
        "invalid restore receipt"
    );
    let held = if exists(&root.join(RESUMED))? {
        let release: Release = read_document(&root.join(RESUMED))?;
        ensure!(
            release.protocol == 1
                && release.restore_id == receipt.restore_id
                && release.receipt_blake3
                    == blake3::hash(&serde_json::to_vec(&receipt)?)
                        .to_hex()
                        .as_str()
                && release.acknowledge_pending_jobs,
            "restore job release does not match receipt"
        );
        false
    } else {
        true
    };
    Ok(Some(RestoreStatus {
        receipt,
        jobs_held: held,
    }))
}
pub fn resume_restored_jobs(
    root: impl AsRef<Path>,
    restore_id: &str,
    acknowledge_pending_jobs: bool,
) -> Result<RestoreStatus> {
    ensure!(
        acknowledge_pending_jobs,
        "explicit acknowledgment of preexisting jobs is required"
    );
    let root = root.as_ref();
    let mut status = restore_status(root)?.context("catalog is not a restored instance")?;
    ensure!(
        status.receipt.restore_id == restore_id,
        "restore receipt identity changed"
    );
    if status.jobs_held {
        let r = Release {
            protocol: 1,
            restore_id: restore_id.into(),
            receipt_blake3: blake3::hash(&serde_json::to_vec(&status.receipt)?)
                .to_hex()
                .to_string(),
            acknowledge_pending_jobs,
        };
        write_document(&root.join(RESUMED), &r)?;
        status.jobs_held = false;
    }
    Ok(status)
}
pub(crate) fn require_jobs_released(root: &Path) -> Result<()> {
    if let Some(s) = restore_status(root)? {
        ensure!(
            !s.jobs_held,
            "restored external jobs are held; explicitly resume restore {} and acknowledge preexisting jobs",
            s.receipt.restore_id
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests;
