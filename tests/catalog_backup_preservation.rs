//! Public-domain preservation with real packets, choices, recipes and history.
//! All originals are generated locally; backup/restore runs while they are offline.
#[path = "support/backup_snapshot.rs"]
mod snapshot;

use anyhow::Result;
use photocatalog::{
    Catalog,
    catalog_backup::{Limits, backup_catalog, restore_catalog, restore_status},
    catalog_edits::VariantKey,
    edit::{Recipe, RecipeV1},
    organization::{KeywordKind, Operation},
    xmp::{self, Edit, Value},
};
use serde_json::json;
use std::fs;

fn packet(rating: u8) -> String {
    format!(
        r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="subject" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="https://example.invalid/unknown/" xmp:Rating="{rating}"><u:complex rdf:parseType="Resource"><u:preserve>雪 &amp; sunshine</u:preserve></u:complex></rdf:Description></rdf:RDF>"#
    )
}
fn recipe(ev: f32) -> Recipe {
    Recipe::V1(RecipeV1 {
        exposure_ev: ev,
        ..Default::default()
    })
}
fn public_state(cat: &Catalog, key: &VariantKey) -> Result<serde_json::Value> {
    let history = cat.metadata_history_for_image(key, 0, 100)?;
    let packets = history.iter().map(|o| {
        Ok(json!({"observation":o.id,"packets":cat.metadata_packets_for_image(key,o.id)?,
            "models":o.models.iter().map(|m|cat.metadata_model_for_image(key,m.id)).collect::<Result<Vec<_>>>()?}))
    }).collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"image":cat.image(key)?,"edit":cat.edit_variant(key)?,
        "edits":cat.edit_history(key,-1,100)?,"metadata":cat.metadata_for_image(key)?,
        "history":history,"packets":packets,"decisions":cat.metadata_decisions_for_image(key,0,100)?}),
    )
}

