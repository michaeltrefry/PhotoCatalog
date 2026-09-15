//! G-owned managed export child and exact ordinary F-stage coordinator.
use crate::{
    application::U64,
    catalog_session::{LeaseId, RootCapability, export_executor, export_native::*, export_stage},
    filesystem_worker::wire::{Failure, FailureKind},
    preview::{ByteBudget, ByteReservation},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// G's adapter is the only implementation allowed to set supervisor=true.
pub(super) trait Stages: Send + Sync {
    fn call(
        &self,
        request: &export_stage::Request,
        cancel: &AtomicBool,
    ) -> Result<export_stage::Reply>;
    fn executor_call(
        &self,
        request: &export_executor::Request,
        cancel: &AtomicBool,
    ) -> Result<export_executor::Reply>;
}

#[derive(Clone)]
struct Completed {
    operation: u64,
    digest: [u8; 32],
    dispatch: DispatchState,
    outcome: std::result::Result<export_stage::Reply, Failure>,
}
struct Pending {
    request: export_stage::Request,
    digest: [u8; 32],
    dispatch: DispatchState,
    active: bool,
}
/// Only an exact F receipt is terminal authority. Unknown custody after a
/// bound Begin failure permits captured-identity cleanup, never path adoption.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BeginCustody {
    Never,
    CanceledBeforeAdmission,
    RejectedWithoutCustody,
    Begun,
    Unknown,
}
/// F's acknowledged drain is distinct from a bound failed attempt. Only F
/// Release can prove disposal after a failure; a refused Release allows a new
/// privileged attempt once the external blocker is repaired.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DrainState {
    None,
    Acknowledged,
    Failed,
    RetryAfterRelease,
}
struct StageState {
    high_water: u64,
    retired: bool,
    pending: Option<Pending>,
    last: Option<Completed>,
    effects: bool,
    begin_custody: BeginCustody,
    armed: bool,
    released: bool,
    sealed: bool,
    supervisor_next: u64,
    supervisor_pending: Option<export_stage::Request>,
    supervisor_last: Option<Completed>,
    drain: DrainState,
}
struct SendState {
    input: Option<ChildStdin>,
    start: bool,
    stop: bool,
    done: bool,
    started: bool,
    error: Option<String>,
    join_failed: bool,
}
struct Slot {
    root: RootCapability,
    executor: LeaseId,
    operation: U64,
    stage: LeaseId,
    binding: export_stage::Binding,
    registration_digest: [u8; 32],
    begin_digest: [u8; 32],
    worker_bytes: u64,
    working_bytes: u64,
    reservation: Mutex<Option<ByteReservation>>,
    stage_state: Mutex<StageState>,
    lifecycle: Mutex<()>,
    supervisor: Mutex<()>,
    terminal: Mutex<Option<export_stage::NativeTerminal>>,
    stage_wake: Condvar,
    child: Mutex<Option<Child>>,
    send: Mutex<SendState>,
    send_wake: Condvar,
    writer: Mutex<Option<JoinHandle<()>>>,
    #[cfg(test)]
    fail_writer_spawn: AtomicBool,
    reaper: Mutex<Option<JoinHandle<()>>>,
    retry: AtomicBool,
    stop: AtomicBool,
    status: Mutex<Status>,
    stages: Arc<dyn Stages>,
}

#[derive(Clone)]
struct CompletedExecutor {
    request: export_executor::Request,
    digest: String,
    reply: export_executor::Reply,
}
struct ActiveExecutor {
    root: RootCapability,
    executor: LeaseId,
    high_water: u64,
    pending: Option<export_executor::Request>,
    last: Option<CompletedExecutor>,
    acquired: bool,
    closing: bool,
}
#[derive(Default)]
struct ExecutorState {
    lifecycle_high_water: u64,
    active: Option<ActiveExecutor>,
    previous_close: Option<CompletedExecutor>,
}

pub(crate) fn owner_layouts() -> [(usize, usize); 8] {
    [
        (std::mem::size_of::<Owner>(), std::mem::align_of::<Owner>()),
        (std::mem::size_of::<Slot>(), std::mem::align_of::<Slot>()),
        (
            std::mem::size_of::<StageState>(),
            std::mem::align_of::<StageState>(),
        ),
        (
            std::mem::size_of::<Pending>(),
            std::mem::align_of::<Pending>(),
        ),
        (
            std::mem::size_of::<Completed>(),
            std::mem::align_of::<Completed>(),
        ),
        (
            std::mem::size_of::<ExecutorState>(),
            std::mem::align_of::<ExecutorState>(),
        ),
        (
            std::mem::size_of::<ActiveExecutor>(),
            std::mem::align_of::<ActiveExecutor>(),
        ),
        (
            std::mem::size_of::<CompletedExecutor>(),
            std::mem::align_of::<CompletedExecutor>(),
        ),
    ]
}

fn bounded(error: impl std::fmt::Display) -> String {
    struct Bounded(String);
    impl std::fmt::Write for Bounded {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let mut end = text.len().min(ERROR_BYTES.saturating_sub(self.0.len()));
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.0.push_str(&text[..end]);
            if end < text.len() {
                Err(std::fmt::Error)
            } else {
                Ok(())
            }
        }
    }
    let mut out = Bounded(String::with_capacity(ERROR_BYTES));
    let _ = std::fmt::write(&mut out, format_args!("{error}"));
    out.0
}
fn bound_failure(value: &Failure, request: &export_stage::Request) -> bool {
    value.validate().is_ok()
        && value.object_receipt.as_ref().is_some_and(|receipt| {
            receipt.operation == request.operation
                && receipt.step == U64(0)
                && request
                    .digest()
                    .is_ok_and(|digest| receipt.request_digest == digest)
        })
}

fn failure(error: anyhow::Error, unknown: bool) -> Failure {
    if let Some(value) = error.downcast_ref::<Failure>() {
        return value.clone();
    }
    let kind = if error.downcast_ref::<crate::preview::ByteLimit>().is_some()
        || error
            .downcast_ref::<crate::catalog_session::store::ResourceLimit>()
            .is_some()
    {
        FailureKind::ResourceLimit
    } else if unknown {
        FailureKind::Unknown
    } else {
        FailureKind::Rejected
    };
    Failure::new(kind, error)
}
fn failure_error(value: &Failure) -> anyhow::Error {
    anyhow::Error::new(value.clone())
}

