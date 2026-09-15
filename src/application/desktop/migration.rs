//! G owns the upload, executor, Source broker and checked terminal publication.
use super::{Shared, lightroom_migration as relay};
use crate::{
    application::{self, BridgeError, ErrorCode, U64, lightroom_migration as api},
    catalog_writer::{ExternalAdmission, ExternalLease, Writers},
    lightroom_migration_worker::{
        identity::FileKey,
        input::{self, TEXT_CHUNK},
        lease::DestinationPin,
        memory::{MemoryBudget, Reservation, SharedAllocationGrant},
        process::Stop,
        protocol::{Guard, WriteKind},
        supervisor::{self, Admission, Drained, FailureCause},
        worker,
    },
    preview::{ByteBudget, ByteReservation},
};
use anyhow::{Context, Result, ensure};
use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, Weak, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

fn bridge(e: anyhow::Error) -> BridgeError {
    if e.downcast_ref::<crate::lightroom_migration_worker::memory::ResourceLimit>()
        .is_some()
    {
        application::error(ErrorCode::ResourceLimit, e.to_string())
    } else {
        application::error(ErrorCode::InvalidRequest, e.to_string())
    }
}
struct Count {
    bytes: usize,
    maximum: usize,
}
impl std::io::Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("JSON size overflow"))?;
        if self.bytes > self.maximum {
            return Err(std::io::Error::other(
                "configured migration message allowance",
            ));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn measured(value: &impl serde::Serialize, maximum: usize) -> Result<usize> {
    let mut sink = Count { bytes: 0, maximum };
    serde_json::to_writer(&mut sink, value)?;
    Ok(sink.bytes)
}
fn incoming(request: &api::Request, maximum: usize) -> Result<usize> {
    #[derive(serde::Serialize)]
    struct Args<'a> {
        request: &'a api::Request,
    }
    #[derive(serde::Serialize)]
    struct Request<'a> {
        command: &'static str,
        args: Args<'a>,
    }
    measured(
        &Request {
            command: "lightroom_migration",
            args: Args { request },
        },
        maximum,
    )
}
// Count the complete unchanged application Reply envelope without allocating
// an escaped response. Migration data never travels through C's public queue.
fn outgoing(value: &impl serde::Serialize, maximum: usize) -> Result<usize> {
    #[derive(serde::Serialize)]
    struct Value<'a, T> {
        kind: &'static str,
        data: &'a T,
    }
    #[derive(serde::Serialize)]
    struct Reply<'a, T> {
        status: &'static str,
        value: Value<'a, T>,
    }
    measured(
        &Reply {
            status: "ok",
            value: Value {
                kind: "lightroom_migration",
                data: value,
            },
        },
        maximum,
    )
}
fn snapshot_bytes(snapshot: &api::Snapshot, maximum: usize) -> Result<usize> {
    #[derive(serde::Serialize)]
    struct Status<'a> {
        kind: &'static str,
        data: &'a api::Snapshot,
    }
    outgoing(
        &Status {
            kind: "status",
            data: snapshot,
        },
        maximum,
    )
}
fn failure(value: &supervisor::Failure) -> api::Failure {
    let (code, required, available) = match &value.cause {
        FailureCause::ResourceLimit(limit) => (
            ErrorCode::ResourceLimit,
            Some(U64(limit.required as u64)),
            Some(U64(limit.available as u64)),
        ),
        FailureCause::Canceled => (ErrorCode::Canceled, None, None),
        FailureCause::Rejected(_) => (ErrorCode::Native, None, None),
        FailureCause::StatusUnavailable => (ErrorCode::Native, None, None),
    };
    api::Failure {
        code,
        detail: value.to_string(),
        required,
        available,
        poisoned: value.poisoned,
        outcome_unknown: value.outcome_unknown,
    }
}

