use photocatalog::{Catalog, preview::*, xmp};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
fn configuration(root: &Path) -> StoreConfig {
    StoreConfig {
        manifest_root: root.join("manifest"),
        thumbnail_root: root.join("thumbnails"),
        large_root: root.join("large"),
        layout: Layout::HashPrefix,
        thumbnail_bytes: 4 * 1024 * 1024,
        large_bytes: 4 * 1024 * 1024,
    }
}
fn service(root: &Path, originals: &Path, limits: ServiceLimits) -> PreviewService {
    PreviewService::open(
        configuration(root),
        &[originals.to_path_buf()],
        PathBuf::from(env!("CARGO_BIN_EXE_photocatalog")),
        PreviewPolicy::default(),
        limits,
    )
    .unwrap()
}
fn image(path: &Path, color: [u8; 3]) {
    image::RgbImage::from_pixel(18, 12, image::Rgb(color))
        .save(path)
        .unwrap();
}
fn omitted_roots_service(root: &Path) -> PreviewService {
    PreviewService::open(
        configuration(root),
        &[],
        PathBuf::from(env!("CARGO_BIN_EXE_photocatalog")),
        PreviewPolicy::default(),
        ServiceLimits::default(),
    )
    .unwrap()
}
fn wanted_rows(previews: &PreviewService) -> Vec<(String, String, Option<String>)> {
    let db = rusqlite::Connection::open(
        previews
            .cache_configuration()
            .manifest_root
            .join("previews.sqlite3"),
    )
    .unwrap();
    let mut statement = db
        .prepare("SELECT asset,desired,current FROM wanted ORDER BY asset,variant,tier")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn omitted_original_roots_cannot_admit_direct_import_or_scan_of_any_cache_root() {
    for tier in 0..3 {
        let root = tempfile::tempdir().unwrap();
        let mut previews = omitted_roots_service(&root.path().join("cache"));
        let config = previews.cache_configuration().clone();
        let source_root = [
            config.manifest_root,
            config.thumbnail_root,
            config.large_root,
        ][tier]
            .clone();
        let source = source_root.join("original.png");
        image(&source, [41, 87, 149]);
        let bytes = std::fs::read(&source).unwrap();
        let catalog_root = root.path().join("catalog");
        let mut catalog = Catalog::open(&catalog_root).unwrap();
        assert!(
            catalog
                .import_with_previews(&source_root, None, |_| Ok(()), &mut previews)
                .is_err()
        );
        assert!(catalog.browse(0, 10).unwrap().is_empty());
        let mut scan = catalog.begin_import(&source_root, None).unwrap();
        assert!(scan.advance(&mut catalog, &mut previews).is_err());
        assert!(catalog.browse(0, 10).unwrap().is_empty());
        drop(scan);
        // Compatibility import seeds an existing catalog reference; direct S6
        // requests still must reject it when configured original roots are empty.
        catalog.import(&source_root, None, |_| Ok(())).unwrap();
        let asset = catalog.browse(0, 10).unwrap().remove(0);
        assert!(
            previews
                .request(&mut catalog, &asset.id, Tier::Large, Priority::Foreground)
                .is_err()
        );
        let db = rusqlite::Connection::open(catalog_root.join("catalog.sqlite3")).unwrap();
        db.execute("UPDATE assets SET state='pending' WHERE id=?1", [&asset.id])
            .unwrap();
        assert!(
            previews
                .submit_import(&mut catalog, &asset.id, &source, &"a".repeat(64))
                .is_err()
        );
        assert!(previews.jobs(0, 10).unwrap().is_empty());
        assert!(wanted_rows(&previews).is_empty());
        assert!(previews.is_drained());
        assert_eq!(std::fs::read(source).unwrap(), bytes);
    }
}
#[test]
fn relink_into_relocated_cache_is_rejected_without_changing_desired_preview() {
    use photocatalog::{catalog_storage::RelinkScope, storage_volume::NativePath};
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    let source = originals.join("original.png");
    image(&source, [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    catalog.import(&originals, None, |_| Ok(())).unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let mut previews = omitted_roots_service(&root.path().join("cache"));
    let legitimate = previews
        .request(&mut catalog, &asset.id, Tier::Large, Priority::Foreground)
        .unwrap();
    previews.cancel(legitimate).unwrap();
    let moved = root.path().join("relocated-large");
    previews.begin_relocation(Tier::Large, &moved, &[]).unwrap();
    while !previews
        .relocation_step(Tier::Large, 1, 1024 * 1024)
        .unwrap()
        .complete
    {}
    let destination = moved.join("original.png");
    std::fs::copy(&source, &destination).unwrap();
    let plan = catalog
        .begin_relink(RelinkScope::Asset {
            asset_id: asset.id.clone(),
            destinations: vec![NativePath::from_path(&destination)],
        })
        .unwrap();
    while catalog.relink_plan(&plan.id).unwrap().state == "preparing" {
        catalog.prepare_relink_batch(&plan.id, 1).unwrap();
    }
    catalog.apply_relink(&plan.id).unwrap();
    let before = wanted_rows(&previews);
    assert!(
        previews
            .request(&mut catalog, &asset.id, Tier::Large, Priority::Foreground)
            .is_err()
    );
    assert_eq!(wanted_rows(&previews), before);
    assert!(previews.jobs(0, 10).unwrap().is_empty());
    assert!(previews.is_drained());
    assert_eq!(
        std::fs::read(source).unwrap(),
        std::fs::read(destination).unwrap()
    );
}
#[test]
fn frozen_worker_allowance_queues_second_actual_child_until_first_releases() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("a.png"), [41, 87, 149]);
    image(&originals.join("b.png"), [91, 45, 63]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    let mut previews = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits {
            workers: 2,
            per_worker_bytes: 2_269_118_464,
            working_bytes: 3 * 1024 * 1024 * 1024,
            ..ServiceLimits::default()
        },
    );
    catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
    let assets = catalog.browse(0, 10).unwrap();
    let first = previews
        .request(
            &mut catalog,
            &assets[0].id,
            Tier::Large,
            Priority::Foreground,
        )
        .unwrap();
    let second = previews
        .request(
            &mut catalog,
            &assets[1].id,
            Tier::Large,
            Priority::Foreground,
        )
        .unwrap();
    previews.tick(&mut catalog).unwrap();
    assert_eq!(previews.scheduler_usage().active, 1);
    assert_eq!(previews.scheduler_usage().queued, 1);
    assert_eq!(previews.scheduler_usage().reserved_bytes, 2_269_118_464);
    assert!(matches!(
        await_result(&mut previews, &mut catalog, first),
        ServiceCompletion::Ready
    ));
    assert!(matches!(
        await_result(&mut previews, &mut catalog, second),
        ServiceCompletion::Ready
    ));
    assert_eq!(previews.scheduler_usage().reserved_bytes, 0);
    assert!(previews.is_drained());
}
#[test]
fn held_export_with_maximum_staging_limit_blocks_worker_without_overflow_then_recovers() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("a.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    let mut previews = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits {
            encoded_staging_bytes: u64::MAX,
            ..ServiceLimits::default()
        },
    );
    catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let held = previews
        .encoded_cached(&catalog, &asset.id, Tier::Thumbnail, false)
        .unwrap()
        .unwrap();
    // The held export reserves the remaining allowance, not a giant allocation.
    let native = previews
        .request(&mut catalog, &asset.id, Tier::Large, Priority::Background)
        .unwrap();
    previews.tick(&mut catalog).unwrap();
    assert_eq!(previews.scheduler_usage().active, 0);
    assert_eq!(previews.scheduler_usage().queued, 1);
    let objects = previews.store_usage().unwrap().objects;
    let request = previews
        .queue_read(
            &catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )
        .unwrap();
    assert_eq!(previews.tick_read(&catalog), Some(request));
    assert!(matches!(
        previews.take_read(request).unwrap().outcome,
        ReadOutcome::Failed {
            resource_limit: true,
            ..
        }
    ));
    assert_eq!(
        previews.store_usage().unwrap().objects,
        objects,
        "encoded pressure cannot invalidate retained state"
    );
    drop(held);
    let retry = previews
        .queue_read(
            &catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )
        .unwrap();
    assert_eq!(previews.tick_read(&catalog), Some(retry));
    assert!(matches!(
        previews.take_read(retry).unwrap().outcome,
        ReadOutcome::Ready(_)
    ));
    assert!(matches!(
        await_result(&mut previews, &mut catalog, native),
        ServiceCompletion::Ready
    ));
    assert!(previews.is_drained());
}
#[test]
fn retained_read_queue_prioritizes_and_shares_admission_with_native_jobs() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("a.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    let mut previews = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits {
            requests: 3,
            ..ServiceLimits::default()
        },
    );
    catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let native = previews
        .request(&mut catalog, &asset.id, Tier::Large, Priority::Background)
        .unwrap();
    let background = previews
        .queue_read(
            &catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Background,
        )
        .unwrap();
    let foreground = previews
        .queue_read(
            &catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )
        .unwrap();
    assert_eq!(previews.available_request_slots(), 0);
    assert!(
        previews
            .queue_read(
                &catalog,
                &asset.id,
                Tier::Thumbnail,
                false,
                Priority::Foreground
            )
            .is_err()
    );
    assert_eq!(previews.tick_read(&catalog), Some(foreground));
    assert_eq!(previews.tick_read(&catalog), Some(background));
    assert_eq!(
        previews.available_request_slots(),
        0,
        "unconsumed results remain admitted"
    );
    let ReadOutcome::Ready(held) = previews.take_read(foreground).unwrap().outcome else {
        panic!("expected retained pixels");
    };
    previews.cancel(native).unwrap();
    assert!(previews.cancel_read(background));
    previews.clear_decoded_cache();
    assert!(
        previews.decoded_live_bytes() > 0,
        "caller-held pixels remain charged after cancellation/eviction"
    );
    assert!(!previews.cancel_read(foreground));
    drop(held);
    assert_eq!(previews.decoded_live_bytes(), 0);
    let old = previews
        .queue_read(
            &catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )
        .unwrap();
    assert!(previews.cancel_read(old));
    let new = previews
        .queue_read(
            &catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )
        .unwrap();
    assert!(new.0 > old.0);
    assert_eq!(previews.tick_read(&catalog), Some(new));
    assert!(previews.take_read(old).is_none());
    assert!(previews.cancel_read(new));
    assert!(previews.is_drained());
}

