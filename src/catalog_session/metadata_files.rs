//! Bounded C-to-F transport for controlled XMP and retained-evidence files.
//! F alone opens the selected destination and recovery objects. C retains SQL
//! authority and sends packet bytes as fixed-size binary chunks.
use super::{LeaseId, RootCapability};
use crate::{application::U64, storage_volume::NativePath};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const CHUNK_BYTES: usize = 16 * 1024;
pub const PACKET_BYTES: u64 = 16 * 1024 * 1024;
pub const EVIDENCE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Mode {
    Plan {
        destination: NativePath,
        max_existing_bytes: U64,
        alias_limits: crate::catalog_export_alias::AliasLimits,
    },
    Apply {
        plan: crate::metadata_export::ExportPlan,
    },
    Recover {
        plan: crate::metadata_export::ExportPlan,
    },
    Restore {
        plan: crate::metadata_export::ExportPlan,
    },
    Evidence {
        destination: NativePath,
    },
    Existing {
        plan: crate::metadata_export::ExportPlan,
        offset: U64,
        length: u32,
    },
}

impl Mode {
    fn payload_limit(&self) -> u64 {
        match self {
            Self::Evidence { .. } => EVIDENCE_BYTES,
            Self::Plan { .. } | Self::Apply { .. } => PACKET_BYTES,
            Self::Recover { .. } | Self::Restore { .. } | Self::Existing { .. } => 0,
        }
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::Plan {
                destination,
                max_existing_bytes,
                alias_limits,
            } => {
                super::validate_path(destination)?;
                ensure!(
                    (1..=EVIDENCE_BYTES).contains(&max_existing_bytes.0),
                    "existing sidecar byte limit"
                );
                alias_limits.validate()?;
            }
            Self::Apply { plan } | Self::Recover { plan } | Self::Restore { plan } => {
                crate::metadata_export::validate_plan_wire(plan)?;
                ensure!(plan.is_xmp(), "metadata file operation requires XMP plan");
            }
            Self::Evidence { destination } => super::validate_path(destination)?,
            Self::Existing {
                plan,
                offset,
                length,
            } => {
                crate::metadata_export::validate_plan_wire(plan)?;
                ensure!(
                    plan.is_xmp(),
                    "metadata existing-file read requires XMP plan"
                );
                let expected = plan
                    .expected
                    .as_ref()
                    .context("metadata plan has no existing destination")?;
                ensure!(
                    plan.max_existing_bytes.is_some()
                        && *length > 0
                        && *length as usize <= CHUNK_BYTES
                        && offset.0 < expected.bytes,
                    "metadata existing-file chunk authority"
                );
            }
        }
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
pub enum Action {
    Begin {
        mode: Mode,
        bytes: U64,
        blake3: String,
    },
    Append {
        offset: U64,
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    Finish,
    Release,
    Discover {
        directory: NativePath,
        after: Option<NativePath>,
        scan_rows: U64,
        page_rows: U64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: RootCapability,
    pub transfer: LeaseId,
    pub operation: U64,
    pub action: Action,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        super::validate_path(&self.root.canonical_root)?;
        ensure!(self.operation.0 > 0, "metadata file operation identity");
        match &self.action {
            Action::Begin {
                mode,
                bytes,
                blake3: digest,
            } => {
                mode.validate()?;
                ensure!(
                    bytes.0 <= mode.payload_limit(),
                    "metadata payload byte limit"
                );
                hash(digest)?;
                ensure!(
                    bytes.0 > 0 || digest == blake3::hash(&[]).to_hex().as_str(),
                    "empty payload digest"
                );
            }
            Action::Append { bytes, .. } => ensure!(
                !bytes.is_empty() && bytes.len() <= CHUNK_BYTES,
                "metadata upload chunk limit"
            ),
            Action::Discover {
                directory,
                after,
                scan_rows,
                page_rows,
            } => {
                super::validate_path(directory)?;
                if let Some(path) = after {
                    super::validate_path(path)?;
                }
                ensure!(
                    (1..=1000).contains(&scan_rows.0)
                        && (1..=100).contains(&page_rows.0)
                        && page_rows.0 <= scan_rows.0,
                    "metadata discovery bounds"
                );
            }
            Action::Finish | Action::Release => {}
        }
        crate::filesystem_worker::wire::encode(self, super::ENVELOPE_BYTES)?;
        Ok(())
    }
    pub fn cleanup(&self) -> bool {
        matches!(self.action, Action::Release)
    }
    pub fn binary(&self) -> &[u8] {
        match &self.action {
            Action::Append { bytes, .. } => bytes,
            _ => &[],
        }
    }
    pub fn set_binary(&mut self, bytes: Vec<u8>) -> Result<()> {
        match &mut self.action {
            Action::Append { bytes: value, .. } => *value = bytes,
            _ => ensure!(bytes.is_empty(), "unexpected metadata file binary"),
        };
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceReceipt {
    pub destination: NativePath,
    pub bytes: U64,
    pub blake3: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryEntry {
    pub directory: NativePath,
    pub name: NativePath,
    pub kind: String,
    pub operation: Option<String>,
    pub plan_digest: Option<String>,
    pub detail: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Value {
    Begun,
    Appended {
        offset: U64,
    },
    Plan(crate::metadata_export::ExportPlan),
    Receipt(crate::metadata_export::ExportReceipt),
    Evidence(EvidenceReceipt),
    Existing {
        offset: U64,
        total: U64,
        bytes: Vec<u8>,
        blake3: String,
    },
    Discovery {
        rows: Vec<DiscoveryEntry>,
        next: Option<NativePath>,
        scanned: U64,
    },
    Released,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub root: RootCapability,
    pub transfer: LeaseId,
    pub operation: U64,
    pub value: Value,
}
impl Reply {
    pub fn validate(&self, request: &Request) -> Result<()> {
        request.validate()?;
        ensure!(
            self.root == request.root
                && self.transfer == request.transfer
                && self.operation == request.operation,
            "metadata file reply authority"
        );
        match (&request.action, &self.value) {
            (Action::Begin { .. }, Value::Begun)
            | (Action::Append { .. }, Value::Appended { .. })
            | (Action::Release, Value::Released) => {}
            (Action::Finish, Value::Plan(plan)) => {
                crate::metadata_export::validate_plan_wire(plan)?
            }
            (Action::Finish, Value::Receipt(receipt)) => {
                crate::metadata_export::validate_export_receipt_basic(receipt)?
            }
            (Action::Finish, Value::Evidence(receipt)) => {
                super::validate_path(&receipt.destination)?;
                hash(&receipt.blake3)?;
            }
            (
                Action::Finish,
                Value::Existing {
                    offset,
                    total,
                    bytes,
                    blake3,
                },
            ) => {
                hash(blake3)?;
                ensure!(
                    bytes.len() <= CHUNK_BYTES && offset.0 <= total.0,
                    "metadata existing-file chunk reply bounds"
                );
            }
            (
                Action::Discover {
                    page_rows,
                    scan_rows,
                    ..
                },
                Value::Discovery { rows, scanned, .. },
            ) => ensure!(
                rows.len() <= page_rows.0 as usize && scanned.0 <= scan_rows.0,
                "metadata discovery reply bounds"
            ),
            _ => anyhow::bail!("metadata file reply action mismatch"),
        }
        Ok(())
    }
}
fn hash(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|v| v.is_ascii_hexdigit()),
        "metadata digest"
    );
    Ok(())
}
