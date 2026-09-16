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

struct Starting {
    filesystem: Arc<filesystem::Parent>,
    metadata: preview_metadata_admission::ProcessReservation,
    // Keep the split alive until either Coordinator owns it or checked startup
    // cleanup disarms the aggregate reservation.
    _migration: migration::Funding,
    dependencies: Option<Arc<super::super::lightroom_managed::Owner>>,
    generation: Option<Arc<super::super::lightroom_managed::Generation>>,
}
impl Starting {
    fn drain(&self) -> anyhow::Result<()> {
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
    let mut starting = Starting {
        filesystem: parent.clone(),
        metadata: metadata.clone(),
        _migration: migration.clone(),
        dependencies: None,
        generation: None,
    };
    let result = (|| {
        parent.retain_metadata(metadata.clone())?;
        client.wait_ready(std::time::Duration::from_secs(30))?;
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
        let dispatcher = workbench::Dispatcher::start(&generation, config.limits.clone())?;
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
}