#[test]
fn retained_read_error_is_consumable_and_does_not_poison_future_progress() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("a.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    let mut previews = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits::default(),
    );
    catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let ticket = previews
        .queue_read(
            &catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )
        .unwrap();
    previews.tick_read(&catalog);
    let ReadOutcome::Ready(view) = previews.take_read(ticket).unwrap().outcome else {
        panic!("retained read");
    };
    let digest = view.key.as_ref().unwrap().digest().unwrap();
    drop(view);
    let encoded = previews
        .cache_configuration()
        .thumbnail_root
        .join(&digest[..2])
        .join(&digest[2..4])
        .join(digest);
    std::fs::write(encoded, b"corrupt").unwrap();
    let ticket = previews
        .queue_read(
            &catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )
        .unwrap();
    assert_eq!(previews.tick_read(&catalog), Some(ticket));
    assert!(matches!(
        previews.take_read(ticket).unwrap().outcome,
        ReadOutcome::Failed { .. }
    ));
    assert!(previews.is_drained());
    let retry = previews
        .queue_read(
            &catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )
        .unwrap();
    assert_eq!(previews.tick_read(&catalog), Some(retry));
    assert!(matches!(
        previews.take_read(retry).unwrap().outcome,
        ReadOutcome::Missing
    ));
    assert!(previews.is_drained());
}
#[test]
fn measured_cache_reads_use_identical_pixels_and_report_hits_misses_and_errors() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("a.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    let mut previews = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits::default(),
    );
    catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let mut metrics = CacheReadMetrics::default();
    let first = previews
        .cached_with_metrics(&catalog, &asset.id, Tier::Thumbnail, false, &mut metrics)
        .unwrap()
        .unwrap();
    assert_eq!((metrics.decoded_hits, metrics.decoded_misses), (0, 1));
    assert!(metrics.returned_pixels);
    let next = previews
        .cached_with_metrics(&catalog, &asset.id, Tier::Thumbnail, false, &mut metrics)
        .unwrap()
        .unwrap();
    assert_eq!((metrics.decoded_hits, metrics.decoded_misses), (1, 0));
    assert_eq!(
        first.pixels.pixels().pixels(),
        next.pixels.pixels().pixels()
    );
    let ordinary = previews
        .cached(&catalog, &asset.id, Tier::Thumbnail, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        ordinary.pixels.pixels().pixels(),
        first.pixels.pixels().pixels()
    );
    assert!(
        previews
            .cached_with_metrics(&catalog, "missing", Tier::Thumbnail, false, &mut metrics)
            .is_err()
    );
    assert!(!metrics.returned_pixels);
    assert_eq!((metrics.decoded_hits, metrics.decoded_misses), (0, 0));
    assert!(metrics.total_ms >= metrics.catalog_identity_ms);
}
#[test]
fn canceled_active_key_cannot_remove_replacement_before_reap() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("a.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    catalog.import(&originals, None, |_| Ok(())).unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let mut previews = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits::default(),
    );
    let first = previews
        .request(&mut catalog, &asset.id, Tier::Large, Priority::Foreground)
        .unwrap();
    previews.tick(&mut catalog).unwrap();
    assert_eq!(previews.scheduler_usage().active, 1);
    previews.cancel(first).unwrap();
    let replacement = previews
        .request(&mut catalog, &asset.id, Tier::Large, Priority::Foreground)
        .unwrap();
    assert_ne!(first, replacement);
    assert_eq!(previews.jobs(0, 10).unwrap().len(), 1);
    assert!(matches!(
        await_result(&mut previews, &mut catalog, replacement),
        ServiceCompletion::Ready
    ));
    assert!(previews.take_completion(first).is_none());
    assert!(previews.jobs(0, 10).unwrap().is_empty());
    assert_eq!(previews.scheduler_usage().reserved_bytes, 0);
    assert!(previews.is_drained());
}

