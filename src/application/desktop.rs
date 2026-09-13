//! Additive desktop process façade. Production still selects the local Bridge.
//! Transport qualification does not establish SQL/source/FS custody isolation.
use super::{
    Bridge, BridgeError, Cancellation, Config, ErrorCode, Limits, Pending, PendingBytes,
    PreviewBytes, Reply, Request, error, failure,
};
use crate::storage_volume::NativePath;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};
mod process;
#[cfg(test)]
mod tests;
mod wire;
use wire::{BytesRequest, Kind, Message};
type Result<T> = std::result::Result<T, BridgeError>;
const CONTROL_SLOTS: usize = 16;

/// Closed means that the process was waited, not simply that its pipe ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportPhase {
    Starting,
    Ready,
    Draining,
    Failed,
    Closed,
}
#[derive(Debug, Clone)]
pub struct TransportStatus {
    pub phase: TransportPhase,
    pub pending: usize,
    pub outcome_unknown: bool,
    pub message: Option<String>,
    pub pid: u32,
}
enum Delivery {
    Command(mpsc::SyncSender<Reply>),
    Bytes {
        request: BytesRequest,
        reply: mpsc::SyncSender<Result<PreviewBytes>>,
    },
}
struct Entry {
    delivery: Delivery,
    cancel: Cancellation,
    sent_cancel: bool,
    control: bool,
}
struct State {
    phase: TransportPhase,
    message: Option<String>,
    unknown: bool,
    next: u64,
    pending: HashMap<u64, Entry>,
    control: VecDeque<Message>,
    data: VecDeque<Message>,
    ready: bool,
    stopping: bool,
    shutdown_attempt: u64,
    shutdown_sent: u64,
    drain_error: Option<String>,
    reaped: bool,
}
struct Shared {
    session: [u8; 16],
    limits: Limits,
    state: Mutex<State>,
    wake: Condvar,
    binary: Arc<AtomicUsize>,
}
impl Shared {
    fn fail(&self, message: impl Into<String>) {
        let mut s = self.state.lock().unwrap();
        if s.phase == TransportPhase::Closed {
            return;
        }
        s.unknown |= !s.pending.is_empty();
        s.message.get_or_insert(message.into());
        s.phase = TransportPhase::Failed;
        if !s.stopping { s.shutdown_attempt += 1; }
        s.stopping = true;
        self.wake.notify_all();
    }
    fn stop(&self) {
        let mut s = self.state.lock().unwrap();
        if s.phase == TransportPhase::Closed {
            return;
        }
        if s.phase != TransportPhase::Failed {
            s.phase = TransportPhase::Draining;
        }
        if !s.stopping || s.drain_error.take().is_some() { s.shutdown_attempt += 1; }
        s.stopping = true;
        for entry in s.pending.values() {
            if matches!(entry.delivery, Delivery::Bytes { .. }) {
                entry.cancel.0.store(true, Ordering::Release);
            }
        }
        self.wake.notify_all();
    }
    fn drain_failed(&self, attempt: u64, message: String) -> std::io::Result<()> {
        let mut s = self.state.lock().unwrap();
        if attempt < s.shutdown_attempt { return Ok(()); }
        if attempt == 0 || attempt != s.shutdown_attempt { return Err(wire::invalid("unowned drain response")); }
        s.drain_error = Some(message); s.phase = TransportPhase::Draining;
        self.wake.notify_all(); Ok(())
    }
    fn complete_failure(&self) {
        let entries = {
            let mut s = self.state.lock().unwrap();
            s.data.clear();
            s.control.clear();
            s.unknown |= !s.pending.is_empty();
            std::mem::take(&mut s.pending)
        };
        for (_, e) in entries {
            let message = "desktop process stopped; unacknowledged operation outcome is unknown; explicitly reopen and inspect saved state";
            match e.delivery {
                Delivery::Command(tx) => {
                    let _ = tx.send(failure(ErrorCode::Closed, message));
                }
                Delivery::Bytes { reply, .. } => {
                    let _ = reply.send(Err(error(ErrorCode::Closed, message)));
                }
            }
        }
    }
    fn acknowledge(&self, id: u64) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        if s.reaped {
            // The child's reservation was already released by verified process exit.
            return Ok(());
        }
        if s.control.len() >= self.limits.queued + CONTROL_SLOTS {
            return Err(error(
                ErrorCode::ResourceLimit,
                "desktop control queue limit",
            ));
        }
        s.control.push_back(Message::new(Kind::Ack, id, vec![]));
        self.wake.notify_all();
        Ok(())
    }
}
struct Handle {
    local: Bridge,
    shared: Arc<Shared>,
    process: Mutex<process::Owner>,
    pid: u32,
}
impl Handle {
    fn shutdown(&self) -> Result<()> {
        // Signal both independent owners before either potentially blocking join.
        self.shared.stop();
        self.local
            .0
            .shared
            .lightroom
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .signal_shutdown();
        let result = self.process.lock().unwrap().drain();
        self.local.shutdown();
        result
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        // A failed explicit drain retains custody; dropping the façade is not a retry.
        if self.shared.state.lock().unwrap().drain_error.is_none() {
            let _ = self.shutdown();
        }
    }
}

