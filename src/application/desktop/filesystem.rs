//! Private C→G→F relay. G owns the real F process; C holds only this proxy.
//! This is additive until every managed actor dependency exists.
mod export_native;
mod native;
use crate::{
    application::U64,
    catalog_backup::RestoreStatus,
    catalog_session::{
        CatalogBootstrap, CatalogFilesystem, ConfirmSqlAdmission, ExportAliasFactReply,
        ExportAliasFactRequest, ExportDestinationSnapshotReply, ExportDestinationSnapshotRequest,
        ExportOriginalReply, ExportOriginalRequest, ExportProfileReply, ExportProfileRequest,
        ExportPublicationReply, ExportPublicationRequest, InspectExportOriginal,
        InspectedExportOriginal, LeaseId, MigrationIdentityReply, MigrationIdentityRequest,
        PrepareCatalog, PrepareExportDirectory, PreparedExportDirectory, RootCapability,
        SqlAdmissionConfirmed, store,
    },
    filesystem_worker::{
        client::Client,
        wire::{AdmissionSnapshot, Failure, FailureKind},
    },
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    io::Write,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub(super) const BYTES: usize = 1024 * 1024;
pub(super) const CONTROL_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    pub nonce: LeaseId,
    pub epoch: LeaseId,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "arguments", deny_unknown_fields)]