#[test]
fn synchronous_import_rejects_caller_owned_request_without_consuming_it() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("a.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    catalog.import(&originals, None, |_| Ok(())).unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let limits = ServiceLimits {
        requests: 1,
        ..ServiceLimits::default()
    };
    let mut previews = service(&root.path().join("cache"), &originals, limits);
    let caller = previews
        .request(&mut catalog, &asset.id, Tier::Large, Priority::Foreground)
        .unwrap();
    let error = catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap_err();
    assert!(error.to_string().contains("drained service"));
    assert_eq!(previews.scheduler_usage().queued, 1);
    assert!(matches!(
        await_result(&mut previews, &mut catalog, caller),
        ServiceCompletion::Ready
    ));
    catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
}

#[test]
fn resume_error_rolls_back_new_handles_but_preserves_durable_jobs() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    for n in 0..2 {
        image(&originals.join(format!("{n}.png")), [41 + n, 87, 149]);
    }
    let catalog_path = root.path().join("catalog");
    let cache = root.path().join("cache");
    let mut catalog = Catalog::open(&catalog_path).unwrap();
    catalog.import(&originals, None, |_| Ok(())).unwrap();
    let assets = catalog.browse(0, 10).unwrap();
    let mut previews = service(&cache, &originals, ServiceLimits::default());
    previews
        .request(
            &mut catalog,
            &assets[0].id,
            Tier::Large,
            Priority::Background,
        )
        .unwrap();
    previews
        .request_interactive(
            &mut catalog,
            &photocatalog::catalog_edits::VariantKey::master(&assets[1].id),
            Tier::Large,
            Priority::Background,
        )
        .unwrap();
    drop(previews); // queued journals survive owner shutdown
    // A source-binding change now correctly invalidates the physical identity.
    // Instead change policy to make only the second (interactive) job invalid,
    // exercising admission rollback after the first handle was created.
    let mut policy = PreviewPolicy::default();
    policy.large.edge = 1601;
    let mut previews = PreviewService::open(
        configuration(&cache),
        std::slice::from_ref(&originals),
        PathBuf::from(env!("CARGO_BIN_EXE_photocatalog")),
        policy,
        ServiceLimits::default(),
    )
    .unwrap();
    assert!(previews.resume(&mut catalog, 0, 10, false).is_err());
    assert!(previews.is_drained());
    assert_eq!(previews.scheduler_usage().consumers, 0);
    assert_eq!(previews.jobs(0, 10).unwrap().len(), 2);
    drop(previews);
    let mut previews = service(&cache, &originals, ServiceLimits::default());
    let (_, consumers) = previews.resume(&mut catalog, 0, 10, false).unwrap();
    assert_eq!(consumers.len(), 2);
    for consumer in consumers {
        assert!(matches!(
            await_result(&mut previews, &mut catalog, consumer),
            ServiceCompletion::Ready
        ));
    }
    assert!(previews.is_drained());
    assert!(previews.jobs(0, 10).unwrap().is_empty());
}
fn await_result(
    service: &mut PreviewService,
    catalog: &mut Catalog,
    consumer: Consumer,
) -> ServiceCompletion {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        service.tick(catalog).unwrap();
        if let Some(result) = service.take_completion(consumer) {
            return result;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn service_import_publishes_real_worker_pixels_and_reads_without_originals() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    let source = originals.join("photo.png");
    image(&source, [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    let mut service = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits::default(),
    );
    let report = catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut service)
        .unwrap();
    assert_eq!(report.imported, 1);
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    assert_eq!(asset.state, "ready");
    let first = service
        .cached(&catalog, &asset.id, Tier::Thumbnail, false)
        .unwrap()
        .unwrap();
    assert!(!first.stale);
    assert!(first.record.is_some());
    assert_eq!(first.pixels.pixels().width(), 18);
    assert_eq!(
        first.record.unwrap().provenance.pipeline_version,
        "photocatalog-render-4"
    );
    std::fs::remove_file(source).unwrap();
    service.clear_decoded_cache();
    assert!(
        !service
            .cached(&catalog, &asset.id, Tier::Thumbnail, false)
            .unwrap()
            .unwrap()
            .stale
    );
}
#[test]
fn resource_limited_replacement_keeps_legacy_thumbnail_and_resumes_durably() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    let source = originals.join("photo.png");
    image(&source, [41, 87, 149]);
    let catalog_path = root.path().join("catalog");
    let mut catalog = Catalog::open(&catalog_path).unwrap();
    catalog.import(&originals, None, |_| Ok(())).unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let legacy = catalog.preview(&asset.id).unwrap();
    image(&source, [149, 87, 41]);
    let mut limits = ServiceLimits::default();
    limits.decode_limits.max_intermediate_pixels = 1;
    let cache = root.path().join("cache");
    let mut previews = service(&cache, &originals, limits);
    let report = catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
    assert_eq!(report.awaiting_resources, 1);
    assert_eq!(catalog.get(&asset.id).unwrap().state, "pending");
    let fallback = previews
        .cached(&catalog, &asset.id, Tier::Thumbnail, true)
        .unwrap()
        .unwrap();
    assert!(fallback.stale && fallback.key.is_none() && fallback.record.is_none());
    assert_eq!(
        fallback.legacy_hash.as_deref(),
        Some(blake3::hash(&legacy).to_hex().as_str())
    );
    assert!(matches!(
        previews.jobs(0, 10).unwrap()[0].state,
        JobState::NeedsResources(_)
    ));
    drop(fallback);
    drop(previews);
    drop(catalog);
    let mut catalog = Catalog::open(catalog_path).unwrap();
    let mut previews = service(&cache, &originals, ServiceLimits::default());
    let (_, no_retry) = previews.resume(&mut catalog, 0, 10, false).unwrap();
    assert!(no_retry.is_empty());
    let (_, retry) = previews.resume(&mut catalog, 0, 10, true).unwrap();
    assert_eq!(retry.len(), 1);
    assert!(matches!(
        await_result(&mut previews, &mut catalog, retry[0]),
        ServiceCompletion::Ready
    ));
    assert!(previews.jobs(0, 10).unwrap().is_empty());
    assert_eq!(catalog.get(&asset.id).unwrap().state, "ready");
    assert!(
        !previews
            .cached(&catalog, &asset.id, Tier::Thumbnail, false)
            .unwrap()
            .unwrap()
            .stale
    );
}
#[test]
fn catalog_generation_rejects_completed_old_worker_pixels() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("photo.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    catalog.import(&originals, None, |_| Ok(())).unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let mut previews = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits::default(),
    );
    let consumer = previews
        .request(
            &mut catalog,
            &asset.id,
            Tier::Thumbnail,
            Priority::Foreground,
        )
        .unwrap();
    previews.tick(&mut catalog).unwrap();
    assert_eq!(previews.scheduler_usage().active, 1);
    let retained = previews
        .queue_read(
            &catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )
        .unwrap();
    let revision = catalog.metadata(&asset.id).unwrap().revision;
    catalog
        .edit_metadata(
            &asset.id,
            revision,
            None,
            &[xmp::Edit::Set {
                namespace: xmp::XMP.into(),
                path: "Rating".into(),
                value: "4".into(),
            }],
        )
        .unwrap();
    assert_eq!(previews.tick_read(&catalog), Some(retained));
    let stale = previews.take_read(retained).unwrap();
    assert!(matches!(stale.outcome, ReadOutcome::Stale));
    assert_eq!(
        stale.metrics.decoded_misses, 0,
        "changed generation is rejected before decode"
    );
    assert!(matches!(
        await_result(&mut previews, &mut catalog, consumer),
        ServiceCompletion::Stale
    ));
    assert_eq!(previews.scheduler_usage().reserved_bytes, 0);
    assert_eq!(previews.store_usage().unwrap().objects, 0);
}
#[test]
fn known_nonpixel_rating_change_does_not_discard_pending_worker() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("photo.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    catalog.import(&originals, None, |_| Ok(())).unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let mut previews = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits::default(),
    );
    let consumer = previews
        .request(
            &mut catalog,
            &asset.id,
            Tier::Thumbnail,
            Priority::Foreground,
        )
        .unwrap();
    previews.tick(&mut catalog).unwrap();
    let revision = catalog.metadata(&asset.id).unwrap().revision;
    catalog
        .organize_asset(
            &asset.id,
            revision,
            photocatalog::organization::Operation::Rating { value: 4 },
        )
        .unwrap();
    assert!(matches!(
        await_result(&mut previews, &mut catalog, consumer),
        ServiceCompletion::Ready
    ));
}

