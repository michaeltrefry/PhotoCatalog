//! Test-only one-shot gate inside the actual production Child wait owner.
use std::{
    cell::RefCell,
    io,
    process::{Child, ExitStatus},
    sync::mpsc,
    time::Duration,
};
thread_local! { static NEXT: RefCell<Option<WaitGate>> = const { RefCell::new(None) }; }
enum Action {
    Release,
    Terminate,
}
pub(crate) struct Gate {
    command: mpsc::SyncSender<Action>,
    killed: mpsc::Receiver<io::Result<()>>,
    waited: mpsc::Receiver<Result<ExitStatus, String>>,
}
pub(super) struct WaitGate {
    command: mpsc::Receiver<Action>,
    killed: mpsc::SyncSender<io::Result<()>>,
    waited: mpsc::SyncSender<Result<ExitStatus, String>>,
}
struct Restore(Option<WaitGate>);
impl Drop for Restore {
    fn drop(&mut self) {
        NEXT.with(|slot| {
            *slot.borrow_mut() = self.0.take();
        });
    }
}
pub(crate) fn armed<T>(start: impl FnOnce() -> T) -> (T, Gate) {
    let (command, receiver) = mpsc::sync_channel(1);
    let (killed_tx, killed) = mpsc::sync_channel(1);
    let (waited_tx, waited) = mpsc::sync_channel(1);
    let restore = Restore(NEXT.with(|slot| {
        slot.replace(Some(WaitGate {
            command: receiver,
            killed: killed_tx,
            waited: waited_tx,
        }))
    }));
    let gate = Gate {
        command,
        killed,
        waited,
    };
    let result = start();
    drop(restore);
    (result, gate)
}
pub(super) fn take() -> Option<WaitGate> {
    NEXT.with(|slot| slot.borrow_mut().take())
}
impl WaitGate {
    pub(super) fn before_wait(&self, child: &mut Child) {
        if matches!(
            self.command.recv_timeout(Duration::from_secs(60)),
            Ok(Action::Terminate)
        ) {
            // The same owner retains Child here and cannot wait concurrently.
            let _ = self.killed.send(child.kill());
        }
        // Release or disconnection proceeds to ordinary checked wait and joins.
    }
    pub(super) fn after_wait(&self, result: &io::Result<ExitStatus>) {
        let _ = self
            .waited
            .send(result.as_ref().copied().map_err(ToString::to_string));
    }
}
impl Gate {
    pub(crate) fn terminate(&self) -> anyhow::Result<()> {
        self.command.send(Action::Terminate)?;
        self.killed.recv_timeout(Duration::from_secs(15))??;
        Ok(())
    }
    pub(crate) fn waited(&self) -> anyhow::Result<ExitStatus> {
        self.waited
            .recv_timeout(Duration::from_secs(15))?
            .map_err(anyhow::Error::msg)
    }
}
impl Drop for Gate {
    fn drop(&mut self) {
        let _ = self.command.try_send(Action::Release);
    }
}
