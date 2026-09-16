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
pub(super) mod backup;
#[allow(dead_code)] // Additive, unselected until the FS6 managed actor dependency is admitted.
mod filesystem;
pub(crate) use filesystem::admit_export_stage_reply;
#[cfg(test)]
pub(crate) use filesystem::roundtrip_export_stage;
mod export_native;
#[cfg(test)]
pub(crate) use export_native::tests::fixture as export_native_test_fixture;
#[cfg(test)]
mod filesystem_tests;
pub(crate) mod lightroom_migration;
mod migration;
mod native;
mod preview_metadata_admission;
pub(crate) mod preview_metadata_capacity;
mod process;
#[cfg(test)]
mod tests;
mod wire;
#[allow(dead_code)] // Selected by the managed public factory after integration gates.
pub(crate) mod workbench;
#[cfg(test)]
mod workbench_capability_tests;
use wire::{BytesRequest, Kind, Message};
type Result<T> = std::result::Result<T, BridgeError>;
pub(super) const CONTROL_SLOTS: usize = 16;

fn validate_public_request(request: &Request, limit: usize) -> Result<()> {
    struct Count {
        remaining: usize,
        exceeded: bool,
    }
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let Some(remaining) = self.remaining.checked_sub(bytes.len()) else {
                self.exceeded = true;
                return Err(std::io::Error::other("request byte limit"));
            };
            self.remaining = remaining;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // Reject before allocating an encoded copy of an oversized caller value.
    let mut count = Count {
        remaining: limit,
        exceeded: false,
    };
    let encoded = serde_json::to_writer(&mut count, request);
    if count.exceeded {
        return Err(error(ErrorCode::ResourceLimit, "request byte limit"));
    }
    encoded.map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))
}

