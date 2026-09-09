//! Read-only volume identities and mount-relative paths. OS identities are
//! evidence for reconnection, never asset IDs or proof of file-content equality.
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "units")]
pub enum NativePath {
    UnixBytes(Vec<u8>),
    WindowsWide(Vec<u16>),
}
impl NativePath {
    pub fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self::UnixBytes(path.as_os_str().as_bytes().to_vec())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self::WindowsWide(path.as_os_str().encode_wide().collect())
        }
    }
    /// Foreign-platform paths stay lossless in storage; they require an explicit
    /// mapping, not lossy conversion or interpretation on the current platform.
    pub fn to_path(&self) -> io::Result<PathBuf> {
        match self {
            #[cfg(unix)]
            Self::UnixBytes(bytes) if !bytes.contains(&0) => {
                use std::os::unix::ffi::OsStringExt;
                Ok(std::ffi::OsString::from_vec(bytes.clone()).into())
            }
            #[cfg(windows)]
            Self::WindowsWide(units) if !units.contains(&0) => {
                use std::os::windows::ffi::OsStringExt;
                Ok(std::ffi::OsString::from_wide(units).into())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "foreign native-path encoding or embedded NUL",
            )),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum IdentityScheme {
    MacVolumeUuid,
    WindowsVolumeGuid,
    LinuxFilesystemUuid,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PersistentVolumeId {
    pub scheme: IdentityScheme,
    pub value: String,
}
impl PersistentVolumeId {
    pub fn new(scheme: IdentityScheme, value: &str) -> io::Result<Self> {
        if value.is_empty()
            || value.len() > 256
            || !value.is_ascii()
            || value
                .bytes()
                .any(|b| b.is_ascii_control() || b == b'/' || b == b'\\')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid persistent volume identifier",
            ));
        }
        let value = match scheme {
            IdentityScheme::MacVolumeUuid | IdentityScheme::WindowsVolumeGuid => {
                uuid::Uuid::parse_str(value)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid volume UUID"))?
                    .to_string()
            }
            IdentityScheme::LinuxFilesystemUuid => value.to_owned(),
        };
        Ok(Self { scheme, value })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalVolume {
    /// Generated once and persisted by the catalog; never derived from mount path.
    pub id: String,
    pub persistent_identity: Option<PersistentVolumeId>,
}
impl LogicalVolume {
    pub fn new(persistent_identity: Option<PersistentVolumeId>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            persistent_identity,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueKind {
    IdentityUnavailable,
    IdentityAmbiguous,
    MappingUnavailable,
    ObservationChanged,
    Inaccessible,
    Malformed,
    ResourceLimit,
    Unsupported,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeIssue {
    pub kind: IssueKind,
    pub path: Option<NativePath>,
    pub detail: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceNumber {
    pub major: u32,
    pub minor: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountedVolume {
    pub mount_path: NativePath,
    /// Filesystem-internal subdirectory mounted here (Linux bind/subvolume roots).
    /// Empty for a whole-volume root. Always a relative path.
    pub volume_subpath: NativePath,
    pub persistent_identity: Option<PersistentVolumeId>,
    pub filesystem: String,
    /// Current Linux device number only; never a persistent identity.
    pub device_number: Option<DeviceNumber>,
    pub issues: Vec<VolumeIssue>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSnapshot {
    pub mounts: Vec<MountedVolume>,
    /// False means enumeration failed or was bounded/truncated; no match then
    /// means indeterminate, never proof that a registered volume is offline.
    pub complete: bool,
    pub issues: Vec<VolumeIssue>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocationState {
    Available,
    MissingPath,
    Inaccessible,
    Unsupported,
    Changed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExistingAncestor {
    pub path: NativePath,
    pub canonical_path: NativePath,
    pub volume: MountedVolume,
    pub relative_in_volume: Option<NativePath>,
    /// Lexical missing suffix, not proof that this suffix belongs to this volume.
    pub unresolved_suffix: NativePath,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeLocation {
    pub requested_path: NativePath,
    pub state: LocationState,
    pub canonical_path: Option<NativePath>,
    pub volume: Option<MountedVolume>,
    /// None explicitly disables automatic mount-relative path reconstruction.
    pub relative_in_volume: Option<NativePath>,
    pub existing_ancestor: Option<ExistingAncestor>,
    pub issues: Vec<VolumeIssue>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MountMatch {
    IdentityUnavailable,
    Offline,
    Indeterminate,
    Unique(MountedVolume),
    Ambiguous(Vec<MountedVolume>),
}
/// Pure snapshot matching. Duplicate UUIDs/mount aliases remain ambiguous;
/// labels, prior paths and transient device IDs never break a tie.
pub fn match_mounts(volume: &LogicalVolume, snapshot: &MountSnapshot) -> MountMatch {
    let Some(identity) = &volume.persistent_identity else {
        return MountMatch::IdentityUnavailable;
    };
    let found: Vec<_> = snapshot
        .mounts
        .iter()
        .filter(|mount| mount.persistent_identity.as_ref() == Some(identity))
        .cloned()
        .collect();
    match found.len() {
        0 if snapshot.complete => MountMatch::Offline,
        0 => MountMatch::Indeterminate,
        1 if snapshot.complete => MountMatch::Unique(found[0].clone()),
        1 => MountMatch::Indeterminate,
        _ => MountMatch::Ambiguous(found),
    }
}
/// Build a candidate only; callers must re-probe its volume and verify original
/// content/revisions before committing a relink. This does not read the file.
pub fn candidate_path(mount: &MountedVolume, relative: &NativePath) -> io::Result<PathBuf> {
    let root = mount.mount_path.to_path()?;
    let subpath = mount.volume_subpath.to_path()?;
    let relative = relative.to_path()?;
    if !root.is_absolute() || !safe_relative(&subpath) || !safe_relative(&relative) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid mount-relative path",
        ));
    }
    let suffix = relative.strip_prefix(&subpath).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "path is outside this mounted subdirectory",
        )
    })?;
    // Joining an empty suffix adds a trailing separator. A file bind mount is
    // itself the requested object, and a trailing separator makes it unusable.
    Ok(if suffix.as_os_str().is_empty() {
        root
    } else {
        root.join(suffix)
    })
}
fn safe_relative(path: &Path) -> bool {
    path.components().all(|c| matches!(c, Component::Normal(_)))
}
fn issue(kind: IssueKind, path: Option<&Path>, detail: impl Into<String>) -> VolumeIssue {
    VolumeIssue {
        kind,
        path: path.map(NativePath::from_path),
        detail: detail.into(),
    }
}
fn io_issue(kind: IssueKind, path: &Path, error: &io::Error) -> VolumeIssue {
    issue(
        kind,
        Some(path),
        format!("{:?}; os_error={:?}", error.kind(), error.raw_os_error()),
    )
}
/// Observe actual mounted filesystems. OS calls can block on an unresponsive
/// remote filesystem; the caller must use its cancellable worker boundary.
pub fn mounted_volumes() -> io::Result<MountSnapshot> {
    platform::mounted_volumes()
}
/// Inspect an existing selected root/file. A missing path is never assigned its
/// nearest existing ancestor's volume (which could be the boot disk after unmount).
pub fn locate(path: &Path) -> VolumeLocation {
    let mut result = VolumeLocation {
        requested_path: NativePath::from_path(path),
        state: LocationState::Inaccessible,
        canonical_path: None,
        volume: None,
        relative_in_volume: None,
        existing_ancestor: None,
        issues: vec![],
    };
    if !valid_absolute(path) {
        result.issues.push(issue(
            IssueKind::Malformed,
            Some(path),
            "volume inspection requires an absolute path",
        ));
        return result;
    }
    let before = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() || metadata.is_dir() => metadata,
        Ok(_) => {
            result.state = LocationState::Unsupported;
            result.issues.push(issue(
                IssueKind::Unsupported,
                Some(path),
                "special filesystem object",
            ));
            return result;
        }
        Err(error) => {
            result.state = if error.kind() == io::ErrorKind::NotFound {
                LocationState::MissingPath
            } else {
                LocationState::Inaccessible
            };
            result
                .issues
                .push(io_issue(IssueKind::Inaccessible, path, &error));
            if result.state == LocationState::MissingPath {
                result.existing_ancestor = existing_ancestor(path);
                match fs::metadata(path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    _ => {
                        result.state = LocationState::Changed;
                        result.existing_ancestor = None;
                    }
                }
            }
            return result;
        }
    };
    let before_key = match object_key(path, &before) {
        Ok(key) => key,
        Err(error) => {
            result
                .issues
                .push(io_issue(IssueKind::Inaccessible, path, &error));
            return result;
        }
    };
    let canonical = match fs::canonicalize(path) {
        Ok(value) => value,
        Err(error) => {
            result
                .issues
                .push(io_issue(IssueKind::Inaccessible, path, &error));
            return result;
        }
    };
    if !fs::metadata(&canonical)
        .and_then(|metadata| object_key(&canonical, &metadata))
        .is_ok_and(|key| key == before_key)
    {
        result.state = LocationState::Changed;
        result.issues.push(issue(
            IssueKind::ObservationChanged,
            Some(path),
            "canonical target changed during observation",
        ));
        return result;
    }
    let (volume, relative) = match platform::locate(&canonical) {
        Ok(value) => value,
        Err(error) => {
            result
                .issues
                .push(io_issue(IssueKind::Inaccessible, path, &error));
            return result;
        }
    };
    let unchanged = fs::metadata(path)
        .and_then(|after| object_key(path, &after))
        .is_ok_and(|key| key == before_key)
        && fs::metadata(&canonical)
            .and_then(|after| object_key(&canonical, &after))
            .is_ok_and(|key| key == before_key);
    if !unchanged {
        result.state = LocationState::Changed;
        result.issues.push(issue(
            IssueKind::ObservationChanged,
            Some(path),
            "selected filesystem object changed during volume observation",
        ));
        return result;
    }
    result.state = LocationState::Available;
    result.canonical_path = Some(NativePath::from_path(&canonical));
    result.relative_in_volume = relative.map(|value| NativePath::from_path(&value));
    result.issues.extend(volume.issues.iter().cloned());
    result.volume = Some(volume);
    result
}
fn valid_absolute(path: &Path) -> bool {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    #[cfg(windows)]
    {
        use std::path::Prefix;
        match path.components().next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(_)
                | Prefix::VerbatimDisk(_)
                | Prefix::UNC(_, _)
                | Prefix::VerbatimUNC(_, _) => {}
                Prefix::Verbatim(_) => {
                    use std::os::windows::ffi::OsStrExt;
                    if parse_windows_volume_guid_path(
                        &path.as_os_str().encode_wide().collect::<Vec<_>>(),
                    )
                    .is_err()
                    {
                        return false;
                    }
                }
                _ => return false,
            },
            _ => return false,
        }
    }
    true
}
fn existing_ancestor(path: &Path) -> Option<ExistingAncestor> {
    for parent in path.ancestors().skip(1) {
        match fs::metadata(parent) {
            Ok(metadata) if metadata.is_dir() => {
                let suffix = path.strip_prefix(parent).ok()?;
                if !safe_relative(suffix) {
                    return None;
                }
                let observed = locate(parent);
                if observed.state != LocationState::Available {
                    return None;
                }
                return Some(ExistingAncestor {
                    path: NativePath::from_path(parent),
                    canonical_path: observed.canonical_path?,
                    volume: observed.volume?,
                    relative_in_volume: observed.relative_in_volume,
                    unresolved_suffix: NativePath::from_path(suffix),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            _ => return None,
        }
    }
    None
}
pub(crate) fn object_key(_path: &Path, metadata: &fs::Metadata) -> io::Result<(u64, u128)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((metadata.dev(), u128::from(metadata.ino())))
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        platform::object_key(_path)
    }
}
#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
fn relative_path(canonical: &Path, mount: &MountedVolume) -> io::Result<PathBuf> {
    let root = mount.mount_path.to_path()?;
    let subpath = mount.volume_subpath.to_path()?;
    let relative = canonical.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "canonical path does not lie below observed mount",
        )
    })?;
    if !safe_relative(&subpath) || !safe_relative(relative) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe relative volume path",
        ));
    }
    Ok(if relative.as_os_str().is_empty() {
        subpath
    } else {
        subpath.join(relative)
    })
}

