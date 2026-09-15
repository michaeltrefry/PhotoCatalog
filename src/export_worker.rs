//! One owned export process. Small IPC carries only admission; selected packets
//! and ICC profiles use bounded, hash-bound files. EOF revokes native ownership.
use crate::{
    catalog_exports::{ExportWork, StoredOutput, StoredProfile},
    image_export::{OutputProfile, OutputSpec},
    metadata_export::{self, FileRevision, SealedPhotoExport, VerifiedFile},
    photo_render::{self, PhotoRenderLimits, PhotoRenderRequest, StagedPhoto},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};
#[path = "export_worker/wire.rs"]
mod wire;
pub(crate) const REQUEST_LIMIT: u64 = 256 * 1024;
pub(crate) const RECEIPT_LIMIT: u64 = 64 * 1024;
pub(crate) const BLOB_LIMIT: u64 = 16 * 1024 * 1024;
struct Request {
    version: u32,
    work: ExportWork,
    limits: PhotoRenderLimits,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "wire::Completion")]
pub struct CompletedExport {
    pub authority: String,
    pub attempt: String,
    pub sealed: SealedPhotoExport,
    pub rendered: StagedPhoto,
    pub seal_ms: f64,
    pub peak_resident_bytes: Option<u64>,
    pub peak_method: String,
}
/// Bounded facts emitted by the native renderer. Destination sealing is a
/// separate parent operation after the exact child has exited.
#[derive(Debug, Deserialize)]
#[serde(try_from = "wire::RenderingCompletion")]
pub struct ExportRenderingFacts {
    pub job: String,
    pub sequence: i64,
    pub authority: String,
    pub attempt: String,
    pub output: StoredOutput,
    pub output_revision: FileRevision,
    pub rendered: StagedPhoto,
    pub peak_resident_bytes: Option<u64>,
    pub peak_method: String,
}
pub(crate) fn admit_output_path(work: &ExportWork, path: &Path) -> Result<()> {
    // Reserve half the established receipt limit for fixed encoder reports,
    // notes and timing fields; path arrays and the snapshot share the remainder.
    let paths = serde_json::to_vec(&crate::storage_volume::NativePath::from_path(path))?.len()
        + serde_json::to_vec(&work.plan.destination)?.len();
    ensure!(
        paths as u64 <= RECEIPT_LIMIT / 2,
        "export native paths exceed worker result budget"
    );
    Ok(())
}
fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = metadata_export::open_regular(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.len() <= limit, "worker artifact size/type");
    let mut bytes = vec![0; usize::try_from(metadata.len())?];
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
    ensure!(matches!(request.version, 1 | 2), "export worker protocol");
    ensure!(
        request.version != 1 || matches!(request.work.plan.version, 1 | 2),
        "legacy worker requires legacy plan"
    );
    crate::catalog_exports::checked_plan(request.work.plan.raw(), &request.work.authority)?;
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

/// Produces the exact request consumed by the render-only native worker. The
/// managed filesystem owner calls this before creating any stage effects.
pub(crate) fn prepare_managed_request(
    work: &ExportWork,
    limits: PhotoRenderLimits,
    staging: &Path,
) -> Result<Vec<u8>> {
    let request = Request {
        version: 2,
        work: work.clone(),
        limits,
    };
    validate(&request)?;
    admit_output_path(work, &staging.join("output"))?;
    let bytes = serde_json::to_vec(&request)?;
    ensure!(
        bytes.len() as u64 <= REQUEST_LIMIT,
        "export worker request limit"
    );
    Ok(bytes)
}

