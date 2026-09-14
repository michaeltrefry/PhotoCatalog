//! Fixed reader vocabulary. No locked command accepts a new pathname.
use super::wire::Query;
use crate::{
    application::U64,
    catalog_migration::artifacts::{ArtifactDescriptor, ArtifactLimits},
    lightroom::migration_source::{InputSeal, ReadLimits},
    lightroom_migration_worker::{identity::FileKey, protocol::Guard},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub(super) const AUTHORITY_BYTES: usize = 32 * 1024 * 1024;
// Core pages/manifests remain fully representable, including JSON escaping and
// byte-vector encoding. This is transport overhead, not a smaller core budget.
pub(super) const RESULT_BYTES: usize = 6 * crate::lightroom::MANIFEST_BYTES + 1024 * 1024;
pub(super) const CHUNK_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Epoch {
    pub guard: Guard,
    pub reader: String,
}
impl Epoch {
    pub fn validate(&self) -> Result<()> {
        self.guard.validate()?;
        let mut reader = self.guard.clone();
        reader.operation.clone_from(&self.reader);
        reader.validate()
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SqlLimits {
    pub page_bytes: U64,
    pub inline_bytes: U64,
    pub chunk_bytes: U64,
    pub vm_steps: U64,
    pub deadline_ms: U64,
    pub open_deadline_ms: U64,
}
impl From<ReadLimits> for SqlLimits {
    fn from(v: ReadLimits) -> Self {
        Self {
            page_bytes: U64(v.page_bytes as u64),
            inline_bytes: U64(v.inline_bytes as u64),
            chunk_bytes: U64(v.chunk_bytes as u64),
            vm_steps: U64(v.vm_steps),
            deadline_ms: U64(v.deadline_ms),
            open_deadline_ms: U64(v.open_deadline_ms),
        }
    }
}
impl TryFrom<SqlLimits> for ReadLimits {
    type Error = anyhow::Error;
    fn try_from(v: SqlLimits) -> Result<Self> {
        Ok(Self {
            page_bytes: v.page_bytes.0.try_into()?,
            inline_bytes: v.inline_bytes.0.try_into()?,
            chunk_bytes: v.chunk_bytes.0.try_into()?,
            vm_steps: v.vm_steps.0,
            deadline_ms: v.deadline_ms.0,
            open_deadline_ms: v.open_deadline_ms.0,
        })
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawLimits {
    pub maximum_bytes: U64,
    pub open_deadline_ms: U64,
    pub chunk_deadline_ms: U64,
    pub chunk_bytes: U64,
}
impl From<ArtifactLimits> for RawLimits {
    fn from(v: ArtifactLimits) -> Self {
        Self {
            maximum_bytes: U64(v.maximum_bytes),
            open_deadline_ms: U64(v.open_deadline_ms),
            chunk_deadline_ms: U64(v.chunk_deadline_ms),
            chunk_bytes: U64(v.chunk_bytes as u64),
        }
    }
}
impl TryFrom<RawLimits> for ArtifactLimits {
    type Error = anyhow::Error;
    fn try_from(v: RawLimits) -> Result<Self> {
        Ok(Self {
            maximum_bytes: v.maximum_bytes.0,
            open_deadline_ms: v.open_deadline_ms.0,
            chunk_deadline_ms: v.chunk_deadline_ms.0,
            chunk_bytes: v.chunk_bytes.0.try_into()?,
        })
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "mode", content = "authority", deny_unknown_fields)]
pub(super) enum Authority {
    Sql {
        seal: InputSeal,
        limits: SqlLimits,
        protected: Vec<FileKey>,
    },
    Artifact {
        descriptor: ArtifactDescriptor,
        limits: RawLimits,
        protected: Vec<FileKey>,
    },
}
impl Authority {
    pub fn binding(&self) -> Result<String> {
        Ok(match self {
            Self::Sql { seal, .. } => seal.binding_blake3()?,
            Self::Artifact { descriptor, .. } => {
                crate::lightroom::digest(&crate::lightroom::bounded_json(descriptor, 64 * 1024)?)
            }
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
pub(super) enum Read {
    Sql(Query),
    ArtifactVerify,
    ArtifactChunk { offset: U64 },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(super) enum Request {
    Begin {
        epoch: Epoch,
        role: super::relay::Kind,
        build: String,
        bytes: U64,
        blake3: String,
    },
    Authority {
        epoch: Epoch,
        offset: U64,
        bytes: Vec<u8>,
    },
    Open {
        epoch: Epoch,
    },
    Read {
        epoch: Epoch,
        sequence: U64,
        binding: String,
        query: Read,
    },
    Reserved {
        epoch: Epoch,
        sequence: U64,
        bytes: U64,
    },
    Cancel {
        epoch: Epoch,
    },
    Retire {
        epoch: Epoch,
        completed: U64,
        chain: String,
    },
}
impl Request {
    pub fn epoch(&self) -> &Epoch {
        match self {
            Self::Begin { epoch, .. }
            | Self::Authority { epoch, .. }
            | Self::Open { epoch }
            | Self::Read { epoch, .. }
            | Self::Reserved { epoch, .. }
            | Self::Cancel { epoch }
            | Self::Retire { epoch, .. } => epoch,
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(super) enum Reply {
    Reserve {
        epoch: Epoch,
        sequence: U64,
        bytes: U64,
    },
    Ready {
        epoch: Epoch,
        binding: String,
    },
    Result {
        epoch: Epoch,
        sequence: U64,
        query_blake3: String,
        bytes: U64,
        blake3: String,
    },
    Chunk {
        epoch: Epoch,
        sequence: U64,
        offset: U64,
        bytes: Vec<u8>,
    },
    Ticket {
        epoch: Epoch,
        sequence: U64,
        binding: String,
        blake3: String,
        chain: String,
    },
    Failed {
        epoch: Epoch,
        detail: String,
    },
    Retired {
        epoch: Epoch,
    },
}
impl Reply {
    pub fn epoch(&self) -> &Epoch {
        match self {
            Self::Reserve { epoch, .. }
            | Self::Ready { epoch, .. }
            | Self::Result { epoch, .. }
            | Self::Chunk { epoch, .. }
            | Self::Ticket { epoch, .. }
            | Self::Failed { epoch, .. }
            | Self::Retired { epoch } => epoch,
        }
    }
}
pub(super) fn digest_valid(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(super) fn next_chain(
    previous: &str,
    sequence: u64,
    query: &str,
    result: &str,
) -> Result<String> {
    ensure!(
        digest_valid(previous) && digest_valid(query) && digest_valid(result),
        "reader ticket digest"
    );
    let mut h = blake3::Hasher::new();
    h.update(b"photocatalog-source-ticket-v1\0");
    h.update(previous.as_bytes());
    h.update(&sequence.to_be_bytes());
    h.update(query.as_bytes());
    h.update(result.as_bytes());
    Ok(h.finalize().to_hex().to_string())
}

impl Read {
    pub fn expected_kind(&self) -> super::wire::Kind {
        use super::wire::{Kind as K, Query as Q};
        match self {
            Self::Sql(Q::CaptureManifest { .. }) => K::Manifest,
            Self::Sql(Q::StableSource { .. }) => K::StableSource,
            Self::Sql(Q::OriginPacketRoster { .. }) => K::OriginPacketRoster,
            Self::Sql(Q::Page { .. }) => K::Page,
            Self::Sql(Q::ReadChunk { .. }) | Self::ArtifactChunk { .. } => K::Chunk,
            Self::Sql(Q::Count { .. }) => K::Count,
            Self::Sql(Q::Resolve { .. }) => K::Resolution,
            Self::Sql(Q::ImageLinks { .. }) => K::ImageLinks,
            Self::ArtifactVerify => K::Verified,
        }
    }
}

/// Two passes over an already-owned typed value: count without allocating an
/// output buffer, then fill exactly that admitted byte allocation. A geometric
/// Vec writer's capacity is not bounded by its serialized-length check.
/// This accounts for requested Rust payload bytes, not allocator metadata/RSS.
pub(super) fn exact_json_length<T: Serialize>(
    value: &T,
    maximum: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize> {
    use std::{io, sync::atomic::Ordering};
    struct Count<'a> {
        length: usize,
        maximum: usize,
        cancel: &'a std::sync::atomic::AtomicBool,
    }
    impl io::Write for Count<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.cancel.load(Ordering::Acquire) {
                return Err(io::Error::other("source encoding canceled"));
            }
            if bytes.len() > self.maximum.saturating_sub(self.length) {
                return Err(io::Error::other("source encoded byte limit exceeded"));
            }
            self.length += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count {
        length: 0,
        maximum,
        cancel,
    };
    serde_json::to_writer(&mut count, value)?;
    // Check again before the sole output allocation, including empty values.
    ensure!(!cancel.load(Ordering::Acquire), "source encoding canceled");
    Ok(count.length)
}

pub(super) fn exact_json<T: Serialize>(
    value: &T,
    maximum: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<u8>> {
    use std::{io, sync::atomic::Ordering};
    let length = exact_json_length(value, maximum, cancel)?;
    let mut bytes = vec![0; length];
    struct Fill<'a> {
        remaining: &'a mut [u8],
        cancel: &'a std::sync::atomic::AtomicBool,
    }
    impl io::Write for Fill<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.cancel.load(Ordering::Acquire) {
                return Err(io::Error::other("source encoding canceled"));
            }
            if bytes.len() > self.remaining.len() {
                return Err(io::Error::other("source serializer changed between passes"));
            }
            let remaining = std::mem::take(&mut self.remaining);
            let (target, rest) = remaining.split_at_mut(bytes.len());
            target.copy_from_slice(bytes);
            self.remaining = rest;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut fill = Fill {
        remaining: &mut bytes,
        cancel,
    };
    serde_json::to_writer(&mut fill, value)?;
    ensure!(
        fill.remaining.is_empty(),
        "source serializer changed between passes"
    );
    Ok(bytes)
}

#[cfg(all(test, feature = "internal-capacity-probes"))]
#[test]
fn capacity_fixed_source_transport_layouts() {
    let baseline = crate::capacity_probes::begin();
    crate::capacity_probes::fixed_layout::<Request>("SourceRequest");
    crate::capacity_probes::fixed_layout::<Reply>("SourceReply");
    crate::capacity_probes::report("fixed-source-transport", baseline);
}
