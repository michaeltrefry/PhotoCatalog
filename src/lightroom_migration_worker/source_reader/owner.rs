//! This process has one Opening -> Locked -> Retiring lifetime. Runtime and
//! control-thread setup precede the fixed source roster. No locked command
//! contains a path or invokes a filesystem/output utility.
use super::{transport::*, wire};
use crate::{
    catalog_migration::artifacts::{ArtifactRead, ArtifactReader},
    lightroom::migration_source::MigrationSource,
    lightroom_migration_worker::protocol::{read_frame_optional, write_frame},
};
use anyhow::{Context, Result, ensure};
use std::{
    io::{Read as IoRead, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

struct Incoming {
    pending: Option<Request>,
    error: Option<String>,
    ended: bool,
}
struct Controls {
    state: Mutex<Incoming>,
    changed: Condvar,
    cancel: Arc<AtomicBool>,
    epoch: Mutex<Option<Epoch>>,
}
impl Controls {
    fn receive(&self) -> Result<Request> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(error) = &state.error {
                anyhow::bail!("source control: {error}");
            }
            if let Some(request) = state.pending.take() {
                self.changed.notify_all();
                return Ok(request);
            }
            ensure!(!state.ended, "source control ended");
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
    fn fail(&self, detail: String) {
        self.cancel.store(true, Ordering::Release);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.error = Some(detail);
        self.changed.notify_all();
    }
}

/// Input and output are pre-established anonymous pipes. Production dispatch
/// calls this before creating any GUI, Catalog, logger or other source owner.
pub(super) fn serve(input: impl IoRead + Send + 'static, mut output: impl Write) -> Result<()> {
    let controls = Arc::new(Controls {
        state: Mutex::new(Incoming {
            pending: None,
            error: None,
            ended: false,
        }),
        changed: Condvar::new(),
        cancel: Arc::new(AtomicBool::new(false)),
        epoch: Mutex::new(None),
    });
    let listener = controls.clone();
    // The listener opens no paths, owns no SQL, and signals cancellation before
    // enqueueing any work. Normal Retire ends it; parent EOF is abrupt loss.
    let listener_thread =
        thread::Builder::new()
            .name("source-control".into())
            .spawn(move || {
                let mut input = input;
                let result = (|| -> Result<()> {
                    while let Some(request) = read_frame_optional::<Request>(&mut input)? {
                        request.epoch().validate()?;
                        let mut epoch = listener.epoch.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(expected) = epoch.as_ref() {
                            ensure!(expected == request.epoch(), "source epoch mismatch");
                        } else {
                            ensure!(
                                matches!(request, Request::Begin { .. }),
                                "source Begin required"
                            );
                            *epoch = Some(request.epoch().clone());
                        }
                        drop(epoch);
                        if matches!(request, Request::Cancel { .. }) {
                            listener.cancel.store(true, Ordering::Release);
                            continue;
                        }
                        let retiring = matches!(request, Request::Retire { .. });
                        let mut state = listener.state.lock().unwrap_or_else(|e| e.into_inner());
                        // Backpressure admits at most one frame. The sender serializes
                        // queries, so Cancel never waits behind a complete queued query.
                        while state.pending.is_some() && state.error.is_none() {
                            state = listener
                                .changed
                                .wait(state)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                        ensure!(state.error.is_none(), "source owner ended");
                        state.pending = Some(request);
                        listener.changed.notify_all();
                        if retiring {
                            return Ok(());
                        }
                    }
                    anyhow::bail!("source parent pipe ended")
                })();
                if let Err(error) = result {
                    listener.fail(format!("{error:#}"));
                }
                let mut state = listener.state.lock().unwrap_or_else(|e| e.into_inner());
                state.ended = true;
                listener.changed.notify_all();
            })?;
    let result = run(&controls, &mut output);
    if let Err(error) = &result {
        // Opening can fail before Ready (missing source, invalid authority,
        // identity or bounds). Preserve that reason as a bounded protocol reply;
        // never rely on unframed harness/stderr diagnostics or open a log file.
        let epoch = controls
            .epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(epoch) = epoch {
            let mut detail = format!("{error:#}");
            while detail.len() > 4096 {
                detail.pop();
            }
            let _ = write_frame(&mut output, &Reply::Failed { epoch, detail });
        }
    }
    // On successful Retire the listener has consumed its final frame. On
    // malformed/EOF failure this process must exit; never block on another read.
    if result.is_ok() || listener_thread.is_finished() {
        let _ = listener_thread.join();
    }
    result
}

enum Roster {
    Sql(MigrationSource),
    Artifact(ArtifactReader),
}
impl Roster {
    fn open(authority: Authority, cancel: Arc<AtomicBool>) -> Result<Self> {
        ensure!(!cancel.load(Ordering::Acquire), "source admission canceled");
        match authority {
            Authority::Sql {
                seal,
                limits,
                protected,
            } => Ok(Self::Sql(MigrationSource::open_closed_roster(
                seal,
                limits.try_into()?,
                cancel,
                &protected,
            )?)),
            Authority::Artifact {
                descriptor,
                limits,
                protected,
            } => Ok(Self::Artifact(ArtifactReader::open_descriptor(
                descriptor,
                limits.try_into()?,
                &|| cancel.load(Ordering::Acquire),
                &protected,
            )?)),
        }
    }
    fn read(&mut self, query: Read, cancel: &AtomicBool) -> Result<Vec<u8>> {
        ensure!(!cancel.load(Ordering::Acquire), "source read canceled");
        match (self, query) {
            (Self::Sql(source), Read::Sql(query)) => {
                crate::lightroom::bounded_json(&query.read(source)?, RESULT_BYTES)
            }
            (Self::Artifact(source), Read::ArtifactVerify) => {
                ArtifactRead::verify(source)?;
                crate::lightroom::bounded_json(&wire::Value::Verified, RESULT_BYTES)
            }
            (Self::Artifact(source), Read::ArtifactChunk { offset }) => {
                let bytes =
                    ArtifactRead::chunk(source, offset.0, &|| cancel.load(Ordering::Acquire))?;
                crate::lightroom::bounded_json(&wire::Value::Chunk(bytes), RESULT_BYTES)
            }
            _ => anyhow::bail!("query does not belong to admitted source mode"),
        }
    }
}
fn run(controls: &Controls, output: &mut impl Write) -> Result<()> {
    let Request::Begin {
        epoch,
        bytes,
        blake3,
    } = controls.receive()?
    else {
        anyhow::bail!("source Begin required");
    };
    let length: usize = bytes.0.try_into()?;
    ensure!(
        (1..=AUTHORITY_BYTES).contains(&length) && digest_valid(&blake3),
        "source authority bounds"
    );
    let mut encoded = Vec::with_capacity(length);
    loop {
        ensure!(
            !controls.cancel.load(Ordering::Acquire),
            "source opening canceled"
        );
        match controls.receive()? {
            Request::Authority { offset, bytes, .. } => {
                ensure!(
                    offset.0 == encoded.len() as u64
                        && !bytes.is_empty()
                        && bytes.len() <= CHUNK_BYTES
                        && bytes.len() <= length - encoded.len(),
                    "source authority continuity/bounds"
                );
                encoded.extend_from_slice(&bytes);
            }
            Request::Open { .. } => break,
            _ => anyhow::bail!("source authority command order"),
        }
    }
    ensure!(
        encoded.len() == length && crate::lightroom::digest(&encoded) == blake3,
        "source authority digest/length"
    );
    let authority: Authority = serde_json::from_slice(&encoded)?;
    drop(encoded);
    let binding = authority.binding()?;
    let mut roster = Roster::open(authority, controls.cancel.clone())?;
    write_frame(
        output,
        &Reply::Ready {
            epoch: epoch.clone(),
            binding: binding.clone(),
        },
    )?;
    let mut next = 1u64;
    let mut completed = 0u64;
    let mut chain = binding.clone();
    let mut poisoned = false;
    loop {
        match controls.receive()? {
            Request::Read {
                sequence,
                binding: requested_binding,
                query,
                ..
            } => {
                let read = (|| -> Result<()> {
                    ensure!(
                        !poisoned && sequence.0 == next && requested_binding == binding,
                        "source request identity/sequence"
                    );
                    next = next.checked_add(1).context("source sequence exhausted")?;
                    let query_digest = crate::lightroom::digest(&crate::lightroom::bounded_json(
                        &query,
                        120 * 1024,
                    )?);
                    let bytes = roster.read(query, &controls.cancel)?;
                    let digest = crate::lightroom::digest(&bytes);
                    let next_chain = next_chain(&chain, sequence.0, &query_digest, &digest)?;
                    write_frame(
                        output,
                        &Reply::Result {
                            epoch: epoch.clone(),
                            sequence,
                            query_blake3: query_digest,
                            bytes: crate::application::U64(bytes.len() as u64),
                            blake3: digest.clone(),
                        },
                    )?;
                    for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
                        // Cancellation may interrupt computation, but once a result
                        // starts its exact ticket framing is finished or the epoch dies.
                        write_frame(
                            output,
                            &Reply::Chunk {
                                epoch: epoch.clone(),
                                sequence,
                                offset: crate::application::U64((index * CHUNK_BYTES) as u64),
                                bytes: chunk.to_vec(),
                            },
                        )?;
                    }
                    write_frame(
                        output,
                        &Reply::Ticket {
                            epoch: epoch.clone(),
                            sequence,
                            binding: binding.clone(),
                            blake3: digest,
                            chain: next_chain.clone(),
                        },
                    )?;
                    completed = sequence.0;
                    chain = next_chain;
                    Ok(())
                })();
                if let Err(error) = read {
                    poisoned = true;
                    controls.cancel.store(true, Ordering::Release);
                    let mut detail = format!("{error:#}");
                    while detail.len() > 4096 {
                        detail.pop();
                    }
                    write_frame(
                        output,
                        &Reply::Failed {
                            epoch: epoch.clone(),
                            detail,
                        },
                    )?;
                }
            }
            Request::Retire {
                completed: ack,
                chain: ack_chain,
                ..
            } => {
                ensure!(
                    ack.0 == completed && ack_chain == chain,
                    "source consumption acknowledgement mismatch"
                );
                // Caller has already retired every dependent transaction/permit.
                // SQLite closes first inside MigrationSource, then the raw roster.
                drop(roster);
                write_frame(output, &Reply::Retired { epoch })?;
                return Ok(());
            }
            _ => {
                poisoned = true;
                controls.cancel.store(true, Ordering::Release);
                write_frame(
                    output,
                    &Reply::Failed {
                        epoch: epoch.clone(),
                        detail: "locked source command refused".into(),
                    },
                )?;
            }
        }
    }
}
