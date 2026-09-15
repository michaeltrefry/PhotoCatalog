//! UI-independent catalog owner. Requests are bounded and native workers remain
//! owned/reaped by PreviewService; no webview thread touches SQLite.
pub mod backup;
pub mod browse;
pub mod copy;
pub mod desktop;
mod dto;
pub mod exports;
mod hydration;
pub mod lightroom;
pub mod lightroom_bridge;
pub mod metadata;
pub mod organization;
mod preview_delivery;
pub mod relink;
use crate::{
    Catalog,
    catalog_edits::{VariantKey, VariantView},
    organization_search::{Cursor, Query},
    preview::{self, PreviewService},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
pub use dto::*;
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const CURSOR_BYTES: usize = 32 * 1024;

// The renderer treats this entire string as opaque, so nested i64 JSON never
// crosses the JavaScript number boundary. Canonical encoding rejects extra fields.
fn encode_cursor(session: &str, cursor: &Cursor) -> std::result::Result<String, BridgeError> {
    let token = serde_json::to_string(&(session, cursor)).map_err(|e| native(e.into()))?;
    if token.len() > CURSOR_BYTES {
        return Err(error(ErrorCode::ResourceLimit, "search cursor byte limit"));
    }
    Ok(token)
}
fn decode_cursor(token: &str, session: &str) -> std::result::Result<Cursor, BridgeError> {
    if token.len() > CURSOR_BYTES {
        return Err(error(ErrorCode::ResourceLimit, "search cursor byte limit"));
    }
    let (owner, cursor): (String, Cursor) = serde_json::from_str(token).map_err(|e| {
        error(
            ErrorCode::InvalidRequest,
            format!("invalid search cursor: {e}"),
        )
    })?;
    if owner != session {
        return Err(error(
            ErrorCode::StaleSession,
            "search cursor belongs to another session",
        ));
    }
    if encode_cursor(session, &cursor)? != token
        || cursor.epoch < 0
        || cursor.high_water < 0
        || cursor.sequence <= 0
        || cursor.sequence > cursor.high_water
    {
        return Err(error(
            ErrorCode::InvalidRequest,
            "invalid search cursor encoding or range",
        ));
    }
    Ok(cursor)
}

#[derive(Clone)]
pub struct Config {
    pub worker_executable: PathBuf,
    /// App-owned cache parent; each resolved catalog path gets a separate namespace.
    /// Relocating a catalog starts a fresh regenerable namespace. None stores it in the catalog.
    pub cache_root: Option<PathBuf>,
    pub original_roots: Vec<PathBuf>,
    pub preview_policy: preview::PreviewPolicy,
    pub preview_limits: preview::ServiceLimits,
    pub limits: Limits,
    #[cfg(test)]
    import_checkpoint: Option<crate::import_preparation::Checkpoint>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub queued: usize,
    pub request_bytes: usize,
    pub reply_bytes: usize,
    pub page_rows: u16,
    pub scan_rows: usize,
    pub page_bytes: usize,
    pub tickets: usize,
    pub ttl_seconds: u64,
    pub binary_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            queued: 128,
            request_bytes: 1024 * 1024,
            reply_bytes: 1024 * 1024,
            page_rows: 100,
            scan_rows: 512,
            page_bytes: 128 * 1024,
            tickets: 128,
            ttl_seconds: 60,
            binary_bytes: 16 * 1024 * 1024,
        }
    }
}
impl Config {
    /// Requested Rust backing for the managed catalog process's bounded preview
    /// metadata graph. Pixel, encoded-image, codec/native, SQLite and runtime
    /// storage have their own admissions and are deliberately not included.
    pub fn requested_preview_metadata_bytes(&self) -> Result<u64> {
        desktop::preview_metadata_capacity::requested_bytes(self)
    }

