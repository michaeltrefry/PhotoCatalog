//! Parent-side protocol and writer ownership. This runs on the owned supervisor,
//! never the catalog actor. There is no destination/source filesystem access.
use super::{
    identity::FileKey,
    input::{INPUT_BYTES, TEXT_CHUNK, digest},
    process::{Output, Process, Stop},
    protocol::{ChildFrame, DestinationPin, Guard, ParentFrame, WriteKind},
};
use crate::{
    application::U64,
    catalog_writer::{Permit, Priority, Writers},
};
use anyhow::{Context, Result, ensure};
use std::{
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const RESULT_BYTES: usize = 8 * 1024 * 1024;

/// The coordinator implements this against immutable operation admission and
/// the actor's cached attached identities. `writer` obtains an exact actor hold
/// acknowledgement before returning its shared Writers registry entry. Its
/// wait must observe Stop/deadline; it must not open supplied pathnames.
pub(crate) trait Admission {
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
    permit: Permit,
}
struct State<A: Admission> {
    admission: A,
    guard: Guard,
    input_digest: String,
    admitted: bool,
    lock: Option<(String, FileKey)>,
    next_write: u64,
    held: Option<Held>,
    result: String,
    terminal: Option<Result<()>>,
}
impl<A: Admission> State<A> {
    fn new(admission: A, guard: Guard, input_digest: String) -> Self {
        Self {
            admission,
            guard,
            input_digest,
            admitted: false,
            lock: None,
            next_write: 1,
            held: None,
            result: String::with_capacity(RESULT_BYTES),
            terminal: None,
        }
    }
    fn accept(
        &mut self,
        frame: ChildFrame,
        stop: &Stop,
        until: Instant,
    ) -> Result<Option<ParentFrame>> {
        ensure!(
            self.terminal.is_none(),
            "helper frame after terminal result"
        );
        let guard = match &frame {
            ChildFrame::Admitted { guard, .. }
            | ChildFrame::LockAcquired { guard, .. }
            | ChildFrame::NeedWrite { guard, .. }
            | ChildFrame::ReleaseWrite { guard, .. }
            | ChildFrame::Progress { guard, .. }
            | ChildFrame::Result { guard, .. }
            | ChildFrame::Finished { guard, .. }
            | ChildFrame::Failed { guard, .. } => guard,
        };
        ensure!(guard == &self.guard, "stale migration helper frame");
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
                self.admission.lock(&target_token, &destination, &lock)?;
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
                    sequence.0 == self.next_write && self.held.is_none(),
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
                let writers = self.admission.writer(
                    sequence.0,
                    write,
                    &target_token,
                    lock.as_ref(),
                    stop,
                    until,
                )?;
                let permit = match writers.enter_cancellable(
                    Priority::Background,
                    stop.admission(),
                    Some(until),
                ) {
                    Ok(permit) => permit,
                    Err(error) => {
                        self.admission.release(sequence.0, write);
                        return Err(error);
                    }
                };
                // Retain before enqueueing Grant. Lost/blocked output is an
                // uncertain grant, resolved only by exact release or actual reap.
                self.held = Some(Held {
                    sequence: sequence.0,
                    kind: write,
                    permit,
                });
                self.next_write = self
                    .next_write
                    .checked_add(1)
                    .context("migration grant sequence exhausted")?;
                return Ok(Some(ParentFrame::Grant {
                    guard: self.guard.clone(),
                    sequence,
                    write,
                }));
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
                self.release();
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
                    .progress(&phase, completed.0, total.map(|n| n.0))?;
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
                    self.held.is_none(),
                    "helper finished with an unreleased writer"
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
    fn release(&mut self) {
        if let Some(Held {
            sequence,
            kind,
            permit,
        }) = self.held.take()
        {
            drop(permit);
            self.admission.release(sequence, kind);
        }
    }
}
impl<A: Admission> Drop for State<A> {
    fn drop(&mut self) {
        self.release();
    }
}

/// Field order is intentional: process kill/reap and both pipe joins precede
/// State's parent permit release, including unwinding and malformed output.
struct Owned<A: Admission> {
    process: Process,
    state: State<A>,
}
impl<A: Admission> Drop for Owned<A> {
    fn drop(&mut self) {
        // Queue-bypass cancellation is attempted even if admission failed or
        // the caller unwinds. A blocked/full pipe cannot delay kill/reap.
        let _ = self.process.try_send(ParentFrame::Cancel {
            guard: self.state.guard.clone(),
        });
        self.process.terminate();
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

/// Owns only bounded UTF-8 authority/result memory plus bounded pipe frames.
/// Large saved evidence is returned by the paged/chunk query layer; this limit
/// never truncates results or changes a caller's reviewed migration scope.
pub(crate) fn execute<A: Admission>(
    executable: &Path,
    guard: Guard,
    request: &str,
    stop: Arc<Stop>,
    until: Instant,
    admission: A,
) -> Result<String> {
    guard.validate()?;
    ensure!(
        request.len() <= INPUT_BYTES,
        "migration request byte admission"
    );
    ensure!(
        !stop.requested() && Instant::now() < until,
        "migration admission canceled/deadline"
    );
    let input_digest = blake3::hash(request.as_bytes()).to_hex().to_string();
    let mut owner = Owned {
        process: Process::spawn(executable, stop.clone())?,
        state: State::new(admission, guard.clone(), input_digest.clone()),
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
    loop {
        ensure!(
            !stop.requested(),
            "migration operation canceled; helper drained before return"
        );
        ensure!(
            Instant::now() < until,
            "migration operation deadline; helper drained before return"
        );
        if !ended {
            match owner.process.try_receive()? {
                Output::Pending => {}
                Output::Frame(frame) => {
                    if let Some(grant) = owner.state.accept(frame, &stop, until)? {
                        send(&owner.process, grant, &stop, until)?;
                    }
                    continue;
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
            // Drop/join transport threads before returning the cached result.
            owner.process.terminate();
            return Ok(std::mem::take(&mut owner.state.result));
        }
        ensure!(
            !stop.requested(),
            "migration operation canceled; helper drained before return"
        );
        ensure!(
            Instant::now() < until,
            "migration operation deadline; helper drained before return"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests;
