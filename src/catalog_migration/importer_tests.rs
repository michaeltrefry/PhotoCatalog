//! Coordinator integration fixtures: local synthetic copies only. These tests
//! do not grant import authority for a user's originals or inspection database.
use super::{
    artifacts::{ArtifactLimits, ArtifactMapping},
    import_artifacts::Worker,
    importer::{ArtifactInput, KeywordOverlap, OverlapPolicy, Policy, Progress, Stage},
    originals::SourceKey,
};
use crate::{
    Catalog,
    catalog_edits::VariantKey,
    edit::{Recipe, RecipeV1},
    lightroom::{
        capture::Artifact,
        migration_source::{Collection, SelectedCapture, tests::Fixture},
        plan::Cell,
        source::Source,
    },
    storage_volume::NativePath,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use std::{
    fs,
    path::{Path, PathBuf},
};

const APPROVAL: &[u8] = b"two selected synthetic families into one test destination";
const OWNER: &str = "coordinator-fixture";
fn cell(value: &str) -> Cell {
    Cell::Text(value.as_bytes().to_vec())
}
fn id(value: i64) -> Cell {
    Cell::Integer(value)
}
fn source_id(table: &str, key: i64) -> String {
    format!("fixture:{table}:{key}")
}
fn schema(db: &Connection, revision: &str, table: &str, columns: &[&str]) -> Result<()> {
    db.execute("INSERT INTO tables(revision,name,columns_json,key_json,schema_json,category,expected,retained,state) VALUES(?1,?2,?3,'[]','{}','fixture',0,0,'complete')",params![revision,table,serde_json::to_string(columns)?])?;
    Ok(())
}
fn row(db: &Connection, revision: &str, table: &str, key: i64, cells: Vec<Cell>) -> Result<()> {
    db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,?2,?3,?4,?5)",params![revision,source_id(table,key),table,serde_json::to_string(&vec![id(key)])?,serde_json::to_string(&cells)?])?;
    db.execute("INSERT INTO entities(revision,source_id,table_name,local_key,fields_json) VALUES(?1,?2,?3,?4,'{}')",params![revision,source_id(table,key),table,serde_json::to_string(&id(key))?])?;
    Ok(())
}
fn link(
    db: &Connection,
    revision: &str,
    table: &str,
    key: i64,
    field: &str,
    target: &str,
    target_key: i64,
) -> Result<()> {
    db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,?2,?3,?4,?5)",params![revision,source_id(table,key),field,target,serde_json::to_string(&id(target_key))?])?;
    Ok(())
}
struct ImportFixture {
    _root: tempfile::TempDir,
    inspection: Fixture,
    destination: PathBuf,
    policy: Policy,
    paths: Vec<NativePath>,
    raw: Vec<Vec<u8>>,
}
impl ImportFixture {
    fn new(oversized: bool) -> Result<Self> {
        Self::with_wrapper(oversized, false)
    }
    fn with_wrapper(oversized: bool, wrapped: bool) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let absolute = fs::canonicalize(root.path())?;
        let mut paths = Vec::new();
        for relative in [
            "originals/2020/trips/shared.jpg",
            "originals/2020/family/one.jpg",
            "originals/2021/nature/two.jpg",
        ] {
            let path = absolute.join(relative);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(
                &path,
                b"tiny synthetic original; no renderer or original reader is invoked",
            )?;
            paths.push(NativePath::from_path(&path));
        }
        let raw_root = absolute.join("sealed/raw");
        fs::create_dir_all(&raw_root)?;
        let mut inspection = Fixture::new();
        let old = inspection.revision().to_owned();
        let source = inspection.open();
        let template = source.capture_manifest(&old)?;
        drop(source);
        let raw = vec![
            (0..4099).map(|n| (n % 251) as u8).collect::<Vec<_>>(),
            (0..3077).map(|n| (n % 239) as u8).collect(),
        ];
        let mut manifests = Vec::new();
        let mut artifacts = Vec::new();
        inspection.seal.selected.clear();
        for (index, bytes) in raw.iter().enumerate() {
            let relative = format!("catalog-{index}.opaque");
            let path = raw_root.join(&relative);
            fs::write(&path, bytes)?;
            let copy = Source::open(&path, u64::MAX)?.before.clone();
            let mut historical = copy.clone();
            historical.object = format!("historical-catalog-{index}");
            historical.changed = "historical captured identity".into();
            let mut manifest = template.clone();
            manifest.artifacts = vec![Artifact {
                source: NativePath::from_path(Path::new("/never-open-historical-source")),
                role: "main".into(),
                relative: NativePath::from_path(Path::new(&relative)),
                stored: format!("raw/{relative}"),
                revision: historical,
                blake3: blake3::hash(bytes).to_hex().to_string(),
            }];
            let revision = crate::lightroom::json_digest(&manifest.artifacts)?;
            manifest.revision_id = Some(revision.clone());
            let text = serde_json::to_string(&manifest)?;
            inspection.seal.selected.push(SelectedCapture {
                revision: revision.clone(),
                family: format!("family-{}", 2020 + index),
                family_evidence_digest: "a".repeat(64),
                manifest_blake3: blake3::hash(text.as_bytes()).to_hex().to_string(),
                evidence_revision: 9,
            });
            artifacts.push(ArtifactInput {
                capture_revision: revision.clone(),
                member_index: 0,
                mapping: ArtifactMapping {
                    root: NativePath::from_path(&raw_root),
                    relative: NativePath::from_path(Path::new(&relative)),
                    copy_identity: copy,
                },
            });
            manifests.push((revision, text));
        }
        inspection.seal.approval.document_blake3 = blake3::hash(APPROVAL).to_hex().to_string();
        inspection.edit(|db| {
            (|| -> Result<()> {
                for table in ["captures","schema_objects","tables","rows","entities","references_out","paths","packets","metadata_facts","issues","family_choices"] {
                    db.execute(&format!("DELETE FROM {table} WHERE revision=?"),[&old])?;
                }
                for (index,(revision,manifest)) in manifests.iter().enumerate() {
                    db.execute("INSERT INTO captures(revision,lineage,path,manifest,stage,evidence_revision) VALUES(?1,?2,'never-open',?3,'inspection_complete_with_reported_gaps',9)",params![revision,format!("lineage-{index}"),manifest])?;
                    db.execute("INSERT INTO family_choices VALUES(?1,?2,?3,'synthetic test choice')",params![format!("family-{}",2020+index),revision,"a".repeat(64)])?;
                    for (table,columns) in [
                        ("AgLibraryFile",vec!["id_local"]),
                        ("Adobe_images",vec!["id_local","rootFile","masterImage","fileFormat","developSettingsIDCache","copyName","rating","pick","colorLabels"]),
                        ("Adobe_imageDevelopSettings",vec!["id_local","text"]),
                        ("AgLibraryKeyword",vec!["id_local","name","parent"]),
                        ("AgLibraryCollection",vec!["id_local","name","parent","creationId"]),
                        ("AgLibraryKeywordImage",vec!["id_local","image","tag"]),
                        ("AgLibraryCollectionImage",vec!["id_local","image","collection","positionInCollection"]),
                    ] { schema(db,revision,table,&columns)?; }
                    for key in [10,11] {
                        row(db,revision,"AgLibraryFile",key,vec![id(key)])?;
                        let path = &paths[if key==10 {0} else {index+1}];
                        // Every candidate origin is explicitly absent. No XMP
                        // presence/completeness is inferred from a filename.
                        let observations = ["embedded","sidecar_xmp","sidecar_XMP","sidecar_appended_xmp","sidecar_appended_XMP"].map(|origin|serde_json::json!({"origin":origin,"status":"Absent","revision":{"length":0,"blake3":"a".repeat(64),"modified_unix_ns":null},"packets":0,"parse_inputs":0,"issues":[]}));
                        db.execute("INSERT INTO paths(revision,source_id,original,inspection_path,state,evidence) VALUES(?1,?2,'synthetic',?3,'available_packets_retained',?4)",params![revision,source_id("AgLibraryFile",key),serde_json::to_string(path)?,serde_json::json!({"inspections":observations}).to_string()])?;
                    }
                    for (key,file,master,ev) in [(20,10,0,1.0),(21,10,20,-1.0),(22,11,0,2.0)] {
                        let settings = key+10;
                        row(db,revision,"Adobe_images",key,vec![id(key),id(file),id(master),cell("JPEG"),id(settings),cell(if master==0 {"Original"} else {"Virtual"}),id(if key==21 {2}else{4}),id(1),cell("Green")])?;
                        row(db,revision,"Adobe_imageDevelopSettings",settings,vec![id(settings),cell(&format!("{}{{ProcessVersion='11.0',Exposure2012={},UnknownPlugin={{opaque='keep'}}}}",if wrapped {"s = "} else {""},ev+index as f64*0.25))])?;
                        link(db,revision,"Adobe_images",key,"rootFile","AgLibraryFile",file)?;
                        link(db,revision,"Adobe_images",key,"developSettingsIDCache","Adobe_imageDevelopSettings",settings)?;
                        if master!=0 { link(db,revision,"Adobe_images",key,"masterImage","Adobe_images",master)?; }
                    }
                    row(db,revision,"AgLibraryKeyword",50,vec![id(50),cell(&format!("Family-{index}")),Cell::Null])?;
                    row(db,revision,"AgLibraryCollection",60,vec![id(60),cell(&format!("Year-{}",2020+index)),Cell::Null,cell("com.adobe.ag.library.collection")])?;
                    row(db,revision,"AgLibraryKeywordImage",70,vec![id(70),id(21),id(50)])?;
                    row(db,revision,"AgLibraryCollectionImage",80,vec![id(80),id(21),id(60),id(3)])?;
                    link(db,revision,"AgLibraryKeywordImage",70,"image","Adobe_images",21)?;
                    link(db,revision,"AgLibraryKeywordImage",70,"tag","AgLibraryKeyword",50)?;
                    link(db,revision,"AgLibraryCollectionImage",80,"image","Adobe_images",21)?;
                    link(db,revision,"AgLibraryCollectionImage",80,"collection","AgLibraryCollection",60)?;
                    if oversized && index==0 {
                        for key in [90,91,92] {
                            let name = if key==90 {cell(&"n".repeat(4097))}else{cell("retained")};
                            let mut cells = vec![id(key),id(10),id(0),cell("JPEG"),Cell::Null,name,id(0),id(0),Cell::Null];
                            if key==91 { cells[5]=Cell::Blob(vec![7;5*1024*1024]); }
                            row(db,revision,"Adobe_images",key,cells)?;
                            link(db,revision,"Adobe_images",key,"rootFile","AgLibraryFile",10)?;
                            if key==92 {
                                db.execute("UPDATE rows SET key_json=?1 WHERE revision=?2 AND source_id=?3",params![serde_json::to_string(&vec![Cell::Text(vec![b'k';40000])])?,revision,source_id("Adobe_images",key)])?;
                            }
                        }
                    }
                    db.execute("UPDATE tables SET expected=(SELECT count(*) FROM rows r WHERE r.revision=tables.revision AND r.table_name=tables.name),retained=(SELECT count(*) FROM rows r WHERE r.revision=tables.revision AND r.table_name=tables.name) WHERE revision=?",[revision])?;
                }
                Ok(())
            })().unwrap();
        });
        Ok(Self {
            _root: root,
            inspection,
            destination: absolute.join("one-destination"),
            policy: Policy {
                import_source: OWNER.into(),
                overlap: OverlapPolicy::ReuseExactPath {
                    reason: "explicit synthetic shared original decision".into(),
                },
                keyword_overlap: KeywordOverlap::ReuseExactHierarchy {
                    reason: "synthetic exact hierarchy reuse".into(),
                },
                artifacts,
                supplements: vec![],
            },
            paths,
            raw,
        })
    }
    fn open(&self) -> Result<Catalog> {
        let catalog = Catalog::open(&self.destination)?;
        // Temporary module registration is separate from this test-only file.
        super::importer::install(&catalog.db)?;
        super::reconciliation::install(&catalog.db)?;
        Ok(catalog)
    }
    fn limits() -> ArtifactLimits {
        ArtifactLimits {
            maximum_bytes: 1024 * 1024,
            open_deadline_ms: 30000,
            chunk_deadline_ms: 10000,
            chunk_bytes: 512,
        }
    }
    fn key(&self, catalog: &Catalog, index: usize, key: i64) -> Result<VariantKey> {
        super::images::mapped_image(
            &catalog.db,
            OWNER,
            &SourceKey {
                capture_revision: self.inspection.seal.selected[index].revision.clone(),
                table: "Adobe_images".into(),
                key: vec![id(key)],
            },
        )
    }
}
fn drive(worker: &mut Worker<'_>, catalog: &mut Catalog, stage: Stage) -> Result<Progress> {
    for _ in 0..4000 {
        let step = worker.step(catalog, &|| false)?;
        ensure!(
            step.needs_decision.is_none(),
            "unexpected fixture decision: {:?}",
            step.needs_decision
        );
        if step.progress.stage == stage {
            return Ok(step.progress);
        }
    }
    anyhow::bail!("bounded fixture failed to reach {stage:?}")
}
fn count(catalog: &Catalog, sql: &str) -> Result<i64> {
    Ok(catalog.db.query_row(sql, [], |r| r.get(0))?)
}

