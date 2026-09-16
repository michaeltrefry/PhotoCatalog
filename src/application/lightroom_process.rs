//! Unselected G-owned process boundary for the complete Workbench protocol.
//!
//! W owns the inspection SQLite roles and retained public request/result state.
//! This transport contains no production factory selection; the desktop owner
//! may opt into it only after the remaining F/S custody map is qualified.
use super::lightroom_bridge::{Coordinator, Request, Response};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
#[cfg(test)]
use std::process::ChildStdout;
use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Condvar, Mutex, atomic::AtomicBool},
    thread,
};

pub const ENVELOPE_BYTES: usize = 128 * 1024;
const HEADER_BYTES: usize = 48;
const PROTOCOL: u8 = 1;
const MAGIC: &[u8; 4] = b"PCWB";
const ERROR_BYTES: usize = 4096;
pub(crate) const CALLBACK_BYTES: usize = 64 * 1024 * 1024;
const CALLBACK_CHUNK_BYTES: usize = (ENVELOPE_BYTES - 16 * 1024) / 6;

pub fn build_identity() -> String {
    blake3::hash(
        concat!(
            env!("CARGO_PKG_VERSION"),
            include_str!("lightroom.rs"),
            include_str!("lightroom/worker.rs"),
            include_str!("lightroom_bridge.rs"),
            include_str!("lightroom_bridge/wire.rs"),
            include_str!("lightroom_managed.rs"),
            include_str!("lightroom_process.rs"),
            include_str!("../lightroom/plan.rs"),
            include_str!("../lightroom/plan/desktop.rs"),
            include_str!("../lightroom/plan/selection.rs"),
            include_str!("../lightroom/plan/selection/snapshot.rs"),
            include_str!("../lightroom/migration_source.rs"),
            include_str!("../lightroom/migration_source/reader.rs"),
            include_str!("../../Cargo.lock")
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Startup {
    protocol: u8,
    build: String,
    instance: crate::catalog_session::LeaseId,
    envelope_bytes: crate::application::U64,
    managed: bool,
}
impl Startup {
    fn new(managed: bool) -> Self {
        Self {
            protocol: PROTOCOL,
            build: build_identity(),
            instance: crate::catalog_session::LeaseId::new(),
            envelope_bytes: crate::application::U64(ENVELOPE_BYTES as u64),
            managed,
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.protocol == PROTOCOL
                && self.build == build_identity()
                && self.envelope_bytes.0 == ENVELOPE_BYTES as u64,
            "Workbench process bootstrap mismatch"
        );
        crate::catalog_session::LeaseId::parse(self.instance.as_str())?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Work {
    Request {
        sequence: crate::application::U64,
        request_digest: String,
        request: Request,
    },
    Shutdown {
        sequence: crate::application::U64,
    },
    CallbackBegin {
        sequence: crate::application::U64,
        bytes: crate::application::U64,
        blake3: String,
    },
    CallbackChunk {
        sequence: crate::application::U64,
        offset: crate::application::U64,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
enum CallbackRequest {
    Admit,
    Commit,
    Filesystem {
        request: crate::filesystem_worker::wire::LightroomWorkbenchIo,
    },
    SourceOpen {
        authority: crate::lightroom_migration_worker::source_reader::CaptureSqlAuthority,
    },
    SourceSqlOpen {
        seal: crate::lightroom::migration_source::InputSeal,
        limits: crate::lightroom::migration_source::ReadLimits,
        protected: Vec<crate::lightroom_migration_worker::identity::FileKey>,
    },
    SourceSchema {
        source: String,
    },
    SourceRows {
        source: String,
        handle: String,
        cursor: Option<Vec<crate::lightroom::plan::Cell>>,
        limit: crate::application::U64,
    },
    SourceCurrent {
        source: String,
    },
    SourceRetire {
        source: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "callback_kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
enum CallbackValue {
    Admitted,
    Filesystem(crate::filesystem_worker::wire::LightroomWorkbenchIoReply),
    Source(String),
    Schema(crate::lightroom_migration_worker::source_reader::capture_wire::SchemaObjects),
    Table(crate::lightroom_migration_worker::source_reader::capture_wire::TableValue),
    Current(crate::lightroom_migration_worker::source_reader::capture_wire::Current),
    Retired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub detail: String,
    pub fatal: bool,
}
impl Failure {
    fn new(error: impl std::fmt::Display) -> Self {
        Self {
            detail: format!("{error:#}").chars().take(ERROR_BYTES).collect(),
            fatal: false,
        }
    }
    fn fatal(error: impl std::fmt::Display) -> Self {
        Self {
            detail: format!("{error:#}").chars().take(ERROR_BYTES).collect(),
            fatal: true,
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.detail.is_empty() && self.detail.len() <= ERROR_BYTES,
            "Workbench failure detail bound"
        );
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
enum Outcome {
    Ready {
        instance: crate::catalog_session::LeaseId,
        build: String,
    },
    Reply {
        sequence: crate::application::U64,
        request_digest: String,
        result: std::result::Result<Response, Failure>,
    },
    Drained {
        sequence: crate::application::U64,
        instance: crate::catalog_session::LeaseId,
    },
    Callback {
        sequence: crate::application::U64,
        request: CallbackRequest,
    },
}

fn encode_limit(value: &impl Serialize, maximum: usize) -> Result<Vec<u8>> {
    struct Count {
        bytes: usize,
        maximum: usize,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .filter(|bytes| *bytes <= self.maximum)
                .ok_or_else(|| std::io::Error::other("Workbench envelope byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { bytes: 0, maximum };
    serde_json::to_writer(&mut count, value)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(count.bytes)?;
    serde_json::to_writer(&mut bytes, value)?;
    ensure!(
        bytes.len() == count.bytes,
        "Workbench encoding length changed"
    );
    Ok(bytes)
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    encode_limit(value, ENVELOPE_BYTES)
}

fn decode_limit<T: DeserializeOwned>(bytes: &[u8], maximum: usize) -> Result<T> {
    ensure!(bytes.len() <= maximum, "Workbench decoded byte limit");
    Ok(serde_json::from_slice(bytes)?)
}
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    decode_limit(bytes, ENVELOPE_BYTES)
}

fn write_packet(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = encode(value)?;
    let length = u32::try_from(bytes.len())?;
    let digest = blake3::hash(&bytes);
    let mut header = [0; HEADER_BYTES];
    header[..4].copy_from_slice(MAGIC);
    header[4] = PROTOCOL;
    header[8..12].copy_from_slice(&length.to_le_bytes());
    header[12..44].copy_from_slice(digest.as_bytes());
    writer.write_all(&header)?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_packet<T: DeserializeOwned>(reader: &mut impl Read) -> Result<Option<T>> {
    let mut header = [0; HEADER_BYTES];
    loop {
        match reader.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => break,
            Ok(_) => unreachable!(),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    reader.read_exact(&mut header[1..])?;
    ensure!(
        &header[..4] == MAGIC
            && header[4] == PROTOCOL
            && header[5..8] == [0, 0, 0]
            && header[44..48] == [0, 0, 0, 0],
        "Workbench frame header mismatch: {}",
        header[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    ensure!(length <= ENVELOPE_BYTES, "Workbench frame byte limit");
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length)?;
    bytes.resize(length, 0);
    reader.read_exact(&mut bytes)?;
    ensure!(
        blake3::hash(&bytes).as_bytes()[..] == header[12..44],
        "Workbench frame digest mismatch"
    );
    Ok(Some(decode(&bytes)?))
}

fn request_digest(request: &Request) -> Result<String> {
    Ok(blake3::hash(&encode(request)?).to_hex().to_string())
}

#[derive(Default)]
struct CallbackState {
    next: u64,
    assembly: Option<CallbackAssembly>,
    waiting: Option<(u64, std::result::Result<CallbackValue, Failure>)>,
    closed: bool,
}
struct CallbackAssembly {
    sequence: u64,
    length: usize,
    blake3: String,
    bytes: Vec<u8>,
}
struct CallbackProxy {
    output: Arc<Mutex<std::io::Stdout>>,
    state: Mutex<CallbackState>,
    wake: Condvar,
}
impl CallbackProxy {
    fn call(&self, request: CallbackRequest, cancel: &AtomicBool) -> Result<CallbackValue> {
        let sequence = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(!state.closed, "Workbench supervisor callback is closed");
            state.next = state
                .next
                .checked_add(1)
                .context("callback sequence exhausted")?;
            state.next
        };
        write_packet(
            &mut *self.output.lock().unwrap_or_else(|e| e.into_inner()),
            &Outcome::Callback {
                sequence: crate::application::U64(sequence),
                request,
            },
        )?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Acquire),
                "Workbench callback canceled"
            );
            ensure!(!state.closed, "Workbench supervisor callback closed");
            if state
                .waiting
                .as_ref()
                .is_some_and(|value| value.0 == sequence)
            {
                let (_, result) = state.waiting.take().unwrap();
                return result.map_err(|failure| anyhow::anyhow!(failure.detail));
            }
            let (next, _) = self
                .wake
                .wait_timeout(state, std::time::Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner());
            state = next;
        }
    }
    fn reply(
        &self,
        sequence: crate::application::U64,
        length: crate::application::U64,
        blake3: String,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            sequence.0 > 0
                && sequence.0 <= state.next
                && state.waiting.is_none()
                && state.assembly.is_none(),
            "Workbench callback result sequence"
        );
        let length = usize::try_from(length.0)?;
        ensure!(
            (1..=CALLBACK_BYTES).contains(&length),
            "Workbench callback result length"
        );
        ensure!(
            blake3.len() == 64
                && blake3
                    .bytes()
                    .all(|value| value.is_ascii_hexdigit() && !value.is_ascii_uppercase()),
            "Workbench callback result digest"
        );
        state.assembly = Some(CallbackAssembly {
            sequence: sequence.0,
            length,
            blake3,
            bytes: Vec::with_capacity(length),
        });
        Ok(())
    }
    fn chunk(
        &self,
        sequence: crate::application::U64,
        offset: crate::application::U64,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let assembly = state
            .assembly
            .as_mut()
            .context("Workbench callback result has no begin")?;
        ensure!(
            assembly.sequence == sequence.0
                && offset.0 == assembly.bytes.len() as u64
                && !bytes.is_empty()
                && bytes.len() <= CALLBACK_CHUNK_BYTES
                && bytes.len() <= assembly.length - assembly.bytes.len(),
            "Workbench callback result continuity"
        );
        assembly.bytes.extend_from_slice(&bytes);
        if assembly.bytes.len() == assembly.length {
            let assembly = state.assembly.take().unwrap();
            ensure!(
                crate::lightroom::digest(&assembly.bytes) == assembly.blake3,
                "Workbench callback result changed"
            );
            let result = decode_limit(&assembly.bytes, CALLBACK_BYTES)?;
            state.waiting = Some((assembly.sequence, result));
            self.wake.notify_all();
        }
        Ok(())
    }
    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        self.wake.notify_all();
    }
}
impl super::lightroom::ManagedIo for CallbackProxy {
    fn admit(&self) -> Result<()> {
        match self.call(CallbackRequest::Admit, &AtomicBool::new(false))? {
            CallbackValue::Admitted => Ok(()),
            _ => anyhow::bail!("Workbench admission callback result kind"),
        }
    }

    fn commit(&self) -> Result<()> {
        match self.call(CallbackRequest::Commit, &AtomicBool::new(false))? {
            CallbackValue::Admitted => Ok(()),
            _ => anyhow::bail!("Workbench commit callback result kind"),
        }
    }

    fn revoke_generation(&self) {
        // Only the concrete G owner may revoke the outer generation.
    }

    fn workbench_reaped(&self) {
        // Only the concrete G owner records the outer W reap.
    }

    fn filesystem(
        &self,
        request: crate::filesystem_worker::wire::LightroomWorkbenchIo,
        cancel: &AtomicBool,
    ) -> Result<crate::filesystem_worker::wire::LightroomWorkbenchIoReply> {
        match self.call(CallbackRequest::Filesystem { request }, cancel)? {
            CallbackValue::Filesystem(value) => Ok(value),
            _ => anyhow::bail!("Workbench filesystem callback result kind"),
        }
    }
    fn source_open(
        &self,
        authority: crate::lightroom_migration_worker::source_reader::CaptureSqlAuthority,
        cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        match self.call(CallbackRequest::SourceOpen { authority }, &cancel)? {
            CallbackValue::Source(value) => Ok(value),
            _ => anyhow::bail!("Workbench source-open callback result kind"),
        }
    }
    fn source_sql_open(
        &self,
        seal: crate::lightroom::migration_source::InputSeal,
        limits: crate::lightroom::migration_source::ReadLimits,
        protected: Vec<crate::lightroom_migration_worker::identity::FileKey>,
        cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        match self.call(
            CallbackRequest::SourceSqlOpen {
                seal,
                limits,
                protected,
            },
            &cancel,
        )? {
            CallbackValue::Source(value) => Ok(value),
            _ => anyhow::bail!("Workbench SQL13 source-open callback result kind"),
        }
    }
    fn source_schema(
        &self,
        source: &str,
    ) -> Result<crate::lightroom_migration_worker::source_reader::capture_wire::SchemaObjects> {
        match self.call(
            CallbackRequest::SourceSchema {
                source: source.into(),
            },
            &AtomicBool::new(false),
        )? {
            CallbackValue::Schema(value) => Ok(value),
            _ => anyhow::bail!("Workbench source-schema callback result kind"),
        }
    }
    fn source_rows(
        &self,
        source: &str,
        handle: String,
        cursor: Option<Vec<crate::lightroom::plan::Cell>>,
        limit: usize,
    ) -> Result<crate::lightroom_migration_worker::source_reader::capture_wire::TableValue> {
        match self.call(
            CallbackRequest::SourceRows {
                source: source.into(),
                handle,
                cursor,
                limit: crate::application::U64(limit.try_into()?),
            },
            &AtomicBool::new(false),
        )? {
            CallbackValue::Table(value) => Ok(value),
            _ => anyhow::bail!("Workbench source-rows callback result kind"),
        }
    }
    fn source_current(
        &self,
        source: &str,
    ) -> Result<crate::lightroom_migration_worker::source_reader::capture_wire::Current> {
        match self.call(
            CallbackRequest::SourceCurrent {
                source: source.into(),
            },
            &AtomicBool::new(false),
        )? {
            CallbackValue::Current(value) => Ok(value),
            _ => anyhow::bail!("Workbench source-current callback result kind"),
        }
    }
    fn source_retire(&self, source: &str) -> Result<()> {
        match self.call(
            CallbackRequest::SourceRetire {
                source: source.into(),
            },
            &AtomicBool::new(false),
        )? {
            CallbackValue::Retired => Ok(()),
            _ => anyhow::bail!("Workbench source-retire callback result kind"),
        }
    }
    fn drain_sources(&self) -> Result<()> {
        // This is W's callback proxy, not G's concrete dependency owner.
        Ok(())
    }
    fn drain_filesystem(&self) -> Result<()> {
        Ok(())
    }
}

/// Hidden W entrypoint. stdout is the only response stream; stderr remains
/// unused so an unframed diagnostic can never be mistaken for protocol data.
pub fn worker_main() -> Result<()> {
    let mut input = std::io::stdin().lock();
    let startup: Startup = read_packet(&mut input)?.context("missing Workbench bootstrap")?;
    startup.validate()?;
    let output = Arc::new(Mutex::new(std::io::stdout()));
    write_packet(
        &mut *output.lock().unwrap_or_else(|e| e.into_inner()),
        &Outcome::Ready {
            instance: startup.instance.clone(),
            build: build_identity(),
        },
    )?;
    let executable = std::env::current_exe()?;
    let control = std::sync::Arc::new(Mutex::new(super::lightroom_bridge::Control::default()));
    let callback = Arc::new(CallbackProxy {
        output: output.clone(),
        state: Mutex::new(CallbackState::default()),
        wake: Condvar::new(),
    });
    let mut coordinator = if startup.managed {
        Coordinator::new_managed(control, callback.clone())
    } else {
        Coordinator::new(control)
    };
    loop {
        let work: Work = read_packet(&mut input)?.context("Workbench control stream ended")?;
        match work {
            Work::Request {
                sequence,
                request_digest: expected,
                request,
            } => {
                ensure!(
                    sequence.0 > 0 && request_digest(&request)? == expected,
                    "Workbench request binding mismatch"
                );
                #[cfg(test)]
                if let Request::Status {
                    attempt: Some(marker),
                    ..
                } = &request
                    && let Some(marker) = marker.strip_prefix("fixture-stall:")
                {
                    std::fs::write(marker, b"entered Workbench request")?;
                    loop {
                        std::thread::park();
                    }
                }
                coordinator.maintain()?;
                let result = coordinator
                    .request(request, &executable, ENVELOPE_BYTES)
                    .map_err(|error| {
                        if coordinator.fatal() {
                            Failure::fatal(error)
                        } else {
                            Failure::new(error)
                        }
                    });
                if let Err(error) = &result {
                    error.validate()?;
                }
                write_packet(
                    &mut *output.lock().unwrap_or_else(|e| e.into_inner()),
                    &Outcome::Reply {
                        sequence,
                        request_digest: expected,
                        result,
                    },
                )?;
            }
            Work::Shutdown { sequence } => {
                ensure!(sequence.0 > 0, "Workbench shutdown sequence");
                let drain_callback = callback.clone();
                let drain_output = output.clone();
                let instance = startup.instance.clone();
                let retained = Arc::new(Mutex::new(Some(coordinator)));
                let drain_owner = retained.clone();
                let drain = thread::Builder::new()
                    .name("workbench-checked-drain".into())
                    .spawn(move || {
                        let Some(mut coordinator) = drain_owner
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .take()
                        else {
                            drain_callback.close();
                            std::process::exit(1);
                        };
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            coordinator.shutdown()
                        }));
                        if !matches!(result, Ok(Ok(()))) {
                            drain_callback.close();
                            // No Drained acknowledgement can escape a failed
                            // or panicked SQLite close. OS teardown releases
                            // retained W handles and the parent proves exit.
                            std::process::exit(1);
                        }
                        drain_callback.close();
                        write_packet(
                            &mut *drain_output
                                .lock()
                                .unwrap_or_else(|error| error.into_inner()),
                            &Outcome::Drained { sequence, instance },
                        )
                    });
                let drain = match drain {
                    Ok(drain) => drain,
                    Err(_) => {
                        callback.close();
                        // `retained` still owns Coordinator here. Exiting
                        // without unwinding prevents its blocking Drop from
                        // re-entering the callback path on this input loop.
                        std::process::exit(1);
                    }
                };
                drop(retained);
                loop {
                    let Some(work) = read_packet::<Work>(&mut input)? else {
                        break;
                    };
                    match work {
                        Work::CallbackBegin {
                            sequence,
                            bytes,
                            blake3,
                        } => callback.reply(sequence, bytes, blake3)?,
                        Work::CallbackChunk {
                            sequence,
                            offset,
                            bytes,
                        } => callback.chunk(sequence, offset, bytes)?,
                        _ => anyhow::bail!("only callback replies are accepted while draining"),
                    }
                }
                return drain
                    .join()
                    .map_err(|_| anyhow::anyhow!("Workbench checked drain panicked"))?;
            }
            Work::CallbackBegin {
                sequence,
                bytes,
                blake3,
            } => {
                ensure!(startup.managed, "callback reply for unmanaged Workbench");
                callback.reply(sequence, bytes, blake3)?;
            }
            Work::CallbackChunk {
                sequence,
                offset,
                bytes,
            } => {
                ensure!(startup.managed, "callback reply for unmanaged Workbench");
                callback.chunk(sequence, offset, bytes)?;
            }
        }
    }
}

struct Transport {
    input: ChildStdin,
    output: Box<dyn Read + Send>,
    sequence: u64,
}

#[cfg(test)]
struct HarnessOutput {
    inner: ChildStdout,
    prefix: Vec<u8>,
    ready: bool,
    copied: usize,
}
#[cfg(test)]
impl Read for HarnessOutput {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        while !self.ready {
            let mut byte = [0];
            if self.inner.read(&mut byte)? == 0 {
                return Ok(0);
            }
            self.prefix.push(byte[0]);
            if self.prefix.ends_with(MAGIC) {
                self.ready = true;
            } else if self.prefix.len() == 512 {
                return Err(std::io::Error::other(
                    "Workbench fixture harness prefix exceeded 512 bytes",
                ));
            }
        }
        if self.copied < MAGIC.len() {
            let size = output.len().min(MAGIC.len() - self.copied);
            output[..size].copy_from_slice(&MAGIC[self.copied..self.copied + size]);
            self.copied += size;
            return Ok(size);
        }
        self.inner.read(output)
    }
}

struct Owner {
    transport: Option<Transport>,
    drained: bool,
    poisoned: Option<String>,
}
impl Owner {
    fn revoke(&mut self, child_owner: &Mutex<Option<Child>>) -> Result<()> {
        drop(self.transport.take());
        let mut slot = child_owner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = slot.as_mut() {
            if child.try_wait()?.is_none() {
                child.kill().context("revoke Workbench child")?;
            }
            child.wait()?;
            slot.take();
        }
        Ok(())
    }
}

struct StartingChild(Option<Child>);
impl StartingChild {
    fn reap(&mut self) -> Result<()> {
        if let Some(child) = self.0.as_mut() {
            if child.try_wait()?.is_none() {
                child.kill().context("revoke starting Workbench child")?;
            }
            child.wait().context("reap starting Workbench child")?;
            self.0.take();
        }
        Ok(())
    }
}
impl Drop for StartingChild {
    fn drop(&mut self) {
        while self.0.is_some() {
            let _ = self.reap();
            if self.0.is_some() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

/// G-owned W child. Calls are serialized and bounded, while W's own operation
/// thread keeps Status/Cancel/Close requests responsive during long actions.
pub struct Client {
    instance: crate::catalog_session::LeaseId,
    owner: Mutex<Owner>,
    child: Mutex<Option<Child>>,
    interrupted: AtomicBool,
    managed: Option<Arc<dyn super::lightroom::ManagedIo>>,
}
impl Client {
    pub fn spawn(executable: &Path) -> Result<Self> {
        Self::spawn_inner(executable, None)
    }
    #[allow(dead_code)]
    pub(crate) fn spawn_managed(
        executable: &Path,
        managed: &Arc<dyn super::lightroom::ManagedIo>,
    ) -> Result<Self> {
        // The caller retains the authoritative owner if W startup fails and
        // can explicitly drain/retry it; an app-setup error cannot consume F/S.
        Self::spawn_inner(executable, Some(managed.clone()))
    }
    fn spawn_inner(
        executable: &Path,
        managed: Option<Arc<dyn super::lightroom::ManagedIo>>,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command.arg("--lightroom-workbench-worker");
        Self::spawn_command(command, managed, false)
    }
    #[cfg(test)]
    pub(crate) fn spawn_managed_fixture(
        executable: &Path,
        managed: &Arc<dyn super::lightroom::ManagedIo>,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command.args([
            "--ignored",
            "--exact",
            "application::lightroom_process::tests::owned_workbench_entrypoint",
            "--nocapture",
            "--test-threads=1",
        ]);
        Self::spawn_command(command, Some(managed.clone()), true)
    }
    fn spawn_command(
        mut command: Command,
        managed: Option<Arc<dyn super::lightroom::ManagedIo>>,
        fixture_harness: bool,
    ) -> Result<Self> {
        let startup = Startup::new(managed.is_some());
        startup.validate()?;
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if fixture_harness {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .spawn()?;
        let mut starting = StartingChild(Some(child));
        let bootstrap = (|| -> Result<(Child, ChildStdin, Box<dyn Read + Send>)> {
            let child = starting
                .0
                .as_mut()
                .context("Workbench startup child missing")?;
            let mut input = child.stdin.take().context("Workbench stdin missing")?;
            let output = child.stdout.take().context("Workbench stdout missing")?;
            #[cfg(test)]
            let mut output: Box<dyn Read + Send> = if fixture_harness {
                Box::new(HarnessOutput {
                    inner: output,
                    prefix: Vec::with_capacity(512),
                    ready: false,
                    copied: 0,
                })
            } else {
                Box::new(output)
            };
            #[cfg(not(test))]
            let mut output: Box<dyn Read + Send> = {
                let _ = fixture_harness;
                Box::new(output)
            };
            write_packet(&mut input, &startup)?;
            let ready: Outcome = read_packet(&mut output)?.context("Workbench Ready missing")?;
            ensure!(
                matches!(
                    ready,
                    Outcome::Ready { ref instance, ref build }
                        if instance == &startup.instance && build == &startup.build
                ),
                "Workbench Ready binding mismatch"
            );
            let child = starting.0.take().context("Workbench startup owner lost")?;
            Ok((child, input, output))
        })();
        let (child, input, output) = match bootstrap {
            Ok(ready) => ready,
            Err(primary) => match starting.reap() {
                Ok(()) => return Err(primary),
                Err(cleanup) => {
                    return Err(primary.context(format!(
                        "Workbench startup checked reap also failed: {cleanup:#}"
                    )));
                }
            },
        };
        Ok(Self {
            instance: startup.instance,
            managed,
            child: Mutex::new(Some(child)),
            interrupted: AtomicBool::new(false),
            owner: Mutex::new(Owner {
                transport: Some(Transport {
                    input,
                    output,
                    sequence: 0,
                }),
                drained: false,
                poisoned: None,
            }),
        })
    }

    fn callback(
        managed: &dyn super::lightroom::ManagedIo,
        request: CallbackRequest,
    ) -> Result<CallbackValue> {
        let cancel = AtomicBool::new(false);
        match request {
            CallbackRequest::Admit => {
                managed.admit()?;
                Ok(CallbackValue::Admitted)
            }
            CallbackRequest::Commit => {
                managed.commit()?;
                Ok(CallbackValue::Admitted)
            }
            CallbackRequest::Filesystem { request } => Ok(CallbackValue::Filesystem(
                managed.filesystem(request, &cancel)?,
            )),
            CallbackRequest::SourceOpen { authority } => Ok(CallbackValue::Source(
                managed.source_open(authority, Arc::new(cancel))?,
            )),
            CallbackRequest::SourceSqlOpen {
                seal,
                limits,
                protected,
            } => Ok(CallbackValue::Source(managed.source_sql_open(
                seal,
                limits,
                protected,
                Arc::new(cancel),
            )?)),
            CallbackRequest::SourceSchema { source } => {
                Ok(CallbackValue::Schema(managed.source_schema(&source)?))
            }
            CallbackRequest::SourceRows {
                source,
                handle,
                cursor,
                limit,
            } => Ok(CallbackValue::Table(managed.source_rows(
                &source,
                handle,
                cursor,
                usize::try_from(limit.0)?,
            )?)),
            CallbackRequest::SourceCurrent { source } => {
                Ok(CallbackValue::Current(managed.source_current(&source)?))
            }
            CallbackRequest::SourceRetire { source } => {
                managed.source_retire(&source)?;
                Ok(CallbackValue::Retired)
            }
        }
    }

    fn reply_callback(
        transport: &mut Transport,
        sequence: crate::application::U64,
        request: CallbackRequest,
        managed: Option<&Arc<dyn super::lightroom::ManagedIo>>,
    ) -> Result<()> {
        let fatal = matches!(&request, CallbackRequest::Admit | CallbackRequest::Commit);
        let result = managed
            .context("unmanaged Workbench requested a supervisor callback")
            .and_then(|managed| Self::callback(managed.as_ref(), request))
            .map_err(|error| {
                if fatal {
                    Failure::fatal(error)
                } else {
                    Failure::new(error)
                }
            });
        if let Err(error) = &result {
            error.validate()?;
        }
        let encoded = encode_limit(&result, CALLBACK_BYTES)?;
        let digest = crate::lightroom::digest(&encoded);
        write_packet(
            &mut transport.input,
            &Work::CallbackBegin {
                sequence,
                bytes: crate::application::U64(encoded.len().try_into()?),
                blake3: digest,
            },
        )?;
        for (index, bytes) in encoded.chunks(CALLBACK_CHUNK_BYTES).enumerate() {
            write_packet(
                &mut transport.input,
                &Work::CallbackChunk {
                    sequence,
                    offset: crate::application::U64((index * CALLBACK_CHUNK_BYTES) as u64),
                    bytes: bytes.to_vec(),
                },
            )?;
        }
        Ok(())
    }

    fn terminal_failure(&self, error: anyhow::Error, kind: &str) -> anyhow::Error {
        if let Some(managed) = &self.managed {
            managed.revoke_generation();
        }
        let cleanup = self.shutdown();
        if self.checked_drained() {
            error.context(format!(
                "fatal Workbench {kind} checked-drained{}",
                cleanup
                    .err()
                    .map(|value| format!(" after {value:#}"))
                    .unwrap_or_default()
            ))
        } else {
            error.context(format!(
                "fatal Workbench {kind} remains nonterminal; checked drain failed: {:#}",
                cleanup.expect_err("undrained shutdown must report failure")
            ))
        }
    }

    /// Interrupt the exact retained process without waiting for a blocked
    /// transport lock. Reaping and filesystem reconciliation remain in shutdown.
    pub(crate) fn interrupt(&self) -> Result<()> {
        self.interrupted
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(managed) = &self.managed {
            managed.revoke_generation();
        }
        let mut slot = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = slot.as_mut() {
            // Do not wait here: Source owners must be checked-drained first.
            // An already exited child is reconciled by the same shutdown path.
            if let Err(error) = child.kill() {
                if error.kind() != std::io::ErrorKind::InvalidInput {
                    return Err(error).context("interrupt Workbench child");
                }
            }
        }
        Ok(())
    }

    pub fn call(&self, request: Request) -> Result<Response> {
        ensure!(
            !self.interrupted.load(std::sync::atomic::Ordering::Acquire),
            "Workbench generation was interrupted"
        );
        if let Some(managed) = &self.managed
            && let Err(error) = managed.admit()
        {
            return Err(self.terminal_failure(error, "admission"));
        }
        let digest = request_digest(&request)?;
        let mut owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(!owner.drained, "Workbench process is drained");
        ensure!(
            owner.poisoned.is_none(),
            "Workbench process is poisoned: {}",
            owner.poisoned.as_deref().unwrap_or_default()
        );
        let transport_result = (|| -> Result<std::result::Result<Response, Failure>> {
            let transport = owner
                .transport
                .as_mut()
                .context("Workbench transport is retained after failure")?;
            transport.sequence = transport
                .sequence
                .checked_add(1)
                .context("Workbench sequence exhausted")?;
            let sequence = crate::application::U64(transport.sequence);
            write_packet(
                &mut transport.input,
                &Work::Request {
                    sequence,
                    request_digest: digest.clone(),
                    request,
                },
            )?;
            let result = loop {
                let outcome: Outcome = read_packet(&mut transport.output)?
                    .context("Workbench reply lost; child remains owned for checked drain")?;
                match outcome {
                    Outcome::Callback { sequence, request } => {
                        Self::reply_callback(transport, sequence, request, self.managed.as_ref())?;
                    }
                    Outcome::Reply {
                        sequence: actual,
                        request_digest: actual_digest,
                        result,
                    } => {
                        ensure!(
                            actual == sequence && actual_digest == digest,
                            "Workbench reply binding mismatch"
                        );
                        break result;
                    }
                    _ => anyhow::bail!("unexpected Workbench outcome"),
                }
            };
            Ok(result)
        })();
        let result = match transport_result {
            Ok(result) => result,
            Err(error) => {
                owner.poisoned = Some(format!("{error:#}"));
                drop(owner);
                return Err(self.terminal_failure(error, "transport"));
            }
        };
        match result {
            Ok(response) => Ok(response),
            Err(failure) => {
                if failure.fatal {
                    owner.poisoned = Some(failure.detail.clone());
                    drop(owner);
                    return Err(self.terminal_failure(anyhow::anyhow!(failure.detail), "reply"));
                }
                Err(anyhow::anyhow!(failure.detail))
            }
        }
    }

    pub fn shutdown(&self) -> Result<()> {
        let source_result = self
            .managed
            .as_ref()
            .map(|managed| managed.drain_sources())
            .unwrap_or(Ok(()));
        let mut owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        if owner.drained {
            return source_result;
        }
        let graceful = (|| -> Result<()> {
            ensure!(
                owner.poisoned.is_none()
                    && !self.interrupted.load(std::sync::atomic::Ordering::Acquire),
                "poisoned or interrupted Workbench requires checked revoke"
            );
            if self
                .child
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
            {
                return Ok(());
            }
            let transport = owner
                .transport
                .as_mut()
                .context("Workbench transport is retained after failure")?;
            transport.sequence = transport
                .sequence
                .checked_add(1)
                .context("Workbench sequence exhausted")?;
            let sequence = crate::application::U64(transport.sequence);
            write_packet(&mut transport.input, &Work::Shutdown { sequence })?;
            loop {
                let outcome: Outcome = read_packet(&mut transport.output)?
                    .context("Workbench drain acknowledgement lost")?;
                match outcome {
                    Outcome::Callback { sequence, request } => {
                        Self::reply_callback(transport, sequence, request, self.managed.as_ref())?
                    }
                    Outcome::Drained {
                        sequence: actual,
                        ref instance,
                    } => {
                        ensure!(
                            actual == sequence && instance == &self.instance,
                            "Workbench drain binding mismatch"
                        );
                        break;
                    }
                    _ => anyhow::bail!("unexpected Workbench drain outcome"),
                }
            }
            drop(owner.transport.take());
            let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
            let status = child
                .as_mut()
                .context("Workbench child missing before reap")?
                .wait()?;
            ensure!(status.success(), "Workbench child exited {status}");
            child.take();
            Ok(())
        })();
        let revoke = if graceful.is_err() {
            owner.revoke(&self.child)
        } else {
            Ok(())
        };
        let reaped = self
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
            && owner.transport.is_none();
        if reaped && let Some(managed) = &self.managed {
            managed.workbench_reaped();
        }
        let filesystem_result = if source_result.is_ok() && reaped {
            self.managed
                .as_ref()
                .map(|managed| managed.drain_filesystem())
                .unwrap_or(Ok(()))
        } else {
            Err(anyhow::anyhow!(
                "F reconciliation retained until Source and W are checked-drained"
            ))
        };
        owner.drained = reaped && source_result.is_ok() && filesystem_result.is_ok();
        let mut failures = Vec::new();
        if let Err(error) = source_result {
            failures.push(format!("Source checked drain: {error:#}"));
        }
        if let Err(error) = graceful {
            failures.push(format!("Workbench graceful drain: {error:#}"));
        }
        if let Err(error) = revoke {
            failures.push(format!("Workbench checked revoke: {error:#}"));
        }
        if let Err(error) = filesystem_result {
            failures.push(format!("filesystem reconciliation: {error:#}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("managed Workbench drain failed: {}", failures.join("; "))
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(Child::id)
    }

    pub(crate) fn checked_drained(&self) -> bool {
        self.owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drained
    }

    #[cfg(test)]
    pub(crate) fn terminate_for_test(&self) -> Result<()> {
        let mut slot = self.child.lock().unwrap_or_else(|error| error.into_inner());
        let child = slot.as_mut().context("Workbench child missing")?;
        child.kill().context("terminate Workbench fixture")
    }

    #[cfg(test)]
    pub(crate) fn poison_for_test(&self, detail: &str) {
        self.owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .poisoned = Some(detail.into());
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            {
                let mut owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
                while self
                    .child
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some()
                {
                    let _ = owner.revoke(&self.child);
                    if self
                        .child
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_some()
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            }
            if let Some(managed) = self.managed.take() {
                managed.workbench_reaped();
                let cleanup = managed
                    .drain_sources()
                    .and_then(|()| managed.drain_filesystem());
                if cleanup.is_err() {
                    std::mem::forget(managed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "owned Workbench fixture subprocess entrypoint"]
    fn owned_workbench_entrypoint() {
        match worker_main() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("owned Workbench fixture failed: {error:#}");
                std::process::exit(1);
            }
        }
    }

    #[test]
    fn frame_round_trip_rejects_corruption_and_oversize() -> Result<()> {
        let startup = Startup::new(false);
        let mut bytes = Vec::new();
        write_packet(&mut bytes, &startup)?;
        let decoded: Startup = read_packet(&mut bytes.as_slice())?.context("frame")?;
        decoded.validate()?;
        assert_eq!(decoded.instance, startup.instance);
        let end = bytes.len() - 1;
        bytes[end] ^= 1;
        assert!(read_packet::<Startup>(&mut bytes.as_slice()).is_err());
        let mut oversized = Vec::from(*MAGIC);
        oversized.extend_from_slice(&[PROTOCOL, 0, 0, 0]);
        oversized.extend_from_slice(&u32::MAX.to_le_bytes());
        oversized.extend_from_slice(&[0; 36]);
        assert!(read_packet::<Startup>(&mut oversized.as_slice()).is_err());
        Ok(())
    }
}
