//! Real cache filesystem execution. No SQL and no native child lives here.
use super::store::StoreOwner;
use super::wire::{Failure, FailureKind};
use crate::{application::U64, catalog_session::preview_io::*, preview::Layout};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

struct Stream {
    file: File,
    expected: Expected,
    temporary: Option<String>,
    offset: u64,
    hash: blake3::Hasher,
}
pub(super) struct ObjectOwner {
    high_water: u64,
    group: Option<[u8; 16]>,
    operation: u64,
    step: u64,
    digest: Option<[u8; 32]>,
    result: Option<std::result::Result<Reply, Failure>>,
    stream: Option<Stream>,
    unresolved: bool,
    budget: usize,
    #[cfg(test)]
    fail_after_effect: bool,
}
// One stream + one latest receipt; allow old/new status overlap during replacement.
// No object length enters this reservation. Path/request/codec scratch is separate.
fn retained_reservation() -> usize {
    std::mem::size_of::<ObjectOwner>() + 2 * std::mem::size_of::<Snapshot>()
        + CHUNK_BYTES + 64 + 3 * 36 + 128 // latest reply variants
        + 36 + 64 + 64 + 128 // stream token, key, checksum, temporary
        + 2 * (4 * 36 + super::wire::ERROR_BYTES) // status bindings and errors
        + super::wire::ERROR_BYTES // latest failed receipt alternative
}
impl Default for ObjectOwner {
    fn default() -> Self {
        Self {
            high_water: 0,
            group: None,
            operation: 0,
            step: 0,
            digest: None,
            result: None,
            stream: None,
            unresolved: false,
            budget: retained_reservation(),
            #[cfg(test)]
            fail_after_effect: false,
        }
    }
}
fn canceled(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        Failure::new(
            FailureKind::Canceled,
            "cache operation canceled before effects"
        )
    );
    Ok(())
}
fn check_loop(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "cache operation interrupted; reconcile partial effects"
    );
    Ok(())
}
fn path(store: &StoreOwner, request: &Request, object: &Object) -> Result<PathBuf> {
    object.validate()?;
    let (root, layout) = store.object_root(&request.group, &object.root)?;
    Ok(match layout {
        Layout::Flat => root.join(&object.key),
        Layout::HashPrefix => root
            .join(&object.key[..2])
            .join(&object.key[2..4])
            .join(&object.key),
    })
}
fn parent(store: &StoreOwner, request: &Request, object: &Object) -> Result<()> {
    let (root, layout) = store.object_root(&request.group, &object.root)?;
    if layout == Layout::Flat {
        return Ok(());
    }
    let mut current = root;
    for name in [&object.key[..2], &object.key[2..4]] {
        let next = current.join(name);
        match fs::create_dir(&next) {
            Ok(()) => {
                sync(&current)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => ensure!(
                fs::symlink_metadata(&next)?.file_type().is_dir(),
                "cache parent is not a directory"
            ),
            Err(e) => return Err(e.into()),
        }
        current = next;
    }
    Ok(())
}
fn sync(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn integrity(
    file: &mut File,
    length: u64,
    checksum: &str,
    cancel: &AtomicBool,
) -> Result<Integrity> {
    if file.metadata()?.len() != length {
        return Ok(Integrity::Corrupt);
    }
    let mut hash = blake3::Hasher::new();
    let mut count = 0u64;
    let mut scratch = [0; CHUNK_BYTES];
    loop {
        check_loop(cancel)?;
        let n = file.read(&mut scratch)?;
        if n == 0 {
            break;
        }
        count = count
            .checked_add(n as u64)
            .context("cache length overflow")?;
        if count > length {
            return Ok(Integrity::Corrupt);
        }
        hash.update(&scratch[..n]);
    }
    Ok(
        if count == length && hash.finalize().to_hex().as_str() == checksum {
            Integrity::Intact
        } else {
            Integrity::Corrupt
        },
    )
}
impl ObjectOwner {
    pub(super) fn execute(
        &mut self,
        store: &StoreOwner,
        request: &Request,
        cancel: &AtomicBool,
        mut publish: impl FnMut(Snapshot) -> Result<()>,
    ) -> Result<Reply> {
        request.validate()?;
        let digest = request.digest()?;
        if request.operation.0 == self.operation
            && request.step.0 == self.step
            && self.digest == Some(digest)
        {
            return self
                .result
                .clone()
                .context("cache operation still executing")?
                .map_err(Into::into);
        }
        let group = *uuid::Uuid::parse_str(request.group.as_str())?.as_bytes();
        let admission = (|| -> Result<()> {
            if request.operation.0 == self.operation {
                ensure!(self.group == Some(group), "cache operation group changed");
                ensure!(
                    request.step.0
                        == self
                            .step
                            .checked_add(1)
                            .context("cache step identity exhausted")?,
                    "stale or altered cache step"
                );
                ensure!(
                    (self.stream.is_some() || self.unresolved)
                        && (!self.unresolved || request.cleanup()),
                    "cache operation terminal or requires abort"
                );
                ensure!(
                    matches!(
                        request.action,
                        Action::Read { .. } | Action::Write { .. } | Action::Finish | Action::Abort
                    ),
                    "new operation inside cache transfer"
                );
            } else {
                ensure!(
                    request.operation.0 > self.high_water && request.step.0 == 0,
                    "stale cache operation"
                );
                ensure!(
                    self.stream.is_none() && !self.unresolved,
                    "cache transfer slot occupied; reconcile or abort before retry"
                );
                ensure!(
                    !matches!(
                        request.action,
                        Action::Read { .. } | Action::Write { .. } | Action::Finish | Action::Abort
                    ),
                    "cache continuation without begin"
                );
            }
            Ok(())
        })();
        admission.map_err(|e| Failure::new(FailureKind::Rejected, e))?;
        if !request.cleanup() {
            canceled(cancel)?;
            ensure!(
                retained_reservation() <= self.budget,
                crate::catalog_session::store::ResourceLimit(
                    "Cache transfer custody budget is occupied; finish or reconcile the active transfer and retry"
                )
            );
        }
        self.high_water = self.high_water.max(request.operation.0);
        self.group = Some(group);
        self.operation = request.operation.0;
        self.step = request.step.0;
        self.digest = Some(digest);
        self.result = None;
        // Record possible effects before opening, writing, publishing or removing.
        self.unresolved = true;
        publish(self.snapshot(request))?;
        let execution = self.inner(store, request, cancel);
        #[cfg(test)]
        let execution = if self.fail_after_effect && execution.is_ok() {
            self.fail_after_effect = false;
            Err(anyhow::anyhow!(
                "injected failure after filesystem effect before receipt"
            ))
        } else {
            execution
        };
        let result = execution
            .map(|value| Reply {
                epoch: request.root.epoch.clone(),
                session: request.root.session.clone(),
                group: request.group.clone(),
                operation: request.operation,
                step: request.step,
                value,
            })
            .map_err(|error| {
                let mut failure = super::filesystem_failure(error);
                failure.object_receipt = Some(FailureReceipt {
                    operation: request.operation,
                    step: request.step,
                    request_digest: digest,
                });
                if failure.kind == FailureKind::Unknown
                    && self.stream.is_none()
                    && matches!(
                        request.action,
                        Action::Check(_)
                            | Action::InspectRelocation { .. }
                            | Action::CheckRelocation { .. }
                            | Action::BeginRead { .. }
                    )
                {
                    failure.kind = FailureKind::Rejected;
                }
                failure
            });
        if result.is_ok()
            || result
                .as_ref()
                .is_err_and(|f| f.kind != FailureKind::Unknown)
        {
            self.unresolved = false;
        }
        self.result = Some(result.clone());
        publish(self.snapshot(request))?;
        result.map_err(Into::into)
    }
    fn inner(
        &mut self,
        store: &StoreOwner,
        request: &Request,
        cancel: &AtomicBool,
    ) -> Result<Value> {
        match &request.action {
            Action::Check(expected) => {
                let mut file = match File::open(path(store, request, &expected.object)?) {
                    Ok(file) => file,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(Value::Integrity(Integrity::Missing));
                    }
                    Err(e) => return Err(e.into()),
                };
                Ok(Value::Integrity(integrity(
                    &mut file,
                    expected.bytes.0,
                    &expected.checksum,
                    cancel,
                )?))
            }
            Action::BeginRead {
                expected,
                allowance,
            } => {
                ensure!(self.stream.is_none(), "cache transfer already active");
                let file = match File::open(path(store, request, &expected.object)?) {
                    Ok(file) => file,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(Value::Integrity(Integrity::Missing));
                    }
                    Err(e) => return Err(e.into()),
                };
                if file.metadata()?.len() != expected.bytes.0 {
                    return Ok(Value::Integrity(Integrity::Corrupt));
                }
                ensure!(
                    expected.bytes.0 <= allowance.0,
                    super::super::catalog_session::store::ResourceLimit(
                        "Cache object exceeds the admitted encoded byte allowance"
                    )
                );
                self.stream = Some(Stream {
                    file,
                    expected: expected.clone(),
                    temporary: None,
                    offset: 0,
                    hash: blake3::Hasher::new(),
                });
                Ok(Value::Integrity(Integrity::Intact))
            }
            Action::Read { offset } => {
                let stream = self.stream.as_mut().context("no cache read transfer")?;
                ensure!(
                    stream.temporary.is_none() && stream.offset == offset.0,
                    "cache read phase/offset mismatch"
                );
                path(store, request, &stream.expected.object)?;
                let length =
                    (stream.expected.bytes.0 - stream.offset).min(CHUNK_BYTES as u64) as usize;
                let mut bytes = vec![0; length];
                if let Err(error) = stream.file.read_exact(&mut bytes) {
                    if error.kind() == std::io::ErrorKind::UnexpectedEof {
                        self.stream.take();
                        return Ok(Value::Integrity(Integrity::Corrupt));
                    }
                    return Err(error.into());
                }
                stream.hash.update(&bytes);
                stream.offset += length as u64;
                Ok(Value::Chunk {
                    offset: *offset,
                    checksum: blake3::hash(&bytes).to_hex().to_string(),
                    bytes,
                })
            }
            Action::BeginWrite {
                expected,
                temporary,
            } => {
                ensure!(self.stream.is_none(), "cache transfer already active");
                parent(store, request, &expected.object)?;
                let destination = path(store, request, &expected.object)?;
                ensure!(
                    !destination.exists(),
                    "untracked immutable destination requires recovery"
                );
                let file = OpenOptions::new().write(true).create_new(true).open(
                    destination
                        .parent()
                        .context("cache parent")?
                        .join(temporary),
                )?;
                self.stream = Some(Stream {
                    file,
                    expected: expected.clone(),
                    temporary: Some(temporary.clone()),
                    offset: 0,
                    hash: blake3::Hasher::new(),
                });
                Ok(Value::Unit)
            }
            Action::Write { offset, bytes, .. } => {
                let stream = self.stream.as_mut().context("no cache upload")?;
                ensure!(
                    stream.temporary.is_some() && stream.offset == offset.0,
                    "cache write phase/offset mismatch"
                );
                path(store, request, &stream.expected.object)?;
                let end = stream
                    .offset
                    .checked_add(bytes.len() as u64)
                    .filter(|v| *v <= stream.expected.bytes.0)
                    .context("cache upload grew")?;
                stream.file.write_all(bytes)?;
                stream.hash.update(bytes);
                stream.offset = end;
                Ok(Value::Unit)
            }
            Action::Finish => {
                let stream = self
                    .stream
                    .as_mut()
                    .context("no cache transfer to finish")?;
                ensure!(
                    stream.offset == stream.expected.bytes.0,
                    "cache transfer length mismatch"
                );
                let checksum_matches =
                    stream.hash.finalize().to_hex().as_str() == stream.expected.checksum;
                if stream.temporary.is_none() && !checksum_matches {
                    self.stream.take();
                    return Ok(Value::Integrity(Integrity::Corrupt));
                }
                ensure!(checksum_matches, "cache upload checksum mismatch");
                let destination = path(store, request, &stream.expected.object)?;
                if let Some(temporary) = &stream.temporary {
                    stream.file.sync_all()?;
                    ensure!(
                        !destination.exists(),
                        "immutable preview destination already exists"
                    );
                    let temporary_path = destination
                        .parent()
                        .context("cache parent")?
                        .join(temporary);
                    fs::hard_link(&temporary_path, &destination)
                        .context("publish cache object without replacement")?;
                    remove(&temporary_path)?;
                    sync(destination.parent().context("cache parent")?)?;
                } else {
                    let mut extra = [0];
                    if stream.file.read(&mut extra)? != 0 {
                        self.stream.take();
                        return Ok(Value::Integrity(Integrity::Corrupt));
                    }
                }
                self.stream.take();
                Ok(Value::Unit)
            }
            Action::Abort => {
                // This closes a transfer and removes only its uncommitted temporary.
                // A linked immutable destination is never rolled back here. C must
                // retain its SQL journal/attachment authority and verify that object.
                if let Some(stream) = &self.stream
                    && let Some(temporary) = &stream.temporary
                {
                    let destination = path(store, request, &stream.expected.object)?;
                    remove(
                        &destination
                            .parent()
                            .context("cache parent")?
                            .join(temporary),
                    )?;
                }
                self.stream.take();
                Ok(Value::Unit)
            }
            Action::Remove { object, temporary } => {
                let destination = path(store, request, object)?;
                if let Some(temporary) = temporary {
                    remove(
                        &destination
                            .parent()
                            .context("cache parent")?
                            .join(temporary),
                    )?;
                }
                remove(&destination)?;
                Ok(Value::Unit)
            }
            Action::InspectRelocation { target } => {
                inspect_relocation(&store.reserved_path(&request.group, target)?)?;
                Ok(Value::Unit)
            }
            Action::AdmitRelocation { target } => self.admit_relocation(store, request, target),
            Action::CheckRelocation { target, id } => {
                marker(store, request, target, id)?;
                Ok(Value::Unit)
            }
            Action::Relocate {
                source,
                target,
                id,
                key,
                bytes,
                checksum,
                cleanup,
            } => {
                marker(store, request, target, id)?;
                let old = Object {
                    root: source.clone(),
                    key: key.clone(),
                };
                let new = Object {
                    root: target.clone(),
                    key: key.clone(),
                };
                relocate(
                    store,
                    request,
                    (&old, &new),
                    bytes.0,
                    checksum,
                    *cleanup,
                    cancel,
                )?;
                Ok(Value::Unit)
            }
        }
    }
    #[cfg(test)]
    pub(super) fn fail_next_effect(&mut self) {
        self.fail_after_effect = true;
    }
    #[cfg(test)]
    pub(super) fn with_budget(budget: usize) -> Self {
        Self {
            budget,
            ..Self::default()
        }
    }
    #[cfg(test)]
    pub(super) fn restore_budget(&mut self) {
        self.budget = retained_reservation();
    }
    #[cfg(test)]
    pub(super) fn capacity(&self) -> (usize, usize) {
        (self.owned_bytes(), retained_reservation())
    }
    pub(super) fn drain(&mut self) {
        self.stream.take();
        self.unresolved = false;
    }
}
fn marker(
    store: &StoreOwner,
    request: &Request,
    target: &crate::catalog_session::LeaseId,
    id: &str,
) -> Result<()> {
    let (root, _) = store.object_root(&request.group, target)?;
    let mut file = File::open(root.join(".photocatalog-relocation"))?;
    ensure!(
        id.len() <= 128 && file.metadata()?.len() == id.len() as u64,
        "relocation marker length"
    );
    let mut bytes = [0; 128];
    file.read_exact(&mut bytes[..id.len()])?;
    let mut extra = [0];
    ensure!(
        &bytes[..id.len()] == id.as_bytes() && file.read(&mut extra)? == 0,
        "relocation destination ownership changed"
    );
    Ok(())
}
impl ObjectOwner {
    fn admit_relocation(
        &self,
        store: &StoreOwner,
        request: &Request,
        target: &crate::catalog_session::LeaseId,
    ) -> Result<Value> {
        let (root, _) = store.object_root(&request.group, target)?;
        inspect_relocation(&root)?;
        let marker = root.join(".photocatalog-relocation");
        let id = if marker.exists() {
            ensure!(
                fs::metadata(&marker)?.len() <= 36,
                "invalid relocation admission marker"
            );
            // Preserve the existing 36-byte marker admission; a fixed extra byte
            // detects growth without materializing the marker.
            let mut bytes = [0; 37];
            let mut file = File::open(&marker)?;
            let mut n = 0;
            while n < bytes.len() {
                let read = file.read(&mut bytes[n..])?;
                if read == 0 {
                    break;
                }
                n += read;
            }
            std::str::from_utf8(&bytes[..n])
                .ok()
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .unwrap_or_else(uuid::Uuid::new_v4)
                .to_string()
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(marker)?;
        output.write_all(id.as_bytes())?;
        output.sync_all()?;
        sync(&root)?;
        Ok(Value::Relocation(id))
    }
}
fn inspect_relocation(root: &Path) -> Result<()> {
    if root.exists() {
        // Only two admission names can pass, so fail on the third entry without
        // materializing an arbitrary directory listing.
        for entry in fs::read_dir(root)?.take(3) {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_file()
                    && matches!(
                        entry.file_name().to_str(),
                        Some(".photocatalog-preview-owner" | ".photocatalog-relocation")
                    ),
                "relocation destination contains non-admission files"
            );
        }
    }
    if root.join(".photocatalog-relocation").exists() {
        ensure!(
            fs::symlink_metadata(root.join(".photocatalog-preview-owner"))?.len() > 0,
            "relocation admission has no recorded owner identity"
        );
    }
    Ok(())
}
fn relocate(
    store: &StoreOwner,
    request: &Request,
    (old, new): (&Object, &Object),
    length: u64,
    checksum: &str,
    cleanup: bool,
    cancel: &AtomicBool,
) -> Result<()> {
    let source = path(store, request, old)?;
    let destination = path(store, request, new)?;
    let verify = |path: &Path| -> Result<()> {
        ensure!(
            integrity(&mut File::open(path)?, length, checksum, cancel)? == Integrity::Intact,
            "relocation checksum/length mismatch"
        );
        Ok(())
    };
    if cleanup {
        verify(&destination)?;
        if source.exists() {
            verify(&source)?;
            remove(&source)?;
        }
        return Ok(());
    }
    if destination.exists() {
        return verify(&destination);
    }
    parent(store, request, new)?;
    let temporary = destination.with_extension("relocation-pending");
    if temporary.exists() {
        ensure!(
            fs::symlink_metadata(&temporary)?.file_type().is_file(),
            "invalid relocation temporary file"
        );
        remove(&temporary)?;
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let mut input = File::open(&source)?;
    let mut hash = blake3::Hasher::new();
    let mut scratch = [0; CHUNK_BYTES];
    let mut copied = 0u64;
    loop {
        check_loop(cancel)?;
        let n = input.read(&mut scratch)?;
        if n == 0 {
            break;
        }
        copied = copied
            .checked_add(n as u64)
            .context("relocation length overflow")?;
        ensure!(copied <= length, "relocation source grew");
        output.write_all(&scratch[..n])?;
        hash.update(&scratch[..n]);
    }
    ensure!(
        copied == length && hash.finalize().to_hex().as_str() == checksum,
        "relocation source checksum mismatch"
    );
    output.sync_all()?;
    drop(output);
    fs::hard_link(&temporary, &destination)
        .context("publish relocation without replacing existing object")?;
    remove(&temporary)?;
    sync(destination.parent().context("relocation parent")?)?;
    Ok(())
}

#[derive(Clone)]
pub(super) struct Snapshot {
    binding: crate::catalog_session::store::StatusQuery,
    group: crate::catalog_session::LeaseId,
    operation: U64,
    stage: crate::catalog_session::store::Stage,
    progress: Progress,
    owned_bytes: usize,
}
impl Snapshot {
    pub(super) fn status(
        &self,
        query: &crate::catalog_session::store::StatusQuery,
    ) -> Result<crate::catalog_session::store::Status> {
        use crate::catalog_session::store::{Status, StatusKind};
        query.validate()?;
        ensure!(
            query.kind == StatusKind::Objects
                && query.epoch == self.binding.epoch
                && query.token == self.binding.token
                && query.session == self.binding.session
                && query.root_physical == self.binding.root_physical
                && query.catalog_physical == self.binding.catalog_physical,
            "cache status belongs to another catalog session"
        );
        let matches = query.operation == self.operation;
        Ok(Status {
            kind: StatusKind::Objects,
            operation: query.operation,
            group: Some(self.group.clone()),
            stage: matches.then_some(self.stage),
            slots: 0,
            owned_bytes: self.owned_bytes,
            selected: None,
            object: matches.then(|| self.progress.clone()),
        })
    }
}
impl ObjectOwner {
    fn snapshot(&self, request: &Request) -> Snapshot {
        use crate::catalog_session::store::{Query, Stage, StatusKind, StatusQuery};
        let progress = Progress {
            step: U64(self.step),
            offset: U64(self.stream.as_ref().map_or(0, |s| s.offset)),
            bytes: U64(self.stream.as_ref().map_or(0, |s| s.expected.bytes.0)),
            receiving: self.stream.as_ref().is_some_and(|s| s.temporary.is_some()),
            unresolved: self.unresolved,
            failure: self
                .result
                .as_ref()
                .and_then(|r| r.as_ref().err())
                .map(|f| (f.kind, f.message.clone())),
        };
        let stage = if self.result.is_none() {
            Stage::Acquiring
        } else if self.result.as_ref().is_some_and(|r| r.is_err()) {
            Stage::Failed
        } else if self.stream.is_some() {
            Stage::Held
        } else {
            Stage::Complete
        };
        Snapshot {
            binding: StatusQuery::from(&Query {
                kind: StatusKind::Objects,
                root: request.root.clone(),
                operation: request.operation,
                selected: None,
            }),
            group: request.group.clone(),
            operation: request.operation,
            stage,
            progress,
            owned_bytes: self.owned_bytes(),
        }
    }
    fn owned_bytes(&self) -> usize {
        let receipt = self.result.as_ref().map_or(0, |r| match r {
            Ok(reply) => {
                3 * 36
                    + match &reply.value {
                        Value::Chunk {
                            bytes, checksum, ..
                        } => bytes.capacity() + checksum.capacity(),
                        Value::Relocation(id) => id.capacity(),
                        _ => 0,
                    }
            }
            Err(error) => error.message.capacity(),
        });
        std::mem::size_of::<Self>()
            + std::mem::size_of::<Snapshot>()
            + 4 * 36
            + self
                .result
                .as_ref()
                .and_then(|r| r.as_ref().err())
                .map_or(0, |e| e.message.len())
            + receipt
            + self.stream.as_ref().map_or(0, |s| {
                36 + s.expected.object.key.capacity()
                    + s.expected.checksum.capacity()
                    + s.temporary.as_ref().map_or(0, String::capacity)
            })
    }
}
