//! Restartable selected-catalog import. Every native mutation has an immutable
//! source receipt; advancing the walk separately is safe after interruption.
use super::{
    images,
    lookup::{Lookup, LookupCursor},
    organization::SourceRecord,
    originals::{OriginalDecision, OriginalRequest, SourceKey},
    retention,
    walk::{LinkResolution, Walk},
};
use crate::{
    Catalog,
    catalog_writer::Priority,
    lightroom::{
        migration_source::{Collection, Field, MigrationSource},
        plan::Cell,
    },
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

const ADAPTER: &str = "lightroom-selected-import-v2";
const LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum OverlapPolicy {
    RequireDecision,
    /// Explicit permission to share a physical asset at identical native path
    /// bytes. Each source image still gets its own logical image and edit state.
    ReuseExactPath {
        reason: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum KeywordOverlap {
    RequireDecision,
    ReuseExactHierarchy { reason: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactInput {
    pub capture_revision: String,
    pub member_index: usize,
    pub mapping: super::artifacts::ArtifactMapping,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplementInput {
    pub capture_revision: String,
    pub source_id: String,
    pub origin: super::file_metadata::Origin,
    pub evidence: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub import_source: String,
    pub overlap: OverlapPolicy,
    pub keyword_overlap: KeywordOverlap,
    pub artifacts: Vec<ArtifactInput>,
    #[serde(default)]
    pub supplements: Vec<SupplementInput>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Custody,
    SupplementCustody,
    Index,
    Files,
    Masters,
    VirtualCopies,
    FileEmbedded,
    FileSidecarXmp,
    FileSidecarUpper,
    FileAppendedXmp,
    FileAppendedUpper,
    CatalogXmp,
    CurrentDevelop,
    ImageFields,
    Keywords,
    Collections,
    KeywordSynonyms,
    KeywordMemberships,
    CollectionMemberships,
    History,
    DevelopSettings,
    BeforeSettings,
    Snapshots,
    SmartCollections,
    Stacks,
    ArtifactCustody,
    Reconciliation,
    Complete,
}
impl Stage {
    fn next(self) -> Self {
        match self {
            Self::Custody => Self::SupplementCustody,
            Self::SupplementCustody => Self::Index,
            Self::Index => Self::Files,
            Self::Files => Self::Masters,
            Self::Masters => Self::VirtualCopies,
            Self::VirtualCopies => Self::FileEmbedded,
            Self::FileEmbedded => Self::FileSidecarXmp,
            Self::FileSidecarXmp => Self::FileSidecarUpper,
            Self::FileSidecarUpper => Self::FileAppendedXmp,
            Self::FileAppendedXmp => Self::FileAppendedUpper,
            Self::FileAppendedUpper => Self::CatalogXmp,
            Self::CatalogXmp => Self::CurrentDevelop,
            Self::CurrentDevelop => Self::ImageFields,
            Self::ImageFields => Self::Keywords,
            Self::Keywords => Self::Collections,
            Self::Collections => Self::KeywordSynonyms,
            Self::KeywordSynonyms => Self::KeywordMemberships,
            Self::KeywordMemberships => Self::CollectionMemberships,
            Self::CollectionMemberships => Self::History,
            Self::History => Self::DevelopSettings,
            Self::DevelopSettings => Self::BeforeSettings,
            Self::BeforeSettings => Self::Snapshots,
            Self::Snapshots => Self::SmartCollections,
            Self::SmartCollections => Self::Stacks,
            Self::Stacks => Self::ArtifactCustody,
            Self::ArtifactCustody => Self::Reconciliation,
            Self::Reconciliation | Self::Complete => Self::Complete,
        }
    }
    pub(crate) fn table(self) -> Option<&'static str> {
        match self {
            Self::Files
            | Self::FileEmbedded
            | Self::FileSidecarXmp
            | Self::FileSidecarUpper
            | Self::FileAppendedXmp
            | Self::FileAppendedUpper => Some("AgLibraryFile"),
            Self::Masters
            | Self::VirtualCopies
            | Self::CurrentDevelop
            | Self::ImageFields
            | Self::Stacks => Some("Adobe_images"),
            Self::CatalogXmp => Some("Adobe_AdditionalMetadata"),
            Self::Keywords => Some("AgLibraryKeyword"),
            Self::Collections => Some("AgLibraryCollection"),
            Self::KeywordSynonyms => Some("AgLibraryKeywordSynonym"),
            Self::KeywordMemberships => Some("AgLibraryKeywordImage"),
            Self::CollectionMemberships => Some("AgLibraryCollectionImage"),
            Self::History => Some("Adobe_libraryImageDevelopHistoryStep"),
            Self::DevelopSettings => Some("Adobe_imageDevelopSettings"),
            Self::BeforeSettings => Some("Adobe_imageDevelopBeforeSettings"),
            Self::Snapshots => Some("Adobe_libraryImageDevelopSnapshot"),
            Self::SmartCollections => Some("AgLibraryCollectionContent"),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub id: String,
    pub input: String,
    pub stage: Stage,
    pub capture_index: usize,
    pub artifact_index: usize,
    pub cursor: Option<LookupCursor>,
    pub processed: u64,
    /// This coordinator's native stages are distinct from raw evidence custody.
    pub complete: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Outcome {
    Original {
        asset_id: String,
        created: bool,
    },
    Image(images::Outcome),
    Metadata {
        input_digest: String,
        state: String,
        observation: Option<i64>,
        reason: Option<String>,
    },
    FileMetadata(super::file_metadata::ProjectionResult),
    Organization(Vec<super::organization::ProjectionResult>),
    OtherImageRole,
    Retained {
        reason: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub progress: Progress,
    pub outcome: Option<Outcome>,
    /// The cursor remains on this row until an explicit decision is supplied.
    pub needs_decision: Option<String>,
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS migration_runs(
      id TEXT PRIMARY KEY,input TEXT NOT NULL REFERENCES migration_retention(id),
      policy BLOB NOT NULL,progress BLOB NOT NULL);
      CREATE TABLE IF NOT EXISTS migration_run_items(
      run TEXT NOT NULL REFERENCES migration_runs(id),stage TEXT NOT NULL,
      record INTEGER NOT NULL REFERENCES migration_retained_records(sequence),
      revision TEXT NOT NULL,
      outcome BLOB NOT NULL,PRIMARY KEY(run,stage,record));
      CREATE INDEX IF NOT EXISTS migration_run_capture ON migration_run_items(run,stage,revision);
      CREATE TABLE IF NOT EXISTS migration_run_supplements(
      run TEXT NOT NULL REFERENCES migration_runs(id),revision TEXT NOT NULL,source_id TEXT NOT NULL,
      origin TEXT NOT NULL,proof TEXT NOT NULL,evidence TEXT NOT NULL,semantic TEXT NOT NULL,
      PRIMARY KEY(run,revision,source_id,origin));",
    )?;
    Ok(())
}
fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    crate::lightroom::bounded_json(v, LIMIT)
}
pub(crate) fn read(db: &Connection, id: &str) -> Result<(Progress, Policy)> {
    ensure!(id.len() == 64, "migration run identity bounds");
    let (p, q): (Vec<u8>, Vec<u8>) = db.query_row(
        "SELECT progress,policy FROM migration_runs WHERE id=?",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        p.len() <= LIMIT && q.len() <= LIMIT,
        "migration state bounds"
    );
    let p: Progress = serde_json::from_slice(&p)?;
    ensure!(p.id == id, "migration state identity differs");
    let policy: Policy = serde_json::from_slice(&q)?;
    let expected = blake3::hash(&encode(&(ADAPTER, &p.input, &policy))?)
        .to_hex()
        .to_string();
    ensure!(
        expected == id,
        "legacy migration run requires a new v2 run; retained component receipts remain reusable"
    );
    Ok((p, policy))
}
pub(crate) fn advance(
    catalog: &mut Catalog,
    before: &Progress,
    after: &Progress,
    item: Option<(i64, &Outcome)>,
) -> Result<()> {
    let old = encode(before)?;
    let new = encode(after)?;
    let item = item
        .map(|(record, outcome)| Ok::<_, anyhow::Error>((record, encode(outcome)?)))
        .transpose()?;
    let stage = serde_json::to_string(&before.stage)?;
    let _permit = catalog.writers.enter(Priority::Background)?;
    let tx = catalog
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(
        tx.execute(
            "UPDATE migration_runs SET progress=?3 WHERE id=?1 AND progress=?2",
            params![before.id, old, new]
        )? == 1,
        "migration cursor changed; retry"
    );
    if let Some((record, outcome)) = item {
        ensure!(tx.execute(
            "INSERT INTO migration_run_items SELECT ?1,?2,?3,revision,?4 FROM migration_retained_records WHERE sequence=?3 AND input=?5 AND complete=1",
            params![before.id, stage, record, outcome,before.input],
        )?==1,"migration item custody changed before checkpoint");
    }
    tx.commit()?;
    Ok(())
}
fn admit_supplements(catalog: &Catalog, source: &MigrationSource, policy: &Policy) -> Result<()> {
    let pins = &source.seal().supplements;
    ensure!(
        pins.len() == policy.supplements.len(),
        "selected seal/policy supplement roster differs"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut remaining = 32 * 1024 * 1024u64;
    for pin in pins {
        let matches = policy
            .supplements
            .iter()
            .filter(|s| {
                s.capture_revision == pin.revision
                    && s.source_id == pin.source_id
                    && s.origin == super::file_metadata::Origin::Embedded
            })
            .collect::<Vec<_>>();
        ensure!(
            pin.origin == "embedded" && matches.len() == 1,
            "selected supplement policy member missing or duplicated"
        );
        let state = catalog.migration_evidence(&matches[0].evidence)?;
        ensure!(
            state.complete && state.length <= 8 * 1024 * 1024 && state.length <= remaining,
            "supplement admission document budget/state"
        );
        remaining -= state.length;
        ensure!(
            std::time::Instant::now() < deadline,
            "supplement admission deadline"
        );
        super::file_metadata::supplement_document(catalog, &matches[0].evidence, pin)?;
        ensure!(
            std::time::Instant::now() < deadline,
            "supplement admission deadline"
        );
    }
    Ok(())
}
fn supplement_step(
    catalog: &mut Catalog,
    source: &MigrationSource,
    before: &Progress,
    policy: &Policy,
) -> Result<Step> {
    let mut after = before.clone();
    let pins = &source.seal().supplements;
    if before.artifact_index == pins.len() {
        after.artifact_index = 0;
        after.stage = Stage::Index;
        advance(catalog, before, &after, None)?;
        return Ok(Step {
            progress: after,
            outcome: None,
            needs_decision: None,
        });
    }
    let pin = pins
        .get(before.artifact_index)
        .context("supplement custody cursor bounds")?;
    let member = policy
        .supplements
        .iter()
        .find(|s| {
            s.capture_revision == pin.revision
                && s.source_id == pin.source_id
                && s.origin == super::file_metadata::Origin::Embedded
        })
        .context("supplement policy member absent")?;
    let semantic = super::file_metadata::verify_supplement_custody(catalog, &member.evidence, pin)?;
    after.artifact_index += 1;
    let old = encode(before)?;
    let new = encode(&after)?;
    let _permit = catalog.writers.enter(Priority::Background)?;
    let tx = catalog
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(
        tx.execute(
            "UPDATE migration_runs SET progress=?3 WHERE id=?1 AND progress=?2",
            params![before.id, old, new]
        )? == 1,
        "supplement custody cursor changed; retry"
    );
    tx.execute(
        "INSERT INTO migration_run_supplements VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            before.id,
            pin.revision,
            pin.source_id,
            pin.origin,
            pin.proof_blake3,
            member.evidence,
            semantic
        ],
    )?;
    tx.commit()?;
    Ok(Step {
        progress: after,
        outcome: None,
        needs_decision: None,
    })
}
impl Catalog {
    pub fn begin_selected_import(
        &mut self,
        source: &MigrationSource,
        approval: &[u8],
        policy: &Policy,
    ) -> Result<Progress> {
        ensure!(
            !policy.import_source.trim().is_empty()
                && policy.import_source.len() <= 4096
                && !policy.import_source.contains('\0'),
            "migration owner bounds"
        );
        if let OverlapPolicy::ReuseExactPath { reason } = &policy.overlap {
            ensure!(
                !reason.trim().is_empty() && reason.len() <= 4096,
                "path-sharing decision requires a bounded reason"
            );
        }
        ensure!(
            policy.artifacts.len() <= 4096,
            "artifact mapping roster bound"
        );
        ensure!(
            policy.supplements.len() <= 4096,
            "supplement mapping roster bound"
        );
        let mut supplemental_keys = std::collections::BTreeSet::new();
        for supplement in &policy.supplements {
            ensure!(
                supplement.capture_revision.len() == 64
                    && !supplement.source_id.is_empty()
                    && supplement.source_id.len() <= 4096
                    && supplement.evidence.len() == 64,
                "supplement mapping identity bound"
            );
            ensure!(
                source
                    .seal()
                    .selected
                    .iter()
                    .any(|s| s.revision == supplement.capture_revision),
                "supplement mapping is not selected"
            );
            ensure!(
                supplemental_keys.insert((
                    &supplement.capture_revision,
                    &supplement.source_id,
                    serde_json::to_string(&supplement.origin)?
                )),
                "multiple supplemental outcomes require explicit selection"
            );
        }
        let mut member_keys = std::collections::BTreeSet::new();
        for artifact in &policy.artifacts {
            ensure!(
                source
                    .seal()
                    .selected
                    .iter()
                    .any(|s| s.revision == artifact.capture_revision),
                "artifact mapping is not selected"
            );
            ensure!(
                member_keys.insert((&artifact.capture_revision, artifact.member_index)),
                "duplicate artifact mapping"
            );
            for path in [&artifact.mapping.root, &artifact.mapping.relative] {
                let n = match path {
                    NativePath::UnixBytes(v) => v.len(),
                    NativePath::WindowsWide(v) => v.len(),
                };
                ensure!((1..=32768).contains(&n), "artifact mapping path bound");
            }
        }
        if let KeywordOverlap::ReuseExactHierarchy { reason } = &policy.keyword_overlap {
            ensure!(
                !reason.trim().is_empty() && reason.len() <= 4096,
                "keyword reuse reason bounds"
            );
        }
        admit_supplements(self, source, policy)?;
        let policy_bytes = encode(policy)?;
        let id = blake3::hash(&encode(&(ADAPTER, source.binding_blake3(), policy))?)
            .to_hex()
            .to_string();
        let retained = self.begin_migration_retention(source, approval)?;
        let progress = Progress {
            id: id.clone(),
            input: retained.input,
            stage: Stage::Custody,
            capture_index: 0,
            artifact_index: 0,
            cursor: None,
            processed: 0,
            complete: false,
        };
        let encoded = encode(&progress)?;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO migration_runs VALUES(?1,?2,?3,?4)",
            params![id, progress.input, policy_bytes, encoded],
        )?;
        let (old, old_policy) = read(&tx, &id)?;
        ensure!(
            old.input == source.binding_blake3() && encode(&old_policy)? == policy_bytes,
            "migration run identity differs"
        );
        tx.commit()?;
        Ok(old)
    }
    pub fn selected_import_progress(&self, id: &str) -> Result<Progress> {
        Ok(read(&self.db, id)?.0)
    }
    /// One bounded custody/index operation or one native source row. No original
    /// file is opened. Callers may cancel between steps without losing receipts.
    pub fn step_selected_import(&mut self, source: &MigrationSource, id: &str) -> Result<Step> {
        let (before, policy) = read(&self.db, id)?;
        ensure!(
            before.input == source.binding_blake3(),
            "migration source binding differs"
        );
        let mut after = before.clone();
        let mut outcome = None;
        match before.stage {
            Stage::Custody => {
                if self.step_migration_retention(source)?.complete {
                    after.stage = Stage::SupplementCustody;
                }
            }
            Stage::SupplementCustody => return supplement_step(self, source, &before, &policy),
            Stage::Index => {
                if self.step_migration_lookup(&before.input, 64)?.complete {
                    after.stage = Stage::Files;
                }
            }
            Stage::ArtifactCustody => {
                return super::import_artifacts::pending(self, source, &before, &policy);
            }
            Stage::Reconciliation => return super::reconciliation::step(self, source, &before),
            Stage::Complete => {
                return Ok(Step {
                    progress: before,
                    outcome: None,
                    needs_decision: None,
                });
            }
            _ => {
                let captures = &source.seal().selected;
                if before.capture_index == captures.len() {
                    after.capture_index = 0;
                    after.cursor = None;
                    after.stage = before.stage.next();
                    after.complete = after.stage == Stage::Complete;
                } else {
                    let revision = &captures
                        .get(before.capture_index)
                        .context("migration capture cursor bounds")?
                        .revision;
                    let query = Lookup::RowsByTable(
                        before
                            .stage
                            .table()
                            .context("stage has no source table")?
                            .into(),
                    );
                    let page = self.migration_lookup(
                        &before.input,
                        revision,
                        &query,
                        before.cursor.as_ref(),
                        1,
                    )?;
                    if let Some(hit) = page.records.first() {
                        let prepared = prepared_origin(self, source, revision, hit.sequence)?;
                        let result = if let Some(reason) = prepared.1 {
                            retained(reason)
                        } else {
                            let origin = prepared.0.context("source row interpretation absent")?;
                            match before.stage {
                                Stage::Files => original(self, source, &policy, &origin)?,
                                Stage::Masters | Stage::VirtualCopies => {
                                    image(self, source, &policy, &origin, before.stage)?
                                }
                                Stage::FileEmbedded
                                | Stage::FileSidecarXmp
                                | Stage::FileSidecarUpper
                                | Stage::FileAppendedXmp
                                | Stage::FileAppendedUpper => {
                                    file_metadata(self, source, &policy, &origin, before.stage)?
                                }
                                Stage::CatalogXmp | Stage::CurrentDevelop => {
                                    metadata(self, source, &policy, &origin, before.stage)?
                                }
                                _ => super::organization_walk::project(
                                    self,
                                    source,
                                    &policy,
                                    &origin,
                                    before.stage,
                                )?,
                            }
                        };
                        match result {
                            RowResult::Applied(value) => outcome = Some(value),
                            RowResult::NeedsDecision(reason) => {
                                return Ok(Step {
                                    progress: before,
                                    outcome: None,
                                    needs_decision: Some(reason),
                                });
                            }
                        }
                        after.processed = after
                            .processed
                            .checked_add(1)
                            .context("migration count overflow")?;
                    }
                    after.cursor = page.next;
                    if after.cursor.is_none() {
                        after.capture_index += 1;
                    }
                    advance(
                        self,
                        &before,
                        &after,
                        page.records
                            .first()
                            .zip(outcome.as_ref())
                            .map(|(hit, outcome)| (hit.sequence, outcome)),
                    )?;
                    return Ok(Step {
                        progress: after,
                        outcome,
                        needs_decision: None,
                    });
                }
            }
        }
        if encode(&before)? != encode(&after)? {
            advance(self, &before, &after, None)?;
        }
        Ok(Step {
            progress: after,
            outcome,
            needs_decision: None,
        })
    }
}
pub(crate) enum RowResult {
    Applied(Outcome),
    NeedsDecision(String),
}
pub(crate) fn retained(reason: impl Into<String>) -> RowResult {
    RowResult::Applied(Outcome::Retained {
        reason: reason.into(),
    })
}
fn original(
    catalog: &mut Catalog,
    source: &MigrationSource,
    policy: &Policy,
    origin: &SourceRecord,
) -> Result<RowResult> {
    let key = origin.source.identity()?;
    // An identical resume preserves subsequent user relinks and its original
    // overlap decision, including interruption between registration and cursor.
    let previous: Option<(String, String)> = catalog
        .db
        .query_row(
            "SELECT import_source,decision_json FROM migration_originals WHERE source_identity=?",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let decision = if let Some((owner, decision)) = previous {
        ensure!(
            owner == policy.import_source && decision.len() <= LIMIT,
            "previous original owner or decision differs"
        );
        serde_json::from_str(&decision)?
    } else {
        let walk = Walk::new(catalog, source, &origin.source.capture_revision)?;
        let source_id = walk.source_id(origin)?;
        let page = catalog.migration_lookup(
            source.binding_blake3(),
            &origin.source.capture_revision,
            &Lookup::PathsBySource(source_id),
            None,
            2,
        )?;
        ensure!(
            page.keys_complete,
            "path lookup keys unavailable; resolve evidence before importing originals"
        );
        ensure!(
            page.records.len() <= 1 && page.next.is_none(),
            "multiple retained path records for original"
        );
        let Some(hit) = page.records.first() else {
            return Ok(retained("No retained original path"));
        };
        let record = catalog.migration_lookup_record(hit.sequence)?;
        ensure!(
            record.collection == Collection::Paths,
            "original path record type differs"
        );
        if matches!(
            record.fields.get("inspection_path"),
            Some(Field::Inline(Cell::Null))
        ) {
            return Ok(retained(
                "Original path is unresolved; original row and path evidence retained",
            ));
        }
        let path: NativePath = serde_json::from_slice(&retention::field_bytes(
            &catalog.db,
            hit.sequence,
            &record,
            "inspection_path",
            131072,
        )?)?;
        let existing: Option<String> = catalog
            .db
            .query_row(
                "SELECT id FROM assets WHERE location=?",
                [crate::catalog_storage::encoded_bytes(&path)],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            None => OriginalDecision::Create { path },
            Some(asset_id) => match &policy.overlap {
                OverlapPolicy::RequireDecision => {
                    return Ok(RowResult::NeedsDecision(format!(
                        "Source file {key} shares an existing path; choose an explicit physical-file reuse policy"
                    )));
                }
                OverlapPolicy::ReuseExactPath { .. } => OriginalDecision::Reuse {
                    asset_id,
                    expected_path: path,
                },
            },
        }
    };
    let result = catalog.register_migration_original(&OriginalRequest {
        import_source: policy.import_source.clone(),
        source: origin.source.clone(),
        decision,
        retained_record: origin.retained_record,
    })?;
    Ok(RowResult::Applied(Outcome::Original {
        asset_id: result.asset_id,
        created: result.created,
    }))
}
fn zero(cell: &Cell) -> bool {
    match cell {
        Cell::Null => true,
        Cell::Integer(v) => *v == 0,
        Cell::RealBits(v) => f64::from_bits(*v) == 0.0,
        _ => false,
    }
}
fn image(
    catalog: &mut Catalog,
    source: &MigrationSource,
    policy: &Policy,
    origin: &SourceRecord,
    stage: Stage,
) -> Result<RowResult> {
    let walk = Walk::new(catalog, source, &origin.source.capture_revision)?;
    let table = walk.schema("Adobe_images")?;
    let raw = catalog.migration_lookup_record(origin.retained_record)?;
    let schema = catalog.migration_lookup_record(table)?;
    if field_length(&raw, "cells_json")? > LIMIT || field_length(&schema, "columns_json")? > LIMIT {
        let result=catalog.project_migration_image(Some(source),&images::Projection{origin:origin.clone(),retained_table:table,import_source:policy.import_source.clone(),decision:images::Decision::Retain{reason:"Image interpretation exceeds bounded cells/columns limits; complete original row retained".into()}})?;
        return Ok(RowResult::Applied(Outcome::Image(result.outcome)));
    }
    let fields = walk.columns(origin)?;
    let master = fields.get("masterImage");
    let is_master = master.is_some_and(zero);
    if stage == Stage::Masters && !is_master || stage == Stage::VirtualCopies && is_master {
        return Ok(RowResult::Applied(Outcome::OtherImageRole));
    }
    let mut reason = None;
    let label = match fields.get("copyName") {
        Some(Cell::Text(bytes)) if !bytes.is_empty() => {
            if bytes.len() <= 4096 {
                match String::from_utf8(bytes.clone()) {
                    Ok(value) => value,
                    Err(_) => {
                        reason =
                            Some("Source copy name is not UTF-8; complete source retained".into());
                        String::new()
                    }
                }
            } else {
                reason = Some(
                    "Source copy name exceeds interpretation limit; complete source retained"
                        .into(),
                );
                String::new()
            }
        }
        Some(Cell::Null) | None => {
            if is_master {
                "Original".into()
            } else {
                "Virtual copy".into()
            }
        }
        Some(Cell::Text(_)) => {
            if is_master {
                "Original".into()
            } else {
                "Virtual copy".into()
            }
        }
        _ => {
            reason = Some("Source copy name has unsupported type; complete source retained".into());
            String::new()
        }
    };
    let file = match walk.link(origin, "rootFile", "AgLibraryFile")? {
        LinkResolution::Unique(link) => Some(link),
        other => {
            reason = Some(format!("Original file relation: {other:?}"));
            None
        }
    };
    if let Some(file) = &file {
        let mapped:bool=catalog.db.query_row("SELECT EXISTS(SELECT 1 FROM migration_originals WHERE source_identity=?1 AND import_source=?2)",params![file.target.source.identity()?,policy.import_source],|r|r.get(0))?;
        if !mapped {
            reason =
                Some("Original path is unresolved; no native physical asset was invented".into());
        }
    }
    let role = if is_master {
        Some(images::Role::Master)
    } else if master.is_none() {
        reason = Some("Source schema has no masterImage classification".into());
        None
    } else {
        match walk.link(origin, "masterImage", "Adobe_images")? {
            LinkResolution::Unique(link) => {
                let parent = walk.columns(&link.target)?;
                let mapped:bool=catalog.db.query_row("SELECT EXISTS(SELECT 1 FROM migration_images WHERE source_identity=?1 AND owner=?2 AND json_extract(result,'$.outcome.kind')='Image')",params![link.target.source.identity()?,policy.import_source],|r|r.get(0))?;
                if parent.get("masterImage").is_some_and(zero) && mapped {
                    Some(images::Role::Virtual { master: *link })
                } else {
                    reason = Some("Virtual parent is not a registered source master".into());
                    None
                }
            }
            other => {
                reason = Some(format!("Virtual parent relation: {other:?}"));
                None
            }
        }
    };
    let decision = if let Some(reason) = reason {
        images::Decision::Retain { reason }
    } else {
        images::Decision::Register {
            file: file.context("missing file decision")?,
            role: role.context("missing role decision")?,
            label,
        }
    };
    let result = catalog.project_migration_image(
        Some(source),
        &images::Projection {
            origin: origin.clone(),
            retained_table: table,
            import_source: policy.import_source.clone(),
            decision,
        },
    )?;
    Ok(RowResult::Applied(Outcome::Image(result.outcome)))
}
// Match the complete import identity so metadata walks use a point lookup,
// rather than scanning all images belonging to the import owner.
const MAPPED_IMAGE_EXISTS_SQL: &str = "SELECT EXISTS(SELECT 1 FROM image_import_map WHERE import_source=?1 AND capture_revision=?2 AND source_table=?3 AND source_id=?4)";

fn has_mapped_image(db: &Connection, owner: &str, source: &SourceKey) -> Result<bool> {
    Ok(db.query_row(
        MAPPED_IMAGE_EXISTS_SQL,
        params![
            owner,
            source.capture_revision,
            source.table,
            source.identity()?
        ],
        |r| r.get(0),
    )?)
}

fn metadata(
    catalog: &mut Catalog,
    source: &MigrationSource,
    policy: &Policy,
    origin: &SourceRecord,
    stage: Stage,
) -> Result<RowResult> {
    use super::metadata::{CatalogXmp, CurrentDevelop};
    let walk = Walk::new(catalog, source, &origin.source.capture_revision)?;
    if stage == Stage::CurrentDevelop {
        let mapped = has_mapped_image(&catalog.db, &policy.import_source, &origin.source)?;
        if !mapped {
            return Ok(retained(
                "Current settings have no mapped image; source evidence retained",
            ));
        }
        let settings = match walk.link(
            origin,
            "developSettingsIDCache",
            "Adobe_imageDevelopSettings",
        )? {
            LinkResolution::Unique(v) => v,
            other => return Ok(retained(format!("Current settings relation: {other:?}"))),
        };
        let request = CurrentDevelop {
            image: origin.clone(),
            image_table: walk.schema("Adobe_images")?,
            settings_table: walk.schema("Adobe_imageDevelopSettings")?,
            settings: *settings,
            settings_path: vec![],
            import_source: policy.import_source.clone(),
            expected_edit_revision: 0,
        };
        Ok(RowResult::Applied(metadata_outcome(
            catalog.project_migration_current_develop(Some(source), &request)?,
        )))
    } else {
        let image = match walk.link(origin, "image", "Adobe_images")? {
            LinkResolution::Unique(v) => v,
            other => return Ok(retained(format!("Catalog XMP image relation: {other:?}"))),
        };
        let mapped = has_mapped_image(&catalog.db, &policy.import_source, &image.target.source)?;
        if !mapped {
            return Ok(retained(
                "Catalog XMP has no mapped image; original packet retained",
            ));
        }
        let page = catalog.migration_lookup(
            source.binding_blake3(),
            &origin.source.capture_revision,
            &Lookup::Packets {
                source_id: walk.source_id(origin)?,
                origin: Some("catalog".into()),
            },
            None,
            2,
        )?;
        ensure!(
            page.keys_complete && page.next.is_none() && page.records.len() <= 1,
            "catalog XMP packet lookup is incomplete or ambiguous"
        );
        let Some(packet) = page.records.first() else {
            return Ok(retained(
                "No catalog XMP packet; original source row retained",
            ));
        };
        let request = CatalogXmp {
            origin: origin.clone(),
            retained_table: walk.schema("Adobe_AdditionalMetadata")?,
            packet_record: packet.sequence,
            image: *image,
            import_source: policy.import_source.clone(),
        };
        Ok(RowResult::Applied(metadata_outcome(
            catalog.project_migration_catalog_xmp(Some(source), &request)?,
        )))
    }
}
fn file_metadata(
    catalog: &mut Catalog,
    source: &MigrationSource,
    policy: &Policy,
    origin: &SourceRecord,
    stage: Stage,
) -> Result<RowResult> {
    use super::file_metadata::{Association, Origin, Projection};
    let (kind, name) = match stage {
        Stage::FileEmbedded => (Origin::Embedded, "embedded"),
        Stage::FileSidecarXmp => (Origin::SidecarXmp, "sidecar_xmp"),
        Stage::FileSidecarUpper => (Origin::SidecarUpper, "sidecar_XMP"),
        Stage::FileAppendedXmp => (Origin::AppendedXmp, "sidecar_appended_xmp"),
        Stage::FileAppendedUpper => (Origin::AppendedUpper, "sidecar_appended_XMP"),
        _ => anyhow::bail!("not a file metadata stage"),
    };
    let mapped:bool=catalog.db.query_row("SELECT EXISTS(SELECT 1 FROM migration_originals WHERE source_identity=?1 AND import_source=?2)",params![origin.source.identity()?,policy.import_source],|r|r.get(0))?;
    if !mapped {
        return Ok(retained(
            "File metadata has no mapped original; all source evidence remains retained",
        ));
    }
    let walk = Walk::new(catalog, source, &origin.source.capture_revision)?;
    let source_id = walk.source_id(origin)?;
    let page = catalog.migration_lookup(
        source.binding_blake3(),
        &origin.source.capture_revision,
        &Lookup::PathsBySource(source_id.clone()),
        None,
        2,
    )?;
    ensure!(
        page.keys_complete && page.next.is_none() && page.records.len() <= 1,
        "file metadata path roster unavailable or ambiguous"
    );
    let Some(path) = page.records.first() else {
        return Ok(retained("No file metadata path observation"));
    };
    let roster = source.origin_packet_roster(&origin.source.capture_revision, &source_id, name)?;
    // Source sequence is only a locator. The destination join is scoped by the
    // exact sealed input/capture/collection and each component verifies payloads.
    let mut packet_records = Vec::with_capacity(roster.len());
    for source_rowid in roster {
        packet_records.push(catalog.db.query_row("SELECT sequence FROM migration_retained_records WHERE input=?1 AND revision=?2 AND collection=7 AND source_rowid=?3 AND complete=1",params![source.binding_blake3(),origin.source.capture_revision,source_rowid],|r|r.get(0))?);
    }
    let request = Projection {
        file: origin.clone(),
        retained_path: path.sequence,
        origin: kind,
        packet_records,
        import_source: policy.import_source.clone(),
        association: if kind == Origin::Embedded {
            Association::Confirmed {
                reason: "Retained embedded packet belongs to its exact selected file source".into(),
            }
        } else {
            Association::Unresolved
        },
        supplement: None,
    };
    let historical = catalog.project_migration_file_metadata(Some(source), &request)?;
    let supplemental = policy.supplements.iter().find(|p| {
        p.capture_revision == origin.source.capture_revision
            && p.source_id == source_id
            && p.origin == kind
    });
    let result = if let Some(supplement) = supplemental {
        let mut request = request;
        request.supplement = Some(supplement.evidence.clone());
        catalog.project_migration_file_metadata(Some(source), &request)?
    } else {
        historical
    };
    Ok(RowResult::Applied(Outcome::FileMetadata(result)))
}
fn metadata_outcome(result: super::metadata::ResultRecord) -> Outcome {
    // Full extracted settings already live in their source-bound projection.
    // The run ledger stores a compact receipt instead of duplicating all values.
    Outcome::Metadata {
        input_digest: result.input_digest,
        state: result.state,
        observation: result.observation,
        reason: result.reason,
    }
}
fn field_length(
    record: &crate::lightroom::migration_source::EvidenceRecord,
    name: &str,
) -> Result<usize> {
    Ok(
        match record
            .fields
            .get(name)
            .context("retained source field missing")?
        {
            Field::Inline(Cell::Text(v) | Cell::Blob(v)) => v.len(),
            Field::Bytes(v) => usize::try_from(v.bytes)?,
            _ => anyhow::bail!("retained source field is not bytes"),
        },
    )
}
fn prepared_origin(
    catalog: &Catalog,
    source: &MigrationSource,
    revision: &str,
    sequence: i64,
) -> Result<(Option<SourceRecord>, Option<String>)> {
    let record = catalog.migration_lookup_record(sequence)?;
    ensure!(
        record.revision == revision && record.collection == Collection::Rows,
        "source walk row scope differs"
    );
    let input: String = catalog.db.query_row(
        "SELECT input FROM migration_retained_records WHERE sequence=?",
        [sequence],
        |r| r.get(0),
    )?;
    ensure!(
        input == source.binding_blake3(),
        "source walk input differs"
    );
    for (name, maximum) in [
        ("source_id", 4096),
        ("table_name", 1024),
        ("key_json", 65536),
    ] {
        if field_length(&record, name)? > maximum {
            return Ok((
                None,
                Some(format!(
                    "Source {name} exceeds supported identity bounds; full row retained"
                )),
            ));
        }
    }
    let key: Vec<Cell> = serde_json::from_slice(&retention::field_bytes(
        &catalog.db,
        sequence,
        &record,
        "key_json",
        65536,
    )?)?;
    let bytes = key.iter().try_fold(0usize, |sum, cell| {
        sum.checked_add(match cell {
            Cell::Text(v) | Cell::Blob(v) => v.len(),
            _ => 8,
        })
        .context("source key byte overflow")
    })?;
    if key.is_empty() || key.len() > 128 || bytes > 16384 {
        return Ok((
            None,
            Some("Source typed key exceeds supported identity bounds; full row retained".into()),
        ));
    }
    let id = retention::field_bytes(&catalog.db, sequence, &record, "source_id", 4096)?;
    if std::str::from_utf8(&id).is_err() {
        return Ok((
            None,
            Some("Source identity is not UTF-8; full row retained".into()),
        ));
    }
    Ok((
        Some(Walk::new(catalog, source, revision)?.source_record(sequence)?),
        None,
    ))
}

#[cfg(test)]
mod mapping_query_tests {
    use super::*;
    use rusqlite::StatementStatus;

    #[test]
    fn metadata_mapping_uses_bounded_complete_identity_lookup() -> Result<()> {
        // Use the installed product table/index DDL, not a simplified test index.
        // Only this isolated in-memory copy omits referenced image records.
        let root = tempfile::tempdir()?;
        let catalog = Catalog::open(root.path())?;
        let mut db = Connection::open_in_memory()?;
        db.execute_batch("PRAGMA foreign_keys=OFF")?;
        let mut ddl = catalog.db.prepare(
            "SELECT sql FROM sqlite_schema WHERE tbl_name='image_import_map' AND type IN ('table','index') AND sql IS NOT NULL ORDER BY type DESC",
        )?;
        for sql in ddl.query_map([], |r| r.get::<_, String>(0))? {
            db.execute_batch(&sql?)?;
        }
        let source = |key| SourceKey {
            capture_revision: "a".repeat(64),
            table: "Adobe_images".into(),
            key: vec![Cell::Integer(key)],
        };
        let tx = db.transaction()?;
        {
            let mut insert = tx
                .prepare("INSERT INTO image_import_map VALUES(?1,?2,?3,?4,'input','fixture',?5)")?;
            for key in 0..20_000 {
                let row = source(key);
                insert.execute(params![
                    "owner",
                    row.capture_revision,
                    row.table,
                    row.identity()?,
                    format!("image-{key}")
                ])?;
            }
        }
        tx.commit()?;

        let matched = source(19_999);
        let missing = source(20_000);
        let mut other_capture = matched.clone();
        other_capture.capture_revision = "b".repeat(64);
        let mut other_table = matched.clone();
        other_table.table = "AgLibraryFile".into();
        let mut other_typed_key = matched.clone();
        other_typed_key.key = vec![Cell::Text(b"19999".to_vec())];
        for (owner, row, expected) in [
            ("owner", &matched, true),
            ("owner", &missing, false),
            ("other-owner", &matched, false),
            ("owner", &other_capture, false),
            ("owner", &other_table, false),
            ("owner", &other_typed_key, false),
        ] {
            assert_eq!(has_mapped_image(&db, owner, row)?, expected);
            let mut statement = db.prepare(MAPPED_IMAGE_EXISTS_SQL)?;
            let found: bool = statement.query_row(
                params![owner, row.capture_revision, row.table, row.identity()?],
                |r| r.get(0),
            )?;
            assert_eq!(found, expected);
            let work = statement.get_status(StatementStatus::VmStep);
            assert!(
                (1..=100).contains(&work),
                "mapping lookup used {work} VM steps"
            );
        }
        // Control: the previous production predicate must fail the same work
        // bound on a miss, regardless of the ordering of hashed source IDs.
        let mut legacy = db.prepare(
            "SELECT EXISTS(SELECT 1 FROM image_import_map WHERE source_id=?1 AND import_source=?2)",
        )?;
        assert!(
            !legacy.query_row(params![missing.identity()?, "owner"], |r| r
                .get::<_, bool>(0))?
        );
        assert!(legacy.get_status(StatementStatus::VmStep) > 20_000);
        Ok(())
    }
}
