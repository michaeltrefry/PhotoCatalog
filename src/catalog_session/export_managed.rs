//! Session-lifetime managed export identities. The private root is introduced
//! here and never returned to the C export adapter.
use super::{
    AuthorityMode, CatalogSessionAuthority, LeaseId, export_executor, export_native, export_stage,
};
use crate::{application::U64, catalog_exports::ExportWork, export_service::ExportServiceLimits};
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, Mutex, atomic::AtomicBool};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExportBackendKind {
    Local,
    Managed,
}

#[derive(Default)]
struct State {
    generation_high_water: u64,
    native_high_water: u64,
    service_claimed: bool,
    active: Option<ExecutorState>,
    pending_close: Option<export_executor::Request>,
}

struct ExecutorState {
    executor: LeaseId,
    high_water: u64,
    pending: Option<export_executor::Request>,
    acquired: bool,
    closing: bool,
}

#[derive(Default)]
pub(super) struct Registry(Mutex<State>);

pub(crate) struct ManagedExportExecutor {
    authority: Arc<CatalogSessionAuthority>,
    executor: LeaseId,
    closed: bool,
}

pub(crate) struct ManagedExportAttempt {
    authority: Arc<CatalogSessionAuthority>,
    executor: LeaseId,
    native: U64,
    stage: LeaseId,
    binding: export_stage::Binding,
    begin: export_stage::Request,
    limits: ExportServiceLimits,
    stage_high_water: u64,
    pending_stage: Option<export_stage::Request>,
    terminal_stage_failure: Option<String>,
}

fn managed(
    authority: &CatalogSessionAuthority,
) -> Result<(Arc<dyn super::CatalogFilesystem>, super::RootCapability)> {
    match &authority.mode {
        AuthorityMode::Managed {
            filesystem, root, ..
        } => Ok((filesystem.clone(), root.clone())),
        AuthorityMode::Legacy(_) => anyhow::bail!("legacy catalog has no managed export owner"),
    }
}

impl CatalogSessionAuthority {
    pub(crate) fn export_backend(&self) -> ExportBackendKind {
        match &self.mode {
            AuthorityMode::Legacy(_) => ExportBackendKind::Local,
            AuthorityMode::Managed { .. } => ExportBackendKind::Managed,
        }
    }

