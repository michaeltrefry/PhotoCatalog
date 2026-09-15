//! Real child-process export, interruption and preview ownership using disposable
//! PNG originals. No user originals or native measurement fixtures are involved.
use photocatalog::{
    Catalog,
    catalog_edits::VariantKey,
    catalog_exports::{ExportControl, ExportTarget, MetadataSelection},
    edit::{Recipe, RecipeV1, RenderLimits},
    export_service::{ExportEvent, ExportService, ExportServiceLimits},
    image_export::*,
    media::DecodeLimits,
    photo_render::PhotoRenderLimits,
    preview::*,
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

#[test]
fn detached_executor_requires_same_catalog_drained_permit_and_reaps_before_release()
-> anyhow::Result<()> {
    use std::sync::{Arc, atomic::Ordering, mpsc};
    for action in ["yield", "cancel", "drop", "drain"] {
        let root = tempfile::tempdir()?;
        let (mut catalog, key, mut previews, original) = setup(root.path())?;
        let bytes = std::fs::read(&original)?;
        let job = enqueue(
            &mut catalog,
            &key,
            &root.path().join("detached.png"),
            OutputFormat::Png {
                depth: IntegerDepth::Eight,
            },
        )?;
        let handle = catalog.relink_worker_handle()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        let id = job.clone();
        let (ready, opened) = mpsc::sync_channel(1);
        let (permit_send, permit_recv) = mpsc::sync_channel::<NativeLaunchPermit>(1);
        let (started, running) = mpsc::sync_channel(1);
        let (advance, advance_recv) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || -> anyhow::Result<()> {
            let mut selected = handle.open()?;
            let mut service = ExportService::open(&selected, &executable(), limits())?;
            // Open and read never execute recovery; this is the explicit action.
            assert!(
                service
                    .tick_detached(&mut selected, &id, &mut ExportControl::new(&signal))
                    .is_err()
            );
            assert!(
                service
                    .recover_cancellable(&mut selected, 32, &mut ExportControl::new(&signal))?
                    .complete
            );
            assert!(matches!(
                service.tick_detached(&mut selected, &id, &mut ExportControl::new(&signal))?,
                ExportEvent::WaitingForPreviews
            ));
            ready.send(())?;
            let permit = permit_recv.recv_timeout(Duration::from_secs(10))?;
            service.admit_native(&selected, permit)?;
            let ExportEvent::Started { pid, .. } =
                service.tick_detached(&mut selected, &id, &mut ExportControl::new(&signal))?
            else {
                anyhow::bail!("native worker did not start")
            };
            started.send(pid)?;
            advance_recv.recv_timeout(Duration::from_secs(10))?;
            if action == "drop" {
                drop(service);
                return Ok(());
            }
            if action == "drain" {
                service.drain_native()?;
                assert!(!service.is_active());
                assert_eq!(service.reserved_bytes(), 0);
                return Ok(());
            }
            let event = if action == "cancel" {
                service.tick_detached(&mut selected, &id, &mut ExportControl::new(&signal))?
            } else {
                service.yield_to_previews(&mut selected)?
            };
            assert!(matches!(
                event,
                ExportEvent::Failed { .. } | ExportEvent::Yielded { .. }
            ));
            assert_eq!(service.reserved_bytes(), 0);
            assert!(!service.is_active());
            Ok(())
        });
        opened.recv_timeout(Duration::from_secs(10))?;
        let pause = previews.pause_native_launches()?;
        assert!(previews.native_work_drained());
        let permit = previews.native_launch_permit(&catalog, pause)?;
        let duplicate = previews.pause_native_launches()?;
        assert!(previews.native_launch_permit(&catalog, duplicate).is_err());
        let wrong = Catalog::open(root.path().join("other-catalog"))?;
        let wrong_pause = previews.pause_native_launches()?;
        assert!(previews.native_launch_permit(&wrong, wrong_pause).is_err());
        permit_send
            .send(permit)
            .map_err(|_| anyhow::anyhow!("permit receiver stopped"))?;
        let pid = running.recv_timeout(Duration::from_secs(10))?;
        assert_eq!(catalog.photo_export_job(&job)?.state, "queued");
        assert_eq!(
            catalog.photo_export_items(&job, 0, 1)?[0].state,
            "rendering"
        );
        assert_eq!(catalog.edit_variant(&key)?.revision, 0);
        let duplicate = previews.pause_native_launches()?;
        assert!(previews.native_launch_permit(&catalog, duplicate).is_err());
        if action == "cancel" {
            cancel.store(true, Ordering::Release);
        }
        advance.send(())?;
        thread.join().unwrap()?;
        #[cfg(unix)]
        {
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        #[cfg(windows)]
        let _ = pid;
        let pause = previews.pause_native_launches()?;
        drop(previews.native_launch_permit(&catalog, pause)?);
        assert_eq!(
            catalog.photo_export_job(&job)?.state,
            if action == "cancel" {
                "canceled"
            } else {
                "queued"
            }
        );
        if ["drop", "drain"].contains(&action) {
            assert_eq!(
                catalog.photo_export_items(&job, 0, 1)?[0].state,
                "rendering"
            );
            // Reopen alone preserves evidence; only explicit recovery fences it.
            let mut service = ExportService::open(&catalog, &executable(), limits())?;
            assert_eq!(
                catalog.photo_export_items(&job, 0, 1)?[0].state,
                "rendering"
            );
            assert!(service.recover(&mut catalog, 32)?.complete);
            assert_eq!(catalog.photo_export_items(&job, 0, 1)?[0].state, "pending");
        }
        assert_eq!(std::fs::read(&original)?, bytes);
        assert!(!root.path().join("detached.png").exists());
    }
    Ok(())
}
fn limits() -> ExportServiceLimits {
    let render = RenderLimits {
        max_pixels: 1_000_000,
        max_allocation_bytes: 64 * 1024 * 1024,
        max_live_bytes: 128 * 1024 * 1024,
    };
    ExportServiceLimits {
        worker_bytes: 256 * 1024 * 1024,
        working_bytes: 256 * 1024 * 1024,
        render: PhotoRenderLimits {
            decode: DecodeLimits {
                max_encoded_bytes: 8 * 1024 * 1024,
                max_intermediate_pixels: 1_000_000,
                max_allocation_bytes: 64 * 1024 * 1024,
            },
            render,
            encode: EncodeLimits {
                render,
                ..Default::default()
            },
            max_encoded_extent: 8 * 1024 * 1024,
        },
    }
}
fn executable() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_photocatalog"))
}
fn setup(root: &Path) -> anyhow::Result<(Catalog, VariantKey, PreviewService, PathBuf)> {
    let originals = root.join("originals");
    std::fs::create_dir(&originals)?;
    let original = originals.join("photo.png");
    image::RgbImage::from_pixel(48, 32, image::Rgb([64u8, 90, 120])).save(&original)?;
    let mut catalog = Catalog::open(root.join("catalog"))?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let key = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
    let previews = PreviewService::open(
        StoreConfig {
            manifest_root: root.join("cache"),
            thumbnail_root: root.join("thumb"),
            large_root: root.join("large"),
            layout: Layout::Flat,
            thumbnail_bytes: 8 * 1024 * 1024,
            large_bytes: 8 * 1024 * 1024,
        },
        &[originals],
        executable(),
        PreviewPolicy::default(),
        ServiceLimits::default(),
    )?;
    Ok((catalog, key, previews, original))
}
fn enqueue(
    c: &mut Catalog,
    key: &VariantKey,
    path: &Path,
    format: OutputFormat,
) -> anyhow::Result<String> {
    let job = c.begin_photo_export()?;
    c.append_photo_export(
        &job.id,
        0,
        &ExportTarget {
            key: key.clone(),
            expected_revision: c.edit_variant(key)?.revision,
            destination: path.to_owned(),
            overwrite: false,
            metadata: MetadataSelection::Omit,
        },
        &OutputSpec {
            size: OutputSize::Original,
            format,
            profile: if matches!(
                format,
                OutputFormat::Tiff {
                    depth: TiffDepth::Float32
                }
            ) {
                OutputProfile::LinearSrgb
            } else {
                OutputProfile::Srgb
            },
            alpha: AlphaPolicy::Composite {
                linear_rgb: [1.; 3],
            },
        },
        8 * 1024 * 1024,
        8 * 1024 * 1024,
    )?;
    c.seal_photo_export_job(&job.id, 1)?;
    Ok(job.id)
}
fn finish(
    service: &mut ExportService,
    c: &mut Catalog,
    p: &mut PreviewService,
    id: &str,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        p.tick(c)?;
        let event = service.tick(c, p, id, &AtomicBool::new(false))?;
        assert!(!matches!(event, ExportEvent::Failed { .. }), "{event:?}");
        if c.photo_export_job(id)?.state == "complete" {
            break;
        }
        assert!(Instant::now() < deadline, "export timeout {event:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(service.reserved_bytes(), 0);
    Ok(())
}
#[test]
fn exact_batch_formats_publish_edited_originals_and_survive_reopen() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let (mut c, key, mut p, original) = setup(root.path())?;
    let original_bytes = std::fs::read(&original)?;
    c.save_edit_recipe(
        &key,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 1.0,
            ..Default::default()
        }),
    )?;
    let mut service = ExportService::open(&c, &executable(), limits())?;
    assert!(service.recover(&mut c, 32)?.complete);
    for (i, (format, extension)) in [
        (OutputFormat::Jpeg { quality: 92 }, "jpg"),
        (
            OutputFormat::Png {
                depth: IntegerDepth::Eight,
            },
            "png",
        ),
        (
            OutputFormat::Png {
                depth: IntegerDepth::Sixteen,
            },
            "png",
        ),
        (
            OutputFormat::Tiff {
                depth: TiffDepth::Eight,
            },
            "tif",
        ),
        (
            OutputFormat::Tiff {
                depth: TiffDepth::Sixteen,
            },
            "tif",
        ),
        (
            OutputFormat::Tiff {
                depth: TiffDepth::Float32,
            },
            "tif",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = root.path().join(format!("export-{i}.{extension}"));
        let job = enqueue(&mut c, &key, &path, format)?;
        finish(&mut service, &mut c, &mut p, &job)?;
        let image = image::open(&path)?;
        assert_eq!((image.width(), image.height()), (48, 32));
        if matches!(format, OutputFormat::Png { .. }) {
            assert!(image.to_rgb8().get_pixel(0, 0).0[0] > 75);
        }
        assert_eq!(c.photo_export_items(&job, 0, 1)?[0].state, "published");
        let metrics = service.take_completion_metrics().unwrap();
        assert_eq!(metrics.job, job);
        assert_ne!(metrics.worker_pid, std::process::id());
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(
            metrics
                .worker_peak_resident_bytes
                .is_some_and(|bytes| bytes > 0)
        );
        assert!(!metrics.worker_peak_method.is_empty());
        assert!(service.take_completion_metrics().is_none());
    }
    assert_eq!(std::fs::read(original)?, original_bytes);
    drop(service);
    drop(c);
    let c = Catalog::open(root.path().join("catalog"))?;
    assert_eq!(c.photo_export_jobs(0, 10)?.len(), 6);
    assert!(
        c.photo_export_jobs(0, 10)?
            .iter()
            .all(|j| j.state == "complete")
    );
    Ok(())
}
#[test]
fn foreground_preemption_reaps_before_preview_launch_and_retries_with_new_attempt()
-> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let (mut c, key, mut p, _) = setup(root.path())?;
    let job = enqueue(
        &mut c,
        &key,
        &root.path().join("output.png"),
        OutputFormat::Png {
            depth: IntegerDepth::Eight,
        },
    )?;
    let mut service = ExportService::open(&c, &executable(), limits())?;
    service.recover(&mut c, 32)?;
    assert!(matches!(
        service.tick(&mut c, &mut p, &job, &AtomicBool::new(false))?,
        ExportEvent::Started { .. }
    ));
    let old = c.photo_export_attempt(&job, 1)?.attempt;
    let consumer = p.request_interactive(&mut c, &key, Tier::Thumbnail, Priority::Foreground)?;
    p.tick(&mut c)?;
    assert!(p.native_work_drained());
    assert!(service.reserved_bytes() > 0);
    assert!(matches!(
        service.yield_to_previews(&mut c)?,
        ExportEvent::Yielded { .. }
    ));
    assert_eq!(service.reserved_bytes(), 0);
    p.tick(&mut c)?;
    assert!(!p.native_work_drained());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        p.tick(&mut c)?;
        if let Some(done) = p.take_completion(consumer) {
            assert!(matches!(done, ServiceCompletion::Ready));
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(
        service.tick(&mut c, &mut p, &job, &AtomicBool::new(false))?,
        ExportEvent::Started { .. }
    ));
    assert_ne!(c.photo_export_attempt(&job, 1)?.attempt, old);
    finish(&mut service, &mut c, &mut p, &job)?;
    Ok(())
}
#[test]
fn owner_drop_recovers_rendering_authority_and_canceled_batch_never_publishes() -> anyhow::Result<()>
{
    let root = tempfile::tempdir()?;
    let (mut c, key, mut p, _) = setup(root.path())?;
    let output = root.path().join("output.png");
    let job = enqueue(
        &mut c,
        &key,
        &output,
        OutputFormat::Png {
            depth: IntegerDepth::Eight,
        },
    )?;
    let mut service = ExportService::open(&c, &executable(), limits())?;
    service.recover(&mut c, 32)?;
    service.tick(&mut c, &mut p, &job, &AtomicBool::new(false))?;
    assert!(ExportService::open(&c, &executable(), limits()).is_err());
    drop(service);
    let mut service = ExportService::open(&c, &executable(), limits())?;
    let result = service.recover(&mut c, 32)?;
    assert!(result.complete);
    assert_eq!(result.fenced, 1);
    service.tick(&mut c, &mut p, &job, &AtomicBool::new(false))?;
    let event = service.tick(&mut c, &mut p, &job, &AtomicBool::new(true))?;
    assert!(matches!(event, ExportEvent::Failed { .. }));
    assert_eq!(c.photo_export_job(&job)?.state, "canceled");
    assert!(!output.exists());
    assert_eq!(service.reserved_bytes(), 0);
    Ok(())
}
#[test]
fn undo_aba_invalidates_active_export_before_any_destination_publication() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let (mut c, key, mut p, _) = setup(root.path())?;
    let first = c.save_edit_recipe(
        &key,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 1.0,
            ..Default::default()
        }),
    )?;
    let output = root.path().join("output.png");
    let job = enqueue(
        &mut c,
        &key,
        &output,
        OutputFormat::Png {
            depth: IntegerDepth::Eight,
        },
    )?;
    let mut service = ExportService::open(&c, &executable(), limits())?;
    service.recover(&mut c, 32)?;
    service.tick(&mut c, &mut p, &job, &AtomicBool::new(false))?;
    let second = c.save_edit_recipe(
        &key,
        first.revision,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 2.0,
            ..Default::default()
        }),
    )?;
    let undo = c.undo_edit(&key, second.revision)?;
    assert_eq!(undo.recipe_digest, first.recipe_digest);
    let event = service.tick(&mut c, &mut p, &job, &AtomicBool::new(false))?;
    assert!(matches!(event, ExportEvent::Failed { .. }));
    assert!(!output.exists());
    assert_eq!(service.reserved_bytes(), 0);
    Ok(())
}

