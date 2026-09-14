//! Checked requested-Rust-storage admission shared by one migration operation.
//! A configured allowance is a resource decision, never a data-format ceiling.
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(crate) struct MemoryBudget(Arc<Mutex<State>>);
struct State {
    limit: usize,
    used: usize,
}
pub(crate) struct Reservation {
    budget: MemoryBudget,
    held: usize,
}
impl MemoryBudget {
    pub(crate) fn new(limit: usize) -> Result<Self> {
        ensure!(
            limit != 0,
            "migration allocation allowance must be positive"
        );
        Ok(Self(Arc::new(Mutex::new(State { limit, used: 0 }))))
    }
    pub(crate) fn reservation(&self) -> Reservation {
        Reservation {
            budget: self.clone(),
            held: 0,
        }
    }
    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.0.lock().unwrap().used
    }
}
impl Reservation {
    pub(crate) fn grow(&mut self, bytes: usize) -> Result<()> {
        let held = self
            .held
            .checked_add(bytes)
            .context("migration allocation charge overflow")?;
        let mut state = self
            .budget
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("migration allocation allowance poisoned"))?;
        let used = state
            .used
            .checked_add(bytes)
            .context("migration allocation total overflow")?;
        ensure!(
            used <= state.limit,
            "migration allocation allowance exhausted; requested {bytes} additional bytes, {} available",
            state.limit - state.used
        );
        state.used = used;
        self.held = held;
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self.budget.0.lock().unwrap_or_else(|e| e.into_inner());
        // This owner alone retires its monotonic contribution. No allocation,
        // filesystem access, callback or process wait occurs under this lock.
        state.used -= self.held;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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
