//! Explicit sidecar publication with retained recovery evidence.
//!
//! Capture temporarily removes the destination: this is NOT compare-and-swap and
//! does not promise continuous pathname availability. Captures are never deleted,
//! even after success: another process may still hold a writable handle to them.
//! Callers own XMP semantics, user authorization, and asset-revision validation.
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const PREFIX: &str = ".photocatalog-xmp-export-";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRevision {
    pub bytes: u64,
    pub digest: String,
    pub modified_ns: u128,
    pub identity: (u64, u64),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportPlan {
    pub version: u32,
    pub operation: String,
    pub destination: PathBuf,
    pub expected: Option<FileRevision>,
    pub payload_digest: String,
    pub payload_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportState {
    Published,
    /// Captured bytes restored without replacing any concurrent destination.
    Restored,
    /// Destination belongs to another revision; captured bytes remain available.
    Conflict,
    /// I/O, unsupported filesystem, incomplete preparation, or uncertain durability.
    Recoverable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportReceipt {
    pub state: ExportState,
    pub destination: PathBuf,
    pub recovery_directory: PathBuf,
    pub captured_original: Option<PathBuf>,
    pub detail: String,
}

/// Fault/race seam used by deterministic tests; hooks never change the protocol.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportBoundary {
    BeforePayload,
    Prepared,
    BeforeCapture,
    Captured,
    BeforePublish,
    Published,
    BeforeRestore,
}

/// Read-only planning. Existing destinations must be ordinary files, not links.
pub fn plan_export(destination: &Path, payload: &[u8]) -> Result<ExportPlan> {
    let destination = normalize_destination(destination)?;
    let expected = revision_if_exists(&destination)?;
    Ok(ExportPlan {
        version: 1,
        operation: uuid::Uuid::new_v4().to_string(),
        destination,
        expected,
        payload_digest: blake3::hash(payload).to_hex().to_string(),
        payload_bytes: payload.len() as u64,
    })
}

pub fn apply_export(plan: &ExportPlan, payload: &[u8]) -> Result<ExportReceipt> {
    apply_export_with_hook(plan, payload, |_| Ok(()))
}

#[doc(hidden)]
pub fn apply_export_with_hook(
    plan: &ExportPlan,
    payload: &[u8],
    mut hook: impl FnMut(ExportBoundary) -> io::Result<()>,
) -> Result<ExportReceipt> {
    validate_plan(plan)?;
    ensure!(
        payload.len() as u64 == plan.payload_bytes
            && blake3::hash(payload).to_hex().as_str() == plan.payload_digest,
        "payload differs from reviewed export plan"
    );
    let directory = recovery_path(plan);
    if !directory.try_exists()? {
        // Staging is also discoverable after a crash; no destination changes occur
        // until the complete plan and payload have been durably published together.
        let staging = directory.with_file_name(format!(
            "{PREFIX}{}-preparing-{}",
            plan.operation,
            uuid::Uuid::new_v4()
        ));
        create_private_directory(&staging)?;
        let prepared = (|| -> Result<()> {
            hook(ExportBoundary::BeforePayload)?;
            write_new(&staging.join("payload"), payload)?;
            write_new(&staging.join("plan.json"), &serde_json::to_vec(plan)?)?;
            write_new(&staging.join("operation.lock"), b"")?;
            sync_directory(&staging)?;
            move_to_private(&staging, &directory)?;
            sync_directory(directory.parent().unwrap())?;
            Ok(())
        })();
        if let Err(error) = prepared {
            // Another application of this same plan may have prepared it first.
            // Retain our staging evidence; do not remove any filesystem entries.
            if !directory.try_exists()? {
                return Ok(receipt(
                    plan,
                    &staging,
                    ExportState::Recoverable,
                    format!("preparation failed before capture: {error:#}"),
                ));
            }
        }
    }
    run_recovery(&directory, Some(plan), &mut hook, false)
}

/// Resume a known operation. No file is unlinked or overwritten by recovery.
pub fn recover_export(directory: &Path) -> Result<ExportReceipt> {
    run_recovery(directory, None, &mut |_| Ok(()), false)
}

/// Restore only; never publish staged metadata. Used when catalog revisions have
/// changed since an interrupted export. Concurrent destinations remain untouched.
pub fn restore_planned_export(plan: &ExportPlan) -> Result<ExportReceipt> {
    validate_plan(plan)?;
    run_recovery(&recovery_path(plan), Some(plan), &mut |_| Ok(()), true)
}

/// Includes interrupted preparation directories; malformed/incomplete operations
/// remain visible for review rather than being silently deleted.
pub fn discover_exports(parent: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for item in fs::read_dir(parent.canonicalize()?)? {
        let item = item?;
        if item.file_name().to_string_lossy().starts_with(PREFIX) && item.file_type()?.is_dir() {
            paths.push(item.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn run_recovery(
    directory: &Path,
    expected_plan: Option<&ExportPlan>,
    hook: &mut impl FnMut(ExportBoundary) -> io::Result<()>,
    restore_only: bool,
) -> Result<ExportReceipt> {
    ensure!(
        fs::symlink_metadata(directory)?.file_type().is_dir(),
        "recovery directory is not an ordinary directory"
    );
    let directory = directory.canonicalize()?;
    let lock = OperationLock(open_regular(&directory.join("operation.lock"))?);
    lock.0
        .try_lock_exclusive()
        .context("export recovery is already running")?;
    {
        let mut bytes = Vec::new();
        open_regular(&directory.join("plan.json"))?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 64 * 1024, "export journal exceeds limit");
        let plan: ExportPlan = serde_json::from_slice(&bytes)?;
        validate_plan(&plan)?;
        ensure!(
            directory == recovery_path(&plan),
            "recovery directory does not match plan"
        );
        if let Some(expected) = expected_plan {
            ensure!(*expected == plan, "existing operation has a different plan");
        }
        if restore_only {
            return rollback(
                &plan,
                &directory,
                "catalog revision changed; staged payload must not be published".into(),
                hook,
            );
        }
        let result = resume(&plan, &directory, hook);
        match result {
            Ok(receipt) => Ok(receipt),
            Err(error) => rollback(
                &plan,
                &directory,
                format!("publication failed: {error:#}"),
                hook,
            ),
        }
    }
}

struct OperationLock(File);
impl Drop for OperationLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn resume(
    plan: &ExportPlan,
    directory: &Path,
    hook: &mut impl FnMut(ExportBoundary) -> io::Result<()>,
) -> Result<ExportReceipt> {
    let payload = directory.join("payload");
    let staged = revision(&payload)?;
    ensure!(
        staged.bytes == plan.payload_bytes && staged.digest == plan.payload_digest,
        "retained payload is corrupt or changed"
    );
    // Identity as well as digest proves this operation published the current file.
    if let Ok(Some(current)) = revision_if_exists(&plan.destination)
        && current.identity == staged.identity
        && current.digest == staged.digest
    {
        sync_regular(&plan.destination)?;
        sync_directory(plan.destination.parent().unwrap())?;
        return Ok(receipt(
            plan,
            directory,
            ExportState::Published,
            "published bytes and durability barriers verified".into(),
        ));
    }
    hook(ExportBoundary::Prepared)?;
    let captured = directory.join("original");
    if plan.expected.is_some() && !captured.try_exists()? {
        // Preflight refuses an already visible symlink. A replacement after this
        // read is captured without following it and is checked again below.
        match revision_if_exists(&plan.destination)? {
            None => {
                return Ok(receipt(
                    plan,
                    directory,
                    ExportState::Conflict,
                    "expected destination is now absent; nothing captured".into(),
                ));
            }
            Some(current) if Some(&current) != plan.expected.as_ref() => {
                return Ok(receipt(
                    plan,
                    directory,
                    ExportState::Conflict,
                    "destination changed since planning; untouched".into(),
                ));
            }
            _ => {}
        }
        hook(ExportBoundary::BeforeCapture)?;
        move_to_private(&plan.destination, &captured)?;
        sync_directory(plan.destination.parent().unwrap())?;
        sync_directory(directory)?;
        hook(ExportBoundary::Captured)?;
    }
    if plan.expected.is_some() {
        let actual = revision(&captured)?;
        if Some(&actual) != plan.expected.as_ref() {
            return rollback(
                plan,
                directory,
                "captured destination differs from plan".into(),
                hook,
            );
        }
    }
    hook(ExportBoundary::BeforePublish)?;
    publish_noclobber(&payload, &plan.destination)?;
    hook(ExportBoundary::Published)?;
    sync_regular(&plan.destination)?;
    sync_directory(plan.destination.parent().unwrap())?;
    let installed = revision(&plan.destination)?;
    ensure!(
        installed.identity == staged.identity && installed.digest == staged.digest,
        "destination changed during publication; recovery evidence retained"
    );
    Ok(receipt(
        plan,
        directory,
        ExportState::Published,
        "published without clobber; capture retained; durability barriers completed".into(),
    ))
}

fn rollback(
    plan: &ExportPlan,
    directory: &Path,
    detail: String,
    hook: &mut impl FnMut(ExportBoundary) -> io::Result<()>,
) -> Result<ExportReceipt> {
    if let (Ok(current), Ok(staged)) = (
        revision(&plan.destination),
        revision(&directory.join("payload")),
    ) && current.identity == staged.identity
    {
        return Ok(receipt(
            plan,
            directory,
            ExportState::Recoverable,
            format!(
                "{detail}; payload inode is visible, durability is not confirmed; retry recovery"
            ),
        ));
    }
    let original = directory.join("original");
    let state = if fs::symlink_metadata(&original).is_ok() {
        // A raced symlink/special-file capture is retained without reading it.
        if !fs::symlink_metadata(&original)?.file_type().is_file() {
            return Ok(receipt(
                plan,
                directory,
                ExportState::Recoverable,
                format!("{detail}; non-regular capture retained for explicit recovery"),
            ));
        }
        if hook(ExportBoundary::BeforeRestore).is_err() {
            ExportState::Recoverable
        } else {
            match publish_noclobber(&original, &plan.destination) {
                Ok(()) => {
                    if sync_regular(&plan.destination)
                        .and_then(|_| sync_directory(plan.destination.parent().unwrap()))
                        .is_ok()
                    {
                        ExportState::Restored
                    } else {
                        ExportState::Recoverable
                    }
                }
                Err(_) if fs::symlink_metadata(&plan.destination).is_ok() => ExportState::Conflict,
                Err(_) => ExportState::Recoverable,
            }
        }
    } else if fs::symlink_metadata(&plan.destination).is_ok() {
        ExportState::Conflict
    } else {
        ExportState::Recoverable
    };
    Ok(receipt(plan, directory, state, detail))
}

fn receipt(
    plan: &ExportPlan,
    directory: &Path,
    state: ExportState,
    detail: String,
) -> ExportReceipt {
    let original = directory.join("original");
    ExportReceipt {
        state,
        destination: plan.destination.clone(),
        recovery_directory: directory.to_path_buf(),
        captured_original: fs::symlink_metadata(&original).is_ok().then_some(original),
        detail,
    }
}

fn recovery_path(plan: &ExportPlan) -> PathBuf {
    plan.destination
        .parent()
        .unwrap()
        .join(format!("{PREFIX}{}", plan.operation))
}

fn normalize_destination(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("xmp")),
        "export requires an explicit .xmp destination"
    );
    let filename = path.file_name().context("destination has no filename")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(
        fs::symlink_metadata(parent)?.file_type().is_dir(),
        "destination parent is not an ordinary directory"
    );
    Ok(parent.canonicalize()?.join(filename))
}

fn validate_plan(plan: &ExportPlan) -> Result<()> {
    ensure!(plan.version == 1, "unsupported export plan version");
    ensure!(
        uuid::Uuid::parse_str(&plan.operation)?.to_string() == plan.operation,
        "invalid operation identity"
    );
    ensure!(
        normalize_destination(&plan.destination)? == plan.destination,
        "destination parent/path changed since planning"
    );
    ensure!(
        plan.payload_digest.len() == 64
            && plan.payload_digest.bytes().all(|x| x.is_ascii_hexdigit()),
        "invalid payload digest"
    );
    Ok(())
}

fn revision_if_exists(path: &Path) -> Result<Option<FileRevision>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file(),
                "destination is not an ordinary file (symlinks refused)"
            );
            revision(path).map(Some)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn revision(path: &Path) -> Result<FileRevision> {
    let file = open_regular(path)?;
    let before = file.metadata()?;
    let identity = identity(&file, &before)?;
    let mut hash = blake3::Hasher::new();
    let mut read = (&file).take(before.len().saturating_add(1));
    let mut buffer = [0u8; 64 * 1024];
    let mut count = 0u64;
    loop {
        let n = read.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        count += n as u64;
    }
    let after = file.metadata()?;
    ensure!(
        count == before.len()
            && before.len() == after.len()
            && before.modified()? == after.modified()?,
        "file changed while reading revision"
    );
    Ok(FileRevision {
        bytes: count,
        digest: hash.finalize().to_hex().to_string(),
        modified_ns: before
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
        identity,
    })
}

fn open_regular(path: &Path) -> Result<File> {
    open_regular_options(path, false)
}

fn open_regular_options(path: &Path, write: bool) -> Result<File> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "not an ordinary file"
    );
    let mut options = OpenOptions::new();
    options.read(true).write(write);
    // NONBLOCK also prevents a regular-file -> FIFO race from hanging this call.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(if cfg!(target_os = "macos") {
            0x100 | 4
        } else {
            0x20000 | 0x800
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.file_type().is_file(),
        "opened object is not an ordinary file"
    );
    Ok(file)
}