#[test]
#[ignore = "subprocess entry; invoked by catalog_manifest_interruptions_reconcile_without_originals"]
fn crash_child_entry() {
    let root = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_SERVICE_CRASH_ROOT").expect("private crash fixture"),
    );
    let phase: ServiceEvent =
        serde_json::from_str(&std::env::var("PHOTOCATALOG_SERVICE_CRASH_PHASE").unwrap()).unwrap();
    let originals = root.join("originals");
    let mut catalog = Catalog::open(root.join("catalog")).unwrap();
    let mut previews = service(&root.join("cache"), &originals, ServiceLimits::default());
    previews.set_observer(move |event| {
        if event == phase {
            std::process::exit(86);
        }
        Ok(())
    });
    catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
    panic!("named durable boundary was not reached");
}
#[test]
fn catalog_manifest_interruptions_reconcile_without_originals() {
    for phase in [
        ServiceEvent::ManifestAttached,
        ServiceEvent::BeforeCatalogCommit,
        ServiceEvent::CatalogCommitted,
        ServiceEvent::JournalRemoving,
    ] {
        let root = tempfile::tempdir().unwrap();
        let originals = root.path().join("originals");
        std::fs::create_dir(&originals).unwrap();
        let source = originals.join("photo.png");
        image(&source, [41, 87, 149]);
        let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
        let mut previews = service(
            &root.path().join("cache"),
            &originals,
            ServiceLimits::default(),
        );
        catalog
            .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
            .unwrap();
        let asset = catalog.browse(0, 10).unwrap().remove(0);
        drop(previews);
        drop(catalog);
        image(&source, [149, 87, 41]);
        let original = std::fs::read(&source).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_child_entry", "--ignored"])
            .env("PHOTOCATALOG_SERVICE_CRASH_ROOT", root.path())
            .env(
                "PHOTOCATALOG_SERVICE_CRASH_PHASE",
                serde_json::to_string(&phase).unwrap(),
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("owner checkpoint timeout {phase:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.code(), Some(86), "{phase:?}");
        assert_eq!(std::fs::read(&source).unwrap(), original);
        std::fs::remove_file(source).unwrap();
        let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
        let mut previews = service(
            &root.path().join("cache"),
            &originals,
            ServiceLimits::default(),
        );
        assert_eq!(previews.jobs(0, 10).unwrap().len(), 1);
        let (_, workers) = previews.resume(&mut catalog, 0, 10, false).unwrap();
        assert!(
            workers.is_empty(),
            "valid attached output must not rerender"
        );
        assert_eq!(catalog.get(&asset.id).unwrap().state, "ready");
        assert!(previews.jobs(0, 10).unwrap().is_empty());
        let ready = previews
            .cached(&catalog, &asset.id, Tier::Thumbnail, false)
            .unwrap()
            .unwrap();
        assert!(!ready.stale && ready.record.is_some());
    }
}
#[test]
fn relocation_resumes_copy_and_authoritative_switch_with_old_settings() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    for n in 0..2 {
        image(&originals.join(format!("photo{n}.png")), [41 + n, 87, 149]);
    }
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    let cache = root.path().join("cache");
    let mut previews = service(&cache, &originals, ServiceLimits::default());
    catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
    let assets = catalog.browse(0, 10).unwrap();
    let old = previews.cache_configuration().thumbnail_root.clone();
    let destination = root.path().join("moved-thumbnails");
    previews
        .begin_relocation(
            Tier::Thumbnail,
            &destination,
            std::slice::from_ref(&originals),
        )
        .unwrap();
    let first = previews
        .relocation_step(Tier::Thumbnail, 1, 1024 * 1024)
        .unwrap();
    assert_eq!(first.phase, "copy");
    assert_eq!(previews.cache_configuration().thumbnail_root, old);
    drop(previews);
    let mut previews = service(&cache, &originals, ServiceLimits::default());
    loop {
        let result = previews
            .relocation_step(Tier::Thumbnail, 1, 1024 * 1024)
            .unwrap();
        if result.phase == "cleanup" {
            break;
        }
    }
    assert_eq!(
        previews.cache_configuration().thumbnail_root,
        std::fs::canonicalize(&destination).unwrap()
    );
    drop(previews);
    let mut previews = service(&cache, &originals, ServiceLimits::default());
    assert_eq!(
        previews.cache_configuration().thumbnail_root,
        std::fs::canonicalize(destination).unwrap()
    );
    while !previews
        .relocation_step(Tier::Thumbnail, 1, 1024 * 1024)
        .unwrap()
        .complete
    {}
    for asset in assets {
        previews.clear_decoded_cache();
        assert!(
            !previews
                .cached(&catalog, &asset.id, Tier::Thumbnail, false)
                .unwrap()
                .unwrap()
                .stale
        );
    }
}

#[test]
fn incremental_import_yields_to_foreground_and_resumes_its_background_worker() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("a.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    let mut previews = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits::default(),
    );
    catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
    let existing = catalog.browse(0, 10).unwrap().remove(0);
    image(&originals.join("b.png"), [149, 87, 41]);
    let mut scan = catalog.begin_import(&originals, None).unwrap();
    let background = loop {
        let progress = scan.advance(&mut catalog, &mut previews).unwrap();
        if let Some(consumer) = progress.consumer {
            break consumer;
        }
        assert!(!progress.finished);
    };
    previews.tick(&mut catalog).unwrap();
    assert_eq!(previews.scheduler_usage().active, 1);
    let reserved = previews.scheduler_usage().reserved_bytes;
    let retained = previews
        .queue_read(
            &catalog,
            &existing.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )
        .unwrap();
    assert_eq!(previews.tick_read(&catalog), Some(retained));
    let ReadOutcome::Ready(visible) = previews.take_read(retained).unwrap().outcome else {
        panic!("retained foreground read must progress while import owns the native slot");
    };
    assert_eq!(previews.scheduler_usage().active, 1);
    assert_eq!(previews.scheduler_usage().reserved_bytes, reserved);
    assert!(previews.decoded_live_bytes() > 0);
    let foreground = previews
        .request(
            &mut catalog,
            &existing.id,
            Tier::Large,
            Priority::Foreground,
        )
        .unwrap();
    assert!(matches!(
        await_result(&mut previews, &mut catalog, foreground),
        ServiceCompletion::Ready
    ));
    assert!(
        previews.take_completion(background).is_none(),
        "background import must yield until foreground is complete"
    );
    let result = await_result(&mut previews, &mut catalog, background);
    assert!(matches!(result, ServiceCompletion::Ready));
    scan.record_completion(&result);
    assert_eq!(previews.scheduler_usage().reserved_bytes, 0);
    assert_eq!(scan.report().imported, 1);
    assert!(
        catalog
            .browse(0, 10)
            .unwrap()
            .iter()
            .all(|asset| asset.state == "ready")
    );
    drop(visible);
    previews.clear_decoded_cache();
    assert_eq!(previews.decoded_live_bytes(), 0);
}

