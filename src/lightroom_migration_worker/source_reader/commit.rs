//! Nonblocking observation only. This module never installs SQLite callbacks;
//! the fresh destination-connection constructor owns that single hook slot.
use super::Health;
use anyhow::{Result, ensure};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct CommitHealth {
    sealed: Health,
    raw: Mutex<Option<Health>>,
    poisoned: AtomicBool,
}
impl CommitHealth {
    pub(crate) fn new(sealed: Health) -> Arc<Self> {
        Arc::new(Self {
            sealed,
            raw: Mutex::new(None),
            poisoned: AtomicBool::new(false),
        })
    }
    /// At most the sealed inspection and one current raw member are admitted.
    /// A busy/poisoned roster fails commit; the callback never waits for it.
    pub(crate) fn failed(&self) -> bool {
        let failed = self.poisoned.load(Ordering::Acquire)
            || self.sealed.failed()
            || match self.raw.try_lock() {
                Ok(raw) => raw.as_ref().is_some_and(Health::failed),
                Err(_) => true,
            };
        if failed {
            self.poisoned.store(true, Ordering::Release);
        }
        failed
    }
    pub(crate) fn attach_raw(self: &Arc<Self>, health: Health) -> Result<RawTicket> {
        ensure!(
            !self.failed() && !health.failed(),
            "source admission unavailable"
        );
        let mut raw = self.raw.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(raw.is_none(), "previous raw epoch has not drained");
        *raw = Some(health);
        Ok(RawTicket {
            owner: self.clone(),
            finished: false,
        })
    }
}
/// Explicit successful retirement follows source-reader reap AND all dependent
/// consumer SQL/permits. Error or unwind permanently poisons this operation.
pub(crate) struct RawTicket {
    owner: Arc<CommitHealth>,
    finished: bool,
}
impl RawTicket {
    pub(crate) fn finish(mut self) {
        self.finished = true;
    }
}
impl Drop for RawTicket {
    fn drop(&mut self) {
        if !self.finished {
            self.owner.poisoned.store(true, Ordering::Release);
        }
        self.owner
            .raw
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}
