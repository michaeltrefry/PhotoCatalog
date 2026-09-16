//! Concrete G owner for one managed Workbench generation.
//!
//! This type is an unselected construction path. It directly retains F, the
//! independent Source broker/pump, every SQL13/CaptureSql reader and the exact
//! F cleanup receipts until checked terminal drain.
use super::lightroom::ManagedIo;
use super::{lightroom_bridge, lightroom_process};
use crate::{
    filesystem_worker::{
        client::Client as FilesystemClient,
        wire::{
            LightroomArtifactPreparation, LightroomArtifactPreparationReply,
            LightroomSealedDocumentPage, LightroomSealedRead, LightroomWorkbenchIo,
            LightroomWorkbenchIoReply, LightroomWorkbenchSealState,
            Operation as FilesystemOperation, Phase as FilesystemPhase,
            Response as FilesystemResponse,
        },
    },
    lightroom::{
        migration_source::{InputSeal, ReadLimits},
        plan::Cell,
    },
    lightroom_migration_worker::{
        identity::FileKey,
        memory::MemoryBudget,
        process::Stop,
        protocol::{ChildFrame, Guard, Publish, SourceListener},
        source_reader::{
            CaptureSqlAuthority, CaptureSqlReader, SqlReader,
            capture_wire::{Current, SchemaObjects, TableValue},
            relay::{self, broker::Broker, client::Client as RelayClient},
        },
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    path::Path,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub(crate) const FAILURE_CHARS: usize = 4096;
pub(super) const CAPABILITY_RECEIPTS: usize = 4_096;

fn bounded_failure(value: impl std::fmt::Display) -> String {
    value.to_string().chars().take(FAILURE_CHARS).collect()
}

struct SourceRouter {
    guard: Guard,
    stop: Arc<Stop>,
    broker: Mutex<Broker>,
    relay: Mutex<Weak<RelayClient>>,
    pump: Mutex<Option<JoinHandle<Result<()>>>>,
    stopping: AtomicBool,
    failure: Mutex<Option<String>>,
}

pub(super) struct MetadataLayouts {
    pub owner: usize,
    pub generation: usize,
    pub router: usize,
    pub reader: usize,
    pub retained_reader: usize,
    pub sql_reader_backing: usize,
    pub capture_reader_backing: usize,
    pub custody: usize,
    pub identity: usize,
    pub resource: usize,
    pub capability_custody: usize,
    pub receipt: usize,
}

pub(super) fn metadata_layouts() -> MetadataLayouts {
    let (sql_reader_backing, capture_reader_backing) =
        crate::lightroom_migration_worker::source_reader::reader_metadata_layouts();
    MetadataLayouts {
        owner: std::mem::size_of::<Owner>(),
        generation: std::mem::size_of::<Generation>(),
        router: std::mem::size_of::<SourceRouter>(),
        reader: std::mem::size_of::<Reader>(),
        retained_reader: std::mem::size_of::<RetainedReader>(),
        sql_reader_backing,
        capture_reader_backing,
        custody: std::mem::size_of::<Custody>(),
        identity: std::mem::size_of::<FIdentity>(),
        resource: std::mem::size_of::<FResource>(),
        capability_custody: std::mem::size_of::<CapabilityCustody>(),
        receipt: std::mem::size_of::<String>(),
    }
}

impl SourceRouter {
    fn start(
        executable: &Path,
        guard: Guard,
        budget: MemoryBudget,
    ) -> Result<(Arc<Self>, Arc<RelayClient>)> {
        let stop = Arc::new(Stop::default());
        let broker = Broker::start(
            executable.to_path_buf(),
            guard.clone(),
            stop.clone(),
            budget.clone(),
        )?;
        let router = Arc::new(Self {
            guard: guard.clone(),
            stop,
            broker: Mutex::new(broker),
            relay: Mutex::new(Weak::new()),
            pump: Mutex::new(None),
            stopping: AtomicBool::new(false),
            failure: Mutex::new(None),
        });
        let poison = router.clone();
        let relay = RelayClient::new(
            guard,
            router.clone(),
            Arc::new(move || poison.fail("Source relay requested abort")),
            budget,
        )?;
        *router
            .relay
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Arc::downgrade(&relay);
        let pump_router = router.clone();
        let pump = thread::Builder::new()
            .name("workbench-source-pump".into())
            .spawn(move || {
                let result = pump_router.run();
                if let Err(error) = &result {
                    pump_router.fail(format!("Source pump failed: {error:#}"));
                }
                result
            })
            .context("start Workbench Source pump")?;
        *router
            .pump
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(pump);
        Ok((router, relay))
    }

    fn fail(&self, detail: impl Into<String>) {
        let mut failure = self
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if failure.is_none() {
            *failure = Some(bounded_failure(detail.into()));
        }
        self.stopping.store(true, Ordering::Release);
        self.stop.cancel();
        if let Some(relay) = self
            .relay
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .upgrade()
        {
            relay.revoke();
        }
        self.broker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .revoke_after_lm();
    }

    fn check(&self) -> Result<()> {
        ensure!(
            !self.stopping.load(Ordering::Acquire),
            "Workbench Source owner is draining"
        );
        if let Some(failure) = self
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            anyhow::bail!("Workbench Source owner failed: {failure}");
        }
        Ok(())
    }

    fn run(&self) -> Result<()> {
        while !self.stopping.load(Ordering::Acquire) {
            let relay = self
                .relay
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .upgrade()
                .context("Workbench Source relay disappeared")?;
            let (urgent, event, failed) = {
                let broker = self
                    .broker
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                (
                    broker.try_urgent(),
                    broker.try_receive()?,
                    matches!(
                        broker.stop_state(&self.stop),
                        relay::broker::StopState::BrokerFailed
                    ),
                )
            };
            if let Some(event) = urgent {
                relay.accept(event)?;
            }
            if let Some(event) = event {
                relay.accept(event)?;
            }
            if failed {
                self.fail("Source broker failed");
            }
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    fn finish(&self, relay: &RelayClient) -> Result<()> {
        self.stopping.store(true, Ordering::Release);
        relay.revoke();
        self.broker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .revoke_after_lm();
        let pump = self
            .pump
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let pump_failure = pump.and_then(|owner| match owner.join() {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some(anyhow::anyhow!("Workbench Source pump panicked")),
        });
        let broker = self
            .broker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .finish();
        // `failure` records why admission was permanently revoked. It is not
        // itself a failure to retire the already-revoked Source owner. Checked
        // drain succeeds once both concrete owners have joined successfully.
        match (pump_failure, broker) {
            (None, Ok(())) => Ok(()),
            (Some(error), _) => Err(error),
            (None, Err(error)) => Err(error),
        }
    }
}

impl Publish for SourceRouter {
    fn publish(&self, frame: &ChildFrame) -> Result<()> {
        self.check()?;
        let ChildFrame::Source { guard, command } = frame else {
            anyhow::bail!("Workbench Source relay emitted a non-Source frame")
        };
        ensure!(guard == &self.guard, "Workbench Source relay guard differs");
        let mut command = command.clone();
        loop {
            self.check()?;
            let pending = self
                .broker
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .try_send(command)?;
            match pending {
                None => return Ok(()),
                Some(value) => command = value,
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

enum Reader {
    Capture {
        owner: Box<CaptureSqlReader>,
        cancel: Arc<AtomicBool>,
    },
    Sql {
        owner: Box<SqlReader>,
        cancel: Arc<AtomicBool>,
    },
}
struct RetainedReader {
    id: String,
    reader: Reader,
}
impl Reader {
    fn retire(self) -> Result<()> {
        match self {
            Self::Capture { owner, .. } => (*owner).retire(),
            Self::Sql { owner, .. } => (*owner).retire(),
        }
    }
}

#[derive(Clone)]
struct FIdentity {
    operation: String,
    workbench: String,
    generation: String,
}
#[derive(Clone)]
struct FResource {
    identity: FIdentity,
    resource: String,
}
#[derive(Clone, Default)]
struct Custody {
    root: Option<FIdentity>,
    capture: Option<FIdentity>,
    evidence: Option<FResource>,
    original: Option<FResource>,
    seal: Option<FResource>,
}

impl Custody {
    fn before(&mut self, request: &LightroomWorkbenchIo) {
        match request {
            LightroomWorkbenchIo::RootBegin {
                operation,
                workbench,
                generation,
                ..
            } => {
                self.root = Some(FIdentity {
                    operation: operation.clone(),
                    workbench: workbench.clone(),
                    generation: generation.clone(),
                });
            }
            LightroomWorkbenchIo::CaptureStart {
                operation,
                workbench,
                generation,
                ..
            } => {
                self.capture = Some(FIdentity {
                    operation: operation.clone(),
                    workbench: workbench.clone(),
                    generation: generation.clone(),
                });
            }
            LightroomWorkbenchIo::EvidenceBegin {
                operation,
                workbench,
                generation,
                capture_generation,
                ..
            } => {
                self.evidence = Some(FResource {
                    identity: FIdentity {
                        operation: operation.clone(),
                        workbench: workbench.clone(),
                        generation: generation.clone(),
                    },
                    resource: capture_generation.clone(),
                });
            }
            LightroomWorkbenchIo::OriginalBegin {
                operation,
                workbench,
                generation,
                candidate,
                ..
            } => {
                self.original = Some(FResource {
                    identity: FIdentity {
                        operation: operation.clone(),
                        workbench: workbench.clone(),
                        generation: generation.clone(),
                    },
                    resource: candidate.token.clone(),
                });
            }
            LightroomWorkbenchIo::SealBegin {
                operation,
                workbench,
                generation,
                token,
                ..
            } => {
                self.seal = Some(FResource {
                    identity: FIdentity {
                        operation: operation.clone(),
                        workbench: workbench.clone(),
                        generation: generation.clone(),
                    },
                    resource: token.clone(),
                });
            }
            _ => {}
        }
    }

    fn after(&mut self, request: &LightroomWorkbenchIo, reply: &LightroomWorkbenchIoReply) {
        match request {
            LightroomWorkbenchIo::RootRelease { .. } => self.root = None,
            LightroomWorkbenchIo::CaptureRetire { .. } => self.capture = None,
            LightroomWorkbenchIo::EvidenceRelease { .. } => self.evidence = None,
            LightroomWorkbenchIo::OriginalRelease { .. } => self.original = None,
            LightroomWorkbenchIo::SealAbort { .. }
            | LightroomWorkbenchIo::SealPublish { .. }
            | LightroomWorkbenchIo::SealStatus { .. }
                if matches!(
                    reply,
                    LightroomWorkbenchIoReply::SealState {
                        state: LightroomWorkbenchSealState::Published
                            | LightroomWorkbenchSealState::Aborted,
                        ..
                    }
                ) =>
            {
                self.seal = None
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)] // One bounded inline pending slot is capacity-accounted.
enum CapabilityPending {
    Sealed(LightroomSealedRead),
    Artifact(LightroomArtifactPreparation),
}

enum CapabilityOutcome<T> {
    Reply(T),
    Rejected(anyhow::Error),
    Unknown(anyhow::Error),
}

#[derive(Default)]
struct CapabilityCustody {
    // Set before the request can reach F. It remains set until an exact,
    // validated response (including an authoritative rejection) is observed.
    pending: Option<CapabilityPending>,
    sealed: Option<LightroomSealedRead>,
    artifact: Option<LightroomArtifactPreparation>,
    receipts: Vec<String>,
}

impl CapabilityCustody {
    fn before(&mut self, request: CapabilityPending) -> Result<()> {
        ensure!(
            self.pending.is_none(),
            "previous filesystem capability outcome remains unresolved"
        );
        self.pending = Some(request);
        Ok(())
    }

    fn reject(&mut self) {
        self.pending = None;
    }

    fn after_sealed(
        &mut self,
        request: &LightroomSealedRead,
        reply: &Option<LightroomSealedDocumentPage>,
    ) -> Result<()> {
        match (request, reply) {
            (LightroomSealedRead::Begin { .. }, Some(reply)) => {
                reply.validate_for(request)?;
                self.sealed = Some(request.clone());
            }
            (LightroomSealedRead::Page { .. }, Some(reply)) => {
                reply.validate_for(request)?;
            }
            (LightroomSealedRead::Discard { .. }, None) => self.sealed = None,
            _ => anyhow::bail!("sealed document response is absent or differs"),
        }
        self.pending = None;
        Ok(())
    }

    fn after_artifact(
        &mut self,
        request: &LightroomArtifactPreparation,
        reply: &Option<LightroomArtifactPreparationReply>,
    ) -> Result<()> {
        match (request, reply) {
            (LightroomArtifactPreparation::Begin { .. }, Some(reply)) => {
                reply.validate_for(request)?;
                self.artifact = Some(request.clone());
            }
            (
                LightroomArtifactPreparation::Member { .. },
                Some(reply @ LightroomArtifactPreparationReply::Prepared { receipt, .. }),
            ) => {
                reply.validate_for(request)?;
                if !self.receipts.contains(receipt) {
                    ensure!(
                        self.receipts.len() < CAPABILITY_RECEIPTS,
                        "managed artifact receipt capacity"
                    );
                    self.receipts.push(receipt.clone());
                }
            }
            (LightroomArtifactPreparation::Resolve { .. }, Some(reply)) => {
                reply.validate_for(request)?;
            }
            (LightroomArtifactPreparation::DiscardReceipt { receipt }, None) => {
                self.receipts.retain(|value| value != receipt);
            }
            (LightroomArtifactPreparation::Discard { .. }, None) => self.artifact = None,
            _ => anyhow::bail!("artifact preparation response is absent or differs"),
        }
        self.pending = None;
        Ok(())
    }

    fn empty(&self) -> bool {
        self.pending.is_none()
            && self.sealed.is_none()
            && self.artifact.is_none()
            && self.receipts.is_empty()
    }
}

/// Unselected managed construction path. The caller retains its F Arc and the
/// shared allocation pool if construction fails, so startup can be retried or
/// explicitly drained without losing an active owner.
pub(crate) struct Owner {
    metadata: super::lightroom_capacity::Admission,
    filesystem: Arc<FilesystemClient>,
    router: Arc<SourceRouter>,
    relay: Arc<RelayClient>,
    guard: Guard,
    reader: Mutex<Option<RetainedReader>>,
    next_reader: AtomicU64,
    custody: Mutex<Custody>,
    capability_custody: Mutex<CapabilityCustody>,
    failed: AtomicBool,
    root_release_allowed: AtomicBool,
    workbench_started: AtomicBool,
    workbench_reaped: AtomicBool,
    monitor_stop: AtomicBool,
    monitor: Mutex<Option<JoinHandle<()>>>,
    drain_lock: Mutex<()>,
    source_drain: Mutex<Option<Option<String>>>,
    filesystem_drained: AtomicBool,
    #[cfg(test)]
    fail_next_commit: AtomicBool,
    #[cfg(test)]
    capability_ack_probe: Mutex<Option<Arc<tests::CapabilityAckProbe>>>,
}

/// One concrete G generation. Construction borrows the dependency owner, so a
/// failed W bootstrap leaves the exact F/S owner with the caller for checked
/// cleanup or an explicit retry. W is declared first and therefore drains
/// before the final dependency Arc can be released.
pub(crate) struct Generation {
    workbench: lightroom_process::Client,
    dependencies: Arc<Owner>,
}

impl Generation {
    pub(crate) fn start(dependencies: &Arc<Owner>, executable: &Path) -> Result<Self> {
        dependencies.admit()?;
        let managed = dependencies.as_io();
        let workbench = lightroom_process::Client::spawn_managed(executable, &managed)?;
        dependencies
            .workbench_started
            .store(true, Ordering::Release);
        Ok(Self {
            workbench,
            dependencies: dependencies.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn start_fixture(dependencies: &Arc<Owner>, executable: &Path) -> Result<Self> {
        dependencies.admit()?;
        let managed = dependencies.as_io();
        let workbench = lightroom_process::Client::spawn_managed_fixture(executable, &managed)?;
        dependencies
            .workbench_started
            .store(true, Ordering::Release);
        Ok(Self {
            workbench,
            dependencies: dependencies.clone(),
        })
    }

    pub(crate) fn call(
        &self,
        request: lightroom_bridge::Request,
    ) -> Result<lightroom_bridge::Response> {
        if let Err(error) = self.dependencies.admit() {
            let cleanup = self.shutdown_checked();
            return if self.workbench.checked_drained() {
                Err(error.context(format!(
                    "managed generation failure checked-drained{}",
                    cleanup
                        .err()
                        .map(|value| format!(" after {value:#}"))
                        .unwrap_or_default()
                )))
            } else {
                Err(error.context(format!(
                    "managed generation remains nonterminal; checked drain failed: {:#}",
                    cleanup.expect_err("undrained generation must report failure")
                )))
            };
        }
        self.workbench.call(request)
    }

    pub(crate) fn interrupt(&self) -> Result<()> {
        self.workbench.interrupt()
    }

    pub(crate) fn shutdown_checked(&self) -> Result<()> {
        self.workbench.shutdown()
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.workbench.pid()
    }
}

impl Owner {
    pub(crate) fn start(
        filesystem: &Arc<FilesystemClient>,
        source_executable: &Path,
        allocation: super::lightroom_capacity::Allocation,
    ) -> Result<Arc<Self>> {
        ensure!(
            source_executable.is_absolute(),
            "absolute Source executable required"
        );
        ensure!(
            filesystem.status().phase == FilesystemPhase::Ready,
            "filesystem owner is not ready for Workbench admission"
        );
        let guard = Guard {
            session: crate::catalog_session::LeaseId::new().as_str().into(),
            generation: crate::catalog_session::LeaseId::new().as_str().into(),
            operation: "workbench-sources".into(),
        };
        guard.validate()?;
        let (metadata, source_payloads) = allocation.into_parts();
        let budget = MemoryBudget::from_shared(source_payloads);
        let (router, relay) = SourceRouter::start(source_executable, guard.clone(), budget)?;
        metadata.arm();
        let owner = Arc::new(Self {
            metadata,
            filesystem: filesystem.clone(),
            router,
            relay,
            guard,
            reader: Mutex::new(None),
            next_reader: AtomicU64::new(1),
            custody: Mutex::new(Custody::default()),
            capability_custody: Mutex::new(CapabilityCustody::default()),
            failed: AtomicBool::new(false),
            root_release_allowed: AtomicBool::new(true),
            workbench_started: AtomicBool::new(false),
            workbench_reaped: AtomicBool::new(false),
            monitor_stop: AtomicBool::new(false),
            monitor: Mutex::new(None),
            drain_lock: Mutex::new(()),
            source_drain: Mutex::new(None),
            filesystem_drained: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_commit: AtomicBool::new(false),
            #[cfg(test)]
            capability_ack_probe: Mutex::new(None),
        });
        let weak = Arc::downgrade(&owner);
        let monitor = thread::Builder::new()
            .name("workbench-filesystem-monitor".into())
            .spawn(move || {
                while let Some(owner) = weak.upgrade() {
                    if owner.monitor_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if owner.filesystem.status().phase != FilesystemPhase::Ready {
                        owner.fail_sources();
                        break;
                    }
                    drop(owner);
                    thread::sleep(Duration::from_millis(10));
                }
            });
        match monitor {
            Ok(monitor) => {
                *owner
                    .monitor
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(monitor);
                Ok(owner)
            }
            Err(error) => {
                let primary =
                    anyhow::Error::new(error).context("start Workbench filesystem monitor");
                let cleanup = owner.drain_checked();
                match cleanup {
                    Ok(()) => Err(primary),
                    Err(cleanup) => Err(primary.context(format!(
                        "managed owner startup cleanup also failed: {cleanup:#}"
                    ))),
                }
            }
        }
    }

    fn check_source(&self) -> Result<()> {
        ensure!(
            !self.failed.load(Ordering::Acquire),
            "managed Workbench owner failed"
        );
        self.router.check()
    }

    fn fail_sources(&self) {
        self.failed.store(true, Ordering::Release);
        self.root_release_allowed.store(false, Ordering::Release);
        self.relay.revoke();
        self.router.fail("managed Workbench owner failed");
    }

    fn commit(&self) -> Result<()> {
        #[cfg(test)]
        if self.fail_next_commit.swap(false, Ordering::AcqRel) {
            self.fail_sources();
            anyhow::bail!("injected F loss at managed commit gate");
        }
        self.admit()
            .context("managed Workbench commit authority revoked")
    }

    #[cfg(test)]
    pub(crate) fn inject_f_loss_at_next_commit(&self) {
        self.fail_next_commit.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn install_capability_ack_probe(&self, probe: Arc<tests::CapabilityAckProbe>) {
        *self
            .capability_ack_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(probe);
    }

    #[cfg(test)]
    fn pause_capability_ack(&self, event: Option<tests::CapabilityAck>, receipt: Option<&str>) {
        let Some(event) = event else { return };
        let probe = {
            let mut slot = self
                .capability_ack_probe
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if slot.as_ref().is_some_and(|probe| probe.target == event) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(probe) = probe {
            probe.pause(receipt);
        }
    }

    #[cfg(test)]
    pub(crate) fn capability_custody_empty(&self) -> bool {
        self.capability_custody
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .empty()
    }

    fn revoke_generation(&self) {
        self.fail_sources();
    }

    fn workbench_reaped(&self) {
        self.workbench_reaped.store(true, Ordering::Release);
    }

    pub(crate) fn as_io(self: &Arc<Self>) -> Arc<dyn ManagedIo> {
        self.clone()
    }

    pub(crate) fn admit(&self) -> Result<()> {
        let status = self.filesystem.status();
        if status.phase != FilesystemPhase::Ready {
            self.fail_sources();
            anyhow::bail!("filesystem owner is not ready for Workbench admission: {status:?}")
        }
        self.check_source()
    }

    fn reader_id(&self, kind: &str) -> Result<String> {
        let sequence = self
            .next_reader
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("Workbench Source generation exhausted"))?;
        Ok(format!("{kind}-{sequence}"))
    }

    fn call_sealed_capability(
        &self,
        request: &LightroomSealedRead,
        cancel: &AtomicBool,
    ) -> CapabilityOutcome<Option<LightroomSealedDocumentPage>> {
        match self.filesystem.execute(
            FilesystemOperation::LightroomSealedRead(request.clone()),
            cancel,
        ) {
            Ok(FilesystemResponse::LightroomSealedDocument(value)) => {
                CapabilityOutcome::Reply(value)
            }
            Ok(_) => CapabilityOutcome::Unknown(anyhow::anyhow!(
                "filesystem sealed-document response kind"
            )),
            Err(error) if self.filesystem.status().phase == FilesystemPhase::Ready => {
                CapabilityOutcome::Rejected(error)
            }
            Err(error) => CapabilityOutcome::Unknown(error),
        }
    }

    fn call_artifact_capability(
        &self,
        request: &LightroomArtifactPreparation,
        cancel: &AtomicBool,
    ) -> CapabilityOutcome<Option<LightroomArtifactPreparationReply>> {
        match self.filesystem.execute(
            FilesystemOperation::LightroomArtifactPreparation(request.clone()),
            cancel,
        ) {
            Ok(FilesystemResponse::LightroomArtifactPreparation(value)) => {
                CapabilityOutcome::Reply(value)
            }
            Ok(_) => CapabilityOutcome::Unknown(anyhow::anyhow!(
                "filesystem artifact-preparation response kind"
            )),
            Err(error) if self.filesystem.status().phase == FilesystemPhase::Ready => {
                CapabilityOutcome::Rejected(error)
            }
            Err(error) => CapabilityOutcome::Unknown(error),
        }
    }

    fn sealed_capability(
        &self,
        request: LightroomSealedRead,
        cancel: &AtomicBool,
    ) -> Result<Option<LightroomSealedDocumentPage>> {
        request.validate()?;
        self.capability_custody
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .before(CapabilityPending::Sealed(request.clone()))?;
        match self.call_sealed_capability(&request, cancel) {
            CapabilityOutcome::Reply(reply) => {
                if let Err(error) = self
                    .capability_custody
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .after_sealed(&request, &reply)
                {
                    self.fail_sources();
                    return Err(error.context("record sealed-document custody"));
                }
                #[cfg(test)]
                self.pause_capability_ack(tests::CapabilityAck::for_sealed(&request), None);
                Ok(reply)
            }
            CapabilityOutcome::Rejected(error) => {
                self.capability_custody
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .reject();
                Err(error)
            }
            CapabilityOutcome::Unknown(error) => {
                self.fail_sources();
                Err(error.context("sealed-document outcome unknown and retained"))
            }
        }
    }

    fn artifact_capability(
        &self,
        request: LightroomArtifactPreparation,
        cancel: &AtomicBool,
    ) -> Result<Option<LightroomArtifactPreparationReply>> {
        request.validate()?;
        self.capability_custody
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .before(CapabilityPending::Artifact(request.clone()))?;
        match self.call_artifact_capability(&request, cancel) {
            CapabilityOutcome::Reply(reply) => {
                if let Err(error) = self
                    .capability_custody
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .after_artifact(&request, &reply)
                {
                    self.fail_sources();
                    return Err(error.context("record artifact-preparation custody"));
                }
                #[cfg(test)]
                self.pause_capability_ack(
                    tests::CapabilityAck::for_artifact(&request),
                    match &reply {
                        Some(LightroomArtifactPreparationReply::Prepared { receipt, .. }) => {
                            Some(receipt.as_str())
                        }
                        _ => None,
                    },
                );
                Ok(reply)
            }
            CapabilityOutcome::Rejected(error) => {
                self.capability_custody
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .reject();
                Err(error)
            }
            CapabilityOutcome::Unknown(error) => {
                self.fail_sources();
                Err(error.context("artifact-preparation outcome unknown and retained"))
            }
        }
    }

    fn reconcile_pending_capability(&self, cancel: &AtomicBool) -> Result<()> {
        let pending = self
            .capability_custody
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .clone();
        match pending {
            None => Ok(()),
            Some(CapabilityPending::Sealed(request)) => {
                let CapabilityOutcome::Reply(reply) = self.call_sealed_capability(&request, cancel)
                else {
                    anyhow::bail!("pending sealed-document outcome could not be reconciled")
                };
                self.capability_custody
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .after_sealed(&request, &reply)
                    .context("reconcile pending sealed-document custody")
            }
            Some(CapabilityPending::Artifact(request)) => {
                let CapabilityOutcome::Reply(reply) =
                    self.call_artifact_capability(&request, cancel)
                else {
                    anyhow::bail!("pending artifact-preparation outcome could not be reconciled")
                };
                self.capability_custody
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .after_artifact(&request, &reply)
                    .context("reconcile pending artifact-preparation custody")
            }
        }
    }

    fn cleanup_capabilities(&self, cancel: &AtomicBool) -> Result<()> {
        self.reconcile_pending_capability(cancel)?;
        loop {
            let receipt = self
                .capability_custody
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .receipts
                .last()
                .cloned();
            let Some(receipt) = receipt else { break };
            self.artifact_capability(
                LightroomArtifactPreparation::DiscardReceipt { receipt },
                cancel,
            )?;
        }
        let artifact = self
            .capability_custody
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .artifact
            .clone();
        if let Some(LightroomArtifactPreparation::Begin { session, .. }) = artifact {
            self.artifact_capability(LightroomArtifactPreparation::Discard { session }, cancel)?;
        }
        let sealed = self
            .capability_custody
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sealed
            .clone();
        if let Some(LightroomSealedRead::Begin { session, .. }) = sealed {
            self.sealed_capability(LightroomSealedRead::Discard { session }, cancel)?;
        }
        ensure!(
            self.capability_custody
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .empty(),
            "filesystem capability custody remains retained"
        );
        Ok(())
    }

    fn cleanup_filesystem(&self) -> Result<()> {
        let custody = self
            .custody
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let cancel = AtomicBool::new(false);
        let mut failures = Vec::new();
        if let Err(error) = self.cleanup_capabilities(&cancel) {
            failures.push(bounded_failure(format!(
                "F capability reconciliation retained: {error:#}"
            )));
        }
        macro_rules! call {
            ($request:expr) => {
                if let Err(error) = self.cleanup_call($request, &cancel) {
                    failures.push(bounded_failure(error));
                }
            };
        }
        if let Some(resource) = custody.original {
            call!(LightroomWorkbenchIo::OriginalRelease {
                operation: resource.identity.operation,
                workbench: resource.identity.workbench,
                generation: resource.identity.generation,
                token: resource.resource,
            });
        }
        if let Some(resource) = custody.evidence {
            call!(LightroomWorkbenchIo::EvidenceRelease {
                operation: resource.identity.operation,
                workbench: resource.identity.workbench,
                generation: resource.identity.generation,
                capture_generation: resource.resource,
            });
        }
        if let Some(identity) = custody.capture {
            call!(LightroomWorkbenchIo::CaptureCancel {
                operation: identity.operation.clone(),
                workbench: identity.workbench.clone(),
                generation: identity.generation.clone(),
            });
            call!(LightroomWorkbenchIo::CaptureRetire {
                operation: identity.operation,
                workbench: identity.workbench,
                generation: identity.generation,
            });
        }
        if let Some(resource) = custody.seal {
            let status = LightroomWorkbenchIo::SealStatus {
                operation: resource.identity.operation.clone(),
                workbench: resource.identity.workbench.clone(),
                generation: resource.identity.generation.clone(),
                token: resource.resource.clone(),
            };
            match self.filesystem.lightroom_workbench_io(&status, &cancel) {
                Ok(
                    reply @ LightroomWorkbenchIoReply::SealState {
                        state:
                            LightroomWorkbenchSealState::Published
                            | LightroomWorkbenchSealState::Aborted,
                        ..
                    },
                ) => {
                    self.custody
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .after(&status, &reply);
                }
                Ok(reply) => {
                    if let Err(error) = reply.validate_for(&status) {
                        failures.push(bounded_failure(format!("F seal status receipt: {error:#}")));
                    } else {
                        call!(LightroomWorkbenchIo::SealAbort {
                            operation: resource.identity.operation,
                            workbench: resource.identity.workbench,
                            generation: resource.identity.generation,
                            token: resource.resource,
                        });
                    }
                }
                Err(error) => failures.push(bounded_failure(format!(
                    "F seal status unknown and retained: {error:#}"
                ))),
            }
        }
        let subordinates_drained = {
            let custody = self
                .custody
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let capabilities_drained = self
                .capability_custody
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .empty();
            capabilities_drained
                && custody.capture.is_none()
                && custody.evidence.is_none()
                && custody.original.is_none()
                && custody.seal.is_none()
        };
        if subordinates_drained {
            if let Some(identity) = custody.root {
                call!(LightroomWorkbenchIo::RootRelease {
                    operation: identity.operation,
                    workbench: identity.workbench,
                    generation: identity.generation,
                });
            }
        } else {
            failures.push("F root retained behind unresolved subordinate custody".into());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "managed Workbench F cleanup failed: {}",
                failures.join("; ")
            )
        }
    }

    fn cleanup_call(
        &self,
        request: LightroomWorkbenchIo,
        cancel: &AtomicBool,
    ) -> std::result::Result<(), String> {
        match self.filesystem.lightroom_workbench_io(&request, cancel) {
            Ok(reply) => {
                reply
                    .validate_for(&request)
                    .map_err(|error| format!("F cleanup receipt: {error:#}"))?;
                self.custody
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .after(&request, &reply);
                Ok(())
            }
            Err(error) => Err(bounded_failure(format!("F cleanup retained: {error:#}"))),
        }
    }

    fn drain_sources(&self) -> Result<()> {
        let _drain = self
            .drain_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.failed.store(true, Ordering::Release);
        let source_failure = {
            let mut state = self
                .source_drain
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.is_none() {
                let mut failures = Vec::new();
                self.monitor_stop.store(true, Ordering::Release);
                if let Some(monitor) = self
                    .monitor
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                    && monitor.join().is_err()
                {
                    failures.push("filesystem monitor panicked".into());
                }
                let reader = std::mem::take(
                    &mut *self
                        .reader
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()),
                );
                if let Some(reader) = reader
                    && let Err(error) = reader.reader.retire()
                {
                    failures.push(bounded_failure(format!(
                        "Source {} retirement: {error:#}",
                        reader.id
                    )));
                }
                if let Err(error) = self.router.finish(&self.relay) {
                    failures.push(bounded_failure(format!("Source checked drain: {error:#}")));
                }
                *state = Some(if failures.is_empty() {
                    None
                } else {
                    Some(failures.join("; "))
                });
            }
            state.clone().flatten()
        };
        match source_failure {
            None => Ok(()),
            Some(error) => Err(anyhow::anyhow!(error)),
        }
    }

    fn drain_filesystem(&self) -> Result<()> {
        let _drain = self
            .drain_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            self.source_drain
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .is_some_and(Option::is_none),
            "Source owners are not checked-drained before F reconciliation"
        );
        ensure!(
            !self.workbench_started.load(Ordering::Acquire)
                || self.workbench_reaped.load(Ordering::Acquire),
            "F reconciliation retained until W is checked-reaped"
        );
        if self.filesystem_drained.load(Ordering::Acquire) {
            return Ok(());
        }
        self.cleanup_filesystem()
            .context("filesystem checked drain")?;
        self.filesystem_drained.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn drain_checked(&self) -> Result<()> {
        self.drain_sources()?;
        self.drain_filesystem()?;
        self.metadata.checked_drained();
        Ok(())
    }
}

impl ManagedIo for Owner {
    fn admit(&self) -> Result<()> {
        Owner::admit(self)
    }

    fn commit(&self) -> Result<()> {
        Owner::commit(self)
    }

    fn revoke_generation(&self) {
        Owner::revoke_generation(self);
    }

    fn workbench_reaped(&self) {
        Owner::workbench_reaped(self);
    }

    fn filesystem(
        &self,
        request: LightroomWorkbenchIo,
        cancel: &AtomicBool,
    ) -> Result<LightroomWorkbenchIoReply> {
        request.validate()?;
        if matches!(&request, LightroomWorkbenchIo::RootRelease { .. }) {
            ensure!(
                self.root_release_allowed.load(Ordering::Acquire),
                "F root release retained until fatal W is checked-reaped"
            );
        }
        self.custody
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .before(&request);
        let result = self.filesystem.lightroom_workbench_io(&request, cancel);
        match result {
            Ok(reply) => {
                self.custody
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .after(&request, &reply);
                Ok(reply)
            }
            Err(error) => {
                if !cancel.load(Ordering::Acquire) {
                    self.fail_sources();
                }
                Err(error)
            }
        }
    }

    fn sealed_document(
        &self,
        request: LightroomSealedRead,
        cancel: &AtomicBool,
    ) -> Result<Option<LightroomSealedDocumentPage>> {
        self.sealed_capability(request, cancel)
    }

    fn artifact_preparation(
        &self,
        request: LightroomArtifactPreparation,
        cancel: &AtomicBool,
    ) -> Result<Option<LightroomArtifactPreparationReply>> {
        self.artifact_capability(request, cancel)
    }

    fn source_open(
        &self,
        authority: CaptureSqlAuthority,
        cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        self.check_source()?;
        let reader = self.reader_id("capture-sql")?;
        let owner = CaptureSqlReader::open(
            self.relay.clone(),
            self.guard.clone(),
            reader.clone(),
            authority,
            cancel.clone(),
        );
        match owner {
            Ok(owner) => {
                let mut retained = self
                    .reader
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                ensure!(
                    retained.is_none(),
                    "previous Source generation remains retained"
                );
                *retained = Some(RetainedReader {
                    id: reader.clone(),
                    reader: Reader::Capture {
                        owner: Box::new(owner),
                        cancel,
                    },
                });
                Ok(reader)
            }
            Err(error) => {
                if !cancel.load(Ordering::Acquire) {
                    self.fail_sources();
                }
                Err(error)
            }
        }
    }

    fn source_sql_open(
        &self,
        seal: InputSeal,
        limits: ReadLimits,
        protected: Vec<FileKey>,
        cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        self.check_source()?;
        let reader = self.reader_id("sql13")?;
        let owner = SqlReader::open(
            self.relay.clone(),
            self.guard.clone(),
            reader.clone(),
            seal,
            limits,
            protected,
            cancel.clone(),
        );
        match owner {
            Ok(owner) => {
                let mut retained = self
                    .reader
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                ensure!(
                    retained.is_none(),
                    "previous Source generation remains retained"
                );
                *retained = Some(RetainedReader {
                    id: reader.clone(),
                    reader: Reader::Sql {
                        owner: Box::new(owner),
                        cancel,
                    },
                });
                Ok(reader)
            }
            Err(error) => {
                if !cancel.load(Ordering::Acquire) {
                    self.fail_sources();
                }
                Err(error)
            }
        }
    }

    fn source_schema(&self, source: &str) -> Result<SchemaObjects> {
        self.check_source()?;
        let reader = self
            .reader
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(RetainedReader {
            id,
            reader: Reader::Capture { owner, cancel },
        }) = reader.as_ref()
        else {
            anyhow::bail!("CaptureSql source generation differs")
        };
        ensure!(id == source, "CaptureSql source generation differs");
        owner.schema_objects().inspect_err(|_| {
            if !cancel.load(Ordering::Acquire) {
                self.fail_sources();
            }
        })
    }

    fn source_rows(
        &self,
        source: &str,
        handle: String,
        cursor: Option<Vec<Cell>>,
        limit: usize,
    ) -> Result<TableValue> {
        self.check_source()?;
        let reader = self
            .reader
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(RetainedReader {
            id,
            reader: Reader::Capture { owner, cancel },
        }) = reader.as_ref()
        else {
            anyhow::bail!("CaptureSql source generation differs")
        };
        ensure!(id == source, "CaptureSql source generation differs");
        owner.table_rows(handle, cursor, limit).inspect_err(|_| {
            if !cancel.load(Ordering::Acquire) {
                self.fail_sources();
            }
        })
    }

    fn source_current(&self, source: &str) -> Result<Current> {
        self.check_source()?;
        let reader = self
            .reader
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(RetainedReader {
            id,
            reader: Reader::Capture { owner, cancel },
        }) = reader.as_ref()
        else {
            anyhow::bail!("CaptureSql source generation differs")
        };
        ensure!(id == source, "CaptureSql source generation differs");
        owner.current().inspect_err(|_| {
            if !cancel.load(Ordering::Acquire) {
                self.fail_sources();
            }
        })
    }

    fn source_retire(&self, source: &str) -> Result<()> {
        let mut retained = self
            .reader
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            retained.as_ref().is_some_and(|reader| reader.id == source),
            "Source generation is not retained"
        );
        let reader = retained
            .take()
            .context("Source generation is not retained")?;
        drop(retained);
        reader.reader.retire().inspect_err(|_| self.fail_sources())
    }

    fn drain_sources(&self) -> Result<()> {
        Owner::drain_sources(self)
    }

    fn drain_filesystem(&self) -> Result<()> {
        Owner::drain_filesystem(self)
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.drain_checked();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::storage_volume::NativePath;
    use std::{
        sync::{Condvar, MutexGuard},
        time::{Duration, Instant},
    };

    static PROCESS_SERIAL: Mutex<()> = Mutex::new(());

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum CapabilityAck {
        SealedBegin,
        ArtifactBegin,
        ArtifactMember,
    }

    impl CapabilityAck {
        pub(super) fn for_sealed(request: &LightroomSealedRead) -> Option<Self> {
            matches!(request, LightroomSealedRead::Begin { .. }).then_some(Self::SealedBegin)
        }

        pub(super) fn for_artifact(request: &LightroomArtifactPreparation) -> Option<Self> {
            match request {
                LightroomArtifactPreparation::Begin { .. } => Some(Self::ArtifactBegin),
                LightroomArtifactPreparation::Member { .. } => Some(Self::ArtifactMember),
                _ => None,
            }
        }
    }

    #[derive(Default)]
    struct CapabilityAckState {
        reached: bool,
        released: bool,
        receipt: Option<String>,
    }

    pub(crate) struct CapabilityAckProbe {
        pub(super) target: CapabilityAck,
        state: Mutex<CapabilityAckState>,
        wake: Condvar,
    }

    impl CapabilityAckProbe {
        pub(crate) fn new(target: CapabilityAck) -> Arc<Self> {
            Arc::new(Self {
                target,
                state: Mutex::new(CapabilityAckState::default()),
                wake: Condvar::new(),
            })
        }

        pub(super) fn pause(&self, receipt: Option<&str>) {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.receipt = receipt.map(str::to_owned);
            state.reached = true;
            self.wake.notify_all();
            while !state.released {
                state = self
                    .wake
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }

        pub(crate) fn wait_reached(&self, timeout: Duration) -> Result<()> {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let (state, _) = self
                .wake
                .wait_timeout_while(state, timeout, |state| !state.reached)
                .unwrap_or_else(|error| error.into_inner());
            ensure!(state.reached, "capability acknowledgement probe timed out");
            Ok(())
        }

        pub(crate) fn release(&self) {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .released = true;
            self.wake.notify_all();
        }

        pub(crate) fn receipt(&self) -> Option<String> {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .receipt
                .clone()
        }
    }

    impl Drop for CapabilityAckProbe {
        fn drop(&mut self) {
            self.release();
        }
    }

    pub(crate) struct ManagedFixture {
        pub(crate) filesystem: Arc<FilesystemClient>,
        pub(crate) owner: Arc<Owner>,
    }
    impl ManagedFixture {
        pub(crate) fn start(temp: &Path) -> Result<Self> {
            let config = super::super::Config {
                worker_executable: std::env::current_exe()?,
                cache_root: None,
                original_roots: Vec::new(),
                preview_policy: Default::default(),
                preview_limits: Default::default(),
                limits: Default::default(),
                import_checkpoint: None,
            };
            let requirement = super::super::lightroom_capacity::Requirement::from_config(
                &config,
                super::super::desktop::CONTROL_SLOTS,
            )?;
            let metadata = crate::preview::ByteBudget::new(requirement.bytes())?;
            let reservation = metadata.reserve_exact(requirement.bytes())?;
            let allocation = super::super::lightroom_capacity::Allocation::from_subgrant(
                requirement,
                reservation,
                crate::preview::ByteBudget::new(
                    super::super::lightroom_capacity::source_requirement()?,
                )?,
            )?;
            let temp = std::fs::canonicalize(temp)?;
            let filesystem = Arc::new(crate::filesystem_worker::client::migration_fixture(&temp)?);
            filesystem.wait_ready(Duration::from_secs(20))?;
            let owner = Owner::start(&filesystem, &std::env::current_exe()?, allocation)?;
            Ok(Self { filesystem, owner })
        }
        fn drain(&self) -> Result<()> {
            self.owner.drain_checked()?;
            self.filesystem.try_shutdown()
        }
    }
    impl Drop for ManagedFixture {
        fn drop(&mut self) {
            if self.drain().is_err() {
                std::mem::forget(self.filesystem.clone());
                std::mem::forget(self.owner.clone());
            }
        }
    }

    fn process_serial() -> MutexGuard<'static, ()> {
        PROCESS_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn open_managed_plan(generation: &Generation, parent: &Path) -> Result<()> {
        let parent = std::fs::canonicalize(parent)?;
        let attempt = uuid::Uuid::new_v4().to_string();
        let status = match generation.call(lightroom_bridge::Request::Open {
            attempt: attempt.clone(),
            root: NativePath::from_path(&parent.join("inspection")),
            mode: crate::application::lightroom::OpenMode::Create,
            capture_staging: NativePath::from_path(&parent),
            limits: crate::application::lightroom::Limits::default().into(),
        })? {
            lightroom_bridge::Response::Status(Some(status)) => status,
            _ => anyhow::bail!("managed Workbench open reply kind"),
        };
        let workbench = status.workbench;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = match generation.call(lightroom_bridge::Request::Status {
                workbench: Some(workbench.clone()),
                attempt: Some(attempt.clone()),
            })? {
                lightroom_bridge::Response::Status(Some(status)) => status,
                _ => anyhow::bail!("managed Workbench status reply kind"),
            };
            if status.initialized {
                return Ok(());
            }
            ensure!(
                status.error.is_none() && Instant::now() < deadline,
                "managed Workbench plan did not initialize: {status:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn seal_request(action: &str) -> LightroomWorkbenchIo {
        match action {
            "publish" => LightroomWorkbenchIo::SealPublish {
                operation: "operation".into(),
                workbench: "workbench".into(),
                generation: "generation".into(),
                token: "seal".into(),
            },
            "status" => LightroomWorkbenchIo::SealStatus {
                operation: "operation".into(),
                workbench: "workbench".into(),
                generation: "generation".into(),
                token: "seal".into(),
            },
            _ => unreachable!(),
        }
    }

    fn seal_reply(state: LightroomWorkbenchSealState) -> LightroomWorkbenchIoReply {
        LightroomWorkbenchIoReply::SealState {
            operation: "operation".into(),
            token: "seal".into(),
            state,
            directory: NativePath::from_path(Path::new("/retained/seal")),
            seal_path: NativePath::from_path(Path::new("/retained/seal/input-seal.json")),
            approval_path: NativePath::from_path(Path::new("/retained/seal/approval.json")),
            seal_blake3: None,
        }
    }

    #[test]
    fn seal_custody_survives_nonterminal_reply_until_exact_status() {
        let mut custody = Custody {
            root: Some(FIdentity {
                operation: "operation".into(),
                workbench: "workbench".into(),
                generation: "generation".into(),
            }),
            seal: Some(FResource {
                identity: FIdentity {
                    operation: "operation".into(),
                    workbench: "workbench".into(),
                    generation: "generation".into(),
                },
                resource: "seal".into(),
            }),
            ..Default::default()
        };

        custody.after(
            &seal_request("publish"),
            &seal_reply(LightroomWorkbenchSealState::Staging),
        );
        assert!(custody.seal.is_some());
        assert!(custody.root.is_some());

        custody.after(
            &seal_request("status"),
            &seal_reply(LightroomWorkbenchSealState::Published),
        );
        assert!(custody.seal.is_none());
        assert!(custody.root.is_some());

        let release = LightroomWorkbenchIo::RootRelease {
            operation: "operation".into(),
            workbench: "workbench".into(),
            generation: "generation".into(),
        };
        custody.after(
            &release,
            &LightroomWorkbenchIoReply::Released {
                operation: "operation".into(),
            },
        );
        assert!(custody.root.is_none());
    }

    #[test]
    fn real_generation_checked_shutdown_reaps_w_then_f() -> Result<()> {
        let _serial = process_serial();
        let temp = tempfile::tempdir()?;
        let fixture = ManagedFixture::start(temp.path())?;
        let generation = Generation::start_fixture(&fixture.owner, &std::env::current_exe()?)?;
        let pid = generation.pid().context("Workbench fixture PID")?;
        assert!(matches!(
            generation.call(lightroom_bridge::Request::Options {})?,
            lightroom_bridge::Response::Options(_)
        ));

        generation.shutdown_checked()?;
        assert!(generation.pid().is_none());
        assert!(fixture.owner.workbench_reaped.load(Ordering::Acquire));
        fixture.drain()?;
        #[cfg(unix)]
        {
            assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        Ok(())
    }

    #[test]
    fn opened_plan_shutdown_serves_filesystem_callbacks_until_checked_reap() -> Result<()> {
        let _serial = process_serial();
        let temp = tempfile::tempdir()?;
        let fixture = ManagedFixture::start(temp.path())?;
        let generation = Generation::start_fixture(&fixture.owner, &std::env::current_exe()?)?;
        open_managed_plan(&generation, temp.path())?;
        let pid = generation.pid().context("Workbench fixture PID")?;

        generation.shutdown_checked()?;

        assert!(generation.pid().is_none());
        assert!(fixture.owner.workbench_reaped.load(Ordering::Acquire));
        fixture.drain()?;
        #[cfg(unix)]
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        Ok(())
    }

    #[test]
    fn poisoned_live_workbench_revokes_before_filesystem_release() -> Result<()> {
        let _serial = process_serial();
        let temp = tempfile::tempdir()?;
        let fixture = ManagedFixture::start(temp.path())?;
        let generation = Generation::start_fixture(&fixture.owner, &std::env::current_exe()?)?;
        open_managed_plan(&generation, temp.path())?;
        let pid = generation.pid().context("Workbench fixture PID")?;
        generation
            .workbench
            .poison_for_test("injected retained fatal SQL owner");

        let failure = generation.shutdown_checked().unwrap_err();

        assert!(failure.to_string().contains("poisoned Workbench"));
        assert!(generation.pid().is_none());
        assert!(fixture.owner.workbench_reaped.load(Ordering::Acquire));
        fixture.drain()?;
        #[cfg(unix)]
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        Ok(())
    }

    #[test]
    fn failed_w_startup_retains_retryable_dependencies() -> Result<()> {
        let _serial = process_serial();
        let temp = tempfile::tempdir()?;
        let fixture = ManagedFixture::start(temp.path())?;
        let missing = temp.path().join("missing-workbench-executable");
        let failure = match Generation::start(&fixture.owner, &missing) {
            Ok(_) => anyhow::bail!("missing Workbench executable was admitted"),
            Err(failure) => failure,
        };
        assert!(!format!("{failure:#}").is_empty());
        fixture.owner.admit()?;
        fixture.drain()?;
        Ok(())
    }

    #[test]
    fn injected_f_loss_revokes_commit_authority_and_admission() -> Result<()> {
        let _serial = process_serial();
        let temp = tempfile::tempdir()?;
        let fixture = ManagedFixture::start(temp.path())?;
        fixture.owner.inject_f_loss_at_next_commit();
        let failure = fixture.owner.commit().unwrap_err();
        assert!(failure.to_string().contains("injected F loss"));
        assert!(fixture.owner.admit().is_err());
        fixture.drain()?;
        Ok(())
    }

    #[test]
    fn filesystem_reconciliation_waits_for_checked_w_reap() -> Result<()> {
        let _serial = process_serial();
        let temp = tempfile::tempdir()?;
        let fixture = ManagedFixture::start(temp.path())?;
        fixture
            .owner
            .workbench_started
            .store(true, Ordering::Release);
        fixture.owner.drain_sources()?;
        let failure = fixture.owner.drain_filesystem().unwrap_err();
        assert!(failure.to_string().contains("W is checked-reaped"));
        assert_eq!(fixture.filesystem.status().phase, FilesystemPhase::Ready);

        fixture.owner.workbench_reaped();
        fixture.owner.drain_filesystem()?;
        fixture.filesystem.try_shutdown()?;
        Ok(())
    }

    #[test]
    fn fatal_w_transport_drains_before_return_and_never_readmits() -> Result<()> {
        let _serial = process_serial();
        let temp = tempfile::tempdir()?;
        let fixture = ManagedFixture::start(temp.path())?;
        let generation = Generation::start_fixture(&fixture.owner, &std::env::current_exe()?)?;
        let pid = generation.pid().context("Workbench fixture PID")?;
        generation.workbench.terminate_for_test()?;

        let failure = generation
            .call(lightroom_bridge::Request::Options {})
            .unwrap_err();
        assert!(
            format!("{failure:#}").contains("checked-drained"),
            "fatal result escaped before exact drain: {failure:#}"
        );
        assert!(generation.pid().is_none());
        assert!(fixture.owner.workbench_reaped.load(Ordering::Acquire));
        assert!(
            generation
                .call(lightroom_bridge::Request::Options {})
                .is_err()
        );
        fixture.drain()?;
        #[cfg(unix)]
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        Ok(())
    }
}
