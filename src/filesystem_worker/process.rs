//! Single execution owner and independent bounded controls for the F helper.
use super::wire::*;
use crate::application::U64;
use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

/// Implemented locally by the filesystem engine. No remote callback is accepted.
pub trait Handler: Send + 'static {
    fn execute(&mut self, operation: Operation, context: &OperationContext) -> Outcome;
    /// Refuse while dependent SQL/native ownership has not been explicitly retired.
    fn shutdown(&mut self) -> std::result::Result<(), Failure>;
}

// An unexpected unwind must not release engine pins while sibling SQL owners
// may still use their admission. Only successful checked shutdown retires this.
struct RetainedHandler<H>(Option<H>);
impl<H> Drop for RetainedHandler<H> {
    fn drop(&mut self) {
        if let Some(handler) = self.0.take() {
            std::mem::forget(handler);
        }
    }
}

struct Pending {
    sequence: u64,
    operation: Operation,
    cancel: Arc<AtomicBool>,
}
struct Retained {
    info: ResultInfo,
    bytes: Arc<Vec<u8>>,
    sent: bool,
}
struct State {
    phase: Phase,
    active: Option<(u64, Arc<AtomicBool>)>,
    queued: Option<Pending>,
    retained: Option<Retained>,
    admission: Option<AdmissionSnapshot>,
    admission_reply: Option<(u64, Control)>,
    store: Option<super::store::Snapshot>,
    objects: Option<super::preview_io::Snapshot>,
    store_reply: Option<(u64, Control)>,
    error: Option<Failure>,
    last_sequence: u64,
    revision: u64,
    stop_attempt: u64,
    shutdown_wire: u64,
    shutdown_reported: u64,
    stopped_attempt: u64,
    input_closed: bool,
    early_cancel: Option<u64>,
    execution_done: bool,
    canceled_before_execution: Option<U64>,
}
impl State {
    fn status(&self) -> Status {
        Status {
            phase: self.phase,
            shutdown_attempt: U64(
                if matches!(self.phase, Phase::DrainFailed | Phase::Stopped) {
                    self.shutdown_reported
                } else {
                    self.shutdown_wire
                },
            ),
            active: self.active.as_ref().map(|(id, _)| U64(*id)),
            queued: self.queued.as_ref().map(|p| U64(p.sequence)),
            retained: self.retained.as_ref().map(|r| r.info.clone()),
            canceled_before_execution: self.canceled_before_execution,
            error: self.error.clone(),
        }
    }
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    fn stop(&mut self) {
        self.stop_attempt = self.stop_attempt.saturating_add(1);
        self.phase = Phase::Stopping;
        if let Some((_, cancel)) = &self.active {
            cancel.store(true, Ordering::Release);
        }
        // This item has not entered Handler. A stopped owner never dispatches it.
        if self
            .queued
            .as_ref()
            .is_some_and(|p| !p.operation.is_cleanup())
            && let Some(pending) = self.queued.take()
        {
            self.canceled_before_execution = Some(U64(pending.sequence));
        }
        self.changed();
    }
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    epoch: crate::catalog_session::LeaseId,
}
impl Shared {
    fn new(epoch: crate::catalog_session::LeaseId) -> Self {
        Self {
            state: Mutex::new(State {
                phase: Phase::Starting,
                active: None,
                queued: None,
                retained: None,
                admission: None,
                admission_reply: None,
                store: None,
                objects: None,
                store_reply: None,
                error: None,
                last_sequence: 0,
                revision: 1,
                stop_attempt: 0,
                shutdown_wire: 0,
                shutdown_reported: 0,
                stopped_attempt: 0,
                input_closed: false,
                early_cancel: None,
                execution_done: false,
                canceled_before_execution: None,
            }),
            wake: Condvar::new(),
            epoch,
        }
    }
    fn fail(&self, message: impl std::fmt::Display) {
        let mut state = self.state.lock().unwrap();
        state.error = Some(Failure::new(FailureKind::Unknown, message));
        state.stop();
        self.wake.notify_all();
    }
}

