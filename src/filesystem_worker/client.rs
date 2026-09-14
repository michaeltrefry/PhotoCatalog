//! G-owned sibling-process client. Never construct this client inside managed C.
use super::wire::*;
use crate::{
    application::U64,
    catalog_backup::RestoreStatus,
    catalog_session::{
        CatalogBootstrap, CatalogFilesystem, ConfirmSqlAdmission, LeaseId, PrepareCatalog,
        RootCapability, SqlAdmissionConfirmed,
    },
    storage_volume::NativePath,
};
use anyhow::{Result, ensure};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

struct Outbound {
    command: Option<Message>,
    command_cleanup: bool,
    cancel: Option<u64>,
    ack: Option<Ack>,
    admission: Option<(u64, AdmissionQuery)>,
    store: Option<(u64, crate::catalog_session::store::StatusQuery)>,
    status: Option<u64>,
    stop: Option<u64>,
    close: bool,
}
struct State {
    status: Status,
    outgoing: Outbound,
    sequence: u64,
    waiting: Option<u64>,
    result: Option<(u64, Outcome, String)>,
    admission: Option<(u64, Option<AdmissionSnapshot>)>,
    store: Option<(
        u64,
        std::result::Result<crate::catalog_session::store::Status, Failure>,
    )>,
    store_sequence: u64,
    store_waiting: Option<(u64, crate::catalog_session::store::StatusQuery)>,
    failure: Option<Failure>,
    reaped: bool,
    io_drained: bool,
    closing: bool,
    completed: Option<(u64, String)>,
    calls: usize,
    shutdown_requested: u64,
    startup_failed: bool,
}
struct CallSlot<'a>(&'a Shared);
impl Drop for CallSlot<'_> {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.calls -= 1;
        self.0.wake.notify_all();
    }
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    nonce: [u8; 16],
}
impl Shared {
    fn fail(&self, message: impl std::fmt::Display) {
        let mut state = self.state.lock().unwrap();
        if state.failure.is_none() {
            state.failure = Some(Failure::new(FailureKind::Unknown, message));
        }
        state.status.phase = Phase::Unknown;
        state.status.error = state.failure.clone();
        self.wake.notify_all();
    }
}
#[cfg(test)]
#[derive(Default)]
struct Faults {
    fail_thread_at: Option<usize>,
    thread_attempt: usize,
    try_wait: bool,
    wait: bool,
}
struct Owner {
    child: Option<Child>,
    threads: Vec<thread::JoinHandle<()>>,
    #[cfg(test)]
    faults: Faults,
}
impl Owner {
    fn start_io(
        &mut self,
        name: &str,
        task: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<()> {
        #[cfg(test)]
        {
            let attempt = self.faults.thread_attempt;
            self.faults.thread_attempt += 1;
            if self.faults.fail_thread_at == Some(attempt) {
                return Err(std::io::Error::other(
                    "injected post-spawn I/O thread creation failure",
                ));
            }
        }
        self.threads
            .push(thread::Builder::new().name(name.into()).spawn(task)?);
        Ok(())
    }
    fn poll(&mut self, shared: &Shared) -> Result<bool> {
        #[cfg(test)]
        if self.child.is_some() && std::mem::take(&mut self.faults.try_wait) {
            anyhow::bail!("injected owned try_wait failure");
        }
        if let Some(child) = self.child.as_mut()
            && let Some(status) = child.try_wait()?
        {
            self.child.take();
            let mut state = shared.state.lock().unwrap();
            state.reaped = true;
            state.outgoing.close = true;
            if !status.success() {
                state.failure = Some(Failure::new(
                    FailureKind::Unknown,
                    format!("filesystem helper exited {status}"),
                ));
            }
            shared.wake.notify_all();
        }
        if self.child.is_none() && self.threads.iter().all(thread::JoinHandle::is_finished) {
            self.join_all(shared);
            {
                let mut state = shared.state.lock().unwrap();
                state.reaped = true;
                state.io_drained = true;
            }
            shared.wake.notify_all();
            return Ok(true);
        }
        Ok(false)
    }
    fn join_all(&mut self, shared: &Shared) {
        let mut panicked = false;
        for owner in self.threads.drain(..) {
            panicked |= owner.join().is_err();
        }
        if panicked {
            shared.fail("filesystem I/O owner panicked; outcomes remain uncertain");
        }
    }
}

pub struct Client {
    shared: Arc<Shared>,
    owner: Mutex<Owner>,
    calls: Mutex<()>,
    admission_calls: Mutex<()>,
    store_calls: Mutex<()>,
    shutdown: Mutex<()>,
    epoch: LeaseId,
    pid: u32,
}
impl Client {
    /// Configured binary only. This API creates no user-selected executable route.
    /// The parent must own this F independently of, and longer than, dependent C.
    pub fn spawn(executable: &Path, original_roots: Vec<NativePath>) -> Result<Self> {
        let startup = Startup::new(original_roots)?;
        let hello = encode(&startup, CONFIG_BYTES)?;
        let mut child = Command::new(executable)
            .arg("--catalog-filesystem-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().expect("configured stdin");
        let output = child.stdout.take().expect("configured stdout");
        let control = child.stderr.take().expect("configured stderr");
        let pid = child.id();
        Ok(Self::connect(
            startup,
            hello,
            Owner {
                child: Some(child),
                threads: vec![],
                #[cfg(test)]
                faults: Faults::default(),
            },
            (input, output, control),
            pid,
        ))
    }
    fn connect(
        startup: Startup,
        hello: Vec<u8>,
        mut owner: Owner,
        streams: (
            impl Write + Send + 'static,
            impl Read + Send + 'static,
            impl Read + Send + 'static,
        ),
        pid: u32,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                status: Status {
                    phase: Phase::Starting,
                    shutdown_attempt: U64(0),
                    active: None,
                    queued: None,
                    retained: None,
                    canceled_before_execution: None,
                    error: None,
                },
                outgoing: Outbound {
                    command: None,
                    command_cleanup: false,
                    cancel: None,
                    ack: None,
                    admission: None,
                    store: None,
                    status: None,
                    stop: None,
                    close: false,
                },
                sequence: 0,
                waiting: None,
                result: None,
                admission: None,
                store: None,
                store_sequence: 0,
                store_waiting: None,
                failure: None,
                reaped: false,
                io_drained: false,
                closing: false,
                completed: None,
                calls: 0,
                shutdown_requested: 0,
                startup_failed: false,
            }),
            wake: Condvar::new(),
            nonce: startup.nonce(),
        });
        let (input, output, control) = streams;
        let started = (|| -> std::io::Result<()> {
            let state = shared.clone();
            owner.start_io("filesystem-client-input", move || {
                if let Err(error) = write_loop(input, hello, &state) {
                    state.fail(error);
                }
            })?;
            let state = shared.clone();
            owner.start_io("filesystem-client-results", move || {
                if let Err(error) = read_loop(output, false, &state) {
                    state.fail(error);
                }
            })?;
            let state = shared.clone();
            owner.start_io("filesystem-client-controls", move || {
                if let Err(error) = read_loop(control, true, &state) {
                    state.fail(error);
                }
            })?;
            Ok(())
        })();
        if let Err(error) = started {
            // No operation can be submitted before this Client is returned. The
            // factory is prohibited from filesystem work, so failed startup has
            // no dependent owners. Keep the exact Child reachable for retirement.
            {
                let mut state = shared.state.lock().unwrap();
                state.startup_failed = true;
                state.outgoing.close = true;
            }
            shared.fail(error);
        }
        Self {
            shared,
            owner: Mutex::new(owner),
            calls: Mutex::new(()),
            admission_calls: Mutex::new(()),
            store_calls: Mutex::new(()),
            shutdown: Mutex::new(()),
            epoch: startup.epoch,
            pid,
        }
    }
    /// Historical spawn identity only; successful checked wait proves retirement.
    pub fn pid(&self) -> u32 {
        self.pid
    }
    #[cfg(test)]
    pub(super) fn test_streams(
        startup: Startup,
        streams: (
            impl Write + Send + 'static,
            impl Read + Send + 'static,
            impl Read + Send + 'static,
        ),
        server: thread::JoinHandle<()>,
    ) -> Result<Self> {
        let hello = encode(&startup, CONFIG_BYTES)?;
        Ok(Self::connect(
            startup,
            hello,
            Owner {
                child: None,
                threads: vec![server],
                faults: Faults::default(),
            },
            streams,
            0,
        ))
    }
    #[cfg(test)]
    pub(super) fn test_stale_ready_after_stop(&self) {
        let mut state = self.shared.state.lock().unwrap();
        assert!(state.closing);
        state.status.phase = Phase::Ready;
    }
    pub fn epoch(&self) -> &LeaseId {
        &self.epoch
    }
    pub fn status(&self) -> Status {
        let state = self.shared.state.lock().unwrap();
        let mut value = state.status.clone();
        if let Some(error) = &state.failure {
            value.phase = Phase::Unknown;
            value.error = Some(error.clone());
        }
        if state.closing && matches!(value.phase, Phase::Starting | Phase::Ready) {
            value.phase = Phase::Stopping;
        }
        if value.phase == Phase::Stopped && !(state.reaped && state.io_drained) {
            value.phase = Phase::Stopping;
        }
        value
    }
    pub fn refresh_status(&self) {
        self.shared.state.lock().unwrap().outgoing.status = Some(0);
        self.shared.wake.notify_all();
    }
    pub fn cancel(&self, sequence: U64) -> Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        ensure!(
            state.waiting == Some(sequence.0),
            "filesystem cancellation belongs to another operation"
        );
        state.outgoing.cancel = Some(sequence.0);
        self.shared.wake.notify_all();
        Ok(())
    }
    /// A worker may wait here; independent status/cancel/stop never takes calls.
    pub fn execute(&self, operation: Operation, cancel: &AtomicBool) -> Result<Response> {
        let _admission = {
            let mut state = self.shared.state.lock().unwrap();
            ensure!(
                state.calls < 2,
                "filesystem client active/queued admission limit"
            );
            state.calls += 1;
            CallSlot(&self.shared)
        };
        operation.validate()?;
        let bytes = super::wire::encode_operation(&operation)?;
        let _call = self.calls.lock().unwrap();
        let mut state = self.shared.state.lock().unwrap();
        while state.status.phase == Phase::Starting && state.failure.is_none() {
            state = self.shared.wake.wait(state).unwrap();
        }
        if let Some(error) = &state.failure {
            return Err(error.clone().into());
        }
        ensure!(
            (!state.closing && state.status.phase == Phase::Ready)
                || (matches!(
                    state.status.phase,
                    Phase::Ready | Phase::Stopping | Phase::DrainFailed
                ) && operation.is_cleanup()),
            "filesystem helper is not admitting operations"
        );
        if cancel.load(Ordering::Acquire) && !operation.is_cleanup() {
            return Err(Failure::new(
                FailureKind::Canceled,
                "filesystem operation canceled before admission",
            )
            .into());
        }
        ensure!(
            state.waiting.is_none() && state.result.is_none(),
            "filesystem result remains unresolved"
        );
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("filesystem sequence exhausted"))?;
        let sequence = state.sequence;
        state.waiting = Some(sequence);
        state.outgoing.command_cleanup = operation.is_cleanup();
        state.outgoing.command = Some(Message {
            kind: Kind::Execute,
            sequence,
            bytes,
        });
        self.shared.wake.notify_all();
        let mut signaled = false;
        let mut retrieved = false;
        loop {
            if state
                .result
                .as_ref()
                .is_some_and(|(id, _, _)| *id == sequence)
            {
                let (_, result, digest) = state.result.take().unwrap();
                state.waiting = None;
                state.completed = Some((sequence, digest.clone()));
                if !digest.is_empty() {
                    state.outgoing.ack = Some(Ack {
                        sequence: U64(sequence),
                        blake3: digest,
                    });
                }
                self.shared.wake.notify_all();
                // Cancellation after successful publication does not change it.
                return result.map_err(Into::into);
            }
            if let Some(error) = &state.failure {
                return Err(error.clone().into());
            }
            if state.status.canceled_before_execution == Some(U64(sequence)) {
                state.waiting = None;
                return Err(Failure::new(
                    FailureKind::Canceled,
                    "filesystem request removed before execution by Stop",
                )
                .into());
            }
            // A terminal control frame may precede its stdout outcome. Keep
            // the call and acknowledgement slot until that exact result arrives.
            // Poisoned pipes are handled by the sticky failure above.

            if !signaled && cancel.load(Ordering::Acquire) && !operation.is_cleanup() {
                state.outgoing.cancel = Some(sequence);
                signaled = true;
                self.shared.wake.notify_all();
            }
            if !retrieved
                && state
                    .status
                    .retained
                    .as_ref()
                    .is_some_and(|r| r.sequence.0 == sequence)
            {
                state.outgoing.status = Some(sequence);
                retrieved = true;
                self.shared.wake.notify_all();
            }
            state = self
                .shared
                .wake
                .wait_timeout(state, Duration::from_millis(20))
                .unwrap()
                .0;
        }
    }
    /// Cached facts only. No creation, path opens, or confirmation is replayed.
    pub fn admission_status(
        &self,
        operation: U64,
        session: &LeaseId,
    ) -> Result<Option<AdmissionSnapshot>> {
        let _call = self.admission_calls.lock().unwrap();
        let mut state = self.shared.state.lock().unwrap();
        ensure!(operation.0 > 0, "invalid prepare operation");
        state.admission = None;
        state.outgoing.admission = Some((
            operation.0,
            AdmissionQuery {
                operation,
                session: session.clone(),
            },
        ));
        self.shared.wake.notify_all();
        loop {
            if state
                .admission
                .as_ref()
                .is_some_and(|(id, _)| *id == operation.0)
            {
                let (_, value) = state.admission.take().unwrap();
                if let Some(snapshot) = &value {
                    ensure!(
                        snapshot.operation == operation && snapshot.session == *session,
                        "cached admission belongs to another owner"
                    );
                }
                return Ok(value);
            }
            if state.reaped {
                anyhow::bail!("filesystem admission owner has exited");
            }
            if let Some(error) = &state.failure {
                return Err(error.clone().into());
            }
            state = self.shared.wake.wait(state).unwrap();
        }
    }
    pub fn store_status(
        &self,
        query: &crate::catalog_session::store::StatusQuery,
    ) -> Result<crate::catalog_session::store::Status> {
        query.validate()?;
        ensure!(
            query.epoch == self.epoch,
            "preview query helper epoch mismatch"
        );
        let _call = self.store_calls.lock().unwrap();
        let mut state = self.shared.state.lock().unwrap();
        // A fully received matching response is authoritative even when Stop
        // arrived immediately afterward. No new query is sent to a dead writer.
        if state
            .store_waiting
            .as_ref()
            .is_some_and(|(_, active)| active == query)
            && let Some((sequence, value)) = state.store.take()
        {
            ensure!(
                state
                    .store_waiting
                    .as_ref()
                    .is_some_and(|(id, _)| *id == sequence),
                "preview status sequence mismatch"
            );
            state.store_waiting = None;
            return value.map_err(Into::into);
        }
        ensure!(
            !state.reaped && !state.outgoing.close && state.status.phase != Phase::Stopped,
            "preview filesystem owner is terminal"
        );
        if let Some(error) = &state.failure {
            return Err(error.clone().into());
        }
        ensure!(
            state.outgoing.store.is_none()
                && state.store.is_none()
                && state.store_waiting.is_none(),
            "preview status slot busy"
        );
        state.store_sequence = state
            .store_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("preview query sequence exhausted"))?;
        let sequence = state.store_sequence;
        state.store_waiting = Some((sequence, query.clone()));
        state.outgoing.store = Some((sequence, query.clone()));
        self.shared.wake.notify_all();
        loop {
            if let Some((actual, value)) = state.store.take() {
                ensure!(actual == sequence, "preview status sequence mismatch");
                state.store_waiting = None;
                return value.map_err(Into::into);
            }
            if state.reaped || state.outgoing.close || state.status.phase == Phase::Stopped {
                state.store_waiting = None;
                state.outgoing.store = None;
                anyhow::bail!("preview filesystem owner is terminal");
            }
            if let Some(error) = &state.failure {
                return Err(error.clone().into());
            }
            state = self.shared.wake.wait(state).unwrap();
        }
    }
    pub fn signal_stop(&self) {
        let mut state = self.shared.state.lock().unwrap();
        if state.status.phase == Phase::Stopped {
            return;
        }
        state.closing = true;
        if !state.outgoing.command_cleanup
            && let Some(command) = state.outgoing.command.take()
        {
            state.result = Some((
                command.sequence,
                Err(Failure::new(
                    FailureKind::Canceled,
                    "filesystem request stopped before sending",
                )),
                String::new(),
            ));
        }
        let Some(attempt) = state.shutdown_requested.checked_add(1) else {
            state.failure = Some(Failure::new(
                FailureKind::Unknown,
                "filesystem shutdown identity exhausted",
            ));
            state.status.phase = Phase::Unknown;
            self.shared.wake.notify_all();
            return;
        };
        state.shutdown_requested = attempt;
        state.outgoing.stop = Some(attempt);
        state.status.phase = Phase::Stopping;
        state.status.error = None;
        self.shared.wake.notify_all();
    }
    /// Caller first retires every dependent C/SQL/native owner and releases root
    /// capabilities. Failed wait or checked shutdown keeps the same Child owner.
    pub fn try_shutdown(&self) -> Result<()> {
        let _attempt = self.shutdown.lock().unwrap();
        self.signal_stop();
        loop {
            let complete = self.owner.lock().unwrap().poll(&self.shared)?;
            if complete {
                let mut state = self.shared.state.lock().unwrap();
                ensure!(
                    state.status.phase == Phase::Stopped && state.failure.is_none(),
                    "filesystem child reaped without verified clean shutdown"
                );
                state.status.phase = Phase::Stopped;
                return Ok(());
            }
            let state = self.shared.state.lock().unwrap();
            if state.status.phase == Phase::DrainFailed
                && state.status.shutdown_attempt.0 == state.shutdown_requested
            {
                anyhow::bail!(
                    "{}",
                    state
                        .status
                        .error
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "filesystem drain failed".into())
                );
            }
            if state.failure.is_some() && !state.reaped {
                anyhow::bail!("filesystem transport uncertain; process owner retained");
            }
            drop(
                self.shared
                    .wake
                    .wait_timeout(state, Duration::from_millis(20))
                    .unwrap(),
            );
        }
    }
    /// Retire an owned post-spawn startup failure. This proof is internal: no
    /// Client operation was exposed and the factory is forbidden to access FS.
    /// A failed wait leaves this same owner available for another attempt.
    pub fn retire_failed_startup(&self) -> Result<()> {
        ensure!(
            self.shared.state.lock().unwrap().startup_failed,
            "filesystem startup was admitted; dependent drain proof is required"
        );
        self.terminate_after_dependents_drained()
    }
    /// Irrecoverable transport only. G must first verify retirement of every
    /// dependent C/SQL/native owner. This is a Rust coordinator boundary, never
    /// a renderer operation and never called implicitly by Drop. The caller's
    /// checked shutdown receipts will provide that proof when CT relay is wired.
    /// Successful return proves process/I/O retirement, NOT operation rollback.
    pub fn terminate_after_dependents_drained(&self) -> Result<()> {
        let _attempt = self.shutdown.lock().unwrap();
        {
            let mut state = self.shared.state.lock().unwrap();
            state.closing = true;
            state.outgoing.close = true;
            state.failure = Some(Failure::new(
                FailureKind::Unknown,
                "filesystem transport terminated after dependent drain; reconcile operation outcomes",
            ));
            state.status.phase = Phase::Unknown;
            state.status.error = state.failure.clone();
            self.shared.wake.notify_all();
        }
        let mut owner = self.owner.lock().unwrap();
        #[cfg(test)]
        let fail_wait = std::mem::take(&mut owner.faults.wait);
        if let Some(child) = owner.child.as_mut() {
            // A kill error does not establish that the process is still alive;
            // a successful wait is the sole authority to release its owner.
            let _ = child.kill();
            #[cfg(test)]
            if fail_wait {
                anyhow::bail!("injected owned wait failure after kill");
            }
            child.wait()?;
            owner.child.take();
            self.shared.state.lock().unwrap().reaped = true;
            self.shared.wake.notify_all();
        }
        owner.join_all(&self.shared);
        self.shared.state.lock().unwrap().io_drained = true;
        self.shared.wake.notify_all();
        Ok(())
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        // An unexpected last-reference Drop is not evidence that a sibling C has
        // drained. Retain any unretired process/pipe owners rather than kill it.
        let owner = self
            .owner
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner());
        if owner.child.is_some() || !owner.threads.is_empty() {
            let retained = Owner {
                child: owner.child.take(),
                threads: std::mem::take(&mut owner.threads),
                #[cfg(test)]
                faults: std::mem::take(&mut owner.faults),
            };
            std::mem::forget(retained);
        }
    }
}

