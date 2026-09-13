//! Legacy coordinator emulation uses the real explicit-path projection API and
//! importer checkpoint helper. No receipt is invented by changing JSON fields.
use super::*;
use crate::catalog_migration::{
    current_repair,
    importer::{self, Outcome},
    metadata,
    walk::{LinkResolution, Walk},
};
use crate::lightroom::migration_source::MigrationSource;
use anyhow::Context;

#[path = "catalog_backup_current_tests.rs"]
mod catalog_backup_tests;

fn old_complete(fixture: &ImportFixture) -> Result<(Catalog, MigrationSource, Progress)> {
    let source = fixture.inspection.open();
    let mut catalog = fixture.open()?;
    current_repair::install(&catalog.db)?;
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::CurrentDevelop)?;
    drop(worker);
    // Reproduce only the old coordinator's settings_path=[] selection, retaining
    // the production source proof, recipe installation and outcome serializer.
    let rows:Vec<(i64,String)>=catalog.db.prepare("SELECT record,revision FROM migration_record_lookup WHERE input=? AND collection=3 AND table_name='Adobe_images' ORDER BY record LIMIT 64")?.query_map([source.binding_blake3()],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    assert_eq!(rows.len(), 6);
    for (record, revision) in rows {
        let walk = Walk::new(&catalog, &source, &revision)?;
        let image = walk.source_record(record)?;
        let settings = match walk.link(
            &image,
            "developSettingsIDCache",
            "Adobe_imageDevelopSettings",
        )? {
            LinkResolution::Unique(v) => *v,
            _ => anyhow::bail!("fixture missing current pointer"),
        };
        let request = metadata::CurrentDevelop {
            image,
            image_table: walk.schema("Adobe_images")?,
            settings,
            settings_table: walk.schema("Adobe_imageDevelopSettings")?,
            settings_path: vec![],
            import_source: OWNER.into(),
            expected_edit_revision: 0,
        };
        let result = catalog.project_migration_current_develop(Some(&source), &request)?;
        assert_eq!(result.state, "retained_only");
        assert_eq!(result.edit_revision, Some(1));
        let before = catalog.selected_import_progress(&run.id)?;
        let mut after = before.clone();
        after.processed += 1;
        importer::advance(
            &mut catalog,
            &before,
            &after,
            Some((
                record,
                &Outcome::Metadata {
                    input_digest: result.input_digest,
                    state: result.state,
                    observation: result.observation,
                    reason: result.reason,
                },
            )),
        )?;
    }
    let before = catalog.selected_import_progress(&run.id)?;
    let mut after = before.clone();
    after.stage = Stage::ImageFields;
    after.capture_index = 0;
    after.cursor = None;
    importer::advance(&mut catalog, &before, &after, None)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    let complete = drive(&mut worker, &mut catalog, Stage::Complete)?;
    drop(worker);
    Ok((catalog, source, complete))
}
fn request(catalog: &Catalog, run: &str) -> Result<current_repair::Request> {
    let raw: Vec<u8> = catalog.db.query_row(
        "SELECT progress FROM migration_runs WHERE id=?",
        [run],
        |r| r.get(0),
    )?;
    Ok(current_repair::Request {
        run: run.into(),
        expected_complete_progress_blake3: blake3::hash(&raw).to_hex().to_string(),
        expected_mapping_epoch: catalog.db.query_row(
            "SELECT epoch FROM migration_mapping_epoch WHERE id=1",
            [],
            |r| r.get(0),
        )?,
        reason: "synthetic wrapped-container integration correction".into(),
    })
}
fn to_project(catalog: &mut Catalog, source: &MigrationSource, id: &str) -> Result<()> {
    for _ in 0..10 {
        if catalog.current_develop_repair_progress(id)?.phase == current_repair::Phase::Project {
            return Ok(());
        }
        catalog.step_current_develop_repair(source, id)?;
    }
    anyhow::bail!("fixture did not reach repair project")
}
fn finish(
    catalog: &mut Catalog,
    source: &MigrationSource,
    id: &str,
) -> Result<current_repair::Progress> {
    for _ in 0..64 {
        let p = catalog.step_current_develop_repair(source, id)?.progress;
        if p.complete {
            return Ok(p);
        }
    }
    anyhow::bail!("fixture repair did not complete")
}

