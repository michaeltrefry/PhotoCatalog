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
/// During managed startup, revoke the partial child and the other owners before
/// any failure cleanup can enter a blocking wait. Successful startup transfers
/// the complete Process and never invokes this callback.
struct SetupOwner<'a, T> {
    process: Option<Process<T>>,
    before_wait: Option<&'a mut dyn FnMut()>,
}
impl<T> std::ops::Deref for SetupOwner<'_, T> {
    type Target = Process<T>;
    fn deref(&self) -> &Self::Target {
        self.process.as_ref().expect("owned process setup")
    }
}
impl<T> std::ops::DerefMut for SetupOwner<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.process.as_mut().expect("owned process setup")
    }
}
impl<T> Drop for SetupOwner<'_, T> {
    fn drop(&mut self) {
        if let Some(process) = &mut self.process {
            process.revoke();
            if let Some(before_wait) = &mut self.before_wait {
                before_wait();
            }
        }
        // The Process field's Drop now performs the checked ownership wait.
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
impl<T: serde::de::DeserializeOwned + Send + 'static> Process<T> {
    pub(crate) fn spawn(executable: &Path, stop: Arc<Stop>) -> Result<Self> {
        ensure!(
            executable.is_absolute(),
            "absolute configured migration executable required"
        );
        Self::spawn_role(executable, "--lightroom-migration-worker", stop)
    }
    pub(crate) fn spawn_role(
        executable: &Path,
        role: &'static str,
        stop: Arc<Stop>,
    ) -> Result<Self> {
        Self::spawn_role_with_cleanup(executable, role, stop, None)
    }
    pub(crate) fn spawn_role_with_cleanup(
        executable: &Path,
        role: &'static str,
        stop: Arc<Stop>,
        before_wait: Option<&mut dyn FnMut()>,
    ) -> Result<Self> {
        ensure!(
            executable.is_absolute(),
            "absolute configured worker required"
        );
        let mut command = Command::new(executable);
        command.arg(role);
        if matches!(
            role,
            "--lightroom-source-reader"
                | "--lightroom-source-reader-sql"
                | "--lightroom-source-reader-raw"
        ) {
            source_environment(&mut command);
        }
        Self::spawn_command(command, stop, before_wait)
    }
    fn spawn_command(
        mut command: Command,
        stop: Arc<Stop>,
        before_wait: Option<&mut dyn FnMut()>,
    ) -> Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        Self::spawn_configured(command, stop, before_wait, |child| {
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
    pub(crate) fn spawn_test_command_with_cleanup(
        mut command: Command,
        stop: Arc<Stop>,
        before_wait: Option<&mut dyn FnMut()>,
    ) -> Result<Self> {
        // Unit-test harness chatter stays on discarded stdout. The same
        // bounded protocol and process owner read direct helper stderr bytes.
        #[cfg(not(feature = "internal-capacity-probes"))]
        command.stdout(Stdio::null());
        #[cfg(feature = "internal-capacity-probes")]
        command.stdout(Stdio::inherit());
        command.stderr(Stdio::piped());
        Self::spawn_configured(command, stop, before_wait, |child| {
            Ok(Box::new(
                child.stderr.take().context("test helper output pipe")?,
            ))
        })
    }
    fn spawn_configured(
        mut command: Command,
        stop: Arc<Stop>,
        before_wait: Option<&mut dyn FnMut()>,
        output: impl FnOnce(&mut Child) -> Result<Box<dyn Read + Send>>,
    ) -> Result<Self> {
        let child = command
            .stdin(Stdio::piped())
            .spawn()
            .context("start owned migration helper")?;
        // Establish the kill/reap owner before any fallible thread creation.
        let mut owner = SetupOwner {
            process: Some(Self {
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
            }),
            before_wait,
        };
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
        owner.writer = Some(
            thread::Builder::new()
                .name("migration-input".into())
                .spawn(move || {
                    let mut completion = IoCompletion {
                        health: write_health,
                        completed: false,
                    };
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
                                    match incoming.recv_timeout(Duration::from_millis(2)) {
                                        Ok(frame) => frame,
                                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                    }
                                }
                            }
                        };
                        let result = stdin.write_all(&frame).and_then(|_| stdin.flush());
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
        Ok(owner.process.take().expect("complete owned process setup"))
    }
    #[cfg(test)]
    pub(crate) fn input_failure_probe(&self) -> Arc<Mutex<Option<Vec<u8>>>> {
        self.injected_input.clone()
    }
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
    fn try_send_to<F: serde::Serialize>(&self, frame: F, urgent: bool) -> Result<Option<F>> {
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
            Ok(()) => Ok(None),
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
    /// Nonblocking first pass over every owned child. Call this for the executor
    /// and both Sources before waiting for any one child; a failed first kill
    /// must not postpone revoking the other owners.
    pub(crate) fn revoke(&mut self) {
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
        self.terminate();
        ensure!(
            !self.io_panicked,
            "migration I/O thread panicked during owned drain"
        );
        Ok(())
    }
    /// Revokes both channels before kill/wait, so a reader blocked on its
    /// bounded output queue cannot obstruct cleanup. Never called on the actor.
    pub(crate) fn terminate(&mut self) {
        self.input.take();
        self.control.take();
        self.output.take();
        self.reap_before_release();
        if let Some(writer) = self.writer.take() {
            self.io_panicked |= writer.join().is_err();
        }
        if let Some(reader) = self.reader.take() {
            self.io_panicked |= reader.join().is_err();
        }
    }
    fn reap_before_release(&mut self) {
        // Reap errors cannot authorize releasing a parent writer permit. Keep
        // ownership and retry rather than returning an unverified terminal.
        while self.reaped.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.reaped = Some(status);
                break;
            }
            let _ = self.child.kill();
            match self.child.wait() {
                Ok(status) => self.reaped = Some(status),
                Err(_) => thread::sleep(Duration::from_millis(20)),
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
}
