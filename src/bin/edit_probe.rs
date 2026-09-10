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
    preview::{self, PreviewService, Priority, ServiceCompletion, ServiceLimits, Tier},
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
    #[arg(long, conflicts_with = "fingerprint")]
    request: Option<PathBuf>,
    #[arg(long, conflicts_with = "request")]
    fingerprint: Option<PathBuf>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Correctness,
    Kernel,
    Full,
    FirstRaw,
    WarmService,
    Export,
    Support100mp,
    Refusal,
    ProxyReference,
    ExportCorrectness,
    OverlapImport,
    OverlapExport,
}
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
    #[serde(default)]
    resolve_embedded: bool,
    #[serde(default)]
    background_source: Option<PathBuf>,
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
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "reparse input refused"
        );
    }
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "source size/type admission"
    );
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
    let mut file = raw
        .map(|path| OpenOptions::new().write(true).create_new(true).open(path))
        .transpose()?;
    let mut bytes = Vec::with_capacity(65536);
    for pixel in &image.pixels {
        alpha[if pixel[3] == 0.0 {
            0
        } else if pixel[3] == 1.0 {
            2
        } else {
            1
        }] += 1;
        for (channel, value) in pixel.iter().enumerate() {
            nonfinite += u64::from(!value.is_finite());
            minimum[channel] = minimum[channel].min(*value);
            maximum[channel] = maximum[channel].max(*value);
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        if bytes.len() == 65536 {
            hash.update(&bytes);
            if let Some(file) = &mut file {
                file.write_all(&bytes)?;
            }
            bytes.clear();
        }
    }
    hash.update(&bytes);
    if let Some(file) = &mut file {
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    ensure!(
        nonfinite == 0 && minimum[3] >= 0.0 && maximum[3] <= 1.0,
        "invalid edited pixel components"
    );
    Ok(
        json!({"width":image.width,"height":image.height,"rgba_f32le_blake3":hash.finalize().to_hex().to_string(),
        "minimum":minimum,"maximum":maximum,"alpha_zero_partial_opaque":alpha,"nonfinite":nonfinite,
        "provenance":image.provenance,"raw":raw}),
    )
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
    let image = edit::decode_original(
        OriginalRequest {
            path: &request.source,
            expected_fingerprint: &request.source_blake3,
            white_balance: &validated.settings().white_balance,
        },
        request.decode,
        &(),
    )?;
    ensure!(
        (image.width(), image.height()) == (request.width, request.height),
        "independent dimensions disagree"
    );
    Ok(image)
}
fn samples_path(request: &Request) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(request.output.join("samples.jsonl"))?)
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
        let input = if matches!(request.phase, Phase::Kernel | Phase::ProxyReference) {
            let proxy = edit::prepare_linear_proxy(&original, 1600, request.render, &())?;
            drop(original);
            proxy
        } else {
            original
        };
        let validated = recipe.validate()?;
        for iteration in 0..request.warmups + request.repetitions {
            record(
                samples,
                json!({"kind":"attempt","recipe_index":recipe_index,"iteration":iteration,"started":stamp()}),
            )?;
            let start_anchor = stamp();
            let start = Instant::now();
            let edited = edit::render_recipe(
                &input,
                &validated,
                if matches!(request.phase, Phase::Kernel | Phase::ProxyReference) {
                    RenderPurpose::InteractiveProxy { longest_edge: 1600 }
                } else {
                    RenderPurpose::ExportExact
                },
                request.render,
                &(),
            )?;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            let finished = stamp();
            let raw = (iteration == 0
                && (request.phase == Phase::ProxyReference
                    || (matches!(request.phase, Phase::Correctness | Phase::Support100mp)
                        && (request.operation == "combined"
                            || u64::from(request.width) * u64::from(request.height)
                                <= 512 * 512))))
                .then(|| {
                    request
                        .output
                        .join(format!("recipe-{recipe_index}.rgba.f32"))
                });
            let proof = surface(edited.as_rendered(), raw.as_deref())?;
            let preview_reference = if request.phase == Phase::ProxyReference {
                let rgb = preview::prepare(edited.as_rendered(), 1600)?;
                let bytes = preview::encode(
                    &rgb,
                    preview::CodecSettings {
                        codec: preview::Codec::Jpeg,
                        quality: 80,
                    },
                    None,
                )?;
                let path = request.output.join(format!("recipe-{recipe_index}.jpg"));
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                Some(
                    json!({"path":path,"blake3":blake3::hash(&bytes).to_hex().to_string(),
                    "prepared_rgb_blake3":rgb.digest(),"width":rgb.width(),"height":rgb.height()}),
                )
            } else {
                None
            };
            let mut exports = Vec::new();
            if matches!(request.phase, Phase::Correctness | Phase::Support100mp) {
                for (index, output) in request.outputs.iter().enumerate() {
                    let path = request.output.join(format!(
                        "recipe-{recipe_index}-{iteration}-output-{index}.image"
                    ));
                    let file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)?;
                    let mut sink = BoundedSeekWriter::new(file, request.encoded_extent)?;
                    let encoded = image_export::encode_export(
                        &edited,
                        output,
                        &request.metadata,
                        &mut sink,
                        EncodeLimits {
                            render: request.render,
                            ..Default::default()
                        },
                        &(),
                    )?;
                    drop(sink);
                    exports.push(json!({"path":path,"report":encoded,
                        "blake3":fingerprint(&path, request.encoded_extent)?}));
                }
            }
            record(
                samples,
                json!({"kind":"observation","recipe_index":recipe_index,"recipe_digest":validated.digest(),
                "iteration":iteration,"warmup":iteration<request.warmups,"started":start_anchor,
                "finished":finished,"elapsed_ms":elapsed_ms,"pixels":proof,"exports":exports,"preview_reference":preview_reference}),
            )?;
        }
    }
    Ok(())
}
fn service(request: &Request) -> Result<(Catalog, PreviewService, VariantKey)> {
    let root = &request.output;
    let mut previews = PreviewService::open(
        preview::StoreConfig {
            manifest_root: root.join("preview-manifest"),
            thumbnail_root: root.join("thumbnails"),
            large_root: root.join("large"),
            layout: preview::Layout::Flat,
            thumbnail_bytes: 64 * 1024 * 1024,
            large_bytes: 256 * 1024 * 1024,
        },
        &[request.source.parent().context("source parent")?.to_owned()],
        request.worker.clone(),
        preview::PreviewPolicy::default(),
        ServiceLimits {
            workers: 1,
            working_bytes: request.render.max_live_bytes,
            per_worker_bytes: request.render.max_live_bytes,
            decode_limits: request.decode,
            prepared_cache_bytes: if request.phase == Phase::FirstRaw {
                0
            } else {
                256 * 1024 * 1024
            },
            prepared_cache_entries: if request.phase == Phase::FirstRaw {
                0
            } else {
                16
            },
            ..Default::default()
        },
    )?;
    let mut catalog = Catalog::open(root.join("catalog"))?;
    // The coordinator's owned input directory contains exactly this one byte-
    // verified source copy. Never scan a user's original parent directory.
    let parent = request.source.parent().context("source parent")?;
    let entries = fs::read_dir(parent)?
        .take(2)
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() == 1 && entries[0].path() == request.source,
        "isolated one-file input directory required"
    );
    catalog.import_with_previews(parent, None, |_| Ok(()), &mut previews)?;
    let assets = catalog.browse(0, 2)?;
    ensure!(assets.len() == 1, "single source import required");
    Ok((catalog, previews, VariantKey::master(&assets[0].id)))
}
fn delivery(request: &Request, samples: &mut File) -> Result<()> {
    let (mut catalog, mut previews, variant) = service(request)?;
    let mut revision = 0;
    let mut prepared_digests: Vec<Option<String>> = vec![None; request.recipes.len()];
    for iteration in 0..request.warmups + request.repetitions {
        let recipe = &request.recipes[iteration % request.recipes.len()];
        let saved = catalog.save_edit_recipe(&variant, revision, recipe)?;
        ensure!(
            saved.revision > revision,
            "delivery trial reused an unchanged recipe revision"
        );
        revision = saved.revision;
        record(
            samples,
            json!({"kind":"attempt","iteration":iteration,"started":stamp()}),
        )?;
        let started = stamp();
        let start = Instant::now();
        let ticket = previews.request_interactive(
            &mut catalog,
            &variant,
            Tier::Large,
            Priority::Foreground,
        )?;
        let mut observed_workers = std::collections::BTreeSet::new();
        loop {
            observed_workers.extend(previews.active_worker_pids());
            previews.tick(&mut catalog)?;
            observed_workers.extend(previews.active_worker_pids());
            if let Some(done) = previews.take_completion(ticket) {
                ensure!(
                    matches!(done, ServiceCompletion::Ready),
                    "preview failed: {done:?}"
                );
                break;
            }
            ensure!(
                start.elapsed() < Duration::from_secs(180),
                "preview delivery deadline"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        ensure!(
            !observed_workers.is_empty(),
            "delivery did not observe a new worker lifecycle"
        );
        let view = previews
            .cached_interactive(&catalog, &variant, Tier::Large, false)?
            .context("current preview missing")?;
        let render_record = view
            .record
            .as_ref()
            .context("edited result lacks render record")?;
        if request.phase == Phase::WarmService && iteration >= request.warmups {
            let preview::EditInputProvenance::PreparedProxy {
                receipt,
                source_instance_digest,
            } = render_record
                .edit_input
                .as_ref()
                .context("typed prepared source absent")?
            else {
                anyhow::bail!("warm sample decoded original rather than reusing prepared input");
            };
            let expected_recipe = recipe.validate()?;
            ensure!(
                receipt.identity.source_fingerprint == request.source_blake3
                    && receipt.identity.white_balance == expected_recipe.settings().white_balance
                    && receipt.identity.renderer_identity == edit::renderer_identity()
                    && receipt.identity.original_dimensions == (request.width, request.height)
                    && receipt.identity.longest_edge == 1600
                    && receipt.blake3.len() == 64
                    && source_instance_digest.len() == 64,
                "consumed prepared identity mismatch"
            );
            let remembered = &mut prepared_digests[iteration % request.recipes.len()];
            if let Some(expected) = remembered {
                ensure!(*expected == receipt.blake3, "warm prepared bytes changed");
            } else {
                *remembered = Some(receipt.blake3.clone());
            }
        }
        if request.phase == Phase::FirstRaw {
            ensure!(
                matches!(
                    render_record.edit_input,
                    Some(preview::EditInputProvenance::OriginalDecoded)
                ),
                "first RAW trial reused prepared input"
            );
        }
        let key = view.key.context("variant result lacks key")?;
        ensure!(
            key.edit_revision == revision as u64 && key.variant_id == variant.variant_id,
            "stale delivery"
        );
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        let finished = stamp();
        // Artifact reads/copies are after the completed delivery timer. Every
        // trial is hashed; each alternating basis retains one complete JPEG.
        let encoded = previews
            .encoded_cached_variant(&catalog, &variant, Tier::Large, false, true)?
            .context("current encoded preview missing")?;
        let encoded_hash = blake3::hash(encoded.bytes()).to_hex().to_string();
        let retained = if iteration < request.recipes.len() {
            let path = request
                .output
                .join(format!("delivery-basis-{iteration}.jpg"));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(encoded.bytes())?;
            file.sync_all()?;
            Some(path)
        } else {
            None
        };
        drop(encoded);
        record(
            samples,
            json!({"kind":"observation","iteration":iteration,"warmup":iteration<request.warmups,
            "started":started,"finished":finished,"elapsed_ms":elapsed_ms,"key":key,
            "encoded_blake3":encoded_hash,"retained_encoded":retained,
            "decoded_blake3":view.pixels.pixels().digest(),"record":view.record,"observed_worker_pids":observed_workers,
            "native_drained":previews.native_work_drained()}),
        )?;
    }
    Ok(())
}
fn export(request: &Request, samples: &mut File) -> Result<()> {
    let (mut catalog, mut previews, variant) = service(request)?;
    let saved = catalog.save_edit_recipe(&variant, 0, &request.recipes[0])?;
    let metadata_selection = if request.resolve_embedded {
        ensure!(
            request.fixture_id == "analytic-metadata",
            "resolved fixture must be explicitly controlled"
        );
        let metadata = catalog.metadata(&variant.asset_id)?;
        let models: Vec<_> = catalog
            .metadata_history(&variant.asset_id, 0, 100)?
            .into_iter()
            .filter(|observation| observation.current)
            .flat_map(|observation| observation.models)
            .collect();
        ensure!(
            models.len() == 1 && models[0].error.is_none(),
            "controlled source must have exactly one valid full base"
        );
        MetadataSelection::Resolved {
            expected_revision: metadata.revision,
            base_model: Some(models[0].id),
        }
    } else {
        MetadataSelection::Omit
    };
    let mut exports = ExportService::open(
        &catalog,
        &request.worker,
        ExportServiceLimits {
            worker_bytes: request.render.max_live_bytes,
            working_bytes: request.render.max_live_bytes,
            render: PhotoRenderLimits {
                decode: request.decode,
                render: request.render,
                encode: EncodeLimits {
                    render: request.render,
                    ..Default::default()
                },
                max_encoded_extent: request.encoded_extent,
            },
        },
    )?;
    ensure!(
        exports.recover(&mut catalog, 128)?.complete,
        "fresh export recovery incomplete"
    );
    for iteration in 0..request.warmups + request.repetitions {
        let extension = match request.outputs[0].format {
            image_export::OutputFormat::Jpeg { .. } => "jpg",
            image_export::OutputFormat::Png { .. } => "png",
            image_export::OutputFormat::Tiff { .. } => "tiff",
        };
        let destination = request
            .output
            .join(format!("export-{iteration}.{extension}"));
        record(
            samples,
            json!({"kind":"attempt","iteration":iteration,"started":stamp()}),
        )?;
        let started = stamp();
        let start = Instant::now();
        let job = catalog.begin_photo_export()?;
        catalog.append_photo_export(
            &job.id,
            0,
            &ExportTarget {
                key: variant.clone(),
                expected_revision: saved.revision,
                destination: destination.clone(),
                overwrite: false,
                metadata: metadata_selection.clone(),
            },
            &request.outputs[0],
            request.decode.max_encoded_bytes,
            request.encoded_extent,
        )?;
        catalog.seal_photo_export_job(&job.id, 1)?;
        let mut events = Vec::new();
        loop {
            let event = exports.tick(
                &mut catalog,
                &mut previews,
                &job.id,
                &AtomicBool::new(false),
            )?;
            let done = matches!(event, ExportEvent::Published { .. });
            ensure!(
                !matches!(
                    event,
                    ExportEvent::Failed { .. } | ExportEvent::Yielded { .. }
                ),
                "export failed: {event:?}"
            );
            if !matches!(event, ExportEvent::Rendering { .. }) {
                events
                    .push(json!({"elapsed_ms":start.elapsed().as_secs_f64()*1000.0,"event":event}));
            }
            if done {
                break;
            }
            ensure!(
                start.elapsed() < Duration::from_secs(180),
                "export deadline"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        let finished = stamp();
        ensure!(
            !exports.is_active() && exports.reserved_bytes() == 0,
            "export ownership not released"
        );
        let phases = exports
            .take_completion_metrics()
            .context("published export lacks actual phase metrics")?;
        ensure!(
            phases.job == job.id && phases.sequence == 1 && phases.publication.is_some(),
            "export phase authority mismatch"
        );
        record(
            samples,
            json!({"kind":"observation","iteration":iteration,"warmup":iteration<request.warmups,
            "started":started,"finished":finished,"elapsed_ms":elapsed_ms,"events":events,"phases":phases,
            "path":destination,"blake3":fingerprint(&destination,request.encoded_extent)?,
            "job":catalog.photo_export_job(&job.id)?,"items":catalog.photo_export_items(&job.id,0,10)?}),
        )?;
    }
    Ok(())
}
#[derive(Default, Clone)]
struct OverlapState {
    pids: Vec<u32>,
    done: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LiveWorkerIdentity {
    pid: u32,
    parent_pid: u32,
    start_seconds: u64,
    start_microseconds: u64,
}

// The fixed timing host is macOS. Read kernel identity immediately on each side
// of a durable save; a stale actor-owned PID or a reaped zombie cannot count.
#[cfg(target_os = "macos")]
fn live_worker_identity(pid: u32) -> Result<LiveWorkerIdentity> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: the destination has exactly the ABI type/extent passed to libproc.
    // No value is read unless libproc reports the complete initialized structure.
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid)?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            i32::try_from(size)?,
        )
    };
    ensure!(
        usize::try_from(read).ok() == Some(size),
        "worker identity unavailable or process exited"
    );
    // SAFETY: complete structure length was checked above.
    let info = unsafe { info.assume_init() };
    ensure!(
        info.pbi_pid == pid
            && info.pbi_ppid == std::process::id()
            && info.pbi_status != libc::SZOMB
            && info.pbi_start_tvusec < 1_000_000,
        "worker is no longer a live owned process"
    );
    Ok(LiveWorkerIdentity {
        pid,
        parent_pid: info.pbi_ppid,
        start_seconds: info.pbi_start_tvsec,
        start_microseconds: info.pbi_start_tvusec,
    })
}

#[cfg(not(target_os = "macos"))]
fn live_worker_identity(_pid: u32) -> Result<LiveWorkerIdentity> {
    anyhow::bail!("fixed Mac overlap qualification requires libproc live identities")
}

fn live_worker_identities(pids: &[u32]) -> Result<Vec<LiveWorkerIdentity>> {
    ensure!(
        !pids.is_empty() && pids.len() <= 1,
        "one normal worker required"
    );
    pids.iter().copied().map(live_worker_identity).collect()
}

fn overlap(request: &Request, samples: &mut File) -> Result<()> {
    use std::sync::{Mutex, atomic::Ordering};
    let (mut catalog, mut previews, master) = service(request)?;
    let foreground = catalog.create_edit_variant(&master, 0, "foreground qualification")?;
    let catalog_root = request.output.join("catalog");
    let state = Mutex::new(OverlapState::default());
    let stop = AtomicBool::new(false);
    let background_start = stamp();
    std::thread::scope(|scope| -> Result<()> {
        let background = scope.spawn(|| -> Result<Vec<Value>> {
            let mut background_catalog = Catalog::open(&catalog_root)?;
            let mut events = Vec::new();
            let start = Instant::now();
            if request.phase == Phase::OverlapImport {
                let source = request.background_source.as_ref().context("owned background source directory required")?;
                ensure!(source.is_absolute(), "absolute background directory");
                let mut session = background_catalog.begin_import(source, Some(1))?;
                let mut consumer = None;
                let mut scan_done = false;
                while !stop.load(Ordering::Acquire) {
                    if !scan_done && consumer.is_none() {
                        let advanced = session.advance(&mut background_catalog, &mut previews)?;
                        scan_done = advanced.finished;
                        consumer = advanced.consumer;
                    }
                    previews.tick(&mut background_catalog)?;
                    let pids = previews.active_worker_pids();
                    {
                        let mut shared = state.lock().unwrap();
                        if shared.pids != pids { events.push(json!({"at":stamp(),"pids":pids})); }
                        shared.pids = pids;
                    }
                    if let Some(ticket) = consumer
                        && let Some(done) = previews.take_completion(ticket) {
                            ensure!(matches!(done, ServiceCompletion::Ready), "overlap import failed: {done:?}");
                            session.record_completion(&done);
                            consumer = None;
                    }
                    if scan_done && consumer.is_none() && previews.native_work_drained() { break; }
                    ensure!(events.len() <= 256 && start.elapsed() < Duration::from_secs(180), "background import bounds");
                    std::thread::sleep(Duration::from_millis(1));
                }
            } else {
                ensure!(request.outputs.len() == 1, "one background export configuration");
                let mut exports = ExportService::open(&background_catalog, &request.worker, ExportServiceLimits {
                    worker_bytes:request.render.max_live_bytes, working_bytes:request.render.max_live_bytes,
                    render:PhotoRenderLimits { decode:request.decode, render:request.render,
                        encode:EncodeLimits { render:request.render, ..Default::default() }, max_encoded_extent:request.encoded_extent },
                })?;
                ensure!(exports.recover(&mut background_catalog,128)?.complete, "fresh export recovery");
                let job = background_catalog.begin_photo_export()?;
                background_catalog.append_photo_export(&job.id,0,&ExportTarget { key:master.clone(), expected_revision:0,
                    destination:request.output.join("background-export.tiff"),overwrite:false,metadata:MetadataSelection::Omit },
                    &request.outputs[0],request.decode.max_encoded_bytes,request.encoded_extent)?;
                background_catalog.seal_photo_export_job(&job.id,1)?;
                while !stop.load(Ordering::Acquire) {
                    let event = exports.tick(&mut background_catalog,&mut previews,&job.id,&stop)?;
                    if let ExportEvent::Started { pid, .. } = event {
                        state.lock().unwrap().pids=vec![pid];
                        events.push(json!({"at":stamp(),"event":event}));
                    } else if matches!(event,ExportEvent::Published { .. }) {
                        state.lock().unwrap().pids.clear();
                        events.push(json!({"at":stamp(),"event":event,"phases":exports.take_completion_metrics()}));
                        break;
                    } else if matches!(event,ExportEvent::Failed { .. } | ExportEvent::Yielded { .. }) {
                        anyhow::bail!("background export failed: {event:?}");
                    }
                    ensure!(start.elapsed()<Duration::from_secs(180),"background export deadline");
                    std::thread::sleep(Duration::from_millis(1));
                }
                drop(exports); // Owned process destructor reaps on early cancellation.
            }
            drop(previews);
            let mut shared=state.lock().unwrap();
            shared.pids.clear();
            shared.done=true;
            Ok(events)
        });
        let foreground_result = (|| -> Result<()> {
            let wait = Instant::now();
            while state.lock().unwrap().pids.is_empty() {
                ensure!(
                    !background.is_finished() && wait.elapsed() < Duration::from_secs(180),
                    "background never admitted a worker"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut revision = foreground.revision;
            for iteration in 0..request.repetitions {
                let before = state.lock().unwrap().clone();
                ensure!(
                    !before.pids.is_empty() && !before.done,
                    "save began outside background worker lifecycle"
                );
                let live_before = live_worker_identities(&before.pids)?;
                let live_before_at = stamp();
                record(
                    samples,
                    json!({"kind":"attempt","iteration":iteration,"started":stamp(),"owned_pids":before.pids,
                        "live_workers":live_before}),
                )?;
                let started = stamp();
                let now = Instant::now();
                let saved = catalog.save_edit_recipe(
                    &foreground.key,
                    revision,
                    &request.recipes[iteration % request.recipes.len()],
                )?;
                let elapsed_ms = now.elapsed().as_secs_f64() * 1000.0;
                let finished = stamp();
                let live_after = live_worker_identities(&before.pids)?;
                let live_after_at = stamp();
                let after = state.lock().unwrap().clone();
                ensure!(
                    saved.revision > revision
                        && before.pids == after.pids
                        && !after.done
                        && live_before == live_after,
                    "save lacks continuous same-worker lifecycle overlap"
                );
                revision = saved.revision;
                record(
                    samples,
                    json!({"kind":"observation","iteration":iteration,"warmup":false,
                    "started":started,"finished":finished,"elapsed_ms":elapsed_ms,"revision":revision,
                    "owned_pids_before":before.pids,"owned_pids_after":after.pids,
                    "live_workers_before":live_before,"live_workers_after":live_after,
                    "live_before_at":live_before_at,"live_after_at":live_after_at,
                    "overlap_scope":"kernel non-zombie parent/start identity on both sides of durable save; external observer cross-check required"}),
                )?;
            }
            Ok(())
        })();
        if foreground_result.is_err() {
            stop.store(true, Ordering::Release);
        }
        let background_result = background
            .join()
            .map_err(|_| anyhow::anyhow!("background thread panicked"))?;
        // Keep lifecycle events even if foreground acceptance fails.
        if let Ok(events) = &background_result {
            exclusive(
                &request.output.join("background.json"),
                &json!({"started":background_start,"finished":stamp(),"events":events}),
            )?;
        }
        foreground_result?;
        background_result?;
        Ok(())
    })
}
fn refusal(request: &Request, samples: &mut File) -> Result<()> {
    use edit::RenderError;
    let recipe = request.recipes[0].validate()?;
    record(
        samples,
        json!({"kind":"attempt","iteration":0,"started":stamp()}),
    )?;
    let observed = match request.operation.as_str() {
        "decode_allocation" => {
            let mut limits = request.decode;
            limits.max_allocation_bytes = 1;
            match edit::decode_original(
                OriginalRequest {
                    path: &request.source,
                    expected_fingerprint: &request.source_blake3,
                    white_balance: &recipe.settings().white_balance,
                },
                limits,
                &(),
            ) {
                Err(RenderError::ResourceLimit { .. }) => "typed_render_resource_limit",
                Err(RenderError::Decode(error))
                    if error.status == photocatalog::media::DecodeStatus::ResourceLimit =>
                {
                    "typed_decode_resource_limit"
                }
                _ => anyhow::bail!("decode allocation refusal not observed"),
            }
        }
        "canceled" | "source_changed" => {
            let canceled = AtomicBool::new(request.operation == "canceled");
            let expected = if request.operation == "source_changed" {
                "0000000000000000000000000000000000000000000000000000000000000000"
            } else {
                &request.source_blake3
            };
            match edit::decode_original(
                OriginalRequest {
                    path: &request.source,
                    expected_fingerprint: expected,
                    white_balance: &recipe.settings().white_balance,
                },
                request.decode,
                &canceled,
            ) {
                Err(RenderError::Canceled) if request.operation == "canceled" => "typed_canceled",
                Err(RenderError::SourceChanged) if request.operation == "source_changed" => {
                    "typed_source_changed"
                }
                _ => anyhow::bail!("expected source/cancellation refusal not observed"),
            }
        }
        "render_allocation" | "encoded_extent" | "invalid_profile" => {
            let original = load_original(request, &request.recipes[0])?;
            let mut limits = request.render;
            if request.operation == "render_allocation" {
                limits.max_allocation_bytes = 1;
            }
            match edit::render_recipe(&original, &recipe, RenderPurpose::ExportExact, limits, &()) {
                Err(RenderError::ResourceLimit { .. })
                    if request.operation == "render_allocation" =>
                {
                    "typed_render_resource_limit"
                }
                Ok(image) if request.operation != "render_allocation" => {
                    ensure!(request.outputs.len() == 1, "refusal output configuration");
                    let file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(request.output.join("refused.partial"))?;
                    let mut sink = BoundedSeekWriter::new(
                        file,
                        if request.operation == "encoded_extent" {
                            16
                        } else {
                            request.encoded_extent
                        },
                    )?;
                    let result = image_export::encode_export(
                        &image,
                        &request.outputs[0],
                        &request.metadata,
                        &mut sink,
                        EncodeLimits {
                            render: request.render,
                            ..Default::default()
                        },
                        &(),
                    );
                    ensure!(
                        sink.extent() <= sink.limit(),
                        "encoded extent exceeded admission"
                    );
                    match result {
                        Err(RenderError::ResourceLimit { .. })
                            if request.operation == "encoded_extent" =>
                        {
                            "typed_encoded_resource_limit"
                        }
                        Err(RenderError::InvalidProfile(_))
                            if request.operation == "invalid_profile" =>
                        {
                            "typed_invalid_profile"
                        }
                        _ => anyhow::bail!("expected encoding refusal not observed"),
                    }
                }
                _ => anyhow::bail!("expected rendering admission outcome not observed"),
            }
        }
        _ => anyhow::bail!("unknown refusal operation"),
    };
    record(
        samples,
        json!({"kind":"observation","iteration":0,"warmup":false,"expected_refusal":observed}),
    )
}
fn run(request: &Request, samples: &mut File) -> Result<()> {
    ensure!(
        request.version == 1
            && request.source.is_absolute()
            && request.output.is_absolute()
            && request.worker.is_absolute(),
        "probe request version/paths"
    );
    ensure!(
        !request.recipes.is_empty() && request.recipes.len() <= 32 && request.outputs.len() <= 32,
        "matrix bound"
    );
    let expected = match request.phase {
        Phase::Kernel | Phase::WarmService => (2, 100),
        Phase::OverlapImport | Phase::OverlapExport => (0, 100),
        Phase::Full | Phase::FirstRaw | Phase::Export => (2, 20),
        Phase::Correctness
        | Phase::Support100mp
        | Phase::Refusal
        | Phase::ProxyReference
        | Phase::ExportCorrectness => (0, 1),
    };
    ensure!(
        (request.warmups, request.repetitions) == expected,
        "frozen sample counts changed"
    );
    if matches!(request.phase, Phase::Export | Phase::ExportCorrectness) {
        ensure!(
            request.recipes.len() == 1 && request.outputs.len() == 1,
            "export single configuration"
        );
    }
    if matches!(request.phase, Phase::WarmService | Phase::FirstRaw) {
        ensure!(
            request.recipes.len() == 2 && request.recipes[0] != request.recipes[1],
            "delivery requires alternating recipes"
        );
    }
    let pixel_count = u64::from(request.width) * u64::from(request.height);
    ensure!(
        pixel_count
            <= if matches!(request.phase, Phase::Support100mp | Phase::Refusal) {
                100_000_000
            } else {
                32_000_000
            },
        "source pixel admission"
    );
    ensure!(
        fingerprint(&request.source, request.decode.max_encoded_bytes)? == request.source_blake3,
        "source before mismatch"
    );
    match request.phase {
        Phase::Correctness
        | Phase::Kernel
        | Phase::Full
        | Phase::Support100mp
        | Phase::ProxyReference => pixels(request, samples)?,
        Phase::FirstRaw | Phase::WarmService => delivery(request, samples)?,
        Phase::Export | Phase::ExportCorrectness => export(request, samples)?,
        Phase::Refusal => refusal(request, samples)?,
        Phase::OverlapImport | Phase::OverlapExport => overlap(request, samples)?,
    }
    ensure!(
        fingerprint(&request.source, request.decode.max_encoded_bytes)? == request.source_blake3,
        "source after mismatch"
    );
    Ok(())
}
fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(path) = args.fingerprint {
        println!(
            "{}",
            json!({"blake3":fingerprint(&path, 2*1024*1024*1024)?})
        );
        return Ok(());
    }
    let path = args
        .request
        .context("--request or --fingerprint required")?;
    ensure!(
        fs::metadata(&path)?.len() <= 256 * 1024,
        "request byte bound"
    );
    let request: Request = serde_json::from_slice(&fs::read(&path)?)?;
    fs::create_dir(&request.output)?;
    exclusive(&request.output.join("request.json"), &request)?;
    let started = stamp();
    let mut samples = samples_path(&request)?;
    let result = run(&request, &mut samples);
    samples.sync_all()?;
    exclusive(
        &request.output.join("receipt.json"),
        &json!({"version":1,"probe_pid":std::process::id(),"started":started,"finished":stamp(),
        "probe_complete":result.is_ok(),"qualification_complete":false,
        "independent_oracles":"pending separate verifier","error":result.as_ref().err().map(|e|format!("{e:#}")),
        "source":request.source,"source_sha256":request.source_sha256,"source_blake3":request.source_blake3,
        "phase":request.phase,"fixture_id":request.fixture_id,"operation":request.operation,
        "rss":rss(),"renderer":edit::renderer_identity(),
        "export_renderer":photocatalog::photo_render::output_renderer_identity(),
        "probe_source_blake3":blake3::hash(include_bytes!("edit_probe.rs")).to_hex().to_string()}),
    )?;
    result
}