#[test]
fn wrapped_repair_preserves_archives_maps_variants_and_resumes() -> Result<()> {
    let fixture = ImportFixture::with_wrapper(false, true)?;
    let (mut catalog, source, complete) = old_complete(&fixture)?;
    let source_before = fs::read(&fixture.inspection.path)?;
    let mappings: i64 = count(&catalog, "SELECT count(*) FROM image_import_map")?;
    let old_outcomes:Vec<(i64,Vec<u8>)>=catalog.db.prepare("SELECT record,outcome FROM migration_run_items WHERE run=? AND stage='\"CurrentDevelop\"' ORDER BY record")?.query_map([&complete.id],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let old_results: std::collections::BTreeMap<String, Vec<u8>> = catalog
        .db
        .prepare("SELECT input_digest,result FROM migration_metadata WHERE slot='current_develop'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let req = request(&catalog, &complete.id)?;
    let start = catalog.begin_current_develop_repair(&source, &req)?;
    assert!(!catalog.selected_import_progress(&complete.id)?.complete);
    assert!(catalog.step_selected_import(&source, &complete.id).is_err());
    to_project(&mut catalog, &source, &start.id)?;
    catalog.step_current_develop_repair(&source, &start.id)?;
    let checkpoint = catalog.current_develop_repair_progress(&start.id)?;
    assert_eq!(checkpoint.repaired, 1);
    drop(catalog);
    let mut catalog = fixture.open()?;
    assert_eq!(
        serde_json::to_vec(&catalog.begin_current_develop_repair(&source, &req)?)?,
        serde_json::to_vec(&checkpoint)?
    );
    let done = finish(&mut catalog, &source, &start.id)?;
    assert_eq!((done.examined, done.repaired, done.unchanged), (6, 6, 0));
    assert!(catalog.selected_import_progress(&complete.id)?.complete);
    assert_eq!(
        catalog.selected_import_progress(&complete.id)?.processed,
        complete.processed
    );
    assert_eq!(
        count(&catalog, "SELECT count(*) FROM image_import_map")?,
        mappings
    );
    assert_eq!(
        count(
            &catalog,
            "SELECT epoch FROM migration_mapping_epoch WHERE id=1"
        )?,
        req.expected_mapping_epoch
    );
    for (record, bytes) in old_outcomes {
        let archived = catalog.current_develop_repair_predecessor(&start.id, record)?;
        assert_eq!(archived.outcome, bytes);
        let (header, result) = archived.metadata.context("old metadata archive")?;
        let old_digest = header["input_digest"].as_str().unwrap();
        assert_eq!(&result, &old_results[old_digest]);
        assert_ne!(
            archived.new_result_digest,
            Some(blake3::hash(&result).to_hex().to_string())
        );
        let old: metadata::ResultRecord = serde_json::from_slice(&result)?;
        assert!(old.extraction.unwrap().input.settings_path.is_empty());
    }
    assert_eq!(
        count(
            &catalog,
            "SELECT count(*) FROM migration_current_repair_reports"
        )?,
        2
    );
    for i in 0..2 {
        for (local, ev) in [(20, 1.0), (21, -1.0), (22, 2.0)] {
            let key = fixture.key(&catalog, i, local)?;
            let view = catalog.edit_variant(&key)?;
            assert_eq!(
                view.recipe.validate()?.settings().exposure_ev,
                ev + i as f32 * 0.25
            );
            assert_eq!(view.revision, 2);
        }
        let report = catalog.selected_import_reconciliation(
            &complete.id,
            &fixture.inspection.seal.selected[i].revision,
        )?;
        assert_eq!(
            report
                .classifications
                .get("CurrentDevelop: translated_with_appearance_gaps"),
            Some(&3)
        );
    }
    let key = fixture.key(&catalog, 0, 20)?;
    let v = catalog.edit_variant(&key)?;
    let edited = catalog.save_edit_recipe(
        &key,
        v.revision,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 3.0,
            ..Default::default()
        }),
    )?;
    assert!(
        catalog
            .begin_current_develop_repair(&source, &req)?
            .complete
    );
    assert!(
        catalog
            .step_current_develop_repair(&source, &start.id)?
            .progress
            .complete
    );
    assert_eq!(
        catalog.edit_variant(&key)?.recipe_digest,
        edited.recipe_digest
    );
    assert_eq!(fs::read(&fixture.inspection.path)?, source_before);
    Ok(())
}
#[test]
fn repair_transaction_failure_restores_recipe_receipts_archive_and_cursor() -> Result<()> {
    let fixture = ImportFixture::with_wrapper(false, true)?;
    let (mut catalog, source, run) = old_complete(&fixture)?;
    let req = request(&catalog, &run.id)?;
    let p = catalog.begin_current_develop_repair(&source, &req)?;
    to_project(&mut catalog, &source, &p.id)?;
    let before = catalog.current_develop_repair_progress(&p.id)?;
    let key = fixture.key(&catalog, 0, 20)?;
    let recipe = catalog.edit_variant(&key)?;
    let old: Vec<u8> = catalog.db.query_row(
        "SELECT result FROM migration_metadata WHERE slot='current_develop' ORDER BY rowid LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    for trigger in [
        "CREATE TRIGGER fixture_repair_fail BEFORE UPDATE OF outcome ON migration_run_items BEGIN SELECT RAISE(ABORT,'fixture outcome failure'); END;",
        "CREATE TRIGGER fixture_repair_fail BEFORE INSERT ON migration_current_repair_items BEGIN SELECT RAISE(ABORT,'fixture archive failure'); END;",
    ] {
        catalog.db.execute_batch(trigger)?;
        assert!(catalog.step_current_develop_repair(&source, &p.id).is_err());
        catalog
            .db
            .execute_batch("DROP TRIGGER fixture_repair_fail;")?;
        assert_eq!(
            serde_json::to_vec(&catalog.current_develop_repair_progress(&p.id)?)?,
            serde_json::to_vec(&before)?
        );
        assert_eq!(catalog.edit_variant(&key)?.revision, recipe.revision);
        assert_eq!(
            catalog.edit_variant(&key)?.recipe_digest,
            recipe.recipe_digest
        );
        assert_eq!(
            count(
                &catalog,
                "SELECT count(*) FROM migration_current_repair_items"
            )?,
            0
        );
        let actual:Vec<u8>=catalog.db.query_row("SELECT result FROM migration_metadata WHERE slot='current_develop' ORDER BY rowid LIMIT 1",[],|r|r.get(0))?;
        assert_eq!(actual, old);
    }
    assert_eq!(
        catalog
            .step_current_develop_repair(&source, &p.id)?
            .progress
            .repaired,
        1
    );
    Ok(())
}
#[test]
fn repair_refuses_user_edits_wrong_epoch_progress_and_outcome() -> Result<()> {
    let fixture = ImportFixture::with_wrapper(false, true)?;
    let (mut catalog, source, run) = old_complete(&fixture)?;
    let req = request(&catalog, &run.id)?;
    let mut bad = req.clone();
    bad.expected_mapping_epoch += 1;
    assert!(catalog.begin_current_develop_repair(&source, &bad).is_err());
    bad = req.clone();
    bad.expected_complete_progress_blake3 = "a".repeat(64);
    assert!(catalog.begin_current_develop_repair(&source, &bad).is_err());
    assert_eq!(
        count(&catalog, "SELECT count(*) FROM migration_current_repairs")?,
        0
    );
    let p = catalog.begin_current_develop_repair(&source, &req)?;
    to_project(&mut catalog, &source, &p.id)?;
    let before = catalog.current_develop_repair_progress(&p.id)?;
    catalog.db.execute("UPDATE migration_run_items SET outcome=json_set(outcome,'$.Metadata.input_digest','wrong') WHERE record=(SELECT min(record) FROM migration_run_items WHERE stage='\"CurrentDevelop\"') AND stage='\"CurrentDevelop\"'",[])?;
    assert!(catalog.step_current_develop_repair(&source, &p.id).is_err());
    // Restore the exact serialized production outcome from its unchanged metadata receipt.
    let raw: Vec<u8> = catalog.db.query_row(
        "SELECT result FROM migration_metadata WHERE slot='current_develop' ORDER BY rowid LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    let r: metadata::ResultRecord = serde_json::from_slice(&raw)?;
    let exact = serde_json::to_vec(&Outcome::Metadata {
        input_digest: r.input_digest,
        state: r.state,
        observation: r.observation,
        reason: r.reason,
    })?;
    catalog.db.execute("UPDATE migration_run_items SET outcome=? WHERE record=(SELECT min(record) FROM migration_run_items WHERE stage='\"CurrentDevelop\"') AND stage='\"CurrentDevelop\"'",[exact])?;
    let mut corrupt: metadata::ResultRecord = serde_json::from_slice(&raw)?;
    corrupt.extraction.as_mut().unwrap().input.payload_blake3 = "f".repeat(64);
    catalog.db.execute("UPDATE migration_metadata SET result=? WHERE rowid=(SELECT min(rowid) FROM migration_metadata WHERE slot='current_develop')",[serde_json::to_vec(&corrupt)?])?;
    assert!(catalog.step_current_develop_repair(&source, &p.id).is_err());
    catalog.db.execute("UPDATE migration_metadata SET result=? WHERE rowid=(SELECT min(rowid) FROM migration_metadata WHERE slot='current_develop')",[raw])?;
    let key = fixture.key(&catalog, 0, 20)?;
    let view = catalog.edit_variant(&key)?;
    let edited = catalog.save_edit_recipe(
        &key,
        view.revision,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 4.0,
            ..Default::default()
        }),
    )?;
    assert!(catalog.step_current_develop_repair(&source, &p.id).is_err());
    assert_eq!(
        catalog.edit_variant(&key)?.recipe_digest,
        edited.recipe_digest
    );
    assert_eq!(
        serde_json::to_vec(&catalog.current_develop_repair_progress(&p.id)?)?,
        serde_json::to_vec(&before)?
    );
    Ok(())
}
#[test]
fn unsupported_wrapped_settings_remain_explicit_and_archive_corruption_refused() -> Result<()> {
    let mut fixture = ImportFixture::with_wrapper(false, true)?;
    fixture.inspection.edit(|db| {
        let mut statement=db.prepare("SELECT rowid,cells_json FROM rows WHERE table_name='Adobe_imageDevelopSettings' LIMIT 16").unwrap();
        let rows:Vec<(i64,String)>=statement.query_map([],|r|Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();drop(statement);
        for (id,cells) in rows {
            let mut cells:Vec<Cell>=serde_json::from_str(&cells).unwrap();
            for cell in &mut cells {if let Cell::Text(v)=cell {*v=String::from_utf8(v.clone()).unwrap().replace("11.0","6.7").into_bytes();}}
            db.execute("UPDATE rows SET cells_json=?1 WHERE rowid=?2",params![serde_json::to_string(&cells).unwrap(),id]).unwrap();
        }
    });
    let (mut catalog, source, run) = old_complete(&fixture)?;
    let req = request(&catalog, &run.id)?;
    let p = catalog.begin_current_develop_repair(&source, &req)?;
    let done = finish(&mut catalog, &source, &p.id)?;
    assert_eq!(done.repaired, 6);
    for i in 0..2 {
        for local in [20, 21, 22] {
            let view = catalog.edit_variant(&fixture.key(&catalog, i, local)?)?;
            assert_eq!(view.recipe.validate()?.settings().exposure_ev, 0.0);
        }
    }
    assert_eq!(
        count(
            &catalog,
            "SELECT count(*) FROM migration_metadata WHERE slot='current_develop' AND json_extract(result,'$.state')='retained_only'"
        )?,
        6
    );
    let record: i64 = catalog.db.query_row(
        "SELECT min(record) FROM migration_current_repair_items",
        [],
        |r| r.get(0),
    )?;
    catalog.db.execute("UPDATE migration_current_repair_items SET old_result=x'0001' WHERE repair=?1 AND record=?2",params![p.id,record])?;
    assert!(
        catalog
            .current_develop_repair_predecessor(&p.id, record)
            .is_err()
    );
    Ok(())
}
#[test]
fn correctly_bound_projection_is_unchanged_and_foreign_seal_refused() -> Result<()> {
    let fixture = ImportFixture::with_wrapper(false, true)?;
    let source = fixture.inspection.open();
    let mut catalog = fixture.open()?;
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::Complete)?;
    drop(worker);
    let req = request(&catalog, &run.id)?;
    let other = ImportFixture::with_wrapper(false, true)?;
    let foreign = other.inspection.open();
    assert!(
        catalog
            .begin_current_develop_repair(&foreign, &req)
            .is_err()
    );
    let key = fixture.key(&catalog, 0, 20)?;
    let view = catalog.edit_variant(&key)?;
    let p = catalog.begin_current_develop_repair(&source, &req)?;
    let done = finish(&mut catalog, &source, &p.id)?;
    assert_eq!((done.examined, done.repaired, done.unchanged), (6, 0, 6));
    assert_eq!(catalog.edit_variant(&key)?.revision, view.revision);
    assert_eq!(
        catalog.edit_variant(&key)?.recipe_digest,
        view.recipe_digest
    );
    Ok(())
}

