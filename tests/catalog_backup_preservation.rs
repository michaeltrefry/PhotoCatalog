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
