use super::*;
use crate::lightroom::{bounded_json, capture::Manifest, digest, plan, source::Source};
use anyhow::bail;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use std::{
    cell::Cell as Flag,
    collections::BTreeSet,
    ffi::CString,
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::{Duration, Instant},
};

struct Spec {
    table: &'static str,
    keys: &'static [&'static str],
    fields: &'static [&'static str],
    numeric_key: bool,
}
impl Collection {
    fn spec(self) -> Spec {
        match self {
            Self::Captures => Spec {
                table: "captures",
                keys: &["revision"],
                fields: &[
                    "lineage",
                    "path",
                    "manifest",
                    "stage",
                    "schema_version",
                    "provider",
                    "evidence_revision",
                ],
                numeric_key: false,
            },
            Self::Rows => Spec {
                table: "rows",
                keys: &["sequence"],
                fields: &["source_id", "table_name", "key_json", "cells_json"],
                numeric_key: true,
            },
            Self::Entities => Spec {
                table: "entities",
                keys: &["source_id"],
                fields: &[
                    "source_id",
                    "table_name",
                    "local_key",
                    "global_key",
                    "fields_json",
                ],
                numeric_key: false,
            },
            Self::References => Spec {
                table: "references_out",
                keys: &["source_id", "field", "target_table", "target_key"],
                fields: &["source_id", "field", "target_table", "target_key"],
                numeric_key: false,
            },
            Self::Paths => Spec {
                table: "paths",
                keys: &["sequence"],
                fields: &[
                    "source_id",
                    "original",
                    "inspection_path",
                    "state",
                    "evidence",
                ],
                numeric_key: true,
            },
            Self::Packets => Spec {
                table: "packets",
                keys: &["sequence"],
                fields: &[
                    "source_id",
                    "origin",
                    "raw_digest",
                    "raw",
                    "decoded",
                    "detail",
                ],
                numeric_key: true,
            },
            Self::MetadataFacts => Spec {
                table: "metadata_facts",
                keys: &["source_id", "origin", "packet_digest", "field"],
                fields: &[
                    "source_id",
                    "file_source_id",
                    "origin",
                    "packet_digest",
                    "field",
                    "value_json",
                ],
                numeric_key: false,
            },
            Self::Issues => Spec {
                table: "issues",
                keys: &["sequence"],
                fields: &["source_id", "code", "detail"],
                numeric_key: true,
            },
            Self::Tables => Spec {
                table: "tables",
                keys: &["name"],
                fields: &[
                    "name",
                    "columns_json",
                    "key_json",
                    "schema_json",
                    "category",
                    "expected",
                    "retained",
                    "cursor",
                    "state",
                    "issue",
                ],
                numeric_key: false,
            },
            Self::SchemaObjects => Spec {
                table: "schema_objects",
                keys: &["kind", "name"],
                fields: &["kind", "name", "table_name", "sql_text"],
                numeric_key: false,
            },
        }
    }
}

struct Budget {
    until: Instant,
    remaining: u64,
}
unsafe extern "C" fn progress(context: *mut std::ffi::c_void) -> i32 {
    let budget = unsafe { &mut *context.cast::<Budget>() };
    if budget.remaining == 0
        || (budget.remaining.is_multiple_of(1000) && Instant::now() >= budget.until)
    {
        return 1;
    }
    budget.remaining -= 1;
    0
}
struct QueryBudget<'a> {
    db: &'a Connection,
    state: Box<Budget>,
}
impl<'a> QueryBudget<'a> {
    fn new(db: &'a Connection, limits: ReadLimits) -> Self {
        let mut state = Box::new(Budget {
            until: Instant::now() + Duration::from_millis(limits.deadline_ms),
            remaining: limits.vm_steps,
        });
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                db.handle(),
                1,
                Some(progress),
                (&mut *state as *mut Budget).cast(),
            );
        }
        Self { db, state }
    }
    fn check(&self) -> Result<()> {
        ensure!(
            Instant::now() < self.state.until,
            "inspection-source read deadline exceeded"
        );
        Ok(())
    }
}
impl Drop for QueryBudget<'_> {
    fn drop(&mut self) {
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                self.db.handle(),
                0,
                None,
                std::ptr::null_mut(),
            );
        }
    }
}

