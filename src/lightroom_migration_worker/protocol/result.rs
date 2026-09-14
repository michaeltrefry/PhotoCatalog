//! Exact result sizing and chunk publication. The typed value is serialized
//! twice so the parent can reserve its retained copy before any result byte is
//! published; neither pass constructs a complete temporary JSON string.
use super::{ChildFrame, Guard, Publish};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::io::{self, Write};

const CHUNK: usize = 16 * 1024;
const DIGEST_BYTES: usize = 64;
const SUPPLEMENT_SOURCE_ID_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Measured {
    pub(crate) bytes: usize,
    pub(crate) blake3: String,
}

/// Created only by `Controls` after an exact parent ResultGrant.
pub(crate) struct Grant(Measured);
impl Grant {
    pub(super) fn new(measured: Measured) -> Self {
        Self(measured)
    }
    pub(crate) fn measured(&self) -> &Measured {
        &self.0
    }
}

struct Counter {
    bytes: usize,
    hash: blake3::Hasher,
}
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("migration result byte count overflow"))?;
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn measure(value: &impl Serialize, maximum: usize) -> Result<Measured> {
    let mut counter = Counter {
        bytes: 0,
        hash: blake3::Hasher::new(),
    };
    serde_json::to_writer(&mut counter, value)?;
    ensure!(
        counter.bytes <= maximum,
        "migration result accepted-type byte bound"
    );
    Ok(Measured {
        bytes: counter.bytes,
        blake3: counter.hash.finalize().to_hex().to_string(),
    })
}

struct Chunks<'a> {
    guard: &'a Guard,
    output: &'a dyn Publish,
    expected: &'a Measured,
    offset: usize,
    hash: blake3::Hasher,
    pending: Vec<u8>,
}
impl Chunks<'_> {
    fn publish_prefix(&mut self, boundary: usize) -> io::Result<()> {
        let tail = self.pending.split_off(boundary);
        let prefix = std::mem::replace(&mut self.pending, tail);
        let text = String::from_utf8(prefix)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "result is not UTF-8"))?;
        self.hash.update(text.as_bytes());
        self.output
            .publish(&ChildFrame::Result {
                guard: self.guard.clone(),
                offset: crate::application::U64(self.offset as u64),
                text,
            })
            .map_err(io::Error::other)?;
        self.offset = self
            .offset
            .checked_add(boundary)
            .ok_or_else(|| io::Error::other("migration result offset overflow"))?;
        Ok(())
    }
    fn publish_full_chunk(&mut self) -> io::Result<()> {
        debug_assert!(self.pending.len() >= CHUNK);
        let boundary = match std::str::from_utf8(&self.pending[..CHUNK]) {
            Ok(_) => CHUNK,
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "result serializer emitted invalid UTF-8",
                ));
            }
        };
        if boundary == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "result UTF-8 scalar exceeds chunk bound",
            ));
        }
        self.publish_prefix(boundary)
    }
    fn finish(mut self) -> Result<()> {
        if !self.pending.is_empty() {
            let length = self.pending.len();
            std::str::from_utf8(&self.pending).context("result is not UTF-8")?;
            self.publish_prefix(length)?;
        }
        ensure!(
            self.offset == self.expected.bytes
                && self.hash.finalize().to_hex().as_str() == self.expected.blake3,
            "migration result changed after parent admission"
        );
        Ok(())
    }
}
impl Write for Chunks<'_> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let original = bytes.len();
        while !bytes.is_empty() {
            let available = CHUNK.saturating_sub(self.pending.len());
            if available == 0 {
                self.publish_full_chunk()?;
                continue;
            }
            let copied = available.min(bytes.len());
            self.pending.extend_from_slice(&bytes[..copied]);
            bytes = &bytes[copied..];
        }
        Ok(original)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn publish(
    value: &impl Serialize,
    grant: Grant,
    guard: &Guard,
    output: &dyn Publish,
) -> Result<()> {
    let mut chunks = Chunks {
        guard,
        output,
        expected: grant.measured(),
        offset: 0,
        hash: blake3::Hasher::new(),
        pending: Vec::with_capacity(CHUNK),
    };
    serde_json::to_writer(&mut chunks, value)?;
    chunks.finish()?;
    output.publish(&ChildFrame::Finished {
        guard: guard.clone(),
        result_blake3: grant.measured().blake3.clone(),
        bytes: crate::application::U64(grant.measured().bytes.try_into()?),
    })
}

fn add(total: &mut usize, value: usize) -> Result<()> {
    *total = total
        .checked_add(value)
        .context("supplement result bound overflow")?;
    Ok(())
}
fn ascii_string(bytes: usize) -> Result<usize> {
    bytes
        .checked_add(2)
        .context("supplement ASCII string bound overflow")
}
fn untrusted_string(bytes: usize) -> Result<usize> {
    bytes
        .checked_mul(6)
        .and_then(|n| n.checked_add(2))
        .context("supplement JSON string bound overflow")
}

