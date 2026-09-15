//! G owns every actual Source handle. This state is driven only on the independent
//! broker thread; the foreground actor and LM executor never join these children.
use super::super::transport::{Epoch, Reply, Request};
use super::{Assembly, COUNT, Command, Event, Kind, Outgoing};
use crate::{
    application::U64,
    lightroom_migration_worker::{
        memory::{MemoryBudget, Reservation},
        process::{Output, Process, Stop},
        protocol::Guard,
    },
};
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, Mutex};

pub(super) type Charge = Arc<Mutex<Reservation>>;
pub(super) type Charges = Arc<Mutex<[Option<Charge>; COUNT]>>;
pub(super) type Spawn<'a> =
    dyn FnMut(Kind, Arc<Stop>, &mut dyn FnMut()) -> Result<Process<Reply>> + 'a;

struct Slot {
    process: Process<Reply>,
    stop: Arc<Stop>,
    epoch: Epoch,
    token: String,
    memory: Charge,
    next_memory: u64,
    next_input: u64,
    next_output: u64,
    assembly: Option<Assembly>,
    pending: Option<Request>,
    pending_offset: usize,
    outgoing: Option<Outgoing>,
    waiting_consumed: Option<u64>,
    retiring: bool,
    retired_reply: bool,
    closing: bool,
    eof: bool,
}
struct Retired {
    token: String,
    // Physical Source/pipe owners are gone, but LM may still retain epoch
    // payloads. Only exact worker quiescence or whole broker drain retires this.
    _memory: Charge,
}
pub(crate) struct Owner {
    guard: Guard,
    slots: [Option<Slot>; COUNT],
    retired: [Option<Retired>; COUNT],
    memory: MemoryBudget,
    charges: Charges,
    next_start: u64,
    cursor: usize,
}
impl Owner {
    pub(crate) fn new(guard: Guard, memory: MemoryBudget, charges: Charges) -> Result<Self> {
        guard.validate()?;
        Ok(Self {
            guard,
            slots: std::array::from_fn(|_| None),
            retired: std::array::from_fn(|_| None),
            memory,
            charges,
            next_start: 1,
            cursor: 0,
        })
    }
    pub(super) fn accept(
        &mut self,
        command: Command,
        spawn: &mut Spawn<'_>,
    ) -> Result<Option<Event>> {
        Ok(match command {
            Command::Start {
                sequence,
                kind,
                reader,
            } => {
                ensure!(
                    sequence.0 == self.next_start
                        && self.slots[kind.index()].is_none()
                        && self.retired[kind.index()].is_none(),
                    "Source relay slot/start replay"
                );
                let next = self
                    .next_start
                    .checked_add(1)
                    .context("Source start sequence exhausted")?;
                let epoch = Epoch {
                    guard: self.guard.clone(),
                    reader,
                };
                epoch.validate()?;
                let token = crate::lightroom::digest(&crate::lightroom::bounded_json(
                    &(&epoch, sequence, kind),
                    4096,
                )?);
                let stop = Arc::new(Stop::default());
                let memory = Arc::new(Mutex::new(self.memory.reservation()));
                // G retains a second owner outside this broker thread. Even an
                // abrupt thread/LM loss cannot retire charges before G drains LM.
                self.charges.lock().unwrap_or_else(|e| e.into_inner())[kind.index()] =
                    Some(memory.clone());
                let process = spawn(kind, stop.clone(), &mut || self.revoke_all())?;
                // Own the child and every pipe before constructing/publishing its
                // acknowledgement. A lost Started reply cannot orphan it.
                self.slots[kind.index()] = Some(Slot {
                    process,
                    stop,
                    epoch,
                    token: token.clone(),
                    memory,
                    next_memory: 1,
                    next_input: 1,
                    next_output: 1,
                    assembly: None,
                    pending: None,
                    pending_offset: 0,
                    outgoing: None,
                    waiting_consumed: None,
                    retiring: false,
                    retired_reply: false,
                    closing: false,
                    eof: false,
                });
                self.next_start = next;
                Some(Event::Started { sequence, token })
            }
            Command::Reserve {
                token,
                sequence,
                bytes,
            } => {
                let slot = self.slot(&token)?;
                ensure!(
                    !slot.closing && !slot.retiring && sequence.0 == slot.next_memory,
                    "Source allocation scope/sequence differs"
                );
                let next = slot
                    .next_memory
                    .checked_add(1)
                    .context("Source allocation sequence exhausted")?;
                slot.memory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .grow(usize::try_from(bytes.0)?)?;
                slot.next_memory = next;
                Some(Event::Reserved {
                    token,
                    sequence,
                    bytes,
                })
            }
            Command::Quiesced { token } => {
                let index = self
                    .retired
                    .iter()
                    .position(|r| r.as_ref().is_some_and(|r| r.token == token))
                    .context("Source quiescence precedes drain or has stale scope")?;
                // Drained was emitted only after checked Child wait and both
                // pipe joins. This ordered worker acknowledgement is the second
                // condition; pipe/relay framing remains operation-scoped.
                self.retired[index].take();
                let charge = self.charges.lock().unwrap_or_else(|e| e.into_inner())[index].take();
                drop(charge);
                Some(Event::Quiesced { token })
            }
            Command::Input {
                token,
                frame,
                offset,
                total,
                bytes,
            } => {
                let slot = self.slot(&token)?;
                ensure!(
                    !slot.closing && slot.pending.is_none() && frame.0 == slot.next_input,
                    "Source relay input state/replay"
                );
                if let Some(assembly) = slot.assembly.as_mut() {
                    assembly.push(frame.0, offset.0, total.0, &bytes)?;
                } else {
                    slot.assembly = Some(Assembly::start(frame.0, total.0, offset.0, &bytes)?);
                }
                let assembly = slot.assembly.as_ref().expect("retained Source assembly");
                let accepted_offset = assembly.offset();
                if assembly.complete() {
                    let encoded = slot
                        .assembly
                        .take()
                        .expect("complete Source assembly")
                        .finish()?;
                    let mut framed = std::io::Cursor::new(&encoded);
                    let request: Request =
                        crate::lightroom_migration_worker::protocol::read_frame(&mut framed)?;
                    ensure!(
                        framed.position() == encoded.len() as u64,
                        "Source relay trailing frame bytes"
                    );
                    ensure!(
                        request.epoch() == &slot.epoch,
                        "Source relay request epoch differs"
                    );
                    slot.retiring |= matches!(request, Request::Retire { .. });
                    slot.pending_offset = accepted_offset;
                    slot.pending = Some(request);
                    // Complete-frame acknowledgement waits until Source's own
                    // bounded input queue accepts the exact retained request.
                    None
                } else {
                    Some(Event::Accepted {
                        token,
                        frame,
                        offset: U64(accepted_offset.try_into()?),
                    })
                }
            }
            Command::Consumed { token, frame } => {
                let slot = self.slot(&token)?;
                ensure!(
                    slot.waiting_consumed == Some(frame.0),
                    "Source relay output consumption replay"
                );
                slot.waiting_consumed = None;
                None
            }
            Command::Drain { token } => {
                let slot = self.slot(&token)?;
                ensure!(!slot.closing, "Source relay repeated drain");
                slot.closing = true;
                slot.stop.cancel();
                slot.process.revoke();
                None
            }
        })
    }
    fn slot(&mut self, token: &str) -> Result<&mut Slot> {
        self.slots
            .iter_mut()
            .flatten()
            .find(|s| s.token == token)
            .context("Source relay token is stale or foreign")
    }
    /// Poll fairly without waiting for any child. Returned events are retained
    /// by the broker until its bounded output queue accepts them.
    pub(crate) fn poll(&mut self) -> Result<Option<Event>> {
        for _ in 0..COUNT {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % COUNT;
            let Some(slot) = self.slots[index].as_mut() else {
                continue;
            };
            if slot.closing || slot.eof {
                if let Some(status) = slot.process.try_reap()? {
                    slot.process.drain_checked()?;
                    ensure!(
                        slot.closing || status.success(),
                        "Source exited unsuccessfully after retirement"
                    );
                    let token = slot.token.clone();
                    let slot = self.slots[index].take().expect("drained Source slot");
                    self.retired[index] = Some(Retired {
                        token: token.clone(),
                        _memory: slot.memory,
                    });
                    // All remaining slot payloads drop before Drained can leave
                    // this method. Parent pipe/framing allowances are separate.
                    return Ok(Some(Event::Drained { token }));
                }
                continue;
            }
            if let Some(request) = slot.pending.take() {
                match slot.process.try_send(request)? {
                    None => {
                        let frame = slot.next_input;
                        slot.next_input = frame
                            .checked_add(1)
                            .context("Source relay input sequence exhausted")?;
                        return Ok(Some(Event::Accepted {
                            token: slot.token.clone(),
                            frame: U64(frame),
                            offset: U64(slot.pending_offset.try_into()?),
                        }));
                    }
                    Some(request) => slot.pending = Some(request),
                }
            }
            if slot.outgoing.is_none() && slot.waiting_consumed.is_none() {
                match slot.process.try_receive()? {
                    Output::Pending => {}
                    Output::End => {
                        ensure!(
                            slot.retired_reply,
                            "Source process lost before retirement acknowledgement"
                        );
                        slot.eof = true;
                        continue;
                    }
                    Output::Frame(reply) => {
                        ensure!(
                            reply.epoch() == &slot.epoch,
                            "Source relay reply epoch differs"
                        );
                        if let Reply::Failed { detail, .. } = &reply {
                            anyhow::bail!("Source owner failed: {detail}");
                        }
                        if matches!(reply, Reply::Retired { .. }) {
                            ensure!(
                                slot.retiring,
                                "unsolicited Source retirement acknowledgement"
                            );
                            slot.retired_reply = true;
                        }
                        let mut encoded = Vec::new();
                        crate::lightroom_migration_worker::protocol::write_frame(
                            &mut encoded,
                            &reply,
                        )?;
                        slot.outgoing = Some(Outgoing::new(slot.next_output, encoded)?);
                    }
                }
            }
            if let Some(output) = &mut slot.outgoing {
                let offset = output.offset();
                let event = Event::Output {
                    token: slot.token.clone(),
                    frame: U64(output.frame),
                    offset: U64(offset.try_into()?),
                    total: U64(output.total().try_into()?),
                    bytes: output.chunk().to_vec(),
                };
                // The broker owns this event before a fallible queue send and
                // cannot call poll again until that send succeeds.
                if output.accepted(offset.try_into()?)? {
                    slot.waiting_consumed = Some(output.frame);
                    slot.next_output = slot
                        .next_output
                        .checked_add(1)
                        .context("Source relay output sequence exhausted")?;
                    slot.outgoing.take();
                }
                return Ok(Some(event));
            }
            // The I/O reader may be backpressured behind a complete reply; actual
            // process death must still be observed independently of consumption.
            if slot.process.try_reap()?.is_some() {
                ensure!(slot.retiring, "Source process lost with queued output");
            }
        }
        Ok(None)
    }
    pub(crate) fn check_liveness(&mut self) -> Result<()> {
        for slot in self.slots.iter_mut().flatten() {
            ensure!(
                !slot.process.transport_failed(),
                "Source transport failed with queued relay output"
            );
            ensure!(
                !slot.process.output_ended() || slot.closing || slot.retiring,
                "Source output ended before retirement"
            );
            if slot.process.try_reap()?.is_some() {
                ensure!(
                    slot.closing || slot.retiring,
                    "Source process lost with pending relay output"
                );
            }
        }
        Ok(())
    }
    pub(crate) fn live(&self) -> usize {
        self.slots.iter().flatten().count() + self.retired.iter().flatten().count()
    }
    pub(crate) fn failures(&self, detail: &str) -> [Option<Event>; COUNT] {
        std::array::from_fn(|index| {
            self.slots[index]
                .as_ref()
                .map(|slot| &slot.token)
                .or_else(|| self.retired[index].as_ref().map(|slot| &slot.token))
                .map(|token| Event::Failed {
                    token: token.clone(),
                    detail: detail.to_owned(),
                })
        })
    }
    pub(crate) fn revoke_all(&mut self) {
        for slot in self.slots.iter_mut().flatten() {
            slot.closing = true;
            slot.stop.cancel();
            slot.process.revoke();
        }
    }
    pub(crate) fn drain_all(&mut self) -> Result<()> {
        // Signal BOTH children before the first retrying wait/pipe join.
        self.revoke_all();
        let mut first = None;
        loop {
            let mut complete = true;
            for slot in self.slots.iter_mut().flatten() {
                match slot.process.retry_drain() {
                    Ok(Some(report)) => {
                        if report.io_panicked {
                            first.get_or_insert_with(|| {
                                anyhow::anyhow!("Source I/O thread panicked during owned drain")
                            });
                        }
                        if !report.status.success() && !slot.closing {
                            first.get_or_insert_with(|| {
                                anyhow::anyhow!("Source helper exited {}", report.status)
                            });
                        }
                    }
                    Ok(None) => complete = false,
                    Err(error) => {
                        first.get_or_insert(error);
                        complete = false;
                    }
                }
            }
            if complete {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.slots = [None, None];
        if let Some(error) = first {
            return Err(error);
        }
        Ok(())
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.drain_all();
    }
}
