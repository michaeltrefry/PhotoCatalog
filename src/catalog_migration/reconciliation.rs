//! Capture-level reconciliation is a separate durable stage. It compares the
//! sealed source roster with completed destination custody and every native walk.
use super::importer::{self, Progress, Stage, Step};
use crate::{
    Catalog,
    catalog_writer::Priority,
    lightroom::migration_source::{Collection, MigrationSource},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureReport {
    pub revision: String,
    pub collections: BTreeMap<String, u64>,
    pub walked: BTreeMap<String, u64>,
    pub classifications: BTreeMap<String, u64>,
    pub native_images: u64,
    pub native_original_mappings: u64,
    pub raw_artifacts: usize,
    pub supplements: Vec<SupplementReport>,
    pub mapping_epoch: i64,
    pub adobe_rendering_equivalent: bool,
    pub native_collection_order_equivalent: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SupplementReport {
    pub source_id: String,
    pub origin: String,
    pub proof_blake3: String,
    pub evidence: String,
    pub state: String,
    pub projection_input_digest: Option<String>,
    pub reason: Option<String>,
}
// Both lookup indexes share input/revision/collection. Seek the exact source ID
// rather than scanning every AgLibraryFile row before checking that ID.
const SUPPLEMENT_PROJECTION: &str = "SELECT m.input_digest,m.result FROM migration_record_lookup l INDEXED BY migration_lookup_source CROSS JOIN migration_file_metadata m ON m.retained_file=l.record WHERE m.owner=?1 AND m.origin=?2 AND m.supplement=?3 AND l.input=?4 AND l.revision=?5 AND l.collection=3 AND l.table_name='AgLibraryFile' AND l.source_id=?6 LIMIT 2";

// Keep the source outcome out of the GROUP BY sorter. The materialized row
// contains only the classification, with its original SQLite value type.
const CLASSIFICATIONS: &str = "WITH classes AS MATERIALIZED (
    SELECT COALESCE(json_extract(outcome,'$.Retained.reason'),json_extract(outcome,'$.Metadata.state'),json_extract(outcome,'$.FileMetadata.state'),json_extract(outcome,'$.Image.kind'),'projected_or_role_skipped') classification
    FROM migration_run_items WHERE run=?1 AND stage=?2 AND revision=?3)
    SELECT classification,count(*) FROM classes GROUP BY classification LIMIT 257";

/// Every sealed pin has verified generic payload custody, plus a separately
/// reported projection state. Unprojected evidence is explicit, never omitted.
pub(crate) fn supplement_reports(
    catalog: &Catalog,
    source: &MigrationSource,
    before: &Progress,
    policy: &importer::Policy,
    revision: &str,
) -> Result<Vec<SupplementReport>> {
    let pins = source
        .seal()
        .supplements
        .iter()
        .filter(|p| p.revision == revision)
        .collect::<Vec<_>>();
    let count: i64 = catalog
        .db
        .query_row(
            "SELECT count(*) FROM migration_run_supplements WHERE run=?1 AND revision=?2",
            params![before.id, revision],
            |r| r.get(0),
        )
        .context("count supplemental custody receipts")?;
    ensure!(
        usize::try_from(count)? == pins.len(),
        "supplement custody receipt roster differs"
    );
    let mut reports = Vec::new();
    let mut report_bytes = 0usize;
    for pin in pins {
        let member = policy
            .supplements
            .iter()
            .find(|s| {
                s.capture_revision == pin.revision
                    && s.source_id == pin.source_id
                    && s.origin == super::file_metadata::Origin::Embedded
            })
            .context("supplement policy member missing at reconciliation")?;
        let (hash,evidence,semantic):(String,String,String)=catalog.db.query_row("SELECT proof,evidence,semantic FROM migration_run_supplements WHERE run=?1 AND revision=?2 AND source_id=?3 AND origin=?4",params![before.id,revision,pin.source_id,pin.origin],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))
            .with_context(|| format!("read supplemental custody receipt source_id={} origin={}", pin.source_id, pin.origin))?;
        ensure!(
            hash == pin.proof_blake3 && evidence == member.evidence,
            "supplement custody proof differs"
        );
        let mut statement = catalog
            .db
            .prepare(SUPPLEMENT_PROJECTION)
            .context("prepare supplemental projection lookup")?;
        let mut rows = statement
            .query(params![
                policy.import_source,
                pin.origin,
                semantic,
                before.input,
                revision,
                pin.source_id
            ])
            .context("query supplemental projection lookup")?;
        let projection = if let Some(row) = rows.next().with_context(|| {
            format!(
                "read supplemental projection source_id={} origin={}",
                pin.source_id, pin.origin
            )
        })? {
            let value = row.get_ref(1)?;
            let bytes = value.as_bytes()?;
            ensure!(
                bytes.len() <= 8 * 1024 * 1024,
                "supplement projection receipt bound"
            );
            let result: super::file_metadata::ProjectionResult = serde_json::from_slice(bytes)?;
            ensure!(
                result.input_digest == row.get::<_, String>(0)?
                    && result.historical_status == Some(pin.historical_status),
                "supplement projection receipt association differs"
            );
            Some(result)
        } else {
            None
        };
        ensure!(
            rows.next()
                .with_context(|| format!(
                    "check supplemental projection ambiguity source_id={} origin={}",
                    pin.source_id, pin.origin
                ))?
                .is_none(),
            "supplement projection association ambiguous"
        );
        let report = SupplementReport {
            source_id: pin.source_id.clone(),
            origin: pin.origin.clone(),
            proof_blake3: hash,
            evidence,
            state: projection
                .as_ref()
                .map(|p| p.state.clone())
                .unwrap_or_else(|| "retained_only_unprojected".into()),
            projection_input_digest: projection.as_ref().map(|p| p.input_digest.clone()),
            reason: projection.and_then(|p| p.reason),
        };
        let mut report = report;
        if report.projection_input_digest.is_none() {
            report.reason=Some("No exact supplemental projection receipt for this selected input/file; verified complete proof and payloads retained without claiming native projection".into());
        }
        report_bytes = report_bytes
            .checked_add(crate::lightroom::bounded_json(&report, 16384)?.len())
            .context("supplement report byte overflow")?;
        ensure!(
            report_bytes <= 2 * 1024 * 1024,
            "supplement report aggregate bound"
        );
        reports.push(report);
    }
    Ok(reports)
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS migration_reconciliation(
      run TEXT NOT NULL REFERENCES migration_runs(id),revision TEXT NOT NULL,
      report BLOB NOT NULL,epoch INTEGER NOT NULL,PRIMARY KEY(run,revision));
      CREATE TABLE IF NOT EXISTS migration_mapping_epoch(id INTEGER PRIMARY KEY CHECK(id=1),epoch INTEGER NOT NULL);
      INSERT OR IGNORE INTO migration_mapping_epoch VALUES(1,0);
      CREATE TRIGGER IF NOT EXISTS migration_image_mapping_insert AFTER INSERT ON image_import_map BEGIN UPDATE migration_mapping_epoch SET epoch=epoch+1 WHERE id=1; END;
      CREATE TRIGGER IF NOT EXISTS migration_image_mapping_update AFTER UPDATE ON image_import_map BEGIN UPDATE migration_mapping_epoch SET epoch=epoch+1 WHERE id=1; END;
      CREATE TRIGGER IF NOT EXISTS migration_image_mapping_delete AFTER DELETE ON image_import_map BEGIN UPDATE migration_mapping_epoch SET epoch=epoch+1 WHERE id=1; END;
      CREATE TRIGGER IF NOT EXISTS migration_original_mapping_insert AFTER INSERT ON migration_originals BEGIN UPDATE migration_mapping_epoch SET epoch=epoch+1 WHERE id=1; END;
      CREATE TRIGGER IF NOT EXISTS migration_original_mapping_update AFTER UPDATE ON migration_originals BEGIN UPDATE migration_mapping_epoch SET epoch=epoch+1 WHERE id=1; END;
      CREATE TRIGGER IF NOT EXISTS migration_original_mapping_delete AFTER DELETE ON migration_originals BEGIN UPDATE migration_mapping_epoch SET epoch=epoch+1 WHERE id=1; END;
      CREATE INDEX IF NOT EXISTS migration_original_capture ON migration_originals(import_source,json_extract(source_json,'$.capture_revision'));")?;
    Ok(())
}
// Complete is constrained to 0/1. Count all matching index entries, then subtract
// only matching incomplete rows. The total stays index-only instead of fetching
// every completed record's payload-bearing table row to inspect `complete`.
const RETAINED_COMPLETE_COUNT: &str = "SELECT
    (SELECT count(*) FROM migration_retained_records
     WHERE input=?1 AND revision=?2 AND collection=?3) -
    (SELECT count(*) FROM migration_retained_records INDEXED BY migration_retained_pending
     WHERE input=?1 AND revision=?2 AND collection=?3 AND complete=0)";