pub(super) struct Coordinator {
    shared: Weak<Shared>,
    executable: PathBuf,
    pool: Option<ByteBudget>,
    slot: Mutex<Option<Entry>>,
    #[cfg(test)]
    faults: Arc<Mutex<Faults>>,
}
struct Entry {
    guard: Guard,
    header_digest: blake3::Hash,
    catalog: Option<String>,
    uploaded: u64,
    state: EntryState,
    // Retained API bookkeeping is distinct from operation and result custody.
    _bookkeeping: Arc<ByteReservation>,
}
enum EntryState {
    Upload(Upload),
    Active {
        job: Arc<Job>,
        thread: Option<JoinHandle<Drained>>,
    },
    Terminal {
        drained: Drained,
        progress: Option<(String, U64, Option<U64>)>,
    },
}
struct Upload {
    header: api::Header,
    texts: Vec<String>,
    next: usize,
    hash: blake3::Hasher,
    budget: MemoryBudget,
    _memory: Reservation,
}
struct Report {
    phase: api::Phase,
    catalog: Option<String>,
    progress: Option<(String, U64, Option<U64>)>,
    failure: Option<api::Failure>,
    retry: u64,
}
#[cfg(test)]
#[derive(Default)]
struct Faults {
    lost_acquire: usize,
    lost_release: usize,
    pause_drain: bool,
    drain_paused: bool,
    lost: usize,
    wait_failures: usize,
    wait_injected: bool,
    pause_writer: bool,
    writer_held: bool,
    pause_result: bool,
    result_seen: bool,
    graphs_built: usize,
    pause_target: bool,
    target_before_submit: bool,
    enqueue_target: bool,
    enqueue_writer: bool,
    enqueued: bool,
    fail_waiter_spawn: bool,
    pause_external: bool,
    external_pending: bool,
    lost_target: usize,
    unknown_target: usize,
    unknown_write: usize,
    disconnect_target: usize,
    disconnect_write: usize,
    catalog_acquire_faults_only: bool,
    pause_after_acquire_fault: bool,
    acquire_fault_seen: bool,
    pause_acquire_recovery: bool,
    acquire_recovery_seen: bool,
}
struct Job {
    stop: Arc<Stop>,
    report: Mutex<Report>,
    wake: Condvar,
    #[cfg(test)]
    faults: Arc<Mutex<Faults>>,
}
impl Job {
    fn cancel(&self) {
        self.stop.cancel();
        let mut r = self.report.lock().unwrap();
        r.retry = r.retry.wrapping_add(1);
        if r.phase != api::Phase::DrainPending {
            r.phase = api::Phase::CancelRequested;
        }
        self.wake.notify_all();
    }
    fn retry(&self) {
        let mut r = self.report.lock().unwrap();
        r.retry = r.retry.wrapping_add(1);
        self.wake.notify_all();
    }
    fn pending(&self, cause: Option<&supervisor::Failure>) {
        let mut r = self.report.lock().unwrap();
        r.phase = api::Phase::DrainPending;
        r.failure = cause.map(failure);
    }
}
impl Coordinator {
    pub(super) fn new(shared: &Arc<Shared>, executable: PathBuf, pool: Option<ByteBudget>) -> Self {
        // Move the caller's existing Config path; idle production state adds no
        // separate Arc/Vec/String backing. Begin funds operation bookkeeping
        // before any worker-path copy or operation owner is constructed.
        Self {
            shared: Arc::downgrade(shared),
            executable,
            pool,
            slot: Mutex::new(None),
            #[cfg(test)]
            faults: Default::default(),
        }
    }
    fn bookkeeping_bytes(&self, operation: &str, reply: usize) -> Result<u64> {
        // Borrowed lengths and fixed layouts only: the token precedes every new
        // owned Guard, preflight Snapshot and operation graph.
        use std::mem::size_of;
        let values = [
            size_of::<Self>(),
            self.executable.as_os_str().len(),
            size_of::<Entry>(),
            size_of::<Job>(),
            size_of::<Report>(),
            size_of::<Proxy>(),
            2 * size_of::<Waiting>(),
            size_of::<WriteAttempt>(),
            2 * size_of::<Mutex<Acquisition>>(),
            8 * size_of::<usize>(),
            size_of::<api::Snapshot>(),
            2 * size_of::<Guard>(),
            2 * (36 + 64),
            operation.len(),
            operation.len(),
            128 + 256 + 64,
            reply,
            reply,
            32 * 1024,
            32 * 1024,
        ];
        let bytes = values
            .into_iter()
            .try_fold(0usize, |sum, n| sum.checked_add(n))
            .context("migration bookkeeping overflow")?;
        Ok(u64::try_from(bytes)?)
    }
    fn reap(entry: &mut Entry) -> Result<()> {
        let EntryState::Active { job, thread } = &mut entry.state else {
            return Ok(());
        };
        if !thread.as_ref().is_some_and(JoinHandle::is_finished) {
            return Ok(());
        }
        // is_finished proves join cannot wait on active native work. A panic is
        // not a completed operation: retain the slot and refuse target reuse.
        let outcome = thread
            .take()
            .context("migration join already attempted")?
            .join();
        match outcome {
            Ok(drained) => {
                let r = job.report.lock().unwrap();
                entry.catalog = r.catalog.clone();
                let progress = r.progress.clone();
                drop(r);
                entry.state = EntryState::Terminal { drained, progress };
                Ok(())
            }
            Err(_) => {
                job.pending(None);
                job.report.lock().unwrap().failure = Some(api::Failure {
                    code: ErrorCode::Native,
                    detail: "migration owner thread panicked; custody requires recovery".into(),
                    required: None,
                    available: None,
                    poisoned: true,
                    outcome_unknown: true,
                });
                anyhow::bail!("migration owner thread panicked; custody requires recovery")
            }
        }
    }
    pub(super) fn signal_shutdown(&self) {
        let mut slot = self.slot.lock().unwrap();
        if let Some(entry) = &mut *slot {
            if let EntryState::Active { job, .. } = &entry.state {
                job.cancel();
            } else if matches!(entry.state, EntryState::Upload(_)) {
                *slot = None;
            }
        }
    }
    pub(super) fn drained(&self) -> bool {
        let mut slot = self.slot.lock().unwrap();
        slot.as_mut().is_none_or(|entry| {
            Self::reap(entry).is_ok() && matches!(entry.state, EntryState::Terminal { .. })
        })
    }
    pub(super) fn before_catalog_request(
        &self,
        request: &application::Request,
    ) -> std::result::Result<(), BridgeError> {
        if !matches!(
            request,
            application::Request::Close { .. }
                | application::Request::Create { .. }
                | application::Request::OpenExisting { .. }
                | application::Request::BackupRestore { .. }
        ) {
            return Ok(());
        }
        let mut slot = self.slot.lock().unwrap();
        if let Some(entry) = &mut *slot {
            Self::reap(entry).map_err(bridge)?;
            if !matches!(entry.state, EntryState::Terminal { .. }) {
                if matches!(request, application::Request::Close { .. }) {
                    if let EntryState::Active { job, .. } = &entry.state {
                        job.cancel();
                    }
                }
                return Err(application::error(
                    ErrorCode::Busy,
                    "migration retained; cancel and retry Close after checked drain",
                ));
            }
        }
        Ok(())
    }
    pub(super) fn request(
        &self,
        request: api::Request,
    ) -> std::result::Result<api::Response, BridgeError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| application::error(ErrorCode::Closed, "desktop owner unavailable"))?;
        incoming(&request, shared.limits.request_bytes).map_err(|_| {
            application::error(ErrorCode::ResourceLimit, "migration request byte allowance")
        })?;
        let mut slot = self.slot.lock().unwrap();
        if let Some(entry) = &mut *slot {
            Self::reap(entry).map_err(bridge)?;
        }
        if let api::Request::Begin { operation, header } = request {
            let mut hash = blake3::Hasher::new();
            serde_json::to_writer(&mut hash, &header).map_err(|e| bridge(e.into()))?;
            let header_digest = hash.finalize();
            if let Some(entry) = &*slot {
                if entry.guard.operation == operation && entry.header_digest == header_digest {
                    return self
                        .snapshot(entry, shared.limits.reply_bytes)
                        .map(api::Response::Status)
                        .map_err(bridge);
                }
                return Err(application::error(
                    ErrorCode::Busy,
                    "discard the retained migration before replacement",
                ));
            }
            let state = shared.state.lock().unwrap();
            if state.stopping || !state.ready {
                return Err(application::error(
                    ErrorCode::Closed,
                    "desktop migration admission closed",
                ));
            }
            drop(state);
            let pool = self.pool.as_ref().ok_or_else(|| {
                application::error(
                    ErrorCode::InvalidRequest,
                    "migration requires managed desktop shared admission",
                )
            })?;
            let bytes = self
                .bookkeeping_bytes(&operation, shared.limits.reply_bytes)
                .map_err(bridge)?;
            let bookkeeping = pool
                .reserve_exact(bytes)
                .map_err(|e| application::error(ErrorCode::ResourceLimit, e.to_string()))?;
            #[cfg(test)]
            {
                self.faults.lock().unwrap().graphs_built += 1;
            }
            let guard = Guard {
                session: uuid::Uuid::from_bytes(shared.session).to_string(),
                generation: format!(
                    "{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                ),
                operation,
            };
            guard.validate().map_err(bridge)?;
            if header
                .catalog
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > 128)
            {
                return Err(application::error(
                    ErrorCode::InvalidRequest,
                    "migration catalog token bounds",
                ));
            }
            // Refuse an unusable response allowance before storing an upload.
            let minimal = api::Snapshot {
                guard: guard.clone(),
                phase: api::Phase::DrainPending,
                catalog: Some("x".repeat(128)),
                uploaded: U64(u64::MAX),
                next_role: Some(api::InputRole::ExecutionAuthorization),
                progress: Some(("\u{0000}".repeat(256), U64(u64::MAX), Some(U64(u64::MAX)))),
                failure: Some(api::Failure {
                    code: ErrorCode::ResourceLimit,
                    detail: String::new(),
                    required: Some(U64(u64::MAX)),
                    available: Some(U64(u64::MAX)),
                    poisoned: true,
                    outcome_unknown: true,
                }),
                result: Some(api::ResultIdentity {
                    bytes: U64(u64::MAX),
                    blake3: "f".repeat(64),
                    pages: U64(u64::MAX),
                }),
            };
            snapshot_bytes(&minimal, shared.limits.reply_bytes).map_err(|_| {
                application::error(
                    ErrorCode::ResourceLimit,
                    "migration reply allowance cannot preserve guarded status",
                )
            })?;
            drop(minimal);
            if header.timeout_ms.0 == 0
                || Instant::now()
                    .checked_add(Duration::from_millis(header.timeout_ms.0))
                    .is_none()
            {
                return Err(application::error(
                    ErrorCode::InvalidRequest,
                    "invalid migration deadline",
                ));
            }
            let header_bytes = measured(&header, shared.limits.request_bytes).map_err(bridge)?;
            let budget = MemoryBudget::from_parent(
                SharedAllocationGrant::new(pool.clone()).map_err(bridge)?,
            );
            let mut memory = budget.reservation();
            let backing =
                crate::lightroom_migration_worker::memory::core::worker_envelope(header_bytes)
                    .map_err(bridge)?;
            // Two exact acquisition requests survive their transient reply
            // slots (target + writer). Fund their owned backing before Act.
            let relay_backing = shared
                .limits
                .request_bytes
                .checked_mul(4)
                .and_then(|bytes| {
                    shared
                        .limits
                        .reply_bytes
                        .checked_mul(2)
                        .and_then(|reply| bytes.checked_add(reply))
                });
            memory
                .grow(
                    relay_backing
                        .and_then(|bytes| backing.checked_add(bytes))
                        .ok_or_else(|| {
                            application::error(
                                ErrorCode::ResourceLimit,
                                "migration backing overflow",
                            )
                        })?,
                )
                .map_err(bridge)?;
            let envelope = worker::Envelope {
                protocol: 1,
                build: worker::build_identity().into(),
                target_token: guard.generation.clone(),
                destination: header.destination.clone(),
                expected_destination: None,
                protected: vec![],
                parts: header.parts.clone(),
                operation: header.operation.clone(),
            };
            envelope.validate().map_err(bridge)?;
            drop(envelope);
            let total = header
                .parts
                .iter()
                .try_fold(0usize, |n, p| {
                    n.checked_add(usize::try_from(p.bytes.0).ok()?)
                })
                .ok_or_else(|| {
                    application::error(ErrorCode::ResourceLimit, "migration upload sum overflow")
                })?;
            memory.grow(total).map_err(bridge)?;
            let texts = header
                .parts
                .iter()
                .map(|p| String::with_capacity(p.bytes.0 as usize))
                .collect();
            let catalog = header.catalog.clone();
            *slot = Some(Entry {
                guard,
                header_digest,
                catalog,
                uploaded: 0,
                state: EntryState::Upload(Upload {
                    header,
                    texts,
                    next: 0,
                    hash: blake3::Hasher::new(),
                    budget,
                    _memory: memory,
                }),
                _bookkeeping: Arc::new(bookkeeping),
            });
            return self
                .snapshot(slot.as_ref().unwrap(), shared.limits.reply_bytes)
                .map(api::Response::Status)
                .map_err(bridge);
        }
        let guard = match &request {
            api::Request::Begin { .. } => unreachable!(),
            api::Request::Upload { guard, .. }
            | api::Request::Finish { guard, .. }
            | api::Request::Act { guard }
            | api::Request::Status { guard }
            | api::Request::Cancel { guard }
            | api::Request::RetryDrain { guard }
            | api::Request::ResultPage { guard, .. }
            | api::Request::Discard { guard } => guard,
        };
        let entry = slot.as_mut().ok_or_else(|| {
            application::error(ErrorCode::StaleSession, "migration operation absent")
        })?;
        if entry.guard != *guard {
            return Err(application::error(
                ErrorCode::StaleSession,
                "migration guard changed",
            ));
        }
        match request {
            api::Request::Begin { .. } => unreachable!(),
            api::Request::Upload {
                role, offset, text, ..
            } => {
                let EntryState::Upload(upload) = &mut entry.state else {
                    return Err(application::error(
                        ErrorCode::Busy,
                        "migration already acted",
                    ));
                };
                let part = upload.header.parts.get(upload.next).ok_or_else(|| {
                    application::error(ErrorCode::InvalidRequest, "all migration parts finished")
                })?;
                let target = &mut upload.texts[upload.next];
                if role != part.role
                    || offset.0 != target.len() as u64
                    || text.is_empty()
                    || text.len() > TEXT_CHUNK
                    || text.len() > part.bytes.0 as usize - target.len()
                {
                    return Err(application::error(
                        ErrorCode::InvalidRequest,
                        "migration part order, offset or length differs",
                    ));
                }
                upload.hash.update(text.as_bytes());
                target.push_str(&text);
                entry.uploaded += text.len() as u64;
            }
            api::Request::Finish { role, blake3, .. } => {
                let EntryState::Upload(upload) = &mut entry.state else {
                    return Err(application::error(
                        ErrorCode::Busy,
                        "migration already acted",
                    ));
                };
                let part = upload.header.parts.get(upload.next).ok_or_else(|| {
                    application::error(ErrorCode::InvalidRequest, "all migration parts finished")
                })?;
                if role != part.role
                    || blake3 != part.blake3
                    || upload.hash.finalize().to_hex().as_str() != blake3
                    || upload.texts[upload.next].len() as u64 != part.bytes.0
                {
                    return Err(application::error(
                        ErrorCode::InvalidRequest,
                        "migration part digest or length differs",
                    ));
                }
                upload.next += 1;
                upload.hash = blake3::Hasher::new();
            }
            api::Request::Act { .. } => {
                if !matches!(&entry.state, EntryState::Upload(_)) {
                    return self
                        .snapshot(entry, shared.limits.reply_bytes)
                        .map(api::Response::Status)
                        .map_err(bridge);
                }
                if !matches!(&entry.state, EntryState::Upload(upload) if upload.next == upload.header.parts.len())
                {
                    return Err(application::error(
                        ErrorCode::Busy,
                        "migration needs complete uploads or already acted",
                    ));
                }
                let job = Arc::new(Job {
                    stop: Arc::new(Stop::default()),
                    report: Mutex::new(Report {
                        phase: api::Phase::Running,
                        catalog: entry.catalog.clone(),
                        progress: None,
                        failure: None,
                        retry: 0,
                    }),
                    wake: Condvar::new(),
                    #[cfg(test)]
                    faults: self.faults.clone(),
                });
                *shared.migration_stop.lock().unwrap() = Some(Arc::downgrade(&job.stop));
                // Move input owners only after the OS thread exists. Failed spawn
                // leaves the complete upload available for same-pool retry.
                let (tx, rx) = mpsc::sync_channel::<Upload>(1);
                let worker_job = job.clone();
                let bookkeeping = entry._bookkeeping.clone();
                let executable = self.executable.clone();
                let guard = entry.guard.clone();
                let weak = self.shared.clone();
                let pool = self.pool.as_ref().unwrap().clone();
                let thread = thread::Builder::new()
                    .name("lightroom-migration-owner".into())
                    .spawn(move || {
                        let upload = rx.recv().expect("admitted migration upload owner");
                        let _bookkeeping = bookkeeping;
                        run(upload, worker_job, weak, executable, guard, pool)
                    })
                    .map_err(|e| application::error(ErrorCode::Native, e.to_string()))?;
                let old = std::mem::replace(
                    &mut entry.state,
                    EntryState::Active {
                        job,
                        thread: Some(thread),
                    },
                );
                let EntryState::Upload(upload) = old else {
                    unreachable!()
                };
                tx.send(upload)
                    .expect("new migration owner receives upload");
            }
            api::Request::Cancel { .. } => match &entry.state {
                EntryState::Active { job, .. } => job.cancel(),
                EntryState::Upload(_) => {
                    let guard = entry.guard.clone();
                    *slot = None;
                    return Ok(api::Response::Discarded { guard });
                }
                EntryState::Terminal { .. } => {}
            },
            api::Request::RetryDrain { .. } => {
                if let EntryState::Active { job, .. } = &entry.state {
                    job.retry();
                }
            }
            api::Request::Discard { .. } => {
                if matches!(entry.state, EntryState::Active { .. }) {
                    return Err(application::error(
                        ErrorCode::Busy,
                        "migration drain and join remain pending",
                    ));
                }
                let guard = entry.guard.clone();
                *slot = None;
                return Ok(api::Response::Discarded { guard });
            }
            api::Request::ResultPage {
                page,
                offset,
                maximum_bytes,
                ..
            } => {
                let EntryState::Terminal {
                    drained: Drained::Complete(result),
                    ..
                } = &entry.state
                else {
                    return Err(application::error(
                        ErrorCode::Busy,
                        "migration result requires checked completion",
                    ));
                };
                let text = result
                    .page(usize::try_from(page.0).map_err(|_| {
                        application::error(ErrorCode::InvalidRequest, "result page overflow")
                    })?)
                    .ok_or_else(|| {
                        application::error(ErrorCode::InvalidRequest, "result page absent")
                    })?;
                let start = usize::try_from(offset.0).map_err(|_| {
                    application::error(ErrorCode::InvalidRequest, "result offset overflow")
                })?;
                let maximum = usize::try_from(maximum_bytes.0)
                    .map_err(|_| {
                        application::error(ErrorCode::InvalidRequest, "result window overflow")
                    })?
                    .min(TEXT_CHUNK);
                if maximum == 0 || start > text.len() || !text.is_char_boundary(start) {
                    return Err(application::error(
                        ErrorCode::InvalidRequest,
                        "invalid result window",
                    ));
                }
                let mut end = start.saturating_add(maximum).min(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                if end == start && start != text.len() {
                    return Err(application::error(
                        ErrorCode::InvalidRequest,
                        "result window smaller than UTF-8 character",
                    ));
                }
                let response = api::Response::Page {
                    guard: entry.guard.clone(),
                    page,
                    offset,
                    text: text[start..end].into(),
                    next_offset: (end < text.len()).then_some(U64(end as u64)),
                };
                outgoing(&response, shared.limits.reply_bytes).map_err(|_| {
                    application::error(ErrorCode::ResourceLimit, "result page response allowance")
                })?;
                return Ok(response);
            }
            api::Request::Status { .. } => {}
        }
        self.snapshot(entry, shared.limits.reply_bytes)
            .map(api::Response::Status)
            .map_err(bridge)
    }
    fn snapshot(&self, entry: &Entry, limit: usize) -> Result<api::Snapshot> {
        let (phase, next_role, progress, failed, result, catalog) = match &entry.state {
            EntryState::Upload(upload) => (
                (if upload.next == upload.header.parts.len() {
                    api::Phase::Ready
                } else {
                    api::Phase::Uploading
                }),
                upload.header.parts.get(upload.next).map(|p| p.role),
                None,
                None,
                None,
                entry.catalog.clone(),
            ),
            EntryState::Active { job, .. } => {
                let r = job.report.lock().unwrap();
                (
                    r.phase.clone(),
                    None,
                    r.progress.clone(),
                    r.failure.clone(),
                    None,
                    r.catalog.clone(),
                )
            }
            EntryState::Terminal { drained, progress } => match drained {
                Drained::Complete(result) => (
                    api::Phase::Complete,
                    None,
                    progress.clone(),
                    None,
                    result
                        .identity()
                        .map(|(bytes, digest)| api::ResultIdentity {
                            bytes: U64(bytes as u64),
                            blake3: digest.into(),
                            pages: U64(result.page_count() as u64),
                        }),
                    entry.catalog.clone(),
                ),
                Drained::Failed(failed) => (
                    api::Phase::Failed,
                    None,
                    progress.clone(),
                    Some(failure(failed)),
                    None,
                    entry.catalog.clone(),
                ),
            },
        };
        let mut value = api::Snapshot {
            guard: entry.guard.clone(),
            phase,
            catalog,
            uploaded: U64(entry.uploaded),
            next_role,
            progress,
            failure: failed,
            result,
        };
        loop {
            if snapshot_bytes(&value, limit).is_ok() {
                return Ok(value);
            }
            let detail = &mut value
                .failure
                .as_mut()
                .context("migration guarded status reply allowance")?
                .detail;
            ensure!(
                !detail.is_empty(),
                "migration guarded failure reply allowance"
            );
            let mut end = detail.len() / 2;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Submission {
    #[default]
    Never,
    Submitted(blake3::Hash),
    Acknowledged(blake3::Hash),
    Refused(blake3::Hash),
}
#[derive(Default)]
struct Acquisition {
    state: Submission,
    // The owner outlives Waiting and the local external-admission waiter. Its
    // immutable request is the source for exact recovery even after ACK loss.
    request: Option<relay::Request>,
}
impl Acquisition {
    fn recovery(&self) -> Result<relay::Request> {
        let request = self
            .request
            .as_ref()
            .context("acquisition request unavailable")?;
        let Submission::Submitted(digest) = self.state else {
            anyhow::bail!("acquisition recovery without submitted identity")
        };
        ensure!(
            blake3::hash(&serde_json::to_vec(request)?) == digest,
            "retained acquisition request identity changed"
        );
        let action = match &request.action {
            relay::Action::AcquireTarget { .. } => relay::Action::RecoverTarget {
                acquire_digest: digest.to_hex().to_string(),
            },
            relay::Action::AcquireWrite {
                sequence,
                kind,
                request_digest,
                ..
            } => relay::Action::RecoverWrite {
                sequence: *sequence,
                kind: *kind,
                request_digest: request_digest.clone(),
            },
            _ => anyhow::bail!("retained request is not an acquisition"),
        };
        Ok(relay::Request {
            guard: request.guard.clone(),
            action,
        })
    }
}
struct Waiting {
    digest: blake3::Hash,
    pending: relay::Pending,
    submission: Option<Arc<Mutex<Acquisition>>>,
    first_submission: bool,
}
struct Proxy {
    shared: Weak<Shared>,
    client: relay::Client,
    guard: Guard,
    job: Arc<Job>,
    authority: Mutex<Option<Waiting>>,
    recovery: Mutex<Option<Waiting>>,
    pin: Mutex<Option<DestinationPin>>,
}
impl Proxy {
    fn reaped(&self) -> bool {
        self.shared
            .upgrade()
            .is_some_and(|s| s.state.lock().unwrap().reaped)
    }
    fn stopping(&self) -> bool {
        self.shared
            .upgrade()
            .is_none_or(|s| s.state.lock().unwrap().stopping)
    }
    fn call(&self, action: relay::Action, until: Instant) -> Result<relay::Snapshot> {
        self.call_recorded(action, until, None)
    }
    fn call_recorded(
        &self,
        action: relay::Action,
        until: Instant,
        submission: Option<&Arc<Mutex<Acquisition>>>,
    ) -> Result<relay::Snapshot> {
        let recovery = action.recovery();
        let request = relay::Request {
            guard: self.guard.clone(),
            action,
        };
        let digest = blake3::hash(&serde_json::to_vec(&request)?);
        let mut lane = if recovery {
            self.recovery.lock().unwrap()
        } else {
            self.authority.lock().unwrap()
        };
        loop {
            ensure!(
                !self.reaped(),
                "catalog child reaped; prior migration outcome may be unknown"
            );
            if !recovery && self.job.stop.requested() {
                if let Some(waiting) = &*lane {
                    waiting.pending.cancel.cancel();
                }
                return Err(supervisor::OperationCanceled.into());
            }
            ensure!(
                Instant::now() < until,
                "catalog migration acknowledgement remains pending"
            );
            if lane.is_none() {
                let first_submission = if let Some(state) = submission {
                    let state = state.lock().unwrap();
                    ensure!(
                        state.state == Submission::Never
                            || matches!(state.state, Submission::Submitted(d) if d == digest),
                        "migration submission identity changed"
                    );
                    state.state == Submission::Never
                } else {
                    true
                };
                #[cfg(not(test))]
                let pending = self
                    .client
                    .submit(request.clone())
                    .map_err(anyhow::Error::new)?;
                #[cfg(test)]
                let pending = self
                    .client
                    .submit_observed(request.clone(), |cancel| {
                        let pause = {
                            let faults = self.job.faults.lock().unwrap();
                            matches!(request.action, relay::Action::AcquireTarget { .. })
                                && faults.enqueue_target
                                || matches!(request.action, relay::Action::AcquireWrite { .. })
                                    && faults.enqueue_writer
                        };
                        if pause {
                            self.job.faults.lock().unwrap().enqueued = true;
                            while !self.job.stop.requested() && Instant::now() < until {
                                thread::sleep(Duration::from_millis(2));
                            }
                            // Still under G queue lock: C sees the real early Cancel
                            // before dispatching this already-enqueued request.
                            cancel.cancel();
                        }
                    })
                    .map_err(anyhow::Error::new)?;
                if let Some(state) = submission {
                    let mut state = state.lock().unwrap();
                    if first_submission {
                        state.request = Some(request.clone());
                    }
                    state.state = Submission::Submitted(digest);
                }
                *lane = Some(Waiting {
                    digest,
                    pending,
                    submission: submission.cloned(),
                    first_submission,
                });
            }
            let waiting = lane.as_ref().unwrap();
            match waiting
                .pending
                .receiver
                .recv_timeout(Duration::from_millis(10))
            {
                Ok(reply) => {
                    let same = waiting.digest == digest;
                    let recorded = waiting.submission.clone();
                    let received_digest = waiting.digest;
                    let first_submission = waiting.first_submission;
                    *lane = None;
                    if !same {
                        self.record_reply(
                            recorded.as_ref(),
                            received_digest,
                            first_submission,
                            &reply,
                        )?;
                        continue;
                    }
                    #[cfg(test)]
                    let reply = {
                        let mut faults = self.job.faults.lock().unwrap();
                        let writer = matches!(&request.action, relay::Action::AcquireWrite { kind, .. }
                            if !faults.catalog_acquire_faults_only || *kind == WriteKind::Catalog);
                        let target = matches!(&request.action, relay::Action::AcquireTarget { .. });
                        let count = match &request.action {
                            relay::Action::AcquireWrite { .. } if writer => {
                                &mut faults.lost_acquire
                            }
                            relay::Action::AcquireTarget { .. } => &mut faults.lost_target,
                            relay::Action::ReleaseWrite { .. } => &mut faults.lost_release,
                            _ => &mut 0,
                        };
                        let lost = *count != 0;
                        if lost {
                            *count -= 1;
                            faults.lost += 1;
                        }
                        let unknown = if target {
                            &mut faults.unknown_target
                        } else if writer {
                            &mut faults.unknown_write
                        } else {
                            &mut 0
                        };
                        let substitute = *unknown != 0;
                        *unknown = unknown.saturating_sub(1);
                        let disconnect = if target {
                            &mut faults.disconnect_target
                        } else if writer {
                            &mut faults.disconnect_write
                        } else {
                            &mut 0
                        };
                        let disconnected = *disconnect != 0;
                        *disconnect = disconnect.saturating_sub(1);
                        let acquisition_fault =
                            (target || writer) && (lost || substitute || disconnected);
                        if acquisition_fault {
                            assert!(
                                matches!(reply, relay::Reply::Ok(_)),
                                "fault follows actual C action"
                            );
                            faults.acquire_fault_seen = true;
                        }
                        drop(faults);
                        if disconnected {
                            // C has really acted. Replace its consumed ACK with
                            // a genuinely disconnected local receiver, retaining
                            // the original slot to exercise absent's disconnect.
                            let (tx, receiver) = mpsc::sync_channel(1);
                            drop(tx);
                            *lane = Some(Waiting {
                                digest: received_digest,
                                pending: relay::Pending {
                                    receiver,
                                    cancel: Default::default(),
                                },
                                submission: recorded.clone(),
                                first_submission,
                            });
                        }
                        while acquisition_fault
                            && self.job.faults.lock().unwrap().pause_after_acquire_fault
                            && !self.job.stop.requested()
                        {
                            thread::sleep(Duration::from_millis(2));
                        }
                        if lost || disconnected {
                            continue;
                        }
                        if substitute {
                            // Exercise the real post-action bounded-reply
                            // producer, never an error-code approximation.
                            serde_json::from_slice::<relay::Reply>(&reply.message(1, 1).bytes)?
                        } else {
                            reply
                        }
                    };
                    self.record_reply(
                        recorded.as_ref(),
                        received_digest,
                        first_submission,
                        &reply,
                    )?;
                    let snapshot = match reply {
                        relay::Reply::Ok(snapshot) => snapshot,
                        relay::Reply::Refused(error) | relay::Reply::Error(error) => {
                            if matches!(error.code, ErrorCode::Canceled) {
                                return Err(supervisor::OperationCanceled.into());
                            }
                            return Err(error.into());
                        }
                    };
                    ensure!(
                        snapshot.guard == self.guard,
                        "catalog migration reply guard changed"
                    );
                    if let Some(pin) = &snapshot.destination {
                        *self.pin.lock().unwrap() = Some(pin.clone());
                    }
                    if let Some(catalog) = &snapshot.catalog {
                        self.job.report.lock().unwrap().catalog = Some(catalog.clone());
                    }
                    return Ok(snapshot);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    *lane = None;
                    anyhow::bail!("catalog migration acknowledgement lost")
                }
            }
        }
    }
    fn record_reply(
        &self,
        submission: Option<&Arc<Mutex<Acquisition>>>,
        digest: blake3::Hash,
        first_submission: bool,
        reply: &relay::Reply,
    ) -> Result<()> {
        if let Some(state) = submission {
            let mut state = state.lock().unwrap();
            ensure!(
                state.state == Submission::Submitted(digest),
                "migration acknowledgement identity changed"
            );
            state.state = match reply {
                relay::Reply::Ok(snapshot) => {
                    ensure!(
                        snapshot.guard == self.guard,
                        "migration acknowledgement guard changed"
                    );
                    Submission::Acknowledged(digest)
                }
                relay::Reply::Refused(_) if first_submission => Submission::Refused(digest),
                // Refusing a replay does not prove the original request absent.
                relay::Reply::Refused(_) | relay::Reply::Error(_) => Submission::Submitted(digest),
            };
        }
        Ok(())
    }
    /// Only an exact terminal acquisition refusal, or no successful enqueue,
    /// proves remote authority absent. Unknown/Status/missing writer never do.
    fn absent(&self, submission: &Arc<Mutex<Acquisition>>, until: Instant) -> Result<bool> {
        loop {
            match submission.lock().unwrap().state {
                Submission::Never | Submission::Refused(_) => return Ok(true),
                Submission::Acknowledged(_) => return Ok(false),
                Submission::Submitted(_) => {}
            }
            ensure!(
                Instant::now() < until,
                "acquisition acknowledgement still unknown"
            );
            #[cfg(test)]
            {
                let mut faults = self.job.faults.lock().unwrap();
                faults.acquire_recovery_seen = true;
                ensure!(
                    !faults.pause_acquire_recovery,
                    "injected pause before exact acquisition recovery"
                );
            }
            let mut lane = self.authority.lock().unwrap();
            if lane.is_none() {
                let (request, digest) = {
                    let acquisition = submission.lock().unwrap();
                    let Submission::Submitted(digest) = acquisition.state else {
                        anyhow::bail!("acquisition state changed during recovery")
                    };
                    (acquisition.recovery()?, digest)
                };
                // This lookup is admitted during C shutdown and cannot acquire
                // fresh authority. Refusal/missing state leaves the original
                // uncertainty intact; only an exact snapshot permits release.
                let pending = self.client.submit(request).map_err(anyhow::Error::new)?;
                *lane = Some(Waiting {
                    digest,
                    pending,
                    submission: Some(submission.clone()),
                    first_submission: false,
                });
            }
            let waiting = lane.as_ref().unwrap();
            ensure!(
                waiting
                    .submission
                    .as_ref()
                    .is_some_and(|s| Arc::ptr_eq(s, submission)),
                "different acquisition acknowledgement retained"
            );
            if waiting.first_submission {
                waiting.pending.cancel.cancel();
            }
            match waiting
                .pending
                .receiver
                .recv_timeout(Duration::from_millis(10))
            {
                Ok(reply) => {
                    self.record_reply(
                        Some(submission),
                        waiting.digest,
                        waiting.first_submission,
                        &reply,
                    )?;
                    *lane = None;
                    if matches!(submission.lock().unwrap().state, Submission::Submitted(_)) {
                        anyhow::bail!(
                            "acquisition acknowledgement still unknown after exact recovery"
                        )
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    *lane = None;
                    anyhow::bail!("acquisition acknowledgement lost")
                }
            }
        }
    }
    fn acquire(
        &self,
        action: relay::Action,
        sequence: u64,
        kind: WriteKind,
        digest: &str,
        until: Instant,
        submission: &Arc<Mutex<Acquisition>>,
    ) -> Result<()> {
        let mut snapshot = self.call_recorded(action, until, Some(submission))?;
        loop {
            ensure!(
                snapshot.sequence == Some(U64(sequence))
                    && snapshot.write_kind == Some(kind)
                    && snapshot.request_digest.as_deref() == Some(digest),
                "catalog writer attempt changed"
            );
            if let Some(failure) = snapshot.failure {
                return Err(failure.into());
            }
            if snapshot.phase == relay::Phase::Held {
                #[cfg(test)]
                if kind == WriteKind::Catalog {
                    loop {
                        let paused = {
                            let mut faults = self.job.faults.lock().unwrap();
                            faults.writer_held = true;
                            faults.pause_writer
                        };
                        if !paused {
                            break;
                        }
                        if self.job.stop.requested() {
                            return Err(supervisor::OperationCanceled.into());
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                }
                return Ok(());
            }
            ensure!(
                matches!(snapshot.phase, relay::Phase::Attempted),
                "catalog writer was not granted"
            );
            if self.job.stop.requested() {
                return Err(supervisor::OperationCanceled.into());
            }
            thread::sleep(Duration::from_millis(2));
            snapshot = self.call(relay::Action::Status, until)?;
        }
    }
}
struct WriterProxy {
    proxy: Arc<Proxy>,
    action: relay::Action,
    sequence: u64,
    kind: WriteKind,
    digest: String,
    until: Instant,
    submission: Arc<Mutex<Acquisition>>,
}
struct Lease;
impl ExternalLease for Lease {
    fn release(&mut self) {
        // The actual C release is checked by Admission::release, after this G
        // thread-affine Permit has dropped. This token cannot retire C custody.
    }
}
impl ExternalAdmission for WriterProxy {
    fn acquire(&self) -> Result<Box<dyn ExternalLease>> {
        self.proxy.acquire(
            self.action.clone(),
            self.sequence,
            self.kind,
            &self.digest,
            self.until,
            &self.submission,
        )?;
        Ok(Box::new(Lease))
    }
}
struct WriteAttempt {
    sequence: u64,
    kind: WriteKind,
    digest: String,
    submission: Arc<Mutex<Acquisition>>,
}
struct CatalogAdmission {
    proxy: Arc<Proxy>,
    attempt: Option<WriteAttempt>,
}
impl Admission for CatalogAdmission {
    #[cfg(test)]
    fn fail_waiter_spawn(&mut self) -> bool {
        std::mem::take(&mut self.proxy.job.faults.lock().unwrap().fail_waiter_spawn)
    }
    fn lock(&mut self, target: &str, pin: &DestinationPin, _: &FileKey) -> Result<()> {
        ensure!(
            target == self.proxy.guard.generation
                && self.proxy.pin.lock().unwrap().as_ref() == Some(pin),
            "executor destination differs from C admission"
        );
        Ok(())
    }
    fn writer(
        &mut self,
        sequence: u64,
        kind: WriteKind,
        target: &str,
        lock: Option<&FileKey>,
        _: &Stop,
        until: Instant,
    ) -> Result<Arc<Writers>> {
        let digest = relay::write_digest(&self.proxy.guard, U64(sequence), kind, target, lock)
            .map_err(anyhow::Error::new)?;
        let submission = Arc::new(Mutex::new(Acquisition::default()));
        self.attempt = Some(WriteAttempt {
            sequence,
            kind,
            digest: digest.clone(),
            submission: submission.clone(),
        });
        #[cfg(test)]
        loop {
            let paused = {
                let mut faults = self.proxy.job.faults.lock().unwrap();
                faults.external_pending = true;
                faults.pause_external
            };
            if !paused || self.proxy.job.stop.requested() || Instant::now() >= until {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let action = relay::Action::AcquireWrite {
            sequence: U64(sequence),
            kind,
            target: target.into(),
            lock: lock.cloned(),
            request_digest: digest.clone(),
        };
        Ok(Writers::with_external(Arc::new(WriterProxy {
            proxy: self.proxy.clone(),
            action,
            sequence,
            kind,
            digest,
            until,
            submission,
        })))
    }
    fn release(&mut self, sequence: u64, kind: WriteKind) -> Result<()> {
        // Synchronous compatibility adapter. The supervisor uses poll_release
        // and retains normal pending progress in its own event loop.
        loop {
            if self.poll_release(sequence, kind)? == supervisor::ReleaseProgress::Released {
                return Ok(());
            }
            let report = self.proxy.job.report.lock().unwrap();
            drop(
                self.proxy
                    .job
                    .wake
                    .wait_timeout(report, Duration::from_millis(20))
                    .unwrap(),
            );
        }
    }
    fn poll_release(
        &mut self,
        sequence: u64,
        kind: WriteKind,
    ) -> Result<supervisor::ReleaseProgress> {
        let attempt = self
            .attempt
            .as_ref()
            .context("catalog writer release without attempt")?;
        ensure!(
            (attempt.sequence, attempt.kind) == (sequence, kind),
            "catalog writer release changed"
        );
        if self.proxy.reaped() {
            return Ok(supervisor::ReleaseProgress::Released);
        }
        // Admission::release is called only after the local waiter/permit has
        // joined; external acquisition cannot enqueue behind this proof.
        if self.proxy.absent(
            &attempt.submission,
            Instant::now() + Duration::from_millis(100),
        )? {
            return Ok(supervisor::ReleaseProgress::Released);
        }
        let digest = &attempt.digest;
        let snapshot = self.proxy.call(
            relay::Action::ReleaseWrite {
                sequence: U64(sequence),
                kind,
                request_digest: digest.clone(),
            },
            Instant::now() + Duration::from_millis(100),
        )?;
        ensure!(
            snapshot.sequence == Some(U64(sequence))
                && snapshot.write_kind == Some(kind)
                && snapshot.request_digest.as_deref() == Some(digest),
            "catalog release acknowledgement changed"
        );
        match snapshot.phase {
            relay::Phase::Released | relay::Phase::Drained => {
                Ok(supervisor::ReleaseProgress::Released)
            }
            // C has requested release but has not yet confirmed owner join. A
            // retained acquisition failure can also precede that checked join.
            relay::Phase::Releasing | relay::Phase::Failed => {
                Ok(supervisor::ReleaseProgress::Pending)
            }
            _ => anyhow::bail!("catalog writer release acknowledgement phase changed"),
        }
    }
    fn progress(&mut self, phase: &str, completed: u64, total: Option<u64>) -> Result<()> {
        self.proxy.job.report.lock().unwrap().progress =
            Some((phase.into(), U64(completed), total.map(U64)));
        self.proxy.call(
            relay::Action::Progress {
                phase: phase.into(),
                completed: U64(completed),
                total: total.map(U64),
            },
            Instant::now() + Duration::from_secs(1),
        )?;
        Ok(())
    }
}
fn run(
    upload: Upload,
    job: Arc<Job>,
    shared: Weak<Shared>,
    executable: PathBuf,
    guard: Guard,
    pool: ByteBudget,
) -> Drained {
    let result_budget = MemoryBudget::from_shared(pool);
    let proxy = Arc::new(Proxy {
        client: relay::Client::new(&shared.upgrade().expect("live desktop migration owner")),
        shared,
        guard: guard.clone(),
        job: job.clone(),
        authority: Mutex::new(None),
        recovery: Mutex::new(None),
        pin: Mutex::new(None),
    });
    let until = Instant::now()
        .checked_add(Duration::from_millis(upload.header.timeout_ms.0))
        .expect("admitted migration deadline");
    let target_submission = Arc::new(Mutex::new(Acquisition::default()));
    let setup = (|| -> Result<(String, usize)> {
        let expected = if let Some(catalog) = &upload.header.catalog {
            Some(
                proxy
                    .call(
                        relay::Action::InspectTarget {
                            catalog: catalog.clone(),
                            destination: upload.header.destination.clone(),
                        },
                        until,
                    )?
                    .destination
                    .context("C inspection omitted destination")?,
            )
        } else {
            None
        };
        #[cfg(test)]
        loop {
            let pause = {
                let mut faults = job.faults.lock().unwrap();
                faults.target_before_submit = true;
                faults.pause_target
            };
            if !pause || job.stop.requested() || Instant::now() >= until {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let target = proxy.call_recorded(
            relay::Action::AcquireTarget {
                catalog: upload.header.catalog.clone(),
                destination: upload.header.destination.clone(),
                expected,
            },
            until,
            Some(&target_submission),
        )?;
        ensure!(
            target.phase == relay::Phase::Target,
            "catalog target not acquired"
        );
        let envelope = worker::Envelope {
            protocol: 1,
            build: worker::build_identity().into(),
            target_token: guard.generation.clone(),
            destination: upload.header.destination.clone(),
            expected_destination: target.destination,
            protected: vec![],
            parts: upload.header.parts.clone(),
            operation: upload.header.operation.clone(),
        };
        envelope.validate()?;
        let maximum = envelope.operation.result_maximum()?;
        let limit = proxy
            .shared
            .upgrade()
            .context("desktop migration owner absent")?
            .limits
            .request_bytes;
        measured(&envelope, limit)?;
        Ok((serde_json::to_string(&envelope)?, maximum))
    })();
    let mut drained = match setup {
        Err(error) => Drained::Failed(supervisor::Failure::from_error(
            error,
            false,
            matches!(
                target_submission.lock().unwrap().state,
                Submission::Submitted(_)
            ),
            &result_budget,
        )),
        Ok((header, maximum)) => {
            let parts: Vec<_> = upload
                .header
                .parts
                .iter()
                .zip(&upload.texts)
                .map(|(part, text)| supervisor::InputPart {
                    role: part.role,
                    text,
                })
                .collect();
            let mut operation = supervisor::execute_operation_with_result_budget(
                &executable,
                guard,
                &header,
                job.stop.clone(),
                until,
                CatalogAdmission {
                    proxy: proxy.clone(),
                    attempt: None,
                },
                upload.budget.clone(),
                result_budget.clone(),
                &parts,
                maximum,
            );
            loop {
                #[cfg(test)]
                {
                    let mut faults = job.faults.lock().unwrap();
                    if faults.wait_failures != 0 {
                        faults.wait_injected = operation.inject_wait_failures(faults.wait_failures);
                        faults.wait_failures = 0;
                    }
                }
                if proxy.stopping() {
                    job.stop.cancel();
                }
                #[cfg(test)]
                if operation.receiving_result() {
                    let paused = {
                        let mut faults = job.faults.lock().unwrap();
                        faults.result_seen = true;
                        faults.pause_result
                    };
                    if paused && !job.stop.requested() {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                }
                if operation.retry_drain().is_some() {
                    break;
                }
                if let supervisor::Operation::DrainPending(pending) = &operation {
                    job.pending(pending.failure());
                    if pending.retry_failed() && !proxy.stopping() {
                        let mut report = job.report.lock().unwrap();
                        let retry = report.retry;
                        while report.retry == retry && !proxy.stopping() {
                            report = job
                                .wake
                                .wait_timeout(report, Duration::from_millis(20))
                                .unwrap()
                                .0;
                        }
                    }
                }
                let report = job.report.lock().unwrap();
                drop(
                    job.wake
                        .wait_timeout(report, Duration::from_millis(10))
                        .unwrap(),
                );
            }
            match operation {
                supervisor::Operation::Drained(drained) => drained,
                _ => unreachable!(),
            }
        }
    };
    // Exact uploaded documents and all parent-granted charges remain owned here
    // after LM/Source drain until C confirms target/permit retirement.
    {
        loop {
            #[cfg(test)]
            if job.faults.lock().unwrap().pause_drain && !proxy.stopping() {
                job.faults.lock().unwrap().drain_paused = true;
                job.pending(match &drained {
                    Drained::Failed(failure) => Some(failure),
                    _ => None,
                });
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            if proxy.reaped() {
                break;
            }
            match proxy.absent(
                &target_submission,
                Instant::now() + Duration::from_millis(100),
            ) {
                Ok(true) => break,
                Ok(false) => {}
                Err(_) => {
                    job.pending(match &drained {
                        Drained::Failed(f) => Some(f),
                        _ => None,
                    });
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
            }
            match proxy.call(
                relay::Action::DrainOperation,
                Instant::now() + Duration::from_millis(100),
            ) {
                Ok(snapshot) if snapshot.phase == relay::Phase::Drained => break,
                _ => {
                    job.pending(match &drained {
                        Drained::Failed(failure) => Some(failure),
                        _ => None,
                    });
                    let report = job.report.lock().unwrap();
                    drop(
                        job.wake
                            .wait_timeout(report, Duration::from_millis(20))
                            .unwrap(),
                    );
                }
            }
        }
    }
    if proxy.reaped() {
        drained = match drained {
            Drained::Failed(mut failure) => {
                failure.poisoned = true;
                failure.outcome_unknown = true;
                Drained::Failed(failure)
            }
            Drained::Complete(result) => {
                drop(result);
                Drained::Failed(supervisor::Failure::from_error(
                    anyhow::anyhow!("catalog process lost; inspect saved migration state"),
                    true,
                    true,
                    &result_budget,
                ))
            }
        };
    }
    job.pending(match &drained {
        Drained::Failed(failure) => Some(failure),
        _ => None,
    });
    drop(proxy);
    drop(upload);
    drained
}

#[cfg(test)]
mod tests;
