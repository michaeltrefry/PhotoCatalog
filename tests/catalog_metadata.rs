use anyhow::{Result, ensure};
use photocatalog::{
    Catalog,
    catalog_metadata::Source,
    xmp::{self, Edit, Value},
    xmp_packets::{self, Limits},
};
use std::{fs, path::Path};
fn packet(rating: i32) -> String {
    format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="https://example.invalid/unknown/" xmp:Rating="{rating}"><u:complex rdf:parseType="Resource"><u:preserve>雪 &amp; sunshine</u:preserve></u:complex></rdf:Description></rdf:RDF></x:xmpmeta>"#
    )
}
fn jpeg(path: &Path) {
    image::RgbImage::from_pixel(8, 8, image::Rgb([20, 40, 60]))
        .save(path)
        .unwrap();
}
#[test]
fn automatic_sidecars_history_conflicts_edits_and_source_invariance() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    let path = originals.join("photo.jpg");
    jpeg(&path);
    let before = fs::read(&path)?;
    let sidecar = originals.join("photo.xmp");
    fs::write(&sidecar, packet(2))?;
    let mut cat = Catalog::open(temp.path().join("catalog"))?;
    let first = cat.import(&originals, None, |_| Ok(()))?;
    ensure!(first.imported == 1);
    let asset = cat.browse(0, 10)?.remove(0);
    let view = cat.metadata(&asset.id)?;
    let rating = view.fields.iter().find(|f| f.name == "rating").unwrap();
    ensure!(rating.value == Some(Value::Text("2".into())) && !rating.conflicted);
    let model = rating.selected_model.unwrap();
    let generation = cat.render_identity(&asset.id)?.generation;
    let change = cat.edit_metadata(
        &asset.id,
        view.revision,
        Some(model),
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Rating".into(),
            value: "4".into(),
        }],
    )?;
    ensure!(cat.render_identity(&asset.id)?.generation == generation + 1);
    ensure!(
        cat.metadata(&asset.id)?
            .fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .value
            == Some(Value::Text("4".into()))
    );
    ensure!(fs::read(&sidecar)? == packet(2).as_bytes());
    fs::write(&sidecar, packet(3))?;
    let scan = cat.import(&originals, None, |_| Ok(()))?;
    ensure!(scan.unchanged == 1 && scan.metadata_updated == 1);
    let view = cat.metadata(&asset.id)?;
    let rating = view.fields.iter().find(|f| f.name == "rating").unwrap();
    // A catalog edit remains explicitly chosen; the competing sidecar value is still visible.
    ensure!(rating.candidates.len() == 2 && rating.value == Some(Value::Text("4".into())));
    ensure!(
        cat.resolve_metadata(&asset.id, change.revision, "rating", model)
            .is_err()
    );
    let sid = rating
        .candidates
        .iter()
        .find(|c| c.source_kind == "sidecar")
        .unwrap()
        .model_id;
    cat.resolve_metadata(&asset.id, view.revision, "rating", sid)?;
    fs::write(&sidecar, packet(5))?;
    cat.import(&originals, None, |_| Ok(()))?;
    ensure!(
        cat.metadata(&asset.id)?
            .fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .conflicted
    );
    let history = cat.metadata_history(&asset.id, 0, 100)?;
    let old = history
        .iter()
        .find(|o| o.models.iter().any(|m| m.id == model))
        .unwrap();
    ensure!(cat.metadata_packets(&asset.id, old.id)?[0].bytes == packet(2).as_bytes());
    ensure!(fs::read(&path)? == before);
    let stable = cat.metadata(&asset.id)?.revision;
    cat.import(&originals, None, |_| Ok(()))?;
    ensure!(cat.metadata(&asset.id)?.revision == stable);
    drop(cat);
    let cat = Catalog::open(temp.path().join("catalog"))?;
    ensure!(cat.metadata(&asset.id)?.revision == stable);
    Ok(())
}
#[test]
fn same_stem_association_is_ambiguous_and_decode_failure_still_retains_xmp() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    jpeg(&originals.join("photo.jpg"));
    fs::write(originals.join("photo.CR2"), b"damaged raw")?;
    fs::write(originals.join("photo.XMP"), packet(2))?;
    let mut cat = Catalog::open(temp.path().join("catalog"))?;
    let report = cat.import(&originals, None, |_| Ok(()))?;
    ensure!(report.imported == 1 && report.failed == 1);
    for asset in cat.browse(0, 10)? {
        let view = cat.metadata(&asset.id)?;
        let field = view.fields.iter().find(|f| f.name == "rating").unwrap();
        ensure!(field.conflicted && field.value.is_none() && field.candidates[0].ambiguous);
        ensure!(
            cat.metadata_model(&asset.id, field.candidates[0].model_id)? == packet(2).as_bytes()
        );
    }
    Ok(())
}
#[test]
fn explicit_removal_and_subsequent_edits_do_not_resurrect_source_values() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    jpeg(&originals.join("photo.jpg"));
    fs::write(originals.join("photo.xmp"), packet(2))?;
    let mut cat = Catalog::open(temp.path().join("catalog"))?;
    cat.import(&originals, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let view = cat.metadata(&asset.id)?;
    let mid = view.fields[0].selected_model.unwrap();
    let removed = cat.edit_metadata(
        &asset.id,
        view.revision,
        Some(mid),
        &[Edit::Remove {
            namespace: xmp::XMP.into(),
            path: "Rating".into(),
        }],
    )?;
    ensure!(cat.metadata(&asset.id)?.fields[0].value == Some(Value::Removed));
    let added = cat.edit_metadata(
        &asset.id,
        removed.revision,
        Some(removed.model_ids[0]),
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Label".into(),
            value: "Red".into(),
        }],
    )?;
    let view = cat.metadata(&asset.id)?;
    ensure!(
        view.fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .value
            == Some(Value::Removed)
    );
    ensure!(
        !xmp::project(&cat.metadata_model(&asset.id, added.model_ids[0])?)?
            .fields
            .contains_key("rating")
    );
    Ok(())
}
#[test]
fn malformed_sidecar_is_retained_and_independent_source_conflict_is_visible() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    jpeg(&originals.join("photo.jpg"));
    fs::write(originals.join("photo.xmp"), b"<broken rdf")?;
    let mut cat = Catalog::open(temp.path().join("catalog"))?;
    cat.import(&originals, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let history = cat.metadata_history(&asset.id, 0, 100)?;
    let malformed = history
        .iter()
        .find(|o| o.models.iter().any(|m| m.error.is_some()))
        .unwrap();
    ensure!(cat.metadata_packets(&asset.id, malformed.id)?[0].bytes == b"<broken rdf");
    for (key, rating) in [("source-a", 2), ("source-b", 4)] {
        let p = temp.path().join(format!("{key}.xmp"));
        fs::write(&p, packet(rating))?;
        cat.retain_metadata(
            &asset.id,
            &Source {
                kind: "imported_catalog".into(),
                locator: key.as_bytes().to_vec(),
                display: key.into(),
                ambiguous: false,
                provenance: serde_json::json!({"external_catalog":key}),
            },
            &xmp_packets::inspect_sidecar(&p, &Limits::default())?,
        )?;
    }
    let view = cat.metadata(&asset.id)?;
    let field = view.fields.iter().find(|f| f.name == "rating").unwrap();
    ensure!(field.conflicted && field.candidates.len() == 2);
    Ok(())
}
#[test]
fn version_one_migration_preserves_asset_bytes_and_foreign_database_is_untouched() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("old");
    fs::create_dir(&root)?;
    let db = rusqlite::Connection::open(root.join("catalog.sqlite3"))?;
    db.execute_batch("CREATE TABLE assets(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,location BLOB NOT NULL UNIQUE,path_display TEXT NOT NULL,fingerprint TEXT,state TEXT NOT NULL,metadata TEXT,preview_hash TEXT,error TEXT); INSERT INTO assets(id,location,path_display,fingerprint,state,metadata,preview_hash,error) VALUES('stable',x'00FF','opaque','digest','pending','original-json','cached-hash','original-error'); PRAGMA application_id=1346913089; PRAGMA user_version=1;")?;
    drop(db);
    let _cat = Catalog::open(&root)?;
    let db = rusqlite::Connection::open(root.join("catalog.sqlite3"))?;
    let row: (Vec<u8>, String, String, String, String, i64) = db.query_row(
        "SELECT location,metadata,preview_hash,error,id,render_generation FROM assets",
        [],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    ensure!(
        row == (
            vec![0, 255],
            "original-json".into(),
            "cached-hash".into(),
            "original-error".into(),
            "stable".into(),
            0
        )
    );
    let foreign = temp.path().join("foreign");
    fs::create_dir(&foreign)?;
    let f = rusqlite::Connection::open(foreign.join("catalog.sqlite3"))?;
    f.execute_batch("CREATE TABLE unrelated(secret TEXT); INSERT INTO unrelated VALUES('keep');")?;
    drop(f);
    let before = fs::read(foreign.join("catalog.sqlite3"))?;
    ensure!(Catalog::open(&foreign).is_err());
    ensure!(fs::read(foreign.join("catalog.sqlite3"))? == before);
    Ok(())
}