#[cfg(test)]
mod exact_mount_tests {
    use super::*;

    #[test]
    fn file_mount_root_retains_exact_volume_relative_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("mounted.cr2");
        let source = Path::new("original/image.cr2");
        let mount = MountedVolume {
            mount_path: NativePath::from_path(&file),
            volume_subpath: NativePath::from_path(source),
            persistent_identity: None,
            filesystem: "synthetic".into(),
            device_number: None,
            issues: vec![],
        };
        let relative = relative_path(&file, &mount).unwrap();
        // Path equality normalizes separators; compare the stored locator bytes.
        assert_eq!(relative.as_os_str(), source.as_os_str());
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::ffi::{CString, c_void};
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;
    const MAX_MOUNTS: usize = 4096;
    type Cf = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFURLVolumeUUIDStringKey: Cf;
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: Cf,
            bytes: *const u8,
            len: isize,
            directory: u8,
        ) -> Cf;
        fn CFURLCopyResourcePropertyForKey(url: Cf, key: Cf, value: *mut Cf, error: *mut Cf) -> u8;
        fn CFStringGetCString(value: Cf, buffer: *mut i8, size: isize, encoding: u32) -> u8;
        fn CFGetTypeID(value: Cf) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFRelease(value: Cf);
    }
    // Foundation supplies NSURL resource-property support on macOS.
    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {}
    struct OwnedCf(Cf);
    impl Drop for OwnedCf {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }
    fn volume_uuid(path: &Path) -> io::Result<Option<PersistentVolumeId>> {
        let bytes = path.as_os_str().as_bytes();
        let url = OwnedCf(unsafe {
            CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                bytes.as_ptr(),
                bytes.len() as isize,
                1,
            )
        });
        if url.0.is_null() {
            return Err(io::Error::other("CFURL filesystem URL creation failed"));
        }
        let mut value = std::ptr::null();
        let mut error = std::ptr::null();
        let success = unsafe {
            CFURLCopyResourcePropertyForKey(
                url.0,
                kCFURLVolumeUUIDStringKey,
                &mut value,
                &mut error,
            )
        };
        let value = OwnedCf(value);
        let _error = OwnedCf(error);
        if success == 0 {
            return Err(io::Error::other(
                "CFURL persistent-volume-UUID query failed",
            ));
        }
        if value.0.is_null() {
            return Ok(None);
        }
        if unsafe { CFGetTypeID(value.0) != CFStringGetTypeID() } {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "volume UUID property is not a string",
            ));
        }
        let mut buffer = [0i8; 257];
        if unsafe {
            CFStringGetCString(
                value.0,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
                0x0800_0100,
            )
        } == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "volume UUID string exceeds bound or UTF-8 conversion failed",
            ));
        }
        let end = buffer
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unterminated UUID"))?;
        let bytes: Vec<_> = buffer[..end].iter().map(|b| *b as u8).collect();
        let value = std::str::from_utf8(&bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 volume UUID"))?;
        if value.is_empty() {
            return Ok(None);
        }
        PersistentVolumeId::new(IdentityScheme::MacVolumeUuid, value).map(Some)
    }
    fn c_bytes(bytes: &[i8]) -> io::Result<Vec<u8>> {
        let end = bytes.iter().position(|b| *b == 0).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "unterminated mount field")
        })?;
        Ok(bytes[..end].iter().map(|b| *b as u8).collect())
    }
    fn stat(path: &Path) -> io::Result<libc::statfs> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
        let mut value = MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(path.as_ptr(), value.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { value.assume_init() })
    }
    fn fsid(value: &libc::statfs) -> [i32; 2] {
        // Darwin fsid_t is exactly two int32_t values; transmute also enforces
        // the ABI size. libc keeps the fields private without extra_traits.
        unsafe { std::mem::transmute(value.f_fsid) }
    }
    fn mounted(value: &libc::statfs) -> io::Result<MountedVolume> {
        let path = NativePath::UnixBytes(c_bytes(&value.f_mntonname)?);
        let native = path.to_path()?;
        let mut issues = Vec::new();
        let persistent_identity = match volume_uuid(&native) {
            Ok(Some(id)) => Some(id),
            Ok(None) => {
                issues.push(issue(
                    IssueKind::IdentityUnavailable,
                    Some(&native),
                    "filesystem provides no persistent volume UUID",
                ));
                None
            }
            Err(error) => {
                issues.push(io_issue(IssueKind::Inaccessible, &native, &error));
                None
            }
        };
        Ok(MountedVolume {
            mount_path: path,
            volume_subpath: NativePath::UnixBytes(vec![]),
            persistent_identity,
            filesystem: String::from_utf8_lossy(&c_bytes(&value.f_fstypename)?).into_owned(),
            device_number: None,
            issues,
        })
    }
    pub fn mounted_volumes() -> io::Result<MountSnapshot> {
        let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let capacity = (count as usize + 16).min(MAX_MOUNTS);
        let mut data = vec![MaybeUninit::<libc::statfs>::uninit(); capacity];
        let bytes = capacity
            .checked_mul(std::mem::size_of::<libc::statfs>())
            .ok_or_else(|| io::Error::other("mount buffer overflow"))?;
        let count =
            unsafe { libc::getfsstat(data.as_mut_ptr().cast(), bytes as i32, libc::MNT_NOWAIT) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut result = MountSnapshot {
            mounts: vec![],
            complete: (count as usize) < capacity,
            issues: vec![],
        };
        if !result.complete {
            result.issues.push(issue(
                IssueKind::ResourceLimit,
                None,
                "mount snapshot may be truncated or changed; refresh required",
            ));
        }
        for value in data.iter().take((count as usize).min(capacity)) {
            match mounted(unsafe { value.assume_init_ref() }) {
                Ok(mount) => {
                    result.complete &= !mount
                        .issues
                        .iter()
                        .any(|i| i.kind == IssueKind::Inaccessible);
                    result.mounts.push(mount);
                }
                Err(error) => {
                    result.complete = false;
                    result
                        .issues
                        .push(issue(IssueKind::Malformed, None, error.to_string()));
                }
            }
        }
        Ok(result)
    }
    pub fn locate(path: &Path) -> io::Result<(MountedVolume, Option<PathBuf>)> {
        let before = stat(path)?;
        let mut mount = mounted(&before)?;
        let relative = match relative_path(path, &mount) {
            Ok(relative) => Some(relative),
            Err(_) => {
                // APFS firmlinks can expose /Users on the Data volume without
                // the physical mount path as a lexical prefix. Verify an alias;
                // never hardcode '/Users' or claim the boot volume by path shape.
                let root = mount.mount_path.to_path()?;
                let candidate = root.join(path.strip_prefix("/").map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "nonabsolute canonical path")
                })?);
                match (
                    fs::metadata(path),
                    fs::metadata(&candidate),
                    stat(&candidate),
                ) {
                    (Ok(a), Ok(b), Ok(candidate_fs))
                        if same_file(&a, &b) && fsid(&before) == fsid(&candidate_fs) =>
                    {
                        relative_path(&candidate, &mount).ok()
                    }
                    _ => None,
                }
            }
        };
        if relative.is_none() {
            mount.issues.push(issue(
                IssueKind::MappingUnavailable,
                Some(path),
                "no verified path within physical mount; explicit relink required",
            ));
        }
        let after = stat(path)?;
        if fsid(&before) != fsid(&after) || before.f_mntonname != after.f_mntonname {
            return Err(io::Error::other("mount changed during volume observation"));
        }
        Ok((mount, relative))
    }
}

