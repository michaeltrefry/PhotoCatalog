//! Lexically scoped requested-storage accounting for core-owned Rust values.
//!
//! A scope names storage that can be live at the same time as its siblings. The
//! callback receives only a new local high water; the managed implementation
//! forwards it into the operation's existing monotonic core reservation. This
//! type neither owns a pool nor releases parent admission on scope drop.
use anyhow::{Context, Result};
use std::cell::Cell;

pub(crate) struct Requested<'a> {
    admit: &'a dyn Fn(usize) -> Result<()>,
    live: Cell<usize>,
    high: Cell<usize>,
}

pub(crate) struct Scope<'a> {
    requested: &'a Requested<'a>,
    bytes: usize,
}

impl<'a> Requested<'a> {
    pub(crate) fn new(admit: &'a dyn Fn(usize) -> Result<()>) -> Self {
        Self {
            admit,
            live: Cell::new(0),
            high: Cell::new(0),
        }
    }

    /// Admit before constructing the owner. The returned guard must be declared
    /// before that owner (or stored after it in a struct) so it drops last.
    pub(crate) fn scope(&'a self, bytes: usize) -> Result<Scope<'a>> {
        let live = self
            .live
            .get()
            .checked_add(bytes)
            .context("requested core storage overflow")?;
        if live > self.high.get() {
            (self.admit)(live)?;
            self.high.set(live);
        }
        self.live.set(live);
        Ok(Scope {
            requested: self,
            bytes,
        })
    }

    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.live.get()
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        self.requested.live.set(
            self.requested
                .live
                .get()
                .checked_sub(self.bytes)
                .expect("requested-storage scope imbalance"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lightroom_migration_worker::memory::MemoryBudget;
    use std::cell::RefCell;

    #[test]
    fn scopes_report_one_high_water_and_retire_local_contributions() -> Result<()> {
        let calls = RefCell::new(Vec::new());
        let admit = |bytes| {
            calls.borrow_mut().push(bytes);
            Ok(())
        };
        let requested = Requested::new(&admit);
        let first = requested.scope(10)?;
        let second = requested.scope(20)?;
        drop(second);
        let third = requested.scope(15)?;
        assert_eq!(requested.live(), 25);
        drop(third);
        drop(first);
        assert_eq!(requested.live(), 0);
        assert_eq!(*calls.borrow(), [10, 30]);
        Ok(())
    }

    #[test]
    fn denied_growth_does_not_publish_a_scope_or_change_live_storage() -> Result<()> {
        let admit = |bytes| {
            if bytes > 10 {
                anyhow::bail!("denied")
            }
            Ok(())
        };
        let requested = Requested::new(&admit);
        let first = requested.scope(10)?;
        assert!(requested.scope(1).is_err());
        assert_eq!(requested.live(), 10);
        drop(first);
        assert_eq!(requested.live(), 0);
        Ok(())
    }

    #[test]
    fn same_pool_denial_retries_after_competitor_release_and_owner_retires() -> Result<()> {
        let pool = MemoryBudget::new(30)?;
        let mut competitor = pool.reservation();
        competitor.grow(20)?;
        let operation = RefCell::new(pool.reservation());
        let admit = |bytes| operation.borrow_mut().ensure_at_least(bytes);
        let requested = Requested::new(&admit);
        assert!(requested.scope(11).is_err());
        assert_eq!(pool.used(), 20);
        drop(competitor);
        let scope = requested.scope(11)?;
        assert_eq!(pool.used(), 11);
        drop(scope);
        assert_eq!(pool.used(), 11);
        drop(requested);
        drop(operation);
        assert_eq!(pool.used(), 0);
        Ok(())
    }
}
