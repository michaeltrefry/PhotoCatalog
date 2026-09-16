//! Durable adjustment-copy requests and one-target actor maintenance.
use super::{BridgeError, Cancellation, ErrorCode, I64, Limits, U64, error, native};
use crate::{
    Catalog,
    catalog_edits::{self as core, VariantKey},
    edit::{AdjustmentGroup, Recipe},
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
type Result<T> = std::result::Result<T, BridgeError>;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "command",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    Begin {
        source: VariantKey,
        expected_revision: I64,
        groups: Vec<AdjustmentGroup>,
    },
    Append {
        job: String,
        expected_total: I64,
        targets: Vec<Target>,
    },
    Seal {
        job: String,
        expected_total: I64,
    },
    Run {
        job: String,
    },
    Status {
        operation: Option<String>,
    },
    Cancel {
        job: String,
        operation: Option<String>,
    },
    Job {
        job: String,
    },
    Jobs {
        after: I64,
        limit: U64,
    },
    Items {
        job: String,
        after: I64,
        limit: U64,
    },
    Inspect {
        job: String,
    },
}
impl Request {
    pub(super) fn read_only(&self) -> bool {
        matches!(
            self,
            Self::Status { .. }
                | Self::Job { .. }
                | Self::Jobs { .. }
                | Self::Items { .. }
                | Self::Inspect { .. }
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub key: VariantKey,
    pub expected_revision: I64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub sequence: I64,
    pub id: String,
    pub state: String,
    pub total: I64,
    pub completed: I64,
}
impl From<core::CopyJob> for Job {
    fn from(v: core::CopyJob) -> Self {
        Self {
            sequence: I64(v.sequence),
            id: v.id,
            state: v.state,
            total: I64(v.total),
            completed: I64(v.completed),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Name {
    pub filename: String,
    pub variant_label: String,
    pub available: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub sequence: I64,
    pub target: Target,
    pub name: Name,
    pub state: String,
    pub applied_revision: Option<I64>,
    pub current_revision: Option<I64>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inspection {
    pub job: Job,
    pub source: Target,
    pub name: Name,
    pub digest: String,
    pub recipe: Recipe,
    pub groups: Vec<AdjustmentGroup>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Running,
    Paused,
    CancelRequested,
    Complete,
    Canceled,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub job: Job,
    pub phase: Phase,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Response {
    Job(Job),
    Jobs { rows: Vec<Job>, next: Option<I64> },
    Items { rows: Vec<Item>, next: Option<I64> },
    Inspection(Box<Inspection>),
    Operation(Option<Operation>),
}
#[derive(Default)]
pub(super) struct Control {
    pub status: Option<Operation>,
    cancel: Option<Cancellation>,
    running: bool,
}
impl Control {
    pub fn status(&self, operation: Option<&str>) -> Result<Option<Operation>> {
        if operation.is_some_and(|id| self.status.as_ref().is_none_or(|s| s.id != id)) {
            return Err(error(ErrorCode::StaleSession, "copy operation changed"));
        }
        Ok(self.status.clone())
    }
    /// None means an inactive draft/queued job needs the actor's durable cancel.
    pub fn cancel(&mut self, job: &str, operation: Option<&str>) -> Result<Option<Operation>> {
        let Some(operation) = operation else {
            if self.running && self.status.as_ref().is_some_and(|s| s.job.id == job) {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "active copy cancellation requires its operation id",
                ));
            }
            return Ok(None);
        };
        self.status(Some(operation))?;
        if self.status.as_ref().is_none_or(|s| s.job.id != job) {
            return Err(error(ErrorCode::StaleSession, "copy job changed"));
        }
        if self.running {
            self.cancel.as_ref().unwrap().cancel();
            self.status.as_mut().unwrap().phase = Phase::CancelRequested;
        }
        Ok(self.status.clone())
    }
    fn finish(&mut self, result: std::result::Result<core::CopyJob, String>) {
        self.running = false;
        self.cancel = None;
        if let Some(s) = &mut self.status {
            match result {
                Ok(job) => {
                    s.phase = if job.state == "canceled" {
                        Phase::Canceled
                    } else {
                        Phase::Complete
                    };
                    s.job = job.into();
                    s.error = None
                }
                Err(e) => {
                    s.phase = Phase::Failed;
                    s.error = Some(e.chars().take(2048).collect())
                }
            }
        }
    }
}
fn id(v: &str) -> Result<()> {
    if v.is_empty() || v.len() > 256 {
        Err(error(ErrorCode::InvalidRequest, "copy job identity length"))
    } else {
        Ok(())
    }
}
fn bounded<T: Serialize>(v: &T, n: usize) -> Result<()> {
    if serde_json::to_vec(v).map_err(|e| native(e.into()))?.len() > n {
        Err(error(
            ErrorCode::ResourceLimit,
            "copy response byte allowance",
        ))
    } else {
        Ok(())
    }
}
fn page(after: I64, limit: U64, limits: &Limits) -> Result<usize> {
    if after.0 < 0 || limit.0 == 0 || limit.0 > 200 || limit.0 > limits.page_rows as u64 {
        Err(error(ErrorCode::InvalidRequest, "copy page allowance"))
    } else {
        Ok(limit.0 as usize)
    }
}
fn name(c: &Catalog, key: &VariantKey, bytes: usize) -> Result<(Name, Option<I64>)> {
    key.validate().map_err(native)?;
    let row:Option<(Option<String>,Option<String>,Option<i64>)>=c.db.query_row("SELECT CASE WHEN length(CAST(a.path_display AS BLOB))<=?3 THEN a.path_display END,CASE WHEN length(CAST(COALESCE(v.label,'Master') AS BLOB))<=?3 THEN COALESCE(v.label,'Master') END,CASE WHEN ?2='master' THEN COALESCE(v.revision,0) ELSE v.revision END FROM assets a LEFT JOIN edit_variants v ON v.asset_id=a.id AND v.id=?2 WHERE a.id=?1",rusqlite::params![key.asset_id,key.variant_id,bytes.min(32768) as i64],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|e|native(e.into()))?;
    let Some((path, label, revision)) = row else {
        return Ok((
            Name {
                filename: "Unavailable photo".into(),
                variant_label: "Unavailable variant".into(),
                available: false,
            },
            None,
        ));
    };
    let path = path.ok_or_else(|| {
        error(
            ErrorCode::ResourceLimit,
            "copy filename exceeds byte allowance",
        )
    })?;
    let label = label.ok_or_else(|| {
        error(
            ErrorCode::ResourceLimit,
            "copy variant label exceeds byte allowance",
        )
    })?;
    Ok((
        Name {
            filename: path.rsplit(['/', '\\']).next().unwrap_or(&path).into(),
            variant_label: if revision.is_some() {
                label
            } else {
                "Unavailable variant".into()
            },
            available: revision.is_some(),
        },
        revision.map(I64),
    ))
}
fn item(c: &Catalog, job: &str, after: i64, limits: &Limits) -> Result<Option<Item>> {
    let size:Option<i64>=c.db.query_row("SELECT length(CAST(asset_id AS BLOB))+length(CAST(variant_id AS BLOB))+length(CAST(state AS BLOB))+COALESCE(length(CAST(error AS BLOB)),0) FROM edit_copy_items WHERE job=?1 AND sequence>?2 ORDER BY sequence LIMIT 1",rusqlite::params![job,after],|r|r.get(0)).optional().map_err(|e|native(e.into()))?;
    let Some(size) = size else { return Ok(None) };
    if size > limits.page_bytes as i64 {
        return Err(error(
            ErrorCode::ResourceLimit,
            "copy item exceeds byte allowance",
        ));
    }
    let v = c
        .edit_copy_items(job, after, 1)
        .map_err(native)?
        .pop()
        .unwrap();
    let (name, current_revision) = name(c, &v.target.key, limits.page_bytes)?;
    Ok(Some(Item {
        sequence: I64(v.sequence),
        target: Target {
            key: v.target.key,
            expected_revision: I64(v.target.expected_revision),
        },
        name,
        state: v.state,
        applied_revision: v.applied_revision.map(I64),
        current_revision,
        error: v.error,
    }))
}
fn read(c: &Catalog, r: Request, limits: &Limits) -> Result<Response> {
    let response = match r {
        Request::Job { job } => {
            id(&job)?;
            Response::Job(c.edit_copy_job(&job).map_err(native)?.into())
        }
        Request::Inspect { job } => {
            id(&job)?;
            let v = c.edit_copy_description(&job).map_err(native)?;
            let (name, _) = name(c, &v.source.key, limits.page_bytes)?;
            Response::Inspection(Box::new(Inspection {
                job: c.edit_copy_job(&job).map_err(native)?.into(),
                source: Target {
                    key: v.source.key,
                    expected_revision: I64(v.source.expected_revision),
                },
                name,
                digest: v.digest,
                recipe: v.recipe,
                groups: v.groups,
            }))
        }
        Request::Items { job, after, limit } => {
            id(&job)?;
            c.edit_copy_job(&job).map_err(native)?;
            let n = page(after, limit, limits)?;
            let mut cursor = after.0;
            let mut rows = Vec::new();
            let mut more = false;
            loop {
                if rows.len() == n {
                    more=c.db.query_row("SELECT EXISTS(SELECT 1 FROM edit_copy_items WHERE job=?1 AND sequence>?2)",rusqlite::params![job,cursor],|r|r.get(0)).map_err(|e|native(e.into()))?;
                    break;
                }
                let Some(v) = item(c, &job, cursor, limits)? else {
                    break;
                };
                rows.push(v);
                if bounded(&rows, limits.page_bytes).is_err() {
                    rows.pop();
                    if rows.is_empty() {
                        return Err(error(
                            ErrorCode::ResourceLimit,
                            "single copy item exceeds page bytes",
                        ));
                    }
                    more = true;
                    break;
                }
                cursor = rows.last().unwrap().sequence.0;
            }
            Response::Items {
                rows,
                next: more.then_some(I64(cursor)),
            }
        }
        Request::Jobs { after, limit } => {
            let n = page(after, limit, limits)?;
            let mut cursor = after.0;
            let mut rows = Vec::new();
            let mut more = false;
            loop {
                let next:Option<(i64,Option<String>)>=c.db.query_row("SELECT sequence,CASE WHEN length(CAST(id AS BLOB))<=256 THEN id END FROM edit_copy_jobs WHERE sequence>?1 ORDER BY sequence LIMIT 1",[cursor],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|native(e.into()))?;
                let Some((seq, id)) = next else { break };
                if rows.len() == n {
                    more = true;
                    break;
                }
                let id = id.ok_or_else(|| {
                    error(ErrorCode::ResourceLimit, "copy job identity exceeds limit")
                })?;
                rows.push(Job::from(c.edit_copy_job(&id).map_err(native)?));
                if bounded(&rows, limits.page_bytes).is_err() {
                    rows.pop();
                    if rows.is_empty() {
                        return Err(error(
                            ErrorCode::ResourceLimit,
                            "single copy job exceeds page bytes",
                        ));
                    }
                    more = true;
                    break;
                }
                cursor = seq;
            }
            Response::Jobs {
                rows,
                next: more.then_some(I64(cursor)),
            }
        }
        _ => return Err(error(ErrorCode::InvalidRequest, "not a copy read")),
    };
    bounded(&response, limits.reply_bytes)?;
    Ok(response)
}
pub(super) fn execute(
    c: &mut Catalog,
    r: Request,
    limits: &Limits,
    control: &Arc<Mutex<Control>>,
    held: bool,
) -> Result<Response> {
    if let Request::Status { operation } = r {
        return Ok(Response::Operation(
            control.lock().unwrap().status(operation.as_deref())?,
        ));
    }
    if r.read_only() {
        let tx = c.db.unchecked_transaction().map_err(|e| native(e.into()))?;
        let out = read(c, r, limits);
        tx.commit().map_err(|e| native(e.into()))?;
        return out;
    }
    if held {
        return Err(error(
            ErrorCode::Busy,
            "copy writes are held; review and release restored/relink jobs first",
        ));
    }
    let response = match r {
        Request::Begin {
            source,
            expected_revision,
            groups,
        } => {
            source.validate().map_err(native)?;
            Response::Job(
                c.begin_edit_copy(&source, expected_revision.0, &groups)
                    .map_err(native)?
                    .into(),
            )
        }
        Request::Append {
            job,
            expected_total,
            targets,
        } => {
            id(&job)?;
            if targets.len() > limits.page_rows as usize {
                return Err(error(
                    ErrorCode::ResourceLimit,
                    "copy target page allowance",
                ));
            }
            let targets = targets
                .into_iter()
                .map(|t| core::EditTarget {
                    key: t.key,
                    expected_revision: t.expected_revision.0,
                })
                .collect::<Vec<_>>();
            Response::Job(
                c.append_edit_copy(&job, expected_total.0, &targets)
                    .map_err(native)?
                    .into(),
            )
        }
        Request::Seal {
            job,
            expected_total,
        } => {
            id(&job)?;
            Response::Job(
                c.seal_edit_copy(&job, expected_total.0)
                    .map_err(native)?
                    .into(),
            )
        }
        Request::Run { job } => {
            id(&job)?;
            let frozen = c.edit_copy_description(&job).map_err(native)?;
            bounded(&frozen, limits.page_bytes)?;
            let j = c.edit_copy_job(&job).map_err(native)?;
            if j.state != "queued" {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "copy job must be sealed and queued",
                ));
            }
            let mut ctl = control.lock().unwrap();
            if ctl.running {
                return Err(error(ErrorCode::Busy, "another copy operation is active"));
            }
            ctl.cancel = Some(Cancellation::default());
            ctl.running = true;
            ctl.status = Some(Operation {
                id: uuid::Uuid::new_v4().to_string(),
                job: j.into(),
                phase: Phase::Running,
                error: None,
            });
            Response::Operation(ctl.status.clone())
        }
        Request::Cancel { job, operation } => {
            id(&job)?;
            let mut ctl = control.lock().unwrap();
            if let Some(s) = ctl.cancel(&job, operation.as_deref())? {
                Response::Operation(Some(s))
            } else {
                Response::Job(c.cancel_edit_copy(&job).map_err(native)?.into())
            }
        }
        _ => unreachable!(),
    };
    bounded(&response, limits.reply_bytes)?;
    Ok(response)
}
/// Called once per maintenance opportunity, after all foreground commands.
pub(super) fn advance(
    c: &mut Catalog,
    control: &Arc<Mutex<Control>>,
    foreground: bool,
    held: bool,
    #[cfg(test)] checkpoint: Option<crate::import_preparation::Checkpoint>,
) -> bool {
    let (job, cancel) = {
        let mut ctl = control.lock().unwrap();
        if !ctl.running {
            return false;
        }
        let cancel = ctl.cancel.as_ref().unwrap().clone();
        let status = ctl.status.as_mut().unwrap();
        if held {
            status.phase = if cancel.is_canceled() {
                Phase::CancelRequested
            } else {
                Phase::Paused
            };
            return false;
        }
        if foreground && !cancel.is_canceled() {
            return false;
        }
        status.phase = if cancel.is_canceled() {
            Phase::CancelRequested
        } else {
            Phase::Running
        };
        (status.job.id.clone(), cancel)
    };
    #[cfg(test)]
    if let Some(checkpoint) = checkpoint {
        checkpoint("copy_before_step", &cancel.0);
    }
    let result = if cancel.is_canceled() {
        c.cancel_edit_copy(&job)
    } else {
        c.apply_edit_copy_step(&job, 1)
    };
    let mut ctl = control.lock().unwrap();
    match result {
        Ok(j) => {
            if j.state == "queued" {
                ctl.status.as_mut().unwrap().job = j.into();
            } else {
                ctl.finish(Ok(j));
            }
        }
        Err(e) => ctl.finish(Err(format!("{e:#}"))),
    }
    true
}
pub(super) fn close(c: &mut Catalog, control: &Arc<Mutex<Control>>) {
    let mut ctl = control.lock().unwrap();
    if ctl.running && ctl.cancel.as_ref().is_some_and(Cancellation::is_canceled) {
        let job = ctl.status.as_ref().unwrap().job.id.clone();
        let result = c.cancel_edit_copy(&job).map_err(|e| format!("{e:#}"));
        ctl.finish(result);
    }
    *ctl = Control::default();
}
