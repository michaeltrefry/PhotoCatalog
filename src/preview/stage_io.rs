//! C/G stage adapter. One exact pending identity survives uncertain transport.
use crate::application::U64;
use crate::catalog_session::{CatalogFilesystem, LeaseId, RootCapability, preview_stage::*};
use crate::filesystem_worker::wire::{Failure, FailureKind};
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, Mutex, atomic::AtomicBool};

#[derive(Debug)]
pub(crate) struct Busy(pub &'static str);
impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Busy {}
struct State {
    next: u64,
    pending: Option<Request>,
    pending_lane: u64,
}
pub(crate) struct Calls {
    filesystem: Arc<dyn CatalogFilesystem>,
    pub root: RootCapability,
    supervisor: bool,
    native_next: Arc<std::sync::atomic::AtomicU64>,
    lane: u64,
    lane_next: Arc<std::sync::atomic::AtomicU64>,
    wait_busy: bool,
    native_pending: Arc<Mutex<Option<U64>>>,
    active_native: std::sync::atomic::AtomicU64,
    native_stopping: AtomicBool,
    unadmitted: Mutex<Option<LeaseId>>,
    legacy_transfer: Mutex<Option<LeaseId>>,
    state: Arc<Mutex<State>>,
}
pub(crate) fn retry_busy<T>(mut work: impl FnMut() -> Result<T>) -> Result<T> {
    loop {
        match work() {
            Err(error) if error.downcast_ref::<Busy>().is_some() => {
                std::thread::sleep(std::time::Duration::from_millis(2))
            }
            result => return result,
        }
    }
}
pub(crate) fn digest(request: &Request) -> Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    serde_json::to_writer(&mut hash, request)?;
    hash.update(request.binary());
    Ok(*hash.finalize().as_bytes())
}
impl Calls {
    pub fn new(
        filesystem: Arc<dyn CatalogFilesystem>,
        root: RootCapability,
        supervisor: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            filesystem,
            root,
            supervisor,
            native_next: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            lane: 0,
            lane_next: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            wait_busy: false,
            native_pending: Arc::new(Mutex::new(None)),
            active_native: std::sync::atomic::AtomicU64::new(0),
            native_stopping: AtomicBool::new(false),
            unadmitted: Mutex::new(None),
            legacy_transfer: Mutex::new(None),
            state: Arc::new(Mutex::new(State {
                next: 1,
                pending: None,
                pending_lane: 0,
            })),
        })
    }
    #[cfg(test)]
    pub fn lane(&self) -> Result<Arc<Self>> {
        self.new_lane(false)
    }
    pub fn task_lane(&self) -> Result<Arc<Self>> {
        self.new_lane(true)
    }
    fn new_lane(&self, wait_busy: bool) -> Result<Arc<Self>> {
        let lane = self
            .lane_next
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| n.checked_add(1),
            )
            .map_err(|_| anyhow::anyhow!("stage caller identity exhausted"))?;
        Ok(Arc::new(Self {
            filesystem: self.filesystem.clone(),
            root: self.root.clone(),
            supervisor: self.supervisor,
            native_next: self.native_next.clone(),
            lane,
            lane_next: self.lane_next.clone(),
            wait_busy,
            native_pending: self.native_pending.clone(),
            active_native: std::sync::atomic::AtomicU64::new(0),
            native_stopping: AtomicBool::new(false),
            unadmitted: Mutex::new(None),
            legacy_transfer: Mutex::new(None),
            state: self.state.clone(),
        }))
    }
    pub fn native_spawn(
        &self,
        request: &mut crate::catalog_session::native::Request,
    ) -> Result<crate::catalog_session::native::Status> {
        loop {
            if request.operation.0 == 0
                && self
                    .native_stopping
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(Failure::new(
                    FailureKind::Canceled,
                    "native admission canceled while waiting for operation allocation",
                )
                .into());
            }

            let mut pending = self
                .native_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if pending.is_some_and(|op| op != request.operation) {
                drop(pending);
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            if request.operation.0 == 0 {
                if self
                    .native_stopping
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    return Err(Failure::new(
                        FailureKind::Canceled,
                        "native admission canceled before operation allocation",
                    )
                    .into());
                }
                request.operation = self.native_operation()?;
            }
            *pending = Some(request.operation);
            self.active_native
                .store(request.operation.0, std::sync::atomic::Ordering::Release);
            if self
                .native_stopping
                .load(std::sync::atomic::Ordering::Acquire)
            {
                self.filesystem
                    .native()
                    .context("native custody missing")?
                    .signal_stop(&self.root, request.operation)?;
            }
            let result = retry_busy(|| {
                self.filesystem
                    .native()
                    .context("native custody missing")?
                    .call(request, &AtomicBool::new(false))
            });
            match &result {
                Ok(status) => {
                    status.validate(&request.root, request.operation)?;
                    let crate::catalog_session::native::Action::Spawn { stage, .. } =
                        &request.action
                    else {
                        anyhow::bail!("native admission requires Spawn")
                    };
                    ensure!(status.stage == *stage, "native spawn stage mismatch");
                    *pending = None;
                    if self
                        .native_stopping
                        .load(std::sync::atomic::Ordering::Acquire)
                    {
                        self.filesystem
                            .native()
                            .context("native custody missing")?
                            .signal_stop(&self.root, request.operation)?;
                    }
                }
                Err(error)
                    if error
                        .downcast_ref::<Failure>()
                        .is_some_and(|f| f.kind != FailureKind::Unknown) =>
                {
                    *pending = None;
                }
                _ => {}
            }
            return result;
        }
    }
    pub fn native_canceled(&self) -> bool {
        self.native_stopping
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn signal_native_stop(&self) {
        self.native_stopping
            .store(true, std::sync::atomic::Ordering::Release);
        let operation = self
            .active_native
            .load(std::sync::atomic::Ordering::Acquire);
        if operation != 0
            && let Some(native) = self.filesystem.native()
        {
            let _ = native.signal_stop(&self.root, U64(operation));
        }
    }
    pub fn retired_native(&self, operation: U64) {
        let _ = self.active_native.compare_exchange(
            operation.0,
            0,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }
    pub fn confirm_native(&self, operation: U64) {
        let mut pending = self
            .native_pending
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if *pending == Some(operation) {
            *pending = None;
        }
    }
    pub fn native_operation(&self) -> Result<U64> {
        self.native_next
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| n.checked_add(1),
            )
            .map(U64)
            .map_err(|_| anyhow::anyhow!("native operation exhausted"))
    }
    pub fn filesystem(&self) -> &Arc<dyn CatalogFilesystem> {
        &self.filesystem
    }
    pub fn call(&self, action: Action, cancel: &AtomicBool) -> Result<Value> {
        if !self.wait_busy {
            return self.call_once(action, cancel);
        }
        loop {
            match self.call_once(action.clone(), cancel) {
                Err(error)
                    if error.downcast_ref::<Busy>().is_some()
                        && !self
                            .native_stopping
                            .load(std::sync::atomic::Ordering::Acquire) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(2))
                }
                result => return result,
            }
        }
    }
    fn call_once(&self, action: Action, cancel: &AtomicBool) -> Result<Value> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let proposed = Request {
            root: self.root.clone(),
            operation: U64(state.pending.as_ref().map_or(state.next, |p| p.operation.0)),
            supervisor: self.supervisor,
            action,
        };
        proposed.validate()?;
        if let Some(pending) = &state.pending {
            ensure!(
                state.pending_lane == self.lane,
                Busy("stage transport busy: another caller owns an unresolved operation")
            );
            ensure!(
                digest(pending)? == digest(&proposed)?,
                "An uncertain stage operation remains owned; retry the exact operation before another stage action"
            );
        } else {
            state.next = state
                .next
                .checked_add(1)
                .context("stage operation identity exhausted")?;
            state.pending = Some(proposed);
            state.pending_lane = self.lane;
        }
        let request = state.pending.as_ref().unwrap();
        let result = self.filesystem.preview_stage_call(request, cancel);
        let terminal = match &result {
            Ok(reply) => {
                reply.validate(request)?;
                true
            }
            Err(error) => error.downcast_ref::<Failure>().is_some_and(|f| {
                f.object_receipt.as_ref().is_some_and(|r| {
                    r.operation == request.operation
                        && r.step == U64(0)
                        && digest(request).is_ok_and(|d| d == r.request_digest)
                }) || f.kind != FailureKind::Unknown
            }),
        };
        if terminal {
            state.pending.take();
        }
        result.map(|r| r.value)
    }
    /// Reconcile the exact admitted request. An authenticated terminal error
    /// retires the request but is still returned to its caller.
    pub fn cleanup_unadmitted(&self) -> Result<()> {
        let mut retained = self.unadmitted.lock().unwrap_or_else(|p| p.into_inner());
        if retained.is_none() {
            match self.reconcile_value()? {
                Some(Value::Admitted { stage, .. }) => *retained = Some(stage),
                Some(Value::LegacyRead { transfer, .. }) => {
                    *self
                        .legacy_transfer
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = transfer;
                }
                _ => {}
            }
        }
        {
            let mut transfer = self
                .legacy_transfer
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(id) = transfer.as_ref() {
                self.abort_read(id)?;
                *transfer = None;
            }
        }
        if let Some(stage) = retained.as_ref() {
            self.abort_read(stage)?;
            self.unit(Action::Release {
                stage: stage.clone(),
            })?;
            *retained = None;
        }
        Ok(())
    }
    pub fn reconcile(&self) -> Result<()> {
        self.reconcile_value().map(|_| ())
    }
    pub fn reconcile_value(&self) -> Result<Option<Value>> {
        loop {
            let action = {
                let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.pending.is_some() && state.pending_lane != self.lane {
                    drop(state);
                    if !self.wait_busy
                        || self
                            .native_stopping
                            .load(std::sync::atomic::Ordering::Acquire)
                    {
                        return Err(Busy(
                            "stage transport busy: another caller owns reconciliation",
                        )
                        .into());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                state.pending.as_ref().map(|r| r.action.clone())
            };
            return action
                .map(|action| self.call(action, &AtomicBool::new(false)))
                .transpose();
        }
    }
    pub fn reconcile_cleanup(&self) -> Result<()> {
        let result = self.reconcile();
        if self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pending
            .is_some()
        {
            result?;
        }
        Ok(())
    }
    pub fn abort_read(&self, stage: &LeaseId) -> Result<()> {
        self.reconcile_cleanup()?;
        self.unit(Action::AbortRead {
            stage: stage.clone(),
        })
    }
    pub fn unit(&self, action: Action) -> Result<()> {
        ensure!(
            matches!(self.call(action, &AtomicBool::new(false))?, Value::Unit),
            "wrong stage unit reply"
        );
        Ok(())
    }
    pub fn metadata(&self, stage: &LeaseId, artifact: Artifact) -> Result<Option<Vec<u8>>> {
        match self.call(
            Action::Metadata {
                stage: stage.clone(),
                artifact,
            },
            &AtomicBool::new(false),
        )? {
            Value::Metadata(v) => Ok(v),
            _ => anyhow::bail!("wrong stage metadata reply"),
        }
    }
    pub fn upload(&self, stage: &LeaseId, bytes: &[u8], cancel: &AtomicBool) -> Result<()> {
        let mut offset = 0;
        for chunk in bytes.chunks(crate::catalog_session::preview_io::CHUNK_BYTES) {
            self.call(
                Action::Upload {
                    stage: stage.clone(),
                    offset: U64(offset),
                    bytes: chunk.to_vec(),
                },
                cancel,
            )?;
            offset += chunk.len() as u64;
        }
        self.unit(Action::SealInput {
            stage: stage.clone(),
            bytes: U64(offset),
            digest: blake3::hash(bytes).to_hex().to_string(),
        })
    }
    /// Same single F stream, rooted in the selected catalog's legacy namespace.
    /// The lane retains its transfer token until Finish/Abort is acknowledged.
    pub fn legacy_read(
        &self,
        hash: &str,
        allowance: u64,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<u8>>> {
        let result = (|| -> Result<Option<Vec<u8>>> {
            let Value::LegacyRead { transfer, bytes } = self.call(
                Action::BeginLegacyRead {
                    hash: hash.into(),
                    allowance: U64(allowance),
                },
                cancel,
            )?
            else {
                anyhow::bail!("wrong legacy read admission reply")
            };
            let Some(transfer) = transfer else {
                ensure!(bytes.0 == 0, "missing legacy read length");
                return Ok(None);
            };
            *self
                .legacy_transfer
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(transfer.clone());
            ensure!(
                bytes.0 <= allowance && bytes.0 <= LEGACY_BYTES,
                "legacy read admission length"
            );
            let mut output = vec![0; usize::try_from(bytes.0)?];
            let mut offset = 0;
            while offset < output.len() {
                let Value::Chunk { bytes: chunk } = self.call(
                    Action::Read {
                        stage: transfer.clone(),
                        offset: U64(offset as u64),
                    },
                    cancel,
                )?
                else {
                    anyhow::bail!("wrong legacy read chunk")
                };
                ensure!(
                    !chunk.is_empty() && chunk.len() <= output.len() - offset,
                    "legacy read progress"
                );
                output[offset..offset + chunk.len()].copy_from_slice(&chunk);
                offset += chunk.len();
            }
            self.unit(Action::FinishRead { stage: transfer })?;
            self.legacy_transfer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            ensure!(
                blake3::hash(&output).to_hex().as_str() == hash,
                "legacy preview checksum mismatch"
            );
            Ok(Some(output))
        })();
        if let Err(error) = result {
            self.cleanup_unadmitted()
                .context("legacy read failed; exact reconciliation/Abort remains owned")?;
            return Err(error);
        }
        result
    }
    pub fn read_into(
        &self,
        stage: &LeaseId,
        artifact: Artifact,
        bytes: u64,
        digest: &str,
        out: &mut [u8],
        cancel: &AtomicBool,
    ) -> Result<()> {
        ensure!(
            u64::try_from(out.len())? == bytes,
            "stage destination admission mismatch"
        );
        let result = (|| -> Result<()> {
            self.unit(Action::BeginRead {
                stage: stage.clone(),
                artifact,
                bytes: U64(bytes),
                digest: digest.into(),
            })?;
            let mut offset = 0;
            while offset < out.len() {
                let value = self.call(
                    Action::Read {
                        stage: stage.clone(),
                        offset: U64(offset as u64),
                    },
                    cancel,
                )?;
                let Value::Chunk { bytes: chunk } = value else {
                    anyhow::bail!("wrong stage chunk reply")
                };
                ensure!(
                    !chunk.is_empty() && chunk.len() <= out.len() - offset,
                    "stage chunk length"
                );
                out[offset..offset + chunk.len()].copy_from_slice(&chunk);
                offset += chunk.len();
            }
            self.unit(Action::FinishRead {
                stage: stage.clone(),
            })?;
            ensure!(
                blake3::hash(out).to_hex().as_str() == digest,
                "stage transfer digest mismatch"
            );
            Ok(())
        })();
        if let Err(error) = result {
            self.abort_read(stage)
                .context("stage read failed and AbortRead remains pending")?;
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
