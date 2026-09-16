//! Managed Source relay vocabulary. Complete existing Source frames travel in
//! bounded opaque chunks; authority/result bytes are never normalized here.
use crate::application::U64;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub(crate) mod broker;
pub(crate) mod client;
pub(crate) mod server;

pub(crate) const CHUNK: usize = 16 * 1024;
pub(crate) const COUNT: usize = 3;
// Preserve the complete Source frame, including its four-byte length prefix.
const FRAME: usize = crate::lightroom_migration_worker::protocol::FRAME_BYTES + 4;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Sql,
    Raw,
    CaptureSql,
}
impl Kind {
    pub(crate) const ALL: [Self; COUNT] = [Self::Sql, Self::Raw, Self::CaptureSql];
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Sql => 0,
            Self::Raw => 1,
            Self::CaptureSql => 2,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "relay", deny_unknown_fields)]
pub enum Command {
    Start {
        sequence: U64,
        kind: Kind,
        reader: String,
    },
    Reserve {
        token: String,
        sequence: U64,
        bytes: U64,
    },
    Quiesced {
        token: String,
    },
    Input {
        token: String,
        frame: U64,
        offset: U64,
        total: U64,
        bytes: Vec<u8>,
    },
    Consumed {
        token: String,
        frame: U64,
    },
    Drain {
        token: String,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "relay", deny_unknown_fields)]
pub enum Event {
    Started {
        sequence: U64,
        token: String,
    },
    Reserved {
        token: String,
        sequence: U64,
        bytes: U64,
    },
    Quiesced {
        token: String,
    },
    Accepted {
        token: String,
        frame: U64,
        offset: U64,
    },
    Output {
        token: String,
        frame: U64,
        offset: U64,
        total: U64,
        bytes: Vec<u8>,
    },
    Drained {
        token: String,
    },
    Failed {
        token: String,
        detail: String,
    },
}
/// An admitted but incomplete frame has one fixed-capacity owner. The caller
/// retains it before acknowledging the first chunk and never replaces it after
/// malformed continuity. Error poisons the associated Source/operation.
pub(crate) struct Assembly {
    frame: u64,
    total: usize,
    bytes: Vec<u8>,
}
impl Assembly {
    pub(crate) fn start(frame: u64, total: u64, offset: u64, bytes: &[u8]) -> Result<Self> {
        let total = usize::try_from(total)?;
        ensure!(
            frame != 0 && offset == 0 && (1..=FRAME).contains(&total),
            "Source relay frame admission"
        );
        // Validate the first chunk before allocating the declared whole frame.
        ensure!(
            !bytes.is_empty() && bytes.len() <= CHUNK && bytes.len() <= total,
            "Source relay chunk admission"
        );
        let mut value = Self {
            frame,
            total,
            bytes: Vec::with_capacity(total),
        };
        value.push(frame, 0, total as u64, bytes)?;
        Ok(value)
    }
    pub(crate) fn push(&mut self, frame: u64, offset: u64, total: u64, bytes: &[u8]) -> Result<()> {
        ensure!(
            frame == self.frame && total == self.total as u64 && offset == self.bytes.len() as u64,
            "Source relay frame continuity"
        );
        ensure!(
            !bytes.is_empty()
                && bytes.len() <= CHUNK
                && bytes.len() <= self.total - self.bytes.len(),
            "Source relay chunk bounds"
        );
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    pub(crate) fn offset(&self) -> usize {
        self.bytes.len()
    }
    pub(crate) fn complete(&self) -> bool {
        self.bytes.len() == self.total
    }
    pub(crate) fn finish(self) -> Result<Vec<u8>> {
        ensure!(self.complete(), "Source relay frame incomplete");
        Ok(self.bytes)
    }
}
/// Holds the complete encoded frame until its last opaque chunk is delivered.
/// `next` does not advance; only successful queue admission calls `accepted`.
pub(crate) struct Outgoing {
    pub(crate) frame: u64,
    bytes: Vec<u8>,
    offset: usize,
}
impl Outgoing {
    pub(crate) fn new(frame: u64, bytes: Vec<u8>) -> Result<Self> {
        ensure!(
            frame != 0 && (1..=FRAME).contains(&bytes.len()),
            "Source relay output admission"
        );
        Ok(Self {
            frame,
            bytes,
            offset: 0,
        })
    }
    pub(crate) fn total(&self) -> usize {
        self.bytes.len()
    }
    pub(crate) fn offset(&self) -> usize {
        self.offset
    }
    pub(crate) fn chunk(&self) -> &[u8] {
        &self.bytes[self.offset..self.offset.saturating_add(CHUNK).min(self.bytes.len())]
    }
    pub(crate) fn accepted(&mut self, offset: u64) -> Result<bool> {
        ensure!(
            offset == self.offset as u64 && self.offset < self.bytes.len(),
            "Source relay output acknowledgement differs"
        );
        self.offset = self
            .offset
            .checked_add(self.chunk().len())
            .context("Source relay offset overflow")?;
        Ok(self.offset == self.bytes.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relay_preserves_complete_source_envelope_and_rejects_replayed_chunks() -> Result<()> {
        let original: Vec<_> = (0..FRAME).map(|n| (n % 256) as u8).collect();
        let mut outgoing = Outgoing::new(1, original.clone())?;
        let mut incoming = None;
        loop {
            let offset = outgoing.offset() as u64;
            let bytes = outgoing.chunk();
            let envelope = Command::Input {
                token: "a".repeat(64),
                frame: U64(1),
                offset: U64(offset),
                total: U64(FRAME as u64),
                bytes: bytes.to_vec(),
            };
            assert!(
                crate::lightroom::bounded_json(
                    &envelope,
                    crate::lightroom_migration_worker::protocol::FRAME_BYTES
                )?
                .len()
                    <= crate::lightroom_migration_worker::protocol::FRAME_BYTES
            );
            if let Some(value) = &mut incoming {
                let value: &mut Assembly = value;
                value.push(1, offset, FRAME as u64, bytes)?;
                let before = value.offset();
                assert!(value.push(1, offset, FRAME as u64, bytes).is_err());
                assert_eq!(value.offset(), before);
            } else {
                incoming = Some(Assembly::start(1, FRAME as u64, offset, bytes)?);
            }
            if outgoing.accepted(offset)? {
                break;
            }
        }
        assert_eq!(incoming.unwrap().finish()?, original);
        assert!(Assembly::start(1, FRAME as u64 + 1, 0, &[0]).is_err());
        assert!(Assembly::start(0, 1, 0, &[0]).is_err());
        assert!(Assembly::start(1, 1, 1, &[0]).is_err());
        assert!(Assembly::start(1, 1, 0, &[]).is_err());
        Ok(())
    }
}
