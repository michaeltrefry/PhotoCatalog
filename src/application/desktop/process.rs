use super::*;
use std::{
    io::{Read, Write},
    process::{Child, Command, Stdio},
    time::Duration,
};
use wire::{Assembly, BinaryHeader, Frame, Seen};

type IoThreads = Vec<thread::JoinHandle<()>>;
type DrainResult = (Option<Child>, IoThreads);
/// Remove the owned process only after an affirmative OS wait result.
fn wait_owned<T>(slot: &mut Option<T>, wait: impl FnOnce(&mut T) -> std::io::Result<std::process::ExitStatus>) -> std::io::Result<std::process::ExitStatus> {
    let status = wait(slot.as_mut().expect("owned process"))?;
    slot.take();
    Ok(status)
}
pub(super) struct Owner {
    child: Option<Child>,
    threads: IoThreads,
    supervisor: Option<thread::JoinHandle<DrainResult>>,
    shared: Arc<Shared>,
    pid: u32,
}
fn finish_wait(shared: &Shared, status: std::process::ExitStatus, threads: IoThreads) {
    if !status.success() {
        shared.fail(format!(
            "desktop process exited {status}; unacknowledged outcomes unknown"
        ));
    }
    shared.state.lock().unwrap().reaped = true;
    shared.wake.notify_all();
    for thread in threads {
        if thread.join().is_err() {
            shared.fail("desktop transport thread panicked");
        }
    }
    shared.complete_failure();
    shared.state.lock().unwrap().phase = TransportPhase::Closed;
    shared.wake.notify_all();
}
impl Owner {
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn spawn(
        executable: &std::path::Path,
        shared: Arc<Shared>,
        hello: Vec<u8>,
    ) -> anyhow::Result<Self> {
        let mut child = Command::new(executable)
            .arg("--catalog-desktop-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let control = child.stderr.take().unwrap();
        let pid = child.id();
        let mut owner = Self {
            child: Some(child),
            threads: vec![],
            supervisor: None,
            shared: shared.clone(),
            pid,
        };
        let result =
            (|| -> std::io::Result<()> {
                let state = shared.clone();
                owner
                    .threads
                    .push(thread::Builder::new().name("desktop-input".into()).spawn(
                        move || {
                            if let Err(e) = parent_write(input, &state, hello) {
                                state.fail(format!("desktop input: {e}"));
                            }
                        },
                    )?);
                let state = shared.clone();
                owner.threads.push(
                    thread::Builder::new()
                        .name("desktop-control".into())
                        .spawn(move || {
                            if let Err(e) = parent_control(control, &state) {
                                state.fail(format!("desktop control: {e}"));
                            }
                        })?,
                );
                let state = shared.clone();
                owner
                    .threads
                    .push(thread::Builder::new().name("desktop-binary".into()).spawn(
                        move || {
                            if let Err(e) = parent_binary(output, &state) {
                                state.fail(format!("desktop binary: {e}"));
                            }
                        },
                    )?);
                Ok(())
            })();
        if let Err(e) = result {
            owner.shared.stop();
            let _ = owner.drain();
            return Err(e.into());
        }
        // Put ownership in a shared slot before spawning so failed thread creation
        // cannot drop an un-waited Child or lose its live pipe owners.
        let slot = Arc::new(Mutex::new(Some((
            owner.child.take().unwrap(),
            std::mem::take(&mut owner.threads),
        ))));
        let owned = slot.clone();
        match thread::Builder::new()
            .name("desktop-reap".into())
            .spawn(move || {
                let (child, threads) = owned.lock().unwrap().take().unwrap();
                let mut child = Some(child);
                match wait_owned(&mut child, Child::wait) {
                    Ok(status) => {
                        finish_wait(&shared, status, threads);
                        (None, vec![])
                    }
                    Err(e) => {
                        shared.fail(format!("desktop process wait failed; owner retained: {e}"));
                        let attempt = shared.state.lock().unwrap().shutdown_attempt;
                        let _ = shared.drain_failed(attempt, format!("desktop process wait failed: {e}"));
                        (child, threads)
                    }
                }
            }) {
            Ok(handle) => owner.supervisor = Some(handle),
            Err(e) => {
                let (child, threads) = slot.lock().unwrap().take().unwrap();
                owner.child = Some(child);
                owner.threads = threads;
                owner.shared.stop();
                let _ = owner.drain();
                return Err(e.into());
            }
        }
        Ok(owner)
    }
    pub fn drain(&mut self) -> Result<()> {
        if self.supervisor.as_ref().is_some_and(|s| !s.is_finished()) {
            let mut state = self.shared.state.lock().unwrap();
            while !state.reaped && state.drain_error.is_none() {
                state = self.shared.wake.wait(state).unwrap();
            }
            if let Some(message) = &state.drain_error {
                return Err(error(ErrorCode::Native, message));
            }
        }
        if let Some(supervisor) = self.supervisor.take() {
            match supervisor.join() {
                Ok((child, threads)) => {
                    self.child = child;
                    self.threads = threads;
                }
                Err(_) => {
                    self.shared
                        .fail("desktop supervisor panicked; process custody unknown");
                    return Err(error(
                        ErrorCode::Native,
                        "desktop supervisor lost process custody",
                    ));
                }
            }
        }
        if self.child.is_some() {
            let status = wait_owned(&mut self.child, Child::wait).map_err(|e| {
                self.shared
                    .fail(format!("desktop process wait failed; owner retained: {e}"));
                error(
                    ErrorCode::Native,
                    format!("desktop process not reaped: {e}"),
                )
            })?;
            finish_wait(&self.shared, status, std::mem::take(&mut self.threads));
        }
        let s = self.shared.state.lock().unwrap();
        match &s.message {
            Some(m) => Err(error(ErrorCode::Native, m)),
            None => Ok(()),
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if !self.shared.state.lock().unwrap().stopping {
            self.shared.stop();
        }
        if self.shared.state.lock().unwrap().reaped {
            let _ = self.drain();
        } else {
            // The supervisor/pipe owners retain the child and its failed drain.
            // No implicit retry, force exit or replacement is authorized by Drop.
            std::mem::forget(self.child.take());
            std::mem::forget(self.supervisor.take());
            std::mem::forget(std::mem::take(&mut self.threads));
            std::mem::forget(self.shared.clone());
        }
    }
}
pub(super) fn next_outgoing(shared: &Shared, active: &mut Option<Message>) -> Option<Message> {
    let mut s = shared.state.lock().unwrap();
    loop {
        if s.reaped { return None; }
        if s.stopping {
            if let Some(index) = s.control.iter().position(|m| m.kind == Kind::Ack) {
                return s.control.remove(index);
            }
            if s.shutdown_sent < s.shutdown_attempt {
                s.shutdown_sent = s.shutdown_attempt;
                return Some(Message::new(Kind::Shutdown, s.shutdown_attempt, vec![]));
            }
            s = shared.wake.wait(s).unwrap();
            continue;
        }
        if let Some((&id, entry)) = s
            .pending
            .iter_mut()
            .find(|(_, p)| p.cancel.is_canceled() && !p.sent_cancel)
        {
            entry.sent_cancel = true;
            return Some(Message::new(Kind::Cancel, id, vec![]));
        }
        if let Some(m) = s.control.pop_front() {
            return Some(m);
        }
        if let Some(m) = active.take() {
            return Some(m);
        }
        if let Some(m) = s.data.pop_front() {
            return Some(m);
        }
        s = shared.wake.wait(s).unwrap();
    }
}
fn parent_write(mut w: impl Write, shared: &Shared, hello: Vec<u8>) -> std::io::Result<()> {
    Message::new(Kind::Hello, 0, hello).write(shared.session, &mut w)?;
    let mut active = None;
    while let Some(mut m) = next_outgoing(shared, &mut active) {
        m.next(shared.session).write(&mut w)?;
        if !m.finished() {
            active = Some(m);
        }
    }
    Ok(())
}
fn session(f: &Frame, shared: &Shared) -> std::io::Result<()> {
    if f.session != shared.session {
        return Err(wire::invalid("stale desktop session frame"));
    }
    Ok(())
}
fn parent_control(mut r: impl Read, shared: &Shared) -> std::io::Result<()> {
    let mut assembly: Option<Assembly> = None;
    while let Some(f) = Frame::read(&mut r)? {
        session(&f, shared)?;
        if !matches!(f.kind, Kind::Ready | Kind::Reply | Kind::BytesError | Kind::DrainError) {
            return Err(wire::invalid("unexpected control frame"));
        }
        let a = match assembly.as_mut() {
            Some(a) => a,
            None => assembly.insert(Assembly::start(&f, if matches!(f.kind, Kind::Ready | Kind::DrainError) {wire::CHUNK} else {shared.limits.reply_bytes.max(wire::ERROR_BYTES)})?),
        };
        if !a.push(f)? {
            continue;
        }
        let (kind, id, bytes) = assembly.take().unwrap().finish();
        if kind == Kind::DrainError {
            let error: BridgeError = serde_json::from_slice(&bytes).map_err(|_|wire::invalid("drain error response"))?;
            shared.drain_failed(id, error.message)?; continue;
        }
        if kind == Kind::Ready {
            if id != 0 || bytes != wire::build_identity().as_bytes() {
                return Err(wire::invalid("desktop handshake identity"));
            }
            let mut s = shared.state.lock().unwrap();
            if s.ready {
                return Err(wire::invalid("duplicate handshake"));
            }
            s.ready = true;
            if !s.stopping {
                s.phase = TransportPhase::Ready;
            }
            shared.wake.notify_all();
            continue;
        }
        // Validate bytes before consuming the pending authority; malformed replies
        // keep the operation unknown until process drain.
        let decoded_reply = if kind == Kind::Reply {
            Some(
                serde_json::from_slice::<Reply>(&bytes)
                    .map_err(|_| wire::invalid("invalid desktop reply"))?,
            )
        } else {
            None
        };
        let decoded_error = if kind == Kind::BytesError {
            Some(
                serde_json::from_slice::<BridgeError>(&bytes)
                    .map_err(|_| wire::invalid("invalid desktop byte error"))?,
            )
        } else {
            None
        };
        if bytes.len() > shared.limits.reply_bytes && decoded_reply.as_ref().is_some_and(|reply| !matches!(reply, Reply::Error { .. })) {
            return Err(wire::invalid("success reply exceeds configured budget"));
        }
        let entry = {
            let mut state = shared.state.lock().unwrap();
            let pending = state
                .pending
                .get(&id)
                .ok_or_else(|| wire::invalid("unowned desktop reply"))?;
            if !matches!(
                (kind, &pending.delivery),
                (Kind::Reply, Delivery::Command(_)) | (Kind::BytesError, Delivery::Bytes { .. })
            ) {
                return Err(wire::invalid("reply kind does not match request"));
            }
            state.pending.remove(&id).unwrap()
        };
        match (kind, entry.delivery) {
            (Kind::Reply, Delivery::Command(tx)) => {
                let reply = decoded_reply.unwrap();
                let _ = tx.send(reply);
            }
            (Kind::BytesError, Delivery::Bytes { reply, .. }) => {
                let err = decoded_error.unwrap();
                let _ = reply.send(Err(err));
            }
            _ => return Err(wire::invalid("reply kind does not match request")),
        }
    }
    if assembly.is_some() {
        return Err(wire::invalid("truncated control message"));
    }
    if !shared.state.lock().unwrap().stopping {
        shared.fail("desktop control EOF; outcomes unknown until process drain");
    }
    Ok(())
}

pub(super) struct BinaryAssembly {
    id: u64,
    header: BinaryHeader,
    bytes: Vec<u8>,
    usage: Arc<AtomicUsize>,
    reservation: usize,
}
impl BinaryAssembly {
    pub fn new(id: u64, header: BinaryHeader, shared: &Shared) -> std::io::Result<Self> {
        if header.mime.len() > 128
            || header.digest.len() != 64
            || header.bytes > shared.limits.binary_bytes
        {
            return Err(wire::invalid("binary descriptor admission"));
        }
        {
            let s = shared.state.lock().unwrap();
            match s.pending.get(&id).map(|p| &p.delivery) {
                Some(Delivery::Bytes { request, .. })
                    if request.catalog == header.catalog && request.ticket == header.ticket => {}
                _ => return Err(wire::invalid("binary selected request identity")),
            }
        }
        shared
            .binary
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(header.bytes)
                    .filter(|v| *v <= shared.limits.binary_bytes)
            })
            .map_err(|_| wire::invalid("aggregate binary reservation"))?;
        let reservation = header.bytes;
        Ok(Self {
            id,
            header,
            bytes: Vec::with_capacity(reservation),
            usage: shared.binary.clone(),
            reservation,
        })
    }
    pub fn push(&mut self, f: Frame) -> std::io::Result<()> {
        if f.kind != Kind::BinaryChunk
            || f.id != self.id
            || f.offset != self.bytes.len()
            || f.total != self.header.bytes
            || f.payload.is_empty()
            || f.offset
                .checked_add(f.payload.len())
                .is_none_or(|n| n > self.header.bytes)
        {
            return Err(wire::invalid("binary frame continuity"));
        }
        self.bytes.extend(f.payload);
        Ok(())
    }
    pub fn finish(mut self, shared: &Shared) -> std::io::Result<()> {
        if self.bytes.len() != self.header.bytes
            || blake3::hash(&self.bytes).to_hex().as_str() != self.header.digest
        {
            return Err(wire::invalid("binary length/checksum"));
        }
        let entry = shared
            .state
            .lock()
            .unwrap()
            .pending
            .remove(&self.id)
            .ok_or_else(|| wire::invalid("binary pending expired"))?;
        shared
            .acknowledge(self.id)
            .map_err(|_| wire::invalid("binary acknowledgement unavailable"))?;
        if let Delivery::Bytes { reply, .. } = entry.delivery {
            if entry.cancel.is_canceled() {
                let _ = reply.send(Err(error(ErrorCode::Canceled, "preview transfer canceled")));
            } else {
                let value = PreviewBytes {
                    mime: std::mem::take(&mut self.header.mime),
                    bytes: std::mem::take(&mut self.bytes),
                    usage: self.usage.clone(),
                };
                self.reservation = 0;
                let _ = reply.send(Ok(value));
            }
            Ok(())
        } else {
            Err(wire::invalid("binary delivery changed"))
        }
    }
}
impl Drop for BinaryAssembly {
    fn drop(&mut self) {
        self.usage.fetch_sub(self.reservation, Ordering::AcqRel);
    }
}
fn parent_binary(mut r: impl Read, shared: &Shared) -> std::io::Result<()> {
    let mut current: Option<BinaryAssembly> = None;
    while let Some(f) = Frame::read(&mut r)? {
        session(&f, shared)?;
        match f.kind {
            Kind::BinaryStart
                if current.is_none() && f.offset == 0 && f.total == f.payload.len() =>
            {
                let header = serde_json::from_slice(&f.payload)
                    .map_err(|_| wire::invalid("binary descriptor"))?;
                current = Some(BinaryAssembly::new(f.id, header, shared)?);
            }
            Kind::BinaryChunk => current
                .as_mut()
                .ok_or_else(|| wire::invalid("binary header missing"))?
                .push(f)?,
            Kind::BinaryEnd if f.offset == 0 && f.total == 0 && f.payload.is_empty() => {
                let data = current
                    .take()
                    .ok_or_else(|| wire::invalid("binary header missing"))?;
                if data.id != f.id {
                    return Err(wire::invalid("binary end identity"));
                }
                data.finish(shared)?;
            }
            _ => return Err(wire::invalid("unexpected binary frame")),
        }
    }
    if current.is_some() {
        return Err(wire::invalid("truncated binary transfer"));
    }
    if !shared.state.lock().unwrap().stopping {
        shared.fail("desktop binary EOF; process drain required");
    }
    Ok(())
}

