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
pub(super) fn allocation_backing() -> Result<usize> {
    use crate::lightroom_migration_worker::memory::{channels, layout::add};
    use std::alloc::Layout;
    // The control listener and sole reader access state/epoch; no third thread
    // uses these mutexes. Thread/runtime objects remain the explicit baseline.
    add(
        add(
            channels::arc(Layout::new::<Controls>())?,
            channels::arc(Layout::new::<AtomicBool>())?,
        )?,
        add(
            channels::pthread_mutexes(4)?,
            channels::pthread_condvars(2)?,
        )?,
    )
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
    fn reserved(&self, sequence: u64, bytes: usize) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            ensure!(
                !self.cancel.load(Ordering::Acquire),
                "source allocation admission canceled"
            );
            ensure!(
                state.error.is_none() && !state.ended,
                "source allocation grant stream ended"
            );
            if let Some(request) = state.pending.take() {
                self.changed.notify_all();
                ensure!(
                    matches!(request, Request::Reserved { sequence: n, bytes: b, .. } if n.0 == sequence && b.0 == bytes as u64),
                    "source allocation grant identity differs"
                );
                return Ok(());
            }
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
    fn signal_cancel(&self) {
        // Synchronize with the predicate check -> Condvar wait transition.
        // An atomic flag plus notify without this lock can lose the wakeup.
        let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.cancel.store(true, Ordering::Release);
        self.changed.notify_all();
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
pub(super) fn serve(input: impl IoRead + Send + 'static, output: impl Write) -> Result<()> {
    serve_mode(input, output, None)
}
pub(super) fn serve_mode(
    input: impl IoRead + Send + 'static,
    mut output: impl Write,
    expected: Option<super::relay::Kind>,
) -> Result<()> {
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
                            listener.signal_cancel();
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
    let result = run(&controls, &mut output, expected);
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
            let detail = reply_error_text(error);
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

#[allow(clippy::large_enum_variant)]
enum Roster {
    Sql(MigrationSource),
    Artifact(ArtifactReader),
    CaptureSql(super::capture_source::CaptureSource),
}
impl Roster {
    fn open(
        authority: Authority,
        cancel: Arc<AtomicBool>,
        admit: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<Self> {
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
                admit,
            )?)),
            Authority::Artifact {
                descriptor,
                limits,
                protected,
            } => Ok(Self::Artifact(ArtifactReader::open_owned_descriptor(
                descriptor,
                limits.try_into()?,
                &|| cancel.load(Ordering::Acquire),
                &protected,
                admit,
            )?)),
            Authority::CaptureSql { value } => {
                let opening = usize::try_from(value.limits.schema_bytes.0)?
                    .checked_mul(8)
                    .and_then(|v| v.checked_add(usize::try_from(value.limits.result_bytes.0).ok()?))
                    .context("CaptureSql opening allocation")?;
                admit(opening)?;
                Ok(Self::CaptureSql(
                    super::capture_source::CaptureSource::open(value, cancel)?,
                ))
            }
        }
    }
    fn read(&mut self, query: Read, sequence: u64, cancel: &AtomicBool) -> Result<Vec<u8>> {
        ensure!(!cancel.load(Ordering::Acquire), "source read canceled");
        match (self, query) {
            (Self::Sql(source), Read::Sql(query)) => {
                exact_json(&query.read(source)?, RESULT_BYTES, cancel)
            }
            (Self::Artifact(source), Read::ArtifactVerify) => {
                ArtifactRead::verify(source)?;
                exact_json(&wire::Value::Verified, RESULT_BYTES, cancel)
            }
            (Self::Artifact(source), Read::ArtifactChunk { offset }) => {
                let bytes =
                    ArtifactRead::chunk(source, offset.0, &|| cancel.load(Ordering::Acquire))?;
                exact_json(&wire::Value::Chunk(bytes), RESULT_BYTES, cancel)
            }
            (Self::CaptureSql(source), Read::CaptureSql(query)) => {
                let value = match query {
                    super::capture_wire::Query::SchemaObjects => {
                        wire::Value::CaptureSchemaObjects(source.schema())
                    }
                    super::capture_wire::Query::Variables => wire::Value::CaptureVariables {
                        authority_binding: source.binding().into(),
                        schema_roster_blake3: source.schema_digest().into(),
                        values: source.variables_value(),
                    },
                    super::capture_wire::Query::TableRows {
                        table_handle,
                        cursor,
                        limit,
                    } => {
                        let value = match source.table_rows(
                            table_handle,
                            cursor,
                            usize::try_from(limit.0)?,
                            sequence,
                        )? {
                            Ok(v) => super::capture_wire::TableValue::Batch(v),
                            Err(v) => super::capture_wire::TableValue::Failure(v),
                        };
                        wire::Value::CaptureTable(value)
                    }
                    super::capture_wire::Query::Current => {
                        wire::Value::CaptureCurrent(source.current()?)
                    }
                };
                exact_json(&value, source.result_limit()?, cancel)
            }
            _ => anyhow::bail!("query does not belong to admitted source mode"),
        }
    }
}
fn run(
    controls: &Controls,
    output: &mut impl Write,
    expected: Option<super::relay::Kind>,
) -> Result<()> {
    let Request::Begin {
        epoch,
        role,
        build,
        bytes,
        blake3,
    } = controls.receive()?
    else {
        anyhow::bail!("source Begin required");
    };
    validate_managed_handshake(role, &build, expected)?;
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
    let authority =
        super::authority_json::decode(&encoded, &|| controls.cancel.load(Ordering::Acquire))?;
    ensure!(
        matches!(
            (&authority, role),
            (Authority::Sql { .. }, super::relay::Kind::Sql)
                | (Authority::Artifact { .. }, super::relay::Kind::Raw)
                | (Authority::CaptureSql { .. }, super::relay::Kind::CaptureSql)
        ),
        "managed Source role/authority mismatch"
    );
    #[cfg(all(test, feature = "internal-capacity-probes"))]
    crate::capacity_probes::observe(crate::capacity_probes::OPEN_DECODED, encoded.capacity());
    drop(encoded);
    let binding = authority.binding()?;
    let mut allocation_sequence = 0u64;
    let mut reserve = |bytes: usize| -> Result<()> {
        ensure!(
            !controls.cancel.load(Ordering::Acquire),
            "source allocation admission canceled"
        );
        allocation_sequence = allocation_sequence
            .checked_add(1)
            .context("source allocation sequence exhausted")?;
        write_frame(
            output,
            &Reply::Reserve {
                epoch: epoch.clone(),
                sequence: crate::application::U64(allocation_sequence),
                bytes: crate::application::U64(bytes.try_into()?),
            },
        )?;
        controls.reserved(allocation_sequence, bytes)
    };
    let mut roster = Roster::open(authority, controls.cancel.clone(), &mut reserve)?;
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
                    let bytes = roster.read(query, sequence.0, &controls.cancel)?;
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
                    let detail = reply_error_text(&error);
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

fn validate_managed_handshake(
    kind: super::relay::Kind,
    build: &str,
    expected: Option<super::relay::Kind>,
) -> Result<()> {
    ensure!(
        build == crate::lightroom_migration_worker::worker::build_identity(),
        "managed Source build mismatch"
    );
    ensure!(expected == Some(kind), "managed Source role mismatch");
    Ok(())
}

// This is the existing wire error-prefix limit, enforced before formatting can
// grow an intermediate String. Underlying error/context storage is separate.
const REPLY_ERROR_BYTES: usize = 4096;
struct ReplyErrorText {
    text: String,
    stopped: bool,
}
impl std::fmt::Write for ReplyErrorText {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self.stopped {
            return Err(std::fmt::Error);
        }
        let remaining = REPLY_ERROR_BYTES - self.text.len();
        let mut end = remaining.min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&value[..end]);
        if end < value.len() || self.text.len() == REPLY_ERROR_BYTES {
            // Sticky even when a Display implementation ignores fmt::Error:
            // later short pieces must not replace the first truncated scalar.
            self.stopped = true;
            return Err(std::fmt::Error);
        }
        Ok(())
    }
}
fn reply_prefix(arguments: std::fmt::Arguments<'_>) -> String {
    let mut output = ReplyErrorText {
        text: String::with_capacity(REPLY_ERROR_BYTES),
        stopped: false,
    };
    // fmt::Error is our bounded-output stop signal. This never materializes the
    // complete formatted chain and asks standard formatters to stop immediately.
    let _ = std::fmt::write(&mut output, arguments);
    output.text
}
pub(super) fn reply_error_text(error: &anyhow::Error) -> String {
    reply_prefix(format_args!("{error:#}"))
}