fn write_loop(mut input: impl Write, hello: Vec<u8>, shared: &Shared) -> Result<()> {
    Message {
        kind: Kind::Startup,
        sequence: 0,
        bytes: hello,
    }
    .write(shared.nonce, &mut input)?;
    loop {
        let message = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if state.outgoing.close {
                    return Ok(());
                }
                let out = &mut state.outgoing;
                // A pending cleanup must reach even an idle F before Stop can
                // affirm shutdown; preserving the slot alone is insufficient.
                if out.command_cleanup
                    && let Some(command) = out.command.take()
                {
                    out.command_cleanup = false;
                    break command;
                }
                if let Some(sequence) = out.stop.take() {
                    break Message {
                        kind: Kind::Stop,
                        sequence,
                        bytes: vec![],
                    };
                }
                if let Some(id) = out.cancel.take() {
                    break Message {
                        kind: Kind::Cancel,
                        sequence: id,
                        bytes: vec![],
                    };
                }
                if let Some(ack) = out.ack.take() {
                    break Message {
                        kind: Kind::Ack,
                        sequence: ack.sequence.0,
                        bytes: encode(&ack, CHUNK_BYTES)?,
                    };
                }
                if let Some((id, query)) = out.store.take() {
                    break Message {
                        kind: Kind::StoreStatus,
                        sequence: id,
                        bytes: encode(&query, CHUNK_BYTES)?,
                    };
                }
                if let Some((id, query)) = out.admission.take() {
                    break Message {
                        kind: Kind::AdmissionStatus,
                        sequence: id,
                        bytes: encode(&query, CHUNK_BYTES)?,
                    };
                }
                if let Some(sequence) = out.status.take() {
                    break Message {
                        kind: Kind::Status,
                        sequence,
                        bytes: vec![],
                    };
                }
                if let Some(message) = out.command.take() {
                    break message;
                }
                state = shared.wake.wait(state).unwrap();
            }
        };
        message.write(shared.nonce, &mut input)?;
    }
}
fn read_loop(mut input: impl Read, control: bool, shared: &Shared) -> Result<()> {
    let mut assembly: Option<Assembly> = None;
    while let Some(frame) = Frame::read(&mut input)? {
        ensure!(
            frame.epoch == shared.nonce,
            "filesystem reply epoch mismatch"
        );
        ensure!(
            frame.kind == if control { Kind::Control } else { Kind::Result },
            "unexpected filesystem output channel"
        );
        if assembly.is_none() {
            assembly = Some(Assembly::start(&frame)?);
        }
        if !assembly.as_mut().unwrap().push(frame)? {
            continue;
        }
        let message = assembly.take().unwrap().finish()?;
        if control {
            let reply: Control = decode(&message.bytes, MESSAGE_BYTES)?;
            let mut state = shared.state.lock().unwrap();
            match reply {
                Control::Status(value) => {
                    if let Some(error) = &value.error {
                        error.validate()?;
                    }
                    if value.phase == Phase::Stopped {
                        state.outgoing.close = true;
                    }
                    if value.phase == Phase::Unknown {
                        state.failure = Some(value.error.clone().unwrap_or_else(|| {
                            Failure::new(FailureKind::Unknown, "filesystem execution owner failed")
                        }));
                        state.outgoing.close = true;
                    }
                    state.status = value;
                }
                Control::Store(value) => {
                    let (sequence, query) = state
                        .store_waiting
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("unsolicited preview status response"))?;
                    ensure!(
                        *sequence == message.sequence,
                        "preview status response sequence mismatch"
                    );
                    ensure!(state.store.is_none(), "preview status reply slot occupied");
                    match &value {
                        Ok(status) => status.validate(query)?,
                        Err(error) => error.validate()?,
                    }
                    state.store = Some((message.sequence, value));
                }
                Control::Admission(value) => {
                    if let Some(snapshot) = &value {
                        snapshot.validate()?;
                    }
                    ensure!(
                        state.admission.is_none(),
                        "unconsumed filesystem admission reply"
                    );
                    state.admission = Some((message.sequence, value));
                }
                Control::Rejected(error) => {
                    error.validate()?;
                    ensure!(
                        state.waiting == Some(message.sequence),
                        "rejection sequence mismatch"
                    );
                    state.result = Some((message.sequence, Err(error), String::new()));
                }
            }
            shared.wake.notify_all();
        } else {
            let value: Outcome = super::wire::decode_outcome(&message.bytes)?;
            if let Err(error) = &value {
                error.validate()?;
            }
            let digest = blake3::hash(&message.bytes).to_hex().to_string();
            let mut state = shared.state.lock().unwrap();
            if state
                .completed
                .as_ref()
                .is_some_and(|(id, hash)| *id == message.sequence && *hash == digest)
            {
                continue;
            }
            if state
                .result
                .as_ref()
                .is_some_and(|(id, _, hash)| *id == message.sequence && *hash == digest)
            {
                continue;
            }
            ensure!(
                state.waiting == Some(message.sequence) && state.result.is_none(),
                "filesystem result belongs to another operation"
            );
            state.result = Some((message.sequence, value, digest));
            shared.wake.notify_all();
        }
    }
    ensure!(assembly.is_none(), "filesystem output ended inside a frame");
    let state = shared.state.lock().unwrap();
    ensure!(
        state.status.phase == Phase::Stopped || (!control && state.closing),
        "filesystem pipe closed before retirement; outcomes unknown"
    );
    Ok(())
}