enum ChildPending {
    Command(Pending),
    Bytes(PendingBytes, BytesRequest),
    Reply(Message),
    Binary(Arc<PreviewBytes>, BytesRequest),
}
struct ChildState {
    pending: HashMap<u64, ChildPending>,
    cancels: HashMap<u64, Cancellation>,
    early: std::collections::HashSet<u64>,
    retained: HashMap<u64, Arc<PreviewBytes>>,
    stopping: bool,
}
type ChildShared = Arc<Mutex<ChildState>>;
struct BinaryOutput {
    id: u64,
    request: BytesRequest,
    value: Arc<PreviewBytes>,
}
fn output_writer(mut w: impl Write, rx: mpsc::Receiver<Message>, session: [u8; 16]) {
    for m in rx {
        if m.write(session, &mut w).is_err() {
            break;
        }
    }
}
fn binary_writer(mut w: impl Write, rx: mpsc::Receiver<BinaryOutput>, session: [u8; 16]) {
    for out in rx {
        let result = (|| -> std::io::Result<()> {
            let header = BinaryHeader {
                catalog: out.request.catalog,
                ticket: out.request.ticket,
                mime: out.value.mime.clone(),
                bytes: out.value.bytes().len(),
                digest: blake3::hash(out.value.bytes()).to_hex().to_string(),
            };
            Message::new(
                Kind::BinaryStart,
                out.id,
                serde_json::to_vec(&header).unwrap(),
            )
            .write(session, &mut w)?;
            for (index, bytes) in out.value.bytes().chunks(wire::CHUNK).enumerate() {
                Frame {
                    kind: Kind::BinaryChunk,
                    session,
                    id: out.id,
                    offset: index * wire::CHUNK,
                    total: header.bytes,
                    payload: bytes.to_vec(),
                }
                .write(&mut w)?;
            }
            Message::new(Kind::BinaryEnd, out.id, vec![]).write(session, &mut w)
        })();
        if result.is_err() {
            break;
        }
    }
}
fn checked_message(kind: Kind, id: u64, value: &impl serde::Serialize, limit: usize) -> Message {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    if bytes.len() <= limit {
        Message::new(kind, id, bytes)
    } else {
        let e = error(ErrorCode::ResourceLimit, "desktop reply byte limit");
        let bytes = if kind == Kind::Reply {
            serde_json::to_vec(&Reply::Error { error: e }).unwrap()
        } else {
            serde_json::to_vec(&e).unwrap()
        };
        Message::new(kind, id, bytes)
    }
}
fn collect(
    shared: &ChildShared,
    tx: &mpsc::SyncSender<Message>,
    binary: &mpsc::SyncSender<BinaryOutput>,
    limit: usize,
) {
    let mut state = shared.lock().unwrap();
    let ids = state.pending.keys().copied().collect::<Vec<_>>();
    for id in ids {
        let p = state.pending.remove(&id).unwrap();
        let ready = match p {
            ChildPending::Command(p) => match p.receiver.try_recv() {
                Ok(r) => ChildPending::Reply(checked_message(Kind::Reply, id, &r, limit)),
                Err(mpsc::TryRecvError::Empty) => {
                    state.pending.insert(id, ChildPending::Command(p));
                    continue;
                }
                Err(_) => ChildPending::Reply(checked_message(
                    Kind::Reply,
                    id,
                    &failure(ErrorCode::Closed, "actor pending disconnected"),
                    limit,
                )),
            },
            ChildPending::Bytes(p, request) => match p.receiver.try_recv() {
                Ok(Ok(v)) => ChildPending::Binary(Arc::new(v), request),
                Ok(Err(e)) => ChildPending::Reply(checked_message(Kind::BytesError, id, &e, limit)),
                Err(mpsc::TryRecvError::Empty) => {
                    state.pending.insert(id, ChildPending::Bytes(p, request));
                    continue;
                }
                Err(_) => ChildPending::Reply(checked_message(
                    Kind::BytesError,
                    id,
                    &error(ErrorCode::Closed, "actor bytes disconnected"),
                    limit,
                )),
            },
            ready => ready,
        };
        match ready {
            ChildPending::Reply(m) => match tx.try_send(m) {
                Ok(()) => {
                    state.cancels.remove(&id);
                }
                Err(mpsc::TrySendError::Full(m)) => {
                    state.pending.insert(id, ChildPending::Reply(m));
                }
                Err(_) => {
                    state.stopping = true;
                    return;
                }
            },
            ChildPending::Binary(value, request) => {
                state.retained.insert(id, value.clone());
                match binary.try_send(BinaryOutput { id, request, value }) {
                    Ok(()) => {
                        state.cancels.remove(&id);
                    }
                    Err(mpsc::TrySendError::Full(out)) => {
                        state.retained.remove(&id);
                        state
                            .pending
                            .insert(id, ChildPending::Binary(out.value, out.request));
                    }
                    Err(_) => {
                        state.stopping = true;
                        return;
                    }
                }
            }
            _ => unreachable!(),
        }
    }
}

