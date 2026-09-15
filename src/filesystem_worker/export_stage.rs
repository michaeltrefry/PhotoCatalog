//! Export-only F stage ownership. Native process custody belongs to G and SQL
//! authority belongs to C; this owner controls only closed transport artifacts.
use super::wire::{Failure, FailureKind};
use crate::{
    application::U64,
    catalog_exports::{ExportWork, StoredProfile},
    catalog_session::{LeaseId, RootCapability, export_stage::*},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy)]
enum Artifact {
    ParentLock,
    ActiveLock,
    Request,
    Icc,
    Xmp,
    Output,
    Receipt,
    Error,
}
impl Artifact {
    const ALL: [Self; 8] = [
        Self::ParentLock,
        Self::ActiveLock,
        Self::Request,
        Self::Icc,
        Self::Xmp,
        Self::Output,
        Self::Receipt,
        Self::Error,
    ];
    fn name(self) -> &'static str {
        match self {
            Self::ParentLock => "parent.lock",
            Self::ActiveLock => "active.lock",
            Self::Request => "request.json",
            Self::Icc => "profile.icc",
            Self::Xmp => "selected.xmp",
            Self::Output => "output",
            Self::Receipt => "result.json",
            Self::Error => "error.json",
        }
    }
}

#[derive(Clone)]
struct Cached {
    stage: LeaseId,
    operation: u64,
    digest: [u8; 32],
    result: std::result::Result<Reply, Failure>,
}

struct Upload {
    file: File,
    offset: u64,
    hasher: blake3::Hasher,
}
struct Uploaded {
    file: File,
    bytes: u64,
    digest: String,
}
enum BlobState {
    Absent,
    Uploading(Upload),
    Complete(Uploaded),
}
impl Default for BlobState {
    fn default() -> Self {
        Self::Absent
    }
}

struct Stage {
    id: LeaseId,
    root: RootCapability,
    binding: Binding,
    path: PathBuf,
    directory: File,
    parent_directory: File,
    removing: bool,
    removed: bool,
    parent_lock: File,
    active_lock: File,
    active_locked: bool,
    request_file: File,
    request_bytes: Vec<u8>,
    work: Box<ExportWork>,
    icc: BlobState,
    xmp: BlobState,
    ready: bool,
    native: Option<u64>,
    drained: Option<NativeTerminal>,
    inputs_valid: bool,
}
impl Stage {
    fn artifact(&self, artifact: Artifact) -> PathBuf {
        self.path.join(artifact.name())
    }
    fn verify(&self) -> Result<()> {
        verify_directory(&self.path, &self.directory, &self.parent_directory)?;
        if self.removing {
            return Ok(());
        }
        verify_file(&self.artifact(Artifact::ParentLock), &self.parent_lock)?;
        verify_file(&self.artifact(Artifact::ActiveLock), &self.active_lock)?;
        Ok(())
    }
    fn verify_request(&self) -> Result<()> {
        let bytes = bounded_read(
            &self.artifact(Artifact::Request),
            Some(&self.request_file),
            REQUEST_BYTES as u64,
        )?;
        ensure!(bytes == self.request_bytes, "export stage request changed");
        Ok(())
    }
    fn verify_inputs(&self) -> Result<()> {
        verify_blob(&self.path, Artifact::Icc, &self.icc)?;
        verify_blob(&self.path, Artifact::Xmp, &self.xmp)
    }
}

