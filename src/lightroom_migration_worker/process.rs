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
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    input_error: Arc<Mutex<Option<String>>>,
    reaped: Option<ExitStatus>,
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
        ensure!(
            executable.is_absolute(),
            "absolute configured worker required"
        );
        let mut command = Command::new(executable);
        command.arg(role);
        if role == "--lightroom-source-reader" {
            source_environment(&mut command);
        }
        Self::spawn_command(command, stop)
    }
    fn spawn_command(mut command: Command, stop: Arc<Stop>) -> Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        Self::spawn_configured(command, stop, |child| {
            Ok(Box::new(
                child.stdout.take().context("migration output pipe")?,
            ))
        })
    }
    #[cfg(test)]
    pub(crate) fn spawn_test_command(mut command: Command, stop: Arc<Stop>) -> Result<Self> {
        // Unit-test harness chatter stays on discarded stdout. The same
        // bounded protocol and process owner read direct helper stderr bytes.
        #[cfg(not(feature = "internal-capacity-probes"))]
        command.stdout(Stdio::null());
        #[cfg(feature = "internal-capacity-probes")]
        command.stdout(Stdio::inherit());
        command.stderr(Stdio::piped());
        Self::spawn_configured(command, stop, |child| {
            Ok(Box::new(
                child.stderr.take().context("test helper output pipe")?,
            ))
        })
    }
    fn spawn_configured(
        mut command: Command,
        stop: Arc<Stop>,
        output: impl FnOnce(&mut Child) -> Result<Box<dyn Read + Send>>,
    ) -> Result<Self> {
        let child = command
            .stdin(Stdio::piped())
            .spawn()
            .context("start owned migration helper")?;
        // Establish the kill/reap owner before any fallible thread creation.
        let mut owner = Self {
            child,
            input: None,
            output: None,
            writer: None,
            reader: None,
            input_error: Arc::new(Mutex::new(None)),
            reaped: None,
        };
        let mut stdin = owner.child.stdin.take().context("migration input pipe")?;
        let mut stdout = output(&mut owner.child)?;
        let (input, incoming) = mpsc::sync_channel::<Vec<u8>>(1);
        let (outgoing, output) = mpsc::sync_channel(1);
        owner.input = Some(input);
        owner.output = Some(output);
        let errors = owner.input_error.clone();
        let write_stop = stop.clone();
        owner.writer = Some(
            thread::Builder::new()
                .name("migration-input".into())
                .spawn(move || {
                    while let Ok(frame) = incoming.recv() {
                        let result = stdin.write_all(&frame).and_then(|_| stdin.flush());
                        if let Err(error) = result {
                            *errors.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(error.to_string());
                            write_stop.admission.store(true, Ordering::Release);
                            break;
                        }
                    }
                    // Closing this pipe revokes helper ownership even if the GUI died
                    // before it could send an explicit cancellation frame.
                })?,
        );
        owner.reader = Some(
            thread::Builder::new()
                .name("migration-output".into())
                .spawn(move || {
                    loop {
                        let frame = read_frame_optional::<T>(&mut stdout);
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
                })?,
        );
        Ok(owner)
    }
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Encode with the whole-frame bound before queuing. On a full queue the
    /// caller retains the exact frame and may retry after checking cancel/time.
    pub(crate) fn try_send<F: serde::Serialize>(&self, frame: F) -> Result<Option<F>> {
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

        match self
            .input
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
    pub(crate) fn try_reap(&mut self) -> Result<Option<ExitStatus>> {
        if self.reaped.is_none() {
            self.reaped = self.child.try_wait()?;
        }
        Ok(self.reaped)
    }
}
impl<T> Process<T> {
    /// Revokes both channels before kill/wait, so a reader blocked on its
    /// bounded output queue cannot obstruct cleanup. Never called on the actor.
    pub(crate) fn terminate(&mut self) {
        self.input.take();
        self.output.take();
        self.reap_before_release();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
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