/// Reads only render facts and performs the same strict post-exit sealing and
/// durable readback as the standalone parent. Callers must first prove that the
/// exact native operation has drained.
pub(crate) fn complete_managed_rendering(
    work: &ExportWork,
    staging: &Path,
    canceled: &AtomicBool,
    admit: impl FnOnce(CompletedExport) -> Result<()>,
) -> Result<CompletedExport> {
    let facts = match wire::decode_completion(&read(&staging.join("result.json"), RECEIPT_LIMIT)?)?
    {
        wire::DecodedCompletion::Rendering(facts) => facts,
        wire::DecodedCompletion::Legacy(_) => {
            bail!("managed export requires render-only native facts")
        }
    };
    validate_rendering(work, staging, &facts)?;
    // The 64 KiB allowance belongs to N's receipt. Admit the enriched result
    // through the actual F/C envelopes before creating any durable seal. Only
    // payload filesystem identity/mtime and elapsed seal timing can change;
    // maximal-width scalar representatives bound their serialized backing.
    let mut payload = facts.output_revision.clone();
    payload.modified_ns = u128::MAX;
    payload.identity = (u64::MAX, u64::MAX);
    admit(CompletedExport {
        authority: facts.authority.clone(),
        attempt: facts.attempt.clone(),
        sealed: SealedPhotoExport {
            version: work.plan.destination.version,
            snapshot: work.plan.destination.clone(),
            authority_digest: work.authority.clone(),
            max_payload_bytes: work.plan.max_payload_bytes,
            payload,
        },
        rendered: facts.rendered.clone(),
        seal_ms: -1.2345678901234567e-123,
        peak_resident_bytes: facts.peak_resident_bytes,
        peak_method: facts.peak_method.clone(),
    })?;
    complete_rendering(work, facts, canceled)
}
/// Application supplies the executable, work authority and explicit limits. It
/// owns the global native reservation until poll/stop has reaped this child.
pub struct ExportWorkerProcess {
    child: Child,
    lease: Option<ChildStdin>,
    parent_lease: RefCell<Option<File>>,
    staging: PathBuf,
    request: Request,
    exited: bool,
    consumed: bool,
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
            version: 2,
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
        admit_output_path(&request.work, &staging.join("output"))?;
        // This distinct parent lease protects the stage after child wait while
        // poll validates the receipt and seals/reads back the rendered output.
        write(&staging.join("parent.lock"), b"")?;
        let parent_lease = metadata_export::open_regular(&staging.join("parent.lock"))?;
        parent_lease
            .try_lock_exclusive()
            .context("acquire export parent lease")?;
        check_parent_lease(&staging, &parent_lease)?;
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
            parent_lease: RefCell::new(Some(parent_lease)),
            staging,
            request,
            exited: false,
            consumed: false,
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
        ensure!(!self.consumed, "export worker already consumed");
        if let Some(parent_lease) = self.parent_lease.borrow().as_ref() {
            check_parent_lease(&self.staging, parent_lease)?;
        }
        if !self.exited {
            if canceled.load(Ordering::Acquire) {
                self.stop()?;
                self.consumed = true;
                bail!("export worker canceled");
            }
            let Some(status) = self.child.try_wait()? else {
                return Ok(None);
            };
            self.exited = true;
            self.lease.take();
            if !status.success() {
                self.consumed = true;
                let detail = read(&self.staging.join("error.json"), RECEIPT_LIMIT)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<String>(&b).ok())
                    .unwrap_or_else(|| format!("export worker failed ({status})"));
                bail!("{detail}");
            }
        }
        self.consumed = true;
        let result =
            wire::decode_completion(&read(&self.staging.join("result.json"), RECEIPT_LIMIT)?)?;
        let work = &self.request.work;
        let result = match result {
            wire::DecodedCompletion::Legacy(result) => {
                validate_completed(work, &self.staging, &result)?;
                // Old children sealed before exit. Preserve their strict parent
                // readback so an existing version-1/2 receipt remains usable.
                ensure!(
                    read_seal_checked(work, canceled)? == result.sealed,
                    "export worker seal changed"
                );
                result
            }
            wire::DecodedCompletion::Rendering(facts) => {
                validate_rendering(work, &self.staging, &facts)?;
                complete_rendering(work, facts, canceled)?
            }
        };
        Ok(Some(result))
    }
    pub fn stop(&mut self) -> Result<()> {
        // Stop permanently revokes result consumption, including when wait must retry.
        self.consumed = true;
        if !self.exited {
            self.lease.take();
            if self.child.try_wait()?.is_none() {
                // Kill can race natural exit; wait remains authoritative. A
                // kill error must not release native ownership without reaping.
                let _ = self.child.kill();
            }
            self.child.wait()?;
            self.exited = true;
        }
        Ok(())
    }
    #[cfg(test)]
    fn wait_for_rendering_exit_for_test(&mut self) -> Result<()> {
        ensure!(!self.exited, "export worker already exited");
        let status = self.child.wait()?;
        self.exited = true;
        self.lease.take();
        if !status.success() {
            let detail = read(&self.staging.join("error.json"), RECEIPT_LIMIT)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<String>(&bytes).ok())
                .unwrap_or_else(|| "no bounded worker error receipt".into());
            bail!("export worker failed ({status}): {detail}");
        }
        Ok(())
    }
    /// Only this reaped worker's disposable transport files are removed. Durable
    /// publication/restore evidence resides in its separately sealed directory.
    pub fn retire_transport(&self) -> Result<()> {
        ensure!(self.exited, "cannot retire a live export worker");
        if let Some(parent_lease) = self.parent_lease.borrow_mut().take() {
            check_parent_lease(&self.staging, &parent_lease)?;
            FileExt::unlock(&parent_lease)?;
            drop(parent_lease);
        }
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

fn cancellation_checkpoint(canceled: &AtomicBool, operation: &str) -> std::io::Result<()> {
    if canceled.load(Ordering::Acquire) {
        Err(std::io::Error::other(format!(
            "export cancellation requested during {operation}"
        )))
    } else {
        Ok(())
    }
}

fn verified_checkpoint(
    output: &VerifiedFile,
    canceled: &AtomicBool,
    operation: &str,
) -> std::io::Result<()> {
    let rechecked = output
        .recheck()
        .map_err(|error| std::io::Error::other(format!("{error:#}")));
    let cancellation = cancellation_checkpoint(canceled, operation);
    rechecked?;
    cancellation
}

fn validate_rendered(work: &ExportWork, staging: &Path, rendered: &StagedPhoto) -> Result<()> {
    ensure!(
        rendered.renderer_identity == work.plan.renderer_identity
            && rendered.staging == staging.join("output")
            && rendered.encoding.source_fingerprint == work.plan.original_revision.digest
            && rendered.encoding.recipe_digest == work.plan.identity.recipe_digest
            && rendered.encoding.encoded_extent > 0
            && rendered.encoding.encoded_extent <= work.plan.max_payload_bytes,
        "export worker result binding mismatch"
    );
    Ok(())
}

fn validate_rendering(
    work: &ExportWork,
    staging: &Path,
    facts: &ExportRenderingFacts,
) -> Result<()> {
    let output_matches = facts.output.size == work.plan.output.size
        && facts.output.format == work.plan.output.format
        && facts.output.alpha == work.plan.output.alpha
        && match (&facts.output.profile, &work.plan.output.profile) {
            (StoredProfile::Srgb, StoredProfile::Srgb)
            | (StoredProfile::LinearSrgb, StoredProfile::LinearSrgb) => true,
            (StoredProfile::Icc { blob: actual }, StoredProfile::Icc { blob: expected }) => {
                actual == expected
            }
            _ => false,
        };
    ensure!(
        facts.job == work.job
            && facts.sequence == work.sequence
            && facts.authority == work.authority
            && facts.attempt == work.attempt
            && output_matches
            && facts.output_revision.bytes == facts.rendered.encoding.encoded_extent,
        "export worker result binding mismatch"
    );
    validate_rendered(work, staging, &facts.rendered)
}

fn validate_completed(work: &ExportWork, staging: &Path, result: &CompletedExport) -> Result<()> {
    ensure!(
        result.authority == work.authority
            && result.attempt == work.attempt
            && result.sealed.authority_digest == work.authority
            && result.sealed.snapshot == work.plan.destination
            && result.sealed.max_payload_bytes == work.plan.max_payload_bytes
            && result.rendered.encoding.encoded_extent == result.sealed.payload.bytes,
        "export worker result binding mismatch"
    );
    validate_rendered(work, staging, &result.rendered)
}

fn read_seal_checked(work: &ExportWork, canceled: &AtomicBool) -> Result<SealedPhotoExport> {
    metadata_export::read_photo_seal_with_checkpoint(
        &work.plan.destination,
        &work.authority,
        &mut |_| cancellation_checkpoint(canceled, "seal verification"),
    )
}

fn complete_rendering(
    work: &ExportWork,
    facts: ExportRenderingFacts,
    canceled: &AtomicBool,
) -> Result<CompletedExport> {
    let started = std::time::Instant::now();
    let verified = VerifiedFile::read_with_checkpoint(
        &facts.rendered.staging,
        work.plan.max_payload_bytes,
        &mut |_| cancellation_checkpoint(canceled, "fresh-output verification"),
    )?;
    ensure!(
        verified.revision() == &facts.output_revision,
        "native output changed before parent sealing"
    );
    let prior_directory = work
        .plan
        .destination
        .destination
        .parent()
        .context("export destination parent")?
        .join(format!(
            ".photocatalog-photo-export-{}",
            work.plan.destination.operation
        ));
    let sealed = if prior_directory.try_exists()? {
        // A fenced retry independently rendered again. Reuse is allowed only
        // after strict verification of both durable and fresh complete bytes.
        let prior = metadata_export::read_photo_seal_with_checkpoint(
            &work.plan.destination,
            &work.authority,
            &mut |_| verified_checkpoint(&verified, canceled, "seal verification"),
        )?;
        ensure!(
            facts.output_revision.bytes == prior.payload.bytes
                && facts.output_revision.digest == prior.payload.digest,
            "orphan seal differs from fresh render; prepare a new export plan"
        );
        prior
    } else {
        metadata_export::seal_photo_export(
            &work.plan.destination,
            &facts.rendered.staging,
            work.plan.max_payload_bytes,
            &work.authority,
            |_| verified_checkpoint(&verified, canceled, "seal creation"),
        )?
    };
    ensure!(
        sealed.payload.bytes == facts.output_revision.bytes
            && sealed.payload.digest == facts.output_revision.digest,
        "export worker result binding mismatch"
    );
    // Always reread the installed durable evidence before exposing the same
    // CompletedExport value that standalone callers already consume.
    ensure!(
        metadata_export::read_photo_seal_with_checkpoint(
            &work.plan.destination,
            &work.authority,
            &mut |_| verified_checkpoint(&verified, canceled, "seal verification"),
        )? == sealed,
        "export worker seal changed"
    );
    Ok(CompletedExport {
        authority: facts.authority,
        attempt: facts.attempt,
        sealed,
        rendered: facts.rendered,
        // This interval now explicitly belongs to the local parent and includes
        // fresh/orphan comparison or creation plus strict durable readback.
        seal_ms: started.elapsed().as_secs_f64() * 1000.,
        peak_resident_bytes: facts.peak_resident_bytes,
        peak_method: facts.peak_method,
    })
}
impl Drop for ExportWorkerProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

const RETIRED_PREFIX: &str = "photo-retired-";
const TRANSPORT_FILES: &[&str] = &[
    "parent.lock",
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
    #[serde(with = "crate::metadata_export::wire::native_path")]
    pub staging: PathBuf,
    pub work: ExportWork,
}
#[derive(Debug, Serialize)]
pub struct RetainedExportTransport {
    #[serde(with = "crate::metadata_export::wire::native_path")]
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
enum ParentLeaseInspection {
    Absent,
    Acquired(AcquiredLease),
    Busy,
}
fn acquire_parent_lease(path: &Path) -> Result<ParentLeaseInspection> {
    let target = path.join("parent.lock");
    match fs::symlink_metadata(&target) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file(),
            "invalid export parent lease type"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ParentLeaseInspection::Absent);
        }
        Err(error) => return Err(error.into()),
    }
    let file = metadata_export::open_regular(&target)?;
    if let Err(error) = file.try_lock_exclusive() {
        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
            return Ok(ParentLeaseInspection::Busy);
        }
        return Err(error.into());
    }
    check_parent_lease(path, &file)?;
    Ok(ParentLeaseInspection::Acquired(AcquiredLease(file)))
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
fn check_parent_lease(path: &Path, lock: &File) -> Result<()> {
    ensure!(
        lock.metadata()?.len() == 0,
        "invalid export parent lease marker"
    );
    ensure!(
        fs::symlink_metadata(path.join("parent.lock"))?
            .file_type()
            .is_file(),
        "export parent lease type changed"
    );
    ensure!(
        lease_identity(lock)?
            == lease_identity(&metadata_export::open_regular(&path.join("parent.lock"))?)?,
        "export parent lease identity changed"
    );
    Ok(())
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
    let parent = match acquire_parent_lease(path)? {
        ParentLeaseInspection::Absent => None,
        ParentLeaseInspection::Acquired(parent) => Some(parent),
        ParentLeaseInspection::Busy => {
            return Ok(Inspection::Retained(
                "parent lease is busy; stage remains owned".into(),
            ));
        }
    };
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
            drop(parent);
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
    drop(parent);
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
    recover_export_transports_with_checkpoint(root, max_directories, &mut || Ok(()))
}
pub fn recover_export_transports_with_checkpoint(
    root: &Path,
    max_directories: usize,
    checkpoint: &mut dyn FnMut() -> Result<()>,
) -> Result<ExportTransportRecovery> {
    checkpoint()?;
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
        checkpoint()?;
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
    let parent = match acquire_parent_lease(&retired.staging)? {
        ParentLeaseInspection::Absent => None,
        ParentLeaseInspection::Acquired(parent) => Some(parent),
        ParentLeaseInspection::Busy => bail!("retired export parent lease busy"),
    };
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
    drop(parent);
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
        admit_output_path(&request.work, &current.join("output"))?;
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
        let native_output = VerifiedFile::read_with_checkpoint(
            &rendered.staging,
            plan.max_payload_bytes,
            &mut |_| Ok(()),
        )?;
        let output_revision = native_output.revision().clone();
        native_output.recheck()?;
        let output = plan.output.clone();
        let (peak_resident_bytes, peak_method) = crate::preview::peak_resident_memory();
        let receipt = ExportRenderingFacts {
            job: request.work.job,
            sequence: request.work.sequence,
            authority: request.work.authority,
            attempt: request.work.attempt,
            output,
            output_revision,
            rendered,
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
