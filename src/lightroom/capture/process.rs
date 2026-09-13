//! Owned capture process. The parent opens only private transport files, never
//! source inodes: closing a second original FD would release POSIX byte locks.
use super::{Manifest, Request};
use crate::lightroom::{MANIFEST_BYTES, bounded_json, source::Source};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
};

const RESULT_BYTES: usize = MANIFEST_BYTES + 1024;
const ERROR_BYTES: usize = 32 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    request_digest: String,
    manifest: Manifest,
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create capture transport {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    // Reuse no-follow regular-file opening and descriptor/path identity checks
    // for transport files only. There are no byte locks on these handles.
    let mut transport = Source::open(path, maximum as u64)?;
    transport.verify()?;
    let mut bytes = vec![0; usize::try_from(transport.before.bytes)?];
    transport.file.read_exact(&mut bytes)?;
    let mut extra = [0];
    ensure!(
        transport.file.read(&mut extra)? == 0,
        "capture transport grew"
    );
    transport.verify()?;
    Ok(bytes)
}

/// One dedicated child and its stdin ownership lease. Spawn writes only bounded
/// transport files in `staging_root`. Source/catalog/companion FDs belong solely
/// to the child. Use on an owned coordinator thread: process launch, bounded
/// receipt I/O and kill/wait can block on the OS, and do not belong on a UI actor.
///
/// Captures and transport diagnostics are retained, including partial failures.
/// This object never removes capture evidence and never resumes a saved request.
pub struct CaptureProcess {
    child: Child,
    lease: Option<ChildStdin>,
    staging: PathBuf,
    request_digest: String,
    exited: bool,
}

impl CaptureProcess {
    pub fn spawn(executable: &Path, staging_root: &Path, request: &Request) -> Result<Self> {
        ensure!(
            executable.is_absolute() && staging_root.is_absolute(),
            "absolute capture executable/staging paths required"
        );
        let mut command = Command::new(executable);
        command.arg("--lightroom-capture-worker");
        Self::spawn_command(command, staging_root, request)
    }

    fn spawn_command(mut command: Command, staging_root: &Path, request: &Request) -> Result<Self> {
        request.limits.validate()?;
        // NativePath conversion is lexical only. Never canonicalize or stat a
        // source from the parent, even as an admission convenience.
        ensure!(
            request.source.to_path()?.is_absolute() && request.output.to_path()?.is_absolute(),
            "absolute capture source/output paths required"
        );
        let bytes = bounded_json(request, MANIFEST_BYTES)?;
        let request_digest = blake3::hash(&bytes).to_hex().to_string();
        let staging_root = staging_root.canonicalize()?;
        let staging = tempfile::Builder::new()
            .prefix("lightroom-capture-")
            .tempdir_in(staging_root)?
            .keep();
        write_new(&staging.join("request.json"), &bytes)?;
        write_new(&staging.join("active.lock"), b"")?;
        let mut child = command
            .current_dir(&staging)
            .stdin(Stdio::piped())
            // No pipe buffers/drainer threads: errors use a bounded retained
            // artifact. Child or inherited stdout/stderr cannot deadlock wait.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| {
                format!(
                    "start isolated Lightroom capture; retained transport {}",
                    staging.display()
                )
            })?;
        let lease = child.stdin.take();
        let mut owner = Self {
            child,
            lease,
            staging,
            request_digest,
            exited: false,
        };
        let lease = owner
            .lease
            .as_mut()
            .context("capture stdin ownership lease")?;
        // The fixed 65-byte admission fits the empty pipe. Bind the request
        // before any child source I/O, not merely when accepting its result.
        // Ownership exists before admission; any
        // subsequent error drops the owner and kills/reaps the child.
        lease.write_all(b"!")?;
        lease.write_all(owner.request_digest.as_bytes())?;
        lease.flush()?;
        Ok(owner)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
    pub fn staging_directory(&self) -> &Path {
        &self.staging
    }

    /// None means still owned/running. A terminal Manifest can itself describe
    /// a failed capture, exactly as the CLI does. This consumes the child result.
    pub fn poll(&mut self) -> Result<Option<Manifest>> {
        ensure!(!self.exited, "capture process already consumed");
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        self.exited = true;
        self.lease.take();
        if !status.success() {
            let error = read_bounded(&self.staging.join("error.json"), ERROR_BYTES)
                .ok()
                .and_then(|b| serde_json::from_slice::<String>(&b).ok())
                .unwrap_or_else(|| {
                    format!(
                        "capture worker failed ({status}); retained transport: {}",
                        self.staging.display()
                    )
                });
            bail!("{error}");
        }
        let result: Receipt = serde_json::from_slice(&read_bounded(
            &self.staging.join("result.json"),
            RESULT_BYTES,
        )?)?;
        ensure!(
            result.request_digest == self.request_digest
                && result.manifest.protocol == crate::lightroom::PROTOCOL,
            "capture result request/protocol mismatch"
        );
        Ok(Some(result.manifest))
    }

    /// Revokes ownership, kills and reaps before returning. Existing committed
    /// capture manifests are not invalidated: cancellation may race publication;
    /// explicitly inspect retained artifacts before deciding whether to recapture.
    pub fn cancel_and_wait(&mut self) -> Result<()> {
        self.stop("owner_cancel")
    }

    fn stop(&mut self, reason: &str) -> Result<()> {
        if !self.exited {
            self.lease.take();
            let status = match self.child.try_wait()? {
                Some(status) => status,
                None => {
                    // An already-exiting child can reject kill. Wait remains
                    // authoritative; never abandon ownership on a kill error.
                    let _ = self.child.kill();
                    self.child.wait()?
                }
            };
            self.exited = true;
            write_new(
                &self.staging.join("termination.json"),
                &bounded_json(
                    &serde_json::json!({"reason":reason,"status":status.to_string()}),
                    ERROR_BYTES,
                )?,
            )?;
        }
        Ok(())
    }
}

