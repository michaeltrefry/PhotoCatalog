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
        if self.entries.contains_key(&key) {
            self.remove_entry(&key, |path| fs::remove_file(path))?;
        }
        while self.entries.len() >= self.count || self.bytes + receipt.bytes > self.limit {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
                .context("prepared eviction")?;
            self.remove_entry(&oldest, |path| fs::remove_file(path))?;
        }
        let path = self.root.join(format!("{key}.linear"));
        let clock = self.clock.checked_add(1).context("prepared LRU overflow")?;
        fs::hard_link(&produced.path, &path)?;
        self.clock = clock;
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
    fn remove_entry(
        &mut self,
        key: &str,
        remove: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> Result<()> {
        let old = self.entries.get(key).context("prepared entry missing")?;
        match remove(&old.value.path.to_path()?) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // A failed unlink must retain both the entry and its disk charge.
        let old = self.entries.remove(key).unwrap();
        self.bytes -= old.value.receipt.bytes;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::PreparedProxyIdentity;

    fn produced(root: &Path) -> ProducedPrepared {
        let path = root.join("worker-output.linear");
        // Storage accounting fixture; container decoding has separate tests.
        let bytes = b"bounded proxy storage fixture";
        fs::write(&path, bytes).unwrap();
        ProducedPrepared {
            source: SourceInstance::read(&path).unwrap(),
            path,
            receipt: PreparedProxyReceipt {
                bytes: bytes.len() as u64,
                blake3: blake3::hash(bytes).to_hex().to_string(),
                identity: PreparedProxyIdentity {
                    source_fingerprint: "a".repeat(64),
                    white_balance: WhiteBalance::AsShot,
                    renderer_identity: crate::edit::renderer_identity().into(),
                    original_dimensions: (1, 1),
                    longest_edge: PROXY_EDGE,
                    width: 1,
                    height: 1,
                },
            },
        }
    }
    #[test]
    fn failed_unlink_keeps_disk_charge_and_entry_until_replacement_or_eviction_retries() {
        for next_generation in [1, 2] {
            let root = tempfile::tempdir().unwrap();
            let produced = produced(root.path());
            let mut cache =
                PreparedCache::open(root.path().join("cache"), produced.receipt.bytes, 1).unwrap();
            cache.adopt(1, &produced).unwrap();
            let key = identity(1, &"a".repeat(64), &WhiteBalance::AsShot).unwrap();
            let old_path = cache.entries[&key].value.path.to_path().unwrap();
            let error = cache
                .remove_entry(&key, |_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected unlink refusal",
                    ))
                })
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert_eq!(cache.bytes, produced.receipt.bytes);
            assert_eq!(cache.entries.len(), 1);
            assert!(cache.entries.contains_key(&key));
            assert!(old_path.is_file());
            cache.adopt(next_generation, &produced).unwrap();
            assert_eq!(cache.bytes, produced.receipt.bytes);
            assert_eq!(cache.entries.len(), 1);
            let next_key =
                identity(next_generation, &"a".repeat(64), &WhiteBalance::AsShot).unwrap();
            assert!(cache.entries.contains_key(&next_key));
            assert_eq!(fs::read_dir(&cache.root).unwrap().count(), 1);
        }
    }
    #[test]
    fn missing_cache_file_releases_its_charge_and_allows_readmission() {
        let root = tempfile::tempdir().unwrap();
        let produced = produced(root.path());
        let mut cache =
            PreparedCache::open(root.path().join("cache"), produced.receipt.bytes, 1).unwrap();
        cache.adopt(1, &produced).unwrap();
        let key = identity(1, &"a".repeat(64), &WhiteBalance::AsShot).unwrap();
        fs::remove_file(cache.entries[&key].value.path.to_path().unwrap()).unwrap();
        cache.adopt(1, &produced).unwrap();
        assert_eq!(cache.bytes, produced.receipt.bytes);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(fs::read_dir(&cache.root).unwrap().count(), 1);
    }
}
