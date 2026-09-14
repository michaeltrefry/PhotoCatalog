//! Independent G broker. Failure/control publication never waits for the data
//! queue or for an actor writer acknowledgement. Only this thread owns Sources.
use super::super::transport::Reply;
use super::{Command, Event, Kind, server::Owner};
use crate::lightroom_migration_worker::{
    memory::MemoryBudget,
    process::{Process, Stop},
    protocol::Guard,
};
use anyhow::{Context, Result, ensure};
use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

struct Control {
    revoke: bool,
    revoked: bool,
    drain: bool,
    active: usize,
    #[cfg(test)]
    event_full: bool,
    // At most one exact failure for each of the two live epochs. This reserved
    // storage is independent of the normal event queue's occupancy.
    urgent: [Option<Event>; 2],
}
struct Shared {
    state: Mutex<Control>,
    changed: Condvar,
}
pub(crate) struct Broker {
    commands: Option<SyncSender<Command>>,
    events: Receiver<Event>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<Result<()>>>,
    // Survives worker-thread exit until G's Owned has waited LM and this broker.
    // Only acknowledged per-epoch quiescence removes a charge early.
    _charges: super::server::Charges,
}
impl Broker {
    pub(crate) fn start(
        executable: PathBuf,
        guard: Guard,
        stop: Arc<Stop>,
        memory: MemoryBudget,
    ) -> Result<Self> {
        ensure!(
            executable.is_absolute(),
            "absolute configured Source executable required"
        );
        Self::start_with(guard, stop, memory, move |kind, stop, before_wait| {
            let role = match kind {
                Kind::Sql => "--lightroom-source-reader-sql",
                Kind::Raw => "--lightroom-source-reader-raw",
            };
            Process::spawn_role_with_cleanup(&executable, role, stop, Some(before_wait))
        })
    }
    fn start_with(
        guard: Guard,
        stop: Arc<Stop>,
        memory: MemoryBudget,
        mut spawn: impl FnMut(Kind, Arc<Stop>, &mut dyn FnMut()) -> Result<Process<Reply>>
        + Send
        + 'static,
    ) -> Result<Self> {
        // Managed G admission is backed by the caller's local shared pool;
        // holding a charge lock never invokes an IPC allocation callback.
        memory.snapshot()?;
        let charges = Arc::new(Mutex::new([None, None]));
        let mut owner = Owner::new(guard, memory, charges.clone())?;
        let (commands, incoming) = mpsc::sync_channel(2);
        let (outgoing, events) = mpsc::sync_channel(2);
        let shared = Arc::new(Shared {
            state: Mutex::new(Control {
                revoke: false,
                revoked: false,
                drain: false,
                active: 0,
                #[cfg(test)]
                event_full: false,
                urgent: [None, None],
            }),
            changed: Condvar::new(),
        });
        let control = shared.clone();
        let worker = thread::Builder::new()
            .name("migration-source-broker".into())
            .spawn(move || {
                // Every exit, including unwind, first revokes all children, then
                // waits for G's LM-revoked acknowledgement before a blocking drain.
                struct Drain<'a> {
                    owner: &'a mut Owner,
                    shared: &'a Shared,
                }
                impl Drain<'_> {
                    fn finish(&mut self) -> Result<()> {
                        self.owner.revoke_all();
                        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.revoked = true;
                        self.shared.changed.notify_all();
                        while !state.drain {
                            state = self
                                .shared
                                .changed
                                .wait(state)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                        drop(state);
                        self.owner.drain_all()
                    }
                }
                impl Drop for Drain<'_> {
                    fn drop(&mut self) {
                        let _ = self.finish();
                    }
                }
                let mut draining = Drain {
                    owner: &mut owner,
                    shared: &control,
                };
                let run =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let mut pending = None;
                        loop {
                            if stop.requested()
                                || control
                                    .state
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .revoke
                            {
                                return Ok(());
                            }
                            // Death remains observable even when the normal event queue
                            // and a current event are full. No dequeue is needed here.
                            draining.owner.check_liveness()?;
                            if let Some(event) = pending.take() {
                                match outgoing.try_send(event) {
                                    Ok(()) => {}
                                    Err(TrySendError::Full(event)) => {
                                        #[cfg(test)]
                                        {
                                            control
                                                .state
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .event_full = true;
                                        }
                                        pending = Some(event);
                                    }
                                    Err(TrySendError::Disconnected(_)) => {
                                        anyhow::bail!("Source event receiver lost")
                                    }
                                }
                            }
                            if pending.is_none() {
                                match incoming.try_recv() {
                                    Ok(command) => {
                                        pending = draining.owner.accept(
                                            command,
                                            &mut |kind, child_stop, revoke_sources| {
                                                spawn(kind, child_stop, &mut || {
                                                    // The partial Process has already been revoked
                                                    // by SetupOwner before entering this callback.
                                                    stop.cancel();
                                                    revoke_sources();
                                                    let mut state = control
                                                        .state
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner());
                                                    state.revoked = true;
                                                    control.changed.notify_all();
                                                    while !state.drain {
                                                        state = control
                                                            .changed
                                                            .wait(state)
                                                            .unwrap_or_else(|e| e.into_inner());
                                                    }
                                                })
                                            },
                                        )?
                                    }
                                    Err(TryRecvError::Disconnected) => return Ok(()),
                                    Err(TryRecvError::Empty) => {}
                                }
                                if pending.is_none() {
                                    pending = draining.owner.poll()?;
                                }
                            }
                            control
                                .state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .active = draining.owner.live();
                            // A timed wait sleeps without preventing revoke notification.
                            let state = control.state.lock().unwrap_or_else(|e| e.into_inner());
                            let _ = control
                                .changed
                                .wait_timeout(state, Duration::from_millis(2))
                                .unwrap_or_else(|e| e.into_inner());
                        }
                    }));
                let result = match run {
                    Ok(result) => result,
                    Err(_) => Err(anyhow::anyhow!("Source broker panicked")),
                };
                if let Err(error) = &result {
                    let detail = super::super::owner::reply_error_text(error);
                    let urgent = draining.owner.failures(&detail);
                    let mut state = control.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.urgent = urgent;
                    stop.cancel();
                    control.changed.notify_all();
                }
                // Drop acknowledges revoke before any Source wait, and cannot leave
                // this scope before G has acknowledged its own LM revocation.
                let drained = draining.finish();
                drop(draining);
                result.and(drained)
            })
            .context("start owned Source broker")?;
        Ok(Self {
            commands: Some(commands),
            events,
            shared,
            worker: Some(worker),
            _charges: charges,
        })
    }
    pub(crate) fn try_send(&self, command: Command) -> Result<Option<Command>> {
        match self
            .commands
            .as_ref()
            .context("Source broker retired")?
            .try_send(command)
        {
            Ok(()) => {
                self.shared.changed.notify_all();
                Ok(None)
            }
            Err(TrySendError::Full(command)) => Ok(Some(command)),
            Err(TrySendError::Disconnected(_)) => anyhow::bail!("Source broker ended"),
        }
    }
    pub(crate) fn try_urgent(&self) -> Option<Event> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .urgent
            .iter_mut()
            .find_map(Option::take)
    }
    pub(crate) fn try_receive(&self) -> Result<Option<Event>> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => anyhow::bail!("Source broker ended"),
        }
    }
    /// Caller has already revoked LM. Signal the independent broker immediately;
    /// no join or source wait can precede that acknowledgement.
    pub(crate) fn revoke_after_lm(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.revoke = true;
        state.drain = true;
        self.shared.changed.notify_all();
    }
    pub(crate) fn ensure_idle(&self) -> Result<()> {
        ensure!(
            self.shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
                == 0,
            "migration executor finished with live Source ownership"
        );
        Ok(())
    }
    pub(crate) fn wait_revoked(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        while !state.revoked {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
    pub(crate) fn finish(&mut self) -> Result<()> {
        self.revoke_after_lm();
        self.commands.take();
        if let Some(worker) = self.worker.take() {
            return worker
                .join()
                .map_err(|_| anyhow::anyhow!("Source broker join panic"))?;
        }
        Ok(())
    }
}
impl Drop for Broker {
    fn drop(&mut self) {
        // Broker is constructed before LM, or owned with a Drop which revokes LM
        // first. No naked Broker is exposed to application callers.
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests;
