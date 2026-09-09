//! Bounded indexed candidate streams and explicit stable read sessions.
use crate::{
    Catalog,
    organization::{self, Flag},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, params_from_iter, types::Value as SqlValue};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Sequence,
    Capture,
    Filename,
    Rating,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    Ascending,
    Descending,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Query {
    pub text: Option<String>,
    pub keyword: Option<i64>,
    pub keyword_direct: bool,
    pub folder: Option<i64>,
    pub folder_recursive: bool,
    pub collection: Option<String>,
    pub date_from: Option<String>,
    pub date_until: Option<String>,
    pub camera_make: Option<String>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub format: Option<String>,
    pub rating: Option<u8>,
    pub flag: Option<Flag>,
    pub label: Option<String>,
    pub only_conflicted: bool,
    pub sort: Sort,
    pub direction: Direction,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Key {
    Integer(i64),
    Text(String),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    pub version: u32,
    pub query_hash: String,
    pub epoch: i64,
    pub high_water: i64,
    pub sequence: i64,
    pub key: Key,
}
#[derive(Debug, Serialize)]
pub struct SearchRow {
    pub sequence: i64,
    pub asset_id: String,
    pub state: String,
    pub metadata_revision: i64,
    pub folder: Option<i64>,
    pub filename: String,
    pub capture: String,
    pub camera_make: String,
    pub camera: String,
    pub lens: String,
    pub format: String,
    /// None is unknown/conflicted; imported XMP -1 remains distinct from 0 stars.
    pub rating: Option<i64>,
    pub flag: String,
    pub label: String,
    pub conflicts: Vec<String>,
    pub provenance: serde_json::Value,
}
#[derive(Debug, Serialize)]
pub struct Page {
    pub rows: Vec<SearchRow>,
    pub scanned: usize,
    pub has_more: bool,
    pub exhausted: bool,
    pub page_complete: bool,
    pub next: Option<Cursor>,
    pub vm_steps: i64,
    pub sorts: i64,
    pub text_work: TextWork,
    pub elapsed_ms: f64,
}
/// Admission for candidate-local text matching, independent of result/scan limits.
/// Limits account UTF-8 source bytes, not SQLite allocator or total process RSS.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextLimits {
    pub document_bytes: usize,
    pub page_bytes: usize,
}
impl Default for TextLimits {
    fn default() -> Self {
        Self {
            document_bytes: 1024 * 1024,
            page_bytes: 8 * 1024 * 1024,
        }
    }
}
impl TextLimits {
    fn validate(self) -> Result<()> {
        ensure!(
            self.document_bytes > 0
                && self.document_bytes <= self.page_bytes
                && self.page_bytes <= 64 * 1024 * 1024,
            "text admission requires 0 < document_bytes <= page_bytes <= 64 MiB"
        );
        Ok(())
    }
}
#[derive(Debug, Default, Serialize)]
pub struct TextWork {
    /// Includes a row inspected but left unconsumed at the byte-admission boundary.
    pub candidate_rows_read: usize,
    pub indexed_rows: usize,
    pub indexed_bytes: usize,
    pub batches: usize,
    /// TEMP setup/reset/insert/MATCH statements, including final reset. These do
    /// not count every instruction executed inside SQLite's FTS extension.
    pub vm_steps: i64,
    pub sorts: i64,
    pub admission_limited: bool,
}
const TEXT_BATCH_ROWS: usize = 128;
const LOCAL_TEXT: &str = "organization_candidate_text";
const RESET_TEXT: &str = "INSERT INTO temp.organization_candidate_text(organization_candidate_text) VALUES('delete-all')";

