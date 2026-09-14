//! C retains opaque stage/native identities. Only F opens output files.
use super::*;
use crate::{
    application::U64,
    catalog_session::{LeaseId, native as n, preview_stage as f},
};
use std::sync::{Arc, Mutex};
pub(crate) struct Stage {
    pub calls: Arc<crate::preview::stage_io::Calls>,
    pub id: LeaseId,
    released: Mutex<bool>,
}
impl Stage {
    pub fn release(&self) -> Result<()> {
        let mut released = self.released.lock().unwrap_or_else(|p| p.into_inner());
        if !*released {
            self.calls.abort_read(&self.id)?;
            self.calls.unit(f::Action::Release {
                stage: self.id.clone(),
            })?;
            *released = true;
        }
        Ok(())
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        // F retains the authoritative stage on failure, fencing final root
        // release. G's checked abandoned-stage drain is the final fallback.
        let _ = self.release();
    }
}
pub(crate) struct Job {
    pub stage: Arc<Stage>,
    pub operation: U64,
    spawn: n::Request,
    pub status: Option<n::Status>,
    pub cost: u64,
    started: bool,
    spawn_attempted: bool,
    retired: bool,
    canceling: bool,
    admission_failure: Option<String>,
}
impl Job {
    pub fn admit(
        calls: Arc<crate::preview::stage_io::Calls>,
        work: n::Work,
        limits: &super::super::ServiceLimits,
        cost: u64,
    ) -> Result<Self> {
        ensure!(
            calls.filesystem().native().is_some(),
            "native custody unavailable"
        );
        let rgb = match &work {
            n::Work::Render(r) => r.keys.iter().try_fold(0u64, |sum, k| {
                sum.checked_add(n::rgb_bytes(k.edge, k.edge)?)
                    .context("RGB stage overflow")
            })?,
            _ => 2 * 8192 * 8192 * 3,
        };
        let value = calls.call(
            f::Action::Admit {
                limits: f::Limits {
                    workers: u8::try_from(limits.workers)?,
                    encoded: U64(match &work {
                        n::Work::Render(r) => r.encoded_limit,
                        n::Work::DecodeEncoded { encoded_bytes, .. } => encoded_bytes.0,
                    }),
                    rgb: U64(rgb),
                    prepared: U64(limits.prepared_cache_bytes.min(MAX_PROXY_BYTES)),
                },
            },
            &AtomicBool::new(false),
        )?;
        let f::Value::Admitted {
            stage,
            ready,
            error,
        } = value
        else {
            bail!("wrong stage admission reply")
        };
        let stage = Arc::new(Stage {
            calls: calls.clone(),
            id: stage,
            released: Mutex::new(false),
        });
        let admission_failure = (!ready)
            .then(|| error.unwrap_or_else(|| "unknown filesystem admission failure".into()));
        let operation = U64(0);
        let spawn = n::Request {
            root: calls.root.clone(),
            operation,
            action: n::Action::Spawn {
                stage: stage.id.clone(),
                work,
                workers: u8::try_from(limits.workers)?,
                working_bytes: U64(cost),
            },
        };
        Ok(Self {
            stage,
            operation,
            spawn,
            status: None,
            cost,
            started: false,
            spawn_attempted: false,
            retired: false,
            canceling: false,
            admission_failure,
        })
    }
    pub fn call(&mut self, action: n::Action) -> Result<n::Status> {
        let native = self
            .stage
            .calls
            .filesystem()
            .native()
            .context("native custody missing")?;
        let result = crate::preview::stage_io::retry_busy(|| {
            native.call(
                &n::Request {
                    root: self.stage.calls.root.clone(),
                    operation: self.operation,
                    action: action.clone(),
                },
                &AtomicBool::new(false),
            )
        })?;
        result.validate(&self.stage.calls.root, self.operation)?;
        ensure!(
            result.stage == self.stage.id,
            "native status stage mismatch"
        );
        self.status = Some(result.clone());
        Ok(result)
    }
    pub fn start(&mut self) -> Result<()> {
        if let Some(error) = &self.admission_failure {
            anyhow::bail!("stage admission failed: {error}");
        }
        if self.started {
            return Ok(());
        }
        self.spawn_attempted = true;
        let result = self.stage.calls.native_spawn(&mut self.spawn);
        self.operation = self.spawn.operation;
        let status = match result {
            Ok(status) => status,
            Err(error) => {
                if error
                    .downcast_ref::<crate::filesystem_worker::wire::Failure>()
                    .is_some_and(|f| f.kind != crate::filesystem_worker::wire::FailureKind::Unknown)
                {
                    self.spawn_attempted = false;
                }
                return Err(error);
            }
        };
        status.validate(&self.stage.calls.root, self.operation)?;
        ensure!(status.stage == self.stage.id, "native spawn stage mismatch");
        self.status = Some(status);
        if self.canceling
            || self.stage.calls.native_canceled()
            || self.status.as_ref().is_some_and(|s| s.pid.is_none())
        {
            self.canceling = true;
            self.call(n::Action::Drain)?;
            return Ok(());
        }
        self.call(n::Action::Start)?;
        self.started = true;
        Ok(())
    }
    pub fn refresh(&mut self) -> Result<n::Status> {
        let status = crate::preview::stage_io::retry_busy(|| {
            self.stage
                .calls
                .filesystem()
                .native()
                .context("native custody missing")?
                .status(&self.stage.calls.root, self.operation)
        })?;
        status.validate(&self.stage.calls.root, self.operation)?;
        ensure!(
            status.stage == self.stage.id,
            "native status stage mismatch"
        );
        self.stage.calls.confirm_native(self.operation);
        self.status = Some(status.clone());
        Ok(status)
    }
    pub fn signal_stop(&mut self) {
        self.canceling = true;
        if self.spawn_attempted && !self.retired {
            let _ = self.call(n::Action::Stop);
        }
    }
    pub fn drain(&mut self) -> Result<bool> {
        if !self.spawn_attempted {
            self.retired = true;
            return Ok(true);
        }
        self.signal_stop();
        // Prefer the retained G identity. Only an unresolved initial dispatch
        // retries the exact Spawn; it never invents a replacement operation.
        if self.status.is_none() && self.refresh().is_err() {
            self.start()?;
            self.signal_stop();
        }
        let status = self.call(n::Action::Drain)?;
        Ok(status.phase == n::Phase::Drained)
    }
    pub fn successor(&self, cost: u64, dimensions: (u32, u32)) -> Result<Self> {
        ensure!(self.retired, "header predecessor not retired");
        let operation = U64(0);
        let mut spawn = self.spawn.clone();
        spawn.operation = operation;
        let n::Action::Spawn {
            work,
            working_bytes,
            ..
        } = &mut spawn.action
        else {
            unreachable!()
        };
        let n::Work::DecodeEncoded {
            expected_dimensions,
            ..
        } = work
        else {
            bail!("render cannot rearm header stage")
        };
        *expected_dimensions = Some(dimensions);
        *working_bytes = U64(cost);
        Ok(Self {
            stage: self.stage.clone(),
            operation,
            spawn,
            status: None,
            cost,
            started: false,
            spawn_attempted: false,
            retired: false,
            canceling: false,
            admission_failure: None,
        })
    }
    pub fn is_retired(&self) -> bool {
        self.retired
    }
    pub fn retire(&mut self) -> Result<()> {
        if !self.retired {
            self.call(n::Action::Retire)?;
            self.retired = true;
            self.stage.calls.retired_native(self.operation);
        }
        Ok(())
    }
    pub fn header(&self) -> Result<Option<n::Header>> {
        let Some(bytes) = self
            .stage
            .calls
            .metadata(&self.stage.id, f::Artifact::Header)?
        else {
            return Ok(None);
        };
        let header: n::Header = serde_json::from_slice(&bytes)?;
        header.validate()?;
        ensure!(
            header.operation == self.operation && header.stage == self.stage.id,
            "managed header identity"
        );
        let n::Work::DecodeEncoded {
            codec,
            encoded_bytes,
            encoded_digest,
            expected_dimensions,
        } = (match &self.spawn.action {
            n::Action::Spawn { work, .. } => work,
            _ => unreachable!(),
        })
        else {
            bail!("header on render work")
        };
        ensure!(
            header.codec == *codec
                && header.input_bytes == *encoded_bytes
                && header.input_digest == *encoded_digest
                && expected_dimensions.is_none_or(|d| d == (header.width, header.height)),
            "managed header input binding"
        );
        Ok(Some(header))
    }
    pub fn read(
        &self,
        artifact: f::Artifact,
        bytes: u64,
        digest: &str,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.try_reserve_exact(usize::try_from(bytes)?)?;
        out.resize(usize::try_from(bytes)?, 0);
        self.stage
            .calls
            .read_into(&self.stage.id, artifact, bytes, digest, &mut out, cancel)?;
        Ok(out)
    }
    pub fn failure(&self, status: &n::Status) -> Result<WorkerFailure> {
        let bytes = self
            .stage
            .calls
            .metadata(&self.stage.id, f::Artifact::Error)?;
        if let Some(bytes) = bytes {
            return Ok(serde_json::from_slice(&bytes)?);
        }
        Ok(WorkerFailure {
            decode_status: None,
            message: status
                .error
                .clone()
                .unwrap_or_else(|| "managed native exited unsuccessfully".into()),
        })
    }
    pub fn poll_render(
        &mut self,
        request: &RenderWork,
        canceled: &AtomicBool,
    ) -> Result<Option<RenderedPreviewBatch>> {
        if canceled.load(Ordering::Acquire) && !self.spawn_attempted {
            self.drain()?;
            return Err(WorkerFailure {
                decode_status: None,
                message: "preview request canceled before native spawn".into(),
            }
            .into());
        }
        self.start()?;
        if canceled.load(Ordering::Acquire) {
            self.signal_stop();
        }
        if self.canceling {
            self.call(n::Action::Drain)?;
        }
        let status = self.refresh()?;
        if matches!(
            status.phase,
            n::Phase::WaitFailed | n::Phase::PipeJoinFailed
        ) {
            self.call(n::Action::Drain)?;
            return Ok(None);
        }
        if status.phase != n::Phase::Drained {
            if !status.encode_sent
                && !self.canceling
                && self
                    .stage
                    .calls
                    .metadata(&self.stage.id, f::Artifact::Decoded)?
                    .is_some_and(|b| b == b"decoded")
            {
                let rgb = request.keys.iter().try_fold(0u64, |sum, k| {
                    sum.checked_add(n::rgb_bytes(k.edge, k.edge)?)
                        .context("RGB grant overflow")
                })?;
                self.call(n::Action::Encode {
                    header: None,
                    working_bytes: U64(self.cost),
                    rgb_bytes: U64(rgb),
                })?;
            }
            return Ok(None);
        }
        if self.canceling || !status.success.unwrap_or(false) {
            let error = self.failure(&status)?;
            self.retire()?;
            return Err(error.into());
        }
        let bytes = self
            .stage
            .calls
            .metadata(&self.stage.id, f::Artifact::Receipt)?
            .context("managed render receipt absent")?;
        let receipt: RenderReceipt = serde_json::from_slice(&bytes)?;
        let binding = receipt
            .native
            .context("managed receipt lacks native identity")?;
        ensure!(
            binding.operation == self.operation && binding.stage == self.stage.id,
            "managed render receipt binding"
        );
        ensure!(
            receipt.objects.len() == request.keys.len(),
            "incomplete managed tier set"
        );
        validate_edit_input(request, &receipt.edit_input)?;
        let mut objects = Vec::new();
        let mut encoded_total = 0u64;
        for (index, object) in receipt.objects.into_iter().enumerate() {
            ensure!(
                object.key == request.keys[index]
                    && object.width <= object.key.edge
                    && object.height <= object.key.edge,
                "managed render key/dimensions"
            );
            let required = n::rgb_bytes(object.width, object.height)?;
            encoded_total = encoded_total
                .checked_add(object.bytes)
                .context("managed encoded sum overflow")?;
            ensure!(
                encoded_total <= request.encoded_limit,
                "managed encoded allowance"
            );
            let rgb = object.rgb.context("managed verified RGB receipt absent")?;
            ensure!(rgb.bytes.0 == required, "managed RGB length mismatch");
            let encoded = self.read(
                f::Artifact::Encoded(index as u8),
                object.bytes,
                &object.checksum,
                canceled,
            )?;
            let pixels = self.read(
                f::Artifact::Rgb(index as u8),
                required,
                &rgb.digest,
                canceled,
            )?;
            objects.push(ProducedPreview {
                key: object.key,
                pixels: PreparedRgb::new(object.width, object.height, pixels)?,
                encoded,
            });
        }
        self.retire()?;
        Ok(Some(RenderedPreviewBatch {
            edit_input: receipt.edit_input,
            peak_resident_bytes: receipt.peak_resident_bytes,
            peak_method: receipt.peak_method,
            metadata: receipt.metadata,
            provenance: receipt.provenance,
            objects,
            prepared: receipt.prepared.map(|(receipt, source)| ProducedPrepared {
                path: PathBuf::new(),
                managed: Some(self.stage.clone()),
                receipt,
                source,
            }),
        }))
    }
}
pub(crate) fn cleanup_unadmitted(calls: &crate::preview::stage_io::Calls) -> Result<()> {
    calls.cleanup_unadmitted()
}
fn validate_edit_input(request: &RenderWork, input: &Option<EditInputProvenance>) -> Result<()> {
    match (&request.edit, input) {
        (None, None) | (Some(_), Some(EditInputProvenance::OriginalDecoded)) => Ok(()),
        (
            Some(edit),
            Some(EditInputProvenance::PreparedProxy {
                receipt,
                source_instance_digest,
            }),
        ) => {
            let expected = edit.prepared.as_ref().context("unrequested prepared hit")?;
            ensure!(
                edit.interactive
                    && *receipt == expected.receipt
                    && *source_instance_digest == expected.source.digest()?,
                "prepared hit differs from admitted request"
            );
            Ok(())
        }
        _ => bail!("worker edit input evidence differs from request"),
    }
}

#[cfg(test)]
mod tests;
