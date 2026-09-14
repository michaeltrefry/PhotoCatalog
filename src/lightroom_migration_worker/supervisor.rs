//! Parent-side protocol and writer ownership. This runs on the owned supervisor,
//! never the catalog actor. There is no destination/source filesystem access.
use super::{
    identity::FileKey,
    input::{INPUT_BYTES, TEXT_CHUNK, digest},
    memory::{MemoryBudget, Reservation},
    process::{Output, Process, Stop},
    protocol::{ChildFrame, DestinationPin, Guard, ParentFrame, WriteKind},
    source_reader::relay::broker::Broker,
};
use crate::{
    application::U64,
    catalog_writer::{Priority, Writers},
};
use anyhow::{Context, Result, ensure};
use std::{
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

mod pending;

const RESULT_BYTES: usize = 8 * 1024 * 1024;

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
    fn release(&mut self, sequence: u64, kind: WriteKind);
    fn progress(&mut self, phase: &str, completed: u64, total: Option<u64>) -> Result<()>;
}
struct Held {
    sequence: u64,
    kind: WriteKind,
    permit: pending::Lease,
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
    memory: Reservation,
    held: Option<Held>,
    attempted: Option<(u64, WriteKind)>,
    result: String,
    terminal: Option<Result<()>>,
}
impl<A: Admission> State<A> {
    fn new(admission: A, guard: Guard, input_digest: String, memory: Reservation) -> Self {
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
            result: String::with_capacity(RESULT_BYTES),
            terminal: None,
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
        if let ChildFrame::Admitted { request_blake3, .. } = frame {
            ensure!(
                !self.admitted && request_blake3 == self.input_digest,
                "helper admission digest/replay differs"
            );
            self.admitted = true;
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
                    Err(error) => {
                        self.attempted.take();
                        return Err(error);
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
            ChildFrame::BeginResult { .. } => {
                anyhow::bail!("streamed result unsupported by legacy supervisor")
            }
            ChildFrame::Result { offset, text, .. } => {
                ensure!(
                    offset.0 == self.result.len() as u64
                        && text.len() <= RESULT_BYTES - self.result.len(),
                    "migration result offset/byte admission"
                );
                self.result.push_str(&text);
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
                ensure!(
                    bytes.0 == self.result.len() as u64
                        && result_blake3 == blake3::hash(self.result.as_bytes()).to_hex().as_str(),
                    "migration result digest differs"
                );
                self.terminal = Some(Ok(()));
            }
            ChildFrame::Failed { detail, .. } => {
                ensure!(detail.len() <= 32 * 1024, "migration error byte admission");
                self.terminal = Some(Err(anyhow::anyhow!("{detail}")));
            }
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
            pending::Outcome::Panicked(panic) => std::panic::resume_unwind(panic),
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
    fn release(&mut self) -> Result<()> {
        if let Some(mut waiter) = self.pending.take() {
            waiter.stop.cancel();
            let (admission, outcome) = waiter.join();
            self.admission = Some(admission);
            drop(outcome);
        }
        let retired = if let Some(Held { mut permit, .. }) = self.held.take() {
            permit.retire()
        } else {
            Ok(())
        };
        if let Some((sequence, kind)) = self.attempted.take() {
            if let Some(admission) = self.admission.as_mut() {
                admission.release(sequence, kind);
            }
        }
        retired
    }
}
impl<A: Admission> Drop for State<A> {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// Field order is intentional: process kill/reap and both pipe joins precede
/// State's parent permit release, including unwinding and malformed output.
struct Owned<A: Admission> {
    process: Process,
    broker: Option<Broker>,
    state: State<A>,
}
impl<A: Admission> Drop for Owned<A> {
    fn drop(&mut self) {
        // Queue-bypass cancellation is attempted even if admission failed or
        // the caller unwinds. A blocked/full pipe cannot delay kill/reap.
        let _ = self.process.try_send_control(ParentFrame::Cancel {
            guard: self.state.guard.clone(),
        });
        if let Some(waiter) = &self.state.pending {
            waiter.stop.cancel();
        }
        self.process.revoke();
        if let Some(broker) = &self.broker {
            broker.revoke_after_lm();
            broker.wait_revoked();
        }
        let _ = self.process.drain_checked();
        if let Some(broker) = &mut self.broker {
            let _ = broker.finish();
        }
    }
}
fn send(process: &Process, mut frame: ParentFrame, stop: &Stop, until: Instant) -> Result<()> {
    loop {
        ensure!(!stop.requested(), "migration operation canceled");
        ensure!(Instant::now() < until, "migration operation deadline");
        match process.try_send(frame)? {
            None => return Ok(()),
            Some(unsent) => frame = unsent,
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Exact result bytes stay charged while cached by the coordinator. The private
/// wrapper provides borrowed access; moving out a naked String would lose that
/// lifetime, so no such conversion is exposed.
pub(crate) struct SavedResult {
    text: String,
    memory: Reservation,
}
impl SavedResult {
    pub(crate) fn text(&self) -> &str {
        &self.text
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
    execute_with_broker(
        |stop| Process::spawn(executable, stop),
        Some(executable),
        guard,
        request,
        stop,
        until,
        admission,
        budget,
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
    execute_with_broker(spawn, None, guard, request, stop, until, admission, budget)
}
fn execute_with_broker<A: Admission>(
    spawn: impl FnOnce(Arc<Stop>) -> Result<Process>,
    source_executable: Option<&Path>,
    guard: Guard,
    request: &str,
    stop: Arc<Stop>,
    until: Instant,
    admission: A,
    budget: MemoryBudget,
) -> Result<SavedResult> {
    guard.validate()?;
    ensure!(
        request.len() <= INPUT_BYTES,
        "migration request byte admission"
    );
    ensure!(
        !stop.requested() && Instant::now() < until,
        "migration admission canceled/deadline"
    );
    // Result storage is allocated below and stays owned across lost replies.
    // Full typed/core phase admission is added by the coordinator separately.
    let mut memory = budget.reservation();
    memory.grow(INPUT_BYTES)?;
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
    let mut result_memory = budget.reservation();
    result_memory.grow(RESULT_BYTES)?;
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
    let mut owner = Owned {
        process: spawn(stop.clone())?,
        broker,
        state: State::new(admission, guard.clone(), input_digest.clone(), memory),
    };
    send(
        &owner.process,
        ParentFrame::Begin {
            guard: guard.clone(),
            request_blake3: input_digest.clone(),
            bytes: U64(request.len() as u64),
        },
        &stop,
        until,
    )?;
    let mut offset = 0;
    while offset < request.len() {
        let mut end = (offset + TEXT_CHUNK).min(request.len());
        while !request.is_char_boundary(end) {
            end -= 1;
        }
        send(
            &owner.process,
            ParentFrame::Input {
                guard: guard.clone(),
                offset: U64(offset as u64),
                text: request[offset..end].into(),
            },
            &stop,
            until,
        )?;
        offset = end;
    }
    send(
        &owner.process,
        ParentFrame::FinishInput {
            guard,
            blake3: input_digest,
        },
        &stop,
        until,
    )?;
    let mut ended = false;
    let mut command = None;
    let mut data = None;
    let mut control = None;
    loop {
        // Reserved failure delivery bypasses both actor admission and the normal
        // data queue. If the executor cannot receive it, revocation kills it;
        // successful commits preceding observed loss retain their exact receipt.
        if let Some(broker) = &owner.broker {
            if let Some(event) = broker.try_urgent() {
                let frame = ParentFrame::Source {
                    guard: owner.state.guard.clone(),
                    event,
                };
                if let Some(unsent) = owner.process.try_send_control(frame)? {
                    control = Some(unsent);
                }
            }
        }
        ensure!(
            !stop.requested(),
            "migration operation canceled; all helpers drained before return"
        );
        ensure!(
            Instant::now() < until,
            "migration operation deadline; all helpers drained before return"
        );
        if let Some(frame) = control.take() {
            control = owner.process.try_send_control(frame)?;
        }
        if control.is_none() {
            control = owner.state.poll_admission()?;
        }
        if let Some(broker) = &owner.broker {
            if let Some(next) = command.take() {
                command = broker.try_send(next)?;
            }
            if data.is_none() {
                data = broker.try_receive()?.map(|event| ParentFrame::Source {
                    guard: owner.state.guard.clone(),
                    event,
                });
            }
        }
        if let Some(frame) = data.take() {
            data = owner.process.try_send(frame)?;
        }
        if !ended && command.is_none() && control.is_none() {
            match owner.process.try_receive()? {
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
                    command = owner
                        .broker
                        .as_ref()
                        .context("test executor has no configured Source broker")?
                        .try_send(next)?;
                }
                Output::Frame(frame) => {
                    control = owner.state.accept(frame, &stop, until)?;
                }
                Output::End => ended = true,
            }
        }
        if ended && let Some(status) = owner.process.try_reap()? {
            ensure!(status.success(), "migration helper exited {status}");
            owner
                .state
                .terminal
                .take()
                .context("migration helper ended without a terminal result")??;
            ensure!(
                command.is_none() && data.is_none() && control.is_none(),
                "migration helper ended with pending relay/control output"
            );
            if let Some(broker) = &owner.broker {
                broker.ensure_idle()?;
            }
            // Signal every actual child before waiting for any one owner's pipes.
            owner.process.revoke();
            if let Some(broker) = &owner.broker {
                broker.revoke_after_lm();
                broker.wait_revoked();
            }
            owner.process.drain_checked()?;
            if let Some(broker) = &mut owner.broker {
                broker.finish()?;
            }
            return Ok(SavedResult {
                text: std::mem::take(&mut owner.state.result),
                memory: result_memory,
            });
        }
        thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests;
