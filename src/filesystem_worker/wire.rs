//! Private F transport. These messages carry facts, never file descriptors.
use crate::{
    application::U64,
    catalog_backup::RestoreStatus,
    catalog_session::{
        CatalogBootstrap, ConfirmSqlAdmission, ExportAliasFactReply, ExportAliasFactRequest,
        ExportDestinationSnapshotReply, ExportDestinationSnapshotRequest, ExportOriginalReply,
        ExportOriginalRequest, ExportProfileReply, ExportProfileRequest, ExportPublicationReply,
        ExportPublicationRequest, InspectExportOriginal, InspectedExportOriginal, LeaseId,
        MigrationIdentityReply, MigrationIdentityRequest, PrepareCatalog, PrepareExportDirectory,
        PreparedExportDirectory, RootCapability, SqlAdmissionConfirmed, validate_path,
    },
    storage_volume::NativePath,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

pub const CONFIG_BYTES: usize = 4 * 1024 * 1024;
pub const MESSAGE_BYTES: usize = 1024 * 1024;
pub const ERROR_BYTES: usize = 4096;
pub const CHUNK_BYTES: usize = 16 * 1024;
const HEADER_BYTES: usize = 48;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Startup {
    pub epoch: LeaseId,
    pub build: String,
    pub original_roots: Vec<NativePath>,
}
impl Startup {
    pub fn new(original_roots: Vec<NativePath>) -> Result<Self> {
        let value = Self {
            epoch: LeaseId::new(),
            build: build_identity(),
            original_roots,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.build == build_identity(),
            "filesystem helper build mismatch"
        );
        encode(self, CONFIG_BYTES)?;
        for path in &self.original_roots {
            validate_path(path)?;
        }
        Ok(())
    }
    pub(crate) fn nonce(&self) -> [u8; 16] {
        *uuid::Uuid::parse_str(self.epoch.as_str())
            .expect("validated LeaseId")
            .as_bytes()
    }
}
/// Identity of this explicit source/dependency set, not an executable-file hash.
pub fn build_identity() -> String {
    blake3::hash(
        concat!(
            env!("CARGO_PKG_VERSION"),
            include_str!("../catalog_row.rs"),
            include_str!("../catalog_edits.rs"),
            include_str!("../catalog_images.rs"),
            include_str!("../catalog_metadata.rs"),
            include_str!("wire.rs"),
            include_str!("../preview/store_custody.rs"),
            include_str!("../preview/store_io.rs"),
            include_str!("../preview/relocation.rs"),
            include_str!("client.rs"),
            include_str!("process.rs"),
            include_str!("../catalog_session.rs"),
            include_str!("../catalog_session/store.rs"),
            include_str!("../catalog_session/preview_io.rs"),
            include_str!("../catalog_session/export_managed.rs"),
            include_str!("../preview/store.rs"),
            include_str!("../filesystem_worker.rs"),
            include_str!("bootstrap.rs"),
            include_str!("store.rs"),
            include_str!("preview_io.rs"),
            include_str!("preview_stage.rs"),
            include_str!("../catalog_session/preview_stage.rs"),
            include_str!("export_stage.rs"),
            include_str!("../catalog_session/export_stage.rs"),
            include_str!("export_executor.rs"),
            include_str!("../catalog_session/export_executor.rs"),
            include_str!("../export_worker.rs"),
            include_str!("../export_worker/wire.rs"),
            include_str!("../export_worker/claim.rs"),
            include_str!("../photo_render.rs"),
            include_str!("../catalog_session/native.rs"),
            include_str!("../preview/stage_io.rs"),
            include_str!("../preview/prepared_cache.rs"),
            include_str!("../preview/worker.rs"),
            include_str!("../preview/worker/managed.rs"),
            include_str!("../preview/worker/managed_process.rs"),
            include_str!("../preview/worker/managed_transport.rs"),
            include_str!("../preview/worker/read_transport.rs"),
            include_str!("../preview/transport_task.rs"),
            include_str!("../catalog_backup.rs"),
            include_str!("../lib.rs"),
            include_str!("../catalog_storage.rs"),
            include_str!("../metadata_export.rs"),
            include_str!("../metadata_export/photo_phases.rs"),
            include_str!("../catalog_exports.rs"),
            include_str!("../catalog_exports/control.rs"),
            include_str!("../storage_volume.rs"),
            include_str!("../../Cargo.lock")
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "command",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
// Only one active and one queued request exist. Keep the fixed eight-role
// confirmation inline instead of adding a separate allocation to every decode.
#[allow(clippy::large_enum_variant)]
pub enum Operation {
    ExportExecutor(crate::catalog_session::export_executor::Request),
    PreviewStore(crate::catalog_session::store::Request),
    PreviewIo(crate::catalog_session::preview_io::Request),
    PreviewStage(crate::catalog_session::preview_stage::Request),
    ExportStage(crate::catalog_session::export_stage::Request),
    ReadPreviewConfiguration(NativePath),
    PrepareExportDirectory(Box<PrepareExportDirectory>),
    ExportDestinationSnapshot(Box<ExportDestinationSnapshotRequest>),
    MigrationIdentity(Box<MigrationIdentityRequest>),
    ExportAliasFact(Box<ExportAliasFactRequest>),
    InspectExportOriginal(Box<InspectExportOriginal>),
    ExportOriginal(Box<ExportOriginalRequest>),
    ExportPublication(Box<ExportPublicationRequest>),
    ExportProfile(Box<ExportProfileRequest>),
    PrepareCatalog(PrepareCatalog),
    ConfirmSqlAdmission(ConfirmSqlAdmission),
    AbandonPrepare {
        operation: U64,
        session: LeaseId,
    },
    RestoreStatus {
        root: RootCapability,
    },
    ResumeRestoredJobs {
        root: RootCapability,
        restore_id: String,
        acknowledge_pending_jobs: bool,
    },
    RequireJobsReleased {
        root: RootCapability,
    },
    ReleaseRoot {
        root: RootCapability,
    },
    GlobalRestoreStatus {
        root: NativePath,
    },
    GlobalResumeRestoredJobs {
        root: NativePath,
        restore_id: String,
        acknowledge_pending_jobs: bool,
    },
}
impl Operation {
    pub(crate) fn is_cleanup(&self) -> bool {
        matches!(self, Self::AbandonPrepare { .. } | Self::ReleaseRoot { .. })
            || matches!(self, Self::ExportExecutor(r) if r.cleanup())
            || matches!(self, Self::ExportProfile(r) if r.cleanup())
            || matches!(self, Self::ExportOriginal(r) if r.cleanup())
            || matches!(self, Self::ExportPublication(r) if r.cleanup())
            || matches!(self, Self::PreviewStore(r) if r.is_cleanup())
            || matches!(self, Self::PreviewIo(r) if r.cleanup())
            || matches!(self, Self::PreviewStage(r) if r.cleanup())
            || matches!(self, Self::ExportStage(r) if r.cleanup())
    }
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::ExportExecutor(value) => value.validate()?,
            Self::PreviewStore(value) => value.validate()?,
            Self::PreviewIo(value) => value.validate()?,
            Self::PreviewStage(value) => value.validate()?,
            Self::ExportStage(value) => value.validate()?,
            Self::ReadPreviewConfiguration(value) => crate::catalog_session::store::path(value)?,
            Self::PrepareExportDirectory(value) => value.validate()?,
            Self::ExportDestinationSnapshot(value) => value.validate()?,
            Self::MigrationIdentity(value) => value.validate()?,
            Self::ExportAliasFact(value) => value.validate()?,
            Self::InspectExportOriginal(value) => value.validate()?,
            Self::ExportOriginal(value) => value.validate()?,
            Self::ExportPublication(value) => value.validate()?,
            Self::ExportProfile(value) => value.validate()?,
            Self::PrepareCatalog(value) => value.validate()?,
            Self::ConfirmSqlAdmission(value) => {
                validate_root(&value.root)?;
                ensure!(value.operation.0 > 0, "invalid prepare operation");
                for row in &value.roles {
                    row.physical.validate()?;
                }
            }
            Self::AbandonPrepare { operation, .. } => {
                ensure!(operation.0 > 0, "invalid prepare operation")
            }
            Self::RestoreStatus { root }
            | Self::RequireJobsReleased { root }
            | Self::ReleaseRoot { root } => validate_root(root)?,
            Self::ResumeRestoredJobs {
                root, restore_id, ..
            } => {
                validate_root(root)?;
                uuid::Uuid::parse_str(restore_id)?;
            }
            Self::GlobalRestoreStatus { root } => validate_path(root)?,
            Self::GlobalResumeRestoredJobs {
                root, restore_id, ..
            } => {
                validate_path(root)?;
                uuid::Uuid::parse_str(restore_id)?;
            }
        }
        Ok(())
    }
}
fn validate_root(root: &RootCapability) -> Result<()> {
    validate_path(&root.canonical_root)?;
    root.root_physical.validate()?;
    root.catalog_physical.validate()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionState {
    Preparing,
    Prepared,
    Confirmed,
    Abandoned,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionSnapshot {
    pub operation: U64,
    pub session: LeaseId,
    pub directory_created: bool,
    pub catalog_created: bool,
    pub manifest_created: bool,
    pub bootstrap: Option<CatalogBootstrap>,
    pub state: AdmissionState,
    pub failure: Option<Failure>,
}
impl AdmissionSnapshot {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "invalid admission snapshot operation");
        if let Some(value) = &self.bootstrap {
            value.validate()?;
            ensure!(
                value.operation == self.operation && value.session == self.session,
                "admission snapshot identity mismatch"
            );
            ensure!(
                value.catalog.created == self.catalog_created
                    && value.manifest.created == self.manifest_created,
                "admission creation facts disagree with bootstrap"
            );
        }
        ensure!(
            !matches!(
                self.state,
                AdmissionState::Prepared | AdmissionState::Confirmed
            ) || self.bootstrap.is_some(),
            "prepared admission has no bootstrap authority"
        );
        if let Some(error) = &self.failure {
            error.validate()?;
        }
        // Includes the Control wrapper budget used by the actual writer.
        encode(&Control::Admission(Some(self.clone())), MESSAGE_BYTES)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[expect(
    clippy::large_enum_variant,
    reason = "inline response variants preserve the bounded protocol root without an extra heap owner"
)]
pub enum Response {
    ExportExecutor(crate::catalog_session::export_executor::Reply),
    PreviewStore(crate::catalog_session::store::Reply),
    PreviewIo(crate::catalog_session::preview_io::Reply),
    PreviewStage(crate::catalog_session::preview_stage::Reply),
    ExportStage(crate::catalog_session::export_stage::Reply),
    PreviewConfiguration(Vec<u8>),
    ExportDirectory(PreparedExportDirectory),
    ExportDestinationSnapshot(ExportDestinationSnapshotReply),
    MigrationIdentity(MigrationIdentityReply),
    ExportAliasFact(ExportAliasFactReply),
    InspectedExportOriginal(InspectedExportOriginal),
    ExportOriginal(ExportOriginalReply),
    ExportPublication(ExportPublicationReply),
    ExportProfile(ExportProfileReply),
    Bootstrap(CatalogBootstrap),
    Confirmed(SqlAdmissionConfirmed),
    RestoreStatus(Option<RestoreStatus>),
    Released(Empty),
    Unit(Empty),
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    ResourceLimit,
    Rejected,
    Canceled,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_receipt: Option<crate::catalog_session::preview_io::FailureReceipt>,
    pub kind: FailureKind,
    pub message: String,
}
impl Failure {
    pub fn new(kind: FailureKind, message: impl std::fmt::Display) -> Self {
        struct Bounded(String);
        impl std::fmt::Write for Bounded {
            fn write_str(&mut self, text: &str) -> std::fmt::Result {
                let mut end = text.len().min(ERROR_BYTES.saturating_sub(self.0.len()));
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                self.0.push_str(&text[..end]);
                Ok(())
            }
        }
        let mut message_out = Bounded(String::with_capacity(ERROR_BYTES));
        let _ = std::fmt::write(&mut message_out, format_args!("{message}"));
        Self {
            object_receipt: None,
            kind,
            message: message_out.0.into_boxed_str().into_string(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.message.len() <= ERROR_BYTES,
            "filesystem error byte limit"
        );
        if let Some(receipt) = &self.object_receipt {
            receipt.validate()?;
        }
        Ok(())
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for Failure {}
pub type Outcome = std::result::Result<Response, Failure>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Ready,
    Stopping,
    DrainFailed,
    Stopped,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultInfo {
    pub sequence: U64,
    pub bytes: U64,
    pub blake3: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub phase: Phase,
    pub shutdown_attempt: U64,
    pub active: Option<U64>,
    pub queued: Option<U64>,
    pub retained: Option<ResultInfo>,
    /// Exact queued request removed before any Handler execution by Stop.
    pub canceled_before_execution: Option<U64>,
    pub error: Option<Failure>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
// This reserved channel retains one admission snapshot and one in-flight frame,
// not a growable collection of variants; inline storage keeps ownership simple.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Control {
    Status(Status),
    Admission(Option<AdmissionSnapshot>),
    Store(std::result::Result<crate::catalog_session::store::Status, Failure>),
    Rejected(Failure),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdmissionQuery {
    pub operation: U64,
    pub session: LeaseId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ack {
    pub sequence: U64,
    pub blake3: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    Startup = 1,
    Ready = 2,
    Execute = 3,
    Status = 4,
    AdmissionStatus = 5,
    Cancel = 6,
    Ack = 7,
    Stop = 8,
    Control = 9,
    Result = 10,
    StoreStatus = 11,
}
impl Kind {
    fn decode(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Startup),
            2 => Ok(Self::Ready),
            3 => Ok(Self::Execute),
            4 => Ok(Self::Status),
            5 => Ok(Self::AdmissionStatus),
            6 => Ok(Self::Cancel),
            7 => Ok(Self::Ack),
            8 => Ok(Self::Stop),
            9 => Ok(Self::Control),
            10 => Ok(Self::Result),
            11 => Ok(Self::StoreStatus),
            _ => Err(invalid("unknown filesystem frame kind")),
        }
    }
    fn cap(self) -> usize {
        if self == Self::Startup {
            CONFIG_BYTES
        } else {
            MESSAGE_BYTES
        }
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) struct Frame {
    pub kind: Kind,
    pub epoch: [u8; 16],
    pub sequence: u64,
    pub offset: usize,
    pub total: usize,
    pub payload: Vec<u8>,
}
impl Frame {
    pub fn read(reader: &mut impl Read) -> io::Result<Option<Self>> {
        let mut header = [0; HEADER_BYTES];
        loop {
            match reader.read(&mut header[..1]) {
                Ok(0) => return Ok(None),
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        reader.read_exact(&mut header[1..])?;
        if &header[..4] != b"PCFS" || header[4] != 1 || header[6..8] != [0, 0] {
            return Err(invalid("filesystem protocol header mismatch"));
        }
        let kind = Kind::decode(header[5])?;
        let offset = usize::try_from(u64::from_le_bytes(header[32..40].try_into().unwrap()))
            .map_err(|_| invalid("filesystem offset overflow"))?;
        let total = u32::from_le_bytes(header[40..44].try_into().unwrap()) as usize;
        let size = u32::from_le_bytes(header[44..48].try_into().unwrap()) as usize;
        if total > kind.cap()
            || size > CHUNK_BYTES
            || offset.checked_add(size).is_none_or(|end| end > total)
            || (size == 0 && total != 0)
        {
            return Err(invalid("filesystem frame byte admission"));
        }
        let mut payload = vec![0; size];
        reader.read_exact(&mut payload)?;
        Ok(Some(Self {
            kind,
            epoch: header[8..24].try_into().unwrap(),
            sequence: u64::from_le_bytes(header[24..32].try_into().unwrap()),
            offset,
            total,
            payload,
        }))
    }
    fn write(&self, writer: &mut impl Write) -> io::Result<()> {
        if self.total > self.kind.cap()
            || self.payload.len() > CHUNK_BYTES
            || self
                .offset
                .checked_add(self.payload.len())
                .is_none_or(|end| end > self.total)
            || (self.payload.is_empty() && self.total != 0)
        {
            return Err(invalid("filesystem outgoing frame byte admission"));
        }
        let mut header = [0; HEADER_BYTES];
        header[..4].copy_from_slice(b"PCFS");
        header[4] = 1;
        header[5] = self.kind as u8;
        header[8..24].copy_from_slice(&self.epoch);
        header[24..32].copy_from_slice(&self.sequence.to_le_bytes());
        header[32..40].copy_from_slice(&(self.offset as u64).to_le_bytes());
        header[40..44].copy_from_slice(&(self.total as u32).to_le_bytes());
        header[44..48].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        writer.write_all(&header)?;
        writer.write_all(&self.payload)?;
        writer.flush()
    }
}

pub(crate) struct Message {
    pub kind: Kind,
    pub sequence: u64,
    pub bytes: Vec<u8>,
}
impl Message {
    pub fn write(self, epoch: [u8; 16], writer: &mut impl Write) -> io::Result<()> {
        Self::write_bytes(self.kind, self.sequence, &self.bytes, epoch, writer)
    }
    pub fn write_bytes(
        kind: Kind,
        sequence: u64,
        bytes: &[u8],
        epoch: [u8; 16],
        writer: &mut impl Write,
    ) -> io::Result<()> {
        if bytes.len() > kind.cap() {
            return Err(invalid("filesystem message byte admission"));
        }
        let mut offset = 0;
        loop {
            let end = bytes.len().min(offset + CHUNK_BYTES);
            Frame {
                kind,
                epoch,
                sequence,
                offset,
                total: bytes.len(),
                payload: bytes[offset..end].to_vec(),
            }
            .write(writer)?;
            offset = end;
            if offset == bytes.len() {
                return Ok(());
            }
        }
    }
}

pub(crate) struct Assembly {
    kind: Kind,
    sequence: u64,
    epoch: [u8; 16],
    bytes: Vec<u8>,
    received: usize,
}
impl Assembly {
    pub fn start(frame: &Frame) -> io::Result<Self> {
        if frame.offset != 0 || frame.total > frame.kind.cap() {
            return Err(invalid("filesystem message initial frame"));
        }
        Ok(Self {
            kind: frame.kind,
            sequence: frame.sequence,
            epoch: frame.epoch,
            bytes: vec![0; frame.total],
            received: 0,
        })
    }
    pub fn push(&mut self, frame: Frame) -> io::Result<bool> {
        if frame.kind != self.kind
            || frame.sequence != self.sequence
            || frame.epoch != self.epoch
            || frame.total != self.bytes.len()
            || frame.offset != self.received
        {
            return Err(invalid("filesystem message continuity"));
        }
        let end = self
            .received
            .checked_add(frame.payload.len())
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| invalid("filesystem fragment length"))?;
        self.bytes[self.received..end].copy_from_slice(&frame.payload);
        self.received = end;
        Ok(end == self.bytes.len())
    }
    pub fn finish(self) -> io::Result<Message> {
        if self.received != self.bytes.len() {
            return Err(invalid("incomplete filesystem message"));
        }
        Ok(Message {
            kind: self.kind,
            sequence: self.sequence,
            bytes: self.bytes,
        })
    }
}

/// Count before the sole exact-sized encoded allocation; no geometric Vec growth.
pub(crate) fn encode(value: &impl Serialize, cap: usize) -> Result<Vec<u8>> {
    struct Count {
        bytes: usize,
        cap: usize,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.cap.saturating_sub(self.bytes) {
                return Err(invalid("filesystem encoded byte limit"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { bytes: 0, cap };
    serde_json::to_writer(&mut count, value)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(count.bytes)?;
    bytes.resize(count.bytes, 0);
    let mut cursor = io::Cursor::new(bytes.as_mut_slice());
    serde_json::to_writer(&mut cursor, value)?;
    ensure!(
        cursor.position() as usize == count.bytes,
        "filesystem serializer changed length"
    );
    Ok(bytes)
}
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8], cap: usize) -> Result<T> {
    ensure!(bytes.len() <= cap, "filesystem decoded envelope byte limit");
    Ok(serde_json::from_slice(bytes)?)
}

pub(crate) fn encode_operation(value: &Operation) -> Result<Vec<u8>> {
    let binary = match value {
        Operation::PreviewIo(r) => r.binary(),
        Operation::PreviewStage(r) => (!r.binary().is_empty()).then(|| r.binary()),
        Operation::ExportStage(r) => (!r.binary().is_empty()).then(|| r.binary()),
        _ => return encode(value, MESSAGE_BYTES),
    };
    crate::catalog_session::preview_io::pack(value, binary, MESSAGE_BYTES)
}
pub(crate) fn decode_operation(bytes: &[u8]) -> Result<Operation> {
    let (mut value, binary): (Operation, _) =
        crate::catalog_session::preview_io::unpack(bytes, MESSAGE_BYTES)?;
    match &mut value {
        Operation::PreviewIo(r) => r.set_binary(binary)?,
        Operation::PreviewStage(r) => r.set_binary(binary.to_vec())?,
        Operation::ExportStage(r) => r.set_binary(binary.to_vec())?,
        _ => ensure!(binary.is_empty(), "unexpected operation binary trailer"),
    }
    value.validate()?;
    Ok(value)
}
pub(crate) fn encode_outcome(value: &Outcome) -> Result<Vec<u8>> {
    let binary = match value {
        Ok(Response::PreviewIo(r)) => r.binary(),
        Ok(Response::PreviewStage(r)) => (!r.binary().is_empty()).then(|| r.binary()),
        Ok(Response::ExportProfile(r)) => r.binary(),
        _ => return encode(value, MESSAGE_BYTES),
    };
    crate::catalog_session::preview_io::pack(value, binary, MESSAGE_BYTES)
}
pub(crate) fn decode_outcome(bytes: &[u8]) -> Result<Outcome> {
    let (mut value, binary): (Outcome, _) =
        crate::catalog_session::preview_io::unpack(bytes, MESSAGE_BYTES)?;
    match &mut value {
        Ok(Response::PreviewIo(r)) => r.set_binary(binary)?,
        Ok(Response::PreviewStage(r)) => r.set_binary(binary.to_vec())?,
        Ok(Response::ExportProfile(r)) => r.set_binary(binary)?,
        _ => ensure!(binary.is_empty(), "unexpected outcome binary trailer"),
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maximum_export_executor_request_and_reply_round_trip() -> Result<()> {
        use crate::catalog_session::{PhysicalObjectId, export_executor as e};
        #[cfg(unix)]
        let physical = |index| PhysicalObjectId::Unix {
            device: U64(u64::MAX),
            inode: U64(index),
        };
        #[cfg(windows)]
        let physical = |index| PhysicalObjectId::Windows {
            volume_serial: U64(u32::MAX as u64),
            file_index: U64(index),
        };
        let mut request = e::Request {
            root: RootCapability {
                epoch: LeaseId::new(),
                token: LeaseId::new(),
                session: LeaseId::new(),
                canonical_root: crate::catalog_session::maximum_absolute_native_path_for_test(),
                root_physical: physical(u64::MAX - 1),
                catalog_physical: physical(u64::MAX),
            },
            executor: LeaseId::new(),
            operation: U64(u64::MAX),
            action: e::Action::Recover {
                max_directories: U64(e::MAX_DIRECTORIES),
            },
        };
        request.executor = e::executor_id(&request.root, u64::MAX)?;
        let encoded = encode_operation(&Operation::ExportExecutor(request.clone()))?;
        let Operation::ExportExecutor(decoded) = decode_operation(&encoded)? else {
            unreachable!()
        };
        assert_eq!(decoded.digest()?, request.digest()?);
        let reply = e::Reply {
            root: request.root.clone(),
            executor: request.executor.clone(),
            operation: request.operation,
            request_digest: request.digest()?,
            value: e::Value::Recovery {
                scanned: U64(e::MAX_DIRECTORIES),
                cleaned: U64(e::MAX_DIRECTORIES),
                retained: U64(0),
                retained_example: None,
                candidate: Some(e::Candidate {
                    token: LeaseId::new(),
                    attempt: e::Attempt {
                        job: "j".repeat(128),
                        sequence: i64::MAX,
                        attempt: "a".repeat(128),
                        authority: "f".repeat(64),
                    },
                }),
            },
        };
        reply.validate(&request)?;
        crate::application::desktop::test_export_executor_relay_admission(&request, &reply)?;
        let encoded = encode_outcome(&Ok(Response::ExportExecutor(reply.clone())))?;
        let Ok(Response::ExportExecutor(decoded)) = decode_outcome(&encoded)? else {
            unreachable!()
        };
        assert_eq!(decoded, reply);
        let diagnostic = e::Reply {
            root: request.root.clone(),
            executor: request.executor.clone(),
            operation: request.operation,
            request_digest: request.digest()?,
            value: e::Value::Recovery {
                scanned: U64(e::MAX_DIRECTORIES),
                cleaned: U64(0),
                retained: U64(e::MAX_DIRECTORIES),
                retained_example: Some("é".repeat(e::ERROR_BYTES / 2)),
                candidate: None,
            },
        };
        diagnostic.validate(&request)?;
        crate::application::desktop::test_export_executor_relay_admission(&request, &diagnostic)?;
        let encoded = encode_outcome(&Ok(Response::ExportExecutor(diagnostic.clone())))?;
        let Ok(Response::ExportExecutor(decoded)) = decode_outcome(&encoded)? else {
            unreachable!()
        };
        assert_eq!(decoded, diagnostic);
        Ok(())
    }

    #[test]
    fn export_profile_outcome_uses_exact_bounded_binary_trailer() -> Result<()> {
        use crate::catalog_session::{
            EXPORT_PROFILE_BYTES, ExportProfileAction, ExportProfileReply, ExportProfileRequest,
            ExportProfileValue, PhysicalObjectId, RootCapability,
        };
        #[cfg(unix)]
        let physical = |index| PhysicalObjectId::Unix {
            device: crate::application::U64(1),
            inode: crate::application::U64(index),
        };
        #[cfg(windows)]
        let physical = |index| PhysicalObjectId::Windows {
            volume_serial: crate::application::U64(1),
            file_index: crate::application::U64(index),
        };
        let request = ExportProfileRequest {
            root: RootCapability {
                epoch: LeaseId::new(),
                token: LeaseId::new(),
                session: LeaseId::new(),
                canonical_root: NativePath::from_path(&std::env::temp_dir()),
                root_physical: physical(2),
                catalog_physical: physical(3),
            },
            requested: NativePath::from_path(&std::env::temp_dir().join("profile.icc")),
            transfer: LeaseId::new(),
            step: crate::application::U64(1),
            allowance: crate::application::U64(EXPORT_PROFILE_BYTES as u64),
            action: ExportProfileAction::Read {
                offset: crate::application::U64(0),
            },
        };
        let bytes = vec![0x3c; CHUNK_BYTES];
        let reply = ExportProfileReply {
            root: request.root.clone(),
            requested: request.requested.clone(),
            transfer: request.transfer.clone(),
            step: request.step,
            value: ExportProfileValue::Chunk {
                offset: crate::application::U64(0),
                checksum: blake3::hash(&bytes).to_hex().to_string(),
                bytes: bytes.clone(),
            },
        };
        let encoded = encode_outcome(&Ok(Response::ExportProfile(reply)))?;
        assert!(encoded.len() < bytes.len() + 2048);
        let Ok(Response::ExportProfile(decoded)) = decode_outcome(&encoded)? else {
            panic!("profile outcome")
        };
        decoded.validate(&request)?;
        assert_eq!(decoded.binary(), Some(bytes.as_slice()));
        let mut foreign = decoded.clone();
        foreign.transfer = LeaseId::new();
        assert!(foreign.validate(&request).is_err());
        let mut corrupt = decoded.clone();
        if let ExportProfileValue::Chunk { checksum, .. } = &mut corrupt.value {
            *checksum = "0".repeat(64);
        }
        assert!(corrupt.validate(&request).is_err());
        let mut oversized = decoded;
        assert!(oversized.set_binary(&vec![0; CHUNK_BYTES + 1]).is_err());
        Ok(())
    }
    #[test]
    fn stage_packed_outcomes_preserve_skipped_chunk_content_and_metadata() -> Result<()> {
        use crate::catalog_session::preview_stage::{Reply, Value};
        for value in [
            Value::Unit,
            Value::Metadata(None),
            Value::Metadata(Some(vec![1, 2, 3])),
            Value::Chunk { bytes: vec![] },
            Value::Chunk {
                bytes: vec![0xa5; CHUNK_BYTES],
            },
        ] {
            let reply = Reply {
                epoch: LeaseId::new(),
                session: LeaseId::new(),
                operation: U64(u64::MAX),
                value,
            };
            let expected = reply.binary().to_vec();
            let encoded = encode_outcome(&Ok(Response::PreviewStage(reply.clone())))?;
            let Ok(Response::PreviewStage(decoded)) = decode_outcome(&encoded)? else {
                anyhow::bail!("stage outcome shape");
            };
            assert_eq!(decoded.epoch, reply.epoch);
            assert_eq!(decoded.session, reply.session);
            assert_eq!(decoded.operation, reply.operation);
            assert_eq!(decoded.binary(), expected);
            assert_eq!(
                serde_json::to_value(&decoded.value)?,
                serde_json::to_value(&reply.value)?
            );
            // Binary is present only in the trailer, never a numeric JSON array.
            assert!(encoded.len() < expected.len() + 1024);
            if matches!(reply.value, Value::Chunk { .. }) {
                assert_eq!(
                    serde_json::to_value(&reply.value)?,
                    serde_json::json!({"kind":"Chunk","value":{}})
                );
            }
        }
        Ok(())
    }
    #[test]
    fn frame_rejects_size_before_read_and_preserves_exact_chunks() -> Result<()> {
        let payload = vec![0xa5; MESSAGE_BYTES];
        let mut stream = vec![];
        Message {
            kind: Kind::Execute,
            sequence: 9007199254740993,
            bytes: payload.clone(),
        }
        .write([7; 16], &mut stream)?;
        let mut input = stream.as_slice();
        let first = Frame::read(&mut input)?.unwrap();
        let mut assembly = Assembly::start(&first)?;
        assembly.push(first)?;
        while let Some(frame) = Frame::read(&mut input)? {
            assembly.push(frame)?;
        }
        assert_eq!(assembly.finish()?.bytes, payload);
        stream[44..48].copy_from_slice(&((CHUNK_BYTES + 1) as u32).to_le_bytes());
        assert!(Frame::read(&mut &stream[..HEADER_BYTES]).is_err());
        Ok(())
    }
    #[test]
    fn exact_encoding_and_epoch_continuity_are_checked() -> Result<()> {
        let value = "é\0".repeat(1000);
        let expected = serde_json::to_vec(&value)?;
        let encoded = encode(&value, expected.len())?;
        assert_eq!(encoded, expected);
        assert_eq!(encoded.capacity(), encoded.len());
        assert!(encode(&value, expected.len() - 1).is_err());
        let mut frame = Frame {
            kind: Kind::Result,
            epoch: [0; 16],
            sequence: 1,
            offset: 0,
            total: 1,
            payload: vec![1],
        };
        let mut assembly = Assembly::start(&frame)?;
        frame.epoch = [1; 16];
        assert!(assembly.push(frame).is_err());
        Ok(())
    }
    #[test]
    fn configuration_and_native_path_boundaries_preserve_full_roster() -> Result<()> {
        #[cfg(unix)]
        let path = NativePath::UnixBytes(b"/a".to_vec());
        #[cfg(windows)]
        let path = NativePath::WindowsWide("C:\\a".encode_utf16().collect());
        let mut startup = Startup::new(vec![])?;
        let base = encode(&startup, CONFIG_BYTES)?.len();
        let item = encode(&path, MESSAGE_BYTES)?.len();
        let count = (CONFIG_BYTES - base + 1) / (item + 1);
        startup.original_roots = vec![path.clone(); count];
        let encoded = encode(&startup, CONFIG_BYTES)?;
        assert!(CONFIG_BYTES - encoded.len() < item + 1);
        let decoded: Startup = decode(&encoded, CONFIG_BYTES)?;
        decoded.validate()?;
        assert_eq!(decoded.original_roots.len(), count);
        assert!(decoded.original_roots.iter().all(|v| v == &path));
        startup.original_roots.push(path.clone());
        assert!(startup.validate().is_err());
        let mut maximum = path;
        match &mut maximum {
            NativePath::UnixBytes(units) => units.resize(crate::catalog_session::PATH_UNITS, b'a'),
            NativePath::WindowsWide(units) => {
                units.resize(crate::catalog_session::PATH_UNITS, b'a' as u16)
            }
        }
        validate_path(&maximum)?;
        match &mut maximum {
            NativePath::UnixBytes(units) => units.push(b'a'),
            NativePath::WindowsWide(units) => units.push(b'a' as u16),
        }
        assert!(validate_path(&maximum).is_err());
        assert!(decode::<Operation>(br#"{"command":"global_restore_status","args":{"root":{"encoding":"UnixBytes","units":[47]},"extra":true}}"#,MESSAGE_BYTES).is_err());
        Ok(())
    }
}