#[derive(Clone)]
pub struct DesktopBridge(Arc<Handle>);
impl DesktopBridge {
    pub fn spawn(config: Config) -> anyhow::Result<Self> {
        config.validate()?;
        let hello = serde_json::to_vec(&wire::ConfigWire::from_config(&config))?;
        anyhow::ensure!(
            hello.len() <= wire::CONFIG_BYTES,
            "desktop configuration byte limit"
        );
        let shared = Arc::new(Shared {
            session: *uuid::Uuid::new_v4().as_bytes(),
            limits: config.limits.clone(),
            state: Mutex::new(State {
                phase: TransportPhase::Starting,
                message: None,
                unknown: false,
                next: 1,
                pending: HashMap::new(),
                control: VecDeque::new(),
                data: VecDeque::new(),
                ready: false,
                stopping: false,
                shutdown_attempt: 0,
                shutdown_sent: 0,
                drain_error: None,
                reaped: false,
            }),
            wake: Condvar::new(),
            binary: Arc::new(AtomicUsize::new(0)),
        });
        let local = Bridge::spawn(config.clone())?;
        let owner = match process::Owner::spawn(&config.worker_executable, shared.clone(), hello) {
            Ok(owner) => owner,
            Err(e) => {
                local.shutdown();
                return Err(e);
            }
        };
        let pid = owner.pid();
        let result = Self(Arc::new(Handle {
            local,
            shared,
            process: Mutex::new(owner),
            pid,
        }));
        // Failure never force-kills a potentially descendant-owning actor. Drop drains it.
        let s = result.0.shared.state.lock().unwrap();
        let (s, timeout) = result
            .0
            .shared
            .wake
            .wait_timeout_while(s, std::time::Duration::from_secs(30), |s| {
                !s.ready && !s.stopping
            })
            .unwrap();
        let ready = s.ready && !s.stopping;
        let message = s.message.clone();
        drop(s);
        anyhow::ensure!(
            ready && !timeout.timed_out(),
            "desktop handshake failed: {}",
            message.as_deref().unwrap_or("timeout")
        );
        Ok(result)
    }
    pub fn status(&self) -> TransportStatus {
        let s = self.0.shared.state.lock().unwrap();
        TransportStatus {
            phase: s.phase.clone(),
            pending: s.pending.len(),
            outcome_unknown: s.unknown,
            message: s.drain_error.clone().or_else(|| s.message.clone()),
            pid: self.0.pid,
        }
    }
    /// Explicit fallible drain for owners that must retain admission after a wait failure.
    pub fn try_shutdown(&self) -> Result<()> {
        self.0.shutdown()
    }
    pub fn shutdown(&self) {
        let _ = self.try_shutdown();
    }
    pub fn submit(&self, request: Request) -> Result<Pending> {
        if local_route(&request) {
            return self.0.local.submit(request);
        }
        let control = control_route(&request);
        let retiring = if let Request::Close { catalog } = &request { Some(catalog.as_str()) } else { None };
        let bytes = serde_json::to_vec(&request)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        if bytes.len() > self.0.shared.limits.request_bytes {
            return Err(error(ErrorCode::ResourceLimit, "request byte limit"));
        }
        let (tx, receiver) = mpsc::sync_channel(1);
        let cancel = self.enqueue(Kind::Command, bytes, Delivery::Command(tx), control, retiring)?;
        Ok(Pending { receiver, cancel })
    }
    pub fn preview_bytes(
        &self,
        catalog: String,
        ticket: String,
        foreground: bool,
    ) -> Result<PendingBytes> {
        if catalog.len() > 128 || ticket.len() > 128 {
            return Err(error(ErrorCode::InvalidRequest, "ticket identity length"));
        }
        let request = BytesRequest {
            catalog,
            ticket,
            foreground,
        };
        let bytes = serde_json::to_vec(&request)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        let (reply, receiver) = mpsc::sync_channel(1);
        let cancel = self.enqueue(
            Kind::Bytes,
            bytes,
            Delivery::Bytes { request, reply },
            false,
            None,
        )?;
        Ok(PendingBytes { receiver, cancel })
    }
    fn enqueue(
        &self,
        kind: Kind,
        bytes: Vec<u8>,
        delivery: Delivery,
        control: bool,
        retiring: Option<&str>,
    ) -> Result<Cancellation> {
        let shared = &self.0.shared;
        let mut s = shared.state.lock().unwrap();
        if !s.ready || s.stopping {
            return Err(error(
                ErrorCode::Closed,
                "desktop owner unavailable; explicit reopen required",
            ));
        }
        let control = control && bytes.len() <= wire::CHUNK;
        let count = s.pending.values().filter(|p| p.control == control).count();
        if count
            >= if control {
                CONTROL_SLOTS
            } else {
                shared.limits.queued
            }
        {
            return Err(error(ErrorCode::Busy, "desktop admission queue full"));
        }
        let id = s.next;
        s.next = id
            .checked_add(1)
            .ok_or_else(|| error(ErrorCode::ResourceLimit, "request identifier exhausted"))?;
        if let Some(catalog) = retiring { retire_bytes(&s, catalog); }
        let weak = Arc::downgrade(shared);
        let cancel = Cancellation(
            Arc::new(AtomicBool::new(false)),
            Some(Arc::new(move || {
                if let Some(shared) = weak.upgrade() {
                    let _guard = shared.state.lock().unwrap();
                    shared.wake.notify_all();
                }
            })),
        );
        s.pending.insert(
            id,
            Entry {
                delivery,
                cancel: cancel.clone(),
                sent_cancel: false,
                control,
            },
        );
        let message = Message::new(kind, id, bytes);
        if control {
            s.control.push_back(message)
        } else {
            s.data.push_back(message)
        }
        shared.wake.notify_all();
        Ok(cancel)
    }
}

