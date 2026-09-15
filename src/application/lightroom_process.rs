//! Unselected G-owned process boundary for the complete Workbench protocol.
//!
//! W owns the inspection SQLite roles and retained public request/result state.
//! This transport contains no production factory selection; the desktop owner
//! may opt into it only after the remaining F/S custody map is qualified.
use super::lightroom_bridge::{Coordinator, Request, Response};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::Mutex,
};

pub const ENVELOPE_BYTES: usize = 128 * 1024;
const HEADER_BYTES: usize = 48;
const PROTOCOL: u8 = 1;
const MAGIC: &[u8; 4] = b"PCWB";
const ERROR_BYTES: usize = 4096;

pub fn build_identity() -> String {
    blake3::hash(
        concat!(
            env!("CARGO_PKG_VERSION"),
            include_str!("lightroom.rs"),
            include_str!("lightroom/worker.rs"),
            include_str!("lightroom_bridge.rs"),
            include_str!("lightroom_bridge/wire.rs"),
            include_str!("lightroom_process.rs"),
            include_str!("../lightroom/plan.rs"),
            include_str!("../lightroom/plan/desktop.rs"),
            include_str!("../lightroom/plan/selection.rs"),
            include_str!("../lightroom/plan/selection/snapshot.rs"),
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
}
impl Startup {
    fn new() -> Self {
        Self {
            protocol: PROTOCOL,
            build: build_identity(),
            instance: crate::catalog_session::LeaseId::new(),
            envelope_bytes: crate::application::U64(ENVELOPE_BYTES as u64),
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub detail: String,
}
impl Failure {
    fn new(error: impl std::fmt::Display) -> Self {
        Self {
            detail: format!("{error:#}").chars().take(ERROR_BYTES).collect(),
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
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .filter(|bytes| *bytes <= ENVELOPE_BYTES)
                .ok_or_else(|| std::io::Error::other("Workbench envelope byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(count.0)?;
    serde_json::to_writer(&mut bytes, value)?;
    ensure!(bytes.len() == count.0, "Workbench encoding length changed");
    Ok(bytes)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ensure!(
        bytes.len() <= ENVELOPE_BYTES,
        "Workbench decoded envelope byte limit"
    );
    Ok(serde_json::from_slice(bytes)?)
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
        "Workbench frame header mismatch"
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

/// Hidden W entrypoint. stdout is the only response stream; stderr remains
/// unused so an unframed diagnostic can never be mistaken for protocol data.
pub fn worker_main() -> Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let startup: Startup = read_packet(&mut input)?.context("missing Workbench bootstrap")?;
    startup.validate()?;
    write_packet(
        &mut output,
        &Outcome::Ready {
            instance: startup.instance.clone(),
            build: build_identity(),
        },
    )?;
    let executable = std::env::current_exe()?;
    let control = std::sync::Arc::new(Mutex::new(super::lightroom_bridge::Control::default()));
    let mut coordinator = Coordinator::new(control);
    loop {
        let work: Work = read_packet(&mut input)?.context("Workbench control stream ended")?;
        match work {
            Work::Request {
                sequence,
                request_digest: expected,
                request,
            } => {
                ensure!(
                    sequence.0 > 0 && request_digest(&request)? == expected,
                    "Workbench request binding mismatch"
                );
                coordinator.maintain();
                let result = coordinator
                    .request(request, &executable, ENVELOPE_BYTES)
                    .map_err(Failure::new);
                if let Err(error) = &result {
                    error.validate()?;
                }
                write_packet(
                    &mut output,
                    &Outcome::Reply {
                        sequence,
                        request_digest: expected,
                        result,
                    },
                )?;
            }
            Work::Shutdown { sequence } => {
                ensure!(sequence.0 > 0, "Workbench shutdown sequence");
                coordinator.shutdown();
                write_packet(
                    &mut output,
                    &Outcome::Drained {
                        sequence,
                        instance: startup.instance,
                    },
                )?;
                return Ok(());
            }
        }
    }
}

struct Transport {
    input: ChildStdin,
    output: ChildStdout,
    sequence: u64,
}

struct Owner {
    child: Option<Child>,
    transport: Option<Transport>,
    drained: bool,
}
impl Owner {
    fn fail_closed(&mut self) {
        if let Some(transport) = self.transport.take() {
            std::mem::forget(transport);
        }
        if let Some(child) = self.child.take() {
            std::mem::forget(child);
        }
    }
}

/// G-owned W child. Calls are serialized and bounded, while W's own operation
/// thread keeps Status/Cancel/Close requests responsive during long actions.
pub struct Client {
    instance: crate::catalog_session::LeaseId,
    owner: Mutex<Owner>,
}
impl Client {
    pub fn spawn(executable: &Path) -> Result<Self> {
        let startup = Startup::new();
        startup.validate()?;
        let mut child = Command::new(executable)
            .arg("--lightroom-workbench-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut input = child.stdin.take().context("Workbench stdin missing")?;
        let mut output = child.stdout.take().context("Workbench stdout missing")?;
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
        Ok(Self {
            instance: startup.instance,
            owner: Mutex::new(Owner {
                child: Some(child),
                transport: Some(Transport {
                    input,
                    output,
                    sequence: 0,
                }),
                drained: false,
            }),
        })
    }

    pub fn call(&self, request: Request) -> Result<Response> {
        let digest = request_digest(&request)?;
        let mut owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(!owner.drained, "Workbench process is drained");
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
        let outcome: Outcome = read_packet(&mut transport.output)?
            .context("Workbench reply lost; child remains owned for checked drain")?;
        let Outcome::Reply {
            sequence: actual,
            request_digest: actual_digest,
            result,
        } = outcome
        else {
            anyhow::bail!("unexpected Workbench outcome")
        };
        ensure!(
            actual == sequence && actual_digest == digest,
            "Workbench reply binding mismatch"
        );
        result.map_err(|failure| anyhow::anyhow!(failure.detail))
    }

    pub fn shutdown(&self) -> Result<()> {
        let mut owner = self.owner.lock().unwrap_or_else(|error| error.into_inner());
        if owner.drained {
            return Ok(());
        }
        let result = (|| -> Result<()> {
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
            let outcome: Outcome = read_packet(&mut transport.output)?
                .context("Workbench drain acknowledgement lost")?;
            ensure!(
                matches!(
                    outcome,
                    Outcome::Drained { sequence: actual, ref instance }
                        if actual == sequence && instance == &self.instance
                ),
                "Workbench drain binding mismatch"
            );
            drop(owner.transport.take());
            let status = owner
                .child
                .as_mut()
                .context("Workbench child missing before reap")?
                .wait()?;
            ensure!(status.success(), "Workbench child exited {status}");
            owner.child.take();
            owner.drained = true;
            Ok(())
        })();
        if result.is_err() {
            owner.fail_closed();
        }
        result
    }

    pub fn pid(&self) -> Option<u32> {
        self.owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .child
            .as_ref()
            .map(Child::id)
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            self.owner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .fail_closed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_rejects_corruption_and_oversize() -> Result<()> {
        let startup = Startup::new();
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
}
