//! Session-bound C adapter. Tokens are facts; only the sibling F owns locks.
use super::*;
use crate::catalog_session::{CatalogFilesystem, RootCapability, store as wire};
use std::sync::{Arc, Mutex, atomic::AtomicBool};

pub(crate) struct ManagedFiles {
    filesystem: Arc<dyn CatalogFilesystem>,
    root: RootCapability,
    manifest: crate::storage_volume::NativePath,
    cancel: Arc<AtomicBool>,
    group: Mutex<Option<wire::Acquired>>,
    pending: Mutex<Calls>,
    objects: Mutex<object_io::Calls>,
    object_operation: std::sync::atomic::AtomicU64,
}
struct Calls {
    next: u64,
    pending: Option<wire::Request>,
}
impl Calls {
    fn next_operation(&mut self) -> Result<crate::application::U64> {
        ensure!(
            self.next != 0,
            wire::ResourceLimit(
                "Preview operation identities are exhausted for this catalog session. Close the catalog successfully, reopen it, and retry"
            )
        );
        let operation = crate::application::U64(self.next);
        self.next = self.next.checked_add(1).unwrap_or(0);
        Ok(operation)
    }
}
struct GroupLease {
    _filesystem: Arc<dyn CatalogFilesystem>,
    _root: RootCapability,
    _group: crate::catalog_session::LeaseId,
}
impl ManagedFiles {
    pub(crate) fn new(
        filesystem: Arc<dyn CatalogFilesystem>,
        root: RootCapability,
        manifest: crate::storage_volume::NativePath,
        cancel: Arc<AtomicBool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            filesystem,
            root,
            manifest,
            cancel,
            group: Mutex::new(None),
            objects: Mutex::new(object_io::Calls::default()),
            object_operation: std::sync::atomic::AtomicU64::new(0),
            pending: Mutex::new(Calls {
                next: 1,
                pending: None,
            }),
        })
    }
    fn call(&self, action: wire::Action) -> Result<wire::Value> {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(previous) = pending.pending.as_ref() {
            ensure!(
                previous.action == action,
                "preview ownership outcome is unknown; retry the same operation or close the catalog successfully"
            );
        }
        if pending.pending.is_none() {
            let operation = pending.next_operation()?;
            pending.pending = Some(wire::Request {
                root: self.root.clone(),
                operation,
                action,
            });
        }
        let request = pending.pending.as_ref().unwrap();
        let cleanup = AtomicBool::new(false);
        let result = self.filesystem.preview_store_call(
            request,
            if request.is_cleanup() {
                &cleanup
            } else {
                &self.cancel
            },
        );
        match result {
            Ok(reply) => {
                wire::validate_reply(request, &reply)?;
                pending.pending.take();
                Ok(reply.value)
            }
            Err(error) => {
                // A delivered terminal response is distinct from a lost/poisoned pipe.
                if error
                    .downcast_ref::<crate::filesystem_worker::wire::Failure>()
                    .is_some_and(|f| f.kind != crate::filesystem_worker::wire::FailureKind::Unknown)
                    || error.is::<wire::ResourceLimit>()
                {
                    pending.pending.take();
                }
                Err(error)
            }
        }
    }
    fn admit(
        &self,
        config: &StoreConfig,
        identity: &str,
        relocation: Option<wire::Relocation>,
    ) -> Result<Arc<dyn Send + Sync>> {
        ensure!(
            crate::storage_volume::NativePath::from_path(&config.manifest_root) == self.manifest,
            "preview configuration manifest differs from its admitted SQL role"
        );
        ensure!(
            self.group
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_none(),
            "preview filesystem group already admitted"
        );
        let roots = [
            crate::storage_volume::NativePath::from_path(&config.thumbnail_root),
            crate::storage_volume::NativePath::from_path(&config.large_root),
        ];
        let wire::Value::Acquired(group) = self.call(wire::Action::Acquire(wire::Descriptor {
            identity: identity.into(),
            layout: config.layout,
            roots: roots.clone(),
            relocation,
        }))?
        else {
            bail!("unexpected preview group response")
        };
        for (actual, expected) in group.tiers.iter().zip(&roots) {
            ensure!(
                &actual.path == expected,
                "preview filesystem canonical facts changed"
            );
        }
        let owner = Arc::new(GroupLease {
            _filesystem: self.filesystem.clone(),
            _root: self.root.clone(),
            _group: group.group.clone(),
        });
        *self.group.lock().unwrap_or_else(|p| p.into_inner()) = Some(group);
        Ok(owner)
    }
    fn group(&self) -> Result<crate::catalog_session::LeaseId> {
        Ok(self
            .group
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .context("preview filesystem group is not admitted")?
            .group
            .clone())
    }
}
impl AdmittedStoreFiles for ManagedFiles {
    #[cfg(test)]
    fn cache_status(&self) -> Result<crate::catalog_session::store::Status> {
        self.filesystem
            .preview_store_status(&crate::catalog_session::store::Query {
                kind: crate::catalog_session::store::StatusKind::Objects,
                root: self.root.clone(),
                operation: crate::application::U64(
                    self.object_operation
                        .load(std::sync::atomic::Ordering::Acquire),
                ),
                selected: None,
            })
    }

