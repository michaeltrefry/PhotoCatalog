//! Closed F stage/prepared operations. No caller-controlled artifact filename.
use super::{LeaseId, RootCapability};
use crate::{application::U64, storage_volume::NativePath};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const LEGACY_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Artifact {
    Input,
    Header,
    Receipt,
    Error,
    Decoded,
    Encoded(u8),
    Rgb(u8),
    Prepared,
}
impl Artifact {
    pub fn name(self) -> Result<&'static str> {
        Ok(match self {
            Self::Input => "input.encoded",
            Self::Header => "header.ready",
            Self::Receipt => "result.json",
            Self::Error => "error.json",
            Self::Decoded => "decoded.ready",
            Self::Prepared => "prepared.linear",
            Self::Encoded(0) => "0.preview",
            Self::Encoded(1) => "1.preview",
            Self::Rgb(0) => "0.rgb",
            Self::Rgb(1) => "1.rgb",
            _ => anyhow::bail!("stage artifact index"),
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub workers: u8,
    pub encoded: U64,
    pub rgb: U64,
    pub prepared: U64,
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=16).contains(&self.workers)
                && self.encoded.0 <= 256 * 1024 * 1024
                && self.rgb.0 <= 2 * 8192 * 8192 * 3
                && self.prepared.0 <= crate::preview::prepared_cache::MAX_PROXY_BYTES,
            "stage allowance"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", content = "arguments", deny_unknown_fields)]
pub enum Action {
    /// Opens only a retained compatibility thumbnail under the catalog root.
    BeginLegacyRead {
        hash: String,
        allowance: U64,
    },
    Admit {
        limits: Limits,
    },
    Upload {
        stage: LeaseId,
        offset: U64,
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    SealInput {
        stage: LeaseId,
        bytes: U64,
        digest: String,
    },
    Arm {
        stage: LeaseId,
        native: U64,
    },
    NativeDrained {
        stage: LeaseId,
        native: U64,
    },
    Metadata {
        stage: LeaseId,
        artifact: Artifact,
    },
    BeginRead {
        stage: LeaseId,
        artifact: Artifact,
        bytes: U64,
        digest: String,
    },
    Read {
        stage: LeaseId,
        offset: U64,
    },
    FinishRead {
        stage: LeaseId,
    },
    AbortRead {
        stage: LeaseId,
    },
    Release {
        stage: LeaseId,
    },
    Recover {
        limit: u16,
    },
    PreparedInitialize,
    ObserveSource {
        path: NativePath,
    },
    PreparedAdopt {
        stage: LeaseId,
        key: String,
        receipt: crate::edit::PreparedProxyReceipt,
    },
    PreparedRemove {
        key: String,
    },
    /// G sends this only after the catalog and every native owner have drained.
    AbandonOwned,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: RootCapability,
    pub operation: U64,
    #[serde(default)]
    pub supervisor: bool,
    pub action: Action,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "zero stage operation");
        ensure!(
            !self.privileged() || self.supervisor,
            "stage native transition requires supervisor"
        );
        match &self.action {
            Action::Admit { limits } => limits.validate()?,
            Action::BeginLegacyRead { hash: digest, .. } => hash(digest)?,
            Action::Upload { bytes, .. } => ensure!(
                !bytes.is_empty() && bytes.len() <= super::preview_io::CHUNK_BYTES,
                "stage chunk limit"
            ),
            Action::SealInput { bytes, digest, .. } | Action::BeginRead { bytes, digest, .. } => {
                ensure!(bytes.0 > 0, "empty stage object");
                hash(digest)?;
            }
            Action::Arm { native, .. } | Action::NativeDrained { native, .. } => {
                ensure!(native.0 > 0, "zero stage native identity")
            }
            Action::Metadata { artifact, .. } => ensure!(
                matches!(
                    artifact,
                    Artifact::Header | Artifact::Receipt | Artifact::Error | Artifact::Decoded
                ),
                "stage metadata kind"
            ),
            Action::Recover { limit } => ensure!((1..=128).contains(limit), "stage recovery batch"),
            Action::PreparedAdopt { key, .. } | Action::PreparedRemove { key } => hash(key)?,
            Action::ObserveSource { path } => super::validate_path(path)?,
            _ => {}
        }
        if let Action::BeginRead { artifact, .. } = &self.action {
            artifact.name()?;
        }
        Ok(())
    }
    pub fn cleanup(&self) -> bool {
        matches!(
            self.action,
            Action::NativeDrained { .. }
                | Action::Release { .. }
                | Action::FinishRead { .. }
                | Action::AbortRead { .. }
                | Action::AbandonOwned
        )
    }
    pub fn privileged(&self) -> bool {
        matches!(
            self.action,
            Action::Arm { .. } | Action::NativeDrained { .. } | Action::AbandonOwned
        )
    }
    pub fn binary(&self) -> &[u8] {
        match &self.action {
            Action::Upload { bytes, .. } => bytes,
            _ => &[],
        }
    }
    pub fn set_binary(&mut self, bytes: Vec<u8>) -> Result<()> {
        match &mut self.action {
            Action::Upload { bytes: out, .. } => *out = bytes,
            _ => ensure!(bytes.is_empty(), "unexpected stage binary"),
        };
        Ok(())
    }
}
fn hash(s: &str) -> Result<()> {
    ensure!(
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "stage digest/key"
    );
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
pub enum Value {
    /// A read lease uses the existing Read/FinishRead/AbortRead stage field.
    /// It has no worker stage to release. Missing files return None and zero.
    LegacyRead {
        transfer: Option<LeaseId>,
        bytes: U64,
    },
    Unit,
    Admitted {
        stage: LeaseId,
        ready: bool,
        error: Option<String>,
    },
    Path(NativePath),
    Metadata(Option<Vec<u8>>),
    Chunk {
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    Count(u16),
    Source(crate::preview::prepared_cache::SourceInstance),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub epoch: LeaseId,
    pub session: LeaseId,
    pub operation: U64,
    pub value: Value,
}
impl Reply {
    pub fn validate(&self, r: &Request) -> Result<()> {
        ensure!(
            self.epoch == r.root.epoch
                && self.session == r.root.session
                && self.operation == r.operation,
            "stage reply identity"
        );
        if let Value::LegacyRead { transfer, bytes } = &self.value {
            let Action::BeginLegacyRead { allowance, .. } = &r.action else {
                anyhow::bail!("unexpected legacy read receipt");
            };
            ensure!(
                bytes.0 <= LEGACY_BYTES && bytes.0 <= allowance.0,
                "legacy read receipt allowance"
            );
            ensure!(
                transfer.is_some() || bytes.0 == 0,
                "missing legacy read receipt length"
            );
        }
        if let Value::Metadata(Some(v)) = &self.value {
            ensure!(
                v.len() <= super::native::REQUEST_BYTES,
                "stage metadata limit"
            );
        }
        if let Value::Chunk { bytes: v } = &self.value {
            ensure!(
                v.len() <= super::preview_io::CHUNK_BYTES,
                "stage reply chunk limit"
            );
        }
        Ok(())
    }
    pub fn binary(&self) -> &[u8] {
        match &self.value {
            Value::Chunk { bytes: v } => v,
            _ => &[],
        }
    }
    pub fn set_binary(&mut self, v: Vec<u8>) -> Result<()> {
        match &mut self.value {
            Value::Chunk { bytes: out } => *out = v,
            _ => ensure!(v.is_empty(), "unexpected stage reply binary"),
        };
        Ok(())
    }
}
