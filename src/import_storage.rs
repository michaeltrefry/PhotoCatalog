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

#[cfg(all(test, target_os = "linux"))]
mod linux_mount_tests {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

    // The launcher runs only this ignored test in a private mount namespace.
    // Never run mount operations in the caller's existing mount namespace.
    fn require_private_namespace() -> Result<()> {
        let outer = std::env::var_os("PHOTOCATALOG_OUTER_MOUNT_NAMESPACE")
            .context("use scripts/validate_linux_bind_mount.py")?;
        ensure!(fs::read_link("/proc/self/ns/mnt")? != Path::new(&outer));
        for line in fs::read_to_string("/proc/self/mountinfo")?.lines() {
            let (optional, _) = line.split_once(" - ").context("mountinfo separator")?;
            ensure!(
                !optional
                    .split_whitespace()
                    .skip(6)
                    .any(|field| { field.starts_with("shared:") || field.starts_with("master:") }),
                "fixture requires private mount propagation"
            );
        }
        Ok(())
    }

    // Independent kernel interface: this does not call our statx helper or
    // mountinfo parser. An open descriptor's mnt_id proves the crossed boundary.
    fn descriptor_mount(path: &Path) -> Result<u64> {
        let file = fs::File::open(path)?;
        let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
        info.lines()
            .find_map(|line| line.strip_prefix("mnt_id:"))
            .context("descriptor mount ID unavailable")?
            .trim()
            .parse()
            .context("invalid descriptor mount ID")
    }

    struct OwnedBindMount {
        target: CString,
        active: bool,
    }
    impl OwnedBindMount {
        fn attach(source: &Path, target: &Path) -> Result<Self> {
            let source = CString::new(source.as_os_str().as_bytes())?;
            let target = CString::new(target.as_os_str().as_bytes())?;
            // SAFETY: both terminated paths name files created by this test;
            // MS_BIND ignores the null filesystem and data arguments.
            let status = unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                )
            };
            if status != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self {
                target,
                active: true,
            })
        }
        fn detach(&mut self) -> Result<()> {
            // SAFETY: this is the exact owned target successfully mounted above.
            // No recursive, forced, or lazy unmount is used.
            if unsafe { libc::umount(self.target.as_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            self.active = false;
            Ok(())
        }
    }
    impl Drop for OwnedBindMount {
        fn drop(&mut self) {
            if self.active
                && let Err(error) = self.detach()
            {
                eprintln!("owned bind fixture cleanup failed: {error}");
            }
        }
    }

    #[test]
    #[ignore = "requires launcher-owned private Linux mount namespace and CAP_SYS_ADMIN"]
    fn same_device_file_bind_mount_bypasses_parent_cache() -> Result<()> {
        require_private_namespace()?;
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let source = root.join("source.jpg");
        let target = root.join("target.jpg");
        let neighbor = root.join("neighbor.jpg");
        fs::write(&source, b"owned bind source")?;
        fs::write(&target, b"owned underlying target")?;
        fs::write(&neighbor, b"owned neighbor")?;
        let parent_key = storage_volume::object_key(&root, &fs::metadata(&root)?)?;
        let source_before = storage_volume::locate(&source);
        ensure!(source_before.state == LocationState::Available);
        let source_relative = source_before
            .relative_in_volume
            .context("source relative path")?;
        let parent_mount = descriptor_mount(&root)?;
        ensure!(
            mount_instance(&root) == Some(parent_mount),
            "statx MNT_ID required"
        );
        ensure!(descriptor_mount(&target)? == parent_mount);

        let mut cache = ImportVolumes::new();
        let ordinary = cache.observe(&neighbor)?;
        ensure!(ordinary.state == LocationState::Available);
        ensure!(cache.directory.is_some(), "parent cache must be populated");
        let mut mounted = OwnedBindMount::attach(&source, &target)?;
        ensure!(fs::metadata(&target)?.dev() == fs::metadata(&root)?.dev());
        ensure!(parent_key == storage_volume::object_key(&root, &fs::metadata(&root)?)?);
        let target_mount = descriptor_mount(&target)?;
        ensure!(
            target_mount != parent_mount,
            "fixture did not create a file mount"
        );
        ensure!(mount_instance(&target) == Some(target_mount));
        ensure!(fs::read(&target)? == b"owned bind source");

        let bound = cache.observe(&target)?;
        ensure!(bound.state == LocationState::Available, "{bound:?}");
        // Expected mount path comes from the mount action, not the parser.
        let volume = bound.volume.as_ref().context("bound volume")?;
        ensure!(
            volume.mount_path.to_path()? == target,
            "cached parent leaked: {bound:?}"
        );
        ensure!(bound.relative_in_volume.as_ref() == Some(&source_relative));
        ensure!(volume.volume_subpath == source_relative);
        ensure!(
            bound
                .canonical_path
                .as_ref()
                .context("canonical path")?
                .to_path()?
                == target
        );
        ensure!(
            cache.directory.is_none(),
            "file boundary must invalidate parent cache"
        );
        ensure!(storage_volume::candidate_path(volume, &source_relative)? == target);
        let again = cache.observe(&neighbor)?;
        ensure!(again.relative_in_volume == ordinary.relative_in_volume);
        ensure!(again.volume.as_ref().context("neighbor volume")?.mount_path != volume.mount_path);
        ensure!(fs::read(&source)? == b"owned bind source");
        ensure!(fs::read(&neighbor)? == b"owned neighbor");
        // Verify ownership again before detaching this exact fixture target.
        ensure!(descriptor_mount(&target)? == target_mount);
        mounted.detach()?;
        ensure!(descriptor_mount(&target)? == parent_mount);
        ensure!(fs::read(&target)? == b"owned underlying target");
        let restored = cache.observe(&target)?;
        ensure!(
            restored
                .volume
                .as_ref()
                .context("restored volume")?
                .mount_path
                != NativePath::from_path(&target)
        );
        ensure!(restored.relative_in_volume != Some(source_relative));
        ensure!(fs::read_dir(&root)?.count() == 3);
        println!(
            "native same-device file bind mount: cache bypass, mapping, source preservation and detach passed"
        );
        Ok(())
    }
}
