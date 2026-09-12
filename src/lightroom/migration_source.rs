//! Read-only, selected evidence from a separately sealed inspection plan.
//!
//! This adapter never calls `Plan::open`, recovers SQLite, inspects originals, or
//! treats retained Adobe instructions as executable recipes. The caller owns the
//! seal and the exclusion of other writers for the entire adapter lifetime.

mod reader;
pub use reader::MigrationSource;

use super::plan::{Cell, RetainedRow};
pub use super::source::Revision as FileIdentity;
use crate::storage_volume::NativePath;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The digest of this complete document is also the cursor namespace. Approval
/// is supplied by the migration coordinator, not manufactured by opening a plan.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSeal {
    pub protocol: u32,
    pub database: NativePath,
    pub identity: FileIdentity,
    pub blake3: String,
    pub approval: SelectionApproval,
    pub selected: Vec<SelectedCapture>,
    pub excluded_revisions: Vec<String>,
    #[serde(default)]
    pub supplements: Vec<SupplementPin>,
}

/// Separately reviewed evidence, never an update to the historical plan status.
/// The coordinator retains/verifies the proof document and successor payloads.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplementPin {
    pub revision: String,
    pub source_id: String,
    pub origin: String,
    pub source_revision: crate::xmp_packets::SourceRevision,
    pub historical_status: crate::xmp_packets::Status,
    pub proof_blake3: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StableSource {
    pub capture_revision: String,
    pub table: String,
    /// Exact schema3 canonical Cell-array key encoding, available in chunks.
    pub source_key: Field,
    pub source_key_blake3: String,
    /// Retained for provenance only: this includes an inspection-local lineage.
    pub inspection_source_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionApproval {
    /// Exact external authorization bytes, retained by the coordinator.
    pub document_blake3: String,
    pub scope: String,
    /// Binds precisely the selected/excluded roster, independently of the DB.
    pub roster_blake3: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedCapture {
    pub revision: String,
    pub family: String,
    pub family_evidence_digest: String,
    /// Exact UTF-8 `captures.manifest` column bytes, not the newline-framed file.
    pub manifest_blake3: String,
    pub evidence_revision: i64,
}

/// These are per-operation bounds, not a claim about total import work.
#[derive(Clone, Copy, Debug)]
pub struct ReadLimits {
    pub page_bytes: usize,
    pub inline_bytes: usize,
    pub chunk_bytes: usize,
    pub vm_steps: u64,
    pub deadline_ms: u64,
    pub open_deadline_ms: u64,
}
impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            page_bytes: super::PAGE_BYTES,
            inline_bytes: 64 * 1024,
            chunk_bytes: 1024 * 1024,
            vm_steps: 10_000_000,
            deadline_ms: 10_000,
            open_deadline_ms: 600_000,
        }
    }
}
impl ReadLimits {
    fn validate(self) -> Result<()> {
        ensure!(
            (1024..=super::PAGE_BYTES).contains(&self.page_bytes),
            "page byte limit"
        );
        ensure!(
            (1..=self.page_bytes / 4).contains(&self.inline_bytes),
            "inline byte limit"
        );
        ensure!(
            (1..=1024 * 1024).contains(&self.chunk_bytes),
            "chunk byte limit"
        );
        ensure!(
            (1000..=1_000_000_000).contains(&self.vm_steps),
            "VM step limit"
        );
        ensure!(
            (1..=120_000).contains(&self.deadline_ms),
            "read deadline limit"
        );
        ensure!(
            (1..=3_600_000).contains(&self.open_deadline_ms),
            "seal deadline limit"
        );
        Ok(())
    }
}

/// A closed roster of schema3 collections; callers cannot supply SQL names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Collection {
    Captures,
    Rows,
    Entities,
    References,
    Paths,
    Packets,
    MetadataFacts,
    Issues,
    Tables,
    SchemaObjects,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub seal: String,
    pub revision: String,
    pub collection: Collection,
    pub after: Vec<Cell>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByteRef {
    pub seal: String,
    pub revision: String,
    pub collection: Collection,
    pub rowid: i64,
    pub field: String,
    pub bytes: u64,
    pub text: bool,
}

/// Text and BLOB bytes remain distinct, including invalid UTF-8 and embedded NUL.
/// Large fields are complete byte descriptors, not truncated previews.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Field {
    Inline(Cell),
    Bytes(ByteRef),
}
impl Field {
    pub fn text(&self) -> Result<&str> {
        match self {
            Self::Inline(Cell::Text(bytes)) => Ok(std::str::from_utf8(bytes)?),
            _ => anyhow::bail!("field is not inline UTF-8 text; use its byte descriptor"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub revision: String,
    pub collection: Collection,
    pub rowid: i64,
    pub key: Vec<Cell>,
    pub fields: BTreeMap<String, Field>,
}
impl EvidenceRecord {
    /// Materialize an ordinary bounded typed row. An oversized row remains
    /// accessible through `fields` and `read_chunk`, without lossy Cell changes.
    pub fn retained_row(&self, columns: Vec<String>, semantics: String) -> Result<RetainedRow> {
        ensure!(
            self.collection == Collection::Rows,
            "not a retained source row"
        );
        let text = |name: &str| self.fields.get(name).context("missing row field")?.text();
        let cells: Vec<Cell> = serde_json::from_str(text("cells_json")?)?;
        ensure!(
            cells.len() == columns.len(),
            "source row/column count differs"
        );
        Ok(RetainedRow {
            sequence: self.rowid,
            source_id: text("source_id")?.into(),
            revision_id: self.revision.clone(),
            table: text("table_name")?.into(),
            source_key: serde_json::from_str(text("key_json")?)?,
            columns,
            cells,
            semantics,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page {
    pub records: Vec<EvidenceRecord>,
    pub next: Option<Cursor>,
    /// True only after the database query reaches the end, not just a short page.
    pub exhausted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    Missing,
    Unique(String),
    /// At least two exact targets; no arbitrary first target is selected.
    Ambiguous,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageLinks {
    pub image_source_id: String,
    pub file: Resolution,
    pub master: Resolution,
    pub current_develop: Resolution,
    /// Missing does not distinguish a genuine master sentinel from an unknown
    /// schema column. Preserve the raw image row for that decision.
    pub limitations: String,
}

#[cfg(test)]
pub(crate) mod tests;