/// Even an early transport error or unwinding must not exit a descendant-owning
/// process after a failed checked drain. There is no force-exit fallback.
struct ChildEngine {
    bridge: Bridge,
    verified: std::cell::Cell<bool>,
    failed: std::cell::Cell<bool>,
}
impl ChildEngine {
    fn drain(&self) -> Result<()> {
        if self.verified.get() { return Ok(()); }
        match self.bridge.try_shutdown() {
            Ok(()) => { self.verified.set(true); self.failed.set(false); Ok(()) }
            Err(error) => { self.failed.set(true); Err(error) }
        }
    }
}
impl Drop for ChildEngine {
    fn drop(&mut self) {
        if !self.verified.get() && !self.failed.get() { let _ = self.drain(); }
        if !self.verified.get() {
            // EOF has removed the explicit retry channel. Retain every actor,
            // lease and descendant owner instead of releasing them via process exit.
            loop { thread::park(); }
        }
    }
}
pub(super) fn worker_main() -> anyhow::Result<()> {
    let mut input = std::io::stdin().lock();
    let first =
        Frame::read(&mut input)?.ok_or_else(|| wire::invalid("missing desktop configuration"))?;
    anyhow::ensure!(
        first.kind == Kind::Hello && first.id == 0,
        "expected desktop configuration"
    );
    let session = first.session;
    let mut assembly = Assembly::start(&first, wire::CONFIG_BYTES)?;
    let mut done = assembly.push(first)?;
    while !done {
        let f = Frame::read(&mut input)?.ok_or_else(|| wire::invalid("truncated configuration"))?;
        anyhow::ensure!(f.session == session, "configuration session mismatch");
        done = assembly.push(f)?;
    }
    let config: wire::ConfigWire = serde_json::from_slice(&assembly.finish().2)?;
    let config = config.into_config()?;
    let limits = config.limits.clone();
    let engine = ChildEngine { bridge: Bridge::spawn(config)?, verified: false.into(), failed: false.into() };
    let bridge = &engine.bridge;
    let shared = Arc::new(Mutex::new(ChildState {
        pending: HashMap::new(),
        cancels: HashMap::new(),
        early: Default::default(),
        retained: HashMap::new(),
        stopping: false,
    }));
    let (tx, rx) = mpsc::sync_channel(limits.queued + CONTROL_SLOTS);
    let (binary, binary_rx) = mpsc::sync_channel(1);
    let control = thread::spawn(move || output_writer(std::io::stderr().lock(), rx, session));
    let data = thread::spawn(move || binary_writer(std::io::stdout().lock(), binary_rx, session));
    let state = shared.clone();
    let results = tx.clone();
    let result_limit = limits.reply_bytes;
    let collector = thread::spawn(move || {
        while !state.lock().unwrap().stopping {
            collect(&state, &results, &binary, result_limit);
            thread::sleep(Duration::from_millis(2));
        }
    });
    let ready = tx.send(Message::new(
        Kind::Ready,
        0,
        wire::build_identity().into_bytes(),
    ));
    let result = ready.map_err(anyhow::Error::from).and_then(|_| child_input(&mut input, session, &mut |kind, bytes| dispatch(bridge, kind, bytes), &shared, &limits, &tx, &mut || engine.drain()));
    // Never force-exit: first drain the engine and its descendants, then transport.
    {
        let state = shared.lock().unwrap();
        for c in state.cancels.values() {
            c.cancel();
        }
    }
    // A failed explicit drain is not retried automatically when the parent
    // disappears. ChildEngine keeps the process alive if that owner is unresolved.
    if !engine.failed.get() { engine.drain()?; }
    anyhow::ensure!(engine.verified.get(), "desktop engine drain unresolved");
    shared.lock().unwrap().stopping = true;
    let _ = collector.join();
    {
        let mut state = shared.lock().unwrap();
        state.pending.clear();
        state.retained.clear();
    }
    drop(tx);
    let _ = control.join();
    let _ = data.join();
    result
}
fn dispatch(bridge: &Bridge, kind: Kind, bytes: &[u8]) -> anyhow::Result<Result<ChildPending>> {
    if kind == Kind::Command {
        let request: Request = serde_json::from_slice(bytes)?;
        anyhow::ensure!(!local_route(&request), "Workbench cannot enter catalog child");
        Ok(bridge.submit(request).map(ChildPending::Command))
    } else {
        let r: BytesRequest = serde_json::from_slice(bytes)?;
        Ok(bridge.preview_bytes(r.catalog.clone(), r.ticket.clone(), r.foreground)
            .map(|p| ChildPending::Bytes(p, r)))
    }
}
fn child_input(
    input: &mut impl Read,
    session: [u8; 16],
    dispatch: &mut impl FnMut(Kind, &[u8]) -> anyhow::Result<Result<ChildPending>>,
    shared: &ChildShared,
    limits: &Limits,
    tx: &mpsc::SyncSender<Message>,
    drain: &mut impl FnMut() -> Result<()>,
) -> anyhow::Result<()> {
    let mut active: Option<Assembly> = None;
    let mut seen = Seen::default();
    let mut drain_attempt = 0;
    while let Some(f) = Frame::read(input)? {
        anyhow::ensure!(f.session == session, "stale child session");
        if matches!(f.kind, Kind::Cancel | Kind::Ack | Kind::Shutdown) {
            anyhow::ensure!(
                f.offset == 0 && f.total == 0 && f.payload.is_empty(),
                "invalid control payload"
            );
            let mut state = shared.lock().unwrap();
            match f.kind {
                Kind::Shutdown => {
                    anyhow::ensure!(f.id > drain_attempt, "shutdown attempt identity");
                    drain_attempt = f.id;
                    drop(state);
                    match drain() {
                        Ok(()) => return Ok(()),
                        Err(error) => {
                            tx.send(checked_message(Kind::DrainError, f.id, &error, wire::CHUNK))?;
                            continue;
                        }
                    }
                }
                Kind::Cancel => {
                    anyhow::ensure!(f.id != 0, "cancel identity");
                    if let Some(c) = state.cancels.get(&f.id) {
                        c.cancel();
                    } else if !seen.contains(f.id) && !state.retained.contains_key(&f.id) {
                        anyhow::ensure!(
                            state.early.len() < limits.queued + CONTROL_SLOTS,
                            "early cancellation bounds"
                        );
                        state.early.insert(f.id);
                    }
                }
                Kind::Ack => {
                    anyhow::ensure!(
                        state.retained.remove(&f.id).is_some(),
                        "unowned binary acknowledgement"
                    );
                }
                _ => unreachable!(),
            }
            continue;
        }
        anyhow::ensure!(
            matches!(f.kind, Kind::Command | Kind::Bytes),
            "unexpected request kind"
        );
        // Priority requests are single frames and may interleave a large request.
        let (kind, id, bytes) = if f.offset == 0 && f.total == f.payload.len() {
            anyhow::ensure!(f.total <= limits.request_bytes, "request byte admission");
            (f.kind, f.id, f.payload)
        } else {
            let a = match active.as_mut() {
                Some(a) => a,
                None => active.insert(Assembly::start(&f, limits.request_bytes)?),
            };
            if !a.push(f)? {
                continue;
            }
            active.take().unwrap().finish()
        };
        seen.insert(id, limits.queued + CONTROL_SLOTS + 1)?;
        // Only this input loop admits requests; the collector can only remove
        // pending entries. Check capacity and consume a winning early cancel
        // before calling any engine entry point, including synchronous commands.
        let canceled = {
            let mut state = shared.lock().unwrap();
            anyhow::ensure!(
                state.pending.len() < limits.queued + CONTROL_SLOTS,
                "child pending bounds"
            );
            state.early.remove(&id)
        };
        let admitted = if canceled {
            Err(error(ErrorCode::Canceled, "request canceled before desktop admission"))
        } else {
            dispatch(kind, &bytes)?
        };
        let mut state = shared.lock().unwrap();
        match admitted {
            Ok(p) => {
                let c = match &p {
                    ChildPending::Command(p) => p.cancellation(),
                    ChildPending::Bytes(p, _) => p.cancellation(),
                    _ => unreachable!(),
                };
                state.cancels.insert(id, c);
                state.pending.insert(id, p);
            }
            Err(e) => {
                state.early.remove(&id);
                let m = if kind == Kind::Command {
                    checked_message(
                        Kind::Reply,
                        id,
                        &Reply::Error { error: e },
                        limits.reply_bytes,
                    )
                } else {
                    checked_message(Kind::BytesError, id, &e, limits.reply_bytes)
                };
                state.pending.insert(id, ChildPending::Reply(m));
            }
        }
        drop(state);
        let _ = tx; // completion publication stays independent of this input loop.
    }
    anyhow::ensure!(active.is_none(), "truncated desktop request");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn child_state() -> ChildShared {
        Arc::new(Mutex::new(ChildState {
            pending: HashMap::new(), cancels: HashMap::new(), early: Default::default(),
            retained: HashMap::new(), stopping: false,
        }))
    }
    #[test]
    fn held_binary_output_does_not_block_command_completion_or_free_child_bytes() {
        let shared = child_state();
        let usage = Arc::new(AtomicUsize::new(4));
        let value = Arc::new(PreviewBytes { mime: "image/png".into(), bytes: b"raw!".to_vec(), usage: usage.clone() });
        let request = BytesRequest { catalog: "A".into(), ticket: "T".into(), foreground: true };
        shared.lock().unwrap().pending.insert(1, ChildPending::Binary(value, request));
        shared.lock().unwrap().pending.insert(2, ChildPending::Reply(checked_message(Kind::Reply, 2, &failure(ErrorCode::Closed, "no catalog"), 1024)));
        let (tx, rx) = mpsc::sync_channel(1);
        let (blocked, _held) = mpsc::sync_channel(0);
        collect(&shared, &tx, &blocked, 1024);
        assert_eq!(rx.try_recv().unwrap().id, 2);
        assert!(shared.lock().unwrap().pending.contains_key(&1));
        assert_eq!(usage.load(Ordering::Acquire), 4);
        let (binary, delivery) = mpsc::sync_channel(1);
        collect(&shared, &tx, &binary, 1024);
        let output = delivery.try_recv().unwrap();
        assert!(!shared.lock().unwrap().pending.contains_key(&1));
        drop(output); // OS pipe delivery is not acknowledgement.
        assert_eq!(usage.load(Ordering::Acquire), 4);
        shared.lock().unwrap().retained.remove(&1).unwrap();
        assert_eq!(usage.load(Ordering::Acquire), 0);
        assert!(shared.lock().unwrap().retained.remove(&1).is_none());
    }
    #[test]
    fn failed_os_wait_preserves_owner_until_explicit_success() {
        #[cfg(unix)] use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)] use std::os::windows::process::ExitStatusExt;
        struct Owned(Arc<AtomicUsize>);
        impl Drop for Owned { fn drop(&mut self) { self.0.fetch_add(1, Ordering::AcqRel); } }
        let drops = Arc::new(AtomicUsize::new(0));
        let mut owner = Some(Owned(drops.clone()));
        assert!(wait_owned(&mut owner, |_| Err(std::io::Error::other("injected wait failure"))).is_err());
        assert!(owner.is_some()); assert_eq!(drops.load(Ordering::Acquire), 0);
        wait_owned(&mut owner, |_| Ok(std::process::ExitStatus::from_raw(0))).unwrap();
        assert!(owner.is_none()); assert_eq!(drops.load(Ordering::Acquire), 1);
    }
    #[test]
    fn tiny_success_budget_still_delivers_bounded_resource_error() {
        let mut shared = super::super::tests::shared(8);
        Arc::get_mut(&mut shared).unwrap().limits.reply_bytes = 1;
        let (tx, rx) = mpsc::sync_channel(1);
        shared.state.lock().unwrap().pending.insert(1, Entry {
            delivery: Delivery::Command(tx), cancel: Cancellation::default(), sent_cancel: false, control: true,
        });
        let message = checked_message(Kind::Reply, 1, &failure(ErrorCode::Busy, "a longer original error"), 1);
        assert!(message.bytes.len() <= wire::ERROR_BYTES);
        let mut bytes = Vec::new(); message.write(shared.session, &mut bytes).unwrap();
        parent_control(std::io::Cursor::new(bytes), &shared).unwrap();
        assert!(matches!(rx.recv().unwrap(), Reply::Error {error: BridgeError {code: ErrorCode::ResourceLimit, ..}}));
    }
    #[test]
    fn replies_complete_out_of_order_but_eof_does_not_guess_lost_outcome() {
        let shared = super::super::tests::shared(8);
        let mut receivers = Vec::new();
        for id in [1,2] {
            let (tx,rx) = mpsc::sync_channel(1);
            shared.state.lock().unwrap().pending.insert(id, Entry {
                delivery: Delivery::Command(tx), cancel: Cancellation::default(), sent_cancel: false, control: true,
            });
            receivers.push(rx);
        }
        let reply = failure(ErrorCode::Busy, "observed second request");
        let mut wire = Vec::new();
        Message::new(Kind::Reply, 2, serde_json::to_vec(&reply).unwrap()).write(shared.session, &mut wire).unwrap();
        parent_control(std::io::Cursor::new(wire.clone()), &shared).unwrap();
        assert!(matches!(receivers[0].try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(matches!(receivers[1].try_recv().unwrap(), Reply::Error {..}));
        assert_eq!(shared.state.lock().unwrap().phase, TransportPhase::Failed);
        assert!(!shared.state.lock().unwrap().reaped);
        assert!(shared.state.lock().unwrap().unknown);
        // Repeated completion and another session cannot consume the remaining owner.
        assert!(parent_control(std::io::Cursor::new(wire.clone()), &shared).is_err());
        wire[8] ^= 1;
        assert!(parent_control(std::io::Cursor::new(wire), &shared).is_err());
        assert!(shared.state.lock().unwrap().pending.contains_key(&1));
        shared.complete_failure(); // Called by the real implementation only after wait.
        assert!(matches!(receivers[0].recv().unwrap(), Reply::Error {..}));
    }
    #[test]
    fn early_cancel_and_full_capacity_prevent_mutation_dispatch() {
        let shared = child_state();
        let (tx, _rx) = mpsc::sync_channel(2);
        let request = Request::Create { path: NativePath::from_path(std::path::Path::new("/synthetic-mutation")) };
        let body = serde_json::to_vec(&request).unwrap();
        let mut input = Vec::new();
        Message::new(Kind::Cancel, 1, vec![]).write([9;16], &mut input).unwrap();
        Message::new(Kind::Command, 1, body.clone()).write([9;16], &mut input).unwrap();
        let mutations = std::cell::Cell::new(0);
        let mut fake = |kind, bytes: &[u8]| {
            assert_eq!(kind, Kind::Command);
            assert!(matches!(serde_json::from_slice::<Request>(bytes).unwrap(), Request::Create { .. }));
            mutations.set(mutations.get() + 1);
            Ok(Err(error(ErrorCode::Native, "mutation recorded by fake engine")))
        };
        child_input(&mut std::io::Cursor::new(input), [9;16], &mut fake, &shared, &Limits::default(), &tx, &mut || Ok(())).unwrap();
        let state = shared.lock().unwrap();
        assert!(state.cancels.is_empty());
        assert!(state.early.is_empty());
        let ChildPending::Reply(reply) = state.pending.get(&1).unwrap() else { panic!("typed canceled reply required") };
        assert!(matches!(serde_json::from_slice::<Reply>(&reply.bytes).unwrap(), Reply::Error {error: BridgeError {code: ErrorCode::Canceled, ..}}));
        drop(state);
        // A full completion registry must reject before the same mutating dispatcher.
        let limits = Limits { queued: 1, ..Limits::default() };
        for id in 2..=(CONTROL_SLOTS + 1) as u64 {
            shared.lock().unwrap().pending.insert(id, ChildPending::Reply(Message::new(Kind::Reply, id, vec![])));
        }
        let mut input = Vec::new();
        Message::new(Kind::Command, 99, body.clone()).write([9;16], &mut input).unwrap();
        let result = child_input(&mut std::io::Cursor::new(input), [9;16], &mut fake, &shared, &limits, &tx, &mut || Ok(()));
        assert!(result.unwrap_err().to_string().contains("child pending bounds"));
        assert_eq!(mutations.get(), 0, "canceled and capacity-rejected writes never reach engine");
        // Prove the fake records an admitted mutation, and repeated IDs cannot dispatch twice.
        let shared = child_state();
        let mut input = Vec::new();
        for _ in 0..2 { Message::new(Kind::Command, 1, body.clone()).write([9;16], &mut input).unwrap(); }
        let result = child_input(&mut std::io::Cursor::new(input), [9;16], &mut fake, &shared, &limits, &tx, &mut || Ok(()));
        assert!(result.unwrap_err().to_string().contains("replayed"));
        assert_eq!(mutations.get(), 1);
    }
    #[test]
    fn child_drain_failure_reports_exact_attempt_and_accepts_same_process_retry() {
        let shared = child_state();
        let (tx, rx) = mpsc::sync_channel(2);
        let mut input = Vec::new();
        Message::new(Kind::Shutdown, 1, vec![]).write([9;16], &mut input).unwrap();
        Message::new(Kind::Shutdown, 2, vec![]).write([9;16], &mut input).unwrap();
        let mut calls = 0;
        child_input(&mut std::io::Cursor::new(input), [9;16], &mut |_, _| panic!("shutdown must not dispatch a command"), &shared, &Limits::default(), &tx, &mut || {
            calls += 1;
            if calls == 1 { Err(error(ErrorCode::Native, "injected native drain failure")) } else { Ok(()) }
        }).unwrap();
        assert_eq!(calls, 2);
        let failure = rx.try_recv().unwrap();
        assert_eq!((failure.kind, failure.id), (Kind::DrainError, 1));
        assert!(rx.try_recv().is_err());
    }
}
