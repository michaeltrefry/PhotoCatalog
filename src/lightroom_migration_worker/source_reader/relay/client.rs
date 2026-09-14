//! LM token proxies. The independent LM control listener updates epoch health
//! without waiting for the worker's result consumer, writer permit, or data queue.
use super::super::transport::{Epoch, Reply, Request, digest_valid};
use super::{Assembly, Command, Event, Kind, Outgoing};
use crate::{
    application::U64,
    lightroom_migration_worker::{
        process::{Output, Stop},
        protocol::{ChildFrame, Guard, Publish, SourceListener},
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Sending {
    outgoing: Outgoing,
    digest: String,
    awaiting: Option<(usize, usize)>,
    complete: bool,
}
struct State {
    token: Option<String>,
    sending: Option<Sending>,
    next_input: u64,
    next_output: u64,
    incoming: Option<Assembly>,
    reply: Option<(u64, Reply)>,
    retired_reply: bool,
    closing: bool,
    closed: bool,
    error: Option<String>,
}
struct Slot {
    sequence: u64,
    kind: Kind,
    epoch: Epoch,
    stop: Arc<Stop>,
    state: Mutex<State>,
}
pub(crate) struct Client {
    guard: Guard,
    output: Arc<dyn Publish>,
    slots: Mutex<[Option<Arc<Slot>>; 2]>,
    next: AtomicU64,
    revoked: AtomicBool,
    abort: Arc<dyn Fn() + Send + Sync>,
}
impl Client {
    pub(crate) fn new(
        guard: Guard,
        output: Arc<dyn Publish>,
        abort: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Arc<Self>> {
        guard.validate()?;
        Ok(Arc::new(Self {
            guard,
            output,
            slots: Mutex::new([None, None]),
            next: AtomicU64::new(1),
            revoked: AtomicBool::new(false),
            abort,
        }))
    }
    fn check(&self) -> Result<()> {
        ensure!(
            !self.revoked.load(Ordering::Acquire),
            "managed Source parent ownership revoked"
        );
        Ok(())
    }
    fn publish(&self, command: Command) -> Result<()> {
        self.check()?;
        let result = self.output.publish(&ChildFrame::Source {
            guard: self.guard.clone(),
            command,
        });
        if result.is_err() {
            self.fail();
        }
        result
    }
    fn fail(&self) {
        self.revoke();
        (self.abort)();
    }
    pub(crate) fn open(
        self: &Arc<Self>,
        kind: Kind,
        epoch: Epoch,
        stop: Arc<Stop>,
        until: Instant,
    ) -> Result<Remote> {
        self.check()?;
        ensure!(epoch.guard == self.guard, "managed Source guard mismatch");
        epoch.validate()?;
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            slots[kind.index()].is_none(),
            "managed Source slot still owned"
        );
        let sequence = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| anyhow::anyhow!("Source relay start sequence exhausted"))?;
        let slot = Arc::new(Slot {
            sequence,
            kind,
            epoch,
            stop,
            state: Mutex::new(State {
                token: None,
                sending: None,
                next_input: 1,
                next_output: 1,
                incoming: None,
                reply: None,
                retired_reply: false,
                closing: false,
                closed: false,
                error: None,
            }),
        });
        slots[kind.index()] = Some(slot.clone());
        drop(slots);
        // The proxy/slot exists before Start can create an actual G-owned child.
        let owner = Remote {
            client: self.clone(),
            slot,
        };
        self.publish(Command::Start {
            sequence: U64(sequence),
            kind,
            reader: owner.slot.epoch.reader.clone(),
        })?;
        loop {
            self.check()?;
            if owner
                .slot
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .token
                .is_some()
            {
                return Ok(owner);
            }
            ensure!(
                Instant::now() < until,
                "managed Source start acknowledgement deadline"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn token_slot(&self, token: &str) -> Result<Arc<Slot>> {
        ensure!(digest_valid(token), "Source relay token encoding");
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flatten()
            .find(|slot| {
                slot.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .token
                    .as_deref()
                    == Some(token)
            })
            .cloned()
            .context("Source relay event token is stale")
    }
}
impl SourceListener for Client {
    fn accept(&self, event: Event) -> Result<()> {
        // No waits, filesystem operations, publishes or joins occur here.
        self.check()?;
        let mut failed = false;
        match event {
            Event::Started { sequence, token } => {
                ensure!(digest_valid(&token), "Source relay token encoding");
                let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
                ensure!(
                    slots.iter().flatten().all(|slot| {
                        slot.state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .token
                            .as_deref()
                            != Some(token.as_str())
                    }),
                    "Source start token already owned"
                );
                let slot = slots
                    .iter()
                    .flatten()
                    .find(|s| s.sequence == sequence.0)
                    .cloned()
                    .context("Source start acknowledgement replay")?;
                let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
                ensure!(
                    state.token.is_none(),
                    "duplicate Source start acknowledgement"
                );
                state.token = Some(token);
            }
            Event::Accepted {
                token,
                frame,
                offset,
            } => {
                let slot = self.token_slot(&token)?;
                let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
                let send = state
                    .sending
                    .as_mut()
                    .context("unsolicited Source input acknowledgement")?;
                let (start, end) = send
                    .awaiting
                    .context("repeated Source input acknowledgement")?;
                ensure!(
                    frame.0 == send.outgoing.frame && offset.0 == end as u64,
                    "Source input acknowledgement differs"
                );
                send.complete = send.outgoing.accepted(start as u64)?;
                send.awaiting = None;
            }
            Event::Output {
                token,
                frame,
                offset,
                total,
                bytes,
            } => {
                let slot = self.token_slot(&token)?;
                let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
                ensure!(
                    !state.closed && state.reply.is_none() && frame.0 == state.next_output,
                    "Source output state/sequence"
                );
                if let Some(assembly) = &mut state.incoming {
                    assembly.push(frame.0, offset.0, total.0, &bytes)?;
                } else {
                    state.incoming = Some(Assembly::start(frame.0, total.0, offset.0, &bytes)?);
                }
                if state.incoming.as_ref().is_some_and(Assembly::complete) {
                    let encoded = state
                        .incoming
                        .take()
                        .expect("complete Source output")
                        .finish()?;
                    let mut framed = std::io::Cursor::new(&encoded);
                    let reply: Reply =
                        crate::lightroom_migration_worker::protocol::read_frame(&mut framed)?;
                    ensure!(
                        framed.position() == encoded.len() as u64,
                        "Source relay trailing output frame bytes"
                    );
                    ensure!(
                        reply.epoch() == &slot.epoch,
                        "managed Source reply epoch mismatch"
                    );
                    if matches!(reply, Reply::Retired { .. }) {
                        state.retired_reply = true;
                    }
                    if matches!(reply, Reply::Failed { .. }) {
                        slot.stop.cancel();
                        failed = true;
                    }
                    state.reply = Some((frame.0, reply));
                    state.next_output = state
                        .next_output
                        .checked_add(1)
                        .context("Source output sequence exhausted")?;
                }
            }
            Event::Failed { token, detail } => {
                ensure!(detail.len() <= 4096, "Source relay error byte bound");
                let slot = self.token_slot(&token)?;
                slot.stop.cancel();
                slot.state.lock().unwrap_or_else(|e| e.into_inner()).error = Some(detail);
                failed = true;
            }
            Event::Drained { token } => {
                let slot = self.token_slot(&token)?;
                let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
                ensure!(!state.closed, "repeated Source drain acknowledgement");
                if !state.closing && !state.retired_reply {
                    slot.stop.cancel();
                    failed = true;
                }
                state.closed = true;
            }
        }
        if failed {
            self.fail();
        }
        Ok(())
    }
    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        for slot in self
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flatten()
        {
            slot.stop.cancel();
        }
    }
}
pub(crate) struct Remote {
    client: Arc<Client>,
    slot: Arc<Slot>,
}
impl Remote {
    pub(crate) fn try_send(&self, request: Request) -> Result<Option<Request>> {
        self.client.check()?;
        ensure!(
            request.epoch() == &self.slot.epoch,
            "managed Source request epoch mismatch"
        );
        let mut encoded = Vec::new();
        crate::lightroom_migration_worker::protocol::write_frame(&mut encoded, &request)?;
        let digest = crate::lightroom::digest(&encoded);
        let command = {
            let mut state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                !state.closed && state.error.is_none(),
                "managed Source input unavailable"
            );
            let token = state.token.clone().context("managed Source not started")?;
            if state.sending.is_none() {
                state.sending = Some(Sending {
                    outgoing: Outgoing::new(state.next_input, encoded)?,
                    digest: digest.clone(),
                    awaiting: None,
                    complete: false,
                });
            }
            let send = state.sending.as_mut().expect("owned Source input");
            ensure!(
                send.digest == digest,
                "Source retry changed its pending frame"
            );
            if send.complete {
                state.sending.take();
                state.next_input = state
                    .next_input
                    .checked_add(1)
                    .context("Source input sequence exhausted")?;
                return Ok(None);
            }
            if send.awaiting.is_some() {
                return Ok(Some(request));
            }
            let start = send.outgoing.offset();
            let bytes = send.outgoing.chunk().to_vec();
            let end = start
                .checked_add(bytes.len())
                .context("Source input offset overflow")?;
            send.awaiting = Some((start, end));
            Command::Input {
                token,
                frame: U64(send.outgoing.frame),
                offset: U64(start as u64),
                total: U64(send.outgoing.total() as u64),
                bytes,
            }
        };
        self.client.publish(command)?;
        Ok(Some(request))
    }
    pub(crate) fn try_receive(&self) -> Result<Output<Reply>> {
        self.client.check()?;
        let (token, reply, closed) = {
            let mut state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(error) = &state.error {
                anyhow::bail!("managed Source failed: {error}");
            }
            (
                state.token.clone().context("managed Source not started")?,
                state.reply.take(),
                state.closed,
            )
        };
        if let Some((frame, reply)) = reply {
            self.client.publish(Command::Consumed {
                token,
                frame: U64(frame),
            })?;
            return Ok(Output::Frame(reply));
        }
        Ok(if closed { Output::End } else { Output::Pending })
    }
    pub(crate) fn reaped(&self) -> Result<bool> {
        self.client.check()?;
        Ok(self
            .slot
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed)
    }
    pub(crate) fn terminate(&self) -> Result<()> {
        let until = Instant::now() + Duration::from_secs(5);
        let token = {
            let mut state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return Ok(());
            }
            if state.closing {
                None
            } else {
                state.closing = true;
                state.token.clone()
            }
        };
        if let Some(token) = token {
            self.client.publish(Command::Drain { token })?;
        }
        loop {
            self.client.check()?;
            if self
                .slot
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed
            {
                return Ok(());
            }
            if Instant::now() >= until {
                self.client.fail();
                anyhow::bail!(
                    "managed Source drain unconfirmed; parent retains actual process ownership"
                );
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        if self.terminate().is_err() {
            self.client.fail();
        }
        let mut slots = self.client.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots[self.slot.kind.index()]
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &self.slot))
        {
            slots[self.slot.kind.index()].take();
        }
    }
}

#[cfg(test)]
mod tests;