/// Read-only incremental access avoids SQLite materializing a large TEXT/BLOB
/// merely to return a small prefix. Every schema3 evidence table has a rowid.
struct Blob(*mut rusqlite::ffi::sqlite3_blob);
impl Blob {
    fn open(db: &Connection, table: &str, column: &str, rowid: i64) -> Result<Self> {
        let table = CString::new(table)?;
        let column = CString::new(column)?;
        let mut handle = std::ptr::null_mut();
        let code = unsafe {
            rusqlite::ffi::sqlite3_blob_open(
                db.handle(),
                c"main".as_ptr(),
                table.as_ptr(),
                column.as_ptr(),
                rowid,
                0,
                &mut handle,
            )
        };
        ensure!(
            code == rusqlite::ffi::SQLITE_OK,
            "read-only evidence blob open failed ({code})"
        );
        Ok(Self(handle))
    }
    fn len(&self) -> usize {
        unsafe { rusqlite::ffi::sqlite3_blob_bytes(self.0) as usize }
    }
    fn read(&self, offset: usize, limit: usize) -> Result<Vec<u8>> {
        ensure!(offset <= self.len(), "evidence byte offset exceeds length");
        let mut bytes = vec![0; limit.min(self.len() - offset)];
        if !bytes.is_empty() {
            let code = unsafe {
                rusqlite::ffi::sqlite3_blob_read(
                    self.0,
                    bytes.as_mut_ptr().cast(),
                    bytes.len().try_into()?,
                    offset.try_into()?,
                )
            };
            ensure!(
                code == rusqlite::ffi::SQLITE_OK,
                "evidence byte read failed ({code})"
            );
        }
        Ok(bytes)
    }
}
impl Drop for Blob {
    fn drop(&mut self) {
        unsafe {
            rusqlite::ffi::sqlite3_blob_close(self.0);
        }
    }
}

/// Hold in one isolated migration worker: POSIX SQLite byte locks are process
/// scoped. The coordinator must not open/close the same inode in another reader
/// in this process, or permit an external non-cooperative writer.
pub struct MigrationSource {
    // Close SQLite before releasing our explicit source locks.
    db: Connection,
    guard: Source,
    #[cfg(windows)]
    _write_lease: fs::File,
    seal: InputSeal,
    namespace: String,
    limits: ReadLimits,
    poisoned: Flag<bool>,
}

fn digest_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn no_companions(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut companion = path.as_os_str().to_os_string();
        companion.push(suffix);
        match fs::symlink_metadata(&companion) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
            Ok(_) => bail!("sealed inspection source has a SQLite companion: {suffix}"),
        }
    }
    Ok(())
}

impl InputSeal {
    pub fn roster_blake3(&self) -> Result<String> {
        Ok(digest(&bounded_json(
            &(&self.selected, &self.excluded_revisions),
            crate::lightroom::MANIFEST_BYTES,
        )?))
    }
    pub fn binding_blake3(&self) -> Result<String> {
        Ok(digest(&bounded_json(
            self,
            crate::lightroom::MANIFEST_BYTES,
        )?))
    }
    fn validate(&self) -> Result<()> {
        ensure!(self.protocol == 1, "unsupported migration-source seal");
        ensure!(
            digest_valid(&self.blake3) && self.identity.bytes > 0,
            "invalid sealed database identity"
        );
        ensure!(
            self.approval.scope == "selected_migration"
                || self.approval.scope == "selected_migration_test",
            "missing explicit selected migration scope"
        );
        ensure!(
            digest_valid(&self.approval.document_blake3),
            "missing selection authorization digest"
        );
        ensure!(
            self.approval.roster_blake3 == self.roster_blake3()?,
            "selection authorization roster differs"
        );
        ensure!(
            !self.selected.is_empty()
                && self.selected.len() + self.excluded_revisions.len() <= 16_384,
            "capture roster limit"
        );
        let mut revisions = BTreeSet::new();
        let mut families = BTreeSet::new();
        for selected in &self.selected {
            ensure!(
                digest_valid(&selected.revision)
                    && digest_valid(&selected.manifest_blake3)
                    && digest_valid(&selected.family_evidence_digest),
                "invalid capture/evidence digest"
            );
            ensure!(
                !selected.family.trim().is_empty()
                    && selected.family.len() <= 256
                    && selected.evidence_revision >= 0,
                "invalid selected family/revision"
            );
            ensure!(
                revisions.insert(&selected.revision) && families.insert(&selected.family),
                "duplicate selected revision/family"
            );
        }
        for revision in &self.excluded_revisions {
            ensure!(
                digest_valid(revision) && revisions.insert(revision),
                "duplicate or overlapping excluded revision"
            );
        }
        ensure!(self.supplements.len() <= 16_384, "supplement roster limit");
        let mut supplements = BTreeSet::new();
        for supplement in &self.supplements {
            ensure!(
                digest_valid(&supplement.proof_blake3)
                    && digest_valid(&supplement.source_revision.blake3),
                "invalid supplemental evidence digest"
            );
            ensure!(
                supplement.origin == "embedded"
                    && !supplement.source_id.is_empty()
                    && supplement.source_id.len() <= 4096,
                "unsupported supplemental evidence scope"
            );
            ensure!(
                supplements.insert((
                    &supplement.revision,
                    &supplement.source_id,
                    &supplement.origin
                )),
                "duplicate supplemental evidence"
            );
        }
        Ok(())
    }
}

