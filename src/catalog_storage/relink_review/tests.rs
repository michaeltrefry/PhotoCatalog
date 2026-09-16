use super::*;
use crate::{
    catalog_edits::{VariantKey, install_import_recipe},
    catalog_images::{ImageRole, ImportImageRequest, TranslationState},
    catalog_migration::{
        file_metadata::{Association, Origin, Projection},
        organization::SourceRecord,
        originals::{OriginalDecision, OriginalRequest, SourceKey},
    },
    edit::{Recipe, RecipeV1},
    lightroom::{
        migration_source::{Collection, tests::Fixture},
        plan::Cell,
    },
};

const XML:&[u8]=br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="source" xmlns:private="urn:original:unknown" private:keep="exact bytes"/></rdf:RDF></x:xmpmeta>"#;
struct Imported {
    _temp: tempfile::TempDir,
    _source: Fixture,
    catalog: Catalog,
    old: PathBuf,
    candidate: PathBuf,
    master: VariantKey,
    copy: VariantKey,
    bytes: Vec<u8>,
}
impl Imported {
    fn new(retained_digest: bool) -> Result<Self> {
        let temp = tempfile::Builder::new().tempdir_in(std::env::temp_dir().canonicalize()?)?;
        let old = temp.path().join("old/nested/original.png");
        let candidate = temp.path().join("new/nested/original.png");
        fs::create_dir_all(candidate.parent().unwrap())?;
        image::RgbImage::from_pixel(32, 24, image::Rgb([30u8, 50, 80])).save(&candidate)?;
        let png = fs::read(&candidate)?;
        let mut payload = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
        let packet_offset = png.len() - 12 + 8 + payload.len();
        payload.extend_from_slice(XML);
        let mut chunk = b"iTXt".to_vec();
        chunk.extend_from_slice(&payload);
        let mut crc = 0xffff_ffffu32;
        for byte in &chunk {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
            }
        }
        let mut bytes = png[..png.len() - 12].to_vec();
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&chunk);
        bytes.extend_from_slice(&(!crc).to_be_bytes());
        bytes.extend_from_slice(&png[png.len() - 12..]);
        fs::write(&candidate, &bytes)?;
        let mut fixture = Fixture::new();
        let revision = fixture.revision().to_owned();
        let approval = b"selected synthetic relink custody";
        fixture.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
        let native = NativePath::from_path(&old);
        let source_revision = crate::xmp_packets::SourceRevision {
            length: bytes.len() as u64,
            blake3: if retained_digest {
                blake3::hash(&bytes).to_hex().to_string()
            } else {
                String::new()
            },
            modified_unix_ns: None,
        };
        fixture.edit(|db| {
            db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,'relink-file','AgLibraryFile',?2,'[]')",params![revision,serde_json::to_string(&vec![Cell::Integer(17)]).unwrap()]).unwrap();
            let observation=serde_json::json!({"origin":"embedded","status":"Complete","revision":source_revision,"packets":1,"parse_inputs":1,"issues":[]});
            db.execute("INSERT INTO paths(revision,source_id,original,inspection_path,state,evidence) VALUES(?1,'relink-file','retained old folder',?2,'available_packets_retained',?3)",params![revision,json(&native).unwrap(),serde_json::json!({"inspections":[observation]}).to_string()]).unwrap();
            let hash=blake3::hash(XML).to_hex().to_string();
            let detail=serde_json::json!({"container":"PngItxt","ranges":[{"offset":packet_offset,"length":XML.len()}],"group":"embedded:packet:0","attributes":{"retained":"unknown"},"source_revision":source_revision,"inspection_status":"Complete","source_path":native});
            db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,detail) VALUES(?1,'relink-file','embedded:packet:0',?2,?3,?4)",params![revision,hash,XML,detail.to_string()]).unwrap();
            let detail=serde_json::json!({"transformation":"Identity","packet_indices":[0],"group":"embedded:packet:0","input_blake3":hash,"source_revision":source_revision,"inspection_status":"Complete"});
            db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,decoded,detail) VALUES(?1,'relink-file','embedded:parse_input:0',?2,x'',?3,?4)",params![revision,hash,XML,detail.to_string()]).unwrap();
        });
        let source = fixture.open();
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        catalog.begin_migration_retention(&source, approval)?;
        for _ in 0..1000 {
            if catalog.step_migration_retention(&source)?.complete {
                break;
            }
        }
        ensure!(
            catalog
                .migration_retention_progress(source.binding_blake3())?
                .complete,
            "fixture custody incomplete"
        );
        let records = |catalog: &Catalog, collection| {
            catalog.retained_migration_records(
                source.binding_blake3(),
                &revision,
                collection,
                0,
                100,
            )
        };
        let file = records(&catalog, Collection::Rows)?
            .into_iter()
            .find(|(_, r)| r.fields["source_id"].text().ok() == Some("relink-file"))
            .unwrap()
            .0;
        let retained_path = records(&catalog, Collection::Paths)?
            .into_iter()
            .find(|(_, r)| r.fields["source_id"].text().ok() == Some("relink-file"))
            .unwrap()
            .0;
        let packet_records = records(&catalog, Collection::Packets)?
            .into_iter()
            .filter(|(_, r)| r.fields["source_id"].text().ok() == Some("relink-file"))
            .map(|(n, _)| n)
            .collect();
        let key = SourceKey {
            capture_revision: revision.clone(),
            table: "AgLibraryFile".into(),
            key: vec![Cell::Integer(17)],
        };
        let asset = catalog
            .register_migration_original(&OriginalRequest {
                import_source: "relink-fixture".into(),
                source: key.clone(),
                decision: OriginalDecision::Create { path: native },
                retained_record: file,
            })?
            .asset_id;
        let mut request = ImportImageRequest {
            import_source: "relink-fixture".into(),
            capture_revision: revision,
            source_table: "Adobe_images".into(),
            source_id: "master".into(),
            input_digest: "master-row".into(),
            adapter_version: "fixture-1".into(),
            asset_id: asset,
            claim_reserved_master: true,
            role: ImageRole::Master,
            master: None,
            label: "Imported original".into(),
        };
        let master = catalog.register_import_image(&request)?.key;
        request.claim_reserved_master = false;
        request.role = ImageRole::Virtual;
        request.master = Some(master.clone());
        request.source_id = "virtual".into();
        request.input_digest = "virtual-row".into();
        request.label = "Imported virtual copy".into();
        let copy = catalog.register_import_image(&request)?.key;
        let tx = catalog.db.transaction()?;
        for (key, exposure) in [(&master, 0.5), (&copy, 1.5)] {
            let recipe = Recipe::V1(RecipeV1 {
                exposure_ev: exposure,
                ..Default::default()
            })
            .validate()?;
            install_import_recipe(
                &tx,
                key,
                0,
                &recipe,
                &serde_json::json!({"translation":"supported","import_source":"relink-fixture"}),
                TranslationState::Translated,
            )?;
        }
        tx.commit()?;
        let projected = catalog.project_migration_file_metadata(
            Some(&source),
            &Projection {
                file: SourceRecord {
                    source: key,
                    retained_record: file,
                },
                retained_path,
                origin: Origin::Embedded,
                packet_records,
                import_source: "relink-fixture".into(),
                association: Association::Confirmed {
                    reason: "selected embedded source".into(),
                },
                supplement: None,
            },
        )?;
        ensure!(
            projected.observation.is_some(),
            "fixture historical metadata must be projected"
        );
        drop(source);
        Ok(Self {
            _temp: temp,
            _source: fixture,
            catalog,
            old,
            candidate,
            master,
            copy,
            bytes,
        })
    }
    fn prepare(&mut self) -> Result<RelinkPlan> {
        let plan = self.catalog.begin_relink_review(RelinkScope::Prefix {
            from: PathReference::native(self.old.parent().unwrap().parent().unwrap()),
            destinations: vec![NativePath::from_path(
                self.candidate.parent().unwrap().parent().unwrap(),
            )],
        })?;
        self.catalog.prepare_relink_batch(&plan.id, 1)
    }
    fn confirm(&mut self, plan: &RelinkPlan) -> Result<RelinkPlan> {
        self.catalog.confirm_relink_associations(
            &plan.id,
            plan.revision,
            plan.confirmation_token.as_deref().unwrap(),
            "no_retained_original_digest",
        )
    }
    fn custody(&self) -> Result<serde_json::Value> {
        let mut tables = std::collections::BTreeMap::new();
        for table in [
            "migration_originals",
            "migration_retained_records",
            "migration_evidence",
            "image_import_map",
            "edit_variants",
            "edit_recipe_nodes",
            "edit_changes",
            "metadata_observations",
            "metadata_packets",
            "metadata_sources",
        ] {
            let mut statement = self
                .catalog
                .db
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))?;
            let columns = statement.column_count();
            let rows = statement
                .query_map([], |r| {
                    (0..columns)
                        .map(|i| {
                            r.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            tables.insert(table, rows);
        }
        Ok(serde_json::to_value(tables)?)
    }
    fn hydrate(&mut self, hash: &str) -> Result<bool> {
        let expected = self.catalog.edit_render_identity(&self.copy)?;
        let metadata = crate::media::Metadata {
            format: "png".into(),
            width: 32,
            height: 24,
            orientation: 1,
            camera_make: None,
            camera_model: None,
            captured_at: None,
            lens: None,
            preview_source: "native fixture".into(),
        };
        let current = self.catalog.preview_original_path(&self.master.asset_id)?;
        Ok(self
            .catalog
            .commit_preview_hydration(
                &expected,
                crate::HydrationPublication {
                    source: &current,
                    fingerprint: hash,
                    metadata: &metadata,
                    preview_key: &"b".repeat(64),
                },
                || Ok(()),
                || Ok(()),
            )?
            .is_some())
    }
}
#[test]
fn imported_unknown_digest_requires_explicit_review_and_preserves_custody_through_hydration_undo()
-> Result<()> {
    let mut f = Imported::new(false)?;
    let before = f.custody()?;
    let images = [f.catalog.image(&f.master)?, f.catalog.image(&f.copy)?];
    let plan = f.prepare()?;
    assert_eq!(
        (plan.matched, plan.unverified, plan.unresolved_sources),
        (0, 1, 0)
    );
    assert_eq!(
        f.catalog.relink_sources(&plan.id, 1, 0, 10)?[0].status,
        "historical"
    );
    assert!(f.catalog.apply_relink(&plan.id).is_err());
    assert!(
        f.catalog
            .confirm_relink_associations(
                &plan.id,
                plan.revision,
                plan.confirmation_token.as_deref().unwrap(),
                "yes"
            )
            .is_err()
    );
    let confirmed = f.confirm(&plan)?;
    assert_eq!(
        (
            confirmed.matched,
            confirmed.user_confirmed,
            confirmed.unresolved
        ),
        (0, 1, 0)
    );
    assert!(f.confirm(&plan).is_err());
    f.catalog.apply_relink_cancellable(
        &plan.id,
        confirmed.revision,
        &AtomicBool::new(false),
        |_| {},
    )?;
    assert_eq!(
        f.catalog.render_identity(&f.master.asset_id)?.fingerprint,
        None
    );
    assert_eq!(f.custody()?, before);
    assert!(f.hydrate(&"c".repeat(64)).is_err());
    let hash = blake3::hash(&f.bytes).to_hex().to_string();
    assert!(f.hydrate(&hash)?);
    assert_eq!(f.custody()?, before);
    f.catalog.undo_relink(&plan.id)?;
    assert_eq!(
        f.catalog.preview_original_path(&f.master.asset_id)?,
        NativePath::from_path(&f.old)
    );
    assert_eq!(
        f.catalog
            .render_identity(&f.master.asset_id)?
            .fingerprint
            .as_deref(),
        Some(hash.as_str())
    );
    assert_eq!(f.custody()?, before);
    for (key, old) in [(&f.master, &images[0]), (&f.copy, &images[1])] {
        let image = f.catalog.image(key)?;
        assert_eq!(
            (image.id, image.key, image.master_sequence, image.origin),
            (
                old.id.clone(),
                old.key.clone(),
                old.master_sequence,
                old.origin.clone()
            )
        );
    }
    assert_eq!(fs::read(&f.candidate)?, f.bytes);
    Ok(())
}
#[test]
fn retained_complete_original_digest_verifies_and_cannot_be_downgraded() -> Result<()> {
    let mut f = Imported::new(true)?;
    let plan = f.prepare()?;
    assert_eq!((plan.matched, plan.unverified), (1, 0));
    assert!(plan.confirmation_token.is_none());
    let wrong = f.candidate.with_file_name("wrong.png");
    fs::write(&wrong, b"different readable original")?;
    let revised = f.catalog.revise_relink(
        &plan.id,
        plan.revision,
        vec![RelinkOverride::Asset {
            asset_id: f.master.asset_id.clone(),
            candidates: vec![NativePath::from_path(&wrong)],
        }],
    )?;
    let bad = f.catalog.prepare_relink_batch(&revised.id, 1)?;
    assert_eq!((bad.matched, bad.unverified), (0, 0));
    assert_eq!(f.catalog.relink_items(&bad.id, 0, 1)?[0].status, "mismatch");
    assert!(f.catalog.apply_relink(&bad.id).is_err());
    Ok(())
}
#[test]
fn preparation_is_detached_cancellable_and_publication_rejects_stale_source() -> Result<()> {
    let mut f = Imported::new(false)?;
    let plan = f.catalog.begin_relink_review(RelinkScope::Asset {
        asset_id: f.master.asset_id.clone(),
        destinations: vec![NativePath::from_path(&f.candidate)],
    })?;
    let snapshot = f.catalog.relink_preparation(&plan.id, 1)?;
    let (send, recv) = std::sync::mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        recv.recv().unwrap();
        snapshot.prepare(&AtomicBool::new(false))
    });
    // Foreground writes succeed while the candidate reader is held.
    f.catalog.save_edit_recipe(
        &f.copy,
        1,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 2.,
            ..Default::default()
        }),
    )?;
    assert_eq!(f.catalog.relink_plan(&plan.id)?.scanned_through, 0);
    f.catalog.db.execute(
        "UPDATE assets SET path_display='changed',location=?2 WHERE id=?1",
        params![
            f.master.asset_id,
            encoded_bytes(&NativePath::from_path(&f.old.with_file_name("changed.png")))
        ],
    )?;
    send.send(())?;
    let prepared = worker.join().unwrap()?;
    assert!(f.catalog.publish_relink_preparation(prepared).is_err());
    assert_eq!(f.catalog.relink_plan(&plan.id)?.scanned_through, 0);
    let canceled = AtomicBool::new(true);
    assert!(read_evidence(&f.candidate, &canceled).is_err());
    Ok(())
}
#[test]
fn cancel_apply_and_undo_roll_back_even_after_mutation() -> Result<()> {
    let mut f = Imported::new(false)?;
    let plan = f.prepare()?;
    let plan = f.confirm(&plan)?;
    let cancel = AtomicBool::new(false);
    let before = f.custody()?;
    assert!(
        f.catalog
            .apply_relink_cancellable(&plan.id, plan.revision, &cancel, |b| {
                if matches!(b, RelinkBoundary::Updated(_)) {
                    cancel.store(true, Ordering::Release)
                }
            })
            .is_err()
    );
    assert_eq!(
        f.catalog.preview_original_path(&f.master.asset_id)?,
        NativePath::from_path(&f.old)
    );
    assert_eq!(f.custody()?, before);
    cancel.store(false, Ordering::Release);
    f.catalog
        .apply_relink_cancellable(&plan.id, plan.revision, &cancel, |_| {})?;
    assert!(
        f.catalog
            .undo_relink_cancellable(&plan.id, &cancel, |b| {
                if matches!(b, RelinkBoundary::Updated(_)) {
                    cancel.store(true, Ordering::Release)
                }
            })
            .is_err()
    );
    assert_eq!(
        f.catalog.preview_original_path(&f.master.asset_id)?,
        NativePath::from_path(&f.candidate)
    );
    assert_eq!(f.catalog.relink_plan(&plan.id)?.state, "applied");
    assert_eq!(fs::read(&f.candidate)?, f.bytes);
    Ok(())
}

