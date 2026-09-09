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
    use photocatalog::storage_volume::NativePath;
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
    for asset in &assets {
        previews
            .request(&mut catalog, &asset.id, Tier::Large, Priority::Background)
            .unwrap();
    }
    drop(previews); // queued journals survive owner shutdown
    let db = rusqlite::Connection::open(catalog_path.join("catalog.sqlite3")).unwrap();
    let original: String = db
        .query_row(
            "SELECT native_path FROM storage_bindings WHERE asset_id=?1",
            [&assets[1].id],
            |r| r.get(0),
        )
        .unwrap();
    #[cfg(unix)]
    let foreign = NativePath::WindowsWide("C:\\unmapped\\photo.png".encode_utf16().collect());
    #[cfg(windows)]
    let foreign = NativePath::UnixBytes(b"/unmapped/photo.png".to_vec());
    db.execute(
        "UPDATE storage_bindings SET native_path=?1 WHERE asset_id=?2",
        rusqlite::params![serde_json::to_string(&foreign).unwrap(), assets[1].id],
    )
    .unwrap();
    let mut previews = service(&cache, &originals, ServiceLimits::default());
    assert!(previews.resume(&mut catalog, 0, 10, false).is_err());
    assert!(previews.is_drained());
    assert_eq!(previews.scheduler_usage().consumers, 0);
    assert_eq!(previews.jobs(0, 10).unwrap().len(), 2);
    db.execute(
        "UPDATE storage_bindings SET native_path=?1 WHERE asset_id=?2",
        rusqlite::params![original, assets[1].id],
    )
    .unwrap();
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
}
