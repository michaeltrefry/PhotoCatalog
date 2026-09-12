//! Capture-level reconciliation is a separate durable stage. It compares the
//! sealed source roster with completed destination custody and every native walk.
use super::importer::{self, Progress, Stage, Step};
use crate::{
    Catalog,
    catalog_writer::Priority,
    lightroom::migration_source::{Collection, MigrationSource},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
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
    pub mapping_epoch: i64,
    pub adobe_rendering_equivalent: bool,
    pub native_collection_order_equivalent: bool,
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
pub(crate) fn step(
    catalog: &mut Catalog,
    source: &MigrationSource,
    before: &Progress,
) -> Result<Step> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut ticks = 0u64;
    catalog.db.progress_handler(
        1000,
        Some(move || {
            ticks += 1;
            ticks > 100000 || std::time::Instant::now() >= deadline
        }),
    )?;
    let result = step_inner(catalog, source, before);
    catalog.db.progress_handler(0, None::<fn() -> bool>)?;
    result
}
fn epoch(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "SELECT epoch FROM migration_mapping_epoch WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}
fn step_inner(catalog: &mut Catalog, source: &MigrationSource, before: &Progress) -> Result<Step> {
    let snapshot_epoch = epoch(&catalog.db)?;
    let stale: bool = catalog.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM migration_reconciliation WHERE run=?1 AND epoch<>?2)",
        params![before.id, snapshot_epoch],
        |r| r.get(0),
    )?;
    if stale {
        let mut reset = before.clone();
        reset.capture_index = 0;
        let old = serde_json::to_vec(before)?;
        let new = serde_json::to_vec(&reset)?;
        let _permit = catalog.writers.enter(Priority::Background)?;
        let tx = catalog
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            epoch(&tx)? == snapshot_epoch,
            "mapping generation changed; retry reconciliation"
        );
        ensure!(
            tx.execute(
                "UPDATE migration_runs SET progress=?3 WHERE id=?1 AND progress=?2",
                params![before.id, old, new]
            )? == 1,
            "reconciliation cursor changed; retry"
        );
        tx.execute(
            "DELETE FROM migration_reconciliation WHERE run=?",
            [&before.id],
        )?;
        tx.commit()?;
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
        let count: i64 = catalog.db.query_row(
            "SELECT count(*) FROM migration_reconciliation WHERE run=?",
            [&before.id],
            |r| r.get(0),
        )?;
        ensure!(
            usize::try_from(count)? == source.seal().selected.len(),
            "capture report roster differs"
        );
        let retained: Vec<String> = catalog
            .db
            .prepare("SELECT DISTINCT revision FROM migration_retained_records WHERE input=?")?
            .query_map([&before.input], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ensure!(
            retained
                .iter()
                .all(|r| source.seal().selected.iter().any(|s| s.revision == *r)
                    && !source.seal().excluded_revisions.contains(r)),
            "excluded or unselected records entered destination custody"
        );
        after.stage = Stage::Complete;
        after.complete = true;
        let old = serde_json::to_vec(before)?;
        let new = serde_json::to_vec(&after)?;
        let _permit = catalog.writers.enter(Priority::Background)?;
        let tx = catalog
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            epoch(&tx)? == snapshot_epoch,
            "mapping generation changed before final completion"
        );
        ensure!(
            tx.execute(
                "UPDATE migration_runs SET progress=?3 WHERE id=?1 AND progress=?2",
                params![before.id, old, new]
            )? == 1,
            "reconciliation cursor changed before completion"
        );
        tx.commit()?;
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
        let expected = source.count(&capture.revision, collection)?;
        let actual:i64=catalog.db.query_row("SELECT count(*) FROM migration_retained_records WHERE input=?1 AND revision=?2 AND collection=?3 AND complete=1",params![before.input,capture.revision,ordinal],|r|r.get(0))?;
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
        let expected:i64=catalog.db.query_row("SELECT count(*) FROM migration_record_lookup WHERE input=?1 AND revision=?2 AND collection=3 AND table_name=?3",params![before.input,capture.revision,table],|r|r.get(0))?;
        let key = serde_json::to_string(&stage)?;
        let actual: i64 = catalog.db.query_row(
            "SELECT count(*) FROM migration_run_items WHERE run=?1 AND stage=?2 AND revision=?3",
            params![before.id, key, capture.revision],
            |r| r.get(0),
        )?;
        ensure!(expected == actual, "source walk {stage:?} count differs");
        report
            .walked
            .insert(format!("{stage:?}"), u64::try_from(actual)?);
        let mut statement=catalog.db.prepare("SELECT COALESCE(json_extract(outcome,'$.Retained.reason'),json_extract(outcome,'$.Metadata.state'),json_extract(outcome,'$.FileMetadata.state'),json_extract(outcome,'$.Image.kind'),'projected_or_role_skipped'),count(*) FROM migration_run_items WHERE run=?1 AND stage=?2 AND revision=?3 GROUP BY 1 LIMIT 257")?;
        let mut rows = statement.query(params![before.id, key, capture.revision])?;
        let mut groups = 0usize;
        while let Some(row) = rows.next()? {
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
    let (_, policy) = importer::read(&catalog.db, &before.id)?;
    report.native_images = u64::try_from(catalog.db.query_row(
        "SELECT count(*) FROM image_import_map WHERE import_source=?1 AND capture_revision=?2",
        params![policy.import_source, capture.revision],
        |r| r.get::<_, i64>(0),
    )?)?;
    report.native_original_mappings=u64::try_from(catalog.db.query_row("SELECT count(*) FROM migration_originals WHERE import_source=?1 AND json_extract(source_json,'$.capture_revision')=?2",params![policy.import_source,capture.revision],|r|r.get::<_,i64>(0))?)?;
    let expected_images:i64=catalog.db.query_row("SELECT count(*) FROM migration_run_items WHERE run=?1 AND revision=?2 AND stage IN ('\"Masters\"','\"VirtualCopies\"') AND json_extract(outcome,'$.Image.kind')='Image'",params![before.id,capture.revision],|r|r.get(0))?;
    let expected_files:i64=catalog.db.query_row("SELECT count(*) FROM migration_run_items WHERE run=?1 AND revision=?2 AND stage='\"Files\"' AND json_type(outcome,'$.Original')='object'",params![before.id,capture.revision],|r|r.get(0))?;
    ensure!(
        report.native_images == u64::try_from(expected_images)?
            && report.native_original_mappings == u64::try_from(expected_files)?,
        "native source mappings differ from successful source receipts"
    );
    let captures = catalog.retained_migration_records(
        &before.input,
        &capture.revision,
        Collection::Captures,
        0,
        2,
    )?;
    ensure!(
        captures.len() == 1,
        "selected capture custody roster differs"
    );
    let manifest = source.capture_manifest(&capture.revision)?;
    for member in 0..manifest.artifacts.len() {
        let (descriptor, state) = catalog.migration_artifact(captures[0].0, member)?;
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
    let bytes = crate::lightroom::bounded_json(&report, 8 * 1024 * 1024)?;
    after.capture_index += 1;
    let old = crate::lightroom::bounded_json(before, 8 * 1024 * 1024)?;
    let new = crate::lightroom::bounded_json(&after, 8 * 1024 * 1024)?;
    let _permit = catalog.writers.enter(Priority::Background)?;
    let tx = catalog
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(
        epoch(&tx)? == snapshot_epoch,
        "native mappings changed during reconciliation; retry"
    );
    ensure!(
        tx.execute(
            "UPDATE migration_runs SET progress=?3 WHERE id=?1 AND progress=?2",
            params![before.id, old, new]
        )? == 1,
        "reconciliation cursor changed; retry"
    );
    tx.execute(
        "INSERT INTO migration_reconciliation VALUES(?1,?2,?3,?4)",
        params![before.id, capture.revision, bytes, snapshot_epoch],
    )?;
    tx.commit()?;
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