    fn cache_root(&self, path: &Path) -> Result<crate::catalog_session::LeaseId> {
        self.object_root(path)
    }
    fn cache_call(
        &self,
        action: crate::catalog_session::preview_io::Action,
    ) -> Result<crate::catalog_session::preview_io::Value> {
        self.object_call(action)
    }
    fn cache_read(
        &self,
        expected: crate::catalog_session::preview_io::Expected,
        allowance: u64,
    ) -> Result<(crate::catalog_session::preview_io::Integrity, Vec<u8>)> {
        self.object_read(expected, allowance)
    }
    fn cache_write(
        &self,
        expected: crate::catalog_session::preview_io::Expected,
        temporary: &str,
        bytes: &[u8],
    ) -> Result<()> {
        self.object_write(expected, temporary, bytes)
    }

    fn lock_tiers(&self, config: &StoreConfig, identity: &str) -> Result<Arc<dyn Send + Sync>> {
        self.admit(config, identity, None)
    }
    fn lock_tiers_recovering(
        &self,
        config: &StoreConfig,
        identity: &str,
        relocation: Option<wire::Relocation>,
    ) -> Result<Arc<dyn Send + Sync>> {
        self.admit(config, identity, relocation)
    }
    fn recovered_root(&self) -> Option<wire::Lease> {
        self.group
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .and_then(|g| g.extra.clone())
    }
    fn reserve_root(&self, destination: &Path, tier: Tier) -> Result<wire::Lease> {
        let wire::Value::Reserved(value) = self.call(wire::Action::Reserve {
            group: self.group()?,
            tier,
            destination: crate::storage_volume::NativePath::from_path(destination),
        })?
        else {
            bail!("unexpected preview reservation response")
        };
        self.group
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
            .context("preview group missing")?
            .extra = Some(value.clone());
        Ok(value)
    }
    fn lock_reserved(&self, reservation: &wire::Lease) -> Result<()> {
        let wire::Value::Locked(value) = self.call(wire::Action::Lock {
            group: self.group()?,
            reservation: reservation.token.clone(),
        })?
        else {
            bail!("unexpected preview lock response")
        };
        ensure!(
            &value == reservation,
            "preview root facts changed during acquisition"
        );
        Ok(())
    }
    fn promote_root(&self, target: &wire::Lease) -> Result<wire::Lease> {
        {
            let state = self.group.lock().unwrap_or_else(|p| p.into_inner());
            let state = state.as_ref().context("preview group missing")?;
            let index = if target.tier == Tier::Thumbnail { 0 } else { 1 };
            if state.tiers[index] == *target {
                return state
                    .extra
                    .clone()
                    .context("preview promotion lacks old root");
            }
        }
        let group = self.group()?;
        self.call(wire::Action::Promote {
            group,
            target: target.token.clone(),
            tier: target.tier,
        })?;
        let mut state = self.group.lock().unwrap_or_else(|p| p.into_inner());
        let state = state.as_mut().context("preview group missing")?;
        let index = if target.tier == Tier::Thumbnail { 0 } else { 1 };
        let old = std::mem::replace(&mut state.tiers[index], target.clone());
        state.extra = Some(old.clone());
        Ok(old)
    }
    fn retire_root(&self, old: &wire::Lease) -> Result<()> {
        self.call(wire::Action::Retire {
            group: self.group()?,
            old: old.token.clone(),
        })?;
        self.group
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
            .context("preview group missing")?
            .extra = None;
        Ok(())
    }
    fn abandon_root(&self, reservation: &wire::Lease) -> Result<()> {
        self.call(wire::Action::Abandon {
            group: self.group()?,
            reservation: reservation.token.clone(),
        })?;
        self.group
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
            .context("preview group missing")?
            .extra = None;
        Ok(())
    }
}

