//! Private G/C migration admission. C keeps the catalog and its actual writer
//! permit; every permit is acquired and dropped on one dedicated owner thread.
//! Transport request IDs may change on retry, but operation/sequence/digest may not.
use super::{Bridge, BridgeError, Cancellation, ErrorCode, error};
use crate::{
    application::{Actor, Envelope as WorkEnvelope, U64, Work},
    catalog_session::{CatalogSessionAuthority, PhysicalObjectId},
    catalog_writer::{Priority, Writers},
    lightroom_migration_worker::{
        identity::FileKey,
        lease::DestinationPin,
        protocol::{Guard, WriteKind},
    },
    storage_volume::NativePath,
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

type Result<T> = std::result::Result<T, BridgeError>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub guard: Guard,
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    InspectTarget {
        catalog: String,
        destination: NativePath,
    },
    AcquireTarget {
        catalog: Option<String>,
        destination: NativePath,
        expected: Option<DestinationPin>,
    },
    AcquireWrite {
        sequence: U64,
        kind: WriteKind,
        target: String,
        lock: Option<FileKey>,
        request_digest: String,
    },
    ReleaseWrite {
        sequence: U64,
        kind: WriteKind,
        request_digest: String,
    },
    Status,
    Cancel,
    Progress {
        phase: String,
        completed: U64,
        total: Option<U64>,
    },
    /// Only G's checked LM + SQL/Raw drain may send this acknowledgement.
    DrainOperation,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Inspected,
    Target,
    Attempted,
    Held,
    Releasing,
    Released,
    Draining,
    Drained,
    Failed,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub guard: Guard,
    pub phase: Phase,
    pub catalog: Option<String>,
    pub destination: Option<DestinationPin>,
    pub sequence: Option<U64>,
    pub request_digest: Option<String>,
    pub write_kind: Option<WriteKind>,
    pub cancel_requested: bool,
    pub progress: Option<(String, U64, Option<U64>)>,
    pub failure: Option<BridgeError>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Reply {
    Ok(Snapshot),
    Error(BridgeError),
}
// Bound raw error text before it enters the retained relay state. Sixfold JSON
// escaping plus the bounded guard/attempt fields fits comfortably in one frame.
const FAILURE_BYTES: usize = 1024;
struct Text(String);
impl std::fmt::Write for Text {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let mut end = value.len().min(FAILURE_BYTES - self.0.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push_str(&value[..end]);
        if end == value.len() {
            Ok(())
        } else {
            Err(std::fmt::Error)
        }
    }
}
fn bound_error(failure: &mut BridgeError) {
    let mut end = failure.message.len().min(FAILURE_BYTES);
    while !failure.message.is_char_boundary(end) {
        end -= 1;
    }
    failure.message.truncate(end);
    // Release excess backing as well as truncating the visible text.
    failure.message = std::mem::take(&mut failure.message)
        .into_boxed_str()
        .into_string();
}
fn bounded_error(mut failure: BridgeError) -> BridgeError {
    bound_error(&mut failure);
    failure
}
fn refusal(code: ErrorCode, cause: impl std::fmt::Display) -> BridgeError {
    let mut text = Text(String::new());
    let _ = std::fmt::write(&mut text, format_args!("{cause}"));
    BridgeError {
        code,
        message: text.0,
    }
}
struct FrameSink {
    bytes: Vec<u8>,
    limit: usize,
}
impl std::io::Write for FrameSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(std::io::Error::other("migration message byte allowance"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct Count {
    bytes: usize,
    limit: usize,
}
impl std::io::Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit - self.bytes {
            return Err(std::io::Error::other("migration message byte allowance"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encoded_len(value: &impl Serialize, limit: usize) -> Result<usize> {
    let mut count = Count { bytes: 0, limit };
    serde_json::to_writer(&mut count, value)
        .map_err(|_| refusal(ErrorCode::ResourceLimit, "migration message byte allowance"))?;
    Ok(count.bytes)
}
fn encode_bounded(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    let length = encoded_len(value, limit)?;
    let mut sink = FrameSink {
        bytes: Vec::with_capacity(length),
        limit: length,
    };
    serde_json::to_writer(&mut sink, value)
        .map_err(|_| refusal(ErrorCode::ResourceLimit, "migration message byte allowance"))?;
    Ok(sink.bytes)
}
// Small identity/digest records retain their independent control allowance.
fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    encode_bounded(value, super::wire::CHUNK)
}
impl Reply {
    pub(super) fn validate(&self) -> Result<()> {
        let failure = match self {
            Self::Error(failure) => Some(failure),
            Self::Ok(snapshot) => {
                snapshot.guard.validate().map_err(invalid)?;
                if snapshot.catalog.as_ref().is_some_and(|v| v.len() > 128)
                    || snapshot
                        .request_digest
                        .as_ref()
                        .is_some_and(|v| v.len() != 64)
                    || snapshot
                        .progress
                        .as_ref()
                        .is_some_and(|(phase, completed, total)| {
                            phase.len() > 128 || total.is_some_and(|total| completed.0 > total.0)
                        })
                {
                    return Err(invalid("migration snapshot field bounds"));
                }
                if let Some(pin) = &snapshot.destination {
                    let units = match &pin.root {
                        NativePath::UnixBytes(v) => v.len(),
                        NativePath::WindowsWide(v) => v.len(),
                    };
                    if !(1..=crate::catalog_session::PATH_UNITS).contains(&units) {
                        return Err(invalid("migration reply native path allowance"));
                    }
                }
                snapshot.failure.as_ref()
            }
        };
        if failure.is_some_and(|failure| failure.message.len() > FAILURE_BYTES) {
            return Err(invalid("migration reply failure allowance"));
        }
        Ok(())
    }
    /// Complete pins are never removed to make a success fit. Target admission
    /// reserves room for its largest recovery snapshot before granting authority.
    pub(super) fn message(mut self, id: u64, limit: usize) -> super::wire::Message {
        match &mut self {
            Self::Error(failure) => bound_error(failure),
            Self::Ok(snapshot) => {
                if let Some(failure) = snapshot.failure.take() {
                    snapshot.failure = Some(bounded_error(failure));
                }
            }
        }
        let allowance = if matches!(self, Self::Error(_)) {
            super::wire::CHUNK
        } else {
            limit
        };
        let bytes = encode_bounded(&self, allowance).unwrap_or_else(|_| {
            encode(&Self::Error(refusal(ErrorCode::ResourceLimit,
                "migration reply exceeds configured allowance; retained authority requires recovery")))
                .expect("bounded refusal fits control allowance")
        });
        super::wire::Message::new(super::wire::Kind::MigrationReply, id, bytes)
    }
    fn from_result(result: Result<Snapshot>) -> Self {
        match result {
            Ok(value) => Self::Ok(value),
            Err(error) => Self::Error(bounded_error(error)),
        }
    }
}
pub(crate) struct Pending {
    pub receiver: mpsc::Receiver<Reply>,
    pub cancel: Cancellation,
}
impl Action {
    pub(crate) fn recovery(&self) -> bool {
        matches!(
            self,
            Self::ReleaseWrite { .. } | Self::Status | Self::Cancel | Self::DrainOperation
        )
    }
}
impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        self.guard.validate().map_err(invalid)?;
        match &self.action {
            Action::InspectTarget {
                catalog,
                destination,
            } => {
                if catalog.is_empty() || catalog.len() > 128 {
                    return Err(invalid("migration catalog token allowance"));
                }
                let units = match destination {
                    NativePath::UnixBytes(v) => v.len(),
                    NativePath::WindowsWide(v) => v.len(),
                };
                if !(1..=crate::catalog_session::PATH_UNITS).contains(&units) {
                    return Err(invalid("migration native path allowance"));
                }
            }
            Action::AcquireWrite {
                target,
                request_digest,
                ..
            } => {
                if target.len() > 64 || request_digest.len() != 64 {
                    return Err(invalid("migration write identity bounds"));
                }
            }
            Action::ReleaseWrite { request_digest, .. } if request_digest.len() != 64 => {
                return Err(invalid("migration release identity bounds"));
            }
            _ => {}
        }
        if let Action::Progress {
            phase,
            completed,
            total,
        } = &self.action
        {
            if phase.len() > 128 || total.is_some_and(|total| completed.0 > total.0) {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "migration progress bounds",
                ));
            }
        }
        if let Action::AcquireTarget {
            catalog,
            destination,
            expected,
        } = &self.action
        {
            if catalog.as_ref().is_some_and(|catalog| catalog.len() > 128) {
                return Err(invalid("migration catalog token allowance"));
            }
            for path in std::iter::once(destination).chain(expected.iter().map(|pin| &pin.root)) {
                let units = match path {
                    NativePath::UnixBytes(v) => v.len(),
                    NativePath::WindowsWide(v) => v.len(),
                };
                if !(1..=crate::catalog_session::PATH_UNITS).contains(&units) {
                    return Err(invalid("migration native path allowance"));
                }
            }
        }
        Ok(())
    }
}
fn invalid(e: impl std::fmt::Display) -> BridgeError {
    refusal(ErrorCode::InvalidRequest, e)
}
fn native(e: impl std::fmt::Display) -> BridgeError {
    refusal(ErrorCode::Native, e)
}

fn request_cap(request: &Request, limits: &crate::application::Limits) -> usize {
    if request.action.recovery() {
        super::wire::CHUNK
    } else {
        limits.request_bytes
    }
}

/// Reserve a complete pin and the largest bounded recovery fields before C
/// acquires target authority or creates a prospective destination. Native units
/// are counted through serde, including full Windows-wide JSON expansion.
fn admit_reply(guard: &Guard, root: &NativePath, limit: usize) -> Result<()> {
    let key = FileKey {
        volume: U64(u64::MAX),
        index: U64(u64::MAX),
    };
    let largest = Reply::Ok(Snapshot {
        guard: guard.clone(),
        phase: Phase::Releasing,
        catalog: Some("c".repeat(36)),
        destination: Some(DestinationPin {
            root: root.clone(),
            root_key: key.clone(),
            database_key: key,
            schema: crate::application::I64(i64::MIN),
        }),
        sequence: Some(U64(u64::MAX)),
        request_digest: Some("d".repeat(64)),
        write_kind: Some(WriteKind::Bootstrap),
        cancel_requested: false,
        progress: Some(("\0".repeat(128), U64(u64::MAX), Some(U64(u64::MAX)))),
        failure: Some(refusal(
            ErrorCode::InvalidRequest,
            "\0".repeat(FAILURE_BYTES),
        )),
    });
    encoded_len(&largest, limit).map(|_| ())
}

/// This digest binds the actual write request, independent of desktop frame ID.
/// Release and Status must recover the same attempt after an acknowledgement loss.
pub(crate) fn write_digest(
    guard: &Guard,
    sequence: U64,
    kind: WriteKind,
    target: &str,
    lock: Option<&FileKey>,
) -> Result<String> {
    Ok(
        blake3::hash(&encode(&(guard, sequence, kind, target, lock))?)
            .to_hex()
            .to_string(),
    )
}

struct PermitState {
    phase: Phase,
    release: bool,
    failure: Option<BridgeError>,
}
struct PermitShared {
    state: Mutex<PermitState>,
    wake: Condvar,
    cancel: AtomicBool,
}
struct PermitOwner {
    shared: Arc<PermitShared>,
    owner: Option<thread::JoinHandle<()>>,
    writers: Arc<Writers>,
}
impl PermitOwner {
    fn start(
        writers: Arc<Writers>,
        verify: impl Fn() -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let shared = Arc::new(PermitShared {
            state: Mutex::new(PermitState {
                phase: Phase::Attempted,
                release: false,
                failure: None,
            }),
            wake: Condvar::new(),
            cancel: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let gate = writers.clone();
        let owner = thread::Builder::new()
            .name("migration-catalog-permit".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let permit =
                        match gate.enter_cancellable(Priority::Background, &worker.cancel, None) {
                            Ok(permit) => permit,
                            Err(cause) => {
                                let mut state =
                                    worker.state.lock().unwrap_or_else(|e| e.into_inner());
                                state.phase = Phase::Failed;
                                state.failure = Some(if worker.cancel.load(Ordering::Acquire) {
                                    error(
                                        ErrorCode::Canceled,
                                        "migration writer acquisition canceled",
                                    )
                                } else {
                                    native(cause)
                                });
                                worker.wake.notify_all();
                                return;
                            }
                        };
                    if let Err(failure) = verify() {
                        drop(permit);
                        let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.phase = Phase::Failed;
                        state.failure = Some(bounded_error(failure));
                        worker.wake.notify_all();
                        return;
                    }
                    let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.phase = if state.release {
                        Phase::Releasing
                    } else {
                        Phase::Held
                    };
                    worker.wake.notify_all();
                    // Cancellation cannot release a held permit. G first reaps all
                    // descendants, or LM explicitly releases after its transaction.
                    while !state.release {
                        state = worker.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    drop(state);
                    drop(permit);
                    let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.phase = Phase::Released;
                    worker.wake.notify_all();
                }));
                if outcome.is_err() {
                    let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.phase = Phase::Failed;
                    state.failure = Some(native(
                        "migration permit owner panicked; checked join required",
                    ));
                    worker.wake.notify_all();
                }
            })
            .map_err(native)?;
        Ok(Self {
            shared,
            owner: Some(owner),
            writers,
        })
    }
    fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Release);
        self.writers.wake_waiters();
    }
    fn release(&self) {
        self.cancel();
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.release = true;
        if matches!(state.phase, Phase::Held | Phase::Attempted) {
            state.phase = Phase::Releasing;
        }
        self.shared.wake.notify_all();
    }
    fn snapshot(&self) -> (Phase, Option<BridgeError>) {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.phase.clone(), state.failure.clone())
    }
    fn join_ready(&mut self) -> Result<bool> {
        if !self.owner.as_ref().is_none_or(|owner| owner.is_finished()) {
            return Ok(false);
        }
        if let Some(owner) = self.owner.take() {
            owner
                .join()
                .map_err(|_| native("migration permit join failed"))?;
        }
        Ok(true)
    }
}
impl Drop for PermitOwner {
    fn drop(&mut self) {
        self.cancel();
        // A dropped façade has no proof that LM/Source transactions are gone.
        // Preserve the dedicated thread and its shared control if not joined.
        if self.owner.is_some() {
            std::mem::forget(self.owner.take());
            std::mem::forget(self.shared.clone());
            std::mem::forget(self.writers.clone());
        }
    }
}

