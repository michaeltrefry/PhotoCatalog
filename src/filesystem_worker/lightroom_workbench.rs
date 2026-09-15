//! Filesystem-owned Workbench resources. F retains every opened object and
//! capture child by exact W generation/operation until an explicit release.
use super::wire::{LightroomWorkbenchIo, LightroomWorkbenchIoReply};
use crate::{
    application::U64,
    lightroom::{
        capture::{CaptureProcess, Manifest},
        source::Source,
    },
    lightroom_migration_worker::{identity::FileKey, source_reader::CaptureSqlAuthority},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

struct Root {
    operation: String,
    workbench: String,
    generation: String,
    path: NativePath,
    database: Source,
}
struct Capture {
    operation: String,
    workbench: String,
    generation: String,
    child: Option<CaptureProcess>,
    result: Option<Manifest>,
    pid: U64,
    staging: NativePath,
}
struct Evidence {
    operation: String,
    workbench: String,
    generation: String,
    capture_generation: String,
    directory: NativePath,
    manifest: Manifest,
    manifest_blake3: String,
    manifest_source: Source,
    logical: Source,
    raw: Vec<Source>,
    authority: CaptureSqlAuthority,
}
#[derive(Default)]
pub(super) struct Owner {
    root: Option<Root>,
    capture: Option<Capture>,
    evidence: Option<Evidence>,
    released_root: Option<ReleaseReceipt>,
    released_capture: Option<ReleaseReceipt>,
    released_evidence: Option<ReleaseReceipt>,
}

#[derive(Clone, PartialEq, Eq)]
struct ReleaseReceipt {
    operation: String,
    workbench: String,
    generation: String,
    resource_generation: Option<String>,
}
impl ReleaseReceipt {
    fn new(
        operation: &str,
        workbench: &str,
        generation: &str,
        resource_generation: Option<&str>,
    ) -> Self {
        Self {
            operation: operation.into(),
            workbench: workbench.into(),
            generation: generation.into(),
            resource_generation: resource_generation.map(Into::into),
        }
    }
    fn reply(&self) -> LightroomWorkbenchIoReply {
        LightroomWorkbenchIoReply::Released {
            operation: self.operation.clone(),
        }
    }
}

fn id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 128 && value.is_ascii(),
        "Workbench F identity"
    );
    Ok(())
}
fn canceled(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "Workbench filesystem operation canceled"
    );
    Ok(())
}
fn same(expected: (&str, &str, &str), actual: (&str, &str, &str)) -> Result<()> {
    ensure!(
        expected == actual,
        "Workbench filesystem operation identity differs"
    );
    Ok(())
}
fn manifest(path: &Path, cancel: &AtomicBool) -> Result<(Manifest, String, Source)> {
    canceled(cancel)?;
    let mut source = Source::open(path, crate::lightroom::MANIFEST_BYTES as u64)?;
    let mut bytes = vec![0; usize::try_from(source.before.bytes)?];
    source.copy_and_hash_controlled(None, || canceled(cancel))?;
    use std::io::{Read, Seek, SeekFrom};
    source.file.seek(SeekFrom::Start(0))?;
    source.file.read_exact(&mut bytes)?;
    let mut extra = [0];
    ensure!(source.file.read(&mut extra)? == 0, "capture manifest grew");
    source.verify()?;
    let digest = crate::lightroom::digest(&bytes);
    let value: Manifest = serde_json::from_slice(&bytes)?;
    Ok((value, digest, source))
}
fn companion_free(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        match fs::symlink_metadata(Path::new(&value)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => anyhow::bail!("captured logical database has a companion"),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

impl Owner {
    pub(super) fn execute(
        &mut self,
        request: LightroomWorkbenchIo,
        cancel: &AtomicBool,
    ) -> Result<LightroomWorkbenchIoReply> {
        match request {
            LightroomWorkbenchIo::RootBegin {
                operation,
                workbench,
                generation,
                root,
                create,
            } => {
                for v in [&operation, &workbench, &generation] {
                    id(v)?
                }
                canceled(cancel)?;
                if let Some(active) = &self.root {
                    same(
                        (&active.operation, &active.workbench, &active.generation),
                        (&operation, &workbench, &generation),
                    )?;
                    return Ok(active.reply());
                }
                self.released_root = None;
                crate::catalog_session::validate_path(&root)?;
                let requested = root.to_path()?;
                if create {
                    ensure!(!requested.exists(), "inspection output must be new");
                    fs::create_dir(&requested)?;
                    let path = requested.join("inspection.sqlite3");
                    fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .open(&path)?
                        .sync_all()?;
                }
                let canonical = fs::canonicalize(&requested)?;
                ensure!(
                    fs::symlink_metadata(&canonical)?.is_dir(),
                    "inspection root is not a direct directory"
                );
                let database = Source::open(&canonical.join("inspection.sqlite3"), u64::MAX)?;
                let value = Root {
                    operation,
                    workbench,
                    generation,
                    path: NativePath::from_path(&canonical),
                    database,
                };
                let reply = value.reply();
                self.root = Some(value);
                Ok(reply)
            }
            LightroomWorkbenchIo::RootCurrent {
                operation,
                workbench,
                generation,
            } => {
                let value = self.root.as_mut().context("no Workbench root retained")?;
                same(
                    (&value.operation, &value.workbench, &value.generation),
                    (&operation, &workbench, &generation),
                )?;
                value.database =
                    Source::open(&value.path.to_path()?.join("inspection.sqlite3"), u64::MAX)?;
                Ok(value.reply())
            }
            LightroomWorkbenchIo::RootRelease {
                operation,
                workbench,
                generation,
            } => {
                let receipt = ReleaseReceipt::new(&operation, &workbench, &generation, None);
                if self.released_root.as_ref() == Some(&receipt) {
                    return Ok(receipt.reply());
                }
                let value = self.root.as_mut().context("no Workbench root retained")?;
                same(
                    (&value.operation, &value.workbench, &value.generation),
                    (&operation, &workbench, &generation),
                )?;
                value.database =
                    Source::open(&value.path.to_path()?.join("inspection.sqlite3"), u64::MAX)?;
                self.root.take();
                self.released_root = Some(receipt.clone());
                Ok(receipt.reply())
            }
            LightroomWorkbenchIo::CaptureStart {
                operation,
                workbench,
                generation,
                executable,
                staging,
                request,
            } => {
                for v in [&operation, &workbench, &generation] {
                    id(v)?
                }
                ensure!(self.capture.is_none(), "capture already retained");
                self.released_capture = None;
                canceled(cancel)?;
                let child =
                    CaptureProcess::spawn(&executable.to_path()?, &staging.to_path()?, &request)?;
                let pid = U64(child.pid() as u64);
                let staging = NativePath::from_path(child.staging_directory());
                self.capture = Some(Capture {
                    operation: operation.clone(),
                    workbench,
                    generation,
                    child: Some(child),
                    result: None,
                    pid,
                    staging: staging.clone(),
                });
                Ok(LightroomWorkbenchIoReply::CaptureRunning {
                    operation,
                    pid,
                    staging,
                })
            }
            LightroomWorkbenchIo::CapturePoll {
                operation,
                workbench,
                generation,
            } => {
                let value = self.capture.as_mut().context("no capture retained")?;
                same(
                    (&value.operation, &value.workbench, &value.generation),
                    (&operation, &workbench, &generation),
                )?;
                if let Some(result) = &value.result {
                    return Ok(LightroomWorkbenchIoReply::CaptureComplete {
                        operation,
                        manifest: result.clone(),
                    });
                }
                let polled = value
                    .child
                    .as_mut()
                    .context("capture child missing")?
                    .poll()?;
                Ok(match polled {
                    Some(v) => {
                        value.result = Some(v.clone());
                        value.child.take();
                        LightroomWorkbenchIoReply::CaptureComplete {
                            operation,
                            manifest: v,
                        }
                    }
                    None => LightroomWorkbenchIoReply::CaptureRunning {
                        operation,
                        pid: value.pid,
                        staging: value.staging.clone(),
                    },
                })
            }
            LightroomWorkbenchIo::CaptureCancel {
                operation,
                workbench,
                generation,
            } => {
                let receipt = ReleaseReceipt::new(&operation, &workbench, &generation, None);
                if self.released_capture.as_ref() == Some(&receipt) {
                    return Ok(receipt.reply());
                }
                let mut value = self.capture.take().context("no capture retained")?;
                same(
                    (&value.operation, &value.workbench, &value.generation),
                    (&operation, &workbench, &generation),
                )?;
                if let Some(child) = &mut value.child {
                    child.cancel_and_wait()?;
                }
                self.released_capture = Some(receipt.clone());
                Ok(receipt.reply())
            }
            LightroomWorkbenchIo::CaptureRetire {
                operation,
                workbench,
                generation,
            } => {
                let receipt = ReleaseReceipt::new(&operation, &workbench, &generation, None);
                if self.released_capture.as_ref() == Some(&receipt) {
                    return Ok(receipt.reply());
                }
                let value = self.capture.as_ref().context("no capture retained")?;
                same(
                    (&value.operation, &value.workbench, &value.generation),
                    (&operation, &workbench, &generation),
                )?;
                ensure!(value.result.is_some(), "capture child is not terminal");
                self.capture.take();
                self.released_capture = Some(receipt.clone());
                Ok(receipt.reply())
            }
            LightroomWorkbenchIo::EvidenceBegin {
                operation,
                workbench,
                generation,
                capture_generation,
                directory,
                source_generation,
                protected,
                limits,
            } => {
                for v in [
                    &operation,
                    &workbench,
                    &generation,
                    &capture_generation,
                    &source_generation,
                ] {
                    id(v)?
                }
                ensure!(self.evidence.is_none(), "capture evidence already retained");
                self.released_evidence = None;
                canceled(cancel)?;
                crate::catalog_session::validate_path(&directory)?;
                let root = fs::canonicalize(directory.to_path()?)?;
                let (manifest, manifest_blake3, manifest_source) =
                    manifest(&root.join("manifest.json"), cancel)?;
                ensure!(
                    manifest.state == "captured"
                        && manifest.sqlite_consistency == "consistent_default_sqlite",
                    "capture is not a consistent snapshot"
                );
                let revision = manifest
                    .revision_id
                    .clone()
                    .context("capture revision missing")?;
                ensure!(
                    crate::lightroom::json_digest(&manifest.artifacts)? == revision,
                    "capture revision differs"
                );
                let logical_revision = manifest
                    .logical_revision
                    .clone()
                    .context("logical revision missing")?;
                let logical_blake3 = manifest
                    .logical_blake3
                    .clone()
                    .context("logical digest missing")?;
                let logical_path = root.join("logical.sqlite3");
                let mut logical =
                    Source::open(&logical_path, manifest.request.limits.max_total_bytes)?;
                ensure!(
                    logical.before == logical_revision,
                    "logical revision differs"
                );
                ensure!(
                    logical.copy_and_hash_controlled(None, || canceled(cancel))? == logical_blake3,
                    "logical digest differs"
                );
                companion_free(&logical_path)?;
                let mut raw = Vec::new();
                let mut roster = BTreeSet::new();
                for artifact in &manifest.artifacts {
                    canceled(cancel)?;
                    let relative = Path::new(&artifact.stored);
                    ensure!(
                        !relative.is_absolute()
                            && relative
                                .components()
                                .all(|c| matches!(c, std::path::Component::Normal(_)))
                            && relative.starts_with("raw"),
                        "unsafe capture artifact member"
                    );
                    ensure!(
                        roster.insert(artifact.stored.clone()),
                        "duplicate capture artifact member"
                    );
                    let mut member =
                        Source::open(&root.join(relative), manifest.request.limits.max_file_bytes)?;
                    ensure!(
                        member.before == artifact.revision
                            && member.copy_and_hash_controlled(None, || canceled(cancel))?
                                == artifact.blake3,
                        "capture artifact differs"
                    );
                    raw.push(member);
                }
                let raw_roster_blake3 = crate::lightroom::digest(&crate::lightroom::bounded_json(
                    &manifest.artifacts,
                    crate::lightroom::MANIFEST_BYTES,
                )?);
                let mut protected = protected;
                protected.sort();
                ensure!(
                    protected.windows(2).all(|w| w[0] != w[1]),
                    "duplicate protected identity"
                );
                let physical = FileKey::of(&logical.file)?;
                let companion_generation = uuid::Uuid::new_v4().to_string();
                let expires = SystemTime::now()
                    .duration_since(UNIX_EPOCH)?
                    .as_millis()
                    .checked_add(u128::from(limits.total_deadline_ms.0))
                    .context("CaptureSql expiry")?;
                let expires = U64(u64::try_from(expires)?);
                let mut authority = CaptureSqlAuthority {
                    protocol: 1,
                    build: crate::lightroom_migration_worker::worker::build_identity().into(),
                    workbench_instance: workbench.clone(),
                    workbench_generation: generation.clone(),
                    filesystem_lease: source_generation,
                    operation: operation.clone(),
                    capture_generation: capture_generation.clone(),
                    expires_unix_ms: expires,
                    capture_root: NativePath::from_path(&root),
                    member: crate::lightroom_migration_worker::source_reader::CAPTURE_SQL_MEMBER
                        .into(),
                    manifest_blake3: manifest_blake3.clone(),
                    revision_id: revision,
                    logical_revision: logical_revision.into(),
                    logical_blake3,
                    maximum_bytes: U64(manifest.request.limits.max_total_bytes),
                    physical,
                    companion_generation,
                    raw_roster_blake3,
                    limits,
                    protected,
                    binding_blake3: String::new(),
                };
                authority.binding_blake3 = authority.computed_binding()?;
                authority.validate()?;
                let value = Evidence {
                    operation,
                    workbench,
                    generation,
                    capture_generation,
                    directory: NativePath::from_path(&root),
                    manifest,
                    manifest_blake3,
                    manifest_source,
                    logical,
                    raw,
                    authority,
                };
                let reply = value.reply();
                self.evidence = Some(value);
                Ok(reply)
            }
            LightroomWorkbenchIo::EvidenceCurrent {
                operation,
                workbench,
                generation,
                capture_generation,
            } => {
                let value = self
                    .evidence
                    .as_mut()
                    .context("no capture evidence retained")?;
                same(
                    (&value.operation, &value.workbench, &value.generation),
                    (&operation, &workbench, &generation),
                )?;
                ensure!(
                    value.capture_generation == capture_generation,
                    "capture evidence generation differs"
                );
                value.verify(cancel)?;
                Ok(value.reply())
            }
            LightroomWorkbenchIo::EvidenceRelease {
                operation,
                workbench,
                generation,
                capture_generation,
            } => {
                let receipt = ReleaseReceipt::new(
                    &operation,
                    &workbench,
                    &generation,
                    Some(&capture_generation),
                );
                if self.released_evidence.as_ref() == Some(&receipt) {
                    return Ok(receipt.reply());
                }
                let value = self
                    .evidence
                    .as_mut()
                    .context("no capture evidence retained")?;
                same(
                    (&value.operation, &value.workbench, &value.generation),
                    (&operation, &workbench, &generation),
                )?;
                ensure!(
                    value.capture_generation == capture_generation,
                    "capture evidence generation differs"
                );
                value.verify(cancel)?;
                self.evidence.take();
                self.released_evidence = Some(receipt.clone());
                Ok(receipt.reply())
            }
        }
    }
}
impl Root {
    fn reply(&self) -> LightroomWorkbenchIoReply {
        LightroomWorkbenchIoReply::Root {
            operation: self.operation.clone(),
            root: self.path.clone(),
            database_revision: self.database.before.clone().into(),
            physical: FileKey::of(&self.database.file).expect("retained root identity"),
        }
    }
}
impl Evidence {
    fn verify(&mut self, cancel: &AtomicBool) -> Result<()> {
        canceled(cancel)?;
        self.manifest_source.verify()?;
        self.logical.verify()?;
        companion_free(&self.logical.path)?;
        for source in &self.raw {
            source.verify()?
        }
        Ok(())
    }
    fn reply(&self) -> LightroomWorkbenchIoReply {
        LightroomWorkbenchIoReply::Evidence {
            operation: self.operation.clone(),
            capture_generation: self.capture_generation.clone(),
            directory: self.directory.clone(),
            manifest: self.manifest.clone(),
            manifest_blake3: self.manifest_blake3.clone(),
            authority: self.authority.clone(),
        }
    }
}
