//! The supervisor owns this object. It never opens a source, destination, or
//! lock pathname; only the configured executable and anonymous byte pipes.
use super::protocol::{ChildFrame, read_frame_optional};
use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Fresh Source processes use framed diagnostics without inherited library
/// backtrace allocations. Preserve RUST_BACKTRACE for the caller's panic policy.
pub(crate) fn source_environment(command: &mut Command) {
    command.env("RUST_LIB_BACKTRACE", "0");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    LightroomMigration,
    SourceSql,
    SourceRaw,
}
impl Role {
    pub(crate) fn argument(self) -> &'static str {
        match self {
            Self::LightroomMigration => "--lightroom-migration-worker",
            Self::SourceSql => "--lightroom-source-reader-sql",
            Self::SourceRaw => "--lightroom-source-reader-raw",
        }
    }
    fn source(self) -> bool {
        matches!(self, Self::SourceSql | Self::SourceRaw)
    }
}

/// At most one encoded input and one decoded output are queued, in addition to
/// the frame currently owned by each I/O thread. Joining is supervisor-only.
pub(crate) struct Process<T = ChildFrame> {
    child: Child,
    input: Option<SyncSender<Vec<u8>>>,
    output: Option<Receiver<Result<Option<T>>>>,
    control: Option<SyncSender<Vec<u8>>>,
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    input_error: Arc<Mutex<Option<String>>>,
    reaped: Option<ExitStatus>,
    io_panicked: bool,
    transport: Arc<TransportHealth>,
    #[cfg(test)]
    injected_input: Arc<Mutex<Option<Vec<u8>>>>,
    #[cfg(test)]
    injected_wait_failures: usize,
    #[cfg(test)]
    startup_order_gate: Arc<Mutex<Option<Arc<StartupOrderGate>>>>,
    #[cfg(test)]
    injected_revoke_failure: InjectedRevokeFailure,
    #[cfg(test)]
    checked_drain: Arc<AtomicBool>,
}
#[cfg(test)]
#[derive(Clone, Copy, Default)]
enum InjectedRevokeFailure {
    #[default]
    None,
    Before,
    After,
}
/// Forces the writer's control-poll/data-receive window without changing its
/// production queue order or holding another encoded frame. All waits are
/// bounded so a failed assertion cannot strand an owned I/O thread.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct StartupOrderGate {
    state: Mutex<(usize, bool, bool)>, // successful sends, waiting, released
    changed: std::sync::Condvar,
}
#[cfg(test)]
impl StartupOrderGate {
    fn before_send(&self) {
        let state = self.state.lock().unwrap();
        if state.0 == 1 {
            let (state, _) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(10), |state| !state.1)
                .unwrap();
            assert!(state.1, "writer did not enter data receive window");
        }
    }
    fn sent(&self, urgent: bool) {
        let mut state = self.state.lock().unwrap();
        let previous_sends = state.0;
        state.0 += 1;
        if !urgent && previous_sends >= 1 && state.1 {
            state.2 = true;
            self.changed.notify_all();
        }
    }
    fn before_data_receive(&self, writes: usize) {
        if writes != 1 {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.1 {
            return;
        }
        state.1 = true;
        self.changed.notify_all();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(10), |state| !state.2)
            .unwrap();
        assert!(state.2, "startup sender did not queue its next data frame");
    }
    pub(crate) fn exercised(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.1 && state.2
    }
}