#[test]
fn retained_oversized_identity_repairs_without_interpreting_or_stalling() -> Result<()> {
    let fixture = ImportFixture::new(true)?;
    let source = fixture.inspection.open();
    let mut catalog = fixture.open()?;
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    let complete = drive(&mut worker, &mut catalog, Stage::Complete)?;
    drop(worker);
    let revision = &fixture.inspection.seal.selected[0].revision;
    let oversized:i64=catalog.db.query_row("SELECT record FROM migration_record_lookup WHERE input=?1 AND revision=?2 AND collection=3 AND source_id=?3",params![source.binding_blake3(),revision,source_id("Adobe_images",92)],|r|r.get(0))?;
    // The valid completed importer intentionally retained this 40,000-byte key;
    // requiring an interpreted SourceRecord is the pre-fix repair failure.
    assert!(
        Walk::new(&catalog, &source, revision)?
            .source_record(oversized)
            .is_err()
    );
    let old:Vec<u8>=catalog.db.query_row("SELECT outcome FROM migration_run_items WHERE run=?1 AND stage='\"CurrentDevelop\"' AND record=?2",params![run.id,oversized],|r|r.get(0))?;
    assert!(matches!(
        serde_json::from_slice::<Outcome>(&old)?,
        Outcome::Retained { .. }
    ));
    let req = request(&catalog, &run.id)?;
    let repair = catalog.begin_current_develop_repair(&source, &req)?;
    let done = finish(&mut catalog, &source, &repair.id)?;
    assert_eq!((done.examined, done.repaired, done.unchanged), (9, 0, 9));
    let archived = catalog.current_develop_repair_predecessor(&repair.id, oversized)?;
    assert_eq!(archived.outcome, old);
    assert!(archived.metadata.is_none());
    assert_eq!(archived.disposition, "unchanged_nonprojection");
    assert_eq!(
        catalog.selected_import_progress(&run.id)?.processed,
        complete.processed
    );
    assert_eq!(count(&catalog, "SELECT count(*) FROM image_import_map")?, 6);
    assert_eq!(
        catalog
            .selected_import_reconciliation(&run.id, revision)?
            .walked["CurrentDevelop"],
        6
    );
    Ok(())
}
