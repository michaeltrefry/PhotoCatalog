//! Bounded byte-only helper protocol. No control message carries an OS handle.
use super::identity::{Audit, FileKey};
pub use super::lease::DestinationPin;
use crate::{
    application::U64,
    catalog_writer::{ExternalAdmission, ExternalLease},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
pub const FRAME_BYTES: usize = 128 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    pub session: String,
    pub generation: String,
    pub operation: String,
}
impl Guard {
    pub fn validate(&self) -> Result<()> {
        for id in [&self.session, &self.generation, &self.operation] {
            ensure!(
                (1..=64).contains(&id.len())
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "helper operation identity bounds"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteKind {
    Bootstrap,
    Catalog,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ParentFrame {
    Begin {
        guard: Guard,
        request_blake3: String,
        bytes: U64,
    },
    Input {
        guard: Guard,
        offset: U64,
        text: String,
    },
    FinishInput {
        guard: Guard,
        blake3: String,
    },
    Grant {
        guard: Guard,
        sequence: U64,
        write: WriteKind,
    },
    Cancel {
        guard: Guard,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ChildFrame {
    Admitted {
        guard: Guard,
        request_blake3: String,
    },
    LockAcquired {
        guard: Guard,
        lock: FileKey,
        destination: DestinationPin,
        target_token: String,
    },
    NeedWrite {
        guard: Guard,
        sequence: U64,
        write: WriteKind,
        target_token: String,
        lock: Option<FileKey>,
    },
    ReleaseWrite {
        guard: Guard,
        sequence: U64,
        write: WriteKind,
    },
    Progress {
        guard: Guard,
        phase: String,
        completed: U64,
        total: Option<U64>,
    },
    Result {
        guard: Guard,
        offset: U64,
        text: String,
    },
    Finished {
        guard: Guard,
        result_blake3: String,
        bytes: U64,
    },
    Failed {
        guard: Guard,
        detail: String,
        poisoned: bool,
    },
}
pub fn write_frame(output: &mut impl Write, frame: &impl Serialize) -> Result<()> {
    let bytes = crate::lightroom::bounded_json(frame, FRAME_BYTES)?;
    output.write_all(&u32::try_from(bytes.len())?.to_be_bytes())?;
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(())
}
pub fn read_frame<T: DeserializeOwned>(input: &mut impl Read) -> Result<T> {
    read_frame_optional(input)?.context("helper frame stream ended")
}
pub(crate) fn read_frame_optional<T: DeserializeOwned>(input: &mut impl Read) -> Result<Option<T>> {
    let mut prefix = [0u8; 4];
    loop {
        match input.read(&mut prefix[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    input.read_exact(&mut prefix[1..])?;
    let size = u32::from_be_bytes(prefix) as usize;
    ensure!(
        (1..=FRAME_BYTES).contains(&size),
        "helper frame byte admission"
    );
    let mut bytes = vec![0; size];
    input.read_exact(&mut bytes)?;
    Ok(Some(
        serde_json::from_slice(&bytes).context("malformed helper frame")?,
    ))
}
pub(crate) trait Publish: Send + Sync {
    fn publish(&self, frame: &ChildFrame) -> Result<()>;
}
impl<T: Write + Send> Publish for Mutex<T> {
    fn publish(&self, frame: &ChildFrame) -> Result<()> {
        write_frame(
            &mut *self
                .lock()
                .map_err(|_| anyhow::anyhow!("helper output poisoned"))?,
            frame,
        )
    }
}
struct Waiting {
    sequence: u64,
    write: WriteKind,
    granted: bool,
}
pub(crate) struct Controls {
    guard: Guard,
    audit: Audit,
    waiting: Mutex<Option<Waiting>>,
    changed: Condvar,
    sequence: AtomicU64,
    until: Instant,
}
impl Controls {
    pub(crate) fn new(guard: Guard, audit: Audit, until: Instant) -> Result<Arc<Self>> {
        guard.validate()?;
        Ok(Arc::new(Self {
            guard,
            audit,
            waiting: Mutex::new(None),
            changed: Condvar::new(),
            sequence: AtomicU64::new(0),
            until,
        }))
    }
    /// The only control-listener operations. Neither can touch SQL or a lock.
    pub(crate) fn accept(&self, frame: ParentFrame) -> Result<()> {
        let checked = (|| match frame {
            ParentFrame::Cancel { guard } => {
                ensure!(guard == self.guard, "stale helper cancellation");
                self.cancel();
                Ok(())
            }
            ParentFrame::Grant {
                guard,
                sequence,
                write,
            } => {
                ensure!(guard == self.guard, "stale helper writer grant");
                self.audit.check()?;
                let mut slot = self
                    .waiting
                    .lock()
                    .map_err(|_| anyhow::anyhow!("helper grant state poisoned"))?;
                let pending = slot.as_mut().context("unsolicited helper writer grant")?;
                ensure!(
                    pending.sequence == sequence.0 && pending.write == write && !pending.granted,
                    "stale or repeated helper writer grant"
                );
                pending.granted = true;
                self.changed.notify_all();
                Ok(())
            }
            _ => anyhow::bail!("input/admission frame after execution began"),
        })();
        if checked.is_err() {
            self.audit.poison();
            self.changed.notify_all();
        }
        checked
    }
    pub(crate) fn cancel(&self) {
        self.audit.cancel();
        self.changed.notify_all();
    }
    pub(crate) fn poison(&self) {
        self.audit.poison();
        self.changed.notify_all();
    }
    fn check(&self) -> Result<()> {
        self.audit.check()?;
        ensure!(Instant::now() < self.until, "helper execution deadline");
        Ok(())
    }
}
/// Created only after source/target admission. Catalog grants additionally bind
/// the same helper's held lock; bootstrap grants carry no lock authority.
pub(crate) struct Grants {
    pub controls: Arc<Controls>,
    pub output: Arc<dyn Publish>,
    pub write: WriteKind,
    pub target_token: String,
    pub lock: Option<FileKey>,
    pub verify: Arc<dyn Fn() -> Result<()> + Send + Sync>,
}
struct Lease {
    controls: Arc<Controls>,
    output: Arc<dyn Publish>,
    sequence: u64,
    write: WriteKind,
}
impl ExternalAdmission for Grants {
    fn acquire(&self) -> Result<Box<dyn ExternalLease>> {
        self.controls.check()?;
        (self.verify)()?;
        ensure!(
            matches!(self.write, WriteKind::Catalog) == self.lock.is_some(),
            "helper grant/lock class mismatch"
        );
        let sequence = self
            .controls
            .sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| anyhow::anyhow!("helper writer sequence exhausted"))?
            + 1;
        {
            let mut slot = self
                .controls
                .waiting
                .lock()
                .map_err(|_| anyhow::anyhow!("helper grant state poisoned"))?;
            ensure!(slot.is_none(), "nested helper grant request");
            *slot = Some(Waiting {
                sequence,
                write: self.write,
                granted: false,
            });
        }
        let result = (|| {
            self.output.publish(&ChildFrame::NeedWrite {
                guard: self.controls.guard.clone(),
                sequence: U64(sequence),
                write: self.write,
                target_token: self.target_token.clone(),
                lock: self.lock.clone(),
            })?;
            let mut slot = self
                .controls
                .waiting
                .lock()
                .map_err(|_| anyhow::anyhow!("helper grant state poisoned"))?;
            loop {
                self.controls.check()?;
                if slot
                    .as_ref()
                    .is_some_and(|p| p.sequence == sequence && p.granted)
                {
                    break;
                }
                slot = self
                    .controls
                    .changed
                    .wait_timeout(slot, Duration::from_millis(50))
                    .map_err(|_| anyhow::anyhow!("helper grant state poisoned"))?
                    .0;
            }
            drop(slot);
            (self.verify)()?;
            self.controls.check()?;
            self.controls.audit.writing(true)?;
            Ok(Box::new(Lease {
                controls: self.controls.clone(),
                output: self.output.clone(),
                sequence,
                write: self.write,
            }) as Box<dyn ExternalLease>)
        })();
        // Failure after a sent request can leave a parent permit outstanding.
        // Stop this helper; only parent release-or-reap can resolve ownership.
        if result.is_err() {
            self.controls.poison();
        }
        result
    }
}
impl ExternalLease for Lease {
    fn release(&mut self) {
        let result = (|| {
            self.controls.audit.writing(false)?;
            self.output.publish(&ChildFrame::ReleaseWrite {
                guard: self.controls.guard.clone(),
                sequence: U64(self.sequence),
                write: self.write,
            })?;
            let mut slot = self
                .controls
                .waiting
                .lock()
                .map_err(|_| anyhow::anyhow!("helper grant state poisoned"))?;
            ensure!(
                slot.as_ref()
                    .is_some_and(|v| v.sequence == self.sequence && v.granted),
                "helper release state differs"
            );
            slot.take();
            Ok(())
        })();
        if result.is_err() {
            self.controls.poison();
        }
    }
}
#[cfg(test)]
mod tests;
