//! Actual executable coverage for variant-aware preview work; originals are tiny
//! disposable PNGs, never the user's photo collection.
use photocatalog::{
    Catalog,
    catalog_edits::VariantKey,
    edit::{Recipe, RecipeV1},
    preview::*,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
fn service(root: &Path, originals: &Path) -> PreviewService {
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
    .unwrap()
}
fn setup(root: &Path) -> (Catalog, VariantKey, PathBuf) {
    let originals = root.join("originals");
    std::fs::create_dir(&originals).unwrap();
    let path = originals.join("pixel.png");
    image::RgbImage::from_pixel(24, 16, image::Rgb([64u8, 90, 120]))
        .save(&path)
        .unwrap();
    let mut catalog = Catalog::open(root.join("catalog")).unwrap();
    catalog.import(&originals, None, |_| Ok(())).unwrap();
    let key = VariantKey::master(catalog.browse(0, 1).unwrap()[0].id.clone());
    (catalog, key, originals)
}
fn finish(
    service: &mut PreviewService,
    catalog: &mut Catalog,
    ticket: Consumer,
) -> ServiceCompletion {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        service.tick(catalog).unwrap();
        if let Some(done) = service.take_completion(ticket) {
            return done;
        }
        assert!(Instant::now() < until, "worker timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn exposure(value: f32) -> Recipe {
    Recipe::V1(RecipeV1 {
        exposure_ev: value,
        ..Default::default()
    })
}
#[test]
fn edited_variants_and_proxy_refinement_coexist_without_mutating_originals() {
    let root = tempfile::tempdir().unwrap();
    let (mut catalog, master, originals) = setup(root.path());
    let original = std::fs::read(originals.join("pixel.png")).unwrap();
    let mut previews = service(&root.path().join("cache"), &originals);
    let variant = catalog.create_edit_variant(&master, 0, "brighter").unwrap();
    let saved = catalog
        .save_edit_recipe(&variant.key, 0, &exposure(1.0))
        .unwrap();
    let original_ticket = previews
        .request(
            &mut catalog,
            &master.asset_id,
            Tier::Thumbnail,
            Priority::Foreground,
        )
        .unwrap();
    assert!(matches!(
        finish(&mut previews, &mut catalog, original_ticket),
        ServiceCompletion::Ready
    ));
    let proxy = previews
        .request_interactive(
            &mut catalog,
            &variant.key,
            Tier::Thumbnail,
            Priority::Foreground,
        )
        .unwrap();
    let refined = previews
        .request_variant(
            &mut catalog,
            &variant.key,
            Tier::Thumbnail,
            Priority::Foreground,
        )
        .unwrap();
    assert!(matches!(
        finish(&mut previews, &mut catalog, proxy),
        ServiceCompletion::Ready
    ));
    assert!(matches!(
        finish(&mut previews, &mut catalog, refined),
        ServiceCompletion::Ready
    ));
    let original = (
        original,
        previews
            .cached(&catalog, &master.asset_id, Tier::Thumbnail, false)
            .unwrap()
            .unwrap(),
    );
    let interactive = previews
        .cached_interactive(&catalog, &variant.key, Tier::Thumbnail, false)
        .unwrap()
        .unwrap();
    let refined = previews
        .cached_variant(&catalog, &variant.key, Tier::Thumbnail, false)
        .unwrap()
        .unwrap();
    assert_ne!(interactive.key, refined.key);
    assert_eq!(
        refined.key.as_ref().unwrap().edit_revision,
        saved.revision as u64
    );
    assert_eq!(
        refined.key.as_ref().unwrap().variant_id,
        variant.key.variant_id
    );
    assert!(refined.pixels.pixels().pixels()[0] > original.1.pixels.pixels().pixels()[0] + 10);
    assert_eq!(
        original.0,
        std::fs::read(originals.join("pixel.png")).unwrap()
    );
    drop(previews);
    drop(catalog);
    let catalog = Catalog::open(root.path().join("catalog")).unwrap();
    std::fs::rename(&originals, root.path().join("offline")).unwrap();
    let mut previews = service(&root.path().join("cache"), &originals);
    assert!(
        previews
            .cached_variant(&catalog, &variant.key, Tier::Thumbnail, false)
            .unwrap()
            .is_some()
    );
    assert!(
        previews
            .cached_interactive(&catalog, &variant.key, Tier::Thumbnail, false)
            .unwrap()
            .is_some()
    );
}
#[test]
fn queued_reads_and_active_children_reject_undo_aba_and_release_before_external_worker() {
    let root = tempfile::tempdir().unwrap();
    let (mut catalog, key, originals) = setup(root.path());
    let mut previews = service(&root.path().join("cache"), &originals);
    let first = catalog.save_edit_recipe(&key, 0, &exposure(1.0)).unwrap();
    let read = previews
        .queue_read_variant(
            &catalog,
            &key,
            Tier::Thumbnail,
            true,
            Priority::Foreground,
            true,
        )
        .unwrap();
    let request = previews
        .request_interactive(&mut catalog, &key, Tier::Thumbnail, Priority::Foreground)
        .unwrap();
    previews.tick(&mut catalog).unwrap();
    assert!(!previews.native_work_drained());
    let second = catalog
        .save_edit_recipe(&key, first.revision, &exposure(2.0))
        .unwrap();
    let undo = catalog.undo_edit(&key, second.revision).unwrap();
    assert_eq!(undo.recipe_digest, first.recipe_digest);
    assert!(undo.revision > first.revision);
    assert_eq!(previews.tick_read(&catalog), Some(read));
    assert!(matches!(
        previews.take_read(read).unwrap().outcome,
        ReadOutcome::Stale
    ));
    assert!(matches!(
        finish(&mut previews, &mut catalog, request),
        ServiceCompletion::Stale
    ));
    assert!(previews.native_work_drained());
    let pause = previews.pause_native_launches().unwrap();
    let queued = previews
        .request_interactive(&mut catalog, &key, Tier::Thumbnail, Priority::Foreground)
        .unwrap();
    previews.tick(&mut catalog).unwrap();
    assert!(previews.native_work_drained());
    assert_eq!(previews.scheduler_usage().queued, 1);
    drop(pause);
    assert!(matches!(
        finish(&mut previews, &mut catalog, queued),
        ServiceCompletion::Ready
    ));
    assert_eq!(
        previews
            .cached_interactive(&catalog, &key, Tier::Thumbnail, false)
            .unwrap()
            .unwrap()
            .key
            .unwrap()
            .edit_revision,
        undo.revision as u64
    );
}
#[test]
fn prepared_cache_reuses_only_same_source_instance_and_recovers_corruption_as_miss() {
    let root = tempfile::tempdir().unwrap();
    let (mut catalog, key, originals) = setup(root.path());
    let cache = root.path().join("cache");
    let mut previews = service(&cache, &originals);
    for (revision, value) in [(0, 1.0), (1, 2.0)] {
        catalog
            .save_edit_recipe(&key, revision, &exposure(value))
            .unwrap();
        let request = previews
            .request_interactive(&mut catalog, &key, Tier::Thumbnail, Priority::Foreground)
            .unwrap();
        assert!(matches!(
            finish(&mut previews, &mut catalog, request),
            ServiceCompletion::Ready
        ));
        let observed = previews
            .cached_interactive(&catalog, &key, Tier::Thumbnail, false)
            .unwrap()
            .unwrap()
            .record
            .unwrap()
            .edit_input
            .unwrap();
        if revision == 0 {
            assert_eq!(observed, EditInputProvenance::OriginalDecoded);
        } else {
            let EditInputProvenance::PreparedProxy {
                receipt,
                source_instance_digest,
            } = observed
            else {
                panic!("warm request unexpectedly decoded the original");
            };
            assert_eq!(
                receipt.identity.source_fingerprint,
                photocatalog::fingerprint(&originals.join("pixel.png")).unwrap()
            );
            assert_eq!(receipt.identity.longest_edge, 1600);
            assert_eq!(receipt.identity.original_dimensions, (24, 16));
            assert_eq!(
                receipt.identity.renderer_identity,
                photocatalog::edit::renderer_identity()
            );
            assert_eq!(source_instance_digest.len(), 64);
            assert_eq!(receipt.blake3.len(), 64);
        }
    }
    let files = std::fs::read_dir(cache.join("manifest/prepared"))
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(files.len(), 1);
    let path = files[0].path();
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    catalog.save_edit_recipe(&key, 2, &exposure(0.5)).unwrap();
    let request = previews
        .request_interactive(&mut catalog, &key, Tier::Thumbnail, Priority::Foreground)
        .unwrap();
    assert!(matches!(
        finish(&mut previews, &mut catalog, request),
        ServiceCompletion::Ready
    ));
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before
    );
    assert!(
        previews
            .cached_interactive(&catalog, &key, Tier::Thumbnail, false)
            .unwrap()
            .unwrap()
            .record
            .unwrap()
            .provenance
            .notes
            .iter()
            .any(|n| n.contains("prepared cache reused=true"))
    );
    std::fs::write(&path, b"corrupt proxy").unwrap();
    catalog.save_edit_recipe(&key, 3, &exposure(0.0)).unwrap();
    let request = previews
        .request_interactive(&mut catalog, &key, Tier::Thumbnail, Priority::Foreground)
        .unwrap();
    assert!(matches!(
        finish(&mut previews, &mut catalog, request),
        ServiceCompletion::Ready
    ));
    assert!(std::fs::metadata(&path).unwrap().len() > 100);
    assert_eq!(
        previews
            .cached_interactive(&catalog, &key, Tier::Thumbnail, false)
            .unwrap()
            .unwrap()
            .record
            .unwrap()
            .edit_input,
        Some(EditInputProvenance::OriginalDecoded)
    );
    // Same path, dimensions and mtime with a different physical file must miss.
    let source = originals.join("pixel.png");
    let changed = originals.join("replacement.png");
    image::RgbImage::from_pixel(24, 16, image::Rgb([10u8, 20, 30]))
        .save(&changed)
        .unwrap();
    let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
    std::fs::File::options()
        .write(true)
        .open(&changed)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    std::fs::rename(&source, originals.join("old.png")).unwrap();
    std::fs::rename(changed, &source).unwrap();
    catalog.save_edit_recipe(&key, 4, &exposure(0.25)).unwrap();
    let request = previews
        .request_interactive(&mut catalog, &key, Tier::Thumbnail, Priority::Foreground)
        .unwrap();
    assert!(matches!(
        finish(&mut previews, &mut catalog, request),
        ServiceCompletion::Failed(_)
    ));
    assert!(
        previews
            .cached_interactive(&catalog, &key, Tier::Thumbnail, false)
            .unwrap()
            .is_none()
    );
    assert!(
        previews
            .cached_interactive(&catalog, &key, Tier::Thumbnail, true)
            .unwrap()
            .unwrap()
            .stale
    );
}

#[test]
fn restart_discards_stale_recipes_and_retries_explicit_resource_failures() {
    let root = tempfile::tempdir().unwrap();
    let (mut catalog, key, originals) = setup(root.path());
    let cache = root.path().join("cache");
    let mut previews = service(&cache, &originals);
    let first = catalog.save_edit_recipe(&key, 0, &exposure(1.0)).unwrap();
    previews
        .request_variant(&mut catalog, &key, Tier::Thumbnail, Priority::Foreground)
        .unwrap();
    drop(previews);
    catalog
        .save_edit_recipe(&key, first.revision, &exposure(2.0))
        .unwrap();
    let mut previews = service(&cache, &originals);
    assert!(
        previews
            .resume(&mut catalog, 0, 10, true)
            .unwrap()
            .1
            .is_empty()
    );
    assert!(previews.jobs(0, 10).unwrap().is_empty());
    drop(previews);
    let mut limits = ServiceLimits::default();
    limits.decode_limits.max_intermediate_pixels = 1;
    let mut previews = PreviewService::open(
        StoreConfig {
            manifest_root: cache.join("manifest"),
            thumbnail_root: cache.join("thumb"),
            large_root: cache.join("large"),
            layout: Layout::Flat,
            thumbnail_bytes: 8 * 1024 * 1024,
            large_bytes: 8 * 1024 * 1024,
        },
        &[originals.clone()],
        PathBuf::from(env!("CARGO_BIN_EXE_photocatalog")),
        PreviewPolicy::default(),
        limits,
    )
    .unwrap();
    let request = previews
        .request_variant(&mut catalog, &key, Tier::Thumbnail, Priority::Foreground)
        .unwrap();
    assert!(matches!(
        finish(&mut previews, &mut catalog, request),
        ServiceCompletion::NeedsResources(_)
    ));
    assert_eq!(previews.jobs(0, 10).unwrap().len(), 1);
    drop(previews);
    let mut previews = service(&cache, &originals);
    assert!(
        previews
            .resume(&mut catalog, 0, 10, false)
            .unwrap()
            .1
            .is_empty()
    );
    let retry = previews.resume(&mut catalog, 0, 10, true).unwrap().1;
    assert_eq!(retry.len(), 1);
    assert!(matches!(
        finish(&mut previews, &mut catalog, retry[0]),
        ServiceCompletion::Ready
    ));
    assert!(previews.jobs(0, 10).unwrap().is_empty());
}
