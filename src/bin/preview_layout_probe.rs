//! Byte-distinct layout fixtures and production-store lookup measurements.
//! No original decode or layout/default selection is performed by this tool.
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use photocatalog::preview::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Prepare {
        #[arg(long)]
        campaign: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        count: u32,
        #[arg(long, value_enum)]
        layout: LayoutArg,
    },
    Lookup {
        #[arg(long)]
        dataset: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Footprint {
        #[arg(long)]
        dataset: PathBuf,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    Flat,
    HashPrefix,
}
#[derive(Serialize, Deserialize)]
struct Seed {
    key: PreviewKey,
    encoded_path: PathBuf,
    encoded_blake3: String,
    decoded_blake3: String,
    width: u32,
    height: u32,
    record: RenderRecord,
}
#[derive(Serialize, Deserialize)]
struct Dataset {
    version: u32,
    count: u32,
    store: StoreConfig,
    seeds: Vec<Seed>,
    marker_overhead_bytes: u32,
}
fn exclusive(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    ensure!(length > 0 && length <= limit, "fixture byte limit");
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)?;
    ensure!(file.read(&mut [0])? == 0, "fixture grew");
    Ok(bytes)
}
fn stamp(start: Instant) -> Value {
    json!({"unix_ns":SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d|d.as_nanos().to_string()),"elapsed_ns":start.elapsed().as_nanos().to_string()})
}
fn distinct_jpeg(base: &[u8], index: u32) -> Result<Vec<u8>> {
    ensure!(
        base.starts_with(&[0xff, 0xd8]) && base.ends_with(&[0xff, 0xd9]),
        "complete JPEG required"
    );
    let comment = format!("photocatalog-layout-v1:{index:010}");
    ensure!(comment.len() == 33, "fixed layout marker length");
    let mut result = Vec::with_capacity(base.len() + 37);
    result.extend_from_slice(&base[..2]);
    result.extend_from_slice(&[0xff, 0xfe]);
    result.extend_from_slice(&35u16.to_be_bytes());
    result.extend_from_slice(comment.as_bytes());
    result.extend_from_slice(&base[2..]);
    Ok(result)
}
fn key(dataset: &Dataset, index: u32) -> PreviewKey {
    let mut key = dataset.seeds[index as usize % dataset.seeds.len()]
        .key
        .clone();
    key.asset_id = format!("layout-{index:010}");
    key
}
fn dataset(path: &Path) -> Result<Dataset> {
    let value: Dataset = serde_json::from_slice(&read_bounded(path, 1024 * 1024)?)?;
    ensure!(
        value.version == 1
            && matches!(value.count, 10_000 | 100_000)
            && value.seeds.len() == 30
            && value.marker_overhead_bytes == 37,
        "unexpected layout dataset"
    );
    for seed in &value.seeds {
        seed.key.validate()?;
        ensure!(
            seed.key.renderer_version == renderer_identity()
                && seed.key.tier == Tier::Thumbnail
                && seed.key.edge == 512
                && seed.key.encoding
                    == CodecSettings {
                        codec: Codec::Jpeg,
                        quality: 80
                    },
            "layout renderer/selection mismatch"
        );
    }
    Ok(value)
}
fn prepare(campaign: &Path, output: &Path, count: u32, layout: LayoutArg) -> Result<()> {
    ensure!(
        matches!(count, 10_000 | 100_000),
        "fixed layout counts required"
    );
    ensure!(
        campaign.is_absolute() && output.is_absolute(),
        "absolute campaign/output paths required"
    );
    fs::create_dir(output).context("new layout output required")?;
    let start = Instant::now();
    let mut receipt = json!({"version":1,"complete":false,"count":count,"started":stamp(start),"phase":"inputs",
        "construction":"round-robin selected JPEG80 plus unique 33-byte ASCII COM payload/4-byte marker; decoded pixels unchanged",
        "marker_overhead_per_object":37,"renderer_identity":renderer_identity(),"entries_completed":0});
    let result = (|| -> Result<()> {
        let campaign_bytes = read_bounded(campaign, 2 * 1024 * 1024)?;
        receipt["campaign_blake3"] = json!(blake3::hash(&campaign_bytes).to_hex().to_string());
        let input: Value = serde_json::from_slice(&campaign_bytes)?;
        ensure!(
            input["complete"] == true && input["source_preserved"] == true,
            "incomplete worker campaign"
        );
        let children = input["children"]
            .as_array()
            .context("missing worker cohort")?;
        ensure!(children.len() == 30, "complete 30-image cohort required");
        let mut seeds = Vec::new();
        let mut payloads = Vec::new();
        let mut retained_bytes = 0u64;
        let mut base_hashes = HashSet::new();
        for child in children {
            ensure!(child["complete"] == true, "incomplete worker child");
            let id = child["id"].as_str().context("fixture id")?;
            ensure!(
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
                "unsafe fixture id"
            );
            let artifact = child["result"]["artifacts"]
                .as_array()
                .context("artifacts")?
                .iter()
                .find(|v| v["key"]["edge"] == 512)
                .context("missing retained tier")?;
            let key: PreviewKey = serde_json::from_value(artifact["key"].clone())?;
            ensure!(
                key.renderer_version == renderer_identity()
                    && key.tier == Tier::Thumbnail
                    && key.encoding
                        == CodecSettings {
                            codec: Codec::Jpeg,
                            quality: 80
                        },
                "selected tier identity mismatch"
            );
            let path = campaign
                .parent()
                .context("campaign parent")?
                .join(id)
                .join("512.jpg");
            let size = fs::metadata(&path)?.len();
            ensure!(
                retained_bytes
                    .checked_add(size)
                    .is_some_and(|v| v <= 16 * 1024 * 1024),
                "seed payload admission"
            );
            let bytes = read_bounded(&path, 8 * 1024 * 1024)?;
            retained_bytes += bytes.len() as u64;
            let encoded_hash = blake3::hash(&bytes).to_hex().to_string();
            ensure!(
                artifact["encoded_blake3"] == encoded_hash,
                "selected encoded bytes changed"
            );
            let (w, h) = encoded_dimensions(&bytes, Codec::Jpeg)?;
            ensure!(w <= 512 && h <= 512, "retained source dimensions");
            let pixels = decode(&bytes, Codec::Jpeg)?;
            ensure!(
                artifact["decoded_blake3"] == pixels.digest(),
                "selected decoded pixels changed"
            );
            // Verify marker insertion against every one of the 30 source images.
            let marked = distinct_jpeg(&bytes, seeds.len() as u32)?;
            ensure!(
                decode(&marked, Codec::Jpeg)?.pixels() == pixels.pixels(),
                "COM marker changed pixels"
            );
            base_hashes.insert(encoded_hash.clone());
            seeds.push(Seed {
                key,
                encoded_path: path,
                encoded_blake3: encoded_hash,
                decoded_blake3: pixels.digest(),
                width: w,
                height: h,
                record: RenderRecord {
                    width: w,
                    height: h,
                    metadata: serde_json::from_value(child["result"]["metadata"].clone())?,
                    provenance: serde_json::from_value(child["result"]["provenance"].clone())?,
                },
            });
            payloads.push(bytes);
        }
        let total_bytes = (0..count)
            .map(|i| payloads[i as usize % 30].len() as u64 + 37)
            .sum::<u64>();
        let data = Dataset {
            version: 1,
            count,
            store: StoreConfig {
                manifest_root: output.join("manifest"),
                thumbnail_root: output.join("thumbnails"),
                large_root: output.join("large"),
                layout: match layout {
                    LayoutArg::Flat => Layout::Flat,
                    LayoutArg::HashPrefix => Layout::HashPrefix,
                },
                thumbnail_bytes: total_bytes,
                large_bytes: 1024,
            },
            seeds,
            marker_overhead_bytes: 37,
        };
        receipt["distinct_base_encoded_payloads"] = json!(base_hashes.len());
        receipt["seed_payload_bytes"] = json!(retained_bytes);
        receipt["phase"] = json!("publication");
        let store = PreviewStore::open(data.store.clone(), &[])?;
        let mut content_hashes = HashSet::new();
        for index in 0..count {
            let key = key(&data, index);
            let bytes = distinct_jpeg(&payloads[index as usize % 30], index)?;
            ensure!(
                content_hashes.insert(*blake3::hash(&bytes).as_bytes()),
                "duplicate generated content hash"
            );
            store.desire(&key, || Ok(true))?;
            ensure!(
                store.publish_record(
                    &key,
                    &bytes,
                    &data.seeds[index as usize % 30].record,
                    |attach| attach()
                )? == Publication::Attached,
                "layout publication failed"
            );
            receipt["entries_completed"] = json!(index + 1);
        }
        let usage = store.usage()?;
        ensure!(
            usage.objects == u64::from(count)
                && usage.thumbnail_bytes == total_bytes
                && usage.pending_objects == 0,
            "manifest cardinality/accounting mismatch"
        );
        receipt["distinct_generated_content_hashes"] = json!(content_hashes.len());
        receipt["encoded_object_bytes"] = json!(total_bytes);
        drop(store);
        exclusive(
            &output.join("dataset.json"),
            &serde_json::to_vec_pretty(&data)?,
        )?;
        let footprint = footprint(&data)?;
        ensure!(
            footprint["thumbnail"]["object_files"].as_u64() == Some(u64::from(count)),
            "actual filesystem object count mismatch"
        );
        receipt["footprint"] = footprint;
        receipt["phase"] = json!("complete");
        receipt["complete"] = json!(true);
        Ok(())
    })();
    if let Err(error) = &result {
        receipt["error"] = json!(format!("{error:#}"));
    }
    receipt["finished"] = stamp(start);
    exclusive(
        &output.join("preparation.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    println!("{receipt}");
    result
}
fn footprint(data: &Dataset) -> Result<Value> {
    let mut result = json!({});
    for (name, root) in [
        ("manifest", &data.store.manifest_root),
        ("thumbnail", &data.store.thumbnail_root),
        ("large", &data.store.large_root),
    ] {
        let (mut files, mut objects, mut directories, mut logical, mut file_alloc, mut dir_alloc) =
            (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
        for entry in walkdir::WalkDir::new(root).follow_links(false).max_open(16) {
            let entry = entry?;
            ensure!(!entry.file_type().is_symlink(), "fixture contains symlink");
            let meta = entry.metadata()?;
            #[cfg(unix)]
            let allocation = {
                use std::os::unix::fs::MetadataExt;
                meta.blocks() * 512
            };
            #[cfg(not(unix))]
            let allocation = 0;
            if meta.is_dir() {
                directories += 1;
                dir_alloc += allocation;
            } else if meta.is_file() {
                files += 1;
                logical += meta.len();
                file_alloc += allocation;
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
                {
                    objects += 1;
                }
            }
        }
        result[name] = json!({"files":files,"object_files":objects,"directories_including_root":directories,"logical_file_bytes":logical,
            "file_allocated_bytes":cfg!(unix).then_some(file_alloc),"directory_allocated_bytes":cfg!(unix).then_some(dir_alloc),
            "allocation_method":if cfg!(unix){"st_blocks * 512; logical directories separately counted"}else{"unavailable; never treated as zero"}});
    }
    Ok(result)
}
fn shuffled(count: u32, seed: u64) -> Vec<u32> {
    let mut values = (0..count).collect::<Vec<_>>();
    let mut state = seed;
    for index in (1..values.len()).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        values.swap(index, (state % ((index + 1) as u64)) as usize);
    }
    values
}
fn lookup(dataset_path: &Path, output: &Path) -> Result<()> {
    ensure!(
        dataset_path.is_absolute() && output.is_absolute(),
        "absolute dataset/output paths required"
    );
    fs::create_dir(output).context("new lookup output required")?;
    let start = Instant::now();
    let mut receipt = json!({"version":1,"complete":false,"started":stamp(start),"passes":[],"renderer_identity":renderer_identity(),
        "state":"one process/manifest connection; no OS cache eviction; first pass separate; production deferred touches enabled",
        "measurement":"full production manifest lookup, file read and checksum; no pixel decode in layout-only measurements"});
    let result = (|| -> Result<()> {
        let data = dataset(dataset_path)?;
        let store = PreviewStore::open(data.store.clone(), &[])?;
        ensure!(
            store.usage()?.objects == u64::from(data.count),
            "fixture count mismatch"
        );
        receipt["footprint_before"] = footprint(&data)?;
        for pass in 0..6u64 {
            let indices = if pass < 3 {
                (0..data.count).collect::<Vec<_>>()
            } else {
                shuffled(data.count, 22841 + pass - 3)
            };
            let mut samples = Vec::with_capacity(data.count as usize);
            let mut bytes = 0u64;
            let mut row = json!({"pass":pass,"order":if pass<3{"sequential"}else{"seeded_random"},"seed":if pass<3{None}else{Some(22841+pass-3)},"started":stamp(start),"complete":false});
            let pass_start = Instant::now();
            let measured = (|| -> Result<()> {
                for index in indices {
                    let expected = key(&data, index);
                    let now = Instant::now();
                    let cached = store
                        .read_limited(&expected, false, 8 * 1024 * 1024)?
                        .context("retained object missing")?;
                    samples.push(now.elapsed().as_secs_f64() * 1000.0);
                    ensure!(
                        !cached.stale && cached.key == expected,
                        "stale/wrong layout object"
                    );
                    bytes += cached.bytes.len() as u64;
                }
                Ok(())
            })();
            row["wall_ms"] = json!(pass_start.elapsed().as_secs_f64() * 1000.0);
            row["finished"] = stamp(start);
            row["lookup_count"] = json!(samples.len());
            row["bytes_read_verified"] = json!(bytes);
            if let Err(error) = &measured {
                row["error"] = json!(format!("{error:#}"));
            }
            row["complete"] = json!(measured.is_ok());
            let raw_name = format!("pass-{pass}-samples.json");
            exclusive(&output.join(&raw_name), &serde_json::to_vec(&samples)?)?;
            row["samples_file"] = json!(raw_name);
            receipt["passes"].as_array_mut().unwrap().push(row);
            measured?;
        }
        drop(store);
        receipt["footprint_after"] = footprint(&data)?;
        receipt["complete"] = json!(true);
        Ok(())
    })();
    if let Err(error) = &result {
        receipt["error"] = json!(format!("{error:#}"));
    }
    receipt["finished"] = stamp(start);
    exclusive(
        &output.join("lookup.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    println!("{receipt}");
    result
}
fn main() -> Result<()> {
    match Args::parse().command {
        Command::Prepare {
            campaign,
            output,
            count,
            layout,
        } => prepare(&campaign, &output, count, layout),
        Command::Lookup { dataset, output } => lookup(&dataset, &output),
        Command::Footprint { dataset: path } => {
            println!("{}", footprint(&dataset(&path)?)?);
            Ok(())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_layout_entries_are_byte_distinct_with_identical_decoded_pixels() {
        let rgb = PreparedRgb::new(
            3,
            2,
            vec![
                12, 34, 56, 78, 90, 12, 34, 56, 78, 90, 12, 34, 56, 78, 90, 12, 34, 56,
            ],
        )
        .unwrap();
        let base = encode(
            &rgb,
            CodecSettings {
                codec: Codec::Jpeg,
                quality: 80,
            },
            None,
        )
        .unwrap();
        let reference = decode(&base, Codec::Jpeg).unwrap();
        let mut hashes = HashSet::new();
        for index in [0, 1, 9_999, 99_999] {
            let marked = distinct_jpeg(&base, index).unwrap();
            assert_eq!(marked.len(), base.len() + 37);
            assert!(hashes.insert(blake3::hash(&marked)));
            assert_eq!(
                decode(&marked, Codec::Jpeg).unwrap().pixels(),
                reference.pixels()
            );
        }
    }
    #[test]
    fn navigation_order_seed_is_reproducible_and_covers_each_entry_once() {
        let values = shuffled(100, 22841);
        assert_eq!(values, shuffled(100, 22841));
        let mut sorted = values.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..100).collect::<Vec<_>>());
        assert_ne!(values, shuffled(100, 22842));
    }
}
