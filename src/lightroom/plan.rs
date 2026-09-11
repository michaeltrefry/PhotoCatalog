//! A separate, resumable inspection plan. Source snapshots remain the ultimate
//! byte evidence; typed row retention and projections are derivative and versioned.
use super::{
    Issue, Limits, PROTOCOL,
    capture::{Manifest, read_manifest},
    digest, json_digest, path_value,
    source::Source,
};
use crate::{storage_volume::NativePath, xmp_packets};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, types::ValueRef};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

/// Inspection-plan storage version; independent of the application catalog.
pub const PLAN_SCHEMA_VERSION: i64 = 3;
const PAGING_INDEXES: &str = "CREATE INDEX rows_revision_sequence ON rows(revision,sequence);
CREATE INDEX issues_revision_sequence ON issues(revision,sequence);
CREATE INDEX packets_revision_sequence ON packets(revision,sequence);
CREATE INDEX paths_revision_sequence ON paths(revision,sequence);
CREATE INDEX facts_revision ON metadata_facts(revision);
CREATE INDEX paths_pending_queue ON paths(revision,sequence) WHERE state='pending';
CREATE INDEX paths_packet_queue ON paths(revision,sequence) WHERE state='pending' OR json_extract(evidence,'$.embedded_sidecar_xmp') IS NOT NULL;";
const ROWS_PAGE: &str = "SELECT r.sequence,r.source_id,r.table_name,r.key_json,t.columns_json,r.cells_json,t.category FROM rows r JOIN tables t ON t.revision=r.revision AND t.name=r.table_name WHERE r.revision=?1 AND r.sequence>?2 ORDER BY r.sequence LIMIT ?3";
const TABLE_ROWS_PAGE: &str = "SELECT r.sequence,r.source_id,r.table_name,r.key_json,t.columns_json,r.cells_json,t.category FROM rows r JOIN tables t ON t.revision=r.revision AND t.name=r.table_name WHERE r.revision=?1 AND r.sequence>?2 AND r.table_name=?3 ORDER BY r.sequence LIMIT ?4";
const ISSUES_PAGE: &str = "SELECT sequence,source_id,code,detail FROM issues WHERE revision=? AND sequence>? ORDER BY sequence LIMIT ?";
const PACKETS_PAGE: &str = "SELECT sequence,source_id,origin,raw_digest,length(raw),length(decoded),detail FROM packets WHERE revision=? AND sequence>? ORDER BY sequence LIMIT ?";
const PATHS_PAGE: &str = "SELECT sequence,source_id,original,inspection_path,state,evidence FROM paths WHERE revision=? AND sequence>? ORDER BY sequence LIMIT ?";

// With both partial indexes SQLite can otherwise choose the broader packet queue
// for metadata-only work and scan all completed-but-uninspected packet markers.
const PATH_QUEUE: &str = "SELECT sequence,source_id,inspection_path FROM paths INDEXED BY paths_pending_queue WHERE revision=?1 AND state='pending' ORDER BY sequence LIMIT ?2";
const PATH_PACKET_QUEUE: &str = "SELECT sequence,source_id,inspection_path FROM paths INDEXED BY paths_packet_queue WHERE revision=?1 AND (state='pending' OR json_extract(evidence,'$.embedded_sidecar_xmp') IS NOT NULL) ORDER BY sequence LIMIT ?2";
const PATH_PENDING: &str = "SELECT EXISTS(SELECT 1 FROM paths INDEXED BY paths_pending_queue WHERE revision=?1 AND state='pending')";
const CONFLICTS_PAGE: &str = "SELECT rowid,source_id,file_source_id,origin,packet_digest,field,value_json FROM metadata_facts f WHERE revision=? AND rowid>? AND (SELECT count(DISTINCT value_json) FROM metadata_facts other WHERE other.revision=f.revision AND other.file_source_id=f.file_source_id AND other.field=f.field)>1 ORDER BY rowid LIMIT ?";

// These indexes are part of schema 2, including partial-queue predicates. A
// missing/replaced index must fail admission rather than silently restoring scans.
fn validate_paging_indexes(db: &Connection) -> Result<()> {
    for definition in PAGING_INDEXES
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let name = definition
            .split_whitespace()
            .nth(2)
            .context("invalid paging index definition")?;
        let actual: Option<String> = db
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='index' AND name=?",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            actual.as_deref() == Some(definition),
            "missing or incompatible inspection paging index: {name}"
        );
    }
    Ok(())
}