pub(super) struct Owner {
    stage: Option<Stage>,
    cleanup: Option<Cleanup>,
    seal: Option<Cached>,
    user: Option<Cached>,
    supervisor: Option<Cached>,
}
pub(super) fn owner_layout() -> (usize, usize) {
    (std::mem::size_of::<Owner>(), std::mem::align_of::<Owner>())
}
impl Default for Owner {
    fn default() -> Self {
        Self {
            stage: None,
            cleanup: None,
            seal: None,
            user: None,
            supervisor: None,
        }
    }
}
impl Owner {
    pub fn empty(&self) -> bool {
        self.stage.is_none() && self.cleanup.is_none()
    }
    pub fn call(
        &mut self,
        manifest: &Path,
        request: &Request,
        cancel: &AtomicBool,
    ) -> Result<Reply> {
        request.validate()?;
        let digest = request.digest()?;
        if let Some(stage) = &self.stage {
            ensure!(
                stage.id == request.stage
                    && stage.binding == request.binding
                    && stage.root == request.root,
                "export stage binding mismatch"
            );
        } else if let Some(cleanup) = &self.cleanup {
            ensure!(
                cleanup.id == request.stage
                    && cleanup.binding == request.binding
                    && cleanup.root == request.root,
                "export cleanup binding mismatch"
            );
        }
        // Seal receipt is terminal and separate from ordinary request replay.
        // Failed/canceled admission is also recorded, so retry never starts an
        // uncertain seal under either the old or a fresh operation identity.
        if let Some(seal) = &self.seal {
            if seal.stage == request.stage && matches!(request.action, Action::ResultAndSeal) {
                ensure!(
                    seal.operation == request.operation.0 && seal.digest == digest,
                    "altered or new export seal attempt"
                );
                return seal.result.clone().map_err(Into::into);
            }
        }
        let cached = if request.supervisor {
            &self.supervisor
        } else {
            &self.user
        };
        if let Some(cached) = cached {
            if cached.stage == request.stage && cached.operation == request.operation.0 {
                ensure!(cached.digest == digest, "altered export stage replay");
                return cached.result.clone().map_err(Into::into);
            }
            ensure!(
                cached.stage != request.stage || request.operation.0 > cached.operation,
                "stale export stage operation"
            );
        }
        let sealing = matches!(request.action, Action::ResultAndSeal);
        let result = if !request.cleanup() && cancel.load(Ordering::Acquire) {
            Err(Failure::new(
                FailureKind::Canceled,
                "export stage operation canceled before admission",
            )
            .into())
        } else {
            // After admission, reconcile the terminal seal despite cancellation.
            let admitted_cancel = AtomicBool::new(!sealing && cancel.load(Ordering::Acquire));
            self.execute(manifest, request, &admitted_cancel)
                .map(|value| reply(request, value))
        };
        let result = result.map_err(|error| {
            let mut failure = error.downcast_ref::<Failure>().cloned().unwrap_or_else(|| {
                let kind = if self.cleanup.is_some()
                    || matches!(
                        request.action,
                        Action::NativeDrained { .. } | Action::ResultAndSeal
                    ) {
                    FailureKind::Unknown
                } else {
                    FailureKind::Rejected
                };
                Failure::new(kind, error)
            });
            failure.object_receipt = Some(crate::catalog_session::preview_io::FailureReceipt {
                operation: request.operation,
                step: U64(0),
                request_digest: digest,
            });
            failure
        });
        let entry = Cached {
            stage: request.stage.clone(),
            operation: request.operation.0,
            digest,
            result: result.clone(),
        };
        if sealing && self.stage.is_some() {
            self.seal = Some(entry);
        } else if request.supervisor {
            self.supervisor = Some(entry);
        } else {
            self.user = Some(entry);
        }
        result.map_err(Into::into)
    }
    fn execute(
        &mut self,
        manifest: &Path,
        request: &Request,
        cancel: &AtomicBool,
    ) -> Result<Value> {
        match &request.action {
            Action::Begin { work, limits } => {
                ensure!(
                    self.stage.is_none() && self.cleanup.is_none(),
                    "another export stage remains owned"
                );
                request.binding.validate_work(work)?;
                let root = manifest.join("export-workers");
                let path = root.join(format!("photo-worker-{}", request.stage.as_str()));
                crate::export_worker::prepare_managed_request(work, *limits, &path)?;
                native_path(&path)?;
                directory(&root)?;
                let root = root.canonicalize()?;
                let path = root.join(format!("photo-worker-{}", request.stage.as_str()));
                let request_bytes =
                    crate::export_worker::prepare_managed_request(work, *limits, &path)?;
                let parent_directory = owned_directory(&root)?;
                ensure!(!path.try_exists()?, "export stage identity already exists");
                // Publish custody before mkdir: an error after its namespace
                // effect must not make the root look empty. Until a directory
                // handle is captured, its identity is unknown and path-only
                // cleanup or later adoption would risk deleting a replacement.
                self.cleanup = Some(Cleanup {
                    id: request.stage.clone(),
                    root: request.root.clone(),
                    binding: request.binding.clone(),
                    path: path.clone(),
                    directory: None,
                    parent_directory,
                    locks: Vec::new(),
                    removed: false,
                });
                let constructed = (|| -> Result<Stage> {
                    fs::create_dir(&path)?;
                    #[cfg(test)]
                    admission_fault(AdmissionFault::DirectoryOpen)?;
                    // Move each acquired handle into the retained owner before
                    // any further fallible work, including handle duplication.
                    self.cleanup.as_mut().unwrap().directory = Some(owned_directory(&path)?);
                    let cleanup = self.cleanup.as_ref().unwrap();
                    #[cfg(test)]
                    admission_fault(AdmissionFault::DirectoryClone)?;
                    let directory = cleanup.directory.as_ref().unwrap().try_clone()?;
                    #[cfg(test)]
                    admission_fault(AdmissionFault::ParentClone)?;
                    let parent_directory = cleanup.parent_directory.try_clone()?;
                    let parent_lock = create(&path.join(Artifact::ParentLock.name()), b"")?;
                    parent_lock
                        .try_lock_exclusive()
                        .context("acquire export parent lock")?;
                    self.cleanup
                        .as_mut()
                        .unwrap()
                        .locks
                        .push(parent_lock.try_clone()?);
                    let active_lock = create(&path.join(Artifact::ActiveLock.name()), b"")?;
                    // F closes this gate only until exact G Arm. G/N then own
                    // process execution while F retains the stage and parent lock.
                    active_lock
                        .try_lock_exclusive()
                        .context("acquire export launch gate")?;
                    self.cleanup
                        .as_mut()
                        .unwrap()
                        .locks
                        .push(active_lock.try_clone()?);
                    let request_file =
                        create(&path.join(Artifact::Request.name()), &request_bytes)?;
                    sync_directory(&path)?;
                    Ok(Stage {
                        id: request.stage.clone(),
                        root: request.root.clone(),
                        binding: request.binding.clone(),
                        path: path.clone(),
                        directory,
                        parent_directory,
                        removing: false,
                        removed: false,
                        parent_lock,
                        active_lock,
                        active_locked: true,
                        request_file,
                        request_bytes,
                        work: work.clone(),
                        icc: BlobState::Absent,
                        xmp: BlobState::Absent,
                        ready: false,
                        native: None,
                        drained: None,
                        inputs_valid: false,
                    })
                })();
                match constructed {
                    Ok(stage) => {
                        self.stage = Some(stage);
                        self.cleanup = None;
                        self.seal = None;
                        self.user = None;
                        self.supervisor = None;
                    }
                    Err(error) => {
                        if let Err(cleanup) = self.retry_cleanup(request) {
                            return Err(Failure::new(
                                FailureKind::Unknown,
                                format!(
                                    "export stage admission failed ({error:#}); cleanup remains owned ({cleanup:#})"
                                ),
                            )
                            .into());
                        }
                        self.cleanup = None;
                        return Err(error);
                    }
                }
                Ok(Value::Begun)
            }
            Action::UploadIcc { offset, bytes } => {
                let stage = self.stage(request)?;
                ensure!(
                    !stage.ready && stage.native.is_none(),
                    "export stage already ready"
                );
                upload(stage, Artifact::Icc, offset.0, bytes, BLOB_BYTES, true)?;
                Ok(Value::Unit)
            }
            Action::UploadXmp { offset, bytes } => {
                let stage = self.stage(request)?;
                ensure!(
                    !stage.ready && stage.native.is_none(),
                    "export stage already ready"
                );
                upload(stage, Artifact::Xmp, offset.0, bytes, BLOB_BYTES, false)?;
                Ok(Value::Unit)
            }
            Action::Ready { icc, xmp } => {
                let stage = self.stage(request)?;
                ensure!(
                    !stage.ready && stage.native.is_none(),
                    "export stage already ready"
                );
                let expected_icc = match &stage.work.plan.output.profile {
                    StoredProfile::Icc { blob } => Some(blob.as_str()),
                    _ => None,
                };
                finish_blob(
                    &stage.path,
                    Artifact::Icc,
                    &mut stage.icc,
                    icc.as_ref(),
                    expected_icc,
                )?;
                finish_blob(
                    &stage.path,
                    Artifact::Xmp,
                    &mut stage.xmp,
                    xmp.as_ref(),
                    stage.work.plan.xmp_blob.as_deref(),
                )?;
                stage.verify_request()?;
                stage.verify_inputs()?;
                sync_directory(&stage.path)?;
                stage.ready = true;
                Ok(Value::Ready {
                    path: native_path(&stage.path)?,
                })
            }
            Action::Arm { native } => {
                let stage = self.stage(request)?;
                ensure!(
                    stage.ready
                        && stage.native.is_none()
                        && stage.drained.is_none()
                        && stage.active_locked,
                    "export stage cannot arm"
                );
                stage.verify_request()?;
                stage.verify_inputs()?;
                FileExt::unlock(&stage.active_lock).context("open export launch gate")?;
                stage.active_locked = false;
                stage.native = Some(native.0);
                Ok(Value::Unit)
            }
            Action::NativeDrained { native, terminal } => {
                let stage = self.stage(request)?;
                ensure!(
                    stage.native == Some(native.0)
                        && stage.drained.is_none()
                        && !stage.active_locked,
                    "export native drain binding mismatch"
                );
                // A checked G drain plus reacquisition of the exact child lease
                // proves that no native process still owns this stage.
                stage
                    .active_lock
                    .try_lock_exclusive()
                    .context("export native lease remains live")?;
                stage.active_locked = true;
                stage.drained = Some(terminal.clone());
                stage.verify_request()?;
                stage.verify_inputs()?;
                stage.inputs_valid = true;
                Ok(Value::Unit)
            }
            Action::ResultAndSeal => {
                let stage = self.stage(request)?;
                ensure!(
                    matches!(stage.drained, Some(NativeTerminal::Succeeded)) && stage.inputs_valid,
                    "successful exact native drain required before sealing"
                );
                stage.verify_request()?;
                stage.verify_inputs()?;
                let completion = crate::export_worker::complete_managed_rendering(
                    &stage.work,
                    &stage.path,
                    cancel,
                    |candidate| {
                        let candidate = reply(
                            request,
                            Value::Completed {
                                path: native_path(&stage.path)?,
                                completion: candidate.into(),
                            },
                        );
                        candidate.validate(request)?;
                        super::wire::encode_outcome(&Ok(super::wire::Response::ExportStage(
                            candidate.clone(),
                        )))?;
                        crate::application::desktop::admit_export_stage_reply(&candidate)
                    },
                )?;
                Ok(Value::Completed {
                    path: native_path(&stage.path)?,
                    completion: completion.into(),
                })
            }
            Action::Abort => {
                if self.stage.is_none() {
                    return self.retry_cleanup(request);
                }
                let stage = self.stage(request)?;
                ensure!(
                    stage.native.is_none(),
                    "armed or sealing export stage cannot abort"
                );
                self.remove_stage()?;
                Ok(Value::Unit)
            }
            Action::Release => {
                if self.stage.is_none() {
                    return self.retry_cleanup(request);
                }
                let stage = self.stage(request)?;
                ensure!(stage.drained.is_some(), "export stage has not drained");
                self.remove_stage()?;
                Ok(Value::Unit)
            }
        }
    }
    fn stage(&mut self, request: &Request) -> Result<&mut Stage> {
        let stage = self.stage.as_mut().context("unknown export stage")?;
        ensure!(
            stage.id == request.stage
                && stage.binding == request.binding
                && stage.root == request.root,
            "export stage binding mismatch"
        );
        if !stage.removed {
            stage.verify()?;
        }
        ensure!(
            !stage.removing || matches!(request.action, Action::Abort | Action::Release),
            "export stage cleanup in progress"
        );
        Ok(stage)
    }
    fn remove_stage(&mut self) -> Result<()> {
        let stage = self.stage.as_mut().context("unknown export stage")?;
        stage.removing = true;
        remove_closed(
            &stage.path,
            &stage.directory,
            &stage.parent_directory,
            &mut stage.removed,
        )?;
        self.stage = None; // Drop both held locks only after complete disposal.
        Ok(())
    }
    fn retry_cleanup(&mut self, request: &Request) -> Result<Value> {
        let cleanup = self
            .cleanup
            .as_mut()
            .context("unknown export stage cleanup")?;
        ensure!(
            cleanup.id == request.stage
                && cleanup.binding == request.binding
                && cleanup.root == request.root,
            "export stage cleanup binding mismatch"
        );
        let directory = cleanup
            .directory
            .as_ref()
            .context("export stage admission directory identity was not captured")?;
        #[cfg(test)]
        admission_fault(AdmissionFault::Cleanup)?;
        remove_closed(
            &cleanup.path,
            directory,
            &cleanup.parent_directory,
            &mut cleanup.removed,
        )?;
        self.cleanup = None;
        Ok(Value::Unit)
    }
}

struct Cleanup {
    id: LeaseId,
    root: RootCapability,
    binding: Binding,
    path: PathBuf,
    // None is retained uncertainty, never evidence that mkdir had no effect.
    // This inline state is included in Owner's measured capacity layout.
    directory: Option<File>,
    parent_directory: File,
    locks: Vec<File>,
    removed: bool,
}
fn reply(request: &Request, value: Value) -> Reply {
    Reply {
        epoch: request.root.epoch.clone(),
        session: request.root.session.clone(),
        stage: request.stage.clone(),
        operation: request.operation,
        binding: request.binding.clone(),
        value,
    }
}

fn upload(
    stage: &mut Stage,
    artifact: Artifact,
    offset: u64,
    bytes: &[u8],
    limit: u64,
    icc: bool,
) -> Result<()> {
    let path = stage.artifact(artifact);
    let state = if icc { &mut stage.icc } else { &mut stage.xmp };
    if matches!(state, BlobState::Absent) {
        ensure!(offset == 0, "export stage upload offset");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        *state = BlobState::Uploading(Upload {
            file,
            offset: 0,
            hasher: blake3::Hasher::new(),
        });
    }
    let BlobState::Uploading(upload) = state else {
        anyhow::bail!("export stage upload already complete")
    };
    ensure!(upload.offset == offset, "export stage upload offset");
    let next = offset
        .checked_add(bytes.len() as u64)
        .filter(|next| *next <= limit)
        .context("export stage blob limit")?;
    upload.file.write_all(bytes)?;
    upload.hasher.update(bytes);
    upload.offset = next;
    Ok(())
}

