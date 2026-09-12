//! One owned export process. Small IPC carries only admission; selected packets
//! and ICC profiles use bounded, hash-bound files. EOF revokes native ownership.
use crate::{
    catalog_exports::{ExportWork, StoredProfile},
    image_export::{OutputProfile, OutputSpec},
    metadata_export::{self, SealedPhotoExport},
    photo_render::{self, PhotoRenderLimits, PhotoRenderRequest, StagedPhoto},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};
const REQUEST_LIMIT: u64 = 256 * 1024;
const RECEIPT_LIMIT: u64 = 64 * 1024;
const BLOB_LIMIT: u64 = 16 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    work: ExportWork,
    limits: PhotoRenderLimits,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedExport {
    pub authority: String,
    pub attempt: String,
    pub sealed: SealedPhotoExport,
    pub rendered: StagedPhoto,
    pub seal_ms: f64,
    pub peak_resident_bytes: Option<u64>,
    pub peak_method: String,
}
fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= limit,
        "worker artifact size/type"
    );
    let mut bytes = vec![0; usize::try_from(metadata.len())?];
    let mut file = File::open(path)?;
    file.read_exact(&mut bytes)?;
    let mut extra = [0];
    ensure!(
        file.read(&mut extra)? == 0,
        "worker artifact changed or exceeded bound"
    );
    Ok(bytes)
}
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn blob(root: &Path, name: &str, expected: &str) -> Result<Vec<u8>> {
    let bytes = read(&root.join(name), BLOB_LIMIT)?;
    ensure!(
        blake3::hash(&bytes).to_hex().as_str() == expected,
        "worker blob digest mismatch"
    );
    Ok(bytes)
}
fn validate(request: &Request) -> Result<()> {
    validate_persisted(request)?;
    ensure!(
        request.work.plan.renderer_identity == photo_render::output_renderer_identity(),
        "export renderer changed; prepare a new plan"
    );
    Ok(())
}
fn validate_persisted(request: &Request) -> Result<()> {
    ensure!(
        request.version == 1
            && ((request.work.plan.version == 1
                && request.work.plan.identity.image_identity.is_none())
                || (request.work.plan.version == 2
                    && request
                        .work
                        .plan
                        .identity
                        .image_identity
                        .as_ref()
                        .is_some_and(|image| image.key == request.work.plan.identity.key))),
        "export worker protocol"
    );
    let encoded = serde_json::to_vec(&request.work.plan)?;
    ensure!(
        blake3::hash(&encoded).to_hex().as_str() == request.work.authority,
        "export worker plan binding"
    );
    ensure!(
        request.work.plan.recipe.validate()?.digest() == request.work.plan.identity.recipe_digest,
        "export worker recipe binding"
    );
    ensure!(
        request.work.plan.identity.source.fingerprint.as_deref()
            == Some(&request.work.plan.original_revision.digest),
        "export worker source binding"
    );
    ensure!(
        request.limits.max_encoded_extent == request.work.plan.max_payload_bytes,
        "export worker extent allowance differs from plan"
    );
    ensure!(
        request.limits.decode.max_encoded_bytes <= request.work.plan.max_original_bytes,
        "export decode allowance exceeds plan"
    );
    Ok(())
}
/// Application supplies the executable, work authority and explicit limits. It
/// owns the global native reservation until poll/stop has reaped this child.
pub struct ExportWorkerProcess {
    child: Child,
    lease: Option<ChildStdin>,
    staging: PathBuf,
    request: Request,
    exited: bool,
}
impl ExportWorkerProcess {
    pub fn spawn(
        executable: &Path,
        staging_root: &Path,
        work: ExportWork,
        output: &OutputSpec,
        xmp: Option<&[u8]>,
        limits: PhotoRenderLimits,
    ) -> Result<Self> {
        ensure!(
            executable.is_absolute() && staging_root.is_absolute(),
            "absolute export executable/staging paths required"
        );
        let request = Request {
            version: 1,
            work,
            limits,
        };
        validate(&request)?;
        let bytes = serde_json::to_vec(&request)?;
        ensure!(
            bytes.len() as u64 <= REQUEST_LIMIT,
            "export worker request limit"
        );
        let plan = &request.work.plan;
        ensure!(
            output.size == plan.output.size
                && output.format == plan.output.format
                && output.alpha == plan.output.alpha,
            "export output differs from plan"
        );
        match (&plan.output.profile, &output.profile) {
            (StoredProfile::Srgb, OutputProfile::Srgb)
            | (StoredProfile::LinearSrgb, OutputProfile::LinearSrgb) => {}
            (StoredProfile::Icc { blob }, OutputProfile::Icc { bytes }) => ensure!(
                bytes.len() as u64 <= BLOB_LIMIT && blake3::hash(bytes).to_hex().as_str() == blob,
                "export ICC differs from plan"
            ),
            _ => bail!("export profile differs from plan"),
        }
        ensure!(
            match (plan.xmp_blob.as_deref(), xmp) {
                (None, None) => true,
                (Some(hash), Some(bytes)) =>
                    bytes.len() as u64 <= BLOB_LIMIT
                        && blake3::hash(bytes).to_hex().as_str() == hash,
                _ => false,
            },
            "selected XMP differs from plan"
        );
        fs::create_dir_all(staging_root)?;
        // current_dir in the child is physical (for example /private/var on
        // macOS); bind transport receipts to that same normalized parent.
        let staging_root = staging_root.canonicalize()?;
        let staging = tempfile::Builder::new()
            .prefix("photo-worker-")
            .tempdir_in(&staging_root)?
            .keep();
        // The owner creates the lease before a child can be canceled or delayed
        // before startup. Children open it; they never recreate a retired lease.
        write(&staging.join("active.lock"), b"")?;
        write(&staging.join("request.json"), &bytes)?;
        if let OutputProfile::Icc { bytes } = &output.profile {
            write(&staging.join("profile.icc"), bytes)?;
        }
        if let Some(bytes) = xmp {
            write(&staging.join("selected.xmp"), bytes)?;
        }
        let mut child = Command::new(executable)
            .arg("--photo-export-worker")
            .current_dir(&staging)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("OMP_NUM_THREADS", "1")
            .env("RAYON_NUM_THREADS", "1")
            .spawn()
            .context("start export worker")?;
        let lease = child.stdin.take().context("export worker stdin lease")?;
        let mut value = Self {
            child,
            lease: Some(lease),
            staging,
            request,
            exited: false,
        };
        let lease = value.lease.as_mut().unwrap();
        lease.write_all(b"!")?;
        lease.flush()?;
        Ok(value)
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
    pub fn staging_directory(&self) -> &Path {
        &self.staging
    }
    pub fn work(&self) -> &ExportWork {
        &self.request.work
    }
    pub fn poll(&mut self, canceled: &AtomicBool) -> Result<Option<CompletedExport>> {
        ensure!(!self.exited, "export worker already consumed");
        if canceled.load(Ordering::Acquire) {
            self.stop()?;
            bail!("export worker canceled");
        }
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        self.exited = true;
        self.lease.take();
        if !status.success() {
            let detail = read(&self.staging.join("error.json"), RECEIPT_LIMIT)
                .ok()
                .and_then(|b| serde_json::from_slice::<String>(&b).ok())
                .unwrap_or_else(|| format!("export worker failed ({status})"));
            bail!("{detail}");
        }
        let result: CompletedExport =
            serde_json::from_slice(&read(&self.staging.join("result.json"), RECEIPT_LIMIT)?)?;
        let work = &self.request.work;
        ensure!(
            result.authority == work.authority
                && result.attempt == work.attempt
                && result.sealed.authority_digest == work.authority
                && result.sealed.snapshot == work.plan.destination
                && result.sealed.max_payload_bytes == work.plan.max_payload_bytes
                && result.rendered.renderer_identity == work.plan.renderer_identity
                && result.rendered.staging == self.staging.join("output")
                && result.rendered.encoding.source_fingerprint
                    == work.plan.original_revision.digest
                && result.rendered.encoding.recipe_digest == work.plan.identity.recipe_digest
                && result.rendered.encoding.encoded_extent == result.sealed.payload.bytes,
            "export worker result binding mismatch"
        );
        // Verify the complete staged object under the durable seal, never an
        // untrusted worker pathname or encoded whole-image transport.
        ensure!(
            metadata_export::read_photo_seal(&work.plan.destination, &work.authority)?
                == result.sealed,
            "export worker seal changed"
        );
        Ok(Some(result))
    }
    pub fn stop(&mut self) -> Result<()> {
        if !self.exited {
            self.lease.take();
            if self.child.try_wait()?.is_none() {
                let killed = self.child.kill();
                if self.child.try_wait()?.is_none() {
                    killed?;
                }
            }
            self.child.wait()?;
            self.exited = true;
        }
        Ok(())
    }
    /// Only this reaped worker's disposable transport files are removed. Durable
    /// publication/restore evidence resides in its separately sealed directory.
    pub fn retire_transport(&self) -> Result<()> {
        ensure!(self.exited, "cannot retire a live export worker");
        match fence_transport(&self.staging)? {
            Inspection::Retired(retired) => {
                ensure!(
                    same_work(&retired.work, &self.request.work),
                    "transport attempt changed"
                );
                discard_retired_export_transport(&retired)
            }
            Inspection::Cleaned => Ok(()),
            Inspection::Retained(reason) => bail!("export transport retained: {reason}"),
        }
    }
}
impl Drop for ExportWorkerProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

const RETIRED_PREFIX: &str = "photo-retired-";
const TRANSPORT_FILES: &[&str] = &[
    "active.lock",
    "request.json",
    "profile.icc",
    "selected.xmp",
    "output",
    "result.json",
    "error.json",
];
#[derive(Debug, Serialize)]
pub struct RetiredExportTransport {
    pub staging: PathBuf,
    pub work: ExportWork,
}
#[derive(Debug, Serialize)]
pub struct RetainedExportTransport {
    pub staging: PathBuf,
    pub reason: String,
}
#[derive(Debug, Default, Serialize)]
pub struct ExportTransportRecovery {
    pub scanned: usize,
    /// Interrupted cleanup of an already fenced, non-launchable directory.
    pub cleaned: usize,
    pub retired: Vec<RetiredExportTransport>,
    pub retained: Vec<RetainedExportTransport>,
}
enum Inspection {
    Retired(Box<RetiredExportTransport>),
    Retained(String),
    Cleaned,
}
fn same_work(a: &ExportWork, b: &ExportWork) -> bool {
    a.authority == b.authority
        && a.attempt == b.attempt
        && a.job == b.job
        && a.sequence == b.sequence
}
fn retired_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(RETIRED_PREFIX))
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}
fn transport_files(path: &Path) -> Result<Vec<PathBuf>> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "transport is not an ordinary directory"
    );
    let mut paths = Vec::new();
    for entry in fs::read_dir(path)?.take(TRANSPORT_FILES.len() + 1) {
        let entry = entry?;
        ensure!(
            paths.len() < TRANSPORT_FILES.len()
                && TRANSPORT_FILES
                    .iter()
                    .any(|name| entry.file_name() == *name)
                && entry.file_type()?.is_file(),
            "unrecognized export transport artifact"
        );
        paths.push(entry.path());
    }
    Ok(paths)
}
fn open_lease(path: &Path) -> Result<File> {
    let target = path.join("active.lock");
    match fs::symlink_metadata(&target) {
        Ok(metadata) => ensure!(metadata.file_type().is_file(), "invalid export lease type"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(target)?)
}
// Construct only after successful acquisition. Closing one descriptor does not
// release flock while a concurrent fork/dup retains its open-file description.
// Explicit unlock ends this authority on every return/error before file close.
struct AcquiredLease(File);
impl Drop for AcquiredLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
fn lease_retired(lock: &mut File) -> Result<bool> {
    ensure!(lock.metadata()?.len() <= 1, "invalid export lease marker");
    lock.seek(SeekFrom::Start(0))?;
    let mut byte = [0];
    let n = lock.read(&mut byte)?;
    ensure!(n == 0 || byte == [1], "unknown export lease marker");
    Ok(n != 0)
}
fn mark_retired(lock: &mut File) -> Result<()> {
    lock.seek(SeekFrom::Start(0))?;
    lock.write_all(&[1])?;
    lock.sync_all()?;
    Ok(())
}
fn lease_identity(file: &File) -> std::io::Result<(u64, u128)> {
    crate::storage_volume::held_object_key(file)
}
fn check_live_lease(path: &Path, lock: &File) -> Result<()> {
    ensure!(
        lock.metadata()?.len() == 0,
        "export worker staging was retired"
    );
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir()
            && fs::symlink_metadata(path.join("active.lock"))?
                .file_type()
                .is_file(),
        "export staging type changed"
    );
    // Compare the held handle, not two fresh path lookups (including Windows).
    ensure!(
        lease_identity(lock)? == lease_identity(&File::open(path.join("active.lock"))?)?,
        "export worker staging lock identity changed"
    );
    Ok(())
}
fn discard_files(path: &Path, lock: AcquiredLease, paths: Vec<PathBuf>) -> Result<()> {
    // The durable marker stays on delayed handles. A retired directory name is
    // never accepted by a child, even after its lock pathname has been removed.
    drop(lock);
    for file in paths.iter().filter(|file| {
        !file
            .file_name()
            .is_some_and(|name| name == "request.json" || name == "active.lock")
    }) {
        fs::remove_file(file)?;
    }
    // Keep attempt proof until all bulky/optional files have been removed.
    for name in ["request.json", "active.lock"] {
        match fs::remove_file(path.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    fs::remove_dir(path)?;
    Ok(())
}
fn fence_transport(path: &Path) -> Result<Inspection> {
    let paths = transport_files(path)?;
    let retired = retired_name(path);
    ensure!(
        retired
            || path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("photo-worker-")),
        "unknown export transport name"
    );
    if retired && paths.is_empty() {
        fs::remove_dir(path)?;
        return Ok(Inspection::Cleaned);
    }
    let lock = open_lease(path)?;
    if let Err(error) = lock.try_lock_exclusive() {
        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
            return Ok(Inspection::Retained(
                "worker lease is busy; no relaunch admitted".into(),
            ));
        }
        return Err(error.into());
    }
    let mut lock = AcquiredLease(lock);
    let marked = lease_retired(&mut lock.0)?;
    ensure!(
        lease_identity(&lock.0)? == lease_identity(&File::open(path.join("active.lock"))?)?,
        "export recovery lease identity changed"
    );
    if retired {
        ensure!(marked, "retired transport lacks retirement marker");
    }
    let request_path = path.join("request.json");
    if !request_path.try_exists()? {
        if retired {
            // Only discard removes request.json, after the catalog owner has
            // reconciled the returned retirement proof. No work is fabricated.
            discard_files(path, lock, paths)?;
            return Ok(Inspection::Cleaned);
        }
        return Ok(Inspection::Retained(
            "partial spawn lacks a request; ownership unknown".into(),
        ));
    }
    let request: Request = serde_json::from_slice(&read(&request_path, REQUEST_LIMIT)?)?;
    validate_persisted(&request)?;
    mark_retired(&mut lock.0)?;
    drop(lock);
    let staging = if retired {
        path.to_owned()
    } else {
        let claimed = path
            .parent()
            .context("export transport parent")?
            .join(format!("{RETIRED_PREFIX}{}", uuid::Uuid::new_v4()));
        // A delayed Windows child may hold its current directory open. Failure
        // leaves the tombstoned original plus request intact for a later pass.
        fs::rename(path, &claimed)?;
        claimed
    };
    Ok(Inspection::Retired(Box::new(RetiredExportTransport {
        staging,
        work: request.work,
    })))
}
/// The caller must hold the catalog's exclusive export-executor lease and have
/// no owned active workers. Every retained entry blocks another native launch.
/// This never kills a PID or accepts/publishes a discovered seal. The bounded
/// scan reads small requests only; output/profile/XMP contents are not loaded.
pub fn recover_export_transports(
    root: &Path,
    max_directories: usize,
) -> Result<ExportTransportRecovery> {
    ensure!(
        root.is_absolute() && (1..=1024).contains(&max_directories),
        "export recovery bounds"
    );
    match fs::symlink_metadata(root) {
        Ok(metadata) => ensure!(metadata.file_type().is_dir(), "export recovery root type"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExportTransportRecovery::default());
        }
        Err(error) => return Err(error.into()),
    }
    let entries = fs::read_dir(root)?
        .take(max_directories + 1)
        .collect::<std::io::Result<Vec<_>>>()?;
    // Do not fence only a prefix and accidentally imply the remaining directory
    // set is quiet. The owner must increase its explicit bound or inspect it.
    ensure!(
        entries.len() <= max_directories,
        "export recovery directory bound exceeded"
    );
    let mut result = ExportTransportRecovery {
        scanned: entries.len(),
        ..Default::default()
    };
    for entry in entries {
        let staging = entry.path();
        match fence_transport(&staging) {
            Ok(Inspection::Retired(value)) => result.retired.push(*value),
            Ok(Inspection::Cleaned) => result.cleaned += 1,
            Ok(Inspection::Retained(reason)) => result
                .retained
                .push(RetainedExportTransport { staging, reason }),
            Err(error) => result.retained.push(RetainedExportTransport {
                staging,
                reason: format!("{error:#}").chars().take(2048).collect(),
            }),
        }
    }
    Ok(result)
}
/// Call only after the catalog has fenced/reconciled this exact retired attempt.
/// Its durable destination seal is outside transport and is never removed here.
pub fn discard_retired_export_transport(retired: &RetiredExportTransport) -> Result<()> {
    ensure!(retired_name(&retired.staging), "transport was not fenced");
    let paths = transport_files(&retired.staging)?;
    let lock = open_lease(&retired.staging)?;
    lock.try_lock_exclusive()
        .context("retired export lease busy")?;
    let mut lock = AcquiredLease(lock);
    ensure!(
        lease_retired(&mut lock.0)?,
        "transport retirement marker missing"
    );
    let request: Request =
        serde_json::from_slice(&read(&retired.staging.join("request.json"), REQUEST_LIMIT)?)?;
    validate_persisted(&request)?;
    ensure!(
        same_work(&retired.work, &request.work),
        "retired export attempt changed"
    );
    discard_files(&retired.staging, lock, paths)
}

