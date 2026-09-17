//! Unselected G-owned process boundary for the complete Workbench protocol.
//!
//! W owns the inspection SQLite roles and retained public request/result state.
//! This transport contains no production factory selection; the desktop owner
//! may opt into it only after the remaining F/S custody map is qualified.
use super::lightroom_bridge::{Coordinator, Request, Response};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
#[cfg(test)]
use std::process::ChildStdout;
use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Condvar, Mutex, atomic::AtomicBool},
    thread,
};

pub const ENVELOPE_BYTES: usize = 128 * 1024;
const HEADER_BYTES: usize = 48;
const PROTOCOL: u8 = 1;
const MAGIC: &[u8; 4] = b"PCWB";
const ERROR_BYTES: usize = 4096;
pub(crate) const CALLBACK_BYTES: usize = 64 * 1024 * 1024;
const CALLBACK_CHUNK_BYTES: usize = (ENVELOPE_BYTES - 16 * 1024) / 6;

pub fn build_identity() -> String {
    blake3::hash(
        concat!(
            env!("CARGO_PKG_VERSION"),
            include_str!("lightroom.rs"),
            include_str!("lightroom/worker.rs"),
            include_str!("lightroom_bridge.rs"),
            include_str!("lightroom_bridge/wire.rs"),
            include_str!("lightroom_managed.rs"),
            include_str!("lightroom_process.rs"),
            include_str!("../lightroom/plan.rs"),
            include_str!("../lightroom/plan/desktop.rs"),
            include_str!("../lightroom/plan/selection.rs"),
            include_str!("../lightroom/plan/selection/snapshot.rs"),
            include_str!("../lightroom/migration_source.rs"),
            include_str!("../lightroom/migration_source/reader.rs"),
            include_str!("../../Cargo.lock")
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Startup {
    protocol: u8,
    build: String,
    instance: crate::catalog_session::LeaseId,
    envelope_bytes: crate::application::U64,
    managed: bool,
}
impl Startup {
    fn new(managed: bool) -> Self {
        Self {
            protocol: PROTOCOL,
            build: build_identity(),
            instance: crate::catalog_session::LeaseId::new(),
            envelope_bytes: crate::application::U64(ENVELOPE_BYTES as u64),
            managed,
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.protocol == PROTOCOL
                && self.build == build_identity()
                && self.envelope_bytes.0 == ENVELOPE_BYTES as u64,
            "Workbench process bootstrap mismatch"
        );
        crate::catalog_session::LeaseId::parse(self.instance.as_str())?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Work {
    Request {
        sequence: crate::application::U64,
        request_digest: String,
        request: Request,
    },
    Shutdown {
        sequence: crate::application::U64,
    },
    CallbackBegin {
        sequence: crate::application::U64,
        bytes: crate::application::U64,
        blake3: String,
    },
    CallbackChunk {
        sequence: crate::application::U64,
        offset: crate::application::U64,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
enum CallbackRequest {
    Admit,
    Commit,
    Filesystem {
        request: crate::filesystem_worker::wire::LightroomWorkbenchIo,
    },
    SealedDocument {
        request: crate::filesystem_worker::wire::LightroomSealedRead,
    },
    ArtifactPreparation {
        request: crate::filesystem_worker::wire::LightroomArtifactPreparation,
    },
    SourceOpen {
        authority: crate::lightroom_migration_worker::source_reader::CaptureSqlAuthority,
    },
    SourceSqlOpen {
        #[serde(with = "callback_seal_wire")]
        seal: crate::lightroom::migration_source::InputSeal,
        limits: crate::lightroom::migration_source::ReadLimits,
        protected: Vec<crate::lightroom_migration_worker::identity::FileKey>,
    },
    SourceSchema {
        source: String,
    },
    SourceRows {
        source: String,
        handle: String,
        cursor: Option<Vec<crate::lightroom::plan::Cell>>,
        limit: crate::application::U64,
    },
    SourceCurrent {
        source: String,
    },
    SourceRetire {
        source: String,
    },
}

// CallbackRequest is internally tagged, so Serde buffers its fields through
// Content while decoding. Content has no u128 representation. Keep InputSeal's
// public/on-disk numeric grammar and use canonical decimal strings only for the
// two timestamp positions on this private, build-bound callback wire.
mod callback_seal_wire {
    use crate::{
        lightroom::{
            migration_source::{InputSeal, SelectedCapture, SelectionApproval, SupplementPin},
            source::Revision,
        },
        storage_volume::NativePath,
        xmp_packets::{SourceRevision, Status},
    };
    use serde::{
        Deserialize, Deserializer, Serialize, Serializer,
        de::{SeqAccess, Visitor},
        ser::SerializeSeq,
    };

    #[derive(Serialize, Deserialize)]
    #[serde(remote = "Revision")]
    struct RevisionDef {
        object: String,
        bytes: u64,
        #[serde(
            with = "crate::filesystem_worker::wire::capture_manifest_wire::option_u128_decimal"
        )]
        modified_ns: Option<u128>,
        changed: String,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(remote = "SourceRevision")]
    struct SourceRevisionDef {
        length: u64,
        blake3: String,
        #[serde(
            with = "crate::filesystem_worker::wire::capture_manifest_wire::option_u128_decimal"
        )]
        modified_unix_ns: Option<u128>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(remote = "SupplementPin")]
    struct SupplementPinDef {
        revision: String,
        source_id: String,
        origin: String,
        #[serde(with = "SourceRevisionDef")]
        source_revision: SourceRevision,
        historical_status: Status,
        proof_blake3: String,
    }

    #[derive(Serialize)]
    struct SupplementRef<'a>(#[serde(with = "SupplementPinDef")] &'a SupplementPin);

    struct SupplementOwned(SupplementPin);
    impl<'de> Deserialize<'de> for SupplementOwned {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            SupplementPinDef::deserialize(deserializer).map(Self)
        }
    }

    mod supplements {
        use super::*;

        pub fn serialize<S: Serializer>(
            values: &[SupplementPin],
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            let mut sequence = serializer.serialize_seq(Some(values.len()))?;
            for value in values {
                sequence.serialize_element(&SupplementRef(value))?;
            }
            sequence.end()
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Vec<SupplementPin>, D::Error> {
            struct SupplementVisitor;
            impl<'de> Visitor<'de> for SupplementVisitor {
                type Value = Vec<SupplementPin>;

                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a selected supplement sequence")
                }

                fn visit_seq<A: SeqAccess<'de>>(
                    self,
                    mut sequence: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                    while let Some(SupplementOwned(value)) = sequence.next_element()? {
                        values.push(value);
                    }
                    Ok(values)
                }
            }
            deserializer.deserialize_seq(SupplementVisitor)
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(remote = "InputSeal")]
    struct InputSealDef {
        protocol: u32,
        database: NativePath,
        #[serde(with = "RevisionDef")]
        identity: Revision,
        blake3: String,
        approval: SelectionApproval,
        selected: Vec<SelectedCapture>,
        excluded_revisions: Vec<String>,
        #[serde(default, with = "supplements")]
        supplements: Vec<SupplementPin>,
    }

    pub fn serialize<S: Serializer>(value: &InputSeal, serializer: S) -> Result<S::Ok, S::Error> {
        InputSealDef::serialize(value, serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<InputSeal, D::Error> {
        InputSealDef::deserialize(deserializer)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "callback_kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
enum CallbackValue {
    Admitted,
    Filesystem(crate::filesystem_worker::wire::LightroomWorkbenchIoReply),
    SealedDocument {
        value: Option<crate::filesystem_worker::wire::LightroomSealedDocumentPage>,
    },
    ArtifactPreparation {
        value: Option<crate::filesystem_worker::wire::LightroomArtifactPreparationReply>,
    },
    Source {
        value: String,
    },
    Schema(crate::lightroom_migration_worker::source_reader::capture_wire::SchemaObjects),
    Table(crate::lightroom_migration_worker::source_reader::capture_wire::TableValue),
    Current(crate::lightroom_migration_worker::source_reader::capture_wire::Current),
    Retired,
}

#[allow(dead_code)] // Read by the integrated Workbench metadata reservation.
pub(super) struct CallbackMetadataLayouts {
    pub proxy: usize,
    pub state: usize,
    pub assembly: usize,
    pub request_assembly: usize,
    pub request: usize,
    pub value: usize,
    pub outcome: usize,
    pub sealed_request: usize,
    pub sealed_reply: usize,
    pub artifact_request: usize,
    pub artifact_reply: usize,
}

#[allow(dead_code)] // Read by the integrated Workbench metadata reservation.
pub(super) fn callback_metadata_layouts() -> CallbackMetadataLayouts {
    CallbackMetadataLayouts {
        proxy: std::mem::size_of::<CallbackProxy>(),
        state: std::mem::size_of::<CallbackState>(),
        assembly: std::mem::size_of::<CallbackAssembly>(),
        request_assembly: std::mem::size_of::<CallbackRequestReceiver>(),
        request: std::mem::size_of::<CallbackRequest>(),
        value: std::mem::size_of::<CallbackValue>(),
        outcome: std::mem::size_of::<Outcome>(),
        sealed_request: std::mem::size_of::<crate::filesystem_worker::wire::LightroomSealedRead>(),
        sealed_reply: std::mem::size_of::<
            crate::filesystem_worker::wire::LightroomSealedDocumentPage,
        >(),
        artifact_request: std::mem::size_of::<
            crate::filesystem_worker::wire::LightroomArtifactPreparation,
        >(),
        artifact_reply: std::mem::size_of::<
            crate::filesystem_worker::wire::LightroomArtifactPreparationReply,
        >(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub detail: String,
    pub fatal: bool,
}
impl Failure {
    fn new(error: impl std::fmt::Display) -> Self {
        Self {
            detail: format!("{error:#}").chars().take(ERROR_BYTES).collect(),
            fatal: false,
        }
    }
    fn fatal(error: impl std::fmt::Display) -> Self {
        Self {
            detail: format!("{error:#}").chars().take(ERROR_BYTES).collect(),
            fatal: true,
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.detail.is_empty() && self.detail.len() <= ERROR_BYTES,
            "Workbench failure detail bound"
        );
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
enum Outcome {
    Ready {
        instance: crate::catalog_session::LeaseId,
        build: String,
    },
    Reply {
        sequence: crate::application::U64,
        request_digest: String,
        result: std::result::Result<Response, Failure>,
    },
    Drained {
        sequence: crate::application::U64,
        instance: crate::catalog_session::LeaseId,
    },
    CallbackBegin {
        sequence: crate::application::U64,
        bytes: crate::application::U64,
        blake3: String,
    },
    CallbackChunk {
        sequence: crate::application::U64,
        offset: crate::application::U64,
        bytes: Vec<u8>,
    },
}

fn encode_limit(value: &impl Serialize, maximum: usize) -> Result<Vec<u8>> {
    struct Count {
        bytes: usize,
        maximum: usize,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .filter(|bytes| *bytes <= self.maximum)
                .ok_or_else(|| std::io::Error::other("Workbench envelope byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { bytes: 0, maximum };
    serde_json::to_writer(&mut count, value)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(count.bytes)?;
    serde_json::to_writer(&mut bytes, value)?;
    ensure!(
        bytes.len() == count.bytes,
        "Workbench encoding length changed"
    );
    Ok(bytes)
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    encode_limit(value, ENVELOPE_BYTES)
}

fn decode_limit<T: DeserializeOwned>(bytes: &[u8], maximum: usize) -> Result<T> {
    ensure!(bytes.len() <= maximum, "Workbench decoded byte limit");
    Ok(serde_json::from_slice(bytes)?)
}
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    decode_limit(bytes, ENVELOPE_BYTES)
}

fn write_packet(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = encode(value)?;
    let length = u32::try_from(bytes.len())?;
    let digest = blake3::hash(&bytes);
    let mut header = [0; HEADER_BYTES];
    header[..4].copy_from_slice(MAGIC);
    header[4] = PROTOCOL;
    header[8..12].copy_from_slice(&length.to_le_bytes());
    header[12..44].copy_from_slice(digest.as_bytes());
    writer.write_all(&header)?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_packet<T: DeserializeOwned>(reader: &mut impl Read) -> Result<Option<T>> {
    let mut header = [0; HEADER_BYTES];
    loop {
        match reader.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => break,
            Ok(_) => unreachable!(),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    reader.read_exact(&mut header[1..])?;
    ensure!(
        &header[..4] == MAGIC
            && header[4] == PROTOCOL
            && header[5..8] == [0, 0, 0]
            && header[44..48] == [0, 0, 0, 0],
        "Workbench frame header mismatch: {}",
        header[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    ensure!(length <= ENVELOPE_BYTES, "Workbench frame byte limit");
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length)?;
    bytes.resize(length, 0);
    reader.read_exact(&mut bytes)?;
    ensure!(
        blake3::hash(&bytes).as_bytes()[..] == header[12..44],
        "Workbench frame digest mismatch"
    );
    Ok(Some(decode(&bytes)?))
}

fn request_digest(request: &Request) -> Result<String> {
    Ok(blake3::hash(&encode(request)?).to_hex().to_string())
}

fn encode_callback_request(request: &CallbackRequest, cancel: &AtomicBool) -> Result<Vec<u8>> {
    ensure!(
        !cancel.load(std::sync::atomic::Ordering::Acquire),
        "Workbench callback canceled"
    );
    let encoded = encode_limit(request, CALLBACK_BYTES)?;
    ensure!(
        !cancel.load(std::sync::atomic::Ordering::Acquire),
        "Workbench callback canceled"
    );
    Ok(encoded)
}

fn write_encoded_callback_request(
    writer: &mut impl Write,
    sequence: crate::application::U64,
    encoded: &[u8],
    cancel: &AtomicBool,
) -> Result<()> {
    let digest = crate::lightroom::digest(encoded);
    write_packet(
        writer,
        &Outcome::CallbackBegin {
            sequence,
            bytes: crate::application::U64(encoded.len().try_into()?),
            blake3: digest,
        },
    )?;
    for (index, bytes) in encoded.chunks(CALLBACK_CHUNK_BYTES).enumerate() {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "Workbench callback canceled"
        );
        write_packet(
            writer,
            &Outcome::CallbackChunk {
                sequence,
                offset: crate::application::U64((index * CALLBACK_CHUNK_BYTES) as u64),
                bytes: bytes.to_vec(),
            },
        )?;
    }
    Ok(())
}

#[derive(Default)]
struct CallbackState {
    next: u64,
    assembly: Option<CallbackAssembly>,
    waiting: Option<(u64, std::result::Result<CallbackValue, Failure>)>,
    closed: bool,
}
struct CallbackAssembly {
    sequence: u64,
    length: usize,
    blake3: String,
    bytes: Vec<u8>,
}
struct CallbackRequestAssembly {
    sequence: u64,
    length: usize,
    blake3: String,
    bytes: Vec<u8>,
}
#[derive(Default)]
struct CallbackRequestReceiver {
    sequence: u64,
    assembly: Option<CallbackRequestAssembly>,
}
impl CallbackRequestReceiver {
    fn begin(
        &mut self,
        sequence: crate::application::U64,
        length: crate::application::U64,
        blake3: String,
        cancel: &AtomicBool,
    ) -> Result<()> {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "Workbench callback request canceled"
        );
        ensure!(self.assembly.is_none(), "Workbench callback interleaved");
        ensure!(
            sequence.0
                == self
                    .sequence
                    .checked_add(1)
                    .context("Workbench callback sequence exhausted")?,
            "Workbench callback request sequence"
        );
        let length = usize::try_from(length.0)?;
        ensure!(
            (1..=CALLBACK_BYTES).contains(&length),
            "Workbench callback request length"
        );
        ensure!(
            blake3.len() == 64
                && blake3
                    .bytes()
                    .all(|value| value.is_ascii_hexdigit() && !value.is_ascii_uppercase()),
            "Workbench callback request digest"
        );
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length)?;
        self.assembly = Some(CallbackRequestAssembly {
            sequence: sequence.0,
            length,
            blake3,
            bytes,
        });
        Ok(())
    }

    fn push(
        &mut self,
        sequence: crate::application::U64,
        offset: crate::application::U64,
        bytes: Vec<u8>,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "Workbench callback request canceled"
        );
        let assembly = self
            .assembly
            .as_mut()
            .context("Workbench callback request has no begin")?;
        let remaining = assembly
            .length
            .checked_sub(assembly.bytes.len())
            .context("Workbench callback request overrun")?;
        ensure!(
            assembly.sequence == sequence.0
                && offset.0 == assembly.bytes.len() as u64
                && !bytes.is_empty()
                && bytes.len() <= CALLBACK_CHUNK_BYTES
                && bytes.len() <= remaining,
            "Workbench callback request continuity"
        );
        assembly.bytes.extend_from_slice(&bytes);
        Ok(assembly.bytes.len() == assembly.length)
    }

    fn request(&self) -> Result<CallbackRequest> {
        let assembly = self
            .assembly
            .as_ref()
            .context("Workbench callback request is incomplete")?;
        ensure!(
            assembly.bytes.len() == assembly.length
                && crate::lightroom::digest(&assembly.bytes) == assembly.blake3,
            "Workbench callback request changed"
        );
        decode_limit(&assembly.bytes, CALLBACK_BYTES)
    }

    fn finish(&mut self, sequence: crate::application::U64) -> Result<()> {
        let assembly = self
            .assembly
            .take()
            .context("Workbench callback request completion missing")?;
        ensure!(
            assembly.sequence == sequence.0 && assembly.bytes.len() == assembly.length,
            "Workbench callback request completion sequence"
        );
        self.sequence = sequence.0;
        Ok(())
    }

    fn is_pending(&self) -> bool {
        self.assembly.is_some()
    }

    fn ensure_idle(&self) -> Result<()> {
        ensure!(
            !self.is_pending(),
            "Workbench callback request ended before completion"
        );
        Ok(())
    }
}
struct CallbackProxy {
    output: Arc<Mutex<std::io::Stdout>>,
    state: Mutex<CallbackState>,
    wake: Condvar,
}
impl CallbackProxy {
    fn call(&self, request: CallbackRequest, cancel: &AtomicBool) -> Result<CallbackValue> {
        let encoded = encode_callback_request(&request, cancel)?;
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        // A preexisting cancellation consumes no sequence. Once a sequence is
        // allocated, Begin is unconditional; cancellation while streaming
        // leaves an explicit partial request for G's checked revoke path.
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "Workbench callback canceled"
        );
        let sequence = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(!state.closed, "Workbench supervisor callback is closed");
            state.next = state
                .next
                .checked_add(1)
                .context("callback sequence exhausted")?;
            state.next
        };
        let sequence = crate::application::U64(sequence);
        write_encoded_callback_request(&mut *output, sequence, &encoded, cancel)?;
        drop(output);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Acquire),
                "Workbench callback canceled"
            );
            ensure!(!state.closed, "Workbench supervisor callback closed");
            if state
                .waiting
                .as_ref()
                .is_some_and(|value| value.0 == sequence.0)
            {
                let (_, result) = state.waiting.take().unwrap();
                return result.map_err(|failure| anyhow::anyhow!(failure.detail));
            }
            let (next, _) = self
                .wake
                .wait_timeout(state, std::time::Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner());
            state = next;
        }
    }
    fn reply(
        &self,
        sequence: crate::application::U64,
        length: crate::application::U64,
        blake3: String,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            sequence.0 > 0
                && sequence.0 <= state.next
                && state.waiting.is_none()
                && state.assembly.is_none(),
            "Workbench callback result sequence"
        );
        let length = usize::try_from(length.0)?;
        ensure!(
            (1..=CALLBACK_BYTES).contains(&length),
            "Workbench callback result length"
        );
        ensure!(
            blake3.len() == 64
                && blake3
                    .bytes()
                    .all(|value| value.is_ascii_hexdigit() && !value.is_ascii_uppercase()),
            "Workbench callback result digest"
        );
        state.assembly = Some(CallbackAssembly {
            sequence: sequence.0,
            length,
            blake3,
            bytes: Vec::with_capacity(length),
        });
        Ok(())
    }
    fn chunk(
        &self,
        sequence: crate::application::U64,
        offset: crate::application::U64,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let assembly = state
            .assembly
            .as_mut()
            .context("Workbench callback result has no begin")?;
        ensure!(
            assembly.sequence == sequence.0
                && offset.0 == assembly.bytes.len() as u64
                && !bytes.is_empty()
                && bytes.len() <= CALLBACK_CHUNK_BYTES
                && bytes.len() <= assembly.length - assembly.bytes.len(),
            "Workbench callback result continuity"
        );
        assembly.bytes.extend_from_slice(&bytes);
        if assembly.bytes.len() == assembly.length {
            let assembly = state.assembly.take().unwrap();
            ensure!(
                crate::lightroom::digest(&assembly.bytes) == assembly.blake3,
                "Workbench callback result changed"
            );
            let result = decode_limit(&assembly.bytes, CALLBACK_BYTES)?;
            state.waiting = Some((assembly.sequence, result));
            self.wake.notify_all();
        }
        Ok(())
    }
    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        self.wake.notify_all();
    }
}
impl super::lightroom::ManagedIo for CallbackProxy {
    fn admit(&self) -> Result<()> {
        match self.call(CallbackRequest::Admit, &AtomicBool::new(false))? {
            CallbackValue::Admitted => Ok(()),
            _ => anyhow::bail!("Workbench admission callback result kind"),
        }
    }

    fn commit(&self) -> Result<()> {
        match self.call(CallbackRequest::Commit, &AtomicBool::new(false))? {
            CallbackValue::Admitted => Ok(()),
            _ => anyhow::bail!("Workbench commit callback result kind"),
        }
    }

    fn revoke_generation(&self) {
        // Only the concrete G owner may revoke the outer generation.
    }

    fn workbench_reaped(&self) {
        // Only the concrete G owner records the outer W reap.
    }

    fn filesystem(
        &self,
        request: crate::filesystem_worker::wire::LightroomWorkbenchIo,
        cancel: &AtomicBool,
    ) -> Result<crate::filesystem_worker::wire::LightroomWorkbenchIoReply> {
        match self.call(CallbackRequest::Filesystem { request }, cancel)? {
            CallbackValue::Filesystem(value) => Ok(value),
            _ => anyhow::bail!("Workbench filesystem callback result kind"),
        }
    }
    fn sealed_document(
        &self,
        request: crate::filesystem_worker::wire::LightroomSealedRead,
        cancel: &AtomicBool,
    ) -> Result<Option<crate::filesystem_worker::wire::LightroomSealedDocumentPage>> {
        match self.call(
            CallbackRequest::SealedDocument {
                request: request.clone(),
            },
            cancel,
        )? {
            CallbackValue::SealedDocument { value } => match (&request, value) {
                (crate::filesystem_worker::wire::LightroomSealedRead::Discard { .. }, None) => {
                    Ok(None)
                }
                (_, Some(value)) => {
                    value.validate_for(&request)?;
                    Ok(Some(value))
                }
                _ => anyhow::bail!("sealed document callback response is absent"),
            },
            _ => anyhow::bail!("Workbench sealed-document callback result kind"),
        }
    }
    fn artifact_preparation(
        &self,
        request: crate::filesystem_worker::wire::LightroomArtifactPreparation,
        cancel: &AtomicBool,
    ) -> Result<Option<crate::filesystem_worker::wire::LightroomArtifactPreparationReply>> {
        match self.call(
            CallbackRequest::ArtifactPreparation {
                request: request.clone(),
            },
            cancel,
        )? {
            CallbackValue::ArtifactPreparation { value } => match (&request, value) {
                (
                    crate::filesystem_worker::wire::LightroomArtifactPreparation::Discard {
                        ..
                    }
                    | crate::filesystem_worker::wire::LightroomArtifactPreparation::DiscardReceipt {
                        ..
                    },
                    None,
                ) => Ok(None),
                (_, Some(value)) => {
                    value.validate_for(&request)?;
                    Ok(Some(value))
                }
                _ => anyhow::bail!("artifact preparation callback response is absent"),
            },
            _ => anyhow::bail!("Workbench artifact-preparation callback result kind"),
        }
    }
    fn source_open(
        &self,
        authority: crate::lightroom_migration_worker::source_reader::CaptureSqlAuthority,
        cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        match self.call(CallbackRequest::SourceOpen { authority }, &cancel)? {
            CallbackValue::Source { value } => Ok(value),
            _ => anyhow::bail!("Workbench source-open callback result kind"),
        }
    }
    fn source_sql_open(
        &self,
        seal: crate::lightroom::migration_source::InputSeal,
        limits: crate::lightroom::migration_source::ReadLimits,
        protected: Vec<crate::lightroom_migration_worker::identity::FileKey>,
        cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        match self.call(
            CallbackRequest::SourceSqlOpen {
                seal,
                limits,
                protected,
            },
            &cancel,
        )? {
            CallbackValue::Source { value } => Ok(value),
            _ => anyhow::bail!("Workbench SQL13 source-open callback result kind"),
        }
    }
    fn source_schema(
        &self,
        source: &str,
    ) -> Result<crate::lightroom_migration_worker::source_reader::capture_wire::SchemaObjects> {
        match self.call(
            CallbackRequest::SourceSchema {
                source: source.into(),
            },
            &AtomicBool::new(false),
        )? {
            CallbackValue::Schema(value) => Ok(value),
            _ => anyhow::bail!("Workbench source-schema callback result kind"),
        }
    }
    fn source_rows(
        &self,
        source: &str,
        handle: String,
        cursor: Option<Vec<crate::lightroom::plan::Cell>>,
        limit: usize,
    ) -> Result<crate::lightroom_migration_worker::source_reader::capture_wire::TableValue> {
        match self.call(
            CallbackRequest::SourceRows {
                source: source.into(),
                handle,
                cursor,
                limit: crate::application::U64(limit.try_into()?),
            },
            &AtomicBool::new(false),
        )? {
            CallbackValue::Table(value) => Ok(value),
            _ => anyhow::bail!("Workbench source-rows callback result kind"),
        }
    }
    fn source_current(
        &self,
        source: &str,
    ) -> Result<crate::lightroom_migration_worker::source_reader::capture_wire::Current> {
        match self.call(
            CallbackRequest::SourceCurrent {
                source: source.into(),
            },
            &AtomicBool::new(false),
        )? {
            CallbackValue::Current(value) => Ok(value),
            _ => anyhow::bail!("Workbench source-current callback result kind"),
        }
    }
    fn source_retire(&self, source: &str) -> Result<()> {
        match self.call(
            CallbackRequest::SourceRetire {
                source: source.into(),
            },
            &AtomicBool::new(false),
        )? {
            CallbackValue::Retired => Ok(()),
            _ => anyhow::bail!("Workbench source-retire callback result kind"),
        }
    }
    fn drain_sources(&self) -> Result<()> {
        // This is W's callback proxy, not G's concrete dependency owner.
        Ok(())
    }
    fn drain_filesystem(&self) -> Result<()> {
        Ok(())
    }
}

// Keep the typed request inline in the admitted fixed-capacity channel.
// Boxing adds a separate allocation without reducing the reserved slot backing.
#[allow(clippy::large_enum_variant)]
enum CoordinatorCommand {
    Request {
        sequence: crate::application::U64,
        request_digest: String,
        request: Request,
    },
    Shutdown {
        sequence: crate::application::U64,
    },
}

pub(super) fn coordinator_channel_backing() -> Result<usize> {
    use crate::lightroom_migration_worker::memory::{channels, layout::add};
    // The channel has two synchronized wakers, its sole receiver blocks, and
    // the coordinator thread locks the retained owner once before dispatch.
    add(
        add(
            channels::bounded(1, std::alloc::Layout::new::<CoordinatorCommand>())?,
            channels::arc(std::alloc::Layout::new::<Mutex<Option<Coordinator>>>())?,
        )?,
        add(channels::pthread_mutexes(3)?, channels::blocking_waiter()?)?,
    )
}

/// Hidden W entrypoint. A single retained thread owns Coordinator; this input
/// thread stays available to deliver callback replies during ordinary requests
/// as well as checked shutdown. No request thread is detached.
pub fn worker_main() -> Result<()> {
    let mut input = std::io::stdin().lock();
    let startup: Startup = read_packet(&mut input)?.context("missing Workbench bootstrap")?;
    startup.validate()?;
    let output = Arc::new(Mutex::new(std::io::stdout()));
    let executable = std::env::current_exe()?;
    let control = Arc::new(Mutex::new(super::lightroom_bridge::Control::default()));
    let callback = Arc::new(CallbackProxy {
        output: output.clone(),
        state: Mutex::new(CallbackState::default()),
        wake: Condvar::new(),
    });
    let coordinator = if startup.managed {
        Coordinator::new_managed(control, callback.clone())
    } else {
        Coordinator::new(control)
    };
    // Thread creation failure must not Drop a callback-owning Coordinator on
    // the sole input thread. Keep its exact owner outside the spawn closure.
    let retained = Arc::new(Mutex::new(Some(coordinator)));
    let owner = retained.clone();
    let worker_callback = callback.clone();
    let worker_output = output.clone();
    let instance = startup.instance.clone();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("workbench-coordinator".into())
        .spawn(move || {
            let Some(mut coordinator) = owner.lock().unwrap_or_else(|e| e.into_inner()).take()
            else {
                std::process::exit(1);
            };
            // Keep Coordinator outside the unwind boundary: on failure, exit
            // without running a potentially blocking owner Drop. G proves the
            // W process exit before reconciling F and retains Source ownership.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                    loop {
                        match receive
                            .recv()
                            .context("Workbench request dispatcher disconnected")?
                        {
                            CoordinatorCommand::Request {
                                sequence,
                                request_digest,
                                request,
                            } => {
                                #[cfg(test)]
                                if let Request::Status {
                                    attempt: Some(marker),
                                    ..
                                } = &request
                                    && let Some(marker) = marker.strip_prefix("fixture-stall:")
                                {
                                    std::fs::write(marker, b"entered Workbench request")?;
                                    loop {
                                        std::thread::park();
                                    }
                                }
                                coordinator.maintain()?;
                                let result = coordinator
                                    .request(request, &executable, ENVELOPE_BYTES)
                                    .map_err(|error| {
                                        if coordinator.fatal() {
                                            Failure::fatal(error)
                                        } else {
                                            Failure::new(error)
                                        }
                                    });
                                if let Err(error) = &result {
                                    error.validate()?;
                                }
                                write_packet(
                                    &mut *worker_output.lock().unwrap_or_else(|e| e.into_inner()),
                                    &Outcome::Reply {
                                        sequence,
                                        request_digest,
                                        result,
                                    },
                                )?;
                            }
                            CoordinatorCommand::Shutdown { sequence } => {
                                coordinator.shutdown()?;
                                worker_callback.close();
                                write_packet(
                                    &mut *worker_output.lock().unwrap_or_else(|e| e.into_inner()),
                                    &Outcome::Drained {
                                        sequence,
                                        instance: instance.clone(),
                                    },
                                )?;
                                return Ok(());
                            }
                        }
                    }
                }));
            if !matches!(result, Ok(Ok(()))) {
                worker_callback.close();
                std::process::exit(1);
            }
        });
    let worker = match worker {
        Ok(worker) => worker,
        Err(_) => std::process::exit(1),
    };
    drop(retained);
    let run = (|| -> Result<()> {
        write_packet(
            &mut *output.lock().unwrap_or_else(|e| e.into_inner()),
            &Outcome::Ready {
                instance: startup.instance,
                build: build_identity(),
            },
        )?;
        let mut draining = false;
        loop {
            let Some(work) = read_packet::<Work>(&mut input)? else {
                ensure!(draining, "Workbench control stream ended before shutdown");
                return Ok(());
            };
            match work {
                Work::Request {
                    sequence,
                    request_digest: expected,
                    request,
                } => {
                    ensure!(!draining, "request during Workbench shutdown");
                    ensure!(
                        sequence.0 > 0 && request_digest(&request)? == expected,
                        "Workbench request binding mismatch"
                    );
                    send.try_send(CoordinatorCommand::Request {
                        sequence,
                        request_digest: expected,
                        request,
                    })
                    .map_err(|_| anyhow::anyhow!("Workbench bounded coordinator unavailable"))?;
                }
                Work::Shutdown { sequence } => {
                    ensure!(!draining && sequence.0 > 0, "Workbench shutdown sequence");
                    send.try_send(CoordinatorCommand::Shutdown { sequence })
                        .map_err(|_| {
                            anyhow::anyhow!("Workbench shutdown dispatcher unavailable")
                        })?;
                    draining = true;
                }
                Work::CallbackBegin {
                    sequence,
                    bytes,
                    blake3,
                } => {
                    ensure!(startup.managed, "callback reply for unmanaged Workbench");
                    callback.reply(sequence, bytes, blake3)?;
                }
                Work::CallbackChunk {
                    sequence,
                    offset,
                    bytes,
                } => {
                    ensure!(startup.managed, "callback reply for unmanaged Workbench");
                    callback.chunk(sequence, offset, bytes)?;
                }
            }
        }
    })();
    callback.close();
    drop(send);
    if run.is_err() {
        // Never join a coordinator while its external callback transport is
        // unavailable; OS teardown is checked by the retained G child owner.
        std::process::exit(1);
    }
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("Workbench coordinator panicked"))?;
    Ok(())
}

