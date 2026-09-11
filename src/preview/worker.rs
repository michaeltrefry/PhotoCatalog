//! Isolated full-image work. The owner keeps a stdin lease open; EOF terminates
//! native work after an owner crash, and cancellation kills and waits for exit.
use super::{
    Codec, EditInputProvenance, PreparedRgb, PreviewKey, decode, encode, encoded_dimensions,
    prepare, renderer_identity,
};
use crate::{
    media::{DecodeError, DecodeLimits, DecodeStatus, Metadata, RenderProvenance},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};

const RECEIPT_LIMIT: u64 = 192 * 1024;
use super::prepared_cache::{
    MAX_PROXY_BYTES, PROXY_EDGE, PreparedReference, ProducedPrepared, SourceInstance,
};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditWork {
    pub recipe: crate::edit::Recipe,
    pub recipe_digest: String,
    pub limits: crate::edit::RenderLimits,
    pub interactive: bool,
    pub prepared_bytes: u64,
    #[serde(default)]
    pub(crate) prepared: Option<PreparedReference>,
}
pub(crate) fn edited_renderer(interactive: bool) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(crate::edit::renderer_identity().as_bytes());
    hash.update(include_bytes!("worker.rs"));
    hash.update(include_bytes!("../edit/prepared.rs"));
    format!(
        "photocatalog-edit-preview-1:{}:{}",
        hash.finalize().to_hex(),
        if interactive { "proxy1600" } else { "original" }
    )
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderWork {
    pub source: NativePath,
    pub keys: Vec<PreviewKey>,
    pub encoded_limit: u64,
    pub decode_limits: DecodeLimits,
    #[serde(default)]
    pub edit: Option<EditWork>,
}
impl RenderWork {
    pub(crate) fn validate(&self) -> Result<PathBuf> {
        let path = self.validate_persisted()?;
        let renderer = self.edit.as_ref().map_or_else(
            || renderer_identity().to_owned(),
            |edit| edited_renderer(edit.interactive),
        );
        ensure!(
            self.keys.iter().all(|key| key.renderer_version == renderer),
            "worker renderer identity mismatch"
        );
        Ok(path)
    }
    /// Recovery must validate the old descriptor before inspecting its old
    /// attachment proof. Only a newly admitted launch requires this binary's
    /// renderer; resume rebuilds keys after checking durable catalog authority.
    pub(crate) fn validate_persisted(&self) -> Result<PathBuf> {
        self.decode_limits.validate()?;
        ensure!((1..=2).contains(&self.keys.len()), "worker tier count");
        ensure!(
            (1..=256 * 1024 * 1024).contains(&self.encoded_limit),
            "worker encoded allowance"
        );
        let first = &self.keys[0];
        if let Some(edit) = &self.edit {
            ensure!(
                edit.prepared_bytes <= MAX_PROXY_BYTES,
                "worker prepared staging allowance"
            );
            let recipe = edit.recipe.validate()?;
            ensure!(
                recipe.digest() == edit.recipe_digest,
                "worker recipe digest mismatch"
            );
            ensure!(
                edit.limits.max_live_bytes > 0
                    && edit.limits.max_allocation_bytes > 0
                    && edit.limits.max_pixels > 0,
                "worker edit resource allowance"
            );
            ensure!(
                !edit.interactive || self.keys.iter().all(|key| key.edge <= PROXY_EDGE),
                "interactive key exceeds prepared source edge"
            );
        } else {
            ensure!(
                first.edit_revision == 0 && first.variant_id == "master",
                "worker missing edit recipe"
            );
        }
        for key in &self.keys {
            key.validate()?;
            ensure!(
                key.renderer_version == first.renderer_version
                    && key.renderer_version.ends_with(":proxy1600")
                        == self.edit.as_ref().is_some_and(|edit| edit.interactive),
                "worker mixed renderer channels"
            );
            ensure!(
                key.asset_id == first.asset_id
                    && key.variant_id == first.variant_id
                    && key.generation == first.generation
                    && key.fingerprint == first.fingerprint
                    && key.edit_revision == first.edit_revision,
                "worker mixed source identities"
            );
        }
        if self.keys.len() == 2 {
            ensure!(
                self.keys[0].tier != self.keys[1].tier,
                "duplicate worker tier"
            );
        }
        let path = self.source.to_path()?;
        ensure!(
            path.is_absolute(),
            "worker requires an absolute original path"
        );
        Ok(path)
    }
}
/// A source can remain supported while its current worker allowance is too
/// small. The service persists ResourceLimit failures for explicit retry after
/// configuration changes, retaining the previous offline thumbnail.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerFailure {
    pub decode_status: Option<DecodeStatus>,
    pub message: String,
}
impl std::fmt::Display for WorkerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for WorkerFailure {}