#[cfg(unix)]
fn identity(_: &File, metadata: &Metadata) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn identity(file: &File, _: &Metadata) -> Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct Info {
        attributes: u32,
        creation: [u32; 2],
        access: [u32; 2],
        write: [u32; 2],
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: *mut std::ffi::c_void, info: *mut Info) -> i32;
    }
    let mut info = std::mem::MaybeUninit::<Info>::uninit();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let info = unsafe { info.assume_init() };
    Ok((
        info.volume as u64,
        ((info.index_high as u64) << 32) | info.index_low as u64,
    ))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    sync_file(&file)?;
    Ok(())
}
fn sync_regular(path: &Path) -> Result<()> {
    sync_file(&open_regular_options(path, cfg!(windows))?)?;
    Ok(())
}

fn sync_file(file: &File) -> Result<()> {
    file.sync_all()?;
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn fcntl(fd: i32, command: i32, ...) -> i32;
        }
        // F_FULLFSYNC asks the device to flush its write cache as well as the OS.
        if unsafe { fcntl(file.as_raw_fd(), 51) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok(())
}
fn create_private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}
#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
// Windows namespace publication below uses MoveFileExW WRITE_THROUGH, including
// the fully prepared recovery directory. File data is flushed before those moves.
#[cfg(windows)]
fn sync_directory(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn move_to_private(from: &Path, to: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(to).is_err_and(|e| e.kind() == io::ErrorKind::NotFound),
        "private capture already exists"
    );
    fs::rename(from, to)?;
    Ok(())
}
#[cfg(windows)]
fn move_to_private(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 8) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn publish_noclobber(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::hard_link(source, destination).context("atomic no-clobber hard-link publication failed (filesystem may not support hard links)")?;
    }
    #[cfg(windows)]
    {
        let alias = source.with_file_name(format!("publication-{}", uuid::Uuid::new_v4()));
        fs::hard_link(source, &alias)
            .context("filesystem cannot stage no-clobber hard-link publication")?;
        move_to_private(&alias, destination)?;
    }
    Ok(())
}
