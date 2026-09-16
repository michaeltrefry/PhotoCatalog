//! Private cache object operations. Regular-file ownership stays in F.
use super::{LeaseId, RootCapability};
use crate::application::U64;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const CHUNK_BYTES: usize = 16 * 1024;
pub const OBJECT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Object {
    pub root: LeaseId,
    pub key: String,
}
impl Object {
    pub fn validate(&self) -> Result<()> {
        hex(&self.key)
    }
}
pub fn hex(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid cache object digest"
    );
    Ok(())
}
pub fn temporary(value: &str, key: &str) -> Result<()> {
    ensure!(
        value.starts_with(key)
            && value.ends_with(".pending")
            && !value.contains('/')
            && !value.contains('\\'),
        "invalid pending cache object name"
    );
    Ok(())
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub object: Object,
    pub bytes: U64,
    pub checksum: String,
}
impl Expected {
    pub fn validate(&self) -> Result<()> {
        self.object.validate()?;
        hex(&self.checksum)?;
        ensure!(self.bytes.0 <= OBJECT_BYTES, "cache object size limit");
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Action {
    Check(Expected),
    BeginRead {
        expected: Expected,
        allowance: U64,
    },
    Read {
        offset: U64,
    },
    BeginWrite {
        expected: Expected,
        temporary: String,
    },
    Write {
        offset: U64,
        checksum: String,
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    Finish,
    Abort,
    Remove {
        object: Object,
        temporary: Option<String>,
    },
    InspectRelocation {
        target: LeaseId,
    },
    AdmitRelocation {
        target: LeaseId,
    },
    CheckRelocation {
        target: LeaseId,
        id: String,
    },
    Relocate {
        source: LeaseId,
        target: LeaseId,
        id: String,
        key: String,
        bytes: U64,
        checksum: String,
        cleanup: bool,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: RootCapability,
    pub group: LeaseId,
    pub operation: U64,
    pub step: U64,
    pub action: Action,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "zero cache operation identity");
        super::store::path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        match &self.action {
            Action::Check(e) | Action::BeginRead { expected: e, .. } => e.validate()?,
            Action::BeginWrite {
                expected,
                temporary: name,
            } => {
                expected.validate()?;
                ensure!(expected.bytes.0 > 0, "empty cache publication");
                temporary(name, &expected.object.key)?;
                ensure!(
                    name.len() <= 128,
                    super::store::ResourceLimit(
                        "Generated cache temporary name exceeds its transfer admission"
                    )
                );
            }
            Action::Write {
                checksum, bytes, ..
            } => {
                hex(checksum)?;
                ensure!(
                    !bytes.is_empty() && bytes.len() <= CHUNK_BYTES,
                    "cache upload chunk size"
                );
                ensure!(
                    blake3::hash(bytes).to_hex().as_str() == checksum,
                    "cache upload chunk checksum"
                );
            }
            Action::Remove {
                object,
                temporary: name,
            } => {
                object.validate()?;
                if let Some(name) = name {
                    temporary(name, &object.key)?;
                    ensure!(
                        name.len() <= super::ENVELOPE_BYTES,
                        super::store::ResourceLimit(
                            "Saved cache temporary name exceeds the 1 MiB metadata admission; no filesystem effect occurred"
                        )
                    );
                }
            }
            Action::CheckRelocation { id, .. } => relocation_id(id)?,
            Action::Relocate {
                id,
                key,
                checksum,
                bytes,
                ..
            } => {
                relocation_id(id)?;
                hex(key)?;
                hex(checksum)?;
                let _ = bytes; // C applies the existing per-step relocation byte allowance.
            }
            _ => {}
        }
        Ok(())
    }
    pub fn cleanup(&self) -> bool {
        matches!(self.action, Action::Abort)
    }
    pub fn binary(&self) -> Option<&[u8]> {
        match &self.action {
            Action::Write { bytes, .. } => Some(bytes),
            _ => None,
        }
    }
    pub fn set_binary(&mut self, value: &[u8]) -> Result<()> {
        match &mut self.action {
            Action::Write { bytes, .. } => {
                ensure!(value.len() <= CHUNK_BYTES, "cache chunk admission");
                *bytes = value.to_vec();
            }
            _ => ensure!(value.is_empty(), "unexpected cache binary input"),
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<[u8; 32]> {
        let metadata = crate::filesystem_worker::wire::encode(self, 1024 * 1024)?;
        let mut hash = blake3::Hasher::new();
        hash.update(&metadata);
        if let Some(bytes) = self.binary() {
            hash.update(bytes);
        }
        Ok(*hash.finalize().as_bytes())
    }
}
/// Only F's admitted ObjectOwner creates this evidence. Receiving a transport
/// error or an outer helper rejection is not proof that an object step entered F.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureReceipt {
    pub operation: U64,
    pub step: U64,
    pub request_digest: [u8; 32],
}
impl FailureReceipt {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "zero failed object receipt identity");
        Ok(())
    }
    pub fn matches(&self, request: &Request) -> Result<bool> {
        self.validate()?;
        Ok(self.operation == request.operation
            && self.step == request.step
            && self.request_digest == request.digest()?)
    }
}
fn relocation_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 128,
        "relocation identity bound"
    );
    Ok(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Integrity {
    Intact,
    Missing,
    Corrupt,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Value {
    Unit,
    Integrity(Integrity),
    Chunk {
        offset: U64,
        checksum: String,
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    Relocation(String),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub epoch: LeaseId,
    pub session: LeaseId,
    pub group: LeaseId,
    pub operation: U64,
    pub step: U64,
    pub value: Value,
}
impl Reply {
    pub fn binary(&self) -> Option<&[u8]> {
        match &self.value {
            Value::Chunk { bytes, .. } => Some(bytes),
            _ => None,
        }
    }
    pub fn set_binary(&mut self, value: &[u8]) -> Result<()> {
        match &mut self.value {
            Value::Chunk { bytes, .. } => {
                ensure!(value.len() <= CHUNK_BYTES, "cache reply chunk admission");
                *bytes = value.to_vec();
            }
            _ => ensure!(value.is_empty(), "unexpected cache binary reply"),
        }
        Ok(())
    }
    pub fn validate(&self, request: &Request) -> Result<()> {
        ensure!(
            self.epoch == request.root.epoch
                && self.session == request.root.session
                && self.group == request.group
                && self.operation == request.operation
                && self.step == request.step,
            "cache reply binding mismatch"
        );
        match (&request.action, &self.value) {
            (Action::Check(_) | Action::BeginRead { .. }, Value::Integrity(_)) => {}
            (
                Action::Read { offset: expected },
                Value::Chunk {
                    offset,
                    checksum,
                    bytes,
                },
            ) => {
                ensure!(
                    expected == offset
                        && bytes.len() <= CHUNK_BYTES
                        && blake3::hash(bytes).to_hex().as_str() == checksum,
                    "cache chunk reply mismatch"
                );
            }
            (Action::AdmitRelocation { .. }, Value::Relocation(id)) => relocation_id(id)?,
            (Action::Read { .. } | Action::Finish, Value::Integrity(Integrity::Corrupt)) => {}
            (
                Action::BeginWrite { .. }
                | Action::Write { .. }
                | Action::Finish
                | Action::Abort
                | Action::InspectRelocation { .. }
                | Action::Remove { .. }
                | Action::CheckRelocation { .. }
                | Action::Relocate { .. },
                Value::Unit,
            ) => {}
            _ => anyhow::bail!("unexpected cache operation reply"),
        }
        Ok(())
    }
}

/// Metadata is JSON; the chunk is a raw trailer, never a JSON byte array.
/// Each exchange remains within the existing complete-message bound.
pub(crate) fn pack(value: &impl Serialize, binary: Option<&[u8]>, cap: usize) -> Result<Vec<u8>> {
    use std::io::Write;
    struct Count {
        length: usize,
        cap: usize,
        exceeded: bool,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let Some(length) = self
                .length
                .checked_add(bytes.len())
                .filter(|n| *n <= self.cap)
            else {
                self.exceeded = true;
                return Err(std::io::Error::other("cache metadata admission"));
            };
            self.length = length;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let header = if binary.is_some() { 8 } else { 0 };
    let payload = binary.map_or(0, <[u8]>::len);
    ensure!(payload <= CHUNK_BYTES, "binary cache chunk limit");
    let overhead = header + payload;
    ensure!(
        overhead <= cap,
        super::store::ResourceLimit("Cache transport envelope admission exhausted before dispatch")
    );
    let mut count = Count {
        length: 0,
        cap: cap - overhead,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut count, value) {
        if count.exceeded {
            return Err(crate::filesystem_worker::wire::Failure::new(crate::filesystem_worker::wire::FailureKind::ResourceLimit, "Cache metadata exceeds the 1 MiB aggregate transport admission; no filesystem effect occurred").into());
        }
        return Err(error.into());
    }
    let total = overhead + count.length;
    let mut output = Vec::new();
    output.try_reserve_exact(total)?;
    output.resize(total, 0);
    if header != 0 {
        output[..4].copy_from_slice(b"PCIO");
        output[4..8].copy_from_slice(&(count.length as u32).to_le_bytes());
    }
    let mut cursor = std::io::Cursor::new(&mut output[header..header + count.length]);
    serde_json::to_writer(&mut cursor, value)?;
    ensure!(
        cursor.position() as usize == count.length,
        "cache serializer changed length"
    );
    if let Some(binary) = binary {
        output[header + count.length..].copy_from_slice(binary);
    }
    Ok(output)
}
pub(crate) fn unpack<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    cap: usize,
) -> Result<(T, &[u8])> {
    ensure!(bytes.len() <= cap, "cache envelope limit");
    if bytes.starts_with(b"PCIO") {
        ensure!(bytes.len() >= 8, "truncated cache metadata header");
        let length = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let end = 8usize
            .checked_add(length)
            .filter(|n| *n <= bytes.len())
            .ok_or_else(|| anyhow::anyhow!("truncated cache metadata"))?;
        ensure!(
            bytes.len() - end <= CHUNK_BYTES,
            "cache binary trailer limit"
        );
        Ok((serde_json::from_slice(&bytes[8..end])?, &bytes[end..]))
    } else {
        Ok((serde_json::from_slice(bytes)?, &[]))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub step: U64,
    pub offset: U64,
    pub bytes: U64,
    pub receiving: bool,
    pub unresolved: bool,
    pub failure: Option<(crate::filesystem_worker::wire::FailureKind, String)>,
}
impl Progress {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.offset.0 <= self.bytes.0 && self.bytes.0 <= OBJECT_BYTES,
            "cache status byte bound"
        );
        if let Some((_, message)) = &self.failure {
            ensure!(message.len() <= 4096, "cache status failure bound");
        }
        Ok(())
    }
}
