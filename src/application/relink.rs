//! Desktop relink review and owned, cancelable storage work.
use super::{BridgeError, Cancellation, ErrorCode, I64, Limits, U64, error, native};
use crate::{
    Catalog,
    catalog_edits::VariantKey,
    catalog_storage as core,
    storage_volume::{self, NativePath},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Instant,
};

type Result<T> = std::result::Result<T, BridgeError>;
#[cfg(test)]
mod tests;
const MAX_MOUNTS: usize = 128;
const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_CANDIDATES: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "scope",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Scope {
    Prefix {
        from: core::PathReference,
        destinations: Vec<NativePath>,
    },
    Asset {
        asset_id: String,
        destinations: Vec<NativePath>,
    },
    Volume {
        logical_volume: String,
        mount_token: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "target",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Override {
    Asset {
        asset_id: String,
        candidates: Vec<NativePath>,
    },
    Source {
        source_id: I64,
        candidates: Vec<NativePath>,
    },
    Prefix {
        from: core::PathReference,
        destinations: Vec<NativePath>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "command",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    Rules {
        plan: String,
        revision: I64,
        after: Option<RuleCursor>,
        limit: U64,
    },
    Begin {
        scope: Scope,
    },
    Prepare {
        plan: String,
        revision: I64,
        batch_rows: U64,
    },
    Revise {
        plan: String,
        revision: I64,
        changes: Vec<Override>,
    },
    Plan {
        plan: String,
    },
    Plans {
        after: String,
        limit: U64,
    },
    Items {
        plan: String,
        revision: I64,
        after: I64,
        limit: U64,
    },
    Sources {
        plan: String,
        revision: I64,
        sequence: I64,
        after: I64,
        limit: U64,
    },
    Confirm {
        plan: String,
        revision: I64,
        token: String,
        acknowledgement: String,
    },
    Apply {
        plan: String,
        revision: I64,
    },
    Undo {
        plan: String,
        revision: I64,
    },
    Mounts,
    Original {
        key: VariantKey,
    },
    Status {
        operation: Option<String>,
    },
    Cancel {
        operation: String,
    },
}
impl Request {
    pub(super) fn read_only(&self) -> bool {
        matches!(
            self,
            Self::Plan { .. }
                | Self::Rules { .. }
                | Self::Plans { .. }
                | Self::Items { .. }
                | Self::Sources { .. }
                | Self::Status { .. }
                | Self::Cancel { .. }
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleStage {
    Scope,
    Prefix,
    Asset,
    Source,
    ExcludedAsset,
    ExcludedSource,
    End,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuleCursor {
    pub plan: String,
    pub revision: I64,
    pub stage: RuleStage,
    pub position: I64,
    pub source: I64,
    pub entity: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "scope", content = "value", rename_all = "snake_case")]
pub enum SavedScope {
    Prefix {
        from: core::PathReference,
        destinations: Vec<NativePath>,
    },
    Asset {
        asset_id: String,
        destinations: Vec<NativePath>,
    },
    Volume {
        logical_volume: String,
        mount_path: NativePath,
        filesystem: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Rule {
    Scope(SavedScope),
    Override(Override),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleRow {
    pub rule: Rule,
    pub label: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub state: String,
    pub revision: I64,
    pub scanned_through: I64,
    pub high_water: I64,
    pub total: I64,
    pub matched: I64,
    pub excluded: I64,
    pub unresolved: I64,
    pub unresolved_sources: I64,
    pub unverified: I64,
    pub user_confirmed: I64,
    pub confirmation_token: Option<String>,
    pub summary_complete: bool,
}
impl From<core::RelinkPlan> for Plan {
    fn from(p: core::RelinkPlan) -> Self {
        Self {
            id: p.id,
            state: p.state,
            revision: I64(p.revision),
            scanned_through: I64(p.scanned_through),
            high_water: I64(p.high_water),
            total: I64(p.total),
            matched: I64(p.matched),
            excluded: I64(p.excluded),
            unresolved: I64(p.unresolved),
            unresolved_sources: I64(p.unresolved_sources),
            unverified: I64(p.unverified),
            user_confirmed: I64(p.user_confirmed),
            confirmation_token: p.confirmation_token,
            summary_complete: p.summary_complete,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub sequence: I64,
    pub asset_id: String,
    pub status: String,
    pub detail: String,
    pub identity_basis: String,
    pub original: core::PathReference,
    pub candidates: Vec<core::Candidate>,
    pub destination: Option<NativePath>,
}
impl From<core::RelinkItem> for Item {
    fn from(p: core::RelinkItem) -> Self {
        Self {
            sequence: I64(p.sequence),
            asset_id: p.asset_id,
            status: p.status,
            detail: p.detail,
            identity_basis: p.identity_basis,
            original: p.original,
            candidates: p.candidates,
            destination: p.destination,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub source_id: I64,
    pub status: String,
    pub detail: String,
    pub original: core::PathReference,
    pub candidates: Vec<core::Candidate>,
    pub destination: Option<NativePath>,
}
impl From<core::RelinkSource> for Source {
    fn from(p: core::RelinkSource) -> Self {
        Self {
            source_id: I64(p.source_id),
            status: p.status,
            detail: p.detail,
            original: p.original,
            candidates: p.candidates,
            destination: p.destination,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mount {
    pub token: String,
    pub path: NativePath,
    pub filesystem: String,
    pub identity_available: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mounts {
    pub rows: Vec<Mount>,
    pub complete: bool,
    pub issues: Vec<storage_volume::VolumeIssue>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Original {
    pub key: VariantKey,
    pub status: core::StorageStatus,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    Observing,
    Draining,
    Applying,
    Undoing,
    CancelRequested,
    Complete,
    Canceled,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Prepare,
    Confirm,
    Revise,
    Apply,
    Undo,
    Mounts,
    Original,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Outcome {
    Plan(Plan),
    Mounts(Mounts),
    Original(Original),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub action: Action,
    pub phase: Phase,
    pub plan: Option<Plan>,
    pub progress: I64,
    pub boundary: Option<String>,
    pub write_hold: bool,
    pub result: Option<Box<Outcome>>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Response {
    Rules {
        rows: Vec<RuleRow>,
        next: Option<RuleCursor>,
        scanned: U64,
    },
    Plan(Plan),
    Plans {
        rows: Vec<Plan>,
        next: Option<String>,
    },
    Items {
        rows: Vec<Item>,
        next: Option<I64>,
    },
    Sources {
        rows: Vec<Source>,
        next: Option<I64>,
    },
    Operation(Option<Operation>),
}

#[derive(Default)]
pub(super) struct Control {
    pub status: Option<Operation>,
    cancel: Option<Cancellation>,
}
impl Control {
    pub fn read(&mut self, operation: Option<&str>, cancel: bool) -> Result<Option<Operation>> {
        if let Some(id) = operation
            && self.status.as_ref().map(|s| s.id.as_str()) != Some(id)
        {
            return Err(error(ErrorCode::StaleSession, "relink operation changed"));
        }
        if cancel && let Some(c) = &self.cancel {
            c.cancel();
            if let Some(s) = &mut self.status {
                s.phase = Phase::CancelRequested;
            }
        }
        Ok(self.status.clone())
    }
    pub fn request_cancel(&self) {
        if let Some(c) = &self.cancel {
            c.cancel();
        }
    }
}

fn bounded<T: Serialize>(value: &T, bytes: usize) -> Result<()> {
    if serde_json::to_vec(value)
        .map_err(|e| native(e.into()))?
        .len()
        > bytes
    {
        return Err(error(
            ErrorCode::ResourceLimit,
            "relink response exceeds byte allowance; reduce page or candidate size",
        ));
    }
    Ok(())
}
fn id(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 {
        return Err(error(ErrorCode::InvalidRequest, "relink identifier length"));
    }
    Ok(())
}
fn paths(values: &[NativePath]) -> Result<()> {
    if values.len() > MAX_CANDIDATES {
        return Err(error(ErrorCode::ResourceLimit, "at most 32 candidates"));
    }
    for value in values {
        bounded(value, MAX_PATH_BYTES)?;
        if !value.to_path().map_err(|e| native(e.into()))?.is_absolute() {
            return Err(error(
                ErrorCode::InvalidRequest,
                "candidate path must be absolute",
            ));
        }
    }
    Ok(())
}
fn page(limit: U64, limits: &Limits) -> Result<usize> {
    if limit.0 == 0 || limit.0 > u64::from(limits.page_rows) {
        return Err(error(
            ErrorCode::InvalidRequest,
            "relink page row allowance",
        ));
    }
    Ok(limit.0 as usize)
}
fn reviewed(catalog: &Catalog, plan: &str, revision: I64) -> Result<core::RelinkPlan> {
    id(plan)?;
    let p = catalog.relink_plan(plan).map_err(native)?;
    if revision.0 < 0 || p.revision != revision.0 {
        return Err(error(
            ErrorCode::StaleSession,
            "relink review changed; reload plan",
        ));
    }
    Ok(p)
}

enum Output {
    Prepared(core::PreparedRelinkBatch),
    Mounts(storage_volume::MountSnapshot),
    Original {
        value: Original,
        identity: Box<crate::catalog_edits::EditRenderIdentity>,
    },
    Committed(core::RelinkPlan),
}
struct Worker {
    receiver: mpsc::Receiver<std::result::Result<Output, String>>,
    join: thread::JoinHandle<()>,
}
enum CommitKind {
    Apply,
    Undo,
    Finalize,
    Confirm {
        token: String,
        acknowledgement: String,
    },
    Revise(Vec<core::RelinkOverride>),
}
struct Commit {
    handle: core::RelinkWorkerHandle,
    plan: String,
    revision: I64,
    kind: CommitKind,
}
#[derive(Default)]
pub(super) struct Coordinator {
    sql_session: Option<Arc<crate::catalog_session::CatalogSessionAuthority>>,
    cancel: Option<Cancellation>,
    worker: Option<Worker>,
    prepare: Option<(String, usize)>,
    commit: Option<Commit>,
    pause: Option<crate::preview::NativeLaunchPause>,
    mounts: HashMap<String, (storage_volume::MountedVolume, Instant)>,
    finalize: Option<(String, I64)>,
    #[cfg(test)]
    pub checkpoint: Option<crate::import_preparation::Checkpoint>,
}
impl Coordinator {
    pub fn write_hold(&self) -> bool {
        self.pause.is_some()
    }
    pub fn busy(&self) -> bool {
        self.worker.is_some()
            || self.prepare.is_some()
            || self.commit.is_some()
            || self.finalize.is_some()
            || self.write_hold()
    }
    fn start(
        &mut self,
        control: &Arc<Mutex<Control>>,
        action: Action,
        phase: Phase,
        plan: Option<Plan>,
        hold: bool,
    ) -> Result<Cancellation> {
        if self.busy() {
            return Err(error(
                ErrorCode::Busy,
                "relink operation is still running or draining",
            ));
        }
        let cancel = Cancellation::default();
        self.cancel = Some(cancel.clone());
        *control.lock().unwrap() = Control {
            status: Some(Operation {
                id: uuid::Uuid::new_v4().to_string(),
                action,
                phase,
                plan,
                progress: I64(0),
                boundary: None,
                write_hold: hold,
                result: None,
                error: None,
            }),
            cancel: Some(cancel.clone()),
        };
        Ok(cancel)
    }
    fn spawn(
        &mut self,
        task: impl FnOnce() -> std::result::Result<Output, String> + Send + 'static,
    ) -> Result<()> {
        let (tx, receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("catalog-relink".into())
            .spawn(move || {
                let _ = tx.send(task());
            })
            .map_err(|e| native(e.into()))?;
        self.worker = Some(Worker { receiver, join });
        Ok(())
    }
    fn finish(
        &mut self,
        control: &Arc<Mutex<Control>>,
        outcome: std::result::Result<Option<Outcome>, String>,
    ) {
        let mut c = control.lock().unwrap();
        let canceled = c.cancel.as_ref().is_some_and(Cancellation::is_canceled);
        if let Some(s) = &mut c.status {
            match outcome {
                Ok(Some(value)) => {
                    if let Outcome::Plan(p) = &value {
                        s.plan = Some(p.clone());
                    }
                    s.result = Some(Box::new(value));
                    s.phase = Phase::Complete;
                }
                Ok(None) => {
                    s.phase = if canceled {
                        Phase::Canceled
                    } else {
                        Phase::Complete
                    }
                }
                Err(message) => {
                    s.phase = if canceled {
                        Phase::Canceled
                    } else {
                        Phase::Failed
                    };
                    s.error = Some(message.chars().take(2048).collect());
                }
            }
            s.write_hold = false;
        }
        c.cancel = None;
        self.cancel = None;
        self.prepare = None;
        self.commit = None;
        self.pause = None;
        self.finalize = None;
    }
    pub fn shutdown(&mut self, control: &Arc<Mutex<Control>>) {
        control.lock().unwrap().request_cancel();
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        if let Some(worker) = self.worker.take() {
            let healthy = worker.join.join().is_ok();
            if let Some(session) = &self.sql_session {
                let _ = session.joined(crate::catalog_session::SqlRole::Relink, healthy);
            }
        }
        self.prepare = None;
        self.commit = None;
        self.pause = None;
        self.finalize = None;
        self.cancel = None;
    }
    pub fn execute(
        &mut self,
        catalog: &mut Catalog,
        request: Request,
        limits: &Limits,
        control: &Arc<Mutex<Control>>,
        jobs_held: bool,
    ) -> Result<Response> {
        self.sql_session = Some(catalog.session.clone());
        if request.read_only() {
            let tx = catalog
                .db
                .unchecked_transaction()
                .map_err(|e| native(e.into()))?;
            let result = read_request(catalog, request, limits, control);
            tx.commit().map_err(|e| native(e.into()))?;
            return result;
        }
        if self.busy() && !request.read_only() {
            return Err(error(
                ErrorCode::Busy,
                "relink operation is still running or draining",
            ));
        }
        let response = match request {
            Request::Status { .. }
            | Request::Rules { .. }
            | Request::Cancel { .. }
            | Request::Plan { .. }
            | Request::Plans { .. }
            | Request::Items { .. }
            | Request::Sources { .. } => unreachable!(),
            Request::Begin { scope } => {
                let scope = match scope {
                    Scope::Prefix { from, destinations } => {
                        bounded(&from, MAX_PATH_BYTES)?;
                        paths(&destinations)?;
                        core::RelinkScope::Prefix { from, destinations }
                    }
                    Scope::Asset {
                        asset_id,
                        destinations,
                    } => {
                        id(&asset_id)?;
                        paths(&destinations)?;
                        core::RelinkScope::Asset {
                            asset_id,
                            destinations,
                        }
                    }
                    Scope::Volume {
                        logical_volume,
                        mount_token,
                    } => {
                        id(&logical_volume)?;
                        id(&mount_token)?;
                        let (mount, at) = self.mounts.get(&mount_token).ok_or_else(|| {
                            error(
                                ErrorCode::StaleSession,
                                "mount token unavailable; observe mounts again",
                            )
                        })?;
                        if at.elapsed().as_secs() > limits.ttl_seconds {
                            return Err(error(
                                ErrorCode::StaleSession,
                                "mount observation expired; observe again",
                            ));
                        }
                        core::RelinkScope::Volume {
                            logical_volume,
                            mount: mount.clone(),
                        }
                    }
                };
                Response::Plan(catalog.begin_relink_review(scope).map_err(native)?.into())
            }
            Request::Prepare {
                plan,
                revision,
                batch_rows,
            } => {
                if jobs_held {
                    return Err(error(
                        ErrorCode::Busy,
                        "restored jobs remain held; review and explicitly resume first",
                    ));
                }
                let p = reviewed(catalog, &plan, revision)?;
                let n = page(batch_rows, limits)?;
                let checking = p.state == "checking";
                let snapshot = if checking {
                    None
                } else {
                    Some(catalog.relink_preparation(&plan, n).map_err(native)?)
                };
                let cancel = self.start(
                    control,
                    Action::Prepare,
                    Phase::Preparing,
                    Some(p.into()),
                    false,
                )?;
                if checking {
                    self.finalize = Some((plan, revision));
                    return Ok(Response::Operation(control.lock().unwrap().status.clone()));
                }
                let snapshot = snapshot.unwrap();
                #[cfg(test)]
                let checkpoint = self.checkpoint.clone();
                self.prepare = Some((plan, n));
                if let Err(e) = self.spawn(move || {
                    #[cfg(test)]
                    if let Some(c) = checkpoint {
                        c("relink_preparation", &cancel.0);
                    }
                    snapshot
                        .prepare(&cancel.0)
                        .map(Output::Prepared)
                        .map_err(|e| format!("{e:#}"))
                }) {
                    self.finish(control, Err(e.message.clone()));
                    return Err(e);
                }
                Response::Operation(control.lock().unwrap().status.clone())
            }
            Request::Mounts => {
                let observer = crate::catalog_session::storage::Observer(catalog.session.clone());
                let cancel = self.start(control, Action::Mounts, Phase::Observing, None, false)?;
                if let Err(e) = self.spawn(move || {
                    if cancel.is_canceled() {
                        return Err("mount observation canceled".into());
                    }
                    observer
                        .mounts(&cancel.0)
                        .map(Output::Mounts)
                        .map_err(|e| e.to_string())
                }) {
                    self.finish(control, Err(e.message.clone()));
                    return Err(e);
                }
                Response::Operation(control.lock().unwrap().status.clone())
            }
            Request::Original { key } => {
                catalog.image(&key).map_err(native)?;
                let handle = catalog.relink_worker_handle().map_err(native)?;
                let cancel =
                    self.start(control, Action::Original, Phase::Observing, None, false)?;
                #[cfg(test)]
                let checkpoint = self.checkpoint.clone();
                if let Err(e) = self.spawn(move || {
                    let task = || -> anyhow::Result<Output> {
                        anyhow::ensure!(!cancel.is_canceled(), "original observation canceled");
                        let catalog = handle.open()?;
                        catalog.image(&key)?;
                        let identity = catalog.edit_render_identity(&key)?;
                        #[cfg(test)] if let Some(c) = checkpoint { c("relink_original", &cancel.0); }
                        let observer = crate::catalog_session::storage::Observer(catalog.session.clone());
                        let mounts = observer.mounts(&cancel.0)?;
                        let mut status = catalog.storage_status(&key.asset_id, &mounts)?;
                        if status.state == "unregistered" && let core::PathReference::Native(path) = &status.current {
                            let observation = observer.locate(&path.to_path()?, &cancel.0)?;
                            status.state = match observation.state {
                                storage_volume::LocationState::Available => "available_unverified",
                                storage_volume::LocationState::MissingPath => "missing",
                                _ => "unavailable",
                            }.into();
                            status.detail = "Current original path observed; no persistent volume binding or content verification is implied".into();
                        }
                        anyhow::ensure!(!cancel.is_canceled(), "original observation canceled");
                        anyhow::ensure!(super::identity_equal(&identity, &catalog.edit_render_identity(&key)?), "source or variant changed during original observation; observe again");
                        Ok(Output::Original {value:Original{key,status},identity:Box::new(identity)})
                    };
                    task().map_err(|e| format!("{e:#}"))
                }) {
                    self.finish(control, Err(e.message.clone())); return Err(e);
                }
                Response::Operation(control.lock().unwrap().status.clone())
            }
            Request::Apply { .. }
            | Request::Undo { .. }
            | Request::Confirm { .. }
            | Request::Revise { .. } => {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "commit requires actor drain admission",
                ));
            }
        };
        bounded(&response, limits.reply_bytes)?;
        Ok(response)
    }
    pub fn needs_finalization(&self) -> bool {
        self.finalize.is_some()
    }
    pub fn admit_finalize(
        &mut self,
        catalog: &Catalog,
        control: &Arc<Mutex<Control>>,
        pause: crate::preview::NativeLaunchPause,
    ) -> Result<()> {
        let (plan, revision) = self.finalize.take().unwrap();
        let handle = catalog.relink_worker_handle().map_err(native)?;
        self.commit = Some(Commit {
            handle,
            plan,
            revision,
            kind: CommitKind::Finalize,
        });
        self.pause = Some(pause);
        if let Some(s) = &mut control.lock().unwrap().status {
            s.phase = Phase::Draining;
            s.write_hold = true;
        }
        Ok(())
    }
    pub fn admit_commit(
        &mut self,
        catalog: &Catalog,
        request: Request,
        control: &Arc<Mutex<Control>>,
        pause: crate::preview::NativeLaunchPause,
    ) -> Result<Response> {
        let (plan, revision, kind) = match request {
            Request::Apply { plan, revision } => (plan, revision, CommitKind::Apply),
            Request::Undo { plan, revision } => (plan, revision, CommitKind::Undo),
            Request::Confirm {
                plan,
                revision,
                token,
                acknowledgement,
            } => {
                id(&token)?;
                (
                    plan,
                    revision,
                    CommitKind::Confirm {
                        token,
                        acknowledgement,
                    },
                )
            }
            Request::Revise {
                plan,
                revision,
                changes,
            } => {
                if changes.len() > 100 {
                    return Err(error(
                        ErrorCode::ResourceLimit,
                        "at most 100 relink overrides per revision",
                    ));
                }
                let changes = changes
                    .into_iter()
                    .map(|c| {
                        Ok(match c {
                            Override::Asset {
                                asset_id,
                                candidates,
                            } => {
                                id(&asset_id)?;
                                paths(&candidates)?;
                                core::RelinkOverride::Asset {
                                    asset_id,
                                    candidates,
                                }
                            }
                            Override::Source {
                                source_id,
                                candidates,
                            } => {
                                if source_id.0 <= 0 {
                                    return Err(error(
                                        ErrorCode::InvalidRequest,
                                        "source identity range",
                                    ));
                                }
                                paths(&candidates)?;
                                core::RelinkOverride::Source {
                                    source_id: source_id.0,
                                    candidates,
                                }
                            }
                            Override::Prefix { from, destinations } => {
                                bounded(&from, MAX_PATH_BYTES)?;
                                paths(&destinations)?;
                                core::RelinkOverride::Prefix { from, destinations }
                            }
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                (plan, revision, CommitKind::Revise(changes))
            }
            _ => {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "not a relink commit operation",
                ));
            }
        };
        let p = reviewed(catalog, &plan, revision)?;
        if !p.summary_complete && !matches!(&kind, CommitKind::Revise(_) | CommitKind::Undo) {
            return Err(error(
                ErrorCode::Busy,
                "legacy plan summary is not complete; revise and prepare a new review",
            ));
        }
        let handle = catalog.relink_worker_handle().map_err(native)?;
        let action = match &kind {
            CommitKind::Apply => Action::Apply,
            CommitKind::Undo => Action::Undo,
            CommitKind::Confirm { .. } => Action::Confirm,
            CommitKind::Revise(_) => Action::Revise,
            CommitKind::Finalize => Action::Prepare,
        };
        self.start(control, action, Phase::Draining, Some(p.into()), true)?;
        self.commit = Some(Commit {
            handle,
            plan,
            revision,
            kind,
        });
        self.pause = Some(pause);
        Ok(Response::Operation(control.lock().unwrap().status.clone()))
    }
    /// Called only after the actor has drained every initial reader/native child.
    pub fn start_commit(&mut self, control: &Arc<Mutex<Control>>) -> Result<()> {
        let Some(commit) = self.commit.take() else {
            return Ok(());
        };
        let cancel = control.lock().unwrap().cancel.clone().unwrap();
        if cancel.is_canceled() {
            self.finish(control, Ok(None));
            return Ok(());
        }
        if let Some(s) = &mut control.lock().unwrap().status {
            s.phase = if matches!(commit.kind, CommitKind::Undo) {
                Phase::Undoing
            } else {
                Phase::Applying
            };
        }
        let progress = Arc::clone(control);
        #[cfg(test)]
        let checkpoint = self.checkpoint.clone();
        self.spawn(move || {
            let task = || -> anyhow::Result<Output> {
                #[cfg(test)]
                if let Some(c) = &checkpoint {
                    c("relink_commit", &cancel.0);
                }
                let mut catalog = commit.handle.open()?;
                anyhow::ensure!(
                    catalog.relink_plan(&commit.plan)?.revision == commit.revision.0,
                    "relink review changed before commit"
                );
                #[cfg(test)]
                let boundary_checkpoint = checkpoint.clone();
                #[cfg(test)]
                let boundary_cancel = cancel.clone();
                let progress = move |boundary: core::RelinkBoundary| {
                    let (name, count) = match boundary {
                        core::RelinkBoundary::Verified(n) => ("verified", Some(n)),
                        core::RelinkBoundary::Updated(n) => ("updated", Some(n)),
                        core::RelinkBoundary::BeforeMutation => ("before_mutation", None),
                        core::RelinkBoundary::BeforeCommit => ("before_commit", None),
                    };
                    if let Some(s) = &mut progress.lock().unwrap().status {
                        s.boundary = Some(name.into());
                        if let Some(n) = count {
                            s.progress = I64(n);
                        }
                    }
                    #[cfg(test)]
                    if matches!(boundary, core::RelinkBoundary::BeforeMutation)
                        && let Some(c) = &boundary_checkpoint
                    {
                        c("relink_before_mutation", &boundary_cancel.0);
                    }
                };
                let p = match commit.kind {
                    CommitKind::Undo => {
                        catalog.undo_relink_cancellable(&commit.plan, &cancel.0, progress)?
                    }
                    CommitKind::Apply => catalog.apply_relink_cancellable(
                        &commit.plan,
                        commit.revision.0,
                        &cancel.0,
                        progress,
                    )?,
                    CommitKind::Finalize => catalog.finalize_relink_review_cancellable(
                        &commit.plan,
                        commit.revision.0,
                        &cancel.0,
                    )?,
                    CommitKind::Confirm {
                        token,
                        acknowledgement,
                    } => catalog.confirm_relink_associations_cancellable(
                        &commit.plan,
                        commit.revision.0,
                        &token,
                        &acknowledgement,
                        &cancel.0,
                    )?,
                    CommitKind::Revise(changes) => catalog.revise_relink_cancellable(
                        &commit.plan,
                        commit.revision.0,
                        changes,
                        &cancel.0,
                    )?,
                };
                #[cfg(test)]
                if let Some(c) = &checkpoint {
                    c("relink_committed", &cancel.0);
                }
                Ok(Output::Committed(p))
            };
            task().map_err(|e| format!("{e:#}"))
        })
    }
    pub fn committing(&self) -> bool {
        self.write_hold() && self.commit.is_none() && self.worker.is_some()
    }
    pub fn advance(
        &mut self,
        catalog: &mut Catalog,
        limits: &Limits,
        control: &Arc<Mutex<Control>>,
        foreground: bool,
    ) {
        if foreground && self.prepare.is_some() {
            return;
        }
        if let Some(worker) = &self.worker {
            if !worker.join.is_finished() {
                return;
            }
            let worker = self.worker.take().unwrap();
            let result = worker
                .receiver
                .try_recv()
                .unwrap_or_else(|_| Err("relink worker ended without a result".into()));
            let healthy = worker.join.join().is_ok();
            if let Some(session) = &self.sql_session {
                let _ = session.joined(crate::catalog_session::SqlRole::Relink, healthy);
            }
            // A successfully committed transaction wins a concurrent cancellation.
            if let Ok(Output::Committed(p)) = result {
                self.finish(control, Ok(Some(Outcome::Plan(p.into()))));
                return;
            }
            if control
                .lock()
                .unwrap()
                .cancel
                .as_ref()
                .is_some_and(Cancellation::is_canceled)
            {
                self.finish(control, Ok(None));
                return;
            }
            let result = (|| -> Result<Option<Outcome>> {
                match result.map_err(|e| error(ErrorCode::Native, e))? {
                    Output::Prepared(batch) => {
                        let p = catalog.publish_relink_preparation(batch).map_err(native)?;
                        let ready = p.state == "ready";
                        let checking = p.state == "checking";
                        let p = Plan::from(p);
                        if checking {
                            self.prepare = None;
                            self.finalize = Some((p.id.clone(), p.revision));
                        }
                        if let Some(s) = &mut control.lock().unwrap().status {
                            s.plan = Some(p.clone());
                            s.progress = p.scanned_through;
                        }
                        if ready {
                            Ok(Some(Outcome::Plan(p)))
                        } else {
                            Ok(None)
                        }
                    }
                    Output::Mounts(snapshot) => {
                        if snapshot.mounts.len() > MAX_MOUNTS {
                            return Err(error(
                                ErrorCode::ResourceLimit,
                                "mount snapshot exceeds 128 entries; use explicit file or prefix relink",
                            ));
                        }
                        bounded(&snapshot, limits.page_bytes)?;
                        let mut tokens = HashMap::new();
                        let mut rows = Vec::new();
                        for mount in &snapshot.mounts {
                            let token = uuid::Uuid::new_v4().to_string();
                            let usable = snapshot.complete
                                && mount.persistent_identity.is_some()
                                && snapshot
                                    .mounts
                                    .iter()
                                    .filter(|m| m.persistent_identity == mount.persistent_identity)
                                    .count()
                                    == 1
                                && mount.issues.is_empty();
                            rows.push(Mount {
                                token: token.clone(),
                                path: mount.mount_path.clone(),
                                filesystem: mount.filesystem.clone(),
                                identity_available: usable,
                            });
                            if usable {
                                tokens.insert(token, (mount.clone(), Instant::now()));
                            }
                        }
                        let out = Outcome::Mounts(Mounts {
                            rows,
                            complete: snapshot.complete,
                            issues: snapshot.issues,
                        });
                        bounded(&out, limits.page_bytes)?;
                        self.mounts = tokens;
                        Ok(Some(out))
                    }
                    Output::Original { value, identity } => {
                        if !super::identity_equal(
                            &identity,
                            &catalog.edit_render_identity(&value.key).map_err(native)?,
                        ) {
                            return Err(error(
                                ErrorCode::StaleSession,
                                "source or variant changed during original observation; observe again",
                            ));
                        }
                        bounded(&value, limits.page_bytes)?;
                        Ok(Some(Outcome::Original(value)))
                    }
                    Output::Committed(_) => unreachable!(),
                }
            })();
            match result {
                Ok(None) => {}
                Ok(value) => {
                    self.finish(control, Ok(value));
                    return;
                }
                Err(e) => {
                    self.finish(control, Err(e.message));
                    return;
                }
            }
        }
        if self.worker.is_none() && self.prepare.is_some() && !foreground {
            let cancel = control.lock().unwrap().cancel.clone().unwrap();
            if cancel.is_canceled() {
                self.finish(control, Ok(None));
                return;
            }
            let (plan, n) = self.prepare.as_ref().unwrap();
            match catalog
                .relink_preparation(plan, *n)
                .map_err(native)
                .and_then(|snapshot| {
                    self.spawn(move || {
                        snapshot
                            .prepare(&cancel.0)
                            .map(Output::Prepared)
                            .map_err(|e| format!("{e:#}"))
                    })
                }) {
                Ok(()) => {}
                Err(e) => self.finish(control, Err(e.message)),
            }
        }
    }
    pub fn fail(&mut self, control: &Arc<Mutex<Control>>, message: String) {
        self.finish(control, Err(message));
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        if let Some(worker) = self.worker.take() {
            let healthy = worker.join.join().is_ok();
            if let Some(session) = &self.sql_session {
                let _ = session.joined(crate::catalog_session::SqlRole::Relink, healthy);
            }
        }
    }
}

fn read_request(
    catalog: &Catalog,
    request: Request,
    limits: &Limits,
    control: &Arc<Mutex<Control>>,
) -> Result<Response> {
    let response = match request {
        Request::Rules {
            plan,
            revision,
            after,
            limit,
        } => rules(catalog, &plan, revision, after, limit, limits)?,
        Request::Status { operation } => {
            Response::Operation(control.lock().unwrap().read(operation.as_deref(), false)?)
        }
        Request::Cancel { operation } => {
            Response::Operation(control.lock().unwrap().read(Some(&operation), true)?)
        }
        Request::Plan { plan } => {
            id(&plan)?;
            Response::Plan(catalog.relink_plan(&plan).map_err(native)?.into())
        }
        Request::Plans { mut after, limit } => {
            if after.len() > 256 {
                return Err(error(ErrorCode::InvalidRequest, "plan cursor length"));
            }
            let n = page(limit, limits)?;
            let mut rows = Vec::new();
            let mut next = None;
            for _ in 0..=n {
                let Some(p) = catalog.relink_plans(&after, 1).map_err(native)?.pop() else {
                    break;
                };
                let p = Plan::from(p);
                if rows.len() == n {
                    next = Some(after);
                    break;
                }
                let previous = after.clone();
                after = p.id.clone();
                rows.push(p);
                if bounded(&rows, limits.page_bytes).is_err() {
                    rows.pop();
                    if rows.is_empty() {
                        return Err(error(
                            ErrorCode::ResourceLimit,
                            "single relink plan exceeds page byte allowance",
                        ));
                    }
                    next = Some(previous);
                    break;
                }
            }
            Response::Plans { rows, next }
        }
        Request::Items {
            plan,
            revision,
            mut after,
            limit,
        } => {
            reviewed(catalog, &plan, revision)?;
            if after.0 < 0 {
                return Err(error(ErrorCode::InvalidRequest, "negative item cursor"));
            }
            let n = page(limit, limits)?;
            let mut rows = Vec::new();
            let mut next = None;
            for _ in 0..=n {
                let Some(p) = catalog
                    .relink_items(&plan, after.0, 1)
                    .map_err(native)?
                    .pop()
                else {
                    break;
                };
                if rows.len() == n {
                    next = Some(after);
                    break;
                }
                let previous = after;
                after = I64(p.sequence);
                rows.push(Item::from(p));
                if bounded(&rows, limits.page_bytes).is_err() {
                    rows.pop();
                    if rows.is_empty() {
                        return Err(error(
                            ErrorCode::ResourceLimit,
                            "single relink item exceeds page byte allowance",
                        ));
                    }
                    next = Some(previous);
                    break;
                }
            }
            Response::Items { rows, next }
        }
        Request::Sources {
            plan,
            revision,
            sequence,
            mut after,
            limit,
        } => {
            reviewed(catalog, &plan, revision)?;
            if after.0 < 0 || sequence.0 <= 0 {
                return Err(error(ErrorCode::InvalidRequest, "source cursor range"));
            }
            let n = page(limit, limits)?;
            let mut rows = Vec::new();
            let mut next = None;
            for _ in 0..=n {
                let Some(p) = catalog
                    .relink_sources(&plan, sequence.0, after.0, 1)
                    .map_err(native)?
                    .pop()
                else {
                    break;
                };
                if rows.len() == n {
                    next = Some(after);
                    break;
                }
                let previous = after;
                after = I64(p.source_id);
                rows.push(Source::from(p));
                if bounded(&rows, limits.page_bytes).is_err() {
                    rows.pop();
                    if rows.is_empty() {
                        return Err(error(
                            ErrorCode::ResourceLimit,
                            "single relink source exceeds page byte allowance",
                        ));
                    }
                    next = Some(previous);
                    break;
                }
            }
            Response::Sources { rows, next }
        }

        _ => return Err(error(ErrorCode::InvalidRequest, "not a read request")),
    };
    bounded(&response, limits.reply_bytes)?;
    Ok(response)
}

fn rules(
    catalog: &Catalog,
    plan: &str,
    revision: I64,
    after: Option<RuleCursor>,
    limit: U64,
    limits: &Limits,
) -> Result<Response> {
    use rusqlite::{OptionalExtension, params};
    reviewed(catalog, plan, revision)?;
    let count = page(limit, limits)?;
    let mut cursor = after.unwrap_or(RuleCursor {
        plan: plan.into(),
        revision,
        stage: RuleStage::Scope,
        position: I64(0),
        source: I64(0),
        entity: String::new(),
    });
    if cursor.plan != plan || cursor.revision != revision {
        return Err(error(
            ErrorCode::StaleSession,
            "relink rule cursor belongs to a different review",
        ));
    }
    if cursor.position.0 < -1 || cursor.source.0 < 0 || cursor.entity.len() > 256 {
        return Err(error(ErrorCode::InvalidRequest, "rule cursor bounds"));
    }
    let lengths: (i64, i64) = catalog
        .db
        .query_row(
            "SELECT length(CAST(request AS BLOB)),length(CAST(rules AS BLOB)) FROM storage_plans WHERE id=?",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| native(e.into()))?;
    if lengths.0 > 1024 * 1024 || lengths.1 > 1024 * 1024 {
        return Err(error(
            ErrorCode::ResourceLimit,
            "saved relink rules exceed 1 MiB; bounded review cannot admit them",
        ));
    }
    let mut rows = Vec::new();
    let mut scanned = 0;
    while rows.len() < count && scanned < limits.scan_rows && cursor.stage != RuleStage::End {
        let before = cursor.clone();
        scanned += 1;
        let row = match cursor.stage {
            RuleStage::Scope => {
                let encoded:Option<String>=catalog.db.query_row("SELECT CASE WHEN length(CAST(request AS BLOB))<=?2 THEN request END FROM storage_plans WHERE id=?1",params![plan,limits.page_bytes as i64],|r|r.get(0)).map_err(|e|native(e.into()))?;
                let encoded = encoded.ok_or_else(|| {
                    error(
                        ErrorCode::ResourceLimit,
                        "saved base scope exceeds page byte allowance",
                    )
                })?;
                let scope: core::RelinkScope =
                    serde_json::from_str(&encoded).map_err(|e| native(e.into()))?;
                let scope = match scope {
                    core::RelinkScope::Prefix { from, destinations } => {
                        SavedScope::Prefix { from, destinations }
                    }
                    core::RelinkScope::Asset {
                        asset_id,
                        destinations,
                    } => SavedScope::Asset {
                        asset_id,
                        destinations,
                    },
                    core::RelinkScope::Volume {
                        logical_volume,
                        mount,
                    } => SavedScope::Volume {
                        logical_volume,
                        mount_path: mount.mount_path,
                        filesystem: mount.filesystem,
                    },
                };
                cursor.stage = RuleStage::Prefix;
                cursor.position = I64(-1);
                Some(Rule::Scope(scope))
            }
            RuleStage::Prefix => {
                let row:Option<(i64,Option<String>)>=catalog.db.query_row("SELECT CAST(j.key AS INTEGER),CASE WHEN length(CAST(j.value AS BLOB))<=?3 THEN j.value END FROM storage_plans p,json_each(p.rules) j WHERE p.id=?1 AND CAST(j.key AS INTEGER)>?2 ORDER BY CAST(j.key AS INTEGER) LIMIT 1",params![plan,cursor.position.0,limits.page_bytes as i64],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|native(e.into()))?;
                if let Some((position, encoded)) = row {
                    cursor.position = I64(position);
                    let rule: core::RelinkOverride =
                        serde_json::from_str(&encoded.ok_or_else(|| {
                            error(
                                ErrorCode::ResourceLimit,
                                "saved prefix rule exceeds page byte allowance",
                            )
                        })?)
                        .map_err(|e| native(e.into()))?;
                    let core::RelinkOverride::Prefix { from, destinations } = rule else {
                        return Err(error(
                            ErrorCode::Native,
                            "unexpected saved prefix rule kind",
                        ));
                    };
                    Some(Rule::Override(Override::Prefix { from, destinations }))
                } else {
                    cursor.stage = RuleStage::Asset;
                    cursor.position = I64(0);
                    cursor.entity.clear();
                    None
                }
            }
            RuleStage::Asset | RuleStage::Source => {
                let asset = cursor.stage == RuleStage::Asset;
                let kind = if asset { "asset" } else { "source" };
                let row:Option<(String,Option<String>)>=catalog.db.query_row("SELECT entity,CASE WHEN length(CAST(candidates AS BLOB))<=?4 THEN candidates END FROM storage_exceptions WHERE plan=?1 AND kind=?2 AND entity>?3 ORDER BY entity LIMIT 1",params![plan,kind,cursor.entity,limits.page_bytes as i64],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|native(e.into()))?;
                if let Some((entity, encoded)) = row {
                    id(&entity)?;
                    cursor.entity = entity.clone();
                    let candidates: Vec<NativePath> =
                        serde_json::from_str(&encoded.ok_or_else(|| {
                            error(
                                ErrorCode::ResourceLimit,
                                "saved candidate rule exceeds page byte allowance",
                            )
                        })?)
                        .map_err(|e| native(e.into()))?;
                    if candidates.len() > MAX_CANDIDATES {
                        return Err(error(
                            ErrorCode::ResourceLimit,
                            "saved candidate rule exceeds 32 paths",
                        ));
                    }
                    Some(Rule::Override(if asset {
                        Override::Asset {
                            asset_id: entity,
                            candidates,
                        }
                    } else {
                        Override::Source {
                            source_id: I64(entity.parse().map_err(|_| {
                                error(ErrorCode::Native, "invalid stored source identity")
                            })?),
                            candidates,
                        }
                    }))
                } else {
                    cursor.stage = if asset {
                        RuleStage::Source
                    } else {
                        RuleStage::ExcludedAsset
                    };
                    cursor.entity.clear();
                    cursor.position = I64(0);
                    None
                }
            }
            RuleStage::ExcludedAsset => {
                // Charge every indexed candidate, including nonexcluded rows.
                // Filtering in SQL would scan arbitrarily far for a rare exclusion.
                let row:Option<(i64,String,bool,bool)>=catalog.db.query_row("SELECT i.sequence,i.asset_id,i.status='excluded',EXISTS(SELECT 1 FROM storage_exceptions e WHERE e.plan=i.plan AND e.kind='asset' AND e.entity=i.asset_id) FROM storage_items i WHERE i.plan=?1 AND i.sequence>?2 ORDER BY i.sequence LIMIT 1",params![plan,cursor.position.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|e|native(e.into()))?;
                if let Some((sequence, asset_id, excluded, listed)) = row {
                    cursor.position = I64(sequence);
                    if !excluded || listed {
                        None
                    } else {
                        Some(Rule::Override(Override::Asset {
                            asset_id,
                            candidates: vec![],
                        }))
                    }
                } else {
                    cursor.stage = RuleStage::ExcludedSource;
                    cursor.position = I64(0);
                    cursor.source = I64(0);
                    None
                }
            }
            RuleStage::ExcludedSource => {
                // Seek the indexed parent key and expose scan progress; rare exclusions
                // never cause a whole-plan scan in one actor command.
                let row:Option<(i64,i64,String,bool)>=catalog.db.query_row("SELECT s.sequence,s.source_id,s.status,EXISTS(SELECT 1 FROM storage_exceptions e WHERE e.plan=s.plan AND e.kind='source' AND e.entity=CAST(s.source_id AS TEXT)) FROM storage_source_items s WHERE s.plan=?1 AND (s.sequence,s.source_id)>(?2,?3) ORDER BY s.sequence,s.source_id LIMIT 1",params![plan,cursor.position.0,cursor.source.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|e|native(e.into()))?;
                if let Some((sequence, source, status, listed)) = row {
                    cursor.position = I64(sequence);
                    cursor.source = I64(source);
                    if status == "excluded" && !listed {
                        Some(Rule::Override(Override::Source {
                            source_id: I64(source),
                            candidates: vec![],
                        }))
                    } else {
                        None
                    }
                } else {
                    cursor.stage = RuleStage::End;
                    None
                }
            }
            RuleStage::End => None,
        };
        if let Some(rule) = row {
            let label = rule_label(catalog, &rule, limits.page_bytes.min(MAX_PATH_BYTES))?;
            rows.push(RuleRow { rule, label });
            if bounded(&rows, limits.page_bytes).is_err() {
                rows.pop();
                if rows.is_empty() {
                    return Err(error(
                        ErrorCode::ResourceLimit,
                        "single saved relink rule exceeds page byte allowance",
                    ));
                }
                cursor = before;
                break;
            }
        }
    }
    Ok(Response::Rules {
        rows,
        next: (cursor.stage != RuleStage::End).then_some(cursor),
        scanned: U64(scanned as u64),
    })
}

fn rule_label(catalog: &Catalog, rule: &Rule, bytes: usize) -> Result<Option<String>> {
    use rusqlite::{OptionalExtension, params};
    let result:Option<Option<String>>=match rule {
        Rule::Override(Override::Asset{asset_id,..})|Rule::Scope(SavedScope::Asset{asset_id,..})=>catalog.db.query_row("SELECT CASE WHEN length(CAST(path_display AS BLOB))<=?2 THEN path_display END FROM assets WHERE id=?1",params![asset_id,bytes as i64],|r|r.get(0)).optional(),
        Rule::Override(Override::Source{source_id,..})=>catalog.db.query_row("SELECT CASE WHEN length(CAST(display AS BLOB))<=?2 THEN display END FROM metadata_sources WHERE id=?1",params![source_id.0,bytes as i64],|r|r.get(0)).optional(),
        _=>return Ok(None),
    }.map_err(|e|native(e.into()))?;
    match result {
        Some(Some(label)) => Ok(Some(label)),
        Some(None) => Err(error(
            ErrorCode::ResourceLimit,
            "relink target display name exceeds row allowance",
        )),
        None => Ok(None),
    }
}