const MAX_MOUNTINFO_BYTES: usize = 4 * 1024 * 1024;
const MAX_MOUNT_ENTRIES: usize = 4096;
fn mount_unescape(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position] == b'\\' {
            let digits = bytes.get(position + 1..position + 4).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "truncated mountinfo escape")
            })?;
            let value = match digits {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid mountinfo escape",
                    ));
                }
            };
            result.push(value);
            position += 4;
        } else {
            result.push(bytes[position]);
            position += 1;
        }
    }
    if result.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "NUL in mountinfo",
        ));
    }
    Ok(result)
}
/// Pure, bounded Linux mountinfo parser, available on every host for fixtures.
/// The result has no persistent IDs until the native UUID/device lookup binds it.
pub fn parse_linux_mountinfo(bytes: &[u8]) -> MountSnapshot {
    let mut result = MountSnapshot {
        mounts: vec![],
        complete: true,
        issues: vec![],
    };
    if bytes.len() > MAX_MOUNTINFO_BYTES {
        result.complete = false;
        result.issues.push(issue(
            IssueKind::ResourceLimit,
            None,
            "mountinfo byte limit exceeded",
        ));
        return result;
    }
    for (line_index, line) in bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if line_index >= MAX_MOUNT_ENTRIES {
            result.complete = false;
            result.issues.push(issue(
                IssueKind::ResourceLimit,
                None,
                "mountinfo entry limit exceeded",
            ));
            break;
        }
        let parsed = (|| -> io::Result<MountedVolume> {
            let fields: Vec<_> = line
                .split(|b| *b == b' ')
                .filter(|v| !v.is_empty())
                .collect();
            let dash = fields.iter().position(|v| *v == b"-").ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "mountinfo separator missing")
            })?;
            if dash < 6 || fields.len() < dash + 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "mountinfo fields missing",
                ));
            }
            let number = |value: &[u8]| -> io::Result<u32> {
                std::str::from_utf8(value)
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid mount device number")
                    })
            };
            let _mount_id = number(fields[0])?;
            let _parent_id = number(fields[1])?;
            let device: Vec<_> = fields[2].split(|b| *b == b':').collect();
            if device.len() != 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid mount device pair",
                ));
            }
            let root = mount_unescape(fields[3])?;
            let mount = mount_unescape(fields[4])?;
            if !root.starts_with(b"/")
                || !mount.starts_with(b"/")
                || root.starts_with(b"//")
                || mount.starts_with(b"//")
                || root.split(|b| *b == b'/').any(|p| p == b".." || p == b".")
                || mount.split(|b| *b == b'/').any(|p| p == b".." || p == b".")
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid absolute mount/root path",
                ));
            }
            let filesystem = std::str::from_utf8(fields[dash + 1])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid filesystem type"))?
                .to_owned();
            Ok(MountedVolume {
                mount_path: NativePath::UnixBytes(mount),
                volume_subpath: NativePath::UnixBytes(root[1..].to_vec()),
                persistent_identity: None,
                filesystem,
                device_number: Some(DeviceNumber {
                    major: number(device[0])?,
                    minor: number(device[1])?,
                }),
                issues: vec![],
            })
        })();
        match parsed {
            Ok(mount) => result.mounts.push(mount),
            Err(error) => {
                result.complete = false;
                result.issues.push(issue(
                    IssueKind::Malformed,
                    None,
                    format!("mountinfo line {}: {error}", line_index + 1),
                ));
            }
        }
    }
    if result.mounts.is_empty() {
        result.complete = false;
        result.issues.push(issue(
            IssueKind::Malformed,
            None,
            "mountinfo contains no usable entries",
        ));
    }
    result
}
#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Read;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    fn read_mounts() -> io::Result<MountSnapshot> {
        let mut bytes = Vec::new();
        fs::File::open("/proc/self/mountinfo")?
            .take(MAX_MOUNTINFO_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(parse_linux_mountinfo(&bytes))
    }
    pub fn mounted_volumes() -> io::Result<MountSnapshot> {
        let mut snapshot = read_mounts()?;
        let mut identifiers: BTreeMap<(u32, u32), Vec<PersistentVolumeId>> = BTreeMap::new();
        match fs::read_dir("/dev/disk/by-uuid") {
            Ok(entries) => {
                for (index, entry) in entries.enumerate() {
                    if index >= MAX_MOUNT_ENTRIES {
                        snapshot.complete = false;
                        snapshot.issues.push(issue(
                            IssueKind::ResourceLimit,
                            None,
                            "filesystem UUID link limit exceeded",
                        ));
                        break;
                    }
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            snapshot.complete = false;
                            snapshot.issues.push(io_issue(
                                IssueKind::Inaccessible,
                                Path::new("/dev/disk/by-uuid"),
                                &error,
                            ));
                            continue;
                        }
                    };
                    let path = entry.path();
                    let metadata = match fs::metadata(&path) {
                        Ok(value) => value,
                        Err(error) => {
                            snapshot.complete = false;
                            snapshot
                                .issues
                                .push(io_issue(IssueKind::Inaccessible, &path, &error));
                            continue;
                        }
                    };
                    if !metadata.file_type().is_block_device() {
                        continue;
                    }
                    let name = entry.file_name();
                    let Some(value) = name.to_str() else {
                        snapshot.complete = false;
                        snapshot.issues.push(issue(
                            IssueKind::Malformed,
                            Some(&path),
                            "non-UTF8 filesystem UUID name",
                        ));
                        continue;
                    };
                    match PersistentVolumeId::new(IdentityScheme::LinuxFilesystemUuid, value) {
                        Ok(identity) => {
                            identifiers
                                .entry((libc::major(metadata.rdev()), libc::minor(metadata.rdev())))
                                .or_default()
                                .push(identity);
                        }
                        Err(error) => {
                            snapshot.complete = false;
                            snapshot
                                .issues
                                .push(io_issue(IssueKind::Malformed, &path, &error));
                        }
                    }
                }
            }
            Err(error) => {
                snapshot.complete = false;
                snapshot.issues.push(io_issue(
                    IssueKind::IdentityUnavailable,
                    Path::new("/dev/disk/by-uuid"),
                    &error,
                ));
            }
        }
        for mount in &mut snapshot.mounts {
            let device = mount.device_number.as_ref().expect("parsed device");
            match identifiers
                .get(&(device.major, device.minor))
                .map(Vec::as_slice)
            {
                Some([identity]) => mount.persistent_identity = Some(identity.clone()),
                Some(_) => {
                    snapshot.complete = false;
                    mount.issues.push(VolumeIssue {
                        kind: IssueKind::IdentityAmbiguous,
                        path: Some(mount.mount_path.clone()),
                        detail: "multiple UUID names resolve to the same current block device"
                            .into(),
                    });
                }
                None => mount.issues.push(VolumeIssue {
                    kind: IssueKind::IdentityUnavailable,
                    path: Some(mount.mount_path.clone()),
                    detail:
                        "no filesystem UUID mapping; transient mount/device IDs are not substitutes"
                            .into(),
                }),
            }
        }
        Ok(snapshot)
    }
    pub fn locate(path: &Path) -> io::Result<(MountedVolume, Option<PathBuf>)> {
        let metadata = fs::metadata(path)?;
        let device = (libc::major(metadata.dev()), libc::minor(metadata.dev()));
        let snapshot = mounted_volumes()?;
        let mut candidates: Vec<_> = snapshot
            .mounts
            .into_iter()
            .filter(|mount| {
                mount
                    .device_number
                    .as_ref()
                    .is_some_and(|n| (n.major, n.minor) == device)
                    && mount
                        .mount_path
                        .to_path()
                        .is_ok_and(|root| path.starts_with(root))
            })
            .collect();
        candidates.sort_by_key(|mount| {
            std::cmp::Reverse(
                mount
                    .mount_path
                    .to_path()
                    .map(|p| p.components().count())
                    .unwrap_or(0),
            )
        });
        let Some(mut mount) = candidates.first().cloned() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no verified current mount for filesystem object",
            ));
        };
        if candidates
            .get(1)
            .is_some_and(|other| other.mount_path == mount.mount_path)
        {
            return Err(io::Error::other("ambiguous stacked mountpoint"));
        }
        if !snapshot.complete {
            mount.issues.push(issue(
                IssueKind::ObservationChanged,
                Some(path),
                "mount/UUID snapshot incomplete; revalidate before relinking",
            ));
        }
        let relative = relative_path(path, &mount)?;
        let after = fs::metadata(path)?;
        if !same_file(&metadata, &after) {
            return Err(io::Error::other("source changed during mount lookup"));
        }
        Ok((mount, Some(relative)))
    }
}

