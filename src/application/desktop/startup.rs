//! Production admission and retained startup ownership before C exists.
use super::*;
use crate::preview::ByteBudget;
use anyhow::ensure;

/// Independent caller-owned allowances. An explicitly smaller operation pool
/// produces a ResourceLimit at admission; it never borrows native or metadata bytes.
pub struct Allocations {
    pub metadata: ByteBudget,
    pub native: ByteBudget,
    pub workbench_source: ByteBudget,
    pub migration_source: ByteBudget,
    pub migration_result: ByteBudget,
}
impl Allocations {
    pub fn for_config(config: &Config) -> anyhow::Result<Self> {
        let (source, result) = migration::operation_requirements(config)?;
        Ok(Self {
            metadata: ByteBudget::new(config.requested_preview_metadata_bytes()?)?,
            native: ByteBudget::new(config.preview_limits.working_bytes)?,
            workbench_source: ByteBudget::new(
                super::super::lightroom_capacity::source_requirement()?,
            )?,
            migration_source: ByteBudget::new(source)?,
            migration_result: ByteBudget::new(result)?,
        })
    }
    fn validate(&self, config: &Config) -> anyhow::Result<()> {
        let pools = [
            &self.metadata,
            &self.native,
            &self.workbench_source,
            &self.migration_source,
            &self.migration_result,
        ];
        for (i, pool) in pools.iter().enumerate() {
            for other in &pools[i + 1..] {
                ensure!(
                    !pool.same_pool(other),
                    "desktop ownership allowances must use distinct pools"
                );
            }
        }
        ensure!(
            self.native.snapshot().0 == config.preview_limits.working_bytes,
            "native allowance differs from configured working bytes"
        );
        Ok(())
    }
}

#[cfg(test)]
#[derive(Default)]
struct TestFaults {
    after_f_ready: std::sync::atomic::AtomicBool,
    after_w_start: std::sync::atomic::AtomicBool,
    nested_dispatcher_failure: std::sync::atomic::AtomicBool,
    fail_next_cleanup: std::sync::atomic::AtomicBool,
    f_pid: std::sync::atomic::AtomicU32,
    w_pid: std::sync::atomic::AtomicU32,
}

struct Starting {
    filesystem: Arc<filesystem::Parent>,
    metadata: preview_metadata_admission::ProcessReservation,
    // Keep the split alive until either Coordinator owns it or checked startup
    // cleanup disarms the aggregate reservation.
    _migration: migration::Funding,
    dependencies: Option<Arc<super::super::lightroom_managed::Owner>>,
    generation: Option<Arc<super::super::lightroom_managed::Generation>>,
    #[cfg(test)]
    faults: Arc<TestFaults>,
}
impl Starting {
    fn drain(&self) -> anyhow::Result<()> {
        #[cfg(test)]
        if self
            .faults
            .fail_next_cleanup
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            anyhow::bail!("injected retained managed startup cleanup");
        }
        if let Some(generation) = &self.generation {
            generation.shutdown_checked()?;
        } else if let Some(dependencies) = &self.dependencies {
            dependencies.drain_checked()?;
        }
        self.filesystem.finish_after_dependents(true)?;
        self.metadata.retire();
        Ok(())
    }
}
/// Startup never substitutes fresh owners for an uncertain old generation.
/// Keep this error to retry checked cleanup when the environment recovers.
pub struct StartupFailure {
    message: String,
    cause: anyhow::Error,
    retained: Mutex<Option<Starting>>,
}
impl std::fmt::Debug for StartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedDesktopStartupFailure")
            .field("message", &self.message)
            .finish()
    }
}
impl std::fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; managed startup owners retained", self.message)
    }
}
impl std::error::Error for StartupFailure {}
impl StartupFailure {
    pub fn try_shutdown(&self) -> anyhow::Result<()> {
        if let Some(nested) = self.cause.downcast_ref::<super::RetainedStartup>() {
            nested.try_shutdown()?;
        }
        let mut retained = self.retained.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(starting) = retained.as_ref() {
            starting.drain()?;
        }
        retained.take();
        Ok(())
    }
}
impl Drop for StartupFailure {
    fn drop(&mut self) {
        if self.try_shutdown().is_err() {
            std::mem::forget(
                self.retained
                    .get_mut()
                    .unwrap_or_else(|e| e.into_inner())
                    .take(),
            );
        }
    }
}

pub(super) fn spawn(config: Config, pools: Allocations) -> anyhow::Result<DesktopBridge> {
    #[cfg(test)]
    return spawn_inner(config, pools, Arc::new(TestFaults::default()));
    #[cfg(not(test))]
    spawn_inner(config, pools)
}

