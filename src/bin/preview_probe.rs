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
            let result = (|| -> Result<Value> {
                ensure!(
                    [256, 512, 1600, 2560].contains(&edge),
                    "edge outside frozen matrix"
                );
                let codec = match codec.as_str() {
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
                    reference.digest()
                        == surface["rgb_blake3"].as_str().context("prepared digest")?,
                    "prepared bytes mismatch"
                );
                drop(preview::encode(&reference, settings, None)?); // one fixed warmup
                let mut encode_times = Vec::new();
                let mut encoded_results = Vec::new();
                let mut first = None;
                for iteration in 0..3 {
                    let start = Instant::now();
                    let encoded = preview::encode(&reference, settings, None)?;
                    encode_times.push(start.elapsed().as_secs_f64() * 1000.);
                    exclusive(
                        &output.join(format!("encode-{iteration}.{}", codec.extension())),
                        &encoded,
                    )?;
                    encoded_results.push(json!({"bytes":encoded.len(),"blake3":sha(&encoded),"jpeg_sampling":if codec==Codec::Jpeg {jpeg_sampling(&encoded)?} else {Value::Null}}));
                    if first.is_none() {
                        first = Some(encoded);
                    }
                }
                let encoded = first.unwrap();
                for _ in 0..3 {
                    drop(preview::decode(&encoded, codec)?);
                }
                let mut decode_times = Vec::new();
                let mut decoded_digest = None;
                for _ in 0..20 {
                    let start = Instant::now();
                    let decoded = preview::decode(&encoded, codec)?;
                    decode_times.push(start.elapsed().as_secs_f64() * 1000.);
                    let digest = decoded.digest();
                    if let Some(previous) = &decoded_digest {
                        ensure!(*previous == digest, "nondeterministic decoded pixels");
                    }
                    decoded_digest = Some(digest);
                }
                let decoded = preview::decode(&encoded, codec)?;
                let metrics = preview::quality_metrics(&reference, &decoded)?;
                decoded.save_reference_png(&output.join("decoded.png"))?;
                Ok(
                    json!({"version":1,"complete":true,"settings":settings.effective(),"versions":preview::versions(),"identity":identity(),"edge":edge,"width":reference.width(),"height":reference.height(),"prepared_blake3":reference.digest(),"prepared_read_ms":read_ms,"encode":distribution(&encode_times),"encode_tail_interpretation":"n3: median and range only; percentiles descriptive","decode":distribution(&decode_times),"decoded_blake3":decoded_digest,"artifacts":encoded_results,"quality":metrics,"timer_boundary":"encoded memory -> allocated fully materialized RGB8 sRGB; file I/O/checksums excluded"}),
                )
            })();
            match result {
                Ok(value) => receipt(&output, value)?,
                Err(e) => {
                    receipt(&output, json!({"complete":false,"error":format!("{e:#}")}))?;
                    return Err(e);
                }
            }
        }
    }
    Ok(())
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
        json!({"complete":true,"quality_only":true,"labels":labels,"crop_width":cw,"crop_height":ch,"crop_origins":rects,"contact_cell_edge":512,"difference_multiplier":8}),
    )
}

fn identity() -> Value {
    json!({"versions":preview::versions(),"preparation":preview::PREPARATION_VERSION,"cargo_lock_blake3":sha(include_bytes!("../../Cargo.lock")),"source_blake3":{
                "preview_probe":sha(include_bytes!("preview_probe.rs")),"codec":sha(include_bytes!("../preview/codec.rs")),"preparation":sha(include_bytes!("../media/full.rs")),"native_preview":sha(include_bytes!("../../native/preview.cpp")),"native_preview_header":sha(include_bytes!("../../native/preview.h")),"metrics":sha(include_bytes!("../preview/metrics.rs")),"native_decode":sha(include_bytes!("../../native/decode.cpp")),"native_dng":sha(include_bytes!("../../native/dng.cpp")),"build":sha(include_bytes!("../../build.rs"))}})
}
