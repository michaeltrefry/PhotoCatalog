//! F-owned worker stages and prepared files. No process or SQLite ownership.
use super::wire::{Failure, FailureKind};
use crate::catalog_session::{LeaseId, preview_stage::*};
use crate::storage_volume::NativePath;
use anyhow::{Context, Result, ensure};
#[cfg(test)]
use std::path::PathBuf;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
const CHUNK: usize = crate::catalog_session::preview_io::CHUNK_BYTES;
const RETAINED_BYTES: usize = 4 * 1024 * 1024;
const STAGE_SLOTS: usize = 16;
struct Stage {
    id: LeaseId,
    path: Box<Path>,
    directory: Option<File>,
    created: bool,
    limits: Limits,
    native: Option<crate::application::U64>,
    drained: bool,
    rearming: Option<crate::application::U64>,
    input: Option<File>,
    input_bytes: u64,
    input_hash: blake3::Hasher,
    sealed: Option<Box<str>>,
}
impl Stage {
    fn verify(&self) -> Result<()> {
        let held = self
            .directory
            .as_ref()
            .context("stage directory admission incomplete; inspect retained token")?;
        let now = super::bootstrap::open_directory(&self.path)?;
        ensure!(
            crate::catalog_storage::physical_object_id(held)?
                == crate::catalog_storage::physical_object_id(&now)?,
            "stage directory moved or replaced"
        );
        Ok(())
    }
}
struct Stream {
    stage: LeaseId,
    file: File,
    offset: u64,
    bytes: u64,
    digest: String,
    hash: blake3::Hasher,
}
struct PreparedRoot {
    path: Box<Path>,
    directory: File,
    ready: bool,
}
impl PreparedRoot {
    fn verify(&self) -> Result<()> {
        ensure!(
            fs::symlink_metadata(&self.path)?.file_type().is_dir(),
            "prepared root is not an ordinary directory"
        );
        let current = super::bootstrap::open_directory(&self.path)?;
        ensure!(
            crate::catalog_storage::physical_object_id(&self.directory)?
                == crate::catalog_storage::physical_object_id(&current)?,
            "prepared root moved or replaced"
        );
        Ok(())
    }
}
#[derive(Default)]
struct Ledger {
    high: u64,
    digest: Option<[u8; 32]>,
    result: Option<std::result::Result<Reply, Failure>>,
}
pub(super) struct Owner {
    stages: Vec<Stage>,
    stream: Option<Stream>,
    prepared: Option<PreparedRoot>,
    ledgers: [Ledger; 2],
    workers: Option<u8>,
    budget: usize,
}
impl Default for Owner {
    fn default() -> Self {
        Self {
            stages: Vec::with_capacity(STAGE_SLOTS),
            stream: None,
            prepared: None,
            ledgers: Default::default(),
            workers: None,
            budget: RETAINED_BYTES,
        }
    }
}
fn failure(kind: FailureKind, e: impl std::fmt::Display) -> anyhow::Error {
    Failure::new(kind, e).into()
}
fn regular(path: &Path) -> Result<File> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && !m.file_type().is_symlink(),
        "stage artifact is not a regular file"
    );
    Ok(File::open(path)?)
}
fn bounded_read(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let mut f = regular(path)?;
    let len = f.metadata()?.len();
    ensure!(len <= cap, "stage artifact length limit");
    let n = usize::try_from(len)?;
    let mut b = Vec::new();
    b.try_reserve_exact(n)?;
    b.resize(n, 0);
    f.read_exact(&mut b)?;
    let mut eof = [0];
    ensure!(f.read(&mut eof)? == 0, "stage artifact grew");
    Ok(b)
}
fn sync(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    };
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "stage root is not a directory"
    );
    Ok(())
}
fn native_path(path: &Path) -> Result<NativePath> {
    #[cfg(unix)]
    let units = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    };
    #[cfg(windows)]
    let units = {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().count()
    };
    ensure!(
        path.is_absolute() && (1..=crate::catalog_session::PATH_UNITS).contains(&units),
        "stage native path admission limit"
    );
    Ok(NativePath::from_path(path))
}
// Cleanup changes worker-UUID to claimed-UUID. Reserve its extra byte before
// creating the stage, including when a failed cleanup retains the renamed path.
fn stage_path_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
        + usize::from(
            path.file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"worker-")),
        )
}
impl Owner {
    pub fn empty(&self) -> bool {
        self.stages.is_empty() && self.stream.is_none()
    }
    fn stage(&self, id: &LeaseId) -> Result<&Stage> {
        let s = self
            .stages
            .iter()
            .find(|s| s.id == *id)
            .context("unknown stage token")?;
        s.verify()?;
        Ok(s)
    }
    fn stage_mut(&mut self, id: &LeaseId) -> Result<&mut Stage> {
        let s = self
            .stages
            .iter_mut()
            .find(|s| s.id == *id)
            .context("unknown stage token")?;
        s.verify()?;
        Ok(s)
    }
    fn retained(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.stages.capacity() * std::mem::size_of::<Stage>()
            + self
                .stages
                .iter()
                .map(|s| stage_path_bytes(&s.path) + s.id.as_str().len() + 64)
                .sum::<usize>()
            + 2 * (crate::catalog_session::native::REQUEST_BYTES + 4096 + 512)
            + CHUNK
            + 256
            + self
                .prepared
                .as_ref()
                .map_or(0, |p| p.path.as_os_str().as_encoded_bytes().len())
    }
    pub fn call(&mut self, manifest: &Path, r: &Request, cancel: &AtomicBool) -> Result<Reply> {
        r.validate()?;
        let index = usize::from(r.supervisor);
        let mut h = blake3::Hasher::new();
        h.update(&serde_json::to_vec(r)?);
        h.update(r.binary());
        let digest = *h.finalize().as_bytes();
        let ledger = &self.ledgers[index];
        if r.operation.0 == ledger.high && ledger.digest == Some(digest) {
            return ledger
                .result
                .clone()
                .context("stage outcome unresolved")?
                .map_err(Into::into);
        }
        ensure!(
            r.operation.0 > ledger.high,
            "stale or altered stage operation"
        );
        ensure!(
            r.cleanup() || self.retained() <= self.budget,
            Failure::new(
                FailureKind::ResourceLimit,
                "Stage metadata capacity remains owned; complete cleanup and retry"
            )
        );
        if !r.cleanup() && cancel.load(Ordering::Acquire) {
            return Err(failure(
                FailureKind::Canceled,
                "stage operation canceled before effects",
            ));
        }
        self.ledgers[index].high = r.operation.0;
        self.ledgers[index].digest = Some(digest);
        self.ledgers[index].result = None;
        let result = self.execute(manifest, r, cancel).map(|value| Reply {
            epoch: r.root.epoch.clone(),
            session: r.root.session.clone(),
            operation: r.operation,
            value,
        });
        let cached = result.map_err(|e| {
            let mut f = e
                .downcast_ref::<Failure>()
                .cloned()
                .unwrap_or_else(|| Failure::new(FailureKind::Unknown, e));
            f.object_receipt = Some(crate::catalog_session::preview_io::FailureReceipt {
                operation: r.operation,
                step: crate::application::U64(0),
                request_digest: digest,
            });
            f
        });
        self.ledgers[index].result = Some(cached.clone());
        cached.map_err(Into::into)
    }
    fn execute(&mut self, manifest: &Path, r: &Request, cancel: &AtomicBool) -> Result<Value> {
        let root = manifest.join("workers");
        match &r.action {
            Action::BeginLegacyRead { hash, allowance } => {
                ensure!(
                    self.stream.is_none(),
                    "another stage transfer remains owned"
                );
                // Bootstrap has already verified this catalog capability. The
                // manifest/cache root can be elsewhere and is not legacy authority.
                let previews = r.root.canonical_root.to_path()?.join("previews");
                let missing = || Value::LegacyRead {
                    transfer: None,
                    bytes: crate::application::U64(0),
                };
                match fs::symlink_metadata(&previews) {
                    Ok(metadata) => ensure!(
                        metadata.file_type().is_dir(),
                        "legacy preview root is not an ordinary directory"
                    ),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(missing()),
                    Err(e) => return Err(e.into()),
                }
                let path = previews.join(format!("{hash}.jpg"));
                native_path(&path)?;
                let file = match regular(&path) {
                    Ok(file) => file,
                    Err(e)
                        if e.downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        return Ok(missing());
                    }
                    Err(e) => return Err(e),
                };
                let bytes = file.metadata()?.len();
                ensure!(
                    bytes <= LEGACY_BYTES,
                    "legacy preview exceeds compatibility format limit"
                );
                ensure!(
                    bytes <= allowance.0,
                    Failure::new(
                        FailureKind::ResourceLimit,
                        "legacy preview encoded allowance"
                    )
                );
                let transfer = LeaseId::new();
                ensure!(
                    !self.stages.iter().any(|stage| stage.id == transfer),
                    "legacy transfer identity collides with worker stage"
                );
                self.stream = Some(Stream {
                    stage: transfer.clone(),
                    file,
                    offset: 0,
                    bytes,
                    digest: hash.clone(),
                    hash: blake3::Hasher::new(),
                });
                Ok(Value::LegacyRead {
                    transfer: Some(transfer),
                    bytes: crate::application::U64(bytes),
                })
            }
            Action::Admit { limits } => {
                ensure!(
                    self.workers.is_none_or(|v| v == limits.workers),
                    "stage worker policy changed"
                );
                ensure!(
                    self.stages.len() < usize::from(limits.workers),
                    Failure::new(
                        FailureKind::ResourceLimit,
                        "Worker stages remain owned; drain and consume or clean them before retry"
                    )
                );
                let id = LeaseId::new();
                let path = root
                    .join(format!("worker-{}", id.as_str()))
                    .into_boxed_path();
                // Admit the returned native path before creating a stage.
                native_path(&path)?;
                let cost = self
                    .retained()
                    .checked_add(stage_path_bytes(&path) + 36 + 64)
                    .context("stage capacity overflow")?;
                ensure!(
                    cost <= self.budget,
                    Failure::new(
                        FailureKind::ResourceLimit,
                        "Worker stage path capacity is exhausted; clean retained stages and retry"
                    )
                );
                // All validated workers fit the storage charged by retained().
                ensure!(
                    self.stages.len() < self.stages.capacity(),
                    "stage slot bound"
                );
                directory(&root)?;
                // Retain the token/path before creation: partial creation is
                // inspected/cleaned through this same admitted identity.
                self.workers = Some(limits.workers);
                self.stages.push(Stage {
                    id: id.clone(),
                    path,
                    directory: None,
                    created: false,
                    limits: limits.clone(),
                    native: None,
                    drained: false,
                    rearming: None,
                    input: None,
                    input_bytes: 0,
                    input_hash: blake3::Hasher::new(),
                    sealed: None,
                });
                let stage = self.stages.last_mut().unwrap();
                let result = (|| -> Result<()> {
                    fs::create_dir(&stage.path)?;
                    stage.created = true;
                    stage.directory = Some(super::bootstrap::open_directory(&stage.path)?);
                    let mut marker = OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(stage.path.join("managed.stage"))?;
                    marker.write_all(b"managed-stage-1")?;
                    marker.sync_all()?;
                    sync(&root)?;
                    Ok(())
                })();
                let error = result
                    .err()
                    .map(|e| Failure::new(FailureKind::Unknown, e).message);
                Ok(Value::Admitted {
                    stage: id,
                    ready: error.is_none(),
                    error,
                })
            }
            Action::Upload {
                stage,
                offset,
                bytes,
            } => {
                let s = self.stage_mut(stage)?;
                ensure!(
                    s.native.is_none() && s.sealed.is_none(),
                    "stage input is immutable"
                );
                ensure!(offset.0 == s.input_bytes, "stage input offset");
                let next = s
                    .input_bytes
                    .checked_add(bytes.len() as u64)
                    .context("stage input overflow")?;
                ensure!(next <= s.limits.encoded.0, "stage input allowance");
                if s.input.is_none() {
                    s.input = Some(
                        OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(s.path.join("input.encoded"))?,
                    );
                }
                s.input.as_mut().unwrap().write_all(bytes)?;
                s.input_hash.update(bytes);
                s.input_bytes = next;
                Ok(Value::Unit)
            }
            Action::SealInput {
                stage,
                bytes,
                digest,
            } => {
                let s = self.stage_mut(stage)?;
                ensure!(s.native.is_none(), "native input already armed");
                ensure!(
                    s.input_bytes == bytes.0
                        && s.input_hash.clone().finalize().to_hex().as_str() == digest,
                    "stage input integrity"
                );
                s.input
                    .as_ref()
                    .context("stage input not written")?
                    .sync_all()?;
                s.input.take();
                s.sealed = Some(digest.clone().into_boxed_str());
                sync(&s.path)?;
                Ok(Value::Unit)
            }
            Action::Arm { stage, native } => {
                ensure!(
                    self.stream.as_ref().is_none_or(|s| s.stage != *stage),
                    "stage transfer remains owned"
                );
                let s = self.stage_mut(stage)?;
                if s.drained {
                    ensure!(
                        s.native.is_some_and(|old| old.0 < native.0) && s.sealed.is_some(),
                        "only a sealed header stage can rearm under a new native identity"
                    );
                    reset_header_stage(s, *native)?;
                    s.drained = false;
                    s.native = None;
                    s.rearming = None;
                }
                ensure!(
                    s.input.is_none() && (s.input_bytes == 0 || s.sealed.is_some()),
                    "stage input not sealed"
                );
                ensure!(
                    s.native.is_none_or(|v| v == *native),
                    "different native stage owner"
                );
                s.native = Some(*native);
                Ok(Value::Path(native_path(&s.path)?))
            }
            Action::NativeDrained { stage, native } => {
                let s = self.stage_mut(stage)?;
                ensure!(
                    s.native.is_none_or(|v| v == *native),
                    "different native stage drain"
                );
                s.native = Some(*native);
                s.drained = true;
                Ok(Value::Unit)
            }
            Action::Metadata { stage, artifact } => {
                let s = self.stage(stage)?;
                if matches!(artifact, Artifact::Receipt | Artifact::Error) {
                    ensure!(s.drained, "native result before verified drain");
                }
                let path = s.path.join(artifact.name()?);
                let cap = if *artifact == Artifact::Decoded {
                    16
                } else {
                    crate::catalog_session::native::REQUEST_BYTES as u64
                };
                match bounded_read(&path, cap) {
                    Ok(v) => Ok(Value::Metadata(Some(v))),
                    Err(e)
                        if e.downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        Ok(Value::Metadata(None))
                    }
                    Err(e) => Err(e),
                }
            }
            Action::BeginRead {
                stage,
                artifact,
                bytes,
                digest,
            } => {
                ensure!(
                    self.stream.is_none(),
                    "another stage transfer remains owned"
                );
                let s = self.stage(stage)?;
                ensure!(s.drained, "stage output before verified native drain");
                let cap = match artifact {
                    Artifact::Encoded(_) => s.limits.encoded.0,
                    Artifact::Rgb(_) => s.limits.rgb.0,
                    Artifact::Prepared => s.limits.prepared.0,
                    _ => anyhow::bail!("not a stage output"),
                };
                ensure!(bytes.0 <= cap, "stage output allowance");
                let file = regular(&s.path.join(artifact.name()?))?;
                ensure!(file.metadata()?.len() == bytes.0, "stage output length");
                self.stream = Some(Stream {
                    stage: stage.clone(),
                    file,
                    offset: 0,
                    bytes: bytes.0,
                    digest: digest.clone(),
                    hash: blake3::Hasher::new(),
                });
                Ok(Value::Unit)
            }
            Action::Read { stage, offset } => {
                let stream = self.stream.as_mut().context("no stage transfer")?;
                ensure!(
                    stream.stage == *stage && stream.offset == offset.0,
                    "stage transfer identity/offset"
                );
                let n = usize::try_from((stream.bytes - stream.offset).min(CHUNK as u64))?;
                let mut bytes = vec![0; n];
                stream.file.read_exact(&mut bytes)?;
                stream.hash.update(&bytes);
                stream.offset += n as u64;
                Ok(Value::Chunk { bytes })
            }
            Action::FinishRead { stage } => {
                if let Some(stream) = &mut self.stream {
                    ensure!(stream.stage == *stage, "stage transfer identity");
                    ensure!(
                        stream.offset == stream.bytes
                            && stream.hash.clone().finalize().to_hex().as_str() == stream.digest,
                        "stage output integrity"
                    );
                    let mut eof = [0];
                    ensure!(stream.file.read(&mut eof)? == 0, "stage output grew");
                }
                self.stream.take();
                Ok(Value::Unit)
            }
            Action::AbortRead { stage } => {
                ensure!(
                    self.stream.as_ref().is_none_or(|s| s.stage == *stage),
                    "stage transfer identity"
                );
                self.stream.take();
                Ok(Value::Unit)
            }
            Action::Release { stage } => {
                self.release(stage)?;
                Ok(Value::Unit)
            }
            Action::AbandonOwned => {
                ensure!(
                    self.stages.iter().all(|s| s.native.is_none() || s.drained),
                    "native stage owner remains live"
                );
                self.stream.take();
                while let Some(stage) = self.stages.first() {
                    let id = stage.id.clone();
                    self.release(&id)?;
                }
                Ok(Value::Unit)
            }
            Action::Recover { limit } => {
                directory(&root)?;
                let mut removed = 0;
                for entry in fs::read_dir(&root)?.take(usize::from(*limit)) {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    let entry = entry?;
                    let path = entry.path();
                    if self.stages.iter().any(|s| s.path.as_ref() == path) {
                        continue;
                    }
                    ensure!(entry.file_type()?.is_dir(), "unknown worker recovery entry");
                    let mut path = path.into_boxed_path();
                    removed += u16::from(clean(&mut path)?);
                }
                Ok(Value::Count(removed))
            }
            Action::PreparedInitialize => {
                if self.prepared.is_none() {
                    let path = manifest.join("prepared").into_boxed_path();
                    native_path(&path)?;
                    ensure!(
                        self.retained()
                            .checked_add(path.as_os_str().as_encoded_bytes().len())
                            .is_some_and(|cost| cost <= self.budget),
                        Failure::new(
                            FailureKind::ResourceLimit,
                            "Prepared root path capacity is exhausted"
                        )
                    );
                    directory(&path)?;
                    let directory = super::bootstrap::open_directory(&path)?;
                    self.prepared = Some(PreparedRoot {
                        path,
                        directory,
                        ready: false,
                    });
                }
                let prepared = self.prepared.as_mut().unwrap();
                prepared.verify()?;
                if !prepared.ready {
                    prepared_initialize(&prepared.path)?;
                    prepared.verify()?;
                    prepared.ready = true;
                }
                Ok(Value::Unit)
            }
            Action::ObserveSource { path } => Ok(Value::Source(
                crate::preview::prepared_cache::SourceInstance::read(&path.to_path()?)?,
            )),
            Action::PreparedAdopt {
                stage,
                key,
                receipt,
            } => {
                let root = self.prepared_root()?;
                let s = self.stage(stage)?;
                ensure!(s.drained, "prepared adoption before native drain");
                ensure!(receipt.bytes <= s.limits.prepared.0, "prepared allowance");
                let source = s.path.join("prepared.linear");
                crate::preview::prepared_cache::verify_file(
                    &source,
                    receipt,
                    crate::preview::prepared_cache::MAX_PROXY_BYTES,
                )?;
                let destination = root.join(format!("{key}.linear"));
                native_path(&destination)?;
                if destination.exists() {
                    crate::preview::prepared_cache::verify_file(
                        &destination,
                        receipt,
                        crate::preview::prepared_cache::MAX_PROXY_BYTES,
                    )?;
                } else {
                    fs::hard_link(&source, &destination)?;
                    sync(root)?;
                }
                Ok(Value::Path(native_path(&destination)?))
            }
            Action::PreparedRemove { key } => {
                let root = self.prepared_root()?;
                match fs::remove_file(root.join(format!("{key}.linear"))) {
                    Ok(()) => sync(root)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                };
                Ok(Value::Unit)
            }
        }
    }
    fn prepared_root(&self) -> Result<&Path> {
        let prepared = self
            .prepared
            .as_ref()
            .context("prepared root not initialized")?;
        prepared.verify()?;
        ensure!(prepared.ready, "prepared initialization remains incomplete");
        Ok(&prepared.path)
    }
    fn release(&mut self, stage: &LeaseId) -> Result<()> {
        ensure!(
            self.stream.as_ref().is_none_or(|s| s.stage != *stage),
            "stage output transfer still owned"
        );
        let s = self
            .stages
            .iter_mut()
            .find(|s| s.id == *stage)
            .context("unknown stage token")?;
        ensure!(
            s.native.is_none() || s.drained,
            "native stage owner remains live"
        );
        s.input.take();
        if !s.created {
            self.stages.retain(|s| s.id != *stage);
            return Ok(());
        }
        s.verify()?;
        s.directory.take();
        let cleaned = clean(&mut s.path);
        if s.path.exists() {
            s.directory = Some(super::bootstrap::open_directory(&s.path)?);
        }
        ensure!(cleaned?, "stage cleanup remains pending; retry");
        self.stages.retain(|s| s.id != *stage);
        Ok(())
    }
}
fn prepared_initialize(root: &Path) -> Result<()> {
    directory(root)?;
    let mut names = Vec::new();
    for entry in fs::read_dir(root)?.take(1025) {
        let entry = entry?;
        let name = entry.file_name();
        let text = name.to_str().context("prepared filename")?;
        let key = text
            .strip_suffix(".linear")
            .context("unknown prepared artifact")?;
        ensure!(
            key.len() == 64
                && key.bytes().all(|b| b.is_ascii_hexdigit())
                && entry.file_type()?.is_file(),
            "unknown prepared artifact"
        );
        ensure!(names.len() < 1024, "prepared recovery entry bound");
        names.push(name);
    }
    for name in names {
        fs::remove_file(root.join(name))?;
    }
    sync(root)
}
fn reset_header_stage(stage: &mut Stage, next: crate::application::U64) -> Result<()> {
    ensure!(
        stage
            .path
            .file_name()
            .and_then(|v| v.to_str())
            .is_some_and(|v| v.starts_with("worker-")),
        "retired stage cannot rearm"
    );
    let allowed = [
        "managed.stage",
        "input.encoded",
        "header.ready",
        "header.pending",
        "active.lock",
    ];
    // Inspect the entire bounded roster before removing any prior checkpoint.
    for entry in fs::read_dir(&stage.path)?.take(allowed.len() + 1) {
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|s| allowed.contains(&s)),
            "stage has native output or unknown artifacts; cannot rearm"
        );
    }
    ensure!(
        bounded_read(&stage.path.join("managed.stage"), 32)? == b"managed-stage-1",
        "stage marker changed"
    );
    if stage.rearming.is_none() {
        let header: crate::catalog_session::native::Header =
            serde_json::from_slice(&bounded_read(
                &stage.path.join("header.ready"),
                crate::catalog_session::native::REQUEST_BYTES as u64,
            )?)?;
        header.validate()?;
        ensure!(
            Some(header.operation) == stage.native
                && header.stage == stage.id
                && header.input_bytes.0 == stage.input_bytes
                && stage.sealed.as_deref() == Some(header.input_digest.as_str()),
            "header does not belong to drained stage input"
        );
        let lock = regular(&stage.path.join("active.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)?;
        ensure!(
            lock.metadata()?.len() == 0,
            "retired or unknown native lease cannot rearm"
        );
        fs2::FileExt::unlock(&lock)?;
        drop(lock);
        // Preserve admission across an error after one checkpoint was removed.
        stage.rearming = Some(next);
    } else {
        ensure!(
            stage.rearming == Some(next),
            "different stage rearm remains incomplete"
        );
    }
    for name in ["header.ready", "header.pending", "active.lock"] {
        match fs::remove_file(stage.path.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    sync(&stage.path)
}

/// A cleanup claim never replaces an existing recovery directory.
fn rename_claim(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "exclusive stage rename is unsupported on this platform",
        ));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
        }
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) } != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}
fn clean(path: &mut Box<Path>) -> Result<bool> {
    use fs2::FileExt;
    match fs::symlink_metadata(path.as_ref()) {
        Ok(m) => ensure!(m.is_dir(), "stage cleanup directory"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e.into()),
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("stage directory name")?;
    ensure!(
        name.starts_with("worker-") || name.starts_with("claimed-"),
        "unrecognized stage directory"
    );
    let managed = match bounded_read(&path.join("managed.stage"), 32) {
        Ok(v) => {
            ensure!(v == b"managed-stage-1", "stage marker changed");
            true
        }
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            false
        }
        Err(e) => return Err(e),
    };
    let lock_path = path.join("active.lock");
    if let Ok(m) = fs::symlink_metadata(&lock_path) {
        ensure!(
            m.is_file() && !m.file_type().is_symlink(),
            "invalid stage lock"
        );
    }
    let mut lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    if let Err(e) = lock.try_lock_exclusive() {
        if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
            return Ok(false);
        }
        return Err(e.into());
    }
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
    let extra = [
        "managed.stage",
        "input.encoded",
        "header.ready",
        "header.pending",
        "0.rgb",
        "1.rgb",
    ];
    let mut names = Vec::new();
    for entry in fs::read_dir(path.as_ref())?.take(allowed.len() + extra.len() + 1) {
        let entry = entry?;
        let name = entry.file_name();
        ensure!(
            entry.file_type()?.is_file()
                && name
                    .to_str()
                    .is_some_and(|s| allowed.contains(&s) || (managed && extra.contains(&s))),
            "unexpected worker artifact requires inspection"
        );
        names.push(name);
    }
    let len = lock.metadata()?.len();
    ensure!(len <= 1, "unrecognized stage lease");
    if len == 0 {
        lock.write_all(&[1])?;
    } else {
        lock.seek(SeekFrom::Start(0))?;
        let mut b = [0];
        lock.read_exact(&mut b)?;
        ensure!(b == [1], "unrecognized stage lease");
    }
    lock.sync_all()?;
    fs2::FileExt::unlock(&lock)?;
    drop(lock);
    if !name.starts_with("claimed-") {
        let next = path
            .parent()
            .context("stage parent")?
            .join(format!("claimed-{}", uuid::Uuid::new_v4()));
        native_path(&next)?;
        rename_claim(path.as_ref(), &next)?;
        *path = next.into_boxed_path();
    }
    // Keep the format marker through any failed output removal so that a
    // retry still recognizes the managed-only artifact names.
    for name in names
        .iter()
        .filter(|n| n.as_os_str() != "active.lock" && n.as_os_str() != "managed.stage")
    {
        match fs::remove_file(path.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if managed {
        fs::remove_file(path.join("managed.stage"))?;
    }
    fs::remove_file(path.join("active.lock"))?;
    fs::remove_dir(path.as_ref())?;
    Ok(true)
}

#[cfg(test)]
mod tests;
