//! Filesystem-owned Workbench resources. F retains every opened object and
//! capture child by exact W generation/operation until an explicit release.
use super::wire::{
    LightroomWorkbenchIo, LightroomWorkbenchIoReply, LightroomWorkbenchSealDocument,
    LightroomWorkbenchSealState,
};
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
    io::Write,
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
struct Original {
    operation: String,
    workbench: String,
    generation: String,
    candidate: crate::lightroom::plan::OriginalCandidate,
    maximum_result_bytes: usize,
    encoded: Vec<u8>,
    blake3: String,
}
struct Upload {
    file: Option<fs::File>,
    expected_bytes: u64,
    expected_blake3: String,
    written: u64,
    hasher: blake3::Hasher,
}
struct Seal {
    operation: String,
    workbench: String,
    generation: String,
    token: String,
    directory: NativePath,
    database: NativePath,
    approval_path: NativePath,
    seal_path: NativePath,
    approval: Upload,
    review: Upload,
    seal: Option<Upload>,
    database_physical: Option<FileKey>,
    database_identity: Option<crate::lightroom::source::Revision>,
    database_blake3: Option<String>,
    published_blake3: Option<String>,
    state: LightroomWorkbenchSealState,
}
#[derive(Default)]
pub(super) struct Owner {
    root: Option<Root>,
    capture: Option<Capture>,
    evidence: Option<Evidence>,
    original: Option<Original>,
    seal: Option<Seal>,
    released_root: Option<ReleaseReceipt>,
    released_capture: Option<ReleaseReceipt>,
    released_evidence: Option<ReleaseReceipt>,
    released_original: Option<ReleaseReceipt>,
}

impl Upload {
    fn create(path: &Path, expected_bytes: u64, expected_blake3: String) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self {
            file: Some(file),
            expected_bytes,
            expected_blake3,
            written: 0,
            hasher: blake3::Hasher::new(),
        })
    }
    fn append(&mut self, offset: u64, bytes: &[u8]) -> Result<u64> {
        ensure!(offset == self.written, "seal upload offset differs");
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .filter(|v| *v <= self.expected_bytes)
            .context("seal upload exceeds admitted length")?;
        self.file
            .as_mut()
            .context("seal upload is already closed")?
            .write_all(bytes)?;
        self.hasher.update(bytes);
        self.written = next;
        Ok(next)
    }
    fn finish(&mut self) -> Result<()> {
        ensure!(
            self.written == self.expected_bytes
                && self.hasher.finalize().to_hex().as_str() == self.expected_blake3,
            "seal uploaded document differs"
        );
        self.file
            .take()
            .context("seal upload is already closed")?
            .sync_all()?;
        Ok(())
    }
}