#[test]
fn virtual_rating_preserves_pending_pixels_and_master_cache() {
    use photocatalog::{catalog_edits::VariantKey, organization::Operation};
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("photo.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    let mut previews = service(
        &root.path().join("cache"),
        &originals,
        ServiceLimits::default(),
    );
    catalog
        .import_with_previews(&originals, None, |_| Ok(()), &mut previews)
        .unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    let master = VariantKey::master(&asset.id);
    let copy = catalog
        .create_edit_variant(&master, 0, "independent")
        .unwrap()
        .key;
    let before = previews
        .cached(&catalog, &asset.id, Tier::Thumbnail, false)
        .unwrap()
        .unwrap()
        .key;
    let consumer = previews
        .request_variant(&mut catalog, &copy, Tier::Thumbnail, Priority::Foreground)
        .unwrap();
    previews.tick(&mut catalog).unwrap();
    let identity = catalog.image_metadata_identity(&copy).unwrap();
    catalog
        .organize_image(
            &copy,
            identity.metadata_revision,
            Operation::Rating { value: 4 },
        )
        .unwrap();
    assert!(matches!(
        await_result(&mut previews, &mut catalog, consumer),
        ServiceCompletion::Ready
    ));
    let rendered = previews
        .cached_variant(&catalog, &copy, Tier::Thumbnail, false)
        .unwrap()
        .unwrap();
    assert!(!rendered.stale);
    assert_eq!(rendered.key.unwrap().variant_id, copy.variant_id);
    assert_eq!(
        previews
            .cached(&catalog, &asset.id, Tier::Thumbnail, false)
            .unwrap()
            .unwrap()
            .key,
        before
    );
}