fn finish_blob(
    stage: &Path,
    artifact: Artifact,
    state: &mut BlobState,
    declared: Option<&Blob>,
    expected: Option<&str>,
) -> Result<()> {
    match (declared, expected) {
        (None, None) => ensure!(matches!(state, BlobState::Absent), "unexpected export blob"),
        (Some(declared), Some(expected)) => {
            declared.validate()?;
            ensure!(declared.digest == expected, "export blob differs from plan");
            let BlobState::Uploading(upload) = std::mem::take(state) else {
                anyhow::bail!("required export blob is absent")
            };
            ensure!(
                upload.offset == declared.bytes.0
                    && upload.hasher.finalize().to_hex().as_str() == declared.digest,
                "export blob length/digest mismatch"
            );
            upload.file.sync_all()?;
            *state = BlobState::Complete(Uploaded {
                file: upload.file,
                bytes: declared.bytes.0,
                digest: declared.digest.clone(),
            });
            verify_blob(stage, artifact, state)?;
        }
        _ => anyhow::bail!("export blob declaration differs from plan"),
    }
    Ok(())
}

fn verify_blob(stage: &Path, artifact: Artifact, state: &BlobState) -> Result<()> {
    match state {
        BlobState::Absent => ensure!(
            matches!(fs::symlink_metadata(stage.join(artifact.name())), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
            "unexpected export blob"
        ),
        BlobState::Uploading(_) => anyhow::bail!("export blob upload incomplete"),
        BlobState::Complete(upload) => {
            let path = stage.join(artifact.name());
            let mut file = checked_file(&path, Some(&upload.file))?;
            ensure!(
                file.metadata()?.len() == upload.bytes,
                "export blob length changed"
            );
            let before = crate::metadata_export::content_change_stamp(&file)?;
            let mut scratch = vec![0; CHUNK_BYTES];
            let mut digest = blake3::Hasher::new();
            let mut remaining = upload.bytes;
            while remaining > 0 {
                let count = usize::try_from(remaining.min(CHUNK_BYTES as u64))?;
                file.read_exact(&mut scratch[..count])?;
                digest.update(&scratch[..count]);
                remaining -= count as u64;
            }
            let mut extra = [0];
            ensure!(
                file.read(&mut extra)? == 0
                    && file.metadata()?.len() == upload.bytes
                    && crate::metadata_export::content_change_stamp(&file)? == before
                    && digest.finalize().to_hex().as_str() == upload.digest,
                "export blob changed"
            );
            verify_file(&path, &file)?;
        }
    }
    Ok(())
}

fn create(path: &Path, bytes: &[u8]) -> Result<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(file)
}
fn checked_file(path: &Path, held: Option<&File>) -> Result<File> {
    let current = crate::metadata_export::open_regular(path)?;
    if let Some(held) = held {
        ensure!(
            crate::catalog_storage::physical_object_id(&current)?
                == crate::catalog_storage::physical_object_id(held)?,
            "export stage artifact changed identity"
        );
    }
    Ok(current)
}
fn verify_file(path: &Path, held: &File) -> Result<()> {
    checked_file(path, Some(held))?;
    Ok(())
}
fn bounded_read(path: &Path, held: Option<&File>, limit: u64) -> Result<Vec<u8>> {
    let mut file = checked_file(path, held)?;
    let metadata = file.metadata()?;
    ensure!(metadata.len() <= limit, "export stage artifact bound");
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(usize::try_from(metadata.len())?)?;
    bytes.resize(usize::try_from(metadata.len())?, 0);
    file.read_exact(&mut bytes)?;
    let mut extra = [0];
    ensure!(file.read(&mut extra)? == 0, "export stage artifact grew");
    verify_file(path, &file)?;
    Ok(bytes)
}
fn directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "export stage root type"
    );
    Ok(())
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn native_path(path: &Path) -> Result<NativePath> {
    let value = NativePath::from_path(path);
    crate::catalog_session::validate_path(&value)?;
    Ok(value)
}
fn owned_directory(path: &Path) -> Result<File> {
    #[cfg(unix)]
    {
        super::bootstrap::open_directory(path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let file = OpenOptions::new()
            .read(true)
            .access_mode(0x80000000 | 0x10000)
            .share_mode(1 | 2)
            .custom_flags(0x02000000 | 0x00200000)
            .open(path)?;
        ensure!(
            file.metadata()?.is_dir() && !fs::symlink_metadata(path)?.file_type().is_symlink(),
            "export directory type"
        );
        Ok(file)
    }
}
#[cfg(windows)]
fn delete_held(file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            class: i32,
            info: *const std::ffi::c_void,
            bytes: u32,
        ) -> i32;
    }
    // FILE_DISPOSITION_INFO_EX with POSIX semantics removes the name when
    // this deletion handle closes, while retained lock/input handles live on.
    // Unsupported filesystems fail closed with cleanup custody still retained.
    let delete: u32 = 1 | 2;
    ensure!(
        unsafe {
            SetFileInformationByHandle(file.as_raw_handle(), 21, (&delete as *const u32).cast(), 4)
        } != 0,
        "export held deletion failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
fn verify_directory(path: &Path, directory: &File, parent: &File) -> Result<()> {
    ensure!(
        path.canonicalize()? == path,
        "export stage link ancestry changed"
    );
    let current = super::bootstrap::open_directory(path)?;
    let current_parent =
        super::bootstrap::open_directory(path.parent().context("export stage parent")?)?;
    ensure!(
        crate::catalog_storage::physical_object_id(&current)?
            == crate::catalog_storage::physical_object_id(directory)?
            && crate::catalog_storage::physical_object_id(&current_parent)?
                == crate::catalog_storage::physical_object_id(parent)?,
        "export stage directory moved or replaced"
    );
    Ok(())
}
fn remove_closed(path: &Path, directory: &File, parent: &File, removed: &mut bool) -> Result<()> {
    let parent_path = path.parent().context("export stage parent")?;
    if !*removed {
        verify_directory(path, directory, parent)?;
        // Inspect the whole closed vocabulary before any deletion. Retain the
        // stage handles and locks if unknown or nonordinary artifacts appear.
        let mut names = Vec::new();
        for entry in fs::read_dir(path)?.take(Artifact::ALL.len() + 1) {
            let entry = entry?;
            let name = entry.file_name();
            ensure!(
                Artifact::ALL.iter().any(|artifact| name == artifact.name()),
                "unknown export stage artifact retained"
            );
            checked_file(&path.join(&name), None)?;
            names.push(name);
        }
        ensure!(
            names.len() <= Artifact::ALL.len(),
            "export stage artifact count"
        );
        for name in names {
            verify_directory(path, directory, parent)?;
            // Unix deletion is relative to the retained directory descriptor:
            // a concurrent pathname swap cannot redirect deletion to a victim.
            #[cfg(unix)]
            {
                use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
                let name = std::ffi::CString::new(name.as_bytes())?;
                ensure!(
                    unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == 0,
                    "export artifact deletion failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                let target = OpenOptions::new()
                    .read(true)
                    .access_mode(0x80000000 | 0x10000)
                    .share_mode(1 | 2 | 4)
                    .custom_flags(0x00200000)
                    .open(path.join(&name))?;
                ensure!(
                    target.metadata()?.is_file(),
                    "export cleanup artifact type changed"
                );
                verify_directory(path, directory, parent)?;
                delete_held(&target)?;
            }
        }
        verify_directory(path, directory, parent)?;
        #[cfg(unix)]
        {
            use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
            let name =
                std::ffi::CString::new(path.file_name().context("export stage name")?.as_bytes())?;
            ensure!(
                unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                    == 0,
                "export directory deletion failed: {}",
                std::io::Error::last_os_error()
            );
        }
        #[cfg(windows)]
        delete_held(directory)?;
        *removed = true;
    }
    #[cfg(test)]
    admission_fault(AdmissionFault::CleanupSync)?;
    // A failed durability barrier retries on the retained parent, never on a
    // newly created path. The closed stage no longer names any mutable files.
    #[cfg(unix)]
    parent.sync_all()?;
    #[cfg(not(unix))]
    sync_directory(parent_path)?;
    let _ = parent_path;
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdmissionFault {
    DirectoryOpen,
    DirectoryClone,
    ParentClone,
    Cleanup,
    CleanupSync,
}
#[cfg(test)]
thread_local! {
    static ADMISSION_FAULTS: std::cell::RefCell<Vec<AdmissionFault>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}
#[cfg(test)]
fn admission_fault(point: AdmissionFault) -> Result<()> {
    ADMISSION_FAULTS.with(|faults| {
        let mut faults = faults.borrow_mut();
        if faults.first() == Some(&point) {
            faults.remove(0);
            anyhow::bail!("injected export admission {point:?}");
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::U64,
        catalog_edits::{EditRenderIdentity, VariantKey},
        catalog_exports::{MetadataSelection, PhotoExportPlan, StoredOutput, StoredProfile},
        catalog_images::ImageMetadataIdentity,
        catalog_metadata::RenderIdentity,
        catalog_session::RootCapability,
        edit::{Recipe, RenderLimits},
        image_export::{
            AlphaPolicy, EncodeLimits, EncodingReport, IntegerDepth, OutputDescriptor,
            OutputFormat, OutputSize,
        },
        media::DecodeLimits,
        metadata_export::{DestinationSnapshot, FileRevision},
        photo_render::{PhotoRenderLimits, PhotoRenderTimings, StagedPhoto},
        storage_volume::NativePath,
    };

    struct Fixture {
        _temp: tempfile::TempDir,
        manifest: PathBuf,
        root: RootCapability,
    }
    fn fixture() -> Result<Fixture> {
        let temp = tempfile::tempdir()?;
        let base = temp.path().canonicalize()?;
        let root_path = base.join("catalog");
        fs::create_dir(&root_path)?;
        let catalog_path = root_path.join("catalog.sqlite3");
        create(&catalog_path, b"sqlite")?;
        let manifest = base.join("manifest");
        fs::create_dir(&manifest)?;
        let root_file = super::super::bootstrap::open_directory(&root_path)?;
        let catalog_file = File::open(&catalog_path)?;
        Ok(Fixture {
            _temp: temp,
            manifest,
            root: RootCapability {
                epoch: LeaseId::new(),
                token: LeaseId::new(),
                session: LeaseId::new(),
                canonical_root: NativePath::from_path(&root_path),
                root_physical: crate::catalog_storage::physical_object_id(&root_file)?,
                catalog_physical: crate::catalog_storage::physical_object_id(&catalog_file)?,
            },
        })
    }
    fn work(root: &Path, icc: Option<&[u8]>, xmp: Option<&[u8]>) -> Result<ExportWork> {
        let root = root.canonicalize()?;
        let recipe = Recipe::default();
        let fingerprint = "a".repeat(64);
        let profile = match icc {
            Some(bytes) => StoredProfile::Icc {
                blob: blake3::hash(bytes).to_hex().to_string(),
            },
            None => StoredProfile::Srgb,
        };
        let plan = PhotoExportPlan {
            version: 3,
            renderer_identity: crate::photo_render::output_renderer_identity().into(),
            identity: EditRenderIdentity {
                image_identity: Some(ImageMetadataIdentity {
                    image_id: "asset".into(),
                    key: VariantKey::master("asset"),
                    metadata_revision: 0,
                    pixel_generation: 1,
                    shared_source_epoch: 0,
                    physical_generation: 1,
                }),
                source: RenderIdentity {
                    asset_id: "asset".into(),
                    generation: 1,
                    fingerprint: Some(fingerprint.clone()),
                    state: "ready".into(),
                    metadata_revision: 0,
                },
                key: VariantKey::master("asset"),
                revision: 1,
                recipe_digest: recipe.validate()?.digest().into(),
            },
            original: NativePath::from_path(&root.join("original.raw")),
            original_revision: FileRevision {
                bytes: 64,
                digest: fingerprint,
                modified_ns: 1,
                identity: (1, 2),
            },
            recipe,
            output: StoredOutput {
                size: OutputSize::Original,
                format: OutputFormat::Png {
                    depth: IntegerDepth::Eight,
                },
                profile,
                alpha: AlphaPolicy::Preserve,
            },
            metadata: MetadataSelection::Omit,
            xmp_blob: xmp.map(|bytes| blake3::hash(bytes).to_hex().to_string()),
            destination: DestinationSnapshot {
                version: 2,
                operation: uuid::Uuid::new_v4().to_string(),
                destination: root.join("destination.png"),
                expected: None,
                max_existing_bytes: 1024,
            },
            max_original_bytes: 1024,
            max_payload_bytes: 1024,
            alias_limits: Default::default(),
        };
        let raw = serde_json::to_string(&plan)?;
        let authority = blake3::hash(raw.as_bytes()).to_hex().to_string();
        Ok(ExportWork {
            job: uuid::Uuid::new_v4().to_string(),
            sequence: 1,
            attempt: uuid::Uuid::new_v4().to_string(),
            authority: authority.clone(),
            plan: crate::catalog_exports::checked_plan(&raw, &authority)?,
        })
    }
    fn limits() -> PhotoRenderLimits {
        PhotoRenderLimits {
            decode: DecodeLimits {
                max_encoded_bytes: 1024,
                ..DecodeLimits::default()
            },
            render: RenderLimits::default(),
            encode: EncodeLimits::default(),
            max_encoded_extent: 1024,
        }
    }
    fn request(
        root: &RootCapability,
        stage: &LeaseId,
        operation: u64,
        supervisor: bool,
        binding: &Binding,
        action: Action,
    ) -> Request {
        Request {
            root: root.clone(),
            stage: stage.clone(),
            operation: U64(operation),
            supervisor,
            binding: binding.clone(),
            action,
        }
    }
    fn upload_blob(
        owner: &mut Owner,
        fixture: &Fixture,
        stage: &LeaseId,
        binding: &Binding,
        mut operation: u64,
        bytes: &[u8],
        icc: bool,
    ) -> Result<u64> {
        for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
            let action = if icc {
                Action::UploadIcc {
                    offset: U64((index * CHUNK_BYTES) as u64),
                    bytes: chunk.to_vec(),
                }
            } else {
                Action::UploadXmp {
                    offset: U64((index * CHUNK_BYTES) as u64),
                    bytes: chunk.to_vec(),
                }
            };
            owner.call(
                &fixture.manifest,
                &request(&fixture.root, stage, operation, false, binding, action),
                &AtomicBool::new(false),
            )?;
            operation += 1;
        }
        Ok(operation)
    }
    fn begin(
        owner: &mut Owner,
        fixture: &Fixture,
        work: &ExportWork,
    ) -> Result<(LeaseId, Binding)> {
        let stage = LeaseId::new();
        let binding = Binding::from_work(work);
        owner.call(
            &fixture.manifest,
            &request(
                &fixture.root,
                &stage,
                1,
                false,
                &binding,
                Action::Begin {
                    work: Box::new(work.clone()),
                    limits: limits(),
                },
            ),
            &AtomicBool::new(false),
        )?;
        Ok((stage, binding))
    }

    struct InjectAdmissionFaults;
    impl Drop for InjectAdmissionFaults {
        fn drop(&mut self) {
            ADMISSION_FAULTS.with(|faults| faults.borrow_mut().clear());
        }
    }
    fn inject_admission_faults(points: &[AdmissionFault]) -> InjectAdmissionFaults {
        ADMISSION_FAULTS.with(|faults| {
            assert!(faults.borrow().is_empty());
            faults.borrow_mut().extend_from_slice(points);
        });
        InjectAdmissionFaults
    }
    fn admission_request(f: &Fixture) -> Result<Request> {
        let work = work(f._temp.path(), None, None)?;
        Ok(request(
            &f.root,
            &LeaseId::new(),
            1,
            false,
            &Binding::from_work(&work),
            Action::Begin {
                work: Box::new(work),
                limits: limits(),
            },
        ))
    }
    fn admission_failure(owner: &mut Owner, f: &Fixture, request: &Request) -> Result<Failure> {
        let failure = owner
            .call(&f.manifest, request, &AtomicBool::new(false))
            .unwrap_err()
            .downcast::<Failure>()?;
        let receipt = failure.object_receipt.as_ref().context("failure receipt")?;
        assert_eq!(receipt.operation, request.operation);
        assert_eq!(receipt.request_digest, request.digest()?);
        Ok(failure)
    }
    fn admission_cleanup(begin: &Request, operation: u64, action: Action) -> Request {
        Request {
            operation: U64(operation),
            action,
            ..begin.clone()
        }
    }

    #[test]
    fn admission_handle_clone_failures_reject_only_after_verified_cleanup() -> Result<()> {
        for point in [AdmissionFault::DirectoryClone, AdmissionFault::ParentClone] {
            let f = fixture()?;
            let begin = admission_request(&f)?;
            let path = f
                .manifest
                .join("export-workers")
                .join(format!("photo-worker-{}", begin.stage.as_str()));
            let mut owner = Owner::default();
            let _faults = inject_admission_faults(&[point]);
            let failed = admission_failure(&mut owner, &f, &begin)?;
            assert_eq!(failed.kind, FailureKind::Rejected);
            assert!(owner.empty());
            assert!(!path.exists());
            let replay = admission_failure(&mut owner, &f, &begin)?;
            assert_eq!(serde_json::to_vec(&failed)?, serde_json::to_vec(&replay)?);
            assert!(!path.exists(), "replay must not repeat admission");
            let mut next = begin.clone();
            next.stage = LeaseId::new();
            owner.call(&f.manifest, &next, &AtomicBool::new(false))?;
            owner.call(
                &f.manifest,
                &admission_cleanup(&next, 2, Action::Abort),
                &AtomicBool::new(true),
            )?;
            assert!(owner.empty());
        }
        Ok(())
    }

    #[test]
    fn admission_uncaptured_directory_identity_stays_unknown_without_path_adoption() -> Result<()> {
        let f = fixture()?;
        let begin = admission_request(&f)?;
        let mut owner = Owner::default();
        let _faults = inject_admission_faults(&[AdmissionFault::DirectoryOpen]);
        let failed = admission_failure(&mut owner, &f, &begin)?;
        assert_eq!(failed.kind, FailureKind::Unknown);
        assert!(!owner.empty());
        let cleanup = owner.cleanup.as_ref().unwrap();
        assert!(cleanup.directory.is_none());
        let path = cleanup.path.clone();
        assert!(
            path.is_dir(),
            "mkdir succeeded before directory acquisition failed"
        );
        let replay = admission_failure(&mut owner, &f, &begin)?;
        assert_eq!(serde_json::to_vec(&failed)?, serde_json::to_vec(&replay)?);
        let mut foreign = begin.clone();
        foreign.root.token = LeaseId::new();
        assert!(
            owner
                .call(&f.manifest, &foreign, &AtomicBool::new(false))
                .is_err()
        );
        let mut altered = begin.clone();
        altered.action = Action::Abort;
        assert!(
            owner
                .call(&f.manifest, &altered, &AtomicBool::new(false))
                .is_err()
        );
        // Even an empty or absent pathname does not identify the mkdir result.
        fs::remove_dir(&path)?;
        let abort = admission_cleanup(&begin, 2, Action::Abort);
        assert_eq!(
            admission_failure(&mut owner, &f, &abort)?.kind,
            FailureKind::Unknown
        );
        assert!(!owner.empty());
        fs::create_dir(&path)?;
        create(&path.join("output"), b"foreign")?;
        let release = admission_cleanup(&begin, 3, Action::Release);
        assert_eq!(
            admission_failure(&mut owner, &f, &release)?.kind,
            FailureKind::Unknown
        );
        assert_eq!(fs::read(path.join("output"))?, b"foreign");
        assert!(!owner.empty());
        let retry = admission_cleanup(&begin, 4, begin.action.clone());
        assert_eq!(
            admission_failure(&mut owner, &f, &retry)?.kind,
            FailureKind::Unknown
        );
        assert_eq!(fs::read(path.join("output"))?, b"foreign");
        Ok(())
    }

    #[test]
    fn admission_cleanup_failure_retains_exact_owner_for_abort_and_release_retry() -> Result<()> {
        for (point, action) in [
            (AdmissionFault::DirectoryClone, Action::Abort),
            (AdmissionFault::ParentClone, Action::Release),
        ] {
            let f = fixture()?;
            let begin = admission_request(&f)?;
            let mut owner = Owner::default();
            let _faults = inject_admission_faults(&[point, AdmissionFault::Cleanup]);
            let failed = admission_failure(&mut owner, &f, &begin)?;
            assert_eq!(failed.kind, FailureKind::Unknown);
            assert!(!owner.empty());
            let cleanup = owner.cleanup.as_ref().unwrap();
            let path = cleanup.path.clone();
            verify_directory(
                &path,
                cleanup.directory.as_ref().unwrap(),
                &cleanup.parent_directory,
            )?;
            let replay = admission_failure(&mut owner, &f, &begin)?;
            assert_eq!(serde_json::to_vec(&failed)?, serde_json::to_vec(&replay)?);
            assert!(path.is_dir());
            let cleanup_request = admission_cleanup(&begin, 2, action);
            let cleaned = owner.call(&f.manifest, &cleanup_request, &AtomicBool::new(true))?;
            assert!(owner.empty());
            assert!(!path.exists());
            let replay = owner.call(&f.manifest, &cleanup_request, &AtomicBool::new(true))?;
            assert_eq!(serde_json::to_vec(&cleaned)?, serde_json::to_vec(&replay)?);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn admission_captured_identity_refuses_replacement_and_recovers_only_original() -> Result<()> {
        let f = fixture()?;
        let begin = admission_request(&f)?;
        let mut owner = Owner::default();
        let _faults =
            inject_admission_faults(&[AdmissionFault::ParentClone, AdmissionFault::Cleanup]);
        assert_eq!(
            admission_failure(&mut owner, &f, &begin)?.kind,
            FailureKind::Unknown
        );
        let path = owner.cleanup.as_ref().unwrap().path.clone();
        let moved = path.with_extension("owned");
        fs::rename(&path, &moved)?;
        fs::create_dir(&path)?;
        create(&path.join("output"), b"replacement")?;
        let abort = admission_cleanup(&begin, 2, Action::Abort);
        assert_eq!(
            admission_failure(&mut owner, &f, &abort)?.kind,
            FailureKind::Unknown
        );
        assert_eq!(fs::read(path.join("output"))?, b"replacement");
        assert!(moved.is_dir());
        assert!(!owner.empty());
        fs::remove_file(path.join("output"))?;
        fs::remove_dir(&path)?;
        let victim = f._temp.path().join("victim");
        fs::create_dir(&victim)?;
        create(&victim.join("output"), b"victim")?;
        std::os::unix::fs::symlink(&victim, &path)?;
        let release = admission_cleanup(&begin, 3, Action::Release);
        assert_eq!(
            admission_failure(&mut owner, &f, &release)?.kind,
            FailureKind::Unknown
        );
        assert_eq!(fs::read(victim.join("output"))?, b"victim");
        fs::remove_file(&path)?;
        fs::rename(&moved, &path)?;
        owner.call(
            &f.manifest,
            &admission_cleanup(&begin, 4, Action::Abort),
            &AtomicBool::new(true),
        )?;
        assert!(owner.empty());
        assert!(!path.exists());
        assert_eq!(fs::read(victim.join("output"))?, b"victim");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn admission_cleanup_sync_retry_never_touches_a_reused_path() -> Result<()> {
        let f = fixture()?;
        let begin = admission_request(&f)?;
        let mut owner = Owner::default();
        let _faults =
            inject_admission_faults(&[AdmissionFault::DirectoryClone, AdmissionFault::CleanupSync]);
        let failed = admission_failure(&mut owner, &f, &begin)?;
        assert_eq!(failed.kind, FailureKind::Unknown);
        let cleanup = owner.cleanup.as_ref().unwrap();
        assert!(cleanup.removed);
        let path = cleanup.path.clone();
        assert!(!path.exists());
        assert!(!owner.empty());
        fs::create_dir(&path)?;
        create(&path.join("output"), b"replacement after removal")?;
        let replay = admission_failure(&mut owner, &f, &begin)?;
        assert_eq!(serde_json::to_vec(&failed)?, serde_json::to_vec(&replay)?);
        owner.call(
            &f.manifest,
            &admission_cleanup(&begin, 2, Action::Release),
            &AtomicBool::new(true),
        )?;
        assert!(owner.empty());
        assert_eq!(fs::read(path.join("output"))?, b"replacement after removal");
        Ok(())
    }

    #[test]
    fn admission_uncertainty_blocks_actual_root_release_until_exact_cleanup() -> Result<()> {
        use super::super::bootstrap::BootstrapOwner;
        use crate::catalog_session::{
            BootstrapMode, ConfirmSqlAdmission, PrepareCatalog, SQL_ROLES, SqlRole,
            SqlRoleObservation,
        };
        for point in [AdmissionFault::DirectoryOpen, AdmissionFault::ParentClone] {
            let temp = tempfile::tempdir()?;
            let base = temp.path().canonicalize()?;
            let mut root_owner = BootstrapOwner::new(LeaseId::new(), vec![]);
            let cancel = AtomicBool::new(false);
            let bootstrap = root_owner.prepare(
                &PrepareCatalog {
                    operation: U64(1),
                    session: LeaseId::new(),
                    mode: BootstrapMode::DesktopCreate,
                    root: NativePath::from_path(&base.join("catalog")),
                    manifest_root: NativePath::from_path(&base.join("manifest")),
                    import_source: None,
                },
                &cancel,
                |_| Ok(()),
            )?;
            let root = bootstrap.root_capability();
            root_owner.confirm(
                &ConfirmSqlAdmission {
                    operation: bootstrap.operation,
                    root: root.clone(),
                    roles: SQL_ROLES.map(|role| SqlRoleObservation {
                        role,
                        physical: if role == SqlRole::Manifest {
                            bootstrap.manifest.physical
                        } else {
                            bootstrap.catalog.physical
                        },
                    }),
                },
                &cancel,
            )?;
            let w = work(&base, None, None)?;
            let begin = request(
                &root,
                &LeaseId::new(),
                1,
                false,
                &Binding::from_work(&w),
                Action::Begin {
                    work: Box::new(w),
                    limits: limits(),
                },
            );
            let points = if point == AdmissionFault::DirectoryOpen {
                vec![point]
            } else {
                vec![point, AdmissionFault::Cleanup]
            };
            let _faults = inject_admission_faults(&points);
            let failed = root_owner
                .export_stage_call(&begin, &cancel)
                .unwrap_err()
                .downcast::<Failure>()?;
            assert_eq!(failed.kind, FailureKind::Unknown);
            assert!(
                root_owner
                    .release(&root)
                    .unwrap_err()
                    .to_string()
                    .contains("export stage/native/seal owner has not drained")
            );
            let cleanup = admission_cleanup(&begin, 2, Action::Abort);
            if point == AdmissionFault::DirectoryOpen {
                let failed = root_owner
                    .export_stage_call(&cleanup, &cancel)
                    .unwrap_err()
                    .downcast::<Failure>()?;
                assert_eq!(failed.kind, FailureKind::Unknown);
                assert!(root_owner.release(&root).is_err());
            } else {
                root_owner.export_stage_call(&cleanup, &cancel)?;
                root_owner.release(&root)?;
            }
        }
        Ok(())
    }

    #[test]
    fn bounded_uploads_ready_and_active_lock_handoff_preserve_exact_custody() -> Result<()> {
        let fixture = fixture()?;
        let icc = vec![0x5a; CHUNK_BYTES + 7];
        let xmp = vec![0xa5; CHUNK_BYTES * 2 + 3];
        let work = work(fixture._temp.path(), Some(&icc), Some(&xmp))?;
        let mut owner = Owner::default();
        let (stage, binding) = begin(&mut owner, &fixture, &work)?;
        let mut operation = upload_blob(&mut owner, &fixture, &stage, &binding, 2, &icc, true)?;
        operation = upload_blob(
            &mut owner, &fixture, &stage, &binding, operation, &xmp, false,
        )?;
        let ready = owner.call(
            &fixture.manifest,
            &request(
                &fixture.root,
                &stage,
                operation,
                false,
                &binding,
                Action::Ready {
                    icc: Some(Blob {
                        bytes: U64(icc.len() as u64),
                        digest: blake3::hash(&icc).to_hex().to_string(),
                    }),
                    xmp: Some(Blob {
                        bytes: U64(xmp.len() as u64),
                        digest: blake3::hash(&xmp).to_hex().to_string(),
                    }),
                },
            ),
            &AtomicBool::new(false),
        )?;
        let Value::Ready { path } = ready.value else {
            anyhow::bail!("ready reply")
        };
        let path = path.to_path()?;
        assert_eq!(fs::read(path.join("profile.icc"))?, icc);
        assert_eq!(fs::read(path.join("selected.xmp"))?, xmp);
        let launch = File::open(path.join("active.lock"))?;
        assert!(launch.try_lock_exclusive().is_err());
        let forged = request(
            &fixture.root,
            &stage,
            99,
            false,
            &binding,
            Action::Arm { native: U64(7) },
        );
        assert!(forged.validate().is_err());
        owner.call(
            &fixture.manifest,
            &request(
                &fixture.root,
                &stage,
                1,
                true,
                &binding,
                Action::Arm { native: U64(7) },
            ),
            &AtomicBool::new(false),
        )?;
        launch.try_lock_exclusive()?;
        FileExt::unlock(&launch)?;
        owner.call(
            &fixture.manifest,
            &request(
                &fixture.root,
                &stage,
                2,
                true,
                &binding,
                Action::NativeDrained {
                    native: U64(7),
                    terminal: NativeTerminal::Failed { code: Some(74) },
                },
            ),
            &AtomicBool::new(false),
        )?;
        assert!(
            owner
                .call(
                    &fixture.manifest,
                    &request(
                        &fixture.root,
                        &stage,
                        operation + 1,
                        false,
                        &binding,
                        Action::ResultAndSeal,
                    ),
                    &AtomicBool::new(false),
                )
                .is_err()
        );
        owner.call(
            &fixture.manifest,
            &request(
                &fixture.root,
                &stage,
                operation + 2,
                false,
                &binding,
                Action::Release,
            ),
            &AtomicBool::new(false),
        )?;
        assert!(owner.empty());
        assert!(!path.exists());
        Ok(())
    }

    pub(super) fn facts(
        work: &ExportWork,
        output: &Path,
    ) -> Result<crate::export_worker::ExportRenderingFacts> {
        let revision = crate::metadata_export::inspect_file_revision(output, 1024)?;
        Ok(crate::export_worker::ExportRenderingFacts {
            job: work.job.clone(),
            sequence: work.sequence,
            authority: work.authority.clone(),
            attempt: work.attempt.clone(),
            output: work.plan.output.clone(),
            output_revision: revision.clone(),
            rendered: StagedPhoto {
                staging: output.to_owned(),
                encoding: EncodingReport {
                    output: OutputDescriptor {
                        width: 1,
                        height: 1,
                        channels: 4,
                        bits_per_sample: 8,
                        floating_point: false,
                        orientation: 1,
                        icc_blake3: "fixture".into(),
                        integer_clips_to_unit_range: true,
                        alpha: AlphaPolicy::Preserve,
                    },
                    encoded_extent: revision.bytes,
                    source_fingerprint: work.plan.original_revision.digest.clone(),
                    recipe_digest: work.plan.identity.recipe_digest.clone(),
                    metadata_blake3: "fixture".into(),
                    compression: "fixture".into(),
                },
                renderer_identity: work.plan.renderer_identity.clone(),
                metadata_notes: Vec::new(),
                timings: PhotoRenderTimings {
                    source_verification_before_ms: 0.,
                    staging_setup_ms: 0.,
                    decode_ms: 0.,
                    recipe_ms: 0.,
                    metadata_ms: 0.,
                    encode_ms: 0.,
                    source_verification_after_ms: 0.,
                    sync_ms: 0.,
                    total_ms: 0.,
                },
            },
            peak_resident_bytes: None,
            peak_method: "fixture".into(),
        })
    }
    fn render_and_drain(
        owner: &mut Owner,
        fixture: &Fixture,
        stage: &LeaseId,
        binding: &Binding,
        work: &ExportWork,
        output_bytes: &[u8],
    ) -> Result<PathBuf> {
        let ready = owner.call(
            &fixture.manifest,
            &request(
                &fixture.root,
                stage,
                2,
                false,
                binding,
                Action::Ready {
                    icc: None,
                    xmp: None,
                },
            ),
            &AtomicBool::new(false),
        )?;
        let Value::Ready { path } = ready.value else {
            anyhow::bail!("ready reply")
        };
        let path = path.to_path()?;
        owner.call(
            &fixture.manifest,
            &request(
                &fixture.root,
                stage,
                1,
                true,
                binding,
                Action::Arm { native: U64(11) },
            ),
            &AtomicBool::new(false),
        )?;
        let native = File::open(path.join("active.lock"))?;
        native.try_lock_exclusive()?;
        create(&path.join("output"), output_bytes)?;
        create(
            &path.join("result.json"),
            &serde_json::to_vec(&facts(work, &path.join("output"))?)?,
        )?;
        FileExt::unlock(&native)?;
        owner.call(
            &fixture.manifest,
            &request(
                &fixture.root,
                stage,
                2,
                true,
                binding,
                Action::NativeDrained {
                    native: U64(11),
                    terminal: NativeTerminal::Succeeded,
                },
            ),
            &AtomicBool::new(false),
        )?;
        Ok(path)
    }

    #[test]
    fn admitted_seal_replays_and_fresh_orphan_requires_exact_bytes() -> Result<()> {
        let fixture = fixture()?;
        let work = work(fixture._temp.path(), None, None)?;
        let mut owner = Owner::default();
        let (stage, binding) = begin(&mut owner, &fixture, &work)?;
        let path = render_and_drain(
            &mut owner,
            &fixture,
            &stage,
            &binding,
            &work,
            b"same-render",
        )?;
        let seal = request(
            &fixture.root,
            &stage,
            3,
            false,
            &binding,
            Action::ResultAndSeal,
        );
        let first = owner.call(&fixture.manifest, &seal, &AtomicBool::new(false))?;
        let replay = owner.call(&fixture.manifest, &seal, &AtomicBool::new(true))?;
        assert_eq!(serde_json::to_vec(&first)?, serde_json::to_vec(&replay)?);
        let recovery = work
            .plan
            .destination
            .destination
            .parent()
            .unwrap()
            .join(format!(
                ".photocatalog-photo-export-{}",
                work.plan.destination.operation
            ));
        assert!(recovery.join("photo-seal.json").is_file());
        owner.call(
            &fixture.manifest,
            &request(&fixture.root, &stage, 4, false, &binding, Action::Release),
            &AtomicBool::new(false),
        )?;
        assert!(!path.exists());

        let (retry_stage, retry_binding) = begin(&mut owner, &fixture, &work)?;
        render_and_drain(
            &mut owner,
            &fixture,
            &retry_stage,
            &retry_binding,
            &work,
            b"different-render",
        )?;
        let mismatch = owner
            .call(
                &fixture.manifest,
                &request(
                    &fixture.root,
                    &retry_stage,
                    3,
                    false,
                    &retry_binding,
                    Action::ResultAndSeal,
                ),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert_eq!(
            mismatch.downcast_ref::<Failure>().unwrap().kind,
            FailureKind::Unknown
        );
        assert!(!owner.empty());
        owner.call(
            &fixture.manifest,
            &request(
                &fixture.root,
                &retry_stage,
                4,
                false,
                &retry_binding,
                Action::Release,
            ),
            &AtomicBool::new(false),
        )?;
        assert!(recovery.join("photo-seal.json").is_file());
        Ok(())
    }

    #[test]
    fn full_envelopes_and_stage_limits_fail_before_effects() -> Result<()> {
        let fixture = fixture()?;
        let work = work(fixture._temp.path(), None, None)?;
        let stage = LeaseId::new();
        let binding = Binding::from_work(&work);
        let oversized_chunk = request(
            &fixture.root,
            &stage,
            2,
            false,
            &binding,
            Action::UploadXmp {
                offset: U64(0),
                bytes: vec![0; CHUNK_BYTES + 1],
            },
        );
        assert!(oversized_chunk.validate().is_err());
        let oversized_blob = request(
            &fixture.root,
            &stage,
            3,
            false,
            &binding,
            Action::Ready {
                icc: None,
                xmp: Some(Blob {
                    bytes: U64(BLOB_BYTES + 1),
                    digest: "a".repeat(64),
                }),
            },
        );
        assert!(oversized_blob.validate().is_err());
        let mut owner = Owner::default();
        let canceled = AtomicBool::new(true);
        assert!(
            owner
                .call(
                    &fixture.manifest,
                    &request(
                        &fixture.root,
                        &stage,
                        1,
                        false,
                        &binding,
                        Action::Begin {
                            work: Box::new(work),
                            limits: limits(),
                        },
                    ),
                    &canceled,
                )
                .is_err()
        );
        assert!(owner.empty());
        assert!(!fixture.manifest.join("export-workers").exists());
        Ok(())
    }
    fn relay(request: &Request) -> Result<Request> {
        let through_c = if request.privileged() {
            request.clone()
        } else {
            crate::application::desktop::roundtrip_export_stage(request)?
        };
        let operation = super::super::wire::Operation::ExportStage(through_c);
        let encoded = super::super::wire::encode_operation(&operation)?;
        let super::super::wire::Operation::ExportStage(decoded) =
            super::super::wire::decode_operation(&encoded)?
        else {
            anyhow::bail!("export F operation round trip");
        };
        Ok(decoded)
    }
    #[test]
    fn packet_and_f_operation_preserve_every_upload_byte_at_chunk_boundaries() -> Result<()> {
        let f = fixture()?;
        let w = work(f._temp.path(), None, None)?;
        let binding = Binding::from_work(&w);
        for size in [1, CHUNK_BYTES - 1, CHUNK_BYTES] {
            let bytes: Vec<u8> = (0..size).map(|n| (n % 251) as u8).collect();
            for icc in [true, false] {
                let action = if icc {
                    Action::UploadIcc {
                        offset: U64(BLOB_BYTES - size as u64),
                        bytes: bytes.clone(),
                    }
                } else {
                    Action::UploadXmp {
                        offset: U64(BLOB_BYTES - size as u64),
                        bytes: bytes.clone(),
                    }
                };
                let r = request(&f.root, &LeaseId::new(), 2, false, &binding, action);
                let decoded = relay(&r)?;
                assert_eq!(decoded.binary(), bytes);
                assert_eq!(decoded.digest()?, r.digest()?);
            }
        }
        Ok(())
    }
    #[test]
    fn exact_plan_whitespace_escapes_and_maximum_raw_plan_survive_both_envelopes() -> Result<()> {
        let f = fixture()?;
        let mut w = work(f._temp.path(), None, None)?;
        let raw = w.plan.raw().to_owned();
        let escaped = raw.replace("asset", "\\u0061sset");
        let maximum = format!("{}{}", "\n".repeat(PLAN_BYTES - escaped.len()), escaped);
        for raw in [format!(" \n{}\t ", raw), escaped, maximum] {
            w.authority = blake3::hash(raw.as_bytes()).to_hex().to_string();
            w.plan = crate::catalog_exports::checked_plan(&raw, &w.authority)?;
            let r = request(
                &f.root,
                &LeaseId::new(),
                1,
                false,
                &Binding::from_work(&w),
                Action::Begin {
                    work: Box::new(w.clone()),
                    limits: limits(),
                },
            );
            let decoded = relay(&r)?;
            let Action::Begin { work, limits } = decoded.action else {
                anyhow::bail!("begin");
            };
            assert_eq!(work.plan.raw().as_bytes(), raw.as_bytes());
            assert_eq!(work.authority, w.authority);
            let native = crate::export_worker::prepare_managed_request(
                &work,
                limits,
                &f.manifest.join("photo-worker-path"),
            )?;
            assert!(native.len() <= REQUEST_BYTES);
            if raw.len() == PLAN_BYTES {
                assert!(native.len() > REQUEST_BYTES - 8192);
            }
        }
        Ok(())
    }
    #[test]
    fn seal_terminal_survives_foreign_requests_changed_operations_and_cleanup() -> Result<()> {
        let f = fixture()?;
        let w = work(f._temp.path(), None, None)?;
        let mut owner = Owner::default();
        let (stage, binding) = begin(&mut owner, &f, &w)?;
        render_and_drain(&mut owner, &f, &stage, &binding, &w, b"sealed")?;
        let seal = relay(&request(
            &f.root,
            &stage,
            3,
            false,
            &binding,
            Action::ResultAndSeal,
        ))?;
        let first = owner.call(&f.manifest, &seal, &AtomicBool::new(false))?;
        let mut foreign = seal.clone();
        foreign.stage = LeaseId::new();
        foreign.operation = U64(100);
        assert!(
            owner
                .call(&f.manifest, &foreign, &AtomicBool::new(false))
                .is_err()
        );
        foreign = seal.clone();
        foreign.root.token = LeaseId::new();
        foreign.operation = U64(101);
        assert!(
            owner
                .call(&f.manifest, &foreign, &AtomicBool::new(false))
                .is_err()
        );
        let mut altered = seal.clone();
        altered.operation = U64(102);
        assert!(
            owner
                .call(&f.manifest, &altered, &AtomicBool::new(false))
                .is_err()
        );
        let replay = owner.call(&f.manifest, &seal, &AtomicBool::new(true))?;
        assert_eq!(serde_json::to_vec(&first)?, serde_json::to_vec(&replay)?);
        owner.call(
            &f.manifest,
            &request(&f.root, &stage, 4, false, &binding, Action::Release),
            &AtomicBool::new(false),
        )?;
        let replay = owner.call(&f.manifest, &seal, &AtomicBool::new(true))?;
        assert_eq!(serde_json::to_vec(&first)?, serde_json::to_vec(&replay)?);
        Ok(())
    }
    #[test]
    fn failed_and_pre_admission_canceled_seals_have_bound_terminal_receipts() -> Result<()> {
        for canceled in [false, true] {
            let f = fixture()?;
            let w = work(f._temp.path(), None, None)?;
            let mut owner = Owner::default();
            let (stage, binding) = begin(&mut owner, &f, &w)?;
            let path = render_and_drain(&mut owner, &f, &stage, &binding, &w, b"sealed")?;
            if !canceled {
                fs::write(path.join("result.json"), b"invalid")?;
            }
            let seal = relay(&request(
                &f.root,
                &stage,
                3,
                false,
                &binding,
                Action::ResultAndSeal,
            ))?;
            let first = owner
                .call(&f.manifest, &seal, &AtomicBool::new(canceled))
                .unwrap_err()
                .downcast::<Failure>()?;
            let receipt = first
                .object_receipt
                .as_ref()
                .context("bound seal receipt")?;
            assert_eq!(receipt.request_digest, seal.digest()?);
            assert_eq!(receipt.operation, seal.operation);
            // Repairing the input must never repeat an admitted failed operation.
            fs::write(
                path.join("result.json"),
                serde_json::to_vec(&facts(&w, &path.join("output"))?)?,
            )?;
            let mut changed = seal.clone();
            changed.operation = U64(4);
            assert!(
                owner
                    .call(&f.manifest, &changed, &AtomicBool::new(false))
                    .is_err()
            );
            let replay = owner
                .call(&f.manifest, &seal, &AtomicBool::new(false))
                .unwrap_err()
                .downcast::<Failure>()?;
            assert_eq!(serde_json::to_vec(&first)?, serde_json::to_vec(&replay)?);
            let recovery = w
                .plan
                .destination
                .destination
                .parent()
                .unwrap()
                .join(format!(
                    ".photocatalog-photo-export-{}",
                    w.plan.destination.operation
                ));
            assert!(!recovery.exists());
            owner.call(
                &f.manifest,
                &request(&f.root, &stage, 5, false, &binding, Action::Release),
                &AtomicBool::new(false),
            )?;
        }
        Ok(())
    }
    #[test]
    fn checked_native_drain_with_modified_input_still_allows_disposal() -> Result<()> {
        let f = fixture()?;
        let w = work(f._temp.path(), None, None)?;
        let mut owner = Owner::default();
        let (stage, binding) = begin(&mut owner, &f, &w)?;
        owner.call(
            &f.manifest,
            &request(
                &f.root,
                &stage,
                2,
                false,
                &binding,
                Action::Ready {
                    icc: None,
                    xmp: None,
                },
            ),
            &AtomicBool::new(false),
        )?;
        let arm = relay(&request(
            &f.root,
            &stage,
            1,
            true,
            &binding,
            Action::Arm { native: U64(11) },
        ))?;
        let armed = owner.call(&f.manifest, &arm, &AtomicBool::new(false))?;
        let path = owner.stage.as_ref().unwrap().path.clone();
        fs::write(
            path.join("request.json"),
            b"changed after exact native exit",
        )?;
        // An ordinary C action cannot select or poison the G replay lane.
        for action in [
            Action::Begin {
                work: Box::new(w.clone()),
                limits: limits(),
            },
            Action::UploadIcc {
                offset: U64(0),
                bytes: vec![1],
            },
            Action::UploadXmp {
                offset: U64(0),
                bytes: vec![1],
            },
            Action::Arm { native: U64(11) },
            Action::NativeDrained {
                native: U64(11),
                terminal: NativeTerminal::Succeeded,
            },
            Action::Ready {
                icc: None,
                xmp: None,
            },
            Action::Release,
            Action::Abort,
            Action::ResultAndSeal,
        ] {
            let forged = request(&f.root, &stage, 100, true, &binding, action);
            assert!(crate::application::desktop::roundtrip_export_stage(&forged).is_err());
        }
        let arm_replay = owner.call(&f.manifest, &arm, &AtomicBool::new(false))?;
        assert_eq!(
            serde_json::to_vec(&armed)?,
            serde_json::to_vec(&arm_replay)?
        );
        let drain = relay(&request(
            &f.root,
            &stage,
            2,
            true,
            &binding,
            Action::NativeDrained {
                native: U64(11),
                terminal: NativeTerminal::Succeeded,
            },
        ))?;
        let failed = owner
            .call(&f.manifest, &drain, &AtomicBool::new(false))
            .unwrap_err()
            .downcast::<Failure>()?;
        assert!(owner.stage.as_ref().unwrap().drained.is_some());
        assert!(!owner.stage.as_ref().unwrap().inputs_valid);
        let replay = owner
            .call(&f.manifest, &drain, &AtomicBool::new(false))
            .unwrap_err()
            .downcast::<Failure>()?;
        assert_eq!(serde_json::to_vec(&failed)?, serde_json::to_vec(&replay)?);
        assert!(
            owner
                .call(
                    &f.manifest,
                    &request(&f.root, &stage, 3, false, &binding, Action::ResultAndSeal),
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        owner.call(
            &f.manifest,
            &request(&f.root, &stage, 4, false, &binding, Action::Release),
            &AtomicBool::new(false),
        )?;
        assert!(owner.empty());
        assert!(!path.exists());
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn cleanup_retains_locks_and_object_identity_across_replacement_symlink_and_partial_retry()
    -> Result<()> {
        let f = fixture()?;
        let w = work(f._temp.path(), None, None)?;
        let mut owner = Owner::default();
        let (stage, binding) = begin(&mut owner, &f, &w)?;
        let path = owner.stage.as_ref().unwrap().path.clone();
        create(&path.join("unknown"), b"retain")?;
        let abort = request(&f.root, &stage, 2, false, &binding, Action::Abort);
        assert!(
            owner
                .call(&f.manifest, &abort, &AtomicBool::new(false))
                .is_err()
        );
        let lock = File::open(path.join("parent.lock"))?;
        assert!(lock.try_lock_exclusive().is_err());
        let moved = path.with_extension("owned");
        fs::rename(&path, &moved)?;
        fs::create_dir(&path)?;
        create(&path.join("output"), b"replacement")?;
        let mut retry = abort.clone();
        retry.operation = U64(3);
        assert!(
            owner
                .call(&f.manifest, &retry, &AtomicBool::new(false))
                .is_err()
        );
        assert_eq!(fs::read(path.join("output"))?, b"replacement");
        fs::remove_file(path.join("output"))?;
        fs::remove_dir(&path)?;
        let victim = f._temp.path().join("victim");
        fs::create_dir(&victim)?;
        create(&victim.join("output"), b"victim")?;
        std::os::unix::fs::symlink(&victim, &path)?;
        retry.operation = U64(4);
        assert!(
            owner
                .call(&f.manifest, &retry, &AtomicBool::new(false))
                .is_err()
        );
        assert_eq!(fs::read(victim.join("output"))?, b"victim");
        fs::remove_file(&path)?;
        fs::rename(&moved, &path)?;
        fs::remove_file(path.join("unknown"))?;
        // Simulate a prior partial unlink: retained directory/locks still prove
        // custody, and absent closed artifacts do not strand cleanup.
        fs::remove_file(path.join("request.json"))?;
        retry.operation = U64(5);
        owner.call(&f.manifest, &retry, &AtomicBool::new(false))?;
        assert!(owner.empty());
        assert!(!path.exists());
        lock.try_lock_exclusive()?;
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn stage_reads_reject_fifo_and_symlink_without_blocking() -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let f = fixture()?;
        let path = f._temp.path().join("fifo");
        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert!(bounded_read(&path, None, REQUEST_BYTES as u64).is_err());
        fs::remove_file(&path)?;
        let regular = f._temp.path().join("regular");
        create(&regular, b"regular")?;
        std::os::unix::fs::symlink(&regular, &path)?;
        assert!(bounded_read(&path, None, REQUEST_BYTES as u64).is_err());
        Ok(())
    }
    #[test]
    fn maximum_native_receipt_can_produce_larger_enriched_reply_before_sealing() -> Result<()> {
        let f = fixture()?;
        let w = work(f._temp.path(), None, None)?;
        let mut owner = Owner::default();
        let (stage, binding) = begin(&mut owner, &f, &w)?;
        let path = render_and_drain(&mut owner, &f, &stage, &binding, &w, b"sealed")?;
        let mut receipt = facts(&w, &path.join("output"))?;
        receipt.rendered.metadata_notes.push(String::new());
        let base = serde_json::to_vec(&receipt)?.len();
        receipt.rendered.metadata_notes[0] = "n".repeat(RECEIPT_BYTES - base);
        let encoded = serde_json::to_vec(&receipt)?;
        assert_eq!(encoded.len(), RECEIPT_BYTES);
        fs::write(path.join("result.json"), encoded)?;
        let recovery = w
            .plan
            .destination
            .destination
            .parent()
            .unwrap()
            .join(format!(
                ".photocatalog-photo-export-{}",
                w.plan.destination.operation
            ));
        assert!(
            crate::export_worker::complete_managed_rendering(
                &w,
                &path,
                &AtomicBool::new(false),
                |_| { anyhow::bail!("simulated configured envelope refusal") }
            )
            .is_err()
        );
        assert!(
            !recovery.exists(),
            "envelope admission must precede durable seal effects"
        );
        let seal = relay(&request(
            &f.root,
            &stage,
            3,
            false,
            &binding,
            Action::ResultAndSeal,
        ))?;
        let completion = owner.call(&f.manifest, &seal, &AtomicBool::new(false))?;
        completion.validate(&seal)?;
        assert!(serde_json::to_vec(&completion)?.len() > RECEIPT_BYTES);
        super::super::wire::encode_outcome(&Ok(super::super::wire::Response::ExportStage(
            completion.clone(),
        )))?;
        crate::application::desktop::admit_export_stage_reply(&completion)?;
        owner.call(
            &f.manifest,
            &request(&f.root, &stage, 4, false, &binding, Action::Release),
            &AtomicBool::new(false),
        )?;
        Ok(())
    }
    #[test]
    fn both_full_size_blobs_are_stream_verified_without_full_blob_backing() -> Result<()> {
        let f = fixture()?;
        let bytes = vec![0x5a; BLOB_BYTES as usize];
        let w = work(f._temp.path(), Some(&bytes), Some(&bytes))?;
        let mut owner = Owner::default();
        let (stage, binding) = begin(&mut owner, &f, &w)?;
        let mut operation = 2;
        for icc in [true, false] {
            for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
                let action = if icc {
                    Action::UploadIcc {
                        offset: U64((index * CHUNK_BYTES) as u64),
                        bytes: chunk.to_vec(),
                    }
                } else {
                    Action::UploadXmp {
                        offset: U64((index * CHUNK_BYTES) as u64),
                        bytes: chunk.to_vec(),
                    }
                };
                let r = relay(&request(
                    &f.root, &stage, operation, false, &binding, action,
                ))?;
                owner.call(&f.manifest, &r, &AtomicBool::new(false))?;
                operation += 1;
            }
        }
        let blob = Blob {
            bytes: U64(BLOB_BYTES),
            digest: blake3::hash(&bytes).to_hex().to_string(),
        };
        let r = relay(&request(
            &f.root,
            &stage,
            operation,
            false,
            &binding,
            Action::Ready {
                icc: Some(blob.clone()),
                xmp: Some(blob),
            },
        ))?;
        owner.call(&f.manifest, &r, &AtomicBool::new(false))?;
        owner.call(
            &f.manifest,
            &request(
                &f.root,
                &stage,
                operation + 1,
                false,
                &binding,
                Action::Abort,
            ),
            &AtomicBool::new(false),
        )?;
        assert!(owner.empty());
        Ok(())
    }
    #[test]
    fn accepted_large_native_paths_and_receipt_details_fit_full_enriched_envelopes() -> Result<()> {
        let f = fixture()?;
        let mut w = work(f._temp.path(), None, None)?;
        let mut plan: PhotoExportPlan = serde_json::from_str(w.plan.raw())?;
        plan.destination.destination = f._temp.path().join("d".repeat(3900));
        let raw = serde_json::to_string(&plan)?;
        w.authority = blake3::hash(raw.as_bytes()).to_hex().to_string();
        w.plan = crate::catalog_exports::checked_plan(&raw, &w.authority)?;
        let staging = f._temp.path().join("s".repeat(3900));
        crate::export_worker::admit_output_path(&w, &staging.join("output"))?;
        let actual = f._temp.path().join("fixture-output");
        create(&actual, b"output")?;
        let mut native = facts(&w, &actual)?;
        native.rendered.staging = staging.join("output");
        native.rendered.metadata_notes.push(String::new());
        let base = serde_json::to_vec(&native)?.len();
        native.rendered.metadata_notes[0] = "n".repeat(RECEIPT_BYTES - base);
        assert_eq!(serde_json::to_vec(&native)?.len(), RECEIPT_BYTES);
        let r = request(
            &f.root,
            &LeaseId::new(),
            3,
            false,
            &Binding::from_work(&w),
            Action::ResultAndSeal,
        );
        let completion = crate::export_worker::CompletedExport {
            authority: w.authority.clone(),
            attempt: w.attempt.clone(),
            sealed: crate::metadata_export::SealedPhotoExport {
                version: 2,
                snapshot: w.plan.destination.clone(),
                authority_digest: w.authority.clone(),
                max_payload_bytes: w.plan.max_payload_bytes,
                payload: native.output_revision,
            },
            rendered: native.rendered,
            seal_ms: 0.,
            peak_resident_bytes: None,
            peak_method: native.peak_method,
        };
        let reply = super::reply(
            &r,
            Value::Completed {
                path: native_path(&staging)?,
                completion: completion.into(),
            },
        );
        reply.validate(&r)?;
        assert!(serde_json::to_vec(&reply)?.len() > RECEIPT_BYTES);
        super::super::wire::encode_outcome(&Ok(super::super::wire::Response::ExportStage(
            reply.clone(),
        )))?;
        crate::application::desktop::admit_export_stage_reply(&reply)?;
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn test_captured_begin_failure() {
    ADMISSION_FAULTS.with(|faults| {
        assert!(faults.borrow().is_empty());
        faults
            .borrow_mut()
            .extend([AdmissionFault::ParentClone, AdmissionFault::Cleanup]);
    });
}

#[cfg(test)]
pub(super) fn test_maximum_receipt(
    work: &crate::catalog_exports::ExportWork,
    path: &Path,
) -> Result<()> {
    fs::write(path.join("output"), b"sealed")?;
    let mut receipt = tests::facts(work, &path.join("output"))?;
    receipt.rendered.metadata_notes.push(String::new());
    let base = serde_json::to_vec(&receipt)?.len();
    receipt.rendered.metadata_notes[0] = "n".repeat(RECEIPT_BYTES - base);
    let bytes = serde_json::to_vec(&receipt)?;
    assert_eq!(bytes.len(), RECEIPT_BYTES);
    fs::write(path.join("result.json"), bytes)?;
    Ok(())
}