#[test]
fn whole_snapshot_preserves_real_xmp_choices_variants_undo_and_partial_evidence() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("catalog");
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    let photo = originals.join("雪.png");
    image::RgbImage::from_pixel(24, 16, image::Rgb([40u8, 70, 90])).save(&photo)?;
    fs::write(originals.join("雪.xmp"), packet(2))?;
    let original = fs::read(&photo)?;
    let mut cat = Catalog::open(&source)?;
    cat.import(&originals, None, |_| Ok(()))?;
    let master = VariantKey::master(cat.browse(0, 10)?[0].id.clone());
    let copy = cat.create_edit_variant(&master, 0, "Independent 雪")?.key;
    let first = cat.save_edit_recipe(&master, 0, &recipe(1.0))?;
    let second = cat.save_edit_recipe(&master, first.revision, &recipe(2.0))?;
    cat.undo_edit(&master, second.revision)?;
    cat.save_edit_recipe(&copy, 0, &recipe(-1.0))?;
    let metadata = cat.metadata_for_image(&copy)?;
    cat.edit_metadata_for_image(
        &copy,
        metadata.revision,
        None,
        &[Edit::Set {
            namespace: xmp::XMP.into(),
            path: "Rating".into(),
            value: "4".into(),
        }],
    )?;
    fs::write(originals.join("雪.xmp"), packet(3))?;
    cat.import(&originals, None, |_| Ok(()))?;
    for _ in 0..10 {
        if !cat.step_image_metadata_refresh(100)?.pending {
            break;
        }
    }
    assert!(!cat.step_image_metadata_refresh(100)?.pending);
    let revision = cat.metadata_for_image(&copy)?.revision;
    cat.organize_image(
        &copy,
        revision,
        Operation::AddKeyword {
            kind: KeywordKind::Hierarchical,
            path: vec!["Family".into(), "雪".into()],
        },
    )?;
    let collection =
        cat.create_collection("Restored family", json!({"source":"synthetic integration"}))?;
    let revision = cat.metadata_for_image(&copy)?.revision;
    cat.organize_image(&copy, revision, Operation::AddCollection { collection })?;
    let bytes: Vec<u8> = (0..8193).map(|v| (v % 251) as u8).collect();
    let evidence = cat.begin_migration_evidence(
        br#"{"kind":"opaque unsupported plugin","schema":1}"#,
        bytes.len() as u64,
    )?;
    let evidence = cat.append_migration_evidence(&evidence.id, 0, &bytes[..4096])?;
    assert!(!evidence.complete);
    assert!(cat.migration_evidence_chunk(&evidence.id, 0).is_err());
    let complete =
        cat.begin_migration_evidence(b"complete opaque companion bytes", bytes.len() as u64)?;
    let complete = cat.append_migration_evidence(&complete.id, 0, &bytes)?;
    assert!(complete.complete);
    let before_master = public_state(&cat, &master)?;
    let before_copy = public_state(&cat, &copy)?;
    assert!(cat.edit_variant(&master)?.can_redo);
    assert_eq!(
        cat.metadata_for_image(&master)?
            .fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .value,
        Some(Value::Text("3".into()))
    );
    assert_eq!(
        cat.metadata_for_image(&copy)?
            .fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .value,
        Some(Value::Text("4".into()))
    );
    let mut retained_packets = Vec::new();
    for observation in cat.metadata_history_for_image(&master, 0, 100)? {
        retained_packets.extend(
            cat.metadata_packets_for_image(&master, observation.id)?
                .into_iter()
                .map(|p| p.bytes),
        );
    }
    assert!(retained_packets.contains(&packet(2).into_bytes()));
    assert!(retained_packets.contains(&packet(3).into_bytes()));
    let before = snapshot::logical_snapshot(&source)?;
    assert!(!before["metadata_blobs"].is_empty());
    assert!(!before["metadata_choices"].is_empty());
    assert!(!before["edit_changes"].is_empty());
    assert!(!before["organization_keywords"].is_empty());
    let offline = temp.path().join("offline");
    fs::rename(&originals, &offline)?;
    let bundle = temp.path().join("backup");
    let restored = temp.path().join("restored");
    let limits = Limits {
        min_free_bytes: 0,
        ..Default::default()
    };
    backup_catalog(&source, &bundle, &limits, |_| Ok(()))?;
    restore_catalog(&bundle, &restored, &limits, |_| Ok(()))?;
    assert_eq!(snapshot::logical_snapshot(&bundle)?, before);
    assert_eq!(snapshot::logical_snapshot(&restored)?, before);
    assert_eq!(snapshot::logical_snapshot(&source)?, before);
    assert!(restore_status(&restored)?.unwrap().jobs_held);
    let mut restored_cat = Catalog::open(&restored)?;
    assert_eq!(public_state(&restored_cat, &master)?, before_master);
    assert_eq!(public_state(&restored_cat, &copy)?, before_copy);
    assert_eq!(restored_cat.migration_evidence(&evidence.id)?, evidence);
    assert!(
        restored_cat
            .migration_evidence_chunk(&evidence.id, 0)
            .is_err()
    );
    assert_eq!(restored_cat.migration_evidence(&complete.id)?, complete);
    assert_eq!(
        restored_cat.migration_evidence_chunk(&complete.id, 0)?,
        bytes
    );
    assert_eq!(
        restored_cat.migration_evidence_descriptor(&evidence.id)?,
        cat.migration_evidence_descriptor(&evidence.id)?
    );
    let current = restored_cat.edit_variant(&master)?;
    let redone = restored_cat.redo_edit(&master, current.revision)?;
    assert_eq!(redone.recipe, recipe(2.0));
    assert_eq!(
        restored_cat.undo_edit(&master, redone.revision)?.recipe,
        recipe(1.0)
    );
    assert_eq!(restored_cat.edit_variant(&copy)?.recipe, recipe(-1.0));
    assert_eq!(snapshot::logical_snapshot(&source)?, before);
    assert_eq!(fs::read(offline.join("雪.png"))?, original);
    assert_eq!(fs::read_to_string(offline.join("雪.xmp"))?, packet(3));
    Ok(())
}