#[test]
fn export_uses_resolved_fields_preserves_unknown_model_and_rejects_stale_plans() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    jpeg(&originals.join("photo.jpg"));
    let sidecar = originals.join("photo.xmp");
    fs::write(&sidecar, packet(2))?;
    let mut cat = Catalog::open(temp.path().join("catalog"))?;
    cat.import(&originals, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let view = cat.metadata(&asset.id)?;
    let base = view.fields[0].selected_model.unwrap();
    let edit = cat.edit_metadata(
        &asset.id,
        view.revision,
        Some(base),
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Rating".into(),
            value: "4".into(),
        }],
    )?;
    let plan = cat.plan_metadata_export(&asset.id, edit.revision, base, &sidecar)?;
    ensure!(fs::read(&sidecar)? == packet(2).as_bytes());
    let receipt = cat.apply_metadata_export(&plan.destination.operation)?;
    ensure!(receipt.state == photocatalog::metadata_export::ExportState::Published);
    let exported = fs::read(&sidecar)?;
    ensure!(xmp::project(&exported)?.fields["rating"] == Value::Text("4".into()));
    let restored = xmp::apply_edits(
        &exported,
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Rating".into(),
            value: "2".into(),
        }],
    )?;
    ensure!(
        xmp::canonical(&xmp::parse(&restored)?)?
            == xmp::canonical(&xmp::parse(packet(2).as_bytes())?)?
    );
    ensure!(fs::read(receipt.captured_original.unwrap())? == packet(2).as_bytes());
    let destination = temp.path().join("changed.xmp");
    fs::write(&destination, packet(1))?;
    let plan = cat.plan_metadata_export(&asset.id, edit.revision, base, &destination)?;
    fs::write(&destination, packet(5))?;
    ensure!(
        cat.apply_metadata_export(&plan.destination.operation)?
            .state
            == photocatalog::metadata_export::ExportState::Conflict
    );
    ensure!(fs::read(&destination)? == packet(5).as_bytes());
    let destination = temp.path().join("stale.xmp");
    let plan = cat.plan_metadata_export(&asset.id, edit.revision, base, &destination)?;
    cat.edit_metadata(
        &asset.id,
        edit.revision,
        Some(edit.model_ids[0]),
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Label".into(),
            value: "Red".into(),
        }],
    )?;
    ensure!(
        cat.apply_metadata_export(&plan.destination.operation)
            .is_err()
            && !destination.exists()
    );
    Ok(())
}