impl Slot {
    fn status(&self) -> Status {
        let mut value = self
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let stage = self.stage_state.lock().unwrap_or_else(|p| p.into_inner());
        value.stage_high_water = U64(stage.high_water);
        value.pending_stage_operation = stage.pending.as_ref().map(|p| p.request.operation);
        value.pending_dispatch = stage
            .pending
            .as_ref()
            .map_or(DispatchState::None, |p| p.dispatch);
        if value.pending_dispatch == DispatchState::None {
            value.pending_dispatch = stage
                .last
                .as_ref()
                .map_or(DispatchState::None, |p| p.dispatch);
        }
        drop(stage);
        let send = self.send.lock().unwrap_or_else(|p| p.into_inner());
        value.started = send.started;
        if value.error.is_none() {
            value.error = send.error.clone();
        }
        value
    }
    fn set_phase(&self, phase: Phase) {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).phase = phase;
    }
    fn fail(&self, phase: Phase, error: impl std::fmt::Display) {
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        status.phase = phase;
        status.error = Some(bounded(error));
        self.retry.store(false, Ordering::Release);
    }
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        {
            let mut send = self.send.lock().unwrap_or_else(|p| p.into_inner());
            send.stop = true;
        }
        self.send_wake.notify_all();
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(child) = child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = child.kill() {
                        self.fail(
                            Phase::WaitFailed,
                            format_args!("export native kill failed: {error}"),
                        );
                        return;
                    }
                }
                Err(error) => {
                    self.fail(
                        Phase::WaitFailed,
                        format_args!("export native observation failed: {error}"),
                    );
                    return;
                }
            }
        }
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        if !matches!(
            status.phase,
            Phase::Drained | Phase::Released | Phase::WaitFailed | Phase::PipeJoinFailed
        ) {
            status.phase = Phase::StopRequested;
        }
    }
    fn start_writer(self: &Arc<Self>) -> Result<()> {
        #[cfg(test)]
        ensure!(
            !self.fail_writer_spawn.swap(false, Ordering::AcqRel),
            "injected export native writer thread creation failure"
        );
        let owner = self.clone();
        let handle = thread::Builder::new()
            .name("export-native-input".into())
            .spawn(move || {
                let input = {
                    let mut state = owner.send.lock().unwrap_or_else(|p| p.into_inner());
                    while !state.start && !state.stop && !state.done {
                        state = owner
                            .send_wake
                            .wait(state)
                            .unwrap_or_else(|p| p.into_inner());
                    }
                    if state.stop || state.done {
                        state.input.take();
                        state.done = true;
                        return;
                    }
                    state.input.take()
                };
                let result = (|| -> Result<()> {
                    use std::io::Write;
                    let mut input = input.context("export native stdin owner missing")?;
                    input.write_all(b"!")?;
                    input.flush()?;
                    {
                        let mut state = owner.send.lock().unwrap_or_else(|p| p.into_inner());
                        state.started = true;
                        owner.set_phase(Phase::Running);
                        while !state.stop && !state.done {
                            state = owner
                                .send_wake
                                .wait(state)
                                .unwrap_or_else(|p| p.into_inner());
                        }
                    }
                    Ok(())
                })();
                let mut state = owner.send.lock().unwrap_or_else(|p| p.into_inner());
                if let Err(error) = result {
                    state.error =
                        Some(bounded(format_args!("export native stdin failed: {error}")));
                }
                state.done = true;
                owner.send_wake.notify_all();
            })?;
        *self.writer.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
        Ok(())
    }
    fn supervisor_call(&self, action: export_stage::Action) -> Result<export_stage::Reply> {
        let supervisor = self.supervisor.lock().unwrap_or_else(|p| p.into_inner());
        let mut stage = self.stage_state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(!stage.retired, "export native owner retired");
        // Reserve the next id before F can have an effect.
        stage
            .supervisor_next
            .checked_add(1)
            .context("export supervisor sequence exhausted")?;
        let mut request = export_stage::Request {
            root: self.root.clone(),
            executor: self.executor.clone(),
            stage: self.stage.clone(),
            operation: U64(stage.supervisor_next),
            supervisor: true,
            binding: self.binding.clone(),
            action,
        };
        if let Some(pending) = &stage.supervisor_pending {
            request.operation = pending.operation;
            ensure!(
                request.digest()? == pending.digest()?,
                "changed pending export supervisor transition"
            );
            request = pending.clone();
        } else {
            stage.supervisor_pending = Some(request.clone());
        }
        drop(stage);
        self.dispatch_supervisor(&supervisor, &request)
    }
    /// Caller owns the supervisor mutex through request selection, F dispatch
    /// and terminal recording. A delivered failure never remains transport-pending.
    fn dispatch_supervisor(
        &self,
        _supervisor: &std::sync::MutexGuard<'_, ()>,
        request: &export_stage::Request,
    ) -> Result<export_stage::Reply> {
        let digest = request.digest()?;
        let result = self
            .stages
            .call(request, &AtomicBool::new(false))
            .and_then(|reply| {
                reply.validate(request)?;
                Ok(reply)
            })
            .map_err(|error| failure(error, true));
        let terminal = match &result {
            Ok(_) => true,
            Err(value) => bound_failure(value, request),
        };
        let mut stage = self.stage_state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            stage
                .supervisor_pending
                .as_ref()
                .is_some_and(|pending| pending.operation == request.operation
                    && pending.digest().ok() == Some(digest)),
            "export supervisor pending identity changed"
        );
        if terminal {
            match request.action {
                export_stage::Action::Arm { .. } => {
                    // F does all fallible validation/unlock work before it sets
                    // native. A bound failed Arm leaves the prior unarmed state.
                    if result.is_ok() {
                        stage.armed = true;
                    }
                }
                export_stage::Action::NativeDrained { .. } => {
                    stage.drain = if result.is_ok() {
                        DrainState::Acknowledged
                    } else {
                        DrainState::Failed
                    };
                }
                _ => unreachable!("only privileged export transitions"),
            }
            stage.supervisor_last = Some(Completed {
                operation: request.operation.0,
                digest,
                dispatch: DispatchState::Completed,
                outcome: result.clone(),
            });
            stage.supervisor_pending = None;
            stage.supervisor_next = request
                .operation
                .0
                .checked_add(1)
                .context("export supervisor sequence exhausted")?;
        }
        result.map_err(|value| failure_error(&value))
    }
    fn reconcile_supervisor(&self) -> Result<()> {
        // Take the supervisor owner before inspecting pending. If a reaper
        // completed while we waited, its exact recorded outcome/phase already
        // applies; never turn a stale action snapshot into a fresh operation.
        let supervisor = self.supervisor.lock().unwrap_or_else(|p| p.into_inner());
        let pending = self
            .stage_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .supervisor_pending
            .clone();
        if let Some(request) = pending {
            if let Err(error) = self.dispatch_supervisor(&supervisor, &request) {
                if self
                    .stage_state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .supervisor_pending
                    .is_some()
                {
                    return Err(error);
                }
                // A bound failure leaves cleanup custody, not missing transport.
            }
        }
        Ok(())
    }
    fn checked_native_retired(&self) -> Result<()> {
        ensure!(
            self.terminal
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_some(),
            "export native terminal remains unproved"
        );
        ensure!(
            self.child
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_none(),
            "export native child remains owned"
        );
        ensure!(
            self.writer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_none(),
            "export native writer remains owned"
        );
        let send = self.send.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            send.input.is_none() && send.done && !send.join_failed,
            "export native pipe retirement remains unproved"
        );
        Ok(())
    }
    fn try_lifecycle(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        match self.lifecycle.try_lock() {
            Ok(guard) => Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                Err(crate::preview::stage_io::Busy("export native lifecycle busy; retry").into())
            }
        }
    }
    fn drain_stage(&self, terminal: export_stage::NativeTerminal) -> Result<()> {
        self.supervisor_call(export_stage::Action::NativeDrained {
            native: self.operation,
            terminal,
        })?;
        Ok(())
    }
    fn start_reaper(self: &Arc<Self>) -> Result<()> {
        if {
            let stage = self.stage_state.lock().unwrap_or_else(|p| p.into_inner());
            stage.released || stage.drain == DrainState::Acknowledged
        } {
            return Ok(());
        }
        let mut retained = self.reaper.lock().unwrap_or_else(|p| p.into_inner());
        if retained.as_ref().is_some_and(|h| !h.is_finished()) {
            return Ok(());
        }
        if let Some(handle) = retained.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("export native reaper join failed"))?;
        }
        let owner = self.clone();
        *retained = Some(
            thread::Builder::new()
                .name("export-native-reaper".into())
                .spawn(move || {
                    if !owner.retry.swap(false, Ordering::AcqRel) {
                        return;
                    }
                    let retained_terminal = owner
                        .terminal
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .clone();
                    let terminal = if let Some(terminal) = retained_terminal {
                        terminal
                    } else {
                        let exit = loop {
                            let observed = {
                                let mut child =
                                    owner.child.lock().unwrap_or_else(|p| p.into_inner());
                                match child.as_mut() {
                                    Some(process) => match process.try_wait() {
                                        Ok(Some(status)) => {
                                            *owner
                                                .terminal
                                                .lock()
                                                .unwrap_or_else(|p| p.into_inner()) =
                                                Some(if status.success() {
                                                    export_stage::NativeTerminal::Succeeded
                                                } else {
                                                    export_stage::NativeTerminal::Failed {
                                                        code: status.code(),
                                                    }
                                                });
                                            child.take();
                                            Ok(Some(status))
                                        }
                                        Ok(None) => Ok(None),
                                        Err(error) => Err(error),
                                    },
                                    None => {
                                        owner.fail(
                                            Phase::WaitFailed,
                                            "export native terminal is not proved",
                                        );
                                        return;
                                    }
                                }
                            };
                            match observed {
                                Ok(Some(status)) => break Some(status),
                                Ok(None) => thread::sleep(Duration::from_millis(2)),
                                Err(error) => {
                                    owner.fail(
                                        Phase::WaitFailed,
                                        format_args!("export native wait failed: {error}"),
                                    );
                                    return;
                                }
                            }
                        };
                        let terminal = exit.as_ref().map_or(
                            export_stage::NativeTerminal::Failed { code: None },
                            |status| {
                                if status.success() {
                                    export_stage::NativeTerminal::Succeeded
                                } else {
                                    export_stage::NativeTerminal::Failed {
                                        code: status.code(),
                                    }
                                }
                            },
                        );
                        *owner.terminal.lock().unwrap_or_else(|p| p.into_inner()) =
                            Some(terminal.clone());
                        terminal
                    };
                    let (code, succeeded) = match &terminal {
                        export_stage::NativeTerminal::Succeeded => (Some(0), true),
                        export_stage::NativeTerminal::Failed { code } => (*code, false),
                    };
                    {
                        let mut status = owner.status.lock().unwrap_or_else(|p| p.into_inner());
                        status.phase = Phase::ExitObserved;
                        status.exit_code = code;
                        status.success = Some(succeeded);
                    }
                    {
                        let mut send = owner.send.lock().unwrap_or_else(|p| p.into_inner());
                        send.done = true;
                        owner.send_wake.notify_all();
                    }
                    if let Some(writer) = owner
                        .writer
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .take()
                        && writer.join().is_err()
                    {
                        owner
                            .send
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .join_failed = true;
                        owner.fail(Phase::PipeJoinFailed, "export native writer join failed");
                        return;
                    }
                    // A completed BrokenPipe is diagnostic, not a live pipe owner.
                    if owner
                        .send
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .join_failed
                    {
                        owner.fail(
                            Phase::PipeJoinFailed,
                            "export native writer join remains unproved",
                        );
                        return;
                    }
                    if let Err(error) = owner.drain_stage(terminal) {
                        owner.fail(
                            Phase::WaitFailed,
                            format_args!("export stage drain acknowledgement failed: {error}"),
                        );
                        return;
                    }
                    owner.set_phase(Phase::Drained);
                })?,
        );
        Ok(())
    }
}