/// Closed requires verified child/pipe drain and verified local Workbench drain.
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
    BackupAdmission(mpsc::SyncSender<backup::AdmissionReply>),
    Migration(mpsc::SyncSender<lightroom_migration::Reply>),
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
    child_finished: bool,
    local_verified: bool,
    filesystem_verified: bool,
    child_exit: Option<i32>,
    catalog_retiring: bool,
    catalog_epoch: u64,
    backup_admitting: bool,
    close_admitting: bool,
}
struct Shared {
    session: [u8; 16],
    limits: Limits,
    state: Mutex<State>,
    wake: Condvar,
    binary: Arc<AtomicUsize>,
    filesystem: Option<Arc<filesystem::Parent>>,
    /// Managed backup B/F run beneath G, never beneath the catalog child C.
    backup: Option<Arc<Mutex<super::backup::Coordinator>>>,
    metadata: preview_metadata_admission::ProcessReservation,
    migration_stop:
        Mutex<Option<std::sync::Weak<crate::lightroom_migration_worker::process::Stop>>>,
    #[cfg(test)]
    fixture: Mutex<Option<std::result::Result<String, String>>>,
}
impl Shared {
    fn child_finished(&self) {
        let mut state = self.state.lock().unwrap();
        state.child_finished = true;
        if state.local_verified && state.filesystem_verified {
            state.phase = TransportPhase::Closed;
        } else if state.phase != TransportPhase::Failed {
            state.phase = TransportPhase::Draining;
        }
        self.wake.notify_all();
    }
    fn drain_local(&self, drain: impl FnOnce() -> Result<()>) -> Result<()> {
        // Keep status observable while the independent local owner is blocked.
        let result = drain();
        let mut state = self.state.lock().unwrap();
        match &result {
            Err(error) => {
                state.local_verified = false;
                state.phase = TransportPhase::Draining;
                state.drain_error = Some(format!(
                    "local Workbench owner not drained: {}",
                    error.message
                ));
            }
            Ok(()) => {
                state.local_verified = true;
                if state.child_finished && state.filesystem_verified {
                    state.phase = TransportPhase::Closed;
                    state.drain_error = None;
                }
            }
        }
        self.wake.notify_all();
        result
    }
    fn fail(&self, message: impl Into<String>) {
        self.cancel_migration();
        if let Some(backup) = &self.backup {
            backup
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .signal_shutdown();
        }
        let message = message.into();
        if let Some(f) = &self.filesystem {
            f.fail(&message);
        }
        let mut s = self.state.lock().unwrap();
        if s.phase == TransportPhase::Closed {
            return;
        }
        s.unknown |= !s.pending.is_empty();
        s.message.get_or_insert(message);
        s.phase = TransportPhase::Failed;
        if !s.stopping {
            s.shutdown_attempt += 1;
        }
        s.stopping = true;
        self.wake.notify_all();
    }
    fn cancel_migration(&self) {
        if let Some(stop) = self
            .migration_stop
            .lock()
            .unwrap()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        {
            stop.cancel();
        }
    }
    fn stop(&self) {
        self.cancel_migration();
        if let Some(backup) = &self.backup {
            backup
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .signal_shutdown();
        }
        if let Some(f) = &self.filesystem {
            f.closing();
        }
        let mut s = self.state.lock().unwrap();
        if s.phase == TransportPhase::Closed {
            return;
        }
        if s.phase != TransportPhase::Failed {
            s.phase = TransportPhase::Draining;
        }
        if !s.stopping || s.drain_error.take().is_some() {
            s.shutdown_attempt += 1;
        }
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
        if attempt < s.shutdown_attempt {
            return Ok(());
        }
        if attempt == 0 || attempt != s.shutdown_attempt {
            return Err(wire::invalid("unowned drain response"));
        }
        s.drain_error = Some(message);
        s.phase = TransportPhase::Draining;
        self.wake.notify_all();
        Ok(())
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
                Delivery::BackupAdmission(tx) => {
                    let _ = tx.send(backup::AdmissionReply::error(error(
                        ErrorCode::Closed,
                        message,
                    )));
                }
                Delivery::Migration(tx) => {
                    let _ = tx.send(lightroom_migration::Reply::Error(error(
                        ErrorCode::Closed,
                        message,
                    )));
                }
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
enum LocalOwner {
    Legacy(Bridge),
    #[allow(dead_code)] // Selected after the managed startup funding is wired.
    Managed(workbench::Dispatcher),
}
impl LocalOwner {
    fn signal_shutdown(&self) {
        match self {
            Self::Legacy(bridge) => bridge
                .0
                .shared
                .lightroom
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .signal_shutdown(),
            Self::Managed(dispatcher) => dispatcher.signal_shutdown(),
        }
    }
    fn try_shutdown(&self) -> Result<()> {
        match self {
            Self::Legacy(bridge) => bridge.try_shutdown(),
            Self::Managed(dispatcher) => dispatcher.shutdown_checked(),
        }
    }
    fn legacy(&self) -> Result<&Bridge> {
        match self {
            Self::Legacy(bridge) => Ok(bridge),
            Self::Managed(_) => Err(error(
                ErrorCode::InvalidRequest,
                "request requires the managed Workbench route",
            )),
        }
    }
}

struct StartupOwners {
    local: LocalOwner,
    filesystem: Option<Arc<filesystem::Parent>>,
}
/// A failed startup whose exact child owners need another checked drain attempt.
/// Callers may downcast the startup error and retry without admitting replacements.
pub struct RetainedStartup {
    message: String,
    owners: Mutex<Option<StartupOwners>>,
}
impl std::fmt::Debug for RetainedStartup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedDesktopStartup")
            .field("message", &self.message)
            .finish()
    }
}
impl std::fmt::Display for RetainedStartup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; desktop startup owners retained", self.message)
    }
}
impl std::error::Error for RetainedStartup {}
impl RetainedStartup {
    pub fn try_shutdown(&self) -> anyhow::Result<()> {
        let mut slot = self.owners.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(owners) = slot.as_ref() {
            owners.local.try_shutdown()?;
            if let Some(filesystem) = &owners.filesystem {
                filesystem.finish_after_dependents(true)?;
            }
        }
        slot.take();
        Ok(())
    }
}
impl Drop for RetainedStartup {
    fn drop(&mut self) {
        if self.try_shutdown().is_err() {
            // Losing the error value is not proof of process retirement. Keep
            // both the exact owner graph and its aggregate allowance retained.
            std::mem::forget(
                self.owners
                    .get_mut()
                    .unwrap_or_else(|e| e.into_inner())
                    .take(),
            );
        }
    }
}
struct Handle {
    local: LocalOwner,
    shared: Arc<Shared>,
    process: Mutex<process::Owner>,
    pid: u32,
    shutdown: Mutex<()>,
    migration: migration::Coordinator,
    control_tasks: Mutex<ControlTasks>,
}
#[derive(Default)]
struct ControlTasks {
    // Fixed inline slots: no unbounded JoinHandle vector or metadata-pool
    // allocation. Each admitted task is joined before its slot can be reused.
    backup_admission: Option<thread::JoinHandle<()>>,
    close: Option<thread::JoinHandle<()>>,
}
pub(super) fn backup_control_tasks_layout() -> (usize, usize) {
    (
        std::mem::size_of::<Mutex<ControlTasks>>(),
        std::mem::align_of::<Mutex<ControlTasks>>(),
    )
}
#[derive(Clone, Copy)]
enum ControlTaskKind {
    BackupAdmission,
    Close,
}
/// This proof depends on process::paired_catalog_route and private migration
/// admission remaining a closed managed allowlist. Their identity-verified C
/// uses only thread owners; native OS children belong to G, never C. Ready binds
/// the complete implementation and the exact F binding. A legacy/unverified C
/// cannot use this abnormal-exit proof. F.finish_after_dependents still checks
/// G's native registry before retiring F, independently of G migration custody.
fn managed_catalog_retired(
    state: &State,
    paired: bool,
    migration_drained: bool,
    backup_drained: bool,
    control_drained: bool,
) -> bool {
    paired
        && state.ready
        && state.reaped
        && state.child_finished
        && migration_drained
        && backup_drained
        && control_drained
}
impl Handle {
    fn spawn_control_task(
        &self,
        kind: ControlTaskKind,
        name: &str,
        work: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        let mut tasks = self.control_tasks.lock().unwrap();
        let slot = match kind {
            ControlTaskKind::BackupAdmission => &mut tasks.backup_admission,
            ControlTaskKind::Close => &mut tasks.close,
        };
        if slot.as_ref().is_some_and(thread::JoinHandle::is_finished)
            && slot.take().unwrap().join().is_err()
        {
            return Err(error(
                ErrorCode::Native,
                "desktop backup control task panicked",
            ));
        }
        if slot.is_some() {
            return Err(error(
                ErrorCode::Busy,
                "desktop backup control task is already active",
            ));
        }
        *slot = Some(
            thread::Builder::new()
                .name(name.into())
                .spawn(work)
                .map_err(|failure| error(ErrorCode::Native, failure.to_string()))?,
        );
        Ok(())
    }
    fn join_control_tasks(&self) -> Result<()> {
        let (backup_admission, close) = {
            let mut tasks = self.control_tasks.lock().unwrap();
            (tasks.backup_admission.take(), tasks.close.take())
        };
        let mut panicked = false;
        for task in [backup_admission, close].into_iter().flatten() {
            if task.join().is_err() {
                panicked = true;
            }
        }
        if panicked {
            Err(error(
                ErrorCode::Native,
                "desktop backup control task panicked during drain",
            ))
        } else {
            Ok(())
        }
    }
    fn shutdown(&self) -> Result<()> {
        let _attempt = self.shutdown.lock().unwrap();
        // Signal both independent owners before either potentially blocking join.
        self.migration.signal_shutdown();
        self.shared.stop();
        self.local.signal_shutdown();
        let result = self.process.lock().unwrap().drain();
        let migration_drained = self.migration.drained();
        let backup_result = self.shared.backup.as_ref().map_or(Ok(()), |backup| {
            backup
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .shutdown()
                .map(|_| ())
                .map_err(super::native)
        });
        let backup_drained = backup_result.is_ok();
        let control_result = self.join_control_tasks();
        let control_drained = control_result.is_ok();
        let local = self.shared.drain_local(|| {
            let drained = self.local.try_shutdown();
            control_result.and(drained)
        });
        let (retired, managed_retired, exit) = {
            let s = self.shared.state.lock().unwrap();
            let managed = managed_catalog_retired(
                &s,
                self.shared.filesystem.is_some(),
                migration_drained,
                backup_drained,
                control_drained,
            );
            (
                migration_drained
                    && control_drained
                    && s.reaped
                    && s.child_finished
                    && (matches!(s.child_exit, Some(0 | 74)) || managed),
                managed,
                s.child_exit,
            )
        };
        let filesystem = if let Some(f) = &self.shared.filesystem {
            if retired && local.is_ok() {
                f.finish_after_dependents(exit != Some(0))
                    .map_err(|e| error(ErrorCode::Native, e.to_string()))
            } else {
                Err(error(
                    ErrorCode::Native,
                    "catalog dependents not verified drained; F retained",
                ))
            }
        } else {
            Ok(())
        };
        if filesystem.is_ok() {
            self.shared.state.lock().unwrap().filesystem_verified = true;
        }
        // Process::drain retains an abnormal-exit diagnostic even after checked
        // wait/join. Safe retirement does not turn that catalog outcome into a
        // known success: Shared.message and the migration failure remain intact.
        let result = if managed_retired && !matches!(exit, Some(0 | 74)) {
            Ok(())
        } else {
            result
        };
        result.and(backup_result).and(filesystem).and(local)
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
        Self::spawn_inner(config, None, None, None, None)
    }
    /// Unselected paired transport. Process metadata, native work, migration
    /// Source payloads and retained migration results use distinct caller-owned
    /// pools. G keeps each exact identity for the lifetime it funds.
    #[allow(dead_code)]
    pub(crate) fn spawn_with_filesystem(
        config: Config,
        client: Arc<crate::filesystem_worker::client::Client>,
        metadata: &crate::preview::ByteBudget,
        native: &crate::preview::ByteBudget,
        migration_source: &crate::preview::ByteBudget,
        migration_result: &crate::preview::ByteBudget,
    ) -> anyhow::Result<Self> {
        config.validate()?;
        anyhow::ensure!(
            !metadata.same_pool(native),
            "managed metadata and native allowances must use distinct pools"
        );
        anyhow::ensure!(
            !migration_source.same_pool(metadata)
                && !migration_source.same_pool(native)
                && !migration_result.same_pool(metadata)
                && !migration_result.same_pool(native)
                && !migration_source.same_pool(migration_result),
            "managed metadata, native, migration Source and migration result allowances must use distinct pools"
        );
        anyhow::ensure!(
            native.snapshot().0 == config.preview_limits.working_bytes,
            "native budget does not match configured working allowance"
        );
        {
            let parent = filesystem::Parent::new(client);
            parent.configure_native(
                config.worker_executable.clone(),
                config.preview_limits.clone(),
                native,
            )?;
            parent.configure_export_native(
                config.worker_executable.clone(),
                config.preview_limits.workers,
                native,
            )?;
            Self::spawn_inner(
                config,
                Some(parent),
                Some(metadata),
                Some(migration_source),
                Some(migration_result),
            )
        }
    }
    fn spawn_inner(
        config: Config,
        filesystem: Option<Arc<filesystem::Parent>>,
        metadata_budget: Option<&crate::preview::ByteBudget>,
        migration_source: Option<&crate::preview::ByteBudget>,
        migration_result: Option<&crate::preview::ByteBudget>,
    ) -> anyhow::Result<Self> {
        let (metadata, migration) = (|| {
            config.validate()?;
            anyhow::ensure!(
                filesystem.is_some() == metadata_budget.is_some()
                    && filesystem.is_some() == migration_source.is_some()
                    && filesystem.is_some() == migration_result.is_some(),
                "managed catalog requires explicit metadata, migration Source and migration result allowances"
            );
            match (metadata_budget, migration_source, migration_result) {
                (Some(metadata_budget), Some(source), Some(result)) => {
                    let reservation = preview_metadata_admission::ProcessReservation::reserve(
                        &config,
                        metadata_budget,
                    )?;
                    let grant = reservation.split_migration(&config)?;
                    let migration = migration::Funding::from_subgrant(
                        &config,
                        grant,
                        source.clone(),
                        result.clone(),
                    )?;
                    Ok((reservation, Some(migration)))
                }
                (None, None, None) => Ok((Default::default(), None)),
                _ => unreachable!("allowance presence checked above"),
            }
        })()
        .map_err(|e: anyhow::Error| match &filesystem {
            Some(owner) => {
                let message = e.to_string();
                e.context(filesystem::Unstarted {
                    owner: owner.clone(),
                    message,
                })
            }
            None => e,
        })?;
        Self::spawn_reserved(config, filesystem, metadata, migration, None)
    }