    pub(crate) fn open_managed_export(
        self: &Arc<Self>,
        cancel: &AtomicBool,
    ) -> Result<Option<ManagedExportExecutor>> {
        if self.export_backend() == ExportBackendKind::Local {
            return Ok(None);
        }
        let (filesystem, root) = managed(self)?;
        let (close, abandoned_closing_executor) = {
            let mut state = self
                .managed_export
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            ensure!(
                !state.service_claimed,
                "managed export service is already open"
            );
            // Reserve the single C owner before any relay. The lock never spans
            // a filesystem call, but this claim serializes pending-Close replay
            // and Acquire construction for concurrent callers.
            state.service_claimed = true;
            (
                state.pending_close.clone(),
                state
                    .active
                    .as_ref()
                    .filter(|active| active.closing)
                    .map(|active| active.executor.clone()),
            )
        };
        let opened = (|| -> Result<ManagedExportExecutor> {
            if let Some(request) = close {
                let reply = filesystem.export_executor_call(&request, &AtomicBool::new(false))?;
                reply.validate(&request)?;
                ensure!(
                    matches!(reply.value, export_executor::Value::Released),
                    "managed export Close reply"
                );
                let mut state = self
                    .managed_export
                    .0
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                ensure!(
                    state.pending_close.as_ref() == Some(&request),
                    "managed export Close identity changed"
                );
                state.pending_close = None;
                state.active = None;
            } else if let Some(executor) = abandoned_closing_executor {
                // Drop may have admitted Close intent before an older ordinary
                // executor operation could be reconciled. Finish that exact
                // operation and Release while preserving this caller's open
                // claim, then allocate a successor generation below.
                let mut abandoned = ManagedExportExecutor {
                    authority: self.clone(),
                    executor,
                    closed: false,
                };
                abandoned.close_with_claim(true)?;
            }
            let (request, executor) = {
                let mut state = self
                    .managed_export
                    .0
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                ensure!(
                    state.service_claimed,
                    "managed export service claim was lost"
                );
                if let Some(active) = state.active.as_ref() {
                    ensure!(
                        !active.acquired
                            && !active.closing
                            && active.high_water == 0
                            && active.pending.as_ref().is_some_and(|request| {
                                matches!(request.action, export_executor::Action::Acquire)
                            }),
                        "previous managed export executor remains owned"
                    );
                } else {
                    let generation = state
                        .generation_high_water
                        .checked_add(1)
                        .context("managed export generation exhausted")?;
                    let executor = export_executor::executor_id(&root, generation)?;
                    let request = export_executor::Request {
                        root: root.clone(),
                        executor: executor.clone(),
                        operation: U64(1),
                        action: export_executor::Action::Acquire,
                    };
                    request.validate()?;
                    state.generation_high_water = generation;
                    state.active = Some(ExecutorState {
                        executor,
                        high_water: 0,
                        pending: Some(request),
                        acquired: false,
                        closing: false,
                    });
                }
                let active = state.active.as_ref().expect("managed executor reserved");
                (
                    active
                        .pending
                        .as_ref()
                        .context("managed Acquire request was not retained")?
                        .clone(),
                    active.executor.clone(),
                )
            };
            let reply = filesystem.export_executor_call(&request, cancel)?;
            reply.validate(&request)?;
            ensure!(
                matches!(reply.value, export_executor::Value::Acquired),
                "managed export Acquire reply"
            );
            {
                let mut state = self
                    .managed_export
                    .0
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let active = state
                    .active
                    .as_mut()
                    .context("managed export Acquire owner was lost")?;
                ensure!(
                    active.executor == executor && active.pending.as_ref() == Some(&request),
                    "managed export Acquire identity changed"
                );
                active.high_water = request.operation.0;
                active.pending = None;
                active.acquired = true;
            }
            Ok(ManagedExportExecutor {
                authority: self.clone(),
                executor,
                closed: false,
            })
        })();
        if opened.is_err() {
            // Every unsuccessful open, including malformed or mismatched ACKs,
            // releases only the C caller claim. The exact pending request stays
            // retained for the next caller to replay.
            self.managed_export
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .service_claimed = false;
        }
        opened.map(Some)
    }
}

impl ManagedExportExecutor {
    fn call(
        &mut self,
        action: export_executor::Action,
        cancel: &AtomicBool,
    ) -> Result<export_executor::Reply> {
        ensure!(!self.closed, "managed export executor is closed");
        let (filesystem, root) = managed(&self.authority)?;
        let request = {
            let mut state = self
                .authority
                .managed_export
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let active = state
                .active
                .as_mut()
                .context("managed export executor is not active")?;
            ensure!(
                active.executor == self.executor
                    && active.acquired
                    && (!active.closing || active.pending.is_some()),
                "managed export executor identity changed"
            );
            if let Some(pending) = &active.pending {
                let proposed = export_executor::Request {
                    root,
                    executor: self.executor.clone(),
                    operation: pending.operation,
                    action,
                };
                ensure!(
                    pending == &proposed,
                    "changed pending managed export operation"
                );
                pending.clone()
            } else {
                ensure!(!active.closing, "managed export executor is closing");
                let operation = active
                    .high_water
                    .checked_add(1)
                    .context("managed export executor operation exhausted")?;
                let request = export_executor::Request {
                    root,
                    executor: self.executor.clone(),
                    operation: U64(operation),
                    action,
                };
                request.validate()?;
                active.pending = Some(request.clone());
                request
            }
        };
        let reply = filesystem.export_executor_call(&request, cancel)?;
        reply.validate(&request)?;
        let mut state = self
            .authority
            .managed_export
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let active = state
            .active
            .as_mut()
            .context("managed export executor was lost")?;
        ensure!(
            active.pending.as_ref() == Some(&request),
            "managed export pending operation changed"
        );
        active.high_water = request.operation.0;
        active.pending = None;
        Ok(reply)
    }