impl Seal {
    fn same(&self, operation: &str, workbench: &str, generation: &str, token: &str) -> Result<()> {
        same(
            (&self.operation, &self.workbench, &self.generation),
            (operation, workbench, generation),
        )?;
        ensure!(self.token == token, "seal token differs");
        Ok(())
    }
    fn upload(&mut self, document: LightroomWorkbenchSealDocument) -> Result<&mut Upload> {
        match document {
            LightroomWorkbenchSealDocument::Approval => Ok(&mut self.approval),
            LightroomWorkbenchSealDocument::Review => Ok(&mut self.review),
            LightroomWorkbenchSealDocument::Seal => self
                .seal
                .as_mut()
                .context("seal publication upload not begun"),
        }
    }
    fn reply(&self) -> LightroomWorkbenchIoReply {
        LightroomWorkbenchIoReply::SealState {
            operation: self.operation.clone(),
            token: self.token.clone(),
            state: self.state,
            directory: self.directory.clone(),
            seal_path: self.seal_path.clone(),
            approval_path: self.approval_path.clone(),
            seal_blake3: self.published_blake3.clone(),
        }
    }
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
fn bounded_json(value: &impl serde::Serialize, maximum: usize) -> Result<Vec<u8>> {
    struct Counter {
        bytes: usize,
        maximum: usize,
    }
    impl Write for Counter {
        fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(value.len())
                .filter(|size| *size <= self.maximum)
                .ok_or_else(|| std::io::Error::other("original result byte limit"))?;
            Ok(value.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, maximum };
    serde_json::to_writer(&mut counter, value)?;
    let mut encoded = Vec::new();
    encoded.try_reserve_exact(counter.bytes)?;
    serde_json::to_writer(&mut encoded, value)?;
    ensure!(encoded.len() == counter.bytes, "original encoding changed");
    Ok(encoded)
}

fn inspect_original(
    candidate: &crate::lightroom::plan::OriginalCandidate,
    cancel: &AtomicBool,
) -> Result<crate::lightroom::plan::OriginalObservation> {
    canceled(cancel)?;
    let path = candidate.path.to_path()?;
    let (base_state, metadata) = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ("missing", serde_json::json!({"missing":true}))
        }
        Err(error) => (
            "unavailable",
            serde_json::json!({"error":error.to_string()}),
        ),
        Ok(value) if !value.is_file() || value.file_type().is_symlink() => {
            ("non_regular", serde_json::json!({"regular":false}))
        }
        Ok(value) => ("available", serde_json::json!({"bytes":value.len()})),
    };
    let mut files = Vec::new();
    let mut gaps = false;
    if candidate.packets {
        let limits = crate::xmp_packets::Limits {
            max_source_bytes: candidate.limits.max_file_bytes,
            max_retained_bytes: candidate.limits.max_cell_bytes,
            max_parse_bytes: candidate.limits.max_cell_bytes,
            ..Default::default()
        };
        let mut paths = vec![("embedded".to_owned(), path.clone())];
        for suffix in ["xmp", "XMP"] {
            paths.push((format!("sidecar_{suffix}"), path.with_extension(suffix)));
            let mut appended = path.as_os_str().to_os_string();
            appended.push(format!(".{suffix}"));
            paths.push((
                format!("sidecar_appended_{suffix}"),
                Path::new(&appended).to_path_buf(),
            ));
        }
        for (origin, path) in paths {
            canceled(cancel)?;
            let result = match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    files.push(crate::lightroom::plan::OriginalFileObservation {
                        origin,
                        path: NativePath::from_path(&path),
                        state: "absent".into(),
                        error: None,
                        inspection: None,
                    });
                    continue;
                }
                Err(error) => Err(error),
                Ok(_) if origin == "embedded" => {
                    crate::xmp_packets::inspect_cancellable(&path, &limits, false, cancel)
                }
                Ok(_) => crate::xmp_packets::inspect_cancellable(&path, &limits, true, cancel),
            };
            match result {
                Ok(value) => {
                    gaps |= !matches!(
                        value.status,
                        crate::xmp_packets::Status::Complete | crate::xmp_packets::Status::Absent
                    );
                    files.push(crate::lightroom::plan::OriginalFileObservation {
                        origin,
                        path: NativePath::from_path(&path),
                        state: "inspected".into(),
                        error: None,
                        inspection: Some(value),
                    });
                }
                Err(error) => {
                    canceled(cancel)?;
                    gaps = true;
                    files.push(crate::lightroom::plan::OriginalFileObservation {
                        origin,
                        path: NativePath::from_path(&path),
                        state: "unavailable".into(),
                        error: Some(error.to_string()),
                        inspection: None,
                    });
                }
            }
        }
    }
    Ok(crate::lightroom::plan::OriginalObservation {
        token: candidate.token.clone(),
        base_state: base_state.into(),
        metadata,
        files,
        packet_gaps: gaps,
    })
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
            LightroomWorkbenchIo::OriginalBegin {
                operation,
                workbench,
                generation,
                candidate,
                maximum_result_bytes,
            } => {
                ensure!(
                    self.original.is_none(),
                    "original inspection already retained"
                );
                self.released_original = None;
                let observation = inspect_original(&candidate, cancel)?;
                let maximum = usize::try_from(maximum_result_bytes.0)?;
                let encoded = bounded_json(&observation, maximum)?;
                let blake3 = crate::lightroom::digest(&encoded);
                let value = Original {
                    operation: operation.clone(),
                    workbench,
                    generation,
                    candidate,
                    maximum_result_bytes: maximum,
                    encoded,
                    blake3: blake3.clone(),
                };
                let reply = LightroomWorkbenchIoReply::OriginalReady {
                    operation,
                    token: value.candidate.token.clone(),
                    bytes: U64(value.encoded.len() as u64),
                    blake3,
                };
                self.original = Some(value);
                Ok(reply)
            }
            LightroomWorkbenchIo::OriginalCurrent {
                operation,
                workbench,
                generation,
                token,
            } => {
                let value = self
                    .original
                    .as_mut()
                    .context("no original inspection retained")?;
                same(
                    (&value.operation, &value.workbench, &value.generation),
                    (&operation, &workbench, &generation),
                )?;
                ensure!(value.candidate.token == token, "original token differs");
                let current = inspect_original(&value.candidate, cancel)?;
                let encoded = bounded_json(&current, value.maximum_result_bytes)?;
                ensure!(
                    crate::lightroom::digest(&encoded) == value.blake3,
                    "original or sidecar changed before commit"
                );
                Ok(LightroomWorkbenchIoReply::OriginalReady {
                    operation,
                    token,
                    bytes: U64(value.encoded.len() as u64),
                    blake3: value.blake3.clone(),
                })
            }
            LightroomWorkbenchIo::OriginalPage {
                operation,
                workbench,
                generation,
                token,
                offset,
                limit,
            } => {
                let value = self
                    .original
                    .as_ref()
                    .context("no original inspection retained")?;
                same(
                    (&value.operation, &value.workbench, &value.generation),
                    (&operation, &workbench, &generation),
                )?;
                ensure!(value.candidate.token == token, "original token differs");
                let start = usize::try_from(offset.0)?;
                let count = usize::try_from(limit.0)?;
                ensure!(start < value.encoded.len(), "original page offset");
                let end = start.saturating_add(count).min(value.encoded.len());
                Ok(LightroomWorkbenchIoReply::OriginalChunk {
                    operation,
                    token,
                    offset,
                    bytes: value.encoded[start..end].to_vec(),
                })
            }
            LightroomWorkbenchIo::OriginalRelease {
                operation,
                workbench,
                generation,
                token,
            } => {
                let receipt =
                    ReleaseReceipt::new(&operation, &workbench, &generation, Some(&token));
                if self.released_original.as_ref() == Some(&receipt) {
                    return Ok(receipt.reply());
                }
                let value = self
                    .original
                    .as_ref()
                    .context("no original inspection retained")?;
                same(
                    (&value.operation, &value.workbench, &value.generation),
                    (&operation, &workbench, &generation),
                )?;
                ensure!(value.candidate.token == token, "original token differs");
                self.original.take();
                self.released_original = Some(receipt.clone());
                Ok(receipt.reply())
            }
            LightroomWorkbenchIo::SealBegin {
                operation,
                workbench,
                generation,
                token,
                output,
                approval_bytes,
                approval_blake3,
                review_bytes,
                review_blake3,
            } => {
                ensure!(self.seal.is_none(), "seal operation already retained");
                canceled(cancel)?;
                let requested = output.to_path()?;
                ensure!(requested.is_absolute(), "seal output must be absolute");
                let parent = requested.parent().context("seal output parent absent")?;
                crate::lightroom::source::reject_links(parent)?;
                fs::create_dir(&requested).context("seal output must be a new directory")?;
                let directory = fs::canonicalize(&requested)?;
                ensure!(
                    fs::symlink_metadata(&directory)?.is_dir(),
                    "seal output is not a direct directory"
                );
                let approval_path = directory.join("approval.json");
                let review_path = directory.join("review.json");
                let database = directory.join("inspection.sqlite3");
                let seal_path = directory.join("input-seal.json");
                let approval = Upload::create(&approval_path, approval_bytes.0, approval_blake3)?;
                let review = Upload::create(&review_path, review_bytes.0, review_blake3)?;
                self.seal = Some(Seal {
                    operation: operation.clone(),
                    workbench,
                    generation,
                    token: token.clone(),
                    directory: NativePath::from_path(&directory),
                    database: NativePath::from_path(&database),
                    approval_path: NativePath::from_path(&approval_path),
                    seal_path: NativePath::from_path(&seal_path),
                    approval,
                    review,
                    seal: None,
                    database_physical: None,
                    database_identity: None,
                    database_blake3: None,
                    published_blake3: None,
                    state: LightroomWorkbenchSealState::Staging,
                });
                Ok(LightroomWorkbenchIoReply::SealUpload {
                    operation,
                    token,
                    document: LightroomWorkbenchSealDocument::Approval,
                    offset: U64(0),
                })
            }
            LightroomWorkbenchIo::SealChunk {
                operation,
                workbench,
                generation,
                token,
                document,
                offset,
                bytes,
            } => {
                canceled(cancel)?;
                let value = self.seal.as_mut().context("no seal operation retained")?;
                value.same(&operation, &workbench, &generation, &token)?;
                ensure!(
                    match document {
                        LightroomWorkbenchSealDocument::Seal => {
                            value.state == LightroomWorkbenchSealState::Hashed
                        }
                        _ => value.state == LightroomWorkbenchSealState::Staging,
                    },
                    "seal upload phase differs"
                );
                let (next, complete) = {
                    let upload = value.upload(document)?;
                    let next = upload.append(offset.0, &bytes)?;
                    (next, next == upload.expected_bytes)
                };
                if document == LightroomWorkbenchSealDocument::Seal && complete {
                    value.state = LightroomWorkbenchSealState::PublishReady;
                }
                Ok(LightroomWorkbenchIoReply::SealUpload {
                    operation,
                    token,
                    document,
                    offset: U64(next),
                })
            }
            LightroomWorkbenchIo::SealStage {
                operation,
                workbench,
                generation,
                token,
            } => {
                canceled(cancel)?;
                let value = self.seal.as_mut().context("no seal operation retained")?;
                value.same(&operation, &workbench, &generation, &token)?;
                ensure!(
                    value.state == LightroomWorkbenchSealState::Staging,
                    "seal is not staging"
                );
                value.approval.finish()?;
                value.review.finish()?;
                let directory = value.directory.to_path()?;
                let mut pending = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(directory.join("pending.json"))?;
                serde_json::to_writer(
                    &mut pending,
                    &serde_json::json!({
                        "protocol": 1,
                        "state": "pending",
                        "token": value.token,
                        "approval_blake3": value.approval.expected_blake3,
                        "review_blake3": value.review.expected_blake3,
                        "admission": "Only the atomic final input-seal.json is completed authority"
                    }),
                )?;
                pending.sync_all()?;
                let path = value.database.to_path()?;
                let database = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                database.sync_all()?;
                let physical = FileKey::of(&database)?;
                drop(database);
                value.database_physical = Some(physical.clone());
                value.state = LightroomWorkbenchSealState::Staged;
                Ok(LightroomWorkbenchIoReply::SealStaged {
                    operation,
                    token,
                    directory: value.directory.clone(),
                    database: value.database.clone(),
                    physical,
                })
            }
            LightroomWorkbenchIo::SealSyncHash {
                operation,
                workbench,
                generation,
                token,
                maximum_bytes,
            } => {
                canceled(cancel)?;
                let value = self.seal.as_mut().context("no seal operation retained")?;
                value.same(&operation, &workbench, &generation, &token)?;
                ensure!(
                    value.state == LightroomWorkbenchSealState::Staged,
                    "seal destination is not staged"
                );
                let path = value.database.to_path()?;
                companion_free(&path)?;
                fs::OpenOptions::new().write(true).open(&path)?.sync_all()?;
                let mut source = Source::open(&path, maximum_bytes.0)?;
                ensure!(
                    FileKey::of(&source.file)?
                        == value
                            .database_physical
                            .clone()
                            .context("seal database identity absent")?,
                    "seal destination object changed"
                );
                let digest = source.copy_and_hash_controlled(None, || canceled(cancel))?;
                source.verify()?;
                let identity = source.before.clone();
                drop(source);
                value.database_identity = Some(identity.clone());
                value.database_blake3 = Some(digest.clone());
                value.state = LightroomWorkbenchSealState::Hashed;
                Ok(LightroomWorkbenchIoReply::SealHashed {
                    operation,
                    token,
                    identity,
                    blake3: digest,
                    directory: value.directory.clone(),
                    database: value.database.clone(),
                })
            }
            LightroomWorkbenchIo::SealPublishBegin {
                operation,
                workbench,
                generation,
                token,
                bytes,
                blake3,
            } => {
                canceled(cancel)?;
                let value = self.seal.as_mut().context("no seal operation retained")?;
                value.same(&operation, &workbench, &generation, &token)?;
                ensure!(
                    value.state == LightroomWorkbenchSealState::Hashed && value.seal.is_none(),
                    "seal publication upload phase differs"
                );
                let pending = value.directory.to_path()?.join("input-seal.pending.json");
                value.seal = Some(Upload::create(&pending, bytes.0, blake3)?);
                Ok(LightroomWorkbenchIoReply::SealUpload {
                    operation,
                    token,
                    document: LightroomWorkbenchSealDocument::Seal,
                    offset: U64(0),
                })
            }
            LightroomWorkbenchIo::SealPublish {
                operation,
                workbench,
                generation,
                token,
            } => {
                let value = self.seal.as_mut().context("no seal operation retained")?;
                value.same(&operation, &workbench, &generation, &token)?;
                if value.state == LightroomWorkbenchSealState::Published {
                    return Ok(value.reply());
                }
                canceled(cancel)?;
                ensure!(
                    value.state == LightroomWorkbenchSealState::PublishReady,
                    "seal is not publishable"
                );
                let upload = value
                    .seal
                    .as_mut()
                    .context("seal publication upload absent")?;
                upload.finish()?;
                let digest = upload.expected_blake3.clone();
                let mut database = Source::open(
                    &value.database.to_path()?,
                    value
                        .database_identity
                        .as_ref()
                        .context("sealed database identity absent")?
                        .bytes,
                )?;
                companion_free(&database.path)?;
                ensure!(
                    database.before
                        == *value
                            .database_identity
                            .as_ref()
                            .context("sealed database identity absent")?
                        && FileKey::of(&database.file)?
                            == value
                                .database_physical
                                .clone()
                                .context("sealed database physical identity absent")?,
                    "sealed database changed before publication"
                );
                let database_digest = database.copy_and_hash_controlled(None, || Ok(()))?;
                companion_free(&database.path)?;
                database.verify()?;
                companion_free(&database.path)?;
                ensure!(
                    Some(&database_digest) == value.database_blake3.as_ref(),
                    "sealed database bytes changed before publication"
                );
                drop(database);
                let pending = value.directory.to_path()?.join("input-seal.pending.json");
                fs::hard_link(&pending, value.seal_path.to_path()?)
                    .context("atomic create-new selection seal publication")?;
                value.published_blake3 = Some(digest);
                value.state = LightroomWorkbenchSealState::Published;
                Ok(value.reply())
            }
            LightroomWorkbenchIo::SealStatus {
                operation,
                workbench,
                generation,
                token,
            } => {
                let value = self.seal.as_ref().context("no seal operation retained")?;
                value.same(&operation, &workbench, &generation, &token)?;
                Ok(value.reply())
            }
            LightroomWorkbenchIo::SealAbort {
                operation,
                workbench,
                generation,
                token,
            } => {
                let value = self.seal.as_mut().context("no seal operation retained")?;
                value.same(&operation, &workbench, &generation, &token)?;
                ensure!(
                    value.state != LightroomWorkbenchSealState::Published,
                    "published seal cannot be aborted"
                );
                value.approval.file.take();
                value.review.file.take();
                if let Some(upload) = &mut value.seal {
                    upload.file.take();
                }
                value.state = LightroomWorkbenchSealState::Aborted;
                Ok(value.reply())
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

#[cfg(test)]
mod tests {
    use super::*;

    const OPERATION: &str = "operation";
    const WORKBENCH: &str = "workbench";
    const GENERATION: &str = "generation";
    const TOKEN: &str = "seal-token";

    fn call(owner: &mut Owner, request: LightroomWorkbenchIo) -> Result<LightroomWorkbenchIoReply> {
        request.validate()?;
        let reply = owner.execute(request.clone(), &AtomicBool::new(false))?;
        reply.validate_for(&request)?;
        Ok(reply)
    }

    fn request_chunk(
        document: LightroomWorkbenchSealDocument,
        bytes: &[u8],
    ) -> LightroomWorkbenchIo {
        LightroomWorkbenchIo::SealChunk {
            operation: OPERATION.into(),
            workbench: WORKBENCH.into(),
            generation: GENERATION.into(),
            token: TOKEN.into(),
            document,
            offset: U64(0),
            bytes: bytes.to_vec(),
        }
    }

    fn staged(owner: &mut Owner, root: &Path) -> Result<NativePath> {
        let approval = br#"{"approval":true}"#;
        let review = br#"{"review":true}"#;
        call(
            owner,
            LightroomWorkbenchIo::SealBegin {
                operation: OPERATION.into(),
                workbench: WORKBENCH.into(),
                generation: GENERATION.into(),
                token: TOKEN.into(),
                output: NativePath::from_path(root),
                approval_bytes: U64(approval.len() as u64),
                approval_blake3: crate::lightroom::digest(approval),
                review_bytes: U64(review.len() as u64),
                review_blake3: crate::lightroom::digest(review),
            },
        )?;
        call(
            owner,
            request_chunk(LightroomWorkbenchSealDocument::Approval, approval),
        )?;
        call(
            owner,
            request_chunk(LightroomWorkbenchSealDocument::Review, review),
        )?;
        let reply = call(
            owner,
            LightroomWorkbenchIo::SealStage {
                operation: OPERATION.into(),
                workbench: WORKBENCH.into(),
                generation: GENERATION.into(),
                token: TOKEN.into(),
            },
        )?;
        let LightroomWorkbenchIoReply::SealStaged { database, .. } = reply else {
            anyhow::bail!("stage reply")
        };
        Ok(database)
    }

    fn hashed(owner: &mut Owner, database: &NativePath) -> Result<()> {
        let db = rusqlite::Connection::open(database.to_path()?)?;
        db.execute_batch(
            "PRAGMA journal_mode=DELETE; CREATE TABLE evidence(value TEXT NOT NULL); INSERT INTO evidence VALUES ('stable');",
        )?;
        drop(db);
        call(
            owner,
            LightroomWorkbenchIo::SealSyncHash {
                operation: OPERATION.into(),
                workbench: WORKBENCH.into(),
                generation: GENERATION.into(),
                token: TOKEN.into(),
                maximum_bytes: U64(1024 * 1024),
            },
        )?;
        let seal = br#"{"protocol":1}"#;
        call(
            owner,
            LightroomWorkbenchIo::SealPublishBegin {
                operation: OPERATION.into(),
                workbench: WORKBENCH.into(),
                generation: GENERATION.into(),
                token: TOKEN.into(),
                bytes: U64(seal.len() as u64),
                blake3: crate::lightroom::digest(seal),
            },
        )?;
        call(
            owner,
            request_chunk(LightroomWorkbenchSealDocument::Seal, seal),
        )?;
        Ok(())
    }

    #[test]
    fn seal_rejects_staged_database_change_before_publication() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut owner = Owner::default();
        let database = staged(&mut owner, &temp.path().join("sealed"))?;
        hashed(&mut owner, &database)?;
        let db = rusqlite::Connection::open(database.to_path()?)?;
        db.execute("INSERT INTO evidence VALUES ('changed')", [])?;
        drop(db);
        let result = call(
            &mut owner,
            LightroomWorkbenchIo::SealPublish {
                operation: OPERATION.into(),
                workbench: WORKBENCH.into(),
                generation: GENERATION.into(),
                token: TOKEN.into(),
            },
        );
        assert!(result.is_err());
        assert!(!temp.path().join("sealed/input-seal.json").exists());
        Ok(())
    }

    #[test]
    fn seal_status_recovers_lost_successful_publication_reply() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut owner = Owner::default();
        let database = staged(&mut owner, &temp.path().join("sealed"))?;
        hashed(&mut owner, &database)?;
        call(
            &mut owner,
            LightroomWorkbenchIo::SealPublish {
                operation: OPERATION.into(),
                workbench: WORKBENCH.into(),
                generation: GENERATION.into(),
                token: TOKEN.into(),
            },
        )?;
        let reply = call(
            &mut owner,
            LightroomWorkbenchIo::SealStatus {
                operation: OPERATION.into(),
                workbench: WORKBENCH.into(),
                generation: GENERATION.into(),
                token: TOKEN.into(),
            },
        )?;
        assert!(matches!(
            reply,
            LightroomWorkbenchIoReply::SealState {
                state: LightroomWorkbenchSealState::Published,
                seal_blake3: Some(_),
                ..
            }
        ));
        assert!(temp.path().join("sealed/input-seal.json").is_file());
        Ok(())
    }

    #[test]
    fn malformed_seal_request_has_no_filesystem_effect() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let output = temp.path().join("must-not-exist");
        let mut owner = Owner::default();
        let request = LightroomWorkbenchIo::SealBegin {
            operation: OPERATION.into(),
            workbench: WORKBENCH.into(),
            generation: GENERATION.into(),
            token: TOKEN.into(),
            output: NativePath::from_path(&output),
            approval_bytes: U64(0),
            approval_blake3: "0".repeat(64),
            review_bytes: U64(1),
            review_blake3: "0".repeat(64),
        };
        assert!(call(&mut owner, request).is_err());
        assert!(!output.exists());
        Ok(())
    }
}
