//! An admitted encoded cache transfer. SQL remains on the service actor.
use super::*;
use crate::catalog_session::preview_io::Integrity;
use crate::preview::transport_task::Task;
use std::sync::atomic::{AtomicBool, Ordering};

struct IoLease(Arc<AtomicBool>);
impl Drop for IoLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
pub(crate) struct Gate {
    _native: NativeLaunchPause,
    blocked: Arc<AtomicBool>,
}
impl Drop for Gate {
    fn drop(&mut self) {
        self.blocked.store(false, Ordering::Release);
    }
}
pub(crate) struct Plan {
    selected: super::super::store::ManagedSelection,
    key: PreviewKey,
    allowance: u64,
    reservation: ByteReservation,
    io: IoLease,
}
pub(crate) struct Transfer {
    task: Task<Output>,
}
pub(crate) struct Output {
    pub key: PreviewKey,
    pub result: Result<(Integrity, Vec<u8>)>,
    _reservation: ByteReservation,
    _io: IoLease,
}
impl Plan {
    pub fn bytes(&self) -> u64 {
        self.selected.expected.bytes.0
    }
    pub fn start(self, cancel: Arc<AtomicBool>) -> Result<Transfer> {
        let task = Task::spawn("preview-encoded-delivery", cancel, move |cancel| {
            let result = self
                .selected
                .files
                .cache_read_cancel(self.selected.expected, self.allowance, &cancel)
                .and_then(|(integrity, bytes)| {
                    ensure!(
                        bytes.capacity() as u64 <= self.allowance,
                        EncodedBudgetExceeded
                    );
                    // The returned payload has a capacity equal to its length, so
                    // its binary reservation covers the Vec handed to transport.
                    let bytes = bytes.into_boxed_slice().into_vec();
                    Ok((integrity, bytes))
                });
            Ok(Output {
                key: self.key,
                result,
                _reservation: self.reservation,
                _io: self.io,
            })
        })?;
        Ok(Transfer { task })
    }
}
impl Transfer {
    pub fn poll(&mut self) -> Result<Option<Output>> {
        self.task.poll()
    }
    pub fn signal_cancel(&self) {
        self.task.signal_cancel();
    }
    pub fn shutdown(&mut self) -> Result<Option<Output>> {
        self.task.shutdown()
    }
}
impl Output {
    pub fn finish(self, service: &PreviewService) -> Result<Vec<u8>> {
        let (integrity, bytes) = self.result?;
        service.store.finish_managed_read(&self.key, integrity)?;
        ensure!(
            integrity == Integrity::Intact,
            "encoded cache object changed during delivery"
        );
        Ok(bytes)
    }
}
impl PreviewService {
    pub(crate) fn pause_for_encoded_delivery(&self) -> Result<Gate> {
        ensure!(
            !self.reads.active(),
            super::super::stage_io::Busy("encoded delivery awaits active read drain")
        );
        let native = self.pause_native_launches()?;
        self.delivery_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| super::super::stage_io::Busy("encoded delivery already queued"))?;
        Ok(Gate {
            _native: native,
            blocked: self.delivery_pending.clone(),
        })
    }
    /// The preceding ReadQueue completion provides the same N/decoded-hit
    /// validation as legacy encoded_cached_variant. This step selects SQL only.
    pub(crate) fn prepare_encoded_delivery(&self, view: &PreviewView) -> Result<Plan> {
        ensure!(
            !self.reads.transport_busy()
                && !self.active.values().any(|a| a.worker.transport_busy()),
            super::super::stage_io::Busy(
                "encoded delivery busy: another filesystem task remains active"
            )
        );
        self.delivery_io
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| super::super::stage_io::Busy("encoded delivery busy"))?;
        let io = IoLease(self.delivery_io.clone());
        let remaining = self.limits.encoded_staging_bytes - self.encoded.used();
        let key = view
            .key
            .clone()
            .context("current encoded delivery requires a manifest key")?;
        let selected = self
            .store
            .select_managed_read(&key, false, remaining)?
            .context("cache changed during delivery")?;
        let allowance = selected.expected.bytes.0;
        let reservation = self
            .encoded
            .try_reserve(allowance)
            .ok_or(EncodedBudgetExceeded)?;
        Ok(Plan {
            selected,
            key,
            allowance,
            reservation,
            io,
        })
    }
}

#[cfg(test)]
#[path = "encoded_delivery/tests.rs"]
mod tests;