const FIRST_RETAINED_REVISION: &str = "SELECT revision FROM migration_retained_records
    INDEXED BY migration_retained_page WHERE input=?1 ORDER BY revision LIMIT 1";
const NEXT_RETAINED_REVISION: &str = "SELECT revision FROM migration_retained_records
    INDEXED BY migration_retained_page WHERE input=?1 AND revision>?2 ORDER BY revision LIMIT 1";

fn validate_retained_roster(
    db: &Connection,
    input: &str,
    max_revisions: usize,
    mut allowed: impl FnMut(&str) -> bool,
) -> Result<()> {
    // Preserve the old single DISTINCT statement's read snapshot across seeks.
    // Release it before the caller's existing writer admission and epoch CAS.
    let tx = db.unchecked_transaction()?;
    {
        let mut first = tx.prepare(FIRST_RETAINED_REVISION)?;
        let mut next = tx.prepare(NEXT_RETAINED_REVISION)?;
        let mut previous: Option<String> = None;
        let mut observed = 0;
        loop {
            let revision: Option<String> = match &previous {
                None => first.query_row([input], |r| r.get(0)).optional()?,
                Some(previous) => next
                    .query_row(params![input, previous], |r| r.get(0))
                    .optional()?,
            };
            let Some(revision) = revision else { break };
            // At most max_revisions present keys plus one mandatory empty seek.
            // Never silently stop after the last expected key: a trailing extra
            // (including an incomplete retained record) must still be rejected.
            ensure!(
                observed < max_revisions && allowed(&revision),
                "excluded or unselected records entered destination custody"
            );
            observed += 1;
            previous = Some(revision);
        }
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn step(
    catalog: &mut Catalog,
    source: &MigrationSource,
    before: &Progress,
) -> Result<Step> {
    step_owned(catalog, source, before, None)
}
pub(crate) fn step_keyword(
    catalog: &mut Catalog,
    source: &MigrationSource,
    before: &Progress,
    owner: &str,
) -> Result<Step> {
    step_owned(catalog, source, before, Some(owner))
}
fn step_owned(
    catalog: &mut Catalog,
    source: &MigrationSource,
    before: &Progress,
    owner: Option<&str>,
) -> Result<Step> {
    super::keyword_repair::require_owner(&catalog.db, &before.id, owner)?;
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(30);
    let mut ticks = 0u64;
    catalog
        .db
        .progress_handler(
            1000,
            Some(move || {
                ticks += 1;
                ticks > 100000 || std::time::Instant::now() >= deadline
            }),
        )
        .context("install reconciliation destination query budget")?;
    let result = step_inner(catalog, source, before, owner);
    catalog
        .db
        .progress_handler(0, None::<fn() -> bool>)
        .context("remove reconciliation destination query budget")?;
    result.with_context(|| {
        format!(
            "reconciliation capture_index={} revision={} elapsed_seconds={:.3}",
            before.capture_index,
            source
                .seal()
                .selected
                .get(before.capture_index)
                .map(|capture| capture.revision.as_str())
                .unwrap_or("final-roster"),
            started.elapsed().as_secs_f64()
        )
    })
}
fn epoch(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "SELECT epoch FROM migration_mapping_epoch WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}
fn step_inner(
    catalog: &mut Catalog,
    source: &MigrationSource,
    before: &Progress,
    owner: Option<&str>,
) -> Result<Step> {
    let snapshot_epoch = epoch(&catalog.db).context("read reconciliation mapping epoch")?;
    let stale: bool = catalog
        .db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM migration_reconciliation WHERE run=?1 AND epoch<>?2)",
            params![before.id, snapshot_epoch],
            |r| r.get(0),
        )
        .context("check stale reconciliation reports")?;
    if stale {
        let mut reset = before.clone();
        reset.capture_index = 0;
        let old = serde_json::to_vec(before)?;
        let new = serde_json::to_vec(&reset)?;
        let _permit = catalog.writers.enter(Priority::Background)?;
        let tx = catalog
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin reconciliation report/cursor transaction")?;
        super::keyword_repair::require_owner(&tx, &before.id, owner)?;
        ensure!(
            epoch(&tx).context("recheck reconciliation mapping epoch before commit")?
                == snapshot_epoch,
            "mapping generation changed; retry reconciliation"
        );
        ensure!(
            tx.execute(
                "UPDATE migration_runs SET progress=?3 WHERE id=?1 AND progress=?2",
                params![before.id, old, new]
            )
            .context("reset stale reconciliation cursor")?
                == 1,
            "reconciliation cursor changed; retry"
        );
        tx.execute(
            "DELETE FROM migration_reconciliation WHERE run=?",
            [&before.id],
        )
        .context("delete stale reconciliation reports")?;
        if let Some(owner) = owner {
            super::keyword_repair::reports_reset(&tx, owner)?;
        }
        tx.commit().context("commit stale reconciliation reset")?;
        return Ok(Step {
            progress: reset,
            outcome: None,
            needs_decision: None,
        });
    }
    let mut after = before.clone();
    ensure!(
        source.binding_blake3() == before.input,
        "reconciliation source binding differs"
    );
    if before.capture_index == source.seal().selected.len() {
        let count: i64 = catalog
            .db
            .query_row(
                "SELECT count(*) FROM migration_reconciliation WHERE run=?",
                [&before.id],
                |r| r.get(0),
            )
            .context("count final reconciliation reports")?;
        ensure!(
            usize::try_from(count)? == source.seal().selected.len(),
            "capture report roster differs"
        );
        validate_retained_roster(
            &catalog.db,
            &before.input,
            source.seal().selected.len(),
            |r| {
                source.seal().selected.iter().any(|s| s.revision == r)
                    && !source
                        .seal()
                        .excluded_revisions
                        .iter()
                        .any(|excluded| excluded == r)
            },
        )
        .context("validate final retained revision roster")?;
        let supplements: i64 = catalog
            .db
            .query_row(
                "SELECT count(*) FROM migration_run_supplements WHERE run=?",
                [&before.id],
                |r| r.get(0),
            )
            .context("count final supplemental custody roster")?;
        ensure!(
            usize::try_from(supplements)? == source.seal().supplements.len(),
            "final supplemental custody roster differs"
        );
        after.stage = Stage::Complete;
        after.complete = true;
        let old = serde_json::to_vec(before)?;
        let new = serde_json::to_vec(&after)?;
        let _permit = catalog.writers.enter(Priority::Background)?;
        let tx = catalog
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin reconciliation report/cursor transaction")?;
        super::keyword_repair::require_owner(&tx, &before.id, owner)?;
        ensure!(
            epoch(&tx).context("recheck reconciliation mapping epoch before commit")?
                == snapshot_epoch,
            "mapping generation changed before final completion"
        );
        ensure!(
            tx.execute(
                "UPDATE migration_runs SET progress=?3 WHERE id=?1 AND progress=?2",
                params![before.id, old, new]
            )
            .context("update final reconciliation cursor")?
                == 1,
            "reconciliation cursor changed before completion"
        );
        if let Some(owner) = owner {
            super::keyword_repair::finish(&tx, owner)?;
        }
        tx.commit()
            .context("commit final reconciliation completion")?;
        return Ok(Step {
            progress: after,
            outcome: None,
            needs_decision: None,
        });
    }
    let capture = source
        .seal()
        .selected
        .get(before.capture_index)
        .context("reconciliation capture cursor bounds")?;
    let mut report = CaptureReport {
        revision: capture.revision.clone(),
        collections: BTreeMap::new(),
        walked: BTreeMap::new(),
        classifications: BTreeMap::new(),
        native_images: 0,
        native_original_mappings: 0,
        raw_artifacts: 0,
        supplements: Vec::new(),
        mapping_epoch: snapshot_epoch,
        adobe_rendering_equivalent: false,
        native_collection_order_equivalent: false,
    };
    for (collection, ordinal) in [
        (Collection::Captures, 0),
        (Collection::SchemaObjects, 1),
        (Collection::Tables, 2),
        (Collection::Rows, 3),
        (Collection::Entities, 4),
        (Collection::References, 5),
        (Collection::Paths, 6),
        (Collection::Packets, 7),
        (Collection::MetadataFacts, 8),
        (Collection::Issues, 9),
    ] {
        let expected = source
            .count(&capture.revision, collection)
            .with_context(|| format!("source {collection:?} count"))?;
        let actual: i64 = catalog
            .db
            .query_row(
                RETAINED_COMPLETE_COUNT,
                params![before.input, capture.revision, ordinal],
                |r| r.get(0),
            )
            .with_context(|| format!("destination retained {collection:?} count"))?;
        ensure!(
            u64::try_from(actual)? == expected,
            "selected {collection:?} source/destination count differs"
        );
        report
            .collections
            .insert(format!("{collection:?}"), expected);
    }
    for stage in [
        Stage::Files,
        Stage::Masters,
        Stage::VirtualCopies,
        Stage::FileEmbedded,
        Stage::FileSidecarXmp,
        Stage::FileSidecarUpper,
        Stage::FileAppendedXmp,
        Stage::FileAppendedUpper,
        Stage::CatalogXmp,
        Stage::CurrentDevelop,
        Stage::ImageFields,
        Stage::Keywords,
        Stage::Collections,
        Stage::KeywordSynonyms,
        Stage::KeywordMemberships,
        Stage::CollectionMemberships,
        Stage::History,
        Stage::DevelopSettings,
        Stage::BeforeSettings,
        Stage::Snapshots,
        Stage::SmartCollections,
        Stage::Stacks,
    ] {
        let table = stage
            .table()
            .context("reconciliation walk stage has no table")?;
        let expected:i64=catalog.db.query_row("SELECT count(*) FROM migration_record_lookup WHERE input=?1 AND revision=?2 AND collection=3 AND table_name=?3",params![before.input,capture.revision,table],|r|r.get(0))
            .with_context(|| format!("destination {stage:?} expected lookup count"))?;
        let key = serde_json::to_string(&stage)?;
        let actual: i64 = catalog.db.query_row(
            "SELECT count(*) FROM migration_run_items WHERE run=?1 AND stage=?2 AND revision=?3",
            params![before.id, key, capture.revision],
            |r| r.get(0),
        ).with_context(|| format!("destination {stage:?} actual walk count"))?;
        ensure!(expected == actual, "source walk {stage:?} count differs");
        report
            .walked
            .insert(format!("{stage:?}"), u64::try_from(actual)?);
        let mut statement = catalog
            .db
            .prepare(CLASSIFICATIONS)
            .with_context(|| format!("prepare destination {stage:?} classifications"))?;
        let mut rows = statement
            .query(params![before.id, key, capture.revision])
            .with_context(|| format!("query destination {stage:?} classifications"))?;
        let mut groups = 0usize;
        while let Some(row) = rows
            .next()
            .with_context(|| format!("read destination {stage:?} classifications"))?
        {
            groups += 1;
            ensure!(
                groups <= 256,
                "reconciliation classification count exceeds bound"
            );
            let value = row.get_ref(0)?;
            let classification = std::str::from_utf8(value.as_bytes()?)?;
            ensure!(
                classification.len() <= 16384,
                "reconciliation classification text exceeds bound"
            );
            let key = format!("{stage:?}: {classification}");
            ensure!(
                report
                    .classifications
                    .keys()
                    .map(String::len)
                    .sum::<usize>()
                    + key.len()
                    <= 512 * 1024,
                "reconciliation classification bytes exceed bound"
            );
            report
                .classifications
                .insert(key, u64::try_from(row.get::<_, i64>(1)?)?);
        }
    }
    let (_, policy) =
        importer::read(&catalog.db, &before.id).context("read reconciliation import policy")?;
    report.native_images = u64::try_from(catalog.db.query_row(
        "SELECT count(*) FROM image_import_map WHERE import_source=?1 AND capture_revision=?2",
        params![policy.import_source, capture.revision],
        |r| r.get::<_, i64>(0),
    ).context("count native image mappings")?)?;
    report.native_original_mappings=u64::try_from(catalog.db.query_row("SELECT count(*) FROM migration_originals WHERE import_source=?1 AND json_extract(source_json,'$.capture_revision')=?2",params![policy.import_source,capture.revision],|r|r.get::<_,i64>(0)).context("count native original mappings")?)?;
    let expected_images:i64=catalog.db.query_row("SELECT count(*) FROM migration_run_items WHERE run=?1 AND revision=?2 AND stage IN ('\"Masters\"','\"VirtualCopies\"') AND json_extract(outcome,'$.Image.kind')='Image'",params![before.id,capture.revision],|r|r.get(0)).context("count expected native image mappings")?;
    let expected_files:i64=catalog.db.query_row("SELECT count(*) FROM migration_run_items WHERE run=?1 AND revision=?2 AND stage='\"Files\"' AND json_type(outcome,'$.Original')='object'",params![before.id,capture.revision],|r|r.get(0)).context("count expected native original mappings")?;
    ensure!(
        report.native_images == u64::try_from(expected_images)?
            && report.native_original_mappings == u64::try_from(expected_files)?,
        "native source mappings differ from successful source receipts"
    );
    let captures = catalog
        .retained_migration_records(&before.input, &capture.revision, Collection::Captures, 0, 2)
        .context("read retained capture custody roster")?;
    ensure!(
        captures.len() == 1,
        "selected capture custody roster differs"
    );
    let manifest = source
        .capture_manifest(&capture.revision)
        .context("read source capture manifest")?;
    for member in 0..manifest.artifacts.len() {
        let (descriptor, state) = catalog
            .migration_artifact(captures[0].0, member)
            .with_context(|| format!("read captured artifact custody member={member}"))?;
        ensure!(
            descriptor.selected_input == before.input
                && descriptor.capture_revision == capture.revision
                && descriptor.manifest_blake3 == capture.manifest_blake3
                && state.complete
                && state.length == manifest.artifacts[member].revision.bytes
                && serde_json::to_vec(&descriptor.artifact)?
                    == serde_json::to_vec(&manifest.artifacts[member])?,
            "selected raw artifact custody differs or is incomplete"
        );
    }
    report.raw_artifacts = manifest.artifacts.len();
    report.supplements = supplement_reports(catalog, source, before, &policy, &capture.revision)
        .context("reconcile supplemental custody and projections")?;
    let bytes = crate::lightroom::bounded_json(&report, 8 * 1024 * 1024)?;
    after.capture_index += 1;
    let old = crate::lightroom::bounded_json(before, 8 * 1024 * 1024)?;
    let new = crate::lightroom::bounded_json(&after, 8 * 1024 * 1024)?;
    let _permit = catalog.writers.enter(Priority::Background)?;
    let tx = catalog
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("begin reconciliation report/cursor transaction")?;
    super::keyword_repair::require_owner(&tx, &before.id, owner)?;
    ensure!(
        epoch(&tx).context("recheck reconciliation mapping epoch before commit")? == snapshot_epoch,
        "native mappings changed during reconciliation; retry"
    );
    ensure!(
        tx.execute(
            "UPDATE migration_runs SET progress=?3 WHERE id=?1 AND progress=?2",
            params![before.id, old, new]
        )
        .context("update capture reconciliation cursor")?
            == 1,
        "reconciliation cursor changed; retry"
    );
    tx.execute(
        "INSERT INTO migration_reconciliation VALUES(?1,?2,?3,?4)",
        params![before.id, capture.revision, bytes, snapshot_epoch],
    )
    .context("insert capture reconciliation report")?;
    if let Some(owner) = owner {
        super::keyword_repair::report_committed(
            &tx,
            owner,
            &capture.revision,
            &bytes,
            snapshot_epoch,
        )?;
    }
    tx.commit()
        .context("commit capture reconciliation report and cursor")?;
    Ok(Step {
        progress: after,
        outcome: None,
        needs_decision: None,
    })
}
impl Catalog {
    pub fn selected_import_reconciliation(
        &self,
        id: &str,
        revision: &str,
    ) -> Result<CaptureReport> {
        ensure!(
            id.len() == 64 && revision.len() == 64,
            "reconciliation identity bounds"
        );
        let bytes: Vec<u8> = self.db.query_row(
            "SELECT report FROM migration_reconciliation WHERE run=?1 AND revision=?2",
            params![id, revision],
            |r| r.get(0),
        )?;
        ensure!(
            bytes.len() <= 8 * 1024 * 1024,
            "reconciliation report bounds"
        );
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::StatementStatus;

    const ORIGINAL_COUNT: &str = "SELECT count(*) FROM migration_retained_records WHERE input=?1 AND revision=?2 AND collection=?3 AND complete=1";

    fn fixture() -> Result<Connection> {
        let db = Connection::open_in_memory()?;
        super::super::retention::install(&db)?;
        // The full installed lookup module adds another retained-record index.
        super::super::lookup::install(&db)?;
        db.execute("INSERT INTO migration_retention(id,seal,approval) VALUES('input',x'',x''),('other',x'',x'')", [])?;
        Ok(db)
    }

    fn count(
        db: &Connection,
        sql: &str,
        input: &str,
        revision: &str,
        collection: i64,
    ) -> Result<(i64, i32)> {
        let mut statement = db.prepare(sql)?;
        let count = statement.query_row(params![input, revision, collection], |r| r.get(0))?;
        Ok((count, statement.get_status(StatementStatus::VmStep)))
    }

    const ORIGINAL_CLASSIFICATIONS: &str = "SELECT COALESCE(json_extract(outcome,'$.Retained.reason'),json_extract(outcome,'$.Metadata.state'),json_extract(outcome,'$.FileMetadata.state'),json_extract(outcome,'$.Image.kind'),'projected_or_role_skipped'),count(*) FROM migration_run_items WHERE run=?1 AND stage=?2 AND revision=?3 GROUP BY 1 LIMIT 257";

    fn classification_fixture() -> Result<Connection> {
        let db = fixture()?;
        super::super::importer::install(&db)?;
        db.execute("INSERT INTO migration_runs VALUES('run','input',x'7b7d',x'7b7d'),('other','input',x'7b7d',x'7b7d')", [])?;
        Ok(db)
    }

    fn classification_item(
        db: &Connection,
        record: i64,
        run: &str,
        stage: &str,
        revision: &str,
        outcome: &[u8],
    ) -> Result<()> {
        db.execute("INSERT INTO migration_retained_records(sequence,input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete) VALUES(?1,'input',?2,3,?1,x'00',1,'digest','cursor',1)", params![record, revision])?;
        db.execute(
            "INSERT INTO migration_run_items VALUES(?1,?2,?3,?4,?5)",
            params![run, stage, record, revision, outcome],
        )?;
        Ok(())
    }

    fn classification_values(
        db: &Connection,
        sql: &str,
    ) -> Result<Vec<(rusqlite::types::Value, i64)>> {
        Ok(db
            .prepare(sql)?
            .query_map(params!["run", "\"ImageFields\"", "capture"], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    #[test]
    fn classification_scalar_projection_preserves_precedence_default_and_scope() -> Result<()> {
        let db = classification_fixture()?;
        for (index, raw) in [
            r#"{"Retained":{"reason":"first"},"Metadata":{"state":"second"},"FileMetadata":{"state":"third"},"Image":{"kind":"fourth"}}"#,
            r#"{"Retained":{"reason":""},"Metadata":{"state":"second"}}"#,
            r#"{"Retained":{"reason":null},"Metadata":{"state":"second"},"FileMetadata":{"state":"third"}}"#,
            r#"{"Metadata":{"state":null},"FileMetadata":{"state":"third"},"Image":{"kind":"fourth"}}"#,
            r#"{"FileMetadata":{"state":null},"Image":{"kind":"fourth"}}"#,
            r#"{}"#,
            r#"{"Retained":{"reason":null},"Metadata":{"state":null},"FileMetadata":{"state":null},"Image":{"kind":null}}"#,
            r#"{"Retained":{"reason":"first"}}"#,
        ].iter().enumerate() {
            classification_item(&db, i64::try_from(index + 1)?, "run", "\"ImageFields\"", "capture", raw.as_bytes())?;
        }
        // Invalid JSON outside any one of the three scope dimensions must not
        // be parsed by either query, including a plain rather than JSON stage.
        for (i, run, stage, revision) in [
            (100, "other", "\"ImageFields\"", "capture"),
            (101, "run", "ImageFields", "capture"),
            (102, "run", "\"ImageFields\"", "other"),
        ] {
            classification_item(&db, i, run, stage, revision, b"invalid JSON")?;
        }
        use rusqlite::types::Value::Text;
        let expected = vec![
            (Text(String::new()), 1),
            (Text("first".into()), 2),
            (Text("fourth".into()), 1),
            (Text("projected_or_role_skipped".into()), 2),
            (Text("second".into()), 1),
            (Text("third".into()), 1),
        ];
        assert_eq!(
            classification_values(&db, ORIGINAL_CLASSIFICATIONS)?,
            expected
        );
        assert_eq!(classification_values(&db, CLASSIFICATIONS)?, expected);
        classification_item(
            &db,
            103,
            "run",
            "\"ImageFields\"",
            "capture",
            b"invalid JSON",
        )?;
        assert!(classification_values(&db, ORIGINAL_CLASSIFICATIONS).is_err());
        assert!(classification_values(&db, CLASSIFICATIONS).is_err());
        Ok(())
    }

    #[test]
    fn classification_scalar_projection_preserves_nontext_rejection() -> Result<()> {
        use rusqlite::types::Value;
        for chosen in [
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::json!(0),
            serde_json::json!(1.5),
        ] {
            let db = classification_fixture()?;
            let raw = serde_json::to_vec(
                &serde_json::json!({"Retained":{"reason":chosen},"Metadata":{"state":"must not win"}}),
            )?;
            classification_item(&db, 1, "run", "\"ImageFields\"", "capture", &raw)?;
            let old = classification_values(&db, ORIGINAL_CLASSIFICATIONS)?;
            assert!(matches!(old[0].0, Value::Integer(_) | Value::Real(_)));
            assert_eq!(classification_values(&db, CLASSIFICATIONS)?, old);
            for sql in [ORIGINAL_CLASSIFICATIONS, CLASSIFICATIONS] {
                let mut statement = db.prepare(sql)?;
                let mut rows = statement.query(params!["run", "\"ImageFields\"", "capture"])?;
                // The production reader calls this exact conversion and still
                // rejects a chosen numeric value rather than casting it to text.
                assert!(
                    rows.next()?
                        .context("classification row")?
                        .get_ref(0)?
                        .as_bytes()
                        .is_err()
                );
            }
        }
        let db = classification_fixture()?;
        for (i, chosen) in [serde_json::json!([1, "x"]), serde_json::json!({"x":1})]
            .iter()
            .enumerate()
        {
            classification_item(
                &db,
                i64::try_from(i + 1)?,
                "run",
                "\"ImageFields\"",
                "capture",
                &serde_json::to_vec(&serde_json::json!({"Retained":{"reason":chosen}}))?,
            )?;
        }
        assert_eq!(
            classification_values(&db, CLASSIFICATIONS)?,
            classification_values(&db, ORIGINAL_CLASSIFICATIONS)?
        );
        assert!(
            classification_values(&db, CLASSIFICATIONS)?
                .iter()
                .all(|(v, _)| matches!(v, Value::Text(_)))
        );
        Ok(())
    }

    #[test]
    fn classification_scalar_projection_preserves_group_and_label_bounds() -> Result<()> {
        let db = classification_fixture()?;
        for i in 1..=258 {
            classification_item(
                &db,
                i,
                "run",
                "\"ImageFields\"",
                "capture",
                &serde_json::to_vec(
                    &serde_json::json!({"Retained":{"reason":format!("class-{i:04}")}}),
                )?,
            )?;
            if i == 256 || i == 257 || i == 258 {
                let old = classification_values(&db, ORIGINAL_CLASSIFICATIONS)?;
                let new = classification_values(&db, CLASSIFICATIONS)?;
                assert_eq!(old, new);
                // 257 is the mandatory sentinel rejected by the unchanged
                // production >256 check, even if more groups exist.
                assert_eq!(new.len(), usize::try_from(i.min(257))?);
            }
        }
        let db = classification_fixture()?;
        let name = "z".repeat(16385);
        classification_item(
            &db,
            1,
            "run",
            "\"ImageFields\"",
            "capture",
            &serde_json::to_vec(&serde_json::json!({"Retained":{"reason":name}}))?,
        )?;
        assert_eq!(
            classification_values(&db, CLASSIFICATIONS)?,
            classification_values(&db, ORIGINAL_CLASSIFICATIONS)?
        );
        let mut statement = db.prepare(CLASSIFICATIONS)?;
        let mut rows = statement.query(params!["run", "\"ImageFields\"", "capture"])?;
        assert_eq!(
            rows.next()?
                .context("oversized classification")?
                .get_ref(0)?
                .as_bytes()?
                .len(),
            16385
        );
        // The unchanged report reader sees all bytes and rejects >16384.
        Ok(())
    }

    #[test]
    fn classification_sorter_carries_only_scalar_despite_large_outcomes() -> Result<()> {
        let db = classification_fixture()?;
        for i in 1..=16 {
            classification_item(
                &db,
                i,
                "run",
                "\"ImageFields\"",
                "capture",
                &serde_json::to_vec(
                    &serde_json::json!({"Retained":{"reason":"one"},"opaque":"x".repeat(256 * 1024)}),
                )?,
            )?;
        }
        assert_eq!(
            classification_values(&db, CLASSIFICATIONS)?,
            classification_values(&db, ORIGINAL_CLASSIFICATIONS)?
        );
        let program = |sql: &str| -> Result<Vec<(String, i64, i64, i64)>> {
            Ok(db
                .prepare(&format!("EXPLAIN {sql}"))?
                .query_map(params!["run", "\"ImageFields\"", "capture"], |r| {
                    Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<rusqlite::Result<_>>()?)
        };
        let width = |ops: &[(String, i64, i64, i64)]| -> Result<i64> {
            let insert = ops
                .iter()
                .position(|v| v.0 == "SorterInsert")
                .context("group sorter")?;
            let register = ops[insert].2;
            let make = ops[..insert]
                .iter()
                .rev()
                .find(|v| v.0 == "MakeRecord" && v.3 == register)
                .context("sorter record builder")?;
            Ok(make.2)
        };
        let original = program(ORIGINAL_CLASSIFICATIONS)?;
        let scalar = program(CLASSIFICATIONS)?;
        assert_eq!(width(&original)?, 2, "original sorter carried outcome");
        assert_eq!(width(&scalar)?, 1, "classification-only sorter");
        assert!(
            scalar
                .iter()
                .filter(|v| v.0 == "MakeRecord")
                .all(|v| v.2 == 1),
            "materialization and sort both hold only one scalar: {scalar:?}"
        );
        let plan = db
            .prepare(&format!("EXPLAIN QUERY PLAN {CLASSIFICATIONS}"))?
            .query_map(params!["run", "\"ImageFields\"", "capture"], |r| {
                r.get::<_, String>(3)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert!(
            plan.iter()
                .any(|s| s.contains("migration_run_capture (run=? AND stage=? AND revision=?)")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .all(|s| !s.starts_with("SCAN migration_run_items")),
            "{plan:?}"
        );
        Ok(())
    }

    fn supplemental_lookup_fixture(noise: i64) -> Result<Connection> {
        let db = fixture()?;
        super::super::file_metadata::install(&db)?;
        db.execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<?1)
          INSERT INTO migration_retained_records(sequence,input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete)
          SELECT i,'input','revision',3,i,x'00',1,'digest','cursor',1 FROM n", [noise + 1])?;
        db.execute("INSERT INTO migration_record_lookup(record,input,revision,collection,digest,raw_length,source_id,table_name,unavailable)
          SELECT sequence,input,revision,collection,digest,raw_length,CASE WHEN sequence=1 THEN 'wanted' ELSE printf('noise-%08d',sequence) END,'AgLibraryFile','[]' FROM migration_retained_records", [])?;
        db.execute("INSERT INTO migration_file_metadata VALUES('file','embedded','proof','owner','digest',1,1,x'00',x'7b7d')", [])?;
        Ok(db)
    }

    type ProjectionRows = Vec<(String, Vec<u8>)>;

    fn supplemental_rows(db: &Connection, sql: &str) -> Result<(ProjectionRows, i32)> {
        let mut statement = db.prepare(sql)?;
        let rows = statement
            .query_map(
                ["owner", "embedded", "proof", "input", "revision", "wanted"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, statement.get_status(StatementStatus::VmStep)))
    }

    #[test]
    fn supplemental_projection_seek_preserves_scope_missing_and_ambiguity() -> Result<()> {
        let db = supplemental_lookup_fixture(6)?;
        // Competing rows have the same source ID but differ in one admitted scope.
        for (record, column, value) in [
            (2, "input", "other"),
            (3, "revision", "other"),
            (4, "collection", "4"),
            (5, "table_name", "Adobe_images"),
        ] {
            db.execute(
                "UPDATE migration_record_lookup SET source_id='wanted' WHERE record=?",
                [record],
            )?;
            db.execute(
                &format!("UPDATE migration_record_lookup SET {column}=?1 WHERE record=?2"),
                params![value, record],
            )?;
        }
        for (name, owner, origin, proof, record) in [
            ("input", "owner", "embedded", "proof", 2),
            ("revision", "owner", "embedded", "proof", 3),
            ("collection", "owner", "embedded", "proof", 4),
            ("table", "owner", "embedded", "proof", 5),
            ("source", "owner", "embedded", "proof", 6),
            ("owner", "other", "embedded", "proof", 1),
            ("origin", "owner", "sidecar", "proof", 1),
            ("proof", "owner", "embedded", "other", 1),
        ] {
            db.execute("INSERT INTO migration_file_metadata VALUES(?1,?2,?3,?4,'wrong',?5,?5,x'00',x'7b7d')",params![name,origin,proof,owner,record])?;
        }
        let original = SUPPLEMENT_PROJECTION.replace(" INDEXED BY migration_lookup_source", "");
        let expected = vec![("digest".to_owned(), b"{}".to_vec())];
        assert_eq!(supplemental_rows(&db, SUPPLEMENT_PROJECTION)?.0, expected);
        assert_eq!(supplemental_rows(&db, &original)?.0, expected);
        db.execute(
            "DELETE FROM migration_file_metadata WHERE file_source='file'",
            [],
        )?;
        assert!(supplemental_rows(&db, SUPPLEMENT_PROJECTION)?.0.is_empty());
        assert!(supplemental_rows(&db, &original)?.0.is_empty());
        for name in ["duplicate-a", "duplicate-b", "duplicate-c"] {
            db.execute("INSERT INTO migration_file_metadata VALUES(?1,'embedded','proof','owner','duplicate',1,1,x'00',x'7b7d')",[name])?;
        }
        // Both forms expose two rows, so the existing second-row check rejects ambiguity.
        assert_eq!(supplemental_rows(&db, SUPPLEMENT_PROJECTION)?.0.len(), 2);
        assert_eq!(supplemental_rows(&db, &original)?.0.len(), 2);
        Ok(())
    }

    #[test]
    fn supplemental_projection_work_scales_with_exact_source_not_table_population() -> Result<()> {
        let original = SUPPLEMENT_PROJECTION.replace(" INDEXED BY migration_lookup_source", "");
        let table_scan = SUPPLEMENT_PROJECTION.replace(
            "INDEXED BY migration_lookup_source",
            "INDEXED BY migration_lookup_table",
        );
        let mut work = Vec::new();
        for noise in [8, 4096] {
            let db = supplemental_lookup_fixture(noise)?;
            let plan = db
                .prepare(&format!("EXPLAIN QUERY PLAN {SUPPLEMENT_PROJECTION}"))?
                .query_map(
                    ["owner", "embedded", "proof", "input", "revision", "wanted"],
                    |r| r.get::<_, String>(3),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert!(
                plan.iter()
                    .any(|p| p.contains("migration_lookup_source") && p.contains("source_id=?")),
                "{plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|p| p.contains("migration_file_metadata_retained_origin")
                        && p.contains("supplement=?")),
                "{plan:?}"
            );
            let (rows, steps) = supplemental_rows(&db, SUPPLEMENT_PROJECTION)?;
            assert_eq!(supplemental_rows(&db, &original)?.0, rows);
            let (prior_plan_rows, prior_steps) = supplemental_rows(&db, &table_scan)?;
            assert_eq!(prior_plan_rows, rows);
            assert!(steps < 100, "exact source steps={steps}");
            if noise == 4096 {
                assert!(
                    prior_steps > steps * 100,
                    "old plan={prior_steps}, source seek={steps}"
                );
            }
            eprintln!(
                "supplemental lookup unrelated_files={noise} exact_source_vm={steps} observed_old_index_vm={prior_steps}"
            );
            work.push(steps);
        }
        assert!(work[1] <= work[0] + 8, "{work:?}");
        Ok(())
    }

    #[test]
    fn retained_complete_count_preserves_full_scope_and_incomplete_rows() -> Result<()> {
        let db = fixture()?;
        let mut source_rowid = 0;
        for input in ["input", "other"] {
            for revision in ["capture", "other-capture"] {
                for collection in [0, 3, 4] {
                    for complete in [0, 1, 1] {
                        source_rowid += 1;
                        db.execute("INSERT INTO migration_retained_records(input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete) VALUES(?1,?2,?3,?4,x'00',1,'digest','cursor',?5)",params![input,revision,collection,source_rowid,complete])?;
                    }
                }
            }
        }
        // A capture containing only incomplete records must still count zero.
        db.execute("INSERT INTO migration_retained_records(input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete) VALUES('input','pending-only',3,100,x'00',1,'digest','cursor',0)", [])?;
        for input in ["input", "other", "absent"] {
            for revision in ["capture", "other-capture", "pending-only", "absent"] {
                for collection in [0, 3, 4, 9] {
                    assert_eq!(
                        count(&db, RETAINED_COMPLETE_COUNT, input, revision, collection)?.0,
                        count(&db, ORIGINAL_COUNT, input, revision, collection)?.0
                    );
                }
            }
        }
        assert_eq!(
            count(&db, RETAINED_COMPLETE_COUNT, "input", "capture", 3)?.0,
            2
        );
        db.execute("UPDATE migration_retained_records SET complete=1 WHERE input='input' AND revision='capture' AND collection=3", [])?;
        assert_eq!(
            count(&db, RETAINED_COMPLETE_COUNT, "input", "capture", 3)?.0,
            3
        );
        db.execute("UPDATE migration_retained_records SET complete=0 WHERE input='input' AND revision='capture' AND collection=3", [])?;
        assert_eq!(
            count(&db, RETAINED_COMPLETE_COUNT, "input", "capture", 3)?.0,
            0
        );
        Ok(())
    }

    #[test]
    fn complete_payload_rows_use_covering_count_and_empty_pending_index() -> Result<()> {
        let db = fixture()?;
        db.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<2048)
            INSERT INTO migration_retained_records(input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete)
            SELECT 'input','capture',3,i,zeroblob(4096),4096,'digest','cursor',1 FROM n;")?;
        let plan = |sql: &str| -> Result<Vec<String>> {
            Ok(db
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
                .query_map(params!["input", "capture", 3], |r| r.get(3))?
                .collect::<rusqlite::Result<_>>()?)
        };
        let selected = plan(RETAINED_COMPLETE_COUNT)?;
        assert!(
            selected.iter().any(|p| p.contains("COVERING INDEX")
                && p.contains("input=? AND revision=? AND collection=?")),
            "{selected:?}"
        );
        assert!(
            selected
                .iter()
                .any(|p| p.contains("USING INDEX migration_retained_pending (input=?)")),
            "{selected:?}"
        );
        assert!(
            !selected
                .iter()
                .any(|p| p.starts_with("SCAN migration_retained_records")),
            "{selected:?}"
        );
        assert!(
            !plan(ORIGINAL_COUNT)?
                .iter()
                .any(|p| p.contains("COVERING INDEX"))
        );
        // All payload rows are complete, so only the covering branch visits them;
        // the pending branch has zero entries and cannot fetch a payload row.
        let optimized = count(&db, RETAINED_COMPLETE_COUNT, "input", "capture", 3)?;
        let original = count(&db, ORIGINAL_COUNT, "input", "capture", 3)?;
        eprintln!(
            "retained count query plans: optimized={selected:?}; original={:?}; work optimized={optimized:?}, original={original:?}",
            plan(ORIGINAL_COUNT)?
        );
        assert_eq!(optimized.0, 2048);
        assert_eq!(optimized.0, original.0);
        assert!(
            optimized.1 * 4 < original.1 * 3,
            "optimized={optimized:?}, original={original:?}"
        );
        Ok(())
    }

    fn insert_revision(
        db: &Connection,
        input: &str,
        revision: &str,
        collection: i64,
        complete: i64,
    ) -> Result<()> {
        db.execute("INSERT INTO migration_retained_records(input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete)
            SELECT ?1,?2,?3,COALESCE(max(source_rowid),0)+1,x'00',1,'digest','cursor',?4 FROM migration_retained_records", params![input,revision,collection,complete])?;
        Ok(())
    }

    #[test]
    fn retained_roster_seeks_match_distinct_validation_and_reject_all_extra_positions() -> Result<()>
    {
        // Empty is an actual first key, not a sentinel that may be skipped.
        for unexpected in [
            None,
            Some(""),
            Some("a-before"),
            Some("d-between"),
            Some("z-after"),
        ] {
            let db = fixture()?;
            for (revision, complete) in [("b-selected", 1), ("f-selected", 0)] {
                for collection in [0, 3, 4] {
                    insert_revision(&db, "input", revision, collection, complete)?;
                }
            }
            insert_revision(&db, "other", "unselected-other-input", 0, 1)?;
            if let Some(revision) = unexpected {
                insert_revision(&db, "input", revision, 9, 0)?;
            }
            for excluded in [None, Some("f-selected")] {
                let allowed =
                    |r: &str| ["b-selected", "f-selected"].contains(&r) && excluded != Some(r);
                let original: Vec<String> = db
                    .prepare(
                        "SELECT DISTINCT revision FROM migration_retained_records WHERE input=?",
                    )?
                    .query_map(["input"], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                let expected = original.iter().all(|r| allowed(r));
                assert_eq!(
                    validate_retained_roster(&db, "input", 2, allowed).is_ok(),
                    expected,
                    "unexpected={unexpected:?}, excluded={excluded:?}"
                );
                assert!(
                    db.is_autocommit(),
                    "success and errors release read snapshot"
                );
            }
        }
        let db = fixture()?;
        validate_retained_roster(&db, "input", 0, |_| false)?;
        insert_revision(&db, "input", "b-selected", 0, 0)?;
        assert!(validate_retained_roster(&db, "input", 0, |_| true).is_err());
        insert_revision(&db, "input", "f-selected", 0, 1)?;
        // Even a permissive predicate cannot suppress the mandatory extra seek.
        assert!(validate_retained_roster(&db, "input", 1, |_| true).is_err());
        assert!(db.is_autocommit());
        Ok(())
    }

    #[test]
    fn retained_roster_seeks_scale_with_distinct_keys_not_payload_population() -> Result<()> {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let mut work = Vec::new();
        for copies in [1, 4096] {
            let db = fixture()?;
            db.execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<?1),
                revisions(r) AS (VALUES('a'),('m'),('z'))
                INSERT INTO migration_retained_records(input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete)
                SELECT 'input',r,3,i,x'00',1,'digest','cursor',1 FROM revisions CROSS JOIN n", [copies])?;
            for (sql, arguments, range) in [
                (FIRST_RETAINED_REVISION, vec!["input"], "(input=?)"),
                (
                    NEXT_RETAINED_REVISION,
                    vec!["input", "a"],
                    "(input=? AND revision>?)",
                ),
            ] {
                let plan = db
                    .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
                    .query_map(rusqlite::params_from_iter(arguments), |r| {
                        r.get::<_, String>(3)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                assert!(
                    plan.iter().any(
                        |p| p.contains("USING COVERING INDEX migration_retained_page")
                            && p.contains(range)
                    ),
                    "{plan:?}"
                );
                assert!(
                    !plan.iter().any(|p| p.contains("TEMP B-TREE")
                        || p.starts_with("SCAN migration_retained_records")),
                    "{plan:?}"
                );
            }
            let ticks = Arc::new(AtomicUsize::new(0));
            let observed = ticks.clone();
            db.progress_handler(
                1,
                Some(move || {
                    observed.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            )?;
            let mut revisions = Vec::new();
            validate_retained_roster(&db, "input", 3, |r| {
                revisions.push(r.to_owned());
                true
            })?;
            db.progress_handler(0, None::<fn() -> bool>)?;
            assert_eq!(revisions, ["a", "m", "z"]);
            let steps = ticks.load(Ordering::Relaxed);
            assert!(steps < 1000, "bounded four-seek work: {steps}");
            work.push(steps);
            let mut original = db.prepare(
                "SELECT DISTINCT revision FROM migration_retained_records WHERE input=?",
            )?;
            let rows = original
                .query_map(["input"], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(rows, revisions);
            if copies == 4096 {
                assert!(original.get_status(StatementStatus::VmStep) > 30_000);
            }
            eprintln!(
                "retained roster copies_per_revision={copies}, seek_vm={steps}, original_vm={}",
                original.get_status(StatementStatus::VmStep)
            );
        }
        assert!(work[1] <= work[0] + 64, "work={work:?}");
        Ok(())
    }

    #[test]
    fn retained_roster_seeks_share_one_snapshot_and_release_it_before_return() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("snapshot.sqlite3");
        let db = Connection::open(&path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        super::super::retention::install(&db)?;
        super::super::lookup::install(&db)?;
        db.execute(
            "INSERT INTO migration_retention(id,seal,approval) VALUES('input',x'',x'')",
            [],
        )?;
        insert_revision(&db, "input", "a", 0, 1)?;
        let writer = Connection::open(&path)?;
        let mut seen = Vec::new();
        validate_retained_roster(&db, "input", 1, |r| {
            seen.push(r.to_owned());
            // The first seek has established the read snapshot. A concurrent
            // commit must not alter the successor query's view of that roster.
            insert_revision(&writer, "input", "", 0, 0).unwrap();
            insert_revision(&writer, "input", "z", 0, 0).unwrap();
            r == "a"
        })?;
        assert_eq!(seen, ["a"]);
        assert!(db.is_autocommit());
        assert!(validate_retained_roster(&db, "input", 1, |r| r == "a").is_err());
        assert!(db.is_autocommit());
        Ok(())
    }
}