pub fn export_worker_main() -> Result<()> {
    let current = std::env::current_dir()?;
    ensure!(
        current
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("photo-worker-")),
        "export worker requires its private staging directory"
    );
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(current.join("active.lock"))?;
    lock.try_lock_exclusive()?;
    let lock = AcquiredLease(lock);
    check_live_lease(&current, &lock.0)?;
    let result = (|| -> Result<()> {
        let mut input = std::io::stdin();
        let mut token = [0];
        input.read_exact(&mut token)?;
        ensure!(&token == b"!", "export owner admission missing");
        std::thread::Builder::new()
            .name("export-owner-lease".into())
            .spawn(move || {
                let _ = input.read(&mut token);
                std::process::exit(74);
            })?;
        let request: Request =
            serde_json::from_slice(&read(&current.join("request.json"), REQUEST_LIMIT)?)?;
        validate(&request)?;
        let plan = &request.work.plan;
        let profile = match &plan.output.profile {
            StoredProfile::Srgb => OutputProfile::Srgb,
            StoredProfile::LinearSrgb => OutputProfile::LinearSrgb,
            StoredProfile::Icc { blob: hash } => OutputProfile::Icc {
                bytes: blob(&current, "profile.icc", hash)?,
            },
        };
        let xmp = plan
            .xmp_blob
            .as_deref()
            .map(|hash| blob(&current, "selected.xmp", hash))
            .transpose()?;
        let output = OutputSpec {
            size: plan.output.size,
            format: plan.output.format,
            profile,
            alpha: plan.output.alpha,
        };
        let original = plan.original.to_path()?;
        let rendered = photo_render::render_staged_photo(
            PhotoRenderRequest {
                original: &original,
                expected_fingerprint: &plan.original_revision.digest,
                recipe: &plan.recipe,
                output: &output,
                selected_xmp: xmp.as_deref(),
                staging: &current.join("output"),
            },
            request.limits,
            &(),
        )?;
        let seal_started = std::time::Instant::now();
        let prior_directory = plan
            .destination
            .destination
            .parent()
            .context("export destination parent")?
            .join(format!(
                ".photocatalog-photo-export-{}",
                plan.destination.operation
            ));
        let sealed = if prior_directory.try_exists()? {
            // A fenced retry must independently render again. It may reuse old
            // sealed bytes only after a complete fresh-byte equality check.
            let prior =
                metadata_export::read_photo_seal(&plan.destination, &request.work.authority)?;
            let fresh =
                metadata_export::inspect_file_revision(&rendered.staging, plan.max_payload_bytes)?;
            ensure!(
                fresh.bytes == prior.payload.bytes && fresh.digest == prior.payload.digest,
                "orphan seal differs from fresh render; prepare a new export plan"
            );
            prior
        } else {
            metadata_export::seal_photo_export(
                &plan.destination,
                &rendered.staging,
                plan.max_payload_bytes,
                &request.work.authority,
                |_| Ok(()),
            )?
        };
        let (peak_resident_bytes, peak_method) = crate::preview::peak_resident_memory();
        let receipt = CompletedExport {
            authority: request.work.authority,
            attempt: request.work.attempt,
            sealed,
            rendered,
            seal_ms: seal_started.elapsed().as_secs_f64() * 1000.,
            peak_resident_bytes,
            peak_method,
        };
        let bytes = serde_json::to_vec(&receipt)?;
        ensure!(
            bytes.len() as u64 <= RECEIPT_LIMIT,
            "export result receipt limit"
        );
        write(&current.join("result.json"), &bytes)?;
        Ok(())
    })();
    if let Err(error) = &result {
        let mut message = format!("{error:#}");
        if message.len() > 8192 {
            let mut n = 8192;
            while !message.is_char_boundary(n) {
                n -= 1;
            }
            message.truncate(n);
        }
        let _ = write(&current.join("error.json"), &serde_json::to_vec(&message)?);
    }
    result
}

#[cfg(test)]
#[path = "export_worker/tests.rs"]
mod tests;