impl CatalogFilesystem for Client {
    fn preview_stage_call(
        &self,
        request: &crate::catalog_session::preview_stage::Request,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::preview_stage::Reply> {
        match self.execute(Operation::PreviewStage(request.clone()), cancel)? {
            Response::PreviewStage(reply) => {
                reply.validate(request)?;
                Ok(reply)
            }
            _ => anyhow::bail!("unexpected stage reply"),
        }
    }
    fn preview_io_call(
        &self,
        request: &crate::catalog_session::preview_io::Request,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::preview_io::Reply> {
        match self.execute(Operation::PreviewIo(request.clone()), cancel)? {
            Response::PreviewIo(reply) => {
                reply.validate(request)?;
                Ok(reply)
            }
            _ => anyhow::bail!("unexpected cache IO response"),
        }
    }

    fn preview_store_call(
        &self,
        request: &crate::catalog_session::store::Request,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::store::Reply> {
        match self.execute(Operation::PreviewStore(request.clone()), cancel)? {
            Response::PreviewStore(value) => {
                crate::catalog_session::store::validate_reply(request, &value)?;
                Ok(value)
            }
            _ => anyhow::bail!("unexpected preview custody response"),
        }
    }
    fn preview_store_status(
        &self,
        query: &crate::catalog_session::store::Query,
    ) -> Result<crate::catalog_session::store::Status> {
        crate::catalog_session::store::path(&query.root.canonical_root)?;
        self.store_status(&crate::catalog_session::store::StatusQuery::from(query))
    }
    fn read_preview_configuration(
        &self,
        path: &NativePath,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        match self.execute(Operation::ReadPreviewConfiguration(path.clone()), cancel)? {
            Response::PreviewConfiguration(value) => {
                ensure!(
                    value.len() <= crate::catalog_session::store::CONFIG_BYTES,
                    "preview configuration reply byte limit"
                );
                Ok(value)
            }
            _ => anyhow::bail!("unexpected preview configuration response"),
        }
    }

    fn prepare_catalog(
        &self,
        request: &PrepareCatalog,
        cancel: &AtomicBool,
    ) -> Result<CatalogBootstrap> {
        match self.execute(Operation::PrepareCatalog(request.clone()), cancel)? {
            Response::Bootstrap(value) => {
                value.validate()?;
                ensure!(
                    value.operation == request.operation
                        && value.session == request.session
                        && value.epoch == self.epoch,
                    "filesystem prepare response authority mismatch"
                );
                Ok(value)
            }
            _ => anyhow::bail!("unexpected filesystem prepare response"),
        }
    }
    fn abandon_prepare(&self, operation: U64, session: &LeaseId) -> Result<()> {
        unit(self.execute(
            Operation::AbandonPrepare {
                operation,
                session: session.clone(),
            },
            &AtomicBool::new(false),
        )?)
    }
    fn confirm_sql_admission(
        &self,
        request: &ConfirmSqlAdmission,
        cancel: &AtomicBool,
    ) -> Result<SqlAdmissionConfirmed> {
        match self.execute(Operation::ConfirmSqlAdmission(request.clone()), cancel)? {
            Response::Confirmed(value) => {
                ensure!(
                    value == *request,
                    "filesystem confirmation authority mismatch"
                );
                Ok(value)
            }
            _ => anyhow::bail!("unexpected filesystem confirmation response"),
        }
    }
    fn restore_status(&self, root: &RootCapability) -> Result<Option<RestoreStatus>> {
        restore(self.execute(
            Operation::RestoreStatus { root: root.clone() },
            &AtomicBool::new(false),
        )?)
    }
    fn resume_restored_jobs(
        &self,
        root: &RootCapability,
        restore_id: &str,
        acknowledge_pending_jobs: bool,
    ) -> Result<RestoreStatus> {
        restore(self.execute(
            Operation::ResumeRestoredJobs {
                root: root.clone(),
                restore_id: restore_id.into(),
                acknowledge_pending_jobs,
            },
            &AtomicBool::new(false),
        )?)?
        .ok_or_else(|| anyhow::anyhow!("filesystem release returned no restore receipt"))
    }
    fn release_root(&self, root: &RootCapability) -> Result<()> {
        unit(self.execute(
            Operation::ReleaseRoot { root: root.clone() },
            &AtomicBool::new(false),
        )?)
    }
}
fn unit(value: Response) -> Result<()> {
    ensure!(
        matches!(value, Response::Unit(_) | Response::Released(_)),
        "unexpected filesystem unit response"
    );
    Ok(())
}
fn restore(value: Response) -> Result<Option<RestoreStatus>> {
    match value {
        Response::RestoreStatus(value) => Ok(value),
        _ => anyhow::bail!("unexpected filesystem restore response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem_worker::process::{Handler, OperationContext};
    use std::{fs, io, path::PathBuf};

    // No child or I/O thread exists in these transport-state fixtures.
    fn bare() -> Result<Client> {
        let startup = Startup::new(vec![])?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                status: Status {
                    phase: Phase::Starting,
                    shutdown_attempt: U64(0),
                    active: None,
                    queued: None,
                    retained: None,
                    canceled_before_execution: None,
                    error: None,
                },
                outgoing: Outbound {
                    command: None,
                    command_cleanup: false,
                    cancel: None,
                    ack: None,
                    admission: None,
                    store: None,
                    status: None,
                    stop: None,
                    close: false,
                },
                sequence: 0,
                waiting: None,
                result: None,
                admission: None,
                store: None,
                store_sequence: 0,
                store_waiting: None,
                failure: None,
                reaped: false,
                io_drained: false,
                closing: false,
                completed: None,
                calls: 0,
                shutdown_requested: 0,
                startup_failed: false,
            }),
            wake: Condvar::new(),
            nonce: startup.nonce(),
        });
        shared.state.lock().unwrap().status.phase = Phase::Ready;
        Ok(Client {
            shared,
            owner: Mutex::new(Owner {
                child: None,
                threads: vec![],
                faults: Faults::default(),
            }),
            calls: Mutex::new(()),
            admission_calls: Mutex::new(()),
            store_calls: Mutex::new(()),
            shutdown: Mutex::new(()),
            epoch: startup.epoch,
            pid: 0,
        })
    }
    fn store_query(client: &Client) -> Result<crate::catalog_session::store::StatusQuery> {
        let file = tempfile::tempfile()?;
        let physical = crate::catalog_storage::physical_object_id(&file)?;
        Ok(crate::catalog_session::store::StatusQuery {
            kind: crate::catalog_session::store::StatusKind::Locks,
            epoch: client.epoch.clone(),
            token: LeaseId::new(),
            session: LeaseId::new(),
            root_physical: physical,
            catalog_physical: physical,
            operation: U64(7),
            selected: None,
        })
    }
    fn control_bytes(
        client: &Client,
        sequence: u64,
        value: Control,
        stopped: bool,
    ) -> Result<Vec<u8>> {
        let mut bytes = vec![];
        Message {
            kind: Kind::Control,
            sequence,
            bytes: encode(&value, MESSAGE_BYTES)?,
        }
        .write(client.shared.nonce, &mut bytes)?;
        if stopped {
            let mut status = client.shared.state.lock().unwrap().status.clone();
            status.phase = Phase::Stopped;
            Message {
                kind: Kind::Control,
                sequence: 0,
                bytes: encode(&Control::Status(status), MESSAGE_BYTES)?,
            }
            .write(client.shared.nonce, &mut bytes)?;
        }
        Ok(bytes)
    }
    #[test]
    fn store_control_validates_active_sequence_payload_bounds_and_terminal_delivery() -> Result<()>
    {
        for invalid in 0..3 {
            let client = bare()?;
            let query = store_query(&client)?;
            if invalid != 2 {
                client.shared.state.lock().unwrap().store_waiting = Some((9, query));
            }
            let failure = Failure {
                object_receipt: None,
                kind: FailureKind::Unknown,
                message: "x".repeat(if invalid == 1 { ERROR_BYTES + 1 } else { 1 }),
            };
            let bytes = control_bytes(
                &client,
                if invalid == 0 { 8 } else { 9 },
                Control::Store(Err(failure)),
                false,
            )?;
            let error = read_loop(bytes.as_slice(), true, &client.shared).unwrap_err();
            client.shared.fail(error);
            let state = client.shared.state.lock().unwrap();
            assert!(state.store.is_none() && state.failure.is_some());
        }
        let client = bare()?;
        let query = store_query(&client)?;
        client.shared.state.lock().unwrap().store_waiting = Some((9, query.clone()));
        let status = crate::catalog_session::store::Status {
            kind: crate::catalog_session::store::StatusKind::Locks,
            object: None,
            operation: query.operation,
            group: None,
            stage: None,
            slots: 0,
            owned_bytes: 0,
            selected: None,
        };
        let bytes = control_bytes(&client, 9, Control::Store(Ok(status.clone())), true)?;
        read_loop(bytes.as_slice(), true, &client.shared)?;
        assert_eq!(
            client.store_status(&query)?,
            status,
            "received reply must win over subsequent Stop"
        );
        assert!(
            client.store_status(&query).is_err(),
            "terminal writer must not accept a new query before reap"
        );
        Ok(())
    }
    #[test]
    fn store_query_waiter_settles_on_clean_terminal_control_before_reap() -> Result<()> {
        let client = Arc::new(bare()?);
        let query = store_query(&client)?;
        let querying = client.clone();
        let waiter = std::thread::spawn(move || querying.store_status(&query));
        {
            let mut state = client.shared.state.lock().unwrap();
            while state.store_waiting.is_none() {
                state = client.shared.wake.wait(state).unwrap();
            }
        }
        let mut status = client.shared.state.lock().unwrap().status.clone();
        status.phase = Phase::Stopped;
        let bytes = control_bytes(&client, 0, Control::Status(status), false)?;
        read_loop(bytes.as_slice(), true, &client.shared)?;
        assert!(waiter.join().unwrap().is_err());
        assert!(client.shared.state.lock().unwrap().store_waiting.is_none());
        Ok(())
    }
    #[test]
    fn stop_preserves_unsent_cleanup_and_writer_dispatches_it_before_stop() -> Result<()> {
        struct Capture {
            shared: Arc<Shared>,
            bytes: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                let bytes = self.bytes.lock().unwrap();
                let mut input = bytes.as_slice();
                while let Some(frame) = Frame::read(&mut input)? {
                    if frame.kind == Kind::Stop {
                        self.shared.state.lock().unwrap().outgoing.close = true;
                    }
                }
                Ok(())
            }
        }
        struct Cleanup(Arc<AtomicBool>);
        impl Handler for Cleanup {
            fn execute(&mut self, operation: Operation, context: &OperationContext) -> Outcome {
                assert!(operation.is_cleanup());
                assert!(!context.cancellation().load(Ordering::Acquire));
                self.0.store(true, Ordering::Release);
                Ok(Response::Unit(Empty {}))
            }
            fn shutdown(&mut self) -> std::result::Result<(), Failure> {
                Ok(())
            }
        }
        let client = bare()?;
        let operation = Operation::AbandonPrepare {
            operation: U64(1),
            session: LeaseId::new(),
        };
        {
            let mut state = client.shared.state.lock().unwrap();
            state.outgoing.command_cleanup = operation.is_cleanup();
            state.outgoing.command = Some(Message {
                kind: Kind::Execute,
                sequence: 1,
                bytes: encode(&operation, MESSAGE_BYTES)?,
            });
        }
        client.signal_stop();
        assert!(
            client
                .shared
                .state
                .lock()
                .unwrap()
                .outgoing
                .command
                .is_some()
        );
        assert!(client.shared.state.lock().unwrap().result.is_none());
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let startup = Startup {
            build: build_identity(),
            epoch: client.epoch.clone(),
            original_roots: vec![],
        };
        write_loop(
            Capture {
                shared: client.shared.clone(),
                bytes: bytes.clone(),
            },
            encode(&startup, CONFIG_BYTES)?,
            &client.shared,
        )?;
        let dispatched = Arc::new(AtomicBool::new(false));
        let observed = dispatched.clone();
        let input = bytes.lock().unwrap().clone();
        let mut frames = input.as_slice();
        let mut kinds = Vec::new();
        while let Some(frame) = Frame::read(&mut frames)? {
            kinds.push(frame.kind);
        }
        assert_eq!(kinds, vec![Kind::Startup, Kind::Execute, Kind::Stop]);
        // Hold execution input until the real F Ready control, like Client does.
        struct ReadyInput {
            input: io::Cursor<Vec<u8>>,
            boundary: u64,
            ready: Arc<(Mutex<bool>, Condvar)>,
        }
        impl Read for ReadyInput {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.input.position() >= self.boundary {
                    let (lock, wake) = &*self.ready;
                    let mut ready = lock.lock().unwrap();
                    while !*ready {
                        ready = wake.wait(ready).unwrap();
                    }
                }
                self.input.read(output)
            }
        }
        struct ReadyOutput {
            bytes: Vec<u8>,
            ready: Arc<(Mutex<bool>, Condvar)>,
        }
        impl Write for ReadyOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                let mut input = self.bytes.as_slice();
                while let Some(frame) = Frame::read(&mut input)? {
                    if let Control::Status(status) =
                        decode::<Control>(&frame.payload, MESSAGE_BYTES)
                            .map_err(io::Error::other)?
                        && status.phase == Phase::Ready
                    {
                        *self.ready.0.lock().unwrap() = true;
                        self.ready.1.notify_all();
                    }
                }
                Ok(())
            }
        }
        let mut cursor = io::Cursor::new(input);
        Frame::read(&mut cursor)?;
        let boundary = cursor.position();
        cursor.set_position(0);
        let ready = Arc::new((Mutex::new(false), Condvar::new()));
        crate::filesystem_worker::process::serve(
            ReadyInput {
                input: cursor,
                boundary,
                ready: ready.clone(),
            },
            io::sink(),
            ReadyOutput {
                bytes: vec![],
                ready,
            },
            move |_| Ok(Cleanup(observed)),
        )?;
        assert!(dispatched.load(Ordering::Acquire));
        Ok(())
    }

    const CHILD: &str = "filesystem_worker::client::tests::owned_child_entrypoint";
    const ROLE: &str = "PHOTOCATALOG_F_OWNERSHIP_FIXTURE";
    const DIRECTORY: &str = "PHOTOCATALOG_F_OWNERSHIP_DIRECTORY";

    // Rust's test harness emits a short banner on stdout before entering its
    // selected test. Only this test connector strips that bounded banner; the
    // production client and configured CLI require PCFS at byte zero.
    struct HarnessOutput<R> {
        inner: R,
        prefix: Vec<u8>,
        ready: bool,
        copied: usize,
    }
    impl<R: Read> Read for HarnessOutput<R> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if output.is_empty() {
                return Ok(0);
            }
            while !self.ready {
                let mut byte = [0];
                if self.inner.read(&mut byte)? == 0 {
                    return Ok(0);
                }
                self.prefix.push(byte[0]);
                if self.prefix.ends_with(b"PCFS") {
                    self.ready = true;
                } else if self.prefix.len() == 512 {
                    return Err(io::Error::other(
                        "fixture harness prefix exceeded 512 bytes",
                    ));
                }
            }
            if self.copied < 4 {
                let size = output.len().min(4 - self.copied);
                output[..size].copy_from_slice(&b"PCFS"[self.copied..self.copied + size]);
                self.copied += size;
                return Ok(size);
            }
            self.inner.read(output)
        }
    }
    struct NoDependents(Client);
    impl Drop for NoDependents {
        fn drop(&mut self) {
            // These fixtures never create a C/SQL/native owner. Even assertion
            // failure must retire the exact child, not leave a running helper.
            // Poison here records a test panic, not an additional dependent;
            // recover only for this fixture's explicit process retirement.
            self.0.owner.clear_poison();
            self.0.shared.state.clear_poison();
            self.0.shutdown.clear_poison();
            {
                let mut owner = self.0.owner.lock().unwrap_or_else(|p| p.into_inner());
                owner.faults.try_wait = false;
                owner.faults.wait = false;
                if owner.child.is_none() && owner.threads.is_empty() {
                    return;
                }
            }
            self.0
                .terminate_after_dependents_drained()
                .expect("fixture child retirement");
        }
    }
    fn spawn_fixture(role: &str, directory: &Path, faults: Faults) -> Result<NoDependents> {
        let startup = Startup::new(vec![])?;
        let hello = encode(&startup, CONFIG_BYTES)?;
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                CHILD,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROLE, role)
            .env(DIRECTORY, directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        eprintln!("F ownership fixture spawned pid={pid} role={role}");
        let input = child.stdin.take().unwrap();
        let output = HarnessOutput {
            inner: child.stdout.take().unwrap(),
            prefix: Vec::with_capacity(512),
            ready: false,
            copied: 0,
        };
        let control = child.stderr.take().unwrap();
        Ok(NoDependents(Client::connect(
            startup,
            hello,
            Owner {
                child: Some(child),
                threads: vec![],
                faults,
            },
            (input, output, control),
            pid,
        )))
    }
    fn has_child(client: &Client) -> bool {
        client.owner.lock().unwrap().child.is_some()
    }
    fn was_reaped(client: &Client) -> bool {
        client.shared.state.lock().unwrap().reaped
    }
    fn assert_retired(client: &Client) {
        let empty = {
            let owner = client.owner.lock().unwrap();
            owner.child.is_none() && owner.threads.is_empty()
        };
        assert!(empty);
        let drained = {
            let state = client.shared.state.lock().unwrap();
            state.reaped && state.io_drained
        };
        assert!(drained);
        #[cfg(unix)]
        {
            assert_eq!(unsafe { libc::kill(client.pid() as libc::pid_t, 0) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        }
        eprintln!("F ownership fixture reaped pid={}", client.pid());
    }
    fn assert_running(client: &Client) {
        let observed = {
            let mut owner = client.owner.lock().unwrap();
            owner
                .child
                .as_mut()
                .map(|child| (child.id(), child.try_wait()))
        };
        let (pid, exited) = observed.expect("same owned child retained");
        assert_eq!(pid, client.pid());
        assert!(exited.unwrap().is_none());
    }

    #[test]
    fn post_spawn_thread_failure_returns_owned_unexposed_client() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let child = spawn_fixture(
            "wait",
            temp.path(),
            Faults {
                fail_thread_at: Some(1),
                ..Faults::default()
            },
        )?;
        assert_eq!(child.0.status().phase, Phase::Unknown);
        let observation = {
            let state = child.0.shared.state.lock().unwrap();
            (state.startup_failed, state.sequence, state.waiting)
        };
        assert_eq!(observation, (true, 0, None));
        assert!(has_child(&child.0));
        assert!(
            child
                .0
                .execute(
                    Operation::GlobalRestoreStatus {
                        root: NativePath::from_path(temp.path()),
                    },
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        child.0.retire_failed_startup()?;
        assert_retired(&child.0);
        Ok(())
    }

    #[test]
    fn failed_owned_wait_keeps_same_child_and_retry_joins_every_owner() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let child = spawn_fixture("wait", temp.path(), Faults::default())?;
        child.0.owner.lock().unwrap().faults.try_wait = true;
        assert!(
            child
                .0
                .try_shutdown()
                .unwrap_err()
                .to_string()
                .contains("try_wait")
        );
        assert_running(&child.0);
        assert!(!was_reaped(&child.0));
        child.0.owner.lock().unwrap().faults.wait = true;
        assert!(
            child
                .0
                .terminate_after_dependents_drained()
                .unwrap_err()
                .to_string()
                .contains("wait failure")
        );
        let observation = {
            let owner = child.0.owner.lock().unwrap();
            (owner.child.as_ref().map(Child::id), owner.threads.len())
        };
        assert_eq!(observation, (Some(child.0.pid()), 3));
        assert!(!was_reaped(&child.0));
        assert_ne!(child.0.status().phase, Phase::Stopped);
        let joined = Arc::new(AtomicBool::new(false));
        let panic_owner = thread::Builder::new().spawn(|| panic!("injected I/O owner panic"))?;
        child.0.owner.lock().unwrap().threads.push(panic_owner);
        let finished = joined.clone();
        let last_owner =
            thread::Builder::new().spawn(move || finished.store(true, Ordering::Release))?;
        child.0.owner.lock().unwrap().threads.push(last_owner);
        child.0.terminate_after_dependents_drained()?;
        assert_retired(&child.0);
        assert!(joined.load(Ordering::Acquire));
        assert_eq!(child.0.status().phase, Phase::Unknown); // retirement, not success
        Ok(())
    }

    struct PanicHandler {
        directory: PathBuf,
        held: Option<fs::File>,
    }
    impl Drop for PanicHandler {
        fn drop(&mut self) {
            if self.held.is_some() {
                fs::write(self.directory.join("handler-dropped"), b"released early").unwrap();
            }
        }
    }
    impl Handler for PanicHandler {
        fn execute(&mut self, _: Operation, _: &OperationContext) -> Outcome {
            let held = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(self.directory.join("held.lock"))
                .unwrap();
            fs2::FileExt::try_lock_exclusive(&held).unwrap();
            self.held = Some(held);
            panic!("injected Handler panic while an admission object is retained");
        }
        fn shutdown(&mut self) -> std::result::Result<(), Failure> {
            assert!(
                self.held.is_none(),
                "poisoned Handler must never be called again"
            );
            Ok(())
        }
    }

    #[test]
    fn panicked_handler_keeps_process_and_lock_until_explicit_retirement() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let child = spawn_fixture("panic", temp.path(), Faults::default())?;
        assert!(
            child
                .0
                .execute(
                    Operation::GlobalRestoreStatus {
                        root: NativePath::from_path(temp.path()),
                    },
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert_eq!(child.0.status().phase, Phase::Unknown);
        assert_running(&child.0);
        assert!(!temp.path().join("handler-dropped").exists());
        let contender = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp.path().join("held.lock"))?;
        assert!(fs2::FileExt::try_lock_exclusive(&contender).is_err());
        assert!(child.0.try_shutdown().is_err());
        assert_running(&child.0);
        assert!(fs2::FileExt::try_lock_exclusive(&contender).is_err());
        // The fixture has now explicitly established no C/SQL/native dependents;
        // failed checked Stop did not itself authorize or perform this kill.
        child.0.terminate_after_dependents_drained()?;
        assert_retired(&child.0);
        fs2::FileExt::try_lock_exclusive(&contender)?;
        fs2::FileExt::unlock(&contender)?;
        assert!(!temp.path().join("handler-dropped").exists());
        Ok(())
    }

    #[test]
    #[ignore = "owned F fixture subprocess entrypoint; inert in normal harness"]
    fn owned_child_entrypoint() -> Result<()> {
        let Some(role) = std::env::var_os(ROLE) else {
            return Ok(());
        };
        std::panic::set_hook(Box::new(|_| {}));
        if role == "wait" {
            io::copy(&mut io::stdin().lock(), &mut io::sink())?;
            return Ok(());
        }
        ensure!(role == "panic", "unknown owned F fixture role");
        let directory = PathBuf::from(std::env::var_os(DIRECTORY).expect("fixture directory"));
        crate::filesystem_worker::process::serve(
            io::stdin().lock(),
            io::stdout(),
            io::stderr(),
            move |_| {
                Ok(PanicHandler {
                    directory,
                    held: None,
                })
            },
        )
    }
}
