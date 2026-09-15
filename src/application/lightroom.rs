//! Serialized desktop inspection owner. No actor dispatch or automatic import.
//!
//! The caller must give this owner exclusive same-process access to inspection
//! SQLite. Close is two-phase: request_close signals first; poll_closed joins
//! only a finished thread. Drop performs an owned blocking drain off the actor.
use super::{I64, U64};
use crate::{
    lightroom::{self as core, control::Control, selection},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};
mod worker;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub request_bytes: usize,
    pub result_bytes: usize,
    pub page_bytes: usize,
    pub row_bytes: usize,
    pub native_path_units: usize,
    pub vm_steps: u64,
    pub deadline_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            request_bytes: core::MANIFEST_BYTES,
            result_bytes: 64 * 1024 * 1024,
            page_bytes: core::PAGE_BYTES,
            row_bytes: 32 * 1024 * 1024,
            native_path_units: 32768,
            vm_steps: 1_000_000_000,
            deadline_ms: 600_000,
        }
    }
}
impl Limits {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1024..=core::MANIFEST_BYTES).contains(&self.request_bytes)
                && (1024..=core::PAGE_BYTES).contains(&self.page_bytes),
            "workbench request/page byte limit"
        );
        ensure!(
            (1..=1024 * 1024).contains(&self.native_path_units),
            "workbench native path limit"
        );
        self.control(Arc::new(AtomicBool::new(false)))?;
        Ok(())
    }
    fn control(&self, cancel: Arc<AtomicBool>) -> Result<Control> {
        Control::new(
            cancel,
            self.vm_steps,
            self.deadline_ms,
            self.row_bytes,
            self.result_bytes,
        )
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum OpenMode {
    Create,
    OpenExisting,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub root: NativePath,
    pub mode: OpenMode,
    pub capture_executable: NativePath,
    pub capture_staging: NativePath,
    pub limits: Limits,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum OriginalInspection {
    MetadataOnly,
    Packets,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Action {
    Discover {
        root: NativePath,
        limits: core::Limits,
    },
    Capture {
        request: core::capture::Request,
    },
    RegisterInventory {
        inventory: core::discovery::Inventory,
    },
    AddCapture {
        directory: NativePath,
    },
    Resume {
        revision: String,
        max_rows: U64,
    },
    /// Separate explicit authority; no other action calls original inspection.
    InspectOriginals {
        revision: String,
        limit: U64,
        inspection: OriginalInspection,
    },
    AssignFamily {
        revision: String,
        family: String,
        reason: String,
    },
    Choose {
        family: String,
        revision: String,
        expected_evidence: String,
        reason: String,
    },
    PrepareSelection {
        request: selection::SelectionRequest,
        limits: selection::SelectionLimits,
    },
    Seal {
        review_token: String,
        approval_blake3: String,
        approval_json: String,
        output: NativePath,
    },
    ApprovalDocuments {
        draft_json: String,
    },
    ReleaseReview,
}
impl Action {
    fn writes(&self) -> bool {
        matches!(
            self,
            Self::RegisterInventory { .. }
                | Self::AddCapture { .. }
                | Self::Resume { .. }
                | Self::InspectOriginals { .. }
                | Self::AssignFamily { .. }
                | Self::Choose { .. }
                | Self::ReleaseReview
        )
    }
    fn allowed_during_review(&self) -> bool {
        matches!(
            self,
            Self::Seal { .. } | Self::ApprovalDocuments { .. } | Self::ReleaseReview
        )
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum ReviewCollection {
    Families,
    Captures,
    UninspectedCandidates,
    ConflictSample,
    PathCollisionSample,
}
impl From<ReviewCollection> for selection::ReviewCollection {
    fn from(value: ReviewCollection) -> Self {
        match value {
            ReviewCollection::Families => Self::Families,
            ReviewCollection::Captures => Self::Captures,
            ReviewCollection::UninspectedCandidates => Self::UninspectedCandidates,
            ReviewCollection::ConflictSample => Self::ConflictSample,
            ReviewCollection::PathCollisionSample => Self::PathCollisionSample,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Query {
    CaptureManifest {
        directory: NativePath,
    },
    Rows {
        revision: String,
        table: Option<String>,
        after: I64,
        limit: U64,
    },
    Report {
        revision: String,
    },
    Paths {
        revision: String,
        after: I64,
        limit: U64,
    },
    Issues {
        revision: String,
        after: I64,
        limit: U64,
    },
    Packets {
        revision: String,
        after: I64,
        limit: U64,
    },
    PacketBytes {
        revision: String,
        sequence: I64,
        decoded: bool,
        offset: I64,
        limit: U64,
    },
    MetadataConflicts {
        revision: String,
        after: I64,
        limit: U64,
    },
    GlobalIdConflicts {
        left: String,
        right: String,
        after_left: String,
        after_right: String,
        limit: U64,
    },
    PathCollisions {
        left: String,
        right: String,
        after_left: I64,
        after_right: I64,
        limit: U64,
    },
    Families,
    SelectionSummary,
    SelectionSources {
        review_token: String,
        revision: String,
        after: I64,
        limit: U64,
    },
    SelectionPreparation {
        review_token: String,
        document: selection::PreparationDocument,
        offset: U64,
        limit: U64,
    },
    SelectionPage {
        review_token: String,
        collection: ReviewCollection,
        after: U64,
        limit: U64,
    },
}
impl Query {
    fn review(&self) -> bool {
        matches!(
            self,
            Self::SelectionSummary
                | Self::SelectionPage { .. }
                | Self::SelectionPreparation { .. }
                | Self::SelectionSources { .. }
        )
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Phase {
    Opening,
    Running,
    Complete,
    Failed,
    CancelRequested,
    Canceled,
    Closing,
    Closed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub workbench: String,
    pub generation: String,
    pub operation: String,
    pub phase: Phase,
    pub initialized: bool,
    pub closed: bool,
    pub root: NativePath,
    pub limits: Limits,
    pub processed: U64,
    pub result_token: Option<String>,
    pub result_bytes: U64,
    pub review_token: Option<String>,
    pub capture_pid: Option<u32>,
    pub capture_staging: Option<NativePath>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResultPage {
    pub workbench: String,
    pub generation: String,
    pub operation: String,
    pub token: String,
    pub offset: U64,
    pub next: Option<U64>,
    pub total_bytes: U64,
    /// Exact UTF-8 fragment of opaque core JSON; reassembly preserves all source
    /// numeric lexemes. The later bridge must not parse IDs as JS doubles.
    pub json_fragment: String,
}
struct Cached {
    generation: String,
    operation: String,
    token: String,
    json: String,
}
struct Shared {
    status: Status,
    control: Control,
    result: Option<Arc<Cached>>,
}
enum Message {
    Action {
        operation: String,
        generation: String,
        action: PendingAction,
        control: Control,
    },
    Read {
        operation: String,
        generation: String,
        query: Query,
        control: Control,
    },
    Close,
}
/// Cloneable cached control; owns no join handle or SQLite connection.
#[derive(Clone)]
pub struct WorkbenchControl {
    shared: Arc<Mutex<Shared>>,
    closing: Arc<AtomicBool>,
    sender: mpsc::SyncSender<Message>,
}
pub struct Workbench {
    control: WorkbenchControl,
    join: Option<JoinHandle<()>>,
}
impl std::ops::Deref for Workbench {
    type Target = WorkbenchControl;
    fn deref(&self) -> &Self::Target {
        &self.control
    }
}
/// Only these bounded transport payloads are decoded on the inspection owner.
/// The enum fixes generation/review authority; callers cannot supply flags.
pub(crate) struct Payload {
    pub chunks: Arc<Vec<String>>,
    pub bytes: usize,
}
impl Payload {
    fn json(&self, control: &Control) -> Result<String> {
        let mut json = String::new();
        json.try_reserve_exact(self.bytes)?;
        for chunk in self.chunks.iter() {
            control.check()?;
            json.push_str(chunk);
        }
        ensure!(
            json.len() == self.bytes,
            "inspection payload byte count differs"
        );
        Ok(json)
    }
}
pub(crate) enum DeferredAction {
    Inventory(Payload),
    Selection {
        json: Payload,
        limits: selection::SelectionLimits,
    },
    Seal {
        json: Payload,
        review_token: String,
        approval_blake3: String,
        output: NativePath,
    },
    ApprovalDocuments {
        json: Payload,
    },
}
enum PendingAction {
    Typed(Action),
    Deferred(DeferredAction),
}
impl PendingAction {
    fn writes(&self) -> bool {
        match self {
            Self::Typed(a) => a.writes(),
            Self::Deferred(DeferredAction::Inventory(_)) => true,
            _ => false,
        }
    }
    fn allowed_during_review(&self) -> bool {
        match self {
            Self::Typed(a) => a.allowed_during_review(),
            Self::Deferred(
                DeferredAction::Seal { .. } | DeferredAction::ApprovalDocuments { .. },
            ) => true,
            _ => false,
        }
    }
    fn decode(self, control: &Control) -> Result<Action> {
        control.check()?;
        let action = match self {
            Self::Typed(a) => a,
            Self::Deferred(DeferredAction::Inventory(json)) => Action::RegisterInventory {
                inventory: serde_json::from_str(&json.json(control)?)?,
            },
            Self::Deferred(DeferredAction::Selection { json, limits }) => {
                Action::PrepareSelection {
                    request: serde_json::from_str(&json.json(control)?)?,
                    limits,
                }
            }
            Self::Deferred(DeferredAction::Seal {
                json,
                review_token,
                approval_blake3,
                output,
            }) => Action::Seal {
                approval_json: json.json(control)?,
                review_token,
                approval_blake3,
                output,
            },
            Self::Deferred(DeferredAction::ApprovalDocuments { json }) => {
                Action::ApprovalDocuments {
                    draft_json: json.json(control)?,
                }
            }
        };
        control.check()?;
        Ok(action)
    }
}

fn token() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn native_units(path: &NativePath, maximum: usize) -> Result<()> {
    let (n, nul) = match path {
        NativePath::UnixBytes(v) => (v.len(), v.contains(&0)),
        NativePath::WindowsWide(v) => (v.len(), v.contains(&0)),
    };
    ensure!(
        n > 0 && n <= maximum && !nul,
        "workbench native path admission"
    );
    Ok(())
}
fn native(path: &NativePath, maximum: usize) -> Result<std::path::PathBuf> {
    native_units(path, maximum)?;
    let value = path.to_path()?;
    ensure!(value.is_absolute(), "workbench path must be absolute");
    Ok(value)
}
impl Workbench {
    pub fn spawn(config: Config) -> Result<Self> {
        Self::spawn_inner(config, || {})
    }
    #[cfg(test)]
    pub(crate) fn spawn_held(
        config: Config,
        before: impl FnOnce() + Send + 'static,
    ) -> Result<Self> {
        Self::spawn_inner(config, before)
    }
    fn spawn_inner(config: Config, before: impl FnOnce() + Send + 'static) -> Result<Self> {
        config.limits.validate()?;
        core::bounded_json(&config, config.limits.request_bytes)?;
        for path in [
            &config.root,
            &config.capture_executable,
            &config.capture_staging,
        ] {
            native(path, config.limits.native_path_units)?;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let control = config.limits.control(cancel)?;
        let shared = Arc::new(Mutex::new(Shared {
            status: Status {
                workbench: token(),
                generation: token(),
                operation: token(),
                phase: Phase::Opening,
                initialized: false,
                closed: false,
                root: config.root.clone(),
                limits: config.limits.clone(),
                processed: U64(0),
                result_token: None,
                result_bytes: U64(0),
                review_token: None,
                capture_pid: None,
                capture_staging: None,
                error: None,
            },
            control,
            result: None,
        }));
        let closing = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(1);
        let state = shared.clone();
        let stop = closing.clone();
        let join = thread::Builder::new()
            .name("lightroom-workbench".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    before();
                    worker::run(config, receiver, state.clone(), stop)
                }));
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.status.closed = true;
                s.status.capture_pid = None;
                s.status.review_token = None;
                if outcome.is_err() {
                    s.status.error = Some(
                        "inspection worker panicked; retained artifacts require explicit review"
                            .into(),
                    );
                    s.status.phase = Phase::Failed;
                } else {
                    s.status.phase = Phase::Closed;
                }
            })?;
        Ok(Self {
            control: WorkbenchControl {
                shared,
                closing,
                sender,
            },
            join: Some(join),
        })
    }
    pub fn control(&self) -> WorkbenchControl {
        self.control.clone()
    }
}
impl WorkbenchControl {
    pub fn status(&self) -> Status {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = s.status.clone();
        out.processed = U64(s.control.processed.load(Ordering::Acquire));
        out
    }
    pub fn start(&self, expected_generation: &str, action: Action) -> Result<String> {
        self.submit(
            expected_generation,
            Some(PendingAction::Typed(action)),
            None,
            None,
        )
    }
    pub fn read(&self, expected_generation: &str, query: Query) -> Result<String> {
        self.submit(expected_generation, None, Some(query), None)
    }
    fn submit(
        &self,
        expected: &str,
        action: Option<PendingAction>,
        query: Option<Query>,
        expected_operation: Option<&str>,
    ) -> Result<String> {
        let limits = self.status().limits;
        match &action {
            Some(PendingAction::Typed(a)) => {
                core::bounded_json(a, limits.request_bytes)?;
            }
            Some(PendingAction::Deferred(a)) => {
                let json = match a {
                    DeferredAction::Inventory(j)
                    | DeferredAction::Selection { json: j, .. }
                    | DeferredAction::Seal { json: j, .. }
                    | DeferredAction::ApprovalDocuments { json: j } => j,
                };
                ensure!(
                    json.bytes <= limits.request_bytes,
                    "workbench deferred input byte limit"
                );
            }
            None => {
                core::bounded_json(&query, limits.request_bytes)?;
            }
        }
        let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            !self.closing.load(Ordering::Acquire) && !s.status.closed,
            "workbench is closing or closed"
        );
        ensure!(
            s.status.initialized,
            "workbench is not initialized; open explicitly after failure"
        );
        ensure!(
            matches!(
                s.status.phase,
                Phase::Complete | Phase::Failed | Phase::Canceled
            ),
            "workbench is busy; no operation backlog"
        );
        ensure!(
            expected_operation.is_none_or(|op| s.status.operation == op),
            "stale workbench operation"
        );
        ensure!(
            s.status.generation == expected,
            "stale workbench generation"
        );
        if s.status.review_token.is_some() {
            ensure!(
                action
                    .as_ref()
                    .is_some_and(PendingAction::allowed_during_review)
                    || query.as_ref().is_some_and(Query::review),
                "explicit ReleaseReview is required before ordinary Plan access"
            );
        }
        let operation = token();
        let control = limits.control(Arc::new(AtomicBool::new(false)))?;
        let generation = if action.as_ref().is_some_and(PendingAction::writes) {
            token()
        } else {
            s.status.generation.clone()
        };
        let message = if let Some(action) = action {
            Message::Action {
                operation: operation.clone(),
                generation: generation.clone(),
                action,
                control: control.clone(),
            }
        } else {
            Message::Read {
                operation: operation.clone(),
                generation: generation.clone(),
                query: query.context("missing workbench query")?,
                control: control.clone(),
            }
        };
        self.sender
            .try_send(message)
            .context("workbench operation admission failed")?;
        s.status.operation = operation.clone();
        s.status.generation = generation;
        s.status.phase = Phase::Running;
        s.status.result_token = None;
        s.status.result_bytes = U64(0);
        s.status.error = None;
        s.status.capture_pid = None;
        s.status.capture_staging = None;
        s.result = None;
        s.control = control;
        Ok(operation)
    }
    pub(crate) fn bridge_start(
        &self,
        generation: &str,
        operation: &str,
        action: Action,
    ) -> Result<String> {
        self.submit(
            generation,
            Some(PendingAction::Typed(action)),
            None,
            Some(operation),
        )
    }
    pub(crate) fn bridge_read(
        &self,
        generation: &str,
        operation: &str,
        query: Query,
    ) -> Result<String> {
        self.submit(generation, None, Some(query), Some(operation))
    }
    pub(crate) fn bridge_deferred(
        &self,
        generation: &str,
        operation: &str,
        action: DeferredAction,
    ) -> Result<String> {
        self.submit(
            generation,
            Some(PendingAction::Deferred(action)),
            None,
            Some(operation),
        )
    }
    pub(crate) fn bridge_cancel(&self, generation: &str, operation: &str) -> Result<Status> {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            s.status.generation == generation && s.status.operation == operation,
            "stale workbench cancellation"
        );
        s.control.cancel.store(true, Ordering::Release);
        drop(s);
        self.cancel(operation)
    }
    pub fn result(
        &self,
        expected_generation: &str,
        expected_operation: &str,
        expected_token: &str,
        offset: U64,
        limit: U64,
    ) -> Result<ResultPage> {
        let (cache, workbench, max) = {
            let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.result.clone().context("no retained workbench result")?,
                s.status.workbench.clone(),
                s.status.limits.page_bytes,
            )
        };
        ensure!(
            cache.generation == expected_generation
                && cache.operation == expected_operation
                && cache.token == expected_token,
            "stale or mismatched workbench result"
        );
        let start = usize::try_from(offset.0)?;
        let limit = usize::try_from(limit.0)?;
        ensure!(
            (1..=max).contains(&limit),
            "workbench result page byte limit"
        );
        ensure!(
            start <= cache.json.len() && cache.json.is_char_boundary(start),
            "invalid result byte cursor"
        );
        let mut end = start.saturating_add(limit).min(cache.json.len());
        while !cache.json.is_char_boundary(end) {
            end -= 1;
        }
        ensure!(
            end > start || start == cache.json.len(),
            "result chunk too small for UTF-8 scalar"
        );
        Ok(ResultPage {
            workbench,
            generation: cache.generation.clone(),
            operation: cache.operation.clone(),
            token: cache.token.clone(),
            offset,
            next: (end < cache.json.len()).then_some(U64(end as u64)),
            total_bytes: U64(cache.json.len() as u64),
            json_fragment: cache.json[start..end].to_owned(),
        })
    }
    pub fn cancel(&self, expected_operation: &str) -> Result<Status> {
        {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                s.status.operation == expected_operation,
                "cancel operation identity differs"
            );
            if matches!(
                s.status.phase,
                Phase::Opening | Phase::Running | Phase::CancelRequested
            ) {
                s.control.cancel.store(true, Ordering::Release);
                s.status.phase = Phase::CancelRequested;
            }
        }
        Ok(self.status())
    }
    pub fn request_close(&self) -> Status {
        self.closing.store(true, Ordering::Release);
        {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.control.cancel.store(true, Ordering::Release);
            if !s.status.closed {
                s.status.phase = Phase::Closing;
            }
        }
        let _ = self.sender.try_send(Message::Close);
        self.status()
    }
}
impl Workbench {
    pub fn poll_closed(&mut self) -> Result<bool> {
        if self.join.as_ref().is_some_and(|j| !j.is_finished()) {
            return Ok(false);
        }
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| anyhow::anyhow!("workbench owner failed during drain"))?;
        }
        Ok(true)
    }
}
impl Drop for Workbench {
    fn drop(&mut self) {
        self.request_close();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests;
