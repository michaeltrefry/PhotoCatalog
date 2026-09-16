//! Private C admission for G-owned managed backup execution.
//!
//! C may attest the currently open catalog token and its already admitted
//! physical identity. It never starts B or F. G consumes this one-shot reply to
//! start the existing managed backup coordinator in its own process.
use super::super::{Actor, Bridge, BridgeError, Cancellation, Envelope, ErrorCode, Work, error};
use crate::{catalog_session::PhysicalObjectId, storage_volume::NativePath};
use serde::{Deserialize, Serialize};
use std::sync::mpsc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdmissionRequest {
    pub catalog: String,
}
impl AdmissionRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.catalog.is_empty() && self.catalog.len() <= 128,
            "catalog identity length"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Admission {
    pub source: NativePath,
    pub expected_source: PhysicalObjectId,
}
impl Admission {
    pub fn validate(&self) -> anyhow::Result<()> {
        crate::catalog_session::validate_path(&self.source)?;
        anyhow::ensure!(
            self.source.to_path()?.is_absolute(),
            "backup source must be absolute"
        );
        self.expected_source.validate()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub(super) enum AdmissionReply {
    Ok(Admission),
    Error(BridgeError),
}
impl AdmissionReply {
    pub fn error(mut error: BridgeError) -> Self {
        let mut end = error.message.len().min(super::wire::ERROR_BYTES);
        while !error.message.is_char_boundary(end) {
            end -= 1;
        }
        error.message.truncate(end);
        error.message = std::mem::take(&mut error.message)
            .into_boxed_str()
            .into_string();
        Self::Error(error)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Ok(admission) => admission.validate(),
            Self::Error(error) => anyhow::ensure!(
                error.message.len() <= super::wire::ERROR_BYTES,
                "backup admission error length"
            ),
        }
    }
}

pub(super) struct Pending {
    pub receiver: mpsc::Receiver<AdmissionReply>,
    pub cancel: Cancellation,
}

impl Bridge {
    pub(super) fn backup_admission(
        &self,
        request: AdmissionRequest,
    ) -> Result<Pending, BridgeError> {
        request
            .validate()
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        let bytes = serde_json::to_vec(&request)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        if bytes.len() > self.0.shared.limits.request_bytes {
            return Err(error(
                ErrorCode::ResourceLimit,
                "backup admission request byte limit",
            ));
        }
        let (tx, receiver) = mpsc::sync_channel(1);
        let cancel = Cancellation::default();
        let mut queue = self.0.shared.queue.lock().unwrap();
        if queue.stopping {
            return Err(error(ErrorCode::Closed, "catalog owner closed"));
        }
        if queue
            .pending
            .iter()
            .any(|entry| matches!(&entry.work, Work::BackupAdmission(..)))
        {
            return Err(error(
                ErrorCode::Busy,
                "backup admission is already pending",
            ));
        }
        queue.pending.push_front(Envelope {
            work: Work::BackupAdmission(request, tx),
            cancel: cancel.clone(),
            created: std::time::Instant::now(),
        });
        self.0.shared.wake.notify_all();
        Ok(Pending { receiver, cancel })
    }
}

impl Actor {
    pub(super) fn backup_admission(
        &mut self,
        request: AdmissionRequest,
    ) -> Result<Admission, BridgeError> {
        if self.managed.is_none() {
            return Err(error(
                ErrorCode::InvalidRequest,
                "backup admission requires the managed catalog actor",
            ));
        }
        if self.open.as_ref().is_some_and(|open| open.closing)
            || self.failed_admission.is_some()
            || self.failed_session.is_some()
        {
            return Err(error(
                ErrorCode::Busy,
                "catalog is closing; retry Close after cleanup failure",
            ));
        }
        if self.migration.held() {
            return Err(error(
                ErrorCode::Busy,
                "migration target hold: backup waits for checked drain",
            ));
        }
        let export_control = self.shared.exports.clone();
        let export_busy = export_control.lock().unwrap().busy();
        let open = self.current(&request.catalog)?;
        if open.relink.write_hold() {
            return Err(error(
                ErrorCode::Busy,
                "relink write hold: wait for completion or cancel the relink operation",
            ));
        }
        if open.exports.write_hold(&export_control) || export_busy {
            return Err(error(
                ErrorCode::Busy,
                "finish or cancel the export operation before starting backup",
            ));
        }
        if open.metadata_write.write_hold() {
            return Err(error(
                ErrorCode::Busy,
                "metadata write hold: wait for completion before starting backup",
            ));
        }
        let expected_source = open.catalog.managed_physical_identity().ok_or_else(|| {
            error(
                ErrorCode::Native,
                "managed catalog physical identity is unavailable",
            )
        })?;
        Ok(Admission {
            source: NativePath::from_path(&open.catalog.root),
            expected_source,
        })
    }
}
