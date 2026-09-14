//! Owned desktop export execution. The actor grants writer/native admission;
//! every source/profile/destination filesystem operation runs on the pinned worker.
use super::{BridgeError, Cancellation, Config, ErrorCode, I64, Limits, U64, error, native};
use crate::{
    Catalog,
    catalog_edits::VariantKey,
    catalog_exports as core,
    preview::{NativeLaunchPause, NativeLaunchPermit, PreviewService},
    storage_volume::NativePath,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
mod dto;
mod read;
mod worker;
pub use dto::*;
type Result<T> = std::result::Result<T, BridgeError>;
const PROFILE_BYTES: usize = crate::catalog_session::EXPORT_PROFILE_BYTES;
const PATH_BYTES: usize = 32 * 1024;
fn identity(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > 256 {
        Err(error(ErrorCode::InvalidRequest, "export identity length"))
    } else {
        Ok(())
    }
}
fn bounded<T: Serialize>(value: &T, bytes: usize) -> Result<()> {
    if serde_json::to_vec(value)
        .map_err(|e| native(e.into()))?
        .len()
        > bytes
    {
        Err(error(
            ErrorCode::ResourceLimit,
            "export byte allowance exceeded",
        ))
    } else {
        Ok(())
    }
}
fn page(after: i64, limit: U64, limits: &Limits) -> Result<usize> {
    if after < 0 || limit.0 == 0 || limit.0 > u64::from(limits.page_rows) {
        Err(error(ErrorCode::InvalidRequest, "export page bounds"))
    } else {
        Ok(limit.0 as usize)
    }
}
fn path(value: &NativePath) -> Result<std::path::PathBuf> {
    bounded(value, PATH_BYTES)?;
    let p = value
        .to_path()
        .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
    if !p.is_absolute() {
        return Err(error(
            ErrorCode::InvalidRequest,
            "absolute native export path required",
        ));
    }
    Ok(p)
}
struct ProfileEntry {
    info: ProfileAdmission,
    bytes: Arc<Vec<u8>>,
}
pub(crate) fn profile_cache_entry_layout() -> (usize, usize) {
    type Entry = (String, ProfileEntry);
    (std::mem::size_of::<Entry>(), std::mem::align_of::<Entry>())
}
#[derive(Default)]
struct Cache {
    profiles: HashMap<String, ProfileEntry>,
    destinations: HashMap<String, Vec<Destination>>,
}
#[derive(Default)]
pub(super) struct Control {
    pub status: Option<Operation>,
    cancel: Cancellation,
    shutdown: Cancellation,
    active: bool,
    yield_requested: bool,
    foreground_yield: bool,
    cancel_job_on_shutdown: Option<String>,
    hold_since: Option<Instant>,
    hold_granted: bool,
    permit_requested: bool,
    permit: Option<NativeLaunchPermit>,
}
impl Control {
    pub fn status(&self, operation: Option<&str>) -> Result<Option<Operation>> {
        if operation.is_some_and(|id| self.status.as_ref().is_none_or(|s| s.id != id)) {
            return Err(error(ErrorCode::StaleSession, "export operation changed"));
        }
        Ok(self.status.clone())
    }
    pub fn cancel(
        &mut self,
        job: Option<&str>,
        operation: Option<&str>,
    ) -> Result<Option<Operation>> {
        let Some(id) = operation else {
            let job =
                job.ok_or_else(|| error(ErrorCode::InvalidRequest, "job or operation required"))?;
            if self.active
                && self
                    .status
                    .as_ref()
                    .is_some_and(|s| s.job.as_ref().is_some_and(|j| j.id == job))
            {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "active export cancellation requires its operation ID",
                ));
            }
            return Ok(None);
        };
        self.status(Some(id))?;
        let status = self.status.as_ref().unwrap();
        if status.job.as_ref().map(|j| j.id.as_str()) != job {
            return Err(error(
                ErrorCode::StaleSession,
                "export operation job changed",
            ));
        }
        if self.active {
            self.cancel.cancel();
            self.status.as_mut().unwrap().phase = "cancel_requested".into();
        }
        Ok(self.status.clone())
    }
    pub fn yield_job(&mut self, job: &str, operation: &str) -> Result<Option<Operation>> {
        self.status(Some(operation))?;
        let s = self.status.as_ref().unwrap();
        if s.kind != "run" || s.job.as_ref().is_none_or(|j| j.id != job) {
            return Err(error(
                ErrorCode::InvalidRequest,
                "yield requires matching export run",
            ));
        }
        if self.active {
            self.yield_requested = true;
            self.status.as_mut().unwrap().stage = "yielding".into();
        }
        Ok(self.status.clone())
    }
    pub fn request_cancel(&self) {
        self.cancel.cancel();
    }
    pub fn busy(&self) -> bool {
        self.active
    }
}
#[derive(Default)]
pub(super) struct Coordinator {
    sql_session: Option<Arc<crate::catalog_session::CatalogSessionAuthority>>,
    sender: Option<mpsc::SyncSender<worker::Task>>,
    thread: Option<thread::JoinHandle<()>>,
    cache: Arc<Mutex<Cache>>,
    pause: Option<NativeLaunchPause>,
    control: Option<Arc<Mutex<Control>>>,
}
impl Coordinator {
    pub fn write_hold(&self, control: &Arc<Mutex<Control>>) -> bool {
        control.lock().unwrap().hold_granted
    }
    pub fn hold_since(&self, control: &Arc<Mutex<Control>>) -> Option<Instant> {
        control.lock().unwrap().hold_since
    }
    pub fn signal_shutdown(&self, control: &Arc<Mutex<Control>>) {
        let mut c = control.lock().unwrap();
        if c.active && c.status.as_ref().is_some_and(|s| s.kind == "run") {
            c.cancel_job_on_shutdown = c
                .status
                .as_ref()
                .and_then(|s| s.job.as_ref().map(|j| j.id.clone()));
        }
        c.cancel.cancel();
        c.shutdown.cancel();
    }
    pub fn shutdown(&mut self, control: &Arc<Mutex<Control>>) {
        self.signal_shutdown(control);
        self.sender.take();
        if let Some(t) = self.thread.take() {
            let healthy = t.join().is_ok();
            if let Some(session) = &self.sql_session {
                let _ = session.joined(crate::catalog_session::SqlRole::Export, healthy);
            }
        }
        self.pause.take();
        self.cache.lock().unwrap().profiles.clear();
        self.cache.lock().unwrap().destinations.clear();
    }
    pub fn advance(
        &mut self,
        catalog: &Catalog,
        service: &mut PreviewService,
        control: &Arc<Mutex<Control>>,
        prior_foreground: bool,
        native_demand: bool,
        other_hold: bool,
    ) -> Result<()> {
        let mut c = control.lock().unwrap();
        if !c.active {
            self.pause.take();
            return Ok(());
        }
        if native_demand {
            c.foreground_yield = true;
        }
        if c.hold_since.is_some() && !c.hold_granted && !prior_foreground && !other_hold {
            c.hold_granted = true;
            if let Some(s) = &mut c.status {
                s.write_hold = true;
            }
        }
        if c.permit_requested && !other_hold && !native_demand && !prior_foreground {
            if self.pause.is_none() {
                self.pause = Some(service.pause_native_launches().map_err(native)?);
            }
            if service.native_work_drained() {
                let permit = service
                    .native_launch_permit(catalog, self.pause.take().unwrap())
                    .map_err(native)?;
                c.permit = Some(permit);
                c.permit_requested = false;
                c.foreground_yield = false;
            }
        } else if native_demand {
            self.pause.take();
        }
        Ok(())
    }
    pub fn execute(
        &mut self,
        catalog: &mut Catalog,
        r: Request,
        config: &Config,
        control: &Arc<Mutex<Control>>,
        held: bool,
        deferred: Option<mpsc::SyncSender<super::Reply>>,
    ) -> Result<Response> {
        let options = worker::options(config);
        if let Request::Status { operation } = &r {
            return Ok(Response::Operation(
                control.lock().unwrap().status(operation.as_deref())?,
            ));
        }
        if r.read_only() {
            return read::execute(catalog, r, &config.limits, &self.cache, &options);
        }
        if let Request::Cancel { job, operation } = &r {
            if operation.is_some() {
                return Ok(Response::Operation(
                    control
                        .lock()
                        .unwrap()
                        .cancel(job.as_deref(), operation.as_deref())?,
                ));
            }
            if deferred.is_none() {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "inactive cancellation requires its owned reply channel",
                ));
            }
            job.as_ref()
                .ok_or_else(|| error(ErrorCode::InvalidRequest, "export job required"))?;
        }
        if let Request::Yield { job, operation } = r {
            return Ok(Response::Operation(
                control.lock().unwrap().yield_job(&job, &operation)?,
            ));
        }
        if held {
            return Err(error(
                ErrorCode::Busy,
                "catalog jobs are held; finish the other operation or explicitly resume restored jobs",
            ));
        }
        if control.lock().unwrap().active {
            return Err(error(ErrorCode::Busy, "another export operation is active"));
        }
        match r {
            Request::Begin => {
                return Ok(Response::Job(
                    catalog.begin_photo_export().map_err(native)?.into(),
                ));
            }
            Request::Seal {
                job,
                expected_total,
            } => {
                read::job(catalog, &job)?;
                return Ok(Response::Job(
                    catalog
                        .seal_photo_export_job(&job, expected_total.0)
                        .map_err(native)?
                        .into(),
                ));
            }
            _ => {}
        }
        worker::validate(&r, config, &options)?;
        let job = r.job_id().map(|j| read::job(catalog, j)).transpose()?;
        if let Request::RetrySeal {
            job,
            sequence,
            authority,
        }
        | Request::Restore {
            job,
            sequence,
            authority,
        } = &r
        {
            let (_, actual) = read::document(catalog, job, sequence.0)?;
            if actual != *authority {
                return Err(error(
                    ErrorCode::Superseded,
                    "export reviewed authority changed",
                ));
            }
        }
        let profile_snapshot = match &r {
            Request::Append {
                output:
                    Output {
                        profile: Profile::Icc { token },
                        ..
                    },
                ..
            } => Some(
                self.cache
                    .lock()
                    .unwrap()
                    .profiles
                    .get(token)
                    .ok_or_else(|| {
                        error(
                            ErrorCode::StaleSession,
                            "ICC token unavailable; admit profile again",
                        )
                    })?
                    .bytes
                    .clone(),
            ),
            _ => None,
        };
        if self.sender.is_none() {
            self.control = Some(Arc::clone(control));
            self.sql_session = Some(catalog.session.clone());
            let handle = catalog
                .sql_worker_handle(crate::catalog_session::SqlRole::Export)
                .map_err(native)?;
            let (tx, rx) = mpsc::sync_channel(1);
            let ctx = worker::Context {
                control: Arc::clone(control),
                cache: Arc::clone(&self.cache),
                config: config.clone(),
            };
            self.thread = Some(
                thread::Builder::new()
                    .name("catalog-photo-export".into())
                    .spawn(move || worker::run(handle, rx, ctx))
                    .map_err(|e| native(e.into()))?,
            );
            self.sender = Some(tx);
        }
        let status = Operation {
            id: uuid::Uuid::new_v4().to_string(),
            kind: r
                .kind()
                .ok_or_else(|| error(ErrorCode::InvalidRequest, "unsupported export command"))?
                .into(),
            phase: "running".into(),
            job,
            sequence: match &r {
                Request::RetrySeal { sequence, .. } | Request::Restore { sequence, .. } => {
                    Some(*sequence)
                }
                _ => None,
            },
            stage: "opening".into(),
            stream_bytes: None,
            processed: U64(0),
            write_hold: false,
            result: None,
            error: None,
        };
        {
            let mut c = control.lock().unwrap();
            c.cancel = Cancellation::default();
            c.active = true;
            c.yield_requested = false;
            c.foreground_yield = false;
            c.hold_since = None;
            c.hold_granted = false;
            c.permit_requested = false;
            c.permit = None;
            c.status = Some(status.clone());
        }
        if self
            .sender
            .as_ref()
            .unwrap()
            .try_send(worker::Task {
                request: r,
                reply: deferred,
                profile_snapshot,
            })
            .is_err()
        {
            let mut c = control.lock().unwrap();
            c.active = false;
            if let Some(s) = &mut c.status {
                s.phase = "failed".into();
                s.error = Some("export worker unavailable".into());
            }
            return Err(error(ErrorCode::Closed, "export worker unavailable"));
        }
        Ok(Response::Operation(Some(status)))
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        if let Some(c) = self.control.clone() {
            self.shutdown(&c);
        }
    }
}

#[cfg(test)]
mod tests;