pub(super) fn normalized(path: &Path, role: &str) -> Result<PathBuf> {
    let resolved = prospective(path)?;
    wire::path(&crate::storage_volume::NativePath::from_path(&resolved))
        .with_context(|| format!("preview {role} path admission"))?;
    Ok(resolved)
}
pub(super) fn object_text<'a>(row: &'a rusqlite::Row<'_>, column: usize) -> Result<&'a str> {
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(column)? else {
        bail!("cache object metadata is not TEXT")
    };
    ensure!(
        bytes.len() <= crate::catalog_session::ENVELOPE_BYTES,
        wire::ResourceLimit(
            "Saved cache object metadata exceeds the 1 MiB admission; no value was materialized or filesystem effect performed"
        )
    );
    Ok(std::str::from_utf8(bytes)?)
}
pub(super) fn identity(db: &Connection, layout: Layout) -> Result<String> {
    let mut statement = db.prepare("SELECT value FROM store_identity WHERE id=1")?;
    let mut rows = statement.query([])?;
    let row = rows.next()?.context("preview store identity missing")?;
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(0)? else {
        bail!("preview store identity is not TEXT")
    };
    ensure!(
        bytes.len() <= wire::MARKER_BYTES,
        wire::ResourceLimit(
            "Preview ownership identity exceeds the 256-byte serialized marker limit; no preview ownership marker was written"
        )
    );
    let identity = std::str::from_utf8(bytes)?;
    wire::marker(identity, Tier::Thumbnail, layout)?;
    wire::marker(identity, Tier::Large, layout)?;
    Ok(identity.to_owned())
}