impl Drop for CaptureProcess {
    fn drop(&mut self) {
        let _ = self.stop("owner_drop");
    }
}

fn error_text(error: &anyhow::Error) -> String {
    struct Text(String);
    impl std::fmt::Write for Text {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            for c in value.chars() {
                if self.0.len() + c.len_utf8() > 4096 {
                    break;
                }
                self.0.push(c);
            }
            Ok(())
        }
    }
    let mut out = Text(String::new());
    let _ = std::fmt::write(&mut out, format_args!("{error:#}"));
    out.0
}

/// Installed hidden dispatch; call before Tauri/SQLite initialization. The only
/// extra thread watches the ownership lease and terminates this whole dedicated
/// process on EOF/error. Capture never starts descendant processes.
pub fn capture_worker_main() -> Result<()> {
    worker_main_with(super::run_isolated)
}

fn worker_main_with(run: impl FnOnce(Request) -> Result<Manifest>) -> Result<()> {
    let root = std::env::current_dir()?;
    ensure!(
        root.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("lightroom-capture-")),
        "capture worker requires its private transport directory"
    );
    let lock = Source::open(&root.join("active.lock"), 0)?;
    lock.verify()?;
    lock.file.try_lock_exclusive()?;
    let result = (|| -> Result<()> {
        let mut input = std::io::stdin();
        let mut token = [0];
        input.read_exact(&mut token)?;
        ensure!(&token == b"!", "capture owner admission missing");
        let mut expected_digest = [0; 64];
        input.read_exact(&mut expected_digest)?;
        std::thread::Builder::new()
            .name("lightroom-capture-owner".into())
            .spawn(move || {
                let _ = input.read(&mut token);
                std::process::exit(74);
            })?;
        let bytes = read_bounded(&root.join("request.json"), MANIFEST_BYTES)?;
        let request_digest = blake3::hash(&bytes).to_hex().to_string();
        ensure!(
            request_digest.as_bytes() == expected_digest,
            "capture request changed before worker admission"
        );
        let request: Request = serde_json::from_slice(&bytes)?;
        let manifest = run(request)?;
        let receipt = Receipt {
            request_digest,
            manifest,
        };
        write_new(
            &root.join("result.json"),
            &bounded_json(&receipt, RESULT_BYTES)?,
        )?;
        Ok(())
    })();
    if let Err(error) = &result {
        let _ = write_new(
            &root.join("error.json"),
            &bounded_json(&error_text(error), ERROR_BYTES)?,
        );
    }
    // Hold the transport lock through result/error flush and all source cleanup.
    drop(lock);
    result
}

#[cfg(test)]
mod tests;