struct Transport {
    input: ChildStdin,
    output: Box<dyn Read + Send>,
    sequence: u64,
    callback: CallbackRequestReceiver,
}

#[cfg(test)]
struct HarnessOutput {
    inner: ChildStdout,
    prefix: Vec<u8>,
    ready: bool,
    copied: usize,
}
#[cfg(test)]
impl Read for HarnessOutput {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        while !self.ready {
            let mut byte = [0];
            if self.inner.read(&mut byte)? == 0 {
                return Ok(0);
            }
            self.prefix.push(byte[0]);
            if self.prefix.ends_with(MAGIC) {
                self.ready = true;
            } else if self.prefix.len() == 512 {
                return Err(std::io::Error::other(
                    "Workbench fixture harness prefix exceeded 512 bytes",
                ));
            }
        }
        if self.copied < MAGIC.len() {
            let size = output.len().min(MAGIC.len() - self.copied);
            output[..size].copy_from_slice(&MAGIC[self.copied..self.copied + size]);
            self.copied += size;
            return Ok(size);
        }
        self.inner.read(output)
    }
}

struct Owner {
    transport: Option<Transport>,
    drained: bool,
    poisoned: Option<String>,
}
impl Owner {
    fn revoke(&mut self, child_owner: &Mutex<Option<Child>>) -> Result<()> {
        drop(self.transport.take());
        let mut slot = child_owner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = slot.as_mut() {
            if child.try_wait()?.is_none() {
                child.kill().context("revoke Workbench child")?;
            }
            child.wait()?;
            slot.take();
        }
        Ok(())
    }
}