#[test]
fn jpeg_main_and_extended_are_joined_by_guid_and_original_fragments_remain_intact() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    let path = originals.join("photo.jpg");
    jpeg(&path);
    let jpeg = fs::read(&path)?;
    let extension = packet(4);
    let guid = format!("{:X}", md5::compute(extension.as_bytes()));
    let main = format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:n="http://ns.adobe.com/xmp/note/" xmlns:xmp="http://ns.adobe.com/xap/1.0/" n:HasExtendedXMP="{guid}" xmp:Label="Green"/></rdf:RDF></x:xmpmeta>"#
    );
    let mut bytes = vec![255, 216];
    let mut main_carrier = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    main_carrier.extend(main.as_bytes());
    let mut extended_carrier = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
    extended_carrier.extend(guid.as_bytes());
    extended_carrier.extend((extension.len() as u32).to_be_bytes());
    extended_carrier.extend(0u32.to_be_bytes());
    extended_carrier.extend(extension.as_bytes());
    for carrier in [&extended_carrier, &main_carrier] {
        bytes.extend([255, 225]);
        bytes.extend(((carrier.len() + 2) as u16).to_be_bytes());
        bytes.extend(carrier);
    }
    bytes.extend(&jpeg[2..]);
    fs::write(&path, &bytes)?;
    let mut cat = Catalog::open(temp.path().join("catalog"))?;
    cat.import(&originals, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let view = cat.metadata(&asset.id)?;
    ensure!(view.fields.len() == 2 && view.fields.iter().all(|f| !f.conflicted));
    let history = cat.metadata_history(&asset.id, 0, 100)?;
    let observation = history.iter().find(|o| o.models.len() == 3).unwrap();
    let merged = observation
        .models
        .iter()
        .find(|m| m.error.is_none())
        .unwrap();
    let model = cat.metadata_model(&asset.id, merged.id)?;
    ensure!(xmp::project(&model)?.fields["rating"] == Value::Text("4".into()));
    let retained = cat.metadata_packets(&asset.id, observation.id)?;
    ensure!(retained[0].bytes == extended_carrier && retained[1].bytes == main_carrier);
    let target = temp.path().join("export.xmp");
    cat.plan_metadata_export(&asset.id, view.revision, merged.id, &target)?;
    ensure!(fs::read(&path)? == bytes);
    Ok(())
}