#[test]
fn nested_folder_and_individual_revisions_do_not_inherit_confirmation() -> Result<()> {
    let mut f = Imported::new(false)?;
    let initial = f.prepare()?;
    let confirmed = f.confirm(&initial)?;
    let elsewhere = f
        .candidate
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("subfolder-remap");
    fs::create_dir(&elsewhere)?;
    fs::write(elsewhere.join("original.png"), &f.bytes)?;
    let revision = f.catalog.revise_relink(
        &confirmed.id,
        confirmed.revision,
        vec![RelinkOverride::Prefix {
            from: PathReference::native(f.old.parent().unwrap()),
            destinations: vec![NativePath::from_path(&elsewhere)],
        }],
    )?;
    let nested = f.catalog.prepare_relink_batch(&revision.id, 1)?;
    assert_eq!((nested.unverified, nested.user_confirmed), (1, 0));
    assert_ne!(nested.confirmation_token, initial.confirmation_token);
    assert_eq!(
        f.catalog.relink_items(&nested.id, 0, 1)?[0].destination,
        Some(NativePath::from_path(&elsewhere.join("original.png")))
    );
    let one = f.catalog.revise_relink(
        &nested.id,
        nested.revision,
        vec![RelinkOverride::Asset {
            asset_id: f.master.asset_id.clone(),
            candidates: vec![NativePath::from_path(&f.candidate)],
        }],
    )?;
    let one = f.catalog.prepare_relink_batch(&one.id, 1)?;
    assert_eq!(
        f.catalog.relink_items(&one.id, 0, 1)?[0].destination,
        Some(NativePath::from_path(&f.candidate))
    );
    assert!(
        f.catalog
            .apply_relink_cancellable(
                &confirmed.id,
                initial.revision,
                &AtomicBool::new(false),
                |_| {}
            )
            .is_err()
    );
    Ok(())
}
#[test]
fn missing_and_ambiguous_unknown_candidates_require_explicit_selection() -> Result<()> {
    let mut f = Imported::new(false)?;
    let plan = f.prepare()?;
    let other = f.candidate.with_file_name("another.png");
    fs::write(&other, &f.bytes)?;
    let two = f.catalog.revise_relink(
        &plan.id,
        plan.revision,
        vec![RelinkOverride::Asset {
            asset_id: f.master.asset_id.clone(),
            candidates: vec![
                NativePath::from_path(&f.candidate),
                NativePath::from_path(&other),
            ],
        }],
    )?;
    let two = f.catalog.prepare_relink_batch(&two.id, 1)?;
    assert_eq!(
        f.catalog.relink_items(&two.id, 0, 1)?[0].status,
        "ambiguous"
    );
    assert!(two.confirmation_token.is_none());
    let missing = f.catalog.revise_relink(
        &two.id,
        two.revision,
        vec![RelinkOverride::Asset {
            asset_id: f.master.asset_id.clone(),
            candidates: vec![NativePath::from_path(&other.with_file_name("missing.png"))],
        }],
    )?;
    let missing = f.catalog.prepare_relink_batch(&missing.id, 1)?;
    assert_eq!(
        f.catalog.relink_items(&missing.id, 0, 1)?[0].status,
        "missing"
    );
    assert!(missing.confirmation_token.is_none());
    Ok(())
}
#[test]
fn source_byte_changes_with_restored_mtime_fail_before_and_after_mutation() -> Result<()> {
    for boundary in [false, true] {
        let mut f = Imported::new(false)?;
        let p = f.prepare()?;
        let p = f.confirm(&p)?;
        let stamp = fs::metadata(&f.candidate)?.modified()?;
        let path = f.candidate.clone();
        let mut changed = f.bytes.clone();
        changed[5] ^= 0x55;
        let mutate = || -> Result<()> {
            fs::write(&path, &changed)?;
            OpenOptions::new()
                .write(true)
                .open(&path)?
                .set_times(std::fs::FileTimes::new().set_modified(stamp))?;
            Ok(())
        };
        if !boundary {
            mutate()?;
        }
        assert!(
            f.catalog
                .apply_relink_with(&p.id, |b| {
                    if boundary && matches!(b, RelinkBoundary::Updated(_)) {
                        mutate()?;
                    }
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(
            f.catalog.preview_original_path(&f.master.asset_id)?,
            NativePath::from_path(&f.old)
        );
        assert_eq!(f.catalog.relink_plan(&p.id)?.state, "ready");
    }
    Ok(())
}
#[test]
fn chained_relink_hydration_then_reverse_undo_preserves_exact_lineage() -> Result<()> {
    let mut f = Imported::new(false)?;
    let p = f.prepare()?;
    let p = f.confirm(&p)?;
    f.catalog.apply_relink(&p.id)?;
    let third = f.candidate.with_file_name("third.png");
    fs::write(&third, &f.bytes)?;
    let q = f.catalog.begin_relink_review(RelinkScope::Asset {
        asset_id: f.master.asset_id.clone(),
        destinations: vec![NativePath::from_path(&third)],
    })?;
    let q = f.catalog.prepare_relink_batch(&q.id, 1)?;
    assert_eq!(q.matched, 1);
    assert_eq!(
        f.catalog.relink_items(&q.id, 0, 1)?[0].identity_basis,
        "user_confirmed_fence"
    );
    f.catalog.apply_relink(&q.id)?;
    let hash = blake3::hash(&f.bytes).to_hex().to_string();
    assert!(f.hydrate(&hash)?);
    f.catalog.undo_relink(&q.id)?;
    f.catalog.undo_relink(&p.id)?;
    assert_eq!(
        f.catalog.preview_original_path(&f.master.asset_id)?,
        NativePath::from_path(&f.old)
    );
    assert_eq!(
        f.catalog.db.query_row(
            "SELECT count(*) FROM storage_hydration_transitions",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}
#[test]
fn hydration_does_not_forgive_intervening_metadata_or_binding_mutations() -> Result<()> {
    let mut f = Imported::new(false)?;
    let p = f.prepare()?;
    let p = f.confirm(&p)?;
    f.catalog.apply_relink(&p.id)?;
    // An independent mutation must not regain undo authority through hydration.
    f.catalog.db.execute(
        "UPDATE metadata_assets SET revision=revision+1 WHERE asset_id=?",
        [&f.master.asset_id],
    )?;
    let hash = blake3::hash(&f.bytes).to_hex().to_string();
    assert!(f.hydrate(&hash)?);
    assert!(f.catalog.undo_relink(&p.id).is_err());
    assert_eq!(
        f.catalog.db.query_row(
            "SELECT count(*) FROM storage_hydration_transitions",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}
#[test]
fn source_snapshot_cap_fails_without_partial_publication() -> Result<()> {
    let mut f = Imported::new(false)?;
    let tx = f.catalog.db.transaction()?;
    for n in 0..1024 {
        tx.execute("INSERT INTO metadata_sources(asset_id,kind,locator,display,association,availability) VALUES(?1,'sidecar',?2,'synthetic limit source','ambiguous','missing')",params![f.master.asset_id,encoded_bytes(&NativePath::from_path(&f.candidate.with_file_name(format!("sidecar-{n}.xmp"))))])?;
    }
    tx.commit()?;
    let p = f.catalog.begin_relink_review(RelinkScope::Asset {
        asset_id: f.master.asset_id.clone(),
        destinations: vec![NativePath::from_path(&f.candidate)],
    })?;
    let error = f
        .catalog
        .relink_preparation(&p.id, 1)
        .unwrap_err()
        .to_string();
    assert!(error.contains("1024 sources"), "{error}");
    assert_eq!(f.catalog.relink_plan(&p.id)?.total, 0);
    Ok(())
}
#[test]
fn plan_status_vm_work_is_independent_of_item_count() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut c = Catalog::open(temp.path())?;
    let p = c.begin_relink_review(RelinkScope::Prefix {
        from: PathReference::native(&temp.path().join("old")),
        destinations: vec![NativePath::from_path(&temp.path().join("new"))],
    })?;
    let measured = |c: &Catalog| -> Result<usize> {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = count.clone();
        c.db.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )?;
        for _ in 0..10 {
            assert!(c.relink_plan(&p.id)?.summary_complete);
        }
        c.db.progress_handler(0, None::<fn() -> bool>)?;
        Ok(count.load(Ordering::Relaxed))
    };
    let empty = measured(&c)?;
    let tx = c.db.transaction()?;
    for n in 0..1000 {
        tx.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?1,'pending')",
            params![format!("test-{n}"), format!("/old/{n}").as_bytes()],
        )?;
        tx.execute(
            "INSERT INTO storage_items VALUES(?1,?2,?3,'unverified','fixture',NULL,NULL,'{}')",
            params![p.id, n, format!("test-{n}")],
        )?;
    }
    tx.commit()?;
    assert_eq!(c.relink_plan(&p.id)?.unverified, 1000);
    let filled = measured(&c)?;
    assert!(
        filled <= empty + 100,
        "status work grew: {empty} -> {filled}"
    );
    Ok(())
}
#[test]
fn existing_worker_opens_same_catalog_without_creation_and_rejects_replacement() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("catalog");
    let c = Catalog::open(&root)?;
    let worker = c.relink_worker_handle()?.open()?;
    assert!(std::sync::Arc::ptr_eq(&c.writers, &worker.writers));
    drop(worker);
    let handle = c.relink_worker_handle()?;
    let path = root.join("catalog.sqlite3");
    #[cfg(unix)]
    {
        fs::rename(&path, root.join("saved.sqlite3"))?;
        fs::copy(root.join("saved.sqlite3"), &path)?;
        assert!(handle.open().is_err());
    }
    #[cfg(not(unix))]
    {
        drop(c);
        fs::remove_file(&path)?;
        assert!(handle.open().is_err());
        assert!(!path.exists());
    }
    Ok(())
}

#[test]
fn source_hash_cancellation_is_checked_between_bounded_reads() -> Result<()> {
    let t = tempfile::tempdir()?;
    let path = t.path().join("source.bin");
    let bytes = vec![37u8; 3 * 65536];
    fs::write(&path, &bytes)?;
    let cancel = AtomicBool::new(false);
    let mut read = 0;
    let result = read_evidence_with(&path, &cancel, |n| {
        read = n;
        cancel.store(true, Ordering::Release)
    });
    assert!(result.is_err());
    assert_eq!(read, 65536);
    assert_eq!(fs::read(path)?, bytes);
    Ok(())
}
#[test]
fn checking_confirmation_and_exclusion_counts_remain_exact() -> Result<()> {
    let mut f = Imported::new(false)?;
    let p = f.catalog.begin_relink_review(RelinkScope::Asset {
        asset_id: f.master.asset_id.clone(),
        destinations: vec![NativePath::from_path(&f.candidate)],
    })?;
    let batch = f
        .catalog
        .relink_preparation(&p.id, 1)?
        .prepare(&AtomicBool::new(false))?;
    let p = f.catalog.publish_relink_preparation(batch)?;
    assert_eq!(p.state, "checking");
    assert!(p.confirmation_token.is_none());
    assert_eq!(p.unverified, 1);
    assert!(
        f.catalog
            .finalize_relink_review_cancellable(&p.id, p.revision, &AtomicBool::new(true))
            .is_err()
    );
    assert_eq!(f.catalog.relink_plan(&p.id)?.state, "checking");
    let p =
        f.catalog
            .finalize_relink_review_cancellable(&p.id, p.revision, &AtomicBool::new(false))?;
    let p = f.confirm(&p)?;
    f.catalog.exclude_relink_item(&p.id, 1)?;
    let q = f.catalog.relink_plan(&p.id)?;
    assert_eq!(
        (
            q.total,
            q.matched,
            q.user_confirmed,
            q.unverified,
            q.excluded,
            q.unresolved,
            q.unresolved_sources
        ),
        (1, 0, 0, 0, 1, 0, 0)
    );
    assert!(q.revision > p.revision);
    assert!(q.confirmation_token.is_none());
    Ok(())
}
#[cfg(unix)]
#[test]
fn worker_rejects_sqlite_open_aba_even_when_path_checks_match() -> Result<()> {
    let t = tempfile::tempdir()?;
    let c = Catalog::open(t.path())?;
    let path = t.path().join("catalog.sqlite3");
    let saved = t.path().join("original.sqlite3");
    let replaced = t.path().join("replacement.sqlite3");
    let result = c.relink_worker_handle()?.open_with(|opened| {
        if !opened {
            fs::rename(&path, &saved)?;
            fs::copy(&saved, &path)?;
        } else {
            fs::rename(&path, &replaced)?;
            fs::rename(&saved, &path)?;
        }
        Ok(())
    });
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("SQLite selected database object changed")
    );
    Ok(())
}
#[test]
fn schema_twelve_keeps_legacy_plan_counts_explicit_and_refreshes_without_startup_scan() -> Result<()>
{
    let mut f = Imported::new(true)?;
    let p = f.prepare()?;
    f.catalog.db.execute_batch(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/relink-v12-downgrade.sql"
    )))?;
    f.catalog.db.pragma_update(None, "user_version", 11)?;
    let root = f.catalog.root.clone();
    drop(f.catalog);
    f.catalog = Catalog::open(root)?;
    let legacy = f.catalog.relink_plan(&p.id)?;
    assert!(!legacy.summary_complete);
    assert_eq!(legacy.state, "ready");
    assert_eq!(
        f.catalog
            .db
            .query_row("SELECT count(*) FROM storage_review_summary", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    let revised = f.catalog.revise_relink(&p.id, legacy.revision, vec![])?;
    let refreshed = f.catalog.prepare_relink_batch(&revised.id, 1)?;
    assert!(refreshed.summary_complete);
    assert_eq!(refreshed.matched, 1);
    assert_eq!(fs::read(&f.candidate)?, f.bytes);
    Ok(())
}

#[test]
fn unrelated_prefix_skips_all_retained_source_and_fence_queries() -> Result<()> {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let mut f = Imported::new(false)?;
    let p = f.catalog.begin_relink_review(RelinkScope::Prefix {
        from: PathReference::native(&f.old.with_file_name("unrelated-folder")),
        destinations: vec![NativePath::from_path(&f.candidate)],
    })?;
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = reads.clone();
    f.catalog.db.authorizer(Some(move |ctx: AuthContext<'_>| {
        if let AuthAction::Read { table_name, .. } = ctx.action
            && matches!(
                table_name,
                "metadata_sources"
                    | "metadata_observations"
                    | "storage_source_fences"
                    | "migration_originals"
            )
        {
            count.fetch_add(1, Ordering::Relaxed);
        }
        Authorization::Allow
    }))?;
    let batch = f.catalog.relink_preparation(&p.id, 100)?;
    f.catalog
        .db
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
    assert!(batch.finished && batch.rows.is_empty());
    assert_eq!(reads.load(Ordering::Relaxed), 0);
    Ok(())
}