/// Admission facts are cached independently of the engine's potentially blocked
/// filesystem operation. Engine retains the original request and actual objects.
pub struct OperationContext {
    cancel: Arc<AtomicBool>,
    shared: Arc<Shared>,
    admission: Option<(U64, crate::catalog_session::LeaseId)>,
    store: Option<crate::catalog_session::store::StatusQuery>,
}
impl OperationContext {
    pub fn cancellation(&self) -> &AtomicBool {
        &self.cancel
    }
    pub(super) fn publish_objects(&self, snapshot: super::preview_io::Snapshot) -> Result<()> {
        let query = self
            .store
            .as_ref()
            .context("operation cannot publish cache status")?;
        ensure!(
            query.kind == crate::catalog_session::store::StatusKind::Objects,
            "cache snapshot family mismatch"
        );
        snapshot.status(query)?.validate(query)?;
        self.shared.state.lock().unwrap().objects = Some(snapshot);
        self.shared.wake.notify_all();
        Ok(())
    }
    pub(super) fn publish_store(&self, snapshot: super::store::Snapshot) -> Result<()> {
        let query = self
            .store
            .as_ref()
            .context("operation cannot publish preview status")?;
        ensure!(
            query.epoch == self.shared.epoch,
            "preview snapshot helper epoch mismatch"
        );
        snapshot.status(query)?.validate(query)?;
        self.shared.state.lock().unwrap().store = Some(snapshot);
        self.shared.wake.notify_all();
        Ok(())
    }
    pub(super) fn clear_store(&self, root: &crate::catalog_session::RootCapability) -> Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        if let Some(snapshot) = &state.store {
            let query = crate::catalog_session::store::StatusQuery::from(
                &crate::catalog_session::store::Query {
                    kind: crate::catalog_session::store::StatusKind::Locks,
                    root: root.clone(),
                    operation: U64(1),
                    selected: None,
                },
            );
            snapshot.status(&query)?;
        }
        state.store = None;
        state.objects = None;
        Ok(())
    }
    pub fn publish_admission(&self, snapshot: AdmissionSnapshot) -> Result<()> {
        snapshot.validate()?;
        ensure!(
            self.admission
                .as_ref()
                .is_some_and(|(operation, session)| *operation == snapshot.operation
                    && *session == snapshot.session),
            "operation cannot publish another admission"
        );
        if let Some(bootstrap) = &snapshot.bootstrap {
            ensure!(
                bootstrap.epoch == self.shared.epoch,
                "admission has another helper epoch"
            );
        }
        let mut state = self.shared.state.lock().unwrap();
        if let Some(previous) = &state.admission {
            ensure!(
                previous.operation == snapshot.operation && previous.session == snapshot.session,
                "outstanding filesystem admission cannot be replaced"
            );
            ensure!(
                !previous.directory_created || snapshot.directory_created,
                "creation fact regressed"
            );
            ensure!(
                !previous.catalog_created || snapshot.catalog_created,
                "creation fact regressed"
            );
            ensure!(
                !previous.manifest_created || snapshot.manifest_created,
                "creation fact regressed"
            );
        }
        state.admission = Some(snapshot);
        state.changed();
        self.shared.wake.notify_all();
        Ok(())
    }
}

fn admission_identity(operation: &Operation) -> Option<(U64, crate::catalog_session::LeaseId)> {
    match operation {
        Operation::PrepareCatalog(v) => Some((v.operation, v.session.clone())),
        Operation::ConfirmSqlAdmission(v) => Some((v.operation, v.root.session.clone())),
        Operation::AbandonPrepare { operation, session } => Some((*operation, session.clone())),
        _ => None,
    }
}
fn preparing(request: &crate::catalog_session::PrepareCatalog) -> AdmissionSnapshot {
    AdmissionSnapshot {
        operation: request.operation,
        session: request.session.clone(),
        directory_created: false,
        catalog_created: false,
        manifest_created: false,
        bootstrap: None,
        state: AdmissionState::Preparing,
        failure: None,
    }
}

/// No SQLite or filesystem handler is constructed before bounded startup and
/// build/nonce validation. stderr is exclusively the framed control stream.
pub fn serve<R, W, C, H, F>(mut input: R, output: W, control: C, factory: F) -> Result<()>
where
    R: Read,
    W: Write + Send + 'static,
    C: Write + Send + 'static,
    H: Handler,
    F: FnOnce(Startup) -> Result<H> + Send + 'static,
{
    let first =
        Frame::read(&mut input)?.ok_or_else(|| anyhow::anyhow!("missing filesystem startup"))?;
    ensure!(
        first.kind == Kind::Startup && first.sequence == 0,
        "expected filesystem startup"
    );
    let nonce = first.epoch;
    let mut assembly = Assembly::start(&first)?;
    let mut complete = assembly.push(first)?;
    while !complete {
        complete = assembly
            .push(Frame::read(&mut input)?.ok_or_else(|| anyhow::anyhow!("incomplete startup"))?)?;
    }
    let startup: Startup = decode(&assembly.finish()?.bytes, CONFIG_BYTES)?;
    startup.validate()?;
    ensure!(
        nonce == startup.nonce(),
        "filesystem startup epoch mismatch"
    );
    let shared = Arc::new(Shared::new(startup.epoch.clone()));
    // Every started thread is retained and joined; no detached mutation owner.
    let execution_state = shared.clone();
    let execution = thread::Builder::new()
        .name("filesystem-execution".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                execution_loop(startup, factory, &execution_state)
            }));
            let failed = !matches!(result, Ok(Ok(())));
            let mut state = execution_state
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            state.execution_done = true;
            if failed {
                state.phase = Phase::Unknown;
                state.error = Some(Failure::new(
                    FailureKind::Unknown,
                    match result {
                        Ok(Err(error)) => error.to_string(),
                        _ => "filesystem handler panicked; outcome unknown".into(),
                    },
                ));
            }
            state.changed();
            execution_state.wake.notify_all();
            drop(state);
            if failed {
                // A poisoned F does not exit and drop admission pins on its own.
                // G terminates/reaps it only after all dependent owners drain.
                loop {
                    thread::park();
                }
            }
        })?;
    let out_state = shared.clone();
    let ordinary = thread::Builder::new()
        .name("filesystem-results".into())
        .spawn(move || {
            if let Err(error) = output_loop(output, nonce, &out_state) {
                out_state.fail(error);
            }
        });
    let ordinary = match ordinary {
        Ok(owner) => owner,
        Err(error) => {
            shared.fail(&error);
            execution.join().ok();
            return Err(error.into());
        }
    };
    let control_state = shared.clone();
    let controls = thread::Builder::new()
        .name("filesystem-controls".into())
        .spawn(move || {
            if let Err(error) = control_loop(control, nonce, &control_state) {
                control_state.fail(error);
            }
        });
    let controls = match controls {
        Ok(owner) => owner,
        Err(error) => {
            shared.fail(&error);
            execution.join().ok();
            ordinary.join().ok();
            return Err(error.into());
        }
    };
    let read_result = input_loop(&mut input, nonce, &shared);
    {
        let mut state = shared.state.lock().unwrap();
        state.input_closed = true;
        if state.phase != Phase::Stopped && !state.execution_done {
            state.stop();
        }
        shared.wake.notify_all();
    }
    // Input EOF is a cancellation request, never permission to release a live
    // admission. Checked Handler::shutdown must still affirm retirement.
    let executed = execution.join();
    let sent = ordinary.join();
    let controlled = controls.join();
    ensure!(
        executed.is_ok() && sent.is_ok() && controlled.is_ok(),
        "filesystem owner thread panicked"
    );
    ensure!(
        shared.state.lock().unwrap().phase == Phase::Stopped,
        "filesystem execution ended without checked retirement"
    );
    read_result
}

