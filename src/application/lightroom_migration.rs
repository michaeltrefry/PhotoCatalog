//! Guarded desktop migration API. Documents are independent exact UTF-8 uploads.
use super::{ErrorCode, U64};
pub use crate::lightroom_migration_worker::{
    input::PartDescriptor,
    protocol::{Guard, InputRole},
    worker::Operation,
};
use crate::storage_volume::NativePath;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub catalog: Option<String>,
    pub destination: NativePath,
    pub operation: Operation,
    pub parts: Vec<PartDescriptor>,
    pub timeout_ms: U64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Begin {
        operation: String,
        header: Header,
    },
    Upload {
        guard: Guard,
        role: InputRole,
        offset: U64,
        text: String,
    },
    Finish {
        guard: Guard,
        role: InputRole,
        blake3: String,
    },
    Act {
        guard: Guard,
    },
    Status {
        guard: Guard,
    },
    Cancel {
        guard: Guard,
    },
    RetryDrain {
        guard: Guard,
    },
    ResultPage {
        guard: Guard,
        page: U64,
        offset: U64,
        maximum_bytes: U64,
    },
    Discard {
        guard: Guard,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Uploading,
    Ready,
    Running,
    CancelRequested,
    DrainPending,
    Complete,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: ErrorCode,
    pub detail: String,
    pub required: Option<U64>,
    pub available: Option<U64>,
    pub poisoned: bool,
    pub outcome_unknown: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub guard: Guard,
    pub phase: Phase,
    pub catalog: Option<String>,
    pub uploaded: U64,
    pub next_role: Option<InputRole>,
    pub progress: Option<(String, U64, Option<U64>)>,
    pub failure: Option<Failure>,
    pub result: Option<ResultIdentity>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultIdentity {
    pub bytes: U64,
    pub blake3: String,
    pub pages: U64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Response {
    Status(Snapshot),
    Page {
        guard: Guard,
        page: U64,
        offset: U64,
        text: String,
        next_offset: Option<U64>,
    },
    Discarded {
        guard: Guard,
    },
}
