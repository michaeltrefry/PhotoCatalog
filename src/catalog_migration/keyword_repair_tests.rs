//! The legacy prefix is produced through the old public Retain decision and
//! coordinator ledger APIs. Tiny fixtures use the same request/count guards as
//! production, including a real completed current-settings repair predecessor.
use super::*;
use crate::catalog_migration::{
    current_repair,
    importer::{self, Outcome},
    keyword_repair::{self, Phase, Request, RootProof},
    organization::{Compatibility, Decision, NativeTarget, Projection},
    walk::Walk,
};
use crate::lightroom::migration_source::MigrationSource;
use anyhow::Context;

fn fixture() -> Result<ImportFixture> {
    let mut f = ImportFixture::with_wrapper(false, true)?;
    let revisions = f
        .inspection
        .seal
        .selected
        .iter()
        .map(|v| v.revision.clone())
        .collect::<Vec<_>>();
    f.inspection.edit(|db| {
        for revision in &revisions {
            // Child source-ID order precedes its parent; root ID is not 20.
            row(db,revision,"AgLibraryKeyword",99,vec![id(99),Cell::Null,Cell::Null]).unwrap();
            db.execute("UPDATE rows SET cells_json=?3 WHERE revision=?1 AND source_id=?2",params![revision,source_id("AgLibraryKeyword",50),serde_json::to_string(&vec![id(50),cell("Parent"),id(99)]).unwrap()]).unwrap();
            link(db,revision,"AgLibraryKeyword",50,"parent","AgLibraryKeyword",99).unwrap();
            row(db,revision,"AgLibraryKeyword",40,vec![id(40),cell("Child"),id(50)]).unwrap();
            link(db,revision,"AgLibraryKeyword",40,"parent","AgLibraryKeyword",50).unwrap();
            for (key,image,tag) in [(71,20,40),(72,21,40),(73,999,50)] {
                row(db,revision,"AgLibraryKeywordImage",key,vec![id(key),id(image),id(tag)]).unwrap();
                link(db,revision,"AgLibraryKeywordImage",key,"image","Adobe_images",image).unwrap();
                link(db,revision,"AgLibraryKeywordImage",key,"tag","AgLibraryKeyword",tag).unwrap();
            }
        }
        db.execute("UPDATE tables SET expected=(SELECT count(*) FROM rows r WHERE r.revision=tables.revision AND r.table_name=tables.name),retained=(SELECT count(*) FROM rows r WHERE r.revision=tables.revision AND r.table_name=tables.name)",[]).unwrap();
    });
    Ok(f)
}
fn transition(c: &mut Catalog, run: &str, stage: Stage) -> Result<()> {
    let before = c.selected_import_progress(run)?;
    let mut after = before.clone();
    after.stage = stage;
    after.capture_index = 0;
    after.cursor = None;
    importer::advance(c, &before, &after, None)
}
fn old_items(c: &mut Catalog, s: &MigrationSource, run: &str, stage: Stage) -> Result<()> {
    let table = stage.table().context("stage table")?;
    let rows:Vec<(i64,String)>=c.db.prepare("SELECT record,revision FROM migration_record_lookup WHERE input=? AND collection=3 AND table_name=? ORDER BY record")?.query_map(params![s.binding_blake3(),table],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (record, revision) in rows {
        let origin = Walk::new(c, s, &revision)?.source_record(record)?;
        let outcome = if stage == Stage::Keywords {
            let r=c.project_migration_organization(Some(s),&Projection{origin,import_source:OWNER.into(),adapter_version:"lightroom-organization-columns-v2".into(),decision:Decision::Retain{construct:"dictionary".into(),compatibility:Compatibility::Unsupported,detail:"Legacy unnamed root or ancestor has no native mapping; full row retained".into(),unresolved:None}})?;
            Outcome::Organization(vec![r])
        } else {
            Outcome::Retained {
                reason: "Keyword membership endpoints lack native mappings".into(),
            }
        };
        let before = c.selected_import_progress(run)?;
        let mut after = before.clone();
        after.processed += 1;
        importer::advance(c, &before, &after, Some((record, &outcome)))?;
    }
    Ok(())
}
fn completed(f: &ImportFixture) -> Result<(Catalog, MigrationSource, String, String)> {
    let source = f.inspection.open();
    let mut c = f.open()?;
    let run = c.begin_selected_import(&source, APPROVAL, &f.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut c, Stage::Keywords)?;
    drop(worker);
    old_items(&mut c, &source, &run.id, Stage::Keywords)?;
    transition(&mut c, &run.id, Stage::Collections)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut c, Stage::KeywordMemberships)?;
    drop(worker);
    old_items(&mut c, &source, &run.id, Stage::KeywordMemberships)?;
    transition(&mut c, &run.id, Stage::CollectionMemberships)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut c, Stage::Complete)?;
    drop(worker);
    let raw: Vec<u8> = c.db.query_row(
        "SELECT progress FROM migration_runs WHERE id=?",
        [&run.id],
        |r| r.get(0),
    )?;
    let request = current_repair::Request {
        run: run.id.clone(),
        expected_complete_progress_blake3: blake3::hash(&raw).to_hex().to_string(),
        expected_mapping_epoch: c.db.query_row(
            "SELECT epoch FROM migration_mapping_epoch WHERE id=1",
            [],
            |r| r.get(0),
        )?,
        reason: "Complete the genuine current repair before keyword fixture".into(),
    };
    let repair = c.begin_current_develop_repair(&source, &request)?;
    for _ in 0..100 {
        if c.step_current_develop_repair(&source, &repair.id)?
            .progress
            .complete
        {
            return Ok((c, source, run.id, repair.id));
        }
    }
    anyhow::bail!("fixture current repair did not complete")
}
fn request(c: &Catalog, s: &MigrationSource, run: &str, current: &str) -> Result<Request> {
    let mut roots = Vec::new();
    for selected in &s.seal().selected {
        let walk = Walk::new(c, s, &selected.revision)?;
        let record = walk
            .singleton(&super::super::lookup::Lookup::RowsBySource(source_id(
                "AgLibraryKeyword",
                99,
            )))?
            .context("root fixture missing")?;
        let table = walk.schema("AgLibraryKeyword")?;
        let sha = |r: i64| -> Result<String> {
            Ok(c.db.query_row(
                "SELECT digest FROM migration_retained_records WHERE sequence=?",
                [r],
                |r| r.get(0),
            )?)
        };
        roots.push(RootProof {
            origin: walk.source_record(record)?,
            raw_digest: sha(record)?,
            retained_table: table,
            table_digest: sha(table)?,
        });
    }
    let p: Vec<u8> = c.db.query_row(
        "SELECT progress FROM migration_runs WHERE id=?",
        [run],
        |r| r.get(0),
    )?;
    let prior = c.current_develop_repair_progress(current)?;
    Ok(Request {
        expected_dictionaries: 6,
        expected_memberships: 8,
        expected_synonyms: 0,
        expected_captures: 2,
        run: run.into(),
        expected_complete_progress_blake3: blake3::hash(&p).to_hex().to_string(),
        expected_mapping_epoch: c.db.query_row(
            "SELECT epoch FROM migration_mapping_epoch WHERE id=1",
            [],
            |r| r.get(0),
        )?,
        current_repair: current.into(),
        expected_current_repair_progress_blake3: blake3::hash(&serde_json::to_vec(&prior)?)
            .to_hex()
            .to_string(),
        expected_roster_blake3: keyword_repair::predecessor_roster_blake3(c, s, run)?,
        predecessor_evidence_blake3: "a".repeat(64),
        roots,
        reason: "Synthetic source-bound old keyword prefix".into(),
    })
}
fn until(c: &mut Catalog, s: &MigrationSource, id: &str, phase: Phase) -> Result<()> {
    for _ in 0..150 {
        if c.keyword_repair_progress(id)?.phase == phase {
            return Ok(());
        }
        c.step_keyword_repair(s, id)?;
    }
    anyhow::bail!("keyword fixture phase not reached")
}
fn snapshot(c: &Catalog, sql: &str) -> Result<Vec<(String, String)>> {
    Ok(c.db
        .prepare(sql)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}
#[test]
fn keyword_repair_archives_resumes_preserves_choices_variants_and_atomic_complete() -> Result<()> {
    let f = fixture()?;
    let (mut c, s, run, current) = completed(&f)?;
    let key = f.key(&c, 0, 21)?;
    let identity = c.image_metadata_identity(&key)?;
    c.organize_image(
        &key,
        identity.metadata_revision,
        crate::organization::Operation::AddKeyword {
            kind: crate::organization::KeywordKind::Hierarchical,
            path: vec!["Local".into()],
        },
    )?;
    let choices = snapshot(
        &c,
        "SELECT asset_id||':'||field,CAST(model_id AS TEXT) FROM metadata_choices ORDER BY asset_id,field",
    )?;
    let recipes = snapshot(
        &c,
        "SELECT CAST(id AS TEXT),CAST(recipe AS TEXT) FROM edit_recipe_nodes ORDER BY id",
    )?;
    let r = request(&c, &s, &run, &current)?;
    let start = c.begin_keyword_repair(&s, &r)?;
    assert!(c.step_selected_import(&s, &run).is_err());
    assert!(c.step_current_develop_repair(&s, &current).is_err());
    let original = super::super::importer::read(&c.db, &run)?.0;
    assert!(super::super::reconciliation::step(&mut c, &s, &original).is_err());
    // Close/reopen at every bounded step, including partial planning and both
    // projection stages. Requests replay unchanged rather than resetting state.
    for _ in 0..150 {
        let before = c.keyword_repair_progress(&start.id)?;
        if before.phase == Phase::Reconciliation {
            let p = c.selected_import_progress(&run)?;
            if p.capture_index == 2 {
                c.db.execute_batch("CREATE TRIGGER fail_keyword_complete BEFORE UPDATE ON migration_keyword_repairs WHEN json_extract(CAST(NEW.progress AS TEXT),'$.complete')=1 BEGIN SELECT RAISE(ABORT,'injected final failure'); END;")?;
                assert!(c.step_keyword_repair(&s, &start.id).is_err());
                assert!(!c.selected_import_progress(&run)?.complete);
                assert!(!c.keyword_repair_progress(&start.id)?.complete);
                c.db.execute_batch("DROP TRIGGER fail_keyword_complete;")?;
            }
        }
        let next = c.step_keyword_repair(&s, &start.id)?.progress;
        assert_eq!(
            snapshot(
                &c,
                "SELECT asset_id||':'||field,CAST(model_id AS TEXT) FROM metadata_choices ORDER BY asset_id,field"
            )?,
            choices
        );
        assert_eq!(
            snapshot(
                &c,
                "SELECT CAST(id AS TEXT),CAST(recipe AS TEXT) FROM edit_recipe_nodes ORDER BY id"
            )?,
            recipes
        );
        drop(c);
        c = f.open()?;
        assert_eq!(
            serde_json::to_vec(&c.begin_keyword_repair(&s, &r)?)?,
            serde_json::to_vec(&next)?
        );
        if next.complete {
            break;
        }
    }
    for root in &r.roots {
        let previous =
            c.keyword_repair_predecessor(&start.id, Stage::Keywords, root.origin.retained_record)?;
        assert!(
            matches!(serde_json::from_slice::<Outcome>(&previous.old_outcome)?,Outcome::Organization(ref values) if values.len()==1 && matches!(values[0].target,NativeTarget::Retained {..}))
        );
        assert!(previous.old_receipt.is_some());
        assert!(matches!(
            previous.planned_decision,
            Some(Decision::KeywordBoundary { .. })
        ));
        assert_eq!(previous.disposition, "projected");
    }
    let done = c.keyword_repair_progress(&start.id)?;
    assert!(done.complete);
    assert_eq!((done.planned, done.examined, done.verified), (14, 14, 14));
    assert_eq!(done.retained, 2);
    assert_eq!(
        count(
            &c,
            "SELECT count(*) FROM migration_keyword_repair_reports WHERE new_digest IS NOT NULL"
        )?,
        2
    );
    let paths: Vec<String> =
        c.db.prepare("SELECT path FROM organization_keywords ORDER BY path")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
    assert!(paths.contains(&"[\"Parent\",\"Child\"]".into()));
    assert!(!paths.iter().any(|v| v.contains("99")));
    // Verify the enum through Rust rather than depending on a guessed JSON tag.
    let receipts: Vec<String> =
        c.db.prepare("SELECT result FROM migration_organization WHERE slot='keyword_membership'")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
    assert_eq!(receipts.len(), 6);
    let mut imported = std::collections::BTreeMap::new();
    for raw in receipts {
        let v: crate::catalog_migration::organization::ProjectionResult =
            serde_json::from_str(&raw)?;
        let NativeTarget::MetadataCandidate {
            key: image,
            revision,
            model_id,
            ..
        } = v.target
        else {
            anyhow::bail!("expected candidate");
        };
        let image_id = c.image_metadata_identity(&image)?.image_id;
        let projection = crate::xmp::project(&c.metadata_model(&image_id, model_id)?)?;
        let value = projection
            .fields
            .get("hierarchical_keywords")
            .context("candidate terms absent")?
            .clone();
        let entry = imported
            .entry((image.asset_id, image.variant_id))
            .or_insert((revision, value.clone()));
        if revision > entry.0 {
            *entry = (revision, value);
        }
    }
    let master = f.key(&c, 0, 20)?;
    assert_eq!(
        master.asset_id, key.asset_id,
        "fixture must share physical storage"
    );
    assert_ne!(master.variant_id, key.variant_id);
    assert_eq!(
        imported[&(master.asset_id.clone(), master.variant_id.clone())].1,
        crate::xmp::Value::List(vec!["Parent|Child".into()])
    );
    assert_eq!(
        imported[&(key.asset_id.clone(), key.variant_id.clone())].1,
        crate::xmp::Value::List(vec!["Parent".into(), "Parent|Child".into()])
    );
    let virtual_model = c.metadata_for_image(&key)?;
    let master_model = c.metadata_for_image(&master)?;
    assert_eq!(
        virtual_model
            .fields
            .iter()
            .find(|f| f.name == "hierarchical_keywords")
            .and_then(|f| f.value.clone()),
        Some(crate::xmp::Value::List(vec!["Local".into()]))
    );
    assert_eq!(
        master_model
            .fields
            .iter()
            .find(|f| f.name == "hierarchical_keywords")
            .and_then(|f| f.value.clone()),
        Some(crate::xmp::Value::List(vec!["Parent|Child".into()]))
    );
    Ok(())
}
#[test]
fn keyword_projection_failure_rolls_back_predecessor_candidate_and_cursor() -> Result<()> {
    let f = fixture()?;
    let (mut c, s, run, current) = completed(&f)?;
    let r = request(&c, &s, &run, &current)?;
    let p = c.begin_keyword_repair(&s, &r)?;
    until(&mut c, &s, &p.id, Phase::Dictionaries)?;
    let before = serde_json::to_vec(&c.keyword_repair_progress(&p.id)?)?;
    let old = snapshot(
        &c,
        "SELECT source_identity||slot,result FROM migration_organization ORDER BY source_identity,slot",
    )?;
    c.db.execute_batch("CREATE TRIGGER fail_keyword_ledger BEFORE UPDATE ON migration_run_items BEGIN SELECT RAISE(ABORT,'injected ledger rollback'); END;")?;
    assert!(c.step_keyword_repair(&s, &p.id).is_err());
    assert_eq!(
        before,
        serde_json::to_vec(&c.keyword_repair_progress(&p.id)?)?
    );
    assert_eq!(
        old,
        snapshot(
            &c,
            "SELECT source_identity||slot,result FROM migration_organization ORDER BY source_identity,slot"
        )?
    );
    c.db.execute_batch("DROP TRIGGER fail_keyword_ledger;")?;
    until(&mut c, &s, &p.id, Phase::Memberships)?;
    let observations = count(&c, "SELECT count(*) FROM metadata_observations")?;
    let before = serde_json::to_vec(&c.keyword_repair_progress(&p.id)?)?;
    c.db.execute_batch("CREATE TRIGGER fail_keyword_candidate BEFORE UPDATE ON migration_keyword_repair_items WHEN NEW.disposition='projected' BEGIN SELECT RAISE(ABORT,'injected candidate rollback'); END;")?;
    assert!(c.step_keyword_repair(&s, &p.id).is_err());
    assert_eq!(
        observations,
        count(&c, "SELECT count(*) FROM metadata_observations")?
    );
    assert_eq!(
        before,
        serde_json::to_vec(&c.keyword_repair_progress(&p.id)?)?
    );
    c.db.execute_batch("DROP TRIGGER fail_keyword_candidate;")?;
    c.step_keyword_repair(&s, &p.id)?;
    Ok(())
}
#[test]
fn keyword_admission_rejects_wrong_roots_counts_and_changed_request_without_writes() -> Result<()> {
    let f = fixture()?;
    let (mut c, s, run, current) = completed(&f)?;
    let r = request(&c, &s, &run, &current)?;
    for kind in 0..4 {
        let mut wrong = r.clone();
        match kind {
            0 => wrong.expected_dictionaries += 1,
            1 => wrong.expected_synonyms = 1,
            2 => wrong.roots[0].raw_digest = "f".repeat(64),
            _ => wrong.expected_current_repair_progress_blake3 = "f".repeat(64),
        };
        assert!(c.begin_keyword_repair(&s, &wrong).is_err());
        assert!(c.selected_import_progress(&run)?.complete);
        assert_eq!(
            count(&c, "SELECT count(*) FROM migration_keyword_repairs")?,
            0
        );
    }
    let p = c.begin_keyword_repair(&s, &r)?;
    let mut wrong = r.clone();
    wrong.reason.push('x');
    assert!(c.begin_keyword_repair(&s, &wrong).is_err());
    assert_eq!(c.keyword_repair_progress(&p.id)?.planned, 0);
    Ok(())
}
#[test]
fn keyword_schema9_upgrade_failure_rolls_back_all_new_tables() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let c = Catalog::open(temp.path())?;
    c.db.execute_batch("DROP TABLE migration_keyword_repair_items; DROP TABLE migration_keyword_repair_reports; DROP TABLE migration_keyword_repairs; CREATE TABLE migration_keyword_repair_items(bad TEXT); PRAGMA user_version=9;")?;
    drop(c);
    assert!(Catalog::open(temp.path()).is_err());
    let db = Connection::open(temp.path().join("catalog.sqlite3"))?;
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
        9
    );
    assert_eq!(db.query_row("SELECT count(*) FROM sqlite_schema WHERE name IN ('migration_keyword_repairs','migration_keyword_repair_reports')",[],|r|r.get::<_,i64>(0))?,0);
    Ok(())
}

