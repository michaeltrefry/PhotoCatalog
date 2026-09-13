use super::*;
use crate::{
    catalog_metadata::Source,
    organization::{Flag, Operation},
    organization_search::Query,
    xmp::{Edit, Value},
    xmp_packets::{self, Status},
};
use std::collections::BTreeMap;
fn catalog() -> Result<(tempfile::TempDir, Catalog)> {
    let t = tempfile::tempdir()?;
    let c = Catalog::open(t.path().join("catalog"))?;
    Ok((t, c))
}
fn asset(c: &mut Catalog, name: &str) -> Result<VariantKey> {
    c.db.execute(
        "INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?1,'pending')",
        params![name, name.as_bytes()],
    )?;
    crate::organization::refresh(&c.db, name)?;
    Ok(VariantKey::master(name))
}
fn input(rating: u8, status: Status) -> xmp_packets::Inspection {
    let bytes=format!(r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="urn:retained" xmp:Rating="{rating}"><u:opaque>unknown</u:opaque></rdf:Description></rdf:RDF>"#).into_bytes();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    xmp_packets::Inspection {
        revision: xmp_packets::SourceRevision {
            length: bytes.len() as u64,
            blake3: hash.clone(),
            modified_unix_ns: None,
        },
        status,
        issues: vec![],
        packets: vec![xmp_packets::Packet {
            container: xmp_packets::Container::Sidecar,
            bytes: bytes.clone(),
            blake3: hash.clone(),
            ranges: vec![],
            group: "fixture".into(),
            attributes: BTreeMap::new(),
        }],
        parse_inputs: vec![xmp_packets::ParseInput {
            bytes,
            blake3: hash,
            packet_indices: vec![0],
            transformation: xmp_packets::Transformation::Identity,
            group: "fixture".into(),
        }],
    }
}
fn source(kind: &str) -> Source {
    Source {
        kind: kind.into(),
        locator: b"fixture-source".to_vec(),
        display: "fixture".into(),
        ambiguous: false,
        provenance: serde_json::json!({"fixture":true}),
    }
}
fn rating(c: &Catalog, k: &VariantKey) -> Result<Option<Value>> {
    Ok(c.metadata_for_image(k)?
        .fields
        .into_iter()
        .find(|f| f.name == "rating")
        .and_then(|f| f.value))
}
fn request(
    asset: &str,
    id: &str,
    role: ImageRole,
    parent: Option<VariantKey>,
) -> ImportImageRequest {
    ImportImageRequest {
        import_source: "job".into(),
        capture_revision: "revision".into(),
        source_table: "Adobe_images".into(),
        source_id: id.into(),
        input_digest: format!("digest:{id}"),
        adapter_version: "adapter1".into(),
        asset_id: asset.into(),
        claim_reserved_master: false,
        role,
        master: parent,
        label: id.into(),
    }
}
#[test]
fn actual_master_claim_is_atomic_idempotent_and_preserves_virtual_roles() -> Result<()> {
    let (_t, mut c) = catalog()?;
    let mut r = request("file", "master-record", ImageRole::Master, None);
    r.claim_reserved_master = true;
    {
        let tx = c.db.transaction()?;
        reserve_import_asset(&tx, "file", "job")?;
        tx.execute("INSERT INTO assets(id,location,path_display,state) VALUES('file',X'FF','missing','pending')",[])?;
        let image = register_import_image(&tx, &r)?;
        assert_eq!(image.key, VariantKey::master("file"));
        assert_eq!(image.id, "file");
        tx.commit()?;
    }
    let m = c.register_import_image(&r)?;
    assert_eq!(c.browse_images(0, 100)?.len(), 1);
    let v = c.register_import_image(&request(
        "file",
        "copy-record",
        ImageRole::Virtual,
        Some(m.key.clone()),
    ))?;
    assert_eq!(v.master_sequence, Some(m.sequence));
    assert_eq!(c.browse_images(0, 100)?.len(), 2);
    assert_eq!(
        c.db.query_row("SELECT count(*) FROM assets", [], |q| q.get::<_, i64>(0))?,
        1
    );
    let v2 = c.register_import_image(&request(
        "file",
        "copy-record",
        ImageRole::Virtual,
        Some(m.key.clone()),
    ))?;
    assert_eq!(v.id, v2.id);
    let mut changed = r.clone();
    changed.input_digest = "different".into();
    assert!(c.register_import_image(&changed).is_err());
    assert!(
        c.register_import_image(&request(
            "file",
            "bad-parent",
            ImageRole::Virtual,
            Some(v.key.clone())
        ))
        .is_err()
    );
    assert!(
        c.db.execute(
            "UPDATE catalog_images SET master_sequence=sequence WHERE id=?",
            [&v.id]
        )
        .is_err()
    );
    let native = asset(&mut c, "native")?;
    let tx = c.db.transaction()?;
    assert!(reserve_import_asset(&tx, &native.asset_id, "job").is_err());
    tx.rollback()?;
    let mut steal = request("native", "steal", ImageRole::Master, None);
    steal.claim_reserved_master = true;
    assert!(c.register_import_image(&steal).is_err());
    // Transaction rollback leaves no asset, image, mapping or reservation.
    {
        let tx = c.db.transaction()?;
        reserve_import_asset(&tx, "rollback", "job")?;
        tx.execute("INSERT INTO assets(id,location,path_display,state) VALUES('rollback',X'AB','missing','pending')",[])?;
        let mut rr = request("rollback", "r", ImageRole::Master, None);
        rr.claim_reserved_master = true;
        register_import_image(&tx, &rr)?;
        tx.rollback()?;
    }
    assert_eq!(
        c.db.query_row("SELECT count(*) FROM assets WHERE id='rollback'", [], |q| q
            .get::<_, i64>(0))?,
        0
    );
    Ok(())
}
#[test]
fn sibling_metadata_history_and_explicit_incomplete_choices_are_independent() -> Result<()> {
    let (_t, mut c) = catalog()?;
    let m = asset(&mut c, "one")?;
    let first = c.retain_metadata_for_image(&m, &source("catalog"), &input(2, Status::Complete))?;
    let v = c.create_edit_variant(&m, 0, "copy")?.key;
    let old = c.metadata_model_for_image(&v, first.model_ids[0])?;
    let second =
        c.retain_metadata_for_image(&m, &source("catalog"), &input(4, Status::Complete))?;
    assert_eq!(rating(&c, &v)?, Some(Value::Text("2".into())));
    assert!(c.metadata_model_for_image(&v, second.model_ids[0]).is_err());
    assert_eq!(c.metadata_model_for_image(&v, first.model_ids[0])?, old);
    let hist = c.metadata_history_for_image(&m, 0, 100)?;
    assert_eq!(hist.len(), 2);
    assert!(!hist[0].current);
    let rev = c.metadata_for_image(&m)?.revision;
    assert!(
        c.resolve_metadata_for_image(&m, rev, "rating", first.model_ids[0])
            .is_err()
    );
    let incomplete =
        c.retain_metadata_for_image(&m, &source("catalog"), &input(5, Status::Malformed))?;
    let view = c.metadata_for_image(&m)?;
    assert!(
        view.fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .conflicted
    );
    c.resolve_metadata_for_image(&m, incomplete.revision, "rating", incomplete.model_ids[0])?;
    assert_eq!(rating(&c, &m)?, Some(Value::Text("5".into())));
    let changed =
        c.retain_metadata_for_image(&m, &source("catalog"), &input(1, Status::SourceChanged))?;
    let hist = c.metadata_history_for_image(&m, 0, 100)?;
    assert_eq!(hist.len(), 4);
    assert!(!hist.last().unwrap().current);
    assert!(
        hist.iter()
            .find(|h| h.id == incomplete.observation_id)
            .unwrap()
            .current
    );
    assert!(
        !c.metadata_packets_for_image(&m, changed.observation_id)?
            .is_empty()
    );
    assert!(
        c.retain_import_metadata_for_image(&v, &source("catalog"), &input(3, Status::Unsupported))
            .is_err()
    );
    assert_eq!(rating(&c, &v)?, Some(Value::Text("2".into())));
    Ok(())
}
#[test]
fn copy_local_organization_and_edit_identity_do_not_touch_siblings_or_physical_generation()
-> Result<()> {
    let (_t, mut c) = catalog()?;
    let m = asset(&mut c, "one")?;
    c.retain_metadata(&m.asset_id, &source("sidecar"), &input(2, Status::Complete))?;
    let v = c.create_edit_variant(&m, 0, "copy")?.key;
    let m_before = c.image_metadata_identity(&m)?;
    let v_before = c.image_metadata_identity(&v)?;
    c.organize_image(
        &v,
        v_before.metadata_revision,
        Operation::Rating { value: 4 },
    )?;
    assert_eq!(rating(&c, &v)?, Some(Value::Text("4".into())));
    assert_eq!(rating(&c, &m)?, Some(Value::Text("2".into())));
    assert_eq!(c.image_metadata_identity(&m)?, m_before);
    assert_eq!(
        c.image_metadata_identity(&v)?.physical_generation,
        v_before.physical_generation
    );
    assert!(
        c.with_image_metadata_identity(&v_before, |_| Ok(()))
            .is_err()
    );
    let revision = c.metadata_for_image(&v)?.revision;
    c.organize_image(&v, revision, Operation::Flag { value: Flag::Pick })?;
    let all = c.search(
        &Query {
            include_variants: true,
            ..Query::default()
        },
        None,
        10,
        10,
    )?;
    assert_eq!(all.rows.len(), 2);
    assert_eq!(
        all.rows
            .iter()
            .find(|x| x.variant_id == v.variant_id)
            .unwrap()
            .flag,
        "pick"
    );
    assert_eq!(
        all.rows
            .iter()
            .find(|x| x.variant_id == MASTER)
            .unwrap()
            .flag,
        "unflagged"
    );
    assert_eq!(c.search(&Query::default(), None, 10, 10)?.rows.len(), 1);
    let rev = c.metadata_for_image(&v)?.revision;
    c.edit_metadata_for_image(
        &v,
        rev,
        None,
        &[Edit::Set {
            namespace: crate::xmp::XMP.into(),
            path: "Rating".into(),
            value: "3".into(),
        }],
    )?;
    assert_eq!(c.image_metadata_identity(&m)?, m_before);
    Ok(())
}
#[test]
fn bounded_shared_refresh_keeps_real_foreground_pages_and_rejects_stale_authority() -> Result<()> {
    let (_t, mut c) = catalog()?;
    let m = asset(&mut c, "one")?;
    c.retain_metadata(&m.asset_id, &source("sidecar"), &input(2, Status::Complete))?;
    let mut last = m.clone();
    for i in 0..220 {
        last = c.create_edit_variant(&m, 0, &format!("copy{i}"))?.key;
    }
    let before = c.image_metadata_identity(&last)?;
    c.retain_metadata(&m.asset_id, &source("sidecar"), &input(3, Status::Complete))?;
    assert!(c.image_metadata_identity(&last).is_err());
    assert!(
        c.save_edit_recipe(&last, 0, &crate::edit::Recipe::default())
            .is_err()
    );
    assert!(c.with_image_metadata_identity(&before, |_| Ok(())).is_err());
    let q = Query {
        include_variants: true,
        ..Query::default()
    };
    let page = c.search(&q, None, 200, 200)?;
    assert_eq!(page.rows.len(), 200);
    assert!(page.rows.iter().any(|x| x.metadata_pending));
    assert!(page.rows.iter().any(|x| !x.metadata_pending));
    let mut session = c.search_session(q.clone(), 30)?;
    let first = session.next_page(100, 100)?;
    assert_eq!(first.rows.len(), 100);
    c.retain_metadata(&m.asset_id, &source("sidecar"), &input(4, Status::Complete))?;
    let mut steps = 0;
    loop {
        let progress = c.step_image_metadata_refresh(17)?;
        assert!(progress.processed <= 17);
        steps += 1;
        if !progress.pending {
            break;
        }
        assert!(steps < 40);
    }
    assert_eq!(rating(&c, &last)?, Some(Value::Text("4".into())));
    assert_eq!(c.metadata_history_for_image(&last, 0, 100)?.len(), 3);
    let rest = session.next_page(200, 200)?;
    assert_eq!(rest.rows.len(), 121);
    session.close()?;
    let page = c.search(&q, None, 200, 200)?;
    assert_eq!(page.rows.len(), 200);
    assert!(page.rows.iter().all(|x| !x.metadata_pending));
    // A physical state transition queues one cursor, rather than inserting 221 dirty records in a trigger.
    c.db.execute(
        "UPDATE assets SET path_display='renamed' WHERE id='one'",
        [],
    )?;
    assert_eq!(
        c.db.query_row("SELECT count(*) FROM image_storage_events", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        c.db.query_row("SELECT count(*) FROM organization_dirty", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    let p = c.search(&q, None, 200, 200)?;
    assert_eq!(p.rows.len(), 200);
    assert!(p.rows.iter().all(|x| x.metadata_pending));
    loop {
        let p = c.organization_index(17)?;
        assert!(p.processed <= 17);
        if !p.pending {
            break;
        }
    }
    assert!(
        c.search(&q, None, 200, 200)?
            .rows
            .iter()
            .all(|x| !x.metadata_pending)
    );
    Ok(())
}
#[test]
fn source_membership_constraints_reject_foreign_history_and_current_pointers() -> Result<()> {
    let (_t, mut c) = catalog()?;
    let a = asset(&mut c, "a")?;
    let b = asset(&mut c, "b")?;
    let obs = c.retain_metadata_for_image(&a, &source("catalog"), &input(2, Status::Complete))?;
    assert!(
        c.db.execute(
            "INSERT INTO metadata_image_observations VALUES(?1,?2)",
            params![b.asset_id, obs.observation_id]
        )
        .is_err()
    );
    let sid: i64 = c.db.query_row(
        "SELECT source_id FROM metadata_observations WHERE id=?",
        [obs.observation_id],
        |r| r.get(0),
    )?;
    assert!(
        c.db.execute(
            "INSERT INTO metadata_image_sources VALUES(?1,?2,?3,0,X'','confirmed','available')",
            params![b.asset_id, sid, obs.observation_id]
        )
        .is_err()
    );
    assert!(
        c.metadata_packets_for_image(&b, obs.observation_id)
            .is_err()
    );
    Ok(())
}
#[test]
fn schema_six_upgrade_preserves_master_job_cursors_recipe_ids_and_legacy_metadata_revision()
-> Result<()> {
    let t = tempfile::tempdir()?;
    let root = t.path().join("catalog");
    std::fs::create_dir(&root)?;
    let db = Connection::open(root.join("catalog.sqlite3"))?;
    db.execute_batch("CREATE TABLE assets(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,location BLOB NOT NULL UNIQUE,path_display TEXT NOT NULL,fingerprint TEXT,state TEXT NOT NULL CHECK(state IN('pending','ready','failed')),metadata TEXT,preview_hash TEXT,error TEXT,CHECK(state!='ready' OR(metadata IS NOT NULL AND preview_hash IS NOT NULL AND fingerprint IS NOT NULL))); PRAGMA application_id=1346913089;")?;
    db.execute_batch(crate::catalog_metadata::SCHEMA)?;
    db.execute_batch(crate::catalog_storage::SCHEMA)?;
    db.execute_batch(crate::catalog_metadata::FILE_INSTANCE_SCHEMA)?;
    db.execute_batch(crate::organization::SCHEMA)?;
    db.execute_batch(crate::organization::CAPTURE_LENS_SCHEMA)?;
    db.execute_batch(crate::catalog_edits::SCHEMA)?;
    db.execute_batch(crate::catalog_exports::SCHEMA)?;
    db.execute_batch(crate::catalog_export_alias::SCHEMA)?;
    db.execute("INSERT INTO assets(sequence,id,location,path_display,state) VALUES(71,'a',X'01','a','pending')",[])?;
    db.execute("UPDATE assets SET render_generation=23 WHERE id='a'", [])?;
    db.execute("INSERT INTO metadata_assets VALUES('a',9)", [])?;
    db.execute("INSERT INTO metadata_history(id,asset_id,revision,action,detail) VALUES(100,'a',9,'old','{}')",[])?;
    db.execute("INSERT INTO organization_jobs(id,operation,state,total) VALUES('old','{\"operation\":\"flag\",\"value\":\"pick\"}','ready',1)",[])?;
    db.execute(
        "INSERT INTO organization_job_items(job,sequence,expected_revision) VALUES('old',71,9)",
        [],
    )?;
    let recipe = crate::edit::Recipe::default();
    let validated = recipe.validate()?;
    db.execute("INSERT INTO edit_variants(sequence,asset_id,id,label,revision) VALUES(17,'a','copy','copy',3)",[])?;
    db.execute("INSERT INTO edit_recipe_nodes(id,asset_id,variant_id,recipe,digest) VALUES(31,'a','copy',?1,?2)",params![validated.canonical_bytes(),validated.digest()])?;
    db.execute("UPDATE edit_variants SET cursor=31 WHERE id='copy'", [])?;
    db.pragma_update(None, "user_version", 6)?;
    drop(db);
    let mut c = Catalog::open(&root)?;
    assert_eq!(c.image(&VariantKey::master("a"))?.sequence, 71);
    assert_eq!(
        c.image_metadata_identity(&VariantKey::master("a"))?
            .physical_generation,
        23
    );
    assert!(
        c.image(&VariantKey {
            asset_id: "a".into(),
            variant_id: "copy".into()
        })?
        .sequence
            > 71
    );
    assert_eq!(c.organization_job_items("old", 70, 10)?[0].sequence, 71);
    assert_eq!(c.metadata("a")?.revision, 9);
    assert_eq!(
        c.db.query_row(
            "SELECT sequence,cursor,revision FROM edit_variants WHERE id='copy'",
            [],
            |r| Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?
            ))
        )?,
        (17, 31, 3)
    );
    while c.organization_index(10)?.pending {}
    assert_eq!(c.step_organization_batch("old")?.state, "complete");
    assert_eq!(
        c.organization_job_items("old", 70, 10)?[0].result_revision,
        Some(10)
    );
    assert_eq!(
        c.db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))?,
        0
    );
    drop(c);
    let c = Catalog::open(&root)?;
    assert_eq!(c.metadata("a")?.revision, 10);
    assert_eq!(
        c.db.query_row(
            "SELECT id FROM metadata_history WHERE action='old'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        100
    );
    Ok(())
}