    pub(crate) fn recover(
        &mut self,
        max_directories: usize,
        cancel: &AtomicBool,
    ) -> Result<export_executor::Reply> {
        self.call(
            export_executor::Action::Recover {
                max_directories: U64(u64::try_from(max_directories)?),
            },
            cancel,
        )
    }

    pub(crate) fn discard(&mut self, token: LeaseId) -> Result<export_executor::Reply> {
        self.call(
            export_executor::Action::Discard { token },
            &AtomicBool::new(false),
        )
    }

    pub(crate) fn prepare_attempt(
        &mut self,
        work: ExportWork,
        mut limits: ExportServiceLimits,
    ) -> Result<ManagedExportAttempt> {
        ensure!(!self.closed, "managed export executor is closed");
        limits.validate()?;
        ensure!(
            work.plan.max_payload_bytes <= limits.render.max_encoded_extent,
            "export plan exceeds configured staging allowance"
        );
        limits.render.max_encoded_extent = work.plan.max_payload_bytes;
        limits.render.decode.max_encoded_bytes = limits
            .render
            .decode
            .max_encoded_bytes
            .min(work.plan.max_original_bytes);
        let (_, root) = managed(&self.authority)?;
        let native = {
            let mut state = self
                .authority
                .managed_export
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let active = state
                .active
                .as_ref()
                .context("managed export executor is not active")?;
            ensure!(
                active.executor == self.executor
                    && active.acquired
                    && !active.closing
                    && active.pending.is_none(),
                "managed export executor is not ready"
            );
            let native = state
                .native_high_water
                .checked_add(1)
                .context("managed export native identity exhausted")?;
            state.native_high_water = native;
            U64(native)
        };
        let stage = LeaseId::new();
        let binding = export_stage::Binding::from_work(&work);
        let begin = export_stage::Request {
            root,
            executor: self.executor.clone(),
            stage: stage.clone(),
            operation: U64(1),
            supervisor: false,
            binding: binding.clone(),
            action: export_stage::Action::Begin {
                work: Box::new(work),
                limits: limits.render,
            },
        };
        begin.validate()?;
        Ok(ManagedExportAttempt {
            authority: self.authority.clone(),
            executor: self.executor.clone(),
            native,
            stage,
            binding,
            begin,
            limits,
            stage_high_water: 0,
            pending_stage: None,
            terminal_stage_failure: None,
        })
    }

    fn reconcile_pending(&mut self) -> Result<()> {
        let request = self
            .authority
            .managed_export
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active
            .as_ref()
            .and_then(|active| active.pending.clone());
        if let Some(request) = request {
            self.call(request.action, &AtomicBool::new(false))?;
        }
        Ok(())
    }