fn retire_bytes(state: &State, catalog: &str) {
    for entry in state.pending.values() {
        if matches!(&entry.delivery, Delivery::Bytes {request,..} if request.catalog == catalog) {
            entry.cancel.0.store(true, Ordering::Release);
        }
    }
}

/// Exhaustive top-level routing is intentional: a new capability needs a custody owner.
fn local_route(request: &Request) -> bool {
    match request {
        Request::Lightroom { .. } => true,
        Request::Export { .. }
        | Request::EditCopy { .. }
        | Request::Relink { .. }
        | Request::OpenExisting { .. }
        | Request::Create { .. }
        | Request::Status
        | Request::Close { .. }
        | Request::ImportStart { .. }
        | Request::ImportResume { .. }
        | Request::ImportStatus { .. }
        | Request::ImportCancel { .. }
        | Request::BackupCreate { .. }
        | Request::BackupInspect { .. }
        | Request::BackupRestore { .. }
        | Request::BackupStatus
        | Request::BackupCancel { .. }
        | Request::RestoreStatus { .. }
        | Request::ResumeRestoredJobs { .. }
        | Request::Folders { .. }
        | Request::Images { .. }
        | Request::Search { .. }
        | Request::Image { .. }
        | Request::Variant { .. }
        | Request::Variants { .. }
        | Request::CreateVariant { .. }
        | Request::SaveRecipe { .. }
        | Request::Undo { .. }
        | Request::Redo { .. }
        | Request::History { .. }
        | Request::Cull { .. }
        | Request::Preview { .. }
        | Request::PreviewStatus { .. }
        | Request::CancelPreview { .. }
        | Request::ReleaseViewport { .. }
        | Request::Metadata { .. }
        | Request::Organization { .. } => false,
    }
}
fn control_route(r: &Request) -> bool {
    matches!(
        r,
        Request::Status
            | Request::Close { .. }
            | Request::BackupStatus
            | Request::BackupCancel { .. }
            | Request::ImportStatus { .. }
            | Request::ImportCancel { .. }
            | Request::CancelPreview { .. }
            | Request::ReleaseViewport { .. }
    ) || matches!(r, Request::Export {request,..} if matches!(request.as_ref(),super::exports::Request::Status{..}|super::exports::Request::Cancel{..}|super::exports::Request::Yield{..}))
        || matches!(r, Request::Relink {request,..} if matches!(request.as_ref(),super::relink::Request::Status{..}|super::relink::Request::Cancel{..}))
        || matches!(r, Request::EditCopy {request,..} if matches!(request.as_ref(),super::copy::Request::Status{..}|super::copy::Request::Cancel{..}))
}

/// Hidden installed/CLI worker mode. No webview initialization occurs here.
pub fn worker_main() -> anyhow::Result<()> {
    process::worker_main()
}