#[test]
fn equal_values_with_distinct_qualifiers_require_explicit_source_choice() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    jpeg(&originals.join("photo.jpg"));
    let mut cat = Catalog::open(temp.path().join("catalog"))?;
    cat.import(&originals, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let mut ids = Vec::new();
    for note in ["embedded", "sidecar"] {
        let payload = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="https://example.invalid/unknown/"><xmp:Rating rdf:parseType="Resource"><rdf:value>3</rdf:value><u:note>{note}</u:note></xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>"#
        );
        let path = temp.path().join(format!("{note}.xmp"));
        fs::write(&path, payload)?;
        let change = cat.retain_metadata(
            &asset.id,
            &Source {
                kind: note.into(),
                locator: note.as_bytes().to_vec(),
                display: note.into(),
                ambiguous: false,
                provenance: serde_json::Value::Null,
            },
            &xmp_packets::inspect_sidecar(&path, &Limits::default())?,
        )?;
        ids.push(change.model_ids[0]);
    }
    let view = cat.metadata(&asset.id)?;
    let field = &view.fields[0];
    ensure!(
        field.conflicted
            && field.candidates[0].value == field.candidates[1].value
            && field.candidates[0].semantic_hash != field.candidates[1].semantic_hash
    );
    let target = temp.path().join("export.xmp");
    ensure!(
        cat.plan_metadata_export(&asset.id, view.revision, ids[1], &target)
            .is_err()
    );
    let revision = cat.resolve_metadata(&asset.id, view.revision, "rating", ids[1])?;
    let plan = cat.plan_metadata_export(&asset.id, revision, ids[1], &target)?;
    cat.apply_metadata_export(&plan.destination.operation)?;
    ensure!(
        xmp::parse(&fs::read(target)?)?
            .qualifier(
                xmp::XMP,
                "Rating",
                "https://example.invalid/unknown/",
                "note"
            )
            .unwrap()
            .value
            == "sidecar"
    );
    Ok(())
}

