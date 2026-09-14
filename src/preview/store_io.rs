//! C-side operation identity and streaming adapter; never opens an object path.
use super::ManagedFiles;
use crate::{
    application::U64,
    catalog_session::{LeaseId, preview_io::*},
    filesystem_worker::wire::{Failure, FailureKind},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use std::{path::Path, sync::atomic::AtomicBool};

pub(super) struct Calls {
    next: u64,
    pending: Option<Request>,
    active: Option<(u64, u64)>,
}
impl Default for Calls {
    fn default() -> Self {
        Self {
            next: 1,
            pending: None,
            active: None,
        }
    }
}
impl Calls {
    fn request(&mut self, owner: &ManagedFiles, action: Action) -> Result<Request> {
        if let Some(pending) = &self.pending {
            ensure!(
                pending.action == action,
                "cache filesystem outcome unknown; reconcile exact operation before another action"
            );
            return Ok(pending.clone());
        }
        let (operation, step) = if let Some((operation, step)) = self.active {
            (
                operation,
                step.checked_add(1).ok_or(crate::catalog_session::store::ResourceLimit("Cache transfer step identities exhausted; close the catalog successfully and retry"))?,
            )
        } else {
            ensure!(
                self.next != 0,
                crate::catalog_session::store::ResourceLimit(
                    "Cache operation identities exhausted; close the catalog successfully and retry"
                )
            );
            let operation = self.next;
            self.next = self.next.checked_add(1).unwrap_or(0);
            (operation, 0)
        };
        let request = Request {
            root: owner.root.clone(),
            group: owner.group()?,
            operation: U64(operation),
            step: U64(step),
            action,
        };
        request.validate()?;
        self.pending = Some(request.clone());
        Ok(request)
    }
    fn call(&mut self, owner: &ManagedFiles, action: Action) -> Result<Value> {
        let request = self.request(owner, action)?;
        owner
            .object_operation
            .store(request.operation.0, std::sync::atomic::Ordering::Release);
        let uncanceled = AtomicBool::new(false);
        let result = owner.filesystem.preview_io_call(
            &request,
            if request.cleanup() {
                &uncanceled
            } else {
                &owner.cancel
            },
        );
        match result {
            Ok(reply) => {
                reply.validate(&request)?;
                let active = matches!(
                    request.action,
                    Action::BeginWrite { .. } | Action::Write { .. }
                ) || matches!(
                    (&request.action, &reply.value),
                    (Action::Read { .. }, Value::Chunk { .. })
                ) || matches!(
                    (&request.action, &reply.value),
                    (
                        Action::BeginRead { .. },
                        Value::Integrity(Integrity::Intact)
                    )
                );
                self.active = active.then_some((request.operation.0, request.step.0));
                self.pending.take();
                Ok(reply.value)
            }
            Err(error) => {
                if error
                    .downcast_ref::<Failure>()
                    .is_some_and(|f| f.kind != FailureKind::Unknown)
                {
                    self.pending.take();
                }
                Err(error)
            }
        }
    }
    /// Compensation is explicit: callers use this only for discarded read buffers
    /// or SQL-pending publication cleanup, never an attached ready object.
    fn abort(&mut self, owner: &ManagedFiles) -> Result<()> {
        let mut terminal = None;
        if let Some(pending) = &self.pending {
            let request = pending.clone();
            match owner
                .filesystem
                .preview_io_call(&request, &AtomicBool::new(false))
            {
                Ok(reply) => {
                    reply.validate(&request)?;
                    let active = matches!(
                        request.action,
                        Action::BeginWrite { .. } | Action::Write { .. }
                    ) || matches!(
                        (&request.action, &reply.value),
                        (Action::Read { .. }, Value::Chunk { .. })
                            | (
                                Action::BeginRead { .. },
                                Value::Integrity(Integrity::Intact)
                            )
                    );
                    self.active = active.then_some((request.operation.0, request.step.0));
                }
                Err(error) => {
                    let failure = error.downcast_ref::<Failure>();
                    if failure.is_some_and(|f| f.kind != FailureKind::Unknown) {
                        // The exact retry delivered a known terminal rejection.
                        // Its original request must not poison subsequent reads.
                        terminal = Some(error);
                    } else if failure
                        .and_then(|f| f.object_receipt)
                        .is_some_and(|r| r.matches(&request).unwrap_or(false))
                    {
                        self.active = Some((request.operation.0, request.step.0));
                    } else {
                        // No authoritative F object receipt: preserve the original
                        // identity even if the pipe labels its uncertainty Unknown.
                        return Err(error);
                    }
                }
            }
            self.pending.take();
        }
        if self.active.is_some() {
            self.call(owner, Action::Abort)?;
        }
        if let Some(error) = terminal {
            return Err(error);
        }
        Ok(())
    }
}
impl ManagedFiles {
    pub(super) fn object_root(&self, path: &Path) -> Result<LeaseId> {
        let expected = NativePath::from_path(path);
        let group = self.group.lock().unwrap_or_else(|p| p.into_inner());
        let group = group.as_ref().context("preview filesystem group missing")?;
        group
            .tiers
            .iter()
            .chain(group.extra.iter())
            .find(|root| root.path == expected)
            .map(|root| root.token.clone())
            .context("cache path has no held preview root token")
    }
    pub(super) fn object_call(&self, action: Action) -> Result<Value> {
        let mut calls = self.objects.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(action, Action::Remove { .. }) {
            calls.abort(self)?;
        }
        let result = calls.call(self, action.clone());
        // Relocation is journal-directed and validates both roots, length and digest
        // on every retry. A delivered Unknown is compensated through the exact
        // receipt + Abort fence before a new operation may reconcile its files.
        if result.as_ref().is_err_and(|e| {
            e.downcast_ref::<Failure>()
                .is_some_and(|f| f.kind == FailureKind::Unknown)
        }) && matches!(
            action,
            Action::Relocate { .. } | Action::AdmitRelocation { .. }
        ) {
            calls.abort(self)?;
        }
        result
    }
    pub(super) fn object_read(
        &self,
        expected: Expected,
        allowance: u64,
    ) -> Result<(Integrity, Vec<u8>)> {
        let mut calls = self.objects.lock().unwrap_or_else(|p| p.into_inner());
        // An earlier failed read never authorizes mutation or buffer publication.
        if calls.pending.as_ref().is_some_and(|p| {
            matches!(
                p.action,
                Action::BeginRead { .. } | Action::Read { .. } | Action::Finish
            )
        }) {
            calls.abort(self)?;
        }
        let result = (|| -> Result<(Integrity, Vec<u8>)> {
            let Value::Integrity(state) = calls.call(
                self,
                Action::BeginRead {
                    expected: expected.clone(),
                    allowance: U64(allowance),
                },
            )?
            else {
                anyhow::bail!("wrong read begin reply")
            };
            if state != Integrity::Intact {
                return Ok((state, vec![]));
            }
            ensure!(expected.bytes.0 <= allowance, "read allowance mismatch");
            let length = usize::try_from(expected.bytes.0)?;
            let mut output = Vec::new();
            output.try_reserve_exact(length)?;
            while output.len() < length {
                let value = calls.call(
                    self,
                    Action::Read {
                        offset: U64(output.len() as u64),
                    },
                )?;
                let bytes = match value {
                    Value::Chunk { bytes, .. } => bytes,
                    Value::Integrity(Integrity::Corrupt) => {
                        return Ok((Integrity::Corrupt, vec![]));
                    }
                    _ => anyhow::bail!("wrong read chunk reply"),
                };
                ensure!(
                    !bytes.is_empty() && bytes.len() <= length - output.len(),
                    "invalid read progress"
                );
                output.extend_from_slice(&bytes);
            }
            if matches!(
                calls.call(self, Action::Finish)?,
                Value::Integrity(Integrity::Corrupt)
            ) {
                return Ok((Integrity::Corrupt, vec![]));
            }
            ensure!(
                blake3::hash(&output).to_hex().as_str() == expected.checksum,
                "cache received checksum mismatch"
            );
            Ok((Integrity::Intact, output))
        })();
        if result.is_err() {
            let _ = calls.abort(self);
        }
        match result {
            Err(error)
                if error
                    .downcast_ref::<Failure>()
                    .is_some_and(|f| f.kind == FailureKind::ResourceLimit) =>
            {
                Err(crate::preview::EncodedBudgetExceeded.into())
            }
            value => value,
        }
    }
    pub(super) fn object_write(
        &self,
        expected: Expected,
        temporary: &str,
        bytes: &[u8],
    ) -> Result<()> {
        ensure!(
            bytes.len() as u64 == expected.bytes.0
                && blake3::hash(bytes).to_hex().as_str() == expected.checksum,
            "cache upload admission mismatch"
        );
        let mut calls = self.objects.lock().unwrap_or_else(|p| p.into_inner());
        calls.call(
            self,
            Action::BeginWrite {
                expected,
                temporary: temporary.into(),
            },
        )?;
        for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
            calls.call(
                self,
                Action::Write {
                    offset: U64((index * CHUNK_BYTES) as u64),
                    checksum: blake3::hash(chunk).to_hex().to_string(),
                    bytes: chunk.to_vec(),
                },
            )?;
        }
        calls.call(self, Action::Finish)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_session as cs;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    struct Fake {
        count: AtomicUsize,
        requests: Mutex<[Option<Request>; 8]>,
        replay: Option<(FailureKind, u8)>,
    }
    impl cs::CatalogFilesystem for Fake {
        fn preview_io_call(&self, request: &Request, _: &AtomicBool) -> Result<Reply> {
            request.validate()?;
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            ensure!(n < 8, "bounded fake dispatch ledger");
            self.requests.lock().unwrap()[n] = Some(request.clone());
            if n == 0 {
                return Err(Failure::new(FailureKind::Unknown, "lost cache result").into());
            }
            if let Some((kind, receipt)) = self.replay {
                if n == 1 || (kind == FailureKind::Unknown && receipt != 1) {
                    let mut failure = Failure::new(kind, "exact retry result");
                    if receipt != 0 {
                        let mut request_digest = request.digest()?;
                        if receipt == 2 {
                            request_digest[0] ^= 1;
                        }
                        failure.object_receipt = Some(FailureReceipt {
                            operation: request.operation,
                            step: request.step,
                            request_digest,
                        });
                    }
                    return Err(failure.into());
                }
                return Ok(Reply {
                    epoch: request.root.epoch.clone(),
                    session: request.root.session.clone(),
                    group: request.group.clone(),
                    operation: request.operation,
                    step: request.step,
                    value: if matches!(request.action, Action::Abort) {
                        Value::Unit
                    } else {
                        Value::Integrity(Integrity::Missing)
                    },
                });
            }
            if n == 2 {
                return Err(
                    Failure::new(FailureKind::ResourceLimit, "known no-effect admission").into(),
                );
            }
            Ok(Reply {
                epoch: request.root.epoch.clone(),
                session: request.root.session.clone(),
                group: request.group.clone(),
                operation: request.operation,
                step: request.step,
                value: Value::Integrity(Integrity::Intact),
            })
        }
        fn prepare_catalog(
            &self,
            _: &cs::PrepareCatalog,
            _: &AtomicBool,
        ) -> Result<cs::CatalogBootstrap> {
            anyhow::bail!("unused")
        }
        fn abandon_prepare(&self, _: U64, _: &LeaseId) -> Result<()> {
            anyhow::bail!("unused")
        }
        fn confirm_sql_admission(
            &self,
            _: &cs::ConfirmSqlAdmission,
            _: &AtomicBool,
        ) -> Result<cs::SqlAdmissionConfirmed> {
            anyhow::bail!("unused")
        }
        fn restore_status(
            &self,
            _: &cs::RootCapability,
        ) -> Result<Option<crate::catalog_backup::RestoreStatus>> {
            anyhow::bail!("unused")
        }
        fn resume_restored_jobs(
            &self,
            _: &cs::RootCapability,
            _: &str,
            _: bool,
        ) -> Result<crate::catalog_backup::RestoreStatus> {
            anyhow::bail!("unused")
        }
        fn release_root(&self, _: &cs::RootCapability) -> Result<()> {
            anyhow::bail!("unused")
        }
    }
    #[test]
    fn cache_adapter_lost_reply_retains_exact_operation_and_blocks_conflicting_dispatch()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = NativePath::from_path(directory.path());
        let physical = crate::catalog_storage::physical_object_id(&tempfile::tempfile()?)?;
        let root = cs::RootCapability {
            epoch: LeaseId::new(),
            token: LeaseId::new(),
            session: LeaseId::new(),
            canonical_root: path.clone(),
            root_physical: physical,
            catalog_physical: physical,
        };
        let fake = Arc::new(Fake {
            replay: None,
            count: AtomicUsize::new(0),
            requests: Mutex::new(std::array::from_fn(|_| None)),
        });
        let adapter = ManagedFiles::new(
            fake.clone(),
            root,
            path.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let lease = cs::store::Lease {
            token: LeaseId::new(),
            tier: crate::preview::Tier::Thumbnail,
            path,
        };
        *adapter.group.lock().unwrap() = Some(cs::store::Acquired {
            group: LeaseId::new(),
            tiers: [
                lease.clone(),
                cs::store::Lease {
                    tier: crate::preview::Tier::Large,
                    ..lease.clone()
                },
            ],
            extra: None,
        });
        let expected = Expected {
            object: Object {
                root: lease.token,
                key: "a".repeat(64),
            },
            bytes: U64(3),
            checksum: blake3::hash(b"abc").to_hex().to_string(),
        };
        let action = Action::Check(expected.clone());
        assert!(adapter.object_call(action.clone()).is_err());
        let mut altered = expected.clone();
        altered.bytes = U64(4);
        assert!(adapter.object_call(Action::Check(altered)).is_err());
        assert_eq!(fake.count.load(Ordering::SeqCst), 1);
        adapter.object_call(action.clone())?;
        assert!(adapter.objects.lock().unwrap().pending.is_none());
        assert_eq!(
            adapter
                .object_call(action.clone())
                .unwrap_err()
                .downcast_ref::<Failure>()
                .unwrap()
                .kind,
            FailureKind::ResourceLimit
        );
        assert!(adapter.objects.lock().unwrap().pending.is_none());
        adapter.object_call(action)?;
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[2].as_ref().unwrap().operation, U64(2));
        assert_eq!(requests[3].as_ref().unwrap().operation, U64(3));
        Ok(())
    }
    fn read_adapter(fake: Arc<Fake>) -> Result<(tempfile::TempDir, Arc<ManagedFiles>, Expected)> {
        let directory = tempfile::tempdir()?;
        let path = NativePath::from_path(directory.path());
        let physical = crate::catalog_storage::physical_object_id(&tempfile::tempfile()?)?;
        let root = cs::RootCapability {
            epoch: LeaseId::new(),
            token: LeaseId::new(),
            session: LeaseId::new(),
            canonical_root: path.clone(),
            root_physical: physical,
            catalog_physical: physical,
        };
        let adapter = ManagedFiles::new(
            fake.clone(),
            root,
            path.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let lease = cs::store::Lease {
            token: LeaseId::new(),
            tier: crate::preview::Tier::Thumbnail,
            path,
        };
        *adapter.group.lock().unwrap() = Some(cs::store::Acquired {
            group: LeaseId::new(),
            tiers: [
                lease.clone(),
                cs::store::Lease {
                    tier: crate::preview::Tier::Large,
                    ..lease.clone()
                },
            ],
            extra: None,
        });
        let expected = Expected {
            object: Object {
                root: lease.token,
                key: "a".repeat(64),
            },
            bytes: U64(3),
            checksum: blake3::hash(b"abc").to_hex().to_string(),
        };
        Ok((directory, adapter, expected))
    }
    #[test]
    fn cache_read_unknown_then_known_replay_retires_pending_and_allows_new_read() -> Result<()> {
        for kind in [
            FailureKind::Rejected,
            FailureKind::ResourceLimit,
            FailureKind::Canceled,
        ] {
            let fake = Arc::new(Fake {
                count: AtomicUsize::new(0),
                requests: Mutex::new(std::array::from_fn(|_| None)),
                replay: Some((kind, 0)),
            });
            let (_directory, adapter, expected) = read_adapter(fake.clone())?;
            assert!(adapter.object_read(expected.clone(), 3).is_err());
            assert!(adapter.objects.lock().unwrap().pending.is_none());
            assert_eq!(adapter.object_read(expected, 3)?.0, Integrity::Missing);
            let requests = fake.requests.lock().unwrap();
            assert_eq!(requests[0], requests[1]);
            assert_eq!(requests[2].as_ref().unwrap().operation, U64(2));
            assert_eq!(fake.count.load(Ordering::SeqCst), 3);
        }
        Ok(())
    }
    #[test]
    fn cache_abort_requires_exact_f_failure_receipt_and_preserves_transport_unknown() -> Result<()>
    {
        for receipt in [0, 1, 2] {
            let fake = Arc::new(Fake {
                count: AtomicUsize::new(0),
                requests: Mutex::new(std::array::from_fn(|_| None)),
                replay: Some((FailureKind::Unknown, receipt)),
            });
            let (_directory, adapter, expected) = read_adapter(fake.clone())?;
            assert!(adapter.object_read(expected.clone(), 3).is_err());
            if receipt == 1 {
                assert!(adapter.objects.lock().unwrap().pending.is_none());
                assert_eq!(adapter.object_read(expected, 3)?.0, Integrity::Missing);
                let requests = fake.requests.lock().unwrap();
                assert_eq!(requests[0], requests[1]);
                let abort = requests[2].as_ref().unwrap();
                assert_eq!(abort.operation, U64(1));
                assert_eq!(abort.step, U64(1));
                assert_eq!(abort.action, Action::Abort);
                assert_eq!(requests[3].as_ref().unwrap().operation, U64(2));
            } else {
                assert!(adapter.object_read(expected, 3).is_err());
                let requests = fake.requests.lock().unwrap();
                assert_eq!(requests[0], requests[1]);
                assert_eq!(requests[0], requests[2]);
                assert_eq!(
                    adapter.objects.lock().unwrap().pending.as_ref(),
                    requests[0].as_ref()
                );
                assert!(
                    requests
                        .iter()
                        .flatten()
                        .all(|r| !matches!(r.action, Action::Abort))
                );
            }
        }
        Ok(())
    }
}
