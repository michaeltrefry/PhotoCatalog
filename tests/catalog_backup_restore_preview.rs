//! Requested preview regeneration uses the real isolated CPU worker, synthetic
//! PNG pixels, and the restored recipe after a public fingerprint-checked relink.
use anyhow::{Result, ensure};
use photocatalog::{
    Catalog,
    catalog_backup::{Limits, backup_catalog, restore_catalog, restore_status},
    catalog_edits::VariantKey,
    catalog_storage::{PathReference, RelinkScope},
    edit::{Recipe, RecipeV1},
    preview::*,
    storage_volume::NativePath,
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn service(root: &Path, originals: &Path) -> Result<PreviewService> {
    PreviewService::open(
        StoreConfig {
            manifest_root: root.join("manifest"),
            thumbnail_root: root.join("thumb"),
            large_root: root.join("large"),
            layout: Layout::Flat,
            thumbnail_bytes: 8 * 1024 * 1024,
            large_bytes: 8 * 1024 * 1024,
        },
        &[originals.to_owned()],
        PathBuf::from(env!("CARGO_BIN_EXE_photocatalog")),
        PreviewPolicy::default(),
        ServiceLimits::default(),
    )
}
fn ready(service: &mut PreviewService, cat: &mut Catalog, key: &VariantKey) -> Result<()> {
    let ticket = service.request_variant(cat, key, Tier::Thumbnail, Priority::Foreground)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        service.tick(cat)?;
        if let Some(done) = service.take_completion(ticket) {
            ensure!(
                matches!(done, ServiceCompletion::Ready),
                "preview failed: {done:?}"
            );
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "synthetic preview worker timed out"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn restore_offline_then_relink_and_regenerate_missing_variant_preview() -> Result<()> {
    let temp = tempfile::Builder::new().tempdir_in(std::env::temp_dir().canonicalize()?)?;
    let originals = temp.path().join("old originals");
    fs::create_dir_all(originals.join("nested 雪"))?;
    let relative = Path::new("nested 雪/photo.png");
    image::RgbImage::from_pixel(24, 16, image::Rgb([40u8, 70, 90]))
        .save(originals.join(relative))?;
    let original_bytes = fs::read(originals.join(relative))?;
    let source = temp.path().join("source");
    let mut cat = Catalog::open(&source)?;
    cat.import(&originals, None, |_| Ok(()))?;
    let master = VariantKey::master(cat.browse(0, 10)?[0].id.clone());
    let copy = cat.create_edit_variant(&master, 0, "brighter")?.key;
    let edited = cat.save_edit_recipe(
        &copy,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 1.0,
            ..Default::default()
        }),
    )?;
    let source_cache = temp.path().join("source-cache");
    let mut old_previews = service(&source_cache, &originals)?;
    ready(&mut old_previews, &mut cat, &copy)?;
    let old = old_previews
        .cached_variant(&cat, &copy, Tier::Thumbnail, false)?
        .unwrap();
    let expected_pixels = old.pixels.pixels().pixels().to_vec();
    drop(old_previews);
    let source_paths = serde_json::to_vec(&cat.browse(0, 10)?)?;
    let legacy_preview = cat.preview(&master.asset_id)?;
    let moved = temp.path().join("new originals");
    fs::rename(&originals, &moved)?;
    let bundle = temp.path().join("bundle");
    let restored = temp.path().join("restored");
    let limits = Limits {
        min_free_bytes: 0,
        ..Default::default()
    };
    backup_catalog(&source, &bundle, &limits, |_| Ok(()))?;
    restore_catalog(&bundle, &restored, &limits, |_| Ok(()))?;
    assert!(fs::read_dir(restored.join("previews"))?.next().is_none());
    let mut restored_cat = Catalog::open(&restored)?;
    assert!(restored_cat.preview(&master.asset_id).is_err());
    assert_eq!(
        restored_cat.edit_variant(&copy)?.recipe_digest,
        edited.recipe_digest
    );
    let plan = restored_cat.begin_relink(RelinkScope::Prefix {
        from: PathReference::native(&originals),
        destinations: vec![NativePath::from_path(&moved)],
    })?;
    for _ in 0..10 {
        if restored_cat.relink_plan(&plan.id)?.state != "preparing" {
            break;
        }
        restored_cat.prepare_relink_batch(&plan.id, 1)?;
    }
    assert_eq!(restored_cat.relink_plan(&plan.id)?.matched, 1);
    restored_cat.apply_relink(&plan.id)?;
    assert_eq!(
        restored_cat.browse(0, 10)?[0].original_path,
        moved.join(relative).to_string_lossy()
    );
    let fresh_cache = temp.path().join("restored-cache");
    let mut previews = service(&fresh_cache, &moved)?;
    assert!(
        previews
            .cached_variant(&restored_cat, &copy, Tier::Thumbnail, false)?
            .is_none()
    );
    ready(&mut previews, &mut restored_cat, &copy)?;
    let new = previews
        .cached_variant(&restored_cat, &copy, Tier::Thumbnail, false)?
        .unwrap();
    assert_eq!(new.pixels.pixels().pixels(), expected_pixels.as_slice());
    assert_eq!(
        new.key.as_ref().unwrap().edit_revision,
        edited.revision as u64
    );
    assert_eq!(new.key.as_ref().unwrap().variant_id, copy.variant_id);
    let metrics = previews.take_worker_metrics().unwrap();
    assert_ne!(metrics.pid, std::process::id());
    assert!(previews.native_work_drained());
    assert!(
        restore_status(&restored)?.unwrap().jobs_held,
        "preview request must not release external execution hold"
    );
    assert_eq!(serde_json::to_vec(&cat.browse(0, 10)?)?, source_paths);
    assert_eq!(cat.preview(&master.asset_id)?, legacy_preview);
    assert_eq!(fs::read(moved.join(relative))?, original_bytes);
    restored_cat.undo_relink(&plan.id)?;
    assert_eq!(
        serde_json::to_vec(&restored_cat.browse(0, 10)?)?,
        source_paths
    );
    Ok(())
}