pub(super) enum Call {
    Prepare(PrepareCatalog),
    Abandon {
        operation: U64,
        session: LeaseId,
    },
    Confirm(Box<ConfirmSqlAdmission>),
    RestoreStatus(RootCapability),
    Resume {
        root: RootCapability,
        restore_id: String,
        acknowledge: bool,
    },
    Release(RootCapability),
    PreviewStore(Box<store::Request>),
    PreviewIo(Box<crate::catalog_session::preview_io::Request>),
    PreviewStage(Box<crate::catalog_session::preview_stage::Request>),
    ExportStage(Box<crate::catalog_session::export_stage::Request>),
    ExportExecutor(crate::catalog_session::export_executor::Request),
    ExportNative(Box<crate::catalog_session::export_native::Request>),
    Native(Box<crate::catalog_session::native::Request>),
    ReadPreviewConfiguration(NativePath),
    PrepareExportDirectory(Box<PrepareExportDirectory>),
    ExportDestinationSnapshot(Box<ExportDestinationSnapshotRequest>),
    MigrationIdentity(Box<MigrationIdentityRequest>),
    ExportAliasFact(Box<ExportAliasFactRequest>),
    Storage(Box<crate::catalog_session::storage::Request>),
    InspectExportOriginal(Box<InspectExportOriginal>),
    ExportOriginal(Box<ExportOriginalRequest>),
    ExportPublication(Box<ExportPublicationRequest>),
    ExportProfile(Box<ExportProfileRequest>),
}
impl Call {
    fn cleanup(&self) -> bool {
        matches!(self, Self::Abandon { .. } | Self::Release(_))
            || matches!(self, Self::PreviewStore(request) if request.is_cleanup())
            || matches!(self, Self::PreviewIo(request) if request.cleanup())
            || matches!(self, Self::PreviewStage(request) if request.cleanup())
            || matches!(self, Self::ExportStage(request) if request.cleanup())
            || matches!(self, Self::ExportExecutor(request) if request.cleanup())
            || matches!(self, Self::ExportProfile(request) if request.cleanup())
            || matches!(self, Self::ExportOriginal(request) if request.cleanup())
            || matches!(self, Self::ExportPublication(request) if request.cleanup())
    }
    fn cancellable(&self) -> bool {
        matches!(
            self,
            Self::Prepare(_)
                | Self::Confirm(_)
                | Self::ReadPreviewConfiguration(_)
                | Self::PrepareExportDirectory(_)
                | Self::ExportDestinationSnapshot(_)
                | Self::MigrationIdentity(_)
                | Self::ExportAliasFact(_)
                | Self::Storage(_)
                | Self::InspectExportOriginal(_)
        ) || matches!(self, Self::PreviewStore(request) if !request.is_cleanup())
            || matches!(self, Self::PreviewIo(request) if !request.cleanup())
            || matches!(self, Self::PreviewStage(request) if !request.cleanup())
            || matches!(self, Self::ExportStage(request) if !request.cleanup())
            || matches!(self, Self::ExportExecutor(request) if !request.cleanup())
            || matches!(self, Self::ExportProfile(request) if !request.cleanup())
            || matches!(self, Self::ExportOriginal(request) if !request.cleanup())
            || matches!(self, Self::ExportPublication(request) if !request.cleanup())
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::Prepare(r) => r.validate(),
            Self::Confirm(r) => {
                ensure!(r.operation.0 != 0, "empty confirmation operation");
                Ok(())
            }
            Self::Abandon { operation, .. } => {
                ensure!(operation.0 != 0, "empty preparation operation");
                Ok(())
            }
            Self::Resume { restore_id, .. } => {
                ensure!(restore_id.len() <= 128, "restore identity limit");
                uuid::Uuid::parse_str(restore_id)?;
                Ok(())
            }
            Self::Native(request) => {
                ensure!(
                    !request.cleanup(),
                    "native cleanup requires reserved control"
                );
                request.validate()
            }
            Self::ExportNative(request) => {
                ensure!(
                    !request.cleanup(),
                    "export native cleanup requires reserved control"
                );
                request.validate()
            }
            Self::PreviewStore(request) => request.validate(),
            Self::PreviewIo(request) => request.validate(),
            Self::PreviewStage(request) => {
                ensure!(
                    !request.supervisor,
                    "C cannot assert supervisor stage transitions"
                );
                request.validate()
            }
            Self::ExportStage(request) => {
                ensure!(
                    !request.supervisor && !request.privileged(),
                    "C cannot assert export native custody transitions"
                );
                request.validate()
            }
            Self::ExportExecutor(request) => request.validate(),
            Self::ReadPreviewConfiguration(path) => store::path(path),
            Self::PrepareExportDirectory(request) => request.validate(),
            Self::ExportDestinationSnapshot(request) => request.validate(),
            Self::MigrationIdentity(request) => request.validate(),
            Self::ExportAliasFact(request) => request.validate(),
            Self::Storage(request) => request.validate(),
            Self::InspectExportOriginal(request) => request.validate(),
            Self::ExportOriginal(request) => request.validate(),
            Self::ExportPublication(request) => request.validate(),
            Self::ExportProfile(request) => request.validate(),
            _ => Ok(()),
        }
    }
}
pub(super) fn maximum_boxed_call_root_bytes() -> usize {
    let single = [
        std::mem::size_of::<ConfirmSqlAdmission>(),
        std::mem::size_of::<store::Request>(),
        std::mem::size_of::<crate::catalog_session::preview_io::Request>(),
        std::mem::size_of::<crate::catalog_session::preview_stage::Request>(),
        std::mem::size_of::<crate::catalog_session::export_stage::Request>(),
        std::mem::size_of::<crate::catalog_session::export_executor::Request>(),
        std::mem::size_of::<crate::catalog_session::export_native::Request>(),
        std::mem::size_of::<PrepareExportDirectory>(),
        std::mem::size_of::<ExportDestinationSnapshotRequest>(),
        std::mem::size_of::<MigrationIdentityRequest>(),
        std::mem::size_of::<ExportAliasFactRequest>(),
        std::mem::size_of::<crate::catalog_session::storage::Request>(),
        std::mem::size_of::<InspectExportOriginal>(),
        std::mem::size_of::<ExportOriginalRequest>(),
        std::mem::size_of::<ExportPublicationRequest>(),
        std::mem::size_of::<ExportProfileRequest>(),
    ]
    .into_iter()
    .max()
    .unwrap();
    single
        .max(
            std::mem::size_of::<crate::catalog_session::native::Request>()
                + std::mem::size_of::<crate::preview::RenderWork>(),
        )
        .max(
            std::mem::size_of::<crate::catalog_session::export_native::Request>()
                + std::mem::size_of::<crate::catalog_session::export_stage::Request>()
                + std::mem::size_of::<crate::catalog_exports::ExportWork>(),
        )
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
#[allow(
    clippy::large_enum_variant,
    reason = "inline replies preserve the explicitly bounded relay root without an unaccounted heap owner"
)]
pub(super) enum Value {
    Bootstrap(CatalogBootstrap),
    Confirmed(SqlAdmissionConfirmed),
    Restore(Option<RestoreStatus>),
    PreviewStore(store::Reply),
    PreviewIo(crate::catalog_session::preview_io::Reply),
    PreviewStage(crate::catalog_session::preview_stage::Reply),
    ExportStage(crate::catalog_session::export_stage::Reply),
    ExportExecutor(crate::catalog_session::export_executor::Reply),
    ExportNative(crate::catalog_session::export_native::Status),
    Native(crate::catalog_session::native::Status),
    Configuration(Vec<u8>),
    ExportDirectory(PreparedExportDirectory),
    ExportDestinationSnapshot(ExportDestinationSnapshotReply),
    MigrationIdentity(MigrationIdentityReply),
    ExportAliasFact(ExportAliasFactReply),
    Storage(crate::catalog_session::storage::Reply),
    InspectedExportOriginal(InspectedExportOriginal),
    ExportOriginal(ExportOriginalReply),
    ExportPublication(ExportPublicationReply),
    ExportProfile(ExportProfileReply),
    Unit,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fault {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    busy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    object_receipt: Option<crate::catalog_session::preview_io::FailureReceipt>,
    message: String,
    unknown: bool,
    kind: FailureKind,
}
impl Fault {
    fn new(message: impl std::fmt::Display, unknown: bool) -> Self {
        let kind = if unknown {
            FailureKind::Unknown
        } else {
            FailureKind::Rejected
        };
        let failure = Failure::new(kind, message);
        Self {
            busy: false,
            object_receipt: None,
            message: failure.message,
            unknown,
            kind,
        }
    }
    fn from_error(error: anyhow::Error, unknown: bool) -> Self {
        let kind = if let Some(failure) = error.downcast_ref::<Failure>() {
            failure.kind
        } else if error.downcast_ref::<store::ResourceLimit>().is_some()
            || error.downcast_ref::<crate::preview::ByteLimit>().is_some()
        {
            FailureKind::ResourceLimit
        } else if unknown {
            FailureKind::Unknown
        } else {
            FailureKind::Rejected
        };
        // Preserve delivered typed failures even if the F status changes after
        // publication. A terminal ResourceLimit is not transport uncertainty.
        let object_receipt = error
            .downcast_ref::<Failure>()
            .and_then(|f| f.object_receipt);
        let failure = Failure::new(kind, error);
        Self {
            busy: false,
            object_receipt,
            message: failure.message,
            unknown: kind == FailureKind::Unknown,
            kind,
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.busy || (self.kind == FailureKind::Rejected && self.object_receipt.is_none()),
            "relay Busy authority mismatch"
        );
        ensure!(
            self.message.len() <= 4096 && self.unknown == (self.kind == FailureKind::Unknown),
            "relay failure bounds/category mismatch"
        );
        if let Some(receipt) = &self.object_receipt {
            receipt.validate()?;
        }
        Ok(())
    }
    fn into_error(self) -> anyhow::Error {
        if self.busy {
            return crate::preview::stage_io::Busy("export native lifecycle busy; retry").into();
        }
        anyhow::Error::new(Failure {
            object_receipt: self.object_receipt,
            kind: self.kind,
            message: self.message,
        })
    }
}
type Outcome = std::result::Result<Value, Fault>;
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
pub(super) enum Control {
    ExportNativeStop {
        key: crate::catalog_session::export_native::Key,
    },
    ExportNativeQuery {
        id: U64,
        query: crate::catalog_session::export_native::Query,
    },
    ExportNativeReply {
        id: U64,
        query: crate::catalog_session::export_native::Query,
        value: std::result::Result<crate::catalog_session::export_native::Status, Fault>,
    },
    NativeStop {
        key: crate::catalog_session::native::Key,
    },
    NativeQuery {
        id: U64,
        query: crate::catalog_session::native::Query,
    },
    NativeReply {
        id: U64,
        query: crate::catalog_session::native::Query,
        value: std::result::Result<crate::catalog_session::native::Status, Fault>,
    },
    Failed(Fault),
    Cancel {
        id: U64,
    },
    Ack {
        id: U64,
        digest: String,
    },
    Status {
        id: U64,
    },
    State {
        id: U64,
        retained: bool,
    },
    Admission {
        id: U64,
        operation: U64,
        session: LeaseId,
    },
    StoreStatus {
        id: U64,
        query: store::StatusQuery,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
#[expect(
    clippy::large_enum_variant,
    reason = "inline packet bodies are part of the relay root accounted before encoding"
)]
pub(super) enum Body {
    Call {
        id: U64,
        call: Call,
    },
    Reply {
        id: U64,
        outcome: Outcome,
    },
    AdmissionReply {
        id: U64,
        value: std::result::Result<Option<AdmissionSnapshot>, Fault>,
    },
    StoreReply {
        id: U64,
        value: std::result::Result<store::Status, Fault>,
    },
    Control(Control),
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Packet {
    binding: Binding,
    body: Body,
}

/// Count before allocating the one exact encoded buffer. This is an encoded
/// transport bound, not a claim about NativePath's aggregate decoded allocation.
fn encode(value: &impl Serialize, cap: usize) -> Result<Vec<u8>> {
    struct Count {
        n: usize,
        cap: usize,
        exceeded: bool,
    }
    impl Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let Some(next) = self.n.checked_add(b.len()).filter(|n| *n <= self.cap) else {
                self.exceeded = true;
                return Err(std::io::Error::other("relay encoded byte admission"));
            };
            self.n = next;
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count {
        n: 0,
        cap,
        exceeded: false,
    };
    let counted = serde_json::to_writer(&mut count, value);
    if count.exceeded {
        return Err(Failure::new(
            FailureKind::ResourceLimit,
            "Filesystem relay message exceeds its encoded byte limit; no request was dispatched",
        )
        .into());
    }
    counted?;
    let mut bytes = Vec::with_capacity(count.n);
    serde_json::to_writer(&mut bytes, value)?;
    ensure!(bytes.len() == count.n, "relay serialization changed");
    Ok(bytes)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lane {
    Control,
    Data,
    Admission,
    Store,
}
pub(super) struct Out {
    pub lane: Lane,
    pub bytes: Arc<Vec<u8>>,
}
#[derive(Default)]
struct Slot {
    entries: VecDeque<(u64, Arc<Vec<u8>>)>,
}
impl Slot {
    fn push(&mut self, id: u64, bytes: Arc<Vec<u8>>, cap: usize) -> Result<()> {
        if let Some((_, old)) = self.entries.iter().find(|(key, _)| *key == id) {
            ensure!(
                old.as_slice() == bytes.as_slice(),
                "altered duplicate relay control"
            );
            return Ok(());
        }
        ensure!(self.entries.len() < cap, "relay control class capacity");
        self.entries.push_back((id, bytes));
        Ok(())
    }
    fn take(&mut self) -> Option<Arc<Vec<u8>>> {
        self.entries.pop_front().map(|(_, bytes)| bytes)
    }
}
#[derive(Default)]
struct Output {
    failed: Option<Arc<Vec<u8>>>,
    cancel: Slot,
    ack: Slot,
    status: Slot,
    state: Slot,
    query: Slot,
    admission: Slot,
    store_query: Slot,
    store_reply: Slot,
    native_stop: Slot,
    native_query: Slot,
    native_reply: Slot,
    export_native_stop: Slot,
    export_native_query: Slot,
    export_native_reply: Slot,
    data: VecDeque<Arc<Vec<u8>>>,
}
impl Output {
    fn push(&mut self, binding: &Binding, body: Body) -> Result<()> {
        // Classify by borrowed scalars before moving the potentially large body.
        let class = match &body {
            Body::Control(Control::ExportNativeStop { key }) => (13, key.operation.0),
            Body::Control(Control::ExportNativeQuery { id, .. }) => (14, id.0),
            Body::Control(Control::ExportNativeReply { id, .. }) => (15, id.0),
            Body::Control(Control::NativeStop { key }) => (10, key.operation.0),
            Body::Control(Control::NativeQuery { id, .. }) => (11, id.0),
            Body::Control(Control::NativeReply { id, .. }) => (12, id.0),
            Body::Control(Control::Failed(_)) => (0, 0),
            Body::Control(Control::Cancel { id }) => (1, id.0),
            Body::Control(Control::Ack { id, .. }) => (2, id.0),
            Body::Control(Control::Status { id }) => (3, id.0),
            Body::Control(Control::State { id, .. }) => (4, id.0),
            Body::Control(Control::Admission { id, .. }) => (5, id.0),
            Body::Control(Control::StoreStatus { id, .. }) => (6, id.0),
            Body::AdmissionReply { id, .. } => (7, id.0),
            Body::StoreReply { id, .. } => (8, id.0),
            _ => (9, 0),
        };
        let control = class.0 < 7 || class.0 >= 10;
        let bytes = Arc::new(encode_packet(
            &Packet {
                binding: binding.clone(),
                body,
            },
            if control { CONTROL_BYTES } else { BYTES },
        )?);
        match class {
            (0, _) => {
                self.failed.get_or_insert(bytes);
                Ok(())
            }
            (1, id) => self.cancel.push(id, bytes, 2),
            (2, id) => self.ack.push(id, bytes, 2),
            (3, id) => self.status.push(id, bytes, 2),
            (4, id) => self.state.push(id, bytes, 2),
            (5, id) => self.query.push(id, bytes, 1),
            (6, id) => self.store_query.push(id, bytes, 1),
            (7, id) => self.admission.push(id, bytes, 1),
            (8, id) => self.store_reply.push(id, bytes, 1),
            (10, id) => self.native_stop.push(id, bytes, 16),
            (11, id) => self.native_query.push(id, bytes, 1),
            (12, id) => self.native_reply.push(id, bytes, 1),
            (13, id) => self.export_native_stop.push(id, bytes, 16),
            (14, id) => self.export_native_query.push(id, bytes, 1),
            (15, id) => self.export_native_reply.push(id, bytes, 1),
            _ => self.encoded(bytes),
        }
    }

    fn encoded(&mut self, bytes: Arc<Vec<u8>>) -> Result<()> {
        if self.data.iter().any(|queued| Arc::ptr_eq(queued, &bytes)) {
            return Ok(());
        }
        ensure!(self.data.len() < 2, "relay ordinary output capacity");
        self.data.push_back(bytes);
        Ok(())
    }
    fn next(&mut self, lane: Lane) -> Option<Out> {
        let bytes = match lane {
            Lane::Control => self
                .failed
                .take()
                .or_else(|| self.export_native_stop.take())
                .or_else(|| self.export_native_query.take())
                .or_else(|| self.export_native_reply.take())
                .or_else(|| self.native_stop.take())
                .or_else(|| self.native_query.take())
                .or_else(|| self.native_reply.take())
                .or_else(|| self.cancel.take())
                .or_else(|| self.ack.take())
                .or_else(|| self.query.take())
                .or_else(|| self.store_query.take())
                .or_else(|| self.state.take())
                .or_else(|| self.status.take()),
            Lane::Data => self.data.pop_front(),
            Lane::Admission => self.admission.take(),
            Lane::Store => self.store_reply.take(),
        }?;
        Some(Out { lane, bytes })
    }
}
fn encode_packet(packet: &Packet, cap: usize) -> Result<Vec<u8>> {
    let binary = match &packet.body {
        Body::Call {
            call: Call::ExportStage(r),
            ..
        } => (!r.binary().is_empty()).then(|| r.binary()),
        Body::Call {
            call: Call::PreviewStage(r),
            ..
        } => (!r.binary().is_empty()).then(|| r.binary()),
        Body::Call {
            call: Call::PreviewIo(r),
            ..
        } => r.binary(),
        Body::Reply {
            outcome: Ok(Value::PreviewStage(r)),
            ..
        } => (!r.binary().is_empty()).then(|| r.binary()),
        Body::Reply {
            outcome: Ok(Value::PreviewIo(r)),
            ..
        } => r.binary(),
        Body::Reply {
            outcome: Ok(Value::ExportProfile(r)),
            ..
        } => r.binary(),
        _ => return encode(packet, cap),
    };
    crate::catalog_session::preview_io::pack(packet, binary, cap)
}

fn decode(binding: &Binding, bytes: &[u8], lane: Lane) -> Result<Body> {
    ensure!(
        bytes.len()
            <= if lane == Lane::Control {
                CONTROL_BYTES
            } else {
                BYTES
            },
        "relay input byte admission"
    );
    let (mut packet, binary): (Packet, _) =
        crate::catalog_session::preview_io::unpack(bytes, BYTES)?;
    match &mut packet.body {
        Body::Call {
            call: Call::ExportStage(r),
            ..
        } => r.set_binary(binary.to_vec())?,
        Body::Call {
            call: Call::PreviewStage(r),
            ..
        } => r.set_binary(binary.to_vec())?,
        Body::Call {
            call: Call::PreviewIo(r),
            ..
        } => r.set_binary(binary)?,
        Body::Reply {
            outcome: Ok(Value::PreviewStage(r)),
            ..
        } => r.set_binary(binary.to_vec())?,
        Body::Reply {
            outcome: Ok(Value::PreviewIo(r)),
            ..
        } => r.set_binary(binary)?,
        Body::Reply {
            outcome: Ok(Value::ExportProfile(r)),
            ..
        } => r.set_binary(binary)?,
        _ => ensure!(binary.is_empty(), "unexpected relay binary trailer"),
    }
    ensure!(&packet.binding == binding, "relay nonce/epoch mismatch");
    let actual = match packet.body {
        Body::Control(_) => Lane::Control,
        Body::AdmissionReply { .. } => Lane::Admission,
        Body::StoreReply { .. } => Lane::Store,
        _ => Lane::Data,
    };
    ensure!(actual == lane, "relay frame class mismatch");
    match &packet.body {
        Body::Reply {
            outcome: Err(fault),
            ..
        }
        | Body::AdmissionReply {
            value: Err(fault), ..
        }
        | Body::StoreReply {
            value: Err(fault), ..
        }
        | Body::Control(Control::Failed(fault)) => fault.validate()?,
        _ => {}
    }
    Ok(packet.body)
}

struct Pending {
    id: u64,
    call: Call,
    cancel: Arc<AtomicBool>,
    digest: String,
}
struct Retained {
    id: u64,
    request: String,
    digest: String,
    bytes: Arc<Vec<u8>>,
}
enum ReadQuery {
    Admission(u64, U64, LeaseId),
    Store(u64, store::StatusQuery),
}
struct ParentState {
    export_native_next: u64,
    export_native_result: Option<(u64, crate::catalog_session::export_native::Query, Body)>,
    native_next: u64,
    native_result: Option<(u64, crate::catalog_session::native::Query, Body)>,
    next: u64,
    queue: VecDeque<Pending>,
    active: Option<(u64, Arc<AtomicBool>, String)>,
    retained: Option<Retained>,
    acknowledged: Option<(u64, String)>,
    admission: Option<(u64, U64, LeaseId)>,
    admission_busy: Option<(u64, U64, LeaseId)>,
    admission_next: u64,
    admission_result: Option<(u64, U64, LeaseId, Arc<Vec<u8>>)>,
    store_query: Option<(u64, store::StatusQuery)>,
    store_busy: Option<(u64, store::StatusQuery)>,
    store_next: u64,
    store_result: Option<(u64, store::StatusQuery, Arc<Vec<u8>>)>,
    output: Output,
    closing: bool,
    stop: bool,
    fault: Option<Fault>,
    early: Vec<u64>,
    original: Option<PrepareCatalog>,
    original_retired: bool,
    retiring: bool,
}
#[cfg(test)]
type Observer = Arc<dyn Fn(&Call, bool) -> Result<()> + Send + Sync>;
pub(super) struct Parent {
    pub binding: Binding,
    client: Arc<Client>,
    native: Mutex<Option<Arc<super::native::Owner>>>,
    export_native: Mutex<Option<Arc<super::export_native::Owner>>>,
    state: Mutex<ParentState>,
    wake: Condvar,
    threads: Mutex<Vec<thread::JoinHandle<()>>>,
    #[cfg(test)]
    pub observer: Mutex<Option<Observer>>,
    // Last: named parent relay backings must drop before their allowance owner.
    metadata: Mutex<Option<super::preview_metadata_admission::ProcessReservation>>,
}
impl Parent {
    pub(super) fn lightroom_sealed_read(
        &self,
        request: &crate::filesystem_worker::wire::LightroomSealedRead,
        cancel: &AtomicBool,
    ) -> Result<Option<crate::filesystem_worker::wire::LightroomSealedDocumentPage>> {
        self.client.lightroom_sealed_read(request, cancel)
    }
    pub(super) fn lightroom_artifact_preparation(
        &self,
        request: &crate::filesystem_worker::wire::LightroomArtifactPreparation,
        cancel: &AtomicBool,
    ) -> Result<Option<crate::filesystem_worker::wire::LightroomArtifactPreparationReply>> {
        self.client.lightroom_artifact_preparation(request, cancel)
    }
    pub fn retain_metadata(
        &self,
        reservation: super::preview_metadata_admission::ProcessReservation,
    ) -> Result<()> {
        let mut held = self.metadata.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            held.is_none(),
            "filesystem parent already owns catalog metadata admission"
        );
        *held = Some(reservation);
        Ok(())
    }
    pub fn new(client: Arc<Client>) -> Arc<Self> {
        let this = Arc::new(Self {
            binding: Binding {
                nonce: LeaseId::new(),
                epoch: client.epoch().clone(),
            },
            client,
            native: Mutex::new(None),
            export_native: Mutex::new(None),
            state: Mutex::new(ParentState {
                export_native_next: 1,
                export_native_result: None,
                native_next: 1,
                native_result: None,
                next: 1,
                queue: VecDeque::new(),
                active: None,
                retained: None,
                acknowledged: None,
                admission: None,
                admission_busy: None,
                admission_next: 1,
                admission_result: None,
                store_query: None,
                store_busy: None,
                store_next: 1,
                store_result: None,
                output: Output::default(),
                closing: false,
                stop: false,
                fault: None,
                early: Vec::new(),
                original: None,
                original_retired: true,
                retiring: false,
            }),
            wake: Condvar::new(),
            threads: Mutex::new(Vec::new()),
            #[cfg(test)]
            observer: Mutex::new(None),
            metadata: Mutex::new(None),
        });
        // Construction always returns the retained owner, including partial startup.
        for admission in [false, true] {
            let owner = this.clone();
            let result = thread::Builder::new()
                .name(
                    if admission {
                        "filesystem-relay-control"
                    } else {
                        "filesystem-relay-executor"
                    }
                    .into(),
                )
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if admission {
                            owner.admission_loop()
                        } else {
                            owner.run()
                        }
                    }));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => owner.fail(format_args!(
                            "filesystem relay owner failed; outcomes unknown: {error:#}"
                        )),
                        Err(_) => owner.fail("filesystem relay owner panicked; outcomes unknown"),
                    }
                });
            match result {
                Ok(thread) => this.threads.lock().unwrap().push(thread),
                Err(error) => {
                    this.fail(error);
                    break;
                }
            }
        }
        this
    }
    pub fn healthy(&self) -> Result<()> {
        let s = self.state.lock().unwrap();
        match &s.fault {
            Some(fault) => Err(fault.clone().into_error()),
            None => Ok(()),
        }
    }
    pub fn fail(&self, error: impl std::fmt::Display) {
        if let Ok(owner) = self.export_native_owner() {
            owner.closing();
        }
        if let Ok(native) = self.native_owner() {
            native.stop_all();
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let fault = Fault::new(error, true);
        s.fault.get_or_insert_with(|| fault.clone());
        s.closing = true;
        let _ = s
            .output
            .push(&self.binding, Body::Control(Control::Failed(fault)));
        if let Some((_, cancel, _)) = &s.active {
            cancel.store(true, Ordering::Release);
        }
        self.wake.notify_all();
    }
    pub fn closing(&self) {
        if let Ok(owner) = self.export_native_owner() {
            owner.closing();
        }
        if let Ok(native) = self.native_owner() {
            native.stop_all();
        }
        let mut s = self.state.lock().unwrap();
        s.closing = true;
        if let Some((_, cancel, _)) = &s.active {
            cancel.store(true, Ordering::Release);
        }
        for p in &s.queue {
            if p.call.cancellable() {
                p.cancel.store(true, Ordering::Release);
            }
        }
        self.wake.notify_all();
    }
    pub fn next(&self, lane: Lane) -> Option<Out> {
        self.state.lock().unwrap().output.next(lane)
    }
    pub fn receive(&self, bytes: &[u8], lane: Lane) -> Result<()> {
        let body = decode(&self.binding, bytes, lane)?;
        if self.receive_export_native(&body)? {
            return Ok(());
        }
        if self.receive_native(&body)? {
            return Ok(());
        }
        let mut s = self.state.lock().unwrap();
        match body {
            Body::Call { id, call } => {
                call.validate()?;
                if let Call::PreviewStore(request) = &call {
                    ensure!(
                        request.root.epoch == self.binding.epoch,
                        "preview store epoch mismatch"
                    );
                }
                if let Call::PreviewIo(request) = &call {
                    ensure!(
                        request.root.epoch == self.binding.epoch,
                        "cache object epoch mismatch"
                    );
                }
                ensure!(id.0 > 0, "relay call identity");
                let digest = blake3::hash(bytes).to_hex().to_string();
                if let Some(r) = &s.retained
                    && r.id == id.0
                {
                    ensure!(r.request == digest, "altered relay duplicate");
                    let bytes = r.bytes.clone();
                    s.output.encoded(bytes)?;
                    return Ok(());
                }
                if let Some((active, _, expected)) = &s.active
                    && *active == id.0
                {
                    ensure!(expected == &digest, "altered active relay call");
                    return Ok(());
                }
                if let Some(p) = s.queue.iter().find(|p| p.id == id.0) {
                    ensure!(p.digest == digest, "altered queued relay call");
                    return Ok(());
                }
                ensure!(id.0 == s.next, "relay call replay/gap");
                ensure!(!s.stop && s.fault.is_none(), "relay unavailable");
                ensure!(!s.closing || call.cleanup(), "relay is draining");
                ensure!(
                    s.queue.len()
                        + usize::from(s.active.is_some())
                        + usize::from(s.retained.is_some())
                        < 2,
                    "relay call capacity"
                );
                s.next = s
                    .next
                    .checked_add(1)
                    .context("relay call identifier exhausted")?;
                let early = s
                    .early
                    .iter()
                    .position(|v| *v == id.0)
                    .map(|i| s.early.remove(i))
                    .is_some();
                if let Call::Prepare(request) = &call
                    && s.original_retired
                {
                    s.original = Some(request.clone());
                    s.original_retired = false;
                }
                s.queue.push_back(Pending {
                    id: id.0,
                    call,
                    cancel: Arc::new(AtomicBool::new(early)),
                    digest,
                });
            }
            Body::Control(Control::Cancel { id }) => {
                if s.acknowledged.as_ref().is_some_and(|r| r.0 == id.0) {
                    return Ok(());
                }
                if let Some(p) = s.queue.iter().find(|p| p.id == id.0) {
                    if p.call.cancellable() {
                        p.cancel.store(true, Ordering::Release);
                    }
                } else if let Some((active, cancel, _)) = &s.active {
                    ensure!(*active == id.0, "unowned relay cancel");
                    cancel.store(true, Ordering::Release);
                } else if id.0 >= s.next && id.0.saturating_sub(s.next) < 2 {
                    if !s.early.contains(&id.0) {
                        ensure!(s.early.len() < 2, "relay early cancel capacity");
                        s.early.push(id.0);
                    }
                } else {
                    ensure!(
                        s.retained.as_ref().is_some_and(|r| r.id == id.0)
                            || s.acknowledged.as_ref().is_some_and(|r| r.0 == id.0),
                        "unowned relay cancel"
                    );
                }
            }
            Body::Control(Control::Ack { id, digest }) => {
                if s.acknowledged.as_ref() == Some(&(id.0, digest.clone())) {
                    return Ok(());
                }
                if let Some(r) = &s.retained {
                    ensure!(
                        r.id == id.0 && r.digest == digest,
                        "relay result acknowledgement mismatch"
                    );
                    s.retained = None;
                    s.acknowledged = Some((id.0, digest));
                } else {
                    ensure!(
                        s.acknowledged.as_ref() == Some(&(id.0, digest)),
                        "unowned relay acknowledgement"
                    );
                }
            }
            Body::Control(Control::Status { id }) => {
                if let Some(r) = &s.retained
                    && r.id == id.0
                {
                    let bytes = r.bytes.clone();
                    s.output.encoded(bytes)?;
                    return Ok(());
                }
                ensure!(
                    s.active.as_ref().is_some_and(|a| a.0 == id.0)
                        || s.queue.iter().any(|p| p.id == id.0)
                        || (id.0 >= s.next && id.0.saturating_sub(s.next) < 2)
                        || s.acknowledged.as_ref().is_some_and(|r| r.0 == id.0),
                    "unknown relay call"
                );
                s.output.push(
                    &self.binding,
                    Body::Control(Control::State {
                        id,
                        retained: false,
                    }),
                )?;
            }
            Body::Control(Control::Admission {
                id,
                operation,
                session,
            }) => {
                ensure!(id.0 > 0 && operation.0 > 0, "admission query identity");
                let query = (id.0, operation, session);
                for pending in [s.admission.as_ref(), s.admission_busy.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    if pending.0 == id.0 {
                        ensure!(pending == &query, "altered admission query");
                        return Ok(());
                    }
                }
                if let Some((old, op, scope, bytes)) = &s.admission_result
                    && *old == id.0
                {
                    ensure!(
                        *op == operation && scope == &query.2,
                        "altered admission query result"
                    );
                    let bytes = bytes.clone();
                    s.output.admission.push(id.0, bytes, 1)?;
                    return Ok(());
                }
                ensure!(id.0 == s.admission_next, "admission query replay/gap");
                ensure!(
                    s.admission.is_none() && s.admission_busy.is_none(),
                    "relay admission query capacity"
                );
                s.admission_next = s
                    .admission_next
                    .checked_add(1)
                    .context("admission query ID exhausted")?;
                s.admission_result = None;
                s.admission = Some(query);
            }
            Body::Control(Control::StoreStatus { id, query }) => {
                query.validate()?;
                ensure!(
                    id.0 > 0 && query.epoch == self.binding.epoch,
                    "store status query epoch/identity"
                );
                let request = (id.0, query);
                for pending in [s.store_query.as_ref(), s.store_busy.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    if pending.0 == id.0 {
                        ensure!(pending == &request, "altered store status query");
                        return Ok(());
                    }
                }
                if let Some((old, expected, bytes)) = &s.store_result
                    && *old == id.0
                {
                    ensure!(expected == &request.1, "altered store status result query");
                    let bytes = bytes.clone();
                    s.output.store_reply.push(id.0, bytes, 1)?;
                    return Ok(());
                }
                ensure!(id.0 == s.store_next, "store status query replay/gap");
                ensure!(
                    s.store_query.is_none() && s.store_busy.is_none(),
                    "store status query capacity"
                );
                s.store_next = s
                    .store_next
                    .checked_add(1)
                    .context("store status query ID exhausted")?;
                s.store_result = None;
                s.store_query = Some(request);
            }
            _ => anyhow::bail!("unexpected C filesystem relay message"),
        }
        self.wake.notify_all();
        Ok(())
    }
    fn run(&self) -> Result<()> {
        loop {
            let pending = {
                let mut s = self.state.lock().unwrap();
                loop {
                    if s.stop {
                        return Ok(());
                    }
                    if s.fault.is_none()
                        && s.retained.is_none()
                        && let Some(p) = s.queue.pop_front()
                    {
                        s.active = Some((p.id, p.cancel.clone(), p.digest.clone()));
                        break p;
                    }
                    s = self.wake.wait(s).unwrap();
                }
            };
            let dispatched = if pending.call.cancellable()
                && !matches!(pending.call, Call::ExportStage(_))
                && pending.cancel.load(Ordering::Acquire)
            {
                Err(anyhow::Error::new(Failure::new(
                    FailureKind::Canceled,
                    "filesystem relay canceled before dispatch",
                )))
            } else {
                self.invoke(&pending.call, &pending.cancel)
            };
            let result = dispatched.map_err(|e| {
                Fault::from_error(
                    e,
                    self.client.status().phase == crate::filesystem_worker::wire::Phase::Unknown,
                )
            });
            let retired = matches!(pending.call, Call::Abandon { .. } | Call::Release(_))
                && matches!(&result, Ok(Value::Unit));
            let bytes = Arc::new(encode_packet(
                &Packet {
                    binding: self.binding.clone(),
                    body: Body::Reply {
                        id: U64(pending.id),
                        outcome: result,
                    },
                },
                BYTES,
            )?);
            let digest = blake3::hash(&bytes).to_hex().to_string();
            let mut s = self.state.lock().unwrap();
            s.active = None;
            if retired {
                s.original_retired = true;
            }
            s.retained = Some(Retained {
                id: pending.id,
                request: pending.digest,
                digest,
                bytes: bytes.clone(),
            });
            s.output.encoded(bytes)?;
            self.wake.notify_all();
        }
    }
    fn invoke(&self, call: &Call, cancel: &AtomicBool) -> Result<Value> {
        #[cfg(test)]
        if let Some(observer) = self.observer.lock().unwrap().clone() {
            observer(call, false)?;
        }
        let result = (|| -> Result<Value> {
            Ok(match call {
                Call::Prepare(r) => Value::Bootstrap(self.client.prepare_catalog(r, cancel)?),
                Call::Abandon { operation, session } => {
                    self.client.abandon_prepare(*operation, session)?;
                    Value::Unit
                }
                Call::Confirm(r) => {
                    let confirmed = self.client.confirm_sql_admission(r, cancel)?;
                    if let Ok(owner) = self.export_native_owner() {
                        owner.bind(&confirmed.root)?;
                    }
                    if let Ok(native) = self.native_owner() {
                        native.bind(&confirmed.root)?;
                    }
                    Value::Confirmed(confirmed)
                }
                Call::RestoreStatus(r) => Value::Restore(self.client.restore_status(r)?),
                Call::Resume {
                    root,
                    restore_id,
                    acknowledge,
                } => Value::Restore(Some(self.client.resume_restored_jobs(
                    root,
                    restore_id,
                    *acknowledge,
                )?)),
                Call::Release(r) => {
                    if let Ok(owner) = self.export_native_owner() {
                        owner.retire_root(r)?;
                    }
                    if let Ok(native) = self.native_owner() {
                        native.retire_root(r)?;
                    }
                    self.client.release_root(r)?;
                    if let Ok(owner) = self.export_native_owner() {
                        owner.forget_released_root(r)?;
                    }
                    if let Ok(native) = self.native_owner() {
                        native.forget_released_root(r)?;
                    }
                    Value::Unit
                }
                Call::Native(request) => Value::Native(self.native_owner()?.call(request)?),
                Call::ExportNative(request) => {
                    Value::ExportNative(self.export_native_owner()?.call(request)?)
                }
                Call::PreviewStage(request) => {
                    Value::PreviewStage(self.client.preview_stage_call(request, cancel)?)
                }
                Call::ExportStage(request) => {
                    Value::ExportStage(self.export_native_owner()?.stage_call(request, cancel)?)
                }
                Call::ExportExecutor(request) => Value::ExportExecutor(
                    self.export_native_owner()?.executor_call(request, cancel)?,
                ),
                Call::PreviewIo(request) => {
                    Value::PreviewIo(self.client.preview_io_call(request, cancel)?)
                }
                Call::PreviewStore(request) => {
                    Value::PreviewStore(self.client.preview_store_call(request, cancel)?)
                }
                Call::ReadPreviewConfiguration(path) => {
                    Value::Configuration(self.client.read_preview_configuration(path, cancel)?)
                }
                Call::PrepareExportDirectory(request) => {
                    Value::ExportDirectory(self.client.prepare_export_directory(request, cancel)?)
                }
                Call::ExportDestinationSnapshot(request) => Value::ExportDestinationSnapshot(
                    self.client.export_destination_snapshot(request, cancel)?,
                ),
                Call::MigrationIdentity(request) => {
                    Value::MigrationIdentity(self.client.migration_identity(request, cancel)?)
                }
                Call::Storage(request) => {
                    Value::Storage(self.client.storage_call(request, cancel)?)
                }
                Call::ExportAliasFact(request) => {
                    Value::ExportAliasFact(self.client.export_alias_fact(request, cancel)?)
                }
                Call::InspectExportOriginal(request) => Value::InspectedExportOriginal(
                    self.client.inspect_export_original(request, cancel)?,
                ),
                Call::ExportOriginal(request) => {
                    Value::ExportOriginal(self.client.export_original_call(request, cancel)?)
                }
                Call::ExportPublication(request) => {
                    Value::ExportPublication(self.client.export_publication_call(request, cancel)?)
                }
                Call::ExportProfile(request) => {
                    Value::ExportProfile(self.client.export_profile_call(request, cancel)?)
                }
            })
        })();
        #[cfg(test)]
        if let Some(observer) = self.observer.lock().unwrap().clone() {
            observer(call, true)?;
        }
        result
    }
    fn admission_loop(&self) -> Result<()> {
        // One existing read/control owner services two independently retained
        // query classes. Alternate when both are waiting; neither joins the
        // mutating executor queue or consumes the other's snapshot slot.
        let mut store_turn = false;
        loop {
            let query = {
                let mut s = self.state.lock().unwrap();
                loop {
                    if s.stop {
                        return Ok(());
                    }
                    let status = self.client.status();
                    let phase = status.phase;
                    if !s.retiring
                        && s.fault.is_none()
                        && matches!(
                            phase,
                            crate::filesystem_worker::wire::Phase::Unknown
                                | crate::filesystem_worker::wire::Phase::Stopped
                                | crate::filesystem_worker::wire::Phase::DrainFailed
                        )
                    {
                        drop(s);
                        if let Some(error) = status.error {
                            self.fail(format_args!(
                                "filesystem owner failed; C must drain: {error}"
                            ));
                        } else {
                            self.fail(format_args!(
                                "filesystem owner phase {phase:?}; C must drain"
                            ));
                        }
                        s = self.state.lock().unwrap();
                    }
                    if (store_turn || s.admission.is_none())
                        && let Some(query) = s.store_query.take()
                    {
                        s.store_busy = Some(query.clone());
                        store_turn = false;
                        break ReadQuery::Store(query.0, query.1);
                    }
                    if let Some(query) = s.admission.take() {
                        s.admission_busy = Some(query.clone());
                        store_turn = true;
                        break ReadQuery::Admission(query.0, query.1, query.2);
                    }
                    s = self
                        .wake
                        .wait_timeout(s, Duration::from_millis(20))
                        .unwrap()
                        .0;
                }
            };
            match query {
                ReadQuery::Admission(id, operation, session) => {
                    let value = self
                        .client
                        .admission_status(operation, &session)
                        .map_err(|e| Fault::from_error(e, true));
                    let bytes = Arc::new(encode_packet(
                        &Packet {
                            binding: self.binding.clone(),
                            body: Body::AdmissionReply { id: U64(id), value },
                        },
                        BYTES,
                    )?);
                    let mut s = self.state.lock().unwrap();
                    s.admission_busy = None;
                    s.admission_result = Some((id, operation, session, bytes.clone()));
                    s.output.admission.push(id, bytes, 1)?;
                }
                ReadQuery::Store(id, query) => {
                    let value = self
                        .client
                        .store_status(&query)
                        .and_then(|status| {
                            status.validate(&query)?;
                            Ok(status)
                        })
                        .map_err(|e| Fault::from_error(e, true));
                    let bytes = Arc::new(encode_packet(
                        &Packet {
                            binding: self.binding.clone(),
                            body: Body::StoreReply { id: U64(id), value },
                        },
                        BYTES,
                    )?);
                    let mut s = self.state.lock().unwrap();
                    s.store_busy = None;
                    s.store_result = Some((id, query, bytes.clone()));
                    s.output.store_reply.push(id, bytes, 1)?;
                }
            }
        }
    }
    /// Only after C and every dependent native owner has been verified drained.
    /// F retirement releases any blocked F calls; only then can these threads join.
    pub fn finish_after_dependents(&self, terminate: bool) -> Result<()> {
        if let Ok(owner) = self.export_native_owner() {
            owner.finish_after_catalog()?;
        }
        if let Ok(native) = self.native_owner() {
            native.finish_after_catalog()?;
        }
        self.state.lock().unwrap().retiring = true;
        if terminate {
            self.client.terminate_after_dependents_drained()?;
        } else {
            self.client.try_shutdown()?;
        }
        {
            let mut s = self.state.lock().unwrap();
            s.stop = true;
            self.wake.notify_all();
        }
        let mut failed = false;
        for handle in self.threads.lock().unwrap().drain(..) {
            failed |= handle.join().is_err();
        }
        ensure!(!failed, "filesystem relay thread join failed");
        Ok(())
    }
}

