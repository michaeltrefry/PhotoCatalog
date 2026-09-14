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
struct Retirement {
    requested: Mutex<bool>,
    changed: Condvar,
}
pub(super) struct Lease {
    thread: Option<JoinHandle<()>>,
    retirement: Arc<Retirement>,
}
impl Lease {
    pub(super) fn retire(&mut self) -> Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        *self
            .retirement
            .requested
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = true;
        self.retirement.changed.notify_all();
        thread
            .join()
            .map_err(|_| anyhow::anyhow!("migration permit owner panicked during retirement"))?;
        Ok(())
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
impl<A: Admission> Pending<A> {
    pub(super) fn start(
        admission: A,
        sequence: u64,
        kind: WriteKind,
        target: String,
        lock: Option<FileKey>,
        stop: Arc<Stop>,
        until: Instant,
    ) -> Result<Self> {
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
            Err(error) => {
                if let Some(mut admission) = state.lock().unwrap_or_else(|e| e.into_inner()).take()
                {
                    admission.release(sequence, kind);
                }
                Err(error.into())
            }
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
    /// Only after the executor/Source drain, or for a never-granted failed wait.
    pub(super) fn join(&mut self) -> (A, Outcome) {
        self.stop.cancel();
        let joined = self.lease.retire();
        let (admission, outcome) = self.take_ready();
        (
            admission,
            if let Err(error) = joined {
                Outcome::Finished(Err(error))
            } else {
                outcome
            },
        )
    }
}
impl<A: Admission> Drop for Pending<A> {
    fn drop(&mut self) {
        if self.lease.thread.is_some() {
            self.stop.cancel();
            let _ = self.lease.retire();
            if let Some(mut admission) = self.state.lock().unwrap_or_else(|e| e.into_inner()).take()
            {
                admission.release(self.sequence, self.kind);
            }
        }
    }
}
