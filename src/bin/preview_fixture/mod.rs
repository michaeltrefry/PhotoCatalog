//! Shared, bounded schema for synthetic retained-cache evidence only.
use anyhow::{Result, ensure};
use photocatalog::preview::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
#[derive(Serialize, Deserialize)]
pub(super) struct Seed {
    pub(super) key: PreviewKey,
    pub(super) encoded_path: PathBuf,
    pub(super) encoded_blake3: String,
    pub(super) decoded_blake3: String,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) record: RenderRecord,
}
#[derive(Serialize, Deserialize)]
pub(super) struct Dataset {
    pub(super) version: u32,
    pub(super) count: u32,
    pub(super) store: StoreConfig,
    pub(super) seeds: Vec<Seed>,
    pub(super) marker_overhead_bytes: u32,
}
pub(super) fn key(dataset: &Dataset, index: u32) -> PreviewKey {
    let mut key = dataset.seeds[index as usize % dataset.seeds.len()]
        .key
        .clone();
    key.asset_id = format!("layout-{index:010}");
    key
}
pub(super) fn dataset(path: &Path) -> Result<Dataset> {
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
pub(super) fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    ensure!(length > 0 && length <= limit, "fixture byte limit");
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)?;
    ensure!(file.read(&mut [0])? == 0, "fixture grew");
    Ok(bytes)
}