#[test]
fn backup_snapshot_excludes_live_application_job_commits_and_preserves_queued_export() -> Result<()>
{
    use photocatalog::{
        catalog_backup::Phase,
        catalog_exports::{ExportTarget, MetadataSelection},
        image_export::{AlphaPolicy, OutputFormat, OutputProfile, OutputSize, OutputSpec},
        organization::BatchItem,
    };
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    for name in ["one", "two"] {
        image::RgbImage::from_pixel(16, 12, image::Rgb([40u8, 70, 90]))
            .save(originals.join(format!("{name}.png")))?;
        fs::write(originals.join(format!("{name}.xmp")), packet(2))?;
    }
    let root = temp.path().join("catalog");
    let mut catalog = Catalog::open(&root)?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let assets = catalog.browse(0, 10)?;
    assert_eq!(assets.len(), 2);
    let job = catalog.begin_organization_batch(Operation::Label {
        value: "Blue".into(),
    })?;
    let items = assets
        .iter()
        .map(|asset| {
            Ok(BatchItem {
                asset_id: asset.id.clone(),
                expected_revision: catalog.metadata(&asset.id)?.revision,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    catalog.append_organization_batch(&job.id, &items)?;
    let ready = catalog.seal_organization_batch(&job.id)?;
    assert_eq!(
        (ready.state.as_str(), ready.pending, ready.applied),
        ("ready", 2, 0)
    );

    // Freeze a real plan, including selected XMP bytes and source/recipe/destination
    // authority, while originals are available. Neither backup nor restore runs it.
    let exports = temp.path().join("exports");
    fs::create_dir(&exports)?;
    let destination = exports.join("queued.jpg");
    let key = VariantKey::master(assets[0].id.clone());
    let metadata = catalog.metadata_for_image(&key)?;
    let base = metadata
        .fields
        .iter()
        .find(|field| field.name == "rating")
        .unwrap()
        .selected_model;
    let export = catalog.begin_photo_export()?;
    let item = catalog.append_photo_export(
        &export.id,
        0,
        &ExportTarget {
            key: key.clone(),
            expected_revision: catalog.edit_variant(&key)?.revision,
            destination: destination.clone(),
            overwrite: false,
            metadata: MetadataSelection::Resolved {
                expected_revision: metadata.revision,
                base_model: base,
            },
        },
        &OutputSpec {
            size: OutputSize::Original,
            format: OutputFormat::Jpeg { quality: 90 },
            profile: OutputProfile::Srgb,
            alpha: AlphaPolicy::Composite {
                linear_rgb: [1.; 3],
            },
        },
        8 * 1024 * 1024,
        8 * 1024 * 1024,
    )?;
    let export = catalog.seal_photo_export_job(&export.id, 1)?;
    assert_eq!(export.state, "queued");
    assert_eq!(item.state, "pending");
    let export_plan = serde_json::to_value(catalog.photo_export_plan(&export.id, item.sequence)?)?;
    let before = snapshot::logical_snapshot(&root)?;
    assert!(!before["photo_export_blobs"].is_empty());
    let before_images = assets
        .iter()
        .map(|asset| public_state(&catalog, &VariantKey::master(&asset.id)))
        .collect::<Result<Vec<_>>>()?;
    let before_job = serde_json::to_value(&ready)?;
    let before_items = serde_json::to_value(catalog.organization_job_items(&job.id, 0, 10)?)?;
    fs::rename(&originals, temp.path().join("offline"))?;

    let mut writer = Catalog::open(&root)?;
    let mut snapshot_write = false;
    let mut copy_write = false;
    let bundle = temp.path().join("bundle");
    let restored = temp.path().join("restored");
    let limits = Limits {
        pages_per_step: 1,
        min_free_bytes: 0,
        max_seconds: 30,
        ..Default::default()
    };
    backup_catalog(&root, &bundle, &limits, |progress| {
        assert!(!originals.exists());
        match progress.phase {
            Phase::Snapshot => {
                assert!(!snapshot_write);
                let running = writer.step_organization_batch(&job.id)?;
                assert_eq!(
                    (running.state.as_str(), running.applied, running.pending),
                    ("running", 1, 1)
                );
                snapshot_write = true;
            }
            Phase::Copy if !copy_write => {
                assert!(snapshot_write);
                let complete = writer.step_organization_batch(&job.id)?;
                assert_eq!(
                    (complete.state.as_str(), complete.applied, complete.pending),
                    ("complete", 2, 0)
                );
                assert_eq!(
                    catalog.organization_job(&job.id)?.applied,
                    2,
                    "foreground reader sees committed job progress while backup remains pinned"
                );
                copy_write = true;
            }
            _ => {}
        }
        Ok(())
    })?;
    assert!(snapshot_write && copy_write);
    assert_eq!(snapshot::logical_snapshot(&bundle)?, before);
    assert_ne!(snapshot::logical_snapshot(&root)?, before);
    for asset in &assets {
        assert_eq!(
            catalog
                .metadata(&asset.id)?
                .fields
                .iter()
                .find(|field| field.name == "label")
                .unwrap()
                .value,
            Some(Value::Text("Blue".into()))
        );
    }
    restore_catalog(&bundle, &restored, &limits, |_| Ok(()))?;
    assert_eq!(snapshot::logical_snapshot(&restored)?, before);
    let mut restored_catalog = Catalog::open(&restored)?;
    assert_eq!(
        serde_json::to_value(restored_catalog.organization_job(&job.id)?)?,
        before_job
    );
    assert_eq!(
        serde_json::to_value(restored_catalog.organization_job_items(&job.id, 0, 10)?)?,
        before_items
    );
    for (asset, expected) in assets.iter().zip(before_images) {
        assert_eq!(
            public_state(&restored_catalog, &VariantKey::master(&asset.id))?,
            expected
        );
    }
    assert_eq!(
        serde_json::to_value(restored_catalog.photo_export_job(&export.id)?)?,
        serde_json::to_value(export.clone())?
    );
    assert_eq!(
        serde_json::to_value(restored_catalog.photo_export_plan(&export.id, item.sequence)?)?,
        export_plan
    );
    assert_eq!(
        restored_catalog.photo_export_items(&export.id, 0, 10)?[0].state,
        "pending"
    );
    assert!(
        restored_catalog.claim_photo_export(&export.id).is_err(),
        "restored queued export must remain held"
    );
    assert!(restore_status(&restored)?.unwrap().jobs_held);
    assert!(!destination.exists());
    assert_eq!(fs::read_dir(&exports)?.count(), 0);
    assert_eq!(catalog.organization_job(&job.id)?.state, "complete");
    assert_eq!(catalog.photo_export_job(&export.id)?.state, "queued");
    Ok(())
}
