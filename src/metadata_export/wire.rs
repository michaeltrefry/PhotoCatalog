//! Persisted export paths. Wire inspection never maps foreign platform units.
//! Materializing an execution plan is an explicit, fallible local conversion.
use super::{
    DestinationSnapshot, ExportPlan, ExportReceipt, ExportState, FileRevision, SealedPhotoExport,
};
use crate::storage_volume::NativePath;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Kept separate from display strings and from local filesystem paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredPath {
    Legacy(PathBuf),
    Native(NativePath),
}
impl StoredPath {
    fn encode(path: PathBuf, native: bool) -> Self {
        if native {
            Self::Native(NativePath::from_path(&path))
        } else {
            Self::Legacy(path)
        }
    }
    pub fn local(self, native: bool) -> Result<PathBuf> {
        let path = match (self, native) {
            (Self::Legacy(path), false) => path,
            (Self::Native(path), true) => path.to_path()?,
            _ => anyhow::bail!("export version/path encoding mismatch"),
        };
        // Apply the same NUL rejection to old strings. No filesystem access.
        NativePath::from_path(&path).to_path()?;
        Ok(path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u32,
    pub operation: String,
    pub destination: StoredPath,
    pub expected: Option<FileRevision>,
    pub payload_digest: String,
    pub payload_bytes: u64,
}
impl From<ExportPlan> for Plan {
    fn from(p: ExportPlan) -> Self {
        Self {
            version: p.version,
            operation: p.operation,
            destination: StoredPath::encode(p.destination, p.version >= 3),
            expected: p.expected,
            payload_digest: p.payload_digest,
            payload_bytes: p.payload_bytes,
        }
    }
}
impl TryFrom<Plan> for ExportPlan {
    type Error = anyhow::Error;
    fn try_from(p: Plan) -> Result<Self> {
        ensure!(
            matches!(p.version, 1..=4),
            "unsupported export plan version"
        );
        Ok(Self {
            version: p.version,
            operation: p.operation,
            destination: p.destination.local(p.version >= 3)?,
            expected: p.expected,
            payload_digest: p.payload_digest,
            payload_bytes: p.payload_bytes,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    pub operation: String,
    pub destination: StoredPath,
    pub expected: Option<FileRevision>,
    pub max_existing_bytes: u64,
}
impl From<DestinationSnapshot> for Snapshot {
    fn from(p: DestinationSnapshot) -> Self {
        Self {
            version: p.version,
            operation: p.operation,
            destination: StoredPath::encode(p.destination, p.version == 2),
            expected: p.expected,
            max_existing_bytes: p.max_existing_bytes,
        }
    }
}
impl TryFrom<Snapshot> for DestinationSnapshot {
    type Error = anyhow::Error;
    fn try_from(p: Snapshot) -> Result<Self> {
        ensure!(
            matches!(p.version, 1 | 2),
            "unsupported photo snapshot version"
        );
        Ok(Self {
            version: p.version,
            operation: p.operation,
            destination: p.destination.local(p.version == 2)?,
            expected: p.expected,
            max_existing_bytes: p.max_existing_bytes,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seal {
    pub version: u32,
    pub snapshot: Snapshot,
    pub authority_digest: String,
    pub max_payload_bytes: u64,
    pub payload: FileRevision,
}
impl From<SealedPhotoExport> for Seal {
    fn from(p: SealedPhotoExport) -> Self {
        Self {
            version: p.version,
            snapshot: p.snapshot.into(),
            authority_digest: p.authority_digest,
            max_payload_bytes: p.max_payload_bytes,
            payload: p.payload,
        }
    }
}
impl TryFrom<Seal> for SealedPhotoExport {
    type Error = anyhow::Error;
    fn try_from(p: Seal) -> Result<Self> {
        ensure!(
            matches!(p.version, 1 | 2) && p.version == p.snapshot.version,
            "unsupported or mixed photo seal version"
        );
        Ok(Self {
            version: p.version,
            snapshot: p.snapshot.try_into()?,
            authority_digest: p.authority_digest,
            max_payload_bytes: p.max_payload_bytes,
            payload: p.payload,
        })
    }
}

fn legacy() -> u32 {
    1
}
fn is_legacy(version: &u32) -> bool {
    *version == 1
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    #[serde(default = "legacy", skip_serializing_if = "is_legacy")]
    pub version: u32,
    pub state: ExportState,
    pub destination: StoredPath,
    pub recovery_directory: StoredPath,
    pub captured_original: Option<StoredPath>,
    pub detail: String,
}
impl From<ExportReceipt> for Receipt {
    fn from(p: ExportReceipt) -> Self {
        Self {
            version: 2,
            state: p.state,
            destination: StoredPath::encode(p.destination, true),
            recovery_directory: StoredPath::encode(p.recovery_directory, true),
            captured_original: p.captured_original.map(|v| StoredPath::encode(v, true)),
            detail: p.detail,
        }
    }
}
impl TryFrom<Receipt> for ExportReceipt {
    type Error = anyhow::Error;
    fn try_from(p: Receipt) -> Result<Self> {
        ensure!(
            matches!(p.version, 1 | 2),
            "unsupported export receipt version"
        );
        Ok(Self {
            state: p.state,
            destination: p.destination.local(p.version == 2)?,
            recovery_directory: p.recovery_directory.local(p.version == 2)?,
            captured_original: p
                .captured_original
                .map(|v| v.local(p.version == 2))
                .transpose()?,
            detail: p.detail,
        })
    }
}

/// Non-authoritative API diagnostics/targets: native writes, dual local reader.
pub mod native_path {
    use super::*;
    pub fn serialize<S: serde::Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        NativePath::from_path(path).serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(de: D) -> Result<PathBuf, D::Error> {
        let path = StoredPath::deserialize(de)?;
        let native = matches!(path, StoredPath::Native(_));
        path.local(native).map_err(serde::de::Error::custom)
    }
}