/// Validate borrowed SQL cells before conversion; the legacy decoder and schema
/// stay authoritative, followed by the existing managed native-path admission.
pub(super) fn saved_paths(db: &Connection) -> Result<()> {
    for (table, sql, columns) in [
        ("locations", "SELECT path FROM locations LIMIT 3", 1),
        (
            "relocations",
            "SELECT source,target FROM relocations LIMIT 3",
            2,
        ),
    ] {
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if !exists {
            continue;
        }
        let mut statement = db.prepare(sql)?;
        let mut rows = statement.query([])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            ensure!(count <= 2, "invalid cache path row count");
            for column in 0..columns {
                match row.get_ref(column)? {
                    rusqlite::types::ValueRef::Text(bytes)
                    | rusqlite::types::ValueRef::Blob(bytes) => ensure!(
                        bytes.len() <= 1024 * 1024,
                        wire::ResourceLimit(
                            "Saved preview path exceeds the existing 1 MiB cell limit; no path was decoded or truncated"
                        )
                    ),
                    _ => bail!("invalid cache path storage type"),
                }
                normalized(&read_path(row, column)?, "saved")?;
            }
        }
    }
    Ok(())
}
pub(super) fn relocation(db: &Connection) -> Result<Option<wire::Relocation>> {
    let mut statement =
        db.prepare("SELECT id,tier,source,target,phase FROM relocations LIMIT 2")?;
    let mut rows = statement.query([])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let text = |column, maximum| -> Result<String> {
        let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(column)? else {
            bail!("invalid preview relocation text storage")
        };
        ensure!(
            bytes.len() <= maximum,
            "preview relocation field byte limit"
        );
        Ok(std::str::from_utf8(bytes)?.to_owned())
    };
    let id = text(0, 128)?;
    let tier = match text(1, 16)?.as_str() {
        "thumbnail" => Tier::Thumbnail,
        "large" => Tier::Large,
        _ => bail!("invalid preview relocation tier"),
    };
    let source = crate::storage_volume::NativePath::from_path(&normalized(
        &read_path(row, 2)?,
        "relocation source",
    )?);
    let target = crate::storage_volume::NativePath::from_path(&normalized(
        &read_path(row, 3)?,
        "relocation target",
    )?);
    let cleanup = match text(4, 16)?.as_str() {
        "copy" => false,
        "cleanup" => true,
        _ => bail!("invalid preview relocation phase"),
    };
    let result = wire::Relocation {
        id,
        tier,
        source,
        target,
        cleanup,
    };
    result.validate()?;
    ensure!(
        rows.next()?.is_none(),
        "multiple preview relocations cannot share one active lock slot"
    );
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_adapter_reply_retains_exact_request_until_replay_or_known_rejection() -> Result<()> {
        use crate::catalog_session as cs;
        use crate::filesystem_worker::wire::{Failure, FailureKind};
        struct Fake {
            typed_unknown: bool,
            requests: Mutex<[Option<wire::Request>; 4]>,
            count: std::sync::atomic::AtomicUsize,
        }
        impl cs::CatalogFilesystem for Fake {
            fn preview_store_call(
                &self,
                request: &wire::Request,
                _: &AtomicBool,
            ) -> Result<wire::Reply> {
                request.validate()?;
                let n = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ensure!(n < 4, "fake request capacity");
                self.requests.lock().unwrap()[n] = Some(request.clone());
                if n == 0 {
                    return Err(if self.typed_unknown {
                        Failure::new(FailureKind::Unknown, "lost reply after dispatch").into()
                    } else {
                        anyhow::anyhow!("transport lost reply")
                    });
                }
                if n == 2 {
                    return Err(Failure::new(
                        FailureKind::ResourceLimit,
                        "known reservation admission rejection",
                    )
                    .into());
                }
                let wire::Action::Reserve {
                    tier, destination, ..
                } = &request.action
                else {
                    bail!("unexpected fake method")
                };
                Ok(wire::Reply {
                    operation: request.operation,
                    value: wire::Value::Reserved(wire::Lease {
                        token: cs::LeaseId::new(),
                        tier: *tier,
                        path: destination.clone(),
                    }),
                })
            }
            fn prepare_catalog(
                &self,
                _: &cs::PrepareCatalog,
                _: &AtomicBool,
            ) -> Result<cs::CatalogBootstrap> {
                bail!("unused")
            }
            fn abandon_prepare(&self, _: crate::application::U64, _: &cs::LeaseId) -> Result<()> {
                bail!("unused")
            }
            fn confirm_sql_admission(
                &self,
                _: &cs::ConfirmSqlAdmission,
                _: &AtomicBool,
            ) -> Result<cs::SqlAdmissionConfirmed> {
                bail!("unused")
            }
            fn restore_status(
                &self,
                _: &cs::RootCapability,
            ) -> Result<Option<crate::catalog_backup::RestoreStatus>> {
                bail!("unused")
            }
            fn resume_restored_jobs(
                &self,
                _: &cs::RootCapability,
                _: &str,
                _: bool,
            ) -> Result<crate::catalog_backup::RestoreStatus> {
                bail!("unused")
            }
            fn release_root(&self, _: &cs::RootCapability) -> Result<()> {
                bail!("unused")
            }
        }
        let directory = tempfile::tempdir()?;
        let physical = crate::catalog_storage::physical_object_id(&tempfile::tempfile()?)?;
        for typed_unknown in [false, true] {
            let fake = Arc::new(Fake {
                typed_unknown,
                requests: Mutex::new(std::array::from_fn(|_| None)),
                count: std::sync::atomic::AtomicUsize::new(0),
            });
            let root = cs::RootCapability {
                epoch: cs::LeaseId::new(),
                token: cs::LeaseId::new(),
                session: cs::LeaseId::new(),
                canonical_root: crate::storage_volume::NativePath::from_path(directory.path()),
                root_physical: physical,
                catalog_physical: physical,
            };
            let adapter = ManagedFiles::new(
                fake.clone(),
                root,
                crate::storage_volume::NativePath::from_path(directory.path()),
                Arc::new(AtomicBool::new(false)),
            );
            let group = cs::LeaseId::new();
            let action = |name: &str| wire::Action::Reserve {
                group: group.clone(),
                tier: Tier::Thumbnail,
                destination: crate::storage_volume::NativePath::from_path(
                    &directory.path().join(name),
                ),
            };
            let original = action("first");
            assert!(adapter.call(original.clone()).is_err());
            assert_eq!(
                adapter
                    .pending
                    .lock()
                    .unwrap()
                    .pending
                    .as_ref()
                    .unwrap()
                    .operation
                    .0,
                1
            );
            assert!(adapter.call(action("conflict")).is_err());
            assert_eq!(
                fake.count.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "conflicting action dispatched"
            );
            adapter.call(original)?;
            assert!(adapter.pending.lock().unwrap().pending.is_none());
            let error = adapter.call(action("limited")).unwrap_err();
            assert_eq!(
                error.downcast_ref::<Failure>().unwrap().kind,
                FailureKind::ResourceLimit
            );
            assert!(adapter.pending.lock().unwrap().pending.is_none());
            adapter.call(action("next"))?;
            let requests = fake.requests.lock().unwrap();
            assert_eq!(
                requests[0], requests[1],
                "retry changed the operation or action"
            );
            assert_eq!(requests[2].as_ref().unwrap().operation.0, 2);
            assert_eq!(requests[3].as_ref().unwrap().operation.0, 3);
        }
        Ok(())
    }
    #[test]
    fn operation_counter_allows_gaps_but_never_wraps_or_reuses_an_identity() -> Result<()> {
        let mut calls = Calls {
            next: u64::MAX - 1,
            pending: None,
        };
        assert_eq!(calls.next_operation()?.0, u64::MAX - 1);
        assert_eq!(calls.next_operation()?.0, u64::MAX);
        assert!(
            calls
                .next_operation()
                .unwrap_err()
                .is::<wire::ResourceLimit>()
        );
        assert!(calls.next_operation().is_err());
        Ok(())
    }
    #[test]
    fn managed_identity_preserves_arbitrary_text_and_counts_exact_marker_before_copy() -> Result<()>
    {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE store_identity(id INTEGER PRIMARY KEY,value TEXT)")?;
        db.execute(
            "INSERT INTO store_identity VALUES(1,?1)",
            ["not a UUID: café"],
        )?;
        assert_eq!(identity(&db, Layout::Flat)?, "not a UUID: café");
        db.execute("UPDATE store_identity SET value=?1", ["x".repeat(257)])?;
        assert!(
            identity(&db, Layout::Flat)
                .unwrap_err()
                .is::<wire::ResourceLimit>()
        );
        db.execute("UPDATE store_identity SET value=?1", ["\\".repeat(240)])?;
        assert!(
            identity(&db, Layout::Flat)
                .unwrap_err()
                .is::<wire::ResourceLimit>()
        );
        db.execute("UPDATE store_identity SET value=x'6162'", [])?;
        assert!(identity(&db, Layout::Flat).is_err());
        Ok(())
    }
    #[test]
    fn saved_path_cell_limit_precedes_decode_and_managed_path_limit_never_truncates() -> Result<()>
    {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE locations(path BLOB)")?;
        db.execute(
            "INSERT INTO locations VALUES(?1)",
            [vec![0_u8; 1024 * 1024 + 1]],
        )?;
        assert!(saved_paths(&db).unwrap_err().is::<wire::ResourceLimit>());
        #[cfg(unix)]
        {
            let path = crate::storage_volume::NativePath::UnixBytes(
                [vec![b'/'], vec![b'x'; 32768]].concat(),
            );
            assert!(wire::path(&path).unwrap_err().is::<wire::ResourceLimit>());
            assert_eq!(
                match path {
                    crate::storage_volume::NativePath::UnixBytes(v) => v.len(),
                    _ => unreachable!(),
                },
                32769
            );
        }
        Ok(())
    }
}

#[path = "store_io.rs"]
mod object_io;
