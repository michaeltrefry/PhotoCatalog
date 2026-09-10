//! Source-frozen Stage B probes. No workload is run by compilation or discovery.
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use photocatalog::{media::DecodeLimits, preview::*, storage_volume::NativePath};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Worker(WorkerArgs),
    /// Untimed reread and complete production decode of saved artifacts.
    Verify {
        folder: PathBuf,
    },
}
#[derive(clap::Args)]
struct WorkerArgs {
    /// Actual application binary, whose --preview-worker is the production path.
    #[arg(long)]
    worker: PathBuf,
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    fixture_id: String,
    #[arg(long)]
    limits: PathBuf,
    #[arg(long)]
    output: PathBuf,
}
fn exclusive(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn fingerprint(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finalize().to_hex().to_string())
}
fn anchor(started: Instant) -> Value {
    json!({"unix_ns":SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|v|v.as_nanos().to_string()),
        "elapsed_ns":started.elapsed().as_nanos().to_string()})
}
fn run(args: WorkerArgs) -> Result<()> {
    std::fs::create_dir(&args.output).context("probe output must be new")?;
    let started = Instant::now();
    let mut receipt = json!({"version":1,"complete":false,"fixture_id":args.fixture_id,
        "started":anchor(started),"renderer_identity":renderer_identity(),
        "preparation_version":PREPARATION_VERSION,"codec_versions":versions(),
        "cargo_lock_blake3":blake3::hash(include_bytes!("../../Cargo.lock")).to_hex().to_string(),
        "probe_source_blake3":blake3::hash(include_bytes!("preview_runtime_probe.rs")).to_hex().to_string(),
        "phase":"preflight","artifacts":[],"worker_timeout_seconds":300,
        "worker_rss_scope":"process high-water through all image work and final source verification; bounded receipt write follows"});
    let result = (|| -> Result<()> {
        ensure!(
            args.source.is_absolute() && args.worker.is_absolute(),
            "absolute source/worker required"
        );
        ensure!(
            std::fs::metadata(&args.limits)?.len() <= 4096,
            "limits receipt too large"
        );
        let limits: DecodeLimits = serde_json::from_slice(&std::fs::read(&args.limits)?)?;
        limits.validate()?;
        receipt["decode_limits"] = serde_json::to_value(limits)?;
        let before = fingerprint(&args.source)?;
        receipt["source_blake3"] = json!(before);
        receipt["worker_binary_blake3"] = json!(fingerprint(&args.worker)?);
        let keys = [(Tier::Thumbnail, 512), (Tier::Large, 1600)]
            .into_iter()
            .map(|(tier, edge)| PreviewKey {
                asset_id: args.fixture_id.clone(),
                variant_id: "master".into(),
                generation: 1,
                fingerprint: before.clone(),
                edit_revision: 0,
                renderer_version: renderer_identity().into(),
                preparation_version: PREPARATION_VERSION.into(),
                tier,
                edge,
                encoding: CodecSettings {
                    codec: Codec::Jpeg,
                    quality: 80,
                },
            })
            .collect();
        let request = RenderWork {
            edit: None,
            source: NativePath::from_path(&args.source),
            keys,
            encoded_limit: 8 * 1024 * 1024,
            decode_limits: limits,
        };
        receipt["request"] = serde_json::to_value(&request)?;
        receipt["phase"] = json!("worker");
        receipt["worker_started"] = anchor(started);
        let mut worker = WorkerProcess::spawn(&args.worker, &args.output.join("staging"), request)?;
        receipt["worker_pid"] = json!(worker.pid());
        let worker_started = Instant::now();
        let batch = loop {
            if let Some(batch) = worker.poll(&AtomicBool::new(false))? {
                break batch;
            }
            ensure!(
                worker_started.elapsed() < Duration::from_secs(300),
                "worker timeout"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        receipt["worker_finished"] = anchor(started);
        receipt["worker_wall_ms"] = json!(worker_started.elapsed().as_secs_f64() * 1000.0);
        receipt["worker_peak_rss_bytes"] = json!(batch.peak_resident_bytes);
        receipt["worker_peak_method"] = json!(batch.peak_method);
        receipt["metadata"] = serde_json::to_value(batch.metadata)?;
        receipt["provenance"] = serde_json::to_value(batch.provenance)?;
        // poll already verified every encoded checksum and materialized both
        // RGB8 surfaces with the production decoder before this timer ended.
        receipt["phase"] = json!("retain-artifacts");
        for object in batch.objects {
            let name = format!("{}.jpg", object.key.edge);
            exclusive(&args.output.join(&name), &object.encoded)?;
            receipt["artifacts"].as_array_mut().unwrap().push(json!({"path":name,
                "key":object.key,"width":object.pixels.width(),"height":object.pixels.height(),
                "bytes":object.encoded.len(),"encoded_blake3":blake3::hash(&object.encoded).to_hex().to_string(),
                "decoded_blake3":object.pixels.digest()}));
        }
        drop(worker);
        receipt["phase"] = json!("source-recheck");
        let after = fingerprint(&args.source)?;
        receipt["source_after_blake3"] = json!(after);
        ensure!(before == after, "source changed");
        ensure!(
            receipt["worker_peak_rss_bytes"]
                .as_u64()
                .is_some_and(|v| v > 0),
            "complete worker memory evidence unavailable"
        );
        receipt["phase"] = json!("complete");
        receipt["complete"] = json!(true);
        Ok(())
    })();
    if let Err(error) = &result {
        receipt["error"] = json!(format!("{error:#}"));
    }
    receipt["finished"] = anchor(started);
    exclusive(
        &args.output.join("receipt.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    println!("{receipt}");
    result
}
fn verify(folder: &Path) -> Result<Value> {
    let receipt_path = folder.join("receipt.json");
    ensure!(
        std::fs::metadata(&receipt_path)?.len() <= 128 * 1024,
        "receipt size"
    );
    let receipt: Value = serde_json::from_slice(&std::fs::read(receipt_path)?)?;
    ensure!(
        receipt["complete"] == true && receipt["renderer_identity"] == renderer_identity(),
        "incomplete or different renderer receipt"
    );
    let artifacts = receipt["artifacts"]
        .as_array()
        .context("missing artifacts")?;
    ensure!(artifacts.len() == 2, "selected tier count");
    let mut verified = Vec::new();
    for (artifact, edge) in artifacts.iter().zip([512u32, 1600]) {
        let key: PreviewKey = serde_json::from_value(artifact["key"].clone())?;
        ensure!(
            key.edge == edge
                && key.encoding
                    == CodecSettings {
                        codec: Codec::Jpeg,
                        quality: 80
                    },
            "selected encoding mismatch"
        );
        let name = format!("{edge}.jpg");
        ensure!(artifact["path"] == name, "artifact path mismatch");
        let path = folder.join(&name);
        let length = std::fs::metadata(&path)?.len();
        ensure!(
            length > 0 && length <= 8 * 1024 * 1024 && artifact["bytes"].as_u64() == Some(length),
            "artifact byte limit"
        );
        let mut encoded = vec![0; length as usize];
        let mut file = File::open(path)?;
        file.read_exact(&mut encoded)?;
        ensure!(file.read(&mut [0])? == 0, "artifact changed size");
        let hash = blake3::hash(&encoded).to_hex().to_string();
        ensure!(
            artifact["encoded_blake3"] == hash && encoded.ends_with(&[0xff, 0xd9]),
            "artifact digest or completion mismatch"
        );
        let dimensions = encoded_dimensions(&encoded, Codec::Jpeg)?;
        ensure!(
            dimensions.0 <= edge && dimensions.1 <= edge,
            "oversized artifact"
        );
        let decoded = decode(&encoded, Codec::Jpeg)?;
        ensure!(
            artifact["width"].as_u64() == Some(u64::from(decoded.width()))
                && artifact["height"].as_u64() == Some(u64::from(decoded.height()))
                && artifact["decoded_blake3"] == decoded.digest(),
            "decoded surface mismatch"
        );
        verified.push(json!({"path":name,"encoded_blake3":hash,"decoded_blake3":decoded.digest(),"bytes":length}));
    }
    Ok(
        json!({"complete":true,"renderer_identity":renderer_identity(),"fixture_id":receipt["fixture_id"],"artifacts":verified}),
    )
}
fn main() -> Result<()> {
    match Args::parse().command {
        Command::Worker(args) => run(args),
        Command::Verify { folder } => {
            println!("{}", verify(&folder)?);
            Ok(())
        }
    }
}
