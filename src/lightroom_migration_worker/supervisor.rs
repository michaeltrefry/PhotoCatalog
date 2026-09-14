//! Parent-side protocol and writer ownership. This runs on the owned supervisor,
//! never the catalog actor. There is no destination/source filesystem access.
use super::{
    identity::FileKey,
    input::{INPUT_BYTES, TEXT_CHUNK, digest},
    memory::{MemoryBudget, Reservation},
    process::{Output, Process, SpawnFailure, Stop},
    protocol::{ChildFrame, DestinationPin, Guard, ParentFrame, WriteKind},
    source_reader::relay::broker::{Broker, StopState as BrokerStopState},
};
use crate::{
    application::U64,
    catalog_writer::{Priority, Writers},
};
use anyhow::{Context, Result, ensure};
use std::{
    fmt::Write as _,
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

mod pending;

const RESULT_BYTES: usize = 8 * 1024 * 1024;
const FAILURE_BYTES: usize = 32 * 1024;

#[derive(Debug)]
struct OperationCanceled;
impl std::fmt::Display for OperationCanceled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("migration operation canceled")
    }
}
impl std::error::Error for OperationCanceled {}
fn check_canceled(stop: &Stop) -> Result<()> {
    if stop.requested() {
        return Err(OperationCanceled.into());
    }
    Ok(())
}

