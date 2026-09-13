//! UI-independent catalog owner. Requests are bounded and native workers remain
//! owned/reaped by PreviewService; no webview thread touches SQLite.
pub mod browse;
mod dto;
use crate::{
    Catalog,
    catalog_edits::{VariantKey, VariantView},
    organization_search::{Cursor, Query},
    preview::{self, PreviewService},
    storage_volume::NativePath,
};
use anyhow::{Result, ensure};
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
}
#[derive(Clone)]
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
        Ok(())
    }
}
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release)
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
    error(ErrorCode::Native, format!("{e:#}"))
}
fn reply(r: std::result::Result<Response, BridgeError>) -> Reply {
    match r {
        Ok(value) => Reply::Ok { value },
        Err(error) => Reply::Error { error },
    }
}

enum Work {
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
    fn reject(self, code: ErrorCode, message: &str) {
        match self.work {
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
    ticket_foreground: HashMap<(String, String), TicketPriority>,
}
struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    limits: Limits,
    binary: Arc<AtomicUsize>,
}
struct Handle {
    shared: Arc<Shared>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Handle {
    fn shutdown(&self) {
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.stopping = true;
            if let Some(c) = &q.active_cancel {
                c.cancel();
            }
            self.shared.wake.notify_all();
        }
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
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
        config.validate()?;
        let shared = Arc::new(Shared {
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
                ticket_foreground: HashMap::new(),
            }),
            wake: Condvar::new(),
            limits: config.limits.clone(),
            binary: Arc::new(AtomicUsize::new(0)),
        });
        let actor_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("catalog-application".into())
            .spawn(move || Actor::new(config, actor_shared).run())?;
        Ok(Self(Arc::new(Handle {
            shared,
            thread: Mutex::new(Some(thread)),
        })))
    }
    /// Joins the owner after worker cleanup; all cloned handles become closed.
    pub fn shutdown(&self) {
        self.0.shutdown()
    }
    pub fn submit(&self, request: Request) -> std::result::Result<Pending, BridgeError> {
        let size = serde_json::to_vec(&request)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?
            .len();
        if size > self.0.shared.limits.request_bytes {
            return Err(error(ErrorCode::ResourceLimit, "request byte limit"));
        }
        let cancel = Cancellation::default();
        let (tx, receiver) = mpsc::sync_channel(1);
        let mut q = self.0.shared.queue.lock().unwrap();
        if q.stopping {
            return Err(error(ErrorCode::Closed, "catalog owner closed"));
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
    dto: PreviewStatus,
    identity: crate::catalog_edits::EditRenderIdentity,
    consumer: Option<preview::Consumer>,
    tier: preview::Tier,
    interactive: bool,
    touched: Instant,
    cancel: Cancellation,
}
struct Open {
    token: String,
    catalog: Catalog,
    service: PreviewService,
    tickets: HashMap<String, Ticket>,
    index_pending: bool,
    jobs_held: bool,
}
struct Actor {
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
impl Actor {
    fn new(config: Config, shared: Arc<Shared>) -> Self {
        Self {
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
                        Work::Command(r, tx) => {
                            let reindex = matches!(
                                &r,
                                Request::CreateVariant { .. }
                                    | Request::Cull { .. }
                                    | Request::Images { .. }
                                    | Request::Search { .. }
                            );
                            let result = self.command(r, &e.cancel);
                            if reindex && let Some(o) = self.open.as_mut() {
                                o.index_pending = true;
                            }
                            let out = reply(result);
                            let out = match serde_json::to_vec(&out) {
                                Ok(bytes) if bytes.len() <= self.config.limits.reply_bytes => out,
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
                            let result = self.bytes(&catalog, &ticket, &e.cancel);
                            let _ = reply.send(result);
                        }
                    }
                }
            }
            let stopping = {
                let mut q = self.shared.queue.lock().unwrap();
                q.active_cancel = None;
                q.stopping
            };
            if !stopping {
                self.maintain();
            }
        }
        self.close();
    }
    fn close(&mut self) {
        self.set_phase(Phase::Closing, None);
        if let Some(mut open) = self.open.take() {
            for (_, t) in open.tickets.drain() {
                if let Some(c) = t.consumer {
                    let _ = open.service.cancel(c);
                }
            }
            drop(open.service); // worker Drop kills/waits before cache lock release
            drop(open.catalog);
        }
        let mut q = self.shared.queue.lock().unwrap();
        q.viewport.clear();
        q.ticket_foreground.clear();
        q.status = Status {
            phase: Phase::Closed,
            catalog: None,
            jobs_held: false,
            pending_commands: 0,
            active_previews: 0,
            cancel_requested: false,
            message: None,
        };
    }
    fn current(&mut self, token: &str) -> std::result::Result<&mut Open, BridgeError> {
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
        if self.open.is_some() {
            return Err(error(
                ErrorCode::Busy,
                "close the current catalog before opening another",
            ));
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
            let jobs_held =
                crate::catalog_backup::restore_status(&path)?.is_some_and(|s| s.jobs_held);
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
                token: uuid::Uuid::new_v4().to_string(),
                catalog,
                service,
                tickets: HashMap::new(),
                index_pending: true,
                jobs_held,
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
    fn command(
        &mut self,
        r: Request,
        cancel: &Cancellation,
    ) -> std::result::Result<Response, BridgeError> {
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
        match r {
            Request::OpenExisting { path } => self.open_path(path, false, cancel),
            Request::Create { path } => self.open_path(path, true, cancel),
            Request::Status | Request::ReleaseViewport { .. } => {
                Ok(Response::Status(self.status()))
            }
            Request::Close { catalog } => {
                self.current(&catalog)?;
                self.close();
                Ok(Response::Status(self.status()))
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
                if v.image_id != image.id
                    || v.asset_id != key.asset_id
                    || v.variant_id != key.variant_id
                {
                    return Err(error(ErrorCode::Native, "logical image identity changed"));
                }
                Ok(Response::Image(GridImage {
                    image_id: v.image_id,
                    key,
                    sequence: I64(v.sequence),
                    metadata_revision: I64(identity.metadata_revision),
                    metadata_pending: v.metadata_pending
                        || v.metadata_revision != identity.metadata_revision,
                    state: v.state,
                    filename: v.filename,
                    rating: v.rating.map(I64),
                    flag: v.flag,
                    label: v.label,
                    conflicts: v.conflicts,
                }))
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
                let cached = if interactive {
                    core!(o.service.cached_interactive(&o.catalog, &key, tier, false))
                } else {
                    core!(o.service.cached_variant(&o.catalog, &key, tier, false))
                };
                let (state, consumer, message) = if cached.is_some() {
                    (PreviewState::Ready, None, None)
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
                        dto: dto.clone(),
                        identity,
                        consumer,
                        tier,
                        interactive,
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
        let Some(o) = self.open.as_mut() else { return };
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
        for t in o.tickets.values_mut() {
            if let Some(c) = t.consumer {
                if let Some(done) = o.service.take_completion(c) {
                    t.consumer = None;
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
