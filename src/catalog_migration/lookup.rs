//! Compact, selected-custody lookup. Keys are hints into immutable retained
//! records, never replacement payloads or permission to read an original file.
use super::{evidence, retention};
use crate::{
    Catalog,
    catalog_writer::Priority,
    lightroom::migration_source::{Collection, EvidenceRecord, Field, InputSeal},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

const MAX_KEY: usize = 4096;
const MAX_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS migration_record_lookup(
      record INTEGER PRIMARY KEY REFERENCES migration_retained_records(sequence),
      input TEXT NOT NULL, revision TEXT NOT NULL, collection INTEGER NOT NULL,
      digest TEXT NOT NULL, raw_length INTEGER NOT NULL,
      source_id TEXT, table_name TEXT, field TEXT, target_table TEXT, origin TEXT, name TEXT, local_key TEXT, target_key TEXT,
      unavailable TEXT NOT NULL);
      CREATE INDEX IF NOT EXISTS migration_retained_lookup_sequence ON migration_retained_records(input,sequence);
      CREATE INDEX IF NOT EXISTS migration_lookup_source ON migration_record_lookup(input,revision,collection,source_id,record) WHERE source_id IS NOT NULL;
      CREATE INDEX IF NOT EXISTS migration_lookup_table ON migration_record_lookup(input,revision,collection,table_name,record) WHERE table_name IS NOT NULL;
      CREATE INDEX IF NOT EXISTS migration_lookup_field ON migration_record_lookup(input,revision,collection,source_id,field,target_table,record) WHERE source_id IS NOT NULL AND field IS NOT NULL AND target_table IS NOT NULL;
      CREATE INDEX IF NOT EXISTS migration_lookup_source_field ON migration_record_lookup(input,revision,collection,source_id,field,record) WHERE source_id IS NOT NULL AND field IS NOT NULL;
      CREATE INDEX IF NOT EXISTS migration_lookup_source_target ON migration_record_lookup(input,revision,collection,source_id,target_table,record) WHERE source_id IS NOT NULL AND target_table IS NOT NULL;
      CREATE INDEX IF NOT EXISTS migration_lookup_target ON migration_record_lookup(input,revision,collection,target_table,record) WHERE target_table IS NOT NULL;
      CREATE INDEX IF NOT EXISTS migration_lookup_origin ON migration_record_lookup(input,revision,collection,source_id,origin,record) WHERE source_id IS NOT NULL AND origin IS NOT NULL;
      CREATE INDEX IF NOT EXISTS migration_lookup_local_key ON migration_record_lookup(input,revision,collection,table_name,local_key,record) WHERE table_name IS NOT NULL AND local_key IS NOT NULL;
      CREATE INDEX IF NOT EXISTS migration_lookup_target_key ON migration_record_lookup(input,revision,collection,target_table,target_key,record) WHERE target_table IS NOT NULL AND target_key IS NOT NULL;
      CREATE INDEX IF NOT EXISTS migration_lookup_name ON migration_record_lookup(input,revision,collection,name,record) WHERE name IS NOT NULL;
      CREATE INDEX IF NOT EXISTS migration_lookup_unavailable ON migration_record_lookup(input,revision,collection,record) WHERE unavailable!='[]';
      CREATE TABLE IF NOT EXISTS migration_lookup_backfill(
        input TEXT PRIMARY KEY REFERENCES migration_retention(id), cursor INTEGER NOT NULL DEFAULT 0);")?;
    Ok(())
}

/// Schema 9 companion indexes retain NULL keys: an unavailable key can still
/// hide a match. Collection predicates avoid copying ancillary entity rows into
/// reference, packet, or table-name indexes.
pub(crate) fn install_availability_indexes(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE INDEX IF NOT EXISTS migration_unavailable_source ON migration_record_lookup(input,revision,collection,source_id,record) WHERE unavailable!='[]' AND collection IN (3,4,6);
      CREATE INDEX IF NOT EXISTS migration_unavailable_table ON migration_record_lookup(input,revision,collection,table_name,local_key,record) WHERE unavailable!='[]' AND collection IN (3,4);
      CREATE INDEX IF NOT EXISTS migration_unavailable_reference ON migration_record_lookup(input,revision,collection,source_id,field,target_table,record) WHERE unavailable!='[]' AND collection=5;
      CREATE INDEX IF NOT EXISTS migration_unavailable_source_target ON migration_record_lookup(input,revision,collection,source_id,target_table,record) WHERE unavailable!='[]' AND collection=5;
      CREATE INDEX IF NOT EXISTS migration_unavailable_target ON migration_record_lookup(input,revision,collection,target_table,target_key,record) WHERE unavailable!='[]' AND collection=5;
      CREATE INDEX IF NOT EXISTS migration_unavailable_packet ON migration_record_lookup(input,revision,collection,source_id,origin,record) WHERE unavailable!='[]' AND collection=7;
      CREATE INDEX IF NOT EXISTS migration_unavailable_name ON migration_record_lookup(input,revision,collection,name,record) WHERE unavailable!='[]' AND collection=2;")?;
    Ok(())
}