#[test]
fn two_families_share_files_keep_variants_and_resume_full_worker() -> Result<()> {
    let fixture = ImportFixture::new(false)?;
    let source = fixture.inspection.open();
    let source_before = fs::read(&fixture.inspection.path)?;
    let mut catalog = fixture.open()?;
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    assert!(worker.step(&mut catalog, &|| true).is_err());
    assert_eq!(
        catalog.selected_import_progress(&run.id)?.stage,
        Stage::Custody
    );
    // Interrupt in durable custody rather than only at a phase boundary.
    worker.step(&mut catalog, &|| false)?;
    let cursor = catalog.selected_import_progress(&run.id)?;
    drop(worker);
    drop(catalog);
    let mut catalog = fixture.open()?;
    assert_eq!(
        serde_json::to_vec(&catalog.selected_import_progress(&run.id)?)?,
        serde_json::to_vec(&cursor)?
    );
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::Files)?;
    let files_cursor = worker.step(&mut catalog, &|| false)?.progress;
    assert_eq!(files_cursor.stage, Stage::Files);
    assert!(files_cursor.cursor.is_some());
    drop(worker);
    drop(catalog);
    let mut catalog = fixture.open()?;
    assert_eq!(
        serde_json::to_vec(&catalog.selected_import_progress(&run.id)?)?,
        serde_json::to_vec(&files_cursor)?
    );
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::ArtifactCustody)?;
    worker.step(&mut catalog, &|| false)?;
    let pending:i64=catalog.db.query_row("SELECT count(*) FROM migration_artifacts a JOIN migration_evidence e ON e.id=a.evidence WHERE e.committed>0 AND e.complete=0",[],|r|r.get(0))?;
    assert_eq!(pending, 1);
    let raw_cursor = catalog.selected_import_progress(&run.id)?;
    drop(worker);
    drop(catalog);
    let mut catalog = fixture.open()?;
    assert_eq!(
        serde_json::to_vec(&catalog.selected_import_progress(&run.id)?)?,
        serde_json::to_vec(&raw_cursor)?
    );
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    let complete = drive(&mut worker, &mut catalog, Stage::Complete)?;
    assert!(complete.complete);
    assert_eq!(count(&catalog, "SELECT count(*) FROM assets")?, 3);
    assert_eq!(count(&catalog, "SELECT count(*) FROM image_import_map")?, 6);
    assert_eq!(count(&catalog, "SELECT count(*) FROM catalog_images")?, 6);
    assert_eq!(
        count(&catalog, "SELECT count(*) FROM migration_originals")?,
        4
    );
    assert_eq!(
        count(&catalog, "SELECT count(*) FROM migration_file_metadata")?,
        20
    );
    assert_eq!(
        count(&catalog, "SELECT count(*) FROM organization_collections")?,
        2
    );
    assert_eq!(
        count(
            &catalog,
            "SELECT count(*) FROM organization_collection_members"
        )?,
        2
    );
    assert_eq!(
        count(
            &catalog,
            "SELECT count(*) FROM organization_keywords WHERE kind='hierarchical'"
        )?,
        2
    );
    let mut shared = None;
    for index in 0..2 {
        let master = fixture.key(&catalog, index, 20)?;
        let virtual_copy = fixture.key(&catalog, index, 21)?;
        assert_eq!(master.asset_id, virtual_copy.asset_id);
        assert_ne!(master.variant_id, virtual_copy.variant_id);
        if let Some(asset) = &shared {
            assert_eq!(asset, &master.asset_id);
        } else {
            shared = Some(master.asset_id.clone());
        }
        for (key, expected) in [(20, 1.0), (21, -1.0), (22, 2.0)] {
            let native = fixture.key(&catalog, index, key)?;
            assert_eq!(
                catalog
                    .edit_variant(&native)?
                    .recipe
                    .validate()?
                    .settings()
                    .exposure_ev,
                expected + index as f32 * 0.25
            );
        }
        let revision = &fixture.inspection.seal.selected[index].revision;
        let report = catalog.selected_import_reconciliation(&run.id, revision)?;
        assert_eq!(report.native_images, 3);
        assert_eq!(report.native_original_mappings, 2);
        assert_eq!(report.raw_artifacts, 1);
        assert!(!report.adobe_rendering_equivalent);
        let capture = catalog.retained_migration_records(
            source.binding_blake3(),
            revision,
            Collection::Captures,
            0,
            2,
        )?;
        let (descriptor, state) = catalog.migration_artifact(capture[0].0, 0)?;
        assert!(state.complete);
        assert_eq!(state.length, fixture.raw[index].len() as u64);
        assert_eq!(descriptor.capture_revision, *revision);
        let mut restored = Vec::new();
        while restored.len() < fixture.raw[index].len() {
            restored.extend(super::evidence::read(
                &catalog.db,
                &state.id,
                restored.len() as u64,
            )?);
        }
        assert_eq!(restored, fixture.raw[index]);
    }
    for path in &fixture.paths {
        let native = path.to_path()?;
        let parent = NativePath::from_path(native.parent().unwrap());
        // Folder identities alias verbatim drive/UNC spellings. Build the
        // expected alias directly, without using the product folder parser.
        // The physical asset assertion below still requires the original bytes.
        let parent = match parent {
            NativePath::WindowsWide(units) => {
                let unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
                let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
                let ordinary = if let Some(tail) = units.strip_prefix(unc.as_slice()) {
                    r"\\".encode_utf16().chain(tail.iter().copied()).collect()
                } else if let Some(tail) = units.strip_prefix(verbatim.as_slice()) {
                    tail.to_vec()
                } else {
                    units
                };
                NativePath::WindowsWide(ordinary)
            }
            other => other,
        };
        assert_eq!(
            catalog.db.query_row(
                "SELECT count(*) FROM organization_folders WHERE locator=?",
                [serde_json::to_string(&parent)?],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(
            catalog.db.query_row(
                "SELECT count(*) FROM assets WHERE location=?",
                [crate::catalog_storage::encoded_bytes(path)],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
    }
    for revision in &fixture.inspection.seal.excluded_revisions {
        assert_eq!(
            catalog.db.query_row(
                "SELECT count(*) FROM migration_retained_records WHERE revision=?",
                [revision],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        assert_eq!(
            catalog.db.query_row(
                "SELECT count(*) FROM image_import_map WHERE capture_revision=?",
                [revision],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
    }
    let counts = (
        count(&catalog, "SELECT count(*) FROM migration_run_items")?,
        count(&catalog, "SELECT count(*) FROM edit_changes")?,
    );
    worker.step(&mut catalog, &|| false)?;
    assert_eq!(
        counts,
        (
            count(&catalog, "SELECT count(*) FROM migration_run_items")?,
            count(&catalog, "SELECT count(*) FROM edit_changes")?
        )
    );
    assert_eq!(fs::read(&fixture.inspection.path)?, source_before);
    Ok(())
}

#[test]
fn interrupted_image_cursor_reuses_registration_and_preserves_later_user_state() -> Result<()> {
    let fixture = ImportFixture::new(false)?;
    let source = fixture.inspection.open();
    let mut catalog = fixture.open()?;
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::Masters)?;
    catalog.db.execute_batch("CREATE TRIGGER fail_import_cursor BEFORE INSERT ON migration_run_items WHEN NEW.stage='\"Masters\"' BEGIN SELECT RAISE(ABORT,'fixture after image registration before cursor'); END;")?;
    let before = catalog.selected_import_progress(&run.id)?;
    assert!(worker.step(&mut catalog, &|| false).is_err());
    assert_eq!(
        serde_json::to_vec(&catalog.selected_import_progress(&run.id)?)?,
        serde_json::to_vec(&before)?
    );
    assert_eq!(count(&catalog, "SELECT count(*) FROM image_import_map")?, 1);
    catalog
        .db
        .execute_batch("DROP TRIGGER fail_import_cursor;")?;
    let key = fixture.key(&catalog, 0, 20)?;
    // Seed a full fingerprint of these known synthetic bytes, then exercise
    // the actual relink API. The migration itself did not inspect originals.
    let old_path = fixture.paths[0].to_path()?;
    let bytes = fs::read(&old_path)?;
    catalog.db.execute(
        "UPDATE assets SET fingerprint=?1 WHERE id=?2",
        params![blake3::hash(&bytes).to_hex().to_string(), key.asset_id],
    )?;
    let relink_path = fixture._root.path().join("user/relinked.jpg");
    fs::create_dir_all(relink_path.parent().unwrap())?;
    fs::write(&relink_path, &bytes)?;
    let relink = NativePath::from_path(&fs::canonicalize(&relink_path)?);
    let plan = catalog.begin_relink(crate::catalog_storage::RelinkScope::Asset {
        asset_id: key.asset_id.clone(),
        destinations: vec![relink.clone()],
    })?;
    catalog.prepare_relink_batch(&plan.id, 1)?;
    catalog.apply_relink(&plan.id)?;
    drop(worker);
    drop(catalog);
    let mut catalog = fixture.open()?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    worker.step(&mut catalog, &|| false)?;
    assert_eq!(count(&catalog, "SELECT count(*) FROM image_import_map")?, 1);
    assert_eq!(
        catalog.db.query_row(
            "SELECT location FROM assets WHERE id=?",
            [&key.asset_id],
            |r| r.get::<_, Vec<u8>>(0)
        )?,
        crate::catalog_storage::encoded_bytes(&relink)
    );
    // A second interruption occurs after recipe installation but before the
    // walk cursor commits. Later user edits must survive that exact replay.
    drive(&mut worker, &mut catalog, Stage::CurrentDevelop)?;
    catalog.db.execute_batch("CREATE TRIGGER fail_develop_cursor BEFORE INSERT ON migration_run_items WHEN NEW.stage='\"CurrentDevelop\"' BEGIN SELECT RAISE(ABORT,'fixture after recipe before cursor'); END;")?;
    assert!(worker.step(&mut catalog, &|| false).is_err());
    catalog
        .db
        .execute_batch("DROP TRIGGER fail_develop_cursor;")?;
    let view = catalog.edit_variant(&key)?;
    assert_eq!(view.recipe.validate()?.settings().exposure_ev, 1.0);
    let edited = catalog.save_edit_recipe(
        &key,
        view.revision,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 3.0,
            ..Default::default()
        }),
    )?;
    drop(worker);
    drop(catalog);
    let mut catalog = fixture.open()?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::Complete)?;
    assert_eq!(
        catalog.edit_variant(&key)?.recipe_digest,
        edited.recipe_digest
    );
    assert_eq!(
        catalog.db.query_row(
            "SELECT location FROM assets WHERE id=?",
            [&key.asset_id],
            |r| r.get::<_, Vec<u8>>(0)
        )?,
        crate::catalog_storage::encoded_bytes(&relink)
    );
    Ok(())
}

#[test]
fn wrong_source_approval_and_corrupt_native_counts_cannot_complete() -> Result<()> {
    let fixture = ImportFixture::new(false)?;
    let source = fixture.inspection.open();
    let mut catalog = fixture.open()?;
    assert!(
        catalog
            .begin_selected_import(&source, b"not approved", &fixture.policy)
            .is_err()
    );
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let other = Fixture::new();
    let other_source = other.open();
    assert!(
        catalog
            .step_selected_import(&other_source, &run.id)
            .is_err()
    );
    assert_eq!(
        catalog.selected_import_progress(&run.id)?.stage,
        Stage::Custody
    );
    let mut bad_policy = fixture.policy.clone();
    bad_policy.artifacts[0].capture_revision =
        fixture.inspection.seal.excluded_revisions[0].clone();
    assert!(
        catalog
            .begin_selected_import(&source, APPROVAL, &bad_policy)
            .is_err()
    );
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::Reconciliation)?;
    let before = catalog.selected_import_progress(&run.id)?;
    catalog.db.execute("DELETE FROM image_import_map WHERE rowid=(SELECT rowid FROM image_import_map WHERE capture_revision=? LIMIT 1)",[&fixture.inspection.seal.selected[0].revision])?;
    assert!(
        worker
            .step(&mut catalog, &|| false)
            .unwrap_err()
            .to_string()
            .contains("native source mappings")
    );
    assert_eq!(
        serde_json::to_vec(&catalog.selected_import_progress(&run.id)?)?,
        serde_json::to_vec(&before)?
    );
    assert!(!catalog.selected_import_progress(&run.id)?.complete);
    Ok(())
}

#[test]
fn oversized_image_name_cells_and_key_are_retained_without_cursor_stall() -> Result<()> {
    let fixture = ImportFixture::new(true)?;
    let source = fixture.inspection.open();
    let mut catalog = fixture.open()?;
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    let done = drive(&mut worker, &mut catalog, Stage::Complete)?;
    assert!(done.complete);
    assert_eq!(count(&catalog, "SELECT count(*) FROM image_import_map")?, 6);
    let report = catalog
        .selected_import_reconciliation(&run.id, &fixture.inspection.seal.selected[0].revision)?;
    assert_eq!(report.walked["Masters"], 6);
    assert_eq!(report.walked["VirtualCopies"], 6);
    assert!(
        report
            .classifications
            .keys()
            .any(|key| key.contains("retained") || key.contains("Retained"))
    );
    for key in [90, 91, 92] {
        let retained:i64=catalog.db.query_row("SELECT count(*) FROM migration_record_lookup WHERE input=?1 AND revision=?2 AND collection=3 AND source_id=?3",params![source.binding_blake3(),fixture.inspection.seal.selected[0].revision,source_id("Adobe_images",key)],|r|r.get(0))?;
        assert_eq!(retained, 1);
    }
    Ok(())
}

#[test]
fn wrapped_current_settings_cross_worker_render_and_reopen() -> Result<()> {
    let fixture = ImportFixture::with_wrapper(false, true)?;
    let source = fixture.inspection.open();
    let mut catalog = fixture.open()?;
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::Complete)?;
    drop(worker);
    let master = fixture.key(&catalog, 0, 20)?;
    let copy = fixture.key(&catalog, 0, 21)?;
    assert_eq!(master.asset_id, copy.asset_id);
    assert_ne!(master, copy);
    for (key, ev, expected) in [
        (&master, 1.0, [0.25, 0.5, 1.0, 0.75]),
        (&copy, -1.0, [0.0625, 0.125, 0.25, 0.75]),
    ] {
        let stored = catalog.edit_variant(key)?;
        let recipe = stored.recipe.validate()?;
        assert_eq!(recipe.settings().exposure_ev, ev);
        let input = crate::edit::fixture(1, 1, vec![[0.125, 0.25, 0.5, 0.75]]);
        let rendered = crate::edit::render_recipe(
            &input,
            &recipe,
            crate::edit::RenderPurpose::ExportExact,
            crate::edit::RenderLimits::default(),
            &(),
        )?;
        assert!(rendered.exact());
        assert_eq!(rendered.as_rendered().pixels, vec![expected]);
    }
    let extraction_paths: Vec<Vec<crate::lightroom::adobe::Key>> = catalog
        .db
        .prepare("SELECT result FROM migration_metadata WHERE slot='current_develop'")?
        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| {
            let result: super::metadata::ResultRecord = serde_json::from_slice(&r?)?;
            assert_eq!(result.state, "translated_with_appearance_gaps");
            let extraction = result.extraction.unwrap();
            assert!(!extraction.adobe_rendering_equivalent);
            assert!(extraction.properties.iter().any(|p| p.name == "opaque"
                && p.disposition == crate::lightroom::adobe::Disposition::RetainedOnly));
            Ok(extraction.input.settings_path)
        })
        .collect::<Result<_>>()?;
    assert_eq!(extraction_paths.len(), 6);
    assert!(
        extraction_paths
            .iter()
            .all(|p| p == &vec![crate::lightroom::adobe::Key::Name("s".into())])
    );
    let before = catalog.edit_variant(&master)?;
    catalog.save_edit_recipe(
        &master,
        before.revision,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 2.0,
            ..before.recipe.validate()?.settings().clone()
        }),
    )?;
    let edited = catalog.edit_variant(&master)?;
    drop(catalog);
    let mut catalog = fixture.open()?;
    let resumed = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    assert_eq!(resumed.id, run.id);
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    assert!(worker.step(&mut catalog, &|| false)?.progress.complete);
    assert_eq!(catalog.edit_variant(&master)?.revision, edited.revision);
    assert_eq!(catalog.edit_variant(&master)?.recipe, edited.recipe);
    assert_eq!(
        catalog
            .edit_variant(&copy)?
            .recipe
            .validate()?
            .settings()
            .exposure_ev,
        -1.0
    );
    Ok(())
}

#[path = "current_repair_tests.rs"]
mod current_repair_tests;
