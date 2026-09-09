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
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum IdScheme {
    #[default]
    Layout,
    OrganizationFixture,
}
#[derive(Serialize, Deserialize)]
pub(super) struct Dataset {
    #[serde(default)]
    pub(super) id_scheme: IdScheme,
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
    key.asset_id = match dataset.id_scheme {
        IdScheme::Layout => format!("layout-{index:010}"),
        IdScheme::OrganizationFixture => format!("fixture-{:012}", u64::from(index) + 1),
    };
    key
}
pub(super) fn dataset(path: &Path) -> Result<Dataset> {
    let value: Dataset = serde_json::from_slice(&read_bounded(path, 1024 * 1024)?)?;
    ensure!(
        value.version == 1
            && matches!(value.count, 10_000 | 100_000)
            && (value.id_scheme == IdScheme::Layout || value.count == 10_000)
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

pub(super) fn distinct_jpeg(base: &[u8], index: u32) -> Result<Vec<u8>> {
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
pub(super) fn verify_object(bytes: &[u8], index: u32, base_hash: &str) -> Result<[u8; 32]> {
    ensure!(
        bytes.len() > 39 && bytes[..6] == [0xff, 0xd8, 0xff, 0xfe, 0, 35],
        "generated COM header mismatch"
    );
    ensure!(
        &bytes[6..39] == format!("photocatalog-layout-v1:{index:010}").as_bytes(),
        "generated object index mismatch"
    );
    let mut original = blake3::Hasher::new();
    original.update(&bytes[..2]);
    original.update(&bytes[39..]);
    ensure!(
        original.finalize().to_hex().as_str() == base_hash,
        "generated object source payload mismatch"
    );
    Ok(*blake3::hash(bytes).as_bytes())
}
