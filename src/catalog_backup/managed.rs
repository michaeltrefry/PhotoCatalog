//! Dedicated managed backup SQL owner. B opens only SQLite connections; every
//! regular-file operation is relayed to its G-owned sibling F process.
use super::{BackupReceipt, CancellationToken, Limits, Phase, Progress, RestoreReceipt};
use crate::{
    CURRENT_SCHEMA_VERSION,
    application::{I64, U64},
    catalog_backup::managed_filesystem::{Reply as FsReply, Request as FsRequest},
    catalog_session::{CatalogFilesystem, PhysicalObjectId},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    process::{Child as OsChild, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const PROTOCOL: u8 = 1;
const FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    Create {
        source: NativePath,
        bundle: NativePath,
        expected_source: PhysicalObjectId,
    },
    Inspect {
        bundle: NativePath,
    },
    Restore {
        bundle: NativePath,
        destination: NativePath,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Receipt {
    Backup(BackupReceipt),
    Restore(RestoreReceipt),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Startup {
    protocol: u8,
    build: String,
    nonce: String,
    operation: String,
    request: Request,
    limits: Limits,
}
#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Parent {
    Cancel,
    Filesystem {
        sequence: U64,
        reply: std::result::Result<FsReply, String>,
    },
    FilesystemDrained {
        sequence: U64,
        result: std::result::Result<(), String>,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Child {
    Ready {
        protocol: u8,
        build: String,
        nonce: String,
    },
    Progress(Progress),
    Filesystem {
        sequence: U64,
        request: FsRequest,
    },
    FilesystemShutdown {
        sequence: U64,
    },
    Failed {
        detail: String,
    },
    Terminal(std::result::Result<Receipt, String>),
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    protocol: u8,
    nonce: String,
    body: T,
}

pub fn build_identity() -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(
        concat!(
            include_str!("managed.rs"),
            include_str!("managed_filesystem.rs"),
            include_str!("../catalog_backup.rs"),
            include_str!("../catalog_storage.rs"),
            include_str!("../filesystem_worker.rs"),
            include_str!("../filesystem_worker/wire.rs"),
            include_str!("../../Cargo.lock")
        )
        .as_bytes(),
    );
    hash.update(crate::filesystem_worker::wire::build_identity().as_bytes());
    hash.finalize().to_hex().to_string()
}

fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<()> {
    let bytes = crate::filesystem_worker::wire::encode(value, FRAME_BYTES)?;
    let length = u32::try_from(bytes.len())?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}
fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<Option<T>> {
    let mut prefix = [0u8; 4];
    let mut read = 0;
    while read < prefix.len() {
        let n = reader.read(&mut prefix[read..])?;
        if n == 0 {
            ensure!(read == 0, "managed backup frame ended inside length prefix");
            return Ok(None);
        }
        read += n;
    }
    let length = usize::try_from(u32::from_be_bytes(prefix))?;
    ensure!(
        (1..=FRAME_BYTES).contains(&length),
        "managed backup frame length limit"
    );
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(Some(crate::filesystem_worker::wire::decode(
        &bytes,
        FRAME_BYTES,
    )?))
}

/// Test-only observability for the real managed B/F process boundary. The
/// release build keeps the normal entry point and never selects a fault.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessTestFault {
    None,
    ParentProtocolAfterFilesystemRequest,
    LoseTerminal,
    FailBackupWaitOnce,
}
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessEvent {
    Spawned { backup: u32, filesystem: u32 },
    FilesystemActive { backup: u32, filesystem: u32 },
    BackupWaitRetry { backup: u32 },
    BackupReaped { backup: u32 },
    FilesystemReaped { filesystem: u32 },
}

/// Run one B child and one sibling F child. This supervisor returns only after
/// both exact process owners and all pipe owners have been checked and retired.
pub fn run_process(
    executable: &Path,
    operation: String,
    request: Request,
    limits: Limits,
    cancel: &CancellationToken,
    progress: impl FnMut(Progress) -> Result<()>,
) -> Result<Receipt> {
    run_process_inner(
        executable,
        operation,
        request,
        limits,
        cancel,
        progress,
        ProcessTestFault::None,
        |_| {},
    )
}

/// Real-process fault hook used only by integration qualification. It does not
/// alter the installed worker protocol or production factory selection.
#[doc(hidden)]
#[expect(
    clippy::too_many_arguments,
    reason = "Keep process configuration, cancellation, and independent fault/progress/reap probes explicit at the supervision boundary"
)]
pub fn run_process_with_probe(
    executable: &Path,
    operation: String,
    request: Request,
    limits: Limits,
    cancel: &CancellationToken,
    progress: impl FnMut(Progress) -> Result<()>,
    fault: ProcessTestFault,
    probe: impl FnMut(ProcessEvent),
) -> Result<Receipt> {
    run_process_inner(
        executable,
        executable_canonical_operation(operation)?,
        request,
        limits,
        cancel,
        progress,
        fault,
        probe,
    )
}

fn executable_canonical_operation(operation: String) -> Result<String> {
    ensure!(
        uuid::Uuid::parse_str(&operation)?.to_string() == operation,
        "managed backup operation identity"
    );
    Ok(operation)
}

#[expect(
    clippy::too_many_arguments,
    reason = "Keep process configuration, cancellation, and independent fault/progress/reap probes explicit at the supervision boundary"
)]
fn run_process_inner(
    executable: &Path,
    operation: String,
    request: Request,
    limits: Limits,
    cancel: &CancellationToken,
    mut progress: impl FnMut(Progress) -> Result<()>,
    fault: ProcessTestFault,
    mut probe: impl FnMut(ProcessEvent),
) -> Result<Receipt> {
    limits.validate()?;
    let operation = executable_canonical_operation(operation)?;
    let filesystem = crate::filesystem_worker::client::Client::spawn(executable, vec![])
        .context("start managed backup filesystem owner")?;
    let filesystem_pid = filesystem.pid();
    let nonce = uuid::Uuid::new_v4().to_string();
    let startup = Startup {
        protocol: PROTOCOL,
        build: build_identity(),
        nonce: nonce.clone(),
        operation: operation.clone(),
        request,
        limits,
    };
    let mut child = match Command::new(executable)
        .arg("--catalog-backup-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start managed backup SQL owner")
    {
        Ok(child) => Some(child),
        Err(error) => {
            let mut ignored = |_| {};
            let retirement =
                retire_filesystem_after_backup(&filesystem, &operation, false, &mut ignored);
            return if retirement.is_empty() {
                Err(error)
            } else {
                Err(error.context(format!(
                    "filesystem sibling retirement required recovery: {}",
                    retirement.join("; ")
                )))
            };
        }
    };
    let backup_pid = child.as_ref().unwrap().id();
    probe(ProcessEvent::Spawned {
        backup: backup_pid,
        filesystem: filesystem_pid,
    });
    let mut input: Option<ChildStdin> = None;
    let mut reader = None;
    let setup = (|| -> Result<Receipt> {
        input = Some(
            child
                .as_mut()
                .unwrap()
                .stdin
                .take()
                .context("managed backup input")?,
        );
        let output = child
            .as_mut()
            .unwrap()
            .stdout
            .take()
            .context("managed backup output")?;
        write_frame(input.as_mut().unwrap(), &startup)?;
        let (tx, rx) = mpsc::sync_channel(4);
        reader = Some(
            std::thread::Builder::new()
                .name("backup-owner-output".into())
                .spawn(move || {
                    let mut output = output;
                    loop {
                        match read_frame::<Envelope<Child>>(&mut output) {
                            Ok(Some(value)) => {
                                if tx.send(Ok(value)).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(error) => {
                                let _ = tx.send(Err(error));
                                break;
                            }
                        }
                    }
                })?,
        );
        let wire_outcome = supervise(
            child.as_mut().unwrap(),
            input.as_mut().unwrap(),
            &rx,
            &filesystem,
            filesystem_pid,
            backup_pid,
            &nonce,
            cancel,
            &mut progress,
            fault,
            &mut probe,
        );
        let forced = wire_outcome.is_err();
        // Stop the output owner from blocking on a full result channel while
        // forced retirement is joining it after B has stopped.
        drop(rx);
        let mut retirement_errors = retire_backup(
            &mut child,
            &mut input,
            &mut reader,
            forced,
            fault == ProcessTestFault::FailBackupWaitOnce,
            &nonce,
            &mut probe,
        );
        if forced && filesystem.status().phase != crate::filesystem_worker::wire::Phase::Stopped {
            retirement_errors.extend(retire_filesystem_after_backup(
                &filesystem,
                &operation,
                true,
                &mut probe,
            ));
        } else if filesystem.status().phase != crate::filesystem_worker::wire::Phase::Stopped {
            retirement_errors.extend(retire_filesystem_after_backup(
                &filesystem,
                &operation,
                false,
                &mut probe,
            ));
        }
        let outcome = wire_outcome.and_then(|terminal| terminal.map_err(anyhow::Error::msg));
        if retirement_errors.is_empty() {
            outcome
        } else {
            let detail = retirement_errors.join("; ");
            match outcome {
                Ok(_) => bail!("managed backup retirement failed: {detail}"),
                Err(error) => Err(error.context(format!(
                    "managed backup retirement required recovery: {detail}"
                ))),
            }
        }
    })();
    match setup {
        Ok(value) => Ok(value),
        Err(error) if child.is_none() => Err(error),
        Err(error) => {
            // Setup failed after one or both siblings were spawned. Retain the
            // same owners locally and retry checked retirement before returning.
            let mut errors = retire_backup(
                &mut child,
                &mut input,
                &mut reader,
                true,
                fault == ProcessTestFault::FailBackupWaitOnce,
                &nonce,
                &mut probe,
            );
            errors.extend(retire_filesystem_after_backup(
                &filesystem,
                &operation,
                true,
                &mut probe,
            ));
            if errors.is_empty() {
                Err(error)
            } else {
                Err(error.context(format!(
                    "managed backup setup retirement required recovery: {}",
                    errors.join("; ")
                )))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    child: &mut OsChild,
    input: &mut ChildStdin,
    rx: &mpsc::Receiver<Result<Envelope<Child>>>,
    filesystem: &crate::filesystem_worker::client::Client,
    filesystem_pid: u32,
    backup_pid: u32,
    nonce: &str,
    cancel: &CancellationToken,
    progress: &mut impl FnMut(Progress) -> Result<()>,
    fault: ProcessTestFault,
    probe: &mut impl FnMut(ProcessEvent),
) -> Result<std::result::Result<Receipt, String>> {
    let mut ready = false;
    let mut canceled = false;
    let mut next_filesystem = 1u64;
    let mut filesystem_drained = false;
    loop {
        if cancel.is_cancelled() && !canceled {
            write_frame(
                input,
                &Envelope {
                    protocol: PROTOCOL,
                    nonce: nonce.to_owned(),
                    body: Parent::Cancel,
                },
            )?;
            canceled = true;
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Ok(envelope)) => {
                ensure!(
                    envelope.protocol == PROTOCOL && envelope.nonce == nonce,
                    "managed backup reply identity"
                );
                match envelope.body {
                    Child::Ready {
                        protocol,
                        build,
                        nonce: echoed,
                    } => {
                        ensure!(
                            !ready
                                && protocol == PROTOCOL
                                && build == build_identity()
                                && echoed == nonce,
                            "managed backup handshake identity"
                        );
                        ready = true;
                    }
                    Child::Progress(value) => {
                        ensure!(ready, "managed backup progress before handshake");
                        progress(value)?;
                    }
                    Child::Filesystem { sequence, request } => {
                        ensure!(ready, "managed backup filesystem request before handshake");
                        ensure!(
                            !filesystem_drained && sequence.0 == next_filesystem,
                            "managed backup filesystem request sequence"
                        );
                        next_filesystem = next_filesystem
                            .checked_add(1)
                            .context("managed backup filesystem sequence exhausted")?;
                        probe(ProcessEvent::FilesystemActive {
                            backup: backup_pid,
                            filesystem: filesystem_pid,
                        });
                        let reply =
                            CatalogFilesystem::backup_call(filesystem, &request, cancel.0.as_ref())
                                .map_err(|error| bounded_error(&error));
                        ensure!(
                            fault != ProcessTestFault::ParentProtocolAfterFilesystemRequest,
                            "injected malformed managed backup reply"
                        );
                        write_frame(
                            input,
                            &Envelope {
                                protocol: PROTOCOL,
                                nonce: nonce.to_owned(),
                                body: Parent::Filesystem { sequence, reply },
                            },
                        )?;
                    }
                    Child::FilesystemShutdown { sequence } => {
                        ensure!(ready, "managed backup filesystem shutdown before handshake");
                        ensure!(
                            !filesystem_drained && sequence.0 == next_filesystem,
                            "managed backup filesystem shutdown sequence"
                        );
                        next_filesystem = next_filesystem
                            .checked_add(1)
                            .context("managed backup filesystem sequence exhausted")?;
                        let result = filesystem
                            .try_shutdown()
                            .map_err(|error| bounded_error(&error));
                        filesystem_drained = result.is_ok();
                        if filesystem_drained {
                            probe(ProcessEvent::FilesystemReaped {
                                filesystem: filesystem_pid,
                            });
                        }
                        write_frame(
                            input,
                            &Envelope {
                                protocol: PROTOCOL,
                                nonce: nonce.to_owned(),
                                body: Parent::FilesystemDrained { sequence, result },
                            },
                        )?;
                    }
                    Child::Failed { detail } => {
                        ensure!(ready, "managed backup failure before handshake");
                        bail!("managed backup SQL owner failed before F drain: {detail}")
                    }
                    Child::Terminal(value) => {
                        ensure!(ready, "managed backup result before handshake");
                        ensure!(
                            filesystem_drained,
                            "managed backup terminal preceded filesystem checked reap"
                        );
                        ensure!(
                            fault != ProcessTestFault::LoseTerminal,
                            "injected lost managed backup terminal reply"
                        );
                        return Ok(value);
                    }
                }
            }
            Ok(Err(error)) => return Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait()? {
                    bail!("managed backup owner exited {status} before terminal acknowledgement");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("managed backup output ended before terminal acknowledgement")
            }
        }
    }
}

fn retire_backup(
    child: &mut Option<OsChild>,
    input: &mut Option<ChildStdin>,
    reader: &mut Option<std::thread::JoinHandle<()>>,
    forced: bool,
    mut fail_wait_once: bool,
    nonce: &str,
    probe: &mut impl FnMut(ProcessEvent),
) -> Vec<String> {
    let mut errors = Vec::new();
    if forced && let Some(input) = input.as_mut() {
        let _ = write_frame(
            input,
            &Envelope {
                protocol: PROTOCOL,
                nonce: nonce.to_owned(),
                body: Parent::Cancel,
            },
        );
    }
    drop(input.take());
    if let Some(owned) = child.as_mut() {
        let pid = owned.id();
        let mut wait_failure_reported = false;
        if forced && let Err(error) = owned.kill() {
            errors.push(format!("backup kill failed before checked wait: {error}"));
        }
        loop {
            if fail_wait_once {
                fail_wait_once = false;
                probe(ProcessEvent::BackupWaitRetry { backup: pid });
                errors.push("injected backup wait failure; retained exact child for retry".into());
                continue;
            }
            match owned.wait() {
                Ok(status) => {
                    if !forced && !status.success() {
                        errors.push(format!(
                            "managed backup owner exited {status} after terminal acknowledgement"
                        ));
                    }
                    child.take();
                    probe(ProcessEvent::BackupReaped { backup: pid });
                    break;
                }
                Err(error) => {
                    if !wait_failure_reported {
                        errors.push(format!(
                            "backup wait failed; retained exact child for retry: {error}"
                        ));
                        wait_failure_reported = true;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
    if let Some(owner) = reader.take()
        && owner.join().is_err()
    {
        errors.push("managed backup output owner panicked after process reap".into());
    }
    errors
}

fn retire_filesystem_after_backup(
    filesystem: &crate::filesystem_worker::client::Client,
    operation: &str,
    forced: bool,
    probe: &mut impl FnMut(ProcessEvent),
) -> Vec<String> {
    let mut errors = Vec::new();
    if filesystem.status().phase == crate::filesystem_worker::wire::Phase::Stopped {
        probe(ProcessEvent::FilesystemReaped {
            filesystem: filesystem.pid(),
        });
        return errors;
    }
    if forced {
        let cancel = AtomicBool::new(false);
        if let Err(error) = CatalogFilesystem::backup_call(
            filesystem,
            &FsRequest::Abort {
                operation: operation.to_owned(),
            },
            &cancel,
        ) {
            errors.push(format!("filesystem backup abort unavailable: {error:#}"));
        }
    }
    match filesystem.try_shutdown() {
        Ok(()) => {}
        Err(error) => {
            errors.push(format!("filesystem clean drain failed: {error:#}"));
            let mut wait_failure_reported = false;
            loop {
                match filesystem.terminate_after_dependents_drained() {
                    Ok(()) => break,
                    Err(error) => {
                        if !wait_failure_reported {
                            errors.push(format!(
                                "filesystem checked wait failed; retained exact child for retry: {error:#}"
                            ));
                            wait_failure_reported = true;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }
    }
    probe(ProcessEvent::FilesystemReaped {
        filesystem: filesystem.pid(),
    });
    errors
}

fn bounded_error(error: &anyhow::Error) -> String {
    let value = format!("{error:#}");
    let mut end = value.len().min(16 * 1024);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// Hidden configured-binary entry point. This process owns SQLite only; its
/// G-owned sibling F is reached through the closed backup relay below.
pub fn worker_main() -> Result<()> {
    std::panic::set_hook(Box::new(|_| {}));
    let mut input = std::io::stdin();
    let startup: Startup = read_frame(&mut input)?.context("missing managed backup startup")?;
    ensure!(
        startup.protocol == PROTOCOL && startup.build == build_identity(),
        "managed backup build/protocol mismatch"
    );
    ensure!(
        uuid::Uuid::parse_str(&startup.nonce)?.to_string() == startup.nonce,
        "managed backup nonce"
    );
    ensure!(
        uuid::Uuid::parse_str(&startup.operation)?.to_string() == startup.operation,
        "managed backup operation"
    );
    startup.limits.validate()?;
    let cancel = Arc::new(AtomicBool::new(false));
    let reader_cancel = cancel.clone();
    let nonce = startup.nonce.clone();
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    let control = std::thread::Builder::new()
        .name("backup-owner-control".into())
        .spawn(move || {
            let mut input = input;
            while let Ok(Some(envelope)) = read_frame::<Envelope<Parent>>(&mut input) {
                if envelope.protocol != PROTOCOL || envelope.nonce != nonce {
                    break;
                }
                match envelope.body {
                    Parent::Cancel => reader_cancel.store(true, Ordering::Release),
                    reply @ (Parent::Filesystem { .. } | Parent::FilesystemDrained { .. }) => {
                        if reply_tx.send(reply).is_err() {
                            break;
                        }
                    }
                }
            }
            reader_cancel.store(true, Ordering::Release);
        })?;
    let output = Arc::new(Mutex::new(std::io::stdout()));
    emit(
        &output,
        &startup.nonce,
        Child::Ready {
            protocol: PROTOCOL,
            build: build_identity(),
            nonce: startup.nonce.clone(),
        },
    )?;
    let filesystem = RemoteFilesystem {
        output: output.clone(),
        nonce: startup.nonce.clone(),
        replies: Mutex::new(reply_rx),
        sequence: AtomicU64::new(0),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_sql(
            &filesystem,
            &startup.operation,
            startup.request,
            &startup.limits,
            &cancel,
            |value| emit(&output, &startup.nonce, Child::Progress(value)),
        )
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("managed backup SQL owner panicked")));
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            // A failed SQLite close deliberately retains its connection until
            // this process exits. G must reap B before it aborts or retires F.
            emit(
                &output,
                &startup.nonce,
                Child::Failed {
                    detail: bounded_error(&error),
                },
            )?;
            return Ok(());
        }
    };
    if let Err(error) = filesystem.shutdown() {
        let detail = bounded_error(&error.context("filesystem owner failed to drain after backup"));
        emit(&output, &startup.nonce, Child::Failed { detail })?;
        return Ok(());
    }
    emit(&output, &startup.nonce, Child::Terminal(Ok(value)))?;
    drop(output);
    drop(control);
    Ok(())
}

struct RemoteFilesystem {
    output: Arc<Mutex<std::io::Stdout>>,
    nonce: String,
    replies: Mutex<mpsc::Receiver<Parent>>,
    sequence: AtomicU64,
}
impl RemoteFilesystem {
    fn next(&self) -> Result<U64> {
        let value = self
            .sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("managed backup filesystem sequence exhausted"))?
            .checked_add(1)
            .context("managed backup filesystem sequence exhausted")?;
        Ok(U64(value))
    }
    fn shutdown(&self) -> Result<()> {
        let sequence = self.next()?;
        emit(
            &self.output,
            &self.nonce,
            Child::FilesystemShutdown { sequence },
        )?;
        match self.replies.lock().unwrap().recv()? {
            Parent::FilesystemDrained {
                sequence: returned,
                result,
            } => {
                ensure!(
                    returned.0 == sequence.0,
                    "filesystem drain sequence mismatch"
                );
                result.map_err(anyhow::Error::msg)
            }
            _ => bail!("unexpected filesystem drain response"),
        }
    }
}

trait BackupFilesystem {
    fn backup_call(&self, request: &FsRequest, cancel: &AtomicBool) -> Result<FsReply>;
}
impl BackupFilesystem for RemoteFilesystem {
    fn backup_call(&self, request: &FsRequest, cancel: &AtomicBool) -> Result<FsReply> {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "backup filesystem relay cancelled"
        );
        let sequence = self.next()?;
        emit(
            &self.output,
            &self.nonce,
            Child::Filesystem {
                sequence,
                request: request.clone(),
            },
        )?;
        match self.replies.lock().unwrap().recv()? {
            Parent::Filesystem {
                sequence: returned,
                reply,
            } => {
                ensure!(
                    returned.0 == sequence.0,
                    "filesystem reply sequence mismatch"
                );
                reply.map_err(anyhow::Error::msg)
            }
            _ => bail!("unexpected filesystem response"),
        }
    }
}
impl BackupFilesystem for crate::filesystem_worker::client::Client {
    fn backup_call(&self, request: &FsRequest, cancel: &AtomicBool) -> Result<FsReply> {
        CatalogFilesystem::backup_call(self, request, cancel)
    }
}

fn emit(output: &Mutex<impl Write>, nonce: &str, body: Child) -> Result<()> {
    write_frame(
        &mut *output.lock().unwrap(),
        &Envelope {
            protocol: PROTOCOL,
            nonce: nonce.to_owned(),
            body,
        },
    )
}
fn fs_call(
    filesystem: &dyn BackupFilesystem,
    request: FsRequest,
    cancel: &Arc<AtomicBool>,
) -> Result<FsReply> {
    filesystem.backup_call(&request, cancel)
}

fn run_sql(
    filesystem: &dyn BackupFilesystem,
    operation: &str,
    request: Request,
    limits: &Limits,
    cancel: &Arc<AtomicBool>,
    mut callback: impl FnMut(Progress) -> Result<()>,
) -> Result<Receipt> {
    let op = SqlGuard::new(limits, cancel.clone())?;
    match request {
        Request::Create {
            source,
            bundle,
            expected_source,
        } => {
            let FsReply::CreatePrepared {
                source,
                source_physical,
                target,
                target_physical,
            } = fs_call(
                filesystem,
                FsRequest::PrepareCreate {
                    operation: operation.into(),
                    source,
                    bundle,
                    expected_source,
                    limits: limits.clone(),
                },
                cancel,
            )?
            else {
                bail!("unexpected create preparation reply")
            };
            ensure!(
                source_physical != target_physical,
                "managed backup source and target identities are the same object"
            );
            let source = open_verified(
                &source.to_path()?,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                source_physical,
            )?;
            let mut target = open_verified(
                &target.to_path()?,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                target_physical,
            )?;
            configure_readonly(&source)?;
            target.busy_timeout(Duration::ZERO)?;
            op.install(&source)?;
            source.execute_batch("BEGIN")?;
            let _: i64 =
                source.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
            let version = identity_sql(&source)?;
            let pages: i64 = source.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            let page_size: i64 = source.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let bytes = u64::try_from(pages)?
                .checked_mul(u64::try_from(page_size)?)
                .context("database size overflow")?;
            ensure!(
                bytes <= limits.max_database_bytes,
                "database exceeds backup byte limit"
            );
            callback(Progress {
                phase: Phase::Snapshot,
                pages_copied: 0,
                total_pages: u64::try_from(pages)?,
                bytes_processed: 0,
            })?;
            let backup = Backup::new(&source, &mut target)?;
            let mut busy = 0;
            loop {
                check_cancel(cancel)?;
                op.check()?;
                match fs_call(
                    filesystem,
                    FsRequest::CheckCreate {
                        operation: operation.into(),
                        database_bytes: U64(bytes),
                    },
                    cancel,
                )? {
                    FsReply::CreateChecked => {}
                    _ => bail!("unexpected create check reply"),
                }
                let step = backup
                    .step(limits.pages_per_step)
                    .context("SQLite snapshot copy failed")?;
                let p = backup.progress();
                callback(Progress {
                    phase: Phase::Copy,
                    pages_copied: u64::try_from(p.pagecount - p.remaining)?,
                    total_pages: u64::try_from(p.pagecount)?,
                    bytes_processed: u64::try_from(p.pagecount - p.remaining)?
                        .saturating_mul(page_size as u64),
                })?;
                match step {
                    StepResult::Done => break,
                    StepResult::More => {}
                    StepResult::Busy | StepResult::Locked => {
                        busy += 1;
                        ensure!(
                            busy <= limits.max_busy_steps,
                            "backup remained busy/locked; retry in a new destination"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => bail!("unknown SQLite backup result"),
                }
            }
            drop(backup);
            source.execute_batch("ROLLBACK")?;
            close_owned(source)?;
            callback(Progress {
                phase: Phase::Verify,
                pages_copied: 0,
                total_pages: 0,
                bytes_processed: bytes,
            })?;
            finalize_and_verify(&target, &op)?;
            close_owned(target)?;
            loop {
                op.check()?;
                let FsReply::HashProgress { bytes, done } = fs_call(
                    filesystem,
                    FsRequest::HashCreate {
                        operation: operation.into(),
                    },
                    cancel,
                )?
                else {
                    bail!("unexpected backup hash reply")
                };
                callback(Progress {
                    phase: Phase::Hash,
                    pages_copied: 0,
                    total_pages: 0,
                    bytes_processed: bytes.0,
                })?;
                if done {
                    break;
                }
            }
            let FsReply::Backup(receipt) = fs_call(
                filesystem,
                FsRequest::FinishCreate {
                    operation: operation.into(),
                    schema_version: I64(version),
                },
                cancel,
            )?
            else {
                bail!("unexpected backup publication reply")
            };
            callback(Progress {
                phase: Phase::Publish,
                pages_copied: 0,
                total_pages: 0,
                bytes_processed: receipt.database_bytes,
            })?;
            Ok(Receipt::Backup(receipt))
        }
        Request::Inspect { bundle } => {
            let (receipt, database, physical) = prepare_inspect(
                filesystem,
                operation,
                bundle,
                limits,
                cancel,
                &op,
                &mut callback,
            )?;
            callback(Progress {
                phase: Phase::Verify,
                pages_copied: 0,
                total_pages: 0,
                bytes_processed: receipt.database_bytes,
            })?;
            verify_admitted(&database, physical, receipt.schema_version, &op)?;
            let FsReply::Backup(done) = fs_call(
                filesystem,
                FsRequest::FinishInspect {
                    operation: operation.into(),
                },
                cancel,
            )?
            else {
                bail!("unexpected inspect finish reply")
            };
            ensure!(done == receipt, "inspect receipt changed");
            Ok(Receipt::Backup(done))
        }
        Request::Restore {
            bundle,
            destination,
        } => {
            let (receipt, database, physical) = prepare_inspect(
                filesystem,
                operation,
                bundle,
                limits,
                cancel,
                &op,
                &mut callback,
            )?;
            callback(Progress {
                phase: Phase::Verify,
                pages_copied: 0,
                total_pages: 0,
                bytes_processed: receipt.database_bytes,
            })?;
            verify_admitted(&database, physical, receipt.schema_version, &op)?;
            let FsReply::RestorePrepared {
                target,
                target_physical,
                receipt: copied_receipt,
            } = fs_call(
                filesystem,
                FsRequest::PrepareRestore {
                    operation: operation.into(),
                    destination,
                },
                cancel,
            )?
            else {
                bail!("unexpected restore preparation reply")
            };
            ensure!(
                copied_receipt == receipt,
                "restore receipt changed before copy"
            );
            loop {
                op.check()?;
                let FsReply::CopyProgress { bytes, done } = fs_call(
                    filesystem,
                    FsRequest::CopyRestore {
                        operation: operation.into(),
                    },
                    cancel,
                )?
                else {
                    bail!("unexpected restore copy reply")
                };
                callback(Progress {
                    phase: Phase::Copy,
                    pages_copied: 0,
                    total_pages: 0,
                    bytes_processed: bytes.0,
                })?;
                if done {
                    break;
                }
            }
            callback(Progress {
                phase: Phase::Upgrade,
                pages_copied: 0,
                total_pages: 0,
                bytes_processed: receipt.database_bytes,
            })?;
            let target_path = target.to_path()?;
            let target_root = target_path
                .parent()
                .context("restored database has no parent")?
                .to_path_buf();
            ensure!(
                physical != target_physical,
                "managed restore source and target identities are the same object"
            );
            let (target, schema) =
                open_restoring(&target_path, target_physical, &target_root, &op)?;
            close_owned(target)?;
            let FsReply::Restore(restored) = fs_call(
                filesystem,
                FsRequest::FinishRestore {
                    operation: operation.into(),
                    schema_version: I64(schema),
                },
                cancel,
            )?
            else {
                bail!("unexpected restore publication reply")
            };
            callback(Progress {
                phase: Phase::Publish,
                pages_copied: 0,
                total_pages: 0,
                bytes_processed: receipt.database_bytes,
            })?;
            Ok(Receipt::Restore(restored))
        }
    }
}

fn prepare_inspect(
    filesystem: &dyn BackupFilesystem,
    operation: &str,
    bundle: NativePath,
    limits: &Limits,
    cancel: &Arc<AtomicBool>,
    guard: &SqlGuard,
    callback: &mut impl FnMut(Progress) -> Result<()>,
) -> Result<(BackupReceipt, PathBuf, PhysicalObjectId)> {
    let FsReply::InspectPrepared {
        database,
        physical,
        receipt,
    } = fs_call(
        filesystem,
        FsRequest::PrepareInspect {
            operation: operation.into(),
            bundle,
            limits: limits.clone(),
        },
        cancel,
    )?
    else {
        bail!("unexpected inspect preparation reply")
    };
    loop {
        guard.check()?;
        let FsReply::HashProgress { bytes, done } = fs_call(
            filesystem,
            FsRequest::HashInspect {
                operation: operation.into(),
            },
            cancel,
        )?
        else {
            bail!("unexpected inspect hash reply")
        };
        callback(Progress {
            phase: Phase::Hash,
            pages_copied: 0,
            total_pages: 0,
            bytes_processed: bytes.0,
        })?;
        if done {
            break;
        }
    }
    Ok((receipt, database.to_path()?, physical))
}

struct SqlOwner(Option<Connection>);
impl Deref for SqlOwner {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("managed SQL owner")
    }
}
impl DerefMut for SqlOwner {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("managed SQL owner")
    }
}
impl Drop for SqlOwner {
    fn drop(&mut self) {
        // An error path is not proof that SQLite can safely close. B is a
        // single-operation process, so retain the connection until OS reap.
        if let Some(connection) = self.0.take() {
            std::mem::forget(connection);
        }
    }
}

fn open_verified(path: &Path, flags: OpenFlags, expected: PhysicalObjectId) -> Result<SqlOwner> {
    let db = Connection::open_with_flags(path, flags)
        .context("managed backup SQLite admission failed; SQL owner is poisoned")?;
    if let Err(error) = crate::catalog_storage::verify_database_identity(&db, &expected) {
        std::mem::forget(db);
        return Err(error.context(
            "managed backup SQLite admission selected the wrong object; SQL owner is poisoned",
        ));
    }
    Ok(SqlOwner(Some(db)))
}
fn configure_readonly(db: &Connection) -> Result<()> {
    db.busy_timeout(Duration::ZERO)?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; PRAGMA mmap_size=0; PRAGMA cache_size=-32768; PRAGMA temp_store=FILE;")?;
    Ok(())
}
fn identity_sql(db: &Connection) -> Result<i64> {
    let application: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        application == super::APPLICATION_ID && (1..=CURRENT_SCHEMA_VERSION).contains(&schema),
        "not a supported LensWorks backup schema (application={application}, schema={schema})"
    );
    Ok(schema)
}
fn verify_admitted(
    path: &Path,
    physical: PhysicalObjectId,
    schema: i64,
    op: &SqlGuard,
) -> Result<()> {
    let db = open_verified(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        physical,
    )?;
    configure_readonly(&db)?;
    op.install(&db)?;
    ensure!(
        identity_sql(&db)? == schema,
        "backup schema differs from manifest"
    );
    verify_integrity(&db)?;
    close_owned(db)
}
/// Dedicated managed restoring constructor. Actual-handle admission is the
/// first operation after SQLite open; only then may schema SQL run.
fn open_restoring(
    path: &Path,
    physical: PhysicalObjectId,
    root: &Path,
    op: &SqlGuard,
) -> Result<(SqlOwner, i64)> {
    let mut db = open_verified(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        physical,
    )?;
    op.install(&db)?;
    let writers = crate::catalog_writer::for_catalog(root);
    crate::initialize_catalog_connection(&mut db, &writers, |_| Ok(()))
        .context("restored copy schema upgrade failed; original and bundle remain untouched")?;
    finalize_and_verify(&db, op)?;
    let schema = identity_sql(&db)?;
    ensure!(
        schema == CURRENT_SCHEMA_VERSION,
        "restored schema did not reach current version"
    );
    Ok((db, schema))
}
fn finalize_and_verify(db: &Connection, op: &SqlGuard) -> Result<()> {
    let mode: String = db.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
    ensure!(mode == "delete", "cannot finalize self-contained backup");
    op.install(db)?;
    verify_integrity(db)
}
fn verify_integrity(db: &Connection) -> Result<()> {
    let mut statement = db.prepare("PRAGMA integrity_check(1)")?;
    let mut rows = statement.query([])?;
    let first = rows.next()?.context("missing SQLite integrity result")?;
    let answer: String = first.get(0)?;
    ensure!(
        answer == "ok" && rows.next()?.is_none(),
        "backup integrity failure: {answer}"
    );
    ensure!(
        db.prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_none(),
        "backup foreign-key violation"
    );
    Ok(())
}
fn close_owned(mut owner: SqlOwner) -> Result<()> {
    let db = owner.0.take().expect("managed SQL owner");
    match db.close() {
        Ok(()) => Ok(()),
        Err((db, error)) => {
            std::mem::forget(db);
            Err(error).context(
                "managed backup SQLite close failed; SQL owner retained until process retirement",
            )
        }
    }
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "backup/restore cancelled; incomplete destination remains blocked"
    );
    Ok(())
}

struct SqlGuard {
    limits: Limits,
    started: Instant,
    cancel: Arc<AtomicBool>,
    vm: Arc<AtomicU64>,
}
impl SqlGuard {
    fn new(limits: &Limits, cancel: Arc<AtomicBool>) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            limits: limits.clone(),
            started: Instant::now(),
            cancel,
            vm: Arc::new(AtomicU64::new(0)),
        })
    }
    fn check(&self) -> Result<()> {
        check_cancel(&self.cancel)?;
        ensure!(
            self.started.elapsed() < Duration::from_secs(self.limits.max_seconds),
            "backup/restore deadline exceeded; incomplete destination remains blocked"
        );
        ensure!(
            self.vm.load(Ordering::Relaxed) <= self.limits.verification_vm_steps,
            "backup/restore SQLite VM limit exceeded"
        );
        Ok(())
    }
    fn install(&self, db: &Connection) -> Result<()> {
        let cancel = self.cancel.clone();
        let vm = self.vm.clone();
        let limit = self.limits.verification_vm_steps;
        let started = self.started;
        let seconds = self.limits.max_seconds;
        db.progress_handler(
            1000,
            Some(move || {
                cancel.load(Ordering::Acquire)
                    || vm.fetch_add(1000, Ordering::Relaxed) >= limit
                    || started.elapsed() >= Duration::from_secs(seconds)
            }),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Catalog;

    #[cfg(unix)]
    #[test]
    fn admitted_sql_alias_uses_the_actual_handle_while_an_existing_lock_is_live() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("catalog");
        let catalog = Catalog::open(&root)?;
        let expected = crate::catalog_storage::opened_database_identity(&catalog.db)?;
        let alias = temp.path().join("catalog-alias.sqlite3");
        std::fs::hard_link(root.join(super::super::DB), &alias)?;
        catalog.db.execute_batch("BEGIN IMMEDIATE")?;
        let admitted = open_verified(
            &alias,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            expected,
        )?;
        close_owned(admitted)?;
        catalog.db.execute_batch("ROLLBACK")?;
        Ok(())
    }

    #[test]
    fn failed_close_is_reported_as_retained_until_process_retirement() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let handle = unsafe { db.handle() };
        let mut statement = std::ptr::null_mut();
        let code = unsafe {
            rusqlite::ffi::sqlite3_prepare_v2(
                db.handle(),
                c"SELECT 1".as_ptr(),
                -1,
                &mut statement,
                std::ptr::null_mut(),
            )
        };
        ensure!(
            code == rusqlite::ffi::SQLITE_OK && !statement.is_null(),
            "test statement preparation"
        );
        let error = close_owned(SqlOwner(Some(db))).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("retained until process retirement")
        );
        // This test intentionally models a poisoned one-operation B process;
        // explicitly retire the synthetic raw statement and handle afterward.
        unsafe {
            rusqlite::ffi::sqlite3_finalize(statement);
            rusqlite::ffi::sqlite3_close(handle);
        }
        Ok(())
    }
}