fn spawn_inner(
    config: Config,
    pools: Allocations,
    #[cfg(test)] faults: Arc<TestFaults>,
) -> anyhow::Result<DesktopBridge> {
    config.validate()?;
    pools.validate(&config)?;
    let metadata =
        preview_metadata_admission::ProcessReservation::reserve(&config, &pools.metadata)?;
    let migration = migration::Funding::from_subgrant(
        &config,
        metadata.split_migration(&config)?,
        pools.migration_source,
        pools.migration_result,
    )?;
    let requirement =
        super::super::lightroom_capacity::Requirement::from_config(&config, CONTROL_SLOTS)?;
    // Admission precedes the first filesystem process and all subordinate owners.
    let allocation = super::super::lightroom_capacity::Allocation::from_subgrant(
        requirement,
        metadata.split_workbench(&config)?,
        pools.workbench_source,
    )?;
    let roots = config
        .original_roots
        .iter()
        .map(|p| NativePath::from_path(p))
        .collect();
    metadata.arm();
    let client =
        match crate::filesystem_worker::client::Client::spawn(&config.worker_executable, roots) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                metadata.retire();
                return Err(error);
            }
        };
    let parent = filesystem::Parent::new(client.clone());
    #[cfg(test)]
    faults
        .f_pid
        .store(client.pid(), std::sync::atomic::Ordering::Release);
    let mut starting = Starting {
        filesystem: parent.clone(),
        metadata: metadata.clone(),
        _migration: migration.clone(),
        dependencies: None,
        generation: None,
        #[cfg(test)]
        faults: faults.clone(),
    };
    let result = (|| {
        parent.retain_metadata(metadata.clone())?;
        client.wait_ready(std::time::Duration::from_secs(30))?;
        #[cfg(test)]
        if faults
            .after_f_ready
            .load(std::sync::atomic::Ordering::Acquire)
        {
            anyhow::bail!("injected startup failure after F readiness");
        }
        parent.configure_native(
            config.worker_executable.clone(),
            config.preview_limits.clone(),
            &pools.native,
        )?;
        parent.configure_export_native(
            config.worker_executable.clone(),
            config.preview_limits.workers,
            &pools.native,
        )?;
        let dependencies = super::super::lightroom_managed::Owner::start(
            &client,
            &config.worker_executable,
            allocation,
        )?;
        starting.dependencies = Some(dependencies.clone());
        let generation = Arc::new(super::super::lightroom_managed::Generation::start(
            &dependencies,
            &config.worker_executable,
        )?);
        starting.generation = Some(generation.clone());
        #[cfg(test)]
        {
            if let Some(pid) = generation.pid() {
                faults
                    .w_pid
                    .store(pid, std::sync::atomic::Ordering::Release);
            }
            if faults
                .after_w_start
                .load(std::sync::atomic::Ordering::Acquire)
            {
                anyhow::bail!("injected startup failure after W start");
            }
        }
        let dispatcher = workbench::Dispatcher::start(&generation, config.limits.clone())?;
        #[cfg(test)]
        if faults
            .nested_dispatcher_failure
            .load(std::sync::atomic::Ordering::Acquire)
        {
            dispatcher.fail_next_shutdown_for_test();
            // This is the exact owner shape returned by spawn_reserved when C
            // cannot spawn and the first dispatcher cleanup attempt fails.
            let cleanup = dispatcher
                .shutdown_checked()
                .expect_err("injected dispatcher shutdown must fail");
            return Err(anyhow::Error::new(super::RetainedStartup {
                message: format!("injected C spawn refusal; Workbench cleanup: {cleanup}"),
                owners: Mutex::new(Some(super::StartupOwners {
                    local: super::LocalOwner::Managed(dispatcher),
                    filesystem: Some(parent.clone()),
                })),
            }));
        }
        DesktopBridge::spawn_reserved(
            config,
            Some(parent),
            metadata,
            Some(migration),
            Some(dispatcher),
        )
    })();
    match result {
        Ok(bridge) => Ok(bridge),
        Err(error) => match starting.drain() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(anyhow::Error::new(StartupFailure {
                message: format!("{error:#}; cleanup: {cleanup:#}"),
                cause: error,
                retained: Mutex::new(Some(starting)),
            })),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use anyhow::Context;
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
    fn explicit_pool_aliases_fail_before_any_admission() -> anyhow::Result<()> {
        let config = config();
        let mut pools = Allocations::for_config(&config)?;
        pools.migration_result = pools.workbench_source.clone();
        let observed = pools.workbench_source.clone();
        let error = match spawn(config, pools) {
            Ok(_) => anyhow::bail!("aliased pools admitted"),
            Err(error) => error,
        };
        ensure!(error.to_string().contains("distinct pools"));
        ensure!(observed.used() == 0);
        Ok(())
    }
    #[test]
    fn aggregate_required_minus_one_fails_before_filesystem_creation() -> anyhow::Result<()> {
        let config = config();
        let mut pools = Allocations::for_config(&config)?;
        pools.metadata = ByteBudget::new(config.requested_preview_metadata_bytes()? - 1)?;
        let metadata = pools.metadata.clone();
        let source = pools.workbench_source.clone();
        ensure!(spawn(config, pools).is_err());
        ensure!(metadata.used() == 0 && source.used() == 0);
        Ok(())
    }

    #[cfg(unix)]
    struct ObservedPools {
        metadata: ByteBudget,
        native: ByteBudget,
        workbench_source: ByteBudget,
        migration_source: ByteBudget,
        migration_result: ByteBudget,
    }

    #[cfg(unix)]
    impl ObservedPools {
        fn capture(allocations: &Allocations) -> Self {
            Self {
                metadata: allocations.metadata.clone(),
                native: allocations.native.clone(),
                workbench_source: allocations.workbench_source.clone(),
                migration_source: allocations.migration_source.clone(),
                migration_result: allocations.migration_result.clone(),
            }
        }

        fn usage(&self) -> [u64; 5] {
            [
                self.metadata.used(),
                self.native.used(),
                self.workbench_source.used(),
                self.migration_source.used(),
                self.migration_result.used(),
            ]
        }

        fn ensure_released(&self) -> anyhow::Result<()> {
            ensure!(
                self.usage() == [0; 5],
                "managed startup pools remain charged: {:?}",
                self.usage()
            );
            Ok(())
        }
    }

    #[cfg(unix)]
    fn actual_config() -> anyhow::Result<Config> {
        let mut config = config();
        config.worker_executable = std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE")
            .context("fresh exact CLI required")?
            .into();
        ensure!(
            config.worker_executable.is_absolute(),
            "actual CLI must be absolute"
        );
        Ok(config)
    }

    #[cfg(unix)]
    fn pid_is_live(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(unix)]
    fn ensure_reaped(pid: u32) -> anyhow::Result<()> {
        ensure!(pid != 0, "startup fault did not observe its process");
        ensure!(!pid_is_live(pid), "owned process {pid} remains live");
        ensure!(
            std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "owned process {pid} disappearance was not verified"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn actual_failure_after_f_ready_reaps_f_and_releases_all_pools() -> anyhow::Result<()> {
        let config = actual_config()?;
        let allocations = Allocations::for_config(&config)?;
        let pools = ObservedPools::capture(&allocations);
        let faults = Arc::new(TestFaults::default());
        faults
            .after_f_ready
            .store(true, std::sync::atomic::Ordering::Release);
        let error = spawn_inner(config, allocations, faults.clone())
            .err()
            .context("after-F startup fault unexpectedly succeeded")?;
        ensure!(error.to_string().contains("after F readiness"));
        ensure_reaped(faults.f_pid.load(std::sync::atomic::Ordering::Acquire))?;
        pools.ensure_released()
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn actual_failure_after_w_start_reaps_w_then_f_and_releases_all_pools() -> anyhow::Result<()> {
        let config = actual_config()?;
        let allocations = Allocations::for_config(&config)?;
        let pools = ObservedPools::capture(&allocations);
        let faults = Arc::new(TestFaults::default());
        faults
            .after_w_start
            .store(true, std::sync::atomic::Ordering::Release);
        let error = spawn_inner(config, allocations, faults.clone())
            .err()
            .context("after-W startup fault unexpectedly succeeded")?;
        ensure!(error.to_string().contains("after W start"));
        ensure_reaped(faults.w_pid.load(std::sync::atomic::Ordering::Acquire))?;
        ensure_reaped(faults.f_pid.load(std::sync::atomic::Ordering::Acquire))?;
        pools.ensure_released()
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn actual_nested_dispatcher_failure_retains_exact_owners_until_explicit_retry()
    -> anyhow::Result<()> {
        let config = actual_config()?;
        let allocations = Allocations::for_config(&config)?;
        let pools = ObservedPools::capture(&allocations);
        let faults = Arc::new(TestFaults::default());
        faults
            .nested_dispatcher_failure
            .store(true, std::sync::atomic::Ordering::Release);
        faults
            .fail_next_cleanup
            .store(true, std::sync::atomic::Ordering::Release);

        let error = spawn_inner(config, allocations, faults.clone())
            .err()
            .context("nested startup fault unexpectedly succeeded")?;
        let retained = error
            .downcast_ref::<StartupFailure>()
            .context("outer retryable startup owner was not retained")?;
        let nested = retained
            .cause
            .downcast_ref::<super::super::RetainedStartup>()
            .context("nested dispatcher startup owner was not retained")?;
        ensure!(
            nested
                .owners
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .as_ref()
                .is_some_and(|owners| {
                    matches!(&owners.local, super::super::LocalOwner::Managed(_))
                }),
            "nested retained owner lost the managed dispatcher"
        );
        let f_pid = faults.f_pid.load(std::sync::atomic::Ordering::Acquire);
        let w_pid = faults.w_pid.load(std::sync::atomic::Ordering::Acquire);
        ensure!(pid_is_live(f_pid) && pid_is_live(w_pid));
        let retained_usage = pools.usage();
        ensure!(
            retained_usage[0] > 0 && retained_usage[2] > 0,
            "aggregate and Workbench Source charges were not retained: {retained_usage:?}"
        );

        retained.try_shutdown()?;
        ensure_reaped(w_pid)?;
        ensure_reaped(f_pid)?;
        pools.ensure_released()?;
        ensure!(
            nested
                .owners
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .is_none(),
            "nested dispatcher owner remained after checked retry"
        );
        ensure!(
            retained
                .retained
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .is_none(),
            "outer startup owner remained after checked retry"
        );
        retained.try_shutdown()
    }
}