    fn close_with_claim(&mut self, keep_claim: bool) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        {
            let mut state = self
                .authority
                .managed_export
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if state.pending_close.is_none() {
                let active = state
                    .active
                    .as_mut()
                    .context("managed export executor is not active")?;
                ensure!(
                    active.executor == self.executor && active.acquired,
                    "managed export Close identity changed"
                );
                // Check the successor before publishing Close intent or replaying
                // an older executor effect; overflow must not mutate custody.
                active
                    .pending
                    .as_ref()
                    .map_or(active.high_water, |request| request.operation.0)
                    .checked_add(1)
                    .context("managed export Close operation exhausted")?;
                // Persist Close intent before reconciling an older unknown
                // Recover/Discard. A failed Drop can therefore be resumed by
                // the next opener without authorizing a successor Acquire.
                active.closing = true;
            }
        }
        self.reconcile_pending()?;
        let (filesystem, root) = managed(&self.authority)?;
        let request = {
            let mut state = self
                .authority
                .managed_export
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(pending) = &state.pending_close {
                ensure!(
                    pending.executor == self.executor,
                    "managed export Close changed"
                );
                pending.clone()
            } else {
                let operation = {
                    let active = state
                        .active
                        .as_ref()
                        .context("managed export executor is not active")?;
                    ensure!(
                        active.executor == self.executor && active.acquired,
                        "managed export Close identity changed"
                    );
                    active
                        .high_water
                        .checked_add(1)
                        .context("managed export Close operation exhausted")?
                };
                let request = export_executor::Request {
                    root,
                    executor: self.executor.clone(),
                    operation: U64(operation),
                    action: export_executor::Action::Release,
                };
                request.validate()?;
                state.active.as_mut().unwrap().closing = true;
                state.pending_close = Some(request.clone());
                request
            }
        };
        let reply = filesystem.export_executor_call(&request, &AtomicBool::new(false))?;
        reply.validate(&request)?;
        ensure!(
            matches!(reply.value, export_executor::Value::Released),
            "managed export Close reply"
        );
        let mut state = self
            .authority
            .managed_export
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        ensure!(
            state.pending_close.as_ref() == Some(&request),
            "managed export Close identity changed"
        );
        state.pending_close = None;
        state.active = None;
        state.service_claimed = keep_claim;
        self.closed = true;
        Ok(())
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        self.close_with_claim(false)
    }

    pub(crate) fn release_claim_after_failed_drop(&mut self) {
        self.authority
            .managed_export
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .service_claimed = false;
    }
}

impl ManagedExportAttempt {
    pub(crate) fn work(&self) -> &ExportWork {
        match &self.begin.action {
            export_stage::Action::Begin { work, .. } => work,
            _ => unreachable!(),
        }
    }

    pub(crate) fn register(&self, cancel: &AtomicBool) -> Result<export_native::Status> {
        self.native(
            export_native::Action::Register {
                begin: Box::new(self.begin.clone()),
                worker_bytes: U64(self.limits.worker_bytes),
                working_bytes: U64(self.limits.working_bytes),
            },
            cancel,
        )
    }

    pub(crate) fn registration_rejected(&self, error: &anyhow::Error) -> bool {
        let Some(failure) = error.downcast_ref::<crate::filesystem_worker::wire::Failure>() else {
            return false;
        };
        if failure.kind == crate::filesystem_worker::wire::FailureKind::Unknown {
            return false;
        }
        let Ok((_, root)) = managed(&self.authority) else {
            return false;
        };
        let request = export_native::Request {
            root,
            operation: self.native,
            stage: self.stage.clone(),
            binding: self.binding.clone(),
            action: export_native::Action::Register {
                begin: Box::new(self.begin.clone()),
                worker_bytes: U64(self.limits.worker_bytes),
                working_bytes: U64(self.limits.working_bytes),
            },
        };
        failure.validate().is_ok()
            && failure.object_receipt.as_ref().is_some_and(|receipt| {
                receipt.operation == self.native
                    && receipt.step == U64(0)
                    && request
                        .digest()
                        .is_ok_and(|digest| digest == receipt.request_digest)
            })
    }

    pub(crate) fn native(
        &self,
        action: export_native::Action,
        cancel: &AtomicBool,
    ) -> Result<export_native::Status> {
        let (filesystem, root) = managed(&self.authority)?;
        let request = export_native::Request {
            root,
            operation: self.native,
            stage: self.stage.clone(),
            binding: self.binding.clone(),
            action,
        };
        request.validate()?;
        let owner = filesystem
            .export_native()
            .context("managed export native owner is unavailable")?;
        let status = owner.call(&request, cancel)?;
        status.validate(&request)?;
        Ok(status)
    }

