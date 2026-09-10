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
    io::{Read, Write},
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
}
fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= limit,
        "worker artifact size/type"
    );
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
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
    ensure!(
        request.version == 1 && request.work.plan.version == 1,
        "export worker protocol"
    );
    let encoded = serde_json::to_vec(&request.work.plan)?;
    ensure!(
        blake3::hash(&encoded).to_hex().as_str() == request.work.authority,
        "export worker plan binding"
    );
    ensure!(
        request.work.plan.renderer_identity == photo_render::output_renderer_identity(),
        "export renderer changed; prepare a new plan"
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
        let staging = tempfile::Builder::new()
            .prefix("photo-worker-")
            .tempdir_in(staging_root)?
            .keep();
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
        let mut lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.staging.join("active.lock"))?;
        lock.try_lock_exclusive()
            .context("export worker still owns staging")?;
        let allowed = [
            "active.lock",
            "request.json",
            "profile.icc",
            "selected.xmp",
            "output",
            "result.json",
            "error.json",
        ];
        let mut paths = Vec::new();
        for entry in fs::read_dir(&self.staging)? {
            let entry = entry?;
            ensure!(
                paths.len() < allowed.len()
                    && allowed.iter().any(|name| entry.file_name() == *name),
                "unrecognized export worker artifact; retained"
            );
            ensure!(
                entry.file_type()?.is_file(),
                "non-file export worker artifact; retained"
            );
            paths.push(entry.path());
        }
        // A pre-opened child handle must observe retirement after acquiring its
        // lock, including on Windows where handles outlive pathname removal.
        lock.write_all(&[1])?;
        lock.sync_all()?;
        drop(lock);
        paths.sort_by_key(|path| path.file_name().is_some_and(|name| name == "active.lock"));
        for path in paths {
            fs::remove_file(path)?;
        }
        fs::remove_dir(&self.staging)?;
        Ok(())
    }
}
impl Drop for ExportWorkerProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
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
        .create_new(true)
        .open(current.join("active.lock"))?;
    lock.try_lock_exclusive()?;
    ensure!(
        lock.metadata()?.len() == 0,
        "export worker staging was retired"
    );
    ensure!(
        crate::storage_volume::object_key(&current.join("active.lock"), &lock.metadata()?)?
            == crate::storage_volume::object_key(
                &current.join("active.lock"),
                &fs::metadata(current.join("active.lock"))?
            )?,
        "export worker staging lock identity changed"
    );
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
        let receipt = CompletedExport {
            authority: request.work.authority,
            attempt: request.work.attempt,
            sealed,
            rendered,
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
