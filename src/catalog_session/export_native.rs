//! Closed managed export-native custody. The registration carries the exact F
//! Begin packet so G can bind native ownership before any stage side effect.
use super::{LeaseId, RootCapability, export_stage};
use crate::application::U64;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicBool;

pub const ERROR_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Action {
    Register {
        begin: Box<export_stage::Request>,
        worker_bytes: U64,
        working_bytes: U64,
    },
    Spawn,
    Start,
    Stop,
    RetryDrain,
    Retire,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: RootCapability,
    pub operation: U64,
    pub stage: LeaseId,
    pub binding: export_stage::Binding,
    pub action: Action,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "zero export native operation");
        super::validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        self.binding.validate()?;
        match &self.action {
            Action::Register {
                begin,
                worker_bytes,
                working_bytes,
            } => {
                begin.validate()?;
                ensure!(
                    !begin.supervisor
                        && matches!(begin.action, export_stage::Action::Begin { .. })
                        && begin.root == self.root
                        && begin.stage == self.stage
                        && begin.binding == self.binding,
                    "export native registration binding mismatch"
                );
                ensure!(
                    worker_bytes.0 > 0 && worker_bytes.0 <= working_bytes.0,
                    "export native memory admission"
                );
            }
            _ => {}
        }
        Ok(())
    }
    pub fn cleanup(&self) -> bool {
        matches!(
            self.action,
            Action::Stop | Action::RetryDrain | Action::Retire
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Registered,
    Begun,
    Ready,
    Spawned,
    Running,
    StopRequested,
    WaitFailed,
    ExitObserved,
    PipeJoinFailed,
    Drained,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    None,
    Queued,
    Sent,
    Completed,
    NeverDispatched,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub epoch: LeaseId,
    pub session: LeaseId,
    pub operation: U64,
    pub stage: LeaseId,
    pub binding: export_stage::Binding,
    pub pid: Option<u32>,
    pub phase: Phase,
    pub started: bool,
    pub exit_code: Option<i32>,
    pub success: Option<bool>,
    pub stage_high_water: U64,
    pub pending_stage_operation: Option<U64>,
    pub pending_dispatch: DispatchState,
    pub error: Option<String>,
}
impl Status {
    pub fn validate(&self, request: &Request) -> Result<()> {
        ensure!(
            self.epoch == request.root.epoch
                && self.session == request.root.session
                && self.operation == request.operation
                && self.stage == request.stage
                && self.binding == request.binding,
            "export native status binding mismatch"
        );
        ensure!(
            self.error.as_ref().is_none_or(|v| v.len() <= ERROR_BYTES),
            "export native status error limit"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub epoch: LeaseId,
    pub session: LeaseId,
    pub root: LeaseId,
    pub operation: U64,
    pub stage: LeaseId,
}
impl Key {
    pub fn new(root: &RootCapability, operation: U64, stage: &LeaseId) -> Self {
        Self {
            epoch: root.epoch.clone(),
            session: root.session.clone(),
            root: root.token.clone(),
            operation,
            stage: stage.clone(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "zero export native query identity");
        Ok(())
    }
    pub fn matches(&self, root: &RootCapability) -> bool {
        self.epoch == root.epoch && self.session == root.session && self.root == root.token
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryAction {
    Status,
    RetryDrain,
    Retire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub key: Key,
    pub action: QueryAction,
}

pub trait CatalogExportNative: Send + Sync {
    fn signal_stop(&self, request: &Request) -> Result<()> {
        ensure!(
            matches!(request.action, Action::Stop),
            "export native stop action"
        );
        self.call(request, &AtomicBool::new(false)).map(|_| ())
    }
    fn call(&self, request: &Request, cancel: &AtomicBool) -> Result<Status>;
    fn status(&self, key: &Key) -> Result<Status>;
}

pub(crate) fn protocol_layouts() -> [(usize, usize); 6] {
    [
        (
            std::mem::size_of::<Action>(),
            std::mem::align_of::<Action>(),
        ),
        (
            std::mem::size_of::<Request>(),
            std::mem::align_of::<Request>(),
        ),
        (
            std::mem::size_of::<Status>(),
            std::mem::align_of::<Status>(),
        ),
        (std::mem::size_of::<Key>(), std::mem::align_of::<Key>()),
        (std::mem::size_of::<Query>(), std::mem::align_of::<Query>()),
        (
            std::mem::size_of::<export_stage::Request>(),
            std::mem::align_of::<export_stage::Request>(),
        ),
    ]
}

#[cfg(test)]
mod tests;
