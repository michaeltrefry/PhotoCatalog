//! Bounded saved-operation inspection on an already admitted SQL connection.
//! These reads never open an inspection, artifact, original, or destination path.
//! Lists are live keyset pages, not a cross-request snapshot: explicit refresh
//! discovers newly inserted earlier IDs. Documents return their exact bytes and
//! digest; execution must independently revalidate its reviewed authority.
use crate::{
    lightroom::bounded_json,
    lightroom::control::{Control, SqlControl},
};
use anyhow::{Context, Result, ensure};
use flate2::read::ZlibDecoder;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    sync::{Arc, atomic::AtomicBool},
};

pub const DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub rows: usize,
    pub page_bytes: usize,
    pub document_bytes: usize,
    pub vm_steps: u64,
    pub deadline_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            rows: 100,
            page_bytes: 96 * 1024,
            document_bytes: DOCUMENT_BYTES,
            vm_steps: 1_000_000,
            deadline_ms: 10_000,
        }
    }
}
impl Limits {
    fn control(self, cancel: Arc<AtomicBool>) -> Result<Control> {
        ensure!(
            (1..=1000).contains(&self.rows),
            "saved operation page row limit"
        );
        ensure!(
            (1024..=128 * 1024).contains(&self.page_bytes),
            "saved operation page byte limit"
        );
        ensure!(
            (1..=DOCUMENT_BYTES).contains(&self.document_bytes),
            "saved operation document byte limit"
        );
        // A stored run/repair row has more than one bounded document. SQLite's
        // row ceiling must not reject valid pairs before the SQL CASE guard.
        Control::new(
            cancel,
            self.vm_steps,
            self.deadline_ms,
            4 * DOCUMENT_BYTES,
            self.document_bytes.max(self.page_bytes).max(1024),
        )
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Kind {
    Run,
    CurrentRepair,
    KeywordRepair,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    pub id: String,
    /// Input binding for a run; parent run identity for either repair.
    pub owner: String,
    pub progress_bytes: u64,
    pub policy_or_binding_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page {
    pub kind: Kind,
    pub rows: Vec<Header>,
    pub next: Option<String>,
}
fn id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|v| v.is_ascii_hexdigit()),
        "saved operation identity must be a digest"
    );
    Ok(())
}
fn table(kind: Kind) -> &'static str {
    match kind {
        Kind::Run => "migration_runs",
        Kind::CurrentRepair => "migration_current_repairs",
        Kind::KeywordRepair => "migration_keyword_repairs",
    }
}
/// No COUNT, OFFSET, JSON extraction or full-plan progress scan. The primary
/// key seek visits at most the admitted rows plus one lookahead row.
pub fn list(
    db: &Connection,
    kind: Kind,
    after: Option<&str>,
    limits: Limits,
    cancel: Arc<AtomicBool>,
) -> Result<Page> {
    if let Some(after) = after {
        id(after)?;
    }
    let control = limits.control(cancel)?;
    control.check()?;
    let _sql = SqlControl::new(db, control.clone());
    let (owner, document) = if kind == Kind::Run {
        ("input", "policy")
    } else {
        ("run", "binding")
    };
    let query = format!(
        "SELECT CASE WHEN length(CAST(id AS BLOB))=64 THEN id END, CASE WHEN length(CAST({owner} AS BLOB))=64 THEN {owner} END, length(CAST(progress AS BLOB)), length(CAST({document} AS BLOB)) FROM {} WHERE id>?1 ORDER BY id LIMIT ?2",
        table(kind)
    );
    let mut statement = db.prepare(&query)?;
    let mut cursor = statement.query(params![
        after.unwrap_or(""),
        i64::try_from(limits.rows + 1)?
    ])?;
    let mut result = Page {
        kind,
        rows: Vec::new(),
        next: None,
    };
    while let Some(row) = cursor.next()? {
        control.check()?;
        if result.rows.len() == limits.rows {
            result.next = result.rows.last().map(|r| r.id.clone());
            break;
        }
        let value = Header {
            id: row
                .get::<_, Option<String>>(0)?
                .context("saved operation ID byte bound")?,
            owner: row
                .get::<_, Option<String>>(1)?
                .context("saved operation owner byte bound")?,
            progress_bytes: u64::try_from(row.get::<_, i64>(2)?)?,
            policy_or_binding_bytes: u64::try_from(row.get::<_, i64>(3)?)?,
        };
        id(&value.id)?;
        id(&value.owner)?;
        result.rows.push(value);
        // Reserve a full cursor before testing the complete serialized page.
        result.next = result.rows.last().map(|r| r.id.clone());
        if bounded_json(&result, limits.page_bytes).is_err() {
            result.rows.pop();
            ensure!(
                !result.rows.is_empty(),
                "saved operation row exceeds page byte admission"
            );
            result.next = result.rows.last().map(|r| r.id.clone());
            return Ok(result);
        }
        result.next = None;
    }
    control.check()?;
    bounded_json(&result, limits.page_bytes)?;
    Ok(result)
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Document {
    Progress,
    Policy,
    Seal,
    Approval,
    Binding,
    OriginalProgress,
}
#[derive(Clone, Debug)]
pub struct ExactDocument {
    pub kind: Kind,
    pub id: String,
    pub document: Document,
    pub bytes: Vec<u8>,
    pub blake3: String,
}
/// Reads one exact authority/progress document. SQL admits both raw and stored
/// lengths before materializing a BLOB; decompression and hashing are checked
/// in 64 KiB increments. No JSON parsing or reserialization occurs here.
pub fn document(
    db: &Connection,
    kind: Kind,
    key: &str,
    document: Document,
    expected_blake3: Option<&str>,
    limits: Limits,
    cancel: Arc<AtomicBool>,
) -> Result<ExactDocument> {
    id(key)?;
    if let Some(expected) = expected_blake3 {
        id(expected)?;
    }
    let control = limits.control(cancel)?;
    control.check()?;
    let _sql = SqlControl::new(db, control.clone());
    let (table, column, join, compressed) = match (kind, document) {
        (_, Document::Progress) => (table(kind), "p.progress", "", false),
        (Kind::Run, Document::Policy) => (table(kind), "p.policy", "", false),
        (Kind::Run, Document::Seal) => (
            table(kind),
            "r.seal",
            " JOIN migration_retention r ON r.id=p.input",
            false,
        ),
        (Kind::Run, Document::Approval) => (
            table(kind),
            "r.approval",
            " JOIN migration_retention r ON r.id=p.input",
            false,
        ),
        (Kind::CurrentRepair | Kind::KeywordRepair, Document::Binding) => {
            (table(kind), "p.binding", "", false)
        }
        (Kind::CurrentRepair | Kind::KeywordRepair, Document::OriginalProgress) => {
            (table(kind), "p.original_progress", "", true)
        }
        _ => anyhow::bail!("document does not belong to this saved operation kind"),
    };
    let raw = if compressed {
        "p.original_length".to_owned()
    } else {
        format!("length(CAST({column} AS BLOB))")
    };
    let digest = if compressed {
        "CASE WHEN length(CAST(p.original_digest AS BLOB))=64 THEN p.original_digest END"
    } else {
        "NULL"
    };
    let query = format!(
        "SELECT {raw}, length(CAST({column} AS BLOB)), CASE WHEN {raw} BETWEEN 0 AND ?2 AND length(CAST({column} AS BLOB)) BETWEEN 0 AND ?3 THEN CAST({column} AS BLOB) END, {digest} FROM {table} p{join} WHERE p.id=?1"
    );
    let stored_max = limits.document_bytes + if compressed { 65536 } else { 0 };
    let (raw_len, stored_len, bytes, saved_digest): (i64, i64, Option<Vec<u8>>, Option<String>) =
        db.query_row(
            &query,
            params![
                key,
                i64::try_from(limits.document_bytes)?,
                i64::try_from(stored_max)?
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let raw_len = usize::try_from(raw_len)?;
    let stored_len = usize::try_from(stored_len)?;
    ensure!(
        raw_len <= limits.document_bytes && stored_len <= stored_max,
        "saved document exceeds raw/stored byte admission"
    );
    let stored = bytes.context("saved document failed SQL byte admission")?;
    ensure!(
        stored.len() == stored_len,
        "saved document stored length differs"
    );
    let bytes = if compressed {
        let mut decoder = ZlibDecoder::new(stored.as_slice());
        let mut out = Vec::with_capacity(raw_len);
        let mut buffer = [0u8; 64 * 1024];
        loop {
            control.check()?;
            let count = decoder.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            ensure!(
                count <= raw_len.saturating_sub(out.len()),
                "saved archive expands beyond declared length"
            );
            out.extend_from_slice(&buffer[..count]);
        }
        ensure!(
            out.len() == raw_len && decoder.total_in() == stored_len as u64,
            "saved archive length/trailing bytes differ"
        );
        out
    } else {
        stored
    };
    let mut hash = blake3::Hasher::new();
    for chunk in bytes.chunks(64 * 1024) {
        control.check()?;
        hash.update(chunk);
    }
    let hash = hash.finalize().to_hex().to_string();
    if compressed {
        ensure!(
            saved_digest.as_deref() == Some(hash.as_str()),
            "saved predecessor digest differs"
        );
    }
    if let Some(expected) = expected_blake3 {
        ensure!(hash == expected, "saved document changed; refresh review");
    }
    control.check()?;
    Ok(ExactDocument {
        kind,
        id: key.into(),
        document,
        bytes,
        blake3: hash,
    })
}
#[cfg(test)]
mod tests;
