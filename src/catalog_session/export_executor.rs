//! Closed managed export-executor and persisted-transport protocol. C carries
//! only an executor identity and compact attempt facts; F selects and retains
//! every filesystem path and deletion identity.
use super::{LeaseId, RootCapability};
use crate::application::U64;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const MAX_DIRECTORIES: u64 = 1024;
pub const ERROR_BYTES: usize = 2048;

/// C retains this root-scoped counter across service reopen. The exact root
/// capability includes the F epoch/session; a new root starts at generation 1.
pub fn executor_id(root: &RootCapability, generation: u64) -> Result<LeaseId> {
    ensure!(generation > 0, "zero export executor generation");
    let root_bytes = crate::filesystem_worker::wire::encode(root, super::ENVELOPE_BYTES)?;
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&blake3::hash(&root_bytes).as_bytes()[..8]);
    bytes[8..].copy_from_slice(&generation.to_be_bytes());
    LeaseId::parse(&uuid::Uuid::from_bytes(bytes).to_string())
}
pub fn generation(root: &RootCapability, executor: &LeaseId) -> Result<u64> {
    let id = uuid::Uuid::parse_str(executor.as_str())?;
    let generation = u64::from_be_bytes(id.as_bytes()[8..].try_into()?);
    ensure!(
        executor_id(root, generation)? == *executor,
        "foreign export executor generation"
    );
    Ok(generation)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub job: String,
    pub sequence: i64,
    pub attempt: String,
    pub authority: String,
}
impl Attempt {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.job.is_empty() && self.job.len() <= 128,
            "export recovery job bound"
        );
        ensure!(self.sequence >= 0, "export recovery sequence bound");
        ensure!(
            !self.attempt.is_empty() && self.attempt.len() <= 128,
            "export recovery attempt bound"
        );
        ensure!(
            self.authority.len() == 64
                && self.authority.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "export recovery authority digest"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub token: LeaseId,
    pub attempt: Attempt,
}
impl Candidate {
    pub fn validate(&self) -> Result<()> {
        self.attempt.validate()
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
    Acquire,
    Recover { max_directories: U64 },
    Discard { token: LeaseId },
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: RootCapability,
    pub executor: LeaseId,
    pub operation: U64,
    pub action: Action,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        super::validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        generation(&self.root, &self.executor)?;
        ensure!(self.operation.0 > 0, "zero export executor operation");
        match &self.action {
            Action::Acquire => ensure!(self.operation.0 == 1, "export executor Acquire operation"),
            Action::Recover { max_directories } => ensure!(
                (1..=MAX_DIRECTORIES).contains(&max_directories.0),
                "export recovery directory bound"
            ),
            Action::Discard { .. } | Action::Release => {}
        }
        crate::filesystem_worker::wire::encode(self, super::ENVELOPE_BYTES)?;
        Ok(())
    }
    pub fn cleanup(&self) -> bool {
        matches!(&self.action, Action::Discard { .. } | Action::Release)
    }
    pub fn digest(&self) -> Result<String> {
        Ok(blake3::Hash::from_bytes(self.receipt_digest()?)
            .to_hex()
            .to_string())
    }
    pub fn receipt_digest(&self) -> Result<[u8; 32]> {
        Ok(*blake3::hash(&crate::filesystem_worker::wire::encode(
            self,
            super::ENVELOPE_BYTES,
        )?)
        .as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Value {
    Acquired,
    Recovery {
        scanned: U64,
        cleaned: U64,
        retained: U64,
        retained_example: Option<String>,
        candidate: Option<Candidate>,
    },
    Discarded {
        candidate: Option<Candidate>,
    },
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub root: RootCapability,
    pub executor: LeaseId,
    pub operation: U64,
    pub request_digest: String,
    pub value: Value,
}
impl Reply {
    pub fn validate(&self, request: &Request) -> Result<()> {
        request.validate()?;
        ensure!(
            self.root == request.root
                && self.executor == request.executor
                && self.operation == request.operation
                && self.request_digest == request.digest()?,
            "export executor reply provenance mismatch"
        );
        match (&request.action, &self.value) {
            (Action::Acquire, Value::Acquired) | (Action::Release, Value::Released) => {}
            (
                Action::Recover { max_directories },
                Value::Recovery {
                    scanned,
                    cleaned,
                    retained,
                    retained_example,
                    candidate,
                },
            ) => {
                ensure!(
                    scanned.0 <= max_directories.0 && cleaned.0 <= scanned.0,
                    "export recovery reply counts"
                );
                ensure!(retained.0 <= scanned.0, "export recovery retained count");
                ensure!(
                    retained_example
                        .as_ref()
                        .is_none_or(|value| value.len() <= ERROR_BYTES),
                    "export recovery retained diagnostic bound"
                );
                ensure!(
                    retained.0 == 0 || candidate.is_none(),
                    "incomplete export inventory exposed a candidate"
                );
                if let Some(candidate) = candidate {
                    candidate.validate()?;
                }
            }
            (Action::Discard { .. }, Value::Discarded { candidate }) => {
                if let Some(candidate) = candidate {
                    candidate.validate()?;
                }
            }
            _ => anyhow::bail!("export executor reply action mismatch"),
        }
        crate::filesystem_worker::wire::encode(self, super::ENVELOPE_BYTES)?;
        Ok(())
    }
}