#[derive(Debug, Serialize, Deserialize)]
struct ObjectReceipt {
    key: PreviewKey,
    width: u32,
    height: u32,
    bytes: u64,
    checksum: String,
}
#[derive(Debug, Serialize, Deserialize)]
struct RenderReceipt {
    edit_input: Option<EditInputProvenance>,
    peak_resident_bytes: Option<u64>,
    peak_method: String,
    metadata: Metadata,
    provenance: RenderProvenance,
    objects: Vec<ObjectReceipt>,
    prepared: Option<(crate::edit::PreparedProxyReceipt, SourceInstance)>,
}
pub struct ProducedPreview {
    pub key: PreviewKey,
    pub pixels: PreparedRgb,
    pub encoded: Vec<u8>,
}
pub struct RenderedPreviewBatch {
    pub edit_input: Option<EditInputProvenance>,
    pub peak_resident_bytes: Option<u64>,
    pub peak_method: String,
    pub metadata: Metadata,
    pub provenance: RenderProvenance,
    pub objects: Vec<ProducedPreview>,
    pub(crate) prepared: Option<ProducedPrepared>,
}
/// A worker's encoded receipt is not sufficient image-validity evidence. Complete
/// production decoding is required before attachment, under its worker allowance.
fn validate_object(key: &PreviewKey, width: u32, height: u32, bytes: &[u8]) -> Result<PreparedRgb> {
    ensure!(
        width > 0 && height > 0 && width <= key.edge && height <= key.edge,
        "worker dimensions exceed requested tier"
    );
    ensure!(
        encoded_dimensions(bytes, key.encoding.codec)? == (width, height),
        "worker header dimensions mismatch"
    );
    if key.encoding.codec == Codec::Jpeg {
        ensure!(
            bytes.ends_with(&[0xff, 0xd9]),
            "worker JPEG lacks complete end marker"
        );
    }
    let pixels = decode(bytes, key.encoding.codec)?;
    ensure!(
        pixels.width() == width && pixels.height() == height,
        "worker decoded dimensions mismatch"
    );
    Ok(pixels)
}
/// The launcher path is supplied by the application; the library does not guess
/// a binary from test executables or silently fall back to synchronous rendering.
pub struct WorkerProcess {
    child: Child,
    lease: Option<ChildStdin>,
    staging: PathBuf,
    request: RenderWork,
    exited: bool,
    encoding_admitted: bool,
}
impl WorkerProcess {
    pub fn spawn(executable: &Path, staging_root: &Path, request: RenderWork) -> Result<Self> {
        request.validate()?;
        ensure!(
            executable.is_absolute(),
            "worker executable must be absolute"
        );
        let bytes = serde_json::to_vec(&request)?;
        ensure!((bytes.len() as u64) < RECEIPT_LIMIT, "worker request limit");
        fs::create_dir_all(staging_root)?;
        let staging = tempfile::Builder::new()
            .prefix("worker-")
            .tempdir_in(staging_root)?
            .keep();
        let mut child = Command::new(executable)
            .arg("--preview-worker")
            .current_dir(&staging)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("OMP_NUM_THREADS", "1")
            .env("RAYON_NUM_THREADS", "1")
            .spawn()
            .context("start preview worker")?;
        let lease = child.stdin.take().context("worker stdin lease missing")?;
        let mut value = Self {
            child,
            lease: Some(lease),
            staging,
            request,
            exited: false,
            encoding_admitted: false,
        };
        let input = value.lease.as_mut().unwrap();
        input.write_all(&bytes)?;
        input.write_all(b"\n!")?;
        input.flush()?;
        Ok(value)
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
    /// Returns only after exit. The service releases the scheduler reservation
    /// after consuming this batch, including its validated decoded pixel buffers.
    pub fn poll(&mut self, canceled: &AtomicBool) -> Result<Option<RenderedPreviewBatch>> {
        ensure!(!self.exited, "worker already consumed");
        if canceled.load(Ordering::Acquire) {
            self.stop()?;
            bail!("preview request canceled");
        }
        let Some(status) = self.child.try_wait()? else {
            if !self.encoding_admitted && self.awaiting_encode_admission()? {
                let lease = self.lease.as_mut().context("worker lease closed")?;
                lease.write_all(b"E")?;
                lease.flush()?;
                self.encoding_admitted = true;
            }
            return Ok(None);
        };
        self.exited = true;
        self.lease.take();
        if !status.success() {
            let detail = read_bounded(&self.staging.join("error.json"), RECEIPT_LIMIT)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<WorkerFailure>(&bytes).ok())
                .unwrap_or_else(|| WorkerFailure {
                    decode_status: None,
                    message: format!("preview worker failed ({status})"),
                });
            return Err(detail.into());
        }
        let receipt: RenderReceipt = serde_json::from_slice(&read_bounded(
            &self.staging.join("result.json"),
            RECEIPT_LIMIT,
        )?)?;
        ensure!(
            receipt.objects.len() == self.request.keys.len(),
            "incomplete worker tier set"
        );
        match (&self.request.edit, &receipt.edit_input) {
            (None, None) | (Some(_), Some(EditInputProvenance::OriginalDecoded)) => {}
            (
                Some(edit),
                Some(EditInputProvenance::PreparedProxy {
                    receipt,
                    source_instance_digest,
                }),
            ) => {
                let expected = edit.prepared.as_ref().context("unrequested prepared hit")?;
                ensure!(
                    edit.interactive
                        && *receipt == expected.receipt
                        && *source_instance_digest == expected.source.digest()?,
                    "prepared hit differs from admitted request"
                );
            }
            _ => bail!("worker edit input evidence differs from request"),
        }
        let mut total = 0u64;
        let mut objects = Vec::new();
        for (index, object) in receipt.objects.into_iter().enumerate() {
            ensure!(object.key == self.request.keys[index], "stale worker key");
            total = total
                .checked_add(object.bytes)
                .context("worker byte overflow")?;
            ensure!(
                total <= self.request.encoded_limit,
                "worker staged bytes exceeded allowance"
            );
            let bytes = read_bounded(&self.staging.join(format!("{index}.preview")), object.bytes)?;
            ensure!(
                bytes.len() as u64 == object.bytes
                    && blake3::hash(&bytes).to_hex().as_str() == object.checksum,
                "worker object integrity mismatch"
            );
            let pixels = validate_object(&object.key, object.width, object.height, &bytes)?;
            objects.push(ProducedPreview {
                key: object.key,
                pixels,
                encoded: bytes,
            });
        }
        Ok(Some(RenderedPreviewBatch {
            edit_input: receipt.edit_input,
            peak_resident_bytes: receipt.peak_resident_bytes,
            peak_method: receipt.peak_method,
            metadata: receipt.metadata,
            provenance: receipt.provenance,
            objects,
            prepared: receipt.prepared.map(|(receipt, source)| ProducedPrepared {
                path: self.staging.join("prepared.linear"),
                receipt,
                source,
            }),
        }))
    }
    /// The worker holds a decoded source under its reservation until a subsequent
    /// owner poll admits encoding. No output transport or blocking read is needed.
    pub fn awaiting_encode_admission(&self) -> Result<bool> {
        if self.encoding_admitted || self.exited {
            return Ok(false);
        }
        let path = self.staging.join("decoded.ready");
        if !path.exists() {
            return Ok(false);
        }
        ensure!(
            read_bounded(&path, 16)? == b"decoded",
            "invalid worker checkpoint"
        );
        Ok(true)
    }
    fn stop(&mut self) -> Result<()> {
        self.lease.take();
        if !self.exited {
            let kill = self.child.kill();
            let waited = self.child.wait();
            self.exited = waited.is_ok();
            waited.context("wait for canceled preview worker")?;
            // An already-exited child may reject kill; wait remains authoritative.
            let _ = kill;
        }
        Ok(())
    }
}
impl Drop for WorkerProcess {
    fn drop(&mut self) {
        let _ = self.stop();
        if self.exited {
            let _ = clean_staging_directory(&self.staging);
        }
    }
}
/// Called while the service owns the manifest lock. A crashed owner's child may
/// still be observing EOF; its own OS lock prevents deleting live staging files.
/// Each call examines at most `limit` directory entries, never a recursive walk.
pub fn recover_worker_staging(root: &Path, limit: usize) -> Result<usize> {
    ensure!((1..=128).contains(&limit), "worker recovery batch limit");
    fs::create_dir_all(root)?;
    let mut removed = 0;
    for entry in fs::read_dir(root)?.take(limit) {
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_dir()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("worker-") || name.starts_with("claimed-")),
            "unrecognized worker staging entry"
        );
        removed += usize::from(clean_staging_directory(&entry.path())?);
    }
    Ok(removed)
}
// Created only after a successful lock. A duplicated/inherited descriptor must
// not extend completed recovery or worker authority beyond this lexical scope.
struct AcquiredLease(File);
impl Drop for AcquiredLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
fn clean_staging_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path.join("active.lock")) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "invalid worker staging lock"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let lock = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.join("active.lock"))
    {
        Ok(lock) => lock,
        #[cfg(windows)]
        Err(error) if matches!(error.raw_os_error(), Some(5 | 32 | 33 | 303)) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if let Err(error) = lock.try_lock_exclusive() {
        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
            return Ok(false);
        }
        return Err(error.into());
    }
    let mut lock = AcquiredLease(lock);
    let allowed = [
        "active.lock",
        "result.json",
        "error.json",
        "decoded.ready",
        "decoded.pending",
        "0.preview",
        "1.preview",
        "prepared.linear",
    ];
    let mut names = Vec::new();
    for entry in fs::read_dir(path)?.take(allowed.len() + 1) {
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| allowed.contains(&name)),
            "unexpected worker artifact requires inspection"
        );
        names.push(entry.file_name());
    }
    // Windows cannot reliably rename this directory while our own child-file
    // handle is open. Publish retirement through the exclusively locked handle
    // first; every worker checks that same handle after acquiring its lease.
    // A delayed opener can acquire the old lease after this close, but cannot
    // enter native work, even if another open handle prevents the rename.
    retire_staging_lease(&mut lock.0)?;
    drop(lock);
    let claimed = if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("claimed-"))
    {
        path.to_owned()
    } else {
        let claimed = path
            .parent()
            .context("staging parent")?
            .join(format!("claimed-{}", uuid::Uuid::new_v4()));
        match fs::rename(path, &claimed) {
            Ok(()) => {}
            // Windows may deny renaming a directory that a delayed child still
            // has open as its working directory. Leave it unchanged for a later
            // bounded tick after owner EOF makes that child exit.
            #[cfg(windows)]
            Err(error) if matches!(error.raw_os_error(), Some(5 | 32 | 33)) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        claimed
    };
    for name in names
        .iter()
        .filter(|name| name.as_os_str() != "active.lock")
    {
        fs::remove_file(claimed.join(name))?;
    }
    // Retirement persists on delayed handles even after unlink. Claimed names
    // are never launch locations; deletion can finish after delayed handles close.
    fs::remove_file(claimed.join("active.lock"))?;
    match fs::remove_dir(claimed) {
        Ok(()) => Ok(true),
        // An old, delayed Windows handle can retain an already-unlinked lock.
        // The claimed path remains non-launchable and is retried after close.
        #[cfg(windows)]
        Err(error) if matches!(error.raw_os_error(), Some(5 | 32 | 33 | 145)) => Ok(false),
        Err(error) => Err(error.into()),
    }
}
// A single byte has no partially written nonempty prefix to misclassify: any
// nonempty lease rejects startup, and only this owned retirement value is cleaned.
const RETIRED_LEASE: &[u8] = b"\x01";