pub(super) struct Owner {
    admission: Mutex<()>,
    executor_call: Mutex<()>,
    root_closing: AtomicBool,
    executor: Mutex<ExecutorState>,
    executable: PathBuf,
    stages: Arc<dyn Stages>,
    slots: Mutex<VecDeque<Arc<Slot>>>,
    high_water: Mutex<u64>,
    max_slots: usize,
    budget: ByteBudget,
    selected: Mutex<Option<RootCapability>>,
    early_stop: Mutex<Vec<Key>>,
}
impl Owner {
    pub fn new(
        executable: PathBuf,
        stages: Arc<dyn Stages>,
        max_slots: usize,
        budget: &ByteBudget,
    ) -> Result<Self> {
        ensure!(
            executable.is_absolute(),
            "absolute export native executable required"
        );
        ensure!((1..=16).contains(&max_slots), "export native slot bound");
        Ok(Self {
            admission: Mutex::new(()),
            executor_call: Mutex::new(()),
            root_closing: AtomicBool::new(false),
            executor: Mutex::new(ExecutorState::default()),
            executable,
            stages,
            slots: Mutex::new(VecDeque::new()),
            high_water: Mutex::new(0),
            max_slots,
            budget: budget.clone(),
            selected: Mutex::new(None),
            early_stop: Mutex::new(Vec::new()),
        })
    }
    pub fn bind(&self, root: &RootCapability) -> Result<()> {
        let _guard = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        let mut selected = self.selected.lock().unwrap_or_else(|p| p.into_inner());
        if selected.as_ref() == Some(root) {
            return Ok(());
        }
        ensure!(
            selected.is_none()
                && self
                    .slots
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_empty(),
            "previous export native root remains retained"
        );
        *selected = Some(root.clone());
        *self.high_water.lock().unwrap_or_else(|p| p.into_inner()) = 0;
        *self.executor.lock().unwrap_or_else(|p| p.into_inner()) = ExecutorState::default();
        self.root_closing.store(false, Ordering::Release);
        self.early_stop
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        Ok(())
    }
    fn slot(&self, root: &RootCapability, operation: U64, stage: &LeaseId) -> Result<Arc<Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|slot| slot.root == *root && slot.operation == operation && slot.stage == *stage)
            .cloned()
            .context("export native operation is not retained")
    }
    fn compact(&self, key: &Key) -> Result<Arc<Slot>> {
        key.validate()?;
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|slot| {
                slot.operation == key.operation
                    && slot.stage == key.stage
                    && key.matches(&slot.root)
            })
            .cloned()
            .context("export native query owner not retained")
    }
    pub fn stop_key(&self, key: &Key) -> Result<()> {
        let _guard = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok(slot) = self.compact(key) {
            slot.stop();
            return Ok(());
        }
        ensure!(
            self.selected
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .is_some_and(|root| key.matches(root)),
            "export native stop root mismatch"
        );
        let mut early = self.early_stop.lock().unwrap_or_else(|p| p.into_inner());
        if !early.contains(key) {
            ensure!(
                early.len() < self.max_slots,
                "export native early-stop capacity"
            );
            early.push(key.clone());
        }
        Ok(())
    }
    pub fn query(&self, query: &Query) -> Result<Status> {
        let slot = self.compact(&query.key)?;
        match query.action {
            QueryAction::Status => Ok(slot.status()),
            QueryAction::RetryDrain => {
                self.retry_drain(&slot)?;
                Ok(slot.status())
            }
            QueryAction::Retire => self.retire_slot(&slot),
        }
    }
    pub fn executor_call(
        &self,
        request: &export_executor::Request,
        cancel: &AtomicBool,
    ) -> Result<export_executor::Reply> {
        let _call = self.executor_call.lock().unwrap_or_else(|p| p.into_inner());
        request.validate()?;
        let digest = request.digest()?;
        let (replay, close_slots) = {
            let _guard = self.admission.lock().unwrap_or_else(|p| p.into_inner());
            ensure!(
                self.selected
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    == Some(&request.root),
                "export executor catalog is not selected/confirmed"
            );
            let mut state = self.executor.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(closed) = &state.previous_close
                && closed.request.executor == request.executor
                && closed.request.operation == request.operation
            {
                ensure!(
                    closed.digest == digest,
                    "changed export executor Close replay"
                );
                return Ok(closed.reply.clone());
            }
            if matches!(request.action, export_executor::Action::Acquire) && state.active.is_none()
            {
                ensure!(
                    !self.root_closing.load(Ordering::Acquire),
                    "export native root is closing"
                );
                let generation = export_executor::generation(&request.root, &request.executor)?;
                ensure!(
                    generation > state.lifecycle_high_water,
                    "retired export executor generation"
                );
                state.lifecycle_high_water = generation;
                state.active = Some(ActiveExecutor {
                    root: request.root.clone(),
                    executor: request.executor.clone(),
                    high_water: 0,
                    pending: Some(request.clone()),
                    last: None,
                    acquired: false,
                    closing: false,
                });
            }
            let active = state
                .active
                .as_mut()
                .context("export executor is not retained")?;
            ensure!(
                active.root == request.root && active.executor == request.executor,
                "export executor identity mismatch"
            );
            if let Some(last) = &active.last
                && last.request.operation == request.operation
            {
                ensure!(last.digest == digest, "changed export executor replay");
                (Some(last.reply.clone()), Vec::new())
            } else {
                let pending_replay = active
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending == request);
                if let Some(pending) = &active.pending {
                    if pending != request {
                        ensure!(
                            matches!(pending.action, export_executor::Action::Acquire)
                                && matches!(request.action, export_executor::Action::Release)
                                && request.operation.0
                                    == pending
                                        .operation
                                        .0
                                        .checked_add(1)
                                        .context("export executor operation exhausted")?,
                            "another export executor operation is unresolved"
                        );
                        active.pending = Some(request.clone());
                    }
                } else {
                    ensure!(
                        request.operation.0
                            == active
                                .high_water
                                .checked_add(1)
                                .context("export executor operation exhausted")?,
                        "export executor operation gap"
                    );
                    ensure!(
                        !self.root_closing.load(Ordering::Acquire) || request.cleanup(),
                        "export native root is closing"
                    );
                    ensure!(active.acquired, "export executor Acquire is unresolved");
                    active.pending = Some(request.clone());
                }
                if matches!(request.action, export_executor::Action::Release) {
                    active.closing = true;
                } else if !pending_replay {
                    ensure!(!active.closing, "export executor is closing");
                }
                let slots = if matches!(request.action, export_executor::Action::Release) {
                    self.slots
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .iter()
                        .filter(|slot| {
                            slot.root == request.root && slot.executor == request.executor
                        })
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                (None, slots)
            }
        };
        if let Some(reply) = replay {
            return Ok(reply);
        }
        for slot in &close_slots {
            self.cleanup_slot(slot)?;
        }
        for slot in &close_slots {
            self.retire_slot(slot)?;
        }
        let reply = self.stages.executor_call(request, cancel)?;
        reply.validate(request)?;
        let _guard = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.executor.lock().unwrap_or_else(|p| p.into_inner());
        let completed = CompletedExecutor {
            request: request.clone(),
            digest,
            reply: reply.clone(),
        };
        if matches!(request.action, export_executor::Action::Release) {
            ensure!(
                state
                    .active
                    .as_ref()
                    .and_then(|active| active.pending.as_ref())
                    == Some(request),
                "export executor pending request changed"
            );
            state.active.take();
            state.previous_close = Some(completed);
        } else {
            let active = state
                .active
                .as_mut()
                .context("export executor owner was lost")?;
            ensure!(
                active.pending.as_ref() == Some(request),
                "export executor pending request changed"
            );
            active.high_water = request.operation.0;
            active.pending = None;
            active.last = Some(completed);
            if matches!(request.action, export_executor::Action::Acquire) {
                active.acquired = true;
            }
        }
        Ok(reply)
    }
    fn register(&self, request: &Request) -> Result<Status> {
        let Action::Register {
            begin,
            worker_bytes,
            working_bytes,
        } = &request.action
        else {
            unreachable!()
        };
        let _guard = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            self.selected
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                == Some(&request.root),
            "export native catalog is not selected/confirmed"
        );
        ensure!(
            !self.root_closing.load(Ordering::Acquire),
            "export native root is closing"
        );
        let executor = begin.executor.clone();
        ensure!(
            self.executor
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .active
                .as_ref()
                .is_some_and(|active| {
                    active.root == request.root
                        && active.executor == executor
                        && active.acquired
                        && !active.closing
                }),
            "export executor is not open for registration"
        );
        let digest = *blake3::hash(&serde_json::to_vec(request)?).as_bytes();
        let begin_digest = begin.digest()?;
        if let Ok(slot) = self.slot(&request.root, request.operation, &request.stage) {
            ensure!(
                slot.registration_digest == digest,
                "changed export native registration"
            );
            drop(_guard);
            return Ok(slot.status());
        }
        let (capacity, _) = self.budget.snapshot();
        ensure!(
            working_bytes.0 <= capacity,
            "export configured working allowance exceeds shared native pool"
        );
        let export_limits = crate::export_service::ExportServiceLimits {
            worker_bytes: worker_bytes.0,
            working_bytes: working_bytes.0,
            render: match &begin.action {
                export_stage::Action::Begin { limits, .. } => *limits,
                _ => unreachable!(),
            },
        };
        export_limits.validate()?;
        let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            slots.len() < self.max_slots,
            crate::catalog_session::store::ResourceLimit(
                "Export native slots remain owned; wait for checked retirement"
            )
        );
        let mut high = self.high_water.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(request.operation.0 > *high, "stale export native identity");
        slots
            .try_reserve(1)
            .context("export native slot allocation")?;
        let reservation = self
            .budget
            .reserve_exact(worker_bytes.0)
            .map_err(anyhow::Error::new)?;
        let slot = Arc::new(Slot {
            root: request.root.clone(),
            executor,
            operation: request.operation,
            stage: request.stage.clone(),
            binding: request.binding.clone(),
            registration_digest: digest,
            begin_digest,
            worker_bytes: worker_bytes.0,
            working_bytes: working_bytes.0,
            reservation: Mutex::new(Some(reservation)),
            stage_state: Mutex::new(StageState {
                high_water: 0,
                retired: false,
                pending: None,
                last: None,
                effects: false,
                begin_custody: BeginCustody::Never,
                armed: false,
                released: false,
                sealed: false,
                supervisor_next: 1,
                supervisor_pending: None,
                supervisor_last: None,
                drain: DrainState::None,
            }),
            lifecycle: Mutex::new(()),
            supervisor: Mutex::new(()),
            terminal: Mutex::new(None),
            stage_wake: Condvar::new(),
            child: Mutex::new(None),
            send: Mutex::new(SendState {
                input: None,
                start: false,
                stop: false,
                done: false,
                started: false,
                error: None,
                join_failed: false,
            }),
            send_wake: Condvar::new(),
            writer: Mutex::new(None),
            #[cfg(test)]
            fail_writer_spawn: AtomicBool::new(false),
            reaper: Mutex::new(None),
            retry: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            status: Mutex::new(Status {
                epoch: request.root.epoch.clone(),
                session: request.root.session.clone(),
                operation: request.operation,
                stage: request.stage.clone(),
                binding: request.binding.clone(),
                pid: None,
                phase: Phase::Registered,
                started: false,
                exit_code: None,
                success: None,
                stage_high_water: U64(0),
                pending_stage_operation: None,
                pending_dispatch: DispatchState::None,
                error: None,
            }),
            stages: self.stages.clone(),
        });
        let early_stop = {
            let mut early = self.early_stop.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(index) = early.iter().position(|key| {
                key.operation == request.operation
                    && key.stage == request.stage
                    && key.matches(&request.root)
            }) {
                early.remove(index);
                true
            } else {
                false
            }
        };
        if early_stop {
            slot.stop.store(true, Ordering::Release);
        }
        *high = request.operation.0;
        slots.push_back(slot.clone());
        drop(slots);
        drop(high);
        drop(_guard);
        Ok(slot.status())
    }
    pub fn call(&self, request: &Request) -> Result<Status> {
        request.validate()?;
        if matches!(request.action, Action::Register { .. }) {
            return self.register(request);
        }
        let slot = self.slot(&request.root, request.operation, &request.stage)?;
        ensure!(
            slot.binding == request.binding,
            "export native action binding mismatch"
        );
        match request.action {
            Action::Spawn => self.spawn(&slot)?,
            Action::Start => {
                let _lifecycle = slot.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
                ensure!(
                    !self.root_closing.load(Ordering::Acquire)
                        && !slot
                            .stage_state
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .retired,
                    "export native admission closed"
                );
                let mut send = slot.send.lock().unwrap_or_else(|p| p.into_inner());
                ensure!(
                    slot.status
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .pid
                        .is_some(),
                    "export native child was not launched"
                );
                ensure!(!send.stop, "export native already stopping");
                send.start = true;
                slot.send_wake.notify_all();
            }
            Action::Stop => slot.stop(),
            Action::RetryDrain => {
                self.retry_drain(&slot)?;
            }
            Action::Retire => return self.retire_slot(&slot),
            Action::Register { .. } => unreachable!(),
        }
        Ok(slot.status())
    }
    fn retry_drain(&self, slot: &Arc<Slot>) -> Result<()> {
        let _lifecycle = slot.try_lifecycle()?;
        let stage = slot.stage_state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            stage.armed && !stage.retired,
            "export native has no armed drain owner"
        );
        drop(stage);
        slot.retry.store(true, Ordering::Release);
        slot.start_reaper()
    }
    fn spawn(&self, slot: &Arc<Slot>) -> Result<()> {
        let _lifecycle = slot.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            !self.root_closing.load(Ordering::Acquire),
            "export native root is closing"
        );
        ensure!(
            !slot
                .stage_state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retired,
            "export native owner retired"
        );
        let status = slot.status();
        if status.pid.is_some() || matches!(status.phase, Phase::WaitFailed | Phase::Drained) {
            return Ok(());
        }
        ensure!(
            status.phase == Phase::Ready,
            "export stage is not ready for native spawn"
        );
        slot.supervisor_call(export_stage::Action::Arm {
            native: slot.operation,
        })?;
        slot.stage_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .armed = true;
        if slot.stop.load(Ordering::Acquire) {
            *slot.terminal.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(export_stage::NativeTerminal::Failed { code: None });
            slot.fail(Phase::WaitFailed, "export native canceled before OS spawn");
            return Ok(());
        }
        let path = match slot
            .stage_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last
            .as_ref()
            .and_then(|last| last.outcome.as_ref().ok())
            .map(|reply| &reply.value)
        {
            Some(export_stage::Value::Ready { path }) => path.to_path()?,
            _ => anyhow::bail!("export stage ready path is not retained"),
        };
        let spawned = Command::new(&self.executable)
            .arg("--photo-export-worker")
            .current_dir(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("OMP_NUM_THREADS", "1")
            .env("RAYON_NUM_THREADS", "1")
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                *slot.terminal.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(export_stage::NativeTerminal::Failed { code: None });
                slot.fail(
                    Phase::WaitFailed,
                    format_args!("export native launch failed before Child return: {error}"),
                );
                return Ok(());
            }
        };
        let pid = child.id();
        let input = child.stdin.take();
        *slot.child.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
        slot.send.lock().unwrap_or_else(|p| p.into_inner()).input = input;
        {
            let mut status = slot.status.lock().unwrap_or_else(|p| p.into_inner());
            status.pid = Some(pid);
            status.phase = Phase::Spawned;
        }
        if let Err(error) = slot.start_writer() {
            // No thread took ownership on Builder::spawn failure. Close the
            // retained pipe here, before any checked drain can be sent to F.
            {
                let mut send = slot.send.lock().unwrap_or_else(|p| p.into_inner());
                send.input.take();
                send.done = true;
            }
            slot.fail(Phase::PipeJoinFailed, error);
            slot.stop();
        }
        if slot.stop.load(Ordering::Acquire) {
            slot.stop();
        }
        slot.retry.store(true, Ordering::Release);
        if let Err(error) = slot.start_reaper() {
            slot.fail(Phase::WaitFailed, error);
        }
        Ok(())
    }
    pub fn stage_call(
        &self,
        request: &export_stage::Request,
        cancel: &AtomicBool,
    ) -> Result<export_stage::Reply> {
        request.validate()?;
        ensure!(
            !request.supervisor && !request.privileged(),
            "ordinary export stage caller cannot assert supervisor custody"
        );
        let slot = self
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|slot| {
                slot.root == request.root
                    && slot.executor == request.executor
                    && slot.stage == request.stage
                    && slot.binding == request.binding
            })
            .cloned()
            .context("export stage request has no registered G owner")?;
        let digest = request.digest()?;
        loop {
            let mut stage = slot.stage_state.lock().unwrap_or_else(|p| p.into_inner());
            ensure!(!stage.retired, "export native owner retired");
            if let Some(last) = &stage.last
                && last.operation == request.operation.0
            {
                ensure!(last.digest == digest, "altered export stage replay");
                return last
                    .outcome
                    .as_ref()
                    .cloned()
                    .map_err(|f| failure_error(&f));
            }
            if let Some(pending) = stage.pending.as_mut() {
                ensure!(
                    pending.request.operation == request.operation && pending.digest == digest,
                    "another export stage operation is pending"
                );
                if pending.active {
                    stage = slot
                        .stage_wake
                        .wait(stage)
                        .unwrap_or_else(|p| p.into_inner());
                    drop(stage);
                    continue;
                }
                pending.active = true;
                pending.dispatch = DispatchState::Sent;
            } else {
                // Serialise only admission with Close.  The F call itself must
                // run after this guard is released, so reserved control is
                // never held behind an ordinary stage relay operation.
                let admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
                ensure!(
                    request.operation.0
                        == stage
                            .high_water
                            .checked_add(1)
                            .context("stage sequence exhausted")?,
                    "export stage operation gap"
                );
                if matches!(request.action, export_stage::Action::Begin { .. }) {
                    ensure!(
                        request.digest()? == slot.begin_digest,
                        "export Begin changed after registration"
                    );
                } else {
                    ensure!(
                        stage.effects
                            || (request.cleanup() && stage.begin_custody == BeginCustody::Unknown),
                        "export stage Begin has not completed"
                    );
                }
                let executor_closing = !self
                    .executor
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .active
                    .as_ref()
                    .is_some_and(|active| {
                        active.root == request.root
                            && active.executor == request.executor
                            && active.acquired
                            && !active.closing
                    });
                let admission_closed =
                    self.root_closing.load(Ordering::Acquire) || executor_closing;
                if (cancel.load(Ordering::Acquire) || admission_closed) && !request.cleanup() {
                    let outcome = Err(Failure::new(
                        FailureKind::Canceled,
                        if admission_closed {
                            "export stage admission closed before G dispatch"
                        } else {
                            "export stage operation canceled before G dispatch"
                        },
                    ));
                    stage.high_water = request.operation.0;
                    stage.last = Some(Completed {
                        operation: request.operation.0,
                        digest,
                        dispatch: DispatchState::NeverDispatched,
                        outcome: outcome.clone(),
                    });
                    return Err(failure_error(outcome.as_ref().err().unwrap()));
                }
                stage.pending = Some(Pending {
                    request: request.clone(),
                    digest,
                    dispatch: DispatchState::Sent,
                    active: true,
                });
                drop(admission);
            }
            drop(stage);
            // G is the cancellation admission boundary. Once Sent, F must
            // reconcile the exact operation even if C subsequently cancels.
            let result = slot
                .stages
                .call(request, &AtomicBool::new(false))
                .and_then(|reply| {
                    reply.validate(request)?;
                    Ok(reply)
                })
                .map_err(|error| failure(error, true));
            let terminal = match &result {
                Ok(_) => true,
                Err(value) => bound_failure(value, request),
            };
            let mut stage = slot.stage_state.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(request.action, export_stage::Action::Begin { .. }) {
                stage.begin_custody = match &result {
                    Ok(_) => BeginCustody::Begun,
                    Err(value) if terminal && value.kind == FailureKind::Canceled => {
                        BeginCustody::CanceledBeforeAdmission
                    }
                    Err(value) if terminal && value.kind == FailureKind::Rejected => {
                        BeginCustody::RejectedWithoutCustody
                    }
                    Err(_) => BeginCustody::Unknown,
                };
            }
            let pending = stage
                .pending
                .as_mut()
                .context("export stage pending owner lost")?;
            pending.active = false;
            if !terminal {
                pending.dispatch = DispatchState::Unknown;
                slot.stage_wake.notify_all();
                return Err(failure_error(result.as_ref().err().unwrap()));
            }
            if let Ok(reply) = &result {
                reply.validate(request)?;
                match &request.action {
                    export_stage::Action::Begin { .. } => {
                        stage.effects = true;
                        slot.set_phase(Phase::Begun);
                    }
                    export_stage::Action::Ready { .. } => slot.set_phase(Phase::Ready),
                    export_stage::Action::ResultAndSeal => stage.sealed = true,
                    export_stage::Action::Abort | export_stage::Action::Release => {
                        stage.released = true;
                        slot.set_phase(Phase::Released);
                    }
                    _ => {}
                }
            }
            if result.is_err()
                && matches!(request.action, export_stage::Action::Release)
                && stage.armed
                && matches!(stage.drain, DrainState::Failed | DrainState::Acknowledged)
            {
                stage.drain = DrainState::RetryAfterRelease;
            }
            let completed = Completed {
                operation: request.operation.0,
                digest,
                dispatch: DispatchState::Completed,
                outcome: result.clone(),
            };
            stage.high_water = request.operation.0;
            stage.pending = None;
            stage.last = Some(completed);
            slot.stage_wake.notify_all();
            return result.map_err(|failure| failure_error(&failure));
        }
    }
    fn cleanup_slot(&self, slot: &Arc<Slot>) -> Result<()> {
        let _lifecycle = slot.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
        slot.reconcile_supervisor()?;
        loop {
            let pending = {
                let mut stage = slot.stage_state.lock().unwrap_or_else(|p| p.into_inner());
                match stage.pending.as_ref() {
                    Some(pending) if pending.active => {
                        stage = slot
                            .stage_wake
                            .wait(stage)
                            .unwrap_or_else(|p| p.into_inner());
                        drop(stage);
                        continue;
                    }
                    Some(pending) => Some(pending.request.clone()),
                    None => None,
                }
            };
            if let Some(request) = pending {
                if let Err(error) = self.stage_call(&request, &AtomicBool::new(false)) {
                    if slot
                        .stage_state
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .pending
                        .is_some()
                    {
                        return Err(error);
                    }
                    // A bound failed operation is terminal; continue its cleanup.
                }
                continue;
            }
            break;
        }
        let (begin_custody, armed, released, high_water) = {
            let stage = slot.stage_state.lock().unwrap_or_else(|p| p.into_inner());
            (
                stage.begin_custody,
                stage.armed,
                stage.released,
                stage.high_water,
            )
        };
        if released
            || matches!(
                begin_custody,
                BeginCustody::Never
                    | BeginCustody::CanceledBeforeAdmission
                    | BeginCustody::RejectedWithoutCustody
            )
        {
            return Ok(());
        }
        ensure!(
            matches!(begin_custody, BeginCustody::Begun | BeginCustody::Unknown),
            "export Begin custody is unknown; exact owner and reservation retained"
        );
        let next = high_water
            .checked_add(1)
            .context("stage sequence exhausted")?;
        if !armed {
            let request = export_stage::Request {
                root: slot.root.clone(),
                executor: slot.executor.clone(),
                stage: slot.stage.clone(),
                operation: U64(next),
                supervisor: false,
                binding: slot.binding.clone(),
                action: export_stage::Action::Abort,
            };
            self.stage_call(&request, &AtomicBool::new(false))?;
            return Ok(());
        }
        // The lifecycle guard excludes Spawn. A reconciled Arm with no
        // returned PID proves that no OS child was ever published for it.
        if slot
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pid
            .is_none()
        {
            let mut terminal = slot.terminal.lock().unwrap_or_else(|p| p.into_inner());
            if terminal.is_none() {
                *terminal = Some(export_stage::NativeTerminal::Failed { code: None });
            }
        }
        slot.stop();
        let drain = slot
            .stage_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain;
        if drain != DrainState::Failed {
            slot.retry.store(true, Ordering::Release);
            slot.start_reaper()?;
        }
        // Join the actual attempt before interpreting phase, which may still
        // contain a previous failed attempt until the new thread is scheduled.
        if let Some(handle) = slot.reaper.lock().unwrap_or_else(|p| p.into_inner()).take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("export native reaper join failed"))?;
        }
        slot.checked_native_retired()?;
        let drain = slot
            .stage_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain;
        ensure!(
            matches!(drain, DrainState::Acknowledged | DrainState::Failed),
            "export native drain outcome unresolved; exact owner retained"
        );
        // Failed is not F drain authority. Release itself checks whether F
        // acquired/recorded the lease; refusal retains custody and schedules
        // a fresh checked drain on the next cleanup attempt.
        let next = slot
            .stage_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .high_water
            .checked_add(1)
            .context("stage sequence exhausted")?;
        let request = export_stage::Request {
            root: slot.root.clone(),
            executor: slot.executor.clone(),
            stage: slot.stage.clone(),
            operation: U64(next),
            supervisor: false,
            binding: slot.binding.clone(),
            action: export_stage::Action::Release,
        };
        self.stage_call(&request, &AtomicBool::new(false))?;
        Ok(())
    }
    fn retire_slot(&self, slot: &Arc<Slot>) -> Result<Status> {
        let _lifecycle = slot.try_lifecycle()?;
        let mut stage = slot.stage_state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            !stage.retired && stage.pending.is_none() && stage.supervisor_pending.is_none(),
            "export stage transition remains owned"
        );
        ensure!(
            stage.released
                || matches!(
                    stage.begin_custody,
                    BeginCustody::Never
                        | BeginCustody::CanceledBeforeAdmission
                        | BeginCustody::RejectedWithoutCustody
                ),
            "export stage is not retired"
        );
        ensure!(
            slot.child
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_none(),
            "export native child remains owned"
        );
        let send = slot.send.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            send.input.is_none() && !send.join_failed,
            "export native pipe remains owned"
        );
        drop(send);
        ensure!(
            slot.writer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_none(),
            "export native writer remains owned"
        );
        let mut reaper = slot.reaper.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            reaper.as_ref().is_none_or(|h| h.is_finished()),
            "export native reaper remains active"
        );
        if let Some(handle) = reaper.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("export native reaper join failed"))?;
        }
        // Fence every ordinary caller that already cloned this slot, atomically
        // with the negative-custody/pending check above.
        stage.retired = true;
        drop(stage);
        let status = slot.status();
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|candidate| !Arc::ptr_eq(candidate, slot));
        ensure!(
            slot.reservation
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
                .is_some(),
            "export native reservation already retired"
        );
        Ok(status)
    }
    pub fn retire_root(&self, root: &RootCapability) -> Result<()> {
        // Close and a new ordinary admission share this short critical
        // section.  Cleanup is intentionally outside it because it can wait
        // for an already-dispatched F request to resolve.
        let slots = {
            let _guard = self.admission.lock().unwrap_or_else(|p| p.into_inner());
            ensure!(
                self.selected
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    == Some(root),
                "export native root retirement mismatch"
            );
            self.root_closing.store(true, Ordering::Release);
            self.slots
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .filter(|slot| slot.root == *root)
                .cloned()
                .collect::<Vec<_>>()
        };
        loop {
            let request = {
                let state = self.executor.lock().unwrap_or_else(|p| p.into_inner());
                state.active.as_ref().map(|active| match &active.pending {
                    Some(pending) if matches!(pending.action, export_executor::Action::Acquire) => {
                        export_executor::Request {
                            root: active.root.clone(),
                            executor: active.executor.clone(),
                            operation: U64(pending
                                .operation
                                .0
                                .checked_add(1)
                                .expect("validated export executor sequence")),
                            action: export_executor::Action::Release,
                        }
                    }
                    Some(pending) => pending.clone(),
                    None => export_executor::Request {
                        root: active.root.clone(),
                        executor: active.executor.clone(),
                        operation: U64(active
                            .high_water
                            .checked_add(1)
                            .expect("validated export executor sequence")),
                        action: export_executor::Action::Release,
                    },
                })
            };
            let Some(request) = request else {
                break;
            };
            let released = matches!(request.action, export_executor::Action::Release);
            self.executor_call(&request, &AtomicBool::new(false))?;
            if released {
                break;
            }
        }
        ensure!(
            self.slots
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .all(|slot| slot.root != *root),
            "export native slots survived executor Close"
        );
        drop(slots);
        Ok(())
    }
    pub fn forget_released_root(&self, root: &RootCapability) -> Result<()> {
        ensure!(
            self.root_closing.load(Ordering::Acquire),
            "export native root not closed"
        );
        ensure!(
            self.slots
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty(),
            "export native slots remain"
        );
        ensure!(
            self.executor
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .active
                .is_none(),
            "export executor remains active"
        );
        let mut selected = self.selected.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            selected.as_ref() == Some(root),
            "released export native root mismatch"
        );
        *selected = None;
        Ok(())
    }
    pub fn finish_after_catalog(&self) -> Result<()> {
        self.root_closing.store(true, Ordering::Release);
        let root = self
            .selected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(root) = root {
            self.retire_root(&root)?;
        }
        Ok(())
    }
    pub fn stop_all(&self) {
        for slot in self.slots.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            slot.stop();
        }
    }
    pub fn closing(&self) {
        self.root_closing.store(true, Ordering::Release);
        self.stop_all();
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let slots = self.slots.get_mut().unwrap_or_else(|p| p.into_inner());
        for slot in slots.drain(..) {
            slot.stop();
            let _ = slot.start_reaper();
            // Drop has no authority to retire F stage ownership or native bytes.
            std::mem::forget(slot);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