#[test]
fn collection_hierarchy_order_synonyms_and_relationship_replay_preserve_logical_endpoints()
-> Result<()> {
    let (_t, mut c) = catalog()?;
    let m = asset(&mut c, "a")?;
    let v = c.create_edit_variant(&m, 0, "copy")?.key;
    let p = c.create_collection("parent", serde_json::json!({}))?;
    let child = c.create_collection("child", serde_json::json!({}))?;
    c.place_collection(&organization::CollectionPlacement {
        collection: child.clone(),
        parent: Some(p.clone()),
        position: 2,
    })?;
    assert!(
        c.place_collection(&organization::CollectionPlacement {
            collection: p,
            parent: Some(child.clone()),
            position: 0
        })
        .is_err()
    );
    let vi = c.image_metadata_identity(&v)?;
    c.set_image_collection_membership(&vi, &child, 9, &serde_json::json!({"source":"membership"}))?;
    let mi = c.image_metadata_identity(&m)?;
    c.set_image_collection_membership(&mi, &child, 3, &serde_json::json!({}))?;
    let first = c.image_collection_members(&child, None, 1)?;
    assert_eq!(first[0].1.key, m);
    let second = c.image_collection_members(&child, Some((first[0].1.position, first[0].0)), 1)?;
    assert_eq!(second[0].1.key, v);
    assert!(
        c.image_collection_members(&child, Some((second[0].1.position, second[0].0)), 1)?
            .is_empty()
    );
    let keyword = c.create_keyword(crate::organization::KeywordKind::Flat, &["birds".into()])?;
    c.add_keyword_synonym(keyword, "aves", &serde_json::json!({"source":"keyword"}))?;
    assert_eq!(c.keyword_synonyms(keyword, "", 1)?[0].0, "aves");
    let seq = c.retain_image_relation(
        "source:stack",
        "stack",
        &m,
        &v,
        0,
        &serde_json::json!({"raw":"retained"}),
    )?;
    assert_eq!(
        c.retain_image_relation(
            "source:stack",
            "stack",
            &m,
            &v,
            0,
            &serde_json::json!({"raw":"retained"})
        )?,
        seq
    );
    assert!(
        c.retain_image_relation(
            "source:stack",
            "stack",
            &v,
            &m,
            0,
            &serde_json::json!({"raw":"retained"})
        )
        .is_err()
    );
    assert_eq!(c.image_relations(&m, 0, 1)?[0].to, v);
    Ok(())
}