#[test]
fn stale_catalog_recovery_restores_capture_without_publishing_stale_payload() -> Result<()> {
    use photocatalog::metadata_export::{self, ExportBoundary, ExportState};
    use std::io::Read;
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    jpeg(&originals.join("photo.jpg"));
    let sidecar = originals.join("photo.xmp");
    fs::write(&sidecar, packet(2))?;
    let root = temp.path().join("catalog");
    let mut cat = Catalog::open(&root)?;
    cat.import(&originals, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let view = cat.metadata(&asset.id)?;
    let base = view.fields[0].selected_model.unwrap();
    let edit = cat.edit_metadata(
        &asset.id,
        view.revision,
        Some(base),
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Rating".into(),
            value: "4".into(),
        }],
    )?;
    let plan = cat.plan_metadata_export(&asset.id, edit.revision, base, &sidecar)?;
    let db = rusqlite::Connection::open(root.join("catalog.sqlite3"))?;
    let compressed:Vec<u8>=db.query_row("SELECT b.compressed FROM metadata_export_plans p JOIN metadata_blobs b ON b.hash=p.payload_hash WHERE p.operation=?1",[&plan.destination.operation],|r|r.get(0))?;
    drop(db);
    let mut payload = Vec::new();
    flate2::read::ZlibDecoder::new(compressed.as_slice()).read_to_end(&mut payload)?;
    let receipt =
        metadata_export::apply_export_with_hook(&plan.destination, &payload, |boundary| {
            if matches!(
                boundary,
                ExportBoundary::Captured | ExportBoundary::BeforeRestore
            ) {
                Err(std::io::Error::other("injected interruption"))
            } else {
                Ok(())
            }
        })?;
    ensure!(receipt.state == ExportState::Recoverable && !sidecar.exists());
    cat.edit_metadata(
        &asset.id,
        edit.revision,
        Some(edit.model_ids[0]),
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Rating".into(),
            value: "5".into(),
        }],
    )?;
    let restored = cat.recover_metadata_export(&receipt.recovery_directory)?;
    ensure!(restored.state == ExportState::Restored && fs::read(&sidecar)? == packet(2).as_bytes());
    ensure!(
        cat.apply_metadata_export(&plan.destination.operation)
            .is_err()
    );
    Ok(())
}

#[test]
fn preview_attachment_guard_rejects_stale_authority_and_holds_writer_lock() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    jpeg(&originals.join("photo.jpg"));
    let root = temp.path().join("catalog");
    let mut cat = Catalog::open(&root)?;
    cat.import(&originals, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let identity = cat.render_identity(&asset.id)?;
    let db = rusqlite::Connection::open(root.join("catalog.sqlite3"))?;
    db.busy_timeout(std::time::Duration::from_millis(1))?;
    ensure!(
        cat.with_render_identity(&identity, || {
            ensure!(
                db.execute(
                    "UPDATE assets SET render_generation=render_generation+1 WHERE id=?1",
                    [&asset.id]
                )
                .is_err()
            );
            Ok(7)
        })? == Some(7)
    );
    db.execute(
        "UPDATE assets SET render_generation=render_generation+1 WHERE id=?1",
        [&asset.id],
    )?;
    ensure!(
        cat.with_render_identity(&identity, || -> Result<()> {
            panic!("stale attachment entered")
        })?
        .is_none()
    );
    Ok(())
}

#[test]
fn qualifier_only_catalog_edits_are_explicitly_selected_and_stay_selected() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    jpeg(&originals.join("photo.jpg"));
    let payload = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="https://example.invalid/qualifier-review/"><xmp:Rating rdf:parseType="Resource"><rdf:value>3</rdf:value><u:note>before</u:note></xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    fs::write(originals.join("photo.xmp"), payload)?;
    let mut cat = Catalog::open(temp.path().join("catalog"))?;
    cat.import(&originals, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let view = cat.metadata(&asset.id)?;
    let base = view.fields[0].selected_model.unwrap();
    let prefix =
        xmp_toolkit::XmpMeta::namespace_prefix("https://example.invalid/qualifier-review/")
            .unwrap();
    let edit = cat.edit_metadata(
        &asset.id,
        view.revision,
        Some(base),
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: format!("Rating/?{prefix}note"),
            value: "after".into(),
        }],
    )?;
    let view = cat.metadata(&asset.id)?;
    ensure!(!view.fields[0].conflicted && view.fields[0].selected_model == Some(edit.model_ids[0]));
    let next = cat.edit_metadata(
        &asset.id,
        edit.revision,
        Some(edit.model_ids[0]),
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Label".into(),
            value: "Red".into(),
        }],
    )?;
    ensure!(
        cat.metadata(&asset.id)?
            .fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .selected_model
            == Some(next.model_ids[0])
    );
    Ok(())
}