struct StartingChild(Option<Child>);
impl StartingChild {
    fn reap(&mut self) -> Result<()> {
        if let Some(child) = self.0.as_mut() {
            if child.try_wait()?.is_none() {
                child.kill().context("revoke starting Workbench child")?;
            }
            child.wait().context("reap starting Workbench child")?;
            self.0.take();
        }
        Ok(())
    }
}
impl Drop for StartingChild {
    fn drop(&mut self) {
        while self.0.is_some() {
            let _ = self.reap();
            if self.0.is_some() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

/// G-owned W child. Calls are serialized and bounded, while W's own operation
/// thread keeps Status/Cancel/Close requests responsive during long actions.
pub struct Client {
    instance: crate::catalog_session::LeaseId,
    owner: Mutex<Owner>,
    child: Mutex<Option<Child>>,
    interrupted: Arc<AtomicBool>,
    managed: Option<Arc<dyn super::lightroom::ManagedIo>>,
}
impl Client {
    pub fn spawn(executable: &Path) -> Result<Self> {
        Self::spawn_inner(executable, None)
    }
    #[allow(dead_code)]
    pub(crate) fn spawn_managed(
        executable: &Path,
        managed: &Arc<dyn super::lightroom::ManagedIo>,
    ) -> Result<Self> {
        // The caller retains the authoritative owner if W startup fails and
        // can explicitly drain/retry it; an app-setup error cannot consume F/S.
        Self::spawn_inner(executable, Some(managed.clone()))
    }
    fn spawn_inner(
        executable: &Path,
        managed: Option<Arc<dyn super::lightroom::ManagedIo>>,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command.arg("--lightroom-workbench-worker");
        Self::spawn_command(command, managed, false)
    }
    #[cfg(test)]
    pub(crate) fn spawn_managed_fixture(
        executable: &Path,
        managed: &Arc<dyn super::lightroom::ManagedIo>,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command.args([
            "--ignored",
            "--exact",
            "application::lightroom_process::tests::owned_workbench_entrypoint",
            "--nocapture",
            "--test-threads=1",
        ]);
        Self::spawn_command(command, Some(managed.clone()), true)
    }
    fn spawn_command(
        mut command: Command,
        managed: Option<Arc<dyn super::lightroom::ManagedIo>>,
        fixture_harness: bool,
    ) -> Result<Self> {
        let startup = Startup::new(managed.is_some());
        startup.validate()?;
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if fixture_harness {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .spawn()?;
        let mut starting = StartingChild(Some(child));
        let bootstrap = (|| -> Result<(Child, ChildStdin, Box<dyn Read + Send>)> {
            let child = starting
                .0
                .as_mut()
                .context("Workbench startup child missing")?;
            let mut input = child.stdin.take().context("Workbench stdin missing")?;
            let output = child.stdout.take().context("Workbench stdout missing")?;
            #[cfg(test)]
            let mut output: Box<dyn Read + Send> = if fixture_harness {
                Box::new(HarnessOutput {
                    inner: output,
                    prefix: Vec::with_capacity(512),
                    ready: false,
                    copied: 0,
                })
            } else {
                Box::new(output)
            };
            #[cfg(not(test))]
            let mut output: Box<dyn Read + Send> = {
                let _ = fixture_harness;
                Box::new(output)
            };
            write_packet(&mut input, &startup)?;
            let ready: Outcome = read_packet(&mut output)?.context("Workbench Ready missing")?;
            ensure!(
                matches!(
                    ready,
                    Outcome::Ready { ref instance, ref build }
                        if instance == &startup.instance && build == &startup.build
                ),
                "Workbench Ready binding mismatch"
            );
            let child = starting.0.take().context("Workbench startup owner lost")?;
            Ok((child, input, output))
        })();
        let (child, input, output) = match bootstrap {
            Ok(ready) => ready,
            Err(primary) => match starting.reap() {
                Ok(()) => return Err(primary),
                Err(cleanup) => {
                    return Err(primary.context(format!(
                        "Workbench startup checked reap also failed: {cleanup:#}"
                    )));
                }
            },
        };
        Ok(Self {
            instance: startup.instance,
            managed,
            child: Mutex::new(Some(child)),
            interrupted: Arc::new(AtomicBool::new(false)),
            owner: Mutex::new(Owner {
                transport: Some(Transport {
                    input,
                    output,
                    sequence: 0,
                    callback: CallbackRequestReceiver::default(),
                }),
                drained: false,
                poisoned: None,
            }),
        })
    }

    fn callback(
        managed: &dyn super::lightroom::ManagedIo,
        request: CallbackRequest,
        cancel: &Arc<AtomicBool>,
    ) -> Result<CallbackValue> {
        match request {
            CallbackRequest::Admit => {
                managed.admit()?;
                Ok(CallbackValue::Admitted)
            }
            CallbackRequest::Commit => {
                managed.commit()?;
                Ok(CallbackValue::Admitted)
            }
            CallbackRequest::Filesystem { request } => Ok(CallbackValue::Filesystem(
                managed.filesystem(request, cancel.as_ref())?,
            )),
            CallbackRequest::SealedDocument { request } => Ok(CallbackValue::SealedDocument {
                value: managed.sealed_document(request, cancel.as_ref())?,
            }),
            CallbackRequest::ArtifactPreparation { request } => {
                Ok(CallbackValue::ArtifactPreparation {
                    value: managed.artifact_preparation(request, cancel.as_ref())?,
                })
            }
            CallbackRequest::SourceOpen { authority } => Ok(CallbackValue::Source {
                value: managed.source_open(authority, cancel.clone())?,
            }),
            CallbackRequest::SourceSqlOpen {
                seal,
                limits,
                protected,
            } => Ok(CallbackValue::Source {
                value: managed.source_sql_open(seal, limits, protected, cancel.clone())?,
            }),
            CallbackRequest::SourceSchema { source } => {
                Ok(CallbackValue::Schema(managed.source_schema(&source)?))
            }
            CallbackRequest::SourceRows {
                source,
                handle,
                cursor,
                limit,
            } => Ok(CallbackValue::Table(managed.source_rows(
                &source,
                handle,
                cursor,
                usize::try_from(limit.0)?,
            )?)),
            CallbackRequest::SourceCurrent { source } => {
                Ok(CallbackValue::Current(managed.source_current(&source)?))
            }
            CallbackRequest::SourceRetire { source } => {
                managed.source_retire(&source)?;
                Ok(CallbackValue::Retired)
            }
        }
    }

    fn begin_callback_request(
        transport: &mut Transport,
        sequence: crate::application::U64,
        length: crate::application::U64,
        blake3: String,
        cancel: &Arc<AtomicBool>,
    ) -> Result<()> {
        transport.callback.begin(sequence, length, blake3, cancel)
    }

    fn push_callback_request(
        transport: &mut Transport,
        sequence: crate::application::U64,
        offset: crate::application::U64,
        bytes: Vec<u8>,
        managed: Option<&Arc<dyn super::lightroom::ManagedIo>>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<()> {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "Workbench callback request canceled"
        );
        let complete = transport
            .callback
            .push(sequence, offset, bytes, cancel.as_ref())?;
        if complete {
            let request = transport.callback.request()?;
            // Retain the exact encoded request until its checked callback reply
            // has been written. This preserves failure evidence and makes the
            // encoded request, decoded graph, and reply-generation overlap
            // explicit to Workbench capacity admission.
            Self::reply_callback(transport, sequence, request, managed, cancel)?;
            transport.callback.finish(sequence)?;
        }
        Ok(())
    }

    fn reply_callback(
        transport: &mut Transport,
        sequence: crate::application::U64,
        request: CallbackRequest,
        managed: Option<&Arc<dyn super::lightroom::ManagedIo>>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<()> {
        let fatal = matches!(&request, CallbackRequest::Admit | CallbackRequest::Commit);
        let result = managed
            .context("unmanaged Workbench requested a supervisor callback")
            .and_then(|managed| Self::callback(managed.as_ref(), request, cancel))
            .map_err(|error| {
                if fatal {
                    Failure::fatal(error)
                } else {
                    Failure::new(error)
                }
            });
        if let Err(error) = &result {
            error.validate()?;
        }
        let encoded = encode_limit(&result, CALLBACK_BYTES)?;
        let digest = crate::lightroom::digest(&encoded);
        write_packet(
            &mut transport.input,
            &Work::CallbackBegin {
                sequence,
                bytes: crate::application::U64(encoded.len().try_into()?),
                blake3: digest,
            },
        )?;
        for (index, bytes) in encoded.chunks(CALLBACK_CHUNK_BYTES).enumerate() {
            write_packet(
                &mut transport.input,
                &Work::CallbackChunk {
                    sequence,
                    offset: crate::application::U64((index * CALLBACK_CHUNK_BYTES) as u64),
                    bytes: bytes.to_vec(),
                },
            )?;
        }
        Ok(())
    }

    fn terminal_failure(&self, error: anyhow::Error, kind: &str) -> anyhow::Error {
        if let Some(managed) = &self.managed {
            managed.revoke_generation();
        }
        let cleanup = self.shutdown();
        if self.checked_drained() {
            error.context(format!(
                "fatal Workbench {kind} checked-drained{}",
                cleanup
                    .err()
                    .map(|value| format!(" after {value:#}"))
                    .unwrap_or_default()
            ))
        } else {
            error.context(format!(
                "fatal Workbench {kind} remains nonterminal; checked drain failed: {:#}",
                cleanup.expect_err("undrained shutdown must report failure")
            ))
        }
    }

    /// Interrupt the exact retained process without waiting for a blocked
    /// transport lock. Reaping and filesystem reconciliation remain in shutdown.
    pub(crate) fn interrupt(&self) -> Result<()> {
        self.interrupted
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(managed) = &self.managed {
            managed.revoke_generation();
        }
        let mut slot = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = slot.as_mut() {
            // Do not wait here: Source owners must be checked-drained first.
            // An already exited child is reconciled by the same shutdown path.
            if let Err(error) = child.kill()
                && error.kind() != std::io::ErrorKind::InvalidInput
            {
                return Err(error).context("interrupt Workbench child");
            }
        }
        Ok(())
    }

    pub fn call(&self, request: Request) -> Result<Response> {
        ensure!(
            !self.interrupted.load(std::sync::atomic::Ordering::Acquire),
            "Workbench generation was interrupted"
        );
        if let Some(managed) = &self.managed
            && let Err(error) = managed.admit()
        {
            return Err(self.terminal_failure(error, "admission"));
        }
        let digest = request_digest(&request)?;
        let mut owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(!owner.drained, "Workbench process is drained");
        ensure!(
            owner.poisoned.is_none(),
            "Workbench process is poisoned: {}",
            owner.poisoned.as_deref().unwrap_or_default()
        );
        let transport_result = (|| -> Result<std::result::Result<Response, Failure>> {
            let transport = owner
                .transport
                .as_mut()
                .context("Workbench transport is retained after failure")?;
            transport.sequence = transport
                .sequence
                .checked_add(1)
                .context("Workbench sequence exhausted")?;
            let sequence = crate::application::U64(transport.sequence);
            write_packet(
                &mut transport.input,
                &Work::Request {
                    sequence,
                    request_digest: digest.clone(),
                    request,
                },
            )?;
            let result = loop {
                let outcome: Outcome = read_packet(&mut transport.output)?
                    .context("Workbench reply lost; child remains owned for checked drain")?;
                match outcome {
                    Outcome::CallbackBegin {
                        sequence,
                        bytes,
                        blake3,
                    } => Self::begin_callback_request(
                        transport,
                        sequence,
                        bytes,
                        blake3,
                        &self.interrupted,
                    )?,
                    Outcome::CallbackChunk {
                        sequence,
                        offset,
                        bytes,
                    } => {
                        Self::push_callback_request(
                            transport,
                            sequence,
                            offset,
                            bytes,
                            self.managed.as_ref(),
                            &self.interrupted,
                        )?;
                    }
                    Outcome::Reply {
                        sequence: actual,
                        request_digest: actual_digest,
                        result,
                    } => {
                        transport.callback.ensure_idle()?;
                        ensure!(
                            actual == sequence && actual_digest == digest,
                            "Workbench reply binding mismatch"
                        );
                        break result;
                    }
                    _ => anyhow::bail!("unexpected Workbench outcome"),
                }
            };
            Ok(result)
        })();
        let result = match transport_result {
            Ok(result) => result,
            Err(error) => {
                owner.poisoned = Some(format!("{error:#}"));
                drop(owner);
                return Err(self.terminal_failure(error, "transport"));
            }
        };
        match result {
            Ok(response) => Ok(response),
            Err(failure) => {
                if failure.fatal {
                    owner.poisoned = Some(failure.detail.clone());
                    drop(owner);
                    return Err(self.terminal_failure(anyhow::anyhow!(failure.detail), "reply"));
                }
                Err(anyhow::anyhow!(failure.detail))
            }
        }
    }

    pub fn shutdown(&self) -> Result<()> {
        let source_result = self
            .managed
            .as_ref()
            .map(|managed| managed.drain_sources())
            .unwrap_or(Ok(()));
        let mut owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        if owner.drained {
            return source_result;
        }
        let graceful = (|| -> Result<()> {
            ensure!(
                owner.poisoned.is_none(),
                "poisoned Workbench requires checked revoke"
            );
            ensure!(
                !self.interrupted.load(std::sync::atomic::Ordering::Acquire),
                "interrupted Workbench requires checked revoke"
            );
            if self
                .child
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
            {
                return Ok(());
            }
            let transport = owner
                .transport
                .as_mut()
                .context("Workbench transport is retained after failure")?;
            transport.sequence = transport
                .sequence
                .checked_add(1)
                .context("Workbench sequence exhausted")?;
            let sequence = crate::application::U64(transport.sequence);
            write_packet(&mut transport.input, &Work::Shutdown { sequence })?;
            loop {
                let outcome: Outcome = read_packet(&mut transport.output)?
                    .context("Workbench drain acknowledgement lost")?;
                match outcome {
                    Outcome::CallbackBegin {
                        sequence,
                        bytes,
                        blake3,
                    } => Self::begin_callback_request(
                        transport,
                        sequence,
                        bytes,
                        blake3,
                        &self.interrupted,
                    )?,
                    Outcome::CallbackChunk {
                        sequence,
                        offset,
                        bytes,
                    } => Self::push_callback_request(
                        transport,
                        sequence,
                        offset,
                        bytes,
                        self.managed.as_ref(),
                        &self.interrupted,
                    )?,
                    Outcome::Drained {
                        sequence: actual,
                        ref instance,
                    } => {
                        transport.callback.ensure_idle()?;
                        ensure!(
                            actual == sequence && instance == &self.instance,
                            "Workbench drain binding mismatch"
                        );
                        break;
                    }
                    _ => anyhow::bail!("unexpected Workbench drain outcome"),
                }
            }
            drop(owner.transport.take());
            let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
            let status = child
                .as_mut()
                .context("Workbench child missing before reap")?
                .wait()?;
            ensure!(status.success(), "Workbench child exited {status}");
            child.take();
            Ok(())
        })();
        let revoke = if graceful.is_err() {
            owner.revoke(&self.child)
        } else {
            Ok(())
        };
        let reaped = self
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
            && owner.transport.is_none();
        if reaped && let Some(managed) = &self.managed {
            managed.workbench_reaped();
        }
        let filesystem_result = if source_result.is_ok() && reaped {
            self.managed
                .as_ref()
                .map(|managed| managed.drain_filesystem())
                .unwrap_or(Ok(()))
        } else {
            Err(anyhow::anyhow!(
                "F reconciliation retained until Source and W are checked-drained"
            ))
        };
        owner.drained = reaped && source_result.is_ok() && filesystem_result.is_ok();
        let mut failures = Vec::new();
        if let Err(error) = source_result {
            failures.push(format!("Source checked drain: {error:#}"));
        }
        if let Err(error) = graceful {
            failures.push(format!("Workbench graceful drain: {error:#}"));
        }
        if let Err(error) = revoke {
            failures.push(format!("Workbench checked revoke: {error:#}"));
        }
        if let Err(error) = filesystem_result {
            failures.push(format!("filesystem reconciliation: {error:#}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("managed Workbench drain failed: {}", failures.join("; "))
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(Child::id)
    }

    pub(crate) fn checked_drained(&self) -> bool {
        self.owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drained
    }

    #[cfg(test)]
    pub(crate) fn terminate_for_test(&self) -> Result<()> {
        let mut slot = self.child.lock().unwrap_or_else(|error| error.into_inner());
        let child = slot.as_mut().context("Workbench child missing")?;
        child.kill().context("terminate Workbench fixture")
    }

    #[cfg(test)]
    pub(crate) fn poison_for_test(&self, detail: &str) {
        self.owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .poisoned = Some(detail.into());
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            {
                let mut owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
                while self
                    .child
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some()
                {
                    let _ = owner.revoke(&self.child);
                    if self
                        .child
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_some()
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            }
            if let Some(managed) = self.managed.take() {
                managed.workbench_reaped();
                let cleanup = managed
                    .drain_sources()
                    .and_then(|()| managed.drain_filesystem());
                if cleanup.is_err() {
                    std::mem::forget(managed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CancelAfterFrame<'a> {
        bytes: Vec<u8>,
        cancel: &'a AtomicBool,
        frames: usize,
    }
    impl Write for CancelAfterFrame<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.frames += 1;
            if self.frames == 1 {
                self.cancel
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            Ok(())
        }
    }

    fn read_callback_request(reader: &mut impl Read) -> Result<(u64, CallbackRequest, usize)> {
        let cancel = AtomicBool::new(false);
        let mut receiver = CallbackRequestReceiver::default();
        let mut frames = 0;
        loop {
            let outcome: Outcome =
                read_packet(reader)?.context("callback request frame missing")?;
            frames += 1;
            match outcome {
                Outcome::CallbackBegin {
                    sequence,
                    bytes,
                    blake3,
                } => receiver.begin(sequence, bytes, blake3, &cancel)?,
                Outcome::CallbackChunk {
                    sequence,
                    offset,
                    bytes,
                } => {
                    if receiver.push(sequence, offset, bytes, &cancel)? {
                        let request = receiver.request()?;
                        receiver.finish(sequence)?;
                        return Ok((sequence.0, request, frames));
                    }
                }
                _ => anyhow::bail!("unexpected callback request frame"),
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum PendingCallbackFault {
        MalformedFrame,
        Eof,
    }

    fn pending_artifact_callback_fault(fault: PendingCallbackFault) -> Result<()> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([
                "--ignored",
                "--exact",
                "application::lightroom_process::tests::owned_workbench_entrypoint",
                "--nocapture",
                "--test-threads=1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let child = command.spawn()?;
        let mut child = StartingChild(Some(child));
        let owned = child.0.as_mut().context("raw Workbench child missing")?;
        #[cfg(unix)]
        let pid = owned.id();
        let mut input = owned.stdin.take().context("raw Workbench stdin missing")?;
        let output = owned
            .stdout
            .take()
            .context("raw Workbench stdout missing")?;
        let mut output = HarnessOutput {
            inner: output,
            prefix: Vec::with_capacity(512),
            ready: false,
            copied: 0,
        };

        let startup = Startup::new(true);
        write_packet(&mut input, &startup)?;
        let ready: Outcome = read_packet(&mut output)?.context("Workbench Ready missing")?;
        ensure!(
            matches!(ready, Outcome::Ready { instance, build } if instance == startup.instance && build == startup.build),
            "raw Workbench Ready binding mismatch"
        );

        let request = Request::ArtifactPreparation {
            request: crate::filesystem_worker::wire::LightroomArtifactPreparation::Discard {
                session: uuid::Uuid::new_v4().to_string(),
            },
        };
        let digest = request_digest(&request)?;
        write_packet(
            &mut input,
            &Work::Request {
                sequence: crate::application::U64(1),
                request_digest: digest,
                request,
            },
        )?;
        let (_, callback, _) = read_callback_request(&mut output)?;
        ensure!(
            matches!(callback, CallbackRequest::ArtifactPreparation { .. }),
            "Workbench did not stop at the artifact callback boundary"
        );

        match fault {
            PendingCallbackFault::MalformedFrame => {
                let mut malformed = [0u8; HEADER_BYTES];
                malformed[..4].copy_from_slice(MAGIC);
                malformed[4] = PROTOCOL.wrapping_add(1);
                input.write_all(&malformed)?;
                input.flush()?;
                drop(input);
            }
            PendingCallbackFault::Eof => drop(input),
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            let owned = child.0.as_mut().context("raw Workbench owner lost")?;
            if let Some(status) = owned.try_wait()? {
                break status;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "raw Workbench did not exit after {fault:?}"
            );
            thread::sleep(std::time::Duration::from_millis(5));
        };
        ensure!(
            !status.success(),
            "raw Workbench reported success after {fault:?}"
        );

        while let Some(outcome) = read_packet::<Outcome>(&mut output)? {
            ensure!(
                !matches!(outcome, Outcome::Drained { .. }),
                "raw Workbench emitted false Drained after {fault:?}"
            );
        }
        ensure!(
            child
                .0
                .as_mut()
                .context("raw Workbench owner lost after wait")?
                .try_wait()?
                .is_some(),
            "exact raw Workbench child was not reaped"
        );
        #[cfg(unix)]
        {
            ensure!(
                unsafe { libc::kill(pid as libc::pid_t, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
                "raw Workbench PID {pid} remains live after {fault:?}"
            );
        }
        child.0.take();
        Ok(())
    }

    #[test]
    #[ignore = "owned Workbench fixture subprocess entrypoint"]
    fn owned_workbench_entrypoint() {
        match worker_main() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("owned Workbench fixture failed: {error:#}");
                std::process::exit(1);
            }
        }
    }

    #[test]
    fn frame_round_trip_rejects_corruption_and_oversize() -> Result<()> {
        let startup = Startup::new(false);
        let mut bytes = Vec::new();
        write_packet(&mut bytes, &startup)?;
        let decoded: Startup = read_packet(&mut bytes.as_slice())?.context("frame")?;
        decoded.validate()?;
        assert_eq!(decoded.instance, startup.instance);
        let end = bytes.len() - 1;
        bytes[end] ^= 1;
        assert!(read_packet::<Startup>(&mut bytes.as_slice()).is_err());
        let mut oversized = Vec::from(*MAGIC);
        oversized.extend_from_slice(&[PROTOCOL, 0, 0, 0]);
        oversized.extend_from_slice(&u32::MAX.to_le_bytes());
        oversized.extend_from_slice(&[0; 36]);
        assert!(read_packet::<Startup>(&mut oversized.as_slice()).is_err());
        Ok(())
    }

    #[test]
    fn callback_values_round_trip_through_the_chunked_payload_envelope() -> Result<()> {
        use crate::lightroom_migration_worker::source_reader::capture_wire as capture;

        let values = [
            ("admitted", CallbackValue::Admitted),
            (
                "filesystem",
                CallbackValue::Filesystem(
                    crate::filesystem_worker::wire::LightroomWorkbenchIoReply::Released {
                        operation: "operation".into(),
                    },
                ),
            ),
            (
                "sealed_document",
                CallbackValue::SealedDocument { value: None },
            ),
            (
                "artifact_preparation",
                CallbackValue::ArtifactPreparation { value: None },
            ),
            (
                "source",
                CallbackValue::Source {
                    value: "capture-sql-1".into(),
                },
            ),
            (
                "schema",
                CallbackValue::Schema(capture::SchemaObjects {
                    authority_binding: "binding".into(),
                    schema_roster_blake3: "roster".into(),
                    objects: vec![],
                    tables: vec![],
                    variables: std::collections::BTreeMap::new(),
                }),
            ),
            (
                "table",
                CallbackValue::Table(capture::TableValue::Batch(capture::TableBatch {
                    authority_binding: "binding".into(),
                    schema_roster_blake3: "roster".into(),
                    table_handle: "table".into(),
                    request_sequence: crate::application::U64(1),
                    rows: vec![],
                    next_cursor: None,
                    eof: true,
                    observed: crate::application::U64(0),
                })),
            ),
            (
                "current",
                CallbackValue::Current(capture::Current {
                    authority_binding: "binding".into(),
                    schema_roster_blake3: "roster".into(),
                    data_version: crate::application::I64(1),
                }),
            ),
            ("retired", CallbackValue::Retired),
        ];
        for (expected, value) in values {
            let result: std::result::Result<CallbackValue, Failure> = Ok(value);
            let bytes = encode_limit(&result, CALLBACK_BYTES)?;
            let decoded: std::result::Result<CallbackValue, Failure> =
                decode_limit(&bytes, CALLBACK_BYTES)?;
            let actual = match decoded {
                Ok(CallbackValue::Admitted) => "admitted",
                Ok(CallbackValue::Filesystem(_)) => "filesystem",
                Ok(CallbackValue::SealedDocument { .. }) => "sealed_document",
                Ok(CallbackValue::ArtifactPreparation { .. }) => "artifact_preparation",
                Ok(CallbackValue::Source { .. }) => "source",
                Ok(CallbackValue::Schema(_)) => "schema",
                Ok(CallbackValue::Table(_)) => "table",
                Ok(CallbackValue::Current(_)) => "current",
                Ok(CallbackValue::Retired) => "retired",
                Err(failure) => anyhow::bail!("callback {expected} failed: {}", failure.detail),
            };
            assert_eq!(actual, expected);
        }
        Ok(())
    }

    #[test]
    fn source_sql_callback_preserves_full_timestamp_authority() -> Result<()> {
        for modified_ns in [1_789_566_682_669_866_761_u128, u128::MAX] {
            let mut seal: crate::lightroom::migration_source::InputSeal = serde_json::from_str(
                &format!(
                    r#"[1,{{"encoding":"UnixBytes","units":[47,116,109,112]}},["object",1,{modified_ns},"changed"],"",["","",""],[],[]]"#
                ),
            )?;
            seal.supplements
                .push(crate::lightroom::migration_source::SupplementPin {
                    revision: "revision".into(),
                    source_id: "source".into(),
                    origin: "embedded".into(),
                    source_revision: crate::xmp_packets::SourceRevision {
                        length: 1,
                        blake3: "digest".into(),
                        modified_unix_ns: Some(u128::MAX),
                    },
                    historical_status: crate::xmp_packets::Status::Complete,
                    proof_blake3: "proof".into(),
                });
            let request = CallbackRequest::SourceSqlOpen {
                seal,
                limits: crate::lightroom::migration_source::ReadLimits::default(),
                protected: vec![],
            };
            let mut wire = Vec::new();
            let cancel = AtomicBool::new(false);
            let encoded = encode_callback_request(&request, &cancel)?;
            write_encoded_callback_request(
                &mut wire,
                crate::application::U64(1),
                &encoded,
                &cancel,
            )?;
            let (sequence, decoded, _) = read_callback_request(&mut wire.as_slice())?;
            let CallbackRequest::SourceSqlOpen { seal, .. } = decoded else {
                anyhow::bail!("source SQL callback kind changed")
            };
            assert_eq!(sequence, 1);
            assert_eq!(seal.identity.modified_ns, Some(modified_ns));
            assert_eq!(
                seal.supplements[0].source_revision.modified_unix_ns,
                Some(u128::MAX)
            );
        }
        Ok(())
    }

    #[test]
    fn source_sql_callback_chunks_the_full_supplement_contract() -> Result<()> {
        let mut seal: crate::lightroom::migration_source::InputSeal = serde_json::from_str(
            r#"[1,{"encoding":"UnixBytes","units":[47,116,109,112]},["object",1,1789566682669866761,"changed"],"",["","",""],[],[]]"#,
        )?;
        let selected_revision = "a".repeat(64);
        seal.blake3 = "b".repeat(64);
        seal.approval.document_blake3 = "c".repeat(64);
        seal.approval.scope = "selected_migration_test".into();
        seal.selected = vec![crate::lightroom::migration_source::SelectedCapture {
            revision: selected_revision.clone(),
            family: "fixture-family".into(),
            family_evidence_digest: "d".repeat(64),
            manifest_blake3: "e".repeat(64),
            evidence_revision: 1,
        }];
        seal.supplements = (0..4_096)
            .map(|index| crate::lightroom::migration_source::SupplementPin {
                revision: selected_revision.clone(),
                source_id: format!("fixture-source-{index:04}"),
                origin: "embedded".into(),
                source_revision: crate::xmp_packets::SourceRevision {
                    length: 1,
                    blake3: "f".repeat(64),
                    modified_unix_ns: Some(u128::MAX),
                },
                historical_status: crate::xmp_packets::Status::Complete,
                proof_blake3: "0".repeat(64),
            })
            .collect();
        seal.approval.roster_blake3 = seal.roster_blake3()?;
        seal.validate()?;
        let request = CallbackRequest::SourceSqlOpen {
            seal,
            limits: crate::lightroom::migration_source::ReadLimits::default(),
            protected: vec![],
        };
        let encoded = encode_limit(&request, CALLBACK_BYTES)?;
        assert!(encoded.len() > ENVELOPE_BYTES);
        let mut wire = Vec::new();
        write_encoded_callback_request(
            &mut wire,
            crate::application::U64(1),
            &encoded,
            &AtomicBool::new(false),
        )?;
        let (sequence, decoded, frames) = read_callback_request(&mut wire.as_slice())?;
        let CallbackRequest::SourceSqlOpen { seal, .. } = decoded else {
            anyhow::bail!("source SQL callback kind changed")
        };
        assert_eq!(sequence, 1);
        assert!(frames > 2);
        assert_eq!(seal.supplements.len(), 4_096);
        assert_eq!(
            seal.supplements
                .last()
                .unwrap()
                .source_revision
                .modified_unix_ns,
            Some(u128::MAX)
        );
        Ok(())
    }

    #[test]
    fn callback_request_receiver_rejects_invalid_or_canceled_streams() -> Result<()> {
        let cancel = AtomicBool::new(false);
        let request = encode_limit(&CallbackRequest::Admit, CALLBACK_BYTES)?;
        let digest = crate::lightroom::digest(&request);

        for (length, digest) in [
            (0, digest.clone()),
            ((CALLBACK_BYTES as u64) + 1, digest.clone()),
            (request.len() as u64, "BAD".into()),
        ] {
            assert!(
                CallbackRequestReceiver::default()
                    .begin(
                        crate::application::U64(1),
                        crate::application::U64(length),
                        digest,
                        &cancel,
                    )
                    .is_err()
            );
        }
        assert!(
            CallbackRequestReceiver::default()
                .begin(
                    crate::application::U64(2),
                    crate::application::U64(request.len() as u64),
                    digest.clone(),
                    &cancel,
                )
                .is_err()
        );
        assert!(
            CallbackRequestReceiver::default()
                .push(
                    crate::application::U64(1),
                    crate::application::U64(0),
                    request.clone(),
                    &cancel,
                )
                .is_err()
        );

        let mut receiver = CallbackRequestReceiver::default();
        receiver.begin(
            crate::application::U64(1),
            crate::application::U64(request.len() as u64),
            digest.clone(),
            &cancel,
        )?;
        assert!(
            receiver
                .begin(
                    crate::application::U64(1),
                    crate::application::U64(request.len() as u64),
                    digest.clone(),
                    &cancel,
                )
                .is_err()
        );
        for (sequence, offset, bytes) in [
            (2, 0, request.clone()),
            (1, 1, request.clone()),
            (1, 0, Vec::new()),
        ] {
            assert!(
                receiver
                    .push(
                        crate::application::U64(sequence),
                        crate::application::U64(offset),
                        bytes,
                        &cancel,
                    )
                    .is_err()
            );
        }
        cancel.store(true, std::sync::atomic::Ordering::Release);
        assert!(
            receiver
                .push(
                    crate::application::U64(1),
                    crate::application::U64(0),
                    request.clone(),
                    &cancel,
                )
                .is_err()
        );
        assert!(receiver.is_pending());
        assert_eq!(receiver.sequence, 0);

        let cancel = AtomicBool::new(false);
        let mut oversized = CallbackRequestReceiver::default();
        oversized.begin(
            crate::application::U64(1),
            crate::application::U64((CALLBACK_CHUNK_BYTES + 1) as u64),
            "0".repeat(64),
            &cancel,
        )?;
        assert!(
            oversized
                .push(
                    crate::application::U64(1),
                    crate::application::U64(0),
                    vec![0; CALLBACK_CHUNK_BYTES + 1],
                    &cancel,
                )
                .is_err()
        );
        let mut overrun = CallbackRequestReceiver::default();
        overrun.begin(
            crate::application::U64(1),
            crate::application::U64(1),
            "0".repeat(64),
            &cancel,
        )?;
        assert!(
            overrun
                .push(
                    crate::application::U64(1),
                    crate::application::U64(0),
                    vec![0, 1],
                    &cancel,
                )
                .is_err()
        );
        let mut changed = CallbackRequestReceiver::default();
        changed.begin(
            crate::application::U64(1),
            crate::application::U64(request.len() as u64),
            "0".repeat(64),
            &cancel,
        )?;
        assert!(changed.push(
            crate::application::U64(1),
            crate::application::U64(0),
            request.clone(),
            &cancel,
        )?);
        assert!(changed.request().is_err());
        assert!(changed.is_pending());

        let mut completed = CallbackRequestReceiver::default();
        completed.begin(
            crate::application::U64(1),
            crate::application::U64(request.len() as u64),
            digest,
            &cancel,
        )?;
        assert!(completed.push(
            crate::application::U64(1),
            crate::application::U64(0),
            request,
            &cancel,
        )?);
        assert!(matches!(completed.request()?, CallbackRequest::Admit));
        assert!(completed.ensure_idle().is_err());
        completed.finish(crate::application::U64(1))?;
        completed.ensure_idle()?;
        assert_eq!(completed.sequence, 1);
        assert!(
            completed
                .begin(
                    crate::application::U64(1),
                    crate::application::U64(1),
                    "0".repeat(64),
                    &cancel,
                )
                .is_err()
        );
        assert!(
            completed
                .begin(
                    crate::application::U64(3),
                    crate::application::U64(1),
                    "0".repeat(64),
                    &cancel,
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn callback_request_send_cancellation_keeps_sequence_and_partial_state_explicit() -> Result<()>
    {
        let request = CallbackRequest::Admit;
        let canceled = AtomicBool::new(true);
        assert!(encode_callback_request(&request, &canceled).is_err());

        let cancel = AtomicBool::new(false);
        let encoded = encode_callback_request(&request, &cancel)?;
        let mut writer = CancelAfterFrame {
            bytes: Vec::new(),
            cancel: &cancel,
            frames: 0,
        };
        assert!(
            write_encoded_callback_request(
                &mut writer,
                crate::application::U64(1),
                &encoded,
                &cancel,
            )
            .is_err()
        );
        assert_eq!(writer.frames, 1);
        let mut bytes = writer.bytes.as_slice();
        let begin: Outcome = read_packet(&mut bytes)?.context("callback Begin missing")?;
        let mut receiver = CallbackRequestReceiver::default();
        let Outcome::CallbackBegin {
            sequence,
            bytes: length,
            blake3,
        } = begin
        else {
            anyhow::bail!("canceled callback did not publish Begin")
        };
        receiver.begin(sequence, length, blake3, &AtomicBool::new(false))?;
        assert!(read_packet::<Outcome>(&mut bytes)?.is_none());
        assert!(receiver.is_pending());
        assert_eq!(receiver.sequence, 0);
        Ok(())
    }

    #[test]
    fn pending_artifact_callback_malformed_or_eof_never_reports_drained() -> Result<()> {
        for fault in [
            PendingCallbackFault::MalformedFrame,
            PendingCallbackFault::Eof,
        ] {
            pending_artifact_callback_fault(fault)?;
        }
        Ok(())
    }
}