fn ordinal(collection: Collection) -> i64 {
    match collection {
        Collection::Captures => 0,
        Collection::SchemaObjects => 1,
        Collection::Tables => 2,
        Collection::Rows => 3,
        Collection::Entities => 4,
        Collection::References => 5,
        Collection::Paths => 6,
        Collection::Packets => 7,
        Collection::MetadataFacts => 8,
        Collection::Issues => 9,
    }
}
fn expected(collection: Collection) -> &'static [&'static str] {
    match collection {
        Collection::Rows => &["source_id", "table_name"],
        Collection::Entities => &["source_id", "table_name", "local_key"],
        Collection::References => &["source_id", "field", "target_table", "target_key"],
        Collection::Paths => &["source_id"],
        Collection::Packets => &["source_id", "origin"],
        Collection::Tables => &["name"],
        _ => &[],
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnavailableReason {
    Missing,
    NonText,
    NonUtf8,
    Oversized,
    ExternalBytes,
    UnsupportedCollection,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnavailableKey {
    pub field: String,
    pub reason: UnavailableReason,
}

/// Prepare outside writer admission. Private fields prevent a caller substituting
/// keys while retaining a trusted digest. Canonical serialization is the exact
/// existing retention serialization, bounded to one ordinary evidence record.
#[derive(Clone, Debug)]
pub struct PreparedIndex {
    canonical: Vec<u8>,
    revision: String,
    collection: Collection,
    rowid: i64,
    digest: String,
    raw_length: usize,
    keys: [Option<String>; 8],
    unavailable: String,
}
impl PreparedIndex {
    /// Canonical bytes used for the immutable lookup digest, also retained by
    /// the importer so a batch serializes every evidence record only once.
    pub(crate) fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }
    pub fn new(record: &EvidenceRecord) -> Result<Self> {
        // Cell serialization hex-encodes into a temporary string before writing.
        // Bound those allocations too, not only the eventual output writer.
        for cell in record
            .key
            .iter()
            .chain(record.fields.values().filter_map(|f| match f {
                Field::Inline(c) => Some(c),
                _ => None,
            }))
        {
            if let crate::lightroom::plan::Cell::Text(bytes)
            | crate::lightroom::plan::Cell::Blob(bytes) = cell
            {
                ensure!(
                    bytes.len() <= MAX_BYTES / 2,
                    "lookup cell exceeds retained bound"
                );
            }
        }
        struct Bounded(Vec<u8>);
        impl std::io::Write for Bounded {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > MAX_BYTES - self.0.len() {
                    return Err(std::io::Error::other(
                        "lookup record exceeds retained bound",
                    ));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut bounded = Bounded(Vec::new());
        serde_json::to_writer(&mut bounded, record)?;
        let raw = bounded.0;
        ensure!(
            raw.len() <= MAX_BYTES,
            "lookup record exceeds retained bound"
        );
        let mut keys: [Option<String>; 8] = Default::default();
        let mut unavailable = Vec::new();
        for field in expected(record.collection) {
            let value = match record.fields.get(*field) {
                None => Err(UnavailableReason::Missing),
                Some(Field::Bytes(_)) => Err(UnavailableReason::ExternalBytes),
                Some(Field::Inline(crate::lightroom::plan::Cell::Text(bytes))) => {
                    if bytes.len() > MAX_KEY {
                        Err(UnavailableReason::Oversized)
                    } else {
                        std::str::from_utf8(bytes)
                            .map(str::to_owned)
                            .map_err(|_| UnavailableReason::NonUtf8)
                    }
                }
                _ => Err(UnavailableReason::NonText),
            };
            match value {
                Ok(value) => keys[key_slot(field)] = Some(value),
                Err(reason) => unavailable.push(UnavailableKey {
                    field: (*field).into(),
                    reason,
                }),
            }
        }
        if expected(record.collection).is_empty() {
            unavailable.push(UnavailableKey {
                field: String::new(),
                reason: UnavailableReason::UnsupportedCollection,
            });
        }
        Ok(Self {
            revision: record.revision.clone(),
            collection: record.collection,
            rowid: record.rowid,
            digest: blake3::hash(&raw).to_hex().to_string(),
            raw_length: raw.len(),
            keys,
            unavailable: serde_json::to_string(&unavailable)?,
            canonical: raw,
        })
    }
}
fn key_slot(field: &str) -> usize {
    match field {
        "source_id" => 0,
        "table_name" => 1,
        "field" => 2,
        "target_table" => 3,
        "origin" => 4,
        "name" => 5,
        "local_key" => 6,
        "target_key" => 7,
        _ => unreachable!(),
    }
}
fn authority(db: &Connection, input: &str, revision: &str) -> Result<()> {
    let length: i64 = db.query_row(
        "SELECT length(seal) FROM migration_retention WHERE id=?1",
        [input],
        |r| r.get(0),
    )?;
    ensure!(
        (0..=MAX_BYTES as i64).contains(&length),
        "lookup seal bound"
    );
    let bytes: Vec<u8> = db.query_row(
        "SELECT seal FROM migration_retention WHERE id=?1",
        [input],
        |r| r.get(0),
    )?;
    let seal: InputSeal = serde_json::from_slice(&bytes)?;
    ensure!(
        seal.binding_blake3()? == input
            && seal.selected.iter().any(|s| s.revision == revision)
            && !seal.excluded_revisions.iter().any(|r| r == revision),
        "lookup selection authority differs"
    );
    Ok(())
}

/// Transaction helper: caller stages the actual retained record first in this
/// same transaction. Generic evidence uploads have no retained sequence and
/// cannot mint lookup entries. Completion visibility stays owned by retention.
pub(crate) fn index_record(db: &Connection, sequence: i64, p: &PreparedIndex) -> Result<()> {
    let (input,revision,collection,rowid,digest,length):(String,String,i64,i64,String,usize)=db.query_row(
        "SELECT input,revision,collection,source_rowid,digest,raw_length FROM migration_retained_records WHERE sequence=?1",
        [sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,evidence::size(r,5)?)))?;
    ensure!(
        revision == p.revision
            && collection == ordinal(p.collection)
            && rowid == p.rowid
            && digest == p.digest
            && length == p.raw_length,
        "lookup does not match stored retained record"
    );
    authority(db, &input, &revision)?;
    db.execute("INSERT OR IGNORE INTO migration_record_lookup VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",params![sequence,input,revision,collection,digest,i64::try_from(length)?,p.keys[0],p.keys[1],p.keys[2],p.keys[3],p.keys[4],p.keys[5],p.keys[6],p.keys[7],p.unavailable])?;
    let matches:bool=db.query_row("SELECT input=?2 AND revision=?3 AND collection=?4 AND digest=?5 AND raw_length=?6 AND source_id IS ?7 AND table_name IS ?8 AND field IS ?9 AND target_table IS ?10 AND origin IS ?11 AND name IS ?12 AND local_key IS ?13 AND target_key IS ?14 AND unavailable=?15 FROM migration_record_lookup WHERE record=?1",params![sequence,input,revision,collection,digest,i64::try_from(length)?,p.keys[0],p.keys[1],p.keys[2],p.keys[3],p.keys[4],p.keys[5],p.keys[6],p.keys[7],p.unavailable],|r|r.get(0))?;
    ensure!(matches, "immutable lookup replay differs");
    db.execute(
        "INSERT OR IGNORE INTO migration_lookup_backfill(input) VALUES(?1)",
        [&input],
    )?;
    // Advance only a contiguous prefix. Existing older records must be backfilled.
    db.execute("UPDATE migration_lookup_backfill SET cursor=?2 WHERE input=?1 AND cursor=COALESCE((SELECT MAX(sequence) FROM migration_retained_records WHERE input=?1 AND sequence<?2),0)",params![input,sequence])?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lookup {
    RowsBySource(String),
    RowsByTable(String),
    EntitiesBySource(String),
    EntitiesByTable(String),
    /// Exact retained canonical string: never parsed or recanonicalized here.
    EntitiesByLocalKey {
        table_name: String,
        local_key: String,
    },
    References {
        source_id: String,
        field: Option<String>,
        target_table: Option<String>,
    },
    ReferencesByTarget(String),
    ReferencesByTargetKey {
        target_table: String,
        target_key: String,
    },
    Packets {
        source_id: String,
        origin: Option<String>,
    },
    PathsBySource(String),
    TableByName(String),
    /// Explicitly enumerate records whose expected keys cannot be interpreted.
    Unavailable(Collection),
}
impl Lookup {
    fn index(&self) -> &'static str {
        match self {
            Self::RowsBySource(_) | Self::EntitiesBySource(_) | Self::PathsBySource(_) => {
                "migration_lookup_source"
            }
            Self::RowsByTable(_) | Self::EntitiesByTable(_) => "migration_lookup_table",
            Self::References {
                field: Some(_),
                target_table: Some(_),
                ..
            } => "migration_lookup_field",
            Self::References {
                field: Some(_),
                target_table: None,
                ..
            } => "migration_lookup_source_field",
            Self::References {
                field: None,
                target_table: Some(_),
                ..
            } => "migration_lookup_source_target",
            Self::References { .. } | Self::Packets { origin: None, .. } => {
                "migration_lookup_source"
            }
            Self::ReferencesByTarget(_) => "migration_lookup_target",
            Self::EntitiesByLocalKey { .. } => "migration_lookup_local_key",
            Self::ReferencesByTargetKey { .. } => "migration_lookup_target_key",
            Self::Packets {
                origin: Some(_), ..
            } => "migration_lookup_origin",
            Self::TableByName(_) => "migration_lookup_name",
            Self::Unavailable(_) => "migration_lookup_unavailable",
        }
    }
    fn availability_index(&self) -> (&'static str, &'static str) {
        match self {
            Self::RowsBySource(_) | Self::EntitiesBySource(_) | Self::PathsBySource(_) => {
                ("migration_unavailable_source", "l.collection IN (3,4,6)")
            }
            Self::RowsByTable(_) | Self::EntitiesByTable(_) | Self::EntitiesByLocalKey { .. } => {
                ("migration_unavailable_table", "l.collection IN (3,4)")
            }
            Self::References {
                field: None,
                target_table: Some(_),
                ..
            } => ("migration_unavailable_source_target", "l.collection=5"),
            Self::References { .. } => ("migration_unavailable_reference", "l.collection=5"),
            Self::ReferencesByTarget(_) | Self::ReferencesByTargetKey { .. } => {
                ("migration_unavailable_target", "l.collection=5")
            }
            Self::Packets { .. } => ("migration_unavailable_packet", "l.collection=7"),
            Self::TableByName(_) => ("migration_unavailable_name", "l.collection=2"),
            Self::Unavailable(_) => ("migration_lookup_unavailable", "1"),
        }
    }
    fn spec(&self) -> (Collection, Vec<(&'static str, &str)>) {
        match self {
            Self::RowsBySource(v) => (Collection::Rows, vec![("source_id", v)]),
            Self::RowsByTable(v) => (Collection::Rows, vec![("table_name", v)]),
            Self::EntitiesBySource(v) => (Collection::Entities, vec![("source_id", v)]),
            Self::EntitiesByTable(v) => (Collection::Entities, vec![("table_name", v)]),
            Self::EntitiesByLocalKey {
                table_name,
                local_key,
            } => (
                Collection::Entities,
                vec![("table_name", table_name), ("local_key", local_key)],
            ),
            Self::ReferencesByTargetKey {
                target_table,
                target_key,
            } => (
                Collection::References,
                vec![("target_table", target_table), ("target_key", target_key)],
            ),
            Self::References {
                source_id,
                field,
                target_table,
            } => {
                let mut f = vec![("source_id", source_id.as_str())];
                if let Some(v) = field {
                    f.push(("field", v));
                }
                if let Some(v) = target_table {
                    f.push(("target_table", v));
                }
                (Collection::References, f)
            }
            Self::ReferencesByTarget(v) => (Collection::References, vec![("target_table", v)]),
            Self::Packets { source_id, origin } => {
                let mut f = vec![("source_id", source_id.as_str())];
                if let Some(v) = origin {
                    f.push(("origin", v));
                }
                (Collection::Packets, f)
            }
            Self::PathsBySource(v) => (Collection::Paths, vec![("source_id", v)]),
            Self::TableByName(v) => (Collection::Tables, vec![("name", v)]),
            Self::Unavailable(c) => (*c, vec![]),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupCursor {
    pub query_blake3: String,
    pub high_water: i64,
    pub after: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LookupHit {
    pub sequence: i64,
    pub unavailable: Vec<UnavailableKey>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LookupPage {
    pub records: Vec<LookupHit>,
    pub next: Option<LookupCursor>,
    /// True only when all retained records through this page's snapshot have an
    /// index and retention has completed. False never proves a missing target.
    pub coverage_complete: bool,
    /// Conservatively false if a record that could match this query has any
    /// unavailable expected key. Known mismatches exclude unrelated records;
    /// NULL keys remain possible matches, including outside the current page.
    /// Use Unavailable(collection) to inspect all classifications and raw custody.
    pub keys_complete: bool,
}

// At most three filters: enumerate the disjoint known-value/unknown-key cases
// as exact index seeks, rather than scanning the collection's unavailable rows.
// Availability covers the whole query snapshot, not only the returned page.
fn availability_query(
    input: &str,
    revision: &str,
    query: &Lookup,
    high: i64,
) -> (String, Vec<rusqlite::types::Value>) {
    let (collection, filters) = query.spec();
    let (index, predicate) = query.availability_index();
    let mut values: Vec<rusqlite::types::Value> = vec![
        input.to_owned().into(),
        revision.to_owned().into(),
        ordinal(collection).into(),
        high.into(),
    ];
    let mut branches = Vec::new();
    for unknown in 0..(1 << filters.len()) {
        let mut sql = format!(
            "SELECT 1 FROM migration_record_lookup l INDEXED BY {index} JOIN migration_retained_records r ON r.sequence=l.record AND r.complete=1 WHERE l.input=?1 AND l.revision=?2 AND l.collection=?3 AND l.record<=?4 AND l.unavailable!='[]' AND {predicate}"
        );
        for (bit, (field, value)) in filters.iter().enumerate() {
            if unknown & (1 << bit) == 0 {
                values.push((*value).to_owned().into());
                sql.push_str(&format!(" AND l.{field}=?{}", values.len()));
            } else {
                sql.push_str(&format!(" AND l.{field} IS NULL"));
            }
        }
        branches.push(sql);
    }
    (
        format!("SELECT EXISTS({})", branches.join(" UNION ALL ")),
        values,
    )
}

fn page(
    db: &Connection,
    input: &str,
    revision: &str,
    query: &Lookup,
    cursor: Option<&LookupCursor>,
    limit: usize,
) -> Result<LookupPage> {
    ensure!(
        (1..=100).contains(&limit) && input.len() <= MAX_KEY && revision.len() <= MAX_KEY,
        "lookup page bounds"
    );
    let (collection, filters) = query.spec();
    ensure!(
        filters.iter().all(|(_, v)| v.len() <= MAX_KEY),
        "lookup key exceeds inline bound"
    );
    authority(db, input, revision)?;
    let high: i64 = db.query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM migration_retained_records WHERE input=?1",
        [input],
        |r| r.get(0),
    )?;
    let indexed: i64 = db
        .query_row(
            "SELECT cursor FROM migration_lookup_backfill WHERE input=?1",
            [input],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let complete: bool = db.query_row(
        "SELECT complete FROM migration_retention WHERE id=?1",
        [input],
        |r| r.get(0),
    )?;
    let hash = blake3::hash(&serde_json::to_vec(&(1u32, input, revision, query))?)
        .to_hex()
        .to_string();
    let (high, after) = if let Some(c) = cursor {
        ensure!(
            c.query_blake3 == hash
                && c.after >= 0
                && c.after <= c.high_water
                && c.high_water <= high,
            "lookup cursor differs"
        );
        (c.high_water, c.after)
    } else {
        (high, 0)
    };
    // Do not hand out a cursor that could skip a staged record completing later.
    ensure!(
        complete && indexed >= high,
        "lookup index or retention pending; backfill/resume before querying"
    );
    let mut sql = format!(
        "SELECT l.record,l.unavailable,(r.complete=1 AND r.digest=l.digest AND r.raw_length=l.raw_length AND r.input=l.input AND r.revision=l.revision AND r.collection=l.collection) FROM migration_record_lookup l INDEXED BY {} JOIN migration_retained_records r ON r.sequence=l.record WHERE l.input=? AND l.revision=? AND l.collection=? AND l.record>? AND l.record<=?",
        query.index()
    );
    let mut values: Vec<rusqlite::types::Value> = vec![
        input.to_owned().into(),
        revision.to_owned().into(),
        ordinal(collection).into(),
        after.into(),
        high.into(),
    ];
    for (field, value) in filters {
        sql.push_str(&format!(" AND l.{field}=?"));
        values.push(value.to_owned().into());
    }
    if matches!(query, Lookup::Unavailable(_)) {
        sql.push_str(" AND l.unavailable!='[]'");
    }
    sql.push_str(" ORDER BY l.record LIMIT ?");
    values.push(i64::try_from(limit + 1)?.into());
    let mut stmt = db.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(values))?;
    let mut records = Vec::new();
    let mut more = false;
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        if records.len() == limit {
            more = true;
            break;
        }
        ensure!(
            row.get::<_, bool>(2)?,
            "lookup retained identity or completion differs"
        );
        let raw: String = row.get(1)?;
        ensure!(raw.len() <= 4096, "lookup classification exceeds bound");
        let hit = LookupHit {
            sequence: row.get(0)?,
            unavailable: serde_json::from_str(&raw)?,
        };
        bytes += serde_json::to_vec(&hit)?.len();
        ensure!(bytes <= MAX_BYTES, "lookup output bound");
        records.push(hit);
    }
    let (availability_sql, availability_values) = availability_query(input, revision, query, high);
    let unavailable: bool = db.query_row(
        &availability_sql,
        rusqlite::params_from_iter(availability_values),
        |r| r.get(0),
    )?;
    let next = if more {
        Some(LookupCursor {
            query_blake3: hash,
            high_water: high,
            after: records.last().context("empty continuation")?.sequence,
        })
    } else {
        None
    };
    Ok(LookupPage {
        records,
        next,
        coverage_complete: true,
        keys_complete: !unavailable,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackfillProgress {
    pub indexed: usize,
    pub raw_bytes: usize,
    pub cursor: i64,
    pub high_water: i64,
    pub complete: bool,
}
impl Catalog {
    pub fn migration_lookup(
        &self,
        input: &str,
        revision: &str,
        query: &Lookup,
        cursor: Option<&LookupCursor>,
        limit: usize,
    ) -> Result<LookupPage> {
        page(&self.db, input, revision, query, cursor, limit)
    }
    /// Always use this verified read before interpreting IDs returned by lookup.
    pub fn migration_lookup_record(&self, sequence: i64) -> Result<EvidenceRecord> {
        let record = retention::selected_record(&self.db, sequence)?;
        let prepared = PreparedIndex::new(&record)?;
        let p = &prepared;
        let matches:bool=self.db.query_row("SELECT l.digest=?2 AND l.raw_length=?3 AND l.revision=?4 AND l.collection=?5 AND l.source_id IS ?6 AND l.table_name IS ?7 AND l.field IS ?8 AND l.target_table IS ?9 AND l.origin IS ?10 AND l.name IS ?11 AND l.local_key IS ?12 AND l.target_key IS ?13 AND l.unavailable=?14 AND r.input=l.input AND r.revision=l.revision AND r.collection=l.collection AND r.source_rowid=?15 AND r.digest=l.digest AND r.raw_length=l.raw_length FROM migration_record_lookup l JOIN migration_retained_records r ON r.sequence=l.record WHERE l.record=?1",params![sequence,p.digest,i64::try_from(p.raw_length)?,p.revision,ordinal(p.collection),p.keys[0],p.keys[1],p.keys[2],p.keys[3],p.keys[4],p.keys[5],p.keys[6],p.keys[7],p.unavailable,p.rowid],|r|r.get(0))?;
        ensure!(matches, "lookup record changed");
        Ok(record)
    }
    /// Existing completed custody can be indexed without reconnecting its source.
    /// At most 100 records / 8MiB decoded bytes are prepared outside the writer.
    pub fn step_migration_lookup(&mut self, input: &str, limit: usize) -> Result<BackfillProgress> {
        ensure!(
            (1..=100).contains(&limit) && input.len() <= MAX_KEY,
            "lookup backfill bounds"
        );
        let cursor: i64 = self
            .db
            .query_row(
                "SELECT cursor FROM migration_lookup_backfill WHERE input=?1",
                [input],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let high: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM migration_retained_records WHERE input=?1",
            [input],
            |r| r.get(0),
        )?;
        let roster = {
            let mut stmt=self.db.prepare("SELECT sequence,raw_length,complete FROM migration_retained_records WHERE input=?1 AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4")?;
            stmt.query_map(params![input, cursor, high, i64::try_from(limit)?], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    evidence::size(r, 1)?,
                    r.get::<_, bool>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut prepared = Vec::new();
        let mut bytes = 0;
        for (sequence, length, complete) in roster {
            ensure!(length <= MAX_BYTES, "lookup retained record bound");
            if !complete || bytes + length > MAX_BYTES {
                break;
            }
            let record = retention::selected_record(&self.db, sequence)?;
            prepared.push((sequence, PreparedIndex::new(&record)?));
            bytes += length;
        }
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: i64 = tx
            .query_row(
                "SELECT cursor FROM migration_lookup_backfill WHERE input=?1",
                [input],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        ensure!(
            cursor == current,
            "lookup backfill advanced concurrently; retry"
        );
        tx.execute(
            "INSERT OR IGNORE INTO migration_lookup_backfill(input) VALUES(?1)",
            [input],
        )?;
        for (sequence, p) in &prepared {
            index_record(&tx, *sequence, p)?;
        }
        let after = prepared.last().map(|v| v.0).unwrap_or(cursor);
        tx.commit()?;
        Ok(BackfillProgress {
            indexed: prepared.len(),
            raw_bytes: bytes,
            cursor: after,
            high_water: high,
            complete: after >= high,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lightroom::{migration_source::tests::Fixture, plan::Cell};
    use flate2::{Compression, write::ZlibEncoder};
    use std::{collections::BTreeMap, io::Write};

    struct TestCatalog {
        _temp: tempfile::TempDir,
        catalog: Catalog,
        input: String,
        revision: String,
        excluded: String,
    }
    impl TestCatalog {
        fn new() -> Result<Self> {
            let fixture = Fixture::new();
            let input = fixture.seal.binding_blake3()?;
            let revision = fixture.seal.selected[0].revision.clone();
            let excluded = fixture.seal.excluded_revisions[0].clone();
            let temp = tempfile::tempdir()?;
            let catalog = Catalog::open(temp.path().join("catalog"))?;
            install(&catalog.db)?;
            catalog.db.execute(
                "INSERT INTO migration_retention(id,seal,approval,complete) VALUES(?1,?2,?3,1)",
                params![
                    input,
                    serde_json::to_vec(&fixture.seal)?,
                    b"synthetic lookup unit test".as_slice()
                ],
            )?;
            Ok(Self {
                _temp: temp,
                catalog,
                input,
                revision,
                excluded,
            })
        }
        fn record(
            &self,
            collection: Collection,
            rowid: i64,
            fields: &[(&str, &str)],
        ) -> EvidenceRecord {
            EvidenceRecord {
                revision: self.revision.clone(),
                collection,
                rowid,
                key: vec![Cell::Integer(rowid)],
                fields: fields
                    .iter()
                    .map(|(k, v)| {
                        (
                            (*k).into(),
                            Field::Inline(Cell::Text(v.as_bytes().to_vec())),
                        )
                    })
                    .collect(),
            }
        }
        fn store(&self, record: &EvidenceRecord, complete: bool) -> Result<i64> {
            let raw = serde_json::to_vec(record)?;
            let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
            z.write_all(&raw)?;
            self.catalog.db.execute("INSERT INTO migration_retained_records(input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete) VALUES(?1,?2,?3,?4,?5,?6,?7,'unused-test-cursor',?8)",params![self.input,record.revision,ordinal(record.collection),record.rowid,z.finish()?,i64::try_from(raw.len())?,blake3::hash(&raw).to_hex().to_string(),complete])?;
            Ok(self.catalog.db.last_insert_rowid())
        }
        fn index(&self, record: &EvidenceRecord, complete: bool) -> Result<i64> {
            let sequence = self.store(record, complete)?;
            index_record(&self.catalog.db, sequence, &PreparedIndex::new(record)?)?;
            Ok(sequence)
        }
        fn query(&self, q: &Lookup, c: Option<&LookupCursor>, limit: usize) -> Result<LookupPage> {
            self.catalog
                .migration_lookup(&self.input, &self.revision, q, c, limit)
        }
    }

    #[test]
    fn prepared_index_rejects_forgery_replay_mutation_and_unselected_records() -> Result<()> {
        let f = TestCatalog::new()?;
        let r = f.record(
            Collection::Rows,
            1,
            &[("source_id", "x"), ("table_name", "Adobe_images")],
        );
        let p = PreparedIndex::new(&r)?;
        assert!(index_record(&f.catalog.db, 123, &p).is_err());
        let sequence = f.store(&r, false)?;
        index_record(&f.catalog.db, sequence, &p)?;
        index_record(&f.catalog.db, sequence, &p)?;
        assert_eq!(
            f.catalog.db.query_row::<i64, _, _>(
                "SELECT count(*) FROM migration_record_lookup",
                [],
                |r| r.get(0)
            )?,
            1
        );
        let mut wrong = r.clone();
        wrong.fields.insert(
            "source_id".into(),
            Field::Inline(Cell::Text(b"forged".to_vec())),
        );
        assert!(index_record(&f.catalog.db, sequence, &PreparedIndex::new(&wrong)?).is_err());
        // A staged record has no public payload authority.
        assert!(f.catalog.migration_lookup_record(sequence).is_err());
        f.catalog.db.execute(
            "UPDATE migration_record_lookup SET source_id='tampered' WHERE record=?1",
            [sequence],
        )?;
        assert!(index_record(&f.catalog.db, sequence, &p).is_err());
        let mut excluded = r;
        excluded.revision = f.excluded.clone();
        let e = f.store(&excluded, true)?;
        assert!(index_record(&f.catalog.db, e, &PreparedIndex::new(&excluded)?).is_err());
        Ok(())
    }

    #[test]
    fn all_typed_queries_page_duplicates_and_bind_cursor() -> Result<()> {
        let f = TestCatalog::new()?;
        for i in 1..=205 {
            let r = f.record(
                Collection::References,
                i,
                &[
                    ("source_id", "s"),
                    ("field", "folder"),
                    ("target_table", "AgLibraryFolder"),
                ],
            );
            f.index(&r, true)?;
        }
        let cases = [
            (
                Collection::Rows,
                vec![("source_id", "s"), ("table_name", "Adobe_images")],
                vec![
                    Lookup::RowsBySource("s".into()),
                    Lookup::RowsByTable("Adobe_images".into()),
                ],
            ),
            (
                Collection::Entities,
                vec![
                    ("source_id", "s"),
                    ("table_name", "Adobe_images"),
                    ("local_key", "key"),
                ],
                vec![
                    Lookup::EntitiesBySource("s".into()),
                    Lookup::EntitiesByTable("Adobe_images".into()),
                ],
            ),
            (
                Collection::Packets,
                vec![("source_id", "s"), ("origin", "sidecar")],
                vec![
                    Lookup::Packets {
                        source_id: "s".into(),
                        origin: None,
                    },
                    Lookup::Packets {
                        source_id: "s".into(),
                        origin: Some("sidecar".into()),
                    },
                ],
            ),
            (
                Collection::Paths,
                vec![("source_id", "s")],
                vec![Lookup::PathsBySource("s".into())],
            ),
            (
                Collection::Tables,
                vec![("name", "Adobe_images")],
                vec![Lookup::TableByName("Adobe_images".into())],
            ),
        ];
        for (c, fields, queries) in cases {
            let r = f.record(c, 1, &fields);
            let id = f.index(&r, true)?;
            for q in queries {
                let p = f.query(&q, None, 100)?;
                assert_eq!(p.records.len(), 1);
                assert_eq!(p.records[0].sequence, id);
                assert!(p.keys_complete);
                assert!(p.next.is_none());
            }
        }
        for q in [
            Lookup::References {
                source_id: "s".into(),
                field: None,
                target_table: None,
            },
            Lookup::References {
                source_id: "s".into(),
                field: Some("folder".into()),
                target_table: None,
            },
            Lookup::References {
                source_id: "s".into(),
                field: None,
                target_table: Some("AgLibraryFolder".into()),
            },
            Lookup::References {
                source_id: "s".into(),
                field: Some("folder".into()),
                target_table: Some("AgLibraryFolder".into()),
            },
            Lookup::ReferencesByTarget("AgLibraryFolder".into()),
        ] {
            let mut cursor = None;
            let mut all = Vec::new();
            loop {
                let p = f.query(&q, cursor.as_ref(), 37)?;
                all.extend(p.records.iter().map(|r| r.sequence));
                if p.next.is_none() {
                    break;
                }
                cursor = p.next;
            }
            assert_eq!(all, (1..=205).collect::<Vec<_>>());
        }
        let q = Lookup::ReferencesByTarget("AgLibraryFolder".into());
        let p = f.query(&q, None, 100)?;
        assert!(
            f.query(&Lookup::RowsBySource("s".into()), p.next.as_ref(), 100)
                .is_err()
        );
        assert!(f.query(&q, None, 101).is_err());
        assert!(f.query(&q, None, 0).is_err());
        let literal = r#"[{"type":"Integer","value":17913}]"#;
        let entity = f.record(
            Collection::Entities,
            2,
            &[
                ("source_id", "s\0λ"),
                ("table_name", "AgLibraryFolder"),
                ("local_key", literal),
            ],
        );
        let entity_id = f.index(&entity, true)?;
        let q = Lookup::EntitiesByLocalKey {
            table_name: "AgLibraryFolder".into(),
            local_key: literal.into(),
        };
        assert_eq!(f.query(&q, None, 100)?.records[0].sequence, entity_id);
        for different in [
            "17913",
            "17913.0",
            "017913",
            r#"[{"type":"Integer","value":-17913}]"#,
            r#"[{"type":"RealBits","value":4670633915494088704}]"#,
        ] {
            let p = f.query(
                &Lookup::EntitiesByLocalKey {
                    table_name: "AgLibraryFolder".into(),
                    local_key: different.into(),
                },
                None,
                100,
            )?;
            assert!(p.records.is_empty());
            assert!(p.keys_complete);
        }
        assert!(
            f.query(
                &Lookup::EntitiesByLocalKey {
                    table_name: "different".into(),
                    local_key: literal.into()
                },
                None,
                100
            )?
            .records
            .is_empty()
        );
        assert_eq!(
            f.query(&Lookup::EntitiesBySource("s\0λ".into()), None, 100)?
                .records[0]
                .sequence,
            entity_id
        );
        let reference = f.record(
            Collection::References,
            206,
            &[
                ("source_id", "literal-source"),
                ("field", "folder"),
                ("target_table", "AgLibraryFolder"),
                ("target_key", literal),
            ],
        );
        let reference_id = f.index(&reference, true)?;
        assert_eq!(
            f.query(
                &Lookup::ReferencesByTargetKey {
                    target_table: "AgLibraryFolder".into(),
                    target_key: literal.into()
                },
                None,
                100
            )?
            .records[0]
                .sequence,
            reference_id
        );
        let value = f.catalog.migration_lookup_record(1)?;
        assert_eq!(value.collection, Collection::References);
        f.catalog.db.execute(
            "UPDATE migration_retained_records SET compressed=x'00' WHERE sequence=1",
            [],
        )?;
        assert!(f.catalog.migration_lookup_record(1).is_err());
        Ok(())
    }

    #[test]
    fn availability_excludes_known_mismatches_but_preserves_possible_matches() -> Result<()> {
        let f = TestCatalog::new()?;
        for id in 1..=2 {
            f.index(
                &f.record(
                    Collection::Entities,
                    id,
                    &[
                        ("source_id", "image"),
                        ("table_name", "Adobe_images"),
                        ("local_key", "key"),
                    ],
                ),
                true,
            )?;
        }
        let mut ancillary = f.record(
            Collection::Entities,
            3,
            &[
                ("source_id", "ancillary"),
                ("table_name", "ImageChangeCounter"),
            ],
        );
        ancillary
            .fields
            .insert("local_key".into(), Field::Inline(Cell::Integer(17)));
        f.index(&ancillary, true)?;
        let query = Lookup::EntitiesBySource("image".into());
        let first = f.query(&query, None, 1)?;
        assert!(first.keys_complete && first.next.is_some());
        assert_eq!(f.query(&query, first.next.as_ref(), 1)?.records.len(), 1);
        assert!(
            f.query(
                &Lookup::EntitiesByLocalKey {
                    table_name: "Adobe_images".into(),
                    local_key: "key".into()
                },
                None,
                2
            )?
            .keys_complete
        );
        let unknown = f.query(&Lookup::EntitiesBySource("ancillary".into()), None, 2)?;
        assert!(!unknown.keys_complete);
        assert_eq!(
            unknown.records[0].unavailable[0].reason,
            UnavailableReason::NonText
        );
        assert!(
            !f.query(&Lookup::Unavailable(Collection::Entities), None, 100)?
                .keys_complete
        );
        // A later unknown source could hide this image. It affects a new query,
        // but must not alter the earlier cursor's high-water snapshot.
        f.index(
            &f.record(
                Collection::Entities,
                4,
                &[("table_name", "Adobe_images"), ("local_key", "other")],
            ),
            true,
        )?;
        assert!(!f.query(&query, None, 2)?.keys_complete);
        assert!(f.query(&query, first.next.as_ref(), 1)?.keys_complete);
        // A known table mismatch excludes the same unknown source for this query.
        assert!(
            f.query(
                &Lookup::EntitiesByLocalKey {
                    table_name: "Elsewhere".into(),
                    local_key: "key".into()
                },
                None,
                2
            )?
            .keys_complete
        );
        Ok(())
    }

    fn scoped_queries() -> Vec<Lookup> {
        vec![
            Lookup::RowsBySource("s".into()),
            Lookup::RowsByTable("t".into()),
            Lookup::EntitiesBySource("s".into()),
            Lookup::EntitiesByTable("t".into()),
            Lookup::EntitiesByLocalKey {
                table_name: "t".into(),
                local_key: "k".into(),
            },
            Lookup::References {
                source_id: "s".into(),
                field: None,
                target_table: None,
            },
            Lookup::References {
                source_id: "s".into(),
                field: Some("f".into()),
                target_table: None,
            },
            Lookup::References {
                source_id: "s".into(),
                field: None,
                target_table: Some("t".into()),
            },
            Lookup::References {
                source_id: "s".into(),
                field: Some("f".into()),
                target_table: Some("t".into()),
            },
            Lookup::ReferencesByTarget("t".into()),
            Lookup::ReferencesByTargetKey {
                target_table: "t".into(),
                target_key: "k".into(),
            },
            Lookup::PathsBySource("s".into()),
            Lookup::Packets {
                source_id: "s".into(),
                origin: None,
            },
            Lookup::Packets {
                source_id: "s".into(),
                origin: Some("o".into()),
            },
            Lookup::TableByName("n".into()),
        ]
    }

    #[test]
    fn availability_all_filter_shapes_match_possible_key_oracle_and_scope() -> Result<()> {
        let f = TestCatalog::new()?;
        // Installed production DDL; payload interpretation is tested separately.
        // Every filter can be known-equal, unknown, or a known mismatch.
        for query in scoped_queries() {
            let (collection, filters) = query.spec();
            for combination in 0..3usize.pow(filters.len() as u32) {
                let tx = f.catalog.db.unchecked_transaction()?;
                let r = f.record(collection, 1, &[]);
                let id = f.index(&r, true)?;
                let mut state = combination;
                let mut possible = true;
                for (field, value) in &filters {
                    let choice = state % 3;
                    state /= 3;
                    let value = match choice {
                        0 => Some(*value),
                        1 => None,
                        _ => {
                            possible = false;
                            Some("mismatch")
                        }
                    };
                    tx.execute(
                        &format!("UPDATE migration_record_lookup SET {field}=?1 WHERE record=?2"),
                        params![value, id],
                    )?;
                }
                let (sql, values) = availability_query(&f.input, &f.revision, &query, id);
                let result: bool =
                    tx.query_row(&sql, rusqlite::params_from_iter(values), |r| r.get(0))?;
                assert_eq!(result, possible, "{query:?} combination {combination}");
                for (input, revision, high) in [
                    ("other", f.revision.as_str(), id),
                    (f.input.as_str(), "other", id),
                    (f.input.as_str(), f.revision.as_str(), id - 1),
                ] {
                    let (sql, values) = availability_query(input, revision, &query, high);
                    assert!(!tx.query_row::<bool, _, _>(
                        &sql,
                        rusqlite::params_from_iter(values),
                        |r| r.get(0)
                    )?);
                }
                tx.execute(
                    "UPDATE migration_retained_records SET complete=0 WHERE sequence=?1",
                    [id],
                )?;
                let (sql, values) = availability_query(&f.input, &f.revision, &query, id);
                assert!(!tx.query_row::<bool, _, _>(
                    &sql,
                    rusqlite::params_from_iter(values),
                    |r| r.get(0)
                )?);
                tx.rollback()?;
            }
        }
        Ok(())
    }

    #[test]
    fn availability_index_upgrade_is_atomic_and_reopens_existing_catalog() -> Result<()> {
        let f = TestCatalog::new()?;
        let path = f.catalog.root.clone();
        let r = f.record(
            Collection::Entities,
            1,
            &[("source_id", "s"), ("table_name", "t"), ("local_key", "k")],
        );
        let id = f.index(&r, true)?;
        let indexes = [
            "source",
            "table",
            "reference",
            "source_target",
            "target",
            "packet",
            "name",
        ];
        for name in indexes {
            f.catalog
                .db
                .execute_batch(&format!("DROP INDEX migration_unavailable_{name}"))?;
        }
        f.catalog.db.execute_batch(
            "PRAGMA user_version=8; CREATE TABLE migration_unavailable_reference(block_upgrade)",
        )?;
        drop(f.catalog);
        assert!(Catalog::open(&path).is_err());
        let db = Connection::open(path.join("catalog.sqlite3"))?;
        assert_eq!(
            db.query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0))?,
            8
        );
        assert_eq!(db.query_row::<i64,_,_>("SELECT count(*) FROM sqlite_schema WHERE type='index' AND name LIKE 'migration_unavailable_%'", [], |r| r.get(0))?, 0);
        db.execute_batch("DROP TABLE migration_unavailable_reference")?;
        drop(db);
        let catalog = Catalog::open(&path)?;
        assert_eq!(
            catalog
                .db
                .query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0))?,
            crate::CURRENT_SCHEMA_VERSION
        );
        assert_eq!(catalog.migration_lookup_record(id)?.rowid, r.rowid);
        assert!(
            catalog
                .migration_lookup(
                    &f.input,
                    &f.revision,
                    &Lookup::EntitiesBySource("s".into()),
                    None,
                    1
                )?
                .keys_complete
        );
        Ok(())
    }

    #[test]
    fn availability_seeks_do_not_visit_unrelated_unavailable_population() -> Result<()> {
        use rusqlite::StatementStatus;
        let f = TestCatalog::new()?;
        let record = f.record(
            Collection::Entities,
            1,
            &[("source_id", "ancillary"), ("table_name", "Other")],
        );
        let first = f.index(&record, true)?;
        let tx = f.catalog.db.unchecked_transaction()?;
        tx.execute("WITH RECURSIVE n(x) AS (VALUES(2) UNION ALL SELECT x+1 FROM n WHERE x<20000) INSERT INTO migration_retained_records(input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete) SELECT input,revision,collection,n.x,compressed,raw_length,digest,next_cursor,complete FROM migration_retained_records,n WHERE sequence=?1", [first])?;
        tx.execute("INSERT INTO migration_record_lookup(record,input,revision,collection,digest,raw_length,source_id,table_name,unavailable) SELECT sequence,input,revision,collection,digest,raw_length,'ancillary','Other',(SELECT unavailable FROM migration_record_lookup WHERE record=?1) FROM migration_retained_records WHERE sequence>?1", [first])?;
        tx.commit()?;
        for query in scoped_queries() {
            let (sql, values) = availability_query(&f.input, &f.revision, &query, i64::MAX);
            let plan = f
                .catalog
                .db
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
                .query_map(rusqlite::params_from_iter(values.clone()), |r| {
                    r.get::<_, String>(3)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert!(
                plan.iter()
                    .any(|p| p.contains("SEARCH l USING")
                        && p.contains(query.availability_index().0)),
                "{plan:?}"
            );
            assert!(
                !plan
                    .iter()
                    .any(|p| p.contains("SCAN l") || p.contains("TEMP B-TREE")),
                "{plan:?}"
            );
            let mut statement = f.catalog.db.prepare(&sql)?;
            assert!(
                !statement
                    .query_row(rusqlite::params_from_iter(values), |r| r.get::<_, bool>(0))?
            );
            assert!(
                statement.get_status(StatementStatus::VmStep) < 1000,
                "{query:?}: {}",
                statement.get_status(StatementStatus::VmStep)
            );
        }
        let mut old = f.catalog.db.prepare("SELECT EXISTS(SELECT 1 FROM migration_record_lookup l INDEXED BY migration_lookup_unavailable JOIN migration_retained_records r ON r.sequence=l.record AND r.complete=1 WHERE l.input=?1 AND l.revision=?2 AND l.collection=4 AND l.unavailable!='[]' AND l.record<=?3 AND (l.source_id IS NULL OR l.source_id='s'))")?;
        assert!(!old.query_row(params![f.input, f.revision, i64::MAX], |r| {
            r.get::<_, bool>(0)
        })?);
        assert!(old.get_status(StatementStatus::VmStep) > 100_000);
        Ok(())
    }

    #[test]
    fn unavailable_keys_remain_visible_without_lossy_interpretation() -> Result<()> {
        let f = TestCatalog::new()?;
        let values = [
            Field::Inline(Cell::Text(vec![b'x'; MAX_KEY + 1])),
            Field::Inline(Cell::Text(vec![0xff])),
            Field::Inline(Cell::Blob(b"s".to_vec())),
            Field::Inline(Cell::Integer(3)),
            Field::Bytes(crate::lightroom::migration_source::ByteRef {
                seal: f.input.clone(),
                revision: f.revision.clone(),
                collection: Collection::Rows,
                rowid: 5,
                field: "source_id".into(),
                bytes: 99999,
                text: true,
            }),
        ];
        for (i, value) in values.into_iter().enumerate() {
            let mut r = f.record(Collection::Rows, i as i64 + 1, &[("table_name", "table")]);
            r.fields.insert("source_id".into(), value);
            f.index(&r, true)?;
        }
        let r = f.record(Collection::Rows, 6, &[("table_name", "table")]);
        f.index(&r, true)?;
        let p = f.query(&Lookup::RowsBySource("s".into()), None, 100)?;
        assert!(p.records.is_empty());
        assert!(!p.keys_complete);
        let p = f.query(&Lookup::Unavailable(Collection::Rows), None, 100)?;
        assert_eq!(p.records.len(), 6);
        assert_eq!(
            p.records[0].unavailable[0].reason,
            UnavailableReason::Oversized
        );
        assert_eq!(
            p.records[1].unavailable[0].reason,
            UnavailableReason::NonUtf8
        );
        assert_eq!(
            p.records[4].unavailable[0].reason,
            UnavailableReason::ExternalBytes
        );
        assert_eq!(
            p.records[5].unavailable[0].reason,
            UnavailableReason::Missing
        );
        let retained = f.catalog.migration_lookup_record(2)?;
        assert!(matches!(&retained.fields["source_id"],Field::Inline(Cell::Text(v)) if v==&[0xff]));
        Ok(())
    }

    #[test]
    fn backfill_restarts_and_staged_rows_cannot_be_skipped() -> Result<()> {
        let mut f = TestCatalog::new()?;
        for i in 1..=9 {
            let r = f.record(Collection::Paths, i, &[("source_id", "same")]);
            f.store(&r, true)?;
        }
        let q = Lookup::PathsBySource("same".into());
        assert!(f.query(&q, None, 100).is_err());
        let p = f.catalog.step_migration_lookup(&f.input, 3)?;
        assert_eq!(p.cursor, 3);
        assert!(!p.complete);
        let root = f.catalog.root.clone();
        drop(f.catalog);
        f.catalog = Catalog::open(root)?;
        let p = f.catalog.step_migration_lookup(&f.input, 3)?;
        assert_eq!(p.cursor, 6);
        assert!(f.catalog.step_migration_lookup(&f.input, 3)?.complete);
        assert_eq!(f.query(&q, None, 100)?.records.len(), 9);
        let r = f.record(Collection::Paths, 10, &[("source_id", "same")]);
        let id = f.store(&r, false)?;
        assert!(!f.catalog.step_migration_lookup(&f.input, 100)?.complete);
        f.catalog.db.execute(
            "UPDATE migration_retention SET complete=0 WHERE id=?1",
            [&f.input],
        )?;
        index_record(&f.catalog.db, id, &PreparedIndex::new(&r)?)?;
        assert!(f.query(&q, None, 100).is_err());
        f.catalog.db.execute(
            "UPDATE migration_retained_records SET complete=1 WHERE sequence=?1",
            [id],
        )?;
        f.catalog.db.execute(
            "UPDATE migration_retention SET complete=1 WHERE id=?1",
            [&f.input],
        )?;
        assert_eq!(f.query(&q, None, 100)?.records.len(), 10);
        Ok(())
    }

    #[test]
    fn preparation_and_backfill_enforce_record_budget() -> Result<()> {
        let mut f = TestCatalog::new()?;
        let mut r = f.record(
            Collection::Rows,
            1,
            &[("source_id", "s"), ("table_name", "table")],
        );
        r.fields.insert(
            "cells_json".into(),
            Field::Inline(Cell::Text(vec![b'x'; 3 * 1024 * 1024 / 2])),
        );
        for i in 1..=3 {
            r.rowid = i;
            f.store(&r, true)?;
        }
        let p = f.catalog.step_migration_lookup(&f.input, 100)?;
        assert_eq!(p.indexed, 2);
        assert!(p.raw_bytes <= MAX_BYTES);
        assert!(!p.complete);
        assert!(f.catalog.step_migration_lookup(&f.input, 100)?.complete);
        r.fields = BTreeMap::from([(
            "cells_json".into(),
            Field::Inline(Cell::Text(vec![b'x'; MAX_BYTES + 1])),
        )]);
        assert!(PreparedIndex::new(&r).is_err());
        Ok(())
    }

    #[test]
    fn lookup_indexes_offer_key_searches_without_payload_scans() -> Result<()> {
        let f = TestCatalog::new()?;
        for (index, predicate) in [
            ("migration_lookup_source", "source_id='s'"),
            ("migration_lookup_table", "table_name='t'"),
            (
                "migration_lookup_field",
                "source_id='s' AND field='f' AND target_table='t'",
            ),
            (
                "migration_lookup_source_field",
                "source_id='s' AND field='f'",
            ),
            (
                "migration_lookup_source_target",
                "source_id='s' AND target_table='t'",
            ),
            ("migration_lookup_target", "target_table='t'"),
            ("migration_lookup_origin", "source_id='s' AND origin='o'"),
            ("migration_lookup_name", "name='n'"),
            (
                "migration_lookup_local_key",
                "table_name='t' AND local_key='k'",
            ),
            (
                "migration_lookup_target_key",
                "target_table='t' AND target_key='k'",
            ),
            ("migration_lookup_unavailable", "unavailable!='[]'"),
        ] {
            let sql = format!(
                "EXPLAIN QUERY PLAN SELECT record FROM migration_record_lookup INDEXED BY {index} WHERE input='i' AND revision='r' AND collection=3 AND {predicate} AND record>0 ORDER BY record LIMIT 100"
            );
            let detail: Vec<String> = f
                .catalog
                .db
                .prepare(&sql)?
                .query_map([], |r| r.get(3))?
                .collect::<rusqlite::Result<_>>()?;
            assert!(
                detail
                    .iter()
                    .any(|d| d.contains("SEARCH") && d.contains(index)),
                "{detail:?}"
            );
            assert!(
                !detail.iter().any(|d| d.contains("TEMP B-TREE")),
                "{detail:?}"
            );
        }
        Ok(())
    }
}