#[test]
fn aggregate_source_cap_continues_at_whole_asset_boundary() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut c = Catalog::open(temp.path().join("catalog"))?;
    let old = temp.path().join("old");
    let new = temp.path().join("new");
    let tx = c.db.transaction()?;
    for asset in ["a", "b"] {
        tx.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?1,'pending')",
            params![asset, location_bytes(&old.join(format!("{asset}.png")))],
        )?;
        for n in 0..600 {
            tx.execute("INSERT INTO metadata_sources(asset_id,kind,locator,display,association,availability) VALUES(?1,'sidecar',?2,'source','ambiguous','missing')",params![asset,location_bytes(&old.join(format!("{asset}-{n}.xmp")))])?;
        }
    }
    tx.commit()?;
    for asset in ["a", "b"] {
        c.record_storage_path(
            asset,
            &NativePath::from_path(&old.join(format!("{asset}.png"))),
        )?;
    }
    let p = c.begin_relink_review(RelinkScope::Prefix {
        from: PathReference::native(&old),
        destinations: vec![NativePath::from_path(&new)],
    })?;
    let first = c.relink_preparation(&p.id, 1000)?;
    assert!(!first.finished);
    assert_eq!(first.rows.len(), 1);
    assert_eq!(first.rows[0].sources.len(), 600);
    assert_eq!(first.next, first.rows[0].sequence);
    c.publish_relink_preparation(first.prepare(&AtomicBool::new(false))?)?;
    assert_eq!(c.relink_plan(&p.id)?.total, 1);
    let next = c.relink_preparation(&p.id, 1000)?;
    assert!(next.finished);
    assert_eq!(next.rows.len(), 1);
    assert_eq!(next.rows[0].asset, "b");
    assert_eq!(next.rows[0].sources.len(), 600);
    c.publish_relink_preparation(next.prepare(&AtomicBool::new(false))?)?;
    assert_eq!(c.relink_plan(&p.id)?.total, 2);
    let sources: i64 = c.db.query_row(
        "SELECT count(*) FROM storage_source_items WHERE plan=?",
        [&p.id],
        |r| r.get(0),
    )?;
    assert_eq!(sources, 1200);
    Ok(())
}