    fn validate(&self) -> Result<()> {
        let l = &self.limits;
        ensure!(
            (1..=256).contains(&l.queued) && (1..=256).contains(&l.tickets),
            "descriptor limits"
        );
        ensure!(
            (1..=100).contains(&l.page_rows)
                && l.scan_rows >= usize::from(l.page_rows)
                && l.scan_rows <= 4096,
            "page limits"
        );
        ensure!(
            (1..=1024 * 1024).contains(&l.page_bytes)
                && (1..=4 * 1024 * 1024).contains(&l.request_bytes)
                && (1..=4 * 1024 * 1024).contains(&l.reply_bytes),
            "message limits"
        );
        ensure!(
            (1..=300).contains(&l.ttl_seconds) && (1..=32 * 1024 * 1024).contains(&l.binary_bytes),
            "lease limits"
        );
        ensure!(
            self.worker_executable.is_absolute() && self.worker_executable.is_file(),
            "native worker executable unavailable"
        );
        // This is an arithmetic/layout admission only. Existing small working
        // budgets remain valid because metadata is not silently subtracted from
        // the native working allowance.
        self.requested_preview_metadata_bytes()?;
        Ok(())
    }
}
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>, Option<Arc<dyn Fn() + Send + Sync>>);
impl Cancellation {
    pub fn cancel(&self) {
        if !self.0.swap(true, Ordering::AcqRel)
            && let Some(notify) = &self.1
        {
            notify();
        }
    }
    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
pub struct Pending {
    receiver: mpsc::Receiver<Reply>,
    cancel: Cancellation,
}
impl Pending {
    pub fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
    pub fn cancel(&self) {
        self.cancel.cancel()
    }
    pub fn recv(self) -> Reply {
        self.receiver
            .recv()
            .unwrap_or_else(|_| failure(ErrorCode::Closed, "catalog owner stopped"))
    }
}
pub struct PendingBytes {
    receiver: mpsc::Receiver<std::result::Result<PreviewBytes, BridgeError>>,
    cancel: Cancellation,
}
impl PendingBytes {
    pub fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
    pub fn recv(self) -> std::result::Result<PreviewBytes, BridgeError> {
        self.receiver
            .recv()
            .unwrap_or_else(|_| Err(error(ErrorCode::Closed, "catalog owner stopped")))
    }
}
/// Owns a bounded transport copy. Native staging is released after copying;
/// keep this object alive until the transport has copied/consumed bytes.
pub struct PreviewBytes {
    pub mime: String,
    bytes: Vec<u8>,
    usage: Arc<AtomicUsize>,
}
impl PreviewBytes {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
impl Drop for PreviewBytes {
    fn drop(&mut self) {
        self.usage.fetch_sub(self.bytes.len(), Ordering::AcqRel);
    }
}
fn error(code: ErrorCode, message: impl Into<String>) -> BridgeError {
    BridgeError {
        code,
        message: message.into(),
    }
}
fn failure(code: ErrorCode, message: impl Into<String>) -> Reply {
    Reply::Error {
        error: error(code, message),
    }
}
fn native(e: anyhow::Error) -> BridgeError {
    let limited = e.is::<crate::catalog_session::store::ResourceLimit>()
        || e.downcast_ref::<crate::filesystem_worker::wire::Failure>()
            .is_some_and(|f| f.kind == crate::filesystem_worker::wire::FailureKind::ResourceLimit);
    error(
        if limited {
            ErrorCode::ResourceLimit
        } else {
            ErrorCode::Native
        },
        format!("{e:#}"),
    )
}
fn reply(r: std::result::Result<Response, BridgeError>) -> Reply {
    match r {
        Ok(value) => Reply::Ok { value },
        Err(error) => Reply::Error { error },
    }
}

enum Work {
    MigrationAdmission(
        desktop::lightroom_migration::Request,
        mpsc::SyncSender<desktop::lightroom_migration::Reply>,
    ),
    Shutdown(mpsc::SyncSender<std::result::Result<(), BridgeError>>),
    Command(Request, mpsc::SyncSender<Reply>),
    Bytes {
        catalog: String,
        ticket: String,
        foreground: bool,
        reply: mpsc::SyncSender<std::result::Result<PreviewBytes, BridgeError>>,
    },
}
struct Envelope {
    work: Work,
    cancel: Cancellation,
    created: Instant,
}
impl Envelope {
    fn priority(&self) -> u8 {
        match &self.work {
            Work::Shutdown(_) | Work::MigrationAdmission(..) => 0,
            Work::Command(Request::Lightroom { .. }, _) => 4,
            Work::Command(Request::Export { request, .. }, _) => {
                if matches!(
                    request.as_ref(),
                    exports::Request::Cancel { .. } | exports::Request::Yield { .. }
                ) {
                    0
                } else if request.read_only() {
                    2
                } else {
                    4
                }
            }
            Work::Command(Request::EditCopy { request, .. }, _) => {
                if matches!(request.as_ref(), copy::Request::Cancel { .. }) {
                    0
                } else if request.read_only() {
                    2
                } else {
                    4
                }
            }
            Work::Command(Request::Metadata { request, .. }, _) => match request.as_ref() {
                metadata::Request::Resolve { .. } => 1,
                _ => 2,
            },
            Work::Command(Request::Relink { request, .. }, _) => {
                if request.read_only() {
                    2
                } else {
                    1
                }
            }
            Work::Command(Request::Organization { request, .. }, _) => match request.as_ref() {
                organization::Request::Cancel { .. } => 0,
                organization::Request::Apply { .. }
                | organization::Request::SetMember { .. }
                | organization::Request::PlaceCollection { .. }
                | organization::Request::RenameCollection { .. } => 1,
                organization::Request::Step { .. }
                | organization::Request::Append { .. }
                | organization::Request::Seal { .. } => 4,
                _ => 2,
            },
            Work::Command(Request::Close { .. }, _) => 0,
            Work::Command(
                Request::SaveRecipe { .. }
                | Request::Undo { .. }
                | Request::Redo { .. }
                | Request::Cull { .. }
                | Request::CreateVariant { .. },
                _,
            ) => 1,
            Work::Command(
                Request::Preview {
                    foreground: false, ..
                },
                _,
            )
            | Work::Bytes {
                foreground: false, ..
            } => 4,
            Work::Command(Request::Preview { .. }, _) | Work::Bytes { .. } => 3,
            _ => 2,
        }
    }
    fn before_copy(&self) -> bool {
        self.priority() <= 3
    }
    fn reject(self, code: ErrorCode, message: &str) {
        match self.work {
            Work::MigrationAdmission(_, tx) => {
                let _ = tx.send(desktop::lightroom_migration::Reply::Error(error(
                    code, message,
                )));
            }
            Work::Shutdown(tx) => {
                let _ = tx.send(Err(error(code, message)));
            }
            Work::Command(_, tx) => {
                let _ = tx.send(failure(code, message));
            }
            Work::Bytes { reply, .. } => {
                let _ = reply.send(Err(error(code, message)));
            }
        }
    }
}
struct TicketPriority {
    foreground: bool,
    viewport: String,
    generation: u64,
}
struct Queue {
    pending: VecDeque<Envelope>,
    stopping: bool,
    viewport: HashMap<(String, String), u64>,
    status: Status,
    active_cancel: Option<Cancellation>,
    import_status: Option<ImportStatus>,
    import_cancel: Option<Cancellation>,
    ticket_foreground: HashMap<(String, String), TicketPriority>,
}
struct Shared {
    managed_catalog: bool,
    lightroom: Arc<Mutex<lightroom_bridge::Control>>,
    exports: Arc<Mutex<exports::Control>>,
    copy: Arc<Mutex<copy::Control>>,
    relink: Arc<Mutex<relink::Control>>,
    backups: Mutex<backup::Coordinator>,
    queue: Mutex<Queue>,
    wake: Condvar,
    limits: Limits,
    binary: Arc<AtomicUsize>,
}
struct Handle {
    shared: Arc<Shared>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
    shutdown_failure: Mutex<Option<BridgeError>>,
}
impl Handle {
    fn try_shutdown(&self) -> std::result::Result<(), BridgeError> {
        let mut owner = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(failure) = self
            .shutdown_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Err(failure.clone());
        }
        let Some(thread) = owner.as_ref() else {
            return Ok(());
        };
        if !thread.is_finished() {
            let (tx, rx) = mpsc::sync_channel(1);
            {
                let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(c) = &q.active_cancel {
                    c.cancel();
                }
                // One reserved shutdown slot; concurrent shutdowns serialize on owner.
                q.pending.push_front(Envelope {
                    work: Work::Shutdown(tx),
                    cancel: Cancellation::default(),
                    created: Instant::now(),
                });
                self.shared.wake.notify_all();
            }
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(result) => {
                        result?;
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) if !thread.is_finished() => {}
                    Err(_) => break,
                }
            }
        }
        let joined = owner.take().unwrap().join();
        let closed = matches!(
            self.shared
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .status
                .phase,
            Phase::Closed
        );
        if joined.is_err() || !closed {
            let failure = error(
                ErrorCode::Native,
                "catalog owner stopped without verified shutdown",
            );
            *self
                .shutdown_failure
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(failure.clone());
            return Err(failure);
        }
        Ok(())
    }
    fn shutdown(&self) {
        // Compatibility/Drop entry point. On a failed drain the actor retains its
        // Open owner; callers that may exit the process must use try_shutdown.
        let _ = self.try_shutdown();
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
#[derive(Clone)]
pub struct Bridge(Arc<Handle>);
impl Bridge {
    pub fn spawn(config: Config) -> Result<Self> {
        Self::spawn_engine(config, None)
    }
    /// Unselected C-only constructor; caller must keep F independently owned and
    /// forbid any live catalog/backup/native descendant at bootstrap admission.
    #[allow(dead_code)]
    pub(crate) fn spawn_managed(config: Config, managed: ManagedCatalogConfig) -> Result<Self> {
        Self::spawn_engine(config, Some(managed))
    }
    fn spawn_engine(config: Config, managed: Option<ManagedCatalogConfig>) -> Result<Self> {
        config.validate()?;
        let shared = Arc::new(Shared {
            managed_catalog: managed.is_some(),
            lightroom: Arc::new(Mutex::new(lightroom_bridge::Control::default())),
            exports: Arc::new(Mutex::new(exports::Control::default())),
            relink: Arc::new(Mutex::new(relink::Control::default())),
            copy: Arc::new(Mutex::new(copy::Control::default())),
            backups: Mutex::new(backup::Coordinator::new(Default::default())?),
            queue: Mutex::new(Queue {
                pending: VecDeque::new(),
                stopping: false,
                viewport: HashMap::new(),
                status: Status {
                    phase: Phase::Closed,
                    catalog: None,
                    jobs_held: false,
                    pending_commands: 0,
                    active_previews: 0,
                    cancel_requested: false,
                    message: None,
                },
                active_cancel: None,
                import_status: None,
                import_cancel: None,
                ticket_foreground: HashMap::new(),
            }),
            wake: Condvar::new(),
            limits: config.limits.clone(),
            binary: Arc::new(AtomicUsize::new(0)),
        });
        let actor_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("catalog-application".into())
            .spawn(move || {
                let mut actor = Actor::new(config, actor_shared);
                actor.managed = managed;
                actor.run()
            })?;
        Ok(Self(Arc::new(Handle {
            shared,
            thread: Mutex::new(Some(thread)),
            shutdown_failure: Mutex::new(None),
        })))
    }
    /// Attempts shutdown, retaining ownership if cleanup cannot be verified.
    /// Call try_shutdown before exiting the host process or replacing its owner.
    pub fn shutdown(&self) {
        self.0.shutdown()
    }
    /// Reports Closed only after all owned cleanup and actor join succeed.
    pub fn try_shutdown(&self) -> std::result::Result<(), BridgeError> {
        self.0.try_shutdown()
    }
    pub fn submit(&self, request: Request) -> std::result::Result<Pending, BridgeError> {
        if self.0.shared.managed_catalog && matches!(&request, Request::Lightroom { .. }) {
            return Err(error(
                ErrorCode::InvalidRequest,
                "independent Lightroom requests belong to the desktop owner",
            ));
        }
        let size = serde_json::to_vec(&request)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?
            .len();
        if size > self.0.shared.limits.request_bytes
            || (matches!(&request, Request::Lightroom { .. }) && size > 128 * 1024)
        {
            return Err(error(ErrorCode::ResourceLimit, "request byte limit"));
        }
        let cancel = Cancellation::default();
        let (tx, receiver) = mpsc::sync_channel(1);
        let mut q = self.0.shared.queue.lock().unwrap();
        if q.stopping {
            return Err(error(ErrorCode::Closed, "catalog owner closed"));
        }
        if let Request::Lightroom { request } = &request
            && request.direct()
        {
            let budget = self
                .0
                .shared
                .limits
                .reply_bytes
                .min(self.0.shared.limits.request_bytes)
                .min(128 * 1024);
            let response = self
                .0
                .shared
                .lightroom
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .direct((**request).clone(), budget)
                .map_err(native)?;
            let out = Reply::Ok {
                value: Response::Lightroom(Box::new(response)),
            };
            let out = if serde_json::to_vec(&out)
                .map_err(|e| native(e.into()))?
                .len()
                <= budget
            {
                out
            } else {
                failure(ErrorCode::ResourceLimit, "inspection response byte limit")
            };
            let _ = tx.send(out);
            self.0.shared.wake.notify_one();
            return Ok(Pending { receiver, cancel });
        }
        if let Request::Export { catalog, request } = &request
            && matches!(
                request.as_ref(),
                exports::Request::Status { .. }
                    | exports::Request::Cancel { .. }
                    | exports::Request::Yield { .. }
            )
        {
            if q.status.catalog.as_ref() != Some(catalog) {
                return Err(error(ErrorCode::StaleSession, "catalog session changed"));
            }
            let direct = match request.as_ref() {
                exports::Request::Status { operation } => Some(
                    self.0
                        .shared
                        .exports
                        .lock()
                        .unwrap()
                        .status(operation.as_deref())?,
                ),
                exports::Request::Cancel { job, operation } => self
                    .0
                    .shared
                    .exports
                    .lock()
                    .unwrap()
                    .cancel(job.as_deref(), operation.as_deref())?
                    .map(Some),
                exports::Request::Yield { job, operation } => Some(
                    self.0
                        .shared
                        .exports
                        .lock()
                        .unwrap()
                        .yield_job(job, operation)?,
                ),
                _ => unreachable!(),
            };
            if let Some(status) = direct {
                let out = Reply::Ok {
                    value: Response::Export(Box::new(exports::Response::Operation(status))),
                };
                let out = if serde_json::to_vec(&out)
                    .map_err(|e| native(e.into()))?
                    .len()
                    <= self.0.shared.limits.reply_bytes
                {
                    out
                } else {
                    failure(ErrorCode::ResourceLimit, "response byte limit")
                };
                let _ = tx.send(out);
                self.0.shared.wake.notify_one();
                return Ok(Pending { receiver, cancel });
            }
        }
        if let Request::EditCopy { catalog, request } = &request
            && matches!(
                request.as_ref(),
                copy::Request::Status { .. } | copy::Request::Cancel { .. }
            )
        {
            if q.status.catalog.as_ref() != Some(catalog) {
                return Err(error(ErrorCode::StaleSession, "catalog session changed"));
            }
            let direct = match request.as_ref() {
                copy::Request::Status { operation } => Some(
                    self.0
                        .shared
                        .copy
                        .lock()
                        .unwrap()
                        .status(operation.as_deref())?,
                ),
                copy::Request::Cancel { job, operation } => self
                    .0
                    .shared
                    .copy
                    .lock()
                    .unwrap()
                    .cancel(job, operation.as_deref())?
                    .map(Some),
                _ => unreachable!(),
            };
            if let Some(status) = direct {
                let out = Reply::Ok {
                    value: Response::EditCopy(Box::new(copy::Response::Operation(status))),
                };
                let out = if serde_json::to_vec(&out)
                    .map_err(|e| native(e.into()))?
                    .len()
                    <= self.0.shared.limits.reply_bytes
                {
                    out
                } else {
                    failure(ErrorCode::ResourceLimit, "response byte limit")
                };
                let _ = tx.send(out);
                self.0.shared.wake.notify_one();
                return Ok(Pending { receiver, cancel });
            }
        }
        if let Request::Relink { catalog, request } = &request {
            let direct = match request.as_ref() {
                relink::Request::Status { operation } => Some((operation.as_deref(), false)),
                relink::Request::Cancel { operation } => Some((Some(operation.as_str()), true)),
                _ => None,
            };
            if let Some((operation, cancel_requested)) = direct {
                if q.status.catalog.as_ref() != Some(catalog) {
                    return Err(error(ErrorCode::StaleSession, "catalog session changed"));
                }
                let snapshot = self
                    .0
                    .shared
                    .relink
                    .lock()
                    .unwrap()
                    .read(operation, cancel_requested)?;
                let out = Reply::Ok {
                    value: Response::Relink(Box::new(relink::Response::Operation(snapshot))),
                };
                let out = if serde_json::to_vec(&out)
                    .map_err(|e| native(e.into()))?
                    .len()
                    <= self.0.shared.limits.reply_bytes
                {
                    out
                } else {
                    failure(ErrorCode::ResourceLimit, "response byte limit")
                };
                let _ = tx.send(out);
                self.0.shared.wake.notify_one();
                return Ok(Pending { receiver, cancel });
            }
        }
        if let Request::ImportStatus { catalog } | Request::ImportCancel { catalog, .. } = &request
        {
            if q.status.catalog.as_ref() != Some(catalog) {
                return Err(error(ErrorCode::StaleSession, "catalog session changed"));
            }
            if let Request::ImportCancel { import, .. } = &request {
                if q.import_status.as_ref().map(|s| &s.id) != Some(import) {
                    return Err(error(ErrorCode::StaleSession, "import attempt changed"));
                }
                if let Some(c) = &q.import_cancel {
                    c.cancel();
                    if let Some(s) = &mut q.import_status {
                        s.phase = ImportPhase::CancelRequested;
                    }
                }
            }
            let _ = tx.send(Reply::Ok {
                value: Response::Import(q.import_status.clone()),
            });
            self.0.shared.wake.notify_one();
            return Ok(Pending { receiver, cancel });
        }
        if matches!(
            &request,
            Request::BackupStatus | Request::BackupCancel { .. }
        ) {
            let mut backups = self.0.shared.backups.try_lock().map_err(|_| {
                error(
                    ErrorCode::ResourceLimit,
                    "backup control is busy closing; retry shortly",
                )
            })?;
            let snapshot = match &request {
                Request::BackupCancel { operation } => {
                    Some(backups.cancel(operation).map_err(native)?)
                }
                _ => backups.status().map_err(native)?,
            };
            let response = Reply::Ok {
                value: Response::Backup(snapshot),
            };
            let response = match serde_json::to_vec(&response) {
                Ok(bytes) if bytes.len() <= self.0.shared.limits.reply_bytes => response,
                _ => failure(ErrorCode::ResourceLimit, "response byte limit"),
            };
            let _ = tx.send(response);
            return Ok(Pending { receiver, cancel });
        }
        if let Request::ReleaseViewport {
            catalog,
            viewport,
            generation,
        } = &request
        {
            if viewport.len() > 128 {
                return Err(error(ErrorCode::InvalidRequest, "viewport identity length"));
            }
            if q.status.catalog.as_ref() != Some(catalog) {
                return Err(error(ErrorCode::StaleSession, "catalog session changed"));
            }
            if q.viewport.get(&(catalog.clone(), viewport.clone())) == Some(&generation.0) {
                q.viewport.remove(&(catalog.clone(), viewport.clone()));
                let mut keep = VecDeque::new();
                while let Some(old) = q.pending.pop_front() {
                    let released = match &old.work {
                        Work::Command(
                            Request::Preview {
                                catalog: c,
                                viewport: v,
                                generation: g,
                                ..
                            },
                            _,
                        ) => c == catalog && v == viewport && g == generation,
                        Work::Bytes {
                            catalog: c, ticket, ..
                        } => {
                            c == catalog
                                && q.ticket_foreground
                                    .get(&(c.clone(), ticket.clone()))
                                    .is_some_and(|t| {
                                        t.viewport == *viewport && t.generation == generation.0
                                    })
                        }
                        _ => false,
                    };
                    if released {
                        old.reject(ErrorCode::Superseded, "viewport released");
                    } else {
                        keep.push_back(old);
                    }
                }
                q.pending = keep;
                q.ticket_foreground.retain(|(c, _), t| {
                    c != catalog || t.viewport != *viewport || t.generation != generation.0
                });
            }
            self.0.shared.wake.notify_one();
        }
        if matches!(request, Request::Status | Request::ReleaseViewport { .. }) {
            let mut s = q.status.clone();
            s.pending_commands = q.pending.len() as u32;
            s.cancel_requested = q
                .active_cancel
                .as_ref()
                .is_some_and(Cancellation::is_canceled);
            let _ = tx.send(Reply::Ok {
                value: Response::Status(s),
            });
        } else {
            // Existing direct status/cancel routes remain usable during drain.
            // Admit no new catalog work after Close has started.
            if matches!(q.status.phase, Phase::Closing)
                && !matches!(&request, Request::Close { .. } | Request::Lightroom { .. })
            {
                return Err(error(
                    ErrorCode::Busy,
                    "catalog is closing; retry Close after cleanup failure",
                ));
            }

            if let Request::Preview {
                catalog,
                viewport,
                generation,
                ..
            } = &request
            {
                if viewport.len() > 128 {
                    return Err(error(ErrorCode::InvalidRequest, "viewport identity length"));
                }
                let key = (catalog.clone(), viewport.clone());
                if q.viewport.get(&key).is_some_and(|g| *g > generation.0) {
                    return Err(error(
                        ErrorCode::Superseded,
                        "viewport generation already superseded",
                    ));
                }
                if !q.viewport.contains_key(&key)
                    && q.viewport.len() >= self.0.shared.limits.tickets
                {
                    return Err(error(ErrorCode::ResourceLimit, "viewport count limit"));
                }
            }
            let mut keep = VecDeque::new();
            while let Some(old) = q.pending.pop_front() {
                if old.cancel.is_canceled() {
                    old.reject(ErrorCode::Canceled, "operation canceled before execution");
                    continue;
                }
                let obsolete = match (&request, &old.work) {
                    (
                        Request::Images { catalog, .. } | Request::Search { catalog, .. },
                        Work::Command(
                            Request::Images { catalog: c, .. } | Request::Search { catalog: c, .. },
                            _,
                        ),
                    ) => catalog == c,
                    (
                        Request::Preview {
                            catalog,
                            viewport,
                            generation,
                            key,
                            ..
                        },
                        Work::Command(
                            Request::Preview {
                                catalog: c,
                                viewport: v,
                                generation: g,
                                key: k,
                                ..
                            },
                            _,
                        ),
                    ) => {
                        catalog == c
                            && viewport == v
                            && (g < generation || (g == generation && k == key))
                    }
                    (
                        Request::Preview {
                            catalog,
                            viewport,
                            generation,
                            ..
                        },
                        Work::Bytes {
                            catalog: c, ticket, ..
                        },
                    ) => {
                        catalog == c
                            && q.ticket_foreground
                                .get(&(c.clone(), ticket.clone()))
                                .is_some_and(|t| {
                                    t.viewport == *viewport && t.generation < generation.0
                                })
                    }
                    _ => false,
                };
                if obsolete {
                    old.reject(
                        ErrorCode::Superseded,
                        "request superseded by newer selection",
                    )
                } else {
                    keep.push_back(old)
                }
            }
            q.pending = keep;
            if q.pending.len() >= self.0.shared.limits.queued {
                return Err(error(ErrorCode::Busy, "catalog command queue full"));
            }
            if let Request::Preview {
                catalog,
                viewport,
                generation,
                ..
            } = &request
            {
                q.viewport
                    .insert((catalog.clone(), viewport.clone()), generation.0);
            }
            q.pending.push_back(Envelope {
                work: Work::Command(request, tx),
                cancel: cancel.clone(),
                created: Instant::now(),
            });
            self.0.shared.wake.notify_one();
        }
        Ok(Pending { receiver, cancel })
    }
    pub fn preview_bytes(
        &self,
        catalog: String,
        ticket: String,
        foreground: bool,
    ) -> std::result::Result<PendingBytes, BridgeError> {
        if catalog.len() > 128 || ticket.len() > 128 {
            return Err(error(ErrorCode::InvalidRequest, "ticket identity length"));
        }
        let cancel = Cancellation::default();
        let (tx, receiver) = mpsc::sync_channel(1);
        let mut q = self.0.shared.queue.lock().unwrap();
        if q.stopping {
            return Err(error(ErrorCode::Closed, "catalog owner closed"));
        }
        if matches!(q.status.phase, Phase::Closing) {
            return Err(error(ErrorCode::Busy, "catalog is closing"));
        }
        let _ = foreground; // The transport cannot promote a background ticket.
        let foreground = q
            .ticket_foreground
            .get(&(catalog.clone(), ticket.clone()))
            .ok_or_else(|| error(ErrorCode::StaleSession, "preview ticket expired"))?
            .foreground;
        if q.pending.len() >= self.0.shared.limits.queued {
            return Err(error(ErrorCode::Busy, "catalog command queue full"));
        }
        q.pending.push_back(Envelope {
            work: Work::Bytes {
                catalog,
                ticket,
                foreground,
                reply: tx,
            },
            cancel: cancel.clone(),
            created: Instant::now(),
        });
        self.0.shared.wake.notify_one();
        Ok(PendingBytes { receiver, cancel })
    }
}

struct Ticket {
    read: Option<preview::ReadTicket>,
    dto: PreviewStatus,
    identity: crate::catalog_edits::EditRenderIdentity,
    consumer: Option<preview::Consumer>,
    tier: preview::Tier,
    interactive: bool,
    foreground: bool,
    hydration: bool,
    touched: Instant,
    cancel: Cancellation,
}
struct Open {
    managed: Option<crate::catalog_session::ManagedSession>,
    closing: bool,
    exports: exports::Coordinator,
    token: String,
    catalog: Catalog,
    service: PreviewService,
    tickets: HashMap<String, Ticket>,
    deliveries: preview_delivery::Queue,
    index_pending: bool,
    jobs_held: bool,
    import: Option<ImportTask>,
    hydration: hydration::State,
    relink: relink::Coordinator,
}
struct ImportTask {
    import_lock: Option<crate::ImportLock>,
    preparation: Option<crate::import_preparation::Preparation>,
    reference: Option<crate::import_preparation::Reference>,
    status: ImportStatus,
    consumers: Vec<(preview::Consumer, NativePath)>,
    cancel: Cancellation,
    discovery_finished: bool,
    failure: bool,
}
impl ImportTask {
    fn terminal(&self) -> bool {
        matches!(
            self.status.phase,
            ImportPhase::Complete | ImportPhase::Canceled | ImportPhase::Failed
        )
    }
    fn update_counts(&mut self) {
        self.status.pending_previews = self.consumers.len() as u32;
    }
    fn request_cancel_owned(&mut self, service: &mut PreviewService) {
        self.update_counts();
        self.cancel.0.store(true, Ordering::Release);
        if let Some(preparation) = &mut self.preparation {
            preparation.request_cancel();
        }
        self.reference = None;
        for (consumer, _) in self.consumers.drain(..) {
            if let Err(e) = service.cancel(consumer) {
                self.failure = true;
                self.status.error =
                    Some(format!("cancel import: {e:#}").chars().take(2048).collect());
            }
        }
        self.status.pending_previews = 0;
        self.status.phase = ImportPhase::CancelRequested;
    }
    fn cancel_owned(&mut self, service: &mut PreviewService) {
        self.request_cancel_owned(service);
        self.preparation = None; // Join after signaling every owned consumer.
    }
}
#[derive(Clone)]
pub(crate) struct ManagedCatalogConfig {
    pub(crate) filesystem: Arc<dyn crate::catalog_session::CatalogFilesystem>,
}
struct Actor {
    migration: desktop::lightroom_migration::Admission,
    failed_admission: Option<crate::catalog_session::AdmissionCleanup>,
    failed_session: Option<crate::catalog_session::ManagedSession>,
    managed: Option<ManagedCatalogConfig>,
    next_admission: u64,
    #[cfg(test)]
    retained_on_drop: Option<mpsc::SyncSender<Open>>,
    lightroom: lightroom_bridge::Coordinator,
    config: Config,
    shared: Arc<Shared>,
    open: Option<Open>,
}
fn variant(v: VariantView) -> Variant {
    Variant {
        key: v.key,
        label: v.label,
        revision: I64(v.revision),
        recipe: v.recipe,
        recipe_digest: v.recipe_digest,
        can_undo: v.can_undo,
        can_redo: v.can_redo,
    }
}
fn identity_equal(
    a: &crate::catalog_edits::EditRenderIdentity,
    b: &crate::catalog_edits::EditRenderIdentity,
) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}
fn cancel_relink_consumers(open: &mut Open) {
    open.hydration.request_cancel();
    for ticket in open.tickets.values_mut() {
        if let Some(read) = ticket.read.take() {
            open.service.cancel_read(read);
        }
        if let Some(consumer) = ticket.consumer.take() {
            let _ = open.service.cancel(consumer);
            ticket.dto.state = PreviewState::CancelRequested;
        } else if !matches!(ticket.dto.state, PreviewState::Ready) {
            ticket.dto.state = PreviewState::Canceled;
        }
        ticket.hydration = false;
    }
}
fn during_relink_hold(request: &Request) -> bool {
    match request {
        Request::Status
        | Request::Close { .. }
        | Request::ImportStatus { .. }
        | Request::ImportCancel { .. }
        | Request::BackupStatus
        | Request::BackupCancel { .. }
        | Request::RestoreStatus { .. }
        | Request::Folders { .. }
        | Request::Images { .. }
        | Request::Search { .. }
        | Request::Image { .. }
        | Request::Variant { .. }
        | Request::Variants { .. }
        | Request::History { .. }
        | Request::Preview { .. }
        | Request::PreviewStatus { .. }
        | Request::ReleaseViewport { .. }
        | Request::CancelPreview { .. } => true,
        Request::Export { request, .. } => {
            request.read_only()
                || matches!(
                    request.as_ref(),
                    exports::Request::Cancel { .. } | exports::Request::Yield { .. }
                )
        }
        Request::EditCopy { request, .. } => request.read_only(),
        Request::Metadata { request, .. } => {
            !matches!(request.as_ref(), metadata::Request::Resolve { .. })
        }
        Request::Relink { request, .. } => request.read_only(),
        Request::Organization { request, .. } => matches!(
            request.as_ref(),
            organization::Request::Keywords { .. }
                | organization::Request::Synonyms { .. }
                | organization::Request::Collections { .. }
                | organization::Request::Collection { .. }
                | organization::Request::Placement { .. }
                | organization::Request::Members { .. }
                | organization::Request::Identity { .. }
                | organization::Request::Job { .. }
                | organization::Request::Jobs { .. }
                | organization::Request::Items { .. }
                | organization::Request::Review { .. }
        ),
        _ => false,
    }
}
impl Drop for Actor {
    fn drop(&mut self) {
        if self.open.is_some()
            && !std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.close()))
                .is_ok_and(|result| result.is_ok())
        {
            // Also runs during actor unwind, before automatic field destruction.
            // No catalog/import owner is released when drain cannot be verified.
            self.retain_open();
        }
    }
}
impl Actor {
    fn retain_open(&mut self) {
        #[cfg(test)]
        if let Some(tx) = self.retained_on_drop.take() {
            if let Some(open) = self.open.take()
                && let Err(e) = tx.send(open)
            {
                std::mem::forget(e.0);
            }
            return;
        }
        std::mem::forget(self.open.take());
    }
    fn new(config: Config, shared: Arc<Shared>) -> Self {
        Self {
            #[cfg(test)]
            retained_on_drop: None,
            managed: None,
            migration: Default::default(),
            failed_admission: None,
            failed_session: None,
            next_admission: 0,
            lightroom: lightroom_bridge::Coordinator::new(shared.lightroom.clone()),
            config,
            shared,
            open: None,
        }
    }
    fn set_phase(&self, phase: Phase, message: Option<String>) {
        let mut q = self.shared.queue.lock().unwrap();
        q.status.phase = phase;
        q.status.message = message;
    }
    fn run(mut self) {
        loop {
            let envelope = {
                let mut q = self.shared.queue.lock().unwrap();
                if q.stopping {
                    while let Some(e) = q.pending.pop_front() {
                        e.reject(ErrorCode::Closed, "catalog owner closing")
                    }
                    break;
                }
                if q.pending.is_empty() {
                    let (next, _) = self
                        .shared
                        .wake
                        .wait_timeout(q, Duration::from_millis(10))
                        .unwrap();
                    q = next;
                }
                let best = q
                    .pending
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.priority())
                    .map(|(i, _)| i);
                let e = best.and_then(|i| q.pending.remove(i));
                q.active_cancel = e.as_ref().map(|e| e.cancel.clone());
                e
            };
            if let Some(e) = envelope {
                if e.cancel.is_canceled() {
                    e.reject(ErrorCode::Canceled, "operation canceled before execution");
                } else if e.created.elapsed().as_secs() > self.config.limits.ttl_seconds {
                    e.reject(ErrorCode::Canceled, "queued operation expired");
                } else {
                    match e.work {
                        Work::MigrationAdmission(request, tx) => {
                            let _ = tx.send(self.migration_request(request, &e.cancel));
                        }
                        Work::Shutdown(tx) => {
                            self.shared
                                .lightroom
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .signal_shutdown();
                            let result = self.close();
                            if result.is_ok() {
                                self.lightroom.shutdown();
                                self.shared.queue.lock().unwrap().stopping = true;
                            }
                            let _ = tx.send(result);
                        }
                        Work::Command(r, tx) => {
                            if let Request::Export { catalog, request } = &r
                                && matches!(
                                    request.as_ref(),
                                    exports::Request::Cancel {
                                        operation: None,
                                        ..
                                    }
                                )
                            {
                                if let Err(e) = self.export_request(
                                    catalog,
                                    (**request).clone(),
                                    Some(tx.clone()),
                                ) {
                                    let _ = tx.send(reply(Err(e)));
                                }
                                continue;
                            }
                            let reindex = matches!(
                                &r,
                                Request::CreateVariant { .. }
                                    | Request::Cull { .. }
                                    | Request::Images { .. }
                                    | Request::Search { .. }
                                    | Request::Organization { .. }
                            ) || matches!(&r, Request::Metadata { request, .. } if matches!(request.as_ref(), metadata::Request::Resolve { .. }));
                            let reply_limit = if matches!(&r, Request::Lightroom { .. }) {
                                self.config
                                    .limits
                                    .reply_bytes
                                    .min(self.config.limits.request_bytes)
                                    .min(128 * 1024)
                            } else {
                                self.config.limits.reply_bytes
                            };
                            let result = self.command(r, &e.cancel);
                            if reindex && let Some(o) = self.open.as_mut() {
                                o.index_pending = true;
                            }
                            let out = reply(result);
                            let out = match serde_json::to_vec(&out) {
                                Ok(bytes) if bytes.len() <= reply_limit => out,
                                _ => failure(ErrorCode::ResourceLimit, "response byte limit"),
                            };
                            let _ = tx.send(out);
                        }
                        Work::Bytes {
                            catalog,
                            ticket,
                            reply,
                            ..
                        } => {
                            if self.managed.is_some() {
                                self.enqueue_encoded_delivery(catalog, ticket, e.cancel, reply);
                            } else {
                                let result = self.bytes(&catalog, &ticket, &e.cancel);
                                let _ = reply.send(result);
                            }
                        }
                    }
                }
            }
            let stopping = {
                let mut q = self.shared.queue.lock().unwrap();
                q.active_cancel = None;
                q.stopping
            };
            if !stopping && !self.migration.held() {
                self.maintain();
            }
        }
        self.shared
            .lightroom
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .signal_shutdown();
        if self.close().is_err() {
            // A terminal actor must not drop import/cache/catalog ownership while
            // a native worker's exit is unverified. Normal checked failure stays
            // in the loop instead, permitting an explicit Close/shutdown retry.
            self.retain_open();
        }
        self.lightroom.shutdown();
    }
    fn close(&mut self) -> std::result::Result<(), BridgeError> {
        if self.migration.held() {
            self.migration.cancel();
            return Err(error(
                ErrorCode::Busy,
                "migration descendants and permit must drain before Close",
            ));
        }
        self.set_phase(Phase::Closing, None);
        let result = self.close_inner();
        if let Err(e) = &result {
            self.set_phase(Phase::Closing, Some(e.message.clone()));
        }
        result
    }
    fn retained_admission_token(&self) -> Option<&str> {
        self.failed_admission
            .as_ref()
            .map(|c| c.session().as_str())
            .or_else(|| {
                self.failed_session
                    .as_ref()
                    .map(|s| s.bootstrap.session.as_str())
            })
    }
    fn publish_failed_admission(&self, message: String) {
        let token = self.retained_admission_token().map(str::to_owned);
        let mut queue = self.shared.queue.lock().unwrap();
        queue.status.phase = if token.is_some() {
            Phase::Closing
        } else {
            Phase::Failed
        };
        queue.status.catalog = token;
        queue.status.message = Some(if queue.status.catalog.is_some() {
            format!("Catalog admission failed; cleanup owner retained. Retry Close. {message}")
        } else {
            message
        });
    }
    fn close_inner(&mut self) -> std::result::Result<(), BridgeError> {
        if let Some(cleanup) = &mut self.failed_admission {
            cleanup.close().map_err(native)?;
        }
        self.failed_admission.take();
        if let Some(owner) = &mut self.failed_session {
            owner.close().map_err(native)?;
        }
        self.failed_session.take();
        if let Some(open) = self.open.as_mut() {
            open.closing = true;
            if let Some(pool) = open.catalog.session.pool() {
                pool.begin_close();
            }
            open.catalog.session.cancel_searches();
            if let Some(managed) = &mut open.managed
                && managed.sql_returned
            {
                managed.close().map_err(native)?;
            }
            if !open.managed.as_ref().is_some_and(|m| m.sql_returned) {
                open.deliveries.signal_shutdown(&mut open.service);
                open.exports.signal_shutdown(&self.shared.exports);
                open.service.signal_shutdown();
                self.shared.relink.lock().unwrap().request_cancel();
                open.hydration.request_cancel();
                if let Some(import) = &mut open.import {
                    import.request_cancel_owned(&mut open.service);
                }
                for (_, t) in open.tickets.drain() {
                    if let Some(read) = t.read {
                        open.service.cancel_read(read);
                    }
                    if let Some(c) = t.consumer {
                        let _ = open.service.cancel(c);
                    }
                }
                open.deliveries.shutdown(&mut open.service)?;
                self.shared
                    .backups
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .shutdown()
                    .map_err(native)?;
                open.relink.shutdown(&self.shared.relink);
                open.exports.shutdown(&self.shared.exports);
                copy::close(&mut open.catalog, &self.shared.copy);
                if let Some(import) = &mut open.import {
                    import.preparation = None;
                }
                // The complete Open, including import lock/catalog, remains owned on
                // failure. No background maintenance or new command can restart it.
                #[cfg(test)]
                if let Some(checkpoint) = &self.config.import_checkpoint
                    && let Some(import) = &open.import
                {
                    checkpoint("before_service_drop", &import.cancel.0);
                }
                open.service.try_shutdown().map_err(native)?;
                open.catalog.session.drain_searches().map_err(native)?;
                if let Some(managed) = &mut open.managed {
                    open.catalog.db.return_managed();
                    open.service.return_managed_sql();
                    managed.sql_returned = true;
                    managed.close().map_err(native)?;
                }
            }
        }
        if let Some(mut open) = self.open.take() {
            drop(open.hydration);
            drop(open.service);
            drop(open.import.take());
            drop(open.catalog);
        }
        self.shared
            .backups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown()
            .map_err(native)?;
        *self.shared.relink.lock().unwrap() = relink::Control::default();
        *self.shared.copy.lock().unwrap() = copy::Control::default();
        *self.shared.exports.lock().unwrap() = exports::Control::default();
        let mut q = self.shared.queue.lock().unwrap();
        q.viewport.clear();
        q.ticket_foreground.clear();
        q.import_status = None;
        q.import_cancel = None;
        q.status = Status {
            phase: Phase::Closed,
            catalog: None,
            jobs_held: false,
            pending_commands: 0,
            active_previews: 0,
            cancel_requested: false,
            message: None,
        };
        Ok(())
    }
    fn export_request(
        &mut self,
        catalog: &str,
        request: exports::Request,
        deferred: Option<mpsc::SyncSender<Reply>>,
    ) -> std::result::Result<Response, BridgeError> {
        let control = Arc::clone(&self.shared.exports);
        let config = self.config.clone();
        let copy_active = self
            .shared
            .copy
            .lock()
            .unwrap()
            .status
            .as_ref()
            .is_some_and(|s| {
                matches!(
                    s.phase,
                    copy::Phase::Running | copy::Phase::Paused | copy::Phase::CancelRequested
                )
            });
        let backup_active = self
            .shared
            .backups
            .lock()
            .unwrap()
            .status()
            .map_err(native)?
            .is_some_and(|s| {
                matches!(
                    s.state,
                    backup::State::Running | backup::State::CancelRequested
                )
            });
        let o = self.current(catalog)?;
        let held = o.jobs_held
            || o.relink.busy()
            || copy_active
            || backup_active
            || o.import.as_ref().is_some_and(|i| !i.terminal())
            || o.exports.write_hold(&control);
        Ok(Response::Export(Box::new(o.exports.execute(
            &mut o.catalog,
            request,
            &config,
            &control,
            held,
            deferred,
        )?)))
    }
    fn current(&mut self, token: &str) -> std::result::Result<&mut Open, BridgeError> {
        let open = self.current_for_close(token)?;
        if open
            .catalog
            .session
            .pool()
            .is_some_and(|pool| pool.is_poisoned())
        {
            open.closing = true;
        }
        if open.closing {
            return Err(error(
                ErrorCode::Busy,
                "catalog is closing; retry Close after cleanup failure",
            ));
        }
        Ok(open)
    }
    fn current_for_close(&mut self, token: &str) -> std::result::Result<&mut Open, BridgeError> {
        self.open
            .as_mut()
            .filter(|o| o.token == token)
            .ok_or_else(|| {
                error(
                    ErrorCode::StaleSession,
                    "catalog session unavailable or replaced",
                )
            })
    }
    fn status(&self) -> Status {
        self.shared.queue.lock().unwrap().status.clone()
    }
    fn open_path(
        &mut self,
        path: NativePath,
        create: bool,
        cancel: &Cancellation,
    ) -> std::result::Result<Response, BridgeError> {
        if self.open.is_some() || self.failed_admission.is_some() || self.failed_session.is_some() {
            return Err(error(
                ErrorCode::Busy,
                "close the current catalog before opening another",
            ));
        }
        if self.managed.is_some() {
            return self.open_managed_path(path, create, cancel);
        }
        let path = path
            .to_path()
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        if !path.is_absolute() {
            return Err(error(
                ErrorCode::InvalidRequest,
                "catalog path must be absolute",
            ));
        }
        if create {
            if path
                .try_exists()
                .map_err(|e| error(ErrorCode::Native, e.to_string()))?
            {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "new catalog destination already exists",
                ));
            }
        } else {
            let m = std::fs::symlink_metadata(path.join("catalog.sqlite3"))
                .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
            if !m.is_file() || m.file_type().is_symlink() {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "existing catalog database must be a regular file",
                ));
            }
        }
        let resolved = crate::prospective_directory(&path).map_err(native)?;
        for original in &self.config.original_roots {
            let original = crate::prospective_directory(original).map_err(native)?;
            if resolved.starts_with(&original) || original.starts_with(&resolved) {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "catalog and original roots must be separate",
                ));
            }
        }
        if create {
            std::fs::create_dir(&path).map_err(|e| {
                error(
                    ErrorCode::InvalidRequest,
                    format!("create new catalog directory: {e}"),
                )
            })?;
        }
        self.set_phase(Phase::Opening, None);
        let opened = (|| -> Result<Open> {
            let catalog = Catalog::open(&path)?;
            let jobs_held = catalog.restore_status()?.is_some_and(|s| s.jobs_held);
            let cache = match &self.config.cache_root {
                Some(parent) => parent.join(
                    blake3::hash(&serde_json::to_vec(&NativePath::from_path(&resolved))?)
                        .to_hex()
                        .as_str(),
                ),
                None => path.join("application-previews"),
            };
            let service = PreviewService::open(
                preview::StoreConfig {
                    manifest_root: cache.join("manifest"),
                    layout: preview::Layout::HashPrefix,
                    thumbnail_root: cache.join("thumbnail"),
                    large_root: cache.join("large"),
                    thumbnail_bytes: 2 * 1024 * 1024 * 1024,
                    large_bytes: 8 * 1024 * 1024 * 1024,
                },
                &self.config.original_roots,
                self.config.worker_executable.clone(),
                self.config.preview_policy.clone(),
                self.config.preview_limits.clone(),
            )?;
            Ok(Open {
                managed: None,
                closing: false,
                exports: exports::Coordinator::default(),
                token: uuid::Uuid::new_v4().to_string(),
                catalog,
                service,
                tickets: HashMap::new(),
                deliveries: preview_delivery::Queue::default(),
                index_pending: true,
                jobs_held,
                import: None,
                hydration: hydration::State::default(),
                relink: relink::Coordinator::default(),
            })
        })();
        match opened {
            Ok(open) => {
                if cancel.is_canceled() {
                    drop(open);
                    self.set_phase(Phase::Closed, None);
                    return Err(error(
                        ErrorCode::Canceled,
                        "opening canceled at completed native boundary",
                    ));
                }
                let held = open.jobs_held;
                let mut q = self.shared.queue.lock().unwrap();
                q.status.catalog = Some(open.token.clone());
                q.status.jobs_held = held;
                q.status.phase = Phase::Indexing;
                drop(q);
                self.open = Some(open);
                Ok(Response::Status(self.status()))
            }
            Err(e) => {
                let e = native(e);
                self.set_phase(Phase::Failed, Some(e.message.clone()));
                Err(e)
            }
        }
    }
    fn open_managed_path(
        &mut self,
        path: NativePath,
        create: bool,
        cancel: &Cancellation,
    ) -> std::result::Result<Response, BridgeError> {
        use crate::catalog_session::{BootstrapMode, LeaseId, ManagedSession, PrepareCatalog};
        let configured = self.managed.as_ref().unwrap().clone();
        // Full backup SQL isolation remains a separately tracked prerequisite;
        // this unselected bootstrap must never retire an active backup owner.
        self.shared
            .backups
            .lock()
            .unwrap()
            .require_idle_for_catalog_admission()
            .map_err(native)?;
        crate::catalog_session::validate_path(&path).map_err(native)?;
        let root = path.to_path().map_err(|e| native(e.into()))?;
        let resolved = crate::prospective_directory(&root).map_err(native)?;
        let cache = match &self.config.cache_root {
            Some(parent) => crate::prospective_directory(parent).map_err(native)?.join(
                blake3::hash(
                    &serde_json::to_vec(&NativePath::from_path(&resolved))
                        .map_err(|e| native(e.into()))?,
                )
                .to_hex()
                .as_str(),
            ),
            None => resolved.join("application-previews"),
        };
        self.next_admission = self.next_admission.checked_add(1).ok_or_else(|| {
            error(
                ErrorCode::ResourceLimit,
                "catalog admission sequence exhausted",
            )
        })?;
        let request = PrepareCatalog {
            operation: U64(self.next_admission),
            session: LeaseId::new(),
            mode: if create {
                BootstrapMode::DesktopCreate
            } else {
                BootstrapMode::DesktopExisting
            },
            root: path,
            manifest_root: NativePath::from_path(&cache.join("manifest")),
            import_source: None,
        };
        self.set_phase(Phase::Opening, None);
        let mut managed = match ManagedSession::admit(configured.filesystem, &request, &cancel.0) {
            Ok(owner) => owner,
            Err(failure) if failure.is_poisoned() => failure.retire_poisoned(),
            Err(failure) => {
                let message = failure.to_string();
                self.failed_admission = failure
                    .into_cleanup()
                    .filter(|cleanup| !cleanup.is_complete());
                self.publish_failed_admission(message.clone());
                return Err(error(ErrorCode::Native, message));
            }
        };
        let built = (|| -> anyhow::Result<(PreviewService, bool)> {
            let jobs_held = managed
                .catalog
                .as_ref()
                .unwrap()
                .restore_status()?
                .is_some_and(|s| s.jobs_held);
            let origin = if managed.bootstrap.manifest.created {
                preview::ManifestOrigin::CreatedByAdmission
            } else {
                preview::ManifestOrigin::Existing
            };
            let service = PreviewService::open_admitted(
                preview::StoreConfig {
                    manifest_root: managed
                        .bootstrap
                        .manifest
                        .path
                        .to_path()?
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("admitted manifest has no parent"))?
                        .to_path_buf(),
                    layout: preview::Layout::HashPrefix,
                    thumbnail_root: cache.join("thumbnail"),
                    large_root: cache.join("large"),
                    thumbnail_bytes: 2 * 1024 * 1024 * 1024,
                    large_bytes: 8 * 1024 * 1024 * 1024,
                },
                managed.manifest()?,
                origin,
                managed.store_files(cancel.0.clone())?,
                self.config.worker_executable.clone(),
                self.config.preview_policy.clone(),
                self.config.preview_limits.clone(),
            )?;
            Ok((service, jobs_held))
        })();
        match built {
            Ok((service, jobs_held)) => {
                let catalog = managed.catalog.take().unwrap();
                self.open = Some(Open {
                    managed: Some(managed),
                    closing: false,
                    exports: exports::Coordinator::default(),
                    token: uuid::Uuid::new_v4().to_string(),
                    catalog,
                    service,
                    tickets: HashMap::new(),
                    deliveries: preview_delivery::Queue::default(),
                    index_pending: true,
                    jobs_held,
                    import: None,
                    hydration: hydration::State::default(),
                    relink: relink::Coordinator::default(),
                });
                if cancel.is_canceled() {
                    self.close()?;
                    return Err(error(ErrorCode::Canceled, "catalog opening canceled"));
                }
                let open = self.open.as_ref().unwrap();
                let mut queue = self.shared.queue.lock().unwrap();
                queue.status.catalog = Some(open.token.clone());
                queue.status.jobs_held = open.jobs_held;
                queue.status.phase = Phase::Indexing;
                drop(queue);
                Ok(Response::Status(self.status()))
            }
            Err(failure) => {
                if managed.close().is_err() {
                    self.failed_session = Some(managed);
                }
                self.publish_failed_admission(format!("{failure:#}"));
                Err(native(failure))
            }
        }
    }
    fn command(
        &mut self,
        r: Request,
        cancel: &Cancellation,
    ) -> std::result::Result<Response, BridgeError> {
        if let Request::Lightroom { request } = r {
            if self.managed.is_some() {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "independent Lightroom requests belong to the desktop owner",
                ));
            }
            return self
                .lightroom
                .request(
                    *request,
                    &self.config.worker_executable,
                    self.config
                        .limits
                        .reply_bytes
                        .min(self.config.limits.request_bytes)
                        .min(128 * 1024),
                )
                .map(|r| Response::Lightroom(Box::new(r)))
                .map_err(native);
        }
        if (self.open.as_ref().is_some_and(|o| o.closing)
            || self.failed_admission.is_some()
            || self.failed_session.is_some())
            && !matches!(&r, Request::Status | Request::Close { .. })
        {
            return Err(error(
                ErrorCode::Busy,
                "catalog is closing; retry Close after cleanup failure",
            ));
        }
        if self.migration.held()
            && !matches!(
                &r,
                Request::Status
                    | Request::Close { .. }
                    | Request::Folders { .. }
                    | Request::Images { .. }
                    | Request::Search { .. }
                    | Request::Image { .. }
                    | Request::Variant { .. }
                    | Request::Variants { .. }
                    | Request::History { .. }
            )
        {
            return Err(error(
                ErrorCode::Busy,
                "migration target hold: catalog mutation waits for checked drain",
            ));
        }
        let limits = self.config.limits.clone();
        macro_rules! core {
            ($e:expr) => {
                $e.map_err(native)?
            };
        }
        macro_rules! page {
            ($n:expr) => {
                if $n == 0 || $n > limits.page_rows {
                    return Err(error(ErrorCode::InvalidRequest, "page row allowance"));
                }
            };
        }
        if self.open.as_ref().is_some_and(|o| o.relink.write_hold()) && !during_relink_hold(&r) {
            return Err(error(
                ErrorCode::Busy,
                "relink write hold: wait for completion or cancel the relink operation",
            ));
        }
        if self
            .open
            .as_ref()
            .is_some_and(|o| o.exports.write_hold(&self.shared.exports))
            && !during_relink_hold(&r)
        {
            return Err(error(
                ErrorCode::Busy,
                "export write hold: cached reads and cancellation remain available",
            ));
        }
        if self.shared.exports.lock().unwrap().busy()
            && (matches!(
                &r,
                Request::ImportStart { .. }
                    | Request::ImportResume { .. }
                    | Request::BackupCreate { .. }
                    | Request::BackupRestore { .. }
            ) || matches!(&r,Request::Relink{request,..} if !request.read_only())
                || matches!(&r,Request::EditCopy{request,..} if matches!(request.as_ref(),copy::Request::Run{..})))
        {
            return Err(error(
                ErrorCode::Busy,
                "finish or cancel the export operation before starting another catalog worker",
            ));
        }
        match r {
            Request::Lightroom { .. } => unreachable!("inspection dispatched independently"),
            Request::Export { catalog, request } => self.export_request(&catalog, *request, None),
            Request::EditCopy { catalog, request } => {
                let control = Arc::clone(&self.shared.copy);
                let o = self.current(&catalog)?;
                Ok(Response::EditCopy(Box::new(copy::execute(
                    &mut o.catalog,
                    *request,
                    &limits,
                    &control,
                    o.jobs_held || o.relink.write_hold(),
                )?)))
            }
            Request::Relink { catalog, request } => {
                let control = Arc::clone(&self.shared.relink);
                #[cfg(test)]
                let checkpoint = self.config.import_checkpoint.clone();
                let o = self.current(&catalog)?;
                #[cfg(test)]
                {
                    o.relink.checkpoint = checkpoint;
                }
                let commit = matches!(
                    request.as_ref(),
                    relink::Request::Apply { .. }
                        | relink::Request::Undo { .. }
                        | relink::Request::Confirm { .. }
                        | relink::Request::Revise { .. }
                );
                let response = if commit {
                    if o.jobs_held {
                        return Err(error(
                            ErrorCode::Busy,
                            "restored jobs remain held; review and explicitly resume first",
                        ));
                    }
                    if o.import.as_ref().is_some_and(|i| !i.terminal()) {
                        return Err(error(
                            ErrorCode::Busy,
                            "cancel or finish folder import before relink writes; resume import only after reviewing its new path",
                        ));
                    }
                    let pause = core!(o.service.pause_native_launches());
                    let response = o
                        .relink
                        .admit_commit(&o.catalog, *request, &control, pause)?;
                    cancel_relink_consumers(o);
                    response
                } else {
                    o.relink
                        .execute(&mut o.catalog, *request, &limits, &control, o.jobs_held)?
                };
                Ok(Response::Relink(Box::new(response)))
            }
            Request::OpenExisting { path } => self.open_path(path, false, cancel),
            Request::Create { path } => self.open_path(path, true, cancel),
            Request::Status | Request::ReleaseViewport { .. } => {
                Ok(Response::Status(self.status()))
            }
            Request::Close { catalog } => {
                if let Some(expected) = self.retained_admission_token() {
                    if catalog != expected {
                        return Err(error(
                            ErrorCode::StaleSession,
                            "retained catalog cleanup session changed",
                        ));
                    }
                } else {
                    self.current_for_close(&catalog)?;
                }
                self.close()?;
                Ok(Response::Status(self.status()))
            }
            Request::Metadata { catalog, request } => {
                Ok(Response::Metadata(Box::new(metadata::execute_cancellable(
                    &mut self.current(&catalog)?.catalog,
                    *request,
                    &limits,
                    cancel,
                )?)))
            }
            Request::Organization { catalog, request } => Ok(Response::Organization(Box::new(
                organization::execute(&mut self.current(&catalog)?.catalog, *request, &limits)?,
            ))),
            Request::ImportStart { catalog, source }
            | Request::ImportResume { catalog, source } => {
                let shared = Arc::clone(&self.shared);
                #[cfg(test)]
                let checkpoint = self.config.import_checkpoint.clone();
                let o = self.current(&catalog)?;
                if o.import.as_ref().is_some_and(|i| !i.terminal()) {
                    return Err(error(
                        ErrorCode::Busy,
                        "an import is still running or canceling",
                    ));
                }
                let path = source.to_path().map_err(|e| native(e.into()))?;
                if !path.is_absolute() {
                    return Err(error(
                        ErrorCode::InvalidRequest,
                        "import source must be absolute",
                    ));
                }
                let path = std::fs::canonicalize(path).map_err(|e| native(e.into()))?;
                // Both controls remain authoritative: restored-job hold and cache/source separation.
                core!(o.service.ensure_original_separate(&path));
                let import_lock = core!(crate::ImportLock::acquire(
                    &o.catalog.root.join("import.lock")
                ));
                let preparation = core!(crate::import_preparation::Preparation::spawn(
                    &o.catalog,
                    &path,
                    cancel.0.clone(),
                    #[cfg(test)]
                    checkpoint,
                ));
                let status = ImportStatus {
                    id: uuid::Uuid::new_v4().to_string(),
                    source: NativePath::from_path(&path),
                    phase: ImportPhase::Discovering,
                    imported: U64(0),
                    unchanged: U64(0),
                    failed: U64(0),
                    skipped: U64(0),
                    metadata_updated: U64(0),
                    metadata_warnings: U64(0),
                    awaiting_resources: U64(0),
                    pending_previews: 0,
                    error: None,
                    error_source: None,
                };
                o.import = Some(ImportTask {
                    import_lock: Some(import_lock),
                    preparation: Some(preparation),
                    reference: None,
                    status: status.clone(),
                    consumers: Vec::new(),
                    cancel: cancel.clone(),
                    discovery_finished: false,
                    failure: false,
                });
                let mut queue = shared.queue.lock().unwrap();
                queue.import_status = Some(status.clone());
                queue.import_cancel = Some(cancel.clone());
                Ok(Response::Import(Some(status)))
            }
            Request::ImportStatus { .. } | Request::ImportCancel { .. } => Ok(Response::Import(
                self.shared.queue.lock().unwrap().import_status.clone(),
            )),
            Request::BackupCreate { catalog, bundle } => {
                let source = NativePath::from_path(&self.current(&catalog)?.catalog.root);
                let snapshot = core!(
                    self.shared
                        .backups
                        .lock()
                        .unwrap()
                        .start(backup::Request::Create { source, bundle })
                );
                Ok(Response::Backup(Some(snapshot)))
            }
            Request::BackupInspect { bundle } => {
                let snapshot = core!(
                    self.shared
                        .backups
                        .lock()
                        .unwrap()
                        .start(backup::Request::Inspect { bundle })
                );
                Ok(Response::Backup(Some(snapshot)))
            }
            Request::BackupRestore {
                bundle,
                destination,
            } => {
                let snapshot = core!(self.shared.backups.lock().unwrap().start(
                    backup::Request::Restore {
                        bundle,
                        destination
                    }
                ));
                Ok(Response::Backup(Some(snapshot)))
            }
            Request::BackupStatus | Request::BackupCancel { .. } => Ok(Response::Backup(core!(
                self.shared.backups.lock().unwrap().status()
            ))),
            Request::RestoreStatus { catalog } => {
                let status = core!(self.current(&catalog)?.catalog.restore_status());
                Ok(Response::Restore(status.map(Into::into)))
            }
            Request::ResumeRestoredJobs {
                catalog,
                restore_id,
                acknowledge_pending_jobs,
            } => {
                let open = self.current(&catalog)?;
                let status = core!(
                    open.catalog
                        .resume_restored_jobs(&restore_id, acknowledge_pending_jobs)
                );
                open.jobs_held = status.jobs_held;
                self.shared.queue.lock().unwrap().status.jobs_held = status.jobs_held;
                Ok(Response::Restore(Some(status.into())))
            }
            Request::Folders {
                catalog,
                parent,
                after,
                limit,
            } => {
                page!(limit);
                if after.0 < 0 || parent.is_some_and(|p| p.0 <= 0) {
                    return Err(error(ErrorCode::InvalidRequest, "folder cursor"));
                }
                let o = self.current(&catalog)?;
                let rows = core!(o.catalog.organization_folders(
                    parent.map(|p| p.0),
                    after.0,
                    usize::from(limit)
                ));
                let next = (rows.len() == usize::from(limit)).then(|| I64(rows.last().unwrap().id));
                Ok(Response::Folders {
                    rows: rows
                        .into_iter()
                        .map(|f| Folder {
                            id: I64(f.id),
                            parent: f.parent.map(I64),
                            locator: f.locator,
                            name: f.name,
                        })
                        .collect(),
                    next,
                })
            }
            Request::Images {
                catalog,
                folder,
                recursive,
                text,
                cursor,
                limit,
            } => {
                let options = browse::Options {
                    folder,
                    folder_recursive: recursive,
                    text,
                    ..Default::default()
                };
                self.search_page(&catalog, options.query()?, cursor, limit)
            }
            Request::Search {
                catalog,
                options,
                cursor,
                limit,
            } => self.search_page(&catalog, (*options).query()?, cursor, limit),
            Request::Image { catalog, key } => {
                let o = self.current(&catalog)?;
                let image = core!(o.catalog.image(&key));
                let identity = core!(o.catalog.image_metadata_identity(&key));
                let v = core!(o.catalog.grid_image(&key, limits.page_bytes.min(16 * 1024)));
                let mut value = browse::grid_image(v, image)?;
                value.metadata_pending |= value.metadata_revision.0 != identity.metadata_revision;
                value.metadata_revision = I64(identity.metadata_revision);
                Ok(Response::Image(Box::new(value)))
            }
            Request::Variant { catalog, key } => Ok(Response::Variant(variant(core!(
                self.current(&catalog)?.catalog.edit_variant(&key)
            )))),
            Request::Variants {
                catalog,
                asset_id,
                after,
                limit,
            } => {
                page!(limit);
                let rows = core!(self.current(&catalog)?.catalog.edit_variants(
                    &asset_id,
                    after.0,
                    usize::from(limit)
                ));
                let next = (rows.len() == usize::from(limit)).then(|| I64(rows.last().unwrap().0));
                Ok(Response::Variants {
                    rows: rows
                        .into_iter()
                        .map(|(s, v)| (I64(s), variant(v)))
                        .collect(),
                    next,
                })
            }
            Request::CreateVariant {
                catalog,
                key,
                expected_revision,
                label,
            } => Ok(Response::Variant(variant(core!(
                self.current(&catalog)?.catalog.create_edit_variant(
                    &key,
                    expected_revision.0,
                    &label
                )
            )))),
            Request::SaveRecipe {
                catalog,
                key,
                expected_revision,
                recipe,
            } => Ok(Response::Variant(variant(core!(
                self.current(&catalog)?.catalog.save_edit_recipe(
                    &key,
                    expected_revision.0,
                    &recipe
                )
            )))),
            Request::Undo {
                catalog,
                key,
                expected_revision,
            } => Ok(Response::Variant(variant(core!(
                self.current(&catalog)?
                    .catalog
                    .undo_edit(&key, expected_revision.0)
            )))),
            Request::Redo {
                catalog,
                key,
                expected_revision,
            } => Ok(Response::Variant(variant(core!(
                self.current(&catalog)?
                    .catalog
                    .redo_edit(&key, expected_revision.0)
            )))),
            Request::History {
                catalog,
                key,
                after,
                limit,
            } => {
                page!(limit);
                let rows = core!(self.current(&catalog)?.catalog.edit_history(
                    &key,
                    after.0,
                    usize::from(limit)
                ));
                let next =
                    (rows.len() == usize::from(limit)).then(|| I64(rows.last().unwrap().revision));
                Ok(Response::History {
                    rows: rows
                        .into_iter()
                        .map(|h| HistoryEntry {
                            revision: I64(h.revision),
                            kind: h.kind,
                            recipe: h.recipe,
                            recipe_digest: h.recipe_digest,
                        })
                        .collect(),
                    next,
                })
            }
            Request::Cull {
                catalog,
                key,
                expected_revision,
                operation,
            } => {
                let operation = match operation {
                    CullOperation::Rating(value) => {
                        crate::organization::Operation::Rating { value }
                    }
                    CullOperation::Flag(value) => crate::organization::Operation::Flag { value },
                    CullOperation::Label(value) => crate::organization::Operation::Label { value },
                };
                Ok(Response::Culled {
                    metadata_revision: I64(core!(self.current(&catalog)?.catalog.organize_image(
                        &key,
                        expected_revision.0,
                        operation
                    ))),
                })
            }
            Request::Preview {
                catalog,
                key,
                tier,
                interactive,
                viewport,
                generation,
                foreground,
            } => {
                let shared = Arc::clone(&self.shared);
                let o = self.current(&catalog)?;
                if o.tickets.len() >= limits.tickets {
                    return Err(error(ErrorCode::ResourceLimit, "preview ticket count"));
                }
                let identity = core!(o.catalog.edit_render_identity(&key));
                let tier = match tier {
                    PreviewTier::Thumbnail => preview::Tier::Thumbnail,
                    PreviewTier::Large => preview::Tier::Large,
                };
                let read = if o.managed.is_some() {
                    Some(core!(o.service.queue_read_variant(
                        &o.catalog,
                        &key,
                        tier,
                        false,
                        if foreground {
                            preview::Priority::Foreground
                        } else {
                            preview::Priority::Background
                        },
                        interactive
                    )))
                } else {
                    None
                };
                let cached = if read.is_some() {
                    None
                } else if interactive {
                    core!(o.service.cached_interactive(&o.catalog, &key, tier, false))
                } else {
                    core!(o.service.cached_variant(&o.catalog, &key, tier, false))
                };
                if (o.relink.write_hold() || o.exports.write_hold(&shared.exports))
                    && cached.is_none()
                    && read.is_none()
                {
                    return Err(error(
                        ErrorCode::Busy,
                        "catalog write hold: cached previews remain available; request original rendering after completion",
                    ));
                }
                let needs_hydration = identity.source.state == "pending"
                    && identity.source.fingerprint.is_none()
                    && o.catalog
                        .preview_original_path(&key.asset_id)
                        .is_ok_and(|path| {
                            crate::initial_hydration_source(&o.catalog.db, &key.asset_id, &path)
                                .unwrap_or(false)
                        });
                let (state, consumer, message) = if read.is_some() {
                    (PreviewState::Queued, None, None)
                } else if cached.is_some() {
                    (PreviewState::Ready, None, None)
                } else if needs_hydration {
                    (
                        PreviewState::Queued,
                        None,
                        Some("preparing original".into()),
                    )
                } else if identity.source.state != "ready" {
                    (
                        PreviewState::Unavailable,
                        None,
                        Some("original unavailable; no current retained preview".into()),
                    )
                } else {
                    let priority = if foreground {
                        preview::Priority::Foreground
                    } else {
                        preview::Priority::Background
                    };
                    match if interactive {
                        o.service
                            .request_interactive(&mut o.catalog, &key, tier, priority)
                    } else {
                        o.service
                            .request_variant(&mut o.catalog, &key, tier, priority)
                    } {
                        Ok(c) => (PreviewState::Queued, Some(c), None),
                        Err(e) => (PreviewState::Failed, None, Some(format!("{e:#}"))),
                    }
                };
                let mut queue = shared.queue.lock().unwrap();
                if queue.viewport.get(&(catalog.clone(), viewport.clone())) != Some(&generation.0) {
                    if let Some(c) = consumer {
                        core!(o.service.cancel(c));
                    }
                    if let Some(read) = read {
                        o.service.cancel_read(read);
                    }
                    return Err(error(
                        ErrorCode::Superseded,
                        "viewport released during preview admission",
                    ));
                }
                let id = uuid::Uuid::new_v4().to_string();
                let dto = PreviewStatus {
                    ticket: id.clone(),
                    key,
                    revision: I64(identity.revision),
                    recipe_digest: identity.recipe_digest.clone(),
                    viewport,
                    generation,
                    state,
                    message,
                };
                o.tickets.insert(
                    id.clone(),
                    Ticket {
                        read,
                        dto: dto.clone(),
                        identity,
                        consumer,
                        tier,
                        interactive,
                        foreground,
                        hydration: needs_hydration && cached.is_none() && read.is_none(),
                        touched: Instant::now(),
                        cancel: cancel.clone(),
                    },
                );
                queue.ticket_foreground.insert(
                    (catalog, id),
                    TicketPriority {
                        foreground,
                        viewport: dto.viewport.clone(),
                        generation: dto.generation.0,
                    },
                );
                Ok(Response::Preview(dto))
            }
            Request::PreviewStatus { catalog, ticket } => {
                let t = self
                    .current(&catalog)?
                    .tickets
                    .get_mut(&ticket)
                    .ok_or_else(|| error(ErrorCode::StaleSession, "preview ticket expired"))?;
                t.touched = Instant::now();
                Ok(Response::Preview(t.dto.clone()))
            }
            Request::CancelPreview { catalog, ticket } => {
                let o = self.current(&catalog)?;
                let t = o
                    .tickets
                    .get_mut(&ticket)
                    .ok_or_else(|| error(ErrorCode::StaleSession, "preview ticket expired"))?;
                if let Some(read) = t.read.take() {
                    o.service.cancel_read(read);
                }
                if let Some(c) = t.consumer.take() {
                    core!(o.service.cancel(c));
                    t.dto.state = PreviewState::CancelRequested;
                } else {
                    t.dto.state = PreviewState::Canceled;
                }
                Ok(Response::Preview(t.dto.clone()))
            }
        }
    }
    fn bytes(
        &mut self,
        token: &str,
        id: &str,
        cancel: &Cancellation,
    ) -> std::result::Result<PreviewBytes, BridgeError> {
        let shared = Arc::clone(&self.shared);
        let limit = self.config.limits.binary_bytes;
        let policy = self.config.preview_policy.clone();
        let o = self.current(token)?;
        let t = o
            .tickets
            .get_mut(id)
            .ok_or_else(|| error(ErrorCode::StaleSession, "preview ticket expired"))?;
        if !matches!(t.dto.state, PreviewState::Ready) {
            return Err(error(ErrorCode::Busy, "preview is not ready"));
        }
        let before = o.catalog.edit_render_identity(&t.dto.key).map_err(native)?;
        if !identity_equal(&before, &t.identity) {
            t.dto.state = PreviewState::Stale;
            return Err(error(
                ErrorCode::Superseded,
                "preview edit identity changed",
            ));
        }
        let encoded = o
            .service
            .encoded_cached_variant(&o.catalog, &t.dto.key, t.tier, false, t.interactive)
            .map_err(native)?
            .ok_or_else(|| error(ErrorCode::Native, "current preview no longer cached"))?;
        let after = o.catalog.edit_render_identity(&t.dto.key).map_err(native)?;
        let live = {
            let queue = shared.queue.lock().unwrap();
            queue
                .viewport
                .get(&(token.to_owned(), t.dto.viewport.clone()))
                == Some(&t.dto.generation.0)
                && queue
                    .ticket_foreground
                    .contains_key(&(token.to_owned(), id.to_owned()))
        };
        if !identity_equal(&before, &after) || cancel.is_canceled() || !live {
            return Err(error(
                ErrorCode::Canceled,
                "preview changed or delivery canceled",
            ));
        }
        let n = encoded.bytes().len();
        shared
            .binary
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(n).filter(|sum| *sum <= limit)
            })
            .map_err(|_| error(ErrorCode::ResourceLimit, "binary transport byte allowance"))?;
        let bytes = encoded.bytes().to_vec();
        drop(encoded); // Bridge reservation already covers the owned copy.
        t.touched = Instant::now();
        let codec = match t.tier {
            preview::Tier::Thumbnail => policy.thumbnail.encoding.codec,
            preview::Tier::Large => policy.large.encoding.codec,
        };
        let mime = match codec {
            preview::Codec::Jpeg => "image/jpeg",
            preview::Codec::Webp => "image/webp",
            preview::Codec::Avif => "image/avif",
        }
        .into();
        Ok(PreviewBytes {
            mime,
            bytes,
            usage: Arc::clone(&shared.binary),
        })
    }
    fn maintain(&mut self) {
        self.lightroom.maintain();
        let Some(o) = self.open.as_mut() else { return };
        if o.catalog
            .session
            .pool()
            .is_some_and(|pool| pool.is_poisoned())
        {
            o.closing = true;
            let mut queue = self.shared.queue.lock().unwrap();
            queue.status.phase = Phase::Closing;
            queue.status.message = Some("Catalog SQL owner requires checked close".into());
        }
        if o.closing {
            return;
        }
        preview_delivery::advance(o, &self.shared, &self.config.preview_policy);
        let hold_since = o.exports.hold_since(&self.shared.exports);
        let prior_foreground = self
            .shared
            .queue
            .lock()
            .unwrap()
            .pending
            .iter()
            .any(|e| e.priority() <= 3 && hold_since.is_none_or(|at| e.created <= at));
        let native_demand = o.tickets.values().any(|t| {
            t.foreground
                && !t.cancel.is_canceled()
                && matches!(
                    t.dto.state,
                    PreviewState::Queued | PreviewState::CancelRequested
                )
        });
        if let Err(e) = o.exports.advance(
            &o.catalog,
            &mut o.service,
            &self.shared.exports,
            prior_foreground,
            native_demand,
            o.jobs_held || o.relink.write_hold(),
        ) {
            self.shared.queue.lock().unwrap().status.message = Some(e.message);
            self.shared.exports.lock().unwrap().request_cancel();
        }
        if o.exports.write_hold(&self.shared.exports) {
            self.shared.queue.lock().unwrap().status.active_previews =
                o.service.scheduler_usage().active as u32;
            return;
        }
        let relink_foreground = self
            .shared
            .queue
            .lock()
            .unwrap()
            .pending
            .iter()
            .any(|e| e.priority() <= 2);
        let held = o.relink.write_hold();
        o.relink.advance(
            &mut o.catalog,
            &self.config.limits,
            &self.shared.relink,
            relink_foreground,
        );
        if held && !o.relink.write_hold() {
            o.index_pending = true;
        }
        if o.relink.needs_finalization() {
            if o.import.as_ref().is_some_and(|i| !i.terminal()) {
                o.relink.fail(&self.shared.relink, "review preparation is saved; cancel or finish folder import, then explicitly prepare this checking plan again".into());
            } else {
                let result = o
                    .service
                    .pause_native_launches()
                    .map_err(native)
                    .and_then(|pause| {
                        o.relink
                            .admit_finalize(&o.catalog, &self.shared.relink, pause)
                    });
                if let Err(e) = result {
                    o.relink.fail(&self.shared.relink, e.message);
                } else {
                    cancel_relink_consumers(o);
                }
            }
        }
        let copy_foreground = self
            .shared
            .queue
            .lock()
            .unwrap()
            .pending
            .iter()
            .any(Envelope::before_copy);
        if copy::advance(
            &mut o.catalog,
            &self.shared.copy,
            copy_foreground,
            o.jobs_held || o.relink.write_hold(),
            #[cfg(test)]
            self.config.import_checkpoint.clone(),
        ) {
            o.index_pending = true;
        }
        if o.relink.write_hold() {
            if !o.relink.committing() {
                let readers_drained = o.hydration.drain_canceled();
                // All consumers were canceled before this tick; canceled jobs cannot publish.
                if let Err(e) = o.service.tick(&mut o.catalog) {
                    o.relink
                        .fail(&self.shared.relink, format!("draining previews: {e:#}"));
                }
                o.service.tick_read(&o.catalog);
                if readers_drained
                    && o.service.native_work_drained()
                    && let Err(e) = o.relink.start_commit(&self.shared.relink)
                {
                    o.relink.fail(&self.shared.relink, e.message);
                }
            }
            self.shared.queue.lock().unwrap().status.active_previews =
                o.service.scheduler_usage().active as u32;
            return;
        }
        let ttl = Duration::from_secs(self.config.limits.ttl_seconds);
        let (viewport, foreground) = {
            let q = self.shared.queue.lock().unwrap();
            (
                q.viewport.clone(),
                q.pending.iter().any(|e| e.priority() <= 2),
            )
        };
        let mut expired = Vec::new();
        for (id, t) in &mut o.tickets {
            let obsolete = viewport
                .get(&(o.token.clone(), t.dto.viewport.clone()))
                .is_none_or(|g| *g != t.dto.generation.0)
                || !self
                    .shared
                    .queue
                    .lock()
                    .unwrap()
                    .ticket_foreground
                    .contains_key(&(o.token.clone(), id.clone()));
            if obsolete || t.cancel.is_canceled() || t.touched.elapsed() > ttl {
                if let Some(read) = t.read.take() {
                    o.service.cancel_read(read);
                }
                if let Some(c) = t.consumer.take() {
                    let _ = o.service.cancel(c);
                }
                t.dto.state = if o.service.native_work_drained() {
                    PreviewState::Canceled
                } else {
                    PreviewState::CancelRequested
                };
                if obsolete || t.touched.elapsed() > ttl {
                    expired.push(id.clone());
                }
            }
        }
        for id in expired {
            o.tickets.remove(&id);
            self.shared
                .queue
                .lock()
                .unwrap()
                .ticket_foreground
                .remove(&(o.token.clone(), id));
        }
        if let Err(e) = o.service.tick(&mut o.catalog) {
            self.shared.queue.lock().unwrap().status.message =
                Some(format!("preview service: {e:#}"));
        }
        o.service.tick_read(&o.catalog);
        preview_delivery::advance(o, &self.shared, &self.config.preview_policy);
        for t in o.tickets.values_mut() {
            if let Some(read) = t.read
                && let Some(done) = o.service.take_read(read)
            {
                t.read = None;
                let current = o.catalog.edit_render_identity(&t.dto.key);
                if !current
                    .as_ref()
                    .is_ok_and(|v| identity_equal(v, &t.identity))
                {
                    t.dto.state = PreviewState::Stale;
                    continue;
                }
                match done.outcome {
                    preview::ReadOutcome::Ready(_) => {
                        t.dto.state = PreviewState::Ready;
                        t.dto.message = None;
                    }
                    preview::ReadOutcome::Stale => t.dto.state = PreviewState::Stale,
                    preview::ReadOutcome::Failed {
                        resource_limit,
                        message,
                    } => {
                        t.dto.state = if resource_limit {
                            PreviewState::NeedsResources
                        } else {
                            PreviewState::Failed
                        };
                        t.dto.message = Some(message);
                    }
                    preview::ReadOutcome::Missing => {
                        if o.relink.write_hold() || o.exports.write_hold(&self.shared.exports) {
                            t.dto.state = PreviewState::Unavailable;
                            t.dto.message = Some(
                                "catalog write hold; request original rendering after completion"
                                    .into(),
                            );
                            continue;
                        }
                        let identity = current.unwrap();
                        let hydration = identity.source.state == "pending"
                            && identity.source.fingerprint.is_none()
                            && o.catalog
                                .preview_original_path(&t.dto.key.asset_id)
                                .is_ok_and(|path| {
                                    crate::initial_hydration_source(
                                        &o.catalog.db,
                                        &t.dto.key.asset_id,
                                        &path,
                                    )
                                    .unwrap_or(false)
                                });
                        if hydration {
                            t.hydration = true;
                            t.dto.state = PreviewState::Queued;
                            t.dto.message = Some("preparing original".into());
                        } else if identity.source.state != "ready" {
                            t.dto.state = PreviewState::Unavailable;
                            t.dto.message =
                                Some("original unavailable; no current retained preview".into());
                        } else {
                            let priority = if t.foreground {
                                preview::Priority::Foreground
                            } else {
                                preview::Priority::Background
                            };
                            let request = if t.interactive {
                                o.service.request_interactive(
                                    &mut o.catalog,
                                    &t.dto.key,
                                    t.tier,
                                    priority,
                                )
                            } else {
                                o.service.request_variant(
                                    &mut o.catalog,
                                    &t.dto.key,
                                    t.tier,
                                    priority,
                                )
                            };
                            match request {
                                Ok(c) => {
                                    t.consumer = Some(c);
                                    t.dto.state = PreviewState::Queued;
                                }
                                Err(e) => {
                                    t.dto.state = PreviewState::Failed;
                                    t.dto.message = Some(e.to_string());
                                }
                            }
                        }
                    }
                }
            }
            if let Some(c) = t.consumer {
                if let Some(done) = o.service.take_completion(c) {
                    t.consumer = None;
                    if t.hydration && o.hydration.completed(&o.catalog, t, &done) {
                        o.index_pending = true;
                        continue;
                    }
                    t.hydration = false;
                    let (state, message) = match done {
                        preview::ServiceCompletion::Ready => (PreviewState::Ready, None),
                        preview::ServiceCompletion::Stale => (PreviewState::Stale, None),
                        preview::ServiceCompletion::Canceled => (PreviewState::Canceled, None),
                        preview::ServiceCompletion::NeedsResources(s) => {
                            (PreviewState::NeedsResources, Some(s))
                        }
                        preview::ServiceCompletion::Unavailable(s) => {
                            (PreviewState::Unavailable, Some(s))
                        }
                        preview::ServiceCompletion::Failed(s) => (PreviewState::Failed, Some(s)),
                    };
                    t.dto.state = state;
                    t.dto.message = message;
                }
            } else if matches!(t.dto.state, PreviewState::CancelRequested)
                && o.service.native_work_drained()
            {
                t.dto.state = PreviewState::Canceled;
            }
        }
        o.hydration.advance(
            &mut o.catalog,
            &mut o.service,
            &mut o.tickets,
            #[cfg(test)]
            self.config.import_checkpoint.clone(),
        );
        if let Some(import) = &mut o.import
            && !import.terminal()
        {
            let mut remaining = Vec::new();
            for (consumer, path) in import.consumers.drain(..) {
                if let Some(done) = o.service.take_completion(consumer) {
                    match &done {
                        preview::ServiceCompletion::Ready => import.status.imported.0 += 1,
                        preview::ServiceCompletion::NeedsResources(_)
                        | preview::ServiceCompletion::Unavailable(_) => {
                            import.status.awaiting_resources.0 += 1
                        }
                        _ => import.status.failed.0 += 1,
                    }
                    let message = match &done {
                        preview::ServiceCompletion::Failed(message)
                        | preview::ServiceCompletion::NeedsResources(message)
                        | preview::ServiceCompletion::Unavailable(message) => {
                            Some(message.as_str())
                        }
                        preview::ServiceCompletion::Stale => {
                            Some("preview changed during import; re-walk to refresh it")
                        }
                        preview::ServiceCompletion::Canceled => {
                            Some("import preview canceled; re-walk to retry it")
                        }
                        preview::ServiceCompletion::Ready => None,
                    };
                    if let Some(message) = message {
                        import.status.error = Some(message.chars().take(2048).collect());
                        import.status.error_source = Some(path);
                    }
                    o.index_pending = true;
                } else {
                    remaining.push((consumer, path));
                }
            }
            import.consumers = remaining;
            if import.cancel.is_canceled()
                && !matches!(import.status.phase, ImportPhase::CancelRequested)
            {
                import.cancel_owned(&mut o.service);
            }
            if matches!(import.status.phase, ImportPhase::CancelRequested) {
                if o.service.native_work_drained() {
                    import.status.phase = if import.failure {
                        ImportPhase::Failed
                    } else {
                        ImportPhase::Canceled
                    };
                }
            } else if !foreground
                && !import.discovery_finished
                && import.consumers.len() < 8
                && o.service.available_request_slots() > 1
            {
                let applied = (|| -> Result<()> {
                    let Some(event) = import
                        .preparation
                        .as_ref()
                        .context("missing source preparation")?
                        .poll()?
                    else {
                        return Ok(());
                    };
                    use crate::import_preparation::Event;
                    match event {
                        Event::Header(header) => {
                            ensure!(import.reference.is_none(), "unfinished import reference");
                            import.status.error_source = Some(NativePath::from_path(&header.path));
                            import.reference = Some(crate::import_preparation::Reference::begin(
                                &mut o.catalog,
                                *header,
                            )?);
                        }
                        Event::Source(source) => {
                            import
                                .reference
                                .as_mut()
                                .context("metadata without import reference")?
                                .source(&mut o.catalog, &source)?;
                        }
                        Event::End => {
                            let reference = import
                                .reference
                                .take()
                                .context("missing import reference")?;
                            let path = reference.source_path();
                            let (consumer, changed, warnings) =
                                reference.finish(&mut o.catalog, &mut o.service)?;
                            import.status.metadata_updated.0 += u64::from(changed);
                            import.status.metadata_warnings.0 += warnings;
                            if let Some(consumer) = consumer {
                                import.consumers.push((consumer, path));
                            } else {
                                import.status.unchanged.0 += 1;
                            }
                            if import.status.error.is_none() {
                                import.status.error_source = None;
                            }
                        }
                        Event::Skipped => import.status.skipped.0 += 1,
                        Event::Finished => {
                            ensure!(
                                import.reference.is_none(),
                                "source ended inside a prepared file"
                            );
                            import.discovery_finished = true;
                            if let Some(preparation) = import.preparation.take() {
                                preparation.finish();
                            }
                        }
                        Event::Failed { source, message } => {
                            import.status.error_source = Some(source);
                            anyhow::bail!("{message}");
                        }
                    }
                    o.index_pending = true;
                    Ok(())
                })();
                if let Err(e) = applied {
                    import.failure = true;
                    import.status.failed.0 += 1;
                    let message = format!("import stopped: {e:#}");
                    if let Some(reference) = &import.reference {
                        let _ = reference.fail(&mut o.catalog, &message);
                    }
                    import.status.error = Some(message.chars().take(2048).collect());
                    import.cancel_owned(&mut o.service);
                }
            }
            import.update_counts();
            if import.discovery_finished
                && matches!(
                    import.status.phase,
                    ImportPhase::Discovering | ImportPhase::Draining
                )
            {
                import.status.phase = ImportPhase::Draining;
                if import.consumers.is_empty() && !o.index_pending {
                    import.status.phase = ImportPhase::Complete;
                }
            }
            let mut queue = self.shared.queue.lock().unwrap();
            queue.import_status = Some(import.status.clone());
            if import.terminal() {
                import.import_lock = None;
                queue.import_cancel = None;
            }
        }
        if !foreground && o.index_pending {
            let result = (|| -> Result<bool> {
                let m = o.catalog.step_image_metadata_refresh(8)?;
                let i = o.catalog.organization_index(8)?;
                Ok(m.pending || i.pending)
            })();
            let mut q = self.shared.queue.lock().unwrap();
            match result {
                Ok(pending) => {
                    o.index_pending = pending;
                    q.status.phase = if pending {
                        Phase::Indexing
                    } else {
                        Phase::Ready
                    }
                }
                Err(e) => {
                    q.status.phase = Phase::Failed;
                    q.status.message = Some(format!("index maintenance: {e:#}"));
                }
            }
        }
        self.shared.queue.lock().unwrap().status.active_previews =
            o.service.scheduler_usage().active as u32;
    }
}
#[cfg(test)]
mod tests;

#[cfg(test)]
mod metadata_bridge_tests;