/// Parse a native Windows volume GUID path without decoding its filename suffix.
/// Useful for persisted observations and portable protocol fixtures.
pub fn parse_windows_volume_guid_path(
    units: &[u16],
) -> io::Result<(PersistentVolumeId, NativePath)> {
    let prefix: Vec<_> = r"\\?\Volume{".encode_utf16().collect();
    let offset = prefix.len() + 36;
    if units.len() < offset + 2
        || units.contains(&0)
        || !units[..prefix.len()]
            .iter()
            .zip(&prefix)
            .all(|(a, b)| *a <= 127 && (*a as u8).eq_ignore_ascii_case(&(*b as u8)))
        || units[offset] != b'}' as u16
        || units[offset + 1] != b'\\' as u16
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Windows volume GUID path",
        ));
    }
    let value = String::from_utf16(&units[prefix.len()..offset])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid volume GUID encoding"))?;
    let suffix = &units[offset + 2..];
    if suffix
        .first()
        .is_some_and(|c| *c == b'\\' as u16 || *c == b'/' as u16)
        || suffix.contains(&(b':' as u16))
        || suffix
            .split(|c| *c == b'\\' as u16 || *c == b'/' as u16)
            .any(|part| part == [b'.' as u16] || part == [b'.' as u16, b'.' as u16])
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe Windows volume-relative suffix",
        ));
    }
    Ok((
        PersistentVolumeId::new(IdentityScheme::WindowsVolumeGuid, &value)?,
        NativePath::WindowsWide(suffix.to_vec()),
    ))
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    const MAX_WIDE: usize = 32768;
    type Handle = *mut c_void;
    #[repr(C)]
    struct FileIdInfo {
        volume_serial: u64,
        file_id: [u8; 16],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetVolumePathNameW(path: *const u16, root: *mut u16, size: u32) -> i32;
        fn GetVolumeNameForVolumeMountPointW(root: *const u16, name: *mut u16, size: u32) -> i32;
        fn GetVolumePathNamesForVolumeNameW(
            name: *const u16,
            paths: *mut u16,
            size: u32,
            needed: *mut u32,
        ) -> i32;
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn FindFirstVolumeW(name: *mut u16, size: u32) -> Handle;
        fn FindNextVolumeW(handle: Handle, name: *mut u16, size: u32) -> i32;
        fn FindVolumeClose(handle: Handle) -> i32;
        fn GetFinalPathNameByHandleW(handle: Handle, path: *mut u16, size: u32, flags: u32) -> u32;
        fn GetFileInformationByHandleEx(
            handle: Handle,
            class: i32,
            info: *mut c_void,
            size: u32,
        ) -> i32;
    }
    struct VolumeSearch(Handle);
    impl Drop for VolumeSearch {
        fn drop(&mut self) {
            unsafe {
                FindVolumeClose(self.0);
            }
        }
    }
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut value: Vec<_> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) || value.len() >= MAX_WIDE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL or overlong Windows path",
            ));
        }
        value.push(0);
        Ok(value)
    }
    fn terminated(value: &[u16]) -> io::Result<&[u16]> {
        let end = value.iter().position(|v| *v == 0).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "unterminated Windows API path")
        })?;
        Ok(&value[..end])
    }
    fn metadata_handle(path: &Path) -> io::Result<fs::File> {
        // Access zero requests metadata only; backup semantics permits directories.
        // Sharing does not freeze the namespace: subsequent identity checks detect
        // replacements, and catalog commit must independently revalidate evidence.
        fs::OpenOptions::new()
            .read(true)
            .access_mode(0)
            .custom_flags(0x0200_0000)
            .open(path)
    }
    pub fn object_key(path: &Path) -> io::Result<(u64, u128)> {
        let file = metadata_handle(path)?;
        let mut value = MaybeUninit::<FileIdInfo>::uninit();
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                18,
                value.as_mut_ptr().cast(),
                std::mem::size_of::<FileIdInfo>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let value = unsafe { value.assume_init() };
        Ok((value.volume_serial, u128::from_ne_bytes(value.file_id)))
    }
    fn root_for(path: &Path) -> io::Result<Vec<u16>> {
        let path = wide(path)?;
        let mut root = vec![0; MAX_WIDE];
        if unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let end = terminated(&root)?.len();
        root.truncate(end + 1);
        Ok(root)
    }
    fn identity_for(root: &[u16]) -> io::Result<PersistentVolumeId> {
        let mut name = [0; 128];
        if unsafe {
            GetVolumeNameForVolumeMountPointW(root.as_ptr(), name.as_mut_ptr(), name.len() as u32)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let (identity, suffix) = parse_windows_volume_guid_path(terminated(&name)?)?;
        if suffix != NativePath::WindowsWide(vec![]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "volume API returned non-root GUID path",
            ));
        }
        Ok(identity)
    }
    fn mount(root: &[u16]) -> io::Result<MountedVolume> {
        let path = NativePath::WindowsWide(terminated(root)?.to_vec());
        let native = path.to_path()?;
        let mut issues = vec![];
        let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
        let persistent_identity = if drive_type == 4 {
            issues.push(issue(
                IssueKind::IdentityUnavailable,
                Some(&native),
                "remote/UNC volumes have no local mount-manager GUID; explicit relink required",
            ));
            None
        } else {
            match identity_for(root) {
                Ok(id) => Some(id),
                Err(error) => {
                    issues.push(io_issue(IssueKind::IdentityUnavailable, &native, &error));
                    None
                }
            }
        };
        Ok(MountedVolume {
            mount_path: path,
            volume_subpath: NativePath::WindowsWide(vec![]),
            persistent_identity,
            filesystem: if drive_type == 4 {
                "remote"
            } else {
                "local (filesystem type not queried)"
            }
            .into(),
            device_number: None,
            issues,
        })
    }
    pub fn mounted_volumes() -> io::Result<MountSnapshot> {
        let mut name = [0; 128];
        let handle = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
        if handle as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let search = VolumeSearch(handle);
        let mut result = MountSnapshot {
            mounts: vec![],
            complete: true,
            issues: vec![],
        };
        for index in 0..MAX_MOUNT_ENTRIES {
            let mut paths = vec![0; MAX_WIDE];
            let mut needed = 0;
            if unsafe {
                GetVolumePathNamesForVolumeNameW(
                    name.as_ptr(),
                    paths.as_mut_ptr(),
                    paths.len() as u32,
                    &mut needed,
                )
            } == 0
            {
                let error = io::Error::last_os_error();
                result.complete = false;
                result.issues.push(issue(
                    if needed as usize > MAX_WIDE {
                        IssueKind::ResourceLimit
                    } else {
                        IssueKind::Inaccessible
                    },
                    None,
                    format!(
                        "volume mount-path enumeration failed; os_error={:?}",
                        error.raw_os_error()
                    ),
                ));
            } else if needed as usize > paths.len() || needed == 0 {
                result.complete = false;
                result.issues.push(issue(
                    IssueKind::Malformed,
                    None,
                    "invalid Windows mount-path list size",
                ));
            } else {
                let paths = &paths[..needed as usize];
                let mut position = 0;
                while position < paths.len() && paths[position] != 0 {
                    let length = terminated(&paths[position..])?.len();
                    if result.mounts.len() >= MAX_MOUNT_ENTRIES {
                        result.complete = false;
                        result.issues.push(issue(
                            IssueKind::ResourceLimit,
                            None,
                            "mount-path count limit exceeded",
                        ));
                        return Ok(result);
                    }
                    match mount(&paths[position..position + length + 1]) {
                        Ok(mount) => {
                            result.complete &= mount.persistent_identity.is_some();
                            result.mounts.push(mount);
                        }
                        Err(error) => {
                            result.complete = false;
                            result.issues.push(issue(
                                IssueKind::Malformed,
                                None,
                                error.to_string(),
                            ));
                        }
                    }
                    position += length + 1;
                }
                if position >= paths.len() {
                    result.complete = false;
                    result.issues.push(issue(
                        IssueKind::Malformed,
                        None,
                        "missing final mount-list terminator",
                    ));
                }
            }
            if unsafe { FindNextVolumeW(search.0, name.as_mut_ptr(), name.len() as u32) } == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(18) {
                    result.complete = false;
                    result.issues.push(issue(
                        IssueKind::Inaccessible,
                        None,
                        format!(
                            "volume enumeration failed; os_error={:?}",
                            error.raw_os_error()
                        ),
                    ));
                }
                return Ok(result);
            }
            if index + 1 == MAX_MOUNT_ENTRIES {
                result.complete = false;
                result.issues.push(issue(
                    IssueKind::ResourceLimit,
                    None,
                    "volume count limit exceeded",
                ));
            }
        }
        Ok(result)
    }
    pub fn locate(path: &Path) -> io::Result<(MountedVolume, Option<PathBuf>)> {
        let root = root_for(path)?;
        let mut volume = mount(&root)?;
        let file = metadata_handle(path)?;
        let mut final_path = vec![0; MAX_WIDE];
        let count = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                final_path.as_mut_ptr(),
                final_path.len() as u32,
                1,
            )
        } as usize;
        let relative = if count > 0 && count < final_path.len() {
            let (identity, suffix) = parse_windows_volume_guid_path(&final_path[..count])?;
            if Some(&identity) != volume.persistent_identity.as_ref() {
                return Err(io::Error::other(
                    "handle volume differs from mount-path identity",
                ));
            }
            Some(suffix.to_path()?)
        } else if volume.persistent_identity.is_none() {
            // UNC has no GUID. Canonical lexical mapping is useful evidence but
            // can never auto-match a persistent volume because its identity is absent.
            relative_path(path, &volume).ok()
        } else {
            None
        };
        if relative.is_none() {
            volume.issues.push(issue(
                IssueKind::MappingUnavailable,
                Some(path),
                "volume-relative handle path unavailable or exceeds bound",
            ));
        }
        let after = root_for(path)?;
        if root != after || volume.persistent_identity != mount(&after)?.persistent_identity {
            return Err(io::Error::other(
                "volume mapping changed during observation",
            ));
        }
        Ok((volume, relative))
    }
}