/// Exact maximum `serde_json` encoding for the accepted Prepared supplement
/// shape. `source_id` is the sole untrusted string (<=4096 bytes); a control
/// byte needs six JSON bytes. Every identity is lowercase BLAKE3 (64 ASCII),
/// origin is the literal `embedded`, historical status is `Malformed`, and the
/// numeric widths are the full u64/u128 decimal widths.
pub(crate) fn prepared_supplements_bound(count: usize) -> Result<usize> {
    let mut item = 0usize;
    for bytes in [
        br#"{"pin":{"revision":"#.len(),
        ascii_string(DIGEST_BYTES)?,
        b",\"source_id\":".len(),
        untrusted_string(SUPPLEMENT_SOURCE_ID_BYTES)?,
        b",\"origin\":\"embedded\",\"source_revision\":{\"length\":".len(),
        20,
        b",\"blake3\":".len(),
        ascii_string(DIGEST_BYTES)?,
        b",\"modified_unix_ns\":".len(),
        39,
        b"},\"historical_status\":\"Malformed\",\"proof_blake3\":".len(),
        ascii_string(DIGEST_BYTES)?,
        b"},\"evidence\":".len(),
        ascii_string(DIGEST_BYTES)?,
        br#"}"#.len(),
    ] {
        add(&mut item, bytes)?;
    }
    if count == 0 {
        return Ok(2); // []
    }
    count
        .checked_mul(
            item.checked_add(1)
                .context("supplement item bound overflow")?,
        )
        .and_then(|n| n.checked_add(1)) // '[' + n*(item+',') with final ',' replaced by ']'
        .context("supplement result roster bound overflow")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog_migration::supplements::Prepared,
        lightroom::migration_source::SupplementPin,
        xmp_packets::{SourceRevision, Status},
    };
    use std::sync::Mutex;

    struct Frames(Mutex<Vec<ChildFrame>>);
    impl Publish for Frames {
        fn publish(&self, frame: &ChildFrame) -> Result<()> {
            let mut encoded = Vec::new();
            super::super::write_frame(&mut encoded, frame)?;
            self.0
                .lock()
                .unwrap()
                .push(super::super::read_frame(&mut encoded.as_slice())?);
            Ok(())
        }
    }

    fn guard() -> Guard {
        Guard {
            session: "session".into(),
            generation: "generation".into(),
            operation: "operation".into(),
        }
    }

    #[test]
    fn lm_transport_batch1_maximum_prepared_supplement_bound_is_exact_and_above_legacy_cap()
    -> Result<()> {
        let value = Prepared {
            pin: SupplementPin {
                revision: "a".repeat(DIGEST_BYTES),
                source_id: "\0".repeat(SUPPLEMENT_SOURCE_ID_BYTES),
                origin: "embedded".into(),
                source_revision: SourceRevision {
                    length: u64::MAX,
                    blake3: "b".repeat(DIGEST_BYTES),
                    modified_unix_ns: Some(u128::MAX),
                },
                historical_status: Status::Malformed,
                proof_blake3: "c".repeat(DIGEST_BYTES),
            },
            evidence: "d".repeat(DIGEST_BYTES),
        };
        let prepared = vec![value; 1024];
        let maximum = prepared_supplements_bound(prepared.len())?;
        let measured = measure(&prepared, maximum)?;
        assert_eq!(measured.bytes, maximum);
        assert!(measured.bytes > 8 * 1024 * 1024);
        assert!(measure(&prepared, maximum - 1).is_err());
        assert_eq!(
            measure(&Vec::<Prepared>::new(), prepared_supplements_bound(0)?)?.bytes,
            2
        );
        Ok(())
    }

    #[test]
    fn lm_transport_batch1_granted_result_publishes_utf8_chunks_with_exact_offsets_and_identity()
    -> Result<()> {
        let value = serde_json::json!({
            "mixed": format!("qualified-é-🦀-{}", "x".repeat(2 * CHUNK + 7)),
            "nested": [null, true, {"raw": "\\u0061\\/b"}],
        });
        let expected = serde_json::to_string(&value)?;
        let measured = measure(&value, expected.len())?;
        assert_eq!(measured.bytes, expected.len());
        assert_eq!(
            measured.blake3,
            blake3::hash(expected.as_bytes()).to_hex().as_str()
        );
        let output = Frames(Mutex::new(Vec::new()));
        publish(&value, Grant::new(measured.clone()), &guard(), &output)?;
        let mut frames = output.0.into_inner().unwrap();
        let finished = frames.pop().context("terminal result frame required")?;
        assert!(matches!(
            finished,
            ChildFrame::Finished {
                guard: frame_guard,
                bytes: crate::application::U64(bytes),
                result_blake3,
            } if frame_guard == guard()
                && bytes == measured.bytes as u64
                && result_blake3 == measured.blake3
        ));
        assert!(frames.len() >= 3);
        let mut reconstructed = String::new();
        for frame in frames {
            let ChildFrame::Result {
                guard: frame_guard,
                offset,
                text,
            } = frame
            else {
                anyhow::bail!("result chunk required");
            };
            assert_eq!(frame_guard, guard());
            assert_eq!(offset.0, reconstructed.len() as u64);
            assert!(!text.is_empty() && text.len() <= CHUNK);
            reconstructed.push_str(&text);
        }
        assert_eq!(reconstructed, expected);
        Ok(())
    }
}
