//! Private desktop admission and exact catalog-session ownership.
//!
//! Scalar observations are evidence supplied by the filesystem owner. They do
//! not themselves construct a catalog authority or authorize a SQL statement.
use crate::{application::U64, catalog_backup::RestoreStatus, storage_volume::NativePath};
use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
use std::sync::atomic::AtomicBool;

pub const PATH_UNITS: usize = 32_768;
pub const ENVELOPE_BYTES: usize = 1024 * 1024;

/// The same native observation shape applies to a database or a directory.
/// Windows deliberately preserves the existing volume serial / 64-bit index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub enum PhysicalObjectId {
    Unix { device: U64, inode: U64 },
    Windows { volume_serial: U64, file_index: U64 },
}
impl PhysicalObjectId {
    pub fn validate(&self) -> Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix { .. } => Ok(()),
            #[cfg(windows)]
            Self::Windows { volume_serial, .. } => {
                ensure!(volume_serial.0 <= u32::MAX as u64, "invalid volume serial");
                Ok(())
            }
            #[allow(unreachable_patterns)]
            _ => anyhow::bail!("foreign physical object identity"),
        }
    }
}

/// Canonical, bounded identity; never accepted as an Arc session authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseId(String);
impl LeaseId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(value.len() == 36, "invalid lease identity length");
        let id = uuid::Uuid::parse_str(value)?;
        ensure!(id.to_string() == value, "noncanonical lease identity");
        Ok(Self(value.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Default for LeaseId {
    fn default() -> Self {
        Self::new()
    }
}
impl Serialize for LeaseId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for LeaseId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapMode {
    DesktopCreate,
    DesktopExisting,
    OpenOrCreate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareCatalog {
    pub operation: U64,
    pub session: LeaseId,
    pub mode: BootstrapMode,
    pub root: NativePath,
    pub manifest_root: NativePath,
    pub import_source: Option<NativePath>,
}
impl PrepareCatalog {
    /// Managed C resolves directory aliases prospectively using stat/readlink
    /// only, before F creates anything. F still enforces the original mode's
    /// existing/create semantics and rechecks the resulting directory objects.
    fn resolved(&self) -> Result<Self> {
        self.validate()?;
        let mut request = self.clone();
        request.root = NativePath::from_path(&crate::prospective_directory(&self.root.to_path()?)?);
        request.manifest_root = NativePath::from_path(&crate::prospective_directory(
            &self.manifest_root.to_path()?,
        )?);
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "invalid admission operation");
        validate_path(&self.root)?;
        validate_path(&self.manifest_root)?;
        if let Some(path) = &self.import_source {
            validate_path(path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedDatabase {
    pub path: NativePath,
    pub physical: PhysicalObjectId,
    /// True only when THIS retained admission created the file with create-new.
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogBootstrap {
    pub version: u8,
    pub operation: U64,
    pub epoch: LeaseId,
    pub token: LeaseId,
    pub session: LeaseId,
    pub canonical_root: NativePath,
    pub root_physical: PhysicalObjectId,
    pub catalog: PinnedDatabase,
    pub manifest: PinnedDatabase,
}
impl CatalogBootstrap {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported catalog admission version");
        ensure!(self.operation.0 > 0, "invalid admission operation");
        validate_path(&self.canonical_root)?;
        self.root_physical.validate()?;
        for db in [&self.catalog, &self.manifest] {
            validate_path(&db.path)?;
            db.physical.validate()?;
        }
        ensure!(
            self.catalog.physical != self.manifest.physical,
            "catalog and preview manifest are the same object"
        );
        ensure!(
            self.catalog.path.to_path()? == self.canonical_root.to_path()?.join("catalog.sqlite3"),
            "catalog admission path does not match root"
        );
        ensure!(
            self.manifest
                .path
                .to_path()?
                .file_name()
                .is_some_and(|n| n == "previews.sqlite3"),
            "invalid preview manifest admission path"
        );
        Ok(())
    }
    pub fn root_capability(&self) -> RootCapability {
        RootCapability {
            epoch: self.epoch.clone(),
            token: self.token.clone(),
            session: self.session.clone(),
            canonical_root: self.canonical_root.clone(),
            root_physical: self.root_physical,
            catalog_physical: self.catalog.physical,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootCapability {
    pub epoch: LeaseId,
    pub token: LeaseId,
    pub session: LeaseId,
    pub canonical_root: NativePath,
    pub root_physical: PhysicalObjectId,
    pub catalog_physical: PhysicalObjectId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlRole {
    Actor,
    Relink,
    Export,
    Search0,
    Search1,
    Search2,
    Search3,
    Manifest,
}
pub const SQL_ROLES: [SqlRole; 8] = [
    SqlRole::Actor,
    SqlRole::Relink,
    SqlRole::Export,
    SqlRole::Search0,
    SqlRole::Search1,
    SqlRole::Search2,
    SqlRole::Search3,
    SqlRole::Manifest,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlRoleObservation {
    pub role: SqlRole,
    pub physical: PhysicalObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmSqlAdmission {
    pub operation: U64,
    pub root: RootCapability,
    pub roles: [SqlRoleObservation; 8],
}
impl ConfirmSqlAdmission {
    pub fn validate_for(&self, bootstrap: &CatalogBootstrap) -> Result<()> {
        ensure!(
            self.operation == bootstrap.operation && self.root == bootstrap.root_capability(),
            "admission confirmation belongs to another owner"
        );
        for (observation, role) in self.roles.iter().zip(SQL_ROLES) {
            let expected = if role == SqlRole::Manifest {
                bootstrap.manifest.physical
            } else {
                bootstrap.catalog.physical
            };
            ensure!(
                observation.role == role && observation.physical == expected,
                "invalid admission role roster"
            );
        }
        Ok(())
    }
}

/// The filesystem process echoes the complete exact confirmation only AFTER
/// checking that its same-epoch pins still overlap every observed SQL handle.
/// A health response, stale cached Prepare, EOF or lost reply is not this proof.
pub type SqlAdmissionConfirmed = ConfirmSqlAdmission;

pub mod native;
pub mod preview_io;
pub mod preview_stage;
/// Calls run on the admission/operation owner, never the GUI thread. An F client
/// must keep its independent cancel/status controls live while awaiting a reply.
/// Implementations must not fall back to local filesystem access after failure.
pub mod store;

pub trait CatalogFilesystem: Send + Sync {
    fn native(&self) -> Option<&dyn native::CatalogNative> {
        None
    }
    fn preview_stage_call(
        &self,
        _request: &preview_stage::Request,
        _cancel: &AtomicBool,
    ) -> Result<preview_stage::Reply> {
        anyhow::bail!("filesystem owner does not support stage custody")
    }
    fn preview_io_call(
        &self,
        _request: &preview_io::Request,
        _cancel: &AtomicBool,
    ) -> Result<preview_io::Reply> {
        anyhow::bail!("filesystem owner does not support cache object custody")
    }

    fn preview_store_call(
        &self,
        _request: &store::Request,
        _cancel: &AtomicBool,
    ) -> Result<store::Reply> {
        anyhow::bail!("filesystem owner does not support preview custody")
    }
    fn preview_store_status(&self, _query: &store::Query) -> Result<store::Status> {
        anyhow::bail!("filesystem owner does not support preview custody status")
    }
    fn read_preview_configuration(
        &self,
        _path: &NativePath,
        _cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        anyhow::bail!("filesystem owner does not support preview configuration reads")
    }
    /// A lost reply is recovered by the original operation identity inside the
    /// client. Never repeat Prepare/creation. An error/cancel can still leave an
    /// outstanding token, which the caller explicitly abandons before SQL opens.
    fn prepare_catalog(
        &self,
        request: &PrepareCatalog,
        cancel: &AtomicBool,
    ) -> Result<CatalogBootstrap>;
    /// Only before any SQLite open attempt; reconciles a lost Prepare by its
    /// original operation/session and releases pins without deleting evidence.
    fn abandon_prepare(&self, operation: U64, session: &LeaseId) -> Result<()>;
    fn confirm_sql_admission(
        &self,
        request: &ConfirmSqlAdmission,
        cancel: &AtomicBool,
    ) -> Result<SqlAdmissionConfirmed>;
    fn restore_status(&self, root: &RootCapability) -> Result<Option<RestoreStatus>>;
    fn resume_restored_jobs(
        &self,
        root: &RootCapability,
        restore_id: &str,
        acknowledge_pending_jobs: bool,
    ) -> Result<RestoreStatus>;
    /// Called only after dependent SQL/native ownership has been verified drained.
    fn release_root(&self, root: &RootCapability) -> Result<()>;
}

pub fn validate_path(path: &NativePath) -> Result<()> {
    let units = match path {
        NativePath::UnixBytes(v) => v.len(),
        NativePath::WindowsWide(v) => v.len(),
    };
    ensure!(
        (1..=PATH_UNITS).contains(&units),
        "native path admission limit"
    );
    ensure!(
        path.to_path()?.is_absolute(),
        "admission path must be absolute"
    );
    Ok(())
}

mod roles;
use crate::{Catalog, catalog_writer};
pub(crate) use roles::DISCOVERY as DISCOVERY_ROLE;
pub(crate) use roles::index as role_index;
pub(crate) use roles::{RolePool, SqlConnection};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::File,
    mem::ManuallyDrop,
    path::Path,
    sync::{Arc, Mutex, atomic::Ordering},
};

pub(crate) trait SessionTask: Send + Sync {
    fn request_cancel(&self);
    fn is_finished(&self) -> bool;
    fn join(&self) -> Result<()>;
}

enum AuthorityMode {
    Legacy(Arc<File>),
    Managed {
        filesystem: Arc<dyn CatalogFilesystem>,
        root: RootCapability,
        pool: Arc<RolePool>,
    },
}
/// The Arc instance, not its serialized epoch/physical tuple, grants a prepared
/// edit or native permit authority within the original catalog lifetime.
pub(crate) struct CatalogSessionAuthority {
    physical: PhysicalObjectId,
    mode: AuthorityMode,
    searches: Mutex<Vec<Arc<dyn SessionTask>>>,
}
impl CatalogSessionAuthority {
    pub(crate) fn legacy(file: Arc<File>) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            physical: crate::catalog_storage::physical_object_id(&file)?,
            mode: AuthorityMode::Legacy(file),
            searches: Mutex::new(Vec::new()),
        }))
    }
    pub(crate) fn legacy_file(&self) -> Result<&Arc<File>> {
        match &self.mode {
            AuthorityMode::Legacy(file) => Ok(file),
            _ => anyhow::bail!("managed catalog has no raw database file"),
        }
    }
    pub(crate) fn pool(&self) -> Option<&Arc<RolePool>> {
        match &self.mode {
            AuthorityMode::Managed { pool, .. } => Some(pool),
            _ => None,
        }
    }
    pub(crate) fn require_jobs_released(&self, legacy_root: &Path) -> Result<()> {
        if matches!(&self.mode, AuthorityMode::Legacy(_)) {
            return crate::catalog_backup::require_jobs_released(legacy_root);
        }
        if let Some(status) = self.restore_status(legacy_root)? {
            ensure!(
                !status.jobs_held,
                "restored external jobs are held; explicitly resume restore {} and acknowledge preexisting jobs",
                status.receipt.restore_id
            );
        }
        Ok(())
    }
    pub(crate) fn restore_status(&self, legacy_root: &Path) -> Result<Option<RestoreStatus>> {
        match &self.mode {
            AuthorityMode::Legacy(_) => crate::catalog_backup::restore_status(legacy_root),
            AuthorityMode::Managed {
                filesystem, root, ..
            } => filesystem.restore_status(root),
        }
    }
    pub(crate) fn resume(
        &self,
        legacy_root: &Path,
        id: &str,
        acknowledge: bool,
    ) -> Result<RestoreStatus> {
        match &self.mode {
            AuthorityMode::Legacy(_) => {
                crate::catalog_backup::resume_restored_jobs(legacy_root, id, acknowledge)
            }
            AuthorityMode::Managed {
                filesystem, root, ..
            } => filesystem.resume_restored_jobs(root, id, acknowledge),
        }
    }
    pub(crate) fn export_matches(left: &Arc<Self>, right: &Arc<Self>) -> Result<bool> {
        match (&left.mode, &right.mode) {
            (AuthorityMode::Legacy(a), AuthorityMode::Legacy(b)) => {
                Ok(crate::storage_volume::held_object_key(a)?
                    == crate::storage_volume::held_object_key(b)?)
            }
            (AuthorityMode::Managed { .. }, AuthorityMode::Managed { .. }) => {
                Ok(Arc::ptr_eq(left, right) && left.physical == right.physical)
            }
            _ => Ok(false),
        }
    }
    pub(crate) fn reap_finished_searches(&self) -> Result<()> {
        let mut tasks = self.searches.lock().unwrap_or_else(|e| e.into_inner());
        let mut i = 0;
        while i < tasks.len() {
            if tasks[i].is_finished() {
                tasks[i].join()?;
                tasks.remove(i);
            } else {
                i += 1;
            }
        }
        Ok(())
    }
    pub(crate) fn register_search(&self, owner: Arc<dyn SessionTask>) -> Result<()> {
        let mut tasks = self.searches.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            self.pool().is_none_or(|pool| pool.is_open()),
            "catalog session is closing"
        );
        ensure!(tasks.len() < 4, "four browsing snapshots are still owned");
        tasks.push(owner);
        Ok(())
    }
    pub(crate) fn cancel_searches(&self) {
        for task in self
            .searches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            task.request_cancel();
        }
    }
    pub(crate) fn drain_searches(&self) -> Result<()> {
        self.cancel_searches();
        let mut tasks = self.searches.lock().unwrap_or_else(|e| e.into_inner());
        for task in tasks.iter() {
            task.join()?;
        }
        tasks.clear();
        Ok(())
    }
    pub(crate) fn joined(&self, role: SqlRole, healthy: bool) -> Result<()> {
        if let Some(pool) = self.pool() {
            pool.joined(roles::index(role), healthy)?;
        }
        Ok(())
    }
}

/// No destructor in this owner invokes Connection::drop, SQL or rollback. A
/// poisoned admission MUST retire its bootstrap process, not continue dispatch.
pub(crate) struct Candidates(Vec<ManuallyDrop<Connection>>);
impl Candidates {
    fn into_confirmed(mut self) -> Vec<Connection> {
        self.0.drain(..).map(ManuallyDrop::into_inner).collect()
    }
}
pub(crate) enum AdmissionCleanup {
    Prepare {
        filesystem: Arc<dyn CatalogFilesystem>,
        request: PrepareCatalog,
        complete: bool,
    },
    Sql {
        connections: Vec<Connection>,
        filesystem: Arc<dyn CatalogFilesystem>,
        root: RootCapability,
        complete: bool,
    },
}
impl AdmissionCleanup {
    pub(crate) fn session(&self) -> &LeaseId {
        match self {
            Self::Prepare { request, .. } => &request.session,
            Self::Sql { root, .. } => &root.session,
        }
    }
    pub(crate) fn is_complete(&self) -> bool {
        match self {
            Self::Prepare { complete, .. } | Self::Sql { complete, .. } => *complete,
        }
    }
    pub(crate) fn close(&mut self) -> Result<()> {
        match self {
            Self::Prepare {
                filesystem,
                request,
                complete,
            } => {
                if !*complete {
                    filesystem.abandon_prepare(request.operation, &request.session)?;
                    *complete = true;
                }
            }
            Self::Sql {
                connections,
                filesystem,
                root,
                complete,
            } => {
                if *complete {
                    return Ok(());
                }
                let mut failed = Vec::new();
                for db in std::mem::take(connections) {
                    if let Err((db, _)) = db.close() {
                        failed.push(db);
                    }
                }
                *connections = failed;
                ensure!(
                    connections.is_empty(),
                    "SQLite admission cleanup failed; exact owner retained"
                );
                filesystem.release_root(root)?;
                *complete = true;
            }
        }
        Ok(())
    }
}
impl Drop for AdmissionCleanup {
    fn drop(&mut self) {
        // Never retry a failed/unknown release from a destructor.
        match self {
            Self::Prepare {
                filesystem,
                complete: false,
                ..
            } => std::mem::forget(filesystem.clone()),
            Self::Sql {
                connections,
                filesystem,
                complete: false,
                ..
            } => {
                std::mem::forget(std::mem::take(connections));
                std::mem::forget(filesystem.clone());
            }
            _ => {}
        }
    }
}
pub(crate) enum AdmissionFailure {
    BeforeOpen {
        error: anyhow::Error,
        cleanup: Option<AdmissionCleanup>,
    },
    Poisoned {
        error: anyhow::Error,
        _candidates: Candidates,
        // Retain even when this was the final proxy reference. No capability
        // destructor/release may run before bootstrap process retirement.
        _filesystem: ManuallyDrop<Arc<dyn CatalogFilesystem>>,
    },
    Confirmed {
        error: anyhow::Error,
        cleanup: AdmissionCleanup,
    },
}
impl std::fmt::Display for AdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeOpen { error: e, .. }
            | Self::Confirmed { error: e, .. }
            | Self::Poisoned { error: e, .. } => write!(f, "{e:#}"),
        }
    }
}
impl std::fmt::Debug for AdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl AdmissionFailure {
    pub(crate) fn into_cleanup(self) -> Option<AdmissionCleanup> {
        match self {
            Self::BeforeOpen { cleanup, .. } => cleanup,
            Self::Confirmed { cleanup, .. } => Some(cleanup),
            Self::Poisoned { .. } => unreachable!("poisoned admission must retire its process"),
        }
    }
    pub(crate) fn is_poisoned(&self) -> bool {
        matches!(self, Self::Poisoned { .. })
    }
    /// Only the private bootstrap C entry point may call this, after proving it
    /// owns no native descendants. process::exit deliberately skips SQL drops.
    pub(crate) fn retire_poisoned(self) -> ! {
        assert!(self.is_poisoned());
        // No pipe write may delay retirement when the GUI stopped reading.
        std::mem::forget(self);
        std::process::exit(74)
    }
}

pub(crate) struct ManagedSession {
    pub(crate) authority: Arc<CatalogSessionAuthority>,
    pub(crate) catalog: Option<Catalog>,
    pub(crate) bootstrap: CatalogBootstrap,
    close_attempted: bool,
    pub(crate) sql_returned: bool,
}
impl ManagedSession {
    #[allow(
        clippy::result_large_err,
        reason = "Admission errors retain inline cleanup ownership; avoid adding allocation at the bootstrap failure boundary."
    )]
    pub(crate) fn admit(
        filesystem: Arc<dyn CatalogFilesystem>,
        request: &PrepareCatalog,
        cancel: &AtomicBool,
    ) -> std::result::Result<Self, AdmissionFailure> {
        Self::admit_observed(filesystem, request, cancel, |_| {})
    }
    #[allow(
        clippy::result_large_err,
        reason = "Admission errors retain inline cleanup ownership; avoid adding allocation at the bootstrap failure boundary."
    )]
    fn admit_observed(
        filesystem: Arc<dyn CatalogFilesystem>,
        request: &PrepareCatalog,
        cancel: &AtomicBool,
        observed: impl Fn(&Connection),
    ) -> std::result::Result<Self, AdmissionFailure> {
        let request = request
            .resolved()
            .map_err(|error| AdmissionFailure::BeforeOpen {
                error,
                cleanup: None,
            })?;
        let request = &request;
        let bootstrap = (|| -> Result<CatalogBootstrap> {
            ensure!(
                !cancel.load(Ordering::Acquire),
                "catalog admission canceled"
            );
            let value = filesystem.prepare_catalog(request, cancel)?;
            value.validate()?;
            ensure!(
                value.operation == request.operation && value.session == request.session,
                "catalog bootstrap belongs to another request"
            );
            ensure!(
                value.canonical_root == request.root,
                "catalog bootstrap belongs to another resolved root"
            );
            ensure!(
                value.manifest.path.to_path()?
                    == request.manifest_root.to_path()?.join("previews.sqlite3"),
                "manifest bootstrap belongs to another cache"
            );
            ensure!(
                !cancel.load(Ordering::Acquire),
                "catalog admission canceled"
            );
            Ok(value)
        })();
        let bootstrap = match bootstrap {
            Ok(value) => value,
            Err(e) => {
                let mut cleanup = AdmissionCleanup::Prepare {
                    filesystem: filesystem.clone(),
                    request: request.clone(),
                    complete: false,
                };
                let result = cleanup.close();
                return Err(AdmissionFailure::BeforeOpen {
                    error: e,
                    cleanup: if result.is_ok() { None } else { Some(cleanup) },
                });
            }
        };
        let mut candidates = Candidates(Vec::with_capacity(8));
        let verified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            let mut observations = Vec::with_capacity(8);
            for role in SQL_ROLES {
                let pinned = if role == SqlRole::Manifest {
                    &bootstrap.manifest
                } else {
                    &bootstrap.catalog
                };
                let readonly = matches!(
                    role,
                    SqlRole::Search0 | SqlRole::Search1 | SqlRole::Search2 | SqlRole::Search3
                );
                let flags = OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | if readonly {
                        OpenFlags::SQLITE_OPEN_READ_ONLY
                    } else {
                        OpenFlags::SQLITE_OPEN_READ_WRITE
                    };
                let connection = Connection::open_with_flags(pinned.path.to_path()?, flags)?;
                candidates.0.push(ManuallyDrop::new(connection));
                let connection = candidates.0.last().unwrap();
                observed(connection);
                crate::catalog_storage::verify_database_identity(connection, &pinned.physical)?;
                ensure!(
                    connection.is_readonly("main")? == readonly,
                    "SQLite role access mode differs from admission"
                );
                observations.push(SqlRoleObservation {
                    role,
                    physical: pinned.physical,
                });
                ensure!(
                    !cancel.load(Ordering::Acquire),
                    "catalog admission canceled after SQL open"
                );
            }
            let confirmation = ConfirmSqlAdmission {
                operation: bootstrap.operation,
                root: bootstrap.root_capability(),
                roles: observations.try_into().expect("fixed eight roles"),
            };
            confirmation.validate_for(&bootstrap)?;
            let reply = filesystem.confirm_sql_admission(&confirmation, cancel)?;
            ensure!(
                reply == confirmation,
                "filesystem confirmation differs from exact SQL admission"
            );
            Ok(())
        }));
        if !matches!(verified, Ok(Ok(()))) {
            let error = match verified {
                Ok(Err(e)) => e,
                Err(_) => anyhow::anyhow!("panic during unconfirmed SQLite admission"),
                _ => unreachable!(),
            };
            return Err(AdmissionFailure::Poisoned {
                error,
                _candidates: candidates,
                _filesystem: ManuallyDrop::new(filesystem),
            });
        }
        let mut connections = candidates.into_confirmed();
        let root = bootstrap
            .canonical_root
            .to_path()
            .expect("validated native root");
        let writers = catalog_writer::for_catalog(&root);
        let initialized =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                crate::initialize_catalog_connection(&mut connections[0], &writers, |_| Ok(()))?;
                for connection in &connections[1..3] {
                    crate::configure_catalog_connection(connection)?;
                }
                for connection in &connections[3..7] {
                    connection.busy_timeout(std::time::Duration::from_secs(5))?;
                    connection.execute_batch(
                        "PRAGMA cache_size=-262144; PRAGMA mmap_size=0; PRAGMA temp_store=1;",
                    )?;
                }
                // The qualified exclusive TEMP_DB VFS path is required before this
                // unselected route may be enabled. SQLite owns all lazy spill files.
                let discovery = Connection::open("")?;
                connections.push(discovery);
                crate::catalog_metadata::initialize_discovery_connection(
                    connections.last().unwrap(),
                )?;
                Ok(())
            }));
        let initialized = match initialized {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "panic during confirmed catalog initialization"
            )),
        };
        if let Err(error) = initialized {
            let mut cleanup = AdmissionCleanup::Sql {
                connections,
                filesystem,
                root: bootstrap.root_capability(),
                complete: false,
            };
            let _ = cleanup.close();
            return Err(AdmissionFailure::Confirmed { error, cleanup });
        }
        let expected = std::array::from_fn(|i| {
            if i == 7 {
                bootstrap.manifest.physical
            } else {
                bootstrap.catalog.physical
            }
        });
        let pool = RolePool::new(
            connections,
            expected,
            filesystem.clone(),
            bootstrap.root_capability(),
        );
        let authority = Arc::new(CatalogSessionAuthority {
            physical: bootstrap.catalog.physical,
            mode: AuthorityMode::Managed {
                filesystem,
                root: bootstrap.root_capability(),
                pool: pool.clone(),
            },
            searches: Mutex::new(Vec::new()),
        });
        let db = pool.lease(0).expect("new confirmed actor role available");
        let catalog = Catalog {
            db,
            root,
            writers,
            session: authority.clone(),
        };
        Ok(Self {
            authority,
            catalog: Some(catalog),
            bootstrap,
            close_attempted: false,
            sql_returned: false,
        })
    }
    pub(crate) fn manifest(&self) -> Result<SqlConnection> {
        self.authority.pool().unwrap().lease(7)
    }
    pub(crate) fn close(&mut self) -> Result<()> {
        self.close_attempted = true;
        self.authority.pool().unwrap().begin_close();
        self.authority.cancel_searches();
        self.authority.drain_searches()?;
        self.catalog.take();
        let _ = self.authority.joined(SqlRole::Actor, true);
        let _ = self.authority.joined(SqlRole::Manifest, true);
        self.authority.pool().unwrap().close()
    }
}
impl Drop for ManagedSession {
    fn drop(&mut self) {
        if self.authority.pool().unwrap().is_closed() {
            return;
        }
        if self.close_attempted || self.close().is_err() {
            std::mem::forget(self.authority.clone());
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::{retained_admission, unused_filesystem};

#[cfg(test)]
pub(crate) mod overlap_tests;
