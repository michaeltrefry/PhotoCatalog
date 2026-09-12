//! Explicit, resumable correction of the original current-settings container decision.
//! Frozen retained evidence is never replaced. Effective receipts change only with
//! compressed predecessor archives, recipe CAS and the repair cursor in one transaction.
//! A sealed inspection lease is required for the existing unique-link proof; no
//! original image, sidecar or capture artifact is opened by this component.
use super::{
    importer::{self, Outcome, Stage},
    metadata::{self, CurrentDevelop, ResultRecord},
    walk::{LinkResolution, Walk},
};
use crate::{Catalog, catalog_writer::Priority, lightroom::migration_source::MigrationSource};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

const ADAPTER: &str = "lightroom-current-container-repair-v1";
const LIMIT: usize = 8 * 1024 * 1024;
const CURRENT: &str = "\"CurrentDevelop\"";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub run: String,
    /// BLAKE3 of the exact stored, completed importer progress JSON.
    pub expected_complete_progress_blake3: String,
    pub expected_mapping_epoch: i64,
    pub reason: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    ArchiveReports,
    Project,
    Reconciliation,
    Complete,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub id: String,
    pub run: String,
    pub input: String,
    pub phase: Phase,
    pub report_index: usize,
    pub after_record: i64,
    pub examined: u64,
    pub repaired: u64,
    pub unchanged: u64,
    pub complete: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub progress: Progress,
    pub record: Option<i64>,
    pub outcome: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    adapter: String,
    request: Request,
    input: String,
    policy_blake3: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MetadataHeader {
    image_source: String,
    payload_source: String,
    slot: String,
    owner: String,
    input_digest: String,
    retained_record: i64,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS migration_current_repairs(
        id TEXT PRIMARY KEY, run TEXT NOT NULL UNIQUE REFERENCES migration_runs(id),
        binding BLOB NOT NULL, progress BLOB NOT NULL,
        original_progress BLOB NOT NULL, original_length INTEGER NOT NULL CHECK(original_length<=8388608),
        original_digest TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS migration_current_repair_reports(
        repair TEXT NOT NULL REFERENCES migration_current_repairs(id), revision TEXT NOT NULL,
        compressed BLOB NOT NULL, raw_length INTEGER NOT NULL CHECK(raw_length<=8388608),
        digest TEXT NOT NULL, epoch INTEGER NOT NULL, PRIMARY KEY(repair,revision));
      CREATE TABLE IF NOT EXISTS migration_current_repair_items(
        repair TEXT NOT NULL REFERENCES migration_current_repairs(id), record INTEGER NOT NULL,
        revision TEXT NOT NULL, disposition TEXT NOT NULL,
        old_outcome BLOB NOT NULL, outcome_length INTEGER NOT NULL CHECK(outcome_length<=8388608),
        outcome_digest TEXT NOT NULL, metadata_header BLOB,
        old_result BLOB, result_length INTEGER CHECK(result_length<=8388608), result_digest TEXT,
        new_result_digest TEXT, new_outcome_digest TEXT NOT NULL,
        PRIMARY KEY(repair,record));")?;
    Ok(())
}
fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    crate::lightroom::bounded_json(v, LIMIT)
}
fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
fn hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(bytes.len() <= LIMIT, "repair archive exceeds bound");
    let mut w = ZlibEncoder::new(Vec::new(), Compression::default());
    w.write_all(bytes)?;
    Ok(w.finish()?)
}
fn decompress(bytes: &[u8], length: i64, expected: &str) -> Result<Vec<u8>> {
    let length = usize::try_from(length)?;
    ensure!(
        length <= LIMIT && bytes.len() <= LIMIT + 65536,
        "repair archive bounds"
    );
    let mut out = Vec::new();
    ZlibDecoder::new(bytes)
        .take((length + 1) as u64)
        .read_to_end(&mut out)?;
    ensure!(
        out.len() == length && digest(&out) == expected,
        "repair archive identity differs"
    );
    Ok(out)
}
fn epoch(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "SELECT epoch FROM migration_mapping_epoch WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}
fn read(db: &Connection, id: &str) -> Result<(Binding, Progress)> {
    ensure!(hash(id), "repair identity bounds");
    let (a,b):(Option<Vec<u8>>,Option<Vec<u8>>)=db.query_row("SELECT CASE WHEN length(binding)<=8388608 THEN binding END,CASE WHEN length(progress)<=8388608 THEN progress END FROM migration_current_repairs WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let a = a.context("repair binding bound")?;
    let b = b.context("repair progress bound")?;
    let binding: Binding = serde_json::from_slice(&a)?;
    let progress: Progress = serde_json::from_slice(&b)?;
    ensure!(
        binding.adapter == ADAPTER
            && digest(&a) == id
            && progress.id == id
            && progress.run == binding.request.run
            && progress.input == binding.input,
        "repair binding differs"
    );
    ensure!(
        progress.complete == (progress.phase == Phase::Complete),
        "repair completion flag differs"
    );
    Ok((binding, progress))
}
/// Read-only status; does not open or upgrade a catalog.
pub fn read_progress(db: &Connection, id: &str) -> Result<Progress> {
    Ok(read(db, id)?.1)
}
/// Ordinary import cannot reconcile a destination with partially corrected receipts.
pub(crate) fn require_not_pending(db: &Connection, run: &str) -> Result<()> {
    let id: Option<String> = db
        .query_row(
            "SELECT id FROM migration_current_repairs WHERE run=?",
            [run],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = id {
        let (_, p) = read(db, &id)?;
        ensure!(
            matches!(p.phase, Phase::Reconciliation | Phase::Complete),
            "current settings repair pending; use its repair cursor"
        );
    }
    Ok(())
}
fn check(db: &Connection, b: &Binding, p: &Progress) -> Result<()> {
    let (run, policy) = importer::read(db, &p.run)?;
    ensure!(
        run.input == b.input && digest(&encode(&policy)?) == b.policy_blake3,
        "repair run/policy changed"
    );
    ensure!(
        epoch(db)? == b.request.expected_mapping_epoch,
        "repair mapping epoch changed; reconciliation required"
    );
    let (_, now) = read(db, &p.id)?;
    ensure!(encode(&now)? == encode(p)?, "repair cursor changed; retry");
    if matches!(p.phase, Phase::ArchiveReports | Phase::Project) {
        ensure!(
            !run.complete && run.stage == Stage::Reconciliation,
            "pending repair run state changed"
        );
    }
    Ok(())
}
fn advance(db: &Connection, before: &Progress, after: &Progress) -> Result<()> {
    ensure!(
        db.execute(
            "UPDATE migration_current_repairs SET progress=?3 WHERE id=?1 AND progress=?2",
            params![before.id, encode(before)?, encode(after)?]
        )? == 1,
        "repair cursor changed; retry"
    );
    Ok(())
}
fn outcome(result: &ResultRecord) -> Outcome {
    Outcome::Metadata {
        input_digest: result.input_digest.clone(),
        state: result.state.clone(),
        observation: result.observation,
        reason: result.reason.clone(),
    }
}

impl Catalog {
    pub fn begin_current_develop_repair(
        &mut self,
        source: &MigrationSource,
        request: &Request,
    ) -> Result<Progress> {
        ensure!(
            hash(&request.run)
                && hash(&request.expected_complete_progress_blake3)
                && request.expected_mapping_epoch >= 0
                && !request.reason.trim().is_empty()
                && request.reason.len() <= 4096,
            "repair request bounds"
        );
        let (before, policy) = importer::read(&self.db, &request.run)?;
        ensure!(
            before.input == source.binding_blake3(),
            "repair source seal differs"
        );
        // The supplied source must be exactly the already admitted custody seal,
        // including its selection/approval; a new selection is not a repair input.
        let (seal,approval,complete):(Option<Vec<u8>>,Option<Vec<u8>>,bool)=self.db.query_row(
            "SELECT CASE WHEN length(seal)<=8388608 THEN seal END,CASE WHEN length(approval)<=8388608 THEN approval END,complete FROM migration_retention WHERE id=?",
            [&before.input],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(
            complete
                && seal.context("retained seal bound")? == encode(source.seal())?
                && digest(&approval.context("retained approval bound")?)
                    == source.seal().approval.document_blake3,
            "repair retained selection/approval differs"
        );
        let binding = Binding {
            adapter: ADAPTER.into(),
            request: request.clone(),
            input: before.input.clone(),
            policy_blake3: digest(&encode(&policy)?),
        };
        let bytes = encode(&binding)?;
        let id = digest(&bytes);
        let old: Option<String> = self
            .db
            .query_row(
                "SELECT id FROM migration_current_repairs WHERE run=?",
                [&request.run],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(old == id, "repair decision changed");
            return Ok(read(&self.db, &old)?.1);
        }
        ensure!(
            before.complete
                && before.stage == Stage::Complete
                && epoch(&self.db)? == request.expected_mapping_epoch,
            "repair requires the pinned completed run/epoch"
        );
        let original:Option<Vec<u8>>=self.db.query_row("SELECT CASE WHEN length(progress)<=8388608 THEN progress END FROM migration_runs WHERE id=?",[&request.run],|r|r.get(0))?;
        let original = original.context("complete progress bound")?;
        ensure!(
            digest(&original) == request.expected_complete_progress_blake3
                && encode(&before)? == original,
            "complete progress digest differs"
        );
        let compressed = compress(&original)?;
        let progress = Progress {
            id: id.clone(),
            run: request.run.clone(),
            input: before.input.clone(),
            phase: Phase::ArchiveReports,
            report_index: 0,
            after_record: 0,
            examined: 0,
            repaired: 0,
            unchanged: 0,
            complete: false,
        };
        let mut pending = before.clone();
        pending.complete = false;
        pending.stage = Stage::Reconciliation;
        pending.capture_index = 0;
        pending.cursor = None;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            epoch(&tx)? == request.expected_mapping_epoch,
            "repair mapping changed before admission"
        );
        ensure!(
            tx.execute(
                "UPDATE migration_runs SET progress=?3 WHERE id=?1 AND progress=?2",
                params![request.run, original, encode(&pending)?]
            )? == 1,
            "complete progress changed before repair"
        );
        tx.execute(
            "INSERT INTO migration_current_repairs VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                id,
                request.run,
                bytes,
                encode(&progress)?,
                compressed,
                i64::try_from(original.len())?,
                request.expected_complete_progress_blake3
            ],
        )?;
        tx.commit()?;
        Ok(progress)
    }
    pub fn current_develop_repair_progress(&self, id: &str) -> Result<Progress> {
        read_progress(&self.db, id)
    }
    pub fn step_current_develop_repair(
        &mut self,
        source: &MigrationSource,
        id: &str,
    ) -> Result<Step> {
        let (binding, before) = read(&self.db, id)?;
        ensure!(
            source.binding_blake3() == binding.input,
            "repair source seal differs"
        );
        check(&self.db, &binding, &before)?;
        match before.phase {
            Phase::ArchiveReports => self.archive_current_report(source, &binding, &before),
            Phase::Project => self.repair_current_item(source, &binding, &before),
            Phase::Reconciliation => {
                let run = importer::read(&self.db, &before.run)?.0;
                if !run.complete {
                    super::reconciliation::step(self, source, &run)?;
                }
                let run = importer::read(&self.db, &before.run)?.0;
                let mut after = before.clone();
                if run.complete {
                    ensure!(
                        run.stage == Stage::Complete,
                        "repair reconciliation state differs"
                    );
                    after.phase = Phase::Complete;
                    after.complete = true;
                    let _permit = self.writers.enter(Priority::Background)?;
                    let tx = self
                        .db
                        .transaction_with_behavior(TransactionBehavior::Immediate)?;
                    check(&tx, &binding, &before)?;
                    advance(&tx, &before, &after)?;
                    tx.commit()?;
                }
                Ok(Step {
                    progress: after,
                    record: None,
                    outcome: None,
                })
            }
            Phase::Complete => Ok(Step {
                progress: before,
                record: None,
                outcome: None,
            }),
        }
    }
    fn archive_current_report(
        &mut self,
        source: &MigrationSource,
        b: &Binding,
        before: &Progress,
    ) -> Result<Step> {
        let mut after = before.clone();
        if let Some(capture) = source.seal().selected.get(before.report_index) {
            let (bytes,old_epoch):(Option<Vec<u8>>,i64)=self.db.query_row("SELECT CASE WHEN length(report)<=8388608 THEN report END,epoch FROM migration_reconciliation WHERE run=?1 AND revision=?2",params![before.run,capture.revision],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let bytes = bytes.context("old reconciliation report bound")?;
            ensure!(
                old_epoch == b.request.expected_mapping_epoch,
                "old report epoch differs"
            );
            let compressed = compress(&bytes)?;
            let sha = digest(&bytes);
            after.report_index = after
                .report_index
                .checked_add(1)
                .context("repair report cursor overflow")?;
            let _permit = self.writers.enter(Priority::Background)?;
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            check(&tx, b, before)?;
            tx.execute(
                "INSERT INTO migration_current_repair_reports VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    before.id,
                    capture.revision,
                    compressed,
                    i64::try_from(bytes.len())?,
                    sha,
                    old_epoch
                ],
            )?;
            ensure!(tx.execute("DELETE FROM migration_reconciliation WHERE run=?1 AND revision=?2 AND report=?3 AND epoch=?4",params![before.run,capture.revision,bytes,old_epoch])?==1,"old report changed before archival");
            advance(&tx, before, &after)?;
            tx.commit()?;
        } else {
            let _permit = self.writers.enter(Priority::Background)?;
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            check(&tx, b, before)?;
            ensure!(
                !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM migration_reconciliation WHERE run=?)",
                    [&before.run],
                    |r| r.get::<_, bool>(0)
                )?,
                "unexpected old reconciliation report"
            );
            after.phase = Phase::Project;
            advance(&tx, before, &after)?;
            tx.commit()?;
        }
        Ok(Step {
            progress: after,
            record: None,
            outcome: None,
        })
    }
    fn repair_current_item(
        &mut self,
        source: &MigrationSource,
        b: &Binding,
        before: &Progress,
    ) -> Result<Step> {
        // The installed (run,stage,record) primary key makes each step a seek.
        let next:Option<(i64,String,Option<Vec<u8>>)>=self.db.query_row(
            "SELECT record,revision,CASE WHEN length(outcome)<=8388608 THEN outcome END FROM migration_run_items WHERE run=?1 AND stage=?2 AND record>?3 ORDER BY record LIMIT 1",
            params![before.run,CURRENT,before.after_record],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((record, revision, old_outcome)) = next else {
            let mut after = before.clone();
            after.phase = Phase::Reconciliation;
            let _permit = self.writers.enter(Priority::Background)?;
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            check(&tx, b, before)?;
            advance(&tx, before, &after)?;
            tx.commit()?;
            return Ok(Step {
                progress: after,
                record: None,
                outcome: None,
            });
        };
        let old_outcome = old_outcome.context("old current outcome bound")?;
        let old_value: Outcome = serde_json::from_slice(&old_outcome)?;
        let walk = Walk::new(self, source, &revision)?;
        let (_, policy) = importer::read(&self.db, &before.run)?;
        let mut header = None;
        let mut old_result = None;
        let mut prepared = None;
        let mut disposition = "unchanged_nonprojection";
        if matches!(old_value, Outcome::Metadata { .. }) {
            let origin = walk.source_record(record)?;
            ensure!(
                origin.source.table == "Adobe_images",
                "repair item is not an image source"
            );
            let link = match walk.link(
                &origin,
                "developSettingsIDCache",
                "Adobe_imageDevelopSettings",
            )? {
                LinkResolution::Unique(v) => *v,
                _ => anyhow::bail!("repair predecessor current link no longer uniquely proven"),
            };
            let image = origin.source.identity()?;
            let payload = link.target.source.identity()?;
            let (old_owner,old_digest,retained,bytes):(String,String,i64,Option<Vec<u8>>)=self.db.query_row(
                "SELECT owner,input_digest,retained_record,CASE WHEN length(result)<=8388608 THEN result END FROM migration_metadata WHERE image_source=?1 AND payload_source=?2 AND slot='current_develop'",
                params![image,payload],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            let bytes = bytes.context("old metadata result bound")?;
            let old: ResultRecord = serde_json::from_slice(&bytes)?;
            ensure!(
                old_owner == policy.import_source
                    && old_digest == old.input_digest
                    && retained == link.target.retained_record
                    && encode(&outcome(&old))? == old_outcome,
                "predecessor metadata/outcome association differs"
            );
            ensure!(
                old.image
                    == super::images::mapped_image(
                        &self.db,
                        &policy.import_source,
                        &origin.source
                    )?,
                "old metadata image does not match selected mapping"
            );
            let mut request = CurrentDevelop {
                image: origin.clone(),
                image_table: walk.schema("Adobe_images")?,
                settings_table: walk.schema("Adobe_imageDevelopSettings")?,
                settings: link,
                settings_path: old
                    .extraction
                    .as_ref()
                    .map(|e| e.input.settings_path.clone())
                    .unwrap_or_default(),
                import_source: policy.import_source.clone(),
                expected_edit_revision: 0,
            };
            ensure!(
                metadata::current_develop_input_digest(&request)? == old_digest,
                "predecessor is not the original coordinator decision"
            );
            let prior_path = request.settings_path.clone();
            request = self.prepare_migration_current_develop(request)?;
            disposition = "unchanged_container";
            if request.settings_path != prior_path {
                ensure!(
                    prior_path.is_empty()
                        && old.state == "retained_only"
                        && old.observation.is_none(),
                    "repair only adopts original unprojected root-container decisions"
                );
                if let Some(extraction) = &old.extraction {
                    ensure!(
                        extraction.contribution.exposure_ev.is_none()
                            && extraction.contribution.white_balance.is_none(),
                        "predecessor already has translated settings"
                    );
                }
                let old_revision = old
                    .edit_revision
                    .context("repair predecessor has no installed recipe")?;
                request.expected_edit_revision = old_revision;
                // The old importer saved an import edit even when the contribution was empty.
                // Do not assume revision zero, accept an unrelated current recipe, or erase user edits.
                check_recipe(self, &request, &old)?;
                let p = self.prepare_current_develop_projection(source, &request)?;
                let old_input = &old
                    .extraction
                    .as_ref()
                    .context("old extraction input absent")?
                    .input;
                let mut new_input = p
                    .result()
                    .extraction
                    .as_ref()
                    .context("prepared extraction input absent")?
                    .input
                    .clone();
                new_input.settings_path = prior_path.clone();
                ensure!(
                    &new_input == old_input,
                    "old extraction payload/source binding differs from retained data"
                );
                ensure!(
                    p.key() == &old.image
                        && p.image_source() == image
                        && p.payload_source() == payload,
                    "prepared repair image/source differs"
                );
                prepared = Some((p, request, old));
                disposition = "container_rebound";
            }
            header = Some(MetadataHeader {
                image_source: image,
                payload_source: payload,
                slot: "current_develop".into(),
                owner: old_owner,
                input_digest: old_digest,
                retained_record: retained,
            });
            old_result = Some(bytes);
        } else {
            // The importer can retain a row specifically because its typed key or
            // source ID is unavailable to interpretation. Preserve that outcome
            // without requiring the SourceRecord it deliberately could not make.
            let retained = self.migration_lookup_record(record)?;
            let input: String = self.db.query_row(
                "SELECT input FROM migration_retained_records WHERE sequence=?1",
                [record],
                |r| r.get(0),
            )?;
            ensure!(
                input == source.binding_blake3()
                    && retained.revision == revision
                    && retained.collection == crate::lightroom::migration_source::Collection::Rows
                    && super::retention::field_bytes(
                        &self.db,
                        record,
                        &retained,
                        "table_name",
                        1024
                    )? == b"Adobe_images",
                "retained nonprojection item is not this selected image row"
            );
        }
        let archived_outcome = compress(&old_outcome)?;
        let archived_result = old_result.as_deref().map(compress).transpose()?;
        let header_bytes = header.as_ref().map(encode).transpose()?;
        let mut after = before.clone();
        after.after_record = record;
        after.examined = after
            .examined
            .checked_add(1)
            .context("repair count overflow")?;
        if prepared.is_some() {
            after.repaired = after
                .repaired
                .checked_add(1)
                .context("repair count overflow")?;
        } else {
            after.unchanged = after
                .unchanged
                .checked_add(1)
                .context("repair count overflow")?;
        }
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check(&tx, b, before)?;
        let still:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM migration_run_items WHERE run=?1 AND stage=?2 AND record=?3 AND revision=?4 AND outcome=?5)",params![before.run,CURRENT,record,revision,old_outcome],|r|r.get(0))?;
        ensure!(still, "old current run outcome changed");
        let mut new_outcome = old_outcome.clone();
        let mut new_result_digest = old_result.as_deref().map(digest);
        if let Some((p, request, old)) = prepared {
            check_recipe_tx(&tx, &request, &old)?;
            let h = header.as_ref().context("repair metadata header missing")?;
            ensure!(tx.execute("DELETE FROM migration_metadata WHERE image_source=?1 AND payload_source=?2 AND slot=?3 AND owner=?4 AND input_digest=?5 AND retained_record=?6 AND result=?7",params![h.image_source,h.payload_source,h.slot,h.owner,h.input_digest,h.retained_record,old_result.as_ref().context("repair old result missing")?])?==1,"old current metadata changed before adoption");
            let result = metadata::commit_current_develop_projection(&tx, &p)?;
            new_outcome = encode(&outcome(&result))?;
            new_result_digest = Some(digest(&encode(&result)?));
            ensure!(tx.execute("UPDATE migration_run_items SET outcome=?6 WHERE run=?1 AND stage=?2 AND record=?3 AND revision=?4 AND outcome=?5",params![before.run,CURRENT,record,revision,old_outcome,new_outcome])?==1,"repair outcome CAS failed");
        } else if let Some(h) = &header {
            ensure!(tx.query_row("SELECT EXISTS(SELECT 1 FROM migration_metadata WHERE image_source=?1 AND payload_source=?2 AND slot=?3 AND owner=?4 AND input_digest=?5 AND retained_record=?6 AND result=?7)",params![h.image_source,h.payload_source,h.slot,h.owner,h.input_digest,h.retained_record,old_result.as_ref().context("old result absent")?],|r|r.get::<_,bool>(0))?,"unchanged metadata receipt changed");
        }
        tx.execute("INSERT INTO migration_current_repair_items VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",params![before.id,record,revision,disposition,archived_outcome,i64::try_from(old_outcome.len())?,digest(&old_outcome),header_bytes,archived_result,old_result.as_ref().map(|v|i64::try_from(v.len())).transpose()?,old_result.as_deref().map(digest),new_result_digest,digest(&new_outcome)])?;
        advance(&tx, before, &after)?;
        tx.commit()?;
        Ok(Step {
            progress: after,
            record: Some(record),
            outcome: Some(disposition.into()),
        })
    }
    /// Bounded offline recovery of the exact predecessor outcome and optional metadata
    /// result; compressed archives are verified before returning the original bytes.
    pub fn current_develop_repair_predecessor(&self, id: &str, record: i64) -> Result<Predecessor> {
        read(&self.db, id)?;
        ensure!(record > 0, "repair record bounds");
        let row=self.db.query_row("SELECT CASE WHEN length(old_outcome)<=8454144 THEN old_outcome END,outcome_length,outcome_digest,CASE WHEN length(metadata_header)<=8388608 THEN metadata_header END,CASE WHEN length(old_result)<=8454144 THEN old_result END,result_length,result_digest,new_result_digest,new_outcome_digest,disposition FROM migration_current_repair_items WHERE repair=?1 AND record=?2",params![id,record],|r|Ok((r.get::<_,Option<Vec<u8>>>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<Vec<u8>>>(3)?,r.get::<_,Option<Vec<u8>>>(4)?,r.get::<_,Option<i64>>(5)?,r.get::<_,Option<String>>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,String>(8)?,r.get::<_,String>(9)?)))?;
        let outcome = decompress(
            &row.0.context("repair outcome compressed bound")?,
            row.1,
            &row.2,
        )?;
        let metadata = match (row.3, row.4, row.5, row.6) {
            (Some(header), Some(bytes), Some(length), Some(sha)) => Some((
                serde_json::from_slice(&header)?,
                decompress(&bytes, length, &sha)?,
            )),
            (None, None, None, None) => None,
            _ => anyhow::bail!("repair metadata archive incomplete or oversized"),
        };
        Ok(Predecessor {
            outcome,
            metadata,
            new_result_digest: row.7,
            new_outcome_digest: row.8,
            disposition: row.9,
        })
    }
}
/// Byte buffers are raw evidence, not an invitation to serialize unbounded byte arrays.
/// Readback is one item, at most two 8 MiB documents plus a bounded header.
pub struct Predecessor {
    pub outcome: Vec<u8>,
    pub metadata: Option<(serde_json::Value, Vec<u8>)>,
    pub new_result_digest: Option<String>,
    pub new_outcome_digest: String,
    pub disposition: String,
}
fn check_recipe(catalog: &Catalog, request: &CurrentDevelop, old: &ResultRecord) -> Result<()> {
    let current = catalog.edit_variant(&old.image)?;
    ensure!(
        Some(current.revision) == old.edit_revision,
        "variant was edited after the predecessor projection"
    );
    check_recipe_tx(&catalog.db, request, old)
}
fn check_recipe_tx(db: &Connection, request: &CurrentDevelop, old: &ResultRecord) -> Result<()> {
    let revision = old
        .edit_revision
        .context("old recipe revision unavailable")?;
    let (kind,provenance,node,cursor,current_revision,digest,stored_digest):(String,Option<String>,i64,Option<i64>,i64,String,String)=db.query_row(
        "SELECT c.kind,CASE WHEN length(CAST(c.provenance AS BLOB))<=65536 THEN c.provenance END,c.current_node,v.cursor,v.revision,n.digest,current.digest FROM edit_changes c JOIN edit_variants v ON v.asset_id=c.asset_id AND v.id=c.variant_id JOIN edit_recipe_nodes n ON n.id=c.current_node JOIN edit_recipe_nodes current ON current.id=v.cursor WHERE c.asset_id=?1 AND c.variant_id=?2 AND c.revision=?3",
        params![old.image.asset_id,old.image.variant_id,revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
    let provenance: serde_json::Value =
        serde_json::from_str(&provenance.context("old import provenance bound")?)?;
    ensure!(
        kind == "import"
            && cursor == Some(node)
            && current_revision == revision
            && digest == stored_digest,
        "variant recipe changed after original import"
    );
    ensure!(
        provenance
            == serde_json::json!({"adapter":"lightroom-native-metadata-v1","source":request.settings.target.source,"retained_record":request.settings.target.retained_record,"input_digest":old.input_digest}),
        "old recipe is not the source-bound predecessor import"
    );
    Ok(())
}