#[test]
fn requested_large_batch_caps_assets_without_losing_scope() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut c = Catalog::open(temp.path().join("catalog"))?;
    let old = temp.path().join("old");
    for n in 0..65 {
        c.db.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?1,'pending')",
            params![
                format!("a{n}"),
                location_bytes(&old.join(format!("{n}.png")))
            ],
        )?;
    }
    for n in 0..65 {
        c.record_storage_path(
            &format!("a{n}"),
            &NativePath::from_path(&old.join(format!("{n}.png"))),
        )?;
    }
    let p = c.begin_relink_review(RelinkScope::Prefix {
        from: PathReference::native(&old),
        destinations: vec![NativePath::from_path(&temp.path().join("new"))],
    })?;
    let batch = c.relink_preparation(&p.id, 1000)?;
    assert_eq!(batch.rows.len(), 64);
    assert!(!batch.finished);
    c.publish_relink_preparation(batch.prepare(&AtomicBool::new(false))?)?;
    let batch = c.relink_preparation(&p.id, 1000)?;
    assert_eq!(batch.rows.len(), 1);
    assert!(batch.finished);
    c.publish_relink_preparation(batch.prepare(&AtomicBool::new(false))?)?;
    assert_eq!(c.relink_plan(&p.id)?.total, 65);
    Ok(())
}

