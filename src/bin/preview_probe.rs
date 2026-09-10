//! Exclusive-output codec experiment using the production preview APIs.
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use photocatalog::preview::{self, Codec, CodecSettings, PreparedRgb};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Versions,
    /// Untimed end-to-end artifact verification, separate from measured children.
    VerifyCase {
        prepared: PathBuf,
        folder: PathBuf,
    },
    VerifyReview {
        folder: PathBuf,
    },
    Review {
        manifest: PathBuf,
        output: PathBuf,
    },
    Prepare {
        source: PathBuf,
        output: PathBuf,
    },
    Measure {
        #[arg(long)]
        prepared: PathBuf,
        #[arg(long)]
        edge: u32,
        #[arg(long)]
        codec: String,
        #[arg(long)]
        quality: u8,
        #[arg(long)]
        output: PathBuf,
    },
}
fn exclusive(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn receipt(root: &Path, value: Value) -> Result<()> {
    exclusive(
        &root.join("receipt.json"),
        &serde_json::to_vec_pretty(&value)?,
    )?;
    println!("{}", value);
    Ok(())
}
fn sha(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
fn distribution(samples: &[f64]) -> Value {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile = |q: f64| {
        let p = (sorted.len() - 1) as f64 * q;
        let l = p.floor() as usize;
        let h = p.ceil() as usize;
        sorted[l] + (sorted[h] - sorted[l]) * p.fract()
    };
    json!({"n":samples.len(),"samples_ms":samples,"p50_ms":percentile(0.5),"p95_ms":percentile(0.95),"p99_ms":percentile(0.99),"max_ms":sorted.last()})
}
fn run(args: Args) -> Result<()> {
    match args.command {
        Command::Versions => println!("{}", identity()),
        Command::VerifyCase { prepared, folder } => {
            println!("{}", verify_case(&prepared, &folder)?)
        }
        Command::VerifyReview { folder } => {
            let value: Value =
                serde_json::from_slice(&std::fs::read(folder.join("receipt.json"))?)?;
            ensure!(
                value["complete"] == true
                    && directory_artifacts(&folder)? == value["output_artifacts"],
                "quality artifact mismatch"
            );
            println!(
                "{}",
                json!({"complete":true,"receipt_blake3":sha(&std::fs::read(folder.join("receipt.json"))?)})
            );
        }
        Command::Review { manifest, output } => review(&manifest, &output)?,
        Command::Prepare { source, output } => {
            std::fs::create_dir(&output).context("exclusive preparation output")?;
            let start = Instant::now();
            let image = match photocatalog::media::decode_full(&source) {
                Ok(v) => v,
                Err(e) => {
                    receipt(&output, json!({"complete":false,"error":e.to_string()}))?;
                    return Err(e.into());
                }
            };
            let decode_ms = start.elapsed().as_secs_f64() * 1000.;
            let mut surfaces = serde_json::Map::new();
            for edge in [256, 512, 1600, 2560] {
                let started = Instant::now();
                let pixels = preview::prepare(&image, edge)?;
                let elapsed = started.elapsed().as_secs_f64() * 1000.;
                exclusive(&output.join(format!("{edge}.rgb")), pixels.pixels())?;
                pixels.save_reference_png(&output.join(format!("{edge}.png")))?;
                surfaces.insert(edge.to_string(),json!({"width":pixels.width(),"height":pixels.height(),"preparation_ms":elapsed,"rgb_blake3":pixels.digest(),"rgb_bytes":pixels.byte_len()}));
            }
            receipt(
                &output,
                json!({"version":1,"complete":true,"source_width":image.width,"source_height":image.height,"metadata":image.metadata,"render_provenance":image.provenance,"preparation_version":preview::PREPARATION_VERSION,"decode_original_ms":decode_ms,"surfaces":surfaces,"versions":preview::versions(),"identity":identity()}),
            )?;
        }
        Command::Measure {
            prepared,
            edge,
            codec,
            quality,
            output,
        } => {
            std::fs::create_dir(&output).context("exclusive codec output")?;
            measure(&prepared, edge, &codec, quality, &output)?;
        }
    }
    Ok(())
}

fn measure(prepared: &Path, edge: u32, codec: &str, quality: u8, output: &Path) -> Result<()> {
    let mut progress = json!({"phase":"validation","iteration":null,"encode_samples_ms":[],"decode_samples_ms":[],"artifacts":[]});
    let result = (|| -> Result<Value> {
        ensure!(
            [256, 512, 1600, 2560].contains(&edge),
            "edge outside frozen matrix"
        );
        let codec = match codec {
            "jpeg" => Codec::Jpeg,
            "webp" => Codec::Webp,
            "avif" => Codec::Avif,
            _ => anyhow::bail!("unknown codec"),
        };
        ensure!(
            match codec {
                Codec::Avif => [45, 60, 75].contains(&quality),
                _ => [50, 65, 80].contains(&quality),
            },
            "quality outside frozen matrix"
        );
        let settings = CodecSettings { codec, quality };
        let metadata: Value =
            serde_json::from_slice(&std::fs::read(prepared.join("receipt.json"))?)?;
        ensure!(
            metadata["complete"] == true
                && metadata["preparation_version"] == preview::PREPARATION_VERSION,
            "prepared identity mismatch"
        );
        let surface = &metadata["surfaces"][edge.to_string()];
        let read_start = Instant::now();
        let bytes = std::fs::read(prepared.join(format!("{edge}.rgb")))?;
        let read_ms = read_start.elapsed().as_secs_f64() * 1000.;
        let reference = PreparedRgb::new(
            surface["width"].as_u64().context("width")?.try_into()?,
            surface["height"].as_u64().context("height")?.try_into()?,
            bytes,
        )?;
        ensure!(
            reference.digest() == surface["rgb_blake3"].as_str().context("prepared digest")?,
            "prepared bytes mismatch"
        );
        progress["prepared_blake3"] = json!(reference.digest());
        progress["prepared_read_ms"] = json!(read_ms);
        progress["phase"] = json!("encode_warmup");
        drop(preview::encode(&reference, settings, None)?); // one fixed warmup
        let mut encode_times = Vec::new();
        let mut encoded_results = Vec::new();
        let mut first = None;
        for iteration in 0..3 {
            progress["phase"] = json!("encode");
            progress["iteration"] = json!(iteration);
            let start = Instant::now();
            let encoded = preview::encode(&reference, settings, None)?;
            encode_times.push(start.elapsed().as_secs_f64() * 1000.);
            progress["encode_samples_ms"] = json!(encode_times);
            progress["phase"] = json!("encoded_artifact_write");
            progress["pending_artifact"] = json!({"bytes":encoded.len(),"blake3":sha(&encoded)});
            exclusive(
                &output.join(format!("encode-{iteration}.{}", codec.extension())),
                &encoded,
            )?;
            encoded_results.push(json!({"bytes":encoded.len(),"blake3":sha(&encoded),"jpeg_sampling":if codec==Codec::Jpeg {jpeg_sampling(&encoded)?} else {Value::Null}}));
            progress["artifacts"] = json!(encoded_results);
            if first.is_none() {
                first = Some(encoded);
            }
        }
        let encoded = first.unwrap();
        for iteration in 0..3 {
            progress["phase"] = json!("decode_warmup");
            progress["iteration"] = json!(iteration);
            drop(preview::decode(&encoded, codec)?);
        }
        let mut decode_times = Vec::new();
        let mut decoded_digest = None;
        for iteration in 0..20 {
            progress["phase"] = json!("decode");
            progress["iteration"] = json!(iteration);
            let start = Instant::now();
            let decoded = preview::decode(&encoded, codec)?;
            decode_times.push(start.elapsed().as_secs_f64() * 1000.);
            progress["decode_samples_ms"] = json!(decode_times);
            let digest = decoded.digest();
            if let Some(previous) = &decoded_digest {
                ensure!(*previous == digest, "nondeterministic decoded pixels");
            }
            decoded_digest = Some(digest);
        }
        progress["phase"] = json!("quality_artifacts");
        progress["iteration"] = Value::Null;
        let decoded = preview::decode(&encoded, codec)?;
        let metrics = preview::quality_metrics(&reference, &decoded)?;
        ensure!(
            Some(decoded.digest()) == decoded_digest,
            "quality decode differs from measured decode"
        );
        decoded.save_reference_png(&output.join("decoded.png"))?;
        Ok(
            json!({"version":1,"complete":true,"settings":settings.effective(),"versions":preview::versions(),"identity":identity(),"edge":edge,"width":reference.width(),"height":reference.height(),"prepared_blake3":reference.digest(),"prepared_read_ms":read_ms,"encode":distribution(&encode_times),"encode_tail_interpretation":"n3: median and range only; percentiles descriptive","decode":distribution(&decode_times),"decoded_blake3":decoded_digest,"artifacts":encoded_results,"quality":metrics,"timer_boundary":"encoded memory -> allocated fully materialized RGB8 sRGB; file I/O/checksums excluded"}),
        )
    })();
    match result {
        Ok(value) => receipt(output, value),
        Err(e) => {
            receipt(
                output,
                json!({"version":1,"complete":false,"identity":identity(),"edge":edge,"codec":codec,"quality":quality,"error":format!("{e:#}"),"partial":progress}),
            )?;
            Err(e)
        }
    }
}

fn jpeg_sampling(bytes: &[u8]) -> Result<Value> {
    ensure!(bytes.starts_with(&[255, 216]), "JPEG signature");
    let mut i = 2;
    while i + 4 <= bytes.len() {
        ensure!(bytes[i] == 255, "JPEG marker");
        let marker = bytes[i + 1];
        i += 2;
        if marker == 0xda || marker == 0xd9 {
            break;
        }
        let size = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
        ensure!(size >= 2 && i + size <= bytes.len(), "JPEG segment");
        if [0xc0, 0xc1, 0xc2].contains(&marker) {
            ensure!(size >= 8, "JPEG SOF");
            let n = bytes[i + 7] as usize;
            ensure!(size == 8 + 3 * n, "JPEG components");
            return Ok(
                json!({"precision":bytes[i+2],"components":(0..n).map(|j|json!({"id":bytes[i+8+j*3],"horizontal":bytes[i+9+j*3]>>4,"vertical":bytes[i+9+j*3]&15})).collect::<Vec<_>>()}),
            );
        }
        i += size;
    }
    anyhow::bail!("JPEG sampling unavailable")
}
fn main() -> Result<()> {
    run(Args::parse())
}

/// Untimed quality artifacts; kept separate from codec measurement subprocesses.
fn review(manifest: &Path, output: &Path) -> Result<()> {
    let value: Value = serde_json::from_slice(&std::fs::read(manifest)?)?;
    let reference = image::open(value["reference"].as_str().context("reference path")?)?.to_rgb8();
    ensure!(
        rgb_digest(&reference)?
            == value["reference_blake3"]
                .as_str()
                .context("reference digest")?,
        "review reference pixels mismatch"
    );
    let candidates = value["candidates"].as_array().context("candidates")?;
    ensure!(
        candidates.len() == 9,
        "review requires all nine frozen candidates"
    );
    std::fs::create_dir(output)?;
    let save = |path: &Path, image: image::RgbImage| -> Result<()> {
        PreparedRgb::new(image.width(), image.height(), image.into_raw())?.save_reference_png(path)
    };
    save(&output.join("reference.png"), reference.clone())?;
    let mut contact = image::RgbImage::from_pixel(3 * 512, 4 * 512, image::Rgb([255, 255, 255]));
    let edge = 512.min(reference.width().max(reference.height()));
    let thumb = image::DynamicImage::ImageRgb8(reference.clone())
        .thumbnail(edge, edge)
        .to_rgb8();
    image::imageops::replace(&mut contact, &thumb, 0, 0);
    let cw = (reference.width() / 4).max(1);
    let ch = (reference.height() / 4).max(1);
    let rects = [
        ((reference.width() - cw) / 2, (reference.height() - ch) / 2),
        (reference.width() / 8, reference.height() / 8),
        (reference.width() * 5 / 8, reference.height() / 8),
        (reference.width() / 8, reference.height() * 5 / 8),
        (reference.width() * 5 / 8, reference.height() * 5 / 8),
    ];
    let mut labels = Vec::new();
    for (i, candidate) in candidates.iter().enumerate() {
        let label = candidate["label"].as_str().context("label")?;
        ensure!(
            label == char::from(b'A' + i as u8).to_string(),
            "review labels must be A..I"
        );
        let decoded = image::open(candidate["path"].as_str().context("candidate path")?)?.to_rgb8();
        ensure!(
            decoded.dimensions() == reference.dimensions(),
            "review dimensions mismatch"
        );
        ensure!(
            rgb_digest(&decoded)?
                == candidate["decoded_blake3"]
                    .as_str()
                    .context("candidate digest")?,
            "review candidate pixels mismatch"
        );
        save(&output.join(format!("{label}.png")), decoded.clone())?;
        let thumb = image::DynamicImage::ImageRgb8(decoded.clone())
            .thumbnail(edge, edge)
            .to_rgb8();
        image::imageops::replace(
            &mut contact,
            &thumb,
            ((i + 1) % 3 * 512) as i64,
            ((i + 1) / 3 * 512) as i64,
        );
        for (number, (left, top)) in rects.iter().copied().enumerate() {
            let left = left.min(reference.width() - cw);
            let top = top.min(reference.height() - ch);
            let mut panel = image::RgbImage::new(cw * 3, ch);
            for y in 0..ch {
                for x in 0..cw {
                    let a = reference.get_pixel(left + x, top + y);
                    let b = decoded.get_pixel(left + x, top + y);
                    panel.put_pixel(x, y, *a);
                    panel.put_pixel(cw + x, y, *b);
                    panel.put_pixel(
                        cw * 2 + x,
                        y,
                        image::Rgb(std::array::from_fn(|c| {
                            a[c].abs_diff(b[c]).saturating_mul(8)
                        })),
                    );
                }
            }
            save(&output.join(format!("{label}-crop-{number}.png")), panel)?;
        }
        labels.push(label.to_owned());
    }
    save(&output.join("contact.png"), contact)?;
    let mut html = String::from(
        "<!doctype html><meta charset=utf-8><title>Blinded preview quality</title><style>body{font:16px system-ui;background:#eee;color:#111;margin:24px}img{max-width:100%;height:auto}button{font:inherit;margin:8px}#full{max-width:none}figure{margin:20px 0}</style><h1>Blinded preview quality</h1><p>Contact order: reference, A–I. Full image is 1:1; click reference or a candidate to toggle. Crop panels: reference, candidate, absolute error ×8. Codec identities are in the separate mapping.</p><img src=contact.png><p><button onclick=\"document.getElementById('full').src='reference.png'\">Reference</button>",
    );
    for label in &labels {
        html.push_str(&format!(
            "<button onclick=\"document.getElementById('full').src='{label}.png'\">{label}</button>"
        ));
    }
    html.push_str(
        "</p><div style='overflow:auto;max-height:80vh'><img id=full src=reference.png></div>",
    );
    for label in &labels {
        html.push_str(&format!("<h2>Candidate {label}</h2>"));
        for n in 0..5 {
            html.push_str(&format!(
                "<figure><figcaption>Crop {n}</figcaption><img src='{label}-crop-{n}.png'></figure>"
            ));
        }
    }
    exclusive(&output.join("index.html"), html.as_bytes())?;
    receipt(
        output,
        json!({"complete":true,"quality_only":true,"labels":labels,"crop_width":cw,"crop_height":ch,"crop_origins":rects,"contact_cell_edge":512,"difference_multiplier":8,"input_manifest_blake3":sha(&std::fs::read(manifest)?),"inputs":value,"output_artifacts":directory_artifacts(output)?}),
    )
}

fn identity() -> Value {
    json!({"versions":preview::versions(),"preparation":preview::PREPARATION_VERSION,"cargo_lock_blake3":sha(include_bytes!("../../Cargo.lock")),"source_blake3":{
                "preview_probe":sha(include_bytes!("preview_probe.rs")),"codec":sha(include_bytes!("../preview/codec.rs")),"preparation":sha(include_bytes!("../media/full.rs")),"native_preview":sha(include_bytes!("../../native/preview.cpp")),"native_preview_header":sha(include_bytes!("../../native/preview.h")),"metrics":sha(include_bytes!("../preview/metrics.rs")),"native_decode":sha(include_bytes!("../../native/decode.cpp")),"native_dng":sha(include_bytes!("../../native/dng.cpp")),"build":sha(include_bytes!("../../build.rs"))}})
}

fn rgb_digest(image: &image::RgbImage) -> Result<String> {
    Ok(PreparedRgb::new(image.width(), image.height(), image.as_raw().clone())?.digest())
}
fn directory_artifacts(root: &Path) -> Result<Value> {
    let mut entries = std::fs::read_dir(root)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    let mut values = Vec::new();
    for entry in entries {
        if entry.file_name() == "receipt.json" {
            continue;
        }
        let bytes = std::fs::read(entry.path())?;
        let pixels = if entry.path().extension().is_some_and(|x| x == "png") {
            Some(rgb_digest(&image::load_from_memory(&bytes)?.to_rgb8())?)
        } else {
            None
        };
        values.push(json!({"name":entry.file_name().to_str().context("artifact name")?,"bytes":bytes.len(),"blake3":sha(&bytes),"rgb_blake3":pixels}));
    }
    Ok(json!(values))
}
/// Re-read/redecode everything outside all measurement intervals. This binds
/// reported hashes to actual files and the quality PNG to measured decoder pixels.
fn verify_case(prepared: &Path, folder: &Path) -> Result<Value> {
    let value: Value = serde_json::from_slice(&std::fs::read(folder.join("receipt.json"))?)?;
    ensure!(
        value["complete"] == true && value["identity"] == identity(),
        "case identity"
    );
    let edge = value["edge"].as_u64().context("edge")?;
    let meta: Value = serde_json::from_slice(&std::fs::read(prepared.join("receipt.json"))?)?;
    ensure!(
        meta["complete"] == true && meta["identity"] == identity(),
        "preparation identity"
    );
    let surface = &meta["surfaces"][edge.to_string()];
    let reference = PreparedRgb::new(
        surface["width"].as_u64().context("width")?.try_into()?,
        surface["height"].as_u64().context("height")?.try_into()?,
        std::fs::read(prepared.join(format!("{edge}.rgb")))?,
    )?;
    ensure!(
        json!(reference.digest()) == surface["rgb_blake3"]
            && json!(reference.digest()) == value["prepared_blake3"],
        "prepared RGB mismatch"
    );
    ensure!(
        reference.digest()
            == rgb_digest(&image::open(prepared.join(format!("{edge}.png")))?.to_rgb8())?,
        "prepared PNG mismatch"
    );
    let codec: Codec = serde_json::from_value(value["settings"]["codec"].clone())?;
    let artifacts = value["artifacts"].as_array().context("encoded artifacts")?;
    ensure!(artifacts.len() == 3, "encoded artifact count");
    let mut verified = Vec::new();
    for (i, artifact) in artifacts.iter().enumerate() {
        let bytes = std::fs::read(folder.join(format!("encode-{i}.{}", codec.extension())))?;
        ensure!(
            json!(bytes.len()) == artifact["bytes"] && json!(sha(&bytes)) == artifact["blake3"],
            "encoded artifact digest mismatch"
        );
        if codec == Codec::Jpeg {
            ensure!(
                jpeg_sampling(&bytes)? == artifact["jpeg_sampling"],
                "JPEG sampling mismatch"
            );
        }
        let decoded = preview::decode(&bytes, codec)?;
        ensure!(
            json!(decoded.digest()) == value["decoded_blake3"],
            "encoded artifact decodes to different pixels"
        );
        if i == 0 {
            ensure!(
                decoded.digest()
                    == rgb_digest(&image::open(folder.join("decoded.png"))?.to_rgb8())?,
                "decoded PNG mismatch"
            );
            verify_quality(
                &serde_json::to_value(preview::quality_metrics(&reference, &decoded)?)?,
                &value["quality"],
            )?;
        }
        verified.push(
            json!({"bytes":bytes.len(),"blake3":sha(&bytes),"decoded_blake3":decoded.digest()}),
        );
    }
    Ok(
        json!({"complete":true,"identity":identity(),"receipt_blake3":sha(&std::fs::read(folder.join("receipt.json"))?),"prepared_blake3":reference.digest(),"decoded_blake3":value["decoded_blake3"],"artifacts":verified}),
    )
}
fn verify_quality(actual: &Value, reported: &Value) -> Result<()> {
    ensure!(
        actual["identical"] == reported["identical"]
            && actual["maximum_channel_error"] == reported["maximum_channel_error"],
        "quality categorical/integer mismatch"
    );
    for key in ["mse_rgb8", "psnr_db", "block_rgb_ssim"] {
        if actual[key].is_null() {
            ensure!(reported[key].is_null(), "quality nullable field mismatch");
        } else {
            let a = actual[key].as_f64().context("computed quality number")?;
            let b = reported[key].as_f64().context("reported quality number")?;
            // Default serde_json parsing can move the serialized f64 by an ULP.
            // This allowance is only for decimal roundtrip, not codec quality.
            ensure!(
                a.is_finite()
                    && b.is_finite()
                    && (a - b).abs() <= 4.0 * f64::EPSILON * a.abs().max(b.abs()).max(1.0),
                "quality numeric mismatch"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn late_write_failure_preserves_collected_observations() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = dir.path().join("prepared");
        let source = dir.path().join("source.png");
        image::RgbImage::from_pixel(8, 4, image::Rgb([128, 128, 128]))
            .save(&source)
            .unwrap();
        run(Args {
            command: Command::Prepare {
                source,
                output: prepared.clone(),
            },
        })
        .unwrap();
        let output = dir.path().join("failed");
        std::fs::create_dir(&output).unwrap();
        // A real filesystem failure after one successful artifact, not a codec stub.
        std::fs::create_dir(output.join("encode-1.jpg")).unwrap();
        assert!(measure(&prepared, 256, "jpeg", 65, &output).is_err());
        let value: Value =
            serde_json::from_slice(&std::fs::read(output.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(value["complete"], false);
        assert_eq!(value["partial"]["phase"], "encoded_artifact_write");
        assert_eq!(value["partial"]["iteration"], 1);
        assert_eq!(
            value["partial"]["encode_samples_ms"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(value["partial"]["artifacts"].as_array().unwrap().len(), 1);
        assert!(output.join("encode-0.jpg").exists());
    }
}