#[test]
fn scoped_render_authority_preserves_legacy_bytes_and_sibling_isolation() -> Result<()> {
    let (_t, mut c) = catalog()?;
    let m = asset(&mut c, "a")?;
    let v = c.create_edit_variant(&m, 0, "copy")?.key;
    let master = c.edit_render_identity(&m)?;
    let copy = c.edit_render_identity(&v)?;
    let mut legacy = master.clone();
    legacy.image_identity = None;
    legacy.source = c.render_identity("a")?;
    let bytes = serde_json::to_vec(&legacy)?;
    assert!(!String::from_utf8_lossy(&bytes).contains("image_identity"));
    let restored: crate::catalog_edits::EditRenderIdentity = serde_json::from_slice(&bytes)?;
    assert_eq!(serde_json::to_vec(&restored)?, bytes);
    assert_eq!(c.with_edit_identity(&restored, || Ok(7))?, Some(7));
    c.organize_image(
        &v,
        copy.source.metadata_revision,
        Operation::Rating { value: 4 },
    )?;
    assert_eq!(
        c.with_edit_identity(&copy, || panic!("stale callback"))?,
        None::<()>
    );
    assert_eq!(c.with_edit_identity(&master, || Ok(9))?, Some(9));
    c.db.execute("UPDATE assets SET fingerprint='new' WHERE id='a'", [])?;
    assert_eq!(
        c.with_edit_identity(&master, || panic!("stale physical callback"))?,
        None::<()>
    );
    Ok(())
}