#[test]
fn legacy_job_reopens_and_legacy_pixels_remain_exact_without_namespace_relabel() {
    let root = tempfile::tempdir().unwrap();
    let originals = root.path().join("originals");
    std::fs::create_dir(&originals).unwrap();
    image(&originals.join("photo.png"), [41, 87, 149]);
    let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
    catalog.import(&originals, None, |_| Ok(())).unwrap();
    let asset = catalog.browse(0, 10).unwrap().remove(0);
    // Model the exact schema7 migration baseline for a retained schema6 job.
    let fixture_db =
        rusqlite::Connection::open(root.path().join("catalog/catalog.sqlite3")).unwrap();
    fixture_db
        .execute_batch("UPDATE assets SET physical_generation=render_generation; UPDATE catalog_images SET pixel_generation=0")
        .unwrap();
    drop(fixture_db);
    let cache = root.path().join("cache");
    let mut previews = service(&cache, &originals, ServiceLimits::default());
    previews
        .request(
            &mut catalog,
            &asset.id,
            Tier::Thumbnail,
            Priority::Background,
        )
        .unwrap();
    let legacy = catalog.render_identity(&asset.id).unwrap();
    drop(previews);
    let db =
        rusqlite::Connection::open(configuration(&cache).manifest_root.join("previews.sqlite3"))
            .unwrap();
    let (old_id, raw): (String, String) = db
        .query_row("SELECT id,descriptor FROM render_jobs", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    let mut saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
    saved.as_object_mut().unwrap().remove("import_image");
    saved["expected"]["generation"] = legacy.generation.into();
    saved["edit"]["source"]["generation"] = legacy.generation.into();
    saved["edit"]
        .as_object_mut()
        .unwrap()
        .remove("image_identity");
    saved["request"]["keys"][0]
        .as_object_mut()
        .unwrap()
        .remove("image_pixel_generation");
    saved["request"]["keys"][0]["generation"] = legacy.generation.into();
    let keys: Vec<PreviewKey> = serde_json::from_value(saved["request"]["keys"].clone()).unwrap();
    let id = blake3::hash(&serde_json::to_vec(&keys).unwrap())
        .to_hex()
        .to_string();
    db.execute(
        "UPDATE render_jobs SET id=?1,descriptor=?2 WHERE id=?3",
        rusqlite::params![id, saved.to_string(), old_id],
    )
    .unwrap();
    db.execute(
        "UPDATE wanted SET desired=?1,generation=?2,image_pixel_generation=NULL",
        rusqlite::params![keys[0].digest().unwrap(), legacy.generation],
    )
    .unwrap();
    drop(db);
    let mut previews = service(&cache, &originals, ServiceLimits::default());
    let (_, consumers) = previews.resume(&mut catalog, 0, 10, false).unwrap();
    assert_eq!(consumers.len(), 1);
    assert!(matches!(
        await_result(&mut previews, &mut catalog, consumers[0]),
        ServiceCompletion::Ready
    ));
    let retained = previews
        .cached(&catalog, &asset.id, Tier::Thumbnail, false)
        .unwrap()
        .unwrap();
    assert!(!retained.stale);
    assert!(retained.key.unwrap().image_pixel_generation.is_none());
    let current = previews
        .request(
            &mut catalog,
            &asset.id,
            Tier::Thumbnail,
            Priority::Foreground,
        )
        .unwrap();
    let db =
        rusqlite::Connection::open(configuration(&cache).manifest_root.join("previews.sqlite3"))
            .unwrap();
    db.execute(
        "INSERT INTO render_jobs(id,descriptor,created) VALUES(?1,?2,(SELECT value+1 FROM counter WHERE id=1))",
        rusqlite::params![id, saved.to_string()],
    ).unwrap();
    drop(db);
    let (_, obsolete) = previews.resume(&mut catalog, 0, 10, false).unwrap();
    assert!(obsolete.is_empty());
    let db =
        rusqlite::Connection::open(configuration(&cache).manifest_root.join("previews.sqlite3"))
            .unwrap();
    let old_remaining: i64 = db
        .query_row("SELECT count(*) FROM render_jobs WHERE id=?1", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(old_remaining, 0);
    let fresh_remaining: i64 = db
        .query_row("SELECT count(*) FROM render_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fresh_remaining, 1);
    drop(db);
    assert!(matches!(
        await_result(&mut previews, &mut catalog, current),
        ServiceCompletion::Ready
    ));
    assert!(
        previews
            .cached(&catalog, &asset.id, Tier::Thumbnail, false)
            .unwrap()
            .unwrap()
            .key
            .unwrap()
            .image_pixel_generation
            .is_some()
    );
}

#[test]
fn legacy_virtual_pixel_changes_reject_cache_persisted_and_inflight_jobs() {
    use photocatalog::catalog_edits::VariantKey;
    for mode in ["cached", "persisted", "inflight", "rebound"] {
        let root = tempfile::tempdir().unwrap();
        let originals = root.path().join("originals");
        std::fs::create_dir(&originals).unwrap();
        image(&originals.join("photo.png"), [41, 87, 149]);
        let mut catalog = Catalog::open(root.path().join("catalog")).unwrap();
        catalog.import(&originals, None, |_| Ok(())).unwrap();
        let asset = catalog.browse(0, 10).unwrap().remove(0);
        let copy = catalog
            .create_edit_variant(&VariantKey::master(&asset.id), 0, "legacy")
            .unwrap()
            .key;
        assert_eq!(
            catalog
                .image_metadata_identity(&copy)
                .unwrap()
                .pixel_generation,
            0
        );
        // Model the exact schema7 migration baseline for a retained schema6 job.
        let fixture_db =
            rusqlite::Connection::open(root.path().join("catalog/catalog.sqlite3")).unwrap();
        fixture_db
            .execute_batch("UPDATE assets SET physical_generation=render_generation; UPDATE catalog_images SET pixel_generation=0")
            .unwrap();
        drop(fixture_db);
        let cache = root.path().join("cache");
        let mut previews = service(&cache, &originals, ServiceLimits::default());
        previews
            .request_variant(&mut catalog, &copy, Tier::Thumbnail, Priority::Foreground)
            .unwrap();
        drop(previews);
        // A genuine old descriptor has neither per-image scope nor import scope.
        let db = rusqlite::Connection::open(
            configuration(&cache).manifest_root.join("previews.sqlite3"),
        )
        .unwrap();
        let (old_id, raw): (String, String) = db
            .query_row("SELECT id,descriptor FROM render_jobs", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let legacy = catalog.render_identity(&asset.id).unwrap();
        let mut saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
        saved.as_object_mut().unwrap().remove("import_image");
        saved["expected"]["generation"] = legacy.generation.into();
        saved["edit"]["source"]["generation"] = legacy.generation.into();
        saved["edit"]
            .as_object_mut()
            .unwrap()
            .remove("image_identity");
        saved["request"]["keys"][0]
            .as_object_mut()
            .unwrap()
            .remove("image_pixel_generation");
        saved["request"]["keys"][0]["generation"] = legacy.generation.into();
        let keys: Vec<PreviewKey> =
            serde_json::from_value(saved["request"]["keys"].clone()).unwrap();
        let id = blake3::hash(&serde_json::to_vec(&keys).unwrap())
            .to_hex()
            .to_string();
        db.execute(
            "UPDATE render_jobs SET id=?1,descriptor=?2 WHERE id=?3",
            rusqlite::params![id, saved.to_string(), old_id],
        )
        .unwrap();
        db.execute(
            "UPDATE wanted SET desired=?1,generation=?2,image_pixel_generation=NULL",
            rusqlite::params![keys[0].digest().unwrap(), legacy.generation],
        )
        .unwrap();
        drop(db);
        let mut previews = service(&cache, &originals, ServiceLimits::default());
        let consumer = if mode != "persisted" {
            let (_, handles) = previews.resume(&mut catalog, 0, 10, false).unwrap();
            assert_eq!(handles.len(), 1);
            if mode == "cached" || mode == "rebound" {
                assert!(matches!(
                    await_result(&mut previews, &mut catalog, handles[0]),
                    ServiceCompletion::Ready
                ));
                assert!(
                    !previews
                        .cached_variant(&catalog, &copy, Tier::Thumbnail, false)
                        .unwrap()
                        .unwrap()
                        .stale
                );
            } else {
                previews.tick(&mut catalog).unwrap();
            }
            Some(handles[0])
        } else {
            None
        };
        let before = catalog.image_metadata_identity(&copy).unwrap();
        if mode == "rebound" {
            use photocatalog::{catalog_storage::RelinkScope, storage_volume::NativePath};
            let plan = catalog
                .begin_relink(RelinkScope::Asset {
                    asset_id: asset.id.clone(),
                    destinations: vec![NativePath::from_path(&originals.join("photo.png"))],
                })
                .unwrap();
            while catalog.relink_plan(&plan.id).unwrap().state == "preparing" {
                catalog.prepare_relink_batch(&plan.id, 1).unwrap();
            }
            catalog.apply_relink(&plan.id).unwrap();
            assert_eq!(
                catalog
                    .image_metadata_identity(&copy)
                    .unwrap()
                    .pixel_generation,
                before.pixel_generation
            );
            assert!(
                catalog
                    .image_metadata_identity(&copy)
                    .unwrap()
                    .physical_generation
                    > before.physical_generation
            );
        } else {
            catalog
                .edit_metadata_for_image(
                    &copy,
                    before.metadata_revision,
                    None,
                    &[xmp::Edit::Set {
                        namespace: "http://ns.adobe.com/tiff/1.0/".into(),
                        path: "Orientation".into(),
                        value: "6".into(),
                    }],
                )
                .unwrap();
            assert!(
                catalog
                    .image_metadata_identity(&copy)
                    .unwrap()
                    .pixel_generation
                    > before.pixel_generation
            );
        }
        assert_eq!(
            catalog.render_identity(&asset.id).unwrap().generation,
            legacy.generation
        );
        match mode {
            "persisted" => assert!(
                previews
                    .resume(&mut catalog, 0, 10, false)
                    .unwrap()
                    .1
                    .is_empty()
            ),
            "inflight" => assert!(matches!(
                await_result(&mut previews, &mut catalog, consumer.unwrap()),
                ServiceCompletion::Stale | ServiceCompletion::Canceled
            )),
            _ => {}
        }
        assert!(
            previews
                .cached_variant(&catalog, &copy, Tier::Thumbnail, false)
                .unwrap()
                .is_none(),
            "{mode}"
        );
        if let Some(view) = previews
            .cached_variant(&catalog, &copy, Tier::Thumbnail, true)
            .unwrap()
        {
            assert!(view.stale, "{mode}");
        }
    }
}
