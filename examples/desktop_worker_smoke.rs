//! Install-payload proof: real service requests use the supplied desktop worker
//! executable against temporary synthetic PNG pixels. No user originals.
use anyhow::{Result, ensure};
use clap::Parser;
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
    fs,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    worker_executable: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.worker_executable.is_absolute(),
        "absolute installed executable required"
    );
    let executable = args.worker_executable.canonicalize()?;
    let executable_hash = blake3::hash(&fs::read(&executable)?).to_hex().to_string();
    let root = tempfile::Builder::new()
        .prefix("photocatalog-installed-worker-")
        .tempdir_in(std::env::temp_dir().canonicalize()?)?;
    let originals = root.path().join("originals 雪");
    fs::create_dir(&originals)?;
    let original = originals.join("synthetic.png");
    image::RgbImage::from_pixel(48, 32, image::Rgb([64u8, 90, 120])).save(&original)?;
    let original_hash = blake3::hash(&fs::read(&original)?);
    let mut catalog = Catalog::open(root.path().join("catalog"))?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let key = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
    catalog.save_edit_recipe(
        &key,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 1.0,
            ..Default::default()
        }),
    )?;
    let mut previews = PreviewService::open(
        StoreConfig {
            manifest_root: root.path().join("cache"),
            thumbnail_root: root.path().join("thumb"),
            large_root: root.path().join("large"),
            layout: Layout::Flat,
            thumbnail_bytes: 8 * 1024 * 1024,
            large_bytes: 8 * 1024 * 1024,
        },
        &[originals],
        executable.clone(),
        PreviewPolicy::default(),
        ServiceLimits::default(),
    )?;
    let ticket =
        previews.request_variant(&mut catalog, &key, Tier::Thumbnail, Priority::Foreground)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        previews.tick(&mut catalog)?;
        if let Some(completion) = previews.take_completion(ticket) {
            ensure!(
                matches!(completion, ServiceCompletion::Ready),
                "preview: {completion:?}"
            );
            break;
        }
        ensure!(Instant::now() < deadline, "installed preview timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
    let preview = previews
        .cached_variant(&catalog, &key, Tier::Thumbnail, false)?
        .ok_or_else(|| anyhow::anyhow!("missing generated preview"))?;
    ensure!(
        !preview.pixels.pixels().pixels().is_empty(),
        "empty generated pixels"
    );
    let preview_metrics = previews
        .take_worker_metrics()
        .ok_or_else(|| anyhow::anyhow!("no real preview child metrics"))?;
    ensure!(
        preview_metrics.pid != std::process::id() && previews.native_work_drained(),
        "preview child not drained"
    );
    let render = RenderLimits {
        max_pixels: 1_000_000,
        max_allocation_bytes: 64 * 1024 * 1024,
        max_live_bytes: 128 * 1024 * 1024,
    };
    let limits = ExportServiceLimits {
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
    };
    let output = root.path().join("edited-export.png");
    let job = catalog.begin_photo_export()?;
    catalog.append_photo_export(
        &job.id,
        0,
        &ExportTarget {
            key: key.clone(),
            expected_revision: catalog.edit_variant(&key)?.revision,
            destination: output.clone(),
            overwrite: false,
            metadata: MetadataSelection::Omit,
        },
        &OutputSpec {
            size: OutputSize::Original,
            format: OutputFormat::Png {
                depth: IntegerDepth::Eight,
            },
            profile: OutputProfile::Srgb,
            alpha: AlphaPolicy::Composite {
                linear_rgb: [1.; 3],
            },
        },
        8 * 1024 * 1024,
        8 * 1024 * 1024,
    )?;
    catalog.seal_photo_export_job(&job.id, 1)?;
    let mut exports = ExportService::open(&catalog, &executable, limits)?;
    ensure!(
        exports.recover(&mut catalog, 32)?.complete,
        "export recovery not complete"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        previews.tick(&mut catalog)?;
        let event = exports.tick(
            &mut catalog,
            &mut previews,
            &job.id,
            &AtomicBool::new(false),
        )?;
        ensure!(
            !matches!(event, ExportEvent::Failed { .. }),
            "export: {event:?}"
        );
        if catalog.photo_export_job(&job.id)?.state == "complete" {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "installed export timeout: {event:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let pixels = image::open(&output)?.to_rgb8();
    ensure!(
        pixels.dimensions() == (48, 32) && pixels.get_pixel(0, 0).0[0] > 75,
        "export did not preserve dimensions/apply native exposure"
    );
    ensure!(
        catalog.photo_export_items(&job.id, 0, 1)?[0].state == "published",
        "export not published"
    );
    let metrics = exports
        .take_completion_metrics()
        .ok_or_else(|| anyhow::anyhow!("no export child metrics"))?;
    ensure!(
        metrics.worker_pid != std::process::id() && exports.reserved_bytes() == 0,
        "export child not drained"
    );
    ensure!(
        blake3::hash(&fs::read(&original)?) == original_hash,
        "synthetic original changed"
    );
    ensure!(
        blake3::hash(&fs::read(&executable)?).to_hex().as_str() == executable_hash,
        "installed executable changed"
    );
    drop(exports);
    drop(previews);
    drop(catalog);
    root.close()?;
    println!(
        "{}",
        serde_json::json!({"status":"PASS_INSTALLED_PREVIEW_AND_EXPORT_WORKERS",
        "worker_executable":executable, "worker_blake3":executable_hash,
        "preview_pid":preview_metrics.pid, "export_pid":metrics.worker_pid,
        "temporary_state_removed":true, "gui_tested":false})
    );
    Ok(())
}