struct ChildCall {
    id: u64,
    call: Call,
    outcome: Option<Outcome>,
    canceled: bool,
    queried: bool,
}
struct Query {
    id: u64,
    operation: U64,
    session: LeaseId,
    result: Option<std::result::Result<Option<AdmissionSnapshot>, Fault>>,
}
struct StoreQuery {
    id: u64,
    query: store::StatusQuery,
    result: Option<std::result::Result<store::Status, Fault>>,
}
struct ChildState {
    export_native_next: u64,
    export_native_query: Option<export_native::Pending>,
    export_native_last: Option<export_native::Pending>,
    export_native_dead: bool,
    native_next: u64,
    native_query: Option<native::Pending>,
    native_dead: bool,
    next: u64,
    calls: Vec<ChildCall>,
    output: Output,
    fault: Option<Fault>,
    closing: bool,
    query: Option<Query>,
    query_next: u64,
    store_query: Option<StoreQuery>,
    store_next: u64,
    completed: Option<(u64, String)>,
}
pub(super) struct Proxy {
    binding: Binding,
    state: Mutex<ChildState>,
    wake: Condvar,
}
pub(super) fn metadata_proxy_layout() -> (usize, usize) {
    (std::mem::size_of::<Proxy>(), std::mem::align_of::<Proxy>())
}
impl Proxy {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn new(binding: Binding) -> Arc<Self> {
        Arc::new(Self {
            binding,
            state: Mutex::new(ChildState {
                export_native_next: 1,
                export_native_query: None,
                export_native_last: None,
                export_native_dead: false,
                native_next: 1,
                native_query: None,
                native_dead: false,
                next: 1,
                calls: Vec::new(),
                output: Output::default(),
                fault: None,
                closing: false,
                query: None,
                query_next: 1,
                store_query: None,
                store_next: 1,
                completed: None,
            }),
            wake: Condvar::new(),
        })
    }
    pub fn next(&self, lane: Lane) -> Option<Out> {
        self.state.lock().unwrap().output.next(lane)
    }
    pub fn fail(&self, message: impl std::fmt::Display) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.fault.get_or_insert_with(|| Fault::new(message, true));
        s.export_native_dead = true;
        s.native_dead = true;
        self.wake.notify_all();
    }
    pub fn closing(&self) {
        self.state.lock().unwrap().closing = true;
        self.wake.notify_all();
    }
    pub fn receive(&self, bytes: &[u8], lane: Lane) -> Result<()> {
        let body = decode(&self.binding, bytes, lane)?;
        if self.receive_export_native_reply(&body)? {
            return Ok(());
        }
        if self.receive_native_reply(&body)? {
            return Ok(());
        }
        let mut s = self.state.lock().unwrap();
        match body {
            Body::Control(Control::Failed(fault)) => {
                s.fault.get_or_insert(fault);
            }
            Body::Reply { id, outcome } => {
                let digest = blake3::hash(bytes).to_hex().to_string();
                if let Some((completed, expected)) = &s.completed
                    && *completed == id.0
                {
                    ensure!(*expected == digest, "altered completed relay result");
                    s.output
                        .push(&self.binding, Body::Control(Control::Ack { id, digest }))?;
                    return Ok(());
                }
                let call = s
                    .calls
                    .iter_mut()
                    .find(|c| c.id == id.0)
                    .context("unowned filesystem relay result")?;
                if call.outcome.is_some() {
                    anyhow::bail!("duplicate result before adoption invariant");
                }
                if let Ok(value) = &outcome {
                    validate_reply(&call.call, value, &self.binding)?;
                }
                call.outcome = Some(outcome);
                s.completed = Some((id.0, digest.clone()));
                s.output.push(
                    &self.binding,
                    Body::Control(Control::Ack {
                        id,
                        digest: blake3::hash(bytes).to_hex().to_string(),
                    }),
                )?;
            }
            Body::Control(Control::State { id, .. }) => {
                if let Some(call) = s.calls.iter_mut().find(|c| c.id == id.0) {
                    call.queried = false;
                } else {
                    ensure!(
                        s.completed.as_ref().is_some_and(|c| c.0 == id.0),
                        "unowned relay status"
                    );
                }
            }
            Body::AdmissionReply { id, value } => {
                let q = s.query.as_mut().context("unowned admission reply")?;
                ensure!(
                    q.id == id.0 && q.result.is_none(),
                    "admission reply identity"
                );
                if let Ok(Some(snapshot)) = &value {
                    ensure!(
                        snapshot.operation == q.operation && snapshot.session == q.session,
                        "admission snapshot scope mismatch"
                    );
                }
                q.result = Some(value);
            }
            Body::StoreReply { id, value } => {
                let query = s
                    .store_query
                    .as_mut()
                    .context("unowned store status reply")?;
                ensure!(
                    query.id == id.0 && query.result.is_none(),
                    "store status reply identity"
                );
                if let Ok(status) = &value {
                    status.validate(&query.query)?;
                }
                query.result = Some(value);
            }
            _ => anyhow::bail!("unexpected G filesystem relay message"),
        }
        self.wake.notify_all();
        Ok(())
    }
    fn call(&self, call: Call, cancel: &AtomicBool) -> Result<Value> {
        call.validate()?;
        #[cfg(test)]
        if matches!(call, Call::Release(_)) {
            crate::catalog_session::overlap_tests::before_release()?;
        }
        let mut s = self.state.lock().unwrap();
        if let Some(fault) = &s.fault {
            return Err(fault.clone().into_error());
        }
        ensure!(!s.closing || call.cleanup(), "filesystem relay closing");
        ensure!(
            s.calls.len() < 2,
            crate::preview::stage_io::Busy("filesystem relay busy")
        );
        let id = s.next;
        let next = id.checked_add(1).context("relay ID exhausted")?;
        s.output.push(
            &self.binding,
            Body::Call {
                id: U64(id),
                call: call.clone(),
            },
        )?;
        s.next = next;
        s.calls.push(ChildCall {
            id,
            call: call.clone(),
            outcome: None,
            canceled: false,
            queried: false,
        });
        let mut status_at = Instant::now();
        loop {
            let index = s.calls.iter().position(|c| c.id == id).unwrap();
            if let Some(outcome) = s.calls[index].outcome.take() {
                s.calls.remove(index);
                return outcome.map_err(Fault::into_error);
            }
            if let Some(fault) = &s.fault {
                return Err(fault.clone().into_error());
            }
            if call.cancellable() && cancel.load(Ordering::Acquire) && !s.calls[index].canceled {
                s.output.push(
                    &self.binding,
                    Body::Control(Control::Cancel { id: U64(id) }),
                )?;
                s.calls[index].canceled = true;
            }
            if !s.calls[index].queried && status_at.elapsed() >= Duration::from_millis(250) {
                s.output.push(
                    &self.binding,
                    Body::Control(Control::Status { id: U64(id) }),
                )?;
                s.calls[index].queried = true;
                status_at = Instant::now();
            }
            s = self
                .wake
                .wait_timeout(s, Duration::from_millis(20))
                .unwrap()
                .0;
        }
    }
    pub fn admission_status(
        &self,
        operation: U64,
        session: &LeaseId,
    ) -> Result<Option<AdmissionSnapshot>> {
        let mut s = self.state.lock().unwrap();
        ensure!(s.query.is_none(), "relay admission query busy");
        let id = s.query_next;
        s.query_next = id.checked_add(1).context("relay query exhausted")?;
        s.output.push(
            &self.binding,
            Body::Control(Control::Admission {
                id: U64(id),
                operation,
                session: session.clone(),
            }),
        )?;
        s.query = Some(Query {
            id,
            operation,
            session: session.clone(),
            result: None,
        });
        loop {
            if s.query.as_ref().is_some_and(|q| q.result.is_some()) {
                let query = s.query.take().unwrap();
                return query.result.unwrap().map_err(Fault::into_error);
            }
            if let Some(f) = &s.fault {
                return Err(f.clone().into_error());
            }
            s = self.wake.wait(s).unwrap();
        }
    }
}
impl CatalogFilesystem for Proxy {
    fn export_executor_call(
        &self,
        request: &crate::catalog_session::export_executor::Request,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::export_executor::Reply> {
        ensure!(
            request.root.epoch == self.binding.epoch,
            "export executor relay authority"
        );
        match self.call(Call::ExportExecutor(request.clone()), cancel)? {
            Value::ExportExecutor(reply) => {
                reply.validate(request)?;
                Ok(reply)
            }
            _ => anyhow::bail!("unexpected export executor relay reply"),
        }
    }
    fn native(&self) -> Option<&dyn crate::catalog_session::native::CatalogNative> {
        Some(self)
    }
    fn export_native(
        &self,
    ) -> Option<&dyn crate::catalog_session::export_native::CatalogExportNative> {
        Some(self)
    }
    fn preview_stage_call(
        &self,
        request: &crate::catalog_session::preview_stage::Request,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::preview_stage::Reply> {
        ensure!(
            request.root.epoch == self.binding.epoch && !request.supervisor,
            "stage relay authority"
        );
        match self.call(Call::PreviewStage(Box::new(request.clone())), cancel)? {
            Value::PreviewStage(reply) => {
                reply.validate(request)?;
                Ok(reply)
            }
            _ => anyhow::bail!("unexpected stage relay reply"),
        }
    }
    fn export_stage_call(
        &self,
        request: &crate::catalog_session::export_stage::Request,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::export_stage::Reply> {
        ensure!(
            request.root.epoch == self.binding.epoch
                && !request.supervisor
                && !request.privileged(),
            "export stage relay authority"
        );
        match self.call(Call::ExportStage(Box::new(request.clone())), cancel)? {
            Value::ExportStage(reply) => {
                reply.validate(request)?;
                Ok(reply)
            }
            _ => anyhow::bail!("unexpected export stage relay reply"),
        }
    }
    fn preview_io_call(
        &self,
        request: &crate::catalog_session::preview_io::Request,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::preview_io::Reply> {
        ensure!(
            request.root.epoch == self.binding.epoch,
            "cache IO epoch mismatch"
        );
        match self.call(Call::PreviewIo(Box::new(request.clone())), cancel)? {
            Value::PreviewIo(reply) => {
                reply.validate(request)?;
                Ok(reply)
            }
            _ => anyhow::bail!("unexpected cache IO reply"),
        }
    }

    fn prepare_catalog(&self, r: &PrepareCatalog, c: &AtomicBool) -> Result<CatalogBootstrap> {
        match self.call(Call::Prepare(r.clone()), c)? {
            Value::Bootstrap(v) => {
                ensure!(
                    v.operation == r.operation
                        && v.session == r.session
                        && v.epoch == self.binding.epoch,
                    "bootstrap identity mismatch"
                );
                Ok(v)
            }
            _ => anyhow::bail!("wrong prepare reply"),
        }
    }
    fn abandon_prepare(&self, operation: U64, session: &LeaseId) -> Result<()> {
        unit(self.call(
            Call::Abandon {
                operation,
                session: session.clone(),
            },
            &AtomicBool::new(false),
        )?)
    }
    fn confirm_sql_admission(
        &self,
        r: &ConfirmSqlAdmission,
        c: &AtomicBool,
    ) -> Result<SqlAdmissionConfirmed> {
        match self.call(Call::Confirm(Box::new(r.clone())), c)? {
            Value::Confirmed(v) => {
                ensure!(&v == r, "confirmation mismatch");
                Ok(v)
            }
            _ => anyhow::bail!("wrong confirmation reply"),
        }
    }
    fn restore_status(&self, r: &RootCapability) -> Result<Option<RestoreStatus>> {
        match self.call(Call::RestoreStatus(r.clone()), &AtomicBool::new(false))? {
            Value::Restore(v) => Ok(v),
            _ => anyhow::bail!("wrong restore reply"),
        }
    }
    fn resume_restored_jobs(
        &self,
        r: &RootCapability,
        id: &str,
        ack: bool,
    ) -> Result<RestoreStatus> {
        match self.call(
            Call::Resume {
                root: r.clone(),
                restore_id: id.into(),
                acknowledge: ack,
            },
            &AtomicBool::new(false),
        )? {
            Value::Restore(Some(v)) => Ok(v),
            _ => anyhow::bail!("wrong resume reply"),
        }
    }
    fn release_root(&self, r: &RootCapability) -> Result<()> {
        unit(self.call(Call::Release(r.clone()), &AtomicBool::new(false))?)
    }
    fn preview_store_call(
        &self,
        request: &store::Request,
        cancel: &AtomicBool,
    ) -> Result<store::Reply> {
        ensure!(
            request.root.epoch == self.binding.epoch,
            "preview store epoch mismatch"
        );
        match self.call(Call::PreviewStore(Box::new(request.clone())), cancel)? {
            Value::PreviewStore(reply) => {
                store::validate_reply(request, &reply)?;
                Ok(reply)
            }
            _ => anyhow::bail!("wrong preview store reply"),
        }
    }
    fn preview_store_status(&self, query: &store::Query) -> Result<store::Status> {
        store::path(&query.root.canonical_root)?;
        let query = store::StatusQuery::from(query);
        query.validate()?;
        ensure!(
            query.epoch == self.binding.epoch,
            "store status query epoch mismatch"
        );
        let mut s = self.state.lock().unwrap();
        if let Some(fault) = &s.fault {
            return Err(fault.clone().into_error());
        }
        ensure!(s.store_query.is_none(), "relay store status query busy");
        let id = s.store_next;
        let next = id
            .checked_add(1)
            .context("store status query ID exhausted")?;
        s.output.push(
            &self.binding,
            Body::Control(Control::StoreStatus {
                id: U64(id),
                query: query.clone(),
            }),
        )?;
        s.store_next = next;
        s.store_query = Some(StoreQuery {
            id,
            query,
            result: None,
        });
        loop {
            if s.store_query.as_ref().is_some_and(|q| q.result.is_some()) {
                let query = s.store_query.take().unwrap();
                return query.result.unwrap().map_err(Fault::into_error);
            }
            if let Some(fault) = &s.fault {
                return Err(fault.clone().into_error());
            }
            s = self.wake.wait(s).unwrap();
        }
    }
    fn read_preview_configuration(
        &self,
        path: &NativePath,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        // Immutable relay call ID plus F Client sequence bind the selected path;
        // configuration reads grant no root/store authority or replay token.
        match self.call(Call::ReadPreviewConfiguration(path.clone()), cancel)? {
            Value::Configuration(bytes) => {
                ensure!(
                    bytes.len() <= store::CONFIG_BYTES,
                    "preview configuration response byte limit"
                );
                Ok(bytes)
            }
            _ => anyhow::bail!("wrong preview configuration reply"),
        }
    }
    fn prepare_export_directory(
        &self,
        request: &PrepareExportDirectory,
        cancel: &AtomicBool,
    ) -> Result<PreparedExportDirectory> {
        match self.call(
            Call::PrepareExportDirectory(Box::new(request.clone())),
            cancel,
        )? {
            Value::ExportDirectory(value) => {
                value.validate_for(request)?;
                Ok(value)
            }
            _ => anyhow::bail!("wrong export directory reply"),
        }
    }
    fn export_destination_snapshot(
        &self,
        request: &ExportDestinationSnapshotRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportDestinationSnapshotReply> {
        match self.call(
            Call::ExportDestinationSnapshot(Box::new(request.clone())),
            cancel,
        )? {
            Value::ExportDestinationSnapshot(value) => {
                value.validate_for(request)?;
                Ok(value)
            }
            _ => anyhow::bail!("wrong export destination snapshot reply"),
        }
    }
    fn migration_identity(
        &self,
        request: &MigrationIdentityRequest,
        cancel: &AtomicBool,
    ) -> Result<MigrationIdentityReply> {
        match self.call(Call::MigrationIdentity(Box::new(request.clone())), cancel)? {
            Value::MigrationIdentity(value) => {
                value.validate_for(request)?;
                Ok(value)
            }
            _ => anyhow::bail!("unexpected migration identity relay response"),
        }
    }
    fn storage_call(
        &self,
        request: &crate::catalog_session::storage::Request,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::storage::Reply> {
        match self.call(Call::Storage(Box::new(request.clone())), cancel)? {
            Value::Storage(value) => {
                value.validate(request)?;
                Ok(value)
            }
            _ => anyhow::bail!("unexpected storage observation relay response"),
        }
    }
    fn export_alias_fact(
        &self,
        request: &ExportAliasFactRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportAliasFactReply> {
        match self.call(Call::ExportAliasFact(Box::new(request.clone())), cancel)? {
            Value::ExportAliasFact(value) => {
                value.validate_for(request)?;
                Ok(value)
            }
            _ => anyhow::bail!("wrong export alias fact reply"),
        }
    }
    fn export_profile_call(
        &self,
        request: &ExportProfileRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportProfileReply> {
        match self.call(Call::ExportProfile(Box::new(request.clone())), cancel)? {
            Value::ExportProfile(value) => {
                value.validate(request)?;
                Ok(value)
            }
            _ => anyhow::bail!("wrong export profile reply"),
        }
    }
    fn inspect_export_original(
        &self,
        request: &InspectExportOriginal,
        cancel: &AtomicBool,
    ) -> Result<InspectedExportOriginal> {
        match self.call(
            Call::InspectExportOriginal(Box::new(request.clone())),
            cancel,
        )? {
            Value::InspectedExportOriginal(value) => {
                value.validate_for(request)?;
                Ok(value)
            }
            _ => anyhow::bail!("wrong export original inspection reply"),
        }
    }
    fn export_original_call(
        &self,
        request: &ExportOriginalRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportOriginalReply> {
        match self.call(Call::ExportOriginal(Box::new(request.clone())), cancel)? {
            Value::ExportOriginal(value) => {
                value.validate(request)?;
                Ok(value)
            }
            _ => anyhow::bail!("wrong export original reply"),
        }
    }
    fn export_publication_call(
        &self,
        request: &ExportPublicationRequest,
        cancel: &AtomicBool,
    ) -> Result<ExportPublicationReply> {
        match self.call(Call::ExportPublication(Box::new(request.clone())), cancel)? {
            Value::ExportPublication(value) => {
                value.validate(request)?;
                Ok(value)
            }
            _ => anyhow::bail!("wrong export publication reply"),
        }
    }
}
fn unit(v: Value) -> Result<()> {
    ensure!(matches!(v, Value::Unit), "wrong unit reply");
    Ok(())
}

fn validate_reply(call: &Call, value: &Value, binding: &Binding) -> Result<()> {
    match (call, value) {
        (Call::ExportNative(request), Value::ExportNative(status)) => status.validate(request)?,
        (Call::Native(r), Value::Native(v)) => v.validate(&r.root, r.operation)?,
        (Call::Prepare(r), Value::Bootstrap(v)) => {
            v.validate()?;
            ensure!(
                v.operation == r.operation && v.session == r.session && v.epoch == binding.epoch,
                "relay bootstrap authority mismatch"
            );
        }
        (Call::Confirm(r), Value::Confirmed(v)) => {
            ensure!(v == r.as_ref(), "relay confirmation authority mismatch")
        }
        (Call::Abandon { .. } | Call::Release(_), Value::Unit) => {}
        (Call::RestoreStatus(_), Value::Restore(_)) => {}
        (Call::PreviewIo(request), Value::PreviewIo(reply)) => reply.validate(request)?,
        (Call::PreviewStage(request), Value::PreviewStage(reply)) => reply.validate(request)?,
        (Call::ExportStage(request), Value::ExportStage(reply)) => reply.validate(request)?,
        (Call::ExportExecutor(request), Value::ExportExecutor(reply)) => reply.validate(request)?,
        (Call::PreviewStore(request), Value::PreviewStore(reply)) => {
            store::validate_reply(request, reply)?
        }
        (Call::ReadPreviewConfiguration(_), Value::Configuration(bytes)) => ensure!(
            bytes.len() <= store::CONFIG_BYTES,
            "preview configuration response byte limit"
        ),
        (Call::PrepareExportDirectory(request), Value::ExportDirectory(value)) => {
            value.validate_for(request)?
        }
        (Call::ExportDestinationSnapshot(request), Value::ExportDestinationSnapshot(value)) => {
            value.validate_for(request)?
        }
        (Call::MigrationIdentity(request), Value::MigrationIdentity(value)) => {
            value.validate_for(request)?
        }
        (Call::Storage(request), Value::Storage(value)) => value.validate(request)?,
        (Call::ExportAliasFact(request), Value::ExportAliasFact(value)) => {
            value.validate_for(request)?
        }
        (Call::InspectExportOriginal(request), Value::InspectedExportOriginal(value)) => {
            value.validate_for(request)?
        }
        (Call::ExportOriginal(request), Value::ExportOriginal(value)) => value.validate(request)?,
        (Call::ExportPublication(request), Value::ExportPublication(value)) => {
            value.validate(request)?
        }
        (Call::ExportProfile(request), Value::ExportProfile(value)) => value.validate(request)?,
        (Call::Resume { restore_id, .. }, Value::Restore(Some(v))) => ensure!(
            &v.receipt.restore_id == restore_id && !v.jobs_held,
            "relay restored-job receipt mismatch"
        ),
        _ => anyhow::bail!("filesystem relay reply method mismatch"),
    }
    Ok(())
}

/// Retryable F-only startup error: no C Child was created on this branch.
pub(super) struct Unstarted {
    pub owner: Arc<Parent>,
    pub message: String,
}
impl std::fmt::Debug for Unstarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnstartedFilesystemRelay")
            .field("message", &self.message)
            .finish()
    }
}
impl std::fmt::Display for Unstarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; retained F startup owner", self.message)
    }
}
impl std::error::Error for Unstarted {}
impl Unstarted {
    pub fn retire(&self) -> Result<()> {
        self.owner.finish_after_dependents(true)
    }
}

pub(super) fn before_child_failure(
    filesystem: &Option<Arc<Parent>>,
    message: impl ToString,
) -> anyhow::Error {
    match filesystem {
        Some(owner) => anyhow::Error::new(Unstarted {
            owner: owner.clone(),
            message: message.to_string(),
        }),
        None => anyhow::anyhow!(message.to_string()),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod store_tests;

#[cfg(test)]
mod preview_tests;

/// Admit the complete enriched export completion through the C relay envelope.
/// IDs use maximal scalar width; UUIDs have fixed wire width.
pub(crate) fn admit_export_stage_reply(
    reply: &crate::catalog_session::export_stage::Reply,
) -> Result<()> {
    let packet = Packet {
        binding: Binding {
            nonce: LeaseId::new(),
            epoch: reply.epoch.clone(),
        },
        body: Body::Reply {
            id: U64(u64::MAX),
            outcome: Ok(Value::ExportStage(reply.clone())),
        },
    };
    encode_packet(&packet, BYTES)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn roundtrip_export_stage(
    request: &crate::catalog_session::export_stage::Request,
) -> Result<crate::catalog_session::export_stage::Request> {
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: request.root.epoch.clone(),
    };
    let packet = Packet {
        binding: binding.clone(),
        body: Body::Call {
            id: U64(u64::MAX),
            call: Call::ExportStage(Box::new(request.clone())),
        },
    };
    let encoded = encode_packet(&packet, BYTES)?;
    let Body::Call {
        call: Call::ExportStage(decoded),
        ..
    } = decode(&binding, &encoded, Lane::Data)?
    else {
        anyhow::bail!("export C packet round trip");
    };
    Call::ExportStage(decoded.clone()).validate()?;
    Ok(*decoded)
}

#[cfg(test)]
pub(crate) fn roundtrip_export_executor(
    request: &crate::catalog_session::export_executor::Request,
) -> Result<crate::catalog_session::export_executor::Request> {
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: request.root.epoch.clone(),
    };
    let packet = Packet {
        binding: binding.clone(),
        body: Body::Call {
            id: U64(u64::MAX),
            call: Call::ExportExecutor(request.clone()),
        },
    };
    let encoded = encode_packet(&packet, BYTES)?;
    let Body::Call {
        call: Call::ExportExecutor(decoded),
        ..
    } = decode(&binding, &encoded, Lane::Data)?
    else {
        anyhow::bail!("export executor C packet round trip");
    };
    Call::ExportExecutor(decoded.clone()).validate()?;
    Ok(decoded)
}

#[cfg(test)]
pub(crate) fn admit_export_executor_reply(
    request: &crate::catalog_session::export_executor::Request,
    reply: &crate::catalog_session::export_executor::Reply,
) -> Result<()> {
    reply.validate(request)?;
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: request.root.epoch.clone(),
    };
    let packet = Packet {
        binding: binding.clone(),
        body: Body::Reply {
            id: U64(u64::MAX),
            outcome: Ok(Value::ExportExecutor(reply.clone())),
        },
    };
    let encoded = encode_packet(&packet, BYTES)?;
    let Body::Reply {
        outcome: Ok(Value::ExportExecutor(decoded)),
        ..
    } = decode(&binding, &encoded, Lane::Data)?
    else {
        anyhow::bail!("export executor C reply round trip");
    };
    decoded.validate(request)
}
