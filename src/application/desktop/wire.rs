//! Private desktop transport. Frames are bounded before allocation; no paths are reopened.
use super::*;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub(super) const CHUNK: usize = 16 * 1024;
pub(super) const CONFIG_BYTES: usize = 4 * 1024 * 1024;
// Existing Bridge errors remain deliverable even when the configured success
// reply budget is smaller than a serialized ResourceLimit error.
pub(super) const ERROR_BYTES: usize = 1024;
const HEADER: usize = 48;
const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Kind {
    Hello = 1,
    Ready = 2,
    Command = 3,
    Bytes = 4,
    Cancel = 5,
    Ack = 6,
    Shutdown = 7,
    Reply = 8,
    BytesError = 9,
    BinaryStart = 10,
    BinaryChunk = 11,
    BinaryEnd = 12,
    DrainError = 13,
}
impl Kind {
    fn decode(v: u8) -> std::io::Result<Self> {
        match v {
            1 => Ok(Self::Hello),
            2 => Ok(Self::Ready),
            3 => Ok(Self::Command),
            4 => Ok(Self::Bytes),
            5 => Ok(Self::Cancel),
            6 => Ok(Self::Ack),
            7 => Ok(Self::Shutdown),
            8 => Ok(Self::Reply),
            9 => Ok(Self::BytesError),
            10 => Ok(Self::BinaryStart),
            11 => Ok(Self::BinaryChunk),
            12 => Ok(Self::BinaryEnd),
            13 => Ok(Self::DrainError),
            _ => Err(invalid("unknown desktop frame kind")),
        }
    }
}
pub(super) fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}
pub(super) struct Frame {
    pub kind: Kind,
    pub session: [u8; 16],
    pub id: u64,
    pub offset: usize,
    pub total: usize,
    pub payload: Vec<u8>,
}
impl Frame {
    pub fn read(r: &mut impl Read) -> std::io::Result<Option<Self>> {
        let mut h = [0; HEADER];
        loop {
            match r.read(&mut h[..1]) {
                Ok(0) => return Ok(None),
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        r.read_exact(&mut h[1..])?;
        if &h[..4] != b"PCDT" || h[4] != VERSION || h[6..8] != [0, 0] {
            return Err(invalid("desktop protocol version/header mismatch"));
        }
        let kind = Kind::decode(h[5])?;
        let offset = usize::try_from(u64::from_le_bytes(h[32..40].try_into().unwrap()))
            .map_err(|_| invalid("frame offset overflow"))?;
        let total = u32::from_le_bytes(h[40..44].try_into().unwrap()) as usize;
        let size = u32::from_le_bytes(h[44..48].try_into().unwrap()) as usize;
        if size > CHUNK || offset.checked_add(size).is_none_or(|n| n > total) {
            return Err(invalid("desktop frame byte bounds"));
        }
        let mut payload = vec![0; size];
        r.read_exact(&mut payload)?;
        Ok(Some(Self {
            kind,
            session: h[8..24].try_into().unwrap(),
            id: u64::from_le_bytes(h[24..32].try_into().unwrap()),
            offset,
            total,
            payload,
        }))
    }
    pub fn write(&self, w: &mut impl Write) -> std::io::Result<()> {
        if self.payload.len() > CHUNK
            || self
                .offset
                .checked_add(self.payload.len())
                .is_none_or(|n| n > self.total)
        {
            return Err(invalid("outgoing frame bounds"));
        }
        let mut h = [0; HEADER];
        h[..4].copy_from_slice(b"PCDT");
        h[4] = VERSION;
        h[5] = self.kind as u8;
        h[8..24].copy_from_slice(&self.session);
        h[24..32].copy_from_slice(&self.id.to_le_bytes());
        h[32..40].copy_from_slice(&(self.offset as u64).to_le_bytes());
        h[40..44].copy_from_slice(
            &u32::try_from(self.total)
                .map_err(|_| invalid("total overflow"))?
                .to_le_bytes(),
        );
        h[44..48].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        w.write_all(&h)?;
        w.write_all(&self.payload)?;
        w.flush()
    }
}
pub(super) struct Message {
    pub kind: Kind,
    pub id: u64,
    pub bytes: Vec<u8>,
    pub offset: usize,
}
impl Message {
    pub fn new(kind: Kind, id: u64, bytes: Vec<u8>) -> Self {
        Self {
            kind,
            id,
            bytes,
            offset: 0,
        }
    }
    pub fn next(&mut self, session: [u8; 16]) -> Frame {
        let end = self.bytes.len().min(self.offset + CHUNK);
        let frame = Frame {
            kind: self.kind,
            session,
            id: self.id,
            offset: self.offset,
            total: self.bytes.len(),
            payload: self.bytes[self.offset..end].to_vec(),
        };
        self.offset = end;
        frame
    }
    pub fn finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
    pub fn write(mut self, session: [u8; 16], w: &mut impl Write) -> std::io::Result<()> {
        loop {
            self.next(session).write(w)?;
            if self.finished() {
                return Ok(());
            }
        }
    }
}
pub(super) struct Assembly {
    kind: Kind,
    id: u64,
    total: usize,
    bytes: Vec<u8>,
}
impl Assembly {
    pub fn start(f: &Frame, cap: usize) -> std::io::Result<Self> {
        if f.offset != 0 || f.total > cap {
            return Err(invalid("message admission bounds"));
        }
        Ok(Self {
            kind: f.kind,
            id: f.id,
            total: f.total,
            bytes: Vec::with_capacity(f.total),
        })
    }
    pub fn push(&mut self, f: Frame) -> std::io::Result<bool> {
        if f.kind != self.kind
            || f.id != self.id
            || f.total != self.total
            || f.offset != self.bytes.len()
            || (f.payload.is_empty() && f.total != 0)
        {
            return Err(invalid("message continuity"));
        }
        self.bytes.extend(f.payload);
        Ok(self.bytes.len() == self.total)
    }
    pub fn finish(self) -> (Kind, u64, Vec<u8>) {
        (self.kind, self.id, self.bytes)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConfigWire {
    build: String,
    executable: NativePath,
    cache: Option<NativePath>,
    originals: Vec<NativePath>,
    policy: crate::preview::PreviewPolicy,
    preview: crate::preview::ServiceLimits,
    limits: Limits,
}
pub(super) fn build_identity() -> String {
    blake3::hash(
        concat!(
            env!("CARGO_PKG_VERSION"),
            include_str!("wire.rs"),
            include_str!("process.rs"),
            include_str!("../desktop.rs"),
            include_str!("../../application.rs"),
            include_str!("../dto.rs"),
            include_str!("../backup.rs"),
            include_str!("../copy.rs"),
            include_str!("../relink.rs"),
            include_str!("../metadata.rs"),
            include_str!("../organization.rs"),
            include_str!("../exports.rs"),
            include_str!("../lightroom_bridge.rs"),
            include_str!("../../../Cargo.lock")
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}
impl ConfigWire {
    pub fn from_config(c: &Config) -> Self {
        Self {
            build: build_identity(),
            executable: NativePath::from_path(&c.worker_executable),
            cache: c.cache_root.as_deref().map(NativePath::from_path),
            originals: c
                .original_roots
                .iter()
                .map(|p| NativePath::from_path(p))
                .collect(),
            policy: c.preview_policy.clone(),
            preview: c.preview_limits.clone(),
            limits: c.limits.clone(),
        }
    }
    pub fn into_config(self) -> anyhow::Result<Config> {
        anyhow::ensure!(
            self.build == build_identity(),
            "desktop executable protocol build mismatch"
        );
        let config = Config {
            worker_executable: self.executable.to_path()?,
            cache_root: self.cache.map(|p| p.to_path()).transpose()?,
            original_roots: self
                .originals
                .into_iter()
                .map(|p| p.to_path())
                .collect::<std::result::Result<_, _>>()?,
            preview_policy: self.policy,
            preview_limits: self.preview,
            limits: self.limits,
            #[cfg(test)]
            import_checkpoint: None,
        };
        config.validate()?;
        Ok(config)
    }
}
#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(super) struct BytesRequest {
    pub catalog: String,
    pub ticket: String,
    pub foreground: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BinaryHeader {
    pub catalog: String,
    pub ticket: String,
    pub mime: String,
    pub bytes: usize,
    pub digest: String,
}

/// Bounded interval ledger: replay detection without one entry per lifetime request.
#[derive(Default)]
pub(super) struct Seen {
    ranges: Vec<(u64, u64)>,
}
impl Seen {
    pub fn contains(&self, id: u64) -> bool {
        self.ranges.iter().any(|(a, b)| *a <= id && id <= *b)
    }
    pub fn insert(&mut self, id: u64, cap: usize) -> std::io::Result<()> {
        if id == 0 || self.ranges.iter().any(|(a, b)| *a <= id && id <= *b) {
            return Err(invalid("replayed request identifier"));
        }
        self.ranges.push((id, id));
        self.ranges.sort_unstable();
        let mut i = 0;
        while i + 1 < self.ranges.len() {
            if self.ranges[i].1.checked_add(1) == Some(self.ranges[i + 1].0) {
                self.ranges[i].1 = self.ranges.remove(i + 1).1;
            } else {
                i += 1;
            }
        }
        if self.ranges.len() > cap {
            return Err(invalid("request gap budget exceeded"));
        }
        Ok(())
    }
}
