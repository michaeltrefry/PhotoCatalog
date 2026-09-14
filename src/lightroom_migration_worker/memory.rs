//! Checked requested-Rust-storage admission shared by one migration operation.
//! A configured allowance is a resource decision, never a data-format ceiling.
pub(crate) mod channels;
pub(crate) mod core;
pub(crate) mod layout;
pub(crate) mod requested;
pub(crate) mod transport;

use anyhow::{Context, Result, ensure};
use std::{
    fmt,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub(crate) struct MemoryBudget(Arc<Backend>);
enum Backend {
    Local(Mutex<State>),
    Parent(Arc<dyn AllocationGrant>),
    Shared(crate::preview::ByteBudget),
}
/// A remote adapter retains every successful grant in its physical parent.
/// Dropping a child reservation never releases a possibly live cross-process
/// allocation.
pub(crate) trait AllocationGrant: Send + Sync {
    fn reserve(&self, bytes: usize) -> Result<()>;
    fn snapshot(&self) -> Option<Snapshot> {
        None
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub limit: usize,
    pub used: usize,
    pub available: usize,
}
#[derive(Clone, Debug)]
pub(crate) struct ResourceLimit {
    pub required: usize,
    pub available: usize,
}
impl fmt::Display for ResourceLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "migration allocation allowance exhausted; requested {} additional bytes, {} available",
            self.required, self.available
        )
    }
}
impl std::error::Error for ResourceLimit {}

struct State {
    limit: usize,
    used: usize,
}
pub(crate) struct Reservation {
    budget: MemoryBudget,
    held: usize,
    shared: Option<crate::preview::ByteReservation>,
}
impl MemoryBudget {
    pub(crate) fn new(limit: usize) -> Result<Self> {
        ensure!(
            limit != 0,
            "migration allocation allowance must be positive"
        );
        Ok(Self(Arc::new(Backend::Local(Mutex::new(State {
            limit,
            used: 0,
        })))))
    }
    pub(crate) fn from_parent(grant: Arc<dyn AllocationGrant>) -> Self {
        Self(Arc::new(Backend::Parent(grant)))
    }
    /// G-local operation scopes each own one growable token in the actual
    /// desktop pool. A separate result scope can therefore outlive drain
    /// without retaining unrelated process/input allowances.
    pub(crate) fn from_shared(budget: crate::preview::ByteBudget) -> Self {
        Self(Arc::new(Backend::Shared(budget)))
    }
    /// Parent snapshots are current shared-pool state. A child must never report
    /// a local counter as the parent pool's current available allowance.
    pub(crate) fn snapshot(&self) -> Result<Snapshot> {
        match &*self.0 {
            Backend::Local(state) => {
                let state = state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("migration allocation allowance poisoned"))?;
                Ok(Snapshot {
                    limit: state.limit,
                    used: state.used,
                    available: state.limit - state.used,
                })
            }
            Backend::Parent(grant) => grant
                .snapshot()
                .context("allocation availability belongs to the parent coordinator"),
            Backend::Shared(budget) => {
                let (limit, used) = budget.snapshot();
                let limit = usize::try_from(limit).context("shared allocation limit overflow")?;
                let used = usize::try_from(used).context("shared allocation usage overflow")?;
                Ok(Snapshot {
                    limit,
                    used,
                    available: limit
                        .checked_sub(used)
                        .context("shared allocation usage exceeds limit")?,
                })
            }
        }
    }
    pub(crate) fn reservation(&self) -> Reservation {
        Reservation {
            budget: self.clone(),
            held: 0,
            shared: None,
        }
    }
    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.snapshot().unwrap().used
    }
}
impl Reservation {
    pub(crate) fn budget(&self) -> MemoryBudget {
        self.budget.clone()
    }
    /// Admit a complete phase envelope without accumulating the same temporary
    /// allowance on every row. The high-water charge stays owned through drain;
    /// a smaller phase does not release payloads still retained by a sibling.
    pub(crate) fn ensure_at_least(&mut self, required: usize) -> Result<()> {
        if required > self.held {
            self.grow(required - self.held)?;
        }
        Ok(())
    }
    pub(crate) fn grow(&mut self, bytes: usize) -> Result<()> {
        let held = self
            .held
            .checked_add(bytes)
            .context("migration allocation charge overflow")?;
        match &*self.budget.0 {
            Backend::Local(state) => {
                let mut state = state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("migration allocation allowance poisoned"))?;
                let used = state
                    .used
                    .checked_add(bytes)
                    .context("migration allocation total overflow")?;
                if used > state.limit {
                    return Err(ResourceLimit {
                        required: bytes,
                        available: state.limit - state.used,
                    }
                    .into());
                }
                state.used = used;
            }
            // No local pool mutex is held through this IPC callback. Its exact
            // grant sequence is serialized by the protocol's owned waiting slot.
            Backend::Parent(grant) => grant.reserve(bytes)?,
            Backend::Shared(budget) => {
                let bytes = u64::try_from(bytes).context("shared allocation request overflow")?;
                let refused = if let Some(reservation) = &mut self.shared {
                    match reservation.grow_exact(bytes) {
                        Ok(()) => None,
                        Err(limit) => Some(limit),
                    }
                } else {
                    match budget.reserve_exact(bytes) {
                        Ok(reservation) => {
                            self.shared = Some(reservation);
                            None
                        }
                        Err(limit) => Some(limit),
                    }
                };
                if let Some(refused) = refused {
                    return Err(ResourceLimit {
                        required: usize::try_from(refused.required)
                            .context("shared allocation refusal request overflow")?,
                        available: usize::try_from(refused.available)
                            .context("shared allocation refusal availability overflow")?,
                    }
                    .into());
                }
            }
        }
        self.held = held;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Backend::Local(state) = &*self.budget.0 {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            // This owner alone retires its monotonic contribution. No allocation,
            // filesystem access, callback or process wait occurs under this lock.
            state.used -= self.held;
        }
        // Shared drops its single ByteReservation after the checked owner fields
        // above it; Parent deliberately has no local release acknowledgement.
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phase_high_water_reuses_temporary_allowance_and_preserves_sibling_charge() -> Result<()> {
        let pool = MemoryBudget::new(100)?;
        let mut caller = pool.reservation();
        caller.grow(20)?;
        let mut phases = pool.reservation();
        for required in [30, 70, 10, 70] {
            phases.ensure_at_least(required)?;
        }
        assert_eq!(pool.used(), 90);
        let error = phases.ensure_at_least(81).unwrap_err();
        let limit = error.downcast_ref::<ResourceLimit>().unwrap();
        assert_eq!((limit.required, limit.available), (11, 10));
        assert_eq!(pool.used(), 90);
        phases.ensure_at_least(80)?;
        assert_eq!(pool.used(), 100);
        drop(phases);
        assert_eq!(pool.used(), 20);
        drop(caller);
        assert_eq!(pool.used(), 0);
        Ok(())
    }

    #[test]
    fn shared_reservations_deny_before_change_and_retire_exactly() -> Result<()> {
        let budget = MemoryBudget::new(100)?;
        let mut sql = budget.reservation();
        let mut raw = budget.reservation();
        sql.grow(40)?;
        raw.grow(60)?;
        assert!(sql.grow(1).is_err());
        assert!(raw.grow(usize::MAX).is_err());
        assert_eq!(budget.used(), 100);
        drop(sql);
        assert_eq!(budget.used(), 60);
        raw.grow(40)?;
        drop(raw);
        assert_eq!(budget.used(), 0);
        Ok(())
    }
}