// Plans 1/2 contain type-sensitive relationship keys. They remain historical
// evidence: no automatic whole-plan rewrite or partial semantic upgrade is safe.
fn require_current_plan(version: i64) -> Result<()> {
    ensure!(
        version == PLAN_SCHEMA_VERSION,
        "inspection plan schema {version} has incompatible derived relationships; create a new derived plan from preserved captures (required schema {PLAN_SCHEMA_VERSION}); existing evidence is unchanged"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Cell {
    Null,
    Integer(i64),
    RealBits(u64),
    Text(#[serde(with = "hex_bytes")] Vec<u8>),
    Blob(#[serde(with = "hex_bytes")] Vec<u8>),
}
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        use std::fmt::Write;
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(text, "{byte:02x}").unwrap();
        }
        serializer.serialize_str(&text)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.len() % 2 != 0 || !text.is_ascii() {
            return Err(serde::de::Error::custom("invalid hex bytes"));
        }
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16)
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}
impl Cell {
    fn from_sql(value: ValueRef<'_>, limit: usize) -> Result<Self> {
        Ok(match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(v) => Self::Integer(v),
            ValueRef::Real(v) => Self::RealBits(v.to_bits()),
            ValueRef::Text(v) => {
                ensure!(v.len() <= limit, "text cell exceeds byte limit");
                Self::Text(v.into())
            }
            ValueRef::Blob(v) => {
                ensure!(v.len() <= limit, "blob cell exceeds byte limit");
                Self::Blob(v.into())
            }
        })
    }
    fn text(&self) -> Option<&str> {
        if let Self::Text(v) = self {
            std::str::from_utf8(v).ok()
        } else {
            None
        }
    }
    fn integer(&self) -> Option<i64> {
        if let Self::Integer(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    fn numeric(&self) -> Option<f64> {
        match self {
            Self::Integer(v) => Some(*v as f64),
            Self::RealBits(v) => Some(f64::from_bits(*v)),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetainedRow {
    pub sequence: i64,
    pub source_id: String,
    pub revision_id: String,
    pub table: String,
    pub source_key: Vec<Cell>,
    pub columns: Vec<String>,
    pub cells: Vec<Cell>,
    pub semantics: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TableState {
    pub name: String,
    pub category: String,
    pub expected: Option<i64>,
    pub retained: i64,
    pub state: String,
    pub issue: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub protocol: u32,
    pub revision_id: String,
    pub lineage_id: String,
    pub stage: String,
    pub capture: Manifest,
    pub tables: Vec<TableState>,
    pub counts: BTreeMap<String, i64>,
    pub issues: Vec<Issue>,
    pub semantics: String,
    pub rendering: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Progress {
    pub revision_id: String,
    pub retained_this_call: usize,
    pub stage: String,
}
pub struct Plan {
    db: Connection,
    root: PathBuf,
}
fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn uri(path: &Path) -> Result<String> {
    let path = fs::canonicalize(path)?;
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(windows)]
    let bytes = path
        .to_str()
        .context("SQLite URI cannot represent an unpaired Windows surrogate")?
        // Preserve the native namespace (including \\?\ and UNC). SQLite checks
        // URI authority before percent decoding; converting these backslashes
        // to slashes would turn the namespace prefix into an invalid authority.
        .as_bytes()
        .to_vec();
    let escaped: String = bytes
        .iter()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(*b, b'/' | b':' | b'-' | b'_' | b'.') {
                (*b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    Ok(format!("file:{escaped}?mode=ro&immutable=1"))
}
fn restrict(db: &Connection, limits: &Limits) -> Result<()> {
    db.busy_timeout(Duration::from_millis(0))?;
    db.pragma_update(None, "trusted_schema", false)?;
    db.pragma_update(None, "mmap_size", 0)?;
    db.pragma_update(None, "cache_size", -32768)?;
    // The caller controls SQL; extension loading remains disabled by default.
    unsafe {
        rusqlite::ffi::sqlite3_limit(
            db.handle(),
            rusqlite::ffi::SQLITE_LIMIT_LENGTH,
            limits.max_cell_bytes as i32,
        );
        rusqlite::ffi::sqlite3_limit(
            db.handle(),
            rusqlite::ffi::SQLITE_LIMIT_SQL_LENGTH,
            1024 * 1024,
        );
        rusqlite::ffi::sqlite3_limit(db.handle(), rusqlite::ffi::SQLITE_LIMIT_COLUMN, 4096);
        rusqlite::ffi::sqlite3_db_config(
            db.handle(),
            rusqlite::ffi::SQLITE_DBCONFIG_DEFENSIVE,
            1,
            std::ptr::null_mut::<i32>(),
        );
        rusqlite::ffi::sqlite3_set_authorizer(
            db.handle(),
            Some(source_authorizer),
            std::ptr::null_mut(),
        );
    }
    Ok(())
}
unsafe extern "C" fn source_authorizer(
    _: *mut std::ffi::c_void,
    action: i32,
    _a: *const std::ffi::c_char,
    b: *const std::ffi::c_char,
    _db: *const std::ffi::c_char,
    _trigger: *const std::ffi::c_char,
) -> i32 {
    if action == rusqlite::ffi::SQLITE_FUNCTION {
        if b.is_null() {
            return rusqlite::ffi::SQLITE_DENY;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(b) }.to_bytes();
        if !name.eq_ignore_ascii_case(b"count") && !name.eq_ignore_ascii_case(b"coalesce") {
            return rusqlite::ffi::SQLITE_DENY;
        }
    }
    if action == rusqlite::ffi::SQLITE_ATTACH || action == rusqlite::ffi::SQLITE_DETACH {
        return rusqlite::ffi::SQLITE_DENY;
    }
    rusqlite::ffi::SQLITE_OK
}
fn snapshot(path: &Path, limits: &Limits) -> Result<Connection> {
    let db = Connection::open_with_flags(
        uri(path)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    restrict(&db, limits)?;
    db.pragma_update(None, "query_only", true)?;
    Ok(db)
}
pub(super) fn recover_private(source: &Path, destination: &Path, limits: &Limits) -> Result<()> {
    ensure!(
        !destination.exists(),
        "logical snapshot destination already exists"
    );
    let db = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    restrict(&db, limits)?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(
        integrity == "ok",
        "private recovered catalog failed integrity check: {integrity}"
    );
    let target = Connection::open(destination)?;
    let main = c"main";
    // Backup is only ever invoked on the private working copy. No stored source
    // schema text is executed to recreate tables or invoke Adobe instructions.
    unsafe {
        let backup = rusqlite::ffi::sqlite3_backup_init(
            target.handle(),
            main.as_ptr(),
            db.handle(),
            main.as_ptr(),
        );
        ensure!(
            !backup.is_null(),
            "private SQLite backup initialization failed"
        );
        let mut code = rusqlite::ffi::SQLITE_OK;
        while code == rusqlite::ffi::SQLITE_OK {
            code = rusqlite::ffi::sqlite3_backup_step(backup, 256);
        }
        let finish = rusqlite::ffi::sqlite3_backup_finish(backup);
        ensure!(
            code == rusqlite::ffi::SQLITE_DONE && finish == rusqlite::ffi::SQLITE_OK,
            "private SQLite backup failed ({code}, {finish})"
        );
    }
    // Finish any WAL belonging to this app-owned destination before immutable reads.
    target.pragma_update(None, "journal_mode", "DELETE")?;
    drop(target);
    drop(db);
    fs::OpenOptions::new()
        .write(true)
        .open(destination)?
        .sync_all()?;
    Ok(())
}
impl Plan {
    pub fn create(root: &Path) -> Result<Self> {
        ensure!(!root.exists(), "inspection output must be a new directory");
        fs::create_dir(root)?;
        let db = Connection::open(root.join("inspection.sqlite3"))?;
        crate::configure_catalog_connection(&db)?;
        let transaction = db.unchecked_transaction()?;
        transaction.execute_batch("PRAGMA application_id=0x50434c49;
CREATE TABLE captures(revision TEXT PRIMARY KEY,lineage TEXT NOT NULL,path TEXT NOT NULL,manifest TEXT NOT NULL,stage TEXT NOT NULL,schema_version TEXT,provider TEXT,evidence_revision INTEGER NOT NULL DEFAULT 0);
CREATE TABLE schema_objects(revision TEXT NOT NULL,kind TEXT NOT NULL,name TEXT NOT NULL,table_name TEXT NOT NULL,sql_text TEXT NOT NULL,PRIMARY KEY(revision,kind,name));
CREATE TABLE tables(revision TEXT NOT NULL,name TEXT NOT NULL,columns_json TEXT NOT NULL,key_json TEXT NOT NULL,schema_json TEXT NOT NULL,category TEXT NOT NULL,expected INTEGER,retained INTEGER NOT NULL DEFAULT 0,cursor TEXT,state TEXT NOT NULL,issue TEXT,PRIMARY KEY(revision,name));
CREATE TABLE rows(sequence INTEGER PRIMARY KEY,revision TEXT NOT NULL,source_id TEXT NOT NULL,table_name TEXT NOT NULL,key_json TEXT NOT NULL,cells_json TEXT NOT NULL,UNIQUE(revision,table_name,key_json));
CREATE INDEX source_rows ON rows(revision,table_name,sequence);
CREATE INDEX source_ids ON rows(revision,source_id);
CREATE TABLE entities(revision TEXT NOT NULL,source_id TEXT NOT NULL,table_name TEXT NOT NULL,local_key TEXT,global_key TEXT,fields_json TEXT NOT NULL,PRIMARY KEY(revision,source_id));
CREATE INDEX entity_local ON entities(revision,table_name,local_key);
CREATE INDEX entity_global ON entities(revision,table_name,global_key);
CREATE TABLE issues(sequence INTEGER PRIMARY KEY,revision TEXT NOT NULL,source_id TEXT,code TEXT NOT NULL,detail TEXT NOT NULL,UNIQUE(revision,source_id,code,detail));
CREATE TABLE packets(sequence INTEGER PRIMARY KEY,revision TEXT NOT NULL,source_id TEXT NOT NULL,origin TEXT NOT NULL,raw_digest TEXT NOT NULL,raw BLOB NOT NULL,decoded BLOB,detail TEXT NOT NULL,UNIQUE(revision,source_id,origin,raw_digest));
CREATE TABLE metadata_facts(revision TEXT NOT NULL,source_id TEXT NOT NULL,file_source_id TEXT,origin TEXT NOT NULL,packet_digest TEXT NOT NULL,field TEXT NOT NULL,value_json TEXT NOT NULL,PRIMARY KEY(revision,source_id,origin,packet_digest,field));
CREATE INDEX facts_by_file ON metadata_facts(revision,file_source_id,field);
CREATE TABLE references_out(sequence INTEGER PRIMARY KEY,revision TEXT NOT NULL,source_id TEXT NOT NULL,field TEXT NOT NULL,target_table TEXT NOT NULL,target_key TEXT NOT NULL,UNIQUE(revision,source_id,field,target_table,target_key));
CREATE TABLE paths(sequence INTEGER PRIMARY KEY,revision TEXT NOT NULL,source_id TEXT NOT NULL,original TEXT NOT NULL,inspection_path TEXT,state TEXT NOT NULL,evidence TEXT,UNIQUE(revision,source_id));
CREATE INDEX path_locator ON paths(revision,inspection_path,sequence);
CREATE TABLE inventories(digest TEXT PRIMARY KEY,json TEXT NOT NULL);
CREATE TABLE family_assignments(revision TEXT PRIMARY KEY,family TEXT NOT NULL,reason TEXT NOT NULL);
CREATE TABLE family_choices(family TEXT PRIMARY KEY,revision TEXT NOT NULL,evidence_digest TEXT NOT NULL,reason TEXT NOT NULL);
")?;
        transaction.execute_batch(PAGING_INDEXES)?;
        transaction.pragma_update(None, "user_version", PLAN_SCHEMA_VERSION)?;
        transaction.commit()?;
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(Self {
            db,
            root: fs::canonicalize(root)?,
        })
    }
    pub fn open(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let path = root.join("inspection.sqlite3");
        let guard = Source::open(&path, u64::MAX)?;
        // Inspect identity without opening/recovering an unrelated database's WAL
        // or changing its journal mode. Creation checkpoints this fixed schema.
        let check = snapshot(&path, &Limits::default())?;
        let app: i64 = check.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let version: i64 = check.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            app == 0x50434c49,
            "not a supported Lightroom inspection plan"
        );
        require_current_plan(version)?;
        for table in [
            "captures",
            "schema_objects",
            "tables",
            "rows",
            "entities",
            "issues",
            "packets",
            "references_out",
            "metadata_facts",
            "paths",
            "inventories",
            "family_assignments",
            "family_choices",
        ] {
            let count: i64 = check.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name=?",
                [table],
                |r| r.get(0),
            )?;
            ensure!(count == 1, "incomplete inspection schema: {table}");
        }
        check.prepare("SELECT evidence_revision FROM captures LIMIT 0").context("inspection plan predates evidence-revision schema; create a new derived plan from retained captures")?;
        validate_paging_indexes(&check)?;
        guard.verify()?;
        drop(check);
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let app: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        ensure!(
            app == 0x50434c49,
            "inspection identity changed while opening"
        );
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        require_current_plan(version)?;
        validate_paging_indexes(&db)?;
        crate::configure_catalog_connection(&db)?;
        Ok(Self { db, root })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn add_capture(&mut self, directory: &Path) -> Result<String> {
        let directory = fs::canonicalize(directory)?;
        ensure!(
            !directory.starts_with(&self.root) && !self.root.starts_with(&directory),
            "capture evidence and mutable inspection plan must be separate"
        );
        let manifest = read_manifest(&directory)?;
        ensure!(
            manifest.state == "captured"
                && manifest.sqlite_consistency == "consistent_default_sqlite",
            "capture is not a consistent SQLite snapshot; inspect capture issues before adding"
        );
        let revision = manifest
            .revision_id
            .clone()
            .context("capture revision missing")?;
        ensure!(
            json_digest(&manifest.artifacts)? == revision,
            "capture manifest identity mismatch"
        );
        let mut file = Source::open(
            &directory.join("logical.sqlite3"),
            manifest.request.limits.max_total_bytes,
        )?;
        ensure!(
            Some(file.copy_and_hash(None)?) == manifest.logical_blake3,
            "logical snapshot digest mismatch"
        );
        ensure!(
            Some(&file.before) == manifest.logical_revision.as_ref(),
            "logical snapshot object/revision differs from capture"
        );
        for artifact in &manifest.artifacts {
            let relative = Path::new(&artifact.stored);
            ensure!(
                relative
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                    && relative.starts_with("raw"),
                "unsafe retained artifact path"
            );
            let mut raw = Source::open(
                &directory.join(relative),
                manifest.request.limits.max_file_bytes,
            )?;
            ensure!(
                raw.before.bytes == artifact.revision.bytes
                    && raw.copy_and_hash(None)? == artifact.blake3,
                "raw artifact digest mismatch"
            );
        }
        if self
            .db
            .query_row(
                "SELECT 1 FROM captures WHERE revision=?",
                [&revision],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some()
        {
            return Ok(revision);
        }
        let source = snapshot(&directory.join("logical.sqlite3"), &manifest.request.limits)?;
        let schemas = read_schema(&source)?;
        let variables = read_variables(&source, &schemas)?;
        let provider = variables.get("Adobe_storeProviderID").cloned();
        // A source ID namespace is retained independently of its capture revision.
        // This generated namespace is persisted with the capture, not derived from a
        // mutable path or an unproven provider identity. Family membership never
        // collapses source namespaces; original global IDs remain separate evidence.
        let lineage = uuid::Uuid::new_v4().to_string();
        let version = variables.get("Adobe_DBVersion").cloned();
        let transaction = self.db.transaction()?;
        transaction.execute(
            "INSERT INTO captures(revision,lineage,path,manifest,stage,schema_version,provider) VALUES(?,?,?,?,?,?,?)",
            params![
                revision,
                lineage,
                path_value(&directory)?,
                serde_json::to_string(&manifest)?,
                "pending",
                version,
                provider
            ],
        )?;
        for schema in schemas {
            transaction.execute(
                "INSERT INTO schema_objects VALUES(?,?,?,?,?)",
                params![revision, schema.kind, schema.name, schema.table, schema.sql],
            )?;
            if schema.kind != "table" {
                continue;
            }
            let mut columns = vec![];
            let mut pk = vec![];
            let mut issue = None;
            if schema.kind != "table" || schema.root_page == 0 {
                issue = Some(
                    "stored execution surface retained in original snapshot; not evaluated"
                        .to_owned(),
                );
            }
            if issue.is_none() {
                let mut statement =
                    source.prepare(&format!("PRAGMA table_xinfo({})", identifier(&schema.name)))?;
                let mut rows = statement.query([])?;
                while let Some(row) = rows.next()? {
                    let name: String = row.get(1)?;
                    let key: i64 = row.get(5)?;
                    let hidden: i64 = row.get(6)?;
                    if hidden != 0 {
                        issue =
                            Some("generated/hidden column execution surface retained only".into());
                    }
                    if key > 0 {
                        pk.push((key, name.clone()));
                    }
                    columns.push(name);
                }
            }
            pk.sort();
            let key: Vec<String> = if schema.without_rowid {
                pk.into_iter().map(|v| v.1).collect()
            } else {
                ["_rowid_", "rowid", "oid"]
                    .iter()
                    .find(|n| !columns.iter().any(|c| c.eq_ignore_ascii_case(n)))
                    .map(|n| vec![n.to_string()])
                    .unwrap_or_default()
            };
            if issue.is_none() && key.is_empty() {
                issue = Some("no safe bounded physical row cursor; snapshot retained only".into());
            }
            let count = if issue.is_none() {
                match source.query_row(
                    &format!("SELECT count(*) FROM {}", identifier(&schema.name)),
                    [],
                    |r| r.get::<_, i64>(0),
                ) {
                    Ok(n) => Some(n),
                    Err(e) => {
                        issue = Some(e.to_string());
                        None
                    }
                }
            } else {
                None
            };
            transaction.execute("INSERT INTO tables(revision,name,columns_json,key_json,schema_json,category,expected,state,issue) VALUES(?,?,?,?,?,?,?,?,?)",params![revision,schema.name,serde_json::to_string(&columns)?,serde_json::to_string(&key)?,serde_json::to_string(&schema)?,category(&schema.name),count,if issue.is_some(){"retained_snapshot_only"}else{"pending"},issue])?;
        }
        file.verify()?;
        transaction.commit()?;
        Ok(revision)
    }
    /// Retain at most max_rows in transactions of 100. Repeating resumes from the
    /// last committed physical key; source IDs and opaque values remain stable.
    pub fn resume(&mut self, revision: &str, max_rows: usize) -> Result<Progress> {
        ensure!(
            (1..=100_000).contains(&max_rows),
            "row budget must be 1..100000"
        );
        let (directory, manifest) = self.capture(revision)?;
        let guard = Source::open(
            &directory.join("logical.sqlite3"),
            manifest.request.limits.max_total_bytes,
        )?;
        ensure!(
            Some(&guard.before) == manifest.logical_revision.as_ref(),
            "logical snapshot revision changed since capture"
        );
        let source = snapshot(&directory.join("logical.sqlite3"), &manifest.request.limits)?;
        let current_stage: String = self.db.query_row(
            "SELECT stage FROM captures WHERE revision=?",
            [revision],
            |r| r.get(0),
        )?;
        if current_stage != "pending" {
            guard.verify()?;
            return Ok(Progress {
                revision_id: revision.into(),
                retained_this_call: 0,
                stage: current_stage,
            });
        }
        let mut retained = 0;
        while retained < max_rows {
            let table=self.db.query_row("SELECT name,columns_json,key_json,cursor FROM tables WHERE revision=? AND state='pending' ORDER BY name LIMIT 1",[revision],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?))).optional()?;
            let Some((table, columns, key, cursor)) = table else {
                break;
            };
            let columns: Vec<String> = serde_json::from_str(&columns)?;
            let keys: Vec<String> = serde_json::from_str(&key)?;
            let result = read_batch(
                &source,
                &table,
                &columns,
                &keys,
                cursor.as_deref(),
                (max_rows - retained).min(100),
                manifest.request.limits.max_cell_bytes,
            );
            match result {
                Err(error) => {
                    let failure = self.db.transaction()?;
                    failure.execute(
                        "UPDATE tables SET state='failed',issue=? WHERE revision=? AND name=?",
                        params![format!("{error:#}"), revision, table],
                    )?;
                    failure.execute("UPDATE captures SET evidence_revision=evidence_revision+1 WHERE revision=?",[revision])?;
                    failure.commit()?;
                }
                Ok(batch) => {
                    guard.verify()?;
                    let transaction = self.db.transaction()?;
                    if batch.is_empty() {
                        transaction.execute("UPDATE tables SET state=CASE WHEN expected=retained THEN 'complete' ELSE 'count_mismatch' END WHERE revision=? AND name=?",params![revision,table])?;
                    }
                    for (key, cells) in &batch {
                        let identity = source_identity(&table, &columns, cells, key)?;
                        let lineage: String = transaction.query_row(
                            "SELECT lineage FROM captures WHERE revision=?",
                            [revision],
                            |r| r.get(0),
                        )?;
                        let source_id = format!("{lineage}:{}", digest(identity.as_bytes()));
                        let key = serde_json::to_string(key)?;
                        transaction.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?,?,?,?,?)",params![revision,source_id,table,key,serde_json::to_string(cells)?])?;
                        retain_xmp(
                            &transaction,
                            revision,
                            &source_id,
                            &table,
                            &columns,
                            cells,
                            manifest.request.limits.max_cell_bytes,
                        )?;
                        retain_references(
                            &transaction,
                            revision,
                            &source_id,
                            &table,
                            &columns,
                            cells,
                        )?;
                        retain_entity(&transaction, revision, &source_id, &table, &columns, cells)?;
                        transaction.execute("UPDATE tables SET retained=retained+1,cursor=? WHERE revision=? AND name=?",params![key,revision,table])?;
                    }
                    transaction.execute("UPDATE captures SET evidence_revision=evidence_revision+1 WHERE revision=?",[revision])?;
                    transaction.commit()?;
                    retained += batch.len();
                }
            }
        }
        let pending: i64 = self.db.query_row(
            "SELECT count(*) FROM tables WHERE revision=? AND state='pending'",
            [revision],
            |r| r.get(0),
        )?;
        guard.verify()?;
        let stage = if pending == 0 {
            self.reconcile(revision)?
        } else {
            "pending".into()
        };
        Ok(Progress {
            revision_id: revision.into(),
            retained_this_call: retained,
            stage,
        })
    }
    fn capture(&self, revision: &str) -> Result<(PathBuf, Manifest)> {
        let (path, manifest): (String, String) = self.db.query_row(
            "SELECT path,manifest FROM captures WHERE revision=?",
            [revision],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((
            serde_json::from_str::<NativePath>(&path)?.to_path()?,
            serde_json::from_str(&manifest)?,
        ))
    }
    pub fn rows(
        &self,
        revision: &str,
        table: Option<&str>,
        after: i64,
        limit: usize,
    ) -> Result<Vec<RetainedRow>> {
        ensure!(
            after >= 0 && (1..=1000).contains(&limit),
            "invalid row page"
        );
        let mut statement = self.db.prepare(if table.is_some() {
            TABLE_ROWS_PAGE
        } else {
            ROWS_PAGE
        })?;
        let mut rows = if let Some(table) = table {
            statement.query(params![revision, after, table, limit as i64])?
        } else {
            statement.query(params![revision, after, limit as i64])?
        };
        let mut out = vec![];
        let mut bytes = 3usize; // Array brackets plus the CLI framing newline.
        while let Some(row) = rows.next()? {
            let json: String = row.get(5)?;
            let key_json: String = row.get(3)?;
            let columns_json: String = row.get(4)?;
            let value = RetainedRow {
                sequence: row.get(0)?,
                source_id: row.get(1)?,
                revision_id: revision.into(),
                table: row.get(2)?,
                source_key: serde_json::from_str(&key_json)?,
                columns: serde_json::from_str(&columns_json)?,
                cells: serde_json::from_str(&json)?,
                semantics: row.get(6)?,
            };
            let size = super::bounded_json(&value, super::PAGE_BYTES)?.len()
                + usize::from(!out.is_empty());
            if bytes + size > super::PAGE_BYTES {
                ensure!(
                    !out.is_empty(),
                    "row exceeds output page byte budget; original snapshot retained"
                );
                break;
            }
            bytes += size;
            out.push(value);
        }
        Ok(out)
    }
    pub fn report(&self, revision: &str) -> Result<Report> {
        let (_, capture) = self.capture(revision)?;
        let (lineage, stage): (String, String) = self.db.query_row(
            "SELECT lineage,stage FROM captures WHERE revision=?",
            [revision],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut statement=self.db.prepare("SELECT name,category,expected,retained,state,issue FROM tables WHERE revision=? ORDER BY name")?;
        let tables = statement
            .query_map([revision], |r| {
                Ok(TableState {
                    name: r.get(0)?,
                    category: r.get(1)?,
                    expected: r.get(2)?,
                    retained: r.get(3)?,
                    state: r.get(4)?,
                    issue: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut counts = BTreeMap::new();
        for table in &tables {
            *counts.entry(table.category.clone()).or_default() += table.retained;
        }
        for (label, query) in [
            (
                "issues_total",
                "SELECT count(*) FROM issues WHERE revision=?",
            ),
            (
                "schema_objects_retained",
                "SELECT count(*) FROM schema_objects WHERE revision=?",
            ),
            (
                "rows_retained",
                "SELECT count(*) FROM rows WHERE revision=?",
            ),
            (
                "catalog_xmp_packets",
                "SELECT count(*) FROM packets WHERE revision=? AND origin='catalog'",
            ),
            (
                "metadata_conflicting_fields",
                "SELECT count(*) FROM (SELECT file_source_id,field FROM metadata_facts WHERE revision=? AND file_source_id IS NOT NULL GROUP BY file_source_id,field HAVING count(DISTINCT value_json)>1)",
            ),
        ] {
            counts.insert(
                label.into(),
                self.db.query_row(query, [revision], |r| r.get(0))?,
            );
        }
        {
            let mut statement = self
                .db
                .prepare("SELECT state,count(*) FROM paths WHERE revision=? GROUP BY state")?;
            for row in statement.query_map([revision], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })? {
                let (state, count) = row?;
                counts.insert(format!("paths_{state}"), count);
            }
        }
        {
            let mut virtuals = 0;
            let mut masters = 0;
            let mut rated = 0;
            let mut statement = self.db.prepare(
                "SELECT fields_json FROM entities WHERE revision=? AND table_name='Adobe_images'",
            )?;
            for row in statement.query_map([revision], |r| r.get::<_, String>(0))? {
                let fields: Fields = serde_json::from_str(&row?)?;
                if fields
                    .get("masterImage")
                    .is_some_and(|v| !absent_reference(v))
                {
                    virtuals += 1;
                } else {
                    masters += 1;
                }
                if fields
                    .get("rating")
                    .and_then(Cell::integer)
                    .is_some_and(|v| v > 0)
                {
                    rated += 1;
                }
            }
            counts.insert("retained_virtual_copies".into(), virtuals);
            counts.insert("retained_master_images".into(), masters);
            counts.insert("retained_rated_images".into(), rated);
        }
        let issues = self
            .issues(revision, 0, 100)?
            .into_iter()
            .map(serde_json::from_value)
            .collect::<serde_json::Result<Vec<Issue>>>()?;
        Ok(Report {protocol:PROTOCOL,revision_id:revision.into(),lineage_id:lineage,stage,capture,tables,counts,issues,semantics:"Known identifiers, relationships and organization are projected for inspection. Table states identify complete typed retention versus original-snapshot-only or incomplete projection. Adobe instructions, plug-in/smart text and unknown constructs retained only.".into(),rendering:"Adobe rendering equivalence is unverified; this plan never executes migration or develop instructions".into()})
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Schema {
    root_page: i64,
    without_rowid: bool,
    kind: String,
    name: String,
    table: String,
    sql: String,
}
fn read_schema(db: &Connection) -> Result<Vec<Schema>> {
    read_schema_bounded(db, 4096, super::PAGE_BYTES)
}
fn read_schema_bounded(
    db: &Connection,
    max_objects: usize,
    max_bytes: usize,
) -> Result<Vec<Schema>> {
    let mut native = BTreeMap::new();
    let mut native_bytes = 0usize;
    let mut statement = db.prepare("PRAGMA table_list")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(0)? != "main" {
            continue;
        }
        ensure!(
            native.len() < max_objects,
            "source schema object admission limit exceeded"
        );
        let name: String = row.get(1)?;
        native_bytes = native_bytes
            .checked_add(name.len())
            .context("schema name byte overflow")?;
        ensure!(
            native_bytes <= max_bytes,
            "source schema name byte admission limit exceeded"
        );
        native.insert(name, row.get::<_, i64>(4)? != 0);
    }
    let mut statement=db.prepare("SELECT type,name,tbl_name,coalesce(sql,''),rootpage FROM sqlite_schema ORDER BY type,name LIMIT ?")?;
    let mut rows = statement.query([i64::try_from(max_objects)?
        .checked_add(1)
        .context("schema count overflow")?])?;
    let mut schemas = vec![];
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        ensure!(
            schemas.len() < max_objects,
            "source schema object admission limit exceeded"
        );
        let schema = Schema {
            kind: row.get(0)?,
            name: row.get(1)?,
            table: row.get(2)?,
            sql: row.get(3)?,
            root_page: row.get(4)?,
            without_rowid: false,
        };
        bytes = bytes
            .checked_add(
                schema.name.len() + schema.table.len() + schema.kind.len() + schema.sql.len(),
            )
            .context("schema byte overflow")?;
        ensure!(
            bytes <= max_bytes,
            "source schema byte admission limit exceeded; raw snapshot retained"
        );
        let without_rowid = native.get(&schema.name).copied().unwrap_or(false);
        schemas.push(Schema {
            without_rowid,
            ..schema
        });
    }
    Ok(schemas)
}
fn read_variables(db: &Connection, schemas: &[Schema]) -> Result<BTreeMap<String, String>> {
    let Some(schema) = schemas
        .iter()
        .find(|s| s.name == "Adobe_variablesTable" && s.kind == "table")
    else {
        return Ok(BTreeMap::new());
    };
    ensure!(
        schema.root_page > 0,
        "variables table has no ordinary physical root"
    );
    let mut result = BTreeMap::new();
    let mut info = db.prepare("PRAGMA table_xinfo('Adobe_variablesTable')")?;
    let mut fields = info.query([])?;
    while let Some(field) = fields.next()? {
        if field.get::<_, i64>(6)? != 0 {
            return Ok(result);
        }
    }
    let Ok(mut statement)=db.prepare("SELECT name,value FROM Adobe_variablesTable WHERE name IN ('Adobe_DBVersion','Adobe_storeProviderID')") else {return Ok(result)};
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if let (ValueRef::Text(name), ValueRef::Text(value)) = (row.get_ref(0)?, row.get_ref(1)?) {
            result.insert(
                String::from_utf8(name.into())?,
                String::from_utf8(value.into())?,
            );
        }
    }
    Ok(result)
}
fn category(table: &str) -> &'static str {
    match table {
        "Adobe_images" => "images_and_virtual_copies",
        "AgLibraryFile" => "files",
        "AgLibraryFolder" | "AgLibraryRootFolder" => "folders",
        "AgLibraryKeyword" | "AgLibraryKeywordImage" | "AgLibraryKeywordSynonym" => {
            "keywords_hierarchy_membership_synonyms"
        }
        "AgLibraryCollection" | "AgLibraryCollectionImage" => "collections_membership_order",
        "Adobe_imageDevelopSettings" | "Adobe_imageDevelopBeforeSettings" => {
            "develop_retained_only"
        }
        "Adobe_libraryImageDevelopHistoryStep" => "history_retained_only",
        "Adobe_libraryImageDevelopSnapshot" => "snapshots_retained_only",
        "Adobe_AdditionalMetadata" => "catalog_xmp",
        "AgLibraryCollectionContent" => "smart_collection_instructions_retained_only",
        n if n.starts_with("AgHarvested")
            || n.starts_with("AgInterned")
            || n == "AgLibraryIPTC" =>
        {
            "photographic_metadata"
        }
        _ => "unsupported_retained_only",
    }
}
impl rusqlite::ToSql for Cell {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        let value = match self {
            Cell::Null => ValueRef::Null,
            Cell::Integer(v) => ValueRef::Integer(*v),
            Cell::RealBits(v) => ValueRef::Real(f64::from_bits(*v)),
            Cell::Text(v) => ValueRef::Text(v),
            Cell::Blob(v) => ValueRef::Blob(v),
        };
        Ok(rusqlite::types::ToSqlOutput::Borrowed(value))
    }
}
type Batch = Vec<(Vec<Cell>, Vec<Cell>)>;
fn read_batch(
    db: &Connection,
    table: &str,
    columns: &[String],
    keys: &[String],
    cursor: Option<&str>,
    limit: usize,
    max_cell: usize,
) -> Result<Batch> {
    let select = keys
        .iter()
        .chain(columns)
        .map(|v| identifier(v))
        .collect::<Vec<_>>()
        .join(",");
    let order = keys
        .iter()
        .map(|v| identifier(v))
        .collect::<Vec<_>>()
        .join(",");
    let cells: Vec<Cell> = cursor
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    ensure!(
        cells.is_empty() || cells.len() == keys.len(),
        "cursor arity mismatch"
    );
    let predicate = if cells.is_empty() {
        String::new()
    } else {
        format!(" WHERE ({order}) > ({})", vec!["?"; keys.len()].join(","))
    };
    let query = format!(
        "SELECT {select} FROM {}{predicate} ORDER BY {order} LIMIT {limit}",
        identifier(table)
    );
    let mut statement = db.prepare(&query)?;
    let mut rows = statement.query(rusqlite::params_from_iter(cells.iter()))?;
    let mut batch = vec![];
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        let size = (0..keys.len() + columns.len()).try_fold(0usize, |sum, i| -> Result<usize> {
            let size = match row.get_ref(i)? {
                ValueRef::Text(v) | ValueRef::Blob(v) => {
                    v.len().checked_mul(2).context("cell byte overflow")? + 64
                }
                _ => 64,
            };
            sum.checked_add(size).context("row byte overflow")
        })?;
        ensure!(
            size <= super::PAGE_BYTES,
            "row exceeds derivative batch byte budget; original snapshot retains it"
        );
        if bytes + size > super::PAGE_BYTES {
            break;
        }
        bytes += size;
        let all = (0..keys.len() + columns.len())
            .map(|i| Cell::from_sql(row.get_ref(i)?, max_cell))
            .collect::<Result<Vec<_>>>()?;
        batch.push((all[..keys.len()].into(), all[keys.len()..].into()));
    }
    Ok(batch)
}
fn get<'a>(columns: &[String], cells: &'a [Cell], name: &str) -> Option<&'a Cell> {
    columns
        .iter()
        .position(|s| s == name)
        .and_then(|i| cells.get(i))
}
fn source_identity(
    table: &str,
    columns: &[String],
    cells: &[Cell],
    key: &[Cell],
) -> Result<String> {
    // Include original local identity as well as global ID: duplicate global IDs
    // must remain separate source rows and be diagnosed rather than deduplicated.
    Ok(serde_json::to_string(&(
        table,
        get(columns, cells, "id_global"),
        get(columns, cells, "id_local"),
        key,
    ))?)
}
pub fn decode_catalog_xmp(bytes: &[u8], maximum: usize) -> Result<Vec<u8>> {
    ensure!(bytes.len() >= 6, "unknown or truncated catalog XMP wrapper");
    let expected = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    ensure!(expected <= maximum, "catalog XMP expansion limit exceeded");
    let mut decoder = flate2::Decompress::new(true);
    let mut out = vec![0u8; expected.checked_add(1).context("XMP length overflow")?];
    let status = decoder.decompress(&bytes[4..], &mut out, flate2::FlushDecompress::Finish)?;
    ensure!(
        status == flate2::Status::StreamEnd
            && decoder.total_in() == (bytes.len() - 4) as u64
            && decoder.total_out() == expected as u64,
        "catalog XMP length/stream/trailing data mismatch"
    );
    out.truncate(expected);
    Ok(out)
}
fn retain_xmp(
    db: &Connection,
    revision: &str,
    id: &str,
    table: &str,
    columns: &[String],
    cells: &[Cell],
    limit: usize,
) -> Result<()> {
    if table != "Adobe_AdditionalMetadata" {
        return Ok(());
    }
    let Some(cell) = get(columns, cells, "xmp") else {
        return Ok(());
    };
    let (raw, result) = match cell {
        Cell::Blob(bytes) => (bytes, decode_catalog_xmp(bytes, limit)),
        Cell::Text(bytes) => (bytes, Ok(bytes.clone())),
        Cell::Null => return Ok(()),
        _ => return Ok(()),
    };
    let (decoded, detail) = match result {
        Ok(v) => (
            Some(v),
            "decoded parse input; original typed cell retained separately".to_owned(),
        ),
        Err(e) => {
            add_issue(
                db,
                revision,
                Some(id),
                "xmp_decode_failed",
                &format!("{e:#}"),
            )?;
            (None, format!("retained only: {e:#}"))
        }
    };
    if let Some(bytes) = &decoded {
        retain_facts(db, revision, id, None, "catalog", &digest(raw), bytes)?;
    }
    db.execute("INSERT OR IGNORE INTO packets(revision,source_id,origin,raw_digest,raw,decoded,detail) VALUES(?,?,?,?,?,?,?)",params![revision,id,"catalog",digest(raw),raw,decoded,detail])?;
    Ok(())
}
fn add_issue(
    db: &Connection,
    revision: &str,
    id: Option<&str>,
    code: &str,
    detail: &str,
) -> Result<()> {
    // SQLite NULL uniqueness is intentionally avoided for idempotent summary issues.
    db.execute(
        "INSERT OR IGNORE INTO issues(revision,source_id,code,detail) VALUES(?,?,?,?)",
        params![revision, id.unwrap_or(""), code, detail],
    )?;
    Ok(())
}
// This normalization belongs only to derived relationship identifiers. Raw
// Cell encodings, retained rows, source/global IDs and opaque facts never use it.
// The upper bound is exclusive: i64::MAX as f64 rounds UP to 2^63, and Rust's
// saturating float cast would otherwise conflate that value with i64::MAX.
fn relationship_integer(cell: &Cell) -> Option<i64> {
    match cell {
        Cell::Integer(value) => Some(*value),
        Cell::RealBits(bits) => {
            let value = f64::from_bits(*bits);
            if value.is_finite()
                && value.fract() == 0.0
                && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value)
            {
                Some(value as i64)
            } else {
                None
            }
        }
        _ => None,
    }
}
fn absent_reference(cell: &Cell) -> bool {
    matches!(cell, Cell::Null) || relationship_integer(cell) == Some(0)
}
fn relationship_key(cell: &Cell) -> Result<String> {
    Ok(match relationship_integer(cell) {
        Some(value) => serde_json::to_string(&Cell::Integer(value))?,
        None => serde_json::to_string(cell)?,
    })
}

fn relation_fields(table: &str) -> &'static [(&'static str, &'static str)] {
    match table {
        "Adobe_images" => &[
            ("rootFile", "AgLibraryFile"),
            ("masterImage", "Adobe_images"),
            ("developSettingsIDCache", "Adobe_imageDevelopSettings"),
        ],
        "AgLibraryFile" => &[("folder", "AgLibraryFolder")],
        "AgLibraryFolder" => &[
            ("rootFolder", "AgLibraryRootFolder"),
            ("parentId", "AgLibraryFolder"),
        ],
        "AgLibraryKeyword" => &[("parent", "AgLibraryKeyword")],
        "AgLibraryKeywordImage" => &[("image", "Adobe_images"), ("tag", "AgLibraryKeyword")],
        "AgLibraryKeywordSynonym" => &[("keyword", "AgLibraryKeyword")],
        "AgLibraryCollection" => &[("parent", "AgLibraryCollection")],
        "AgLibraryCollectionImage" => &[
            ("image", "Adobe_images"),
            ("collection", "AgLibraryCollection"),
        ],
        "AgLibraryCollectionContent" => &[("collection", "AgLibraryCollection")],
        "Adobe_imageDevelopSettings"
        | "Adobe_libraryImageDevelopHistoryStep"
        | "Adobe_libraryImageDevelopSnapshot"
        | "Adobe_AdditionalMetadata"
        | "AgHarvestedExifMetadata"
        | "AgHarvestedIptcMetadata"
        | "AgLibraryIPTC"
        | "AgVideoInfo" => &[("image", "Adobe_images")],
        "Adobe_imageDevelopBeforeSettings" => &[("developSettings", "Adobe_imageDevelopSettings")],
        _ => &[],
    }
}
fn retain_references(
    db: &Connection,
    revision: &str,
    id: &str,
    table: &str,
    columns: &[String],
    cells: &[Cell],
) -> Result<()> {
    for &(field, target) in relation_fields(table) {
        if let Some(cell) = get(columns, cells, field)
            && !absent_reference(cell)
        {
            db.execute("INSERT OR IGNORE INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?,?,?,?,?)",params![revision,id,field,target,relationship_key(cell)?])?;
        }
    }
    Ok(())
}
type Fields = BTreeMap<String, Cell>;
fn retain_entity(
    db: &Connection,
    revision: &str,
    id: &str,
    table: &str,
    columns: &[String],
    cells: &[Cell],
) -> Result<()> {
    let wanted = [
        "id_local",
        "id_global",
        "rootFile",
        "masterImage",
        "copyName",
        "copyReason",
        "rating",
        "pick",
        "colorLabels",
        "captureTime",
        "touchTime",
        "touchCount",
        "image",
        "folder",
        "rootFolder",
        "parentId",
        "parent",
        "name",
        "baseName",
        "extension",
        "idx_filename",
        "absolutePath",
        "pathFromRoot",
        "relativePathFromCatalog",
        "fileFormat",
        "fileWidth",
        "fileHeight",
        "hasBigData",
        "hasAIMasks",
        "hasMasks",
        "hasLensBlur",
        "processVersion",
        "dateCreated",
        "positionInCollection",
        "positionInFolder",
        "sidecarExtensions",
        "digest",
        "settingsID",
        "snapshotID",
    ];
    let fields: Fields = wanted
        .iter()
        .filter_map(|n| get(columns, cells, n).map(|c| (n.to_string(), c.clone())))
        .collect();
    let local = get(columns, cells, "id_local")
        .filter(|cell| !absent_reference(cell))
        .map(relationship_key)
        .transpose()?;
    let global = get(columns, cells, "id_global")
        .filter(|c| !matches!(c, Cell::Null))
        .map(serde_json::to_string)
        .transpose()?;
    db.execute(
        "INSERT INTO entities VALUES(?,?,?,?,?,?)",
        params![
            revision,
            id,
            table,
            local,
            global,
            serde_json::to_string(&fields)?
        ],
    )?;
    Ok(())
}
impl Plan {
    fn reconcile(&mut self, revision: &str) -> Result<String> {
        let transaction = self.db.transaction()?;
        transaction.execute("DELETE FROM issues WHERE revision=? AND code IN ('dangling_reference','duplicate_local_id','duplicate_global_id','hierarchy_cycle','hierarchy_depth_limit','ambiguous_hierarchy_reference','required_auxiliary_missing','unknown_schema_version')",[revision])?;
        {
            let mut statement=transaction.prepare("SELECT r.source_id,r.field,r.target_table,r.target_key FROM references_out r WHERE r.revision=? AND NOT EXISTS(SELECT 1 FROM entities e WHERE e.revision=r.revision AND e.table_name=r.target_table AND e.local_key=r.target_key) ORDER BY r.sequence")?;
            let mut rows = statement.query([revision])?;
            while let Some(row) = rows.next()? {
                add_issue(
                    &transaction,
                    revision,
                    Some(&row.get::<_, String>(0)?),
                    "dangling_reference",
                    &format!(
                        "{} -> {} {}",
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?
                    ),
                )?;
            }
        }
        for (column, code) in [
            ("local_key", "duplicate_local_id"),
            ("global_key", "duplicate_global_id"),
        ] {
            let mut statement=transaction.prepare(&format!("SELECT source_id,table_name,{column} FROM entities e WHERE revision=?1 AND {column} IS NOT NULL AND (SELECT count(*) FROM entities other WHERE other.revision=e.revision AND other.table_name=e.table_name AND other.{column}=e.{column})>1"))?;
            let mut rows = statement.query([revision])?;
            while let Some(row) = rows.next()? {
                add_issue(
                    &transaction,
                    revision,
                    Some(&row.get::<_, String>(0)?),
                    code,
                    &format!("{} {}", row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                )?;
            }
        }
        let (manifest, version): (String, Option<String>) = transaction.query_row(
            "SELECT manifest,schema_version FROM captures WHERE revision=?",
            [revision],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let manifest: Manifest = serde_json::from_str(&manifest)?;
        if !matches!(version.as_deref(), Some("1100000" | "1300000")) {
            add_issue(
                &transaction,
                revision,
                None,
                "unknown_schema_version",
                &format!(
                    "Observed schema {:?}; typed rows retained, projected semantics unverified",
                    version
                ),
            )?;
        }
        let aux_present = manifest
            .artifacts
            .iter()
            .any(|a| a.role == "auxiliary" && a.revision.bytes > 0);
        if !aux_present {
            let mut statement=transaction.prepare("SELECT source_id,fields_json FROM entities WHERE revision=? AND table_name IN ('Adobe_imageDevelopSettings','Adobe_imageDevelopBeforeSettings','Adobe_libraryImageDevelopHistoryStep','Adobe_libraryImageDevelopSnapshot')")?;
            let mut rows = statement.query([revision])?;
            while let Some(row) = rows.next()? {
                let fields: Fields = serde_json::from_str(&row.get::<_, String>(1)?)?;
                if ["hasBigData", "hasAIMasks", "hasLensBlur"].iter().any(|n| {
                    fields
                        .get(*n)
                        .and_then(Cell::numeric)
                        .is_some_and(|v| v != 0.0)
                }) {
                    add_issue(
                        &transaction,
                        revision,
                        Some(&row.get::<_, String>(0)?),
                        "required_auxiliary_missing",
                        "Source instructions reference auxiliary data but no nonempty companion bytes were retained",
                    )?;
                }
            }
        }
        for (table, field) in [
            ("AgLibraryKeyword", "parent"),
            ("AgLibraryCollection", "parent"),
            ("AgLibraryFolder", "parentId"),
            ("Adobe_images", "masterImage"),
        ] {
            let mut statement=transaction.prepare("SELECT source_id,local_key FROM entities WHERE revision=? AND table_name=? AND local_key IS NOT NULL")?;
            let mut rows = statement.query(params![revision, table])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let start: String = row.get(1)?;
                let mut key = start;
                let mut seen = BTreeSet::new();
                for depth in 0..256 {
                    if !seen.insert(key.clone()) {
                        add_issue(
                            &transaction,
                            revision,
                            Some(&id),
                            "hierarchy_cycle",
                            &format!("{table}.{field} revisits retained local key {key}"),
                        )?;
                        break;
                    }
                    let mut matches = transaction.prepare("SELECT fields_json FROM entities WHERE revision=? AND table_name=? AND local_key=? LIMIT 2")?;
                    let mut matches = matches.query(params![revision, table, key])?;
                    let Some(first) = matches.next()? else { break };
                    let fields: String = first.get(0)?;
                    if matches.next()?.is_some() {
                        add_issue(
                            &transaction,
                            revision,
                            Some(&id),
                            "ambiguous_hierarchy_reference",
                            &format!(
                                "{table}.{field} reaches multiple retained entities at local key {key}; traversal stopped"
                            ),
                        )?;
                        break;
                    }
                    let fields: Fields = serde_json::from_str(&fields)?;
                    let Some(parent) = fields.get(field) else {
                        break;
                    };
                    if absent_reference(parent) {
                        break;
                    }
                    key = relationship_key(parent)?;
                    if depth == 255 {
                        add_issue(
                            &transaction,
                            revision,
                            Some(&id),
                            "hierarchy_depth_limit",
                            "Hierarchy exceeded 256 verified edges; not reported acyclic",
                        )?;
                    }
                }
            }
        }
        // File paths are derivative locators. Original strings remain in typed rows.
        {
            let mut statement=transaction.prepare("SELECT source_id,fields_json FROM entities WHERE revision=? AND table_name='AgLibraryFile'")?;
            let mut rows = statement.query([revision])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let fields: Fields = serde_json::from_str(&row.get::<_, String>(1)?)?;
                let result = original_path(&transaction, revision, &fields);
                let (original, path, state) = match result {
                    Ok((original, path)) => {
                        (original, Some(serde_json::to_string(&path)?), "pending")
                    }
                    Err(e) => {
                        add_issue(
                            &transaction,
                            revision,
                            Some(&id),
                            "unresolved_original_path",
                            &format!("{e:#}"),
                        )?;
                        (serde_json::to_string(&fields)?, None, "unresolved")
                    }
                };
                transaction.execute("INSERT OR IGNORE INTO paths(revision,source_id,original,inspection_path,state) VALUES(?,?,?,?,?)",params![revision,id,original,path,state])?;
            }
        }
        associate_catalog_facts(&transaction, revision)?;
        let failures:i64=transaction.query_row("SELECT count(*) FROM tables WHERE revision=? AND state NOT IN ('complete','retained_snapshot_only')",[revision],|r|r.get(0))?;
        let stage = if failures == 0 {
            "rows_reconciled_paths_pending"
        } else {
            "incomplete_rows"
        };
        transaction.execute(
            "UPDATE captures SET stage=? WHERE revision=?",
            params![stage, revision],
        )?;
        transaction.commit()?;
        Ok(stage.into())
    }
    /// Bounded direct-path inspection. No directory recursion or root guessing.
    /// `packets=false` performs metadata lookups only and leaves explicit XMP gaps.
    pub fn check_paths(&mut self, revision: &str, limit: usize, packets: bool) -> Result<usize> {
        ensure!((1..=1000).contains(&limit), "path budget must be 1..1000");
        let (_, manifest) = self.capture(revision)?;
        let selected = {
            let mut statement = self.db.prepare(if packets {
                PATH_PACKET_QUEUE
            } else {
                PATH_QUEUE
            })?;
            let mut rows = statement.query(params![revision, limit as i64])?;
            let mut selected = vec![];
            let mut bytes = 0usize;
            while let Some(row) = rows.next()? {
                let id: String = row.get(1)?;
                let path: String = row.get(2)?;
                let size = id.len() + path.len() + 64;
                if bytes + size > super::PAGE_BYTES {
                    ensure!(
                        !selected.is_empty(),
                        "path inspection descriptor exceeds byte budget; original evidence retained"
                    );
                    break;
                }
                bytes += size;
                selected.push((row.get::<_, i64>(0)?, id, path));
            }
            selected
        };
        for (sequence, id, path) in &selected {
            let transaction = self.db.transaction()?;
            let native: NativePath = serde_json::from_str(path)?;
            let (state, evidence) = match native.to_path() {
                Err(e) => (
                    "foreign_path".to_owned(),
                    serde_json::json!({"error":e.to_string(),"path":native}),
                ),
                Ok(path) => {
                    let (base, metadata) = match fs::symlink_metadata(&path) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            ("missing", serde_json::json!({"missing":true}))
                        }
                        Err(e) => ("unavailable", serde_json::json!({"error":e.to_string()})),
                        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
                            ("non_regular", serde_json::json!({"regular":false}))
                        }
                        Ok(meta) => ("available", serde_json::json!({"bytes":meta.len()})),
                    };
                    if packets {
                        // A missing original does not imply a missing sidecar.
                        let (inspections, gaps) = inspect_packets(
                            &transaction,
                            revision,
                            id,
                            &path,
                            &manifest.request.limits,
                        )?;
                        let state = if base == "available" {
                            if gaps {
                                "available_packet_gaps"
                            } else {
                                "available_packets_retained"
                            }
                        } else {
                            base
                        };
                        (
                            state.to_owned(),
                            serde_json::json!({"metadata":metadata,"inspections":inspections,"packet_gaps":gaps}),
                        )
                    } else {
                        (
                            if base == "available" {
                                "available_packets_uninspected"
                            } else {
                                base
                            }
                            .to_owned(),
                            serde_json::json!({"metadata":metadata,"embedded_sidecar_xmp":"not inspected by explicit metadata-only mode"}),
                        )
                    }
                }
            };
            transaction.execute(
                "UPDATE paths SET state=?,evidence=? WHERE sequence=?",
                params![state, serde_json::to_string(&evidence)?, sequence],
            )?;
            transaction.execute(
                "UPDATE captures SET evidence_revision=evidence_revision+1 WHERE revision=?",
                [revision],
            )?;
            transaction.commit()?;
        }
        let pending: i64 = self.db.query_row(PATH_PENDING, [revision], |r| r.get(0))?;
        if pending == 0 {
            self.db.execute("UPDATE captures SET stage='inspection_complete_with_reported_gaps' WHERE revision=? AND stage='rows_reconciled_paths_pending'",[revision])?;
        }
        Ok(selected.len())
    }
    pub fn paths(
        &self,
        revision: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        ensure!(
            (1..=1000).contains(&limit) && after >= 0,
            "invalid path page"
        );
        let mut statement = self.db.prepare(PATHS_PAGE)?;
        let mut rows = statement.query(params![revision, after, limit as i64])?;
        let mut out = vec![];
        let mut bytes = 3; // Array brackets plus the CLI framing newline.
        while let Some(row) = rows.next()? {
            let value = serde_json::json!({"sequence":row.get::<_,i64>(0)?,"source_id":row.get::<_,String>(1)?,"original":row.get::<_,String>(2)?,"inspection_path":row.get::<_,Option<String>>(3)?.map(|v|serde_json::from_str::<serde_json::Value>(&v)).transpose()?,"state":row.get::<_,String>(4)?,"evidence":row.get::<_,Option<String>>(5)?.map(|v|serde_json::from_str::<serde_json::Value>(&v)).transpose()?});
            if !append_value(&mut out, &mut bytes, value)? {
                break;
            }
        }
        Ok(out)
    }
}
fn entity_fields(db: &Connection, revision: &str, table: &str, key: &Cell) -> Result<Fields> {
    ensure!(!absent_reference(key), "missing original-path relation");
    let key = relationship_key(key)?;
    let mut statement=db.prepare("SELECT fields_json FROM entities WHERE revision=? AND table_name=? AND local_key=? LIMIT 2")?;
    let mut rows = statement.query(params![revision, table, key])?;
    let fields = rows
        .next()?
        .context("missing original-path relation")?
        .get::<_, String>(0)?;
    ensure!(rows.next()?.is_none(), "ambiguous original-path relation");
    Ok(serde_json::from_str(&fields)?)
}
fn original_path(db: &Connection, revision: &str, file: &Fields) -> Result<(String, NativePath)> {
    let folder = entity_fields(
        db,
        revision,
        "AgLibraryFolder",
        file.get("folder").context("file folder reference absent")?,
    )?;
    let root = entity_fields(
        db,
        revision,
        "AgLibraryRootFolder",
        folder
            .get("rootFolder")
            .context("folder root reference absent")?,
    )?;
    let root = root
        .get("absolutePath")
        .and_then(Cell::text)
        .context("root path is not usable UTF-8 text; exact cell retained")?;
    let relative = folder
        .get("pathFromRoot")
        .and_then(Cell::text)
        .context("folder relative path absent")?;
    let name = file
        .get("idx_filename")
        .and_then(Cell::text)
        .context("filename absent")?;
    ensure!(
        !name.is_empty() && !name.contains(['/', '\\', '\0']),
        "unsafe source filename"
    );
    ensure!(
        !relative.starts_with(['/', '\\'])
            && !relative
                .split(['/', '\\'])
                .any(|v| v == ".." || v.contains('\0')),
        "unsafe folder-relative path"
    );
    let windows = root.as_bytes().get(1) == Some(&b':') || root.starts_with("\\\\");
    let original = format!(
        "{}{}{}{}{}",
        root,
        if root.ends_with(['/', '\\']) {
            ""
        } else if windows {
            "\\"
        } else {
            "/"
        },
        relative,
        if relative.is_empty() || relative.ends_with(['/', '\\']) {
            ""
        } else if windows {
            "\\"
        } else {
            "/"
        },
        name
    );
    let path = if windows {
        // Adobe folder components may use '/'. Windows' verbatim namespace
        // disables slash normalization, so construct native separators for I/O
        // while retaining the exact composed locator in `original` below.
        #[cfg(windows)]
        let native = original.replace('/', "\\");
        #[cfg(not(windows))]
        let native = &original;
        NativePath::WindowsWide(native.encode_utf16().collect())
    } else {
        ensure!(root.starts_with('/'), "root is not an absolute path");
        NativePath::UnixBytes(original.as_bytes().into())
    };
    Ok((original, path))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub revision_id: String,
    pub source: NativePath,
    pub filename_hint: Option<String>,
    pub schema_version: Option<String>,
    pub provider: Option<String>,
    pub modified_ns: Option<u128>,
    pub images: i64,
    pub files: i64,
    pub image_identity_digest: String,
    pub file_identity_digest: String,
    pub latest_image_touch: Option<f64>,
    pub latest_history: Option<f64>,
    pub latest_snapshot: Option<f64>,
    pub row_stage: String,
    pub inspection_evidence_revision: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Family {
    pub id: String,
    pub members: Vec<Member>,
    pub suggested: Option<String>,
    pub selected: Option<String>,
    pub excluded: Vec<String>,
    pub issues: Vec<String>,
    pub evidence_digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FamilyReport {
    pub families: Vec<Family>,
    pub uninspected_candidates: Vec<super::discovery::Candidate>,
    pub inventory_complete: bool,
    pub cross_catalog_conflicts: Vec<serde_json::Value>,
    pub conflict_count: i64,
    pub possible_path_collision_count: i64,
    pub possible_path_collisions: Vec<serde_json::Value>,
}
impl Plan {
    pub fn register_inventory(
        &mut self,
        inventory: &super::discovery::Inventory,
    ) -> Result<String> {
        ensure!(
            inventory.protocol == PROTOCOL,
            "unsupported discovery protocol"
        );
        let json = serde_json::to_string(inventory)?;
        ensure!(
            json.len() <= super::MANIFEST_BYTES,
            "inventory exceeds manifest budget"
        );
        let hash = digest(json.as_bytes());
        self.db.execute(
            "INSERT OR IGNORE INTO inventories VALUES(?,?)",
            params![hash, json],
        )?;
        Ok(hash)
    }
    pub fn assign_family(&mut self, revision: &str, family: &str, reason: &str) -> Result<()> {
        self.capture(revision)?;
        ensure!(
            !family.trim().is_empty()
                && family.len() <= 256
                && !reason.trim().is_empty()
                && reason.len() <= 4096,
            "explicit bounded family and reason required"
        );
        self.db.execute("INSERT INTO family_assignments VALUES(?,?,?) ON CONFLICT(revision) DO UPDATE SET family=excluded.family,reason=excluded.reason",params![revision,family,reason])?;
        Ok(())
    }
    pub fn families(&self) -> Result<FamilyReport> {
        let _read_snapshot = self.db.unchecked_transaction()?;
        let captures = {
            let mut statement=self.db.prepare("SELECT revision,schema_version,provider,stage,evidence_revision FROM captures ORDER BY revision LIMIT 257")?;
            statement
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        ensure!(
            captures.len() <= 256,
            "family comparison exceeds 256-capture admission limit"
        );
        let mut members = vec![];
        for (revision, schema, provider, stage, evidence_revision) in captures {
            let (_, manifest) = self.capture(&revision)?;
            let filename = manifest
                .request
                .source
                .to_path()?
                .file_stem()
                .and_then(|s| s.to_str())
                .map(super::discovery::filename_hint)
                .map(|v| v.0);
            let (images, image_digest) = identity_summary(&self.db, &revision, "Adobe_images")?;
            let (files, file_digest) = identity_summary(&self.db, &revision, "AgLibraryFile")?;
            members.push(Member {
                revision_id: revision.clone(),
                source: manifest.request.source,
                schema_version: schema,
                provider,
                filename_hint: filename,
                modified_ns: manifest
                    .artifacts
                    .iter()
                    .find(|a| a.role == "main")
                    .and_then(|a| a.revision.modified_ns),
                images,
                files,
                image_identity_digest: image_digest,
                file_identity_digest: file_digest,
                latest_image_touch: max_field(&self.db, &revision, "Adobe_images", "touchTime")?,
                latest_history: max_field(
                    &self.db,
                    &revision,
                    "Adobe_libraryImageDevelopHistoryStep",
                    "dateCreated",
                )?,
                latest_snapshot: max_field(
                    &self.db,
                    &revision,
                    "Adobe_libraryImageDevelopSnapshot",
                    "dateCreated",
                )?,
                row_stage: stage,
                inspection_evidence_revision: evidence_revision,
            });
        }
        let assignments = {
            let mut statement = self
                .db
                .prepare("SELECT revision,family FROM family_assignments")?;
            statement
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?
        };
        let mut parents: Vec<usize> = (0..members.len()).collect();
        fn root(parents: &[usize], mut i: usize) -> usize {
            while parents[i] != i {
                i = parents[i];
            }
            i
        }
        for i in 0..members.len() {
            for j in 0..i {
                let a = &members[i];
                let b = &members[j];
                let assigned = match (
                    assignments.get(&a.revision_id),
                    assignments.get(&b.revision_id),
                ) {
                    (Some(a), Some(b)) => Some(a == b),
                    (Some(_), None) | (None, Some(_)) => Some(false),
                    _ => None,
                };
                let named = a.filename_hint.is_some() && a.filename_hint == b.filename_hint;
                let provider = a.provider.is_some() && a.provider == b.provider;
                let overlap = if provider {
                    overlap(&self.db, &a.revision_id, &b.revision_id)?
                } else {
                    0
                };
                if assigned.unwrap_or(named || (provider && overlap > 0)) {
                    let x = root(&parents, i);
                    let y = root(&parents, j);
                    parents[x] = y;
                }
            }
        }
        let mut grouped: BTreeMap<usize, Vec<Member>> = BTreeMap::new();
        for (index, member) in members.iter().enumerate() {
            grouped
                .entry(root(&parents, index))
                .or_default()
                .push(member.clone());
        }
        let mut inventories = vec![];
        {
            let mut statement = self
                .db
                .prepare("SELECT json FROM inventories ORDER BY digest")?;
            for row in statement.query_map([], |r| r.get::<_, String>(0))? {
                inventories.push(serde_json::from_str::<super::discovery::Inventory>(&row?)?);
            }
        }
        let inventory_complete = !inventories.is_empty() && inventories.iter().all(|i| i.complete);
        let paths: BTreeSet<_> = members.iter().map(|m| m.source.clone()).collect();
        let mut uninspected = BTreeMap::new();
        for inventory in &inventories {
            for candidate in &inventory.candidates {
                if !paths.contains(&candidate.path) {
                    uninspected.insert(candidate.path.clone(), candidate.clone());
                }
            }
        }
        let mut families = vec![];
        for mut group in grouped.into_values() {
            group.sort_by(|a, b| a.revision_id.cmp(&b.revision_id));
            let id = assignments
                .get(&group[0].revision_id)
                .map(|s| format!("explicit:{s}"))
                .unwrap_or_else(|| {
                    format!(
                        "evidence:{}",
                        group
                            .iter()
                            .filter_map(|m| m.filename_hint.as_ref())
                            .min()
                            .cloned()
                            .unwrap_or_else(|| group[0].revision_id.clone())
                    )
                });
            let mut issues = vec![];
            if group.iter().any(|m| {
                !m.row_stage.starts_with("rows_reconciled")
                    && !m.row_stage.starts_with("inspection_complete")
            }) {
                issues.push("One or more candidate row inspections are incomplete".into());
            }
            if !inventory_complete {
                issues.push(
                    "Discovery inventory is absent or incomplete; unseen catalogs may exist".into(),
                );
            }
            if uninspected
                .values()
                .any(|c| group.iter().any(|m| m.filename_hint == c.filename_hint))
            {
                issues.push("Filename-related candidates lack internal inspection evidence".into());
            }
            if group.iter().any(|m| m.latest_snapshot.is_some())
                && group.iter().any(|m| m.latest_snapshot.is_none())
            {
                issues.push("Snapshot recency evidence is incomparable: some candidates have no comparable snapshot timestamp".into());
            }
            let mut suggested = None;
            if group.len() == 1 {
                suggested = Some(group[0].revision_id.clone());
            } else {
                let mut dominating = vec![];
                for a in &group {
                    let mut dominates = true;
                    let mut strictly = false;
                    for b in &group {
                        if a.revision_id == b.revision_id {
                            continue;
                        }
                        let evidence = [
                            (a.latest_image_touch, b.latest_image_touch),
                            (a.latest_history, b.latest_history),
                            (a.latest_snapshot, b.latest_snapshot),
                        ];
                        for (av, bv) in evidence {
                            match (av, bv) {
                                (Some(x), Some(y)) if x < y => dominates = false,
                                (Some(x), Some(y)) if x > y => strictly = true,
                                (None, Some(_)) | (Some(_), None) => dominates = false,
                                _ => {}
                            }
                        }
                        if a.images < b.images || a.files < b.files {
                            dominates = false;
                        }
                        if a.provider != b.provider
                            || a.provider.is_none()
                            || overlap(&self.db, &a.revision_id, &b.revision_id)? < b.images
                        {
                            dominates = false;
                        }
                    }
                    if dominates && strictly {
                        dominating.push(a.revision_id.clone());
                    }
                }
                if dominating.len() == 1 {
                    suggested = dominating.pop();
                }
                let content_equal = group.iter().all(|m| {
                    m.image_identity_digest == group[0].image_identity_digest
                        && m.file_identity_digest == group[0].file_identity_digest
                        && m.latest_image_touch == group[0].latest_image_touch
                        && m.latest_history == group[0].latest_history
                        && m.latest_snapshot == group[0].latest_snapshot
                });
                if suggested.is_none() && content_equal {
                    suggested = group
                        .iter()
                        .max_by_key(|m| {
                            m.schema_version
                                .as_ref()
                                .and_then(|v| v.parse::<u64>().ok())
                        })
                        .map(|m| m.revision_id.clone());
                    issues.push("Equivalent identity/change summaries: schema upgrade suggests a member but does not prove equal metadata or newer edits".into());
                }
                if suggested.is_none() {
                    issues.push("Divergent, tied or insufficient internal recency evidence; explicit review required".into());
                }
                let latest_mtime = group
                    .iter()
                    .max_by_key(|m| m.modified_ns)
                    .map(|m| &m.revision_id);
                if suggested.as_ref().is_some_and(|s| Some(s) != latest_mtime) {
                    issues.push("Filesystem modified date contradicts the internal/schema suggestion; copied mtime is not authoritative".into());
                }
                if group.iter().any(|m| m.provider != group[0].provider) {
                    issues.push("Filename family contains different provider identities; confirm or explicitly split family assignments".into());
                }
            }
            let evidence = json_digest(&(
                &group,
                &issues,
                &uninspected.keys().collect::<Vec<_>>(),
                &inventories,
            ))?;
            let choice: Option<(String, String)> = self
                .db
                .query_row(
                    "SELECT revision,evidence_digest FROM family_choices WHERE family=?",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let selected = choice.and_then(|(revision, hash)| {
                if hash == evidence && group.iter().any(|m| m.revision_id == revision) {
                    Some(revision)
                } else {
                    issues
                        .push("Stored choice is stale after evidence changed; review again".into());
                    None
                }
            });
            let excluded = if selected.is_some() {
                group
                    .iter()
                    .filter(|m| Some(&m.revision_id) != selected.as_ref())
                    .map(|m| m.revision_id.clone())
                    .collect()
            } else {
                vec![]
            };
            families.push(Family {
                id,
                members: group,
                suggested,
                selected,
                excluded,
                issues,
                evidence_digest: evidence,
            });
        }
        let selected: Vec<_> = families
            .iter()
            .filter_map(|f| f.selected.as_ref())
            .collect();
        let mut conflicts = vec![];
        let mut conflict_count = 0;
        let mut possible_path_collision_count = 0;
        let mut possible_path_collisions = vec![];
        let mut path_sample_bytes = 3;
        let mut path_sample_full = false;
        let mut conflict_sample_bytes = 3;
        let mut conflict_sample_full = false;
        for (index, a) in selected.iter().enumerate() {
            for b in &selected[..index] {
                let count:i64=self.db.query_row("SELECT count(*) FROM entities a JOIN entities b ON b.table_name=a.table_name AND b.global_key=a.global_key WHERE a.revision=? AND b.revision=? AND a.global_key IS NOT NULL AND a.table_name IN ('Adobe_images','AgLibraryFile')",params![a,b],|r|r.get(0))?;
                conflict_count += count;
                possible_path_collision_count += self.db.query_row("SELECT count(*) FROM paths a JOIN paths b ON a.inspection_path=b.inspection_path WHERE a.revision=? AND b.revision=? AND a.inspection_path IS NOT NULL",params![a,b],|r|r.get::<_,i64>(0))?;
                if !path_sample_full && possible_path_collisions.len() < 1000 {
                    for value in
                        self.path_collisions(a, b, 0, 0, 1000 - possible_path_collisions.len())?
                    {
                        if !append_value(
                            &mut possible_path_collisions,
                            &mut path_sample_bytes,
                            value,
                        )? {
                            path_sample_full = true;
                            break;
                        }
                    }
                }

                let mut statement=self.db.prepare("SELECT a.source_id,b.source_id,a.table_name,a.global_key FROM entities a JOIN entities b ON b.table_name=a.table_name AND b.global_key=a.global_key WHERE a.revision=? AND b.revision=? AND a.global_key IS NOT NULL AND a.table_name IN ('Adobe_images','AgLibraryFile') LIMIT ?")?;
                if !conflict_sample_full {
                    for row in statement.query_map(params![a,b,1000usize.saturating_sub(conflicts.len()) as i64],|r|Ok(serde_json::json!({"a":r.get::<_,String>(0)?,"b":r.get::<_,String>(1)?,"table":r.get::<_,String>(2)?,"global_key":r.get::<_,String>(3)?})))? {
                        if !append_value(&mut conflicts,&mut conflict_sample_bytes,row?)? {conflict_sample_full=true;break;}
                    }
                }
            }
        }
        Ok(FamilyReport {
            families,
            uninspected_candidates: uninspected.into_values().collect(),
            inventory_complete,
            cross_catalog_conflicts: conflicts,
            conflict_count,
            possible_path_collision_count,
            possible_path_collisions,
        })
    }
    /// Exact locator equality is only a possible collision, never file/content identity.
    /// Continue with the last (left_sequence,right_sequence), including short pages.
    pub fn path_collisions(
        &self,
        left: &str,
        right: &str,
        after_left: i64,
        after_right: i64,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        ensure!(
            left != right && after_left >= 0 && after_right >= 0 && (1..=1000).contains(&limit),
            "invalid path collision page"
        );
        let mut statement=self.db.prepare("SELECT a.sequence,b.sequence,a.source_id,b.source_id,a.inspection_path FROM paths a JOIN paths b ON a.inspection_path=b.inspection_path WHERE a.revision=?1 AND b.revision=?2 AND a.inspection_path IS NOT NULL AND (a.sequence,b.sequence)>(?3,?4) ORDER BY a.sequence,b.sequence LIMIT ?5")?;
        bounded_values(statement.query_map(params![left,right,after_left,after_right,limit as i64],|r|Ok(serde_json::json!({"left_revision":left,"right_revision":right,"left_sequence":r.get::<_,i64>(0)?,"right_sequence":r.get::<_,i64>(1)?,"left_source_id":r.get::<_,String>(2)?,"right_source_id":r.get::<_,String>(3)?,"locator":r.get::<_,String>(4)?,"classification":"possible_path_collision_not_identity"})))?)
    }
    pub fn choose(
        &mut self,
        family: &str,
        revision: &str,
        expected_evidence: &str,
        reason: &str,
    ) -> Result<()> {
        ensure!(
            !reason.trim().is_empty() && reason.len() <= 4096,
            "explicit selection reason required"
        );
        let report = self.families()?;
        let value = report
            .families
            .iter()
            .find(|f| f.id == family)
            .context("family absent")?;
        ensure!(
            value.evidence_digest == expected_evidence,
            "family evidence changed; inspect again"
        );
        ensure!(
            value.members.iter().any(|m| m.revision_id == revision),
            "selected revision is outside family"
        );
        self.db.execute("INSERT INTO family_choices VALUES(?,?,?,?) ON CONFLICT(family) DO UPDATE SET revision=excluded.revision,evidence_digest=excluded.evidence_digest,reason=excluded.reason",params![family,revision,expected_evidence,reason])?;
        Ok(())
    }
}
fn identity_summary(db: &Connection, revision: &str, table: &str) -> Result<(i64, String)> {
    let count: i64 = db.query_row(
        "SELECT count(*) FROM entities WHERE revision=? AND table_name=?",
        params![revision, table],
        |r| r.get(0),
    )?;
    let mut hash = blake3::Hasher::new();
    let mut statement = db.prepare(
        "SELECT global_key FROM entities WHERE revision=? AND table_name=? ORDER BY global_key",
    )?;
    for row in statement.query_map(params![revision, table], |r| r.get::<_, Option<String>>(0))? {
        let value = row?;
        let bytes = serde_json::to_vec(&value)?;
        hash.update(&(bytes.len() as u64).to_be_bytes());
        hash.update(&bytes);
    }
    Ok((count, hash.finalize().to_hex().to_string()))
}
fn max_field(db: &Connection, revision: &str, table: &str, field: &str) -> Result<Option<f64>> {
    let mut max = None;
    let mut statement =
        db.prepare("SELECT fields_json FROM entities WHERE revision=? AND table_name=?")?;
    for row in statement.query_map(params![revision, table], |r| r.get::<_, String>(0))? {
        let fields: Fields = serde_json::from_str(&row?)?;
        if let Some(value) = fields
            .get(field)
            .and_then(Cell::numeric)
            .filter(|v| v.is_finite())
        {
            max = Some(max.map_or(value, |m: f64| m.max(value)));
        }
    }
    Ok(max)
}
fn overlap(db: &Connection, a: &str, b: &str) -> Result<i64> {
    Ok(db.query_row("SELECT count(*) FROM (SELECT global_key FROM entities WHERE revision=? AND table_name='Adobe_images' AND global_key IS NOT NULL INTERSECT SELECT global_key FROM entities WHERE revision=? AND table_name='Adobe_images' AND global_key IS NOT NULL)",params![a,b],|r|r.get(0))?)
}
fn retain_facts(
    db: &Connection,
    revision: &str,
    id: &str,
    file_id: Option<&str>,
    origin: &str,
    packet: &str,
    bytes: &[u8],
) -> Result<()> {
    match crate::xmp::project(bytes) {
        Ok(projection) => {
            for issue in projection.issues {
                add_issue(db, revision, Some(id), "xmp_projection_gap", &issue)?;
            }
            for (field, value) in projection.fields {
                db.execute(
                    "INSERT OR IGNORE INTO metadata_facts VALUES(?,?,?,?,?,?,?)",
                    params![
                        revision,
                        id,
                        file_id,
                        origin,
                        packet,
                        field,
                        serde_json::to_string(&value)?
                    ],
                )?;
            }
        }
        Err(e) => add_issue(
            db,
            revision,
            Some(id),
            "xmp_semantics_unverified",
            &format!("{origin}: {e:#}"),
        )?,
    }
    Ok(())
}
// Keep the exact owner reference outermost: without this join-order constraint,
// SQLite can scan every image/file to prove the LIMIT 2 uniqueness condition.
// https://www.sqlite.org/optoverview.html#manual_control_of_query_plans_using_cross_join
const UNIQUE_TARGET_SQL: &str = "SELECT target.source_id FROM references_out reference CROSS JOIN entities target ON target.revision=reference.revision AND target.table_name=reference.target_table AND target.local_key=reference.target_key WHERE reference.revision=? AND reference.source_id=? AND reference.field=? AND reference.target_table=? LIMIT 2";

fn unique_target(
    db: &Connection,
    revision: &str,
    source_id: &str,
    field: &str,
    target_table: &str,
) -> Result<Option<String>> {
    let mut statement = db.prepare(UNIQUE_TARGET_SQL)?;
    let ids = statement
        .query_map(params![revision, source_id, field, target_table], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if ids.len() == 1 {
        Ok(ids.into_iter().next())
    } else {
        Ok(None)
    }
}
fn associate_catalog_facts(db: &Connection, revision: &str) -> Result<()> {
    let mut statement=db.prepare("SELECT DISTINCT source_id FROM metadata_facts WHERE revision=? AND origin='catalog' ORDER BY source_id")?;
    let mut rows = statement.query([revision])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let image = unique_target(db, revision, &id, "image", "Adobe_images")?;
        let file = match image {
            Some(image) => unique_target(db, revision, &image, "rootFile", "AgLibraryFile")?,
            None => None,
        };
        if file.is_none() {
            add_issue(
                db,
                revision,
                Some(&id),
                "ambiguous_metadata_owner",
                "Catalog XMP has no unique image-to-file reference chain; projected facts remain unassociated, with original packets and source IDs retained",
            )?;
        }
        db.execute("UPDATE metadata_facts SET file_source_id=? WHERE revision=? AND source_id=? AND origin='catalog'",params![file,revision,id])?;
    }
    Ok(())
}
fn inspect_packets(
    db: &Connection,
    revision: &str,
    id: &str,
    path: &Path,
    limits: &Limits,
) -> Result<(Vec<serde_json::Value>, bool)> {
    let packet_limits = xmp_packets::Limits {
        max_source_bytes: limits.max_file_bytes,
        max_retained_bytes: limits.max_cell_bytes,
        max_parse_bytes: limits.max_cell_bytes,
        ..Default::default()
    };
    let mut candidates = vec![("embedded".to_owned(), path.to_owned())];
    for suffix in ["xmp", "XMP"] {
        candidates.push((format!("sidecar_{suffix}"), path.with_extension(suffix)));
        let mut appended = path.as_os_str().to_owned();
        appended.push(format!(".{suffix}"));
        candidates.push((
            format!("sidecar_appended_{suffix}"),
            PathBuf::from(appended),
        ));
    }
    let mut observations = vec![];
    let mut gaps = false;
    for (origin, file) in candidates {
        match fs::symlink_metadata(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                observations.push(serde_json::json!({"origin":origin,"path":NativePath::from_path(&file),"state":"absent"}));
                continue;
            }
            Err(e) => {
                gaps = true;
                observations.push(serde_json::json!({"origin":origin,"error":e.to_string()}));
                continue;
            }
            _ => {}
        }
        let inspection = if origin == "embedded" {
            xmp_packets::inspect(&file, &packet_limits)
        } else {
            xmp_packets::inspect_sidecar(&file, &packet_limits)
        };
        match inspection {
            Err(e) => {
                gaps = true;
                observations.push(serde_json::json!({"origin":origin,"error":e.to_string()}));
            }
            Ok(value) => {
                if !matches!(
                    value.status,
                    xmp_packets::Status::Complete | xmp_packets::Status::Absent
                ) {
                    gaps = true;
                }
                for (index, packet) in value.packets.iter().enumerate() {
                    let detail = serde_json::json!({"container":packet.container,"ranges":packet.ranges,"group":packet.group,"attributes":packet.attributes,"source_revision":value.revision,"inspection_status":value.status,"source_path":NativePath::from_path(&file)});
                    db.execute("INSERT OR IGNORE INTO packets(revision,source_id,origin,raw_digest,raw,detail) VALUES(?,?,?,?,?,?)",params![revision,id,format!("{origin}:packet:{index}"),packet.blake3,packet.bytes,detail.to_string()])?;
                }
                for (index, input) in value.parse_inputs.iter().enumerate() {
                    let detail = serde_json::json!({"transformation":input.transformation,"packet_indices":input.packet_indices,"group":input.group,"input_blake3":input.blake3,"source_revision":value.revision,"inspection_status":value.status});
                    db.execute("INSERT OR IGNORE INTO packets(revision,source_id,origin,raw_digest,raw,decoded,detail) VALUES(?,?,?,?,?,?,?)",params![revision,id,format!("{origin}:parse_input:{index}"),input.blake3,Vec::<u8>::new(),input.bytes,detail.to_string()])?;
                    if value.status == xmp_packets::Status::Complete {
                        retain_facts(
                            db,
                            revision,
                            id,
                            Some(id),
                            &origin,
                            &input.blake3,
                            &input.bytes,
                        )?;
                    }
                }
                observations.push(serde_json::json!({"origin":origin,"status":value.status,"revision":value.revision,"packets":value.packets.len(),"parse_inputs":value.parse_inputs.len(),"issues":value.issues}));
            }
        }
    }
    Ok((observations, gaps))
}
impl Plan {
    pub fn issues(
        &self,
        revision: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        ensure!(
            after >= 0 && (1..=1000).contains(&limit),
            "invalid issue page"
        );
        let mut statement = self.db.prepare(ISSUES_PAGE)?;
        bounded_values(statement.query_map(params![revision,after,limit as i64],|r|Ok(serde_json::json!({"sequence":r.get::<_,i64>(0)?,"source_id":r.get::<_,Option<String>>(1)?,"code":r.get::<_,String>(2)?,"detail":r.get::<_,String>(3)?})))?)
    }
    pub fn packets(
        &self,
        revision: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        ensure!(
            after >= 0 && (1..=1000).contains(&limit),
            "invalid packet page"
        );
        let mut statement = self.db.prepare(PACKETS_PAGE)?;
        bounded_values(statement.query_map(params![revision,after,limit as i64],|r|Ok(serde_json::json!({"sequence":r.get::<_,i64>(0)?,"source_id":r.get::<_,String>(1)?,"origin":r.get::<_,String>(2)?,"digest":r.get::<_,String>(3)?,"raw_bytes":r.get::<_,i64>(4)?,"decoded_bytes":r.get::<_,Option<i64>>(5)?,"detail":r.get::<_,String>(6)?})))?)
    }
    /// Exact bounded byte chunks. `decoded` selects explicitly transformed parse
    /// input; the original raw bytes are always independently accessible.
    pub fn packet_bytes(
        &self,
        revision: &str,
        sequence: i64,
        decoded: bool,
        offset: i64,
        limit: usize,
    ) -> Result<serde_json::Value> {
        ensure!(
            offset >= 0 && (1..=1024 * 1024).contains(&limit),
            "invalid packet byte window"
        );
        let column = if decoded { "decoded" } else { "raw" };
        let query = format!(
            "SELECT length({column}),substr({column},?3,?4),raw_digest,origin FROM packets WHERE revision=?1 AND sequence=?2"
        );
        let (total, bytes, digest, origin): (Option<i64>, Option<Vec<u8>>, String, String) =
            self.db.query_row(
                &query,
                params![
                    revision,
                    sequence,
                    offset.checked_add(1).context("packet offset overflow")?,
                    limit as i64
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        ensure!(
            total.is_some(),
            "requested transformed packet does not exist"
        );
        Ok(
            serde_json::json!({"sequence":sequence,"origin":origin,"decoded":decoded,"offset":offset,"total_bytes":total,"record_digest":digest,"bytes":Cell::Blob(bytes.unwrap_or_default())}),
        )
    }
    pub fn metadata_conflicts(
        &self,
        revision: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        ensure!(
            after >= 0 && (1..=1000).contains(&limit),
            "invalid conflict page"
        );
        // Each candidate is independently identified. Different virtual-copy
        // catalog facts are observable; this dry run never resolves them.
        let mut statement = self.db.prepare(CONFLICTS_PAGE)?;
        bounded_values(statement.query_map(params![revision,after,limit as i64],|r|Ok(serde_json::json!({"sequence":r.get::<_,i64>(0)?,"source_id":r.get::<_,String>(1)?,"file_source_id":r.get::<_,String>(2)?,"origin":r.get::<_,String>(3)?,"packet_digest":r.get::<_,String>(4)?,"field":r.get::<_,String>(5)?,"value":r.get::<_,String>(6)?})))?)
    }
}

// A short page is not EOF: callers resume with its last returned cursor.
fn append_value(
    out: &mut Vec<serde_json::Value>,
    bytes: &mut usize,
    value: serde_json::Value,
) -> Result<bool> {
    let size = super::bounded_json(&value, super::PAGE_BYTES)?.len() + usize::from(!out.is_empty());
    if *bytes + size > super::PAGE_BYTES {
        ensure!(
            !out.is_empty(),
            "single descriptor exceeds derived page byte budget; original evidence remains retained"
        );
        return Ok(false);
    }
    *bytes += size;
    out.push(value);
    Ok(true)
}
fn bounded_values(
    values: impl IntoIterator<Item = rusqlite::Result<serde_json::Value>>,
) -> Result<Vec<serde_json::Value>> {
    let mut out = vec![];
    let mut bytes = 3; // Array brackets plus the CLI framing newline.
    for value in values {
        if !append_value(&mut out, &mut bytes, value?)? {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod bounded_plan_tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn windows_inspection_locator_normalizes_separators_without_retyping_foreign_paths() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE entities(revision,table_name,local_key,fields_json);")
            .unwrap();
        let key = relationship_key(&Cell::Integer(1)).unwrap();
        let folder = Fields::from([
            ("rootFolder".into(), Cell::Integer(1)),
            ("pathFromRoot".into(), Cell::Text(b"2022/sub/".to_vec())),
        ]);
        db.execute(
            "INSERT INTO entities VALUES('r','AgLibraryFolder',?,?)",
            params![key, serde_json::to_string(&folder).unwrap()],
        )
        .unwrap();
        let file = Fields::from([
            ("folder".into(), Cell::Integer(1)),
            ("idx_filename".into(), Cell::Text(b"source.CR2".to_vec())),
        ]);
        for root in [
            r"C:\photos",
            r"\\?\C:\photos",
            r"\\?\UNC\server\share\photos",
            r"\\server\share\photos",
            "/foreign/photos",
        ] {
            db.execute(
                "DELETE FROM entities WHERE table_name='AgLibraryRootFolder'",
                [],
            )
            .unwrap();
            let fields =
                Fields::from([("absolutePath".into(), Cell::Text(root.as_bytes().to_vec()))]);
            let retained = serde_json::to_string(&fields).unwrap();
            db.execute(
                "INSERT INTO entities VALUES('r','AgLibraryRootFolder',?,?)",
                params![key, retained],
            )
            .unwrap();
            let (reported, inspected) = original_path(&db, "r", &file).unwrap();
            if root.starts_with('/') {
                assert_eq!(reported, "/foreign/photos/2022/sub/source.CR2");
                assert_eq!(
                    inspected,
                    NativePath::UnixBytes(reported.as_bytes().to_vec())
                );
                assert!(inspected.to_path().is_err());
            } else {
                assert_eq!(reported, format!("{root}\\2022/sub/source.CR2"));
                let expected = format!("{root}\\2022\\sub\\source.CR2");
                assert_eq!(
                    inspected,
                    NativePath::WindowsWide(expected.encode_utf16().collect())
                );
                assert_eq!(inspected.to_path().unwrap(), PathBuf::from(expected));
            }
            assert_eq!(
                entity_fields(&db, "r", "AgLibraryRootFolder", &Cell::Integer(1)).unwrap(),
                fields
            );
        }
    }
    #[cfg(windows)]
    #[test]
    fn immutable_uri_preserves_extended_native_path_and_read_only_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let mut directory = fs::canonicalize(temp.path()).unwrap();
        // Exercise the actual extended-length namespace, not just a mock prefix.
        for _ in 0..6 {
            directory = directory.join("a_directory_long_enough_to_require_native_extended_paths");
        }
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("catalog #100% café.sqlite3");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE evidence(v); INSERT INTO evidence VALUES(17913);")
            .unwrap();
        drop(db);
        let before = fs::read(&path).unwrap();
        let encoded = uri(&path).unwrap();
        assert!(encoded.starts_with("file:%5C%5C%3F%5C"), "{encoded}");
        assert!(encoded.contains("%23100%25%20caf%C3%A9.sqlite3"));
        let db = Connection::open_with_flags(
            &encoded,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        assert_eq!(
            db.query_row("SELECT v FROM evidence", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            17913
        );
        assert!(db.execute("INSERT INTO evidence VALUES(2)", []).is_err());
        drop(db);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    }
    #[cfg(windows)]
    #[test]
    fn immutable_uri_rejects_unpaired_windows_surrogate_without_lossy_alias() {
        use std::os::windows::ffi::OsStringExt;
        let temp = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(temp.path())
            .unwrap()
            .join(std::ffi::OsString::from_wide(&[b'x' as u16, 0xd800]));
        fs::write(&path, b"retained").unwrap();
        assert!(
            uri(&path)
                .unwrap_err()
                .to_string()
                .contains("unpaired Windows surrogate")
        );
        assert_eq!(fs::read(&path).unwrap(), b"retained");
    }
    #[test]
    fn schema_admission_bounds_objects_text_and_native_names() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE a(k); CREATE TABLE b(k); CREATE TABLE c(k);")
            .unwrap();
        assert!(
            read_schema_bounded(&db, 3, 4096)
                .unwrap_err()
                .to_string()
                .contains("object admission")
        );
        assert_eq!(read_schema_bounded(&db, 10, 4096).unwrap().len(), 3);
        assert!(
            read_schema_bounded(&db, 10, 30)
                .unwrap_err()
                .to_string()
                .contains("byte admission")
        );
        db.execute_batch("CREATE TABLE a_very_long_native_table_name_for_testing(k)")
            .unwrap();
        assert!(
            read_schema_bounded(&db, 10, 30)
                .unwrap_err()
                .to_string()
                .contains("name byte admission")
        );
    }
    #[test]
    fn descriptor_pages_account_for_json_escaping_and_allow_bounded_resume() {
        let value =
            serde_json::json!({"sequence":1,"detail":"\"".repeat(super::super::PAGE_BYTES/4)});
        let page = bounded_values([Ok(value.clone()), Ok(value.clone())]).unwrap();
        assert_eq!(page.len(), 1);
        assert!(serde_json::to_vec(&page).unwrap().len() <= super::super::PAGE_BYTES);
        let next = bounded_values([Ok(value)]).unwrap();
        assert_eq!(next.len(), 1);
        assert!(
            bounded_values([Ok(serde_json::json!("x".repeat(super::super::PAGE_BYTES)))]).is_err()
        );
        assert!(bounded_values([Err(rusqlite::Error::InvalidQuery)]).is_err());
    }
    #[test]
    fn row_pages_include_repeated_column_names_and_descriptors_use_the_same_budget() {
        let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let plan = Plan::create(&temp.path().join("plan")).unwrap();
        let long = "q".repeat(super::super::PAGE_BYTES / 2);
        let columns = serde_json::to_string(&vec![long.clone()]).unwrap();
        plan.db.execute("INSERT INTO tables(revision,name,columns_json,key_json,schema_json,category,state) VALUES('r','t',?,'[]','{}','unsupported_retained_only','complete')",[columns]).unwrap();
        for id in 1..=2 {
            let key = serde_json::to_string(&vec![Cell::Integer(id)]).unwrap();
            plan.db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES('r',?,'t',?,'[]')",params![format!("id-{id}"),key]).unwrap();
            plan.db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,detail) VALUES('r',?,'test',?,x'00',?)",params![format!("id-{id}"),format!("digest-{id}"),long]).unwrap();
        }
        let first = plan.rows("r", None, 0, 100).unwrap();
        assert_eq!(first.len(), 1);
        assert!(serde_json::to_vec(&first).unwrap().len() <= super::super::PAGE_BYTES);
        let second = plan.rows("r", None, first[0].sequence, 100).unwrap();
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].source_id, second[0].source_id);
        let first = plan.packets("r", 0, 100).unwrap();
        assert_eq!(first.len(), 1);
        assert!(serde_json::to_vec(&first).unwrap().len() <= super::super::PAGE_BYTES);
        assert_eq!(
            plan.packets("r", first[0]["sequence"].as_i64().unwrap(), 100)
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn unique_owner_lookup_has_bounded_native_work_and_preserves_ambiguity() {
        use rusqlite::StatementStatus;
        // The prior statement is executed only on this disposable fixture to prove
        // the observed growth; the production statement is shared above.
        const ORIGINAL: &str = "SELECT target.source_id FROM references_out reference JOIN entities target ON target.revision=reference.revision AND target.table_name=reference.target_table AND target.local_key=reference.target_key WHERE reference.revision=? AND reference.source_id=? AND reference.field=? AND reference.target_table=? LIMIT 2";
        fn measured(db: &Connection, sql: &str, owner: &str) -> (Vec<String>, i32, Vec<String>) {
            let parameters = ["r", owner, "image", "Adobe_images"];
            let plan = db
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(parameters, |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let mut statement = db.prepare(sql).unwrap();
            assert!(statement.readonly());
            let values = statement
                .query_map(parameters, |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let steps = statement.get_status(StatementStatus::VmStep);
            assert!(steps > 0);
            assert_eq!(statement.get_status(StatementStatus::Sort), 0);
            (values, steps, plan)
        }
        let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
        let key = serde_json::to_string(&Cell::Integer(1)).unwrap();
        plan.db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES('r','owner','image','Adobe_images',?)",[&key]).unwrap();
        let mut receipts = vec![];
        let mut previous = 0;
        for count in [100, 1000, 10000] {
            let transaction = plan.db.transaction().unwrap();
            {
                let mut insert = transaction
                    .prepare("INSERT INTO entities VALUES('r',?,'Adobe_images',?,?,'{}')")
                    .unwrap();
                for index in previous + 1..=count {
                    insert
                        .execute(params![
                            format!("target-{index}"),
                            serde_json::to_string(&Cell::Integer(index)).unwrap(),
                            format!("global-{index}")
                        ])
                        .unwrap();
                }
            }
            transaction.commit().unwrap();
            previous = count;
            let original = measured(&plan.db, ORIGINAL, "owner");
            let corrected = measured(&plan.db, UNIQUE_TARGET_SQL, "owner");
            assert_eq!(original.0, vec!["target-1".to_string()]);
            assert_eq!(corrected.0, original.0);
            assert_eq!(
                unique_target(&plan.db, "r", "owner", "image", "Adobe_images").unwrap(),
                Some("target-1".into())
            );
            assert_eq!(
                unique_target(&plan.db, "r", "missing-owner", "image", "Adobe_images").unwrap(),
                None
            );
            assert!(corrected.2[0].contains("reference"));
            assert!(
                corrected
                    .2
                    .iter()
                    .any(|line| line.contains("entity_local") && line.contains("local_key=?"))
            );
            receipts.push(serde_json::json!({"targets":count,"original_vm_steps":original.1,"corrected_vm_steps":corrected.1,"original_plan":original.2,"corrected_plan":corrected.2}));
        }
        let first = &receipts[0];
        let last = receipts.last().unwrap();
        assert!(
            last["original_vm_steps"].as_i64().unwrap()
                > first["original_vm_steps"].as_i64().unwrap() * 20,
            "fixture must reproduce the prior growth on this bundled engine"
        );
        assert!(
            last["corrected_vm_steps"].as_i64().unwrap()
                <= first["corrected_vm_steps"].as_i64().unwrap() * 2,
            "unrelated target growth must not increase lookup work proportionately"
        );
        plan.db.execute("INSERT INTO entities VALUES('r','duplicate-target','Adobe_images',?,'different-global','{}')",[&key]).unwrap();
        assert_eq!(measured(&plan.db, UNIQUE_TARGET_SQL, "owner").0.len(), 2);
        assert_eq!(
            unique_target(&plan.db, "r", "owner", "image", "Adobe_images").unwrap(),
            None
        );
        plan.db
            .execute(
                "DELETE FROM entities WHERE source_id='duplicate-target'",
                [],
            )
            .unwrap();
        plan.db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES('r','owner','image','Adobe_images',?)",[serde_json::to_string(&Cell::Integer(2)).unwrap()]).unwrap();
        assert_eq!(
            unique_target(&plan.db, "r", "owner", "image", "Adobe_images").unwrap(),
            None
        );
        eprintln!(
            "{}",
            serde_json::json!({"sqlite_version":rusqlite::version(),"scope":"native statement-work regression, not latency qualification","cases":receipts})
        );
    }
}

#[cfg(test)]
#[path = "plan_paging_tests.rs"]
mod paging_tests;

#[cfg(test)]
#[path = "plan_relationship_tests.rs"]
mod relationship_tests;
