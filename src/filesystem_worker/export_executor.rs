//! F-owned export executor lock, staging setup and compact persisted recovery.
use super::wire::{Failure, FailureKind};
use crate::{
    application::U64,
    catalog_session::{LeaseId, RootCapability, export_executor::*},
    export_worker::{
        CompactDiscard, CompactRetiredExportTransport,
        recover_export_transports_compact_with_checkpoint,
    },
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

struct ExecutorLock(File);
impl ExecutorLock {
    fn release(&self) -> Result<()> {
        FileExt::unlock(&self.0).context("release export executor lock")
    }
}
impl Drop for ExecutorLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[derive(Clone)]
struct Cached {
    operation: u64,
    digest: String,
    reply: Reply,
}
struct Active {
    root: RootCapability,
    executor: LeaseId,
    lock: ExecutorLock,
    lock_path: std::path::PathBuf,
    lock_identity: Option<(u64, u128)>,
    staging_path: std::path::PathBuf,
    staging: Option<File>,
    staging_identity: Option<(u64, u128)>,
    high_water: u64,
    closing: bool,
    pending: Option<(u64, String)>,
    last: Option<Cached>,
    inventory: VecDeque<CompactRetiredExportTransport>,
    discard: Option<(CompactDiscard, Reply)>,
}
pub(super) struct Owner {
    lifecycle_high_water: u64,
    active: Option<Active>,
    previous_close: Option<Cached>,
}
impl Default for Owner {
    fn default() -> Self {
        Self {
            lifecycle_high_water: 0,
            active: None,
            previous_close: None,
        }
    }
}
pub(crate) fn owner_layout() -> (usize, usize) {
    (std::mem::size_of::<Owner>(), std::mem::align_of::<Owner>())
}
impl Owner {
    pub fn empty(&self) -> bool {
        self.active.is_none()
    }
    pub fn admits_stage(&self, root: &RootCapability, executor: &LeaseId, cleanup: bool) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.root == *root && active.executor == *executor && (!active.closing || cleanup)
        })
    }
    fn reply(request: &Request, value: Value) -> Result<Reply> {
        let reply = Reply {
            root: request.root.clone(),
            executor: request.executor.clone(),
            operation: request.operation,
            request_digest: request.digest()?,
            value,
        };
        reply.validate(request)?;
        Ok(reply)
    }
    fn finish_acquire(active: &mut Active) -> Result<()> {
        ensure!(
            fs::symlink_metadata(&active.lock_path)?
                .file_type()
                .is_file(),
            "invalid export executor lock type"
        );
        let path_lock = crate::metadata_export::open_regular(&active.lock_path)?;
        let held = crate::storage_volume::held_object_key(&active.lock.0)?;
        let path = crate::storage_volume::held_object_key(&path_lock)?;
        ensure!(held == path, "export executor lock identity changed");
        if let Some(expected) = active.lock_identity {
            ensure!(held == expected, "retained export executor lock changed");
        } else {
            active.lock_identity = Some(held);
        }
        fs::create_dir_all(&active.staging_path)?;
        ensure!(
            fs::symlink_metadata(&active.staging_path)?
                .file_type()
                .is_dir(),
            "export staging must be an ordinary directory"
        );
        if active.staging.is_none() {
            // Retain the opened directory before fallible identity capture.
            active.staging = Some(crate::filesystem_worker::open_directory(
                &active.staging_path,
            )?);
        }
        let staging = active
            .staging
            .as_ref()
            .context("export staging not retained")?;
        let identity = crate::storage_volume::held_object_key(staging)?;
        let path = crate::filesystem_worker::open_directory(&active.staging_path)?;
        ensure!(
            crate::storage_volume::held_object_key(&path)? == identity,
            "export staging identity changed"
        );
        if let Some(expected) = active.staging_identity {
            ensure!(identity == expected, "export staging identity changed");
        } else {
            active.staging_identity = Some(identity);
        }
        Ok(())
    }
    fn verify(active: &Active) -> Result<()> {
        let expected_lock = active
            .lock_identity
            .context("export executor lock identity was not captured")?;
        ensure!(
            crate::storage_volume::held_object_key(&active.lock.0)? == expected_lock
                && fs::symlink_metadata(&active.lock_path)?
                    .file_type()
                    .is_file()
                && crate::storage_volume::held_object_key(&crate::metadata_export::open_regular(
                    &active.lock_path
                )?,)?
                    == expected_lock,
            "export executor lock identity changed"
        );
        let expected_staging = active
            .staging_identity
            .context("export staging identity was not captured")?;
        let held_staging = active
            .staging
            .as_ref()
            .context("export staging not retained")?;
        ensure!(
            crate::storage_volume::held_object_key(held_staging)? == expected_staging
                && fs::symlink_metadata(&active.staging_path)?
                    .file_type()
                    .is_dir()
                && crate::storage_volume::held_object_key(
                    &crate::filesystem_worker::open_directory(&active.staging_path)?,
                )? == expected_staging,
            "export staging identity changed"
        );
        Ok(())
    }
    fn acquire(
        &mut self,
        catalog: &Path,
        request: &Request,
        digest: &str,
        cancel: &AtomicBool,
    ) -> Result<Reply> {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "export executor Acquire canceled"
        );
        ensure!(
            self.active.is_none(),
            "another export executor remains owned"
        );
        let lock_path = catalog.join("photo-export.lock");
        if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
            ensure!(
                metadata.file_type().is_file(),
                "invalid export executor lock type"
            );
        }
        let mut lock_options = OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            lock_options.custom_flags(0x00200000);
        }
        let lock = lock_options.open(&lock_path)?;
        ensure!(
            lock.metadata()?.file_type().is_file(),
            "opened export executor lock is not an ordinary file"
        );
        ensure!(
            fs::symlink_metadata(&lock_path)?.file_type().is_file(),
            "invalid export executor lock type"
        );
        lock.try_lock_exclusive()
            .context("another export executor owns this catalog")?;
        let lock = ExecutorLock(lock);
        // Install custody before any fallible staging setup. A failed Acquire
        // remains owned and only the exact request or Release may continue it.
        self.lifecycle_high_water = generation(&request.root, &request.executor)?;
        self.active = Some(Active {
            root: request.root.clone(),
            executor: request.executor.clone(),
            lock,
            lock_path,
            lock_identity: None,
            staging_path: catalog.join("photo-export-workers"),
            staging: None,
            staging_identity: None,
            high_water: 0,
            closing: false,
            pending: Some((request.operation.0, digest.to_owned())),
            last: None,
            inventory: VecDeque::new(),
            discard: None,
        });
        Self::finish_acquire(self.active.as_mut().expect("Acquire installed custody"))?;
        let reply = Self::reply(request, Value::Acquired)?;
        let active = self.active.as_mut().expect("Acquire installed custody");
        active.high_water = request.operation.0;
        active.pending = None;
        active.last = Some(Cached {
            operation: request.operation.0,
            digest: request.digest()?,
            reply: reply.clone(),
        });
        Ok(reply)
    }
    pub fn call(
        &mut self,
        catalog: &Path,
        manifest: &Path,
        request: &Request,
        cancel: &AtomicBool,
        export_stage_empty: bool,
    ) -> Result<Reply> {
        request.validate()?;
        let digest = request.digest()?;
        if let Some(closed) = &self.previous_close
            && closed.operation == request.operation.0
            && closed.reply.executor == request.executor
        {
            ensure!(
                closed.digest == digest,
                "changed export executor Close replay"
            );
            return Ok(closed.reply.clone());
        }
        if self.active.is_none() {
            let generation = generation(&request.root, &request.executor)?;
            ensure!(
                generation > self.lifecycle_high_water,
                "retired export executor generation"
            );
            if matches!(request.action, Action::Release) && request.operation.0 == 2 {
                let reply = Self::reply(request, Value::Released)?;
                self.lifecycle_high_water = generation;
                self.previous_close = Some(Cached {
                    operation: request.operation.0,
                    digest,
                    reply: reply.clone(),
                });
                return Ok(reply);
            }
            ensure!(
                matches!(request.action, Action::Acquire),
                "export executor is not acquired"
            );
            return self.acquire(catalog, request, &digest, cancel);
        }
        let active = self.active.as_mut().expect("checked active executor");
        ensure!(
            active.root == request.root && active.executor == request.executor,
            "export executor identity mismatch"
        );
        if let Some(last) = &active.last
            && last.operation == request.operation.0
        {
            ensure!(last.digest == digest, "changed export executor replay");
            return Ok(last.reply.clone());
        }
        if let Some((operation, expected)) = &active.pending {
            if *operation != request.operation.0 || expected != &digest {
                ensure!(
                    matches!(request.action, Action::Release)
                        && *operation == 1
                        && request.operation.0
                            == operation
                                .checked_add(1)
                                .context("export executor operation exhausted")?,
                    "another export executor operation remains unresolved"
                );
                active.pending = Some((request.operation.0, digest.clone()));
            }
        } else {
            ensure!(
                request.operation.0
                    == active
                        .high_water
                        .checked_add(1)
                        .context("export executor operation exhausted")?,
                "export executor operation gap"
            );
            active.pending = Some((request.operation.0, digest.clone()));
        }
        if matches!(request.action, Action::Release) {
            active.closing = true;
            ensure!(export_stage_empty, "export stages remain owned");
            let reply = Self::reply(request, Value::Released)?;
            active.inventory.clear();
            active.lock.release()?;
            let closed = Cached {
                operation: request.operation.0,
                digest,
                reply: reply.clone(),
            };
            self.active.take();
            self.previous_close = Some(closed);
            return Ok(reply);
        }
        let value = match &request.action {
            Action::Acquire => {
                Self::finish_acquire(active)?;
                Value::Acquired
            }
            Action::Recover { max_directories } => {
                Self::verify(active)?;
                ensure!(
                    active.inventory.is_empty(),
                    "export recovery inventory remains owned"
                );
                let mut checkpoint = || {
                    ensure!(
                        !cancel.load(Ordering::Acquire),
                        "export transport recovery canceled"
                    );
                    Ok(())
                };
                let catalog_root = catalog.join("photo-export-workers");
                let manifest_root = manifest.join("export-workers");
                let recovered = recover_export_transports_compact_with_checkpoint(
                    &[catalog_root.as_path(), manifest_root.as_path()],
                    usize::try_from(max_directories.0)?,
                    &mut checkpoint,
                )?;
                let scanned = U64(u64::try_from(recovered.scanned)?);
                let cleaned = U64(u64::try_from(recovered.cleaned)?);
                let retained = U64(u64::try_from(recovered.retained)?);
                let retained_example = recovered.retained_example;
                let candidate = if retained.0 == 0 {
                    recovered.retired.first().map(|candidate| Candidate {
                        token: candidate.token.clone(),
                        attempt: candidate.attempt.clone(),
                    })
                } else {
                    None
                };
                let reply = Self::reply(
                    request,
                    Value::Recovery {
                        scanned,
                        cleaned,
                        retained,
                        retained_example,
                        candidate,
                    },
                )?;
                if retained.0 == 0 {
                    active.inventory = recovered.retired.into();
                }
                active.high_water = request.operation.0;
                active.pending = None;
                active.last = Some(Cached {
                    operation: request.operation.0,
                    digest,
                    reply: reply.clone(),
                });
                return Ok(reply);
            }
            Action::Discard { token } => {
                Self::verify(active)?;
                let candidate = active
                    .inventory
                    .front()
                    .context("export recovery candidate is not retained")?;
                ensure!(
                    &candidate.token == token,
                    "export recovery candidate changed"
                );
                if active.discard.is_none() {
                    // Admit the complete next reply before deleting anything.
                    // Both this reply and cleanup progress remain owned on error.
                    let next = active.inventory.get(1).map(|candidate| Candidate {
                        token: candidate.token.clone(),
                        attempt: candidate.attempt.clone(),
                    });
                    let reply = Self::reply(request, Value::Discarded { candidate: next })?;
                    active.discard = Some((CompactDiscard::begin(candidate, request)?, reply));
                }
                let (cleanup, _) = active.discard.as_mut().expect("discard custody");
                cleanup.resume(candidate)?;
                let (_, reply) = active.discard.take().expect("completed discard custody");
                // Everything below is infallible; exact replay is installed in
                // the same call before any later operation can see the queue.
                active.inventory.pop_front();
                active.high_water = request.operation.0;
                active.pending = None;
                active.last = Some(Cached {
                    operation: request.operation.0,
                    digest,
                    reply: reply.clone(),
                });
                return Ok(reply);
            }
            Action::Release => unreachable!(),
        };
        let reply = Self::reply(request, value)?;
        let active = self
            .active
            .as_mut()
            .expect("executor retained after operation");
        active.high_water = request.operation.0;
        active.pending = None;
        active.last = Some(Cached {
            operation: request.operation.0,
            digest,
            reply: reply.clone(),
        });
        Ok(reply)
    }
}

pub(super) fn rejected_stage(
    request: &crate::catalog_session::export_stage::Request,
) -> Result<Failure> {
    let mut failure = Failure::new(
        FailureKind::Rejected,
        "export stage executor lease is not active",
    );
    failure.object_receipt = Some(crate::catalog_session::preview_io::FailureReceipt {
        operation: request.operation,
        step: U64(0),
        request_digest: request.digest()?,
    });
    failure.validate()?;
    Ok(failure)
}
