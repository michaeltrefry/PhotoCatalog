//! The actual thread-affine Writers::Permit never leaves its acquiring thread.
//! Only readiness and retirement tokens cross to the supervisor dispatcher.
use super::*;
use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Condvar, Mutex},
    thread::JoinHandle,
};

pub(super) enum Outcome {
    Finished(Result<()>),
    Panicked(Box<dyn Any + Send>),
}
pub(super) fn panic_detail(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic payload")
}
struct Retirement {
    requested: Mutex<bool>,
    changed: Condvar,
}
pub(super) struct Lease {
    thread: Option<JoinHandle<()>>,
    retirement: Arc<Retirement>,
}
impl Lease {
    fn request_retirement(&self) {
        *self
            .retirement
            .requested
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = true;
        self.retirement.changed.notify_all();
    }
    pub(super) fn retry_retire(&mut self) -> Option<Result<()>> {
        let Some(thread) = self.thread.as_ref() else {
            return Some(Ok(()));
        };
        // A transferred lease leaves an empty shell behind. Dropping that
        // shell must not retire the thread now owned by the receiving lease.
        self.request_retirement();
        if !thread.is_finished() {
            return None;
        }
        let thread = self.thread.take().expect("finished permit owner");
        Some(
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("migration permit owner panicked during retirement")),
        )
    }
    pub(super) fn retire(&mut self) -> Result<()> {
        loop {
            if let Some(result) = self.retry_retire() {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}
pub(super) struct Pending<A: Admission> {
    state: Arc<Mutex<Option<A>>>,
    ready: Arc<Mutex<Option<Outcome>>>,
    lease: Lease,
    pub(super) stop: Arc<Stop>,
    pub(super) sequence: u64,
    pub(super) kind: WriteKind,
}
pub(super) struct StartFailure<A> {
    pub(super) admission: A,
    pub(super) error: anyhow::Error,
}
impl<A: Admission> Pending<A> {
    pub(super) fn start(
        admission: A,
        sequence: u64,
        kind: WriteKind,
        target: String,
        lock: Option<FileKey>,
        stop: Arc<Stop>,
        until: Instant,
    ) -> std::result::Result<Self, StartFailure<A>> {
        #[cfg(test)]
        let mut admission = admission;
        #[cfg(test)]
        if admission.fail_waiter_spawn() {
            return Err(StartFailure {
                admission,
                error: std::io::Error::other("injected migration admission thread spawn failure")
                    .into(),
            });
        }
        let state = Arc::new(Mutex::new(Some(admission)));
        let worker_state = state.clone();
        let ready = Arc::new(Mutex::new(None));
        let worker_ready = ready.clone();
        let retirement = Arc::new(Retirement {
            requested: Mutex::new(false),
            changed: Condvar::new(),
        });
        let worker_retirement = retirement.clone();
        let worker_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("migration-admission".into())
            .spawn(move || {
                let mut admission = worker_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .expect("owned admission state");
                let acquired = catch_unwind(AssertUnwindSafe(|| {
                    let writers = admission.writer(
                        sequence,
                        kind,
                        &target,
                        lock.as_ref(),
                        &worker_stop,
                        until,
                    )?;
                    writers.enter_cancellable(
                        Priority::Background,
                        worker_stop.admission(),
                        Some(until),
                    )
                }));
                // Acknowledgement callbacks return before ready is published. The
                // Permit remains a local variable, including its !Send external lease.
                *worker_state.lock().unwrap_or_else(|e| e.into_inner()) = Some(admission);
                let permit = match acquired {
                    Ok(Ok(permit)) => {
                        *worker_ready.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(Outcome::Finished(Ok(())));
                        permit
                    }
                    Ok(Err(error)) => {
                        *worker_ready.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(Outcome::Finished(Err(error)));
                        return;
                    }
                    Err(panic) => {
                        *worker_ready.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(Outcome::Panicked(panic));
                        return;
                    }
                };
                // Cancellation after acquisition must not release an in-use permit.
                // Only exact ReleaseWrite or G's completed helper drain retires it.
                let mut retire = worker_retirement
                    .requested
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                while !*retire {
                    retire = worker_retirement
                        .changed
                        .wait(retire)
                        .unwrap_or_else(|e| e.into_inner());
                }
                drop(retire);
                drop(permit);
            });
        match thread {
            Ok(thread) => Ok(Self {
                state,
                ready,
                lease: Lease {
                    thread: Some(thread),
                    retirement,
                },
                stop,
                sequence,
                kind,
            }),
            Err(error) => Err(StartFailure {
                admission: state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .expect("unstarted admission owner"),
                error: error.into(),
            }),
        }
    }
    pub(super) fn finished(&self) -> bool {
        self.ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
    pub(super) fn take_ready(&mut self) -> (A, Outcome) {
        let outcome = self
            .ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("published admission outcome");
        let admission = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("admission restored before ready");
        (admission, outcome)
    }
    pub(super) fn into_lease(mut self) -> Lease {
        Lease {
            thread: self.lease.thread.take(),
            retirement: self.lease.retirement.clone(),
        }
    }
    /// Only after executor/Source revocation, or for a never-granted failed wait.
    pub(super) fn retry_join(&mut self) -> Option<(A, Outcome)> {
        self.stop.cancel();
        let joined = self.lease.retry_retire()?;
        let (admission, outcome) = self.take_ready();
        Some((
            admission,
            if let Err(error) = joined {
                Outcome::Finished(Err(error))
            } else {
                outcome
            },
        ))
    }
}
impl<A: Admission> Drop for Pending<A> {
    fn drop(&mut self) {
        if self.lease.thread.is_some() {
            self.stop.cancel();
            let _ = self.lease.retire();
            if let Some(mut admission) = self.state.lock().unwrap_or_else(|e| e.into_inner()).take()
            {
                while admission.release(self.sequence, self.kind).is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }
}