fn input_loop(input: &mut impl Read, nonce: [u8; 16], shared: &Arc<Shared>) -> Result<()> {
    let mut assembly: Option<Assembly> = None;
    while let Some(frame) = Frame::read(input)? {
        ensure!(
            frame.epoch == nonce,
            "filesystem frame belongs to another process"
        );
        match frame.kind {
            Kind::Execute => {
                if assembly.is_none() {
                    assembly = Some(Assembly::start(&frame)?);
                }
                if assembly.as_mut().unwrap().push(frame)? {
                    let message = assembly.take().unwrap().finish()?;
                    let operation: Operation = super::wire::decode_operation(&message.bytes)?;
                    operation.validate()?;
                    let mut state = shared.state.lock().unwrap();
                    // A duplicate is only a status request; it must not replace
                    // an original in-flight outcome with a synthetic rejection.
                    if message.sequence <= state.last_sequence {
                        state.changed();
                        shared.wake.notify_all();
                        continue;
                    }
                    state.last_sequence = message.sequence;
                    // Capacity/lifetime admission precedes any Handler call.
                    if !(state.phase == Phase::Ready
                        || (matches!(state.phase, Phase::Stopping | Phase::DrainFailed)
                            && operation.is_cleanup()))
                        || state.queued.is_some()
                    {
                        ensure!(
                            state.admission_reply.is_none(),
                            "filesystem reserved reply capacity exceeded"
                        );
                        state.admission_reply = Some((
                            message.sequence,
                            Control::Rejected(Failure::new(
                                FailureKind::Rejected,
                                "filesystem operation admission busy",
                            )),
                        ));
                        state.changed();
                        shared.wake.notify_all();
                        continue;
                    }
                    if let Operation::PrepareCatalog(_) = &operation
                        && let Some(previous) = &state.admission
                        && previous.state != AdmissionState::Abandoned
                    {
                        ensure!(
                            state.admission_reply.is_none(),
                            "filesystem reserved reply capacity exceeded"
                        );
                        state.admission_reply = Some((
                            message.sequence,
                            Control::Rejected(Failure::new(
                                FailureKind::Rejected,
                                "previous filesystem admission is retained",
                            )),
                        ));
                        state.changed();
                        shared.wake.notify_all();
                        continue;
                    }
                    let canceled = state.early_cancel == Some(message.sequence);
                    if state.early_cancel.is_some_and(|id| id <= message.sequence) {
                        state.early_cancel = None;
                    }
                    state.queued = Some(Pending {
                        sequence: message.sequence,
                        operation,
                        cancel: Arc::new(AtomicBool::new(canceled)),
                    });
                    state.changed();
                    shared.wake.notify_all();
                }
            }
            Kind::Cancel
            | Kind::Ack
            | Kind::Status
            | Kind::AdmissionStatus
            | Kind::StoreStatus
            | Kind::Stop => {
                ensure!(
                    frame.offset == 0 && frame.total == frame.payload.len(),
                    "fragmented filesystem control"
                );
                let mut state = shared.state.lock().unwrap();
                match frame.kind {
                    Kind::Cancel => {
                        ensure!(frame.payload.is_empty(), "cancel payload is not empty");
                        if let Some((id, cancel)) = &state.active
                            && *id == frame.sequence
                        {
                            cancel.store(true, Ordering::Release);
                        }
                        if let Some(pending) = &state.queued
                            && pending.sequence == frame.sequence
                        {
                            pending.cancel.store(true, Ordering::Release);
                        }
                        if frame.sequence > state.last_sequence {
                            ensure!(
                                state.early_cancel.is_none_or(|id| id == frame.sequence),
                                "filesystem early cancel capacity exceeded"
                            );
                            state.early_cancel = Some(frame.sequence);
                        }
                    }
                    Kind::Ack => {
                        let ack: Ack = decode(&frame.payload, CHUNK_BYTES)?;
                        ensure!(ack.sequence.0 == frame.sequence, "ack sequence mismatch");
                        if let Some(result) = &state.retained {
                            ensure!(
                                result.info.sequence == ack.sequence
                                    && result.info.blake3 == ack.blake3,
                                "ack result mismatch"
                            );
                            state.retained = None;
                        }
                    }
                    Kind::Status => {
                        ensure!(frame.payload.is_empty(), "status payload is not empty");
                        // Exact result retrieval retransmits retained bytes only.
                        // It never re-enters the filesystem Handler.
                        if frame.sequence != 0
                            && let Some(result) = state
                                .retained
                                .as_mut()
                                .filter(|r| r.info.sequence.0 == frame.sequence)
                        {
                            result.sent = false;
                        }
                    }
                    Kind::StoreStatus => {
                        ensure!(
                            state.store_reply.is_none(),
                            "preview status reply already pending"
                        );
                        let query: crate::catalog_session::store::StatusQuery =
                            decode(&frame.payload, CHUNK_BYTES)?;
                        query.validate()?;
                        ensure!(
                            query.epoch == shared.epoch,
                            "preview status helper epoch mismatch"
                        );
                        let value = match query.kind {
                            crate::catalog_session::store::StatusKind::Locks => state
                                .store
                                .as_ref()
                                .context("preview ownership has no published status")
                                .and_then(|s| s.status(&query)),
                            crate::catalog_session::store::StatusKind::Objects => state
                                .objects
                                .as_ref()
                                .context("cache IO has no published status")
                                .and_then(|s| s.status(&query)),
                        }
                        .map_err(|e| Failure::new(FailureKind::Rejected, e));
                        state.store_reply = Some((frame.sequence, Control::Store(value)));
                    }
                    Kind::AdmissionStatus => {
                        ensure!(
                            state.admission_reply.is_none(),
                            "admission status reply already pending"
                        );
                        let query: AdmissionQuery = decode(&frame.payload, CHUNK_BYTES)?;
                        let snapshot = state
                            .admission
                            .as_ref()
                            .filter(|s| {
                                s.operation == query.operation && s.session == query.session
                            })
                            .cloned()
                            .or_else(|| {
                                state.queued.as_ref().and_then(|p| match &p.operation {
                                    Operation::PrepareCatalog(v)
                                        if v.operation == query.operation
                                            && v.session == query.session =>
                                    {
                                        Some(preparing(v))
                                    }
                                    _ => None,
                                })
                            });
                        state.admission_reply =
                            Some((frame.sequence, Control::Admission(snapshot)));
                    }
                    Kind::Stop => {
                        ensure!(frame.payload.is_empty(), "stop payload is not empty");
                        ensure!(frame.sequence > 0, "invalid filesystem shutdown attempt");
                        if frame.sequence > state.shutdown_wire {
                            state.shutdown_wire = frame.sequence;
                            state.stop();
                        }
                    }
                    _ => unreachable!(),
                }
                state.changed();
                shared.wake.notify_all();
            }
            _ => anyhow::bail!("unexpected filesystem input frame"),
        }
    }
    ensure!(
        assembly.is_none(),
        "filesystem input ended inside a message"
    );
    Ok(())
}

