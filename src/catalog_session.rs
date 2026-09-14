//! Private desktop admission and exact catalog-session ownership.
//!
//! Scalar observations are evidence supplied by the filesystem owner. They do
//! not themselves construct a catalog authority or authorize a SQL statement.
use crate::{application::U64, catalog_backup::RestoreStatus, storage_volume::NativePath};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
use std::sync::atomic::AtomicBool;

pub const PATH_UNITS: usize = 32_768;
pub const ENVELOPE_BYTES: usize = 1024 * 1024;
pub const EXPORT_PROFILE_BYTES: usize = 16 * 1024 * 1024;

fn validate_export_revision(
    revision: &crate::metadata_export::FileRevision,
    allowance: u64,
) -> Result<()> {
    ensure!(
        revision.bytes <= allowance
            && revision.digest.len() == 64
            && revision.digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid export original revision"
    );
    Ok(())
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareExportDirectory {
    pub root: RootCapability,
    pub directory: NativePath,
}
impl PrepareExportDirectory {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        validate_path(&self.directory)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedExportDirectory {
    pub root: RootCapability,
    pub requested: NativePath,
    pub directory: NativePath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportAliasFactKind {
    Destination,
    File,
    Directory,
    CanonicalFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportObjectKey {
    pub volume: U64,
    /// Decimal preserves the complete platform object identifier through JSON.
    pub object: String,
}
impl ExportObjectKey {
    pub(crate) fn from_native(value: (u64, u128)) -> Self {
        Self {
            volume: U64(value.0),
            object: value.1.to_string(),
        }
    }
    pub(crate) fn native(&self) -> Result<(u64, u128)> {
        let object = self.object.parse::<u128>()?;
        ensure!(
            object.to_string() == self.object,
            "invalid export object identity"
        );
        Ok((self.volume.0, object))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportAliasFactRequest {
    pub root: RootCapability,
    pub path: NativePath,
    pub kind: ExportAliasFactKind,
}
impl ExportAliasFactRequest {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        validate_path(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExportAliasFactValue {
    Missing,
    File {
        object: ExportObjectKey,
        canonical: Option<NativePath>,
    },
    Directory {
        object: ExportObjectKey,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportAliasFactReply {
    pub root: RootCapability,
    pub path: NativePath,
    pub kind: ExportAliasFactKind,
    pub value: ExportAliasFactValue,
}
impl ExportAliasFactReply {
    pub fn validate_for(&self, request: &ExportAliasFactRequest) -> Result<()> {
        request.validate()?;
        ensure!(
            self.root == request.root && self.path == request.path && self.kind == request.kind,
            "export alias fact belongs to another request"
        );
        match (&request.kind, &self.value) {
            (_, ExportAliasFactValue::Missing) => Ok(()),
            (
                ExportAliasFactKind::Destination | ExportAliasFactKind::File,
                ExportAliasFactValue::File {
                    object,
                    canonical: None,
                },
            ) => {
                object.native()?;
                Ok(())
            }
            (
                ExportAliasFactKind::CanonicalFile,
                ExportAliasFactValue::File {
                    object,
                    canonical: Some(path),
                },
            ) => {
                object.native()?;
                validate_path(path)
            }
            (ExportAliasFactKind::Directory, ExportAliasFactValue::Directory { object }) => {
                object.native()?;
                Ok(())
            }
            _ => anyhow::bail!("unexpected export alias fact"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportDestinationSnapshotRequest {
    pub root: RootCapability,
    pub destination: NativePath,
    pub max_existing_bytes: U64,
}
impl ExportDestinationSnapshotRequest {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        validate_path(&self.destination)?;
        ensure!(
            self.max_existing_bytes.0 > 0,
            "zero destination snapshot allowance"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportDestinationSnapshotReply {
    pub root: RootCapability,
    pub requested: NativePath,
    pub snapshot: crate::metadata_export::DestinationSnapshot,
}
impl ExportDestinationSnapshotReply {
    pub fn validate_for(&self, request: &ExportDestinationSnapshotRequest) -> Result<()> {
        request.validate()?;
        ensure!(
            self.root == request.root
                && self.requested == request.destination
                && self.snapshot.max_existing_bytes == request.max_existing_bytes.0,
            "export destination snapshot belongs to another request"
        );
        crate::metadata_export::validate_destination_snapshot_wire(&self.snapshot)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectExportOriginal {
    pub root: RootCapability,
    pub requested: NativePath,
    pub allowance: U64,
}
impl InspectExportOriginal {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        validate_path(&self.requested)?;
        ensure!(self.allowance.0 > 0, "zero export original allowance");
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedExportOriginal {
    pub root: RootCapability,
    pub requested: NativePath,
    pub allowance: U64,
    pub revision: crate::metadata_export::FileRevision,
}
impl InspectedExportOriginal {
    pub fn validate_for(&self, request: &InspectExportOriginal) -> Result<()> {
        request.validate()?;
        ensure!(
            self.root == request.root
                && self.requested == request.requested
                && self.allowance == request.allowance,
            "inspected export original belongs to another request"
        );
        validate_export_revision(&self.revision, request.allowance.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportOriginalAction {
    Begin,
    Recheck,
    Finish,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportOriginalRequest {
    pub root: RootCapability,
    pub requested: NativePath,
    pub transfer: LeaseId,
    pub step: U64,
    pub allowance: U64,
    pub action: ExportOriginalAction,
}
impl ExportOriginalRequest {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        validate_path(&self.requested)?;
        ensure!(self.allowance.0 > 0, "zero export original allowance");
        ensure!(
            matches!(self.action, ExportOriginalAction::Begin) == (self.step.0 == 0),
            "export original begin/step mismatch"
        );
        Ok(())
    }
    pub fn cleanup(&self) -> bool {
        matches!(
            self.action,
            ExportOriginalAction::Finish | ExportOriginalAction::Abort
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExportOriginalValue {
    Begun {
        revision: crate::metadata_export::FileRevision,
    },
    Rechecked,
    Finished,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportOriginalReply {
    pub root: RootCapability,
    pub requested: NativePath,
    pub transfer: LeaseId,
    pub step: U64,
    pub value: ExportOriginalValue,
}
impl ExportOriginalReply {
    pub fn validate(&self, request: &ExportOriginalRequest) -> Result<()> {
        request.validate()?;
        ensure!(
            self.root == request.root
                && self.requested == request.requested
                && self.transfer == request.transfer
                && self.step == request.step,
            "export original reply provenance mismatch"
        );
        match (&request.action, &self.value) {
            (ExportOriginalAction::Begin, ExportOriginalValue::Begun { revision }) => {
                validate_export_revision(revision, request.allowance.0)
            }
            (ExportOriginalAction::Recheck, ExportOriginalValue::Rechecked)
            | (ExportOriginalAction::Finish, ExportOriginalValue::Finished)
            | (ExportOriginalAction::Abort, ExportOriginalValue::Aborted) => Ok(()),
            _ => anyhow::bail!("unexpected export original reply"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportPublicationMode {
    Publish,
    Restore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExportPublicationSource {
    Sealed(crate::metadata_export::SealedPhotoExport),
    Recovery {
        snapshot: crate::metadata_export::DestinationSnapshot,
        authority_digest: String,
    },
}
impl ExportPublicationSource {
    fn validate(&self, mode: ExportPublicationMode) -> Result<()> {
        match self {
            Self::Sealed(seal) => crate::metadata_export::validate_photo_seal_wire(seal),
            Self::Recovery {
                snapshot,
                authority_digest,
            } => {
                ensure!(
                    mode == ExportPublicationMode::Restore,
                    "publication recovery source requires restore mode"
                );
                crate::metadata_export::validate_destination_snapshot_wire(snapshot)?;
                ensure!(
                    authority_digest.len() == 64
                        && authority_digest
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit()),
                    "invalid publication recovery authority"
                );
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExportPublicationAction {
    Begin,
    RecheckPayload,
    Capture,
    VerifyCapture,
    FailureReceipt { detail: String },
    Link,
    VerifyInstalled,
    RecheckInstalled,
    RestoreLink,
    VerifyRestored,
    RecheckRestored,
    Finish,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportPublicationRequest {
    pub root: RootCapability,
    pub transfer: LeaseId,
    pub step: U64,
    pub mode: ExportPublicationMode,
    pub source: ExportPublicationSource,
    pub action: ExportPublicationAction,
}
impl ExportPublicationRequest {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        self.source.validate(self.mode)?;
        ensure!(
            matches!(self.action, ExportPublicationAction::Begin) == (self.step.0 == 0),
            "publication begin/step mismatch"
        );
        if let ExportPublicationAction::FailureReceipt { detail } = &self.action {
            ensure!(detail.len() <= 8192, "publication failure detail limit");
        }
        Ok(())
    }
    pub(crate) fn digest(&self) -> Result<String> {
        Ok(blake3::hash(&crate::filesystem_worker::wire::encode(
            self,
            crate::filesystem_worker::wire::MESSAGE_BYTES,
        )?)
        .to_hex()
        .to_string())
    }
    pub fn cleanup(&self) -> bool {
        matches!(
            self.action,
            ExportPublicationAction::Finish | ExportPublicationAction::Abort
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExportPublicationValue {
    /// F admitted and consumed this exact step. Outer transport errors never
    /// establish consumption; even cancellation during a hash is cached here.
    Failed(crate::filesystem_worker::wire::Failure),
    Begun {
        installed: bool,
    },
    RecheckedPayload,
    Captured,
    CaptureVerified,
    Receipt(crate::metadata_export::ExportReceipt),
    Linked,
    Installed(crate::metadata_export::ExportReceipt),
    RecheckedInstalled,
    RestoredLinked,
    Restored(crate::metadata_export::ExportReceipt),
    RecheckedRestored,
    Finished,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportPublicationReply {
    pub mode: ExportPublicationMode,
    pub request_digest: String,
    pub root: RootCapability,
    pub transfer: LeaseId,
    pub step: U64,
    pub seal: crate::metadata_export::SealedPhotoExport,
    pub value: ExportPublicationValue,
    pub timings: crate::metadata_export::PhotoPublicationTimings,
    pub hashed_bytes: U64,
}
impl ExportPublicationReply {
    pub fn validate(&self, request: &ExportPublicationRequest) -> Result<()> {
        request.validate()?;
        ensure!(
            self.root == request.root
                && self.transfer == request.transfer
                && self.step == request.step
                && self.mode == request.mode
                && self.request_digest == request.digest()?,
            "publication reply provenance mismatch"
        );
        ensure!(
            [
                self.timings.hash_ms,
                self.timings.capture_ms,
                self.timings.link_ms,
                self.timings.durability_ms,
            ]
            .into_iter()
            .all(|value| value.is_finite() && value >= 0.0),
            "invalid publication timing"
        );
        crate::metadata_export::validate_photo_seal_wire(&self.seal)?;
        match &request.source {
            ExportPublicationSource::Sealed(expected) => {
                ensure!(&self.seal == expected, "publication reply seal mismatch")
            }
            ExportPublicationSource::Recovery {
                snapshot,
                authority_digest,
            } => ensure!(
                &self.seal.snapshot == snapshot && &self.seal.authority_digest == authority_digest,
                "publication recovery seal mismatch"
            ),
        }
        let seal = &self.seal;
        if let ExportPublicationValue::Failed(failure) = &self.value {
            ensure!(
                !matches!(request.action, ExportPublicationAction::Begin),
                "Begin cannot acknowledge a retained failed step"
            );
            failure.validate()?;
            ensure!(
                failure.object_receipt.is_none(),
                "unexpected publication object receipt"
            );
            return Ok(());
        }
        match (&request.action, &self.value) {
            (ExportPublicationAction::Begin, ExportPublicationValue::Begun { .. })
            | (ExportPublicationAction::RecheckPayload, ExportPublicationValue::RecheckedPayload)
            | (ExportPublicationAction::Capture, ExportPublicationValue::Captured)
            | (ExportPublicationAction::VerifyCapture, ExportPublicationValue::CaptureVerified)
            | (ExportPublicationAction::Link, ExportPublicationValue::Linked)
            | (
                ExportPublicationAction::RecheckInstalled,
                ExportPublicationValue::RecheckedInstalled,
            )
            | (ExportPublicationAction::RestoreLink, ExportPublicationValue::RestoredLinked)
            | (
                ExportPublicationAction::RecheckRestored,
                ExportPublicationValue::RecheckedRestored,
            )
            | (ExportPublicationAction::Finish, ExportPublicationValue::Finished)
            | (ExportPublicationAction::Abort, ExportPublicationValue::Aborted) => Ok(()),
            (
                ExportPublicationAction::FailureReceipt { .. },
                ExportPublicationValue::Receipt(receipt),
            ) => crate::metadata_export::validate_export_receipt_wire(receipt, seal),
            (
                ExportPublicationAction::VerifyInstalled,
                ExportPublicationValue::Installed(receipt),
            ) => {
                ensure!(
                    receipt.state == crate::metadata_export::ExportState::Published,
                    "publication receipt state mismatch"
                );
                crate::metadata_export::validate_export_receipt_wire(receipt, seal)
            }
            (
                ExportPublicationAction::VerifyRestored,
                ExportPublicationValue::Restored(receipt),
            ) => {
                ensure!(
                    receipt.state == crate::metadata_export::ExportState::Restored,
                    "restoration receipt state mismatch"
                );
                crate::metadata_export::validate_export_receipt_wire(receipt, seal)
            }
            _ => anyhow::bail!("unexpected publication reply"),
        }
    }
}
impl PreparedExportDirectory {
    pub fn validate_for(&self, request: &PrepareExportDirectory) -> Result<()> {
        request.validate()?;
        validate_path(&self.directory)?;
        ensure!(
            self.root == request.root && self.requested == request.directory,
            "prepared export directory belongs to another request"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExportProfileAction {
    Begin,
    Read { offset: U64 },
    Finish,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportProfileRequest {
    pub root: RootCapability,
    pub requested: NativePath,
    pub transfer: LeaseId,
    pub step: U64,
    pub allowance: U64,
    pub action: ExportProfileAction,
}
impl ExportProfileRequest {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        validate_path(&self.requested)?;
        ensure!(
            (1..=EXPORT_PROFILE_BYTES as u64).contains(&self.allowance.0),
            "export profile allowance"
        );
        ensure!(
            matches!(&self.action, ExportProfileAction::Begin) == (self.step.0 == 0),
            "export profile begin/step mismatch"
        );
        if let ExportProfileAction::Read { offset } = &self.action {
            ensure!(offset.0 < self.allowance.0, "export profile offset limit");
        }
        Ok(())
    }
    pub fn cleanup(&self) -> bool {
        matches!(self.action, ExportProfileAction::Abort)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExportProfileValue {
    Begun {
        bytes: U64,
    },
    Chunk {
        offset: U64,
        checksum: String,
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    Finished {
        bytes: U64,
    },
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportProfileReply {
    pub root: RootCapability,
    pub requested: NativePath,
    pub transfer: LeaseId,
    pub step: U64,
    pub value: ExportProfileValue,
}
impl ExportProfileReply {
    pub fn binary(&self) -> Option<&[u8]> {
        match &self.value {
            ExportProfileValue::Chunk { bytes, .. } => Some(bytes),
            _ => None,
        }
    }
    pub fn set_binary(&mut self, value: &[u8]) -> Result<()> {
        match &mut self.value {
            ExportProfileValue::Chunk { bytes, .. } => {
                ensure!(
                    !value.is_empty() && value.len() <= preview_io::CHUNK_BYTES,
                    "export profile chunk admission"
                );
                *bytes = value.to_vec();
            }
            _ => ensure!(value.is_empty(), "unexpected export profile binary reply"),
        }
        Ok(())
    }
    pub fn validate(&self, request: &ExportProfileRequest) -> Result<()> {
        request.validate()?;
        ensure!(
            self.root == request.root
                && self.requested == request.requested
                && self.transfer == request.transfer
                && self.step == request.step,
            "export profile reply provenance mismatch"
        );
        match (&request.action, &self.value) {
            (ExportProfileAction::Begin, ExportProfileValue::Begun { bytes }) => {
                ensure!(bytes.0 <= request.allowance.0, "export profile byte limit");
                Ok(())
            }
            (
                ExportProfileAction::Read { offset: expected },
                ExportProfileValue::Chunk {
                    offset,
                    checksum,
                    bytes,
                },
            ) => {
                ensure!(
                    offset == expected
                        && !bytes.is_empty()
                        && bytes.len() <= preview_io::CHUNK_BYTES
                        && blake3::hash(bytes).to_hex().as_str() == checksum,
                    "export profile chunk mismatch"
                );
                Ok(())
            }
            (ExportProfileAction::Finish, ExportProfileValue::Finished { bytes }) => {
                ensure!(bytes.0 <= request.allowance.0, "export profile byte limit");
                Ok(())
            }
            (ExportProfileAction::Abort, ExportProfileValue::Aborted) => Ok(()),
            _ => anyhow::bail!("unexpected export profile reply"),
        }
    }
}

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
    /// Resolves one existing export directory under the exact admitted catalog
    /// capability. This is read-only and grants no later alias/publication right.
    fn prepare_export_directory(
        &self,
        _request: &PrepareExportDirectory,
        _cancel: &AtomicBool,
    ) -> Result<PreparedExportDirectory> {
        anyhow::bail!("filesystem owner does not support export directory preparation")
    }
    fn export_destination_snapshot(
        &self,
        _request: &ExportDestinationSnapshotRequest,
        _cancel: &AtomicBool,
    ) -> Result<ExportDestinationSnapshotReply> {
        anyhow::bail!("filesystem owner does not support export destination snapshots")
    }
    fn export_alias_fact(
        &self,
        _request: &ExportAliasFactRequest,
        _cancel: &AtomicBool,
    ) -> Result<ExportAliasFactReply> {
        anyhow::bail!("filesystem owner does not support export alias facts")
    }
    fn export_profile_call(
        &self,
        _request: &ExportProfileRequest,
        _cancel: &AtomicBool,
    ) -> Result<ExportProfileReply> {
        anyhow::bail!("filesystem owner does not support export profile reads")
    }
    fn inspect_export_original(
        &self,
        _request: &InspectExportOriginal,
        _cancel: &AtomicBool,
    ) -> Result<InspectedExportOriginal> {
        anyhow::bail!("filesystem owner does not support export original inspection")
    }
    fn export_original_call(
        &self,
        _request: &ExportOriginalRequest,
        _cancel: &AtomicBool,
    ) -> Result<ExportOriginalReply> {
        anyhow::bail!("filesystem owner does not support export original leases")
    }
    fn export_publication_call(
        &self,
        _request: &ExportPublicationRequest,
        _cancel: &AtomicBool,
    ) -> Result<ExportPublicationReply> {
        anyhow::bail!("filesystem owner does not support export publication leases")
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
#[derive(Clone, Copy)]
enum OriginalCustodyState {
    BeginPending,
    Active { next_step: u64 },
    AbortPending { step: u64 },
}
struct OriginalCustody {
    filesystem: Arc<dyn CatalogFilesystem>,
    root: RootCapability,
    requested: NativePath,
    transfer: LeaseId,
    allowance: u64,
    state: OriginalCustodyState,
}
pub(crate) struct ManagedOriginalLease {
    custody: Arc<Mutex<Option<OriginalCustody>>>,
    transfer: LeaseId,
    revision: crate::metadata_export::FileRevision,
    complete: bool,
}
impl ManagedOriginalLease {
    pub(crate) fn revision(&self) -> &crate::metadata_export::FileRevision {
        &self.revision
    }
    fn request(
        custody: &OriginalCustody,
        step: u64,
        action: ExportOriginalAction,
    ) -> ExportOriginalRequest {
        ExportOriginalRequest {
            root: custody.root.clone(),
            requested: custody.requested.clone(),
            transfer: custody.transfer.clone(),
            step: U64(step),
            allowance: U64(custody.allowance),
            action,
        }
    }
    pub(crate) fn recheck(&mut self, cancel: &AtomicBool) -> Result<()> {
        let mut slot = self.custody.lock().unwrap_or_else(|e| e.into_inner());
        let custody = slot
            .as_mut()
            .context("export original custody is not retained")?;
        ensure!(
            custody.transfer == self.transfer,
            "export original custody changed"
        );
        let OriginalCustodyState::Active { next_step } = custody.state else {
            anyhow::bail!("export original cleanup must reconcile before recheck")
        };
        let request = Self::request(custody, next_step, ExportOriginalAction::Recheck);
        custody.state = OriginalCustodyState::AbortPending {
            step: next_step
                .checked_add(1)
                .context("export original cleanup step exhausted")?,
        };
        let reply = custody.filesystem.export_original_call(&request, cancel)?;
        reply.validate(&request)?;
        ensure!(matches!(reply.value, ExportOriginalValue::Rechecked));
        custody.state = OriginalCustodyState::Active {
            next_step: next_step
                .checked_add(1)
                .context("export original step exhausted")?,
        };
        Ok(())
    }
    fn abort(&mut self, cancel: &AtomicBool) -> Result<()> {
        let mut slot = self.custody.lock().unwrap_or_else(|e| e.into_inner());
        let custody = slot
            .as_mut()
            .context("export original custody is not retained")?;
        ensure!(
            custody.transfer == self.transfer,
            "export original custody changed"
        );
        let step = match custody.state {
            OriginalCustodyState::BeginPending => 1,
            OriginalCustodyState::Active { next_step } => next_step,
            OriginalCustodyState::AbortPending { step } => step,
        };
        custody.state = OriginalCustodyState::AbortPending { step };
        let request = Self::request(custody, step, ExportOriginalAction::Abort);
        let reply = custody.filesystem.export_original_call(&request, cancel)?;
        reply.validate(&request)?;
        ensure!(matches!(reply.value, ExportOriginalValue::Aborted));
        *slot = None;
        self.complete = true;
        Ok(())
    }
    fn finish(&mut self, cancel: &AtomicBool) -> Result<()> {
        let mut slot = self.custody.lock().unwrap_or_else(|e| e.into_inner());
        let custody = slot
            .as_mut()
            .context("export original custody is not retained")?;
        ensure!(
            custody.transfer == self.transfer,
            "export original custody changed"
        );
        let OriginalCustodyState::Active { next_step } = custody.state else {
            anyhow::bail!("export original cleanup must reconcile before finish")
        };
        let abort_step = next_step
            .checked_add(1)
            .context("export original cleanup step exhausted")?;
        let request = Self::request(custody, next_step, ExportOriginalAction::Finish);
        custody.state = OriginalCustodyState::AbortPending { step: abort_step };
        let reply = custody.filesystem.export_original_call(&request, cancel)?;
        reply.validate(&request)?;
        ensure!(matches!(reply.value, ExportOriginalValue::Finished));
        *slot = None;
        self.complete = true;
        Ok(())
    }
    pub(crate) fn complete<T>(mut self, result: Result<T>, cancel: &AtomicBool) -> Result<T> {
        match result {
            Ok(value) => {
                if let Err(error) = self.finish(cancel) {
                    let _ = self.abort(cancel);
                    Err(error)
                } else {
                    Ok(value)
                }
            }
            Err(error) => {
                let _ = self.abort(cancel);
                Err(error)
            }
        }
    }
}
impl Drop for ManagedOriginalLease {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        let mut slot = self.custody.lock().unwrap_or_else(|e| e.into_inner());
        let Some(custody) = slot.as_mut() else {
            return;
        };
        if custody.transfer != self.transfer {
            return;
        }
        custody.state = OriginalCustodyState::AbortPending {
            step: match custody.state {
                OriginalCustodyState::BeginPending => 1,
                OriginalCustodyState::Active { next_step } => next_step,
                OriginalCustodyState::AbortPending { step } => step,
            },
        };
    }
}
pub(crate) fn export_original_custody_layout() -> (usize, usize) {
    (
        std::mem::size_of::<Mutex<Option<OriginalCustody>>>(),
        std::mem::align_of::<Mutex<Option<OriginalCustody>>>(),
    )
}

#[derive(Clone)]
enum PublicationCustodyState {
    BeginPending {
        request: ExportPublicationRequest,
    },
    Active {
        next_step: u64,
    },
    StepPending {
        request: ExportPublicationRequest,
        next_step: u64,
    },
}
struct PublicationCustody {
    original: Arc<Mutex<Option<OriginalCustody>>>,
    original_transfer: Option<LeaseId>,
    cleanup_requested: bool,
    filesystem: Arc<dyn CatalogFilesystem>,
    root: RootCapability,
    transfer: LeaseId,
    mode: ExportPublicationMode,
    source: ExportPublicationSource,
    state: PublicationCustodyState,
}
pub(crate) struct ManagedPublicationLease {
    custody: Arc<Mutex<Option<PublicationCustody>>>,
    transfer: LeaseId,
    seal: crate::metadata_export::SealedPhotoExport,
    installed: bool,
    timings: crate::metadata_export::PhotoPublicationTimings,
    initial_hashed_bytes: u64,
    complete: bool,
}
fn publication_failure_known(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::filesystem_worker::wire::Failure>()
        .is_some_and(|failure| failure.kind != crate::filesystem_worker::wire::FailureKind::Unknown)
}
impl ManagedPublicationLease {
    fn request(
        custody: &PublicationCustody,
        step: u64,
        action: ExportPublicationAction,
    ) -> ExportPublicationRequest {
        ExportPublicationRequest {
            root: custody.root.clone(),
            transfer: custody.transfer.clone(),
            step: U64(step),
            mode: custody.mode,
            source: custody.source.clone(),
            action,
        }
    }
    fn call(
        &mut self,
        action: ExportPublicationAction,
        cancel: &AtomicBool,
    ) -> Result<ExportPublicationReply> {
        let (filesystem, request, next_step) = {
            let mut slot = self.custody.lock().unwrap_or_else(|e| e.into_inner());
            let custody = slot
                .as_mut()
                .context("export publication custody is not retained")?;
            ensure!(
                custody.transfer == self.transfer,
                "export publication custody changed"
            );
            let PublicationCustodyState::Active { next_step } = custody.state.clone() else {
                anyhow::bail!("export publication request requires reconciliation")
            };
            let after = next_step
                .checked_add(1)
                .context("export publication step exhausted")?;
            let request = Self::request(custody, next_step, action);
            custody.state = PublicationCustodyState::StepPending {
                request: request.clone(),
                next_step: after,
            };
            (custody.filesystem.clone(), request, after)
        };
        match filesystem.export_publication_call(&request, cancel) {
            Ok(reply) => {
                apply_publication_reply(&self.custody, &request, &reply, next_step)?;
                self.complete = matches!(
                    reply.value,
                    ExportPublicationValue::Finished | ExportPublicationValue::Aborted
                );
                self.timings = reply.timings.clone();
                if let ExportPublicationValue::Failed(failure) = &reply.value {
                    return Err(failure.clone().into());
                }
                Ok(reply)
            }
            Err(error) => {
                // A direct, known outer failure rejects this fresh step before
                // admission. Consumed failures (including canceled hashes) use
                // Failed replies. Unknown transport errors retain exact pending.
                if publication_failure_known(&error) {
                    let mut slot = self.custody.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(custody) = slot.as_mut() {
                        ensure!(
                            custody.transfer == self.transfer,
                            "publication custody changed"
                        );
                        custody.state = PublicationCustodyState::Active {
                            next_step: request.step.0,
                        };
                    }
                }
                Err(error)
            }
        }
    }
    pub(crate) fn seal(&self) -> &crate::metadata_export::SealedPhotoExport {
        &self.seal
    }
    pub(crate) fn installed(&self) -> bool {
        self.installed
    }
    pub(crate) fn timings(&self) -> &crate::metadata_export::PhotoPublicationTimings {
        &self.timings
    }
    pub(crate) fn take_initial_hashed_bytes(&mut self) -> u64 {
        std::mem::take(&mut self.initial_hashed_bytes)
    }
    pub(crate) fn recheck_payload(&mut self, cancel: &AtomicBool) -> Result<u64> {
        Ok(self
            .call(ExportPublicationAction::RecheckPayload, cancel)?
            .hashed_bytes
            .0)
    }
    pub(crate) fn capture(&mut self, cancel: &AtomicBool) -> Result<u64> {
        Ok(self
            .call(ExportPublicationAction::Capture, cancel)?
            .hashed_bytes
            .0)
    }
    pub(crate) fn verify_capture(&mut self, cancel: &AtomicBool) -> Result<u64> {
        Ok(self
            .call(ExportPublicationAction::VerifyCapture, cancel)?
            .hashed_bytes
            .0)
    }
    pub(crate) fn failure_receipt(
        &mut self,
        detail: String,
        cancel: &AtomicBool,
    ) -> Result<(crate::metadata_export::ExportReceipt, u64)> {
        let reply = self.call(ExportPublicationAction::FailureReceipt { detail }, cancel)?;
        let ExportPublicationValue::Receipt(receipt) = reply.value else {
            unreachable!()
        };
        Ok((receipt, reply.hashed_bytes.0))
    }
    pub(crate) fn link(&mut self, cancel: &AtomicBool) -> Result<u64> {
        Ok(self
            .call(ExportPublicationAction::Link, cancel)?
            .hashed_bytes
            .0)
    }
    pub(crate) fn verify_installed(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<(crate::metadata_export::ExportReceipt, u64)> {
        let reply = self.call(ExportPublicationAction::VerifyInstalled, cancel)?;
        let ExportPublicationValue::Installed(receipt) = reply.value else {
            unreachable!()
        };
        Ok((receipt, reply.hashed_bytes.0))
    }
    pub(crate) fn recheck_installed(&mut self, cancel: &AtomicBool) -> Result<u64> {
        Ok(self
            .call(ExportPublicationAction::RecheckInstalled, cancel)?
            .hashed_bytes
            .0)
    }
    pub(crate) fn restore_link(&mut self, cancel: &AtomicBool) -> Result<u64> {
        Ok(self
            .call(ExportPublicationAction::RestoreLink, cancel)?
            .hashed_bytes
            .0)
    }
    pub(crate) fn verify_restored(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<(crate::metadata_export::ExportReceipt, u64)> {
        let reply = self.call(ExportPublicationAction::VerifyRestored, cancel)?;
        let ExportPublicationValue::Restored(receipt) = reply.value else {
            unreachable!()
        };
        Ok((receipt, reply.hashed_bytes.0))
    }
    pub(crate) fn recheck_restored(&mut self, cancel: &AtomicBool) -> Result<u64> {
        Ok(self
            .call(ExportPublicationAction::RecheckRestored, cancel)?
            .hashed_bytes
            .0)
    }
    fn abort(&mut self) -> Result<()> {
        reconcile_publication(&self.custody, Some(&self.transfer))?;
        self.complete = self
            .custody
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none();
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        publication_original_retired(&self.custody)?;
        self.call(ExportPublicationAction::Finish, &AtomicBool::new(false))?;
        Ok(())
    }
    pub(crate) fn complete<T>(mut self, result: Result<T>) -> Result<T> {
        if let Some(custody) = self
            .custody
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            custody.cleanup_requested = true;
        }
        match result {
            Ok(value) => match self.finish() {
                Ok(()) => Ok(value),
                Err(error) => {
                    let _ = self.abort();
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.abort();
                Err(error)
            }
        }
    }
}
impl Drop for ManagedPublicationLease {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        if let Some(custody) = self
            .custody
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            && custody.transfer == self.transfer
        {
            custody.cleanup_requested = true;
        }
        // Retain exact pending request and both owners. Drop performs no I/O.
    }
}
fn publication_original_retired(registry: &Arc<Mutex<Option<PublicationCustody>>>) -> Result<()> {
    let slot = registry.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(custody) = slot.as_ref() {
        let original = custody.original.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(original) = original.as_ref() {
            ensure!(
                custody.original_transfer.as_ref() == Some(&original.transfer),
                "publication original custody changed"
            );
            anyhow::bail!("publication retains its lock until original cleanup is acknowledged");
        }
    }
    Ok(())
}
fn apply_publication_reply(
    registry: &Arc<Mutex<Option<PublicationCustody>>>,
    request: &ExportPublicationRequest,
    reply: &ExportPublicationReply,
    next_step: u64,
) -> Result<()> {
    reply.validate(request)?;
    let mut slot = registry.lock().unwrap_or_else(|e| e.into_inner());
    let custody = slot.as_mut().context("publication custody disappeared")?;
    let pending = match &custody.state {
        PublicationCustodyState::BeginPending { request }
        | PublicationCustodyState::StepPending { request, .. } => request,
        PublicationCustodyState::Active { .. } => {
            anyhow::bail!("publication reply has no pending request")
        }
    };
    ensure!(pending == request, "publication pending request changed");
    if matches!(
        reply.value,
        ExportPublicationValue::Finished | ExportPublicationValue::Aborted
    ) {
        *slot = None;
    } else {
        custody.state = PublicationCustodyState::Active { next_step };
    }
    Ok(())
}
fn reconcile_publication(
    registry: &Arc<Mutex<Option<PublicationCustody>>>,
    expected: Option<&LeaseId>,
) -> Result<()> {
    publication_original_retired(registry)?;
    // At most one exact pending replay followed by one Abort. An outer error
    // leaves the pending request untouched; it never proves step consumption.
    for _ in 0..2 {
        let (filesystem, request, next_step) = {
            let mut slot = registry.lock().unwrap_or_else(|e| e.into_inner());
            let Some(custody) = slot.as_mut() else {
                return Ok(());
            };
            if let Some(expected) = expected {
                ensure!(&custody.transfer == expected, "publication custody changed");
            }
            custody.cleanup_requested = true;
            match &custody.state {
                PublicationCustodyState::BeginPending { request } => {
                    (custody.filesystem.clone(), request.clone(), 1)
                }
                PublicationCustodyState::StepPending { request, next_step } => {
                    (custody.filesystem.clone(), request.clone(), *next_step)
                }
                PublicationCustodyState::Active { next_step } => {
                    let next = next_step
                        .checked_add(1)
                        .context("publication step exhausted")?;
                    let request = ManagedPublicationLease::request(
                        custody,
                        *next_step,
                        ExportPublicationAction::Abort,
                    );
                    custody.state = PublicationCustodyState::StepPending {
                        request: request.clone(),
                        next_step: next,
                    };
                    (custody.filesystem.clone(), request, next)
                }
            }
        };
        let reply = filesystem.export_publication_call(&request, &AtomicBool::new(false))?;
        apply_publication_reply(registry, &request, &reply, next_step)?;
        if request.cleanup() {
            return match reply.value {
                ExportPublicationValue::Finished | ExportPublicationValue::Aborted => Ok(()),
                ExportPublicationValue::Failed(failure) => Err(failure.into()),
                _ => anyhow::bail!("publication terminal reply did not retire custody"),
            };
        }
    }
    ensure!(
        registry.lock().unwrap_or_else(|e| e.into_inner()).is_none(),
        "publication reconciliation remains pending"
    );
    Ok(())
}
pub(crate) fn export_publication_custody_layout() -> (usize, usize) {
    (
        std::mem::size_of::<Mutex<Option<PublicationCustody>>>(),
        std::mem::align_of::<Mutex<Option<PublicationCustody>>>(),
    )
}
/// The Arc instance, not its serialized epoch/physical tuple, grants a prepared
/// edit or native permit authority within the original catalog lifetime.
pub(crate) struct CatalogSessionAuthority {
    physical: PhysicalObjectId,
    mode: AuthorityMode,
    searches: Mutex<Vec<Arc<dyn SessionTask>>>,
    original: Arc<Mutex<Option<OriginalCustody>>>,
    publication: Arc<Mutex<Option<PublicationCustody>>>,
}
impl CatalogSessionAuthority {
    pub(crate) fn legacy(file: Arc<File>) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            physical: crate::catalog_storage::physical_object_id(&file)?,
            mode: AuthorityMode::Legacy(file),
            searches: Mutex::new(Vec::new()),
            original: Arc::new(Mutex::new(None)),
            publication: Arc::new(Mutex::new(None)),
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
    pub(crate) fn prepare_export_directory(
        &self,
        directory: &NativePath,
        cancel: &AtomicBool,
    ) -> Result<Option<NativePath>> {
        match &self.mode {
            AuthorityMode::Legacy(_) => Ok(None),
            AuthorityMode::Managed {
                filesystem, root, ..
            } => {
                let request = PrepareExportDirectory {
                    root: root.clone(),
                    directory: directory.clone(),
                };
                request.validate()?;
                let prepared = filesystem.prepare_export_directory(&request, cancel)?;
                prepared.validate_for(&request)?;
                Ok(Some(prepared.directory))
            }
        }
    }
    pub(crate) fn inspect_export_original(
        &self,
        requested: &NativePath,
        allowance: u64,
        cancel: &AtomicBool,
    ) -> Result<Option<crate::metadata_export::FileRevision>> {
        let AuthorityMode::Managed {
            filesystem, root, ..
        } = &self.mode
        else {
            return Ok(None);
        };
        self.reconcile_export_original(cancel)?;
        let request = InspectExportOriginal {
            root: root.clone(),
            requested: requested.clone(),
            allowance: U64(allowance),
        };
        request.validate()?;
        let reply = filesystem.inspect_export_original(&request, cancel)?;
        reply.validate_for(&request)?;
        Ok(Some(reply.revision))
    }
    pub(crate) fn begin_export_original(
        &self,
        requested: &NativePath,
        allowance: u64,
        expected: &crate::metadata_export::FileRevision,
        cancel: &AtomicBool,
    ) -> Result<Option<ManagedOriginalLease>> {
        let AuthorityMode::Managed {
            filesystem, root, ..
        } = &self.mode
        else {
            return Ok(None);
        };
        let abandoned_publication = {
            let slot = self.publication.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(publication) = slot.as_ref() {
                ensure!(
                    publication.cleanup_requested || publication.original_transfer.is_none(),
                    "publication already bound to an original transfer"
                );
                publication.cleanup_requested
            } else {
                false
            }
        };
        self.reconcile_export_original(cancel)?;
        if abandoned_publication {
            self.reconcile_export_publication()?;
        }
        let transfer = LeaseId::new();
        let request = ExportOriginalRequest {
            root: root.clone(),
            requested: requested.clone(),
            transfer: transfer.clone(),
            step: U64(0),
            allowance: U64(allowance),
            action: ExportOriginalAction::Begin,
        };
        request.validate()?;
        {
            let mut slot = self.original.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                slot.is_none(),
                "export original custody is already retained"
            );
            *slot = Some(OriginalCustody {
                filesystem: filesystem.clone(),
                root: root.clone(),
                requested: requested.clone(),
                transfer: transfer.clone(),
                allowance,
                state: OriginalCustodyState::BeginPending,
            });
        }
        if let Some(publication) = self
            .publication
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            ensure!(
                publication.original_transfer.is_none(),
                "publication original transfer already bound"
            );
            publication.original_transfer = Some(transfer.clone());
        }
        let reply = filesystem.export_original_call(&request, cancel);
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                let uncertain = error
                    .downcast_ref::<crate::filesystem_worker::wire::Failure>()
                    .is_none_or(|failure| {
                        failure.kind == crate::filesystem_worker::wire::FailureKind::Unknown
                    });
                if uncertain {
                    let _ = self.reconcile_export_original(cancel);
                } else {
                    *self.original.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                return Err(error);
            }
        };
        let validated = reply.validate(&request).and_then(|_| {
            let ExportOriginalValue::Begun { revision } = &reply.value else {
                unreachable!()
            };
            ensure!(
                revision == expected,
                "original changed since export planning"
            );
            Ok(revision.clone())
        });
        let revision = match validated {
            Ok(revision) => revision,
            Err(error) => {
                if let Some(custody) = self
                    .original
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                {
                    custody.state = OriginalCustodyState::AbortPending { step: 1 };
                }
                let _ = self.reconcile_export_original(cancel);
                return Err(error);
            }
        };
        self.original
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .expect("begin custody retained")
            .state = OriginalCustodyState::Active { next_step: 1 };
        Ok(Some(ManagedOriginalLease {
            custody: self.original.clone(),
            transfer,
            revision,
            complete: false,
        }))
    }
    pub(crate) fn reconcile_export_original(&self, cancel: &AtomicBool) -> Result<()> {
        let mut slot = self.original.lock().unwrap_or_else(|e| e.into_inner());
        let Some(custody) = slot.as_mut() else {
            return Ok(());
        };
        if matches!(custody.state, OriginalCustodyState::BeginPending) {
            let request = ManagedOriginalLease::request(custody, 0, ExportOriginalAction::Begin);
            // This exact-id replay is cleanup/reconciliation. The initiating
            // operation's canceled flag cannot suppress the proof needed to
            // learn whether F retained the handle.
            let recovery_cancel = AtomicBool::new(false);
            let reply = match custody
                .filesystem
                .export_original_call(&request, &recovery_cancel)
            {
                Ok(reply) => reply,
                Err(error)
                    if error
                        .downcast_ref::<crate::filesystem_worker::wire::Failure>()
                        .is_some_and(|failure| {
                            failure.kind != crate::filesystem_worker::wire::FailureKind::Unknown
                        }) =>
                {
                    *slot = None;
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            reply.validate(&request)?;
            ensure!(matches!(reply.value, ExportOriginalValue::Begun { .. }));
            custody.state = OriginalCustodyState::Active { next_step: 1 };
        }
        let step = match custody.state {
            OriginalCustodyState::BeginPending => unreachable!(),
            OriginalCustodyState::Active { next_step } => next_step,
            OriginalCustodyState::AbortPending { step } => step,
        };
        custody.state = OriginalCustodyState::AbortPending { step };
        let request = ManagedOriginalLease::request(custody, step, ExportOriginalAction::Abort);
        let reply = custody.filesystem.export_original_call(&request, cancel)?;
        reply.validate(&request)?;
        ensure!(matches!(reply.value, ExportOriginalValue::Aborted));
        *slot = None;
        Ok(())
    }
    pub(crate) fn begin_export_publication(
        &self,
        mode: ExportPublicationMode,
        source: ExportPublicationSource,
        cancel: &AtomicBool,
    ) -> Result<Option<ManagedPublicationLease>> {
        let AuthorityMode::Managed {
            filesystem, root, ..
        } = &self.mode
        else {
            return Ok(None);
        };
        self.reconcile_export_publication()?;
        let transfer = LeaseId::new();
        let request = ExportPublicationRequest {
            root: root.clone(),
            transfer: transfer.clone(),
            step: U64(0),
            mode,
            source: source.clone(),
            action: ExportPublicationAction::Begin,
        };
        request.validate()?;
        {
            let mut slot = self.publication.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                slot.is_none(),
                "export publication custody is already retained"
            );
            *slot = Some(PublicationCustody {
                original: self.original.clone(),
                original_transfer: self
                    .original
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|original| original.transfer.clone()),
                cleanup_requested: false,
                filesystem: filesystem.clone(),
                root: root.clone(),
                transfer: transfer.clone(),
                mode,
                source,
                state: PublicationCustodyState::BeginPending {
                    request: request.clone(),
                },
            });
        }
        let reply = match filesystem.export_publication_call(&request, cancel) {
            Ok(reply) => reply,
            Err(error) => {
                if publication_failure_known(&error) {
                    *self.publication.lock().unwrap_or_else(|e| e.into_inner()) = None;
                } else {
                    if let Some(custody) = self
                        .publication
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_mut()
                    {
                        custody.cleanup_requested = true;
                    }
                    let _ = self.reconcile_export_publication();
                }
                return Err(error);
            }
        };
        let validated = reply.validate(&request);
        if let Err(error) = validated {
            if let Some(custody) = self
                .publication
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
            {
                custody.cleanup_requested = true;
            }
            let _ = self.reconcile_export_publication();
            return Err(error);
        }
        let ExportPublicationValue::Begun { installed } = reply.value else {
            unreachable!()
        };
        self.publication
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .expect("publication begin custody retained")
            .state = PublicationCustodyState::Active { next_step: 1 };
        Ok(Some(ManagedPublicationLease {
            custody: self.publication.clone(),
            transfer,
            seal: reply.seal,
            installed,
            timings: reply.timings,
            initial_hashed_bytes: reply.hashed_bytes.0,
            complete: false,
        }))
    }
    pub(crate) fn reconcile_export_publication(&self) -> Result<()> {
        reconcile_publication(&self.publication, None)
    }
    pub(crate) fn read_export_profile(
        &self,
        requested: &NativePath,
        allowance: u64,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<u8>>> {
        let AuthorityMode::Managed {
            filesystem, root, ..
        } = &self.mode
        else {
            return Ok(None);
        };
        let transfer = LeaseId::new();
        let mut step = 0u64;
        let result = (|| {
            let request = ExportProfileRequest {
                root: root.clone(),
                requested: requested.clone(),
                transfer: transfer.clone(),
                step: U64(step),
                allowance: U64(allowance),
                action: ExportProfileAction::Begin,
            };
            let reply = filesystem.export_profile_call(&request, cancel)?;
            reply.validate(&request)?;
            let ExportProfileValue::Begun { bytes: total } = reply.value else {
                unreachable!()
            };
            let length = usize::try_from(total.0)?;
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(length)?;
            let mut offset = 0usize;
            while offset < length {
                step = step
                    .checked_add(1)
                    .context("export profile step exhausted")?;
                let request = ExportProfileRequest {
                    root: root.clone(),
                    requested: requested.clone(),
                    transfer: transfer.clone(),
                    step: U64(step),
                    allowance: U64(allowance),
                    action: ExportProfileAction::Read {
                        offset: U64(offset as u64),
                    },
                };
                let reply = filesystem.export_profile_call(&request, cancel)?;
                reply.validate(&request)?;
                let ExportProfileValue::Chunk { bytes: chunk, .. } = reply.value else {
                    unreachable!()
                };
                ensure!(
                    chunk.len() <= length - offset,
                    "export profile exceeds declared length"
                );
                offset += chunk.len();
                bytes.extend_from_slice(&chunk);
            }
            step = step
                .checked_add(1)
                .context("export profile step exhausted")?;
            let request = ExportProfileRequest {
                root: root.clone(),
                requested: requested.clone(),
                transfer: transfer.clone(),
                step: U64(step),
                allowance: U64(allowance),
                action: ExportProfileAction::Finish,
            };
            let reply = filesystem.export_profile_call(&request, cancel)?;
            reply.validate(&request)?;
            let ExportProfileValue::Finished { bytes: finished } = reply.value else {
                unreachable!()
            };
            ensure!(finished == total, "export profile finish length changed");
            Ok(bytes)
        })();
        if result.is_err() {
            if let Some(abort_step) = step.checked_add(1) {
                let request = ExportProfileRequest {
                    root: root.clone(),
                    requested: requested.clone(),
                    transfer,
                    step: U64(abort_step),
                    allowance: U64(allowance),
                    action: ExportProfileAction::Abort,
                };
                if let Ok(reply) = filesystem.export_profile_call(&request, cancel) {
                    let _ = reply.validate(&request);
                }
            }
        }
        result.map(Some)
    }
    pub(crate) fn export_destination_snapshot(
        &self,
        destination: &NativePath,
        max_existing_bytes: u64,
        cancel: &AtomicBool,
    ) -> Result<Option<crate::metadata_export::DestinationSnapshot>> {
        let AuthorityMode::Managed {
            filesystem, root, ..
        } = &self.mode
        else {
            return Ok(None);
        };
        let request = ExportDestinationSnapshotRequest {
            root: root.clone(),
            destination: destination.clone(),
            max_existing_bytes: U64(max_existing_bytes),
        };
        request.validate()?;
        let reply = filesystem.export_destination_snapshot(&request, cancel)?;
        reply.validate_for(&request)?;
        Ok(Some(reply.snapshot))
    }
    pub(crate) fn export_alias_fact(
        &self,
        path: &NativePath,
        kind: ExportAliasFactKind,
        cancel: &AtomicBool,
    ) -> Result<Option<ExportAliasFactValue>> {
        let AuthorityMode::Managed {
            filesystem, root, ..
        } = &self.mode
        else {
            return Ok(None);
        };
        let request = ExportAliasFactRequest {
            root: root.clone(),
            path: path.clone(),
            kind,
        };
        request.validate()?;
        let reply = filesystem.export_alias_fact(&request, cancel)?;
        reply.validate_for(&request)?;
        Ok(Some(reply.value))
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
            original: Arc::new(Mutex::new(None)),
            publication: Arc::new(Mutex::new(None)),
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
        self.authority
            .reconcile_export_original(&AtomicBool::new(false))?;
        self.authority.reconcile_export_publication()?;
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
pub(crate) use tests::{
    ExportOriginalTestControl, export_facts_managed_session, export_managed_session,
    export_original_managed_session, export_profile_managed_session, retained_admission,
    unused_filesystem,
};

#[cfg(test)]
pub(crate) mod overlap_tests;