struct Attempt {
    sequence: U64,
    kind: WriteKind,
    digest: String,
    owner: Option<PermitOwner>,
    failure: Option<BridgeError>,
}
// Prospective spelling/metadata is not catalog authority. Only F's later
// managed Create produces authoritative physical identities on every platform.
struct Prospective {
    parent: PathBuf,
    destination: NativePath,
    #[cfg(unix)]
    parent_identity: (u64, u64),
}
impl Prospective {
    fn new(destination: &NativePath) -> Result<Self> {
        let path = destination.to_path().map_err(invalid)?;
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(invalid("absolute destination with a filename required"));
        }
        let parent = path.parent().ok_or_else(|| invalid("destination parent"))?;
        crate::lightroom::source::reject_links(parent).map_err(invalid)?;
        if path.try_exists().map_err(native)? {
            return Err(invalid("prospective migration target already exists"));
        }
        let metadata = std::fs::symlink_metadata(parent).map_err(native)?;
        if !metadata.is_dir() {
            return Err(invalid("destination parent is not a directory"));
        }
        #[cfg(unix)]
        let parent_identity = {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        };
        Ok(Self {
            parent: std::fs::canonicalize(parent).map_err(native)?,
            destination: destination.clone(),
            #[cfg(unix)]
            parent_identity,
        })
    }
    fn verify(&self) -> Result<()> {
        let current = Self::new(&self.destination)?;
        if current.parent != self.parent {
            return Err(invalid("prospective parent spelling changed"));
        }
        #[cfg(unix)]
        if current.parent_identity != self.parent_identity {
            return Err(invalid("prospective parent identity changed"));
        }
        Ok(())
    }
}
fn file_key(physical: PhysicalObjectId) -> Result<FileKey> {
    physical.validate().map_err(invalid)?;
    match physical {
        #[cfg(unix)]
        PhysicalObjectId::Unix { device, inode } => Ok(FileKey {
            volume: device,
            index: inode,
        }),
        #[cfg(windows)]
        PhysicalObjectId::Windows {
            volume_serial,
            file_index,
        } => Ok(FileKey {
            volume: volume_serial,
            index: file_index,
        }),
        #[allow(unreachable_patterns)]
        _ => Err(invalid("foreign migration physical identity")),
    }
}
fn physical_key(key: &FileKey) -> Result<PhysicalObjectId> {
    #[cfg(unix)]
    let physical = PhysicalObjectId::Unix {
        device: key.volume,
        inode: key.index,
    };
    #[cfg(windows)]
    let physical = PhysicalObjectId::Windows {
        volume_serial: key.volume,
        file_index: key.index,
    };
    physical.validate().map_err(invalid)?;
    Ok(physical)
}
fn managed_pin(open: &crate::application::Open) -> Result<DestinationPin> {
    let managed = open
        .managed
        .as_ref()
        .ok_or_else(|| invalid("migration relay requires a managed catalog"))?;
    let root = managed.bootstrap.root_capability();
    // The existing Actor SQL role owns this observation; no raw File or
    // independent SQLite connection may enter C for migration identity checks.
    let schema = open
        .catalog
        .db
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(native)?;
    Ok(DestinationPin {
        root: root.canonical_root,
        root_key: file_key(root.root_physical)?,
        database_key: file_key(root.catalog_physical)?,
        schema: crate::application::I64(schema),
    })
}
struct Target {
    guard: Guard,
    acquire_digest: String,
    catalog: Option<String>,
    destination: NativePath,
    pin: Option<DestinationPin>,
    authority: Option<Arc<CatalogSessionAuthority>>,
    prospective: Option<Prospective>,
    writers: Option<Arc<Writers>>,
    attempt: Option<Attempt>,
    canceled: bool,
    draining: bool,
    progress: Option<(String, U64, Option<U64>)>,
}
impl Target {
    fn snapshot(&self) -> Snapshot {
        let (phase, failure) = self
            .attempt
            .as_ref()
            .map(|attempt| {
                attempt
                    .owner
                    .as_ref()
                    .map(PermitOwner::snapshot)
                    .unwrap_or((Phase::Failed, attempt.failure.clone()))
            })
            .unwrap_or((Phase::Target, None));
        Snapshot {
            guard: self.guard.clone(),
            phase: if self.draining {
                Phase::Draining
            } else {
                phase
            },
            catalog: self.catalog.clone(),
            destination: self.pin.clone(),
            sequence: self.attempt.as_ref().map(|attempt| attempt.sequence),
            request_digest: self.attempt.as_ref().map(|attempt| attempt.digest.clone()),
            write_kind: self.attempt.as_ref().map(|attempt| attempt.kind),
            cancel_requested: self.canceled,
            progress: self.progress.clone(),
            failure,
        }
    }
    fn cancel(&mut self) {
        self.canceled = true;
        if let Some(owner) = self
            .attempt
            .as_ref()
            .and_then(|attempt| attempt.owner.as_ref())
        {
            owner.cancel();
        }
    }
}
#[derive(Default)]
pub(crate) struct Admission {
    target: Option<Target>,
    // Last operation tombstone permits a lost Drain reply to be recovered.
    drained: Option<Snapshot>,
}
impl Admission {
    pub(crate) fn held(&self) -> bool {
        self.target.is_some()
    }
    pub(crate) fn cancel(&mut self) {
        if let Some(target) = &mut self.target {
            target.cancel();
        }
    }
}

