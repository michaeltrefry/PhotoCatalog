//! Explicit selected-run keyword recovery. Every replacement is committed with
//! its exact predecessor archive, ledger CAS and repair cursor. Originals and
//! the completed current-settings repair are never rewritten.
use super::{
    importer::{self, Outcome, Stage},
    organization::{self, Decision, NativeTarget, ProjectionResult, SourceRecord},
    organization_walk,
    walk::Walk,
};
use crate::{Catalog, catalog_writer::Priority, lightroom::migration_source::MigrationSource};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
};
const ADAPTER: &str = "lightroom-keyword-repair-v1";
const LIMIT: usize = 8 * 1024 * 1024;
const KEYWORDS: &str = "\"Keywords\"";
const MEMBERS: &str = "\"KeywordMemberships\"";
const SYNONYMS: &str = "\"KeywordSynonyms\"";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootProof {
    pub origin: SourceRecord,
    pub raw_digest: String,
    pub retained_table: i64,
    pub table_digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub expected_dictionaries: usize,
    pub expected_memberships: usize,
    pub expected_synonyms: usize,
    pub expected_captures: usize,
    pub run: String,
    pub expected_complete_progress_blake3: String,
    pub expected_mapping_epoch: i64,
    pub current_repair: String,
    pub expected_current_repair_progress_blake3: String,
    /// Chain digest defined by roster_next, starting at BLAKE3(ADAPTER bytes).
    pub expected_roster_blake3: String,
    pub predecessor_evidence_blake3: String,
    pub roots: Vec<RootProof>,
    pub reason: String,
}
impl Request {
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.expected_dictionaries,
            self.expected_memberships,
            self.expected_captures,
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    ArchiveReports,
    PlanDictionaries,
    PlanMemberships,
    Order,
    Dictionaries,
    Memberships,
    VerifyDictionaries,
    VerifyMemberships,
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
    pub order_index: i64,
    pub planned: usize,
    pub examined: usize,
    pub repaired: usize,
    pub retained: usize,
    pub verified: usize,
    pub roster_blake3: String,
    pub complete: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub progress: Progress,
    pub record: Option<i64>,
}
/// Exact predecessor bytes remain readable independently of the current model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Predecessor {
    pub origin: SourceRecord,
    pub old_outcome: Vec<u8>,
    pub old_receipt: Option<serde_json::Value>,
    pub planned_decision: Option<Decision>,
    pub disposition: String,
    pub new_outcome_digest: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Binding {
    adapter: String,
    request: Request,
    input: String,
    policy_blake3: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    source_identity: String,
    slot: String,
    owner: String,
    adapter: String,
    input_digest: String,
    result: String,
    retained_record: i64,
    proof: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Archive {
    origin: SourceRecord,
    outcome: Vec<u8>,
    receipt: Option<Receipt>,
    decision: Option<Decision>,
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS migration_keyword_repairs(
      id TEXT PRIMARY KEY,run TEXT NOT NULL UNIQUE REFERENCES migration_runs(id),binding BLOB NOT NULL,progress BLOB NOT NULL,
      original_progress BLOB NOT NULL,original_length INTEGER NOT NULL CHECK(original_length BETWEEN 0 AND 8388608),original_digest TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS migration_keyword_repair_items(
      repair TEXT NOT NULL REFERENCES migration_keyword_repairs(id),stage TEXT NOT NULL,record INTEGER NOT NULL,
      revision TEXT NOT NULL,parent INTEGER,ordering INTEGER,archive BLOB NOT NULL,raw_length INTEGER NOT NULL CHECK(raw_length BETWEEN 0 AND 8388608),digest TEXT NOT NULL,
      disposition TEXT NOT NULL,new_outcome_digest TEXT,new_receipts BLOB,
      PRIMARY KEY(repair,stage,record));
      CREATE INDEX IF NOT EXISTS migration_keyword_repair_order ON migration_keyword_repair_items(repair,stage,ordering,record);
      CREATE TABLE IF NOT EXISTS migration_keyword_repair_reports(
      repair TEXT NOT NULL REFERENCES migration_keyword_repairs(id),revision TEXT NOT NULL,archive BLOB NOT NULL,
      raw_length INTEGER NOT NULL CHECK(raw_length BETWEEN 0 AND 8388608),digest TEXT NOT NULL,epoch INTEGER NOT NULL,new_digest TEXT,new_epoch INTEGER,
      PRIMARY KEY(repair,revision));")?;
    Ok(())
}
fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    crate::lightroom::bounded_json(v, LIMIT)
}
fn digest(v: &[u8]) -> String {
    blake3::hash(v).to_hex().to_string()
}
fn hash(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn compress(v: &[u8]) -> Result<Vec<u8>> {
    ensure!(v.len() <= LIMIT, "keyword archive bound");
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(v)?;
    Ok(z.finish()?)
}
fn decompress(v: &[u8], len: i64, sha: &str) -> Result<Vec<u8>> {
    let n = usize::try_from(len)?;
    ensure!(
        n <= LIMIT && v.len() <= LIMIT + 65536,
        "keyword archive bound"
    );
    let mut out = Vec::new();
    ZlibDecoder::new(v)
        .take((n + 1) as u64)
        .read_to_end(&mut out)?;
    ensure!(
        out.len() == n && digest(&out) == sha,
        "keyword archive digest differs"
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
fn raw_progress(db: &Connection, run: &str) -> Result<Vec<u8>> {
    let b:Option<Vec<u8>>=db.query_row("SELECT CASE WHEN length(progress)<=8388608 THEN progress END FROM migration_runs WHERE id=?",[run],|r|r.get(0))?;
    b.context("run progress bound")
}
fn read(db: &Connection, id: &str) -> Result<(Binding, Progress)> {
    ensure!(hash(id), "keyword repair identity bound");
    let (a,b):(Option<Vec<u8>>,Option<Vec<u8>>)=db.query_row("SELECT CASE WHEN length(binding)<=8388608 THEN binding END,CASE WHEN length(progress)<=8388608 THEN progress END FROM migration_keyword_repairs WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let a = a.context("keyword binding bound")?;
    let b = b.context("keyword progress bound")?;
    let binding: Binding = serde_json::from_slice(&a)?;
    let p: Progress = serde_json::from_slice(&b)?;
    ensure!(
        digest(&a) == id
            && binding.adapter == ADAPTER
            && p.id == id
            && p.run == binding.request.run
            && p.input == binding.input
            && p.complete == (p.phase == Phase::Complete),
        "keyword repair binding differs"
    );
    Ok((binding, p))
}
pub fn read_progress(db: &Connection, id: &str) -> Result<Progress> {
    Ok(read(db, id)?.1)
}
pub(crate) fn require_owner(db: &Connection, run: &str, owner: Option<&str>) -> Result<()> {
    let id: Option<String> = db
        .query_row(
            "SELECT id FROM migration_keyword_repairs WHERE run=?",
            [run],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = id {
        let (_, p) = read(db, &id)?;
        if !p.complete {
            ensure!(
                owner == Some(id.as_str()) && p.phase == Phase::Reconciliation,
                "keyword repair pending; use its owning repair cursor"
            );
        } else {
            ensure!(
                owner.is_none() || owner == Some(id.as_str()),
                "keyword repair owner differs"
            );
        }
    } else {
        ensure!(owner.is_none(), "keyword repair owner absent");
    }
    Ok(())
}
fn current_complete(db: &Connection, r: &Request) -> Result<()> {
    let p = super::current_repair::read_progress(db, &r.current_repair)?;
    ensure!(
        p.run == r.run
            && p.complete
            && digest(&encode(&p)?) == r.expected_current_repair_progress_blake3,
        "prior current repair differs or is pending"
    );
    Ok(())
}
fn validate_request(r: &Request) -> Result<()> {
    for v in [
        &r.run,
        &r.expected_complete_progress_blake3,
        &r.current_repair,
        &r.expected_current_repair_progress_blake3,
        &r.expected_roster_blake3,
        &r.predecessor_evidence_blake3,
    ] {
        ensure!(hash(v), "keyword request digest bounds");
    }
    let (d, m, c) = r.counts();
    ensure!(
        d > 0
            && d <= 1024
            && m <= 10000
            && c > 0
            && c <= 16
            && r.roots.len() == c
            && r.expected_synonyms == 0
            && r.expected_mapping_epoch >= 0
            && !r.reason.trim().is_empty()
            && r.reason.len() <= 4096,
        "keyword repair scope bounds"
    );
    encode(r)?;
    Ok(())
}
fn validate_roots(db: &Connection, input: &str, r: &Request) -> Result<()> {
    let mut revisions = std::collections::BTreeSet::new();
    for root in &r.roots {
        ensure!(
            root.origin.source.table == "AgLibraryKeyword"
                && revisions.insert(root.origin.source.capture_revision.clone()),
            "keyword root identity/duplicate capture differs"
        );
        let mut evidence = organization::Evidence::default();
        evidence.source(db, &root.origin)?;
        let fields = super::images::columns(db, &mut evidence, &root.origin, root.retained_table)?;
        ensure!(
            organization::keyword_boundary_fields(&fields)
                && record_digest(db, input, root.origin.retained_record)? == root.raw_digest
                && record_digest(db, input, root.retained_table)? == root.table_digest,
            "keyword root raw/schema predicate differs"
        );
        let present:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM migration_run_items WHERE run=? AND stage=? AND record=? AND revision=?)",params![r.run,KEYWORDS,root.origin.retained_record,root.origin.source.capture_revision],|r|r.get(0))?;
        ensure!(
            present,
            "admitted keyword root is absent from run dictionary ledger"
        );
    }
    Ok(())
}
/// Read-only preflight used before schema 9→10 admission. Repeated requests must
/// match the immutable existing repair binding; no permissive stale-input path.
pub fn preflight(db: &Connection, input: &str, r: &Request) -> Result<()> {
    validate_request(r)?;
    let (p, policy) = importer::read(db, &r.run)?;
    ensure!(p.input == input, "keyword repair input differs");
    current_complete(db, r)?;
    validate_roots(db, input, r)?;
    let table:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='migration_keyword_repairs')",[],|r|r.get(0))?;
    if table {
        let id: Option<String> = db
            .query_row(
                "SELECT id FROM migration_keyword_repairs WHERE run=?",
                [&r.run],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = id {
            let expected = Binding {
                adapter: ADAPTER.into(),
                request: r.clone(),
                input: input.into(),
                policy_blake3: digest(&encode(&policy)?),
            };
            ensure!(
                digest(&encode(&expected)?) == id,
                "keyword repair request changed"
            );
            return Ok(());
        }
    }
    ensure!(
        p.complete
            && p.stage == Stage::Complete
            && digest(&raw_progress(db, &r.run)?) == r.expected_complete_progress_blake3
            && epoch(db)? == r.expected_mapping_epoch,
        "keyword repair completed predecessor differs"
    );
    for (stage, n) in [
        (KEYWORDS, r.counts().0),
        (MEMBERS, r.counts().1),
        (SYNONYMS, 0),
    ] {
        let actual: i64 = db.query_row(
            "SELECT count(*) FROM migration_run_items WHERE run=? AND stage=?",
            params![r.run, stage],
            |r| r.get(0),
        )?;
        ensure!(
            usize::try_from(actual)? == n,
            "keyword repair stage roster differs: {stage}"
        );
    }
    Ok(())
}
fn check(db: &Connection, b: &Binding, p: &Progress) -> Result<()> {
    let (run, policy) = importer::read(db, &p.run)?;
    ensure!(
        run.input == b.input
            && digest(&encode(&policy)?) == b.policy_blake3
            && epoch(db)? == b.request.expected_mapping_epoch,
        "keyword repair policy or mappings changed"
    );
    current_complete(db, &b.request)?;
    ensure!(
        encode(&read(db, &p.id)?.1)? == encode(p)?,
        "keyword repair cursor changed"
    );
    if !p.complete {
        ensure!(
            !run.complete && run.stage == Stage::Reconciliation,
            "keyword repair run state changed"
        );
    }
    Ok(())
}
fn advance(db: &Connection, a: &Progress, b: &Progress) -> Result<()> {
    ensure!(
        db.execute(
            "UPDATE migration_keyword_repairs SET progress=?3 WHERE id=?1 AND progress=?2",
            params![a.id, encode(a)?, encode(b)?]
        )? == 1,
        "keyword cursor changed"
    );
    Ok(())
}
fn receipt(db: &Connection, source: &str, slot: &str) -> Result<Option<Receipt>> {
    let v=db.query_row("SELECT source_identity,slot,owner,adapter,input_digest,CASE WHEN length(result)<=8388608 THEN result END,retained_record,CASE WHEN length(proof)<=8388608 THEN proof END FROM migration_organization WHERE source_identity=? AND slot=?",params![source,slot],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get::<_,Option<String>>(5)?,r.get(6)?,r.get::<_,Option<String>>(7)?))).optional()?;
    v.map(
        |(source_identity, slot, owner, adapter, input_digest, result, retained_record, proof)| {
            Ok(Receipt {
                source_identity,
                slot,
                owner,
                adapter,
                input_digest,
                result: result.context("keyword receipt result bound")?,
                retained_record,
                proof: proof.context("keyword receipt proof bound")?,
            })
        },
    )
    .transpose()
}
fn item(db: &Connection, run: &str, stage: &str, record: i64) -> Result<Vec<u8>> {
    let b:Option<Vec<u8>>=db.query_row("SELECT CASE WHEN length(outcome)<=8388608 THEN outcome END FROM migration_run_items WHERE run=? AND stage=? AND record=?",params![run,stage,record],|r|r.get(0))?;
    b.context("keyword ledger outcome bound")
}
fn archived(db: &Connection, id: &str, stage: &str, record: i64) -> Result<Archive> {
    let (b,n,d):(Option<Vec<u8>>,i64,String)=db.query_row("SELECT CASE WHEN length(archive)<=8454144 THEN archive END,raw_length,digest FROM migration_keyword_repair_items WHERE repair=? AND stage=? AND record=?",params![id,stage,record],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    Ok(serde_json::from_slice(&decompress(
        &b.context("keyword compressed bound")?,
        n,
        &d,
    )?)?)
}
fn record_digest(db: &Connection, input: &str, record: i64) -> Result<String> {
    Ok(db.query_row(
        "SELECT digest FROM migration_retained_records WHERE sequence=? AND input=? AND complete=1",
        params![record, input],
        |r| r.get(0),
    )?)
}
fn roster_next(prior: &str, stage: &str, record: i64, a: &Archive) -> Result<String> {
    Ok(digest(&encode(&(
        prior,
        stage,
        record,
        digest(&a.outcome),
        a.receipt
            .as_ref()
            .map(encode)
            .transpose()?
            .as_deref()
            .map(digest),
    ))?))
}
#[cfg(test)]
pub(crate) fn predecessor_roster_blake3(
    catalog: &Catalog,
    source: &MigrationSource,
    run: &str,
) -> Result<String> {
    let mut chain = digest(ADAPTER.as_bytes());
    let mut count = 0;
    for stage in [KEYWORDS, MEMBERS] {
        let mut after = 0;
        loop {
            let next:Option<(i64,String)>=catalog.db.query_row("SELECT record,revision FROM migration_run_items WHERE run=? AND stage=? AND record>? ORDER BY record LIMIT 1",params![run,stage,after],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let Some((record, revision)) = next else {
                break;
            };
            count += 1;
            ensure!(count <= 11024, "fixture roster bound");
            let origin = Walk::new(catalog, source, &revision)?.source_record(record)?;
            let old = receipt(
                &catalog.db,
                &origin.source.identity()?,
                if stage == KEYWORDS {
                    "dictionary"
                } else {
                    "keyword_membership"
                },
            )?;
            let archive = Archive {
                origin,
                outcome: item(&catalog.db, run, stage, record)?,
                receipt: old,
                decision: None,
            };
            chain = roster_next(&chain, stage, record, &archive)?;
            after = record;
        }
    }
    Ok(chain)
}

impl Catalog {
    pub fn begin_keyword_repair(
        &mut self,
        source: &MigrationSource,
        r: &Request,
    ) -> Result<Progress> {
        preflight(&self.db, source.binding_blake3(), r)?;
        let (before, policy) = importer::read(&self.db, &r.run)?;
        ensure!(
            source.seal().selected.len() == r.counts().2,
            "keyword selected capture roster differs"
        );
        let (seal,approval,complete):(Option<Vec<u8>>,Option<Vec<u8>>,bool)=self.db.query_row("SELECT CASE WHEN length(seal)<=8388608 THEN seal END,CASE WHEN length(approval)<=8388608 THEN approval END,complete FROM migration_retention WHERE id=?",[&before.input],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(
            complete
                && seal.context("keyword seal bound")? == encode(source.seal())?
                && digest(&approval.context("keyword approval bound")?)
                    == source.seal().approval.document_blake3,
            "keyword retained selection/approval differs"
        );
        let b = Binding {
            adapter: ADAPTER.into(),
            request: r.clone(),
            input: before.input.clone(),
            policy_blake3: digest(&encode(&policy)?),
        };
        let bytes = encode(&b)?;
        let id = digest(&bytes);
        let old: Option<String> = self
            .db
            .query_row(
                "SELECT id FROM migration_keyword_repairs WHERE run=?",
                [&r.run],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(id == old, "keyword request differs");
            return read_progress(&self.db, &id);
        }
        let mut revisions = std::collections::BTreeSet::new();
        for root in &r.roots {
            ensure!(
                revisions.insert(root.origin.source.capture_revision.clone()),
                "duplicate keyword boundary capture"
            );
            let walk = Walk::new(self, source, &root.origin.source.capture_revision)?;
            walk.source_id(&root.origin)?;
            ensure!(
                walk.schema("AgLibraryKeyword")? == root.retained_table
                    && record_digest(&self.db, &b.input, root.origin.retained_record)?
                        == root.raw_digest
                    && record_digest(&self.db, &b.input, root.retained_table)? == root.table_digest,
                "keyword boundary source pins differ"
            );
            ensure!(
                matches!(organization_walk::keyword_row(self,source,&root.origin)?,Decision::KeywordBoundary{retained_table} if retained_table==root.retained_table),
                "keyword boundary predicate differs"
            );
        }
        let original = raw_progress(&self.db, &r.run)?;
        let compressed = compress(&original)?;
        let p = Progress {
            id: id.clone(),
            run: r.run.clone(),
            input: b.input.clone(),
            phase: Phase::ArchiveReports,
            report_index: 0,
            after_record: 0,
            order_index: 0,
            planned: 0,
            examined: 0,
            repaired: 0,
            retained: 0,
            verified: 0,
            roster_blake3: digest(ADAPTER.as_bytes()),
            complete: false,
        };
        let mut pending = before.clone();
        pending.stage = Stage::Reconciliation;
        pending.complete = false;
        pending.capture_index = 0;
        pending.cursor = None;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        preflight(&tx, &b.input, r)?;
        ensure!(
            tx.execute(
                "UPDATE migration_runs SET progress=?3 WHERE id=?1 AND progress=?2",
                params![r.run, original, encode(&pending)?]
            )? == 1,
            "keyword predecessor changed before admission"
        );
        tx.execute(
            "INSERT INTO migration_keyword_repairs VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                id,
                r.run,
                bytes,
                encode(&p)?,
                compressed,
                i64::try_from(original.len())?,
                digest(&original)
            ],
        )?;
        tx.commit()?;
        Ok(p)
    }
    pub fn keyword_repair_predecessor(
        &self,
        id: &str,
        stage: Stage,
        record: i64,
    ) -> Result<Predecessor> {
        read(&self.db, id)?;
        let stage = match stage {
            Stage::Keywords => KEYWORDS,
            Stage::KeywordMemberships => MEMBERS,
            _ => anyhow::bail!("stage is outside keyword repair"),
        };
        let a = archived(&self.db, id, stage, record)?;
        let (disposition,new_outcome_digest)=self.db.query_row("SELECT disposition,new_outcome_digest FROM migration_keyword_repair_items WHERE repair=? AND stage=? AND record=?",params![id,stage,record],|r|Ok((r.get(0)?,r.get(1)?)))?;
        Ok(Predecessor {
            origin: a.origin,
            old_outcome: a.outcome,
            old_receipt: a.receipt.map(serde_json::to_value).transpose()?,
            planned_decision: a.decision,
            disposition,
            new_outcome_digest,
        })
    }
    pub fn keyword_repair_progress(&self, id: &str) -> Result<Progress> {
        read_progress(&self.db, id)
    }
    pub fn step_keyword_repair(&mut self, source: &MigrationSource, id: &str) -> Result<Step> {
        let (b, p) = read(&self.db, id)?;
        ensure!(
            source.binding_blake3() == b.input,
            "keyword source binding differs"
        );
        check(&self.db, &b, &p)?;
        match p.phase {
            Phase::ArchiveReports => self.keyword_archive_report(source, &b, &p),
            Phase::PlanDictionaries | Phase::PlanMemberships => self.keyword_plan(source, &b, &p),
            Phase::Order => self.keyword_order(&b, &p),
            Phase::Dictionaries | Phase::Memberships => self.keyword_project(source, &b, &p),
            Phase::VerifyDictionaries | Phase::VerifyMemberships => self.keyword_verify(&b, &p),
            Phase::Reconciliation => {
                let run = importer::read(&self.db, &p.run)?.0;
                super::reconciliation::step_keyword(self, source, &run, &p.id)?;
                Ok(Step {
                    progress: read_progress(&self.db, &p.id)?,
                    record: None,
                })
            }
            Phase::Complete => Ok(Step {
                progress: p,
                record: None,
            }),
        }
    }
    fn keyword_move(&mut self, b: &Binding, p: &Progress, phase: Phase) -> Result<Step> {
        let mut n = p.clone();
        n.phase = phase;
        n.after_record = 0;
        n.order_index = 0;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check(&tx, b, p)?;
        advance(&tx, p, &n)?;
        tx.commit()?;
        Ok(Step {
            progress: n,
            record: None,
        })
    }
    fn keyword_archive_report(
        &mut self,
        source: &MigrationSource,
        b: &Binding,
        p: &Progress,
    ) -> Result<Step> {
        if p.report_index == source.seal().selected.len() {
            return self.keyword_move(b, p, Phase::PlanDictionaries);
        }
        let revision = &source.seal().selected[p.report_index].revision;
        let (raw,e):(Option<Vec<u8>>,i64)=self.db.query_row("SELECT CASE WHEN length(report)<=8388608 THEN report END,epoch FROM migration_reconciliation WHERE run=? AND revision=?",params![p.run,revision],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let raw = raw.context("keyword predecessor report bound")?;
        ensure!(
            e == b.request.expected_mapping_epoch,
            "keyword predecessor report epoch differs"
        );
        let packed = compress(&raw)?;
        let mut n = p.clone();
        n.report_index += 1;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check(&tx, b, p)?;
        tx.execute(
            "INSERT INTO migration_keyword_repair_reports VALUES(?1,?2,?3,?4,?5,?6,NULL,NULL)",
            params![
                p.id,
                revision,
                packed,
                i64::try_from(raw.len())?,
                digest(&raw),
                e
            ],
        )?;
        ensure!(tx.execute("DELETE FROM migration_reconciliation WHERE run=? AND revision=? AND report=? AND epoch=?",params![p.run,revision,raw,e])?==1,"keyword report predecessor changed");
        advance(&tx, p, &n)?;
        tx.commit()?;
        Ok(Step {
            progress: n,
            record: None,
        })
    }
    fn keyword_plan(
        &mut self,
        source: &MigrationSource,
        b: &Binding,
        p: &Progress,
    ) -> Result<Step> {
        let dict = p.phase == Phase::PlanDictionaries;
        let stage = if dict { KEYWORDS } else { MEMBERS };
        let next:Option<(i64,String)>=self.db.query_row("SELECT record,revision FROM migration_run_items WHERE run=? AND stage=? AND record>? ORDER BY record LIMIT 1",params![p.run,stage,p.after_record],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((record, revision)) = next else {
            return self.keyword_move(
                b,
                p,
                if dict {
                    Phase::PlanMemberships
                } else {
                    Phase::Order
                },
            );
        };
        let walk = Walk::new(self, source, &revision)?;
        let origin = walk.source_record(record)?;
        walk.source_id(&origin)?;
        let raw = item(&self.db, &p.run, stage, record)?;
        let outcome: Outcome = serde_json::from_slice(&raw)?;
        let identity = origin.source.identity()?;
        let old = receipt(
            &self.db,
            &identity,
            if dict {
                "dictionary"
            } else {
                "keyword_membership"
            },
        )?;
        let (decision, parent) = if dict {
            let old = old
                .as_ref()
                .context("keyword dictionary predecessor absent")?;
            ensure!(
                old.owner == importer::read(&self.db, &p.run)?.1.import_source
                    && old.retained_record == record
                    && old.slot == "dictionary"
                    && old.adapter == "lightroom-organization-columns-v2",
                "keyword predecessor ownership differs"
            );
            let result: ProjectionResult = serde_json::from_str(&old.result)?;
            ensure!(
                matches!(result.target, NativeTarget::Retained { .. }),
                "keyword predecessor is not retained"
            );
            ensure!(
                matches!(&outcome,Outcome::Organization(v) if v.len()==1 && encode(&v[0]).ok()==encode(&result).ok()),
                "keyword old ledger does not match its sole dictionary receipt"
            );
            ensure!(
                receipt(&self.db, &identity, "keyword_behavior")?.is_none(),
                "keyword predecessor has an unexpected behavior receipt"
            );
            let decision = organization_walk::keyword_row(self, source, &origin)?;
            let parent = match &decision {
                Decision::Keyword { parent, .. } => {
                    parent.as_ref().map(|v| v.target.retained_record)
                }
                Decision::KeywordBoundary { .. } => {
                    ensure!(
                        b.request
                            .roots
                            .iter()
                            .any(|r| encode(&r.origin).ok() == encode(&origin).ok()),
                        "keyword boundary outside admitted roster"
                    );
                    None
                }
                _ => unreachable!(),
            };
            (Some(decision), parent)
        } else {
            ensure!(
                matches!(outcome, Outcome::Retained { .. })
                    && old.is_none()
                    && origin.source.table == "AgLibraryKeywordImage",
                "keyword membership predecessor must be ledger-only retained"
            );
            (None, None)
        };
        let a = Archive {
            origin,
            outcome: raw,
            receipt: old,
            decision,
        };
        let bytes = encode(&a)?;
        let packed = compress(&bytes)?;
        let mut n = p.clone();
        n.after_record = record;
        n.planned += 1;
        n.roster_blake3 = roster_next(&p.roster_blake3, stage, record, &a)?;
        ensure!(
            n.planned <= b.request.counts().0 + b.request.counts().1,
            "keyword planning roster exceeded"
        );
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check(&tx, b, p)?;
        check_old(&tx, p, stage, record, &a)?;
        tx.execute("INSERT INTO migration_keyword_repair_items VALUES(?1,?2,?3,?4,?5,NULL,?6,?7,?8,'planned',NULL,NULL)",params![p.id,stage,record,revision,parent,packed,i64::try_from(bytes.len())?,digest(&bytes)])?;
        advance(&tx, p, &n)?;
        tx.commit()?;
        Ok(Step {
            progress: n,
            record: Some(record),
        })
    }
    fn keyword_order(&mut self, b: &Binding, p: &Progress) -> Result<Step> {
        ensure!(
            p.planned == b.request.counts().0 + b.request.counts().1
                && p.roster_blake3 == b.request.expected_roster_blake3,
            "keyword planned roster identity differs"
        );
        let mut nodes = BTreeMap::new();
        {
            let mut q=self.db.prepare("SELECT record,parent,revision FROM migration_keyword_repair_items WHERE repair=? AND stage=? ORDER BY record LIMIT 1025")?;
            let mut rows = q.query(params![p.id, KEYWORDS])?;
            while let Some(r) = rows.next()? {
                nodes.insert(
                    r.get::<_, i64>(0)?,
                    (r.get::<_, Option<i64>>(1)?, r.get::<_, String>(2)?),
                );
            }
        }
        ensure!(
            nodes.len() == b.request.counts().0 && encode(&nodes)?.len() <= LIMIT,
            "keyword dictionary plan bound"
        );
        for root in &b.request.roots {
            let record = root.origin.retained_record;
            ensure!(
                nodes.contains_key(&record),
                "admitted keyword root absent from archived dictionary plan"
            );
            let a = archived(&self.db, &p.id, KEYWORDS, record)?;
            ensure!(
                encode(&a.origin)? == encode(&root.origin)?
                    && matches!(a.decision,Some(Decision::KeywordBoundary {retained_table}) if retained_table==root.retained_table),
                "admitted keyword root archive differs"
            );
        }
        let mut order = BTreeMap::new();
        let mut depths = BTreeMap::new();
        while order.len() < nodes.len() {
            let mut changed = false;
            for (record, (parent, revision)) in &nodes {
                if order.contains_key(record) {
                    continue;
                }
                let depth = if let Some(parent) = parent {
                    let (_, pr) = nodes
                        .get(parent)
                        .context("keyword parent outside admitted dictionary roster")?;
                    ensure!(pr == revision, "keyword parent crosses capture");
                    let Some(d) = depths.get(parent) else {
                        continue;
                    };
                    d + 1
                } else {
                    0
                };
                ensure!(depth <= 64, "keyword hierarchy exceeds native bound");
                depths.insert(*record, depth);
                order.insert(*record, order.len() as i64 + 1);
                changed = true;
            }
            ensure!(changed, "keyword dictionary plan contains a cycle");
        }
        let mut n = p.clone();
        n.phase = Phase::Dictionaries;
        n.order_index = 0;
        n.after_record = 0;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check(&tx, b, p)?;
        for (record, index) in order {
            ensure!(tx.execute("UPDATE migration_keyword_repair_items SET ordering=?4 WHERE repair=?1 AND stage=?2 AND record=?3 AND ordering IS NULL",params![p.id,KEYWORDS,record,index])?==1,"keyword order changed");
        }
        advance(&tx, p, &n)?;
        tx.commit()?;
        Ok(Step {
            progress: n,
            record: None,
        })
    }
    fn keyword_project(
        &mut self,
        source: &MigrationSource,
        b: &Binding,
        p: &Progress,
    ) -> Result<Step> {
        let dict = p.phase == Phase::Dictionaries;
        let stage = if dict { KEYWORDS } else { MEMBERS };
        let sql = if dict {
            "SELECT record,ordering FROM migration_keyword_repair_items WHERE repair=? AND stage=? AND ordering>? ORDER BY ordering,record LIMIT 1"
        } else {
            "SELECT record,record FROM migration_keyword_repair_items WHERE repair=? AND stage=? AND record>? ORDER BY record LIMIT 1"
        };
        let next: Option<(i64, i64)> = self
            .db
            .query_row(
                sql,
                params![
                    p.id,
                    stage,
                    if dict { p.order_index } else { p.after_record }
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((record, index)) = next else {
            return self.keyword_move(
                b,
                p,
                if dict {
                    Phase::Memberships
                } else {
                    Phase::VerifyDictionaries
                },
            );
        };
        let a = archived(&self.db, &p.id, stage, record)?;
        check_old(&self.db, p, stage, record, &a)?;
        let policy = importer::read(&self.db, &p.run)?.1;
        let mut requests = Vec::new();
        let mut source_parent = None;
        let mut retained = None;
        if dict {
            let mut decision = organization_walk::keyword_row(self, source, &a.origin)?;
            ensure!(
                encode(&Some(decision.clone()))? == encode(&a.decision)?,
                "keyword source decision changed after planning"
            );
            // Compare the actual parent hierarchy to the source-derived planned chain.
            source_parent = check_parent_path(&self.db, p, &policy, &decision)?;
            organization_walk::keyword_overlap(self, &policy, &mut decision)?;
            let boundary = matches!(decision, Decision::KeywordBoundary { .. });
            requests.push(organization_walk::keyword_request(
                &policy, &a.origin, decision,
            ));
            if !boundary {
                requests.push(organization_walk::keyword_behavior(&policy, &a.origin));
            }
        } else {
            match organization_walk::keyword_member(self, source, &policy, &a.origin)? {
                Ok(request) => requests.push(request),
                Err(reason) => retained = Some(reason),
            }
        }
        let mut prepared = Vec::new();
        for r in &requests {
            let value = self.prepare_keyword_projection(source, r)?;
            if matches!(r.decision, Decision::Keyword { .. })
                && let Some(path) = &source_parent
            {
                value.require_source_parent_path(path)?;
            }
            prepared.push(value);
        }
        let mut n = p.clone();
        n.after_record = record;
        n.order_index = index;
        n.examined += 1;
        if retained.is_some() {
            n.retained += 1;
        } else {
            n.repaired += 1;
        }
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check(&tx, b, p)?;
        check_old(&tx, p, stage, record, &a)?;
        if let Some(old) = &a.receipt {
            ensure!(tx.execute("DELETE FROM migration_organization WHERE source_identity=? AND slot=? AND input_digest=? AND result=? AND proof=?",params![old.source_identity,old.slot,old.input_digest,old.result,old.proof])?==1,"keyword predecessor changed before replacement");
        }
        let mut results = Vec::new();
        for prepared in prepared {
            results.push(organization::commit_keyword_projection(&tx, prepared)?);
        }
        let new = if let Some(reason) = retained {
            Outcome::Retained { reason }
        } else {
            Outcome::Organization(results.clone())
        };
        let raw = encode(&new)?;
        ensure!(tx.execute("UPDATE migration_run_items SET outcome=?4 WHERE run=?1 AND stage=?2 AND record=?3 AND outcome=?5",params![p.run,stage,record,raw,a.outcome])?==1,"keyword ledger changed before replacement");
        let receipts = results
            .iter()
            .map(|r| {
                Ok((
                    r.source_identity.clone(),
                    r.slot.clone(),
                    digest(&encode(r)?),
                    digest(&encode(
                        &receipt(&tx, &r.source_identity, &r.slot)?
                            .context("new keyword receipt absent")?,
                    )?),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(tx.execute("UPDATE migration_keyword_repair_items SET disposition=?4,new_outcome_digest=?5,new_receipts=?6 WHERE repair=?1 AND stage=?2 AND record=?3 AND disposition='planned'",params![p.id,stage,record,if matches!(new,Outcome::Retained{..}){"retained"}else{"projected"},digest(&raw),encode(&receipts)?])?==1,"keyword archive state changed");
        advance(&tx, p, &n)?;
        tx.commit()?;
        Ok(Step {
            progress: n,
            record: Some(record),
        })
    }
    fn keyword_verify(&mut self, b: &Binding, p: &Progress) -> Result<Step> {
        let dict = p.phase == Phase::VerifyDictionaries;
        let stage = if dict { KEYWORDS } else { MEMBERS };
        let next:Option<VerificationItem>=self.db.query_row("SELECT record,disposition,new_outcome_digest,CASE WHEN length(new_receipts)<=8388608 THEN new_receipts END FROM migration_keyword_repair_items WHERE repair=? AND stage=? AND record>? ORDER BY record LIMIT 1",params![p.id,stage,p.after_record],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let Some((record, state, expected, refs)) = next else {
            if !dict {
                ensure!(
                    p.verified == p.planned && p.examined == p.planned,
                    "keyword complete archive coverage differs"
                );
            }
            return self.keyword_move(
                b,
                p,
                if dict {
                    Phase::VerifyMemberships
                } else {
                    Phase::Reconciliation
                },
            );
        };
        ensure!(
            state == "projected" || state == "retained",
            "keyword archive item incomplete"
        );
        let a = archived(&self.db, &p.id, stage, record)?;
        let raw = item(&self.db, &p.run, stage, record)?;
        ensure!(
            Some(digest(&raw)) == expected,
            "keyword repaired ledger digest differs"
        );
        let refs: Vec<(String, String, String, String)> =
            serde_json::from_slice(&refs.context("keyword receipt roster bound")?)?;
        let outcome: Outcome = serde_json::from_slice(&raw)?;
        let results = match outcome {
            Outcome::Organization(v) => v,
            Outcome::Retained { .. } if state == "retained" => vec![],
            _ => anyhow::bail!("keyword repaired outcome differs"),
        };
        ensure!(
            results.len() == refs.len(),
            "keyword repaired receipt coverage differs"
        );
        for ((identity, slot, sha, full_sha), result) in refs.iter().zip(&results) {
            ensure!(
                identity == &a.origin.source.identity()?
                    && identity == &result.source_identity
                    && slot == &result.slot
                    && digest(&encode(result)?) == *sha,
                "keyword repaired receipt identity differs"
            );
            let actual =
                receipt(&self.db, identity, slot)?.context("keyword repaired receipt absent")?;
            ensure!(
                digest(&encode(&serde_json::from_str::<ProjectionResult>(
                    &actual.result
                )?)?)
                    == *sha
                    && digest(&encode(&actual)?) == *full_sha,
                "keyword repaired native receipt differs"
            );
        }
        let mut n = p.clone();
        n.after_record = record;
        n.verified += 1;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check(&tx, b, p)?;
        ensure!(
            item(&tx, &p.run, stage, record)? == raw,
            "keyword ledger changed during coverage check"
        );
        advance(&tx, p, &n)?;
        tx.commit()?;
        Ok(Step {
            progress: n,
            record: Some(record),
        })
    }
}
type VerificationItem = (i64, String, Option<String>, Option<Vec<u8>>);
fn check_old(db: &Connection, p: &Progress, stage: &str, record: i64, a: &Archive) -> Result<()> {
    ensure!(
        item(db, &p.run, stage, record)? == a.outcome,
        "keyword old ledger changed"
    );
    let actual = receipt(
        db,
        &a.origin.source.identity()?,
        if stage == KEYWORDS {
            "dictionary"
        } else {
            "keyword_membership"
        },
    )?;
    ensure!(
        encode(&actual)? == encode(&a.receipt)?,
        "keyword old native receipt changed"
    );
    if stage == KEYWORDS {
        ensure!(
            receipt(db, &a.origin.source.identity()?, "keyword_behavior")?.is_none(),
            "keyword behavior receipt appeared"
        );
    }
    Ok(())
}
fn check_parent_path(
    db: &Connection,
    p: &Progress,
    policy: &importer::Policy,
    decision: &Decision,
) -> Result<Option<Vec<String>>> {
    if let Decision::Keyword {
        parent: Some(parent),
        ..
    } = decision
    {
        let mut next = Some(parent.target.retained_record);
        let mut names = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        while let Some(record) = next {
            ensure!(
                seen.insert(record) && seen.len() <= 64,
                "keyword planned parent cycle/bound"
            );
            let a = archived(db, &p.id, KEYWORDS, record)?;
            match a.decision.context("keyword planned decision absent")? {
                Decision::Keyword { name, parent, .. } => {
                    names.push(name);
                    next = parent.map(|l| l.target.retained_record);
                }
                Decision::KeywordBoundary { .. } => next = None,
                _ => anyhow::bail!("keyword planned parent not a dictionary"),
            }
        }
        names.reverse();
        let (kind, path) =
            organization::keyword_parent_path(db, &policy.import_source, &parent.target)?;
        ensure!(
            kind == crate::organization::KeywordKind::Hierarchical && path == names,
            "keyword parent native hierarchy changed from source"
        );
        return Ok(Some(names));
    }
    Ok(None)
}
/// Called only from the owning reconciliation report transaction.
pub(crate) fn report_committed(
    tx: &Transaction<'_>,
    id: &str,
    revision: &str,
    bytes: &[u8],
    epoch: i64,
) -> Result<()> {
    let (_, p) = read(tx, id)?;
    require_owner(tx, &p.run, Some(id))?;
    ensure!(tx.execute("UPDATE migration_keyword_repair_reports SET new_digest=?3,new_epoch=?4 WHERE repair=?1 AND revision=?2",params![id,revision,digest(bytes),epoch])?==1,"fresh keyword report outside archived roster");
    Ok(())
}
pub(crate) fn reports_reset(tx: &Transaction<'_>, id: &str) -> Result<()> {
    tx.execute(
        "UPDATE migration_keyword_repair_reports SET new_digest=NULL,new_epoch=NULL WHERE repair=?",
        [id],
    )?;
    Ok(())
}
/// Run Complete and repair Complete are published by the same transaction.
pub(crate) fn finish(tx: &Transaction<'_>, id: &str) -> Result<()> {
    let (b, p) = read(tx, id)?;
    require_owner(tx, &p.run, Some(id))?;
    ensure!(
        p.verified == p.planned
            && p.examined == p.planned
            && p.planned == b.request.counts().0 + b.request.counts().1,
        "keyword final coverage differs"
    );
    for (stage, n) in [
        (KEYWORDS, b.request.counts().0),
        (MEMBERS, b.request.counts().1),
        (SYNONYMS, 0),
    ] {
        let actual: i64 = tx.query_row(
            "SELECT count(*) FROM migration_run_items WHERE run=? AND stage=?",
            params![p.run, stage],
            |r| r.get(0),
        )?;
        ensure!(
            usize::try_from(actual)? == n,
            "keyword final stage roster differs"
        );
    }
    let mut q=tx.prepare("SELECT a.revision,a.new_digest,a.new_epoch,CASE WHEN length(r.report)<=8388608 THEN r.report END,r.epoch FROM migration_keyword_repair_reports a LEFT JOIN migration_reconciliation r ON r.run=?2 AND r.revision=a.revision WHERE a.repair=?1 ORDER BY a.revision LIMIT 17")?;
    let mut rows = q.query(params![id, p.run])?;
    let mut count = 0;
    while let Some(r) = rows.next()? {
        let expected: Option<String> = r.get(1)?;
        let e: Option<i64> = r.get(2)?;
        let bytes: Option<Vec<u8>> = r.get(3)?;
        let bytes = bytes.context("fresh keyword report absent/bounded")?;
        ensure!(
            Some(digest(&bytes)) == expected
                && e == r.get::<_, Option<i64>>(4)?
                && e == Some(b.request.expected_mapping_epoch),
            "fresh keyword report identity differs"
        );
        count += 1;
    }
    ensure!(
        count == b.request.counts().2,
        "fresh keyword report roster differs"
    );
    let mut n = p.clone();
    n.phase = Phase::Complete;
    n.complete = true;
    advance(tx, &p, &n)
}

#[cfg(test)]
#[path = "keyword_repair_wire_tests.rs"]
mod wire_tests;