#[cfg(test)]
mod reply_error_tests {
    use super::*;
    use std::{cell::Cell, fmt};

    #[test]
    fn bounded_reply_prefix_matches_existing_chain_and_utf8_behavior() {
        for value in [
            String::new(),
            "short".into(),
            "a".repeat(4096),
            "a".repeat(4097),
            "🦀".repeat(1025),
            format!("{}🦀tail", "a".repeat(4095)),
        ] {
            let error = anyhow::anyhow!(value).context("inner").context("outer");
            let mut expected = format!("{error:#}");
            while expected.len() > REPLY_ERROR_BYTES {
                expected.pop();
            }
            let actual = reply_error_text(&error);
            assert_eq!(actual, expected);
            assert!(actual.len() <= REPLY_ERROR_BYTES);
            assert_eq!(actual.capacity(), REPLY_ERROR_BYTES);
        }
    }

    #[test]
    fn bounded_reply_prefix_stops_formatting_without_full_intermediate_or_suffix() {
        struct StopAfterHead<'a> {
            head: &'a str,
            tail_seen: &'a Cell<bool>,
        }
        impl fmt::Display for StopAfterHead<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.head)?;
                self.tail_seen.set(true);
                f.write_str("tail")
            }
        }
        let head = "x".repeat(8192);
        let tail_seen = Cell::new(false);
        let actual = reply_prefix(format_args!(
            "{}",
            StopAfterHead {
                head: &head,
                tail_seen: &tail_seen
            }
        ));
        assert_eq!(actual, &head[..4096]);
        // Eager format!(...) would visit this tail before truncating its String.
        assert!(!tail_seen.get());

        struct IgnoresWriteError<'a>(&'a str);
        impl fmt::Display for IgnoresWriteError<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.0)?;
                let _ = f.write_str("🦀");
                let _ = f.write_str("z");
                Ok(())
            }
        }
        let head = "x".repeat(4095);
        let actual = reply_prefix(format_args!("{}", IgnoresWriteError(&head)));
        assert_eq!(actual, head);
        assert_eq!(actual.capacity(), REPLY_ERROR_BYTES);
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::*;
    fn controls() -> Controls {
        Controls {
            state: Mutex::new(Incoming {
                pending: None,
                error: None,
                ended: false,
            }),
            changed: Condvar::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            epoch: Mutex::new(None),
        }
    }
    fn epoch() -> Epoch {
        Epoch {
            guard: crate::lightroom_migration_worker::protocol::Guard {
                session: "s".into(),
                generation: "1".into(),
                operation: "o".into(),
            },
            reader: "r".into(),
        }
    }
    #[test]
    fn opening_grant_wait_wakes_for_cancel_or_parent_loss() {
        for ended in [false, true] {
            let c = Arc::new(controls());
            let waiter = c.clone();
            let (send, receive) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || send.send(waiter.reserved(1, 10).is_err()).unwrap());
            if ended {
                let mut state = c.state.lock().unwrap();
                state.ended = true;
                c.changed.notify_all();
            } else {
                c.signal_cancel();
            }
            assert!(
                receive
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
            );
            worker.join().unwrap();
        }
    }
    #[test]
    fn opening_grant_requires_exact_sequence_bytes_and_observes_cancel() -> Result<()> {
        for (sequence, bytes) in [(2, 10), (1, 11)] {
            let c = controls();
            c.state.lock().unwrap().pending = Some(Request::Reserved {
                epoch: epoch(),
                sequence: crate::application::U64(sequence),
                bytes: crate::application::U64(bytes),
            });
            assert!(c.reserved(1, 10).is_err());
        }
        let c = controls();
        c.cancel.store(true, Ordering::Release);
        assert!(c.reserved(1, 10).is_err());
        let c = controls();
        c.state.lock().unwrap().pending = Some(Request::Reserved {
            epoch: epoch(),
            sequence: crate::application::U64(1),
            bytes: crate::application::U64(10),
        });
        c.reserved(1, 10)?;
        Ok(())
    }

    #[test]
    fn lm_executor_batch3_source_build_and_role_are_exact_before_authority_admission() {
        let build = crate::lightroom_migration_worker::worker::build_identity();
        assert!(
            validate_managed_handshake(
                super::super::relay::Kind::Sql,
                build,
                Some(super::super::relay::Kind::Sql)
            )
            .is_ok()
        );
        assert!(
            validate_managed_handshake(
                super::super::relay::Kind::Raw,
                build,
                Some(super::super::relay::Kind::Raw)
            )
            .is_ok()
        );
        assert!(
            validate_managed_handshake(
                super::super::relay::Kind::CaptureSql,
                build,
                Some(super::super::relay::Kind::CaptureSql)
            )
            .is_ok()
        );
        assert!(
            validate_managed_handshake(
                super::super::relay::Kind::Raw,
                build,
                Some(super::super::relay::Kind::Sql)
            )
            .is_err()
        );
        assert!(
            validate_managed_handshake(
                super::super::relay::Kind::Sql,
                "different-build",
                Some(super::super::relay::Kind::Sql)
            )
            .is_err()
        );
        // The legacy unpinned Source entrypoint cannot accept a managed Begin.
        assert!(validate_managed_handshake(super::super::relay::Kind::Sql, build, None).is_err());
    }
}
