//! Source-bound organization decisions, atomic with their destination mappings.
//!
//! The coordinator supplies reviewed values; this module never guesses values
//! from column names, executes smart-collection text, or walks source tables.
//! Every endpoint is a retained selected row and an existing durable mapping.
//! The 100-record/8-MiB limits bound source proofs and decisions, not the number
//! of internal SQLite rows refreshed by the existing per-image organization
//! engine. Original paths are never opened by this component.
use super::{originals::SourceKey, retention};
use crate::{
    Catalog,
    catalog_edits::VariantKey,
    catalog_images::{self, ImageMetadataIdentity},
    catalog_writer::Priority,
    lightroom::{
        migration_source::{Collection, EvidenceRecord, MigrationSource, Resolution},
        plan::Cell,
    },
    organization::{Flag, KeywordKind, Operation},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Write};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_RECORDS: usize = 100;
const DICTIONARY: &str = "dictionary";

mod candidates;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRecord {
    pub retained_record: i64,
    pub source: SourceKey,
}

/// Both retained sides of a numeric reference, plus the target's original row.
/// A live sealed adapter confirms uniqueness before the first native mutation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub reference_record: i64,
    pub target_entity_record: i64,
    pub field: String,
    pub target: SourceRecord,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum DictionaryDecision {
    Create,
    /// Name equality alone is never authorization to reuse a dictionary object.
    Reuse {
        native_id: String,
    },
    /// Exact complete native hierarchy with an explicit source-bound decision.
    ReuseExactHierarchy {
        native_id: String,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compatibility {
    Missing,
    Ambiguous,
    Cycle,
    SmartCollectionRetained,
    StackRetained,
    HistoryRetained,
    Unsupported,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedLink {
    pub field: String,
    pub target_table: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Decision {
    /// Selected raw keyword row with typed Null name and parent. This is a
    /// structural boundary, never a user keyword or a membership endpoint.
    KeywordBoundary {
        retained_table: i64,
    },
    Collection {
        name: String,
        /// None is an explicit reviewed root placement, not a missing-link guess.
        parent: Option<Link>,
        position: i64,
        decision: DictionaryDecision,
    },
    Keyword {
        name: String,
        keyword_kind: KeywordKind,
        parent: Option<Link>,
        decision: DictionaryDecision,
    },
    Synonym {
        keyword: Link,
        value: String,
    },
    /// Distinct source synonym sharing an already retained exact native value.
    ReuseSynonym {
        keyword: Link,
        value: String,
        reason: String,
    },
    CollectionMembership {
        image: Link,
        collection: Link,
        position: i64,
    },
    KeywordMembership {
        image: Link,
        keyword: Link,
    },
    Flag {
        value: Flag,
    },
    Rating {
        value: u8,
    },
    Label {
        value: String,
    },
    Retain {
        construct: String,
        compatibility: Compatibility,
        detail: String,
        /// Required for Missing/Ambiguous; checked against the sealed adapter.
        unresolved: Option<UnresolvedLink>,
    },
}
impl Decision {
    fn slot(&self) -> &str {
        match self {
            Self::Collection { .. } | Self::Keyword { .. } | Self::KeywordBoundary { .. } => {
                DICTIONARY
            }
            Self::Synonym { .. } | Self::ReuseSynonym { .. } => "synonym",
            Self::CollectionMembership { .. } => "collection_membership",
            Self::KeywordMembership { .. } => "keyword_membership",
            Self::Flag { .. } => "flag",
            Self::Rating { .. } => "rating",
            Self::Label { .. } => "label",
            Self::Retain { construct, .. } => construct,
        }
    }
    fn links(&self) -> Vec<&Link> {
        match self {
            Self::Collection { parent, .. } | Self::Keyword { parent, .. } => {
                parent.iter().collect()
            }
            Self::Synonym { keyword, .. } | Self::ReuseSynonym { keyword, .. } => vec![keyword],
            Self::CollectionMembership {
                image, collection, ..
            } => vec![image, collection],
            Self::KeywordMembership { image, keyword } => vec![image, keyword],
            _ => vec![],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    pub origin: SourceRecord,
    /// Stable import owner, not a random inspection lineage or run identifier.
    pub import_source: String,
    pub adapter_version: String,
    pub decision: Decision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum NativeTarget {
    /// No native keyword ID exists for this source-only hierarchy boundary.
    KeywordBoundary,
    Collection {
        id: String,
        revision: i64,
    },
    Keyword {
        id: i64,
    },
    Image {
        key: VariantKey,
        revision: i64,
    },
    /// A source-owned candidate, not a claim that the effective field or native
    /// keyword membership selected this value. Keyword models contain the terms
    /// processed so far, not a claim that the source membership walk is finished.
    MetadataCandidate {
        key: VariantKey,
        revision: i64,
        field: String,
        observation_id: i64,
        model_id: i64,
        conflicted: bool,
        value_retained_only: bool,
    },
    Synonym {
        keyword: i64,
    },
    Retained {
        compatibility: Compatibility,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionResult {
    pub source_identity: String,
    pub slot: String,
    /// Capture + exact typed source key + adapter + reviewed decision, excluding
    /// destination record numbers and random inspection lineage identifiers.
    pub input_digest: String,
    pub target: NativeTarget,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS migration_organization(
        source_identity TEXT NOT NULL, slot TEXT NOT NULL, owner TEXT NOT NULL,
        adapter TEXT NOT NULL, input_digest TEXT NOT NULL, result TEXT NOT NULL,
        retained_record INTEGER NOT NULL REFERENCES migration_retained_records(sequence),
        proof TEXT NOT NULL, PRIMARY KEY(source_identity,slot));",
    )?;
    Ok(())
}

struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_BYTES - self.0.len() {
            return Err(std::io::Error::other("organization decision exceeds 8 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encoded<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut out = Bounded(Vec::new());
    serde_json::to_writer(&mut out, value)?;
    Ok(out.0)
}
fn text(value: &str, maximum: usize) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum && !value.contains('\0'),
        "organization text bounds"
    );
    Ok(())
}
fn dictionary_kind(kind: KeywordKind) -> &'static str {
    match kind {
        KeywordKind::Flat => "flat",
        KeywordKind::Hierarchical => "hierarchical",
    }
}

/// These receipt numbers locate immutable evidence, but are not logical identity.
fn strip_receipt_numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for name in [
                "retained_record",
                "retained_table",
                "reference_record",
                "target_entity_record",
            ] {
                map.remove(name);
            }
            for child in map.values_mut() {
                strip_receipt_numbers(child);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                strip_receipt_numbers(child);
            }
        }
        _ => (),
    }
}
fn input_digest(request: &Projection) -> Result<String> {
    let mut value: serde_json::Value = serde_json::from_slice(&encoded(request)?)?;
    strip_receipt_numbers(&mut value);
    let mut hash = blake3::Hasher::new();
    hash.update(b"photocatalog-organization-projection-v1\0");
    hash.update(&encoded(&value)?);
    Ok(hash.finalize().to_hex().to_string())
}

struct Kept {
    record: EvidenceRecord,
    input: String,
    digest: String,
}
#[derive(Default)]
pub(crate) struct Evidence {
    records: BTreeMap<i64, Kept>,
    bytes: usize,
    record_limit: Option<usize>,
}
impl Evidence {
    /// Larger packet rosters retain the same cumulative 8 MiB descriptor limit.
    pub(crate) fn with_record_limit(limit: usize) -> Result<Self> {
        ensure!((1..=2052).contains(&limit), "evidence record ceiling");
        Ok(Self {
            record_limit: Some(limit),
            ..Self::default()
        })
    }
    pub(crate) fn record(&mut self, db: &Connection, sequence: i64) -> Result<EvidenceRecord> {
        Ok(self.load(db, sequence)?.record.clone())
    }
    pub(crate) fn same_input(&self, first: i64, second: i64) -> Result<()> {
        let a = self
            .records
            .get(&first)
            .context("first evidence record has not been validated")?;
        let b = self
            .records
            .get(&second)
            .context("second evidence record has not been validated")?;
        ensure!(a.input == b.input, "evidence input bindings differ");
        Ok(())
    }
    fn load(&mut self, db: &Connection, sequence: i64) -> Result<&Kept> {
        if !self.records.contains_key(&sequence) {
            ensure!(
                self.records.len() < self.record_limit.unwrap_or(MAX_RECORDS),
                "organization evidence record limit"
            );
            let (input, length, digest): (String, i64, String) = db.query_row(
                "SELECT input,raw_length,digest FROM migration_retained_records WHERE sequence=? AND complete=1",
                [sequence], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let length = usize::try_from(length)?;
            ensure!(
                length <= MAX_BYTES - self.bytes,
                "organization evidence exceeds 8 MiB"
            );
            let record = retention::selected_record(db, sequence)?;
            self.bytes += length;
            self.records.insert(
                sequence,
                Kept {
                    record,
                    input,
                    digest,
                },
            );
        }
        Ok(&self.records[&sequence])
    }
    pub(crate) fn source(&mut self, db: &Connection, reference: &SourceRecord) -> Result<String> {
        reference.source.identity()?;
        let record = self.record(db, reference.retained_record)?;
        ensure!(
            record.collection == Collection::Rows
                && record.revision == reference.source.capture_revision,
            "organization source is not matching selected raw row"
        );
        let table =
            retention::field_bytes(db, reference.retained_record, &record, "table_name", 1024)?;
        let key =
            retention::field_bytes(db, reference.retained_record, &record, "key_json", 65536)?;
        ensure!(
            table == reference.source.table.as_bytes()
                && serde_json::from_slice::<Vec<Cell>>(&key)? == reference.source.key,
            "organization source table/key differs"
        );
        Ok(String::from_utf8(retention::field_bytes(
            db,
            reference.retained_record,
            &record,
            "source_id",
            4096,
        )?)?)
    }
    pub(crate) fn recheck(&self, db: &Connection) -> Result<()> {
        for (sequence, kept) in &self.records {
            let actual: (String, String, bool) = db.query_row(
                "SELECT input,digest,complete FROM migration_retained_records WHERE sequence=?",
                [sequence],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            ensure!(
                actual == (kept.input.clone(), kept.digest.clone(), true),
                "retained organization evidence changed"
            );
        }
        Ok(())
    }
    fn proof(&self, request: &Projection) -> Result<String> {
        Ok(String::from_utf8(encoded(&serde_json::json!({
            "request":request,
            "retained": self.records.iter().map(|(id,k)|serde_json::json!({"record":id,"input":k.input,"digest":k.digest})).collect::<Vec<_>>()
        }))?)?)
    }
}

fn field(db: &Connection, sequence: i64, record: &EvidenceRecord, name: &str) -> Result<String> {
    Ok(String::from_utf8(retention::field_bytes(
        db, sequence, record, name, 65536,
    )?)?)
}

fn verify_link(
    db: &Connection,
    evidence: &mut Evidence,
    origin: &SourceRecord,
    link: &Link,
    source: Option<&MigrationSource>,
    live: bool,
) -> Result<()> {
    text(&link.field, 1024)?;
    let source_id = evidence.source(db, origin)?;
    let target_id = evidence.source(db, &link.target)?;
    let input = evidence.records[&origin.retained_record].input.clone();
    evidence.same_input(origin.retained_record, link.target.retained_record)?;
    ensure!(
        link.target.source.capture_revision == origin.source.capture_revision,
        "relationship endpoint belongs to another retained input/capture"
    );
    let reference = evidence.load(db, link.reference_record)?;
    ensure!(
        reference.input == input
            && reference.record.revision == origin.source.capture_revision
            && reference.record.collection == Collection::References,
        "not a retained source reference"
    );
    ensure!(
        field(db, link.reference_record, &reference.record, "source_id")? == source_id
            && field(db, link.reference_record, &reference.record, "field")? == link.field
            && field(db, link.reference_record, &reference.record, "target_table")?
                == link.target.source.table,
        "retained reference association differs"
    );
    let target_key = field(db, link.reference_record, &reference.record, "target_key")?;
    let target = evidence.load(db, link.target_entity_record)?;
    ensure!(
        target.input == input
            && target.record.revision == origin.source.capture_revision
            && target.record.collection == Collection::Entities,
        "not a retained target entity"
    );
    ensure!(
        field(db, link.target_entity_record, &target.record, "source_id")? == target_id
            && field(db, link.target_entity_record, &target.record, "table_name")?
                == link.target.source.table
            && field(db, link.target_entity_record, &target.record, "local_key")? == target_key,
        "retained entity does not match canonical numeric reference"
    );
    if live {
        let source = source
            .context("new relationship projection requires sealed source uniqueness proof")?;
        ensure!(
            source.binding_blake3() == input,
            "relationship source seal differs"
        );
        ensure!(
            matches!(source.resolve(&origin.source.capture_revision,&source_id,&link.field,&link.target.source.table)?, Resolution::Unique(id) if id==target_id),
            "relationship is missing or ambiguous; retain compatibility instead"
        );
    }
    Ok(())
}

/// Reusable selected-row/reference proof for the native image coordinator.
/// Source queries complete before its writer transaction; recheck the retained
/// Evidence inside that transaction before publishing the destination mapping.
pub(crate) fn verify_unique_link(
    db: &Connection,
    evidence: &mut Evidence,
    origin: &SourceRecord,
    link: &Link,
    source: &MigrationSource,
) -> Result<()> {
    verify_link(db, evidence, origin, link, Some(source), true)
}

fn existing(
    db: &Connection,
    request: &Projection,
    digest: &str,
) -> Result<Option<ProjectionResult>> {
    let identity = request.origin.source.identity()?;
    let value: Option<(String,String,String,String)> = db.query_row(
        "SELECT owner,adapter,input_digest,result FROM migration_organization WHERE source_identity=? AND slot=?",
        params![identity,request.decision.slot()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    value
        .map(|(owner, adapter, stored, result)| {
            ensure!(
                owner == request.import_source
                    && adapter == request.adapter_version
                    && stored == digest,
                "organization mapping decision changed; explicit reconciliation required"
            );
            ensure!(result.len() <= 65536, "stored organization result limit");
            Ok(serde_json::from_str(&result)?)
        })
        .transpose()
}
fn save(
    db: &Connection,
    request: &Projection,
    digest: &str,
    proof: &str,
    target: NativeTarget,
) -> Result<ProjectionResult> {
    let result = ProjectionResult {
        source_identity: request.origin.source.identity()?,
        slot: request.decision.slot().into(),
        input_digest: digest.into(),
        target,
    };
    db.execute(
        "INSERT INTO migration_organization VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            result.source_identity,
            result.slot,
            request.import_source,
            request.adapter_version,
            digest,
            serde_json::to_string(&result)?,
            request.origin.retained_record,
            proof
        ],
    )?;
    Ok(result)
}
fn mapped(db: &Connection, owner: &str, reference: &SourceRecord) -> Result<NativeTarget> {
    let result:String=db.query_row("SELECT result FROM migration_organization WHERE source_identity=? AND slot='dictionary' AND owner=?",params![reference.source.identity()?,owner],|r|r.get(0))?;
    ensure!(result.len() <= 65536, "dictionary mapping size limit");
    Ok(serde_json::from_str::<ProjectionResult>(&result)?.target)
}
fn image(db: &Connection, owner: &str, reference: &SourceRecord) -> Result<ImageMetadataIdentity> {
    ensure!(
        reference.source.table == "Adobe_images",
        "image endpoint requires Adobe_images"
    );
    let id:String=db.query_row("SELECT image_id FROM image_import_map WHERE import_source=? AND capture_revision=? AND source_table=? AND source_id=?",params![owner,reference.source.capture_revision,reference.source.table,reference.source.identity()?],|r|r.get(0))?;
    catalog_images::identity(db, &id)
}
fn collection(db: &Connection, owner: &str, reference: &SourceRecord) -> Result<String> {
    ensure!(
        reference.source.table == "AgLibraryCollection",
        "collection endpoint table differs"
    );
    match mapped(db, owner, reference)? {
        NativeTarget::Collection { id, .. } => Ok(id),
        _ => anyhow::bail!("endpoint is not a projected collection"),
    }
}
pub(crate) fn keyword_boundary_fields(fields: &BTreeMap<String, Cell>) -> bool {
    matches!(fields.get("name"), Some(Cell::Null))
        && matches!(fields.get("parent"), Some(Cell::Null))
}

/// A boundary contributes no name, while its original parent Link remains in
/// the child request and is verified by the normal selected-source proof path.
pub(crate) fn keyword_parent_path(
    db: &Connection,
    owner: &str,
    reference: &SourceRecord,
) -> Result<(KeywordKind, Vec<String>)> {
    ensure!(
        reference.source.table == "AgLibraryKeyword",
        "keyword parent table differs"
    );
    match mapped(db, owner, reference)? {
        NativeTarget::KeywordBoundary => Ok((KeywordKind::Hierarchical, vec![])),
        NativeTarget::Keyword { .. } => {
            let (_, kind, path) = keyword(db, owner, reference)?;
            Ok((kind, path))
        }
        _ => anyhow::bail!("parent is not a native keyword or proven boundary"),
    }
}

fn keyword(
    db: &Connection,
    owner: &str,
    reference: &SourceRecord,
) -> Result<(i64, KeywordKind, Vec<String>)> {
    ensure!(
        reference.source.table == "AgLibraryKeyword",
        "keyword endpoint table differs"
    );
    let NativeTarget::Keyword { id } = mapped(db, owner, reference)? else {
        anyhow::bail!("endpoint is not a projected keyword")
    };
    let (kind, path): (String, String) = db.query_row(
        "SELECT kind,path FROM organization_keywords WHERE id=?",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(path.len() <= 65536, "keyword path bounds");
    let kind = match kind.as_str() {
        "flat" => KeywordKind::Flat,
        "hierarchical" => KeywordKind::Hierarchical,
        _ => anyhow::bail!("unknown keyword kind"),
    };
    Ok((id, kind, serde_json::from_str(&path)?))
}

fn validate(request: &Projection) -> Result<()> {
    text(&request.import_source, 4096)?;
    text(&request.adapter_version, 128)?;
    text(request.decision.slot(), 128)?;
    request.origin.source.identity()?;
    match &request.decision {
        Decision::Keyword {
            decision: DictionaryDecision::ReuseExactHierarchy { reason, .. },
            ..
        }
        | Decision::ReuseSynonym { reason, .. } => text(reason, 4096)?,
        Decision::Collection {
            decision: DictionaryDecision::ReuseExactHierarchy { .. },
            ..
        } => anyhow::bail!("hierarchy reuse is keyword-only"),
        _ => (),
    }
    let required = match &request.decision {
        Decision::KeywordBoundary { .. } => Some("AgLibraryKeyword"),
        Decision::Collection { name, position, .. } => {
            text(name, 1024)?;
            ensure!(*position >= 0, "negative collection position");
            Some("AgLibraryCollection")
        }
        Decision::Keyword { name, .. } => {
            text(name, 1024)?;
            Some("AgLibraryKeyword")
        }
        Decision::Synonym { value, .. } | Decision::ReuseSynonym { value, .. } => {
            text(value, 1024)?;
            Some("AgLibraryKeywordSynonym")
        }
        Decision::CollectionMembership { position, .. } => {
            ensure!(*position >= 0, "negative membership position");
            Some("AgLibraryCollectionImage")
        }
        Decision::KeywordMembership { .. } => Some("AgLibraryKeywordImage"),
        Decision::Flag { .. } | Decision::Rating { .. } | Decision::Label { .. } => {
            Some("Adobe_images")
        }
        Decision::Retain {
            detail,
            compatibility,
            unresolved,
            ..
        } => {
            text(detail, 65536)?;
            ensure!(
                matches!(
                    compatibility,
                    Compatibility::Missing | Compatibility::Ambiguous
                ) == unresolved.is_some(),
                "unresolved relationship proof mismatch"
            );
            None
        }
    };
    if let Some(table) = required {
        ensure!(
            request.origin.source.table == table,
            "organization construct source table differs"
        );
    }
    if let Decision::Rating { value } = request.decision {
        ensure!(value <= 5, "rating must be 0..5");
    }
    if let Decision::Label { value } = &request.decision {
        ensure!(
            value.len() <= 1024 && !value.contains('\0'),
            "invalid label"
        );
    }
    let expected_fields: Vec<(&Link, &str)> = match &request.decision {
        Decision::Collection { parent, .. } | Decision::Keyword { parent, .. } => {
            parent.iter().map(|p| (p, "parent")).collect()
        }
        Decision::Synonym { keyword, .. } => vec![(keyword, "keyword")],
        Decision::CollectionMembership {
            image, collection, ..
        } => vec![(image, "image"), (collection, "collection")],
        Decision::KeywordMembership { image, keyword } => vec![(image, "image"), (keyword, "tag")],
        _ => vec![],
    };
    for (link, expected) in expected_fields {
        ensure!(
            link.field == expected,
            "organization relationship field differs"
        );
    }
    Ok(())
}

/// A fresh keyword projection, prepared without replacing an existing receipt.
/// Only the caller owning an exact predecessor CAS may remove that predecessor.
pub(crate) struct PreparedKeywordProjection {
    request: Projection,
    digest: String,
    evidence: Evidence,
    candidate: Option<(candidates::Candidate, SourceRecord, ImageMetadataIdentity)>,
    keyword_guard: Option<(SourceRecord, i64, KeywordKind, Vec<String>)>,
    parent_guard: Option<(SourceRecord, KeywordKind, Vec<String>)>,
}
impl PreparedKeywordProjection {
    pub(crate) fn require_source_parent_path(&self, expected: &[String]) -> Result<()> {
        ensure!(
            matches!(&self.parent_guard, Some((_, KeywordKind::Hierarchical, actual)) if actual == expected),
            "keyword parent snapshot differs from planned source path"
        );
        Ok(())
    }
}

fn keyword_preparable(decision: &Decision) -> bool {
    matches!(
        decision,
        Decision::KeywordBoundary { .. }
            | Decision::Keyword { .. }
            | Decision::KeywordMembership { .. }
    ) || matches!(decision, Decision::Retain { construct, unresolved: None, .. } if construct == "keyword_behavior")
}
pub(crate) fn commit_keyword_projection(
    tx: &rusqlite::Transaction<'_>,
    p: PreparedKeywordProjection,
) -> Result<ProjectionResult> {
    p.evidence.recheck(tx)?;
    ensure!(
        existing(tx, &p.request, &p.digest)?.is_none(),
        "keyword receipt changed before commit"
    );
    if let Some((reference, kind, path)) = &p.parent_guard {
        ensure!(
            keyword_parent_path(tx, &p.request.import_source, reference)? == (*kind, path.clone()),
            "keyword parent changed before commit"
        );
    }
    if let Some((reference, id, kind, path)) = &p.keyword_guard {
        ensure!(
            keyword(tx, &p.request.import_source, reference)? == (*id, *kind, path.clone()),
            "keyword endpoint changed before commit"
        );
    }
    let proof = p.evidence.proof(&p.request)?;
    let target = if let Some((candidate, reference, expected)) = p.candidate {
        catalog_images::require_image_metadata_identity(tx, &expected)?;
        ensure!(
            image(tx, &p.request.import_source, &reference)? == expected,
            "keyword image endpoint changed before commit"
        );
        candidate.commit(tx, &expected)?
    } else {
        apply(tx, &p.request, &proof)?
    };
    save(tx, &p.request, &p.digest, &proof, target)
}
impl Catalog {
    pub(crate) fn prepare_keyword_projection(
        &self,
        source: &MigrationSource,
        request: &Projection,
    ) -> Result<PreparedKeywordProjection> {
        ensure!(
            keyword_preparable(&request.decision),
            "unsupported keyword repair projection"
        );
        let digest = input_digest(request)?;
        validate(request)?;
        let mut evidence = Evidence::default();
        evidence.source(&self.db, &request.origin)?;
        ensure!(
            source.binding_blake3() == evidence.records[&request.origin.retained_record].input,
            "keyword projection input differs"
        );
        if let Decision::KeywordBoundary { retained_table } = request.decision {
            let fields =
                super::images::columns(&self.db, &mut evidence, &request.origin, retained_table)?;
            ensure!(
                keyword_boundary_fields(&fields),
                "keyword boundary requires typed Null name and parent"
            );
        }
        for link in request.decision.links() {
            verify_unique_link(&self.db, &mut evidence, &request.origin, link, source)?;
        }
        let parent_guard = if let Decision::Keyword {
            parent: Some(parent),
            ..
        } = &request.decision
        {
            let (kind, path) =
                keyword_parent_path(&self.db, &request.import_source, &parent.target)?;
            Some((parent.target.clone(), kind, path))
        } else {
            None
        };
        let mut keyword_guard = None;
        let candidate = if let Decision::KeywordMembership {
            image: reference,
            keyword: term,
        } = &request.decision
        {
            let (id, kind, path) = keyword(&self.db, &request.import_source, &term.target)?;
            keyword_guard = Some((term.target.clone(), id, kind, path.clone()));
            let expected = image(&self.db, &request.import_source, &reference.target)?;
            let prepared = candidates::prepare(
                self,
                Some(source),
                request,
                &reference.target,
                &expected,
                &Operation::AddKeyword { kind, path },
                &mut evidence,
            )?
            .context("keyword candidate was not prepared")?;
            Some((prepared, reference.target.clone(), expected))
        } else {
            None
        };
        Ok(PreparedKeywordProjection {
            request: request.clone(),
            digest,
            evidence,
            candidate,
            keyword_guard,
            parent_guard,
        })
    }
}

impl Catalog {
    /// One bounded source construct per call (at most 100 proof records/8 MiB).
    /// Identical replay needs no connected source and makes no native writes.
    pub fn project_migration_organization(
        &mut self,
        source: Option<&MigrationSource>,
        request: &Projection,
    ) -> Result<ProjectionResult> {
        // Bound serialization before preparing variable-size evidence or native work.
        let digest = input_digest(request)?;
        validate(request)?;
        let mut evidence = Evidence::default();
        let source_id = evidence.source(&self.db, &request.origin)?;
        let prior = existing(&self.db, request, &digest)?;
        if let Decision::KeywordBoundary { retained_table } = &request.decision {
            let fields =
                super::images::columns(&self.db, &mut evidence, &request.origin, *retained_table)?;
            ensure!(
                keyword_boundary_fields(&fields),
                "keyword boundary requires typed Null name and parent",
            );
            if prior.is_none() {
                ensure!(
                    source
                        .context("new keyword boundary requires sealed source")?
                        .binding_blake3()
                        == evidence.records[&request.origin.retained_record].input,
                    "keyword boundary source seal differs",
                );
            }
        }
        for link in request.decision.links() {
            if prior.is_none() {
                verify_unique_link(
                    &self.db,
                    &mut evidence,
                    &request.origin,
                    link,
                    source.context("new relationship projection requires sealed source")?,
                )?;
            } else {
                verify_link(&self.db, &mut evidence, &request.origin, link, None, false)?;
            }
        }
        if let Some(result) = prior {
            return Ok(result);
        }
        if keyword_preparable(&request.decision) && source.is_some() {
            let prepared = self.prepare_keyword_projection(
                source.context("new keyword projection requires sealed source")?,
                request,
            )?;
            let _permit = self.writers.enter(Priority::Background)?;
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(prior) = existing(&tx, request, &digest)? {
                return Ok(prior);
            }
            let result = commit_keyword_projection(&tx, prepared)?;
            tx.commit()?;
            return Ok(result);
        }
        if let Decision::Retain {
            compatibility,
            unresolved: Some(query),
            ..
        } = &request.decision
        {
            text(&query.field, 1024)?;
            text(&query.target_table, 1024)?;
            let source =
                source.context("unresolved relationship classification requires sealed source")?;
            ensure!(
                source.binding_blake3() == evidence.records[&request.origin.retained_record].input,
                "unresolved source seal differs"
            );
            let actual = source.resolve(
                &request.origin.source.capture_revision,
                &source_id,
                &query.field,
                &query.target_table,
            )?;
            ensure!(
                matches!(
                    (compatibility, actual),
                    (Compatibility::Missing, Resolution::Missing)
                        | (Compatibility::Ambiguous, Resolution::Ambiguous)
                ),
                "unresolved classification differs"
            );
        }
        let proof = evidence.proof(request)?;
        // Metadata-producing operations must use the native engine's own atomic
        // XMP edit transaction; its callback publishes our mapping before commit.
        let mut keyword_guard = None;
        let operation = match &request.decision {
            Decision::Flag { value } => Some((
                request.origin.clone(),
                Operation::Flag {
                    value: value.clone(),
                },
            )),
            Decision::Rating { value } => {
                Some((request.origin.clone(), Operation::Rating { value: *value }))
            }
            Decision::Label { value } => Some((
                request.origin.clone(),
                Operation::Label {
                    value: value.clone(),
                },
            )),
            Decision::KeywordMembership {
                image,
                keyword: term,
            } => {
                let (id, kind, path) = keyword(&self.db, &request.import_source, &term.target)?;
                keyword_guard = Some((term.target.clone(), id, kind, path.clone()));
                Some((image.target.clone(), Operation::AddKeyword { kind, path }))
            }
            _ => None,
        };
        if let Some((reference, operation)) = operation {
            let expected = image(&self.db, &request.import_source, &reference)?;
            if let Some(prepared) = candidates::prepare(
                self,
                source,
                request,
                &reference,
                &expected,
                &operation,
                &mut evidence,
            )? {
                let _permit = self.writers.enter(Priority::Background)?;
                let tx = self
                    .db
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                evidence.recheck(&tx)?;
                catalog_images::require_image_metadata_identity(&tx, &expected)?;
                ensure!(
                    image(&tx, &request.import_source, &reference)? == expected,
                    "durable image endpoint changed"
                );
                if let Some((reference, id, kind, path)) = &keyword_guard {
                    ensure!(
                        keyword(&tx, &request.import_source, reference)?
                            == (*id, *kind, path.clone()),
                        "durable keyword endpoint changed"
                    );
                }
                ensure!(
                    existing(&tx, request, &digest)?.is_none(),
                    "organization mapping advanced concurrently; retry"
                );
                let target = prepared.commit(&tx, &expected)?;
                let proof = evidence.proof(request)?;
                let result = save(&tx, request, &digest, &proof, target)?;
                tx.commit()?;
                return Ok(result);
            }
            let mut saved = None;
            self.organize_image_with_commit(
                &expected.key,
                expected.metadata_revision,
                operation,
                |db, revision| {
                    evidence.recheck(db)?;
                    let mut after = expected.clone();
                    after.metadata_revision = revision;
                    catalog_images::require_image_metadata_identity(db, &after)?;
                    ensure!(
                        image(db, &request.import_source, &reference)? == after,
                        "durable image endpoint changed"
                    );
                    if let Some((reference, id, kind, path)) = &keyword_guard {
                        ensure!(
                            keyword(db, &request.import_source, reference)?
                                == (*id, *kind, path.clone()),
                            "durable keyword endpoint changed"
                        );
                    }
                    ensure!(
                        existing(db, request, &digest)?.is_none(),
                        "organization mapping advanced concurrently; retry"
                    );
                    saved = Some(save(
                        db,
                        request,
                        &digest,
                        &proof,
                        NativeTarget::Image {
                            key: expected.key.clone(),
                            revision,
                        },
                    )?);
                    Ok(())
                },
            )?;
            return saved.context("organization engine did not commit projection");
        }
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        evidence.recheck(&tx)?;
        if let Some(prior) = existing(&tx, request, &digest)? {
            return Ok(prior);
        }
        let target = apply(&tx, request, &proof)?;
        let result = save(&tx, request, &digest, &proof, target)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn migration_organization_projection(
        &self,
        source: &SourceKey,
        slot: &str,
    ) -> Result<Option<ProjectionResult>> {
        text(slot, 128)?;
        let result: Option<String> = self
            .db
            .query_row(
                "SELECT result FROM migration_organization WHERE source_identity=? AND slot=?",
                params![source.identity()?, slot],
                |r| r.get(0),
            )
            .optional()?;
        result
            .map(|s| {
                ensure!(s.len() <= 65536, "organization result limit");
                Ok(serde_json::from_str(&s)?)
            })
            .transpose()
    }
}

fn apply(db: &Connection, request: &Projection, proof: &str) -> Result<NativeTarget> {
    let provenance = serde_json::json!({"source":request.origin.source,"adapter":request.adapter_version,"proof_blake3":blake3::hash(proof.as_bytes()).to_hex().to_string()});
    match &request.decision {
        Decision::KeywordBoundary { .. } => Ok(NativeTarget::KeywordBoundary),
        Decision::Collection {
            name,
            parent,
            position,
            decision,
        } => {
            let parent = parent
                .as_ref()
                .map(|p| collection(db, &request.import_source, &p.target))
                .transpose()?;
            let id = match decision {
                DictionaryDecision::Create => {
                    crate::organization::create_collection(db, name, &provenance)?
                }
                DictionaryDecision::Reuse { native_id }
                | DictionaryDecision::ReuseExactHierarchy { native_id, .. } => {
                    text(native_id, 1024)?;
                    let (actual,old_parent,old_position):(String,Option<String>,i64)=db.query_row("SELECT c.name,s.parent,COALESCE(s.position,0) FROM organization_collections c LEFT JOIN organization_collection_structure s ON s.collection=c.id WHERE c.id=?",[native_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                    ensure!(
                        actual == *name && old_parent == parent && old_position == *position,
                        "explicit reused collection differs"
                    );
                    native_id.clone()
                }
            };
            if matches!(decision, DictionaryDecision::Create) {
                catalog_images::organization::place_collection(
                    db,
                    &catalog_images::organization::CollectionPlacement {
                        collection: id.clone(),
                        parent,
                        position: *position,
                    },
                )?;
            }
            let revision = db.query_row(
                "SELECT revision FROM organization_collections WHERE id=?",
                [&id],
                |r| r.get(0),
            )?;
            Ok(NativeTarget::Collection { id, revision })
        }
        Decision::Keyword {
            name,
            keyword_kind,
            parent,
            decision,
        } => {
            let mut path = if let Some(parent) = parent {
                ensure!(
                    *keyword_kind == KeywordKind::Hierarchical,
                    "flat keyword cannot have a parent"
                );
                let (kind, path) = keyword_parent_path(db, &request.import_source, &parent.target)?;
                ensure!(kind == *keyword_kind, "keyword parent kind differs");
                path
            } else {
                vec![]
            };
            path.push(name.clone());
            let actual: Option<i64> = db
                .query_row(
                    "SELECT id FROM organization_keywords WHERE kind=? AND path=?",
                    params![
                        dictionary_kind(*keyword_kind),
                        serde_json::to_string(&path)?
                    ],
                    |r| r.get(0),
                )
                .optional()?;
            let id = match decision {
                DictionaryDecision::Create => {
                    ensure!(
                        actual.is_none(),
                        "keyword path already exists; explicit reuse decision required"
                    );
                    crate::organization::keyword(db, *keyword_kind, &path)?
                }
                DictionaryDecision::Reuse { native_id }
                | DictionaryDecision::ReuseExactHierarchy { native_id, .. } => {
                    text(native_id, 32)?;
                    let id = native_id.parse::<i64>()?;
                    ensure!(actual == Some(id), "explicit reused keyword path differs");
                    id
                }
            };
            Ok(NativeTarget::Keyword { id })
        }
        Decision::Synonym {
            keyword: term,
            value,
        } => {
            let (id, _, _) = keyword(db, &request.import_source, &term.target)?;
            let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM organization_keyword_synonyms WHERE keyword=? AND synonym=?)",params![id,value],|r|r.get(0))?;
            ensure!(
                !exists,
                "synonym already exists; explicit reconciliation required"
            );
            catalog_images::organization::add_keyword_synonym(db, id, value, &provenance)?;
            Ok(NativeTarget::Synonym { keyword: id })
        }
        Decision::ReuseSynonym {
            keyword: term,
            value,
            ..
        } => {
            let (id, _, _) = keyword(db, &request.import_source, &term.target)?;
            let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM organization_keyword_synonyms WHERE keyword=? AND synonym=?)",params![id,value],|r|r.get(0))?;
            ensure!(exists, "explicit reused synonym differs");
            Ok(NativeTarget::Synonym { keyword: id })
        }
        Decision::CollectionMembership {
            image: member,
            collection: group,
            position,
        } => {
            let image = image(db, &request.import_source, &member.target)?;
            let collection = collection(db, &request.import_source, &group.target)?;
            let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM organization_collection_members m JOIN catalog_images i ON i.sequence=m.sequence WHERE m.collection=? AND i.id=?)",params![collection,image.image_id],|r|r.get(0))?;
            ensure!(
                !exists,
                "membership already exists; explicit reconciliation required"
            );
            let revision = catalog_images::organization::set_image_collection_membership(
                db,
                &image,
                &collection,
                *position,
                &provenance,
            )?;
            Ok(NativeTarget::Image {
                key: image.key,
                revision,
            })
        }
        Decision::Retain { compatibility, .. } => Ok(NativeTarget::Retained {
            compatibility: compatibility.clone(),
        }),
        _ => anyhow::bail!("metadata operation requires native edit transaction"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_images::{ImageRole, ImportImageRequest};
    use crate::lightroom::migration_source::tests::Fixture;

    mod candidate_tests;

    struct Bed {
        _temp: tempfile::TempDir,
        catalog: Catalog,
        fixture: Fixture,
        source: MigrationSource,
        rows: BTreeMap<i64, SourceRecord>,
        entities: BTreeMap<i64, i64>,
        links: BTreeMap<(i64, String), i64>,
    }
    impl Bed {
        fn new(ambiguous: bool) -> Result<Self> {
            Self::with_incoming(ambiguous, 0)
        }
        fn with_incoming(ambiguous: bool, incoming: i64) -> Result<Self> {
            let mut fixture = Fixture::new();
            let revision = fixture.revision().to_owned();
            let specs = [
                (100, "AgLibraryCollection"),
                (101, "AgLibraryCollection"),
                (102, "AgLibraryCollection"),
                (103, "AgLibraryCollection"),
                (104, "AgLibraryCollection"),
                (200, "AgLibraryKeyword"),
                (201, "AgLibraryKeyword"),
                (202, "AgLibraryKeyword"),
                (203, "AgLibraryKeyword"),
                (204, "AgLibraryKeywordSynonym"),
                (205, "AgLibraryKeyword"),
                (300, "Adobe_images"),
                (301, "Adobe_images"),
                (400, "AgLibraryCollectionImage"),
                (401, "AgLibraryCollectionImage"),
                (500, "AgLibraryKeywordImage"),
                (501, "AgLibraryKeywordImage"),
                (502, "AgLibraryKeywordImage"),
                (503, "AgLibraryKeywordImage"),
                (504, "AgLibraryKeywordImage"),
                (505, "AgLibraryKeywordImage"),
                (600, "AgLibraryCollectionContent"),
                (601, "Adobe_imageDevelopHistoryStep"),
            ];
            let links = [
                (102, "parent", 100),
                (103, "parent", 101),
                (201, "parent", 200),
                (203, "parent", 202),
                (204, "keyword", 201),
                (400, "image", 300),
                (400, "collection", 102),
                (401, "image", 301),
                (401, "collection", 102),
                (500, "image", 300),
                (500, "tag", 201),
                (501, "image", 300),
                (501, "tag", 999),
                (502, "image", 300),
                (502, "tag", 203),
                (503, "image", 301),
                (503, "tag", 201),
                (504, "image", 300),
                (504, "tag", 201),
                (505, "image", 300),
                (505, "tag", 205),
            ];
            fixture.edit(|db|{
                for (id,table) in specs {
                    db.execute("INSERT OR IGNORE INTO tables(revision,name,columns_json,key_json,schema_json,category,expected,retained,state) VALUES(?1,?2,'[]','[]','{}','fixture',0,0,'complete')",params![revision,table]).unwrap();
                    db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,?2,?3,?4,'[]')",params![revision,format!("row-{id}"),table,serde_json::to_string(&vec![Cell::Integer(id)]).unwrap()]).unwrap();
                    db.execute("INSERT INTO entities VALUES(?1,?2,?3,?4,NULL,'{}')",params![revision,format!("row-{id}"),table,serde_json::to_string(&Cell::Integer(id)).unwrap()]).unwrap();
                }
                for (id,field,target) in links {
                    let table=specs.iter().find(|(i,_)|*i==target).map_or("AgLibraryKeyword",|(_,t)|*t);
                    db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,?2,?3,?4,?5)",params![revision,format!("row-{id}"),field,table,serde_json::to_string(&Cell::Integer(target)).unwrap()]).unwrap();
                }
                for i in 0..incoming {
                    for (id,table) in [(10_000+i,"AgLibraryKeywordImage"),(20_000+i,"Adobe_imageDevelopHistoryStep")] {
                        db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,?2,?3,?4,'[]')",params![revision,format!("row-{id}"),table,serde_json::to_string(&vec![Cell::Integer(id)]).unwrap()]).unwrap();
                        db.execute("INSERT INTO entities VALUES(?1,?2,?3,?4,NULL,'{}')",params![revision,format!("row-{id}"),table,serde_json::to_string(&Cell::Integer(id)).unwrap()]).unwrap();
                        db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,?2,'image','Adobe_images',?3)",params![revision,format!("row-{id}"),serde_json::to_string(&Cell::Integer(300)).unwrap()]).unwrap();
                    }
                }
                if ambiguous {
                    db.execute("INSERT INTO entities VALUES(?1,'duplicate-keyword','AgLibraryKeyword',?2,NULL,'{}')",params![revision,serde_json::to_string(&Cell::Integer(201)).unwrap()]).unwrap();
                }
                db.execute("UPDATE tables SET expected=(SELECT count(*) FROM rows r WHERE r.revision=tables.revision AND r.table_name=tables.name),retained=(SELECT count(*) FROM rows r WHERE r.revision=tables.revision AND r.table_name=tables.name)",[]).unwrap();
            });
            let approval = b"explicit selected-only organization fixture";
            fixture.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
            let source = fixture.open();
            let temp = tempfile::tempdir()?;
            let mut catalog = Catalog::open(temp.path().join("catalog"))?;
            install(&catalog.db)?;
            catalog.begin_migration_retention(&source, approval)?;
            for _ in 0..500 {
                if catalog.step_migration_retention(&source)?.complete {
                    break;
                }
            }
            ensure!(
                catalog
                    .migration_retention_progress(source.binding_blake3())?
                    .complete,
                "fixture retention incomplete"
            );
            let mut rows = BTreeMap::new();
            let mut entities = BTreeMap::new();
            let mut refs = BTreeMap::new();
            for collection in [
                Collection::Rows,
                Collection::Entities,
                Collection::References,
            ] {
                let mut after = 0;
                loop {
                    let records = catalog.retained_migration_records(
                        source.binding_blake3(),
                        &revision,
                        collection,
                        after,
                        100,
                    )?;
                    if records.is_empty() {
                        break;
                    }
                    for (sequence, record) in records {
                        after = sequence;
                        let sid = field(&catalog.db, sequence, &record, "source_id")?;
                        let Some(number) =
                            sid.strip_prefix("row-").and_then(|s| s.parse::<i64>().ok())
                        else {
                            continue;
                        };
                        match collection {
                            Collection::Rows => {
                                rows.insert(
                                    number,
                                    SourceRecord {
                                        retained_record: sequence,
                                        source: SourceKey {
                                            capture_revision: revision.clone(),
                                            table: field(
                                                &catalog.db,
                                                sequence,
                                                &record,
                                                "table_name",
                                            )?,
                                            key: vec![Cell::Integer(number)],
                                        },
                                    },
                                );
                            }
                            Collection::Entities => {
                                entities.insert(number, sequence);
                            }
                            Collection::References => {
                                refs.insert(
                                    (number, field(&catalog.db, sequence, &record, "field")?),
                                    sequence,
                                );
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
            Ok(Self {
                _temp: temp,
                catalog,
                fixture,
                source,
                rows,
                entities,
                links: refs,
            })
        }
        fn link(&self, from: i64, field: &str, target: i64) -> Link {
            Link {
                reference_record: self.links[&(from, field.into())],
                target_entity_record: self.entities[&target],
                field: field.into(),
                target: self.rows[&target].clone(),
            }
        }
        fn request(&self, id: i64, decision: Decision) -> Projection {
            Projection {
                origin: self.rows[&id].clone(),
                import_source: "lightroom".into(),
                adapter_version: "synthetic-v1".into(),
                decision,
            }
        }
        fn project(&mut self, id: i64, decision: Decision) -> Result<ProjectionResult> {
            let request = self.request(id, decision);
            self.catalog
                .project_migration_organization(Some(&self.source), &request)
        }
        fn collections(&mut self) -> Result<()> {
            for (id, name, parent) in [
                (100, "A", None),
                (101, "B", None),
                (102, "Same", Some(100)),
                (103, "Same", Some(101)),
            ] {
                let link = parent.map(|p| self.link(id, "parent", p));
                self.project(
                    id,
                    Decision::Collection {
                        name: name.into(),
                        parent: link,
                        position: id,
                        decision: DictionaryDecision::Create,
                    },
                )?;
            }
            Ok(())
        }
        fn keywords(&mut self) -> Result<()> {
            for (id, name, parent) in [
                (200, "Birds", None),
                (202, "Animals", None),
                (201, "Same", Some(200)),
                (203, "Same", Some(202)),
            ] {
                let link = parent.map(|p| self.link(id, "parent", p));
                self.project(
                    id,
                    Decision::Keyword {
                        name: name.into(),
                        keyword_kind: KeywordKind::Hierarchical,
                        parent: link,
                        decision: DictionaryDecision::Create,
                    },
                )?;
            }
            Ok(())
        }
        fn images(&mut self) -> Result<(VariantKey, VariantKey)> {
            self.catalog.db.execute("INSERT INTO assets(id,location,path_display,state) VALUES('offline-original',X'2F6D697373696E672E646E67','/missing.dng','pending')",[])?;
            let mut keys = vec![];
            for number in [300, 301] {
                let r = &self.rows[&number];
                let image = self.catalog.register_import_image(&ImportImageRequest {
                    import_source: "lightroom".into(),
                    capture_revision: r.source.capture_revision.clone(),
                    source_table: r.source.table.clone(),
                    source_id: r.source.identity()?,
                    input_digest: "fixture-native-mapping".into(),
                    adapter_version: "synthetic-v1".into(),
                    asset_id: "offline-original".into(),
                    claim_reserved_master: false,
                    role: if number == 300 {
                        ImageRole::Master
                    } else {
                        ImageRole::Virtual
                    },
                    master: keys.first().cloned(),
                    label: format!("source-{number}"),
                })?;
                keys.push(image.key);
            }
            Ok((keys[0].clone(), keys[1].clone()))
        }
    }

    #[test]
    fn dictionaries_hierarchy_synonyms_and_explicit_reuse_preserve_identity() -> Result<()> {
        let mut b = Bed::new(false)?;
        b.collections()?;
        b.keywords()?;
        let a = collection(&b.catalog.db, "lightroom", &b.rows[&102])?;
        let c = collection(&b.catalog.db, "lightroom", &b.rows[&103])?;
        assert_ne!(a, c);
        assert_ne!(
            b.catalog.collection_placement(&a)?.parent,
            b.catalog.collection_placement(&c)?.parent
        );
        let (k, _, path) = keyword(&b.catalog.db, "lightroom", &b.rows[&201])?;
        assert_eq!(path, vec!["Birds", "Same"]);
        assert_ne!(k, keyword(&b.catalog.db, "lightroom", &b.rows[&203])?.0);
        let synonym = Decision::Synonym {
            keyword: b.link(204, "keyword", 201),
            value: "Taxonomic alias".into(),
        };
        let request = b.request(204, synonym);
        let first = b
            .catalog
            .project_migration_organization(Some(&b.source), &request)?;
        assert_eq!(
            first,
            b.catalog.project_migration_organization(None, &request)?
        );
        assert_eq!(b.catalog.keyword_synonyms(k, "", 10)?.len(), 1);
        // Same-path dictionaries cannot be silently merged by the native engine.
        let duplicate = Decision::Keyword {
            name: "Birds".into(),
            keyword_kind: KeywordKind::Hierarchical,
            parent: None,
            decision: DictionaryDecision::Create,
        };
        assert!(b.project(205, duplicate).is_err());
        let existing = keyword(&b.catalog.db, "lightroom", &b.rows[&200])?.0;
        b.project(
            205,
            Decision::Keyword {
                name: "Birds".into(),
                keyword_kind: KeywordKind::Hierarchical,
                parent: None,
                decision: DictionaryDecision::Reuse {
                    native_id: existing.to_string(),
                },
            },
        )?;
        let root = collection(&b.catalog.db, "lightroom", &b.rows[&100])?;
        b.project(
            104,
            Decision::Collection {
                name: "A".into(),
                parent: None,
                position: 100,
                decision: DictionaryDecision::Reuse { native_id: root },
            },
        )?;
        Ok(())
    }

    #[test]
    fn logical_images_keep_flags_rating_labels_keywords_and_membership_independent() -> Result<()> {
        let mut b = Bed::new(false)?;
        b.collections()?;
        b.keywords()?;
        let (first, second) = b.images()?;
        for (id, flag, rating, label) in
            [(300, Flag::Pick, 4, "Red"), (301, Flag::Reject, 1, "Blue")]
        {
            b.project(id, Decision::Flag { value: flag })?;
            b.project(id, Decision::Rating { value: rating })?;
            b.project(
                id,
                Decision::Label {
                    value: label.into(),
                },
            )?;
        }
        for (row, image, position) in [(400, 300, 9), (401, 301, 2)] {
            b.project(
                row,
                Decision::CollectionMembership {
                    image: b.link(row, "image", image),
                    collection: b.link(row, "collection", 102),
                    position,
                },
            )?;
        }
        b.project(
            500,
            Decision::KeywordMembership {
                image: b.link(500, "image", 300),
                keyword: b.link(500, "tag", 201),
            },
        )?;
        let members = b.catalog.image_collection_members(
            &collection(&b.catalog.db, "lightroom", &b.rows[&102])?,
            None,
            10,
        )?;
        assert_eq!(
            members
                .iter()
                .map(|(_, m)| (m.position, m.key.clone()))
                .collect::<Vec<_>>(),
            vec![(2, second.clone()), (9, first.clone())]
        );
        let flag = |key: &VariantKey| -> Result<String> {
            Ok(b.catalog.db.query_row("SELECT f.flag FROM organization_flags f JOIN catalog_images i ON i.sequence=f.sequence WHERE i.asset_id=? AND i.variant_id=?",params![key.asset_id,key.variant_id],|r|r.get(0))?)
        };
        assert_eq!(flag(&first)?, "pick");
        assert_eq!(flag(&second)?, "reject");
        for (key, rating, label) in [(&first, "4", "Red"), (&second, "1", "Blue")] {
            let view = b.catalog.metadata_for_image(key)?;
            let value = |name: &str| {
                view.fields
                    .iter()
                    .find(|f| f.name == name)
                    .and_then(|f| f.value.as_ref())
                    .map(|v| serde_json::to_string(v).unwrap())
            };
            assert!(value("rating").unwrap().contains(rating));
            assert!(value("label").unwrap().contains(label));
        }
        let count = |key: &VariantKey| -> Result<i64> {
            Ok(b.catalog.db.query_row("SELECT count(*) FROM organization_keyword_members m JOIN catalog_images i ON i.sequence=m.sequence WHERE i.asset_id=? AND i.variant_id=?",params![key.asset_id,key.variant_id],|r|r.get(0))?)
        };
        assert!(count(&first)? > 0);
        assert_eq!(count(&second)?, 0);
        let request = b.request(300, Decision::Rating { value: 4 });
        let before = b.catalog.image_metadata_identity(&first)?;
        b.catalog.project_migration_organization(None, &request)?;
        assert_eq!(before, b.catalog.image_metadata_identity(&first)?);
        assert!(!std::path::Path::new("/missing.dng").exists());
        let expected = b.fixture.seal.blake3.clone();
        drop(b.source);
        assert_eq!(
            blake3::hash(&std::fs::read(&b.fixture.path)?)
                .to_hex()
                .as_str(),
            expected
        );
        Ok(())
    }

    #[test]
    fn checkpoint_failure_rolls_back_native_changes_and_resume_does_not_duplicate() -> Result<()> {
        let mut b = Bed::new(false)?;
        let (image, _) = b.images()?;
        b.catalog.db.execute_batch("CREATE TRIGGER fail_projection BEFORE INSERT ON migration_organization BEGIN SELECT RAISE(ABORT,'injected projection checkpoint failure'); END;")?;
        let dictionary = b.request(
            100,
            Decision::Collection {
                name: "A".into(),
                parent: None,
                position: 0,
                decision: DictionaryDecision::Create,
            },
        );
        let flag = b.request(300, Decision::Flag { value: Flag::Pick });
        let rating = b.request(300, Decision::Rating { value: 3 });
        let before = b.catalog.image_metadata_identity(&image)?;
        for request in [&dictionary, &flag, &rating] {
            assert!(
                b.catalog
                    .project_migration_organization(Some(&b.source), request)
                    .is_err()
            );
        }
        assert_eq!(
            b.catalog
                .db
                .query_row("SELECT count(*) FROM organization_collections", [], |r| r
                    .get::<_, i64>(
                    0
                ))?,
            0
        );
        assert_eq!(before, b.catalog.image_metadata_identity(&image)?);
        assert_eq!(
            b.catalog
                .db
                .query_row("SELECT count(*) FROM migration_organization", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        b.catalog
            .db
            .execute_batch("DROP TRIGGER fail_projection;")?;
        let first = b
            .catalog
            .project_migration_organization(Some(&b.source), &dictionary)?;
        let native = b
            .catalog
            .project_migration_organization(Some(&b.source), &rating)?;
        let before = b.catalog.image_metadata_identity(&image)?;
        drop(b.catalog);
        let mut catalog = Catalog::open(b._temp.path().join("catalog"))?;
        assert_eq!(
            first,
            catalog.project_migration_organization(None, &dictionary)?
        );
        assert_eq!(
            native,
            catalog.project_migration_organization(None, &rating)?
        );
        assert_eq!(before, catalog.image_metadata_identity(&image)?);
        let mut changed = rating.clone();
        changed.decision = Decision::Rating { value: 5 };
        assert!(
            catalog
                .project_migration_organization(None, &changed)
                .is_err()
        );
        changed = rating.clone();
        changed.import_source = "another-owner".into();
        assert!(
            catalog
                .project_migration_organization(None, &changed)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn absent_ambiguous_and_unsupported_constructs_are_explicit_retained_results() -> Result<()> {
        let mut b = Bed::new(true)?;
        for (id, compatibility) in [
            (501, Compatibility::Missing),
            (500, Compatibility::Ambiguous),
        ] {
            let request = b.request(
                id,
                Decision::Retain {
                    construct: "keyword_membership".into(),
                    compatibility: compatibility.clone(),
                    detail: "unresolved exact tag link".into(),
                    unresolved: Some(UnresolvedLink {
                        field: "tag".into(),
                        target_table: "AgLibraryKeyword".into(),
                    }),
                },
            );
            assert_eq!(
                b.catalog
                    .project_migration_organization(Some(&b.source), &request)?
                    .target,
                NativeTarget::Retained { compatibility }
            );
            b.catalog.project_migration_organization(None, &request)?;
        }
        for (id, compatibility, construct) in [
            (
                600,
                Compatibility::SmartCollectionRetained,
                "smart_collection",
            ),
            (601, Compatibility::HistoryRetained, "history"),
            (100, Compatibility::Cycle, "hierarchy"),
            (300, Compatibility::StackRetained, "stack"),
        ] {
            b.project(
                id,
                Decision::Retain {
                    construct: construct.into(),
                    compatibility,
                    detail: "reviewed unsupported instructions remain in full retained row".into(),
                    unresolved: None,
                },
            )?;
        }
        let wrong = b.request(
            501,
            Decision::Retain {
                construct: "other".into(),
                compatibility: Compatibility::Ambiguous,
                detail: "wrong classification".into(),
                unresolved: Some(UnresolvedLink {
                    field: "tag".into(),
                    target_table: "AgLibraryKeyword".into(),
                }),
            },
        );
        assert!(
            b.catalog
                .project_migration_organization(Some(&b.source), &wrong)
                .is_err()
        );
        assert_eq!(
            b.catalog
                .db
                .query_row("SELECT count(*) FROM organization_collections", [], |r| r
                    .get::<_, i64>(
                    0
                ))?,
            0
        );
        Ok(())
    }

    #[test]
    fn matching_retained_reference_cannot_bypass_actual_target_ambiguity() -> Result<()> {
        let mut b = Bed::new(true)?;
        b.keywords()?;
        let (image, _) = b.images()?;
        let before = b.catalog.image_metadata_identity(&image)?;
        let request = b.request(
            500,
            Decision::KeywordMembership {
                image: b.link(500, "image", 300),
                keyword: b.link(500, "tag", 201),
            },
        );
        assert!(
            b.catalog
                .project_migration_organization(Some(&b.source), &request)
                .is_err()
        );
        assert_eq!(before, b.catalog.image_metadata_identity(&image)?);
        assert!(
            b.catalog
                .migration_organization_projection(&request.origin.source, "keyword_membership")?
                .is_none()
        );
        assert_eq!(
            b.catalog.db.query_row(
                "SELECT count(*) FROM organization_keyword_members",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    }

    #[test]
    fn wrong_source_link_scope_keys_bounds_and_changed_decisions_fail_closed() -> Result<()> {
        let mut b = Bed::new(false)?;
        b.collections()?;
        b.images()?;
        let request = b.request(
            400,
            Decision::CollectionMembership {
                image: b.link(400, "image", 300),
                collection: b.link(400, "collection", 102),
                position: 0,
            },
        );
        let mut wrong = request.clone();
        wrong.origin.source.capture_revision = b.fixture.seal.excluded_revisions[0].clone();
        assert!(
            b.catalog
                .project_migration_organization(Some(&b.source), &wrong)
                .is_err()
        );
        wrong = request.clone();
        wrong.origin.source.key = vec![Cell::Integer(401)];
        assert!(
            b.catalog
                .project_migration_organization(Some(&b.source), &wrong)
                .is_err()
        );
        wrong = request.clone();
        if let Decision::CollectionMembership { image, .. } = &mut wrong.decision {
            image.target_entity_record = b.entities[&301];
        }
        assert!(
            b.catalog
                .project_migration_organization(Some(&b.source), &wrong)
                .is_err()
        );
        assert!(
            b.catalog
                .project_migration_organization(None, &request)
                .is_err()
        );
        wrong = request.clone();
        if let Decision::CollectionMembership { image, .. } = &mut wrong.decision {
            image.field = "rootFile".into();
        }
        assert!(
            b.catalog
                .project_migration_organization(Some(&b.source), &wrong)
                .is_err()
        );
        let huge = b.request(
            600,
            Decision::Retain {
                construct: "oversized".into(),
                compatibility: Compatibility::Unsupported,
                detail: "x".repeat(MAX_BYTES + 1),
                unresolved: None,
            },
        );
        assert!(
            b.catalog
                .project_migration_organization(Some(&b.source), &huge)
                .is_err()
        );
        let mut moved = request.clone();
        moved.origin.retained_record += 1;
        if let Decision::CollectionMembership {
            image, collection, ..
        } = &mut moved.decision
        {
            image.reference_record += 10;
            image.target.retained_record += 1;
            collection.target_entity_record += 10;
        }
        assert_eq!(input_digest(&request)?, input_digest(&moved)?);
        assert_eq!(
            b.catalog.db.query_row(
                "SELECT count(*) FROM migration_organization WHERE slot='collection_membership'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    }
    #[test]
    fn fresh_parentless_keyword_and_behavior_remain_available_offline() -> Result<()> {
        let mut b = Bed::new(false)?;
        let request = b.request(
            200,
            Decision::Keyword {
                name: "Offline".into(),
                keyword_kind: KeywordKind::Hierarchical,
                parent: None,
                decision: DictionaryDecision::Create,
            },
        );
        let result = b.catalog.project_migration_organization(None, &request)?;
        assert!(matches!(result.target, NativeTarget::Keyword { .. }));
        let behavior = b.request(
            200,
            Decision::Retain {
                construct: "keyword_behavior".into(),
                compatibility: Compatibility::Unsupported,
                detail: "Preserved source behavior".into(),
                unresolved: None,
            },
        );
        assert!(matches!(
            b.catalog
                .project_migration_organization(None, &behavior)?
                .target,
            NativeTarget::Retained { .. }
        ));
        assert_eq!(
            b.catalog.project_migration_organization(None, &request)?,
            result
        );
        Ok(())
    }
}