fn retire_staging_lease(lock: &mut File) -> Result<()> {
    match lock.metadata()?.len() {
        0 => lock.write_all(RETIRED_LEASE)?,
        length if length == RETIRED_LEASE.len() as u64 => {
            let mut marker = [0; RETIRED_LEASE.len()];
            lock.read_exact(&mut marker)?;
            ensure!(
                marker.as_slice() == RETIRED_LEASE,
                "unrecognized worker lease state"
            );
        }
        _ => bail!("unrecognized worker lease state"),
    }
    lock.sync_all()?;
    Ok(())
}

fn validate_staging_identity(expected: &Path, lock: &File) -> Result<()> {
    // Inspect the acquired handle, not a second open: Windows byte-range locks
    // deny other handles, and a delayed opener may refer to an unlinked inode.
    ensure!(
        lock.metadata()?.len() == 0,
        "worker staging lease was retired"
    );
    ensure!(
        expected.is_dir() && std::env::current_dir()? == expected,
        "worker staging was claimed for recovery"
    );
    Ok(())
}
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "worker artifact is not a regular file"
    );
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    ensure!(
        len > 0 && len <= limit,
        "worker artifact size exceeds allowance"
    );
    let mut bytes = vec![0; usize::try_from(len)?];
    file.read_exact(&mut bytes)?;
    let mut extra = [0];
    ensure!(file.read(&mut extra)? == 0, "worker artifact grew");
    Ok(bytes)
}
fn write_exclusive(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
/// Internal executable mode. A bounded request is followed by an open stdin
/// lease. The watchdog exits the whole process, including native codec threads.
pub fn worker_main() -> Result<()> {
    let current = std::env::current_dir()?;
    let name = current
        .file_name()
        .and_then(|name| name.to_str())
        .context("worker staging path")?;
    ensure!(
        name.starts_with("worker-"),
        "worker requires a private staging directory"
    );
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(current.join("active.lock"))?;
    lock.try_lock_exclusive().context("worker staging lease")?;
    let lock = AcquiredLease(lock);
    validate_staging_identity(&current, &lock.0)?;
    let result = run_worker();
    if let Err(error) = &result {
        let mut message = format!("{error:#}");
        if message.len() > 8192 {
            let mut boundary = 8192;
            while !message.is_char_boundary(boundary) {
                boundary -= 1;
            }
            message.truncate(boundary);
        }
        let failure = WorkerFailure {
            decode_status: error
                .downcast_ref::<DecodeError>()
                .map(|e| e.status)
                .or_else(|| match error.downcast_ref::<crate::edit::RenderError>() {
                    Some(crate::edit::RenderError::ResourceLimit { .. }) => {
                        Some(DecodeStatus::ResourceLimit)
                    }
                    Some(
                        crate::edit::RenderError::SourceMissing | crate::edit::RenderError::Io(_),
                    ) => Some(DecodeStatus::Io),
                    Some(crate::edit::RenderError::Decode(error)) => Some(error.status),
                    _ => None,
                })
                .or_else(|| {
                    error
                        .downcast_ref::<std::io::Error>()
                        .map(|_| DecodeStatus::Io)
                }),
            message,
        };
        if let Ok(bytes) = serde_json::to_vec(&failure) {
            let _ = write_exclusive(Path::new("error.json"), &bytes);
        }
    }
    result
}
pub(crate) fn peak_resident_memory() -> (Option<u64>, String) {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0
            && let Ok(value) = u64::try_from(usage.ru_maxrss)
        {
            #[cfg(target_os = "macos")]
            let bytes = value;
            #[cfg(target_os = "linux")]
            let bytes = value.saturating_mul(1024);
            return (
                Some(bytes),
                "getrusage process high-water RSS; macOS bytes/Linux KiB normalized".into(),
            );
        }
        (None, "getrusage unavailable".into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        (
            None,
            "process high-water RSS unavailable on this platform".into(),
        )
    }
}
fn run_worker() -> Result<()> {
    let mut input = BufReader::new(std::io::stdin());
    let mut line = Vec::new();
    (&mut input)
        .take(RECEIPT_LIMIT)
        .read_until(b'\n', &mut line)?;
    ensure!(
        line.last() == Some(&b'\n') && (line.len() as u64) < RECEIPT_LIMIT,
        "incomplete worker request"
    );
    let request: RenderWork = serde_json::from_slice(&line)?;
    let source = request.validate()?;
    let mut start = [0];
    input
        .read_exact(&mut start)
        .context("owner lease ended before admission")?;
    ensure!(&start == b"!", "invalid worker start token");
    let (encode_admission, admitted) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("preview-owner-lease".into())
        .spawn(move || {
            let mut command = [0];
            if input.read_exact(&mut command).is_ok() && &command == b"E" {
                let _ = encode_admission.send(());
                // Exactly one encode token is permitted. EOF, another token,
                // or an error revokes the lease for the remainder of native work.
                let _ = input.read(&mut command);
            }
            std::process::exit(74);
        })?;
    let (instance, _source_write_lease) =
        SourceInstance::read_for_worker(&source, request.decode_limits.max_encoded_bytes)?;
    let mut prepared_receipt = None;
    let mut prepared_reused = false;
    let mut edit_input = None;
    let mut rendered = if let Some(edit) = &request.edit {
        let recipe = edit.recipe.validate()?;
        let cached = if edit.interactive {
            edit.prepared
                .as_ref()
                .filter(|reference| reference.source == instance)
                .and_then(|reference| {
                    let path = reference.path.to_path().ok()?;
                    let metadata = fs::symlink_metadata(&path).ok()?;
                    if !metadata.is_file()
                        || metadata.file_type().is_symlink()
                        || metadata.len() != reference.receipt.bytes
                    {
                        return None;
                    }
                    let mut file = File::open(path).ok()?;
                    crate::edit::read_prepared_proxy(
                        &mut file,
                        crate::edit::PreparedProxyExpectation {
                            source_fingerprint: &request.keys[0].fingerprint,
                            white_balance: &recipe.settings().white_balance,
                            longest_edge: PROXY_EDGE,
                            original_dimensions: reference.receipt.identity.original_dimensions,
                            blake3: &reference.receipt.blake3,
                        },
                        MAX_PROXY_BYTES,
                        edit.limits,
                        &(),
                    )
                    .ok()
                })
        } else {
            None
        };
        let input = match cached {
            Some(input) => {
                prepared_reused = true;
                let reference = edit
                    .prepared
                    .as_ref()
                    .context("missing consumed preparation")?;
                edit_input = Some(EditInputProvenance::PreparedProxy {
                    receipt: reference.receipt.clone(),
                    source_instance_digest: instance.digest()?,
                });
                input
            }
            None => {
                let original = crate::edit::decode_original(
                    crate::edit::OriginalRequest {
                        path: &source,
                        expected_fingerprint: &request.keys[0].fingerprint,
                        white_balance: &recipe.settings().white_balance,
                    },
                    request.decode_limits,
                    &(),
                )?;
                edit_input = Some(EditInputProvenance::OriginalDecoded);
                if edit.interactive {
                    let proxy =
                        crate::edit::prepare_linear_proxy(&original, PROXY_EDGE, edit.limits, &())?;
                    drop(original);
                    if edit.prepared_bytes > 0 {
                        let mut file = OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open("prepared.linear")?;
                        let receipt = crate::edit::write_prepared_proxy(
                            &proxy,
                            PROXY_EDGE,
                            &mut file,
                            edit.prepared_bytes,
                            &(),
                        )?;
                        file.sync_all()?;
                        prepared_receipt = Some((receipt, instance.clone()));
                    }
                    proxy
                } else {
                    original
                }
            }
        };
        let edge = request.keys.iter().map(|key| key.edge).max().unwrap();
        crate::edit::render_recipe(
            &input,
            &recipe,
            crate::edit::RenderPurpose::InteractiveProxy { longest_edge: edge },
            edit.limits,
            &(),
        )?
        .image
    } else {
        ensure!(
            crate::fingerprint(&source)? == request.keys[0].fingerprint,
            "original changed before rendering"
        );
        crate::media::decode_full_limited(&source, request.decode_limits)?
    };
    if let Some(edit) = &request.edit {
        rendered.provenance.notes.push(format!("Preview source: {}; prepared cache reused={}; output={}x{}. Interactive source-edge1600 crops/spatial filters are approximate, without upscaling.",
            if edit.interactive { "interactive linear proxy" } else { "refined original development" },
            prepared_reused, rendered.width, rendered.height));
    }
    write_exclusive(Path::new("decoded.pending"), b"decoded")?;
    fs::rename("decoded.pending", "decoded.ready")?;
    admitted.recv().context("owner encode admission ended")?;
    let mut objects = Vec::new();
    let mut total = 0u64;
    for (index, key) in request.keys.iter().enumerate() {
        let rgb = prepare(&rendered, key.edge)?;
        let bytes = encode(&rgb, key.encoding, None)?;
        total = total
            .checked_add(bytes.len() as u64)
            .context("worker bytes overflow")?;
        ensure!(
            total <= request.encoded_limit,
            "encoded worker allowance exceeded"
        );
        write_exclusive(Path::new(&format!("{index}.preview")), &bytes)?;
        objects.push(ObjectReceipt {
            key: key.clone(),
            width: rgb.width(),
            height: rgb.height(),
            bytes: bytes.len() as u64,
            checksum: blake3::hash(&bytes).to_hex().to_string(),
        });
    }
    ensure!(
        instance.same_observed_metadata(&SourceInstance::read(&source)?),
        "original file instance changed during rendering"
    );
    if request.edit.is_none() {
        ensure!(
            crate::fingerprint(&source)? == request.keys[0].fingerprint,
            "original changed during rendering"
        );
    }
    // This covers source decode, both tier preparations/encodes and final
    // source verification. Only the bounded 64 KiB receipt write follows.
    let (peak_resident_bytes, peak_method) = peak_resident_memory();
    let receipt = serde_json::to_vec(&RenderReceipt {
        edit_input,
        peak_resident_bytes,
        peak_method,
        metadata: rendered.metadata,
        provenance: rendered.provenance,
        objects,
        prepared: prepared_receipt,
    })?;
    ensure!(
        receipt.len() as u64 <= RECEIPT_LIMIT,
        "worker receipt limit"
    );
    write_exclusive(Path::new("result.json"), &receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> PreviewKey {
        PreviewKey {
            asset_id: "test".into(),
            variant_id: "master".into(),
            generation: 1,
            fingerprint: "a".repeat(64),
            edit_revision: 0,
            renderer_version: renderer_identity().into(),
            preparation_version: super::super::PREPARATION_VERSION.into(),
            tier: super::super::Tier::Thumbnail,
            edge: 512,
            encoding: super::super::CodecSettings {
                codec: Codec::Jpeg,
                quality: 80,
            },
        }
    }
    #[test]
    fn checksummed_but_truncated_worker_image_is_not_publishable() {
        let key = key();
        let pixels = PreparedRgb::new(17, 11, vec![128; 17 * 11 * 3]).unwrap();
        let mut encoded = encode(&pixels, key.encoding, None).unwrap();
        encoded.truncate(encoded.len() - 2);
        let checksum = blake3::hash(&encoded).to_hex().to_string();
        assert_eq!(blake3::hash(&encoded).to_hex().as_str(), checksum);
        assert!(validate_object(&key, 17, 11, &encoded).is_err());
    }
    #[test]
    fn recovery_preserves_active_staging_and_removes_interrupted_output_after_exit() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("worker-interrupted");
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("0.preview"), b"partial").unwrap();
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(stage.join("active.lock"))
            .unwrap();
        lease.try_lock_exclusive().unwrap();
        assert_eq!(recover_worker_staging(root.path(), 1).unwrap(), 0);
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(stage.join("active.lock"))
            .unwrap();
        assert!(
            contender.try_lock_exclusive().is_err(),
            "failed recovery must not unlock the active owner"
        );
        drop(contender);
        assert!(stage.join("0.preview").exists());
        FileExt::unlock(&lease).unwrap();
        drop(lease);
        assert_eq!(recover_worker_staging(root.path(), 1).unwrap(), 1);
        assert!(!stage.exists());
    }
    #[cfg(unix)]
    #[test]
    fn actually_killed_child_partial_file_never_becomes_a_completed_batch() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("worker-killed");
        fs::create_dir(&stage).unwrap();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "printf partial > 0.preview; kill -KILL $$"])
            .current_dir(&stage)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let lease = child.stdin.take();
        let request = RenderWork {
            edit: None,
            source: NativePath::from_path(&root.path().join("original")),
            keys: vec![key()],
            encoded_limit: 1024,
            decode_limits: DecodeLimits::default(),
        };
        let mut worker = WorkerProcess {
            child,
            lease,
            staging: stage.clone(),
            request,
            exited: false,
            encoding_admitted: false,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match worker.poll(&AtomicBool::new(false)) {
                Ok(None) => {}
                Err(_) => break,
                Ok(Some(_)) => panic!("partial worker published"),
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(fs::read(stage.join("0.preview")).unwrap(), b"partial");
        drop(worker);
        assert!(!stage.exists());
    }
    #[test]
    fn delayed_startup_handle_cannot_enter_recovered_staging() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("worker-delayed");
        fs::create_dir(&stage).unwrap();
        // Exact race order: old child opened the lock but has not acquired it.
        let delayed = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(stage.join("active.lock"))
            .unwrap();
        let removed = clean_staging_directory(&stage).unwrap();
        // A platform may deny locking an already-unlinked handle; if it permits
        // locking, the mandatory post-lock startup identity check still rejects.
        if delayed.try_lock_exclusive().is_ok() {
            assert!(
                validate_staging_identity(&stage, &delayed)
                    .unwrap_err()
                    .to_string()
                    .contains("retired")
            );
            FileExt::unlock(&delayed).unwrap();
        }
        // A delayed Windows handle may also prevent renaming the directory.
        // Retirement must reject it regardless of whether the old path remains.
        assert!(delayed.metadata().unwrap().len() > 0);
        if removed {
            assert!(!stage.exists());
        }
        drop(delayed);
        if !removed {
            assert_eq!(recover_worker_staging(root.path(), 1).unwrap(), 1);
        }
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[test]
    fn retired_lease_rejects_delayed_startup_before_directory_claim() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("worker-retired");
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("0.preview"), b"partial").unwrap();
        let mut recovery = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(stage.join("active.lock"))
            .unwrap();
        let delayed = OpenOptions::new()
            .read(true)
            .write(true)
            .open(stage.join("active.lock"))
            .unwrap();
        recovery.try_lock_exclusive().unwrap();
        retire_staging_lease(&mut recovery).unwrap();
        FileExt::unlock(&recovery).unwrap();
        drop(recovery); // Completed retirement before directory claim.
        delayed.try_lock_exclusive().unwrap();
        assert!(stage.is_dir());
        let error = validate_staging_identity(&stage, &delayed).unwrap_err();
        assert!(error.to_string().contains("retired"));
        assert_eq!(recover_worker_staging(root.path(), 1).unwrap(), 0);
        assert_eq!(fs::read(stage.join("0.preview")).unwrap(), b"partial");
        FileExt::unlock(&delayed).unwrap();
        drop(delayed);
        assert_eq!(recover_worker_staging(root.path(), 1).unwrap(), 1);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn completed_recovery_releases_duplicate_description() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("worker-duplicate-description");
        fs::create_dir(&stage).unwrap();
        let lease = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(stage.join("active.lock"))
            .unwrap();
        lease.try_lock_exclusive().unwrap();
        let mut lease = AcquiredLease(lease);
        let inherited = lease.0.try_clone().unwrap();
        let delayed = OpenOptions::new()
            .read(true)
            .write(true)
            .open(stage.join("active.lock"))
            .unwrap();
        assert!(delayed.try_lock_exclusive().is_err());
        retire_staging_lease(&mut lease.0).unwrap();
        drop(lease);
        delayed.try_lock_exclusive().unwrap();
        assert!(validate_staging_identity(&stage, &delayed).is_err());
        assert_eq!(inherited.metadata().unwrap().len(), 1);
        drop(inherited);
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(stage.join("active.lock"))
            .unwrap();
        assert!(
            contender.try_lock_exclusive().is_err(),
            "closing the old description must not release the new owner"
        );
        FileExt::unlock(&delayed).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recovery_error_and_unwind_release_inherited_description() {
        for unwind in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("active.lock");
            fs::write(&path, []).unwrap();
            let mut inherited = None;
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                    let file = OpenOptions::new().read(true).write(true).open(&path)?;
                    file.try_lock_exclusive()?;
                    let _lease = AcquiredLease(file);
                    inherited = Some(_lease.0.try_clone()?);
                    if unwind {
                        panic!("injected recovery unwind");
                    }
                    bail!("injected recovery validation failure")
                }));
            if unwind {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().is_err());
            }
            let contender = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            contender.try_lock_exclusive().unwrap();
            assert_eq!(contender.metadata().unwrap().len(), 0);
            FileExt::unlock(&contender).unwrap();
            drop(inherited);
        }
    }

    #[test]
    fn unrecognized_lease_state_is_preserved_for_inspection() {
        for marker in [b"\x02".as_slice(), b"unexpected".as_slice()] {
            let root = tempfile::tempdir().unwrap();
            let stage = root.path().join("worker-unknown");
            fs::create_dir(&stage).unwrap();
            fs::write(stage.join("active.lock"), marker).unwrap();
            fs::write(stage.join("0.preview"), b"partial").unwrap();
            assert!(recover_worker_staging(root.path(), 1).is_err());
            assert_eq!(fs::read(stage.join("active.lock")).unwrap(), marker);
            assert_eq!(fs::read(stage.join("0.preview")).unwrap(), b"partial");
        }
    }

    #[test]
    fn interrupted_claim_cleanup_is_resumable() {
        let root = tempfile::tempdir().unwrap();
        let claimed = root.path().join("claimed-interrupted");
        fs::create_dir(&claimed).unwrap();
        fs::write(claimed.join("0.preview"), b"partial").unwrap();
        assert_eq!(recover_worker_staging(root.path(), 1).unwrap(), 1);
        assert!(!claimed.exists());
    }
    #[test]
    fn hostile_output_length_is_rejected_before_reading() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output");
        let file = File::create(&path).unwrap();
        file.set_len(128 * 1024 * 1024).unwrap();
        assert!(
            read_bounded(&path, 1024)
                .err()
                .unwrap()
                .to_string()
                .contains("allowance")
        );
    }
}