impl MigrationSource {
    pub fn open(seal: InputSeal, limits: ReadLimits) -> Result<Self> {
        limits.validate()?;
        seal.validate()?;
        let path = seal.database.to_path()?;
        ensure!(path.is_absolute(), "sealed database path must be absolute");
        no_companions(&path)?;
        #[cfg(windows)]
        let lease = {
            use std::os::windows::fs::OpenOptionsExt;
            crate::lightroom::source::reject_links(&path)?;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(&path)?
        };
        let mut guard = Source::open(&path, seal.identity.bytes)?;
        ensure!(
            guard.before == seal.identity,
            "sealed inspection file identity differs"
        );
        guard.lock(0x4000_0000, 512)?;
        let deadline = Instant::now() + Duration::from_millis(limits.open_deadline_ms);
        guard.file.seek(SeekFrom::Start(0))?;
        let mut left = guard.before.bytes;
        let mut hash = blake3::Hasher::new();
        let mut buffer = [0; 128 * 1024];
        while left > 0 {
            ensure!(
                Instant::now() < deadline,
                "sealed source hashing deadline exceeded"
            );
            let size = left.min(buffer.len() as u64) as usize;
            guard.file.read_exact(&mut buffer[..size])?;
            hash.update(&buffer[..size]);
            left -= size as u64;
        }
        ensure!(
            hash.finalize().to_hex().as_str() == seal.blake3,
            "sealed inspection digest differs"
        );
        guard.verify()?;
        no_companions(&path)?;
        let db = Connection::open_with_flags(
            plan::uri(&path)?,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; PRAGMA mmap_size=0; PRAGMA cache_size=-8192; PRAGMA temp_store=MEMORY;")?;
        db.busy_timeout(Duration::ZERO)?;
        let namespace = seal.binding_blake3()?;
        let value = Self {
            db,
            guard,
            #[cfg(windows)]
            _write_lease: lease,
            seal,
            namespace,
            limits,
            poisoned: Flag::new(false),
        };
        value.operation(|| value.admit())?;
        Ok(value)
    }

    pub fn seal(&self) -> &InputSeal {
        &self.seal
    }
    pub fn binding_blake3(&self) -> &str {
        &self.namespace
    }
    pub fn max_chunk_bytes(&self) -> usize {
        self.limits.chunk_bytes
    }

    fn verify(&self) -> Result<()> {
        ensure!(
            !self.poisoned.get(),
            "inspection source was previously invalidated"
        );
        let result = self
            .guard
            .verify()
            .and_then(|()| no_companions(&self.guard.path));
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }
    fn operation<T>(&self, read: impl FnOnce() -> Result<T>) -> Result<T> {
        self.verify()?;
        let budget = QueryBudget::new(&self.db, self.limits);
        let result = read();
        self.verify()?;
        budget.check()?;
        result
    }
    fn selected(&self, revision: &str) -> Result<&SelectedCapture> {
        self.seal
            .selected
            .iter()
            .find(|entry| entry.revision == revision)
            .context("revision is not in the approved selected roster")
    }

    fn admit(&self) -> Result<()> {
        let app: i64 = self
            .db
            .query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let version: i64 = self.db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let encoding: String = self.db.query_row("PRAGMA encoding", [], |r| r.get(0))?;
        ensure!(
            encoding == "UTF-8",
            "schema3 evidence requires UTF-8 SQLite text storage"
        );
        ensure!(
            app == 0x50434c49 && version == plan::PLAN_SCHEMA_VERSION,
            "requires sealed inspection schema3; old derived keys cannot be migrated implicitly"
        );
        for table in [
            "captures",
            "rows",
            "entities",
            "references_out",
            "paths",
            "packets",
            "metadata_facts",
            "issues",
            "tables",
            "schema_objects",
            "family_choices",
        ] {
            let valid: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1 AND rootpage>0 AND upper(sql) NOT LIKE 'CREATE VIRTUAL%')", [table], |r| r.get(0))?;
            ensure!(valid, "missing ordinary inspection table: {table}");
        }
        plan::validate_paging_indexes(&self.db)?;
        let mut rows = self
            .db
            .prepare("SELECT revision FROM captures ORDER BY revision LIMIT 16385")?;
        let actual = rows
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        let expected = self
            .seal
            .selected
            .iter()
            .map(|r| r.revision.clone())
            .chain(self.seal.excluded_revisions.iter().cloned())
            .collect::<BTreeSet<_>>();
        ensure!(
            actual == expected,
            "selected/excluded capture partition differs from sealed plan"
        );
        for entry in &self.seal.selected {
            let (revision, evidence, reason): (String, String, String) = self.db.query_row(
                "SELECT revision,evidence_digest,reason FROM family_choices WHERE family=?",
                [&entry.family],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            ensure!(
                revision == entry.revision
                    && evidence == entry.family_evidence_digest
                    && !reason.trim().is_empty(),
                "family choice differs from approved selection"
            );
            let (stage, current): (String, i64) = self.db.query_row(
                "SELECT stage,evidence_revision FROM captures WHERE revision=?",
                [&entry.revision],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            ensure!(
                stage == "inspection_complete_with_reported_gaps"
                    && current == entry.evidence_revision,
                "selected inspection not complete or evidence changed"
            );
            let pending: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM paths WHERE revision=? AND (state IN ('pending','available_packets_uninspected') OR json_extract(evidence,'$.embedded_sidecar_xmp') IS NOT NULL))", [&entry.revision], |r| r.get(0))?;
            ensure!(!pending, "selected path/packet inspection remains pending");
            self.manifest_inner(&entry.revision)?;
        }
        for supplement in &self.seal.supplements {
            self.admit_supplement(supplement)?;
        }
        Ok(())
    }

    fn manifest_inner(&self, revision: &str) -> Result<Manifest> {
        let selected = self.selected(revision)?;
        let rowid: i64 = self.db.query_row(
            "SELECT rowid FROM captures WHERE revision=?",
            [revision],
            |r| r.get(0),
        )?;
        let blob = Blob::open(&self.db, "captures", "manifest", rowid)?;
        ensure!(
            blob.len() <= crate::lightroom::MANIFEST_BYTES,
            "capture manifest byte limit"
        );
        let bytes = blob.read(0, blob.len())?;
        ensure!(
            digest(&bytes) == selected.manifest_blake3,
            "capture manifest differs from seal"
        );
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        ensure!(
            manifest.revision_id.as_deref() == Some(revision)
                && manifest.state == "captured"
                && manifest.sqlite_consistency == "consistent_default_sqlite",
            "capture identity/state mismatch"
        );
        ensure!(
            crate::lightroom::json_digest(&manifest.artifacts)? == revision,
            "artifact roster capture identity mismatch"
        );
        for artifact in &manifest.artifacts {
            let path = Path::new(&artifact.stored);
            ensure!(
                path.components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                    && path.starts_with("raw"),
                "unsafe retained artifact descriptor"
            );
            ensure!(digest_valid(&artifact.blake3), "invalid artifact digest");
        }
        Ok(manifest)
    }
    /// Artifact paths are descriptors only. This function never opens the
    /// manifest's original, output or stored paths. No implicit capture-path trust.
    pub fn capture_manifest(&self, revision: &str) -> Result<Manifest> {
        self.operation(|| self.manifest_inner(revision))
    }

    fn admit_supplement(&self, pin: &SupplementPin) -> Result<()> {
        self.selected(&pin.revision)?;
        let rowid: i64 = self.db.query_row(
            "SELECT sequence FROM paths WHERE revision=? AND source_id=?",
            params![pin.revision, pin.source_id],
            |r| r.get(0),
        )?;
        let blob = Blob::open(&self.db, "paths", "evidence", rowid)?;
        ensure!(
            blob.len() <= crate::lightroom::PAGE_BYTES,
            "supplement baseline metadata limit"
        );
        let evidence: serde_json::Value = serde_json::from_slice(&blob.read(0, blob.len())?)?;
        let matches = evidence
            .get("inspections")
            .and_then(|v| v.as_array())
            .context("supplement has no retained original inspection")?
            .iter()
            .filter(|v| v.get("origin").and_then(|v| v.as_str()) == Some(&pin.origin))
            .collect::<Vec<_>>();
        ensure!(
            matches.len() == 1,
            "supplement original association missing or ambiguous"
        );
        let source: crate::xmp_packets::SourceRevision = serde_json::from_value(
            matches[0]
                .get("revision")
                .context("supplement source revision missing")?
                .clone(),
        )?;
        let status: crate::xmp_packets::Status = serde_json::from_value(
            matches[0]
                .get("status")
                .context("supplement status missing")?
                .clone(),
        )?;
        ensure!(
            source == pin.source_revision && status == pin.historical_status,
            "supplement differs from retained original identity/status"
        );
        Ok(())
    }

    /// Identity for destination idempotency: capture + original table + source
    /// key digest. Never use the inspection-local lineage prefix as import ID.
    pub fn stable_source(&self, revision: &str, source_id: &str) -> Result<StableSource> {
        self.selected(revision)?;
        ensure!(
            !source_id.is_empty() && source_id.len() <= 4096,
            "source ID limit"
        );
        self.operation(|| {
            let mut stmt = self.db.prepare("SELECT sequence FROM rows WHERE revision=? AND source_id=? LIMIT 2")?;
            let mut rows = stmt.query(params![revision,source_id])?;
            let rowid: i64 = rows.next()?.context("source has no retained row; snapshot-only evidence must not acquire a guessed logical identity")?.get(0)?;
            ensure!(rows.next()?.is_none(), "source ID maps to multiple raw rows");
            let table = self.field(revision,Collection::Rows,rowid,"table_name",false)?.text()?.to_owned();
            let blob = Blob::open(&self.db,"rows","key_json",rowid)?;
            let mut hash = blake3::Hasher::new();
            let until = Instant::now()+Duration::from_millis(self.limits.deadline_ms);
            let mut offset=0;
            while offset<blob.len() {
                ensure!(Instant::now()<until, "source key digest deadline");
                let bytes=blob.read(offset,self.limits.chunk_bytes)?;
                hash.update(&bytes);
                offset+=bytes.len();
            }
            let key=self.field(revision,Collection::Rows,rowid,"key_json",false)?;
            if let Field::Inline(Cell::Text(bytes))=&key {
                let parsed: Vec<Cell>=serde_json::from_slice(bytes)?;
                ensure!(serde_json::to_vec(&parsed)?==*bytes,"source key is not schema3 canonical Cell JSON");
            }
            Ok(StableSource {capture_revision:revision.into(),table,source_key:key,source_key_blake3:hash.finalize().to_hex().to_string(),inspection_source_id:source_id.into()})
        })
    }

    fn field(
        &self,
        revision: &str,
        collection: Collection,
        rowid: i64,
        column: &str,
        force_reference: bool,
    ) -> Result<Field> {
        let table = collection.spec().table;
        let kind: String = self.db.query_row(
            &format!("SELECT typeof({column}) FROM {table} WHERE rowid=? AND revision=?"),
            params![rowid, revision],
            |r| r.get(0),
        )?;
        match kind.as_str() {
            "null" => Ok(Field::Inline(Cell::Null)),
            "integer" => Ok(Field::Inline(Cell::Integer(self.db.query_row(
                &format!("SELECT {column} FROM {table} WHERE rowid=?"),
                [rowid],
                |r| r.get(0),
            )?))),
            "real" => Ok(Field::Inline(Cell::RealBits(
                self.db
                    .query_row(
                        &format!("SELECT {column} FROM {table} WHERE rowid=?"),
                        [rowid],
                        |r| r.get::<_, f64>(0),
                    )?
                    .to_bits(),
            ))),
            "text" | "blob" => {
                let blob = Blob::open(&self.db, table, column, rowid)?;
                if force_reference || blob.len() > self.limits.inline_bytes {
                    Ok(Field::Bytes(ByteRef {
                        seal: self.namespace.clone(),
                        revision: revision.into(),
                        collection,
                        rowid,
                        field: column.into(),
                        bytes: blob.len() as u64,
                        text: kind == "text",
                    }))
                } else {
                    let bytes = blob.read(0, blob.len())?;
                    Ok(Field::Inline(if kind == "text" {
                        Cell::Text(bytes)
                    } else {
                        Cell::Blob(bytes)
                    }))
                }
            }
            _ => bail!("unexpected inspection storage type"),
        }
    }

    pub fn page(
        &self,
        revision: &str,
        collection: Collection,
        after: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page> {
        self.selected(revision)?;
        ensure!((1..=1000).contains(&limit), "record page count limit");
        let spec = collection.spec();
        let mut values = vec![rusqlite::types::Value::Text(revision.into())];
        let condition = if let Some(cursor) = after {
            ensure!(
                cursor.seal == self.namespace
                    && cursor.revision == revision
                    && cursor.collection == collection
                    && cursor.after.len() == spec.keys.len(),
                "cursor belongs to different source/scope"
            );
            for key in &cursor.after {
                values.push(match key {
                    Cell::Integer(value) if spec.numeric_key && *value > 0 => {
                        rusqlite::types::Value::Integer(*value)
                    }
                    Cell::Text(bytes)
                        if !spec.numeric_key && bytes.len() <= self.limits.inline_bytes =>
                    {
                        rusqlite::types::Value::Text(String::from_utf8(bytes.clone())?)
                    }
                    _ => bail!("cursor key type or length differs"),
                });
            }
            if spec.keys.len() == 1 {
                format!(" AND {}>?", spec.keys[0])
            } else {
                format!(
                    " AND ({})>({})",
                    spec.keys.join(","),
                    vec!["?"; spec.keys.len()].join(",")
                )
            }
        } else {
            String::new()
        };
        values.push(rusqlite::types::Value::Integer(limit as i64 + 1));
        self.operation(|| {
            let sql = format!("SELECT rowid FROM {} WHERE revision=?{condition} ORDER BY {} LIMIT ?", spec.table, spec.keys.join(","));
            let mut statement = self.db.prepare(&sql)?;
            let mut rows = statement.query(params_from_iter(values))?;
            let mut records = Vec::new();
            let mut bytes = 128usize;
            let mut exhausted = true;
            let until = Instant::now()+Duration::from_millis(self.limits.deadline_ms);
            while let Some(row) = rows.next()? {
                ensure!(Instant::now()<until, "record page deadline exceeded");
                if records.len() == limit { exhausted = false; break; }
                let rowid: i64 = row.get(0)?;
                let mut key = Vec::new();
                for column in spec.keys {
                    let Field::Inline(cell) = self.field(revision,collection,rowid,column,false)? else { bail!("record key exceeds bounded cursor size"); };
                    ensure!(matches!(&cell,Cell::Integer(n) if spec.numeric_key && *n>0) || matches!(&cell,Cell::Text(v) if !spec.numeric_key && std::str::from_utf8(v).is_ok()), "record key storage type differs");
                    key.push(cell);
                }
                let mut fields = BTreeMap::new();
                for column in spec.fields {
                    fields.insert((*column).into(), self.field(revision,collection,rowid,column,matches!(*column,"raw"|"decoded"))?);
                }
                let record = EvidenceRecord { revision:revision.into(), collection, rowid, key, fields };
                let size = bounded_json(&record,self.limits.page_bytes)?.len()+1;
                let cursor = Cursor {seal:self.namespace.clone(),revision:revision.into(),collection,after:record.key.clone()};
                let cursor_size=bounded_json(&cursor,self.limits.page_bytes)?.len();
                if bytes + size + cursor_size > self.limits.page_bytes { ensure!(!records.is_empty(), "one record exceeds page budget"); exhausted=false; break; }
                bytes += size;
                records.push(record);
            }
            let next = records.last().map(|record| Cursor { seal:self.namespace.clone(),revision:revision.into(),collection,after:record.key.clone() });
            let page=Page { records, next, exhausted };
            bounded_json(&page,self.limits.page_bytes)?;
            Ok(page)
        })
    }

    pub fn read_chunk(&self, reference: &ByteRef, offset: u64, limit: usize) -> Result<Vec<u8>> {
        self.selected(&reference.revision)?;
        ensure!(
            reference.seal == self.namespace && reference.rowid > 0,
            "byte reference belongs to different source"
        );
        let spec = reference.collection.spec();
        ensure!(
            spec.fields.contains(&reference.field.as_str())
                && (1..=self.limits.chunk_bytes).contains(&limit),
            "byte field/window is not admitted"
        );
        self.operation(|| {
            let current = self.field(
                &reference.revision,
                reference.collection,
                reference.rowid,
                &reference.field,
                true,
            )?;
            ensure!(
                matches!(&current,Field::Bytes(actual) if actual == reference),
                "byte reference differs from sealed record"
            );
            Blob::open(&self.db, spec.table, &reference.field, reference.rowid)?
                .read(offset.try_into()?, limit)
        })
    }

    pub fn count(&self, revision: &str, collection: Collection) -> Result<u64> {
        self.selected(revision)?;
        self.operation(|| {
            let count: i64 = self.db.query_row(
                &format!(
                    "SELECT count(*) FROM {} WHERE revision=?",
                    collection.spec().table
                ),
                [revision],
                |r| r.get(0),
            )?;
            Ok(count.try_into()?)
        })
    }

    pub fn resolve(
        &self,
        revision: &str,
        source_id: &str,
        field: &str,
        target_table: &str,
    ) -> Result<Resolution> {
        self.selected(revision)?;
        ensure!(
            [source_id, field, target_table]
                .iter()
                .all(|v| !v.is_empty() && v.len() <= 4096),
            "relationship query key limit"
        );
        self.operation(|| {
            // Same schema3 numeric keys and join order as Plan::unique_target.
            let mut stmt = self.db.prepare("SELECT target.source_id FROM references_out reference CROSS JOIN entities target ON target.revision=reference.revision AND target.table_name=reference.target_table AND target.local_key=reference.target_key WHERE reference.revision=? AND reference.source_id=? AND reference.field=? AND reference.target_table=? LIMIT 2")?;
            let mut rows = stmt.query(params![revision,source_id,field,target_table])?;
            let first = rows.next()?.map(|r| r.get::<_,String>(0)).transpose()?;
            if rows.next()?.is_some() { return Ok(Resolution::Ambiguous); }
            Ok(first.map_or(Resolution::Missing,Resolution::Unique))
        })
    }

    pub fn image_links(&self, revision: &str, source_id: &str) -> Result<ImageLinks> {
        self.selected(revision)?;
        self.operation(|| {
            let table: Option<String> = self
                .db
                .query_row(
                    "SELECT table_name FROM entities WHERE revision=? AND source_id=?",
                    params![revision, source_id],
                    |r| r.get(0),
                )
                .optional()?;
            ensure!(
                table.as_deref() == Some("Adobe_images"),
                "not an observed Adobe image record"
            );
            Ok(())
        })?;
        Ok(ImageLinks { image_source_id:source_id.into(), file:self.resolve(revision,source_id,"rootFile","AgLibraryFile")?, master:self.resolve(revision,source_id,"masterImage","Adobe_images")?, current_develop:self.resolve(revision,source_id,"developSettingsIDCache","Adobe_imageDevelopSettings")?, limitations:"Only exact unique retained schema3 links; missing is not proof of a master sentinel or current settings. History/snapshots and unknown tables remain separately retained.".into() })
    }
}