    /// Production supplies an already funded generation; the compatibility
    /// transport fixtures construct their legacy local owner here instead.
    fn spawn_reserved(
        config: Config,
        filesystem: Option<Arc<filesystem::Parent>>,
        metadata: preview_metadata_admission::ProcessReservation,
        migration_funding: Option<migration::Funding>,
        workbench: Option<workbench::Dispatcher>,
    ) -> anyhow::Result<Self> {
        let paired = filesystem.is_some();
        let backup = if paired {
            Some(Arc::new(Mutex::new(
                super::backup::Coordinator::new_managed(
                    Default::default(),
                    config.worker_executable.clone(),
                )?,
            )))
        } else {
            None
        };
        let mut configuration = wire::ConfigWire::from_config(&config);
        configuration.filesystem = filesystem.as_ref().map(|f| f.binding.clone());
        let hello = serde_json::to_vec(&configuration)
            .map_err(|e| filesystem::before_child_failure(&filesystem, e))?;
        if hello.len() > wire::CONFIG_BYTES {
            return Err(filesystem::before_child_failure(
                &filesystem,
                "desktop configuration byte limit",
            ));
        }
        if let Some(parent) = &filesystem {
            parent
                .retain_metadata(metadata.clone())
                .map_err(|e| filesystem::before_child_failure(&filesystem, e))?;
        }
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
                child_finished: false,
                local_verified: false,
                filesystem_verified: filesystem.is_none(),
                child_exit: None,
                catalog_retiring: false,
                catalog_epoch: 0,
                backup_admitting: false,
                close_admitting: false,
            }),
            wake: Condvar::new(),
            binary: Arc::new(AtomicUsize::new(0)),
            filesystem,
            backup,
            metadata,
            migration_stop: Mutex::new(None),
            #[cfg(test)]
            fixture: Mutex::new(None),
        });
        let local = match workbench {
            Some(dispatcher) => LocalOwner::Managed(dispatcher),
            None => Bridge::spawn(config.clone())
                .map(LocalOwner::Legacy)
                .map_err(|e| filesystem::before_child_failure(&shared.filesystem, e))?,
        };
        let owner = match process::Owner::spawn(&config.worker_executable, shared.clone(), hello) {
            Ok(owner) => owner,
            Err(e) => {
                if let Err(cleanup) = local.try_shutdown() {
                    return Err(anyhow::Error::new(RetainedStartup {
                        message: format!("{e}; Workbench cleanup: {cleanup}"),
                        owners: Mutex::new(Some(StartupOwners {
                            local,
                            filesystem: shared.filesystem.clone(),
                        })),
                    }));
                }
                if let Some(f) = &shared.filesystem
                    && f.finish_after_dependents(true).is_err()
                {
                    return Err(anyhow::Error::new(filesystem::Unstarted {
                        owner: f.clone(),
                        message: e.to_string(),
                    }));
                }
                return Err(e);
            }
        };
        let pid = owner.pid();
        let migration =
            migration::Coordinator::new(&shared, config.worker_executable, migration_funding);
        let result = Self(Arc::new(Handle {
            migration,
            local,
            shared,
            process: Mutex::new(owner),
            pid,
            shutdown: Mutex::new(()),
            control_tasks: Mutex::new(ControlTasks::default()),
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
        if paired && (!ready || timeout.timed_out()) {
            result
                .0
                .shared
                .fail(message.unwrap_or_else(|| "paired handshake failed; owners retained".into()));
            return Ok(result);
        }
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
    #[cfg(test)]
    pub(crate) fn set_backup_process_probe(
        &self,
        probe: Arc<dyn Fn(crate::catalog_backup::managed::ProcessEvent) + Send + Sync>,
    ) -> Result<()> {
        self.backup_owner()?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_process_probe(probe);
        Ok(())
    }
    /// Explicit fallible drain for owners that must retain admission after a wait failure.
    pub fn try_shutdown(&self) -> Result<()> {
        self.0.shutdown()
    }
    pub fn shutdown(&self) {
        let _ = self.try_shutdown();
    }
    pub fn submit(&self, request: Request) -> Result<Pending> {
        validate_public_request(&request, self.0.shared.limits.request_bytes)?;
        if let LocalOwner::Managed(dispatcher) = &self.0.local
            && let Request::Lightroom { request } = request
        {
            return dispatcher.submit(*request);
        }
        if let Request::Lightroom { request } = &request
            && let super::lightroom_bridge::Request::Action { guard, action } = request.as_ref()
            && let super::lightroom_bridge::Action::ApprovalDocuments {
                input,
                review_token,
            } = action
        {
            let filesystem = self.0.shared.filesystem.clone().ok_or_else(|| {
                error(
                    ErrorCode::InvalidRequest,
                    "approval documents require the managed filesystem owner",
                )
            })?;
            let local = self.0.local.legacy()?.clone();
            let guard = guard.clone();
            let input = input.clone();
            let review_token = review_token.clone();
            let cancel = Cancellation::default();
            let execution_cancel = cancel.flag();
            let (tx, receiver) = mpsc::sync_channel(1);
            thread::Builder::new()
                .name("lightroom-approval-receipt-resolution".into())
                .spawn(move || {
                    let response = local.lightroom_approval_documents(
                        &guard,
                        &input,
                        &review_token,
                        &execution_cancel,
                        |receipt| {
                            let request = crate::filesystem_worker::wire::LightroomArtifactPreparation::Resolve {
                                receipt: receipt.to_string(),
                            };
                            match filesystem.lightroom_artifact_preparation(
                                &request,
                                &execution_cancel,
                            )? {
                                Some(crate::filesystem_worker::wire::LightroomArtifactPreparationReply::Resolved {
                                    input_json,
                                    input_blake3,
                                    ..
                                }) => Ok(crate::lightroom::selection::ExactDocument {
                                    json: input_json,
                                    blake3: input_blake3,
                                }),
                                _ => anyhow::bail!("prepared artifact receipt resolution is absent"),
                            }
                        },
                    );
                    let reply = match response {
                        Ok(value) => Reply::Ok {
                            value: super::Response::Lightroom(Box::new(value)),
                        },
                        Err(error) => Reply::Error { error },
                    };
                    let _ = tx.send(reply);
                })
                .map_err(|failure| error(ErrorCode::Native, failure.to_string()))?;
            return Ok(Pending {
                completion: None,
                receiver,
                cancel,
            });
        }
        if let Request::Lightroom { request } = &request
            && let super::lightroom_bridge::Request::SealedDocument { request } = request.as_ref()
        {
            let filesystem = self.0.shared.filesystem.clone().ok_or_else(|| {
                error(
                    ErrorCode::InvalidRequest,
                    "sealed document reads require the managed filesystem owner",
                )
            })?;
            let request = request.clone();
            let cancel = Cancellation::default();
            let execution_cancel = cancel.flag();
            let (tx, receiver) = mpsc::sync_channel(1);
            thread::Builder::new()
                .name("lightroom-sealed-document-read".into())
                .spawn(move || {
                    let result = filesystem
                        .lightroom_sealed_read(&request, &execution_cancel)
                        .map(|value| Reply::Ok {
                            value: super::Response::Lightroom(Box::new(
                                super::lightroom_bridge::Response::SealedDocument(value),
                            )),
                        })
                        .unwrap_or_else(|failure| super::reply(Err(super::native(failure))));
                    let _ = tx.send(result);
                })
                .map_err(|failure| error(ErrorCode::Native, failure.to_string()))?;
            return Ok(Pending {
                completion: None,
                receiver,
                cancel,
            });
        }
        if let Request::Lightroom { request } = &request
            && let super::lightroom_bridge::Request::ArtifactPreparation { request } =
                request.as_ref()
        {
            let filesystem = self.0.shared.filesystem.clone().ok_or_else(|| {
                error(
                    ErrorCode::InvalidRequest,
                    "artifact preparation requires the managed filesystem owner",
                )
            })?;
            let request = request.clone();
            let cancel = Cancellation::default();
            let execution_cancel = cancel.flag();
            let (tx, receiver) = mpsc::sync_channel(1);
            thread::Builder::new()
                .name("lightroom-artifact-preparation".into())
                .spawn(move || {
                    let result = filesystem
                        .lightroom_artifact_preparation(&request, &execution_cancel)
                        .map(|value| Reply::Ok {
                            value: super::Response::Lightroom(Box::new(
                                super::lightroom_bridge::Response::ArtifactPreparation(value),
                            )),
                        })
                        .unwrap_or_else(|failure| super::reply(Err(super::native(failure))));
                    let _ = tx.send(result);
                })
                .map_err(|failure| error(ErrorCode::Native, failure.to_string()))?;
            return Ok(Pending {
                completion: None,
                receiver,
                cancel,
            });
        }
        if let Request::LightroomMigration { request } = request {
            let response = self.0.migration.request(*request)?;
            let (tx, receiver) = mpsc::sync_channel(1);
            let _ = tx.send(Reply::Ok {
                value: super::Response::LightroomMigration(Box::new(response)),
            });
            return Ok(Pending {
                completion: None,
                receiver,
                cancel: Cancellation::default(),
            });
        }
        self.0.migration.before_catalog_request(&request)?;
        if self.0.shared.backup.is_some() {
            if let Request::Export { request, .. } = &request
                && self.managed_backup_holds_jobs()?
                && !request.read_only()
                && !matches!(
                    request.as_ref(),
                    super::exports::Request::Cancel {
                        operation: Some(_),
                        ..
                    } | super::exports::Request::Yield { .. }
                )
            {
                return Err(error(
                    ErrorCode::Busy,
                    "catalog jobs are held; finish the backup operation",
                ));
            }
            match request {
                Request::BackupCreate { catalog, bundle } => {
                    return self.submit_backup_create(catalog, bundle);
                }
                Request::BackupInspect { bundle } => {
                    return self
                        .start_managed_backup(super::backup::Request::Inspect { bundle }, None);
                }
                Request::BackupRestore {
                    bundle,
                    destination,
                } => {
                    return self.start_managed_backup(
                        super::backup::Request::Restore {
                            bundle,
                            destination,
                        },
                        None,
                    );
                }
                Request::BackupStatus => return self.managed_backup_status(None),
                Request::BackupCancel { operation } => {
                    return self.managed_backup_status(Some(operation));
                }
                Request::Close { catalog } => return self.submit_managed_close(catalog),
                Request::Create { .. } | Request::OpenExisting { .. } => {
                    self.require_managed_backup_idle()?;
                }
                _ => {}
            }
        }
        self.submit_catalog(request)
    }
    fn submit_catalog(&self, request: Request) -> Result<Pending> {
        if local_route(&request) {
            return self.0.local.legacy()?.submit(request);
        }
        let control = control_route(&request);
        let retiring = if let Request::Close { catalog } = &request {
            Some(catalog.as_str())
        } else {
            None
        };
        let bytes = serde_json::to_vec(&request)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        if bytes.len() > self.0.shared.limits.request_bytes {
            return Err(error(ErrorCode::ResourceLimit, "request byte limit"));
        }
        let (tx, receiver) = mpsc::sync_channel(1);
        let cancel = self.enqueue(
            Kind::Command,
            bytes,
            Delivery::Command(tx),
            control,
            retiring,
        )?;
        Ok(Pending {
            completion: None,
            receiver,
            cancel,
        })
    }
    fn submit_close_shared(shared: &Arc<Shared>, catalog: String) -> Result<Pending> {
        let retiring = catalog.clone();
        let request = Request::Close { catalog };
        let bytes = serde_json::to_vec(&request)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        if bytes.len() > shared.limits.request_bytes {
            return Err(error(ErrorCode::ResourceLimit, "request byte limit"));
        }
        let (tx, receiver) = mpsc::sync_channel(1);
        let cancel = Self::enqueue_shared(
            shared,
            Kind::Command,
            bytes,
            Delivery::Command(tx),
            true,
            Some(retiring.as_str()),
        )?;
        Ok(Pending {
            completion: None,
            receiver,
            cancel,
        })
    }
    fn backup_owner(&self) -> Result<&Arc<Mutex<super::backup::Coordinator>>> {
        self.0.shared.backup.as_ref().ok_or_else(|| {
            error(
                ErrorCode::InvalidRequest,
                "managed backup owner is unavailable",
            )
        })
    }
    fn backup_reply(&self, snapshot: Option<super::backup::Snapshot>) -> Reply {
        Self::backup_reply_shared(&self.0.shared, snapshot)
    }
    fn backup_reply_shared(shared: &Shared, snapshot: Option<super::backup::Snapshot>) -> Reply {
        let reply = Reply::Ok {
            value: super::Response::Backup(snapshot),
        };
        match crate::lightroom::bounded_json(&reply, shared.limits.reply_bytes) {
            Ok(_) => reply,
            Err(_) => failure(ErrorCode::ResourceLimit, "response byte limit"),
        }
    }
    fn start_managed_backup(
        &self,
        request: super::backup::Request,
        catalog_epoch: Option<u64>,
    ) -> Result<Pending> {
        let snapshot = Self::start_managed_backup_shared(&self.0.shared, request, catalog_epoch)?;
        let (tx, receiver) = mpsc::sync_channel(1);
        let _ = tx.send(self.backup_reply(Some(snapshot)));
        Ok(Pending {
            completion: None,
            receiver,
            cancel: Cancellation::default(),
        })
    }
    fn start_managed_backup_shared(
        shared: &Arc<Shared>,
        request: super::backup::Request,
        catalog_epoch: Option<u64>,
    ) -> Result<super::backup::Snapshot> {
        let backup = shared.backup.as_ref().ok_or_else(|| {
            error(
                ErrorCode::InvalidRequest,
                "managed backup owner is unavailable",
            )
        })?;
        let mut owner = backup.try_lock().map_err(|_| {
            error(
                ErrorCode::Busy,
                "backup control is busy closing; retry shortly",
            )
        })?;
        let state = shared.state.lock().unwrap();
        if !state.ready
            || state.stopping
            || state.catalog_retiring
            || catalog_epoch.is_some_and(|epoch| epoch != state.catalog_epoch)
        {
            return Err(error(
                ErrorCode::Closed,
                "desktop catalog retirement blocks backup admission",
            ));
        }
        owner.start(request).map_err(super::native)
    }
    fn managed_backup_status(&self, cancel: Option<String>) -> Result<Pending> {
        let mut owner = self.backup_owner()?.try_lock().map_err(|_| {
            error(
                ErrorCode::Busy,
                "backup control is busy closing; retry shortly",
            )
        })?;
        let snapshot = match cancel {
            Some(operation) => Some(owner.cancel(&operation).map_err(super::native)?),
            None => owner.status().map_err(super::native)?,
        };
        let (tx, receiver) = mpsc::sync_channel(1);
        let _ = tx.send(self.backup_reply(snapshot));
        Ok(Pending {
            completion: None,
            receiver,
            cancel: Cancellation::default(),
        })
    }
    fn managed_backup_holds_jobs(&self) -> Result<bool> {
        let mut owner = self.backup_owner()?.try_lock().map_err(|_| {
            error(
                ErrorCode::Busy,
                "backup control is busy closing; retry shortly",
            )
        })?;
        let active = owner
            .status()
            .map_err(super::native)?
            .is_some_and(|snapshot| {
                matches!(
                    snapshot.state,
                    super::backup::State::Running | super::backup::State::CancelRequested
                )
            });
        Ok(active || self.0.shared.state.lock().unwrap().backup_admitting)
    }
    fn require_managed_backup_idle(&self) -> Result<()> {
        let mut owner = self.backup_owner()?.try_lock().map_err(|_| {
            error(
                ErrorCode::Busy,
                "backup control is busy closing; retry shortly",
            )
        })?;
        owner
            .require_idle_for_catalog_admission()
            .map_err(super::native)?;
        let state = self.0.shared.state.lock().unwrap();
        if state.catalog_retiring {
            return Err(error(
                ErrorCode::Busy,
                "catalog retirement admission is pending",
            ));
        }
        Ok(())
    }
    fn enqueue_backup_admission(
        &self,
        request: backup::AdmissionRequest,
    ) -> Result<backup::Pending> {
        let bytes = serde_json::to_vec(&request)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        let (tx, receiver) = mpsc::sync_channel(1);
        let cancel = self.enqueue(
            Kind::BackupAdmission,
            bytes,
            Delivery::BackupAdmission(tx),
            false,
            None,
        )?;
        Ok(backup::Pending { receiver, cancel })
    }
    fn submit_backup_create(&self, catalog: String, bundle: NativePath) -> Result<Pending> {
        let catalog_epoch = {
            let mut state = self.0.shared.state.lock().unwrap();
            if !state.ready || state.stopping || state.catalog_retiring {
                return Err(error(ErrorCode::Closed, "desktop owner unavailable"));
            }
            if state.backup_admitting {
                return Err(error(
                    ErrorCode::Busy,
                    "backup admission is already pending",
                ));
            }
            state.backup_admitting = true;
            state.catalog_epoch
        };
        let admission = self
            .enqueue_backup_admission(backup::AdmissionRequest::Create { catalog })
            .inspect_err(|_| {
                let mut state = self.0.shared.state.lock().unwrap();
                if state.catalog_epoch == catalog_epoch {
                    state.backup_admitting = false;
                }
            })?;
        let cancel = admission.cancel.clone();
        let execution_cancel = cancel.clone();
        let shared = self.0.shared.clone();
        let (tx, receiver) = mpsc::sync_channel(1);
        self.0
            .spawn_control_task(
                ControlTaskKind::BackupAdmission,
                "desktop-backup-admission",
                move || {
                    let reply = match admission.receiver.recv() {
                        Ok(backup::AdmissionReply::Ok(backup::Admission::Create {
                            source,
                            expected_source,
                        })) if !execution_cancel.is_canceled() => {
                            DesktopBridge::start_managed_backup_shared(
                                &shared,
                                super::backup::Request::Create {
                                    source,
                                    bundle,
                                    expected_source: Some(expected_source),
                                },
                                Some(catalog_epoch),
                            )
                            .map(|snapshot| {
                                DesktopBridge::backup_reply_shared(&shared, Some(snapshot))
                            })
                            .unwrap_or_else(|error| Reply::Error { error })
                        }
                        Ok(backup::AdmissionReply::Ok(backup::Admission::Close { .. })) => failure(
                            ErrorCode::Native,
                            "backup Create received a Close admission",
                        ),
                        Ok(backup::AdmissionReply::Ok(_)) => {
                            failure(ErrorCode::Canceled, "backup canceled before G admission")
                        }
                        Ok(backup::AdmissionReply::Error(error)) => Reply::Error { error },
                        Err(_) => failure(ErrorCode::Closed, "backup admission actor disconnected"),
                    };
                    let mut state = shared.state.lock().unwrap();
                    if state.catalog_epoch == catalog_epoch {
                        state.backup_admitting = false;
                    }
                    drop(state);
                    shared.wake.notify_all();
                    let _ = tx.send(reply);
                },
            )
            .map_err(|e| {
                let mut state = self.0.shared.state.lock().unwrap();
                if state.catalog_epoch == catalog_epoch {
                    state.backup_admitting = false;
                }
                error(ErrorCode::Native, e.to_string())
            })?;
        Ok(Pending {
            completion: None,
            receiver,
            cancel,
        })
    }
    fn submit_managed_close(&self, catalog: String) -> Result<Pending> {
        let backup = self.backup_owner()?.clone();
        let catalog_epoch = {
            let mut state = self.0.shared.state.lock().unwrap();
            if !state.ready || state.stopping {
                return Err(error(ErrorCode::Closed, "desktop owner unavailable"));
            }
            if state.catalog_retiring || state.close_admitting {
                return Err(error(
                    ErrorCode::Busy,
                    "catalog retirement is already pending",
                ));
            }
            state.close_admitting = true;
            state.catalog_epoch
        };
        let admission = self
            .enqueue_backup_admission(backup::AdmissionRequest::Close {
                catalog: catalog.clone(),
                epoch: catalog_epoch,
            })
            .inspect_err(|_| {
                let mut state = self.0.shared.state.lock().unwrap();
                if state.catalog_epoch == catalog_epoch {
                    state.close_admitting = false;
                }
            })?;
        let shared = self.0.shared.clone();
        let (tx, receiver) = mpsc::sync_channel(1);
        let cancel_slot = Arc::new(Mutex::new(Some(admission.cancel.clone())));
        let cancel_target = Arc::downgrade(&cancel_slot);
        let cancel = Cancellation(
            Arc::new(AtomicBool::new(false)),
            Some(Arc::new(move || {
                if let Some(slot) = cancel_target.upgrade()
                    && let Some(cancel) = slot.lock().unwrap().as_ref()
                {
                    cancel.cancel();
                }
            })),
        );
        let execution_cancel = cancel.clone();
        self.0
            .spawn_control_task(ControlTaskKind::Close, "desktop-backup-close", move || {
                let mut retirement_started = false;
                let reply = match admission.receiver.recv() {
                    Ok(backup::AdmissionReply::Ok(backup::Admission::Close { epoch }))
                        if epoch == catalog_epoch && !execution_cancel.is_canceled() =>
                    {
                        let transition = (|| -> Result<()> {
                            let mut owner = backup.lock().unwrap_or_else(|e| e.into_inner());
                            let mut state = shared.state.lock().unwrap();
                            if !state.ready
                                || state.stopping
                                || state.catalog_retiring
                                || !state.close_admitting
                                || state.catalog_epoch != catalog_epoch
                            {
                                return Err(error(
                                    ErrorCode::StaleSession,
                                    "Close admission epoch changed",
                                ));
                            }
                            let next_epoch =
                                state.catalog_epoch.checked_add(1).ok_or_else(|| {
                                    error(
                                        ErrorCode::ResourceLimit,
                                        "catalog backup admission epoch exhausted",
                                    )
                                })?;
                            state.close_admitting = false;
                            state.catalog_retiring = true;
                            state.catalog_epoch = next_epoch;
                            state.backup_admitting = false;
                            owner.signal_shutdown();
                            retirement_started = true;
                            Ok(())
                        })();
                        match transition {
                            Err(error) => Reply::Error { error },
                            Ok(()) => {
                                let drained = backup
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .shutdown()
                                    .map_err(super::native);
                                match drained {
                                    Err(error) => Reply::Error { error },
                                    Ok(_) if execution_cancel.is_canceled() => failure(
                                        ErrorCode::Canceled,
                                        "Close canceled after backup drain",
                                    ),
                                    Ok(_) => {
                                        match DesktopBridge::submit_close_shared(&shared, catalog) {
                                            Err(error) => Reply::Error { error },
                                            Ok(pending) => {
                                                let inner_cancel = pending.cancellation();
                                                *cancel_slot.lock().unwrap() =
                                                    Some(inner_cancel.clone());
                                                if execution_cancel.is_canceled() {
                                                    inner_cancel.cancel();
                                                }
                                                pending.recv()
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(backup::AdmissionReply::Ok(backup::Admission::Close { epoch }))
                        if epoch == catalog_epoch =>
                    {
                        failure(ErrorCode::Canceled, "Close canceled before G retirement")
                    }
                    Ok(backup::AdmissionReply::Ok(backup::Admission::Close { .. })) => {
                        failure(ErrorCode::StaleSession, "Close admission epoch changed")
                    }
                    Ok(backup::AdmissionReply::Ok(backup::Admission::Create { .. })) => failure(
                        ErrorCode::Native,
                        "Close received a backup Create admission",
                    ),
                    Ok(backup::AdmissionReply::Error(error)) => Reply::Error { error },
                    Err(_) => failure(ErrorCode::Closed, "Close admission actor disconnected"),
                };
                let mut state = shared.state.lock().unwrap();
                if retirement_started {
                    state.catalog_retiring = false;
                } else if state.catalog_epoch == catalog_epoch {
                    state.close_admitting = false;
                }
                drop(state);
                shared.wake.notify_all();
                let _ = tx.send(reply);
            })
            .map_err(|e| {
                let mut state = self.0.shared.state.lock().unwrap();
                if state.catalog_epoch == catalog_epoch {
                    state.close_admitting = false;
                }
                error(ErrorCode::Native, e.to_string())
            })?;
        Ok(Pending {
            completion: None,
            receiver,
            cancel,
        })
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
        Self::enqueue_shared(&self.0.shared, kind, bytes, delivery, control, retiring)
    }
    fn enqueue_shared(
        shared: &Arc<Shared>,
        kind: Kind,
        bytes: Vec<u8>,
        delivery: Delivery,
        control: bool,
        retiring: Option<&str>,
    ) -> Result<Cancellation> {
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
        if let Some(catalog) = retiring {
            retire_bytes(&s, catalog);
        }
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
        Request::LightroomMigration { .. }
        | Request::Export { .. }
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
        | Request::PreviewSettings { .. }
        | Request::Metadata { .. }
        | Request::MetadataWrite { .. }
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
        || matches!(r, Request::MetadataWrite {request,..} if request.control())
}

/// Hidden installed/CLI worker mode. No webview initialization occurs here.
pub fn worker_main() -> anyhow::Result<()> {
    process::worker_main()
}

#[cfg(test)]
pub(crate) fn test_export_executor_relay_admission(
    request: &crate::catalog_session::export_executor::Request,
    reply: &crate::catalog_session::export_executor::Reply,
) -> anyhow::Result<()> {
    filesystem::roundtrip_export_executor(request)?;
    filesystem::admit_export_executor_reply(request, reply)
}

#[cfg(test)]
pub(crate) fn test_import_relay_admission(
    request: &crate::catalog_session::import::Request,
    reply: &crate::catalog_session::import::Reply,
) -> anyhow::Result<()> {
    filesystem::admit_import_reply(request, reply)
}