impl Bridge {
    pub(crate) fn migration_admission(&self, request: Request) -> Result<Pending> {
        request.validate()?;
        encoded_len(&request, request_cap(&request, &self.0.shared.limits))?;
        let (tx, receiver) = mpsc::sync_channel(1);
        let cancel = Cancellation::default();
        let mut queue = self.0.shared.queue.lock().unwrap();
        if queue.stopping {
            return Err(error(ErrorCode::Closed, "catalog owner closed"));
        }
        // A distinct reserved lane remains usable while ordinary work is full.
        if queue
            .pending
            .iter()
            .filter(|e| matches!(&e.work, Work::MigrationAdmission(r, _) if r.action.recovery() == request.action.recovery()))
            .count()
            >= if request.action.recovery() { super::CONTROL_SLOTS } else { 1 }
        {
            return Err(error(ErrorCode::Busy, "migration control queue full"));
        }
        let recovery = request.action.recovery();
        let envelope = WorkEnvelope {
            work: Work::MigrationAdmission(request, tx),
            cancel: cancel.clone(),
            created: Instant::now(),
        };
        if recovery {
            queue.pending.push_front(envelope);
        } else {
            let position = queue.pending.iter().position(|entry|
                !matches!(&entry.work, Work::MigrationAdmission(request, _) if request.action.recovery()))
                .unwrap_or(queue.pending.len());
            queue.pending.insert(position, envelope);
        }
        self.0.shared.wake.notify_all();
        Ok(Pending { receiver, cancel })
    }
}
impl Actor {
    pub(crate) fn migration_request(&mut self, request: Request, cancel: &Cancellation) -> Reply {
        Reply::from_result(self.migration_action_cancellable(request, cancel))
    }
    #[cfg(test)]
    fn migration_action(&mut self, request: Request) -> Result<Snapshot> {
        self.migration_action_cancellable(request, &Cancellation::default())
    }
    fn migration_action_cancellable(
        &mut self,
        request: Request,
        cancel: &Cancellation,
    ) -> Result<Snapshot> {
        request.validate()?;
        encoded_len(&request, request_cap(&request, &self.config.limits))?;
        if let Action::InspectTarget {
            catalog,
            destination,
        } = &request.action
        {
            let open = self
                .open
                .as_ref()
                .ok_or_else(|| error(ErrorCode::StaleSession, "migration catalog is not open"))?;
            if open.closing {
                return Err(error(ErrorCode::Closed, "migration catalog is closing"));
            }
            if self.managed.is_none() || open.token != *catalog {
                return Err(error(
                    ErrorCode::StaleSession,
                    "migration inspection catalog token changed",
                ));
            }
            let pin = managed_pin(open)?;
            let path = destination.to_path().map_err(invalid)?;
            crate::lightroom::source::reject_links(&path).map_err(invalid)?;
            if std::fs::canonicalize(&path).map_err(native)?
                != pin.root.to_path().map_err(invalid)?
            {
                return Err(error(
                    ErrorCode::StaleSession,
                    "migration inspection path changed",
                ));
            }
            open.catalog
                .session
                .verify_migration_identity(None, &cancel.0)
                .map_err(native)?;
            let snapshot = Snapshot {
                guard: request.guard,
                phase: Phase::Inspected,
                catalog: Some(catalog.clone()),
                destination: Some(pin),
                sequence: None,
                request_digest: None,
                write_kind: None,
                cancel_requested: false,
                progress: None,
                failure: None,
            };
            encoded_len(&Reply::Ok(snapshot.clone()), self.config.limits.reply_bytes)?;
            return Ok(snapshot);
        }
        if let Action::AcquireTarget {
            catalog,
            destination,
            expected,
        } = &request.action
        {
            if self.managed.is_none() {
                return Err(invalid(
                    "migration relay requires the managed desktop catalog route",
                ));
            }
            let acquire_digest =
                blake3::hash(&encode_bounded(&request, self.config.limits.request_bytes)?)
                    .to_hex()
                    .to_string();
            if let Some(target) = &self.migration.target {
                if target.guard == request.guard && target.acquire_digest == acquire_digest {
                    return Ok(target.snapshot());
                }
                return Err(error(
                    ErrorCode::Busy,
                    "migration target retained until checked operation drain",
                ));
            }
            if self
                .migration
                .drained
                .as_ref()
                .is_some_and(|last| last.guard == request.guard)
            {
                return Err(error(
                    ErrorCode::StaleSession,
                    "migration operation already drained",
                ));
            }
            if self.failed_admission.is_some() || self.failed_session.is_some() {
                return Err(error(ErrorCode::Busy, "catalog admission cleanup retained"));
            }
            let (pin, authority, prospective, writers) = match (&self.open, catalog, expected) {
                (Some(open), Some(catalog), Some(expected)) if open.token == *catalog => {
                    if open.closing
                        || open.relink.write_hold()
                        || open.exports.write_hold(&self.shared.exports)
                        || open
                            .import
                            .as_ref()
                            .is_some_and(|import| !import.terminal())
                        || !open.tickets.is_empty()
                    {
                        return Err(error(
                            ErrorCode::Busy,
                            "finish active catalog work before migration admission",
                        ));
                    }
                    let pin = managed_pin(open)?;
                    crate::lightroom::source::reject_links(
                        &destination.to_path().map_err(invalid)?,
                    )
                    .map_err(invalid)?;
                    if std::fs::canonicalize(destination.to_path().map_err(invalid)?)
                        .map_err(native)?
                        != pin.root.to_path().map_err(invalid)?
                        || &pin != expected
                    {
                        return Err(error(
                            ErrorCode::StaleSession,
                            "migration target is not the exact managed catalog",
                        ));
                    }
                    open.catalog
                        .session
                        .verify_migration_identity(None, &cancel.0)
                        .map_err(native)?;
                    (
                        Some(pin),
                        Some(open.catalog.session.clone()),
                        None,
                        Some(open.catalog.writers.clone()),
                    )
                }
                (None, None, None) => (None, None, Some(Prospective::new(destination)?), None),
                _ => {
                    return Err(error(
                        ErrorCode::StaleSession,
                        "migration catalog token or destination identity changed",
                    ));
                }
            };
            let reply_root = match &pin {
                Some(pin) => pin.root.clone(),
                None => {
                    let prospective = prospective.as_ref().unwrap();
                    NativePath::from_path(
                        &prospective
                            .parent
                            .join(destination.to_path().map_err(invalid)?.file_name().unwrap()),
                    )
                }
            };
            admit_reply(&request.guard, &reply_root, self.config.limits.reply_bytes)?;
            self.migration.target = Some(Target {
                guard: request.guard,
                acquire_digest,
                catalog: catalog.clone(),
                destination: destination.clone(),
                pin,
                authority,
                prospective,
                writers,
                attempt: None,
                canceled: false,
                draining: false,
                progress: None,
            });
            return Ok(self.migration.target.as_ref().unwrap().snapshot());
        }
        let Some(target) = &mut self.migration.target else {
            if let Some(drained) = &self.migration.drained {
                if drained.guard == request.guard
                    && matches!(
                        request.action,
                        Action::Status
                            | Action::Cancel
                            | Action::DrainOperation
                            | Action::ReleaseWrite { .. }
                    )
                {
                    if let Action::ReleaseWrite {
                        sequence,
                        request_digest,
                        kind,
                    } = &request.action
                    {
                        if drained.sequence != Some(*sequence)
                            || drained.write_kind != Some(*kind)
                            || drained.request_digest.as_ref() != Some(request_digest)
                        {
                            return Err(invalid("released attempt identity"));
                        }
                    }
                    return Ok(drained.clone());
                }
            }
            if matches!(request.action, Action::Status) {
                return Ok(Snapshot {
                    guard: request.guard,
                    phase: Phase::Unknown,
                    catalog: None,
                    destination: None,
                    sequence: None,
                    request_digest: None,
                    write_kind: None,
                    cancel_requested: false,
                    progress: None,
                    failure: None,
                });
            }
            return Err(error(
                ErrorCode::StaleSession,
                "migration operation not admitted",
            ));
        };
        if target.guard != request.guard {
            return Err(error(
                ErrorCode::StaleSession,
                "migration operation identity changed",
            ));
        }
        match request.action {
            Action::AcquireTarget { .. } | Action::InspectTarget { .. } => unreachable!(),
            Action::Status => {}
            Action::Cancel => target.cancel(),
            Action::Progress {
                phase,
                completed,
                total,
            } => {
                target.progress = Some((phase.clone(), completed, total));
                self.shared.queue.lock().unwrap().status.message =
                    Some(format!("Lightroom migration: {phase} ({})", completed.0));
            }
            Action::AcquireWrite {
                sequence,
                kind,
                target: token,
                lock,
                request_digest,
            } => {
                if sequence.0 == 0
                    || request_digest
                        != write_digest(&request.guard, sequence, kind, &token, lock.as_ref())?
                {
                    return Err(invalid("migration write request digest or sequence"));
                }
                if token != target.guard.generation {
                    return Err(error(
                        ErrorCode::StaleSession,
                        "migration write target token",
                    ));
                }
                if let Some(attempt) = &mut target.attempt {
                    if attempt.sequence == sequence {
                        if attempt.digest != request_digest || attempt.kind != kind {
                            return Err(invalid("migration write replay identity changed"));
                        }
                        return Ok(target.snapshot());
                    }
                    if sequence.0 <= attempt.sequence.0 {
                        return Err(invalid("stale migration write sequence"));
                    }
                    if let Some(owner) = &mut attempt.owner {
                        if owner.snapshot().0 != Phase::Released || !owner.join_ready()? {
                            return Err(error(
                                ErrorCode::Busy,
                                "previous migration permit not released and joined",
                            ));
                        }
                    } else {
                        return Err(error(
                            ErrorCode::Busy,
                            "failed migration acquisition requires operation drain",
                        ));
                    }
                }
                if target.canceled || target.draining {
                    return Err(error(ErrorCode::Canceled, "migration canceled"));
                }
                if (kind == WriteKind::Bootstrap && lock.is_some())
                    || (kind == WriteKind::Catalog && (target.pin.is_none() || lock.is_none()))
                {
                    return Err(invalid("migration write kind and target state differ"));
                }
                // Record before bootstrap or writer acquisition. Lost replies must
                // return the same attempt, never repeat filesystem creation.
                target.attempt = Some(Attempt {
                    sequence,
                    kind,
                    digest: request_digest,
                    owner: None,
                    failure: None,
                });
                let result = self.start_migration_permit(kind, lock.as_ref(), cancel);
                let target = self.migration.target.as_mut().unwrap();
                if let Err(failure) = result {
                    target.attempt.as_mut().unwrap().failure = Some(bounded_error(failure));
                }
            }
            Action::ReleaseWrite {
                sequence,
                kind,
                request_digest,
            } => {
                let attempt = target
                    .attempt
                    .as_mut()
                    .ok_or_else(|| invalid("no attempted migration permit"))?;
                if attempt.sequence != sequence
                    || attempt.kind != kind
                    || attempt.digest != request_digest
                {
                    return Err(invalid("migration release identity changed"));
                }
                if let Some(owner) = &mut attempt.owner {
                    owner.release();
                    owner.join_ready()?;
                }
            }
            Action::DrainOperation => {
                target.draining = true;
                target.cancel();
                if let Some(owner) = target
                    .attempt
                    .as_mut()
                    .and_then(|attempt| attempt.owner.as_mut())
                {
                    owner.release();
                    if !owner.join_ready()? {
                        return Ok(target.snapshot());
                    }
                }
                let mut snapshot = target.snapshot();
                snapshot.phase = Phase::Drained;
                self.migration.target.take();
                self.migration.drained = Some(snapshot.clone());
                if let Some(open) = &mut self.open {
                    open.index_pending = true;
                }
                return Ok(snapshot);
            }
        }
        Ok(self.migration.target.as_ref().unwrap().snapshot())
    }
    fn start_migration_permit(
        &mut self,
        kind: WriteKind,
        lock: Option<&FileKey>,
        cancel: &Cancellation,
    ) -> Result<()> {
        if kind == WriteKind::Bootstrap
            && self
                .migration
                .target
                .as_ref()
                .unwrap()
                .prospective
                .is_some()
        {
            let target = self.migration.target.as_ref().unwrap();
            target.prospective.as_ref().unwrap().verify()?;
            let destination = target.destination.clone();
            // Exactly the Actor Create path, selected only by admitted NeedWrite.
            self.open_path(destination.clone(), true, cancel)?;
            let open = self
                .open
                .as_ref()
                .ok_or_else(|| native("bootstrap did not retain an Open catalog"))?;
            let pin = managed_pin(open)?;
            let target = self.migration.target.as_mut().unwrap();
            target.catalog = Some(open.token.clone());
            target.writers = Some(open.catalog.writers.clone());
            target.authority = Some(open.catalog.session.clone());
            target.pin = Some(pin);
            target.prospective.take();
        }
        let target = self.migration.target.as_mut().unwrap();
        let authority = target
            .authority
            .as_ref()
            .ok_or_else(|| invalid("unadmitted migration destination"))?
            .clone();
        let lock = lock.map(physical_key).transpose()?;
        authority
            .verify_migration_identity(lock, &cancel.0)
            .map_err(native)?;
        let cancel = cancel.0.clone();
        let owner = PermitOwner::start(target.writers.as_ref().unwrap().clone(), move || {
            // F revalidates the root, database, manifest and exact optional lock
            // after the actual thread-affine C Writers wait; C opens no file.
            authority
                .verify_migration_identity(lock, &cancel)
                .map_err(native)
        })?;
        target.attempt.as_mut().unwrap().owner = Some(owner);
        Ok(())
    }
}