#[test]
fn multibyte_source_bytes_are_rejected_before_provenance_materialization() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut c = Catalog::open(temp.path().join("catalog"))?;
    let old = temp.path().join("missing.png");
    c.db.execute("INSERT INTO assets(id,location,path_display,state) VALUES('large-source',?1,'fixture','pending')",[location_bytes(&old)])?;
    // Each TEXT is under the limit; their combined UTF-8 bytes exceed it.
    // Invalid JSON would fail parsing if preflight loaded even the first source.
    let display = "💡".repeat(9 * 1024 * 1024);
    let tx = c.db.transaction()?;
    for n in 0..2 {
        tx.execute("INSERT INTO metadata_sources(asset_id,kind,locator,display,association,availability) VALUES('large-source','sidecar',?1,?2,'ambiguous','missing')",params![format!("sidecar-{n}").as_bytes(),display])?;
        let source = tx.last_insert_rowid();
        tx.execute("INSERT INTO metadata_observations(source_id,revision,status,issues,provenance) VALUES(?1,'revision','Complete','[]','invalid JSON: must never be parsed')",[source])?;
        tx.execute(
            "UPDATE metadata_sources SET current_observation=?2 WHERE id=?1",
            params![source, tx.last_insert_rowid()],
        )?;
    }
    tx.commit()?;
    drop(display);
    let (characters, bytes): (i64,i64) = c.db.query_row("SELECT SUM(length(display)),SUM(length(CAST(display AS BLOB))) FROM metadata_sources WHERE asset_id='large-source'",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
    assert!(characters < MAX_SNAPSHOT_BYTES as i64 && bytes > MAX_SNAPSHOT_BYTES as i64);
    let p = c.begin_relink_review(RelinkScope::Asset {
        asset_id: "large-source".into(),
        destinations: vec![NativePath::from_path(&old)],
    })?;
    let error = c.relink_preparation(&p.id, 1).unwrap_err().to_string();
    assert!(error.contains("64 MiB"), "{error}");
    assert_eq!(c.relink_plan(&p.id)?.total, 0);
    Ok(())
}
