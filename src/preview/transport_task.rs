//! One retained transport result. Catalog SQL stays on its owning actor.
use anyhow::{Context, Result};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

pub(crate) struct Task<T: Send + 'static> {
    handle: Option<JoinHandle<Result<T>>>,
    cancel: Arc<AtomicBool>,
}
impl<T: Send + 'static> Task<T> {
    pub fn spawn(
        name: &str,
        cancel: Arc<AtomicBool>,
        work: impl FnOnce(Arc<AtomicBool>) -> Result<T> + Send + 'static,
    ) -> Result<Self> {
        let task_cancel = cancel.clone();
        let handle = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || work(task_cancel))
            .context("transport task thread creation failed")?;
        Ok(Self {
            handle: Some(handle),
            cancel,
        })
    }

    pub fn running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Returns the sole result after the thread reports completion and joins.
    /// A pending task is never joined here. Polling after consuming a result is
    /// an owner error; it cannot be mistaken for another pending operation.
    pub fn poll(&mut self) -> Result<Option<T>> {
        let handle = self
            .handle
            .as_ref()
            .context("transport task result already consumed")?;
        if !handle.is_finished() {
            return Ok(None);
        }
        self.join().map(Some)
    }

    /// Cancellation is a request. It neither proves that I/O stopped nor
    /// releases the caller's stage, native lease or byte reservation.
    pub fn signal_cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    /// Explicit drain, outside actor ticks. This can wait for blocked I/O.
    /// None means the result was previously consumed, not that work succeeded.
    pub fn shutdown(&mut self) -> Result<Option<T>> {
        self.signal_cancel();
        if self.handle.is_none() {
            return Ok(None);
        }
        self.join().map(Some)
    }

    fn join(&mut self) -> Result<T> {
        self.handle
            .take()
            .context("transport task result already consumed")?
            .join()
            .map_err(|_| {
                anyhow::anyhow!(
                    "transport task panicked; operation outcome requires reconciliation"
                )
            })?
    }
}
impl<T: Send + 'static> Drop for Task<T> {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.signal_cancel();
        }
        // Dropping a JoinHandle detaches without waiting. The closure still
        // owns its captures through completion. Callers must keep this Task
        // and every independent stage/native/budget owner until checked drain;
        // this last-resort Drop is not evidence permitting their retirement.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    fn finished<T: Send + 'static>(task: &mut Task<T>) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !task.handle.as_ref().unwrap().is_finished() {
            if Instant::now() >= deadline {
                task.shutdown()?;
                anyhow::bail!("transport fixture did not finish");
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    #[test]
    fn held_transport_poll_and_cancel_return_before_io_is_released() -> Result<()> {
        let (release, held) = mpsc::channel();
        let (observed, observation) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut task = Task::spawn("held-transport", cancel.clone(), move |_| {
            held.recv()?;
            Ok(7)
        })?;
        // Separate caller lets the test release held I/O even if poll regresses
        // into a blocking join. Every assertion follows checked fixture drain.
        let caller = thread::spawn(move || {
            let pending = matches!(task.poll(), Ok(None));
            task.signal_cancel();
            let _ = observed.send((pending, cancel.load(Ordering::Acquire)));
            task
        });
        let observation = observation.recv_timeout(Duration::from_secs(5));
        release.send(())?;
        let mut task = caller
            .join()
            .map_err(|_| anyhow::anyhow!("fixture caller panic"))?;
        assert_eq!(task.shutdown()?, Some(7));
        assert_eq!(observation?, (true, true));
        assert!(task.handle.is_none());
        Ok(())
    }

    #[test]
    fn completed_result_remains_owned_until_checked_join_and_is_consumed_once() -> Result<()> {
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut task = Task::spawn("retained-result", cancel.clone(), move |_| Ok(owner))?;
        finished(&mut task)?;
        assert!(weak.upgrade().is_some());
        let value = task.poll()?.context("finished result missing")?;
        assert!(task.handle.is_none());
        assert!(task.poll().is_err());
        drop(task);
        assert!(
            !cancel.load(Ordering::Acquire),
            "completed subtask canceled its caller"
        );
        assert!(weak.upgrade().is_some());
        drop(value);
        assert!(weak.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn panic_and_operation_failure_are_returned_after_join_without_success() -> Result<()> {
        let mut panicked =
            Task::<()>::spawn("transport-panic", Arc::new(AtomicBool::new(false)), |_| {
                panic!("fixture panic")
            })?;
        finished(&mut panicked)?;
        let error = panicked.poll().unwrap_err();
        assert!(error.to_string().contains("requires reconciliation"));
        assert!(panicked.handle.is_none());
        assert!(panicked.poll().is_err());
        let mut failed =
            Task::<()>::spawn("transport-error", Arc::new(AtomicBool::new(false)), |_| {
                anyhow::bail!("retained operation failed")
            })?;
        finished(&mut failed)?;
        assert_eq!(
            failed.poll().unwrap_err().to_string(),
            "retained operation failed"
        );
        assert!(failed.handle.is_none());
        Ok(())
    }

    #[test]
    fn explicit_shutdown_keeps_captured_owner_until_io_completes_and_joins() -> Result<()> {
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let (release, held) = mpsc::channel();
        let (observed, observation) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut task = Task::spawn("shutdown-owner", cancel.clone(), move |_| {
            held.recv()?;
            drop(owner);
            Ok(())
        })?;
        let caller = thread::spawn(move || {
            task.signal_cancel();
            let _ = observed.send(());
            task.shutdown()
        });
        let observation = observation.recv_timeout(Duration::from_secs(5));
        let still_owned = weak.upgrade().is_some();
        let canceled = cancel.load(Ordering::Acquire);
        release.send(())?;
        let result = caller
            .join()
            .map_err(|_| anyhow::anyhow!("shutdown caller panic"))??;
        observation?;
        assert!(still_owned && canceled);
        assert_eq!(result, Some(()));
        assert!(weak.upgrade().is_none());
        Ok(())
    }
}