fn execution_loop<H: Handler, F: FnOnce(Startup) -> Result<H>>(
    startup: Startup,
    factory: F,
    shared: &Arc<Shared>,
) -> Result<()> {
    let mut handler = RetainedHandler(Some(factory(startup)?));
    {
        let mut state = shared.state.lock().unwrap();
        if state.stop_attempt == 0 {
            state.phase = Phase::Ready;
        }
        state.changed();
        shared.wake.notify_all();
    }
    loop {
        let pending = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if state.stop_attempt > state.stopped_attempt
                    && !state
                        .queued
                        .as_ref()
                        .is_some_and(|p| p.operation.is_cleanup())
                {
                    let attempt = state.stop_attempt;
                    let wire_attempt = state.shutdown_wire;
                    drop(state);
                    let result = handler.0.as_mut().unwrap().shutdown();
                    state = shared.state.lock().unwrap();
                    state.stopped_attempt = attempt;
                    state.shutdown_reported = wire_attempt;
                    match result {
                        Ok(()) => {
                            // Engine affirmed it owns no live admission. Retire
                            // outside the control lock before publishing Stopped.
                            drop(state);
                            drop(handler.0.take());
                            state = shared.state.lock().unwrap();
                            state.phase = Phase::Stopped;
                            state.changed();
                            shared.wake.notify_all();
                            return Ok(());
                        }
                        Err(error) => {
                            state.phase = if state.stop_attempt > attempt {
                                Phase::Stopping
                            } else {
                                Phase::DrainFailed
                            };
                            state.error = Some(error);
                            state.changed();
                            shared.wake.notify_all();
                        }
                    }
                }
                if (state.phase == Phase::Ready
                    || (matches!(state.phase, Phase::Stopping | Phase::DrainFailed)
                        && state
                            .queued
                            .as_ref()
                            .is_some_and(|p| p.operation.is_cleanup())))
                    && state.retained.is_none()
                    && let Some(pending) = state.queued.take()
                {
                    if let Operation::PrepareCatalog(request) = &pending.operation {
                        state.admission = Some(preparing(request));
                    }
                    state.active = Some((pending.sequence, pending.cancel.clone()));
                    state.changed();
                    shared.wake.notify_all();
                    break pending;
                }
                state = shared.wake.wait(state).unwrap();
            }
        };
        let admission = admission_identity(&pending.operation).or_else(|| {
            if let Operation::ReleaseRoot { root } = &pending.operation {
                shared
                    .state
                    .lock()
                    .unwrap()
                    .admission
                    .as_ref()
                    .filter(|v| {
                        v.session == root.session
                            && v.bootstrap
                                .as_ref()
                                .is_some_and(|b| b.root_capability() == *root)
                    })
                    .map(|v| (v.operation, v.session.clone()))
            } else {
                None
            }
        });
        let context = OperationContext {
            cancel: if pending.operation.is_cleanup() {
                Arc::new(AtomicBool::new(false))
            } else {
                pending.cancel.clone()
            },
            shared: shared.clone(),
            admission,
            store: match &pending.operation {
                Operation::PreviewIo(r) => Some(crate::catalog_session::store::StatusQuery::from(
                    &crate::catalog_session::store::Query {
                        kind: crate::catalog_session::store::StatusKind::Objects,
                        root: r.root.clone(),
                        operation: r.operation,
                        selected: None,
                    },
                )),
                Operation::PreviewStore(r) => {
                    Some(crate::catalog_session::store::StatusQuery::from(
                        &crate::catalog_session::store::Query {
                            kind: crate::catalog_session::store::StatusKind::Locks,
                            root: r.root.clone(),
                            operation: r.operation,
                            selected: None,
                        },
                    ))
                }
                _ => None,
            },
        };
        let result = if pending.cancel.load(Ordering::Acquire) && !pending.operation.is_cleanup() {
            if let Operation::PrepareCatalog(_) = &pending.operation {
                let mut state = shared.state.lock().unwrap();
                if let Some(snapshot) = state.admission.as_mut() {
                    snapshot.state = AdmissionState::Abandoned;
                    snapshot.failure = Some(Failure::new(
                        FailureKind::Canceled,
                        "prepare canceled before any filesystem execution",
                    ));
                }
            }
            Err(Failure::new(
                FailureKind::Canceled,
                "filesystem operation canceled before execution",
            ))
        } else {
            handler
                .0
                .as_mut()
                .unwrap()
                .execute(pending.operation, &context)
        };
        let result = match result {
            Err(error) if error.validate().is_err() => Err(Failure::new(
                FailureKind::Unknown,
                "filesystem handler returned an oversized error; reconcile outcome",
            )),
            value => value,
        };
        let bytes = match super::wire::encode_outcome(&result) {
            Ok(bytes) => bytes,
            Err(error) => encode(
                &Outcome::Err(Failure::new(
                    FailureKind::Unknown,
                    format!("filesystem outcome could not be transported: {error}"),
                )),
                MESSAGE_BYTES,
            )?,
        };
        let info = ResultInfo {
            sequence: U64(pending.sequence),
            bytes: U64(bytes.len() as u64),
            blake3: blake3::hash(&bytes).to_hex().to_string(),
        };
        let mut state = shared.state.lock().unwrap();
        state.active = None;
        state.retained = Some(Retained {
            info,
            bytes: Arc::new(bytes),
            sent: false,
        });
        state.changed();
        shared.wake.notify_all();
    }
}

