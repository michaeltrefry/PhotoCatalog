//! Cooperative budgets for the serialized desktop inspection owner.
use anyhow::{Result, ensure};
use rusqlite::Connection;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(crate) struct Control {
    pub cancel: Arc<AtomicBool>,
    pub processed: Arc<AtomicU64>,
    remaining: Arc<AtomicU64>,
    until: Instant,
    pub row_bytes: usize,
    pub result_bytes: usize,
}
impl Control {
    pub fn new(
        cancel: Arc<AtomicBool>,
        vm_steps: u64,
        deadline_ms: u64,
        row_bytes: usize,
        result_bytes: usize,
    ) -> Result<Self> {
        ensure!(
            (1000..=100_000_000_000).contains(&vm_steps) && (1..=3_600_000).contains(&deadline_ms),
            "inspection execution budget"
        );
        ensure!(
            (1024..=128 * 1024 * 1024).contains(&row_bytes)
                && (1024..=256 * 1024 * 1024).contains(&result_bytes),
            "inspection byte budget"
        );
        Ok(Self {
            cancel,
            processed: Arc::new(AtomicU64::new(0)),
            remaining: Arc::new(AtomicU64::new(vm_steps)),
            until: Instant::now() + Duration::from_millis(deadline_ms),
            row_bytes,
            result_bytes,
        })
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.cancel.load(Ordering::Acquire),
            "inspection operation canceled; committed evidence remains resumable"
        );
        ensure!(
            Instant::now() < self.until,
            "inspection operation deadline exceeded"
        );
        ensure!(
            self.remaining.load(Ordering::Relaxed) > 0,
            "inspection SQL VM budget exceeded"
        );
        Ok(())
    }
    pub fn progress(&self, value: u64) {
        self.processed.store(value, Ordering::Release);
    }
}
unsafe extern "C" fn progress(context: *mut std::ffi::c_void) -> i32 {
    let control = unsafe { &*context.cast::<Control>() };
    if control.check().is_err() {
        return 1;
    }
    match control
        .remaining
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_sub(1000))
        }) {
        Ok(n) if n > 1000 => 0,
        _ => 1,
    }
}
/// Keep boxed control alive until clearing the callback or closing connection.
pub(crate) fn install(db: &Connection, control: Option<&Control>) {
    unsafe {
        rusqlite::ffi::sqlite3_progress_handler(
            db.handle(),
            if control.is_some() { 1000 } else { 0 },
            control.map(|_| progress as unsafe extern "C" fn(*mut std::ffi::c_void) -> i32),
            control.map_or(std::ptr::null_mut(), |c| {
                (c as *const Control).cast_mut().cast()
            }),
        );
    }
}
pub(crate) struct SqlControl<'a> {
    db: &'a Connection,
    _control: Box<Control>,
    previous_length: i32,
}

pub(crate) struct MetadataAllocation {
    pub(crate) boxed_control: usize,
    pub(crate) sql_control: usize,
    pub(crate) retained_arcs: usize,
    pub(crate) replacement_arcs: usize,
}

impl MetadataAllocation {
    pub(crate) fn active(&self) -> Result<usize> {
        Ok(self.replacement_arcs.max(
            self.boxed_control
                .checked_add(self.sql_control)
                .ok_or_else(|| anyhow::anyhow!("progress control metadata overflow"))?,
        ))
    }

    #[cfg(test)]
    fn required(&self) -> Result<usize> {
        self.retained_arcs
            .checked_add(self.active()?)
            .ok_or_else(|| anyhow::anyhow!("progress control high-water overflow"))
    }
}

pub(crate) fn metadata_allocation() -> Result<MetadataAllocation> {
    use crate::lightroom_migration_worker::memory::{
        channels,
        layout::{add, mul},
    };
    use std::alloc::Layout;

    // Control::new owns one cancellation flag and two independent counters.
    // Submission constructs the replacement before replacing Shared::control,
    // so both complete Arc graphs can coexist. Clones in Message/SqlControl
    // share those allocations and add only their inline/boxed Control roots.
    let arcs = add(
        channels::arc(Layout::new::<AtomicBool>())?,
        mul(2, channels::arc(Layout::new::<AtomicU64>())?)?,
    )?;
    Ok(MetadataAllocation {
        boxed_control: std::mem::size_of::<Control>(),
        sql_control: std::mem::size_of::<SqlControl<'static>>(),
        retained_arcs: arcs,
        replacement_arcs: arcs,
    })
}
impl<'a> SqlControl<'a> {
    pub fn new(db: &'a Connection, control: Control) -> Self {
        let control = Box::new(control);
        let previous_length = unsafe {
            rusqlite::ffi::sqlite3_limit(
                db.handle(),
                rusqlite::ffi::SQLITE_LIMIT_LENGTH,
                control.row_bytes as i32,
            )
        };
        install(db, Some(&control));
        Self {
            db,
            _control: control,
            previous_length,
        }
    }
}
impl Drop for SqlControl<'_> {
    fn drop(&mut self) {
        install(self.db, None);
        unsafe {
            rusqlite::ffi::sqlite3_limit(
                self.db.handle(),
                rusqlite::ffi::SQLITE_LIMIT_LENGTH,
                self.previous_length,
            );
        }
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn exact_progress_control_backing_admits_and_one_byte_less_refuses() -> Result<()> {
        let allocation = metadata_allocation()?;
        let required = allocation.required()?;
        let required_u64 = u64::try_from(required)?;
        let exact = crate::preview::ByteBudget::new(required_u64)?;
        let held = exact.reserve_exact(required_u64)?;
        assert_eq!(exact.used(), required_u64);
        drop(held);
        assert_eq!(exact.used(), 0);

        let short = crate::preview::ByteBudget::new(required_u64 - 1)?;
        assert!(short.reserve_exact(required_u64).is_err());
        Ok(())
    }
}
