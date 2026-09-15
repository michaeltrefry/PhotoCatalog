//! Bounded C-to-F import-source protocol. F owns every directory/file handle and
//! the import lock; C owns discovery SQL and catalog publication.
use super::{LeaseId, RootCapability, validate_path};
use crate::{
    application::U64,
    catalog_metadata::Source,
    storage_volume::{NativePath, VolumeLocation},
    xmp_packets,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const CHUNK_BYTES: usize = 16 * 1024;
pub const DIRECTORY_FACTS: usize = 128;
pub const DIRECTORY_FACT_PATH_UNITS: usize = 96 * 1024;
pub const MAX_WALK_ENTRIES: u64 = 1_000_000;
pub const MAX_DIRECTORIES: usize = 100_000;
pub const MAX_INSPECTION_METADATA_BYTES: usize = 8 * 1024 * 1024;
// Preserve both configured 64 MiB packet pools, the complete metadata limit,
// every one of the 2 * 1024 length prefixes, and the fixed envelope header.
pub const MAX_INSPECTION_BYTES: usize =
    136 * 1024 * 1024 + 2 * 1024 * std::mem::size_of::<u64>() + 20;
pub const ORIGINAL_ROOTS: usize = 1024;
pub const ORIGINAL_ROOT_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn validate_source_root(source: &NativePath) -> Result<()> {
    validate_path(source)?;
    ensure!(
        source.to_path()?.is_absolute(),
        "import source must be absolute"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Action {
    Begin { source: NativePath },
    Next,
    Inspect { source: Source },
    Read { offset: U64 },
    FinishInspection,
    ValidateInspection,
    ReleaseInspection { grant: LeaseId },
    ValidateFile,
    ReleaseFile { grant: LeaseId },
    Finish,
    Abort,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: RootCapability,
    pub transfer: LeaseId,
    pub step: U64,
    pub action: Action,
}
impl Request {
    pub fn cleanup(&self) -> bool {
        matches!(self.action, Action::Abort | Action::Finish)
    }
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        ensure!(self.step.0 < u64::MAX, "import step exhausted");
        match &self.action {
            Action::Begin { source } => validate_source_root(source)?,
            Action::Inspect { source } => {
                ensure!(
                    matches!(source.kind.as_str(), "embedded" | "sidecar"),
                    "invalid import source kind"
                );
                validate_path(&NativePath::from_path(&source_path(source)?))?;
                ensure!(
                    source.display.len() <= 128 * 1024,
                    "import source display limit"
                );
                ensure!(
                    serde_json::to_vec(&source.provenance)?.len() <= 64 * 1024,
                    "import source provenance limit"
                );
            }
            _ => {}
        }
        crate::filesystem_worker::wire::encode(
            self,
            crate::filesystem_worker::wire::MESSAGE_BYTES,
        )?;
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        Ok(blake3::hash(&serde_json::to_vec(self)?)
            .to_hex()
            .to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryFact {
    pub path: NativePath,
    pub regular: bool,
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
    reason = "inline observations are included in the bounded relay response root"
)]
pub enum Value {
    Failed(crate::filesystem_worker::wire::Failure),
    Begun {
        source: NativePath,
    },
    DirectoryStart {
        directory: NativePath,
    },
    DirectoryFacts {
        directory: NativePath,
        facts: Vec<DirectoryFact>,
    },
    DirectoryEnd {
        directory: NativePath,
    },
    Skipped,
    Header {
        path: NativePath,
        fingerprint: String,
        observation: VolumeLocation,
    },
    Inspection {
        source: Source,
        bytes: U64,
        checksum: String,
    },
    InspectionFailed {
        source: Source,
        message: String,
    },
    Chunk {
        offset: U64,
        checksum: String,
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    InspectionFinished,
    InspectionValidated {
        grant: LeaseId,
    },
    InspectionReleased {
        grant: LeaseId,
    },
    FileValidated {
        grant: LeaseId,
    },
    FileReleased {
        grant: LeaseId,
    },
    WalkFinished,
    Finished,
    Aborted,
}
impl Value {
    pub fn binary(&self) -> Option<&[u8]> {
        match self {
            Self::Chunk { bytes, .. } => Some(bytes),
            _ => None,
        }
    }
    pub fn set_binary(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Chunk { bytes: value, .. } => {
                ensure!(
                    !bytes.is_empty() && bytes.len() <= CHUNK_BYTES,
                    "import chunk admission"
                );
                *value = bytes.to_vec();
            }
            _ => ensure!(bytes.is_empty(), "unexpected import binary trailer"),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub root: RootCapability,
    pub transfer: LeaseId,
    pub step: U64,
    pub request_digest: String,
    pub value: Value,
}
impl Reply {
    pub fn validate(&self, request: &Request) -> Result<()> {
        request.validate()?;
        ensure!(
            self.root == request.root
                && self.transfer == request.transfer
                && self.step == request.step,
            "import reply provenance mismatch"
        );
        ensure!(
            self.request_digest == request.digest()?,
            "import reply request mismatch"
        );
        match (&request.action, &self.value) {
            (_, Value::Failed(failure)) => failure.validate()?,
            (Action::Begin { .. }, Value::Begun { .. })
            | (
                Action::Next,
                Value::Skipped
                | Value::DirectoryStart { .. }
                | Value::DirectoryFacts { .. }
                | Value::DirectoryEnd { .. }
                | Value::Header { .. }
                | Value::WalkFinished,
            )
            | (Action::Inspect { .. }, Value::Inspection { .. } | Value::InspectionFailed { .. })
            | (Action::Read { .. }, Value::Chunk { .. })
            | (Action::FinishInspection, Value::InspectionFinished)
            | (Action::ValidateInspection, Value::InspectionValidated { .. })
            | (Action::ReleaseInspection { .. }, Value::InspectionReleased { .. })
            | (Action::ValidateFile, Value::FileValidated { .. })
            | (Action::ReleaseFile { .. }, Value::FileReleased { .. })
            | (Action::Finish, Value::Finished)
            | (Action::Abort, Value::Aborted) => {}
            _ => anyhow::bail!("unexpected import reply"),
        }
        if let Value::Begun { source } = &self.value {
            validate_source_root(source)?;
        }
        if let Value::DirectoryFacts { directory, facts } = &self.value {
            validate_path(directory)?;
            ensure!(
                !facts.is_empty() && facts.len() <= DIRECTORY_FACTS,
                "directory fact chunk limit"
            );
            let mut path_units = 0usize;
            for fact in facts {
                validate_path(&fact.path)?;
                path_units = path_units
                    .checked_add(match &fact.path {
                        NativePath::UnixBytes(value) => value.len(),
                        NativePath::WindowsWide(value) => value.len(),
                    })
                    .context("directory fact path units overflow")?;
            }
            ensure!(
                path_units <= DIRECTORY_FACT_PATH_UNITS,
                "directory fact path-unit limit"
            );
        }
        if let Value::Header {
            path,
            fingerprint,
            observation,
        } = &self.value
        {
            validate_path(path)?;
            ensure!(
                fingerprint.len() == 64 && fingerprint.bytes().all(|b| b.is_ascii_hexdigit()),
                "import fingerprint"
            );
            ensure!(
                &observation.requested_path == path,
                "import observation path mismatch"
            );
        }
        if let Value::Inspection {
            source,
            bytes,
            checksum,
        } = &self.value
        {
            ensure!(
                matches!(&request.action, Action::Inspect { source: expected } if same_source(expected, source)),
                "inspection source mismatch"
            );
            ensure!(
                (1..=MAX_INSPECTION_BYTES as u64).contains(&bytes.0),
                "inspection transfer limit"
            );
            hex(checksum)?;
        }
        if let Value::InspectionFailed { source, message } = &self.value {
            ensure!(
                matches!(&request.action, Action::Inspect { source: expected } if same_source(expected, source)),
                "failed inspection source mismatch"
            );
            ensure!(
                !message.is_empty() && message.len() <= 2048,
                "failed inspection detail limit"
            );
        }
        if let Value::Chunk {
            offset,
            checksum,
            bytes,
        } = &self.value
        {
            ensure!(
                matches!(request.action, Action::Read { offset: expected } if expected.0 == offset.0),
                "inspection chunk offset mismatch"
            );
            ensure!(
                !bytes.is_empty() && bytes.len() <= CHUNK_BYTES,
                "inspection chunk limit"
            );
            ensure!(
                blake3::hash(bytes).to_hex().as_str() == checksum,
                "inspection chunk checksum"
            );
        }
        match (&request.action, &self.value) {
            (
                Action::ReleaseInspection { grant: expected },
                Value::InspectionReleased { grant },
            )
            | (Action::ReleaseFile { grant: expected }, Value::FileReleased { grant }) => {
                ensure!(grant == expected, "import release grant mismatch");
            }
            _ => {}
        }
        crate::filesystem_worker::wire::encode(
            self,
            crate::filesystem_worker::wire::MESSAGE_BYTES,
        )?;
        Ok(())
    }
}

fn same_source(left: &Source, right: &Source) -> bool {
    left.kind == right.kind
        && left.locator == right.locator
        && left.display == right.display
        && left.ambiguous == right.ambiguous
        && left.provenance == right.provenance
}

fn hex(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "import digest"
    );
    Ok(())
}

pub fn source_path(source: &Source) -> Result<std::path::PathBuf> {
    let native = if cfg!(unix) {
        NativePath::UnixBytes(source.locator.clone())
    } else {
        ensure!(
            source.locator.len().is_multiple_of(2),
            "Windows import locator byte width"
        );
        NativePath::WindowsWide(
            source
                .locator
                .as_chunks::<2>()
                .0
                .iter()
                .map(|v| u16::from_le_bytes([v[0], v[1]]))
                .collect(),
        )
    };
    native.to_path().context("decode import source path")
}

/// Compact binary envelope: JSON carries structure with empty byte vectors and
/// exact packet/input bytes follow as length-prefixed binary fields.
pub fn encode_inspection(mut value: xmp_packets::Inspection) -> Result<Vec<u8>> {
    validate_inspection(&value)?;
    let mut blobs = Vec::with_capacity(value.packets.len() + value.parse_inputs.len());
    for packet in &mut value.packets {
        blobs.push(std::mem::take(&mut packet.bytes));
    }
    for input in &mut value.parse_inputs {
        blobs.push(std::mem::take(&mut input.bytes));
    }
    let metadata = crate::filesystem_worker::wire::encode(&value, MAX_INSPECTION_METADATA_BYTES)?;
    let total = 20usize
        .checked_add(metadata.len())
        .and_then(|n| {
            blobs
                .iter()
                .try_fold(n, |n, b| n.checked_add(8)?.checked_add(b.len()))
        })
        .context("inspection transfer length overflow")?;
    ensure!(
        total <= MAX_INSPECTION_BYTES,
        "inspection transfer byte limit"
    );
    let mut output = Vec::new();
    output.try_reserve_exact(total)?;
    output.extend_from_slice(b"PCIM");
    output.extend_from_slice(&(metadata.len() as u64).to_le_bytes());
    output.extend_from_slice(&(blobs.len() as u64).to_le_bytes());
    output.extend_from_slice(&metadata);
    for blob in blobs {
        output.extend_from_slice(&(blob.len() as u64).to_le_bytes());
        output.extend_from_slice(&blob);
    }
    ensure!(output.len() == total, "inspection transfer size changed");
    Ok(output)
}

pub fn decode_inspection(bytes: &[u8]) -> Result<xmp_packets::Inspection> {
    ensure!(
        bytes.len() <= MAX_INSPECTION_BYTES && bytes.len() >= 20 && &bytes[..4] == b"PCIM",
        "invalid inspection transfer"
    );
    let metadata_len = usize::try_from(u64::from_le_bytes(bytes[4..12].try_into().unwrap()))?;
    let blob_count = usize::try_from(u64::from_le_bytes(bytes[12..20].try_into().unwrap()))?;
    ensure!(
        metadata_len <= MAX_INSPECTION_METADATA_BYTES,
        "inspection metadata limit"
    );
    let metadata_end = 20usize
        .checked_add(metadata_len)
        .filter(|n| *n <= bytes.len())
        .context("truncated inspection metadata")?;
    let mut value: xmp_packets::Inspection = crate::filesystem_worker::wire::decode(
        &bytes[20..metadata_end],
        MAX_INSPECTION_METADATA_BYTES,
    )?;
    ensure!(
        blob_count
            == value
                .packets
                .len()
                .checked_add(value.parse_inputs.len())
                .context("inspection blob count overflow")?,
        "inspection blob count mismatch"
    );
    let mut at = metadata_end;
    for target in value
        .packets
        .iter_mut()
        .map(|p| &mut p.bytes)
        .chain(value.parse_inputs.iter_mut().map(|p| &mut p.bytes))
    {
        let end = at
            .checked_add(8)
            .filter(|n| *n <= bytes.len())
            .context("truncated inspection blob length")?;
        let length = usize::try_from(u64::from_le_bytes(bytes[at..end].try_into().unwrap()))?;
        at = end;
        ensure!(
            length <= xmp_packets::Limits::default().max_packet_bytes,
            "inspection blob byte limit"
        );
        let end = at
            .checked_add(length)
            .filter(|n| *n <= bytes.len())
            .context("truncated inspection blob")?;
        target.try_reserve_exact(length)?;
        target.extend_from_slice(&bytes[at..end]);
        at = end;
    }
    ensure!(at == bytes.len(), "trailing inspection bytes");
    validate_inspection(&value)?;
    Ok(value)
}

fn validate_inspection(value: &xmp_packets::Inspection) -> Result<()> {
    let limits = xmp_packets::Limits::default();
    hex(&value.revision.blake3)?;
    ensure!(
        value.packets.len() <= limits.max_packets && value.parse_inputs.len() <= limits.max_packets,
        "inspection packet count limit"
    );
    let packet_bytes = value.packets.iter().try_fold(0usize, |total, packet| {
        ensure!(
            packet.bytes.len() <= limits.max_packet_bytes,
            "inspection packet byte limit"
        );
        ensure!(
            blake3::hash(&packet.bytes).to_hex().as_str() == packet.blake3,
            "inspection packet mismatch"
        );
        total
            .checked_add(packet.bytes.len())
            .context("inspection packet byte overflow")
    })?;
    let input_bytes = value.parse_inputs.iter().try_fold(0usize, |total, input| {
        ensure!(
            input.bytes.len() <= limits.max_packet_bytes,
            "inspection parse input byte limit"
        );
        ensure!(
            blake3::hash(&input.bytes).to_hex().as_str() == input.blake3,
            "inspection parse input mismatch"
        );
        ensure!(
            input
                .packet_indices
                .iter()
                .all(|index| *index < value.packets.len()),
            "inspection parse input packet index"
        );
        total
            .checked_add(input.bytes.len())
            .context("inspection parse input byte overflow")
    })?;
    ensure!(
        packet_bytes <= limits.max_retained_bytes,
        "inspection retained packet byte limit"
    );
    ensure!(
        input_bytes <= limits.max_parse_bytes,
        "inspection parse byte limit"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn inspection_envelope_preserves_exact_packet_parse_input_and_unknown_attributes() -> Result<()>
    {
        let packet = b"\0full packet\xff".to_vec();
        let input = b"<x:xmpmeta unknown='preserved'/>".to_vec();
        let inspection = xmp_packets::Inspection {
            revision: xmp_packets::SourceRevision {
                length: 41,
                blake3: "a".repeat(64),
                modified_unix_ns: Some(1),
            },
            status: xmp_packets::Status::Complete,
            packets: vec![xmp_packets::Packet {
                container: xmp_packets::Container::Sidecar,
                bytes: packet.clone(),
                blake3: blake3::hash(&packet).to_hex().to_string(),
                ranges: vec![xmp_packets::ByteRange {
                    offset: 0,
                    length: packet.len() as u64,
                }],
                group: "unknown-carrier".into(),
                attributes: BTreeMap::from([("unknown".into(), "verbatim".into())]),
            }],
            parse_inputs: vec![xmp_packets::ParseInput {
                bytes: input.clone(),
                blake3: blake3::hash(&input).to_hex().to_string(),
                packet_indices: vec![0],
                transformation: xmp_packets::Transformation::Identity,
                group: "unknown-carrier".into(),
            }],
            issues: vec![],
        };
        let encoded = encode_inspection(inspection)?;
        let decoded = decode_inspection(&encoded)?;
        assert_eq!(decoded.packets[0].bytes, packet);
        assert_eq!(decoded.parse_inputs[0].bytes, input);
        assert_eq!(decoded.packets[0].attributes["unknown"], "verbatim");
        let mut corrupt = encoded;
        *corrupt.last_mut().context("encoded inspection byte")? ^= 1;
        assert!(decode_inspection(&corrupt).is_err());
        Ok(())
    }
}
