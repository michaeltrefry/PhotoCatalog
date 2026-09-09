//! Bounded import-local volume observations. Keep only the current directory.
use crate::storage_volume::{self, LocationState, MountSnapshot, NativePath, VolumeLocation};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) struct ImportVolumes {
    directory: Option<(PathBuf, (u64, u128), Instant, VolumeLocation)>,
    snapshot: MountSnapshot,
    parent_mount: Option<u64>,
}
impl ImportVolumes {
    pub(crate) fn new() -> Self {
        Self {
            directory: None,
            parent_mount: None,
            snapshot: MountSnapshot {
                mounts: vec![],
                complete: false,
                issues: vec![],
            },
        }
    }
    pub(crate) fn snapshot(&self) -> &MountSnapshot {
        &self.snapshot
    }
    pub(crate) fn observe(&mut self, path: &Path) -> Result<VolumeLocation> {
        let parent = path.parent().context("original has no parent")?;
        let key = storage_volume::object_key(parent, &fs::metadata(parent)?)?;
        let parent_mount = mount_instance(parent);
        if !self
            .directory
            .as_ref()
            .is_some_and(|(cached, previous, at, _)| {
                cached == parent
                    && *previous == key
                    && self.parent_mount == parent_mount
                    && at.elapsed() < Duration::from_secs(1)
            })
        {
            let observation = storage_volume::locate(parent);
            self.snapshot =
                storage_volume::mounted_volumes().unwrap_or_else(|error| MountSnapshot {
                    mounts: vec![],
                    complete: false,
                    issues: vec![storage_volume::VolumeIssue {
                        kind: storage_volume::IssueKind::Inaccessible,
                        path: None,
                        detail: error.to_string(),
                    }],
                });
            // Recheck after the native volume query; a replaced directory invalidates it.
            ensure!(
                key == storage_volume::object_key(parent, &fs::metadata(parent)?)?,
                "original directory changed during volume inspection"
            );
            self.directory = Some((parent.to_path_buf(), key, Instant::now(), observation));
            self.parent_mount = parent_mount;
        }
        let (_, _, _, directory) = self.directory.as_ref().context("volume cache missing")?;
        if directory.state != LocationState::Available {
            return Ok(storage_volume::locate(path));
        }
        let metadata = fs::symlink_metadata(path)?;
        ensure!(metadata.is_file(), "original is not a regular file");
        let file_key = storage_volume::object_key(path, &metadata)?;
        // A mount at the file or a concurrent directory change requires a fresh native query.
        if file_key.0 != key.0
            || key != storage_volume::object_key(parent, &fs::metadata(parent)?)?
            || (cfg!(target_os = "linux")
                && (parent_mount.is_none()
                    || mount_instance(path) != parent_mount
                    || mount_instance(parent) != parent_mount))
        {
            self.directory = None;
            return Ok(storage_volume::locate(path));
        }
        let name = path.file_name().context("original has no filename")?;
        let mut observation = directory.clone();
        observation.requested_path = NativePath::from_path(path);
        observation.canonical_path = directory
            .canonical_path
            .as_ref()
            .map(|p| p.to_path().map(|p| NativePath::from_path(&p.join(name))))
            .transpose()?;
        observation.relative_in_volume = directory
            .relative_in_volume
            .as_ref()
            .map(|p| p.to_path().map(|p| NativePath::from_path(&p.join(name))))
            .transpose()?;
        Ok(observation)
    }
}

/// A mount instance is transient evidence for a cache observation, never a volume ID.
#[cfg(target_os = "linux")]
fn mount_instance(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: path is NUL-terminated and stat points to writable storage.
    let result = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID,
            stat.as_mut_ptr(),
        )
    };
    if result != 0 {
        return None;
    }
    // SAFETY: successful statx initialized the structure; check the returned mask.
    let stat = unsafe { stat.assume_init() };
    (stat.stx_mask & libc::STATX_MNT_ID != 0).then_some(stat.stx_mnt_id)
}
#[cfg(not(target_os = "linux"))]
fn mount_instance(_: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observations_are_per_file_and_missing_paths_never_use_cached_volume() -> Result<()> {
        let root = tempfile::tempdir()?;
        let a = root.path().join("a.jpg");
        let b = root.path().join("b.jpg");
        fs::write(&a, b"a")?;
        fs::write(&b, b"b")?;
        let mut cache = ImportVolumes::new();
        let first = cache.observe(&a)?;
        let second = cache.observe(&b)?;
        ensure!(first.requested_path != second.requested_path);
        if let (Some(a), Some(b)) = (first.relative_in_volume, second.relative_in_volume) {
            ensure!(a.to_path()?.file_name() == Some(std::ffi::OsStr::new("a.jpg")));
            ensure!(b.to_path()?.file_name() == Some(std::ffi::OsStr::new("b.jpg")));
        }
        fs::remove_file(&b)?;
        ensure!(cache.observe(&b).is_err());
        Ok(())
    }
}
