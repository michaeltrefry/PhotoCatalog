//! Lightroom evidence capture and inspection. This module never imports assets
//! into the application catalog or executes Adobe develop/plug-in instructions.
pub mod adobe;
pub mod capture;
pub mod discovery;
pub mod plan;
mod source;
pub mod wal;

use crate::storage_volume::NativePath;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub const PROTOCOL: u32 = 1;
pub const MANIFEST_BYTES: usize = 16 * 1024 * 1024;
pub const PAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub max_files: usize,
    pub max_depth: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_cell_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 16_384,
            max_depth: 16,
            max_file_bytes: 8 * 1024 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024 * 1024,
            max_cell_bytes: 32 * 1024 * 1024,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_files > 0 && self.max_depth > 0,
            "invalid inventory limits"
        );
        ensure!(
            self.max_file_bytes > 0 && self.max_total_bytes > 0,
            "invalid byte limits"
        );
        ensure!(
            (1024..=128 * 1024 * 1024).contains(&self.max_cell_bytes),
            "invalid cell limit"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Issue {
    pub code: String,
    pub source_id: Option<String>,
    pub detail: String,
}
impl Issue {
    pub(super) fn new(code: &str, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            source_id: None,
            detail: detail.into(),
        }
    }
}

pub(super) fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
pub(super) fn json_digest<T: Serialize>(value: &T) -> Result<String> {
    Ok(digest(&bounded_json(value, MANIFEST_BYTES)?))
}
pub(super) fn path_value(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(&NativePath::from_path(path))?)
}
pub(super) fn write_new_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::io::Write;
    let bytes = bounded_json(value, MANIFEST_BYTES - 1)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}
pub(super) fn bounded_json<T: Serialize>(value: &T, maximum: usize) -> Result<Vec<u8>> {
    struct Limited {
        bytes: Vec<u8>,
        maximum: usize,
    }
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("serialized JSON byte limit exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Limited {
        bytes: vec![],
        maximum,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}
