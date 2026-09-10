//! Real child-process export, interruption and preview ownership using disposable
//! PNG originals. No user originals or native measurement fixtures are involved.
use photocatalog::{
    Catalog,
    catalog_edits::VariantKey,
    catalog_exports::{ExportTarget, MetadataSelection},
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