#[derive(Default)]
struct TransportHealth {
    failed: AtomicBool,
    eof: AtomicBool,
}
struct IoCompletion {
    health: Arc<TransportHealth>,
    completed: bool,
}
impl Drop for IoCompletion {
    fn drop(&mut self) {
        if !self.completed {
            self.health.failed.store(true, Ordering::Release);
        }
    }
}
/// A failure after `Command::spawn` retains the partial Child, pipes and any I/O
/// thread which was already created. Managed callers move this into their
/// operation owner; legacy callers explicitly drain it before returning.
pub(crate) struct SpawnFailure<T = ChildFrame> {
    pub(crate) error: anyhow::Error,
    pub(crate) process: Option<Process<T>>,
}
impl<T> SpawnFailure<T> {
    fn before_child(error: anyhow::Error) -> Self {
        Self {
            error,
            process: None,
        }
    }
    fn after_child(error: anyhow::Error, process: Process<T>) -> Self {
        Self {
            error,
            process: Some(process),
        }
    }
    fn drain_legacy(mut self, before_wait: Option<&mut dyn FnMut()>) -> anyhow::Error {
        if let Some(process) = &mut self.process {
            process.revoke();
            if let Some(before_wait) = before_wait {
                before_wait();
            }
            process.terminate();
        }
        self.error
    }
}
#[derive(Default)]
pub(crate) struct Stop {
    requested: AtomicBool,
    admission: AtomicBool,
}
impl Stop {
    pub(crate) fn cancel(&self) {
        self.requested.store(true, Ordering::Release);
        self.admission.store(true, Ordering::Release);
    }
    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
    pub(crate) fn admission(&self) -> &AtomicBool {
        &self.admission
    }
}
pub(crate) enum Output<T = ChildFrame> {
    Pending,
    Frame(T),
    End,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct DrainReport {
    pub(crate) status: ExitStatus,
    /// Joining a panicked thread proves termination; retain this as poison
    /// without treating the already-consumed handle as retryable.
    pub(crate) io_panicked: bool,
}
impl<T: serde::de::DeserializeOwned + Send + 'static> Process<T> {
    /// Application-owned channel, Arc and erased pipe backing on this target.
    /// OS/process creation, inherited environment and runtime thread/stack
    /// initialization remain explicit baseline owners outside this allowance.
    pub(crate) fn allocation_backing() -> Result<usize> {
        use super::memory::{channels, layout::add};
        use std::alloc::Layout;
        let mut bytes = channels::process::<T>()?;
        for layout in [
            Layout::new::<Mutex<Option<String>>>(),
            Layout::new::<TransportHealth>(),
            Layout::new::<Stop>(),
        ] {
            bytes = add(bytes, channels::arc(layout)?)?;
        }
        bytes = add(bytes, channels::pthread_mutexes(2)?)?;
        add(bytes, std::mem::size_of::<std::process::ChildStdout>())
    }
    #[expect(
        clippy::result_large_err,
        reason = "a failed spawn must retain the complete process owner until checked drain"
    )]
    pub(crate) fn spawn_owned(
        executable: &Path,
        stop: Arc<Stop>,
    ) -> std::result::Result<Self, SpawnFailure<T>> {
        if !executable.is_absolute() {
            return Err(SpawnFailure::before_child(anyhow::anyhow!(
                "absolute configured migration executable required"
            )));
        }
        Self::spawn_role_owned(executable, Role::LightroomMigration, stop)
    }
    #[expect(
        clippy::result_large_err,
        reason = "a failed spawn must retain the complete process owner until checked drain"
    )]
    fn spawn_role_owned(
        executable: &Path,
        role: Role,
        stop: Arc<Stop>,
    ) -> std::result::Result<Self, SpawnFailure<T>> {
        if !executable.is_absolute() {
            return Err(SpawnFailure::before_child(anyhow::anyhow!(
                "absolute configured worker required"
            )));
        }
        let mut command = Command::new(executable);
        command.arg(role.argument());
        if role.source() {
            source_environment(&mut command);
        }
        Self::spawn_command_owned(command, stop)
    }
    pub(crate) fn spawn_role_with_cleanup(
        executable: &Path,
        role: Role,
        stop: Arc<Stop>,
        before_wait: Option<&mut dyn FnMut()>,
    ) -> Result<Self> {
        Self::spawn_role_owned(executable, role, stop)
            .map_err(|failure| failure.drain_legacy(before_wait))
    }
    #[expect(
        clippy::result_large_err,
        reason = "a failed spawn must retain the complete process owner until checked drain"
    )]
    fn spawn_command_owned(
        mut command: Command,
        stop: Arc<Stop>,
    ) -> std::result::Result<Self, SpawnFailure<T>> {
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        Self::spawn_configured_owned(command, stop, |child| {
            Ok(Box::new(
                child.stdout.take().context("migration output pipe")?,
            ))
        })
    }
    #[cfg(test)]
    pub(crate) fn spawn_test_command(command: Command, stop: Arc<Stop>) -> Result<Self> {
        Self::spawn_test_command_with_cleanup(command, stop, None)
    }
    #[cfg(test)]
    #[expect(
        clippy::result_large_err,
        reason = "the failure fixture verifies custody of a partially spawned process"
    )]
    pub(crate) fn spawn_test_command_owned(
        mut command: Command,
        stop: Arc<Stop>,
    ) -> std::result::Result<Self, SpawnFailure<T>> {
        #[cfg(not(feature = "internal-capacity-probes"))]
        command.stdout(Stdio::null());
        #[cfg(feature = "internal-capacity-probes")]
        command.stdout(Stdio::inherit());
        command.stderr(Stdio::piped());
        Self::spawn_configured_owned(command, stop, |child| {
            Ok(Box::new(
                child.stderr.take().context("test helper output pipe")?,
            ))
        })
    }
    #[cfg(test)]
    pub(crate) fn spawn_test_command_with_cleanup(
        command: Command,
        stop: Arc<Stop>,
        before_wait: Option<&mut dyn FnMut()>,
    ) -> Result<Self> {
        Self::spawn_test_command_owned(command, stop)
            .map_err(|failure| failure.drain_legacy(before_wait))
    }
    #[expect(
        clippy::result_large_err,
        reason = "post-spawn failures return the live process owner for mandatory drain"
    )]
    fn spawn_configured_owned(
        mut command: Command,
        stop: Arc<Stop>,
        output: impl FnOnce(&mut Child) -> Result<Box<dyn Read + Send>>,
    ) -> std::result::Result<Self, SpawnFailure<T>> {
        let child = command
            .stdin(Stdio::piped())
            .spawn()
            .context("start owned migration helper")
            .map_err(SpawnFailure::before_child)?;
        // Establish the kill/reap owner before any fallible thread creation.
        let mut owner = Self {
            child,
            input: None,
            output: None,
            control: None,
            writer: None,
            reader: None,
            input_error: Arc::new(Mutex::new(None)),
            reaped: None,
            io_panicked: false,
            transport: Arc::new(TransportHealth::default()),
            #[cfg(test)]
            injected_input: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            injected_wait_failures: 0,
            #[cfg(test)]
            startup_order_gate: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            injected_revoke_failure: InjectedRevokeFailure::None,
            #[cfg(test)]
            checked_drain: Arc::new(AtomicBool::new(false)),
        };
        let configured = (|| -> Result<()> {
            let mut stdin = owner.child.stdin.take().context("migration input pipe")?;
            let mut stdout = output(&mut owner.child)?;
            let (input, incoming) = mpsc::sync_channel::<Vec<u8>>(1);
            let (urgent, controls) = mpsc::sync_channel::<Vec<u8>>(1);
            let (outgoing, output) = mpsc::sync_channel(1);
            owner.input = Some(input);
            owner.control = Some(urgent);
            owner.output = Some(output);
            let errors = owner.input_error.clone();
            let write_stop = stop.clone();
            let write_health = owner.transport.clone();
            let read_health = owner.transport.clone();
            #[cfg(test)]
            let injected = owner.injected_input.clone();
            #[cfg(test)]
            let startup_order_gate = owner.startup_order_gate.clone();
            owner.writer = Some(
                thread::Builder::new()
                    .name("migration-input".into())
                    .spawn(move || {
                        let mut completion = IoCompletion {
                            health: write_health,
                            completed: false,
                        };
                        #[cfg(test)]
                        let mut writes = 0usize;
                        loop {
                            #[cfg(test)]
                            let injected_frame =
                                injected.lock().unwrap_or_else(|e| e.into_inner()).take();
                            #[cfg(not(test))]
                            let injected_frame: Option<Vec<u8>> = None;
                            let frame = if let Some(frame) = injected_frame {
                                frame
                            } else {
                                match controls.try_recv() {
                                    Ok(frame) => frame,
                                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                                        #[cfg(test)]
                                        {
                                            let gate = startup_order_gate.lock().unwrap().clone();
                                            if let Some(gate) = gate {
                                                gate.before_data_receive(writes);
                                            }
                                        }
                                        match incoming.recv_timeout(Duration::from_millis(2)) {
                                            Ok(frame) => frame,
                                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                        }
                                    }
                                }
                            };
                            let result = stdin.write_all(&frame).and_then(|_| stdin.flush());
                            #[cfg(test)]
                            {
                                writes += usize::from(result.is_ok());
                            }
                            if let Err(error) = result {
                                completion.health.failed.store(true, Ordering::Release);
                                *errors.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(error.to_string());
                                write_stop.admission.store(true, Ordering::Release);
                                break;
                            }
                        }
                        completion.completed = true;
                        // Closing this pipe revokes helper ownership even if the GUI died
                        // before it could send an explicit cancellation frame.
                    })?,
            );
            owner.reader = Some(
                thread::Builder::new()
                    .name("migration-output".into())
                    .spawn(move || {
                        let mut completion = IoCompletion {
                            health: read_health,
                            completed: false,
                        };
                        loop {
                            let frame = read_frame_optional::<T>(&mut stdout);
                            match &frame {
                                Err(_) => completion.health.failed.store(true, Ordering::Release),
                                Ok(None) => completion.health.eof.store(true, Ordering::Release),
                                Ok(Some(_)) => {}
                            }
                            #[cfg(all(test, feature = "internal-capacity-probes"))]
                            crate::capacity_probes::observe(
                                crate::capacity_probes::FRAME_OUTPUT,
                                std::mem::size_of_val(&frame),
                            );
                            let failed = !matches!(&frame, Ok(Some(_)));
                            if failed {
                                stop.admission.store(true, Ordering::Release);
                            }
                            if outgoing.send(frame).is_err() || failed {
                                break;
                            }
                        }
                        completion.completed = true;
                    })?,
            );
            Ok(())
        })();
        match configured {
            Ok(()) => Ok(owner),
            Err(error) => Err(SpawnFailure::after_child(error, owner)),
        }
    }
    #[cfg(test)]
    fn spawn_configured(
        command: Command,
        stop: Arc<Stop>,
        before_wait: Option<&mut dyn FnMut()>,
        output: impl FnOnce(&mut Child) -> Result<Box<dyn Read + Send>>,
    ) -> Result<Self> {
        Self::spawn_configured_owned(command, stop, output)
            .map_err(|failure| failure.drain_legacy(before_wait))
    }
    #[cfg(test)]
    pub(crate) fn input_failure_probe(&self) -> Arc<Mutex<Option<Vec<u8>>>> {
        self.injected_input.clone()
    }
    #[cfg(test)]
    pub(crate) fn inject_wait_failures(&mut self, count: usize) {
        self.injected_wait_failures = count;
    }
    #[cfg(test)]
    pub(crate) fn inject_revoke_failure_before_boundary(&mut self) {
        self.injected_revoke_failure = InjectedRevokeFailure::Before;
    }
    #[cfg(test)]
    pub(crate) fn inject_revoke_failure_after_boundary(&mut self) {
        self.injected_revoke_failure = InjectedRevokeFailure::After;
    }
    #[cfg(test)]
    pub(crate) fn checked_drain_probe(&self) -> Arc<AtomicBool> {
        self.checked_drain.clone()
    }
    #[cfg(test)]
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Encode with the whole-frame bound before queuing. On a full queue the
    /// caller retains the exact frame and may retry after checking cancel/time.
    pub(crate) fn try_send<F: serde::Serialize>(&self, frame: F) -> Result<Option<F>> {
        self.try_send_to(frame, false)
    }
    /// Reserved control slot, serviced before another normal data frame. A
    /// currently executing pipe write is still owned and joined during drain.
    pub(crate) fn try_send_control<F: serde::Serialize>(&self, frame: F) -> Result<Option<F>> {
        self.try_send_to(frame, true)
    }
    #[cfg(test)]
    pub(crate) fn force_startup_data_receive_window(&self) -> Arc<StartupOrderGate> {
        let gate = Arc::new(StartupOrderGate::default());
        *self.startup_order_gate.lock().unwrap() = Some(gate.clone());
        gate
    }
    fn try_send_to<F: serde::Serialize>(&self, frame: F, urgent: bool) -> Result<Option<F>> {
        #[cfg(test)]
        let gate = self.startup_order_gate.lock().unwrap().clone();
        #[cfg(test)]
        if let Some(gate) = &gate {
            gate.before_send();
        }
        if let Some(error) = self
            .input_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            anyhow::bail!("migration input failed: {error}");
        }
        let mut bytes = Vec::new();
        super::protocol::write_frame(&mut bytes, &frame)?;
        #[cfg(all(test, feature = "internal-capacity-probes"))]
        crate::capacity_probes::observe(crate::capacity_probes::FRAME_INPUT, bytes.capacity());

        match (if urgent { &self.control } else { &self.input })
            .as_ref()
            .context("migration input closed")?
            .try_send(bytes)
        {
            Ok(()) => {
                #[cfg(test)]
                if let Some(gate) = &gate {
                    gate.sent(urgent);
                }
                Ok(None)
            }
            Err(TrySendError::Full(_)) => Ok(Some(frame)),
            Err(TrySendError::Disconnected(_)) => anyhow::bail!("migration input disconnected"),
        }
    }
    pub(crate) fn try_receive(&self) -> Result<Output<T>> {
        match self
            .output
            .as_ref()
            .context("migration output closed")?
            .try_recv()
        {
            Ok(frame) => frame.map(|f| f.map_or(Output::End, Output::Frame)),
            Err(TryRecvError::Empty) => Ok(Output::Pending),
            Err(TryRecvError::Disconnected) => anyhow::bail!("migration output disconnected"),
        }
    }
    /// Independent atomics are published before a blocking output queue send.
    /// EOF is separate because an orderly Retire may close the pipe normally.
    pub(crate) fn transport_failed(&self) -> bool {
        self.transport.failed.load(Ordering::Acquire)
    }
    pub(crate) fn output_ended(&self) -> bool {
        self.transport.eof.load(Ordering::Acquire)
    }
    pub(crate) fn try_reap(&mut self) -> Result<Option<ExitStatus>> {
        if self.reaped.is_none() {
            self.reaped = self.child.try_wait()?;
        }
        Ok(self.reaped)
    }
}
impl<T> Process<T> {
    #[cfg(all(test, unix))]
    pub(crate) fn inject_io_panic_report(&mut self) {
        self.io_panicked = true;
    }
    /// Nonblocking first pass over every owned child. Call this for the executor
    /// and both Sources before waiting for any one child; a failed first kill
    /// must not postpone revoking the other owners.
    pub(crate) fn revoke(&mut self) {
        #[cfg(test)]
        if matches!(self.injected_revoke_failure, InjectedRevokeFailure::Before) {
            self.transport.failed.store(true, Ordering::Release);
        }
        #[cfg(test)]
        if matches!(self.injected_revoke_failure, InjectedRevokeFailure::After) {
            self.transport.failed.store(true, Ordering::Release);
        }
        self.input.take();
        self.control.take();
        self.output.take();
        if self.reaped.is_none() {
            let _ = self.child.kill();
        }
    }
    /// Checked I/O completion for managed relays. A join panic is reported as an
    /// operation failure; joining still proves that thread has terminated.
    pub(crate) fn drain_checked(&mut self) -> Result<()> {
        let report = loop {
            match self.retry_drain() {
                Ok(Some(report)) => break report,
                Ok(None) | Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        };
        ensure!(
            !report.io_panicked,
            "migration I/O thread panicked during owned drain"
        );
        Ok(())
    }
    /// One retryable drain step. A wait error retains Child and every pipe
    /// handle. Join handles are consumed only after confirmed process reap;
    /// their panic result proves the corresponding thread terminated.
    pub(crate) fn retry_drain(&mut self) -> Result<Option<DrainReport>> {
        self.input.take();
        self.control.take();
        self.output.take();
        if self.reaped.is_none() {
            #[cfg(test)]
            if self.injected_wait_failures != 0 {
                self.injected_wait_failures -= 1;
                anyhow::bail!("injected migration child wait failure");
            }
            match self.child.try_wait()? {
                Some(status) => self.reaped = Some(status),
                None => {
                    let _ = self.child.kill();
                    return Ok(None);
                }
            }
        }
        if let Some(writer) = self.writer.take() {
            self.io_panicked |= writer.join().is_err();
        }
        if let Some(reader) = self.reader.take() {
            self.io_panicked |= reader.join().is_err();
        }
        #[cfg(test)]
        self.checked_drain.store(true, Ordering::Release);
        Ok(Some(DrainReport {
            status: self.reaped.expect("confirmed process reap"),
            io_panicked: self.io_panicked,
        }))
    }
    /// Revokes both channels before kill/wait, so a reader blocked on its
    /// bounded output queue cannot obstruct cleanup. Never called on the actor.
    pub(crate) fn terminate(&mut self) {
        loop {
            match self.retry_drain() {
                Ok(Some(_)) => return,
                Ok(None) | Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}
impl<T> Drop for Process<T> {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(all(test, feature = "internal-capacity-probes"))]
#[path = "process_capacity_tests.rs"]
mod capacity_tests;

#[cfg(test)]
mod source_environment_tests {
    use super::*;
    use std::time::Instant;
    const ENV_FIXTURE: &str = "PHOTOCATALOG_SOURCE_ENV_FIXTURE";
    #[test]
    #[ignore = "owned Source environment subprocess entrypoint"]
    fn source_environment_child() -> Result<()> {
        ensure!(
            std::env::var_os(ENV_FIXTURE).is_some(),
            "owned fixture required"
        );
        let result = (
            std::env::var("RUST_LIB_BACKTRACE")?,
            std::env::var("RUST_BACKTRACE")?,
            format!("{:?}", std::backtrace::Backtrace::capture().status()),
        );
        crate::lightroom_migration_worker::protocol::write_frame(&mut std::io::stderr(), &result)?;
        std::process::exit(0);
    }
    #[test]
    fn source_environment_is_child_local_and_preserves_panic_backtrace() -> Result<()> {
        let original = (
            std::env::var_os("RUST_LIB_BACKTRACE"),
            std::env::var_os("RUST_BACKTRACE"),
        );
        let mut command = Command::new(std::env::current_exe()?);
        command.args(["--exact", "lightroom_migration_worker::process::source_environment_tests::source_environment_child", "--ignored", "--nocapture"])
            .env(ENV_FIXTURE, "1").env("RUST_LIB_BACKTRACE", "1").env("RUST_BACKTRACE", "full");
        source_environment(&mut command);
        let mut process = Process::<(String, String, String)>::spawn_test_command(
            command,
            Arc::new(Stop::default()),
        )?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let result = loop {
            ensure!(
                Instant::now() < deadline,
                "Source environment fixture deadline"
            );
            match process.try_receive()? {
                Output::Frame(value) => break value,
                Output::Pending => std::thread::sleep(Duration::from_millis(5)),
                Output::End => anyhow::bail!("Source environment fixture ended without result"),
            }
        };
        let pid = process.child.id();
        while process.try_reap()?.is_none() {
            ensure!(
                Instant::now() < deadline,
                "Source environment child exit deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        process.terminate();
        ensure!(
            process.reaped.is_some_and(|status| status.success()),
            "Source environment child unsuccessful"
        );
        assert_eq!(result, ("0".into(), "full".into(), "Disabled".into()));
        assert_eq!(
            original,
            (
                std::env::var_os("RUST_LIB_BACKTRACE"),
                std::env::var_os("RUST_BACKTRACE")
            )
        );
        println!(
            "SOURCE_ENV child_pid={pid} reaped=true library=0 panic=full captured=Disabled parent_unchanged=true"
        );
        Ok(())
    }
}

#[cfg(test)]
mod managed_setup_tests {
    use super::*;
    #[test]
    fn partial_spawn_revoke_callback_precedes_wait_and_preserves_owner() -> Result<()> {
        let (notice, observed) = mpsc::sync_channel(1);
        let (allow, allowed) = mpsc::sync_channel(1);
        let (pids, pid) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || -> Result<()> {
            let mut command = Command::new(std::env::current_exe()?);
            command.args(["--exact", "lightroom_migration_worker::source_reader::relay::broker::tests::owned_broker_source_fixture", "--nocapture"])
                .env("PHOTOCATALOG_OWNED_BROKER_SOURCE_FIXTURE", "1")
                .stdout(Stdio::null()).stderr(Stdio::null());
            source_environment(&mut command);
            let mut before_wait = || {
                notice.send(()).unwrap();
                allowed.recv_timeout(Duration::from_secs(5)).unwrap();
            };
            let result = Process::<ChildFrame>::spawn_configured(
                command,
                Arc::new(Stop::default()),
                Some(&mut before_wait),
                |child| {
                    pids.send(child.id())?;
                    anyhow::bail!("injected output setup failure after actual Child ownership")
                },
            );
            ensure!(result.is_err(), "partial setup unexpectedly succeeded");
            Ok(())
        });
        // Always release/join the owned setup thread even when its marker fails.
        let notification = observed.recv_timeout(Duration::from_secs(5));
        let held_before_ack = !worker.is_finished();
        let _ = allow.send(());
        let result = worker
            .join()
            .map_err(|_| anyhow::anyhow!("setup fixture panicked"))?;
        notification?;
        assert!(held_before_ack);
        result?;
        let pid = pid.recv_timeout(Duration::from_secs(1))?;
        #[cfg(unix)]
        {
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        println!("PARTIAL_SOURCE_SETUP pid={pid} callback_before_wait=true owner_joined=true");
        Ok(())
    }

    #[test]
    fn lm_supervisor_batch2_partial_spawn_and_wait_error_retain_one_checked_owner() -> Result<()> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", "lightroom_migration_worker::source_reader::relay::broker::tests::owned_broker_source_fixture", "--nocapture"])
            .env("PHOTOCATALOG_OWNED_BROKER_SOURCE_FIXTURE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        source_environment(&mut command);
        let failure = match Process::<ChildFrame>::spawn_configured_owned(
            command,
            Arc::new(Stop::default()),
            |child| anyhow::bail!("injected output setup failure for child {}", child.id()),
        ) {
            Ok(_) => anyhow::bail!("partial setup unexpectedly succeeded"),
            Err(failure) => failure,
        };
        assert!(failure.error.to_string().contains("injected output setup"));
        let mut process = failure.process.context("partial Process owner required")?;
        let pid = process.pid();
        process.writer = Some(thread::spawn(|| panic!("injected completed I/O panic")));
        process.inject_wait_failures(1);
        process.revoke();
        assert!(process.retry_drain().is_err());
        assert!(process.reaped.is_none());
        let until = std::time::Instant::now() + Duration::from_secs(5);
        let report = loop {
            if let Some(report) = process.retry_drain()? {
                break report;
            }
            ensure!(
                std::time::Instant::now() < until,
                "partial owner drain deadline"
            );
            thread::sleep(Duration::from_millis(2));
        };
        assert!(report.io_panicked);
        let repeated = process
            .retry_drain()?
            .context("completed drain remains idempotently observable")?;
        assert_eq!(report.status, repeated.status);
        assert_eq!(report.io_panicked, repeated.io_panicked);
        #[cfg(unix)]
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        println!(
            "LM_PARTIAL_SETUP child_pid={pid} first_wait_error=retained final_reap=true io_panic_terminated=true repeated_join=false"
        );
        Ok(())
    }
}