fn output_loop(mut output: impl Write, nonce: [u8; 16], shared: &Shared) -> Result<()> {
    loop {
        let (sequence, bytes) = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if let Some(result) = state.retained.as_mut().filter(|r| !r.sent) {
                    result.sent = true;
                    break (result.info.sequence.0, result.bytes.clone());
                }
                if state.execution_done {
                    return Ok(());
                }
                state = shared.wake.wait(state).unwrap();
            }
        };
        // Only this bounded encoded result plus a 16KiB transport chunk is live.
        Message::write_bytes(Kind::Result, sequence, &bytes, nonce, &mut output)?;
    }
}
fn control_loop(mut output: impl Write, nonce: [u8; 16], shared: &Shared) -> Result<()> {
    let mut seen = 0;
    loop {
        let (sequence, value, stopped) = {
            let mut state = shared.state.lock().unwrap();
            while state.revision == seen
                && state.admission_reply.is_none()
                && state.store_reply.is_none()
            {
                state = shared.wake.wait(state).unwrap();
            }
            if let Some((sequence, value)) = state.store_reply.take() {
                (sequence, value, false)
            } else if let Some((sequence, value)) = state.admission_reply.take() {
                (sequence, value, false)
            } else {
                seen = state.revision;
                (0, Control::Status(state.status()), state.execution_done)
            }
        };
        Message {
            kind: Kind::Control,
            sequence,
            bytes: encode(&value, MESSAGE_BYTES)?,
        }
        .write(nonce, &mut output)?;
        if stopped {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_volume::NativePath;
    use std::{collections::VecDeque, io, sync::mpsc, time::Duration};
    struct PipeRead {
        receiver: mpsc::Receiver<Vec<u8>>,
        bytes: VecDeque<u8>,
    }
    #[derive(Clone)]
    struct PipeWrite(mpsc::SyncSender<Vec<u8>>);
    fn pipe() -> (PipeWrite, PipeRead) {
        let (sender, receiver) = mpsc::sync_channel(1);
        (
            PipeWrite(sender),
            PipeRead {
                receiver,
                bytes: VecDeque::new(),
            },
        )
    }
    impl Read for PipeRead {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if out.is_empty() {
                return Ok(0);
            }
            while self.bytes.is_empty() {
                match self.receiver.recv_timeout(Duration::from_secs(5)) {
                    Ok(value) => self.bytes.extend(value),
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "fixture pipe timed out",
                        ));
                    }
                }
            }
            let size = out.len().min(self.bytes.len());
            for value in &mut out[..size] {
                *value = self.bytes.pop_front().unwrap();
            }
            Ok(size)
        }
    }
    impl Write for PipeWrite {
        fn write(&mut self, value: &[u8]) -> io::Result<usize> {
            self.0
                .send(value.to_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "fixture pipe closed"))?;
            Ok(value.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[derive(Default)]
    struct Observations {
        calls: std::sync::atomic::AtomicUsize,
        hold: AtomicBool,
        entered: AtomicBool,
        canceled: AtomicBool,
        may_close: AtomicBool,
    }
    struct Fake(Arc<Observations>, Option<AdmissionSnapshot>);
    impl Handler for Fake {
        fn execute(&mut self, operation: Operation, context: &OperationContext) -> Outcome {
            self.0.calls.fetch_add(1, Ordering::SeqCst);
            self.0.entered.store(true, Ordering::Release);
            while self.0.hold.load(Ordering::Acquire) {
                if context.cancellation().load(Ordering::Acquire) {
                    self.0.canceled.store(true, Ordering::Release);
                }
                thread::sleep(Duration::from_millis(1));
            }
            match operation {
                Operation::PrepareCatalog(request) => {
                    let mut snapshot = preparing(&request);
                    snapshot.catalog_created = true;
                    snapshot.state = AdmissionState::Failed;
                    snapshot.failure = Some(Failure::new(
                        FailureKind::Rejected,
                        "synthetic failure after create",
                    ));
                    context.publish_admission(snapshot.clone()).unwrap();
                    self.1 = Some(snapshot);
                    return Err(Failure::new(
                        FailureKind::Rejected,
                        "synthetic failure after create",
                    ));
                }
                Operation::AbandonPrepare { operation, session } => {
                    let value = self.1.as_mut().unwrap();
                    assert_eq!(value.operation, operation);
                    assert_eq!(value.session, session);
                    value.state = AdmissionState::Abandoned;
                    context.publish_admission(value.clone()).unwrap();
                }
                _ => {}
            }
            // A cancellation observed after publication may still return success.
            Ok(Response::Unit(Empty::default()))
        }
        fn shutdown(&mut self) -> std::result::Result<(), Failure> {
            if !self.0.may_close.load(Ordering::Acquire)
                || self
                    .1
                    .as_ref()
                    .is_some_and(|v| v.state != AdmissionState::Abandoned)
            {
                return Err(Failure::new(
                    FailureKind::Rejected,
                    "dependent catalog still retained",
                ));
            }
            Ok(())
        }
    }
    struct Harness {
        input: Option<PipeWrite>,
        output: PipeRead,
        control: PipeRead,
        owner: Option<thread::JoinHandle<Result<()>>>,
        nonce: [u8; 16],
        shutdown_attempt: u64,
        observed: Arc<Observations>,
    }
    impl Harness {
        fn new() -> Result<Self> {
            let (mut sender, input) = pipe();
            let (output, out) = pipe();
            let (control, controls) = pipe();
            let startup = Startup::new(vec![])?;
            let nonce = startup.nonce();
            let observed = Arc::new(Observations::default());
            observed.may_close.store(true, Ordering::Release);
            let state = observed.clone();
            let owner = thread::spawn(move || {
                serve(input, output, control, move |_| Ok(Fake(state, None)))
            });
            Message {
                kind: Kind::Startup,
                sequence: 0,
                bytes: encode(&startup, CONFIG_BYTES)?,
            }
            .write(nonce, &mut sender)?;
            let mut value = Self {
                input: Some(sender),
                output: out,
                control: controls,
                owner: Some(owner),
                nonce,
                shutdown_attempt: 0,
                observed,
            };
            value.until_status(|s| s.phase == Phase::Ready)?;
            Ok(value)
        }
        fn send(&mut self, kind: Kind, sequence: u64, bytes: Vec<u8>) -> Result<()> {
            let sequence = if kind == Kind::Stop {
                self.shutdown_attempt += 1;
                self.shutdown_attempt
            } else {
                sequence
            };
            Message {
                kind,
                sequence,
                bytes,
            }
            .write(self.nonce, self.input.as_mut().unwrap())?;
            Ok(())
        }
        fn command(&mut self, sequence: u64) -> Result<()> {
            self.send(
                Kind::Execute,
                sequence,
                encode(
                    &Operation::GlobalRestoreStatus {
                        root: NativePath::from_path(&std::env::temp_dir()),
                    },
                    MESSAGE_BYTES,
                )?,
            )
        }
        fn message(reader: &mut PipeRead) -> Result<Message> {
            let first =
                Frame::read(reader)?.ok_or_else(|| anyhow::anyhow!("fixture expected a reply"))?;
            let mut assembly = Assembly::start(&first)?;
            let mut done = assembly.push(first)?;
            while !done {
                done = assembly.push(
                    Frame::read(reader)?.ok_or_else(|| anyhow::anyhow!("partial fixture reply"))?,
                )?;
            }
            Ok(assembly.finish()?)
        }
        fn until_status(&mut self, predicate: impl Fn(&Status) -> bool) -> Result<Status> {
            loop {
                let value: Control =
                    decode(&Self::message(&mut self.control)?.bytes, MESSAGE_BYTES)?;
                if let Control::Status(status) = value
                    && predicate(&status)
                {
                    return Ok(status);
                }
            }
        }
        fn result(&mut self, id: u64) -> Result<Outcome> {
            let message = Self::message(&mut self.output)?;
            assert_eq!(message.sequence, id);
            let value = decode(&message.bytes, MESSAGE_BYTES)?;
            self.send(
                Kind::Ack,
                id,
                encode(
                    &Ack {
                        sequence: U64(id),
                        blake3: blake3::hash(&message.bytes).to_hex().to_string(),
                    },
                    CHUNK_BYTES,
                )?,
            )?;
            Ok(value)
        }
        fn close(mut self) -> Result<()> {
            self.send(Kind::Stop, 0, vec![])?;
            self.until_status(|s| s.phase == Phase::Stopped)?;
            self.input.take();
            while Frame::read(&mut self.control)?.is_some() {}
            self.owner
                .take()
                .unwrap()
                .join()
                .expect("fixture owner panic")?;
            Ok(())
        }
    }
    #[test]
    fn early_cancel_never_dispatches_and_duplicate_never_repeats_mutation() -> Result<()> {
        let mut h = Harness::new()?;
        h.send(Kind::Cancel, 1, vec![])?;
        h.command(1)?;
        assert!(matches!(
            h.result(1)?,
            Err(Failure {
                object_receipt: None,
                kind: FailureKind::Canceled,
                ..
            })
        ));
        assert_eq!(h.observed.calls.load(Ordering::Acquire), 0);
        h.command(2)?;
        assert!(h.result(2)?.is_ok());
        h.command(2)?;
        h.send(Kind::Status, 0, vec![])?;
        h.until_status(|s| s.active.is_none() && s.queued.is_none())?;
        assert_eq!(h.observed.calls.load(Ordering::Acquire), 1);
        h.close()
    }
    #[test]
    fn blocked_handler_keeps_controls_and_queue_admission_bounded() -> Result<()> {
        let mut h = Harness::new()?;
        h.observed.hold.store(true, Ordering::Release);
        h.command(1)?;
        h.until_status(|s| s.active == Some(U64(1)))?;
        h.command(2)?;
        h.until_status(|s| s.queued == Some(U64(2)))?;
        h.command(3)?;
        loop {
            let message = Harness::message(&mut h.control)?;
            if matches!(
                decode::<Control>(&message.bytes, MESSAGE_BYTES)?,
                Control::Rejected(_)
            ) {
                assert_eq!(message.sequence, 3);
                break;
            }
        }
        h.send(Kind::Cancel, 1, vec![])?;
        h.send(Kind::Status, 0, vec![])?;
        h.until_status(|s| s.active == Some(U64(1)))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !h.observed.canceled.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(h.observed.calls.load(Ordering::Acquire), 1);
        h.observed.hold.store(false, Ordering::Release);
        // Ordinary stdout is still unread, but control stays responsive.
        h.until_status(|s| s.retained.as_ref().is_some_and(|r| r.sequence == U64(1)))?;
        assert!(h.result(1)?.is_ok()); // commit-winning cancellation
        assert!(h.result(2)?.is_ok());
        assert_eq!(h.observed.calls.load(Ordering::Acquire), 2);
        h.close()
    }
    #[test]
    fn checked_shutdown_failure_keeps_owner_until_explicit_retry() -> Result<()> {
        let mut h = Harness::new()?;
        h.observed.may_close.store(false, Ordering::Release);
        h.send(Kind::Stop, 0, vec![])?;
        h.until_status(|s| s.phase == Phase::DrainFailed)?;
        assert!(!h.owner.as_ref().unwrap().is_finished());
        h.observed.may_close.store(true, Ordering::Release);
        h.close()
    }
    #[test]
    fn client_keeps_failed_drain_owner_and_allows_only_explicit_cleanup() -> Result<()> {
        let (sender, input) = pipe();
        let (output, out) = pipe();
        let (control, controls) = pipe();
        let observed = Arc::new(Observations::default());
        observed.may_close.store(true, Ordering::Release);
        let obs = observed.clone();
        let server = thread::spawn(move || {
            serve(input, output, control, move |_| Ok(Fake(obs, None))).unwrap();
        });
        let client = super::super::client::Client::test_streams(
            Startup::new(vec![])?,
            (sender, out, controls),
            server,
        )?;
        let session = crate::catalog_session::LeaseId::new();
        let root = NativePath::from_path(&std::env::temp_dir());
        let request = crate::catalog_session::PrepareCatalog {
            operation: U64(9007199254740993),
            session: session.clone(),
            mode: crate::catalog_session::BootstrapMode::DesktopCreate,
            root: root.clone(),
            manifest_root: root,
            import_source: None,
        };
        let cancel = AtomicBool::new(false);
        assert!(
            client
                .execute(Operation::PrepareCatalog(request.clone()), &cancel)
                .is_err()
        );
        assert!(client.try_shutdown().is_err());
        assert_eq!(client.status().phase, Phase::DrainFailed);
        // A Ready frame already queued before Stop may arrive late. Local
        // admission stays closed and the externally observed phase stays draining.
        client.test_stale_ready_after_stop();
        assert_eq!(client.status().phase, Phase::Stopping);
        assert!(
            client
                .execute(Operation::PrepareCatalog(request.clone()), &cancel)
                .is_err()
        );
        assert_eq!(observed.calls.load(Ordering::Acquire), 1);
        let snapshot = client
            .admission_status(request.operation, &session)?
            .unwrap();
        assert!(snapshot.catalog_created);
        assert_eq!(snapshot.state, AdmissionState::Failed);
        client.execute(
            Operation::AbandonPrepare {
                operation: request.operation,
                session,
            },
            &cancel,
        )?;
        assert_eq!(observed.calls.load(Ordering::Acquire), 2);
        client.try_shutdown()?;
        assert_eq!(client.status().phase, Phase::Stopped);
        Ok(())
    }
    #[test]
    fn partial_prepare_facts_survive_result_ack_and_abandon_before_next_prepare() -> Result<()> {
        let mut h = Harness::new()?;
        let session = crate::catalog_session::LeaseId::new();
        let root = NativePath::from_path(&std::env::temp_dir());
        for (sequence, operation) in [(1, 9007199254740993), (3, 9007199254740994)] {
            let request = crate::catalog_session::PrepareCatalog {
                operation: U64(operation),
                session: session.clone(),
                mode: crate::catalog_session::BootstrapMode::DesktopCreate,
                root: root.clone(),
                manifest_root: root.clone(),
                import_source: None,
            };
            h.send(
                Kind::Execute,
                sequence,
                encode(&Operation::PrepareCatalog(request), MESSAGE_BYTES)?,
            )?;
            assert!(h.result(sequence)?.is_err());
            h.send(
                Kind::AdmissionStatus,
                operation,
                encode(
                    &AdmissionQuery {
                        operation: U64(operation),
                        session: session.clone(),
                    },
                    CHUNK_BYTES,
                )?,
            )?;
            loop {
                let message = Harness::message(&mut h.control)?;
                if let Control::Admission(value) = decode(&message.bytes, MESSAGE_BYTES)? {
                    let value = value.unwrap();
                    assert_eq!(value.operation, U64(operation));
                    assert_eq!(value.state, AdmissionState::Failed);
                    assert!(value.catalog_created);
                    assert!(!value.directory_created);
                    break;
                }
            }
            h.send(
                Kind::Execute,
                sequence + 1,
                encode(
                    &Operation::AbandonPrepare {
                        operation: U64(operation),
                        session: session.clone(),
                    },
                    MESSAGE_BYTES,
                )?,
            )?;
            assert!(h.result(sequence + 1)?.is_ok());
        }
        assert_eq!(h.observed.calls.load(Ordering::Acquire), 4);
        h.close()
    }
}
