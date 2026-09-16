//! Cooperative control for detached export execution. Observation cannot inject
//! failures; legacy crash-injection hooks remain separate from this contract.
use anyhow::{Result, ensure};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportCheckpoint {
    Hashing { bytes: u64 },
    Alias,
    BeforeMutation,
    OriginalVerified,
    IntentCommitted,
    Captured,
    CaptureVerified,
    Linked,
    Finalizing,
    InstalledVerified,
}

pub struct ExportControl<'a> {
    canceled: &'a AtomicBool,
    observer: Option<&'a mut dyn FnMut(ExportCheckpoint)>,
    finishing: bool,
}
impl<'a> ExportControl<'a> {
    pub fn new(canceled: &'a AtomicBool) -> Self {
        Self {
            canceled,
            observer: None,
            finishing: false,
        }
    }
    pub fn observed(
        canceled: &'a AtomicBool,
        observer: &'a mut dyn FnMut(ExportCheckpoint),
    ) -> Self {
        Self {
            canceled,
            observer: Some(observer),
            finishing: false,
        }
    }
    pub fn cancel_requested(&self) -> bool {
        self.canceled.load(Ordering::Acquire)
    }
    pub(crate) fn cancellation(&self) -> &AtomicBool {
        self.canceled
    }
    pub(crate) fn finishing(&self) -> bool {
        self.finishing
    }
    pub(crate) fn begin(&mut self) -> Result<()> {
        self.finishing = false;
        self.check(ExportCheckpoint::BeforeMutation)
    }
    pub(crate) fn check(&mut self, point: ExportCheckpoint) -> Result<()> {
        if let Some(observer) = self.observer.as_mut() {
            observer(point);
        }
        ensure!(
            self.finishing || !self.cancel_requested(),
            "export cancellation requested; retained evidence requires explicit resume/recovery"
        );
        Ok(())
    }
    pub(crate) fn hash(&mut self, bytes: u64) -> std::io::Result<()> {
        self.check(ExportCheckpoint::Hashing { bytes })
            .map_err(std::io::Error::other)
    }
    pub(crate) fn linked(&mut self) {
        self.finishing = true;
        if let Some(observer) = self.observer.as_mut() {
            observer(ExportCheckpoint::Linked);
        }
    }
}
