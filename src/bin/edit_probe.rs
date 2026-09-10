//! S8 qualification observations, never a second implementation of editing.
//! A reviewed coordinator supplies exclusive requests and owns process deadlines.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use photocatalog::{
    Catalog,
    catalog_edits::VariantKey,
    catalog_exports::{ExportTarget, MetadataSelection},
    edit::{self, OriginalRequest, PreparedLinearInput, Recipe, RenderLimits, RenderPurpose},
    export_service::{ExportEvent, ExportService, ExportServiceLimits},
    image_export::{self, BoundedSeekWriter, EncodeLimits, OutputSpec, ResolvedExportMetadata},
    media::DecodeLimits,
    photo_render::PhotoRenderLimits,
    preview::{self, PreviewService, ServiceCompletion, ServiceLimits, Tier, Priority},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
struct Args {
    #[arg(long, conflicts_with = "fingerprint")] request: Option<PathBuf>,
    #[arg(long, conflicts_with = "request")] fingerprint: Option<PathBuf>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase { Correctness, Kernel, Full, FirstRaw, WarmService, Export, Support100mp }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    phase: Phase,
    fixture_id: String,
    source: PathBuf,
    /// Coordinator SHA256 and this full BLAKE3 both bind the owned source copy.
    source_blake3: String,
    source_sha256: String,
    width: u32,
    height: u32,
    operation: String,
    recipes: Vec<Recipe>,
    outputs: Vec<OutputSpec>,
    worker: PathBuf,
    output: PathBuf,
    decode: DecodeLimits,
    render: RenderLimits,
    encoded_extent: u64,
    #[serde(default)]
    metadata: ResolvedExportMetadata,
    warmups: usize,
    repetitions: usize,
}
fn exclusive(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}
fn stamp() -> Value {
    json!({"unix_ns":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos().to_string()})
}
fn fingerprint(path: &Path, limit: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.len() <= limit, "source size/type admission");
    let mut left = metadata.len();
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0; 65536];
    while left > 0 {
        let count = usize::try_from(left.min(buffer.len() as u64))?;
        file.read_exact(&mut buffer[..count])?;
        hash.update(&buffer[..count]);
        left -= count as u64;
    }
    ensure!(file.read(&mut buffer[..1])? == 0, "file grew while hashing");
    Ok(hash.finalize().to_hex().to_string())
}
fn surface(image: &photocatalog::media::RenderedImage, raw: Option<&Path>) -> Result<Value> {
    let mut hash = blake3::Hasher::new();
    let mut minimum = [f32::INFINITY; 4];
    let mut maximum = [f32::NEG_INFINITY; 4];
    let mut alpha = [0u64; 3];
    let mut nonfinite = 0u64;
    let mut file = raw.map(|path| OpenOptions::new().write(true).create_new(true).open(path)).transpose()?;
    let mut bytes = Vec::with_capacity(65536);
    for pixel in &image.pixels {
        alpha[if pixel[3] == 0.0 { 0 } else if pixel[3] == 1.0 { 2 } else { 1 }] += 1;
        for (channel, value) in pixel.iter().enumerate() {
            nonfinite += u64::from(!value.is_finite());
            minimum[channel] = minimum[channel].min(*value);
            maximum[channel] = maximum[channel].max(*value);
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        if bytes.len() == 65536 {
            hash.update(&bytes);
            if let Some(file) = &mut file { file.write_all(&bytes)?; }
            bytes.clear();
        }
    }
    hash.update(&bytes);
    if let Some(file) = &mut file { file.write_all(&bytes)?; file.sync_all()?; }
    ensure!(nonfinite == 0 && minimum[3] >= 0.0 && maximum[3] <= 1.0, "invalid edited pixel components");
    Ok(json!({"width":image.width,"height":image.height,"rgba_f32le_blake3":hash.finalize().to_hex().to_string(),
        "minimum":minimum,"maximum":maximum,"alpha_zero_partial_opaque":alpha,"nonfinite":nonfinite,
        "provenance":image.provenance,"raw":raw}))
}
fn rss() -> Value {
    #[cfg(unix)]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
            let scale = if cfg!(target_os = "macos") { 1 } else { 1024 };
            return json!({"status":"available","bytes":usage.ru_maxrss as u64 * scale,
                "scope":"whole probe including setup, hashing and verification; child RSS separately sampled"});
        }
    }
    json!({"status":"unavailable","bytes":null})
}
fn load_original(request: &Request, recipe: &Recipe) -> Result<PreparedLinearInput> {
    let validated = recipe.validate()?;
    let image = edit::decode_original(OriginalRequest {
        path: &request.source, expected_fingerprint: &request.source_blake3,
        white_balance: &validated.settings().white_balance,
    }, request.decode, &())?;
    ensure!((image.width(), image.height()) == (request.width, request.height), "independent dimensions disagree");
    Ok(image)
}
fn samples_path(request: &Request) -> Result<File> {
    Ok(OpenOptions::new().write(true).create_new(true).open(request.output.join("samples.jsonl"))?)
}
fn record(file: &mut File, value: Value) -> Result<()> {
    serde_json::to_writer(&mut *file, &value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}
fn pixels(request: &Request, samples: &mut File) -> Result<()> {
    // Keep only one WB basis live at a time. Pair preparation is outside kernel
    // clocks; source/proxy retention is therefore not hidden in a per-call RSS claim.
    for (recipe_index, recipe) in request.recipes.iter().enumerate() {
        let original = load_original(request, recipe)?;
        let input = if request.phase == Phase::Kernel {
            let proxy = edit::prepare_linear_proxy(&original, 1600, request.render, &())?;
            drop(original);
            proxy
        } else { original };
        let validated = recipe.validate()?;
        for iteration in 0..request.warmups + request.repetitions {
            let start_anchor = stamp();
            let start = Instant::now();
            let edited = edit::render_recipe(&input, &validated,
                if request.phase == Phase::Kernel { RenderPurpose::InteractiveProxy { longest_edge: 1600 } }
                else { RenderPurpose::ExportExact }, request.render, &())?;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            let finished = stamp();
            let raw = (matches!(request.phase, Phase::Correctness | Phase::Support100mp) && iteration == 0
                && (request.operation == "combined" || u64::from(request.width)*u64::from(request.height) <= 512*512))
                .then(|| request.output.join(format!("recipe-{recipe_index}.rgba.f32")));
            let proof = surface(edited.as_rendered(), raw.as_deref())?;
            let mut exports = Vec::new();
            if matches!(request.phase, Phase::Correctness | Phase::Support100mp) {
                for (index, output) in request.outputs.iter().enumerate() {
                    let path = request.output.join(format!("recipe-{recipe_index}-{iteration}-output-{index}.image"));
                    let file = OpenOptions::new().write(true).create_new(true).open(&path)?;
                    let mut sink = BoundedSeekWriter::new(file, request.encoded_extent)?;
                    let encoded = image_export::encode_export(&edited, output,
                        &request.metadata, &mut sink,
                        EncodeLimits { render: request.render, ..Default::default() }, &())?;
                    drop(sink);
                    exports.push(json!({"path":path,"report":encoded,
                        "blake3":fingerprint(&path, request.encoded_extent)?}));
                }
            }
            record(samples, json!({"recipe_index":recipe_index,"recipe_digest":validated.digest(),
                "iteration":iteration,"warmup":iteration<request.warmups,"started":start_anchor,
                "finished":finished,"elapsed_ms":elapsed_ms,"pixels":proof,"exports":exports}))?;
        }
    }
    Ok(())
}
fn service(request: &Request) -> Result<(Catalog, PreviewService, VariantKey)> {
    let root = &request.output;
    let mut previews = PreviewService::open(preview::StoreConfig {
        manifest_root: root.join("preview-manifest"), thumbnail_root: root.join("thumbnails"),
        large_root: root.join("large"), layout: preview::Layout::Flat,
        thumbnail_bytes: 64 * 1024 * 1024, large_bytes: 256 * 1024 * 1024,
    }, &[request.source.parent().context("source parent")?.to_owned()], request.worker.clone(),
        preview::PreviewPolicy::default(), ServiceLimits {
            workers: 1, working_bytes: request.render.max_live_bytes,
            per_worker_bytes: request.render.max_live_bytes, decode_limits: request.decode,
            prepared_cache_bytes: if request.phase == Phase::FirstRaw { 0 } else { 256 * 1024 * 1024 },
            prepared_cache_entries: if request.phase == Phase::FirstRaw { 0 } else { 16 },
            ..Default::default()
        })?;
    let mut catalog = Catalog::open(root.join("catalog"))?;
    // The coordinator's owned input directory contains exactly this one byte-
    // verified source copy. Never scan a user's original parent directory.
    let parent = request.source.parent().context("source parent")?;
    let entries = fs::read_dir(parent)?.take(2).collect::<std::io::Result<Vec<_>>>()?;
    ensure!(entries.len() == 1 && entries[0].path() == request.source, "isolated one-file input directory required");
    catalog.import_with_previews(parent, None, |_| Ok(()), &mut previews)?;
    let assets = catalog.browse(0, 2)?;
    ensure!(assets.len() == 1, "single source import required");
    Ok((catalog, previews, VariantKey::master(&assets[0].id)))
}
fn delivery(request: &Request, samples: &mut File) -> Result<()> {
    let (mut catalog, mut previews, variant) = service(request)?;
    let mut revision = 0;
    for iteration in 0..request.warmups + request.repetitions {
        let recipe = &request.recipes[iteration % request.recipes.len()];
        let saved = catalog.save_edit_recipe(&variant, revision, recipe)?;
        ensure!(saved.revision > revision, "delivery trial reused an unchanged recipe revision");
        revision = saved.revision;
        let started = stamp();
        let start = Instant::now();
        let ticket = previews.request_interactive(&mut catalog, &variant, Tier::Large, Priority::Foreground)?;
        loop {
            previews.tick(&mut catalog)?;
            if let Some(done) = previews.take_completion(ticket) {
                ensure!(matches!(done, ServiceCompletion::Ready), "preview failed: {done:?}");
                break;
            }
            ensure!(start.elapsed() < Duration::from_secs(180), "preview delivery deadline");
            std::thread::sleep(Duration::from_millis(1));
        }
        let view = previews.cached_interactive(&catalog, &variant, Tier::Large, false)?.context("current preview missing")?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        let key = view.key.context("variant result lacks key")?;
        ensure!(key.edit_revision == revision as u64 && key.variant_id == variant.variant_id, "stale delivery");
        record(samples, json!({"iteration":iteration,"warmup":iteration<request.warmups,
            "started":started,"finished":stamp(),"elapsed_ms":elapsed_ms,"key":key,
            "decoded_blake3":view.pixels.pixels().digest(),"record":view.record,
            "native_drained":previews.native_work_drained()}))?;
    }
    Ok(())
}
fn export(request: &Request, samples: &mut File) -> Result<()> {
    let (mut catalog, mut previews, variant) = service(request)?;
    let saved = catalog.save_edit_recipe(&variant, 0, &request.recipes[0])?;
    let mut exports = ExportService::open(&catalog, &request.worker, ExportServiceLimits {
        worker_bytes: request.render.max_live_bytes, working_bytes: request.render.max_live_bytes,
        render: PhotoRenderLimits { decode: request.decode, render: request.render,
            encode: EncodeLimits { render: request.render, ..Default::default() }, max_encoded_extent: request.encoded_extent },
    })?;
    ensure!(exports.recover(&mut catalog, 128)?.complete, "fresh export recovery incomplete");
    for iteration in 0..request.warmups + request.repetitions {
        let extension = match request.outputs[0].format {
            image_export::OutputFormat::Jpeg { .. } => "jpg",
            image_export::OutputFormat::Png { .. } => "png",
            image_export::OutputFormat::Tiff { .. } => "tiff",
        };
        let destination = request.output.join(format!("export-{iteration}.{extension}"));
        let started = stamp();
        let start = Instant::now();
        let job = catalog.begin_photo_export()?;
        catalog.append_photo_export(&job.id, 0, &ExportTarget {
            key: variant.clone(), expected_revision: saved.revision, destination: destination.clone(),
            overwrite: false, metadata: MetadataSelection::Omit,
        }, &request.outputs[0], request.decode.max_encoded_bytes, request.encoded_extent)?;
        catalog.seal_photo_export_job(&job.id, 1)?;
        let mut events = Vec::new();
        loop {
            let event = exports.tick(&mut catalog, &mut previews, &job.id, &AtomicBool::new(false))?;
            let done = matches!(event, ExportEvent::Published { .. });
            ensure!(!matches!(event, ExportEvent::Failed { .. } | ExportEvent::Yielded { .. }), "export failed: {event:?}");
            if !matches!(event, ExportEvent::Rendering { .. }) {
                events.push(json!({"elapsed_ms":start.elapsed().as_secs_f64()*1000.0,"event":event}));
            }
            if done { break; }
            ensure!(start.elapsed() < Duration::from_secs(180), "export deadline");
            std::thread::sleep(Duration::from_millis(1));
        }
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        ensure!(!exports.is_active() && exports.reserved_bytes() == 0, "export ownership not released");
        record(samples, json!({"iteration":iteration,"warmup":iteration<request.warmups,
            "started":started,"finished":stamp(),"elapsed_ms":elapsed_ms,"events":events,
            "path":destination,"blake3":fingerprint(&destination,request.encoded_extent)?,
            "job":catalog.photo_export_job(&job.id)?,"items":catalog.photo_export_items(&job.id,0,10)?,
            "internal_render_encode_seal_timings":"unavailable; no phase estimates"}))?;
    }
    Ok(())
}
fn run(request: &Request, samples: &mut File) -> Result<()> {
    ensure!(request.version == 1 && request.source.is_absolute() && request.output.is_absolute()
        && request.worker.is_absolute(), "probe request version/paths");
    ensure!(!request.recipes.is_empty() && request.recipes.len() <= 32 && request.outputs.len() <= 32, "matrix bound");
    let expected = match request.phase {
        Phase::Kernel | Phase::WarmService => (2, 100),
        Phase::Full | Phase::FirstRaw | Phase::Export => (2, 20),
        Phase::Correctness | Phase::Support100mp => (0, 1),
    };
    ensure!((request.warmups, request.repetitions) == expected, "frozen sample counts changed");
    if request.phase == Phase::Export { ensure!(request.recipes.len() == 1 && request.outputs.len() == 1, "export single configuration"); }
    if matches!(request.phase, Phase::WarmService | Phase::FirstRaw) { ensure!(request.recipes.len() == 2 && request.recipes[0] != request.recipes[1], "delivery requires alternating recipes"); }
    let pixel_count = u64::from(request.width) * u64::from(request.height);
    ensure!(pixel_count <= if request.phase == Phase::Support100mp { 100_000_000 } else { 32_000_000 }, "source pixel admission");
    ensure!(fingerprint(&request.source, request.decode.max_encoded_bytes)? == request.source_blake3, "source before mismatch");
    match request.phase {
        Phase::Correctness | Phase::Kernel | Phase::Full | Phase::Support100mp => pixels(request, samples)?,
        Phase::FirstRaw | Phase::WarmService => delivery(request, samples)?,
        Phase::Export => export(request, samples)?,
    }
    ensure!(fingerprint(&request.source, request.decode.max_encoded_bytes)? == request.source_blake3, "source after mismatch");
    Ok(())
}
fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(path) = args.fingerprint {
        println!("{}", json!({"blake3":fingerprint(&path, 2*1024*1024*1024)?}));
        return Ok(());
    }
    let path = args.request.context("--request or --fingerprint required")?;
    ensure!(fs::metadata(&path)?.len() <= 256 * 1024, "request byte bound");
    let request: Request = serde_json::from_slice(&fs::read(&path)?)?;
    fs::create_dir(&request.output)?;
    exclusive(&request.output.join("request.json"), &request)?;
    let started = stamp();
    let mut samples = samples_path(&request)?;
    let result = run(&request, &mut samples);
    samples.sync_all()?;
    exclusive(&request.output.join("receipt.json"), &json!({"version":1,"started":started,"finished":stamp(),
        "probe_complete":result.is_ok(),"qualification_complete":false,
        "independent_oracles":"pending separate verifier","error":result.as_ref().err().map(|e|format!("{e:#}")),
        "source":request.source,"source_sha256":request.source_sha256,"source_blake3":request.source_blake3,
        "phase":request.phase,"fixture_id":request.fixture_id,"operation":request.operation,
        "rss":rss(),"renderer":edit::renderer_identity(),
        "export_renderer":photocatalog::photo_render::output_renderer_identity(),
        "probe_source_blake3":blake3::hash(include_bytes!("edit_probe.rs")).to_hex().to_string()}))?;
    result
}