pub struct SearchSession {
    sender: Option<mpsc::SyncSender<SessionRequest>>,
    worker: Option<thread::JoinHandle<()>>,
}
struct SessionRequest {
    limit: usize,
    scan: usize,
    reply: mpsc::SyncSender<Result<Page>>,
}
static SNAPSHOTS: AtomicUsize = AtomicUsize::new(0);
struct SnapshotPermit;
impl Drop for SnapshotPermit {
    fn drop(&mut self) {
        SNAPSHOTS.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Sql {
    text: String,
    params: Vec<SqlValue>,
    local_text: bool,
}
fn bind(values: &mut Vec<SqlValue>, value: impl Into<SqlValue>) -> String {
    values.push(value.into());
    format!("?{}", values.len())
}
fn validate(query: &Query) -> Result<()> {
    for value in [
        &query.text,
        &query.date_from,
        &query.date_until,
        &query.camera_make,
        &query.camera,
        &query.lens,
        &query.format,
        &query.label,
        &query.collection,
    ]
    .into_iter()
    .flatten()
    {
        ensure!(
            value.len() <= 1024 && !value.contains('\0'),
            "search value exceeds limit or contains NUL"
        );
    }
    ensure!(
        query.rating.is_none_or(|v| v <= 5),
        "rating filter must be 0..5"
    );
    let mut dates = Vec::new();
    for value in [&query.date_from, &query.date_until].into_iter().flatten() {
        dates.push(
            organization::date_key(value)
                .context("date filter requires a valid photographic calendar date")?,
        );
    }
    ensure!(
        dates.len() != 2 || dates[0] < dates[1],
        "date interval must be nonempty (inclusive from, exclusive until)"
    );
    if let Some(text) = &query.text {
        ensure!(
            !text.trim().is_empty() && text.split_whitespace().count() <= 16,
            "text search requires 1..16 terms"
        );
    }
    Ok(())
}
fn ready(db: &Connection) -> Result<(i64, i64)> {
    let (epoch, after, high): (i64, i64, i64) = db.query_row(
        "SELECT epoch,backfill_after,backfill_high FROM organization_state WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(
        after >= high
            && !db.query_row("SELECT EXISTS(SELECT 1 FROM organization_dirty)", [], |r| r
                .get::<_, bool>(0))?,
        "organization index incomplete; resume organization-index batches"
    );
    let max = db.query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM organization_assets",
        [],
        |r| r.get(0),
    )?;
    Ok((epoch, max))
}
fn query_hash(query: &Query) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(query)?)
        .to_hex()
        .to_string())
}
fn fts_query(text: &str) -> String {
    text.split_whitespace()
        .map(|v| format!("\"{}\"*", v.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}
fn sql(query: &Query, cursor: Option<&Cursor>, high: i64, scan: usize) -> Result<Sql> {
    validate(query)?;
    let mut params = Vec::new();
    let mut predicates = Vec::new();
    for (name, value) in [
        ("camera_make", query.camera_make.as_ref()),
        ("camera", query.camera.as_ref()),
        ("lens", query.lens.as_ref()),
        ("format", query.format.as_ref()),
        ("label", query.label.as_ref()),
    ] {
        if let Some(value) = value {
            let value = if name == "format" {
                value.to_ascii_uppercase()
            } else {
                value.clone()
            };
            predicates.push(format!("a.{name}={}", bind(&mut params, value)));
        }
    }
    if let Some(value) = query.rating {
        predicates.push(format!("a.rating={}", bind(&mut params, i64::from(value))));
    }
    if let Some(value) = &query.flag {
        predicates.push(format!(
            "a.flag={}",
            bind(&mut params, value.sql().to_string())
        ));
    }
    if let Some(value) = query.folder {
        predicates.push(if query.folder_recursive{format!("EXISTS(SELECT 1 FROM organization_folder_members fm WHERE fm.folder={} AND fm.sequence=a.sequence)",bind(&mut params,value))}else{format!("a.folder={}",bind(&mut params,value))});
    }
    if let Some(value) = query.keyword {
        predicates.push(format!("EXISTS(SELECT 1 FROM organization_keyword_members km WHERE km.keyword={} AND km.sequence=a.sequence {})",bind(&mut params,value),if query.keyword_direct{"AND km.direct=1"}else{""}));
    }
    if let Some(value) = &query.collection {
        predicates.push(format!("EXISTS(SELECT 1 FROM organization_collection_members cm WHERE cm.collection={} AND cm.sequence=a.sequence)",bind(&mut params,value.clone())));
    }
    for (operator, value) in [(">=", &query.date_from), ("<", &query.date_until)] {
        if let Some(value) = value {
            predicates.push(format!(
                "a.capture!='' AND a.capture{operator}{}",
                bind(
                    &mut params,
                    organization::date_key(value).context("invalid date")?
                )
            ));
        }
    }
    if query.only_conflicted {
        predicates.push("a.conflicts!='[]'".into());
    }
    let mut source = "organization_assets a".to_string();
    let mut driving = Vec::new();
    let mut local_text = query.text.is_some();
    let mut sequence = "a.sequence";
    let mut key = match query.sort {
        Sort::Sequence => "a.sequence",
        Sort::Capture => "a.capture",
        Sort::Filename => "a.filename",
        Sort::Rating => "a.rating",
    };
    if query.sort == Sort::Sequence {
        if let Some(keyword) = query.keyword {
            source="organization_keyword_members d CROSS JOIN organization_assets a ON a.sequence=d.sequence".into();
            driving.push(format!("d.keyword={}", bind(&mut params, keyword)));
            sequence = "d.sequence";
            key = sequence;
        } else if let Some(folder) = query.folder.filter(|_| query.folder_recursive) {
            source="organization_folder_members d CROSS JOIN organization_assets a ON a.sequence=d.sequence".into();
            driving.push(format!("d.folder={}", bind(&mut params, folder)));
            sequence = "d.sequence";
            key = sequence;
        } else if let Some(collection) = &query.collection {
            source="organization_collection_members d CROSS JOIN organization_assets a ON a.sequence=d.sequence".into();
            driving.push(format!(
                "d.collection={}",
                bind(&mut params, collection.clone())
            ));
            sequence = "d.sequence";
            key = sequence;
        } else if let Some(text) = &query.text {
            // This ordered FTS stream has already applied the exact text query.
            local_text = false;
            source =
                "organization_text d CROSS JOIN organization_assets a ON a.sequence=d.rowid".into();
            driving.push(format!(
                "d.text MATCH {}",
                bind(&mut params, fts_query(text))
            ));
            sequence = "d.rowid";
            key = sequence;
        } else {
            let candidate = if let Some(v) = query.folder {
                Some(("folder", SqlValue::Integer(v)))
            } else if let Some(v) = query.rating {
                Some(("rating", SqlValue::Integer(i64::from(v))))
            } else if let Some(v) = &query.flag {
                Some(("flag", SqlValue::Text(v.sql().into())))
            } else if let Some(v) = &query.label {
                Some(("label", SqlValue::Text(v.clone())))
            } else if let Some(v) = &query.camera {
                Some(("camera", SqlValue::Text(v.clone())))
            } else if let Some(v) = &query.lens {
                Some(("lens", SqlValue::Text(v.clone())))
            } else {
                query
                    .format
                    .as_ref()
                    .map(|v| ("format", SqlValue::Text(v.to_ascii_uppercase())))
            };
            if let Some((name, value)) = candidate {
                source = if name == "rating" {
                    "organization_assets a INDEXED BY organization_sort_rating".into()
                } else {
                    format!("organization_assets a INDEXED BY organization_{name}_sequence")
                };
                driving.push(format!("a.{name}={}", bind(&mut params, value)));
            }
        }
    } else if query.sort == Sort::Capture {
        if let Some(folder) = query.folder.filter(|_| !query.folder_recursive) {
            source = "organization_assets a INDEXED BY organization_folder_capture".into();
            driving.push(format!("a.folder={}", bind(&mut params, folder)));
        } else if let Some(rating) = query.rating {
            source = "organization_assets a INDEXED BY organization_rating_capture".into();
            driving.push(format!("a.rating={}", bind(&mut params, i64::from(rating))));
        } else {
            source = "organization_assets a INDEXED BY organization_sort_capture".into();
        }
        for (operator, value) in [(">=", &query.date_from), ("<", &query.date_until)] {
            if let Some(value) = value {
                driving.push(format!(
                    "a.capture{operator}{}",
                    bind(
                        &mut params,
                        organization::date_key(value).context("invalid date")?
                    )
                ));
            }
        }
    } else {
        source = format!(
            "organization_assets a INDEXED BY organization_sort_{}",
            if query.sort == Sort::Filename {
                "filename"
            } else {
                "rating"
            }
        );
    }
    driving.push(format!("{sequence}<={}", bind(&mut params, high)));
    if let Some(cursor) = cursor {
        let op = if query.direction == Direction::Ascending {
            ">"
        } else {
            "<"
        };
        if query.sort == Sort::Sequence {
            driving.push(format!(
                "{sequence}{op}{}",
                bind(&mut params, cursor.sequence)
            ));
        } else {
            let value = match (&cursor.key, query.sort) {
                (Key::Integer(v), Sort::Rating) => SqlValue::Integer(*v),
                (Key::Text(v), Sort::Capture | Sort::Filename) => SqlValue::Text(v.clone()),
                _ => anyhow::bail!("cursor sort key has incompatible type"),
            };
            let k = bind(&mut params, value);
            let s = bind(&mut params, cursor.sequence);
            driving.push(format!("({key},{sequence}){op}({k},{s})"));
        }
    }
    let direction = if query.direction == Direction::Ascending {
        "ASC"
    } else {
        "DESC"
    };
    let matched = if predicates.is_empty() {
        "1".into()
    } else {
        predicates.join(" AND ")
    };
    let order = if key == sequence {
        format!("{key} {direction}")
    } else {
        format!("{key} {direction},{sequence} {direction}")
    };
    let text_column = if local_text { "a.search_text" } else { "NULL" };
    let limit = bind(&mut params, scan as i64);
    Ok(Sql {
        text: format!(
            "SELECT a.sequence,a.asset_id,a.state,a.metadata_revision,a.folder,a.filename,a.capture,a.camera_make,a.camera,a.lens,a.format,a.rating,a.flag,a.label,a.conflicts,a.provenance,({matched}) AS matched,{text_column} FROM {source} WHERE {} ORDER BY {order} LIMIT {limit}",
            driving.join(" AND ")
        ),
        params,
        local_text,
    })
}
fn text_execute(
    db: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    work: &mut TextWork,
) -> Result<()> {
    let mut statement = db.prepare(sql)?;
    statement.execute(params)?;
    work.vm_steps += i64::from(statement.get_status(rusqlite::StatementStatus::VmStep));
    work.sorts += i64::from(statement.get_status(rusqlite::StatementStatus::Sort));
    Ok(())
}
fn result_row(r: &rusqlite::Row<'_>) -> Result<SearchRow> {
    let rating: i64 = r.get(11)?;
    Ok(SearchRow {
        sequence: r.get(0)?,
        asset_id: r.get(1)?,
        state: r.get(2)?,
        metadata_revision: r.get(3)?,
        folder: r.get(4)?,
        filename: r.get(5)?,
        capture: r.get(6)?,
        camera_make: r.get(7)?,
        camera: r.get(8)?,
        lens: r.get(9)?,
        format: r.get(10)?,
        rating: (rating != -2).then_some(rating),
        flag: r.get(12)?,
        label: r.get(13)?,
        conflicts: serde_json::from_str(&r.get::<_, String>(14)?)?,
        provenance: serde_json::from_str(&r.get::<_, String>(15)?)?,
    })
}
fn page(
    db: &Connection,
    query: &Query,
    cursor: Option<&Cursor>,
    limit: usize,
    scan: usize,
    text_limits: TextLimits,
) -> Result<Page> {
    organization::page_limit(limit)?;
    text_limits.validate()?;
    ensure!(
        (limit..=4096).contains(&scan),
        "scan budget must be >= page limit and <=4096"
    );
    let started = Instant::now();
    let (epoch, max) = ready(db)?;
    let hash = query_hash(query)?;
    if let Some(c) = cursor {
        ensure!(
            c.version == 1 && c.query_hash == hash,
            "cursor belongs to a different query or version"
        );
        ensure!(
            c.epoch == epoch,
            "search cursor stale after edits, relinks or index changes; restart the query"
        );
        ensure!(c.high_water <= max, "cursor high-water exceeds catalog");
    }
    let high = cursor.map(|c| c.high_water).unwrap_or(max);
    let query_sql = sql(query, cursor, high, scan)?;
    let mut text_work = TextWork::default();
    let local_query = query
        .text
        .as_deref()
        .filter(|_| query_sql.local_text)
        .map(fts_query);
    if local_query.is_some() {
        // TEMP belongs to this connection, even when main is opened READ_ONLY.
        // No main schema/index writes and no catalog-sized posting set are made.
        text_execute(
            db,
            "CREATE VIRTUAL TABLE IF NOT EXISTS temp.organization_candidate_text USING fts5(text,content='',tokenize='unicode61')",
            [],
            &mut text_work,
        )?;
        text_execute(db, RESET_TEXT, [], &mut text_work)?;
    }
    let mut statement = db.prepare(&query_sql.text)?;
    let mut rows = statement.query(params_from_iter(query_sql.params))?;
    let mut output = Vec::new();
    let mut scanned = 0;
    let mut last = None;
    let mut exhausted = false;
    while scanned < scan && output.len() < limit && !text_work.admission_limited {
        // Never stage more rows than remaining output slots: every admitted row
        // is consumed in order before advancing the public cursor, even if all
        // rows match. No buffered matches or invisible prefetch survives a call.
        let capacity = TEXT_BATCH_ROWS
            .min(limit - output.len())
            .min(scan - scanned);
        let mut batch = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            let Some(r) = rows.next()? else {
                exhausted = true;
                break;
            };
            text_work.candidate_rows_read += 1;
            let matched = r.get::<_, bool>(16)?;
            if matched && local_query.is_some() {
                let text = r.get_ref(17)?.as_str()?;
                ensure!(
                    text.len() <= text_limits.document_bytes,
                    "candidate {} text requires {} bytes; document admission is {} bytes; retry with larger TextLimits",
                    r.get::<_, i64>(0)?,
                    text.len(),
                    text_limits.document_bytes
                );
                if text.len() > text_limits.page_bytes - text_work.indexed_bytes {
                    text_work.admission_limited = true;
                    break;
                }
                text_execute(
                    db,
                    "INSERT INTO temp.organization_candidate_text(rowid,text) VALUES(?1,?2)",
                    rusqlite::params![r.get::<_, i64>(0)?, text],
                    &mut text_work,
                )?;
                text_work.indexed_rows += 1;
                text_work.indexed_bytes += text.len();
            }
            scanned += 1;
            let sequence = r.get(0)?;
            let key = match query.sort {
                Sort::Sequence => Key::Integer(sequence),
                Sort::Capture => Key::Text(r.get(6)?),
                Sort::Filename => Key::Text(r.get(5)?),
                Sort::Rating => Key::Integer(r.get(11)?),
            };
            last = Some(Cursor {
                version: 1,
                query_hash: hash.clone(),
                epoch,
                high_water: high,
                sequence,
                key,
            });
            if matched {
                batch.push(result_row(r)?);
            }
        }
        if let Some(text) = &local_query {
            if !batch.is_empty() {
                text_work.batches += 1;
                let mut matcher = db.prepare(&format!(
                    "SELECT rowid FROM temp.{LOCAL_TEXT} WHERE {LOCAL_TEXT} MATCH ?1"
                ))?;
                let hits = matcher
                    .query_map([text], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()?;
                text_work.vm_steps +=
                    i64::from(matcher.get_status(rusqlite::StatementStatus::VmStep));
                text_work.sorts += i64::from(matcher.get_status(rusqlite::StatementStatus::Sort));
                output.extend(batch.into_iter().filter(|r| hits.contains(&r.sequence)));
            }
            text_execute(db, RESET_TEXT, [], &mut text_work)?;
        } else {
            output.extend(batch);
        }
        if exhausted {
            break;
        }
    }
    drop(rows);
    let vm_steps =
        i64::from(statement.get_status(rusqlite::StatementStatus::VmStep)) + text_work.vm_steps;
    let sorts = i64::from(statement.get_status(rusqlite::StatementStatus::Sort)) + text_work.sorts;
    Ok(Page {
        page_complete: output.len() == limit || exhausted,
        rows: output,
        scanned,
        has_more: !exhausted,
        exhausted,
        next: if exhausted { None } else { last },
        vm_steps,
        sorts,
        text_work,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}
impl Catalog {
    /// A single transaction makes each page internally consistent. A serialized
    /// cursor detects intervening writes rather than mixing incompatible orderings.
    pub fn search(
        &mut self,
        query: &Query,
        cursor: Option<&Cursor>,
        limit: usize,
        scan: usize,
    ) -> Result<Page> {
        self.search_with_text_limits(query, cursor, limit, scan, TextLimits::default())
    }
    pub fn search_with_text_limits(
        &mut self,
        query: &Query,
        cursor: Option<&Cursor>,
        limit: usize,
        scan: usize,
        text_limits: TextLimits,
    ) -> Result<Page> {
        let tx = self.db.transaction()?;
        let result = page(&tx, query, cursor, limit, scan, text_limits)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn search_session(&self, query: Query, lifetime_seconds: u64) -> Result<SearchSession> {
        self.search_session_with_text_limits(query, lifetime_seconds, TextLimits::default())
    }
    pub fn search_session_with_text_limits(
        &self,
        query: Query,
        lifetime_seconds: u64,
        text_limits: TextLimits,
    ) -> Result<SearchSession> {
        text_limits.validate()?;
        ensure!(
            (1..=300).contains(&lifetime_seconds),
            "snapshot lifetime must be 1..300 seconds"
        );
        validate(&query)?;
        SNAPSHOTS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| {
                anyhow::anyhow!(
                    "four browsing snapshots are already open; close or wait for expiry"
                )
            })?;
        let permit = SnapshotPermit;
        let path = self.root.join("catalog.sqlite3");
        let (sender, receiver) = mpsc::sync_channel::<SessionRequest>(1);
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("catalog-search-snapshot".into())
            .spawn(move || {
                let _permit = permit;
                let opened = (|| -> Result<Connection> {
                    let db = Connection::open_with_flags(
                        path,
                        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )?;
                    db.busy_timeout(Duration::from_secs(5))?;
                    db.pragma_update(None, "cache_size", -262144)?;
                    db.pragma_update(None, "mmap_size", 0)?;
                    db.pragma_update(None, "temp_store", 1)?;
                    db.execute_batch("BEGIN")?;
                    ready(&db)?;
                    Ok(db)
                })();
                let db = match opened {
                    Ok(db) => {
                        let _ = start_tx.send(Ok(()));
                        db
                    }
                    Err(error) => {
                        let _ = start_tx.send(Err(error));
                        return;
                    }
                };
                let started = Instant::now();
                let ttl = Duration::from_secs(lifetime_seconds);
                let mut next = None;
                while started.elapsed() < ttl {
                    let Ok(request) = receiver.recv_timeout(ttl.saturating_sub(started.elapsed()))
                    else {
                        break;
                    };
                    if started.elapsed() >= ttl {
                        let _ = request
                            .reply
                            .send(Err(anyhow::anyhow!("search snapshot expired")));
                        break;
                    }
                    let result = page(
                        &db,
                        &query,
                        next.as_ref(),
                        request.limit,
                        request.scan,
                        text_limits,
                    );
                    let exhausted = result.as_ref().is_ok_and(|p| p.exhausted);
                    if let Ok(result) = &result {
                        next = result.next.clone();
                    }
                    if request.reply.send(result).is_err() || exhausted {
                        break;
                    }
                }
                let _ = db.execute_batch("ROLLBACK");
            })?;
        start_rx
            .recv()
            .context("search snapshot worker stopped")??;
        Ok(SearchSession {
            sender: Some(sender),
            worker: Some(worker),
        })
    }
    pub fn explain_search(
        &self,
        query: &Query,
        cursor: Option<&Cursor>,
        scan: usize,
    ) -> Result<Vec<String>> {
        ensure!((1..=4096).contains(&scan), "invalid scan budget");
        let (_, high) = ready(&self.db)?;
        let sql = sql(query, cursor, high, scan)?;
        Ok(self
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {}", sql.text))?
            .query_map(params_from_iter(sql.params), |r| r.get::<_, String>(3))?
            .collect::<rusqlite::Result<_>>()?)
    }
}
impl SearchSession {
    pub fn next_page(&mut self, limit: usize, scan: usize) -> Result<Page> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .context("search snapshot closed")?
            .send(SessionRequest { limit, scan, reply })
            .map_err(|_| anyhow::anyhow!("search snapshot expired or exhausted; restart query"))?;
        receiver.recv().context("search snapshot worker stopped")?
    }
    pub fn close(mut self) -> Result<()> {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("search snapshot worker panicked"))?;
        }
        Ok(())
    }
}
impl Drop for SearchSession {
    fn drop(&mut self) {
        self.sender.take();
    }
}
