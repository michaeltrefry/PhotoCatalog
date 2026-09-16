//! G retains C's preview metadata allowance from before configuration copying
//! through checked child wait, transport joins and the last named parent relay
//! owner. This is requested Rust
//! backing, not an OS memory limit or an allowance for native/image payloads.
use crate::{
    application::Config,
    preview::{ByteBudget, ByteReservation},
};
use anyhow::Result;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub(super) struct MetadataAllowanceExceeded {
    pub required: u64,
}
impl std::fmt::Display for MetadataAllowanceExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "preview metadata allowance unavailable; {} bytes required",
            self.required
        )
    }
}
impl std::error::Error for MetadataAllowanceExceeded {}

#[derive(Default)]
struct State {
    held: Option<ByteReservation>,
    workbench_granted: bool,
    awaiting_wait: bool,
}
#[derive(Default)]
struct Owned(Mutex<State>);
#[derive(Clone, Default)]
pub(super) struct ProcessReservation(Option<Arc<Owned>>);
pub(super) fn owned_layout() -> (usize, usize) {
    (std::mem::size_of::<Owned>(), std::mem::align_of::<Owned>())
}
impl ProcessReservation {
    pub fn reserve(config: &Config, budget: &ByteBudget) -> Result<Self> {
        let required = config.requested_preview_metadata_bytes()?;
        let held = budget
            .try_reserve(required)
            .ok_or(MetadataAllowanceExceeded { required })?;
        Ok(Self(Some(Arc::new(Owned(Mutex::new(State {
            held: Some(held),
            workbench_granted: false,
            awaiting_wait: false,
        }))))))
    }
    pub fn arm(&self) {
        if let Some(owned) = &self.0 {
            owned
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .awaiting_wait = true;
        }
    }
    /// Transfer the already-charged Workbench generation portion to the
    /// independent G owner. The aggregate C admission includes this exact
    /// amount as retained backing, so the shared pool is not charged again.
    #[allow(dead_code)] // Consumed by the final managed Workbench factory.
    pub(super) fn split_workbench(&self, config: &Config) -> Result<ByteReservation> {
        let required = crate::application::lightroom_capacity::Requirement::from_config(
            config,
            super::CONTROL_SLOTS,
        )?;
        let owned = self
            .0
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("managed Workbench requires process metadata"))?;
        let mut state = owned.0.lock().unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            !state.workbench_granted,
            "Workbench metadata already granted"
        );
        let grant = state
            .held
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("process metadata reservation is unavailable"))?
            .split_exact(required.bytes())
            .map_err(anyhow::Error::new)?;
        state.workbench_granted = true;
        Ok(grant)
    }
    /// Caller proves either OS spawn returned no child, or Child::wait succeeded
    /// and every transport thread has been joined. EOF and drain replies alone
    /// must never call this method. Disarming permits final-owner drop to release
    /// the charge; it does not release still-owned parent relay storage.
    pub fn retire(&self) {
        if let Some(owned) = &self.0 {
            owned
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .awaiting_wait = false;
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        let state = self.0.get_mut().unwrap_or_else(|e| e.into_inner());
        if state.awaiting_wait {
            // A lost/panicked supervisor cannot prove C's owner graph is gone.
            // Keep its charge even if all parent façades have been dropped.
            std::mem::forget(state.held.take());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        Config {
            worker_executable: std::env::current_exe().unwrap(),
            cache_root: None,
            original_roots: vec![],
            preview_policy: Default::default(),
            preview_limits: Default::default(),
            limits: Default::default(),
            import_checkpoint: None,
        }
    }
    #[test]
    fn shared_allowance_refuses_without_charge_then_retries() -> Result<()> {
        let config = config();
        let required = config.requested_preview_metadata_bytes()?;
        let pool = ByteBudget::new(required + 17)?;
        let caller = pool.try_reserve(18).unwrap();
        let denied = ProcessReservation::reserve(&config, &pool).err().unwrap();
        assert_eq!(
            denied
                .downcast_ref::<MetadataAllowanceExceeded>()
                .unwrap()
                .required,
            required
        );
        assert_eq!(pool.used(), 18);
        drop(caller);
        let caller = pool.try_reserve(17).unwrap();
        let reservation = ProcessReservation::reserve(&config, &pool)?;
        assert_eq!(pool.used(), required + 17);
        drop(reservation); // No spawn was attempted.
        assert_eq!(pool.used(), 17);
        drop(caller);
        assert_eq!(pool.used(), 0);
        Ok(())
    }
    #[test]
    fn possible_child_keeps_charge_until_explicit_retirement() -> Result<()> {
        let config = config();
        let required = config.requested_preview_metadata_bytes()?;
        let pool = ByteBudget::new(required)?;
        let reservation = ProcessReservation::reserve(&config, &pool)?;
        reservation.arm();
        assert!(ProcessReservation::reserve(&config, &pool).is_err());
        reservation.retire();
        reservation.retire(); // No double release.
        assert_eq!(pool.used(), required); // Parent metadata is still owned.
        drop(reservation);
        assert_eq!(pool.used(), 0);
        let retry = ProcessReservation::reserve(&config, &pool)?;
        retry.arm();
        drop(retry); // Simulated lost wait owner: the allowance stays charged.
        assert_eq!(pool.used(), required);
        assert!(ProcessReservation::reserve(&config, &pool).is_err());
        Ok(())
    }
    #[test]
    fn workbench_subgrant_transfers_the_aggregate_charge_once() -> Result<()> {
        let config = config();
        let required = config.requested_preview_metadata_bytes()?;
        let workbench = crate::application::lightroom_capacity::Requirement::from_config(
            &config,
            super::CONTROL_SLOTS,
        )?;
        let pool = ByteBudget::new(required)?;
        let reservation = ProcessReservation::reserve(&config, &pool)?;
        let grant = reservation.split_workbench(&config)?;
        assert_eq!(grant.bytes(), workbench.bytes());
        assert_eq!(pool.used(), required);
        assert!(reservation.split_workbench(&config).is_err());
        drop(grant);
        assert_eq!(pool.used(), required - workbench.bytes());
        drop(reservation);
        assert_eq!(pool.used(), 0);
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn actual_no_child_spawn_error_releases_charge() -> Result<()> {
        let config = config();
        let required = config.requested_preview_metadata_bytes()?;
        let pool = ByteBudget::new(required)?;
        let mut shared = super::super::tests::shared(8);
        std::sync::Arc::get_mut(&mut shared).unwrap().metadata =
            ProcessReservation::reserve(&config, &pool)?;
        // A regular non-executable temporary file is a deterministic OS spawn
        // error without reading any user files or launching another process.
        let file = tempfile::NamedTempFile::new()?;
        assert!(super::super::process::Owner::spawn(file.path(), shared.clone(), vec![]).is_err());
        assert_eq!(pool.used(), required); // Parent-side state remains held.
        drop(shared);
        assert_eq!(pool.used(), 0);
        Ok(())
    }

    #[test]
    #[ignore = "configured actual C/F shared allowance refusal and retry"]
    fn actual_managed_startup_refusal_preserves_type_and_retries_same_pool() -> Result<()> {
        use super::super::{DesktopBridge, filesystem};
        use std::sync::Arc;
        let mut config = config();
        config.worker_executable = std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE")
            .ok_or_else(|| anyhow::anyhow!("configured CLI required"))?
            .into();
        let required = config.requested_preview_metadata_bytes()?;
        let pool = ByteBudget::new(required)?;
        let native = ByteBudget::new(config.preview_limits.working_bytes)?;
        let caller = pool.try_reserve(1).unwrap();
        let client = Arc::new(crate::filesystem_worker::client::Client::spawn(
            &config.worker_executable,
            vec![],
        )?);
        let denied = DesktopBridge::spawn_with_filesystem(config.clone(), client, &pool, &native)
            .err()
            .ok_or_else(|| anyhow::anyhow!("undersized startup unexpectedly succeeded"))?;
        let retained = denied
            .downcast_ref::<filesystem::Unstarted>()
            .ok_or_else(|| anyhow::anyhow!("F startup owner was not retained"))?;
        let parent = retained.owner.clone();
        let checked = (|| -> Result<()> {
            anyhow::ensure!(
                denied
                    .downcast_ref::<MetadataAllowanceExceeded>()
                    .is_some_and(|error| error.required == required),
                "resource refusal type lost"
            );
            anyhow::ensure!(pool.used() == 1, "rejected startup charged allowance");
            Ok(())
        })();
        drop(caller);
        if let Err(error) = checked {
            retained.retire()?;
            return Err(error);
        }
        let bridge = DesktopBridge::spawn_inner(config.clone(), Some(parent.clone()), Some(&pool))?;
        anyhow::ensure!(pool.used() == required, "successful retry not charged");
        bridge.try_shutdown()?;
        parent.finish_after_dependents(false)?;
        anyhow::ensure!(
            pool.used() == required,
            "live parent metadata released early"
        );
        drop(bridge);
        drop(denied);
        drop(parent);
        anyhow::ensure!(pool.used() == 0, "retired owners retained metadata");
        let client = Arc::new(crate::filesystem_worker::client::Client::spawn(
            &config.worker_executable,
            vec![],
        )?);
        let second = DesktopBridge::spawn_with_filesystem(config, client, &pool, &native)?;
        anyhow::ensure!(pool.used() == required, "new C startup was not charged");
        second.try_shutdown()?;
        drop(second);
        anyhow::ensure!(pool.used() == 0, "second C owners retained metadata");
        eprintln!(
            "actual startup required={required}; refused with one byte held; same C/F parent and budget retry retired"
        );
        Ok(())
    }
}