    pub(crate) fn status(&self) -> Result<export_native::Status> {
        let (filesystem, root) = managed(&self.authority)?;
        let request = export_native::Request {
            root: root.clone(),
            operation: self.native,
            stage: self.stage.clone(),
            binding: self.binding.clone(),
            action: export_native::Action::Stop,
        };
        request.validate()?;
        let status = filesystem
            .export_native()
            .context("managed export native owner is unavailable")?
            .status(&export_native::Key::new(&root, self.native, &self.stage))?;
        status.validate(&request)?;
        Ok(status)
    }

    pub(crate) fn stage(
        &mut self,
        action: export_stage::Action,
        cancel: &AtomicBool,
    ) -> Result<export_stage::Reply> {
        let (filesystem, root) = managed(&self.authority)?;
        let request = if let Some(pending) = &self.pending_stage {
            let mut proposed = pending.clone();
            proposed.action = action;
            ensure!(
                proposed.digest()? == pending.digest()?,
                "changed pending managed export stage operation"
            );
            pending.clone()
        } else {
            let operation = self
                .stage_high_water
                .checked_add(1)
                .context("managed export stage operation exhausted")?;
            let request = export_stage::Request {
                root,
                executor: self.executor.clone(),
                stage: self.stage.clone(),
                operation: U64(operation),
                supervisor: false,
                binding: self.binding.clone(),
                action,
            };
            request.validate()?;
            self.pending_stage = Some(request.clone());
            request
        };
        let result = filesystem.export_stage_call(&request, cancel);
        match result {
            Ok(reply) => {
                reply.validate(&request)?;
                self.stage_high_water = request.operation.0;
                self.pending_stage = None;
                Ok(reply)
            }
            Err(error) => {
                let delivered_failure = error
                    .downcast_ref::<crate::filesystem_worker::wire::Failure>()
                    .is_some_and(|failure| {
                        failure.kind != crate::filesystem_worker::wire::FailureKind::Unknown
                    });
                if delivered_failure
                    && let Ok(status) = self.status()
                    && status.stage_high_water == request.operation
                    && status.pending_stage_operation != Some(request.operation)
                {
                    // The typed non-Unknown failure supplies the failed value;
                    // matching G state proves that exact operation is terminal.
                    // High-water alone never proves success or fabricates a
                    // Completed seal receipt.
                    self.stage_high_water = request.operation.0;
                    self.pending_stage = None;
                    self.terminal_stage_failure = Some(error.to_string());
                }
                Err(error)
            }
        }
    }

    pub(crate) fn begin(&mut self, cancel: &AtomicBool) -> Result<export_stage::Reply> {
        let action = self.begin.action.clone();
        self.stage(action, cancel)
    }

    pub(crate) fn stage_failure_without_custody(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<crate::filesystem_worker::wire::Failure>()
            .is_some_and(|failure| {
                matches!(
                    failure.kind,
                    crate::filesystem_worker::wire::FailureKind::Canceled
                        | crate::filesystem_worker::wire::FailureKind::Rejected
                )
            })
    }

    pub(crate) fn pending_stage(&self) -> bool {
        self.pending_stage.is_some()
    }

    pub(crate) fn take_terminal_stage_failure(&mut self) -> Option<String> {
        self.terminal_stage_failure.take()
    }
}

pub(crate) fn registry_layouts() -> [(usize, usize); 5] {
    [
        (
            std::mem::size_of::<Registry>(),
            std::mem::align_of::<Registry>(),
        ),
        (std::mem::size_of::<State>(), std::mem::align_of::<State>()),
        (
            std::mem::size_of::<ExecutorState>(),
            std::mem::align_of::<ExecutorState>(),
        ),
        (
            std::mem::size_of::<ManagedExportExecutor>(),
            std::mem::align_of::<ManagedExportExecutor>(),
        ),
        (
            std::mem::size_of::<ManagedExportAttempt>(),
            std::mem::align_of::<ManagedExportAttempt>(),
        ),
    ]
}

#[cfg(test)]
mod tests;