#[test]
fn parent_sealing_rerenders_before_reusing_orphan() -> anyhow::Result<()> {
    for mode in ["resume", "canceled", "stale"] {
        let root = tempfile::tempdir()?;
        let (mut c, key, mut p, _) = setup(root.path())?;
        let path = root.path().join("output.png");
        let job = enqueue(
            &mut c,
            &key,
            &path,
            OutputFormat::Png {
                depth: IntegerDepth::Eight,
            },
        )?;
        let mut service = ExportService::open(&c, &executable(), limits())?;
        service.recover(&mut c, 32)?;
        let work = c.claim_photo_export(&job)?.unwrap();
        let (output, xmp) = c.photo_export_inputs(&work.plan)?;
        let mut worker = photocatalog::export_worker::ExportWorkerProcess::spawn(
            &executable(),
            &root.path().join("catalog/photo-export-workers"),
            work.clone(),
            &output,
            xmp.as_deref(),
            limits().render,
        )?;
        let recovery = path.parent().unwrap().canonicalize()?.join(format!(
            ".photocatalog-photo-export-{}",
            work.plan.destination.operation
        ));
        let deadline = Instant::now() + Duration::from_secs(30);
        let result = loop {
            if let Some(result) = worker.poll(&AtomicBool::new(false))? {
                break result;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        };
        // This is the crash boundary: the local parent sealed the child output,
        // but no catalog acceptance or destination publication happened.
        assert!(!path.exists());
        assert_eq!(result.sealed.recovery_directory(), recovery);
        assert!(recovery.is_dir());
        drop(worker);
        drop(service);
        if mode == "canceled" {
            c.cancel_photo_export_job(&job)?;
        }
        if mode == "stale" {
            c.save_edit_recipe(
                &key,
                0,
                &Recipe::V1(RecipeV1 {
                    exposure_ev: 2.0,
                    ..Default::default()
                }),
            )?;
        }
        let mut service = ExportService::open(&c, &executable(), limits())?;
        assert!(service.recover(&mut c, 32)?.complete);
        if mode == "resume" {
            finish(&mut service, &mut c, &mut p, &job)?;
            assert!(path.exists());
            assert_eq!(
                photocatalog::metadata_export::inspect_file_revision(&path, 8 * 1024 * 1024)?
                    .digest,
                result.sealed.payload.digest
            );
        } else {
            let deadline = Instant::now() + Duration::from_secs(30);
            while c.photo_export_job(&job)?.state == "queued" {
                service.tick(&mut c, &mut p, &job, &AtomicBool::new(false))?;
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(!path.exists());
            assert!(result.sealed.recovery_directory().is_dir());
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn native_catalog_staging_and_destination_paths_use_real_reaped_export_workers()
-> anyhow::Result<()> {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir()?;
    let root = temp
        .path()
        .join(std::ffi::OsString::from_vec(vec![b'c', 255]));
    if let Err(error) = std::fs::create_dir(&root) {
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(92) {
            assert!(!root.exists());
            eprintln!(
                "non-UTF filesystem probe rejected before export: EILSEQ92; byte-wire custody remains tested"
            );
            return Ok(());
        }
        return Err(error.into());
    }
    let (mut c, key, mut previews, original) = setup(&root)?;
    let original_bytes = std::fs::read(&original)?;
    let mut service = ExportService::open(&c, &executable(), limits())?;
    assert!(service.recover(&mut c, 32)?.complete);
    for destination in [
        temp.path().join("utf-destination.png"),
        root.join(std::ffi::OsString::from_vec(b"export\xff.png".to_vec())),
    ] {
        let job = enqueue(
            &mut c,
            &key,
            &destination,
            OutputFormat::Png {
                depth: IntegerDepth::Eight,
            },
        )?;
        finish(&mut service, &mut c, &mut previews, &job)?;
        let metrics = service.take_completion_metrics().unwrap();
        assert_ne!(metrics.worker_pid, std::process::id());
        assert_eq!(service.reserved_bytes(), 0);
        let image = image::open(&destination)?;
        assert_eq!((image.width(), image.height()), (48, 32));
        let item = &c.photo_export_items(&job, 0, 1)?[0];
        assert_eq!(item.state, "published");
        assert_eq!(item.receipt.as_ref().unwrap().destination, destination);
        let (plan, authority) = c.photo_export_plan(&job, 1)?;
        assert_eq!(plan.version, 3);
        assert_eq!(
            blake3::hash(plan.raw().as_bytes()).to_hex().as_str(),
            authority
        );
    }
    assert_eq!(std::fs::read(original)?, original_bytes);
    Ok(())
}
