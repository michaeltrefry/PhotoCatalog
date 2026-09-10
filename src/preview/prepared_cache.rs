//! Rebuildable, service-owned linear proxy files. No float pixels live in the
//! service; loading/rendering remains inside the admitted child. Startup clears
//! this transient cache; retained compressed previews are independent.
use crate::{
    edit::{PreparedProxyReceipt, WhiteBalance},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

pub(crate) const PROXY_EDGE: u32 = 1600;
pub(crate) const MAX_PROXY_BYTES: u64 = 1600 * 1600 * 16 + 128 * 1024 + 64;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceInstance {
    path: NativePath,
    device: u64,
    object: u128,
    bytes: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    changed: Option<(i64, i64)>,
}
impl SourceInstance {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        let canonical = fs::canonicalize(path)?;
        let metadata = fs::metadata(&canonical)?;
        ensure!(metadata.is_file(), "prepared input source is not a file");
        let (device, object) = crate::storage_volume::object_key(&canonical, &metadata)?;
        #[cfg(unix)]
        let changed = {
            use std::os::unix::fs::MetadataExt;
            Some((metadata.ctime(), metadata.ctime_nsec()))
        };
        #[cfg(not(unix))]
        let changed = None;
        Ok(Self {
            path: NativePath::from_path(&canonical),
            device,
            object,
            bytes: metadata.len(),
            modified: metadata.modified()?,
            created: metadata.created().ok(),
            changed,
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PreparedReference {
    pub path: NativePath,
    pub receipt: PreparedProxyReceipt,
    pub source: SourceInstance,
}
pub(crate) struct ProducedPrepared {
    pub path: PathBuf,
    pub receipt: PreparedProxyReceipt,
    pub source: SourceInstance,
}
struct Entry {
    value: PreparedReference,
    used: u64,
}
pub(crate) struct PreparedCache {
    root: PathBuf,
    entries: HashMap<String, Entry>,
    clock: u64,
    bytes: u64,
    limit: u64,
    count: usize,
}
fn identity(generation: u64, fingerprint: &str, wb: &WhiteBalance) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(&(
        generation,
        fingerprint,
        wb,
        crate::edit::renderer_identity(),
        PROXY_EDGE,
    ))?)
    .to_hex()
    .to_string())
}
/// Stream verification never allocates the container. The child independently
/// validates header identity, float pixels, checksum and exact EOF on every hit.
pub(crate) fn verify_file(path: &Path, receipt: &PreparedProxyReceipt, cap: u64) -> Result<()> {
    ensure!(
        receipt.bytes <= cap && receipt.bytes > 0,
        "prepared cache byte allowance"
    );
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == receipt.bytes,
        "invalid prepared cache file"
    );
    let mut file = File::open(path)?;
    let mut remaining = receipt.bytes;
    let mut buffer = [0; 65536];
    let mut hash = blake3::Hasher::new();
    while remaining > 0 {
        let length = usize::try_from(remaining.min(buffer.len() as u64))?;
        file.read_exact(&mut buffer[..length])?;
        hash.update(&buffer[..length]);
        remaining -= length as u64;
    }
    ensure!(
        file.read(&mut buffer[..1])? == 0 && hash.finalize().to_hex().as_str() == receipt.blake3,
        "prepared cache checksum changed"
    );
    Ok(())
}
impl PreparedCache {
    pub(crate) fn open(root: PathBuf, limit: u64, count: usize) -> Result<Self> {
        ensure!(
            count <= 1024 && limit <= 16 * 1024 * 1024 * 1024,
            "prepared cache configuration"
        );
        fs::create_dir_all(&root)?;
        ensure!(
            !fs::symlink_metadata(&root)?.file_type().is_symlink(),
            "prepared root symlink"
        );
        // The exclusive preview-manifest owner is the sole writer here. Active
        // child files remain in the existing worker lease directories instead.
        let paths = fs::read_dir(&root)?
            .take(1025)
            .collect::<std::io::Result<Vec<_>>>()?;
        ensure!(paths.len() <= 1024, "prepared recovery entry bound");
        for entry in &paths {
            let name = entry.file_name();
            let name = name.to_str().context("prepared filename")?;
            let stem = name
                .strip_suffix(".linear")
                .context("unknown prepared cache artifact")?;
            ensure!(
                stem.len() == 64
                    && stem.bytes().all(|x| x.is_ascii_hexdigit())
                    && entry.file_type()?.is_file(),
                "unknown prepared cache artifact"
            );
        }
        for entry in paths {
            fs::remove_file(entry.path())?;
        }
        Ok(Self {
            root,
            entries: HashMap::new(),
            clock: 0,
            bytes: 0,
            limit,
            count,
        })
    }
    pub(crate) fn lookup(
        &mut self,
        source: &Path,
        generation: u64,
        fingerprint: &str,
        wb: &WhiteBalance,
    ) -> Result<Option<PreparedReference>> {
        let key = identity(generation, fingerprint, wb)?;
        let Some(entry) = self.entries.get_mut(&key) else {
            return Ok(None);
        };
        if entry.value.source != SourceInstance::read(source)? {
            return Ok(None);
        }
        // Verification runs in the child under the worker allowance, not on the
        // actor's foreground queue. A bad file is a cache miss in that child.
        self.clock = self.clock.checked_add(1).context("prepared LRU overflow")?;
        entry.used = self.clock;
        Ok(Some(entry.value.clone()))
    }
    pub(crate) fn adopt(&mut self, generation: u64, produced: &ProducedPrepared) -> Result<()> {
        let receipt = &produced.receipt;
        if self.count == 0 || receipt.bytes > self.limit {
            return Ok(());
        }
        verify_file(&produced.path, receipt, MAX_PROXY_BYTES)?;
        let key = identity(
            generation,
            &receipt.identity.source_fingerprint,
            &receipt.identity.white_balance,
        )?;
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.value.receipt.bytes;
            fs::remove_file(old.value.path.to_path()?)?;
        }
        while self.entries.len() >= self.count || self.bytes + receipt.bytes > self.limit {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
                .context("prepared eviction")?;
            let old = self.entries.remove(&oldest).unwrap();
            self.bytes -= old.value.receipt.bytes;
            fs::remove_file(old.value.path.to_path()?)?;
        }
        let path = self.root.join(format!("{key}.linear"));
        fs::hard_link(&produced.path, &path)?;
        self.clock = self.clock.checked_add(1).context("prepared LRU overflow")?;
        self.bytes += receipt.bytes;
        self.entries.insert(
            key,
            Entry {
                value: PreparedReference {
                    path: NativePath::from_path(&path),
                    receipt: receipt.clone(),
                    source: produced.source.clone(),
                },
                used: self.clock,
            },
        );
        Ok(())
    }
}