#[test]
fn import_recipe_status_and_history_commit_with_checkpoint_or_roll_back() -> Result<()> {
    let (_t, mut c) = catalog()?;
    let m = asset(&mut c, "a")?;
    let i = c.register_import_image(&request("a", "import", ImageRole::Master, None))?;
    let recipe = crate::edit::Recipe::default().validate()?;
    {
        let tx = c.db.transaction()?;
        let v = crate::catalog_edits::install_import_recipe(
            &tx,
            &i.key,
            0,
            &recipe,
            &serde_json::json!({"retained_source":"digest"}),
            TranslationState::Translated,
        )?;
        assert_eq!(v.revision, 1);
        assert!(
            crate::catalog_edits::install_import_recipe(
                &tx,
                &i.key,
                0,
                &recipe,
                &serde_json::json!({}),
                TranslationState::Translated
            )
            .is_err()
        );
        // Simulate a failed importer checkpoint: recipe/status/history all roll back.
    }
    assert_eq!(c.edit_variant(&i.key)?.revision, 0);
    assert_eq!(c.image(&i.key)?.translation_state, "untranslated");
    {
        let tx = c.db.transaction()?;
        assert!(
            crate::catalog_edits::install_import_recipe(
                &tx,
                &m,
                0,
                &recipe,
                &serde_json::json!({}),
                TranslationState::Translated
            )
            .is_err()
        );
        crate::catalog_edits::install_import_recipe(
            &tx,
            &i.key,
            0,
            &recipe,
            &serde_json::json!({"retained_source":"digest"}),
            TranslationState::Translated,
        )?;
        tx.commit()?;
    }
    assert_eq!(c.image(&i.key)?.translation_state, "translated");
    assert_eq!(
        c.edit_history(&i.key, -1, 10)?
            .iter()
            .filter(|e| e.kind == "import")
            .count(),
        1
    );
    Ok(())
}