/// The coordinator implements this against immutable operation admission and
/// the actor's cached attached identities. `writer` obtains an exact actor hold
/// acknowledgement before returning its shared Writers registry entry. Its
/// wait must observe Stop/deadline; it must not open supplied pathnames. The
/// supervisor records the attempt before calling writer: release is delivered
/// once after exact ReleaseWrite or helper reap even when writer errors/panics
/// after posting an actor hold. Release must safely retire an unacknowledged hold.
pub(crate) trait Admission: Send + 'static {
    fn lock(&mut self, target: &str, destination: &DestinationPin, lock: &FileKey) -> Result<()>;
    fn writer(
        &mut self,
        sequence: u64,
        kind: WriteKind,
        target: &str,
        lock: Option<&FileKey>,
        stop: &Stop,
        until: Instant,
    ) -> Result<Arc<Writers>>;
    fn release(&mut self, sequence: u64, kind: WriteKind) -> Result<()>;
    fn progress(&mut self, phase: &str, completed: u64, total: Option<u64>) -> Result<()>;
}
struct Held {
    sequence: u64,
    kind: WriteKind,
    permit: pending::Lease,
}
struct StreamedResult {
    bytes: usize,
    blake3: String,
    offset: usize,
    hash: blake3::Hasher,
    pages: Vec<Box<str>>,
}
struct State<A: Admission> {
    admission: Option<A>,
    pending: Option<pending::Pending<A>>,
    guard: Guard,
    input_digest: String,
    admitted: bool,
    lock: Option<(String, FileKey)>,
    next_write: u64,
    next_memory: u64,
    held: Option<Held>,
    attempted: Option<(u64, WriteKind)>,
    result: String,
    streamed_result: Option<StreamedResult>,
    streamed_maximum: Option<usize>,
    terminal: Option<Result<()>>,
    terminal_poisoned: bool,
    // Last: these reservations outlive every owner they fund.
    memory: Reservation,
    streamed_memory: Option<Reservation>,
}
impl<A: Admission> State<A> {
    fn new(admission: A, guard: Guard, input_digest: String, memory: Reservation) -> Self {
        Self::new_with_result(
            admission,
            guard,
            input_digest,
            memory,
            String::with_capacity(RESULT_BYTES),
            None,
            None,
        )
    }
    fn new_streaming(
        admission: A,
        guard: Guard,
        input_digest: String,
        memory: Reservation,
        result_memory: Reservation,
        result_maximum: usize,
    ) -> Self {
        Self::new_with_result(
            admission,
            guard,
            input_digest,
            memory,
            String::new(),
            Some(result_maximum),
            Some(result_memory),
        )
    }
    fn new_with_result(
        admission: A,
        guard: Guard,
        input_digest: String,
        memory: Reservation,
        result: String,
        streamed_maximum: Option<usize>,
        streamed_memory: Option<Reservation>,
    ) -> Self {
        Self {
            admission: Some(admission),
            pending: None,
            guard,
            input_digest,
            admitted: false,
            lock: None,
            next_write: 1,
            next_memory: 1,
            memory,
            held: None,
            attempted: None,
            result,
            streamed_result: None,
            streamed_maximum,
            streamed_memory,
            terminal: None,
            terminal_poisoned: false,
        }
    }
    fn accept(
        &mut self,
        frame: ChildFrame,
        stop: &Arc<Stop>,
        until: Instant,
    ) -> Result<Option<ParentFrame>> {
        ensure!(
            self.terminal.is_none(),
            "helper frame after terminal result"
        );
        let guard = match &frame {
            ChildFrame::Source { guard, .. }
            | ChildFrame::NeedMemory { guard, .. }
            | ChildFrame::Admitted { guard, .. }
            | ChildFrame::LockAcquired { guard, .. }
            | ChildFrame::NeedWrite { guard, .. }
            | ChildFrame::ReleaseWrite { guard, .. }
            | ChildFrame::Progress { guard, .. }
            | ChildFrame::BeginResult { guard, .. }
            | ChildFrame::Result { guard, .. }
            | ChildFrame::Finished { guard, .. }
            | ChildFrame::Failed { guard, .. } => guard,
        };
        ensure!(guard == &self.guard, "stale migration helper frame");
        // Pre-parse allocation permission conveys no source/target authority.
        // It can precede Admitted so decoding does not allocate before its grant.
        if let ChildFrame::NeedMemory {
            sequence, bytes, ..
        } = frame
        {
            ensure!(
                sequence.0 == self.next_memory,
                "stale or repeated helper memory request"
            );
            let next = self
                .next_memory
                .checked_add(1)
                .context("migration memory sequence exhausted")?;
            self.memory.grow(usize::try_from(bytes.0)?)?;
            self.next_memory = next;
            return Ok(Some(ParentFrame::MemoryGrant {
                guard: self.guard.clone(),
                sequence,
                bytes,
            }));
        }
        if let ChildFrame::Admitted {
            request_blake3,
            build,
            ..
        } = frame
        {
            ensure!(
                !self.admitted
                    && request_blake3 == self.input_digest
                    && build == super::worker::build_identity(),
                "helper admission digest/replay differs"
            );
            self.admitted = true;
            return Ok(None);
        }
        // A failed startup conveys no authority. Preserve its bounded terminal
        // cause even if multipart admission never reached Admitted.
        if let ChildFrame::Failed {
            detail, poisoned, ..
        } = frame
        {
            ensure!(detail.len() <= 32 * 1024, "migration error byte admission");
            self.terminal = Some(Err(anyhow::anyhow!("{detail}")));
            self.terminal_poisoned = poisoned;
            return Ok(None);
        }
        ensure!(self.admitted, "helper acted before exact input admission");
        match frame {
            ChildFrame::Source { .. } => {
                anyhow::bail!("Source command bypassed supervisor demultiplexer")
            }
            ChildFrame::NeedMemory { .. } => unreachable!(),
            ChildFrame::LockAcquired {
                lock,
                destination,
                target_token,
                ..
            } => {
                ensure!(
                    self.lock.is_none() && self.held.is_none(),
                    "helper lock order/replay differs"
                );
                digest(&target_token)?;
                self.admission
                    .as_mut()
                    .context("destination lock while writer admission pending")?
                    .lock(&target_token, &destination, &lock)?;
                self.lock = Some((target_token, lock));
            }
            ChildFrame::NeedWrite {
                sequence,
                write,
                target_token,
                lock,
                ..
            } => {
                ensure!(
                    sequence.0 == self.next_write
                        && self.held.is_none()
                        && self.attempted.is_none()
                        && self.pending.is_none(),
                    "nested or stale migration writer request"
                );
                digest(&target_token)?;
                match write {
                    WriteKind::Catalog => ensure!(
                        self.lock.as_ref().is_some_and(
                            |(token, key)| token == &target_token && Some(key) == lock.as_ref()
                        ),
                        "migration writer lock/pin differs"
                    ),
                    WriteKind::Bootstrap => ensure!(
                        self.lock.is_none() && lock.is_none(),
                        "bootstrap after catalog lock admission"
                    ),
                }
                // The callback can post an actor hold before its acknowledgement
                // fails. Own that attempted hold before entering caller code.
                self.attempted = Some((sequence.0, write));
                let admission = self
                    .admission
                    .take()
                    .context("migration admission already pending")?;
                match pending::Pending::start(
                    admission,
                    sequence.0,
                    write,
                    target_token,
                    lock,
                    stop.clone(),
                    until,
                ) {
                    Ok(pending) => self.pending = Some(pending),
                    Err(failure) => {
                        self.admission = Some(failure.admission);
                        return Err(failure.error);
                    }
                }
                return Ok(None);
            }
            ChildFrame::ReleaseWrite {
                sequence, write, ..
            } => {
                ensure!(
                    self.held
                        .as_ref()
                        .is_some_and(|h| h.sequence == sequence.0 && h.kind == write),
                    "unsolicited or stale migration writer release"
                );
                self.release()?;
            }
            ChildFrame::Progress {
                phase,
                completed,
                total,
                ..
            } => {
                ensure!(
                    !phase.is_empty() && phase.len() <= 256 && !phase.contains('\0'),
                    "migration phase bounds"
                );
                self.admission
                    .as_mut()
                    .context("progress while writer admission pending")?
                    .progress(&phase, completed.0, total.map(|n| n.0))?;
            }
            ChildFrame::BeginResult { bytes, blake3, .. } => {
                let maximum = self
                    .streamed_maximum
                    .context("streamed result unsupported by legacy supervisor")?;
                ensure!(
                    self.streamed_result.is_none() && self.result.is_empty(),
                    "repeated migration result admission"
                );
                super::input::digest(&blake3)?;
                let bytes = usize::try_from(bytes.0)?;
                ensure!(
                    bytes <= maximum,
                    "migration result accepted-type byte bound"
                );
                let (storage, pages) = super::protocol::result::retained_storage_bytes(bytes)?;
                self.streamed_memory
                    .as_mut()
                    .context("streamed result reservation absent")?
                    .grow(storage)?;
                self.streamed_result = Some(StreamedResult {
                    bytes,
                    blake3: blake3.clone(),
                    offset: 0,
                    hash: blake3::Hasher::new(),
                    pages: Vec::with_capacity(pages),
                });
                return Ok(Some(ParentFrame::ResultGrant {
                    guard: self.guard.clone(),
                    bytes: U64(bytes.try_into()?),
                    blake3,
                }));
            }
            ChildFrame::Result { offset, text, .. } => {
                if let Some(result) = &mut self.streamed_result {
                    ensure!(
                        offset.0 == result.offset as u64
                            && !text.is_empty()
                            && text.len() <= super::protocol::result::CHUNK
                            && text.len() <= result.bytes - result.offset
                            && result.pages.len() < result.pages.capacity(),
                        "migration streamed result offset/byte admission"
                    );
                    result.hash.update(text.as_bytes());
                    result.offset += text.len();
                    result.pages.push(Box::<str>::from(text.as_str()));
                } else {
                    ensure!(
                        self.streamed_maximum.is_none()
                            && offset.0 == self.result.len() as u64
                            && text.len() <= RESULT_BYTES - self.result.len(),
                        "migration result offset/byte admission"
                    );
                    self.result.push_str(&text);
                }
            }
            ChildFrame::Finished {
                result_blake3,
                bytes,
                ..
            } => {
                ensure!(
                    self.held.is_none() && self.attempted.is_none(),
                    "helper finished with an unreleased writer/admission"
                );
                digest(&result_blake3)?;
                if let Some(result) = &self.streamed_result {
                    ensure!(
                        bytes.0 == result.bytes as u64
                            && result.offset == result.bytes
                            && result_blake3 == result.blake3
                            && result.hash.clone().finalize().to_hex().as_str() == result.blake3,
                        "migration streamed result digest differs"
                    );
                } else {
                    ensure!(
                        self.streamed_maximum.is_none()
                            && bytes.0 == self.result.len() as u64
                            && result_blake3
                                == blake3::hash(self.result.as_bytes()).to_hex().as_str(),
                        "migration result digest differs"
                    );
                }
                self.terminal = Some(Ok(()));
            }
            ChildFrame::Failed { .. } => unreachable!(),
            ChildFrame::Admitted { .. } => unreachable!(),
        }
        Ok(None)
    }
    fn poll_admission(&mut self) -> Result<Option<ParentFrame>> {
        let Some(waiter) = self.pending.as_mut() else {
            return Ok(None);
        };
        if !waiter.finished() {
            return Ok(None);
        }
        let sequence = waiter.sequence;
        let kind = waiter.kind;
        let (admission, outcome) = waiter.take_ready();
        self.admission = Some(admission);
        let waiter = self.pending.take().expect("owned ready admission");
        match outcome {
            pending::Outcome::Finished(result) => result?,
            pending::Outcome::Panicked(panic) => {
                anyhow::bail!(
                    "migration writer admission panicked: {}",
                    pending::panic_detail(panic.as_ref())
                )
            }
        };
        let permit = waiter.into_lease();
        self.held = Some(Held {
            sequence,
            kind,
            permit,
        });
        self.next_write = self
            .next_write
            .checked_add(1)
            .context("migration grant sequence exhausted")?;
        Ok(Some(ParentFrame::Grant {
            guard: self.guard.clone(),
            sequence: U64(sequence),
            write: kind,
        }))
    }
    fn retry_release(&mut self) -> Result<bool> {
        let mut first = None;
        if let Some(waiter) = self.pending.as_mut() {
            waiter.stop.cancel();
            let Some((admission, outcome)) = waiter.retry_join() else {
                return Ok(false);
            };
            self.admission = Some(admission);
            self.pending.take();
            match outcome {
                pending::Outcome::Finished(Ok(())) => {}
                pending::Outcome::Finished(Err(error)) => first = Some(error),
                pending::Outcome::Panicked(panic) => {
                    first = Some(anyhow::anyhow!(
                        "migration writer admission panicked: {}",
                        pending::panic_detail(panic.as_ref())
                    ))
                }
            }
        }
        if let Some(held) = self.held.as_mut() {
            let Some(retired) = held.permit.retry_retire() else {
                return Ok(false);
            };
            self.held.take();
            if let Err(error) = retired {
                first.get_or_insert(error);
            }
        }
        if let Some((sequence, kind)) = self.attempted {
            self.admission
                .as_mut()
                .context("migration release while admission pending")?
                .release(sequence, kind)?;
            self.attempted.take();
        }
        if let Some(error) = first {
            return Err(error);
        }
        Ok(true)
    }
    fn release(&mut self) -> Result<()> {
        loop {
            if self.retry_release()? {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn take_saved_result(&mut self, legacy_memory: Option<Reservation>) -> Result<SavedResult> {
        ensure!(
            self.terminal.as_ref().is_some_and(Result::is_ok),
            "migration result taken before successful terminal"
        );
        if let Some(streamed) = self.streamed_result.take() {
            return Ok(SavedResult {
                body: SavedBody::Paged {
                    pages: streamed.pages,
                    bytes: streamed.bytes,
                    blake3: streamed.blake3,
                },
                memory: self
                    .streamed_memory
                    .take()
                    .context("streamed result reservation absent")?,
            });
        }
        Ok(SavedResult {
            body: SavedBody::Legacy(std::mem::take(&mut self.result)),
            memory: legacy_memory.context("legacy result reservation absent")?,
        })
    }
}
impl<A: Admission> Drop for State<A> {
    fn drop(&mut self) {
        loop {
            match self.retry_release() {
                Ok(true) => return,
                Ok(false) | Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

/// Field order is intentional: process kill/reap and both pipe joins precede
/// State's parent permit release, including unwinding and malformed output.
struct Owned<A: Admission> {
    process: Option<Process>,
    broker: Option<Broker>,
    lm_drained: bool,
    broker_drained: bool,
    drain_fault: Option<anyhow::Error>,
    primary_broker_failure: Option<anyhow::Error>,
    // Last: State's operation reservation funds every owner above it.
    state: State<A>,
}
impl<A: Admission> Owned<A> {
    fn revoke_all(&mut self) {
        if let Some(process) = &self.process {
            let _ = process.try_send_control(ParentFrame::Cancel {
                guard: self.state.guard.clone(),
            });
        }
        if let Some(waiter) = &self.state.pending {
            waiter.stop.cancel();
        }
        if let Some(process) = &mut self.process {
            process.revoke();
        }
        if let Some(broker) = &self.broker {
            broker.revoke_after_lm();
        }
    }
    /// Returns true only after every physical owner and checked release has
    /// retired. Errors from unconfirmed waits/releases retain this whole owner.
    fn retry_drain(&mut self) -> Result<bool> {
        self.revoke_all();
        if !self.lm_drained {
            let report = if let Some(process) = &mut self.process {
                let Some(report) = process.retry_drain()? else {
                    return Ok(false);
                };
                Some(report)
            } else {
                None
            };
            self.lm_drained = true;
            if let Some(report) = report {
                if report.io_panicked {
                    self.drain_fault.get_or_insert_with(|| {
                        anyhow::anyhow!("migration I/O thread panicked during owned drain")
                    });
                }
            }
        }
        if !self.broker_drained {
            if let Some(broker) = &mut self.broker {
                let Some(report) = broker.retry_finish() else {
                    return Ok(false);
                };
                if let Some(error) = report.failure {
                    if report.primary_failure {
                        // The Broker's original run error precedes its internal
                        // Stop and any secondary LM I/O/drain fault.
                        self.primary_broker_failure = Some(error);
                    } else {
                        self.drain_fault.get_or_insert(error);
                    }
                }
            }
            self.broker_drained = true;
        }
        match self.state.retry_release() {
            Ok(false) => return Ok(false),
            Err(error) => {
                if self.state.attempted.is_some() {
                    return Err(error);
                }
                self.drain_fault.get_or_insert(error);
            }
            Ok(true) => {}
        }
        Ok(true)
    }
}
impl<A: Admission> Drop for Owned<A> {
    fn drop(&mut self) {
        while !self.retry_drain().unwrap_or(false) {
            thread::sleep(Duration::from_millis(20));
        }
    }
}
fn send(process: &Process, mut frame: ParentFrame, stop: &Stop, until: Instant) -> Result<()> {
    loop {
        check_canceled(stop)?;
        ensure!(Instant::now() < until, "migration operation deadline");
        match process.try_send(frame)? {
            None => return Ok(()),
            Some(unsent) => frame = unsent,
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Send one described document without nesting it in operation JSON. The
/// worker requests its retained capacity after BeginPart and before it reads
/// content, so the supervisor pumps that exact NeedMemory/MemoryGrant exchange
/// before filling the bounded input queue. The startup grant uses that same
/// FIFO: queueing it on the urgent channel could let content wake a writer
/// already blocked on the data queue and overtake the grant.
fn send_part_admitted<A: Admission>(
    owner: &mut Owned<A>,
    guard: &Guard,
    role: super::protocol::InputRole,
    text: &str,
    stop: &Arc<Stop>,
    until: Instant,
) -> Result<()> {
    ensure!(
        text.len() <= role.maximum_bytes(),
        "migration input part byte admission"
    );
    let blake3 = blake3::hash(text.as_bytes()).to_hex().to_string();
    let before = owner.state.next_memory;
    send(
        owner.process.as_ref().context("migration process absent")?,
        ParentFrame::BeginPart {
            guard: guard.clone(),
            role,
            blake3: blake3.clone(),
            bytes: U64(text.len().try_into()?),
        },
        stop,
        until,
    )?;
    let mut control = None;
    while owner.state.next_memory == before || control.is_some() {
        if let Some(frame) = control.take() {
            control = owner
                .process
                .as_ref()
                .context("migration process absent")?
                .try_send(frame)?;
        }
        if control.is_none() && owner.state.next_memory == before {
            match owner
                .process
                .as_ref()
                .context("migration process absent")?
                .try_receive()?
            {
                Output::Pending => {}
                Output::Frame(
                    frame @ ChildFrame::NeedMemory {
                        bytes: requested, ..
                    },
                ) => {
                    ensure!(
                        requested.0 == text.len() as u64,
                        "multipart retained byte request differs"
                    );
                    control = owner.state.accept(frame, stop, until)?;
                }
                Output::Frame(frame @ ChildFrame::Failed { .. }) => {
                    owner.state.accept(frame, stop, until)?;
                    let error = owner
                        .state
                        .terminal
                        .as_ref()
                        .and_then(|r| r.as_ref().err())
                        .context("startup failure lacks terminal cause")?;
                    anyhow::bail!("{error:#}")
                }
                Output::Frame(_) => {
                    anyhow::bail!("worker acted before multipart input admission")
                }
                Output::End => anyhow::bail!("worker ended during multipart input admission"),
            }
        }
        check_canceled(stop)?;
        ensure!(Instant::now() < until, "migration operation deadline");
        thread::sleep(Duration::from_millis(2));
    }
    let mut offset = 0;
    while offset < text.len() {
        let mut end = (offset + TEXT_CHUNK).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        send(
            owner.process.as_ref().context("migration process absent")?,
            ParentFrame::Part {
                guard: guard.clone(),
                role,
                offset: U64(offset.try_into()?),
                text: text[offset..end].into(),
            },
            stop,
            until,
        )?;
        offset = end;
    }
    send(
        owner.process.as_ref().context("migration process absent")?,
        ParentFrame::FinishPart {
            guard: guard.clone(),
            role,
            blake3,
        },
        stop,
        until,
    )
}

/// Exact result bytes stay charged while cached by the coordinator. The private
/// wrapper provides borrowed access; moving out a naked String would lose that
/// lifetime, so no such conversion is exposed.
enum SavedBody {
    Legacy(String),
    Paged {
        pages: Vec<Box<str>>,
        bytes: usize,
        blake3: String,
    },
}
pub(crate) struct SavedResult {
    body: SavedBody,
    memory: Reservation,
}
impl SavedResult {
    pub(crate) fn text(&self) -> &str {
        match &self.body {
            SavedBody::Legacy(text) => text,
            SavedBody::Paged { .. } => panic!("paged migration result has no contiguous text"),
        }
    }
    pub(crate) fn page_count(&self) -> usize {
        match &self.body {
            SavedBody::Legacy(text) => usize::from(!text.is_empty()),
            SavedBody::Paged { pages, .. } => pages.len(),
        }
    }
    pub(crate) fn page(&self, index: usize) -> Option<&str> {
        match &self.body {
            SavedBody::Legacy(text) => (index == 0 && !text.is_empty()).then_some(text),
            SavedBody::Paged { pages, .. } => pages.get(index).map(AsRef::as_ref),
        }
    }
    pub(crate) fn identity(&self) -> Option<(usize, &str)> {
        match &self.body {
            SavedBody::Legacy(_) => None,
            SavedBody::Paged { bytes, blake3, .. } => Some((*bytes, blake3)),
        }
    }
}

#[derive(Debug)]
pub(crate) enum FailureCause {
    ResourceLimit(crate::lightroom_migration_worker::memory::ResourceLimit),
    Canceled,
    Rejected(String),
    StatusUnavailable,
}
pub(crate) struct Failure {
    pub(crate) cause: FailureCause,
    pub(crate) poisoned: bool,
    pub(crate) outcome_unknown: bool,
    // Last: admitted backing outlives any retained Rejected String.
    _memory: Option<Reservation>,
}
impl std::fmt::Debug for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Failure")
            .field("cause", &self.cause)
            .field("poisoned", &self.poisoned)
            .field("outcome_unknown", &self.outcome_unknown)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.cause {
            FailureCause::ResourceLimit(limit) => write!(formatter, "{limit}"),
            FailureCause::Canceled => formatter.write_str("migration operation canceled"),
            FailureCause::Rejected(detail) => formatter.write_str(detail),
            FailureCause::StatusUnavailable => {
                formatter.write_str("migration failure status admission unavailable")
            }
        }
    }
}
impl std::error::Error for Failure {}
impl Failure {
    fn from_error(
        error: anyhow::Error,
        poisoned: bool,
        outcome_unknown: bool,
        budget: &MemoryBudget,
    ) -> Self {
        if let Some(limit) =
            error.downcast_ref::<crate::lightroom_migration_worker::memory::ResourceLimit>()
        {
            return Self {
                cause: FailureCause::ResourceLimit(limit.clone()),
                poisoned,
                outcome_unknown,
                _memory: None,
            };
        } else if error.downcast_ref::<OperationCanceled>().is_some() {
            return Self {
                cause: FailureCause::Canceled,
                poisoned,
                outcome_unknown,
                _memory: None,
            };
        }
        let mut memory = budget.reservation();
        if let Err(admission) = memory.grow(FAILURE_BYTES) {
            if let Some(limit) =
                admission.downcast_ref::<crate::lightroom_migration_worker::memory::ResourceLimit>()
            {
                return Self {
                    cause: FailureCause::ResourceLimit(limit.clone()),
                    poisoned,
                    outcome_unknown,
                    _memory: None,
                };
            }
            return Self {
                cause: FailureCause::StatusUnavailable,
                poisoned: true,
                outcome_unknown: true,
                _memory: None,
            };
        }
        let mut detail = String::with_capacity(FAILURE_BYTES);
        let _ = write!(
            FailureText {
                text: &mut detail,
                maximum: FAILURE_BYTES,
            },
            "{error}"
        );
        Self {
            cause: FailureCause::Rejected(detail),
            poisoned,
            outcome_unknown,
            _memory: Some(memory),
        }
    }
    fn retain_drain_fault(&mut self) {
        self.poisoned = true;
        self.outcome_unknown = true;
    }
}
struct FailureText<'a> {
    text: &'a mut String,
    maximum: usize,
}
impl std::fmt::Write for FailureText<'_> {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let remaining = self.maximum - self.text.len();
        let mut end = value.len().min(remaining);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&value[..end]);
        Ok(())
    }
}
pub(crate) enum Drained {
    Complete(SavedResult),
    Failed(Failure),
}
impl Drained {
    fn into_result(self) -> Result<SavedResult> {
        match self {
            Self::Complete(result) => Ok(result),
            Self::Failed(Failure {
                cause: FailureCause::ResourceLimit(limit),
                ..
            }) => Err(limit.into()),
            Self::Failed(Failure {
                cause: FailureCause::Canceled,
                ..
            }) => Err(OperationCanceled.into()),
            Self::Failed(failure) => Err(failure.into()),
        }
    }
}
pub(crate) struct DrainPending<A: Admission> {
    owner: Option<Owned<A>>,
    result_memory: Option<Reservation>,
    failure: Option<Failure>,
}
impl<A: Admission> DrainPending<A> {
    fn new(owner: Owned<A>, result_memory: Option<Reservation>, failure: Option<Failure>) -> Self {
        Self {
            owner: Some(owner),
            result_memory,
            failure,
        }
    }
    pub(crate) fn failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }
    pub(crate) fn retry_drain(&mut self) -> Option<Drained> {
        let owner = self.owner.as_mut().expect("pending drain owner");
        match owner.retry_drain() {
            Ok(false) => return None,
            Err(error) => {
                if let Some(failure) = &mut self.failure {
                    failure.retain_drain_fault();
                } else {
                    self.failure = Some(Failure::from_error(
                        error,
                        true,
                        true,
                        &owner.state.memory.budget(),
                    ));
                }
                return None;
            }
            Ok(true) => {}
        }
        if let Some(error) = owner.drain_fault.take() {
            if let Some(failure) = &mut self.failure {
                failure.poisoned = true;
            } else {
                self.failure = Some(Failure::from_error(
                    error,
                    true,
                    false,
                    &owner.state.memory.budget(),
                ));
            }
        }
        if let Some(error) = owner.primary_broker_failure.take() {
            let previous = self.failure.take();
            let outcome_unknown = previous
                .as_ref()
                .is_some_and(|failure| failure.outcome_unknown);
            // Release any secondary retained failure text before admitting the
            // primary Broker status. The original error itself stays owned.
            drop(previous);
            self.failure = Some(Failure::from_error(
                error,
                true,
                outcome_unknown || (owner.state.admitted && owner.state.terminal.is_none()),
                &owner.state.memory.budget(),
            ));
        }
        let failure_budget = owner.state.memory.budget();
        let mut owner = self.owner.take().expect("completed drain owner");
        let drained = if let Some(failure) = self.failure.take() {
            Drained::Failed(failure)
        } else {
            match owner.state.take_saved_result(self.result_memory.take()) {
                Ok(result) => Drained::Complete(result),
                Err(error) => {
                    Drained::Failed(Failure::from_error(error, true, false, &failure_budget))
                }
            }
        };
        drop(owner);
        Some(drained)
    }
}

pub(crate) enum Operation<A: Admission> {
    Running(Running<A>),
    Drained(Drained),
    DrainPending(DrainPending<A>),
}

pub(crate) struct Running<A: Admission> {
    ended: bool,
    command: Option<super::protocol::SourceCommand>,
    data: Option<ParentFrame>,
    control: Option<ParentFrame>,
    stop: Arc<Stop>,
    until: Instant,
    // Last: owner funds all queued frames; result memory funds State.result.
    owner: Option<Owned<A>>,
    result_memory: Option<Reservation>,
}

pub(crate) struct InputPart<'a> {
    pub(crate) role: super::protocol::InputRole,
    pub(crate) text: &'a str,
}
impl<A: Admission> Operation<A> {
    /// One nonblocking execution step. Returns true once execution has left the
    /// running state; physical drain can still be pending.
    pub(crate) fn poll(&mut self) -> bool {
        if let Self::Running(running) = self {
            let failure = match running.poll() {
                Ok(false) => return false,
                Ok(true) => None,
                Err(error) => Some(running.failure(error)),
            };
            let pending = running.take_pending(failure);
            *self = Self::DrainPending(pending);
        }
        true
    }
    pub(crate) fn retry_drain(&mut self) -> Option<&Drained> {
        if !self.poll() {
            return None;
        }
        if let Self::DrainPending(pending) = self {
            if let Some(drained) = pending.retry_drain() {
                *self = Self::Drained(drained);
            } else {
                return None;
            }
        }
        match self {
            Self::Drained(drained) => Some(drained),
            Self::Running(_) | Self::DrainPending(_) => unreachable!(),
        }
    }
    fn drain_blocking(mut self) -> Drained {
        loop {
            if self.retry_drain().is_some() {
                return match self {
                    Self::Drained(drained) => drained,
                    Self::Running(_) | Self::DrainPending(_) => unreachable!(),
                };
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// G owns the executor and both Source roles through the broker. A managed
/// executor receives tokens rather than spawning descendants. The complete
/// application coordinator still supplies approved action/phase admission.
///
/// Owns bounded UTF-8 authority/result memory plus bounded pipe frames.
/// Large saved evidence is returned by the paged/chunk query layer; this limit
/// never truncates results or changes a caller's reviewed migration scope.
pub(crate) fn execute<A: Admission>(
    executable: &Path,
    guard: Guard,
    request: &str,
    stop: Arc<Stop>,
    until: Instant,
    admission: A,
    budget: MemoryBudget,
) -> Result<SavedResult> {
    execute_operation_with_broker(
        |stop| Process::spawn_owned(executable, stop),
        Some(executable),
        guard,
        request,
        stop,
        until,
        admission,
        budget,
        &[],
        None,
    )
    .drain_blocking()
    .into_result()
}
/// New managed callers retain this operation in G and drive `retry_drain`
/// until it reaches a checked terminal. Worker dispatch and the multipart
/// document roster are connected by the next executor batch.
pub(crate) fn execute_operation<A: Admission>(
    executable: &Path,
    guard: Guard,
    operation: &str,
    stop: Arc<Stop>,
    until: Instant,
    admission: A,
    budget: MemoryBudget,
    documents: &[InputPart<'_>],
    result_maximum: usize,
) -> Operation<A> {
    execute_operation_with_broker(
        |stop| Process::spawn_owned(executable, stop),
        Some(executable),
        guard,
        operation,
        stop,
        until,
        admission,
        budget,
        documents,
        Some(result_maximum),
    )
}
#[cfg(test)]
fn execute_owned<A: Admission>(
    spawn: impl FnOnce(Arc<Stop>) -> Result<Process>,
    guard: Guard,
    request: &str,
    stop: Arc<Stop>,
    until: Instant,
    admission: A,
    budget: MemoryBudget,
) -> Result<SavedResult> {
    execute_operation_with_broker(
        |stop| {
            spawn(stop).map_err(|error| SpawnFailure {
                error,
                process: None,
            })
        },
        None,
        guard,
        request,
        stop,
        until,
        admission,
        budget,
        &[],
        None,
    )
    .drain_blocking()
    .into_result()
}
#[cfg(test)]
fn execute_owned_operation<A: Admission>(
    spawn: impl FnOnce(Arc<Stop>) -> std::result::Result<Process, SpawnFailure>,
    guard: Guard,
    request: &str,
    stop: Arc<Stop>,
    until: Instant,
    admission: A,
    budget: MemoryBudget,
    streamed_result_maximum: Option<usize>,
) -> Operation<A> {
    execute_operation_with_broker(
        spawn,
        None,
        guard,
        request,
        stop,
        until,
        admission,
        budget,
        &[],
        streamed_result_maximum,
    )
}
fn execute_operation_with_broker<A: Admission>(
    spawn: impl FnOnce(Arc<Stop>) -> std::result::Result<Process, SpawnFailure>,
    source_executable: Option<&Path>,
    guard: Guard,
    request: &str,
    stop: Arc<Stop>,
    until: Instant,
    admission: A,
    budget: MemoryBudget,
    documents: &[InputPart<'_>],
    streamed_result_maximum: Option<usize>,
) -> Operation<A> {
    let setup = (|| -> Result<(State<A>, Option<Reservation>, String, Option<Broker>)> {
        guard.validate()?;
        ensure!(
            request.len() <= INPUT_BYTES,
            "migration request byte admission"
        );
        check_canceled(&stop)?;
        ensure!(Instant::now() < until, "migration admission deadline");
        // Result storage is allocated below and stays owned across lost replies.
        // Full typed/core phase admission is added by the coordinator separately.
        let mut memory = budget.reservation();
        memory.grow(INPUT_BYTES)?;
        if streamed_result_maximum.is_some() {
            // LM decodes this bounded envelope before its control listener can
            // receive a child-originated grant. Admit the complete typed graph
            // in G before spawn; raw INPUT_BYTES remains a separate owner.
            memory.grow(super::memory::core::worker_envelope(request.len())?)?;
        }
        // Reserve current-target backing before constructing the broker or any
        // Child/pipe owner. Two Sources are the complete concurrent role roster.
        // These fixed owners remain operation-scoped through every checked join.
        let mut backing = Process::<ChildFrame>::allocation_backing()?;
        if let Some(path) = source_executable {
            use super::memory::layout::add;
            backing = add(backing, Broker::allocation_backing()?)?;
            backing = add(backing, path.as_os_str().len())?;
        }
        memory.grow(backing)?;
        // Every pipe/parser/relay owner is funded before any helper or broker can
        // read bytes. This operation reservation survives all physical and logical
        // Source retirement; it is separate from per-query/result/core phases.
        memory.grow(super::memory::transport::payloads(source_executable.is_some())?.total()?)?;
        let (state, result_memory) = if let Some(maximum) = streamed_result_maximum {
            let streamed_memory = budget.reservation();
            (
                State::new_streaming(
                    admission,
                    guard.clone(),
                    blake3::hash(request.as_bytes()).to_hex().to_string(),
                    memory,
                    streamed_memory,
                    maximum,
                ),
                None,
            )
        } else {
            let mut result_memory = budget.reservation();
            result_memory.grow(RESULT_BYTES)?;
            (
                State::new(
                    admission,
                    guard.clone(),
                    blake3::hash(request.as_bytes()).to_hex().to_string(),
                    memory,
                ),
                Some(result_memory),
            )
        };
        let input_digest = blake3::hash(request.as_bytes()).to_hex().to_string();
        // Start the broker before LM; if LM launch fails, no Source command could
        // have arrived and the broker's owned Drop still joins its thread.
        let broker = source_executable
            .map(|path| {
                Broker::start(
                    path.to_path_buf(),
                    guard.clone(),
                    stop.clone(),
                    budget.clone(),
                )
            })
            .transpose()?;
        Ok((state, result_memory, input_digest, broker))
    })();
    let (state, result_memory, input_digest, broker) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            return Operation::Drained(Drained::Failed(Failure::from_error(
                error, false, false, &budget,
            )));
        }
    };
    let spawned = spawn(stop.clone());
    let (process, spawn_error) = match spawned {
        Ok(process) => (Some(process), None),
        Err(failure) => (failure.process, Some(failure.error)),
    };
    let mut owner = Owned {
        process,
        broker,
        state,
        lm_drained: false,
        broker_drained: false,
        drain_fault: None,
        primary_broker_failure: None,
    };
    if let Some(error) = spawn_error {
        let failure = Failure::from_error(error, false, false, &owner.state.memory.budget());
        return Operation::DrainPending(DrainPending::new(owner, result_memory, Some(failure)));
    }
    if let Err(error) = send_inputs(
        &mut owner,
        &guard,
        request,
        input_digest,
        documents,
        &stop,
        until,
    ) {
        let failure = failure_for_owner(&owner, error);
        return Operation::DrainPending(DrainPending::new(owner, result_memory, Some(failure)));
    }
    Operation::Running(Running {
        owner: Some(owner),
        result_memory,
        ended: false,
        command: None,
        data: None,
        control: None,
        stop,
        until,
    })
}

fn failure_for_owner<A: Admission>(owner: &Owned<A>, error: anyhow::Error) -> Failure {
    let terminal_known = owner.state.terminal.is_some();
    let acted = owner.state.admitted;
    Failure::from_error(
        error,
        owner.state.terminal_poisoned || (acted && !terminal_known),
        acted && !terminal_known,
        &owner.state.memory.budget(),
    )
}

fn send_inputs<A: Admission>(
    owner: &mut Owned<A>,
    guard: &Guard,
    request: &str,
    input_digest: String,
    documents: &[InputPart<'_>],
    stop: &Arc<Stop>,
    until: Instant,
) -> Result<()> {
    let process = owner.process.as_ref().context("migration process absent")?;
    send(
        process,
        ParentFrame::Begin {
            guard: guard.clone(),
            request_blake3: input_digest.clone(),
            bytes: U64(request.len() as u64),
        },
        stop,
        until,
    )?;
    let mut offset = 0;
    while offset < request.len() {
        let mut end = (offset + TEXT_CHUNK).min(request.len());
        while !request.is_char_boundary(end) {
            end -= 1;
        }
        send(
            process,
            ParentFrame::Input {
                guard: guard.clone(),
                offset: U64(offset as u64),
                text: request[offset..end].into(),
            },
            stop,
            until,
        )?;
        offset = end;
    }
    send(
        process,
        ParentFrame::FinishInput {
            guard: guard.clone(),
            blake3: input_digest,
        },
        stop,
        until,
    )?;
    for document in documents {
        send_part_admitted(owner, guard, document.role, document.text, stop, until)?;
    }
    Ok(())
}

impl<A: Admission> Running<A> {
    fn failure(&self, error: anyhow::Error) -> Failure {
        failure_for_owner(self.owner.as_ref().expect("running migration owner"), error)
    }
    fn take_pending(&mut self, failure: Option<Failure>) -> DrainPending<A> {
        DrainPending::new(
            self.owner.take().expect("running migration owner"),
            self.result_memory.take(),
            failure,
        )
    }
    fn poll(&mut self) -> Result<bool> {
        let Self {
            owner,
            result_memory: _,
            ended,
            command,
            data,
            control,
            stop,
            until,
        } = self;
        let until = *until;
        let owner = owner.as_mut().context("running migration owner absent")?;
        if let Some(broker) = &owner.broker {
            match broker.stop_state(stop) {
                BrokerStopState::Running => {}
                BrokerStopState::BrokerFailed => return Ok(true),
                BrokerStopState::Canceled => return Err(OperationCanceled.into()),
            }
        } else {
            check_canceled(stop)?;
        }
        // Reserved failure delivery bypasses both actor admission and the normal
        // data queue. If the executor cannot receive it, revocation kills it;
        // successful commits preceding observed loss retain their exact receipt.
        if let Some(broker) = &owner.broker {
            if let Some(event) = broker.try_urgent() {
                let frame = ParentFrame::Source {
                    guard: owner.state.guard.clone(),
                    event,
                };
                if let Some(unsent) = owner
                    .process
                    .as_ref()
                    .context("migration process absent")?
                    .try_send_control(frame)?
                {
                    *control = Some(unsent);
                }
            }
        }
        ensure!(
            Instant::now() < until,
            "migration operation deadline; all helpers drained before return"
        );
        if let Some(frame) = control.take() {
            *control = owner
                .process
                .as_ref()
                .context("migration process absent")?
                .try_send_control(frame)?;
        }
        if control.is_none() {
            *control = owner.state.poll_admission()?;
        }
        if let Some(broker) = &owner.broker {
            if let Some(next) = command.take() {
                *command = broker.try_send(next)?;
            }
            if data.is_none() {
                *data = broker.try_receive()?.map(|event| ParentFrame::Source {
                    guard: owner.state.guard.clone(),
                    event,
                });
            }
        }
        if let Some(frame) = data.take() {
            *data = owner
                .process
                .as_ref()
                .context("migration process absent")?
                .try_send(frame)?;
        }
        if !*ended && command.is_none() && control.is_none() {
            match owner
                .process
                .as_ref()
                .context("migration process absent")?
                .try_receive()?
            {
                Output::Pending => {}
                Output::Frame(ChildFrame::Source {
                    guard,
                    command: next,
                }) => {
                    ensure!(
                        guard == owner.state.guard
                            && owner.state.admitted
                            && owner.state.terminal.is_none(),
                        "Source relay command admission/guard differs"
                    );
                    *command = owner
                        .broker
                        .as_ref()
                        .context("test executor has no configured Source broker")?
                        .try_send(next)?;
                }
                Output::Frame(frame) => {
                    *control = owner.state.accept(frame, stop, until)?;
                }
                Output::End => *ended = true,
            }
        }
        if *ended
            && let Some(status) = owner
                .process
                .as_mut()
                .context("migration process absent")?
                .try_reap()?
        {
            ensure!(status.success(), "migration helper exited {status}");
            match owner
                .state
                .terminal
                .as_ref()
                .context("migration helper ended without a terminal result")?
            {
                Ok(()) => {}
                Err(error) => anyhow::bail!("{error}"),
            }
            ensure!(
                command.is_none() && data.is_none() && control.is_none(),
                "migration helper ended with pending relay/control output"
            );
            if let Some(broker) = &owner.broker {
                broker.ensure_idle()?;
            }
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