#[test]
fn keyword_candidate_stale_identity_and_parent_change_are_fail_closed() -> Result<()> {
    let f = fixture()?;
    let (mut c, s, run, current) = completed(&f)?;
    let r = request(&c, &s, &run, &current)?;
    let p = c.begin_keyword_repair(&s, &r)?;
    until(&mut c, &s, &p.id, Phase::Dictionaries)?;
    // Commit the first boundary and parent, then model a concurrent native
    // hierarchy change. There is no public rename API; this fixture mutation
    // exercises the parent authority recheck, not an advertised user operation.
    let child_order:i64=c.db.query_row("SELECT min(i.ordering) FROM migration_keyword_repair_items i JOIN migration_record_lookup l ON l.record=i.record WHERE i.repair=?1 AND i.stage='\"Keywords\"' AND l.source_id=?2",params![p.id,source_id("AgLibraryKeyword",40)],|r|r.get(0))?;
    while c.keyword_repair_progress(&p.id)?.order_index + 1 < child_order {
        c.step_keyword_repair(&s, &p.id)?;
    }
    let before = serde_json::to_vec(&c.keyword_repair_progress(&p.id)?)?;
    assert_eq!(
        c.db.execute(
            "UPDATE organization_keywords SET path='[\"Changed\"]' WHERE path='[\"Parent\"]'",
            []
        )?,
        1
    );
    assert!(c.step_keyword_repair(&s, &p.id).is_err());
    assert_eq!(
        before,
        serde_json::to_vec(&c.keyword_repair_progress(&p.id)?)?
    );
    c.db.execute(
        "UPDATE organization_keywords SET path='[\"Parent\"]' WHERE path='[\"Changed\"]'",
        [],
    )?;
    until(&mut c, &s, &p.id, Phase::Memberships)?;
    let revision = &s.seal().selected[0].revision;
    let w = Walk::new(&c, &s, revision)?;
    let record = w
        .singleton(&super::super::lookup::Lookup::RowsBySource(source_id(
            "AgLibraryKeywordImage",
            70,
        )))?
        .context("member")?;
    let origin = w.source_record(record)?;
    let req =
        crate::catalog_migration::organization_walk::keyword_member(&c, &s, &f.policy, &origin)?
            .map_err(anyhow::Error::msg)?;
    let prepared = c.prepare_keyword_projection(&s, &req)?;
    let key = f.key(&c, 0, 21)?;
    let identity = c.image_metadata_identity(&key)?;
    c.organize_image(
        &key,
        identity.metadata_revision,
        crate::organization::Operation::Label {
            value: "User purple".into(),
        },
    )?;
    let observations = count(&c, "SELECT count(*) FROM metadata_observations")?;
    let tx = c.db.transaction()?;
    assert!(
        crate::catalog_migration::organization::commit_keyword_projection(&tx, prepared).is_err()
    );
    tx.rollback()?;
    assert_eq!(
        observations,
        count(&c, "SELECT count(*) FROM metadata_observations")?
    );
    assert!(c.metadata_for_image(&key)?.fields.iter().any(
        |v| v.name == "label" && v.value == Some(crate::xmp::Value::Text("User purple".into()))
    ));
    c.step_keyword_repair(&s, &p.id)?;
    Ok(())
}

#[test]
fn keyword_admitted_root_requires_reverse_ledger_coverage() -> Result<()> {
    let f = fixture()?;
    let (mut c, s, run, current) = completed(&f)?;
    let mut r = request(&c, &s, &run, &current)?;
    let missing = r.roots[0].origin.retained_record;
    c.db.execute(
        "DELETE FROM migration_run_items WHERE run=? AND stage='\"Keywords\"' AND record=?",
        params![run, missing],
    )?;
    r.expected_dictionaries -= 1;
    r.expected_roster_blake3 = keyword_repair::predecessor_roster_blake3(&c, &s, &run)?;
    let error = c.begin_keyword_repair(&s, &r).unwrap_err();
    assert!(format!("{error:#}").contains("root is absent"));
    assert!(c.selected_import_progress(&run)?.complete);
    assert_eq!(
        count(&c, "SELECT count(*) FROM migration_keyword_repairs")?,
        0
    );
    Ok(())
}