pub(super) fn reject_queued_authority(state: &mut super::State) {
    while let Some(index) = state.control.iter().position(|message| {
        message.kind == super::wire::Kind::MigrationAdmission
            && !serde_json::from_slice::<Request>(&message.bytes)
                .is_ok_and(|request| request.action.recovery())
    }) {
        let message = state.control.remove(index).unwrap();
        if let Some(entry) = state.pending.remove(&message.id) {
            if let super::Delivery::Migration(tx) = entry.delivery {
                let _ = tx.send(Reply::Error(refusal(
                    ErrorCode::Closed,
                    "desktop stopping; migration authority was not sent",
                )));
            }
        }
    }
}

/// G owns only a weak reference to the C transport; the migration coordinator
/// retains the actual desktop owner until every descendant and release is checked.
#[derive(Clone)]
pub(crate) struct Client {
    shared: Weak<super::Shared>,
}
impl Client {
    pub(super) fn new(shared: &Arc<super::Shared>) -> Self {
        Self {
            shared: Arc::downgrade(shared),
        }
    }
    pub(crate) fn submit(&self, request: Request) -> Result<Pending> {
        request.validate()?;
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| error(ErrorCode::Closed, "desktop migration relay unavailable"))?;
        let (tx, receiver) = mpsc::sync_channel(1);
        let cancel = Cancellation::default();
        let mut state = shared.state.lock().unwrap();
        if state.stopping {
            reject_queued_authority(&mut state);
        }
        if !state.ready || state.reaped || (state.stopping && !request.action.recovery()) {
            return Err(error(
                ErrorCode::Closed,
                "catalog child unavailable; migration drain required",
            ));
        }
        if state
            .pending
            .values()
            .filter(|entry| {
                matches!(entry.delivery, super::Delivery::Migration(_))
                    && entry.control == request.action.recovery()
            })
            .count()
            >= if request.action.recovery() {
                super::CONTROL_SLOTS
            } else {
                1
            }
        {
            return Err(error(ErrorCode::Busy, "migration relay control queue full"));
        }
        let bytes = encode_bounded(&request, request_cap(&request, &shared.limits))?;
        let id = state.next;
        state.next = id.checked_add(1).ok_or_else(|| {
            error(
                ErrorCode::ResourceLimit,
                "desktop request identifier exhausted",
            )
        })?;
        state.pending.insert(
            id,
            super::Entry {
                delivery: super::Delivery::Migration(tx),
                cancel: cancel.clone(),
                sent_cancel: false,
                control: request.action.recovery(),
            },
        );
        state.control.push_back(super::wire::Message::new(
            super::wire::Kind::MigrationAdmission,
            id,
            bytes,
        ));
        shared.wake.notify_all();
        Ok(Pending { receiver, cancel })
    }
}

#[cfg(test)]
mod tests;
