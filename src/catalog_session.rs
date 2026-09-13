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
        ensure!(self.catalog.physical != self.manifest.physical,
            "catalog and preview manifest are the same object");
        ensure!(self.catalog.path.to_path()? == self.canonical_root.to_path()?.join("catalog.sqlite3"),
            "catalog admission path does not match root");
        ensure!(self.manifest.path.to_path()?.file_name().is_some_and(|n| n == "previews.sqlite3"),
            "invalid preview manifest admission path");
        Ok(())
    }
    pub fn root_capability(&self) -> RootCapability {
        RootCapability {
            epoch: self.epoch.clone(), token: self.token.clone(), session: self.session.clone(),
            canonical_root: self.canonical_root.clone(), root_physical: self.root_physical,
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
pub enum SqlRole { Actor, Relink, Export, Search0, Search1, Search2, Search3, Manifest }
pub const SQL_ROLES: [SqlRole; 8] = [SqlRole::Actor, SqlRole::Relink, SqlRole::Export,
    SqlRole::Search0, SqlRole::Search1, SqlRole::Search2, SqlRole::Search3, SqlRole::Manifest];

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
        ensure!(self.operation == bootstrap.operation && self.root == bootstrap.root_capability(),
            "admission confirmation belongs to another owner");
        for (observation, role) in self.roles.iter().zip(SQL_ROLES) {
            let expected = if role == SqlRole::Manifest { bootstrap.manifest.physical }
                else { bootstrap.catalog.physical };
            ensure!(observation.role == role && observation.physical == expected,
                "invalid admission role roster");
        }
        Ok(())
    }
}

/// The filesystem process echoes the complete exact confirmation only AFTER
/// checking that its same-epoch pins still overlap every observed SQL handle.
/// A health response, stale cached Prepare, EOF or lost reply is not this proof.
pub type SqlAdmissionConfirmed = ConfirmSqlAdmission;

/// Calls run on the admission/operation owner, never the GUI thread. An F client
/// must keep its independent cancel/status controls live while awaiting a reply.
/// Implementations must not fall back to local filesystem access after failure.
pub trait CatalogFilesystem: Send + Sync {
    /// A lost reply is recovered by the original operation identity inside the
    /// client. Never repeat Prepare/creation. An error/cancel can still leave an
    /// outstanding token, which the caller explicitly abandons before SQL opens.
    fn prepare_catalog(&self, request: &PrepareCatalog, cancel: &AtomicBool) -> Result<CatalogBootstrap>;
    /// Only before any SQLite open attempt; reconciles a lost Prepare by its
    /// original operation/session and releases pins without deleting evidence.
    fn abandon_prepare(&self, operation: U64, session: &LeaseId) -> Result<()>;
    fn confirm_sql_admission(&self, request: &ConfirmSqlAdmission, cancel: &AtomicBool)
        -> Result<SqlAdmissionConfirmed>;
    fn restore_status(&self, root: &RootCapability) -> Result<Option<RestoreStatus>>;
    fn resume_restored_jobs(&self, root: &RootCapability, restore_id: &str,
        acknowledge_pending_jobs: bool) -> Result<RestoreStatus>;
    /// Called only after dependent SQL/native ownership has been verified drained.
    fn release_root(&self, root: &RootCapability) -> Result<()>;
}

pub fn validate_path(path: &NativePath) -> Result<()> {
    let units = match path { NativePath::UnixBytes(v) => v.len(), NativePath::WindowsWide(v) => v.len() };
    ensure!((1..=PATH_UNITS).contains(&units), "native path admission limit");
    ensure!(path.to_path()?.is_absolute(), "admission path must be absolute");
    Ok(())
}
