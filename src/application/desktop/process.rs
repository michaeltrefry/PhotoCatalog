use super::*;
use anyhow::Context;
use std::{
    io::{Read, Write},
    process::{Child, Command, Stdio},
    time::Duration,
};
use wire::{Assembly, BinaryHeader, Frame, Seen};

struct RelayOutput {
    out: filesystem::Out,
    offset: usize,
}
impl RelayOutput {
    fn frame(&mut self, session: [u8; 16]) -> Frame {
        let end = (self.offset + wire::CHUNK).min(self.out.bytes.len());
        let frame = Frame {
            kind: match self.out.lane {
                filesystem::Lane::Control => Kind::FilesystemControl,
                filesystem::Lane::Data => Kind::Filesystem,
                filesystem::Lane::Admission => Kind::FilesystemAdmission,
                filesystem::Lane::Store => Kind::FilesystemStore,
            },
            session,
            id: 0,
            offset: self.offset,
            total: self.out.bytes.len(),
            payload: self.out.bytes[self.offset..end].to_vec(),
        };
        self.offset = end;
        frame
    }
    fn done(&self) -> bool {
        self.offset == self.out.bytes.len()
    }
}
#[derive(Default)]
struct RelayAssembly {
    data: Option<Assembly>,
    admission: Option<Assembly>,
    store: Option<Assembly>,
}
impl RelayAssembly {
    fn incomplete(&self) -> bool {
        self.data.is_some() || self.admission.is_some() || self.store.is_some()
    }
}
fn relay_input(
    active: &mut RelayAssembly,
    f: Frame,
) -> anyhow::Result<Option<(Vec<u8>, filesystem::Lane)>> {
    anyhow::ensure!(f.id == 0, "filesystem frame envelope identity");
    let lane = match f.kind {
        Kind::FilesystemControl => filesystem::Lane::Control,
        Kind::Filesystem => filesystem::Lane::Data,
        Kind::FilesystemAdmission => filesystem::Lane::Admission,
        Kind::FilesystemStore => filesystem::Lane::Store,
        _ => anyhow::bail!("unexpected relay frame"),
    };
    if lane == filesystem::Lane::Control {
        anyhow::ensure!(
            f.offset == 0 && f.total == f.payload.len() && f.total <= filesystem::CONTROL_BYTES,
            "filesystem control frame bounds"
        );
        return Ok(Some((f.payload, lane)));
    }
    let slot = match lane {
        filesystem::Lane::Admission => &mut active.admission,
        filesystem::Lane::Store => &mut active.store,
        filesystem::Lane::Data => &mut active.data,
        filesystem::Lane::Control => unreachable!(),
    };
    let a = match slot.as_mut() {
        Some(a) => a,
        None => slot.insert(Assembly::start(&f, filesystem::BYTES)?),
    };
    if !a.push(f)? {
        return Ok(None);
    }
    Ok(Some((slot.take().unwrap().finish().2, lane)))
}
type IoThreads = Vec<thread::JoinHandle<()>>;
type DrainResult = (Option<Child>, IoThreads);
/// Remove the owned process only after an affirmative OS wait result.
fn wait_owned<T>(
    slot: &mut Option<T>,
    wait: impl FnOnce(&mut T) -> std::io::Result<std::process::ExitStatus>,
) -> std::io::Result<std::process::ExitStatus> {
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
    #[cfg(test)]
    fixture_child: Option<Arc<Mutex<Option<Child>>>>,
}
fn finish_wait(shared: &Shared, status: std::process::ExitStatus, threads: IoThreads) {
    if !status.success() {
        shared.fail(format!(
            "desktop process exited {status}; unacknowledged outcomes unknown"
        ));
    }
    {
        let mut s = shared.state.lock().unwrap();
        s.reaped = true;
        s.child_exit = status.code();
    }
    shared.wake.notify_all();
    for thread in threads {
        if thread.join().is_err() {
            shared.fail("desktop transport thread panicked");
        }
    }
    shared.complete_failure();
    shared.child_finished();
    shared.metadata.retire();
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
        Self::spawn_configured(
            executable,
            &[std::ffi::OsString::from("--catalog-desktop-worker")],
            shared,
            hello,
            false,
        )
    }
    fn spawn_configured(
        executable: &std::path::Path,
        args: &[std::ffi::OsString],
        shared: Arc<Shared>,
        hello: Vec<u8>,
        harness: bool,
    ) -> anyhow::Result<Self> {
        // Arm before OS spawn: any unwind after a successful spawn must retain
        // admission. Only a proven no-child error or checked wait retires it.
        shared.metadata.arm();
        let mut child = match Command::new(executable)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                shared.metadata.retire();
                return Err(error.into());
            }
        };
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
            #[cfg(test)]
            fixture_child: None,
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
                            #[cfg(test)]
                            let output: Box<dyn Read> = if harness {
                                Box::new(super::filesystem_tests::HarnessOutput::new(output))
                            } else {
                                Box::new(output)
                            };
                            #[cfg(not(test))]
                            let _ = harness;
                            if let Err(e) = parent_binary(output, &state) {
                                state.fail(format!("desktop binary: {e}"));
                            }
                        },
                    )?);
                Ok(())
            })();
        if let Err(e) = result {
            owner.shared.fail(format!(
                "catalog pipe startup failed; process retained: {e}"
            ));
            return Ok(owner);
        }
        #[cfg(test)]
        if harness {
            let child = Arc::new(Mutex::new(owner.child.take()));
            owner.fixture_child = Some(child.clone());
            let retained = Arc::new(Mutex::new(Some(std::mem::take(&mut owner.threads))));
            let io = retained.clone();
            let state = shared.clone();
            match thread::Builder::new()
                .name("fixture-catalog-reap".into())
                .spawn(move || {
                    loop {
                        let result = {
                            let mut slot = child.lock().unwrap_or_else(|e| e.into_inner());
                            match slot.as_mut().unwrap().try_wait() {
                                Ok(Some(status)) => {
                                    slot.take();
                                    Some(Ok(status))
                                }
                                Ok(None) => None,
                                Err(e) => Some(Err(e)),
                            }
                        };
                        if let Some(result) = result {
                            let threads = io.lock().unwrap().take().unwrap();
                            match result {
                                Ok(status) => {
                                    finish_wait(&state, status, threads);
                                    return (None, vec![]);
                                }
                                Err(e) => {
                                    state.fail(format!("fixture C wait failed: {e}"));
                                    return (child.lock().unwrap().take(), threads);
                                }
                            }
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                }) {
                Ok(thread) => owner.supervisor = Some(thread),
                Err(e) => {
                    owner.child = owner.fixture_child.as_ref().unwrap().lock().unwrap().take();
                    owner.threads = retained.lock().unwrap().take().unwrap();
                    owner.shared.fail(format!("fixture reaper startup: {e}"));
                }
            }
            return Ok(owner);
        }
        // Put ownership in a shared slot before spawning so failed thread creation
        // cannot drop an un-waited Child or lose its live pipe owners.
        let slot = Arc::new(Mutex::new(Some((
            owner.child.take().unwrap(),
            std::mem::take(&mut owner.threads),
        ))));
        let owned = slot.clone();
        #[cfg(all(test, unix))]
        let test_reap_gate = test_reap::take();
        match thread::Builder::new()
            .name("desktop-reap".into())
            .spawn(move || {
                let (child, threads) = owned.lock().unwrap().take().unwrap();
                let mut child = Some(child);
                #[cfg(all(test, unix))]
                if let Some(gate) = &test_reap_gate {
                    gate.before_wait(child.as_mut().unwrap());
                }
                let waited = wait_owned(&mut child, Child::wait);
                #[cfg(all(test, unix))]
                if let Some(gate) = &test_reap_gate {
                    gate.after_wait(&waited);
                }
                match waited {
                    Ok(status) => {
                        finish_wait(&shared, status, threads);
                        (None, vec![])
                    }
                    Err(e) => {
                        shared.fail(format!("desktop process wait failed; owner retained: {e}"));
                        let attempt = shared.state.lock().unwrap().shutdown_attempt;
                        let _ = shared
                            .drain_failed(attempt, format!("desktop process wait failed: {e}"));
                        (child, threads)
                    }
                }
            }) {
            Ok(handle) => owner.supervisor = Some(handle),
            Err(e) => {
                let (child, threads) = slot.lock().unwrap().take().unwrap();
                owner.child = Some(child);
                owner.threads = threads;
                owner.shared.fail(format!(
                    "catalog supervisor startup failed; process retained: {e}"
                ));
                return Ok(owner);
            }
        }
        Ok(owner)
    }
    #[cfg(test)]
    pub fn terminate_no_descendant_fixture(&mut self) -> Result<()> {
        if let Some(slot) = &self.fixture_child {
            let mut child = slot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(child) = child.as_mut() {
                child
                    .kill()
                    .map_err(|e| error(ErrorCode::Native, e.to_string()))?;
            }
        } else if let Some(child) = &mut self.child {
            child
                .kill()
                .map_err(|e| error(ErrorCode::Native, e.to_string()))?;
        }
        let result = self.drain();
        if self.shared.state.lock().unwrap().child_finished {
            Ok(())
        } else {
            result
        }
    }
    #[cfg(test)]
    pub fn spawn_test(
        executable: &std::path::Path,
        args: &[std::ffi::OsString],
        shared: Arc<Shared>,
        hello: Vec<u8>,
    ) -> anyhow::Result<Self> {
        Self::spawn_configured(executable, args, shared, hello, true)
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
fn migration_recovery(message: &Message) -> bool {
    message.kind == Kind::MigrationAdmission
        && message.bytes.len() <= wire::CHUNK
        && serde_json::from_slice::<super::lightroom_migration::Request>(&message.bytes)
            .is_ok_and(|request| request.action.recovery())
}

/// Two bounded partial messages, never an id-indexed allocation map. Small
/// complete controls may interrupt either lane without taking its custody.
#[derive(Default)]
struct MigrationAssemblies {
    ordinary: Option<Assembly>,
    migration: Option<Assembly>,
}
impl MigrationAssemblies {
    fn incomplete(&self) -> bool {
        self.ordinary.is_some() || self.migration.is_some()
    }
    fn push(&mut self, frame: Frame, cap: usize) -> std::io::Result<Option<(Kind, u64, Vec<u8>)>> {
        let migration = matches!(frame.kind, Kind::MigrationAdmission | Kind::MigrationReply);
        if frame.offset == 0 && frame.total == frame.payload.len() {
            if frame.total > cap
                || (!matches!(frame.kind, Kind::Ready | Kind::DrainError | Kind::Drained)
                    && [&self.ordinary, &self.migration]
                        .iter()
                        .any(|slot| slot.as_ref().is_some_and(|a| a.id() == frame.id)))
            {
                return Err(wire::invalid("inline message admission/identity"));
            }
            return Ok(Some((frame.kind, frame.id, frame.payload)));
        }
        let (slot, other) = if migration {
            (&mut self.migration, &self.ordinary)
        } else {
            (&mut self.ordinary, &self.migration)
        };
        if other.as_ref().is_some_and(|a| a.id() == frame.id) {
            return Err(wire::invalid("partial message changed lane"));
        }
        let assembly = match slot.as_mut() {
            Some(value) => value,
            None => slot.insert(Assembly::start(&frame, cap)?),
        };
        if !assembly.push(frame)? {
            return Ok(None);
        }
        Ok(Some(slot.take().unwrap().finish()))
    }
}

pub(super) fn next_outgoing(shared: &Shared, active: &mut Option<Message>) -> Option<Message> {
    let mut s = shared.state.lock().unwrap();
    loop {
        if s.reaped {
            return None;
        }
        if s.stopping {
            super::lightroom_migration::reject_queued_authority(&mut s);
            if let Some(index) = s.control.iter().position(|m| {
                matches!(
                    m.kind,
                    Kind::Ack | Kind::DrainAck | Kind::MigrationAdmission
                )
            }) {
                return s.control.remove(index);
            }
            if s.shutdown_sent < s.shutdown_attempt {
                s.shutdown_sent = s.shutdown_attempt;
                return Some(Message::new(Kind::Shutdown, s.shutdown_attempt, vec![]));
            }
            if active
                .as_ref()
                .is_some_and(|m| m.kind == Kind::MigrationAdmission)
            {
                return active.take();
            }
            s = shared
                .wake
                .wait_timeout(s, Duration::from_millis(2))
                .unwrap()
                .0;
            if shared.filesystem.is_some() {
                return None;
            }
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
        if let Some(index) = s.control.iter().position(migration_recovery) {
            return s.control.remove(index);
        }
        if active
            .as_ref()
            .is_some_and(|m| m.kind == Kind::MigrationAdmission)
        {
            return active.take();
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
        s = shared
            .wake
            .wait_timeout(s, Duration::from_millis(2))
            .unwrap()
            .0;
        if shared.filesystem.is_some() {
            return None;
        }
    }
}
#[derive(Default)]
struct ParentMessages {
    ordinary: Option<Message>,
    migration: Option<Message>,
}
impl ParentMessages {
    fn next(&mut self, shared: &Shared) -> Option<Frame> {
        let slot = if self.migration.is_some() {
            &mut self.migration
        } else {
            &mut self.ordinary
        };
        let mut message = next_outgoing(shared, slot)?;
        let frame = message.next(shared.session);
        if !message.finished() {
            if message.kind == Kind::MigrationAdmission {
                self.migration = Some(message);
            } else {
                self.ordinary = Some(message);
            }
        }
        Some(frame)
    }
}
fn parent_write(mut w: impl Write, shared: &Shared, hello: Vec<u8>) -> std::io::Result<()> {
    Message::new(Kind::Hello, 0, hello).write(shared.session, &mut w)?;
    let mut messages = ParentMessages::default();
    let mut relay: Option<RelayOutput> = None;
    let mut admission: Option<RelayOutput> = None;
    let mut store: Option<RelayOutput> = None;
    loop {
        if shared.state.lock().unwrap().reaped {
            return Ok(());
        }
        if let Some(owner) = &shared.filesystem {
            if let Err(error) = owner.healthy()
                && !shared.state.lock().unwrap().stopping
            {
                shared.fail(
                    crate::filesystem_worker::wire::Failure::new(
                        crate::filesystem_worker::wire::FailureKind::Unknown,
                        format_args!("filesystem owner unavailable; paired C must drain: {error}"),
                    )
                    .message,
                );
            }
            if admission.is_none() {
                admission = owner
                    .next(filesystem::Lane::Admission)
                    .map(|out| RelayOutput { out, offset: 0 });
            }
            if let Some(m) = &mut admission {
                m.frame(shared.session).write(&mut w)?;
                if m.done() {
                    admission = None;
                }
            }
            if store.is_none() {
                store = owner
                    .next(filesystem::Lane::Store)
                    .map(|out| RelayOutput { out, offset: 0 });
            }
            if let Some(message) = &mut store {
                message.frame(shared.session).write(&mut w)?;
                if message.done() {
                    store = None;
                }
            }
            if let Some(out) = owner.next(filesystem::Lane::Control) {
                RelayOutput { out, offset: 0 }
                    .frame(shared.session)
                    .write(&mut w)?;
            }
            if relay.is_none() {
                relay = owner
                    .next(filesystem::Lane::Data)
                    .map(|out| RelayOutput { out, offset: 0 });
            }
            if let Some(m) = &mut relay {
                m.frame(shared.session).write(&mut w)?;
                if m.done() {
                    relay = None;
                }
            }
        }
        if let Some(frame) = messages.next(shared) {
            frame.write(&mut w)?;
        }
    }
}
fn session(f: &Frame, shared: &Shared) -> std::io::Result<()> {
    if f.session != shared.session {
        return Err(wire::invalid("stale desktop session frame"));
    }
    Ok(())
}
fn parent_control(mut r: impl Read, shared: &Shared) -> std::io::Result<()> {
    let mut relay = RelayAssembly::default();
    let mut assembly = MigrationAssemblies::default();
    while let Some(f) = Frame::read(&mut r)? {
        session(&f, shared)?;
        #[cfg(test)]
        if f.kind == Kind::Fixture {
            if f.offset != 0 || f.total != f.payload.len() || f.total > wire::CHUNK {
                return Err(wire::invalid("fixture report bound"));
            }
            let report = serde_json::from_slice(&f.payload).map_err(std::io::Error::other)?;
            *shared.fixture.lock().unwrap() = Some(report);
            shared.wake.notify_all();
            continue;
        }
        if matches!(
            f.kind,
            Kind::Filesystem
                | Kind::FilesystemControl
                | Kind::FilesystemAdmission
                | Kind::FilesystemStore
        ) {
            let owner = shared
                .filesystem
                .as_ref()
                .ok_or_else(|| wire::invalid("unselected filesystem relay"))?;
            if let Some((bytes, control)) =
                relay_input(&mut relay, f).map_err(std::io::Error::other)?
            {
                owner
                    .receive(&bytes, control)
                    .map_err(std::io::Error::other)?;
            }
            continue;
        }
        if !matches!(
            f.kind,
            Kind::Ready
                | Kind::Reply
                | Kind::BytesError
                | Kind::DrainError
                | Kind::Drained
                | Kind::MigrationReply
                | Kind::BackupAdmissionReply
        ) {
            return Err(wire::invalid("unexpected control frame"));
        }
        let cap = match f.kind {
            Kind::MigrationReply => shared.limits.reply_bytes.max(wire::CHUNK),
            Kind::Ready | Kind::DrainError | Kind::Drained => wire::CHUNK,
            _ => shared.limits.reply_bytes.max(wire::ERROR_BYTES),
        };
        let Some((kind, id, bytes)) = assembly.push(f, cap)? else {
            continue;
        };
        if kind == Kind::Drained {
            let mut s = shared.state.lock().unwrap();
            if id < s.shutdown_attempt {
                continue;
            }
            if id == 0 || id != s.shutdown_attempt || !bytes.is_empty() {
                return Err(wire::invalid("unowned catalog drain completion"));
            }
            s.control
                .push_back(Message::new(Kind::DrainAck, id, vec![]));
            shared.wake.notify_all();
            continue;
        }
        if kind == Kind::DrainError {
            let error: BridgeError = serde_json::from_slice(&bytes)
                .map_err(|_| wire::invalid("drain error response"))?;
            shared.drain_failed(id, error.message)?;
            continue;
        }
        if kind == Kind::Ready {
            if id != 0 || bytes != wire::ready_bytes(shared.filesystem.as_ref().map(|f| &f.binding))
            {
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
        let decoded_migration = if kind == Kind::MigrationReply {
            let reply: super::lightroom_migration::Reply = serde_json::from_slice(&bytes)
                .map_err(|_| wire::invalid("invalid migration reply"))?;
            reply
                .validate()
                .map_err(|_| wire::invalid("migration reply field bounds"))?;
            Some(reply)
        } else {
            None
        };
        let decoded_backup = if kind == Kind::BackupAdmissionReply {
            let reply = serde_json::from_slice::<super::backup::AdmissionReply>(&bytes)
                .map_err(|_| wire::invalid("invalid backup admission reply"))?;
            reply
                .validate()
                .map_err(|_| wire::invalid("backup admission reply field bounds"))?;
            Some(reply)
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
        if bytes.len() > shared.limits.reply_bytes
            && decoded_reply
                .as_ref()
                .is_some_and(|reply| !matches!(reply, Reply::Error { .. }))
        {
            return Err(wire::invalid("success reply exceeds configured budget"));
        }
        if bytes.len() > shared.limits.reply_bytes
            && decoded_migration
                .as_ref()
                .is_some_and(|reply| matches!(reply, super::lightroom_migration::Reply::Ok(_)))
        {
            return Err(wire::invalid(
                "migration success reply exceeds configured allowance",
            ));
        }
        if bytes.len() > shared.limits.reply_bytes
            && decoded_backup
                .as_ref()
                .is_some_and(|reply| matches!(reply, super::backup::AdmissionReply::Ok(_)))
        {
            return Err(wire::invalid(
                "backup admission success reply exceeds configured allowance",
            ));
        }
        let entry = {
            let mut state = shared.state.lock().unwrap();
            let pending = state
                .pending
                .get(&id)
                .ok_or_else(|| wire::invalid("unowned desktop reply"))?;
            if !matches!(
                (kind, &pending.delivery),
                (Kind::Reply, Delivery::Command(_))
                    | (Kind::BytesError, Delivery::Bytes { .. })
                    | (Kind::MigrationReply, Delivery::Migration(_))
                    | (Kind::BackupAdmissionReply, Delivery::BackupAdmission(_))
            ) {
                return Err(wire::invalid("reply kind does not match request"));
            }
            state.pending.remove(&id).unwrap()
        };
        match (kind, entry.delivery) {
            (Kind::BackupAdmissionReply, Delivery::BackupAdmission(tx)) => {
                let _ = tx.send(decoded_backup.unwrap());
            }
            (Kind::MigrationReply, Delivery::Migration(tx)) => {
                let _ = tx.send(decoded_migration.unwrap());
            }
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
    if assembly.incomplete() || relay.incomplete() {
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
    BackupAdmission(super::backup::Pending),
    Migration(super::lightroom_migration::Pending),
    Command(Pending),
    Bytes(PendingBytes, BytesRequest),
    Reply(Message),
    Binary(Arc<PreviewBytes>, BytesRequest),
}
pub(super) fn migration_pending_layout() -> (usize, usize) {
    (
        std::mem::size_of::<(u64, ChildPending)>(),
        std::mem::align_of::<(u64, ChildPending)>(),
    )
}
struct ChildState {
    pending: HashMap<u64, ChildPending>,
    cancels: HashMap<u64, Cancellation>,
    migration: HashMap<u64, bool>,
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
fn output_writer(
    mut w: impl Write,
    rx: mpsc::Receiver<Message>,
    migration_rx: mpsc::Receiver<Message>,
    session: [u8; 16],
    proxy: Option<Arc<filesystem::Proxy>>,
) {
    let mut ordinary: Option<Message> = None;
    let mut migration: Option<Message> = None;
    let mut ordinary_closed = false;
    let mut migration_closed = false;
    let mut relay: Option<RelayOutput> = None;
    let mut admission: Option<RelayOutput> = None;
    let mut store: Option<RelayOutput> = None;
    loop {
        let result = (|| -> std::io::Result<bool> {
            if let Some(proxy) = &proxy {
                if admission.is_none() {
                    admission = proxy
                        .next(filesystem::Lane::Admission)
                        .map(|out| RelayOutput { out, offset: 0 });
                }
                if let Some(m) = &mut admission {
                    m.frame(session).write(&mut w)?;
                    if m.done() {
                        admission = None;
                    }
                }
                if store.is_none() {
                    store = proxy
                        .next(filesystem::Lane::Store)
                        .map(|out| RelayOutput { out, offset: 0 });
                }
                if let Some(message) = &mut store {
                    message.frame(session).write(&mut w)?;
                    if message.done() {
                        store = None;
                    }
                }
                if let Some(out) = proxy.next(filesystem::Lane::Control) {
                    RelayOutput { out, offset: 0 }
                        .frame(session)
                        .write(&mut w)?;
                }
                if relay.is_none() {
                    relay = proxy
                        .next(filesystem::Lane::Data)
                        .map(|out| RelayOutput { out, offset: 0 });
                }
                if let Some(m) = &mut relay {
                    m.frame(session).write(&mut w)?;
                    if m.done() {
                        relay = None;
                    }
                }
            }
            if migration.is_none() {
                match migration_rx.try_recv() {
                    Ok(message) => migration = Some(message),
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => migration_closed = true,
                }
            }
            // Migration has reserved output capacity and gets a frame before
            // ordinary replies. Each class retains exactly one partial message.
            if let Some(message) = &mut migration {
                message.next(session).write(&mut w)?;
                if message.finished() {
                    migration = None;
                }
            }
            if ordinary.is_none() {
                match rx.recv_timeout(Duration::from_millis(2)) {
                    Ok(m) => ordinary = Some(m),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => ordinary_closed = true,
                }
            }
            if let Some(m) = &mut ordinary {
                m.next(session).write(&mut w)?;
                if m.finished() {
                    ordinary = None;
                }
            }
            if ordinary_closed && migration_closed && ordinary.is_none() && migration.is_none() {
                return Ok(false);
            }
            Ok(true)
        })();
        if !matches!(result, Ok(true)) {
            if let Some(proxy) = &proxy {
                proxy.fail("catalog control output ended");
            }
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
    assert_ne!(
        kind,
        Kind::MigrationReply,
        "migration replies use their bounded identity-preserving encoder"
    );
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    if bytes.len() <= limit {
        Message::new(kind, id, bytes)
    } else {
        let e = error(ErrorCode::ResourceLimit, "desktop reply byte limit");
        let bytes = if kind == Kind::Reply {
            serde_json::to_vec(&Reply::Error { error: e }).unwrap()
        } else if kind == Kind::BackupAdmissionReply {
            serde_json::to_vec(&super::backup::AdmissionReply::error(e)).unwrap()
        } else {
            serde_json::to_vec(&e).unwrap()
        };
        Message::new(kind, id, bytes)
    }
}
fn collect(
    shared: &ChildShared,
    tx: &mpsc::SyncSender<Message>,
    migration: &mpsc::SyncSender<Message>,
    binary: &mpsc::SyncSender<BinaryOutput>,
    limit: usize,
) {
    let mut state = shared.lock().unwrap();
    let ids = state.pending.keys().copied().collect::<Vec<_>>();
    for id in ids {
        let p = state.pending.remove(&id).unwrap();
        let ready = match p {
            ChildPending::BackupAdmission(p) => match p.receiver.try_recv() {
                Ok(reply) => ChildPending::Reply(checked_message(
                    Kind::BackupAdmissionReply,
                    id,
                    &reply,
                    limit,
                )),
                Err(mpsc::TryRecvError::Empty) => {
                    state.pending.insert(id, ChildPending::BackupAdmission(p));
                    continue;
                }
                Err(_) => ChildPending::Reply(checked_message(
                    Kind::BackupAdmissionReply,
                    id,
                    &super::backup::AdmissionReply::error(error(
                        ErrorCode::Closed,
                        "backup admission actor disconnected",
                    )),
                    limit,
                )),
            },
            ChildPending::Migration(p) => match p.receiver.try_recv() {
                Ok(reply) => ChildPending::Reply(reply.message(id, limit)),
                Err(mpsc::TryRecvError::Empty) => {
                    state.pending.insert(id, ChildPending::Migration(p));
                    continue;
                }
                Err(_) => ChildPending::Reply(
                    super::lightroom_migration::Reply::Error(error(
                        ErrorCode::Closed,
                        "migration actor disconnected",
                    ))
                    .message(id, limit),
                ),
            },
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
            ChildPending::Reply(m) => match (if m.kind == Kind::MigrationReply {
                migration
            } else {
                tx
            })
            .try_send(m)
            {
                Ok(()) => {
                    state.cancels.remove(&id);
                    state.migration.remove(&id);
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
struct DrainState {
    attempt: u64,
    verified: bool,
    failed: bool,
    running: bool,
}
struct ChildEngine {
    bridge: Bridge,
    state: Arc<Mutex<DrainState>>,
    owner: Mutex<Option<thread::JoinHandle<()>>>,
    #[cfg(test)]
    fixture: Arc<Mutex<Option<super::filesystem_tests::Driver>>>,
}
impl ChildEngine {
    fn request(&self, attempt: u64, tx: &mpsc::SyncSender<Message>) -> Result<bool> {
        let mut state = self.state.lock().unwrap();
        if state.verified {
            return Ok(true);
        }
        if state.attempt == attempt {
            return Ok(false);
        }
        if state.running {
            return Err(error(ErrorCode::Busy, "catalog drain already running"));
        }
        if attempt <= state.attempt {
            return Err(error(
                ErrorCode::InvalidRequest,
                "catalog drain attempt identity",
            ));
        }
        if let Some(owner) = self.owner.lock().unwrap().take() {
            drop(state);
            let joined = owner.join();
            state = self.state.lock().unwrap();
            if joined.is_err() {
                return Err(error(ErrorCode::Native, "catalog drain owner panicked"));
            }
        }
        state.attempt = attempt;
        state.running = true;
        state.failed = false;
        #[cfg(test)]
        let fixture = self.fixture.clone();
        let bridge = self.bridge.clone();
        let status = self.state.clone();
        let tx = tx.clone();
        let worker = thread::Builder::new()
            .name("catalog-checked-drain".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(test)]
                    if let Some(owner) = fixture.lock().unwrap().take() {
                        let _ = owner.release.send(());
                        owner
                            .owner
                            .join()
                            .map_err(|_| error(ErrorCode::Native, "SQL fixture owner panic"))?
                            .map_err(|e| error(ErrorCode::Native, e.to_string()))?;
                    }
                    bridge.try_shutdown()
                }))
                .unwrap_or_else(|_| {
                    Err(error(
                        ErrorCode::Native,
                        "catalog drain panicked; owners retained",
                    ))
                });
                {
                    let mut s = status.lock().unwrap();
                    s.running = false;
                    s.verified = result.is_ok();
                    s.failed = result.is_err();
                }
                let message = match result {
                    Ok(()) => Message::new(Kind::Drained, attempt, vec![]),
                    Err(e) => checked_message(Kind::DrainError, attempt, &e, wire::CHUNK),
                };
                let _ = tx.send(message);
            });
        match worker {
            Ok(owner) => *self.owner.lock().unwrap() = Some(owner),
            Err(e) => {
                state.running = false;
                state.failed = true;
                return Err(error(ErrorCode::Native, e.to_string()));
            }
        }
        Ok(false)
    }
    fn verified(&self) -> bool {
        self.state.lock().unwrap().verified
    }
    fn finish(&self) -> Result<()> {
        if let Some(owner) = self.owner.lock().unwrap().take() {
            owner
                .join()
                .map_err(|_| error(ErrorCode::Native, "catalog drain join failed"))?;
        }
        if self.verified() {
            Ok(())
        } else {
            Err(error(ErrorCode::Native, "catalog engine drain unresolved"))
        }
    }
}
impl Drop for ChildEngine {
    fn drop(&mut self) {
        // With no reply channel, an unresolved managed close may be waiting on F.
        // Retain the entire process; EOF is never permission to exit with owners.
        if !self.verified() {
            loop {
                thread::park();
            }
        }
    }
}
pub(super) fn worker_main() -> anyhow::Result<()> {
    // Dedicated helper stderr is framed; caught panics must not corrupt it.
    std::panic::set_hook(Box::new(|_| {}));
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
    let proxy = config.filesystem.clone().map(filesystem::Proxy::new);
    #[cfg(test)]
    let fixture = config.fixture.clone();
    let config = config.into_config()?;
    let limits = config.limits.clone();
    let bridge = if let Some(proxy) = &proxy {
        Bridge::spawn_managed(
            config,
            super::super::ManagedCatalogConfig {
                filesystem: proxy.clone(),
            },
        )?
    } else {
        Bridge::spawn(config)?
    };
    let engine = ChildEngine {
        bridge,
        state: Arc::new(Mutex::new(DrainState {
            attempt: 0,
            verified: false,
            failed: false,
            running: false,
        })),
        owner: Mutex::new(None),
        #[cfg(test)]
        fixture: Arc::new(Mutex::new(None)),
    };
    let bridge = &engine.bridge;
    let shared = Arc::new(Mutex::new(ChildState {
        pending: HashMap::new(),
        cancels: HashMap::new(),
        migration: HashMap::new(),
        early: Default::default(),
        retained: HashMap::new(),
        stopping: false,
    }));
    let (tx, rx) = mpsc::sync_channel(limits.queued + CONTROL_SLOTS);
    let (migration_tx, migration_rx) = mpsc::sync_channel(CONTROL_SLOTS + 1);
    let (binary, binary_rx) = mpsc::sync_channel(1);
    let output_proxy = proxy.clone();
    let control = thread::spawn(move || {
        output_writer(
            std::io::stderr().lock(),
            rx,
            migration_rx,
            session,
            output_proxy,
        )
    });
    let data = thread::spawn(move || binary_writer(std::io::stdout().lock(), binary_rx, session));
    let state = shared.clone();
    let results = tx.clone();
    let result_limit = limits.reply_bytes;
    let collector = thread::spawn(move || {
        while !state.lock().unwrap().stopping {
            collect(&state, &results, &migration_tx, &binary, result_limit);
            thread::sleep(Duration::from_millis(2));
        }
    });
    #[cfg(test)]
    if let Some(proxy) = &proxy
        && let Some(driver) =
            super::filesystem_tests::start_driver(fixture, proxy.clone(), tx.clone(), session)?
    {
        *engine.fixture.lock().unwrap() = Some(driver);
    }
    let ready = tx.send(Message::new(
        Kind::Ready,
        0,
        wire::ready_bytes(proxy.as_ref().map(|p| p.binding())),
    ));
    let result = ready.map_err(anyhow::Error::from).and_then(|_| {
        child_input_relay(
            &mut input,
            session,
            &mut |kind, bytes| {
                if proxy.is_some()
                    && !matches!(kind, Kind::MigrationAdmission | Kind::BackupAdmission)
                    && !paired_catalog_route(kind, bytes)?
                {
                    return Ok(Err(error(
                        ErrorCode::InvalidRequest,
                        "request unavailable on the managed preview custody route",
                    )));
                }
                dispatch(bridge, kind, bytes)
            },
            &shared,
            &limits,
            &tx,
            &mut |attempt| engine.request(attempt, &tx),
            proxy.as_deref(),
        )
    });
    // Never force-exit: first drain the engine and its descendants, then transport.
    {
        let state = shared.lock().unwrap();
        for c in state.cancels.values() {
            c.cancel();
        }
    }
    if let Some(proxy) = &proxy {
        proxy.fail("catalog input ended; filesystem outcomes unknown");
    }
    // Keep ordinary legacy EOF cleanup compatible. Managed errors preserve R0
    // and retain unresolved process ownership instead of fabricating cleanup.
    if !engine.verified() && proxy.is_none() {
        let attempt = engine.state.lock().unwrap().attempt + 1;
        let _ = engine.request(attempt, &tx);
    }
    engine.finish()?;
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
// Explicit paired admission covers the qualified preview surface. The default
// desktop constructor stays legacy until the remaining custody routes qualify.
fn paired_catalog_route(kind: Kind, bytes: &[u8]) -> anyhow::Result<bool> {
    if kind == Kind::Bytes {
        return Ok(true);
    }
    if kind != Kind::Command {
        return Ok(false);
    }
    // Catalog commands execute in C. Independent G owners have no public
    // fallback into C; a new request variant requires an explicit owner.
    Ok(match serde_json::from_slice::<Request>(bytes)? {
        Request::Lightroom { .. }
        | Request::LightroomMigration { .. }
        | Request::BackupCreate { .. }
        | Request::BackupInspect { .. }
        | Request::BackupRestore { .. }
        | Request::BackupStatus
        | Request::BackupCancel { .. } => false,
        Request::OpenExisting { .. }
        | Request::Create { .. }
        | Request::Status
        | Request::Close { .. }
        | Request::ImportStart { .. }
        | Request::ImportResume { .. }
        | Request::ImportStatus { .. }
        | Request::ImportCancel { .. }
        | Request::RestoreStatus { .. }
        | Request::ResumeRestoredJobs { .. }
        | Request::Folders { .. }
        | Request::Images { .. }
        | Request::Search { .. }
        | Request::Image { .. }
        | Request::Variant { .. }
        | Request::Variants { .. }
        | Request::CreateVariant { .. }
        | Request::SaveRecipe { .. }
        | Request::Undo { .. }
        | Request::Redo { .. }
        | Request::History { .. }
        | Request::Cull { .. }
        | Request::Preview { .. }
        | Request::PreviewStatus { .. }
        | Request::CancelPreview { .. }
        | Request::ReleaseViewport { .. }
        | Request::PreviewSettings { .. }
        | Request::Metadata { .. }
        | Request::MetadataWrite { .. }
        | Request::Organization { .. }
        | Request::EditCopy { .. }
        | Request::Relink { .. }
        | Request::Export { .. } => true,
    })
}
fn dispatch(bridge: &Bridge, kind: Kind, bytes: &[u8]) -> anyhow::Result<Result<ChildPending>> {
    if kind == Kind::BackupAdmission {
        let request: super::backup::AdmissionRequest = serde_json::from_slice(bytes)?;
        request.validate()?;
        return Ok(bridge
            .backup_admission(request)
            .map(ChildPending::BackupAdmission));
    }
    if kind == Kind::MigrationAdmission {
        return Ok(bridge
            .migration_admission(serde_json::from_slice(bytes)?)
            .map(ChildPending::Migration));
    }
    if kind == Kind::Command {
        let request: Request = serde_json::from_slice(bytes)?;
        anyhow::ensure!(
            !local_route(&request),
            "Workbench cannot enter catalog child"
        );
        Ok(bridge.submit(request).map(ChildPending::Command))
    } else {
        let r: BytesRequest = serde_json::from_slice(bytes)?;
        Ok(bridge
            .preview_bytes(r.catalog.clone(), r.ticket.clone(), r.foreground)
            .map(|p| ChildPending::Bytes(p, r)))
    }
}
// Keep the distinct input, completion, drain, and optional filesystem owners
// explicit at this single protocol dispatch boundary.
#[allow(clippy::too_many_arguments)]
fn child_input_relay(
    input: &mut impl Read,
    session: [u8; 16],
    dispatch: &mut impl FnMut(Kind, &[u8]) -> anyhow::Result<Result<ChildPending>>,
    shared: &ChildShared,
    limits: &Limits,
    tx: &mpsc::SyncSender<Message>,
    drain: &mut impl FnMut(u64) -> Result<bool>,
    proxy: Option<&filesystem::Proxy>,
) -> anyhow::Result<()> {
    let mut active = MigrationAssemblies::default();
    let mut seen = Seen::default();
    let mut drain_attempt = 0;
    let mut closing = false;
    let mut relay = RelayAssembly::default();
    while let Some(f) = Frame::read(input)? {
        anyhow::ensure!(f.session == session, "stale child session");
        if matches!(
            f.kind,
            Kind::Filesystem
                | Kind::FilesystemControl
                | Kind::FilesystemAdmission
                | Kind::FilesystemStore
        ) {
            let proxy = proxy.context("unselected child filesystem relay")?;
            if let Some((bytes, control)) = relay_input(&mut relay, f)? {
                proxy.receive(&bytes, control)?;
            }
            continue;
        }
        if matches!(
            f.kind,
            Kind::Cancel | Kind::Ack | Kind::Shutdown | Kind::DrainAck
        ) {
            anyhow::ensure!(
                f.offset == 0 && f.total == 0 && f.payload.is_empty(),
                "invalid control payload"
            );
            let mut state = shared.lock().unwrap();
            match f.kind {
                Kind::DrainAck => {
                    anyhow::ensure!(
                        f.id == drain_attempt && f.id != 0,
                        "catalog drain acknowledgement identity"
                    );
                    drop(state);
                    anyhow::ensure!(
                        drain(f.id)?,
                        "catalog drain acknowledgement before completion"
                    );
                    return Ok(());
                }
                Kind::Shutdown => {
                    anyhow::ensure!(f.id > drain_attempt, "shutdown attempt identity");
                    drain_attempt = f.id;
                    closing = true;
                    drop(state);
                    if let Some(proxy) = proxy {
                        proxy.closing();
                    }
                    match drain(f.id) {
                        Ok(true) => return Ok(()),
                        Ok(false) => continue,
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
                            state.early.len() < limits.queued + 2 * CONTROL_SLOTS + 1,
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
            matches!(
                f.kind,
                Kind::Command | Kind::Bytes | Kind::MigrationAdmission | Kind::BackupAdmission
            ),
            "unexpected request kind"
        );
        let request_cap = if f.kind == Kind::MigrationAdmission {
            limits.request_bytes.max(wire::CHUNK)
        } else {
            limits.request_bytes
        };
        let Some((kind, id, bytes)) = active.push(f, request_cap)? else {
            continue;
        };
        let recovery = if kind == Kind::MigrationAdmission {
            let request: super::lightroom_migration::Request = serde_json::from_slice(&bytes)?;
            request.validate()?;
            anyhow::ensure!(
                bytes.len()
                    <= if request.action.recovery() {
                        wire::CHUNK
                    } else {
                        limits.request_bytes
                    },
                "migration configured request allowance"
            );
            Some(request.action.recovery())
        } else {
            None
        };
        seen.insert(id, limits.queued + 2 * CONTROL_SLOTS + 2)?;
        // Only this input loop admits requests; the collector can only remove
        // pending entries. Check capacity and consume a winning early cancel
        // before calling any engine entry point, including synchronous commands.
        let canceled = {
            let mut state = shared.lock().unwrap();
            anyhow::ensure!(
                state.pending.len() < limits.queued + 2 * CONTROL_SLOTS + 1,
                "child pending bounds"
            );
            if let Some(recovery) = recovery {
                anyhow::ensure!(
                    state.migration.len() < CONTROL_SLOTS + 1,
                    "migration pending bounds"
                );
                anyhow::ensure!(
                    closing
                        || state
                            .migration
                            .values()
                            .filter(|value| **value == recovery)
                            .count()
                            < if recovery { CONTROL_SLOTS } else { 1 },
                    "migration reserved recovery admission"
                );
                state.migration.insert(id, recovery);
            } else {
                anyhow::ensure!(
                    state
                        .pending
                        .keys()
                        .filter(|id| !state.migration.contains_key(*id))
                        .count()
                        < limits.queued + CONTROL_SLOTS,
                    "ordinary pending bounds"
                );
            }
            state.early.remove(&id)
        };
        let during_close = closing
            && recovery != Some(true)
            && !(kind == Kind::Command
                && serde_json::from_slice::<Request>(&bytes)
                    .is_ok_and(|request| matches!(request, Request::Status)));
        let admitted = if during_close {
            Err(error(
                ErrorCode::Closed,
                "catalog transport is closing; new actor work rejected",
            ))
        } else if canceled {
            Err(error(
                ErrorCode::Canceled,
                "request canceled before desktop admission",
            ))
        } else {
            dispatch(kind, &bytes)?
        };
        let mut state = shared.lock().unwrap();
        match admitted {
            Ok(p) => {
                let c = match &p {
                    ChildPending::BackupAdmission(p) => p.cancel.clone(),
                    ChildPending::Migration(p) => p.cancel.clone(),
                    ChildPending::Command(p) => p.cancellation(),
                    ChildPending::Bytes(p, _) => p.cancellation(),
                    _ => unreachable!(),
                };
                state.cancels.insert(id, c);
                state.pending.insert(id, p);
            }
            Err(e) => {
                state.early.remove(&id);
                let m = if kind == Kind::MigrationAdmission {
                    // Dispatch either never entered the actor or failed to
                    // enqueue. No action has executed on this refusal path.
                    super::lightroom_migration::Reply::Refused(e).message(id, limits.reply_bytes)
                } else if kind == Kind::BackupAdmission {
                    checked_message(
                        Kind::BackupAdmissionReply,
                        id,
                        &super::backup::AdmissionReply::error(e),
                        limits.reply_bytes,
                    )
                } else if kind == Kind::Command {
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
    anyhow::ensure!(
        !active.incomplete() && !relay.incomplete(),
        "truncated desktop request"
    );
    Ok(())
}

#[cfg(test)]
fn child_input(
    input: &mut impl Read,
    session: [u8; 16],
    dispatch: &mut impl FnMut(Kind, &[u8]) -> anyhow::Result<Result<ChildPending>>,
    shared: &ChildShared,
    limits: &Limits,
    tx: &mpsc::SyncSender<Message>,
    drain: &mut impl FnMut() -> Result<()>,
) -> anyhow::Result<()> {
    child_input_relay(
        input,
        session,
        dispatch,
        shared,
        limits,
        tx,
        &mut |_| drain().map(|()| true),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paired_catalog_route_admits_preview_import_and_export_lifecycle() {
        let path = crate::storage_volume::NativePath::from_path(std::path::Path::new("/fixture"));
        let catalog = "catalog".to_owned();
        for request in [
            Request::OpenExisting { path: path.clone() },
            Request::Create { path },
            Request::Status,
            Request::Close {
                catalog: catalog.clone(),
            },
            Request::ImportStart {
                catalog: catalog.clone(),
                source: crate::storage_volume::NativePath::from_path(std::path::Path::new(
                    "/fixture/source",
                )),
            },
            Request::ImportResume {
                catalog: catalog.clone(),
                source: crate::storage_volume::NativePath::from_path(std::path::Path::new(
                    "/fixture/source",
                )),
            },
            Request::ImportStatus {
                catalog: catalog.clone(),
            },
            Request::ImportCancel {
                catalog: catalog.clone(),
                import: "import".into(),
            },
            Request::Preview {
                catalog: catalog.clone(),
                key: crate::catalog_edits::VariantKey::master("asset"),
                tier: crate::application::PreviewTier::Thumbnail,
                interactive: false,
                viewport: "view".into(),
                generation: crate::application::U64(1),
                foreground: true,
            },
            Request::PreviewStatus {
                catalog: catalog.clone(),
                ticket: "ticket".into(),
            },
            Request::CancelPreview {
                catalog: catalog.clone(),
                ticket: "ticket".into(),
            },
            Request::ReleaseViewport {
                catalog: catalog.clone(),
                viewport: "view".into(),
                generation: crate::application::U64(1),
            },
            Request::Export {
                catalog: catalog.clone(),
                request: Box::new(crate::application::exports::Request::Options),
            },
        ] {
            assert!(
                paired_catalog_route(Kind::Command, &serde_json::to_vec(&request).unwrap())
                    .unwrap()
            );
        }
        assert!(paired_catalog_route(Kind::Bytes, b"{}").unwrap());
        assert!(!paired_catalog_route(Kind::Hello, b"{}").unwrap());
    }

    #[test]
    fn paired_route_admits_every_nested_export_request() {
        use crate::{
            application::exports as export,
            image_export::{AlphaPolicy, IntegerDepth, OutputFormat, OutputSize},
        };
        let path = crate::storage_volume::NativePath::from_path(std::path::Path::new("/fixture"));
        let target = export::Target {
            key: crate::catalog_edits::VariantKey::master("asset"),
            expected_revision: crate::application::I64(0),
            destination: path.clone(),
            overwrite: false,
            metadata: export::Metadata::Omit,
        };
        let output = export::Output {
            size: OutputSize::Original,
            format: OutputFormat::Png {
                depth: IntegerDepth::Eight,
            },
            profile: export::Profile::Srgb,
            alpha: AlphaPolicy::Preserve,
        };
        let budgets = export::Budgets {
            max_original_bytes: crate::application::U64(1),
            max_payload_bytes: crate::application::U64(1),
            alias_limits: export::AliasLimits {
                directories: crate::application::U64(1),
                candidates: crate::application::U64(1),
            },
        };
        let render = export::RenderLimits {
            max_pixels: crate::application::U64(1),
            max_allocation_bytes: crate::application::U64(1),
            max_live_bytes: crate::application::U64(1),
        };
        let execution = export::ExecutionLimits {
            worker_bytes: crate::application::U64(1),
            working_bytes: crate::application::U64(1),
            render: export::PhotoLimits {
                decode: export::DecodeLimits {
                    max_encoded_bytes: crate::application::U64(1),
                    max_intermediate_pixels: crate::application::U64(1),
                    max_allocation_bytes: crate::application::U64(1),
                },
                render: render.clone(),
                encode: export::EncodeLimits {
                    render,
                    max_metadata_bytes: crate::application::U64(1),
                    row_buffer_bytes: crate::application::U64(1),
                },
                max_encoded_extent: crate::application::U64(1),
            },
        };
        let requests = vec![
            export::Request::Options,
            export::Request::Profile { path: path.clone() },
            export::Request::ProfileRelease {
                token: "token".into(),
            },
            export::Request::Begin,
            export::Request::Paths {
                limit: crate::application::U64(1),
            },
            export::Request::Destinations {
                directory: path,
                targets: vec![export::TargetKey {
                    key: crate::catalog_edits::VariantKey::master("asset"),
                    expected_revision: crate::application::I64(0),
                }],
                format: output.format,
                naming: export::Naming {
                    prefix: "prefix".into(),
                    suffix: "suffix".into(),
                    variant_suffix: true,
                    sequence_start: Some(crate::application::U64(1)),
                },
            },
            export::Request::DestinationRows {
                token: "token".into(),
                after: crate::application::U64(0),
                limit: crate::application::U64(1),
            },
            export::Request::ResultRelease {
                token: "token".into(),
            },
            export::Request::Append {
                job: "job".into(),
                expected_total: crate::application::I64(0),
                target,
                output,
                budgets: Some(budgets),
            },
            export::Request::Seal {
                job: "job".into(),
                expected_total: crate::application::I64(1),
            },
            export::Request::Job { job: "job".into() },
            export::Request::Jobs {
                after: crate::application::I64(0),
                limit: crate::application::U64(1),
            },
            export::Request::Items {
                job: "job".into(),
                after: crate::application::I64(0),
                limit: crate::application::U64(1),
            },
            export::Request::Plan {
                job: "job".into(),
                sequence: crate::application::I64(0),
            },
            export::Request::PlanChunk {
                job: "job".into(),
                sequence: crate::application::I64(0),
                authority: "a".repeat(64),
                offset: crate::application::U64(0),
                bytes: crate::application::U64(1),
            },
            export::Request::Run {
                job: "job".into(),
                limits: Some(execution.clone()),
                max_items: crate::application::U64(1),
                max_seconds: crate::application::U64(1),
            },
            export::Request::Status { operation: None },
            export::Request::Cancel {
                job: Some("job".into()),
                operation: Some("operation".into()),
            },
            export::Request::Yield {
                job: "job".into(),
                operation: "operation".into(),
            },
            export::Request::Recover {
                directories: crate::application::U64(1),
                limits: Some(execution),
            },
            export::Request::RetrySeal {
                job: "job".into(),
                sequence: crate::application::I64(0),
                authority: "a".repeat(64),
            },
            export::Request::Restore {
                job: "job".into(),
                sequence: crate::application::I64(0),
                authority: "a".repeat(64),
            },
        ];
        assert_eq!(requests.len(), 22);
        for request in requests {
            let outer = Request::Export {
                catalog: "catalog".into(),
                request: Box::new(request),
            };
            assert!(
                paired_catalog_route(Kind::Command, &serde_json::to_vec(&outer).unwrap()).unwrap()
            );
        }
    }
    fn child_state() -> ChildShared {
        Arc::new(Mutex::new(ChildState {
            pending: HashMap::new(),
            cancels: HashMap::new(),
            migration: HashMap::new(),
            early: Default::default(),
            retained: HashMap::new(),
            stopping: false,
        }))
    }
    #[test]
    fn facade_closed_waits_for_held_local_drain_failure_and_explicit_retry() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        let shared = super::super::tests::shared(8);
        shared.stop();
        finish_wait(&shared, std::process::ExitStatus::from_raw(0), vec![]);
        {
            let state = shared.state.lock().unwrap();
            assert!(state.reaped && state.child_finished);
            assert!(!state.local_verified);
            assert_eq!(state.phase, TransportPhase::Draining);
        }
        for succeeds in [false, true] {
            shared.stop(); // The second pass is an explicit retry of the retained local owner.
            let (entered, observed) = mpsc::sync_channel(1);
            let (release, released) = mpsc::sync_channel(1);
            let owned = shared.clone();
            let drain = thread::spawn(move || {
                owned.drain_local(|| {
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    if succeeds {
                        Ok(())
                    } else {
                        Err(error(ErrorCode::Native, "held local drain failed"))
                    }
                })
            });
            observed.recv().unwrap();
            assert_eq!(shared.state.lock().unwrap().phase, TransportPhase::Draining);
            assert!(!shared.state.lock().unwrap().local_verified);
            release.send(()).unwrap();
            assert_eq!(drain.join().unwrap().is_ok(), succeeds);
            let state = shared.state.lock().unwrap();
            assert_eq!(
                state.phase,
                if succeeds {
                    TransportPhase::Closed
                } else {
                    TransportPhase::Draining
                }
            );
            assert_eq!(state.local_verified, succeeds);
        }
        // Reversed completion order has the same conjunction: local alone is insufficient.
        let shared = super::super::tests::shared(8);
        shared.stop();
        shared.drain_local(|| Ok(())).unwrap();
        assert_eq!(shared.state.lock().unwrap().phase, TransportPhase::Draining);
        finish_wait(&shared, std::process::ExitStatus::from_raw(0), vec![]);
        assert_eq!(shared.state.lock().unwrap().phase, TransportPhase::Closed);
    }
    #[test]
    fn held_binary_output_does_not_block_command_completion_or_free_child_bytes() {
        let shared = child_state();
        let usage = Arc::new(AtomicUsize::new(4));
        let value = Arc::new(PreviewBytes {
            mime: "image/png".into(),
            bytes: b"raw!".to_vec(),
            usage: usage.clone(),
        });
        let request = BytesRequest {
            catalog: "A".into(),
            ticket: "T".into(),
            foreground: true,
        };
        shared
            .lock()
            .unwrap()
            .pending
            .insert(1, ChildPending::Binary(value, request));
        shared.lock().unwrap().pending.insert(
            2,
            ChildPending::Reply(checked_message(
                Kind::Reply,
                2,
                &failure(ErrorCode::Closed, "no catalog"),
                1024,
            )),
        );
        let (tx, rx) = mpsc::sync_channel(1);
        let (blocked, _held) = mpsc::sync_channel(0);
        collect(&shared, &tx, &tx, &blocked, 1024);
        assert_eq!(rx.try_recv().unwrap().id, 2);
        assert!(shared.lock().unwrap().pending.contains_key(&1));
        assert_eq!(usage.load(Ordering::Acquire), 4);
        let (binary, delivery) = mpsc::sync_channel(1);
        collect(&shared, &tx, &tx, &binary, 1024);
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
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        struct Owned(Arc<AtomicUsize>);
        impl Drop for Owned {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let mut owner = Some(Owned(drops.clone()));
        assert!(
            wait_owned(&mut owner, |_| Err(std::io::Error::other(
                "injected wait failure"
            )))
            .is_err()
        );
        assert!(owner.is_some());
        assert_eq!(drops.load(Ordering::Acquire), 0);
        wait_owned(&mut owner, |_| Ok(std::process::ExitStatus::from_raw(0))).unwrap();
        assert!(owner.is_none());
        assert_eq!(drops.load(Ordering::Acquire), 1);
    }
    #[test]
    fn tiny_success_budget_still_delivers_bounded_resource_error() {
        let mut shared = super::super::tests::shared(8);
        Arc::get_mut(&mut shared).unwrap().limits.reply_bytes = 1;
        let (tx, rx) = mpsc::sync_channel(1);
        shared.state.lock().unwrap().pending.insert(
            1,
            Entry {
                delivery: Delivery::Command(tx),
                cancel: Cancellation::default(),
                sent_cancel: false,
                control: true,
            },
        );
        let message = checked_message(
            Kind::Reply,
            1,
            &failure(ErrorCode::Busy, "a longer original error"),
            1,
        );
        assert!(message.bytes.len() <= wire::ERROR_BYTES);
        let mut bytes = Vec::new();
        message.write(shared.session, &mut bytes).unwrap();
        parent_control(std::io::Cursor::new(bytes), &shared).unwrap();
        assert!(matches!(
            rx.recv().unwrap(),
            Reply::Error {
                error: BridgeError {
                    code: ErrorCode::ResourceLimit,
                    ..
                }
            }
        ));
    }
    #[test]
    fn replies_complete_out_of_order_but_eof_does_not_guess_lost_outcome() {
        let shared = super::super::tests::shared(8);
        let mut receivers = Vec::new();
        for id in [1, 2] {
            let (tx, rx) = mpsc::sync_channel(1);
            shared.state.lock().unwrap().pending.insert(
                id,
                Entry {
                    delivery: Delivery::Command(tx),
                    cancel: Cancellation::default(),
                    sent_cancel: false,
                    control: true,
                },
            );
            receivers.push(rx);
        }
        let reply = failure(ErrorCode::Busy, "observed second request");
        let mut wire = Vec::new();
        Message::new(Kind::Reply, 2, serde_json::to_vec(&reply).unwrap())
            .write(shared.session, &mut wire)
            .unwrap();
        parent_control(std::io::Cursor::new(wire.clone()), &shared).unwrap();
        assert!(matches!(
            receivers[0].try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(matches!(
            receivers[1].try_recv().unwrap(),
            Reply::Error { .. }
        ));
        assert_eq!(shared.state.lock().unwrap().phase, TransportPhase::Failed);
        assert!(!shared.state.lock().unwrap().reaped);
        assert!(shared.state.lock().unwrap().unknown);
        // Repeated completion and another session cannot consume the remaining owner.
        assert!(parent_control(std::io::Cursor::new(wire.clone()), &shared).is_err());
        wire[8] ^= 1;
        assert!(parent_control(std::io::Cursor::new(wire), &shared).is_err());
        assert!(shared.state.lock().unwrap().pending.contains_key(&1));
        shared.complete_failure(); // Called by the real implementation only after wait.
        assert!(matches!(receivers[0].recv().unwrap(), Reply::Error { .. }));
    }
    #[test]
    fn early_cancel_and_full_capacity_prevent_mutation_dispatch() {
        let shared = child_state();
        let (tx, _rx) = mpsc::sync_channel(2);
        let request = Request::Create {
            path: NativePath::from_path(std::path::Path::new("/synthetic-mutation")),
        };
        let body = serde_json::to_vec(&request).unwrap();
        let mut input = Vec::new();
        Message::new(Kind::Cancel, 1, vec![])
            .write([9; 16], &mut input)
            .unwrap();
        Message::new(Kind::Command, 1, body.clone())
            .write([9; 16], &mut input)
            .unwrap();
        let mutations = std::cell::Cell::new(0);
        let mut fake = |kind, bytes: &[u8]| {
            assert_eq!(kind, Kind::Command);
            assert!(matches!(
                serde_json::from_slice::<Request>(bytes).unwrap(),
                Request::Create { .. }
            ));
            mutations.set(mutations.get() + 1);
            Ok(Err(error(
                ErrorCode::Native,
                "mutation recorded by fake engine",
            )))
        };
        child_input(
            &mut std::io::Cursor::new(input),
            [9; 16],
            &mut fake,
            &shared,
            &Limits::default(),
            &tx,
            &mut || Ok(()),
        )
        .unwrap();
        let state = shared.lock().unwrap();
        assert!(state.cancels.is_empty());
        assert!(state.early.is_empty());
        let ChildPending::Reply(reply) = state.pending.get(&1).unwrap() else {
            panic!("typed canceled reply required")
        };
        assert!(matches!(
            serde_json::from_slice::<Reply>(&reply.bytes).unwrap(),
            Reply::Error {
                error: BridgeError {
                    code: ErrorCode::Canceled,
                    ..
                }
            }
        ));
        drop(state);
        // A full completion registry must reject before the same mutating dispatcher.
        let limits = Limits {
            queued: 1,
            ..Limits::default()
        };
        for id in 2..=(CONTROL_SLOTS + 1) as u64 {
            shared.lock().unwrap().pending.insert(
                id,
                ChildPending::Reply(Message::new(Kind::Reply, id, vec![])),
            );
        }
        let mut input = Vec::new();
        Message::new(Kind::Command, 99, body.clone())
            .write([9; 16], &mut input)
            .unwrap();
        let result = child_input(
            &mut std::io::Cursor::new(input),
            [9; 16],
            &mut fake,
            &shared,
            &limits,
            &tx,
            &mut || Ok(()),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ordinary pending bounds")
        );
        assert_eq!(
            mutations.get(),
            0,
            "canceled and capacity-rejected writes never reach engine"
        );
        // Prove the fake records an admitted mutation, and repeated IDs cannot dispatch twice.
        let shared = child_state();
        let mut input = Vec::new();
        for _ in 0..2 {
            Message::new(Kind::Command, 1, body.clone())
                .write([9; 16], &mut input)
                .unwrap();
        }
        let result = child_input(
            &mut std::io::Cursor::new(input),
            [9; 16],
            &mut fake,
            &shared,
            &limits,
            &tx,
            &mut || Ok(()),
        );
        assert!(result.unwrap_err().to_string().contains("replayed"));
        assert_eq!(mutations.get(), 1);
    }
    #[test]
    fn child_drain_failure_reports_exact_attempt_and_accepts_same_process_retry() {
        let shared = child_state();
        let (tx, rx) = mpsc::sync_channel(2);
        let mut input = Vec::new();
        Message::new(Kind::Shutdown, 1, vec![])
            .write([9; 16], &mut input)
            .unwrap();
        Message::new(Kind::Shutdown, 2, vec![])
            .write([9; 16], &mut input)
            .unwrap();
        let mut calls = 0;
        child_input(
            &mut std::io::Cursor::new(input),
            [9; 16],
            &mut |_, _| panic!("shutdown must not dispatch a command"),
            &shared,
            &Limits::default(),
            &tx,
            &mut || {
                calls += 1;
                if calls == 1 {
                    Err(error(ErrorCode::Native, "injected native drain failure"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert_eq!(calls, 2);
        let failure = rx.try_recv().unwrap();
        assert_eq!((failure.kind, failure.id), (Kind::DrainError, 1));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn held_async_drain_rejects_new_work_before_dispatch_but_keeps_controls() {
        let shared = child_state();
        let (tx, _rx) = mpsc::sync_channel(2);
        let mut input = Vec::new();
        Message::new(Kind::Shutdown, 1, vec![])
            .write([9; 16], &mut input)
            .unwrap();
        let create = Request::Create {
            path: NativePath::from_path(std::path::Path::new("/never-created")),
        };
        Message::new(Kind::Command, 1, serde_json::to_vec(&create).unwrap())
            .write([9; 16], &mut input)
            .unwrap();
        Message::new(
            Kind::Command,
            2,
            serde_json::to_vec(&Request::Status).unwrap(),
        )
        .write([9; 16], &mut input)
        .unwrap();
        Message::new(Kind::Cancel, 3, vec![])
            .write([9; 16], &mut input)
            .unwrap();
        Message::new(Kind::DrainAck, 1, vec![])
            .write([9; 16], &mut input)
            .unwrap();
        let mut status_calls = 0;
        let mut drains = 0;
        child_input_relay(
            &mut std::io::Cursor::new(input),
            [9; 16],
            &mut |kind, bytes| {
                assert_eq!(kind, Kind::Command);
                assert!(
                    matches!(
                        serde_json::from_slice::<Request>(bytes).unwrap(),
                        Request::Status
                    ),
                    "mutation escaped Closing admission"
                );
                status_calls += 1;
                Ok(Err(error(ErrorCode::Busy, "synthetic cached status")))
            },
            &shared,
            &Limits::default(),
            &tx,
            &mut |attempt| {
                assert_eq!(attempt, 1);
                drains += 1;
                Ok(drains == 2) // first request remains held; exact acknowledgement settles it
            },
            None,
        )
        .unwrap();
        assert_eq!(drains, 2);
        assert_eq!(status_calls, 1);
        let state = shared.lock().unwrap();
        let ChildPending::Reply(reply) = state.pending.get(&1).unwrap() else {
            panic!("Closed reply required")
        };
        assert!(matches!(
            serde_json::from_slice::<Reply>(&reply.bytes).unwrap(),
            Reply::Error {
                error: BridgeError {
                    code: ErrorCode::Closed,
                    ..
                }
            }
        ));
        assert!(
            state.early.contains(&3),
            "reserved cancellation still consumed during held drain"
        );
        assert!(
            !state.stopping,
            "input Closing must not stop collectors before verified drain"
        );
    }
    #[test]
    fn admission_frames_complete_independently_of_held_ordinary_packet() {
        use filesystem::{Lane, Out};
        let make = |lane, byte| RelayOutput {
            out: Out {
                lane,
                bytes: Arc::new(vec![byte; wire::CHUNK + 7]),
            },
            offset: 0,
        };
        let mut ordinary = make(Lane::Data, 1);
        let mut admission = make(Lane::Admission, 2);
        let mut store = make(Lane::Store, 5);
        let mut assembly = RelayAssembly::default();
        assert!(
            relay_input(&mut assembly, ordinary.frame([9; 16]))
                .unwrap()
                .is_none()
        );
        assert!(
            relay_input(&mut assembly, admission.frame([9; 16]))
                .unwrap()
                .is_none()
        );
        assert!(
            relay_input(&mut assembly, store.frame([9; 16]))
                .unwrap()
                .is_none()
        );
        let (bytes, lane) = relay_input(&mut assembly, store.frame([9; 16]))
            .unwrap()
            .unwrap();
        assert_eq!((bytes, lane), (vec![5; wire::CHUNK + 7], Lane::Store));
        assert!(
            assembly.data.is_some() && assembly.admission.is_some(),
            "store completion cannot consume admission or ordinary assembly"
        );
        let control = Frame {
            kind: Kind::FilesystemControl,
            session: [9; 16],
            id: 0,
            offset: 0,
            total: 1,
            payload: vec![3],
        };
        assert_eq!(
            relay_input(&mut assembly, control).unwrap().unwrap(),
            (vec![3], Lane::Control)
        );
        let (bytes, lane) = relay_input(&mut assembly, admission.frame([9; 16]))
            .unwrap()
            .unwrap();
        assert_eq!(lane, Lane::Admission);
        assert_eq!(bytes, vec![2; wire::CHUNK + 7]);
        assert!(
            assembly.incomplete(),
            "ordinary packet remains separately owned"
        );
        let (bytes, lane) = relay_input(&mut assembly, ordinary.frame([9; 16]))
            .unwrap()
            .unwrap();
        assert_eq!((bytes, lane), (vec![1; wire::CHUNK + 7], Lane::Data));
        assert!(!assembly.incomplete());
        let mut bad = make(Lane::Admission, 4);
        assert!(
            relay_input(&mut assembly, bad.frame([9; 16]))
                .unwrap()
                .is_none()
        );
        let mut changed = bad.frame([9; 16]);
        changed.offset += 1;
        assert!(
            relay_input(&mut assembly, changed).is_err(),
            "interleaving must not relax packet continuity"
        );
    }
    #[test]
    fn lm_desktop_relay_child_shutdown_refuses_authority_and_dispatches_only_recovery() {
        use super::super::lightroom_migration::{
            Action, Reply as MigrationReply, Request as MigrationRequest,
        };
        use crate::lightroom_migration_worker::protocol::{Guard, WriteKind};
        let shared = child_state();
        let (tx, _rx) = mpsc::sync_channel(2);
        let mut input = Vec::new();
        Message::new(Kind::Shutdown, 1, vec![])
            .write([9; 16], &mut input)
            .unwrap();
        let actions = [
            Action::InspectTarget {
                catalog: "catalog".into(),
                destination: NativePath::from_path(std::path::Path::new("/must-not-open")),
            },
            Action::AcquireTarget {
                catalog: None,
                destination: NativePath::from_path(std::path::Path::new("/must-not-create")),
                expected: None,
            },
            Action::AcquireWrite {
                sequence: crate::application::U64(1),
                kind: WriteKind::Bootstrap,
                target: "a".repeat(64),
                lock: None,
                request_digest: "b".repeat(64),
            },
            Action::Progress {
                phase: "not a recovery".into(),
                completed: crate::application::U64(0),
                total: None,
            },
            Action::ReleaseWrite {
                sequence: crate::application::U64(1),
                kind: WriteKind::Bootstrap,
                request_digest: "b".repeat(64),
            },
            Action::RecoverTarget {
                acquire_digest: "b".repeat(64),
            },
            Action::RecoverWrite {
                sequence: crate::application::U64(1),
                kind: WriteKind::Bootstrap,
                request_digest: "b".repeat(64),
            },
            Action::Status,
            Action::Cancel,
            Action::DrainOperation,
        ];
        for (index, action) in actions.into_iter().enumerate() {
            let request = MigrationRequest {
                guard: Guard {
                    session: "fixture".into(),
                    generation: "a".repeat(64),
                    operation: "operation".into(),
                },
                action,
            };
            Message::new(
                Kind::MigrationAdmission,
                index as u64 + 1,
                serde_json::to_vec(&request).unwrap(),
            )
            .write([9; 16], &mut input)
            .unwrap();
        }
        let mut dispatched = 0;
        child_input_relay(
            &mut std::io::Cursor::new(input),
            [9; 16],
            &mut |kind, bytes| {
                assert_eq!(kind, Kind::MigrationAdmission);
                let request: MigrationRequest = serde_json::from_slice(bytes)?;
                assert!(
                    request.action.recovery(),
                    "new authority reached a closing Actor"
                );
                dispatched += 1;
                Ok(Err(error(ErrorCode::Native, "recovery reached Actor")))
            },
            &shared,
            &Limits::default(),
            &tx,
            &mut |_| Ok(false),
            None,
        )
        .unwrap();
        assert_eq!(dispatched, 6);
        let state = shared.lock().unwrap();
        for id in 1..=10 {
            let ChildPending::Reply(message) = state.pending.get(&id).unwrap() else {
                panic!()
            };
            let MigrationReply::Refused(failure) =
                serde_json::from_slice::<MigrationReply>(&message.bytes).unwrap()
            else {
                panic!()
            };
            assert!(if id <= 4 {
                matches!(failure.code, ErrorCode::Closed)
            } else {
                matches!(failure.code, ErrorCode::Native)
            });
        }
    }
    #[test]
    fn lm_desktop_relay_actor_disconnect_is_unknown_not_refused() -> anyhow::Result<()> {
        use super::super::lightroom_migration::{Pending, Reply as MigrationReply};
        let shared = child_state();
        let (actor_tx, receiver) = mpsc::sync_channel(1);
        drop(actor_tx);
        shared.lock().unwrap().pending.insert(
            1,
            ChildPending::Migration(Pending {
                receiver,
                cancel: Cancellation::default(),
            }),
        );
        let (ordinary, _ordinary_rx) = mpsc::sync_channel(1);
        let (migration, migration_rx) = mpsc::sync_channel(1);
        let (binary, _binary_rx) = mpsc::sync_channel(1);
        collect(&shared, &ordinary, &migration, &binary, 1024);
        let message = migration_rx.recv()?;
        assert!(matches!(
            serde_json::from_slice::<MigrationReply>(&message.bytes)?,
            MigrationReply::Error(BridgeError {
                code: ErrorCode::Closed,
                ..
            })
        ));
        Ok(())
    }

    fn migration_fixture_request(
        action: super::super::lightroom_migration::Action,
    ) -> super::super::lightroom_migration::Request {
        super::super::lightroom_migration::Request {
            guard: crate::lightroom_migration_worker::protocol::Guard {
                session: "fixture".into(),
                generation: "a".repeat(64),
                operation: "operation".into(),
            },
            action,
        }
    }
    #[test]
    fn lm_desktop_relay_multipart_request_custody_preserves_recovery_and_shutdown()
    -> anyhow::Result<()> {
        use super::super::lightroom_migration::{Action, Client};
        for closing in [false, true] {
            let shared = super::super::tests::shared(8);
            let client = Client::new(&shared);
            let mut output = ParentMessages::default();
            let mut input = Vec::new();
            let ordinary = vec![b'x'; wire::CHUNK * 2 + 19];
            if !closing {
                shared.state.lock().unwrap().data.push_back(Message::new(
                    Kind::Command,
                    100,
                    ordinary.clone(),
                ));
                output.next(&shared).unwrap().write(&mut input)?;
                assert!(output.ordinary.is_some());
            }
            let acquire = migration_fixture_request(Action::AcquireTarget {
                catalog: None,
                destination: NativePath::UnixBytes(vec![b'x'; 12000]),
                expected: None,
            });
            let expected = serde_json::to_vec(&acquire)?;
            let _pending = client.submit(acquire)?;
            output.next(&shared).unwrap().write(&mut input)?;
            assert!(output.migration.is_some());
            if closing {
                shared.stop();
                let shutdown = output.next(&shared).unwrap();
                assert_eq!(shutdown.kind, Kind::Shutdown);
                shutdown.write(&mut input)?;
            }
            let _recovery = client.submit(migration_fixture_request(Action::Status))?;
            let recovery = output.next(&shared).unwrap();
            assert_eq!(recovery.id, 2);
            assert_eq!(recovery.offset, 0);
            recovery.write(&mut input)?;
            while output.migration.is_some() {
                output.next(&shared).unwrap().write(&mut input)?;
            }
            if !closing {
                assert!(
                    output.ordinary.is_some(),
                    "multipart authority overwrote ordinary custody"
                );
                while output.ordinary.is_some() {
                    output.next(&shared).unwrap().write(&mut input)?;
                }
            }
            let child = child_state();
            let (tx, _rx) = mpsc::sync_channel(2);
            let mut calls = Vec::new();
            child_input_relay(
                &mut &input[..],
                shared.session,
                &mut |kind, bytes| {
                    calls.push((kind, bytes.to_vec()));
                    Ok(Err(error(ErrorCode::Native, "fixture observation")))
                },
                &child,
                &shared.limits,
                &tx,
                &mut |_| Ok(false),
                None,
            )?;
            assert_eq!(calls.len(), if closing { 1 } else { 3 });
            let recovery: super::super::lightroom_migration::Request =
                serde_json::from_slice(&calls[0].1)?;
            assert!(matches!(recovery.action, Action::Status));
            if !closing {
                assert_eq!(calls[1], (Kind::MigrationAdmission, expected));
                assert_eq!(calls[2], (Kind::Command, ordinary));
            } else {
                let state = child.lock().unwrap();
                let ChildPending::Reply(reply) = state.pending.get(&1).unwrap() else {
                    panic!()
                };
                assert!(matches!(
                    serde_json::from_slice::<super::super::lightroom_migration::Reply>(
                        &reply.bytes
                    )?,
                    super::super::lightroom_migration::Reply::Refused(BridgeError {
                        code: ErrorCode::Closed,
                        ..
                    })
                ));
            }
            shared.complete_failure();
        }
        Ok(())
    }

    #[test]
    fn lm_desktop_relay_multipart_assemblies_reject_changed_kind_offset_and_second_owner()
    -> anyhow::Result<()> {
        for fault in ["kind", "offset", "second", "inline"] {
            let mut assembly = MigrationAssemblies::default();
            let mut message = Message::new(Kind::MigrationAdmission, 1, vec![1; wire::CHUNK * 2]);
            assert!(
                assembly
                    .push(message.next([9; 16]), wire::CHUNK * 2)?
                    .is_none()
            );
            let mut changed = message.next([9; 16]);
            match fault {
                "kind" => changed.kind = Kind::Command,
                "offset" => changed.offset += 1,
                "second" => {
                    changed.id = 2;
                    changed.offset = 0;
                }
                "inline" => {
                    changed.offset = 0;
                    changed.total = changed.payload.len();
                }
                _ => unreachable!(),
            }
            assert!(assembly.push(changed, wire::CHUNK * 2).is_err(), "{fault}");
            assert!(assembly.incomplete());
        }
        let mut assembly = MigrationAssemblies::default();
        let mut oversized = Message::new(Kind::MigrationReply, 1, vec![1; wire::CHUNK + 1]);
        assert!(assembly.push(oversized.next([9; 16]), wire::CHUNK).is_err());
        assert!(!assembly.incomplete());
        Ok(())
    }

    #[test]
    fn lm_desktop_relay_multipart_replies_interleave_without_pin_loss_or_shared_queue_pressure()
    -> anyhow::Result<()> {
        use super::super::lightroom_migration::{
            Action, Client, Phase, Reply as MigrationReply, Snapshot,
        };
        use crate::application::{I64, U64};
        use crate::lightroom_migration_worker::{identity::FileKey, lease::DestinationPin};
        let shared = super::super::tests::shared(8);
        let pending = Client::new(&shared).submit(migration_fixture_request(Action::Status))?;
        let pin = DestinationPin {
            root: NativePath::WindowsWide(vec![65535; crate::catalog_session::PATH_UNITS]),
            root_key: FileKey {
                volume: U64(1),
                index: U64(u64::MAX),
            },
            database_key: FileKey {
                volume: U64(2),
                index: U64(3),
            },
            schema: I64(1),
        };
        let snapshot = Snapshot {
            guard: migration_fixture_request(Action::Status).guard,
            phase: Phase::Target,
            catalog: None,
            destination: Some(pin.clone()),
            sequence: None,
            request_digest: None,
            write_kind: None,
            cancel_requested: false,
            progress: None,
            failure: None,
        };
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        shared.state.lock().unwrap().pending.insert(
            2,
            Entry {
                delivery: Delivery::Command(reply_tx),
                cancel: Cancellation::default(),
                sent_cancel: false,
                control: false,
            },
        );
        let (tx, rx) = mpsc::sync_channel(1);
        let (migration_tx, migration_rx) = mpsc::sync_channel(CONTROL_SLOTS + 1);
        let ordinary = checked_message(
            Kind::Reply,
            2,
            &Reply::Error {
                error: error(ErrorCode::Native, "o".repeat(wire::CHUNK * 3)),
            },
            shared.limits.reply_bytes,
        );
        tx.send(ordinary).unwrap(); // Completely fill the ordinary output lane.
        let child = child_state();
        let (actor_tx, actor_rx) = mpsc::sync_channel(1);
        actor_tx.send(MigrationReply::Ok(snapshot)).unwrap();
        child.lock().unwrap().pending.insert(
            1,
            ChildPending::Migration(super::super::lightroom_migration::Pending {
                receiver: actor_rx,
                cancel: Cancellation::default(),
            }),
        );
        let (binary, _binary_rx) = mpsc::sync_channel(1);
        collect(
            &child,
            &tx,
            &migration_tx,
            &binary,
            shared.limits.reply_bytes,
        );
        assert!(
            child.lock().unwrap().pending.is_empty(),
            "ordinary output pressure consumed migration capacity"
        );
        drop(tx);
        drop(migration_tx);
        let mut bytes = Vec::new();
        output_writer(&mut bytes, rx, migration_rx, shared.session, None);
        let mut cursor = &bytes[..];
        let first = Frame::read(&mut cursor)?.unwrap();
        let second = Frame::read(&mut cursor)?.unwrap();
        assert_eq!(first.kind, Kind::MigrationReply);
        assert_eq!(second.kind, Kind::Reply);
        assert!(first.total > first.payload.len() && second.total > second.payload.len());
        shared.state.lock().unwrap().stopping = true;
        parent_control(&bytes[..], &shared)?;
        let MigrationReply::Ok(reply) = pending.receiver.recv()? else {
            panic!("pin lost")
        };
        assert_eq!(reply.destination, Some(pin));
        assert!(matches!(reply_rx.recv()?, Reply::Error { .. }));
        assert!(shared.state.lock().unwrap().pending.is_empty());
        Ok(())
    }
    #[test]
    fn lm_desktop_relay_parent_valid_saturation_preserves_each_class_and_rejects_overflow()
    -> anyhow::Result<()> {
        use super::super::lightroom_migration::{Action, Client};
        let mut parent = super::super::tests::shared(8);
        Arc::get_mut(&mut parent).unwrap().limits.queued = 3;
        let ordinary_cap = parent.limits.queued + CONTROL_SLOTS;
        let aggregate_cap = ordinary_cap + CONTROL_SLOTS + 1;
        let ordinary_bytes = serde_json::to_vec(&Request::Status)?;
        // Fill both ordinary allowances exactly as admitted by the parent:
        // queued data plus sixteen small controls, all before migration submit.
        {
            let mut state = parent.state.lock().unwrap();
            for index in 0..ordinary_cap {
                let id = state.next;
                state.next += 1;
                let control = index >= parent.limits.queued;
                let (tx, _rx) = mpsc::sync_channel(1);
                state.pending.insert(
                    id,
                    Entry {
                        delivery: Delivery::Command(tx),
                        cancel: Cancellation::default(),
                        sent_cancel: false,
                        control,
                    },
                );
                let message = Message::new(Kind::Command, id, ordinary_bytes.clone());
                if control {
                    state.control.push_back(message);
                } else {
                    state.data.push_back(message);
                }
            }
            assert_eq!(
                state
                    .pending
                    .values()
                    .filter(|entry| !entry.control)
                    .count(),
                parent.limits.queued
            );
            assert_eq!(
                state.pending.values().filter(|entry| entry.control).count(),
                CONTROL_SLOTS
            );
        }
        let client = Client::new(&parent);
        let _authority = client.submit(migration_fixture_request(Action::AcquireTarget {
            catalog: None,
            destination: NativePath::from_path(std::path::Path::new("/fixture")),
            expected: None,
        }))?;
        let mut recoveries = Vec::new();
        for _ in 0..CONTROL_SLOTS {
            recoveries.push(client.submit(migration_fixture_request(Action::Status))?);
        }
        assert_eq!(parent.state.lock().unwrap().pending.len(), aggregate_cap);
        let mut messages = ParentMessages::default();
        let mut input = Vec::new();
        let mut ids = Vec::new();
        for index in 0..aggregate_cap {
            let frame = messages.next(&parent).unwrap();
            if index < CONTROL_SLOTS {
                assert_eq!(frame.kind, Kind::MigrationAdmission);
            }
            assert_eq!(frame.total, frame.payload.len());
            ids.push(frame.id);
            frame.write(&mut input)?;
        }
        assert_eq!(
            *ids.last().unwrap(),
            parent.limits.queued as u64,
            "last queued ordinary request must still be admitted"
        );
        assert!(parent.state.lock().unwrap().control.is_empty());
        assert!(parent.state.lock().unwrap().data.is_empty());

        let run = |bytes: &[u8]| {
            let child = child_state();
            let (tx, _rx) = mpsc::sync_channel(1);
            let mut dispatched = 0;
            let result = child_input_relay(
                &mut &bytes[..],
                parent.session,
                &mut |_, _| {
                    dispatched += 1;
                    Ok(Err(error(ErrorCode::Native, "retained fixture completion")))
                },
                &child,
                &parent.limits,
                &tx,
                &mut |_| Ok(false),
                None,
            );
            (result, child, dispatched)
        };
        // No collector runs: completion timing cannot release any admission.
        let (result, child, dispatched) = run(&input);
        result?;
        assert_eq!(dispatched, aggregate_cap);
        {
            let state = child.lock().unwrap();
            assert_eq!(state.pending.len(), aggregate_cap);
            assert_eq!(state.migration.len(), CONTROL_SLOTS + 1);
            assert_eq!(
                state
                    .pending
                    .keys()
                    .filter(|id| !state.migration.contains_key(*id))
                    .count(),
                ordinary_cap
            );
            assert!(state.pending.contains_key(ids.last().unwrap()));
        }
        let mut aggregate_overflow = input;
        Message::new(Kind::Command, 1000, ordinary_bytes.clone())
            .write(parent.session, &mut aggregate_overflow)?;
        let (result, child, dispatched) = run(&aggregate_overflow);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("child pending bounds")
        );
        assert_eq!(dispatched, aggregate_cap);
        assert_eq!(child.lock().unwrap().pending.len(), aggregate_cap);

        for class in ["ordinary", "authority", "recovery"] {
            let admitted = match class {
                "ordinary" => ordinary_cap,
                "authority" => 1,
                _ => CONTROL_SLOTS,
            };
            let mut bytes = Vec::new();
            for index in 0..=admitted {
                let (kind, body) = if class == "ordinary" {
                    (Kind::Command, ordinary_bytes.clone())
                } else {
                    let action = if class == "authority" {
                        Action::AcquireTarget {
                            catalog: None,
                            destination: NativePath::from_path(std::path::Path::new("/fixture")),
                            expected: None,
                        }
                    } else {
                        Action::Status
                    };
                    (
                        Kind::MigrationAdmission,
                        serde_json::to_vec(&migration_fixture_request(action))?,
                    )
                };
                Message::new(kind, index as u64 + 1, body).write(parent.session, &mut bytes)?;
            }
            let (result, child, dispatched) = run(&bytes);
            let failure = result.unwrap_err().to_string();
            assert!(
                failure.contains(if class == "ordinary" {
                    "ordinary pending bounds"
                } else {
                    "migration reserved recovery admission"
                }),
                "{class}: {failure}"
            );
            assert_eq!(dispatched, admitted);
            assert_eq!(child.lock().unwrap().pending.len(), admitted);
        }
        parent.complete_failure();
        Ok(())
    }
}

#[cfg(all(test, unix))]
pub(super) mod test_reap;
