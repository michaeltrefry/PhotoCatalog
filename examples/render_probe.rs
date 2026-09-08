//! Read-only validation tool. Outputs statistics and optional newly-created JPEG.
use photocatalog::media::decode_full;
use std::{io::Write, path::Path};
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: render_probe ORIGINAL [NEW_PREVIEW.jpg]"))?;
    let start = std::time::Instant::now();
    match decode_full(Path::new(&path)) {
        Ok(image) => {
            let (mut min, mut max) = (f32::INFINITY, f32::NEG_INFINITY);
            let (mut zero, mut partial, mut opaque) = (0u64, 0u64, 0u64);
            let mut digest = blake3::Hasher::new();
            for p in &image.pixels {
                for c in &p[..3] {
                    min = min.min(*c);
                    max = max.max(*c);
                }
                if p[3] == 0.0 {
                    zero += 1;
                } else if p[3] == 1.0 {
                    opaque += 1;
                } else {
                    partial += 1;
                }
                for c in p {
                    digest.update(&c.to_le_bytes());
                }
            }
            println!(
                "{}",
                serde_json::json!({"status":"decoded","width":image.width,"height":image.height,"metadata":image.metadata,"provenance":image.provenance,"nonfinite_components":image.pixels.iter().flatten().filter(|v|!v.is_finite()).count(),"minimum":min,"maximum":max,"alpha_zero":zero,"alpha_partial":partial,"alpha_opaque":opaque,"pixel_blake3":digest.finalize().to_hex().as_str(),"elapsed_seconds":start.elapsed().as_secs_f64()})
            );
            if let Some(out) = args.next() {
                let preview = image.srgb_preview(1024)?;
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(out)?;
                file.write_all(&preview)?;
                file.sync_all()?;
            }
        }
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"status":error.status,"error":error.message,"elapsed_seconds":start.elapsed().as_secs_f64()})
            );
            std::process::exit(1);
        }
    }
    Ok(())
}
