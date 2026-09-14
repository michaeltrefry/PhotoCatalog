//! Managed C→G native custody. A native identity is never a caller-supplied PID.
use super::{LeaseId, RootCapability};
use crate::{
    application::U64,
    preview::{Codec, RenderWork},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicBool;

pub const REQUEST_BYTES: usize = 192 * 1024;
pub const ERROR_BYTES: usize = 4096;
pub const DEFAULT_HEADER_SCRATCH: u64 = 1024 * 1024;
pub const DEFAULT_CODEC_SCRATCH: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", content = "work", deny_unknown_fields)]
pub enum Work {
    Render(Box<RenderWork>),
    DecodeEncoded {
        codec: Codec,
        encoded_bytes: U64,
        encoded_digest: String,
        expected_dimensions: Option<(u32, u32)>,
    },
}
impl Work {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Render(r) => {
                r.validate()?;
            }
            Self::DecodeEncoded {
                encoded_bytes,
                encoded_digest,
                expected_dimensions,
                ..
            } => {
                ensure!(
                    (1..=256 * 1024 * 1024).contains(&encoded_bytes.0),
                    "native input byte limit"
                );
                digest(encoded_digest)?;
                if let Some((w, h)) = expected_dimensions {
                    rgb_bytes(*w, *h)?;
                }
            }
        }
        // The outer codec performs a counting pass before allocating its packet.
        let mut count = Count(0);
        serde_json::to_writer(&mut count, self)?;
        ensure!(count.0 < REQUEST_BYTES, "native initial request limit");
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub operation: U64,
    pub stage: LeaseId,
    pub work: Work,
}
impl Envelope {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        self.work.validate()?;
        ensure!(self.operation.0 > 0, "native envelope identity");
        let mut count = Count(0);
        serde_json::to_writer(&mut count, self)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(count.0)?;
        serde_json::to_writer(&mut bytes, self)?;
        ensure!(
            bytes.len() == count.0,
            "native envelope serialization changed"
        );
        Ok(bytes)
    }
}
struct Count(usize);
impl std::io::Write for Count {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(b.len())
            .ok_or_else(|| std::io::Error::other("native request overflow"))?;
        if self.0 >= REQUEST_BYTES {
            return Err(std::io::Error::other("native request limit"));
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn digest(s: &str) -> Result<()> {
    ensure!(
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "native digest"
    );
    Ok(())
}
pub fn rgb_bytes(width: u32, height: u32) -> Result<u64> {
    ensure!(
        (1..=8192).contains(&width) && (1..=8192).contains(&height),
        "native dimensions"
    );
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|p| p.checked_mul(3))
        .context("RGB byte overflow")
}
/// Known output/intermediate planes only. Codec allocator scratch is a separate
/// admission policy; it is not an asserted hard process RSS limit.
pub fn plane_bytes(codec: Codec, width: u32, height: u32) -> Result<u64> {
    let rgb = rgb_bytes(width, height)?;
    let pixels = rgb / 3;
    // AVIF NextImage precedes final layout rejection: include 16-bit YUV444+
    // alpha, not only accepted 8-bit YUV420. RGB native/copy-out overlap is 2R.
    pixels
        .checked_mul(8)
        .and_then(|v| v.checked_add(if codec == Codec::Avif { 2 * rgb } else { rgb }))
        .context("native plane overflow")
}
pub fn header_cost(encoded: u64, scratch: u64) -> Result<u64> {
    ensure!(scratch > 0, "zero header scratch admission");
    encoded
        .checked_add(scratch)
        .context("native header cost overflow")
}
pub fn decode_cost(
    codec: Codec,
    width: u32,
    height: u32,
    encoded: u64,
    header_scratch: u64,
    codec_scratch: u64,
) -> Result<u64> {
    ensure!(codec_scratch > 0, "zero codec scratch admission");
    let decode = encoded
        .checked_add(plane_bytes(codec, width, height)?)
        .and_then(|v| v.checked_add(codec_scratch))
        .context("native decode cost overflow")?;
    Ok(header_cost(encoded, header_scratch)?.max(decode))
}
pub fn render_cost(request: &RenderWork, baseline: u64, codec_scratch: u64) -> Result<u64> {
    ensure!(baseline > 0 && codec_scratch > 0, "zero render admission");
    let mut validation = 0;
    let mut transfer = 0u64;
    for key in &request.keys {
        validation = validation.max(
            plane_bytes(key.encoding.codec, key.edge, key.edge)?
                .checked_add(codec_scratch)
                .context("validation cost overflow")?,
        );
        transfer = transfer
            .checked_add(rgb_bytes(key.edge, key.edge)?)
            .context("RGB transfer cost overflow")?;
    }
    Ok(baseline
        .checked_add(validation)
        .context("native render cost overflow")?
        .max(transfer))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub operation: U64,
    pub stage: LeaseId,
    pub input_digest: String,
    pub input_bytes: U64,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
}
impl Header {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "zero native operation");
        digest(&self.input_digest)?;
        ensure!(
            (1..=256 * 1024 * 1024).contains(&self.input_bytes.0),
            "native header input limit"
        );
        rgb_bytes(self.width, self.height)?;
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", content = "arguments", deny_unknown_fields)]
pub enum Action {
    Spawn {
        stage: LeaseId,
        work: Work,
        workers: u8,
        working_bytes: U64,
    },
    Start,
    Encode {
        header: Option<Header>,
        working_bytes: U64,
        rgb_bytes: U64,
    },
    Stop,
    Drain,
    Retire,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: RootCapability,
    pub operation: U64,
    pub action: Action,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "zero native operation");
        match &self.action {
            Action::Spawn {
                work,
                workers,
                working_bytes,
                ..
            } => {
                ensure!(
                    (1..=16).contains(workers) && working_bytes.0 > 0,
                    "native admission"
                );
                work.validate()?;
            }
            Action::Encode {
                header,
                working_bytes,
                rgb_bytes,
            } => {
                ensure!(
                    working_bytes.0 > 0 && rgb_bytes.0 > 0,
                    "native encode grant"
                );
                if let Some(h) = header {
                    h.validate()?;
                    ensure!(h.operation == self.operation, "native header operation");
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub fn cleanup(&self) -> bool {
        matches!(self.action, Action::Stop | Action::Drain | Action::Retire)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    Spawned,
    Sending,
    Running,
    StopRequested,
    WaitFailed,
    ExitObserved,
    PipeJoinFailed,
    Drained,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub epoch: LeaseId,
    pub session: LeaseId,
    pub operation: U64,
    pub stage: LeaseId,
    pub pid: Option<u32>,
    pub phase: Phase,
    pub initial_sent: bool,
    pub encode_sent: bool,
    pub exit_code: Option<i32>,
    pub success: Option<bool>,
    pub error: Option<String>,
}
impl Status {
    pub fn validate(&self, root: &RootCapability, operation: U64) -> Result<()> {
        ensure!(
            self.epoch == root.epoch && self.session == root.session && self.operation == operation,
            "native status identity"
        );
        ensure!(
            self.error.as_ref().is_none_or(|e| e.len() <= ERROR_BYTES),
            "native status error limit"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub epoch: LeaseId,
    pub session: LeaseId,
    pub root: LeaseId,
    pub operation: U64,
}
impl Key {
    pub fn new(root: &RootCapability, operation: U64) -> Self {
        Self {
            epoch: root.epoch.clone(),
            session: root.session.clone(),
            root: root.token.clone(),
            operation,
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "zero native query identity");
        Ok(())
    }
    pub fn matches(&self, root: &RootCapability) -> bool {
        self.epoch == root.epoch && self.session == root.session && self.root == root.token
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryAction {
    Status,
    RetryDrain,
    Retire,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub key: Key,
    pub action: QueryAction,
}
pub trait CatalogNative: Send + Sync {
    /// Reserved Stop admission. Production transport enqueues without a reply wait.
    fn signal_stop(&self, root: &RootCapability, operation: U64) -> Result<()> {
        self.call(
            &Request {
                root: root.clone(),
                operation,
                action: Action::Stop,
            },
            &AtomicBool::new(false),
        )
        .map(|_| ())
    }
    fn call(&self, request: &Request, cancel: &AtomicBool) -> Result<Status>;
    /// Reserved metadata-only query; no stdin, F IO or live-child blocking wait.
    fn status(&self, root: &RootCapability, operation: U64) -> Result<Status>;
}

#[cfg(test)]
mod tests;
