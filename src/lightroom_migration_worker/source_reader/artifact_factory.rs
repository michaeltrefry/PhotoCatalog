use super::{CommitHealth, RawReader, relay::client::Client};
use crate::{
    Catalog,
    catalog_migration::{
        artifacts::{ArtifactLimits, ArtifactRead, ArtifactRequest},
        import_artifacts::ArtifactFactory,
    },
    lightroom_migration_worker::{identity::FileKey, protocol::Guard},
};
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, atomic::AtomicBool};

/// The selected-import Worker still owns only one active raw member. Each new
/// member receives a new full-hash admission and cannot reuse a failed epoch.
pub(crate) struct RemoteArtifacts {
    pub relay: Arc<Client>,
    pub guard: Guard,
    pub protected: Vec<FileKey>,
    pub cancel: Arc<AtomicBool>,
    pub health: Arc<CommitHealth>,
    pub next_epoch: u64,
}
impl ArtifactFactory for RemoteArtifacts {
    fn open(
        &mut self,
        catalog: &Catalog,
        request: ArtifactRequest,
        limits: ArtifactLimits,
        stop: &dyn Fn() -> bool,
    ) -> Result<Box<dyn ArtifactRead>> {
        ensure!(
            !stop() && !self.health.failed(),
            "artifact source admission stopped"
        );
        self.relay.admit_core(
            crate::lightroom_migration_worker::memory::core::artifact_constructor(&request)?,
        )?;
        // Destination SQL only: the historical manifest locator is not opened.
        let descriptor = catalog.migration_artifact_reader_descriptor(&request)?;
        let reader_epoch = format!("artifact-{}", self.next_epoch);
        self.next_epoch = self
            .next_epoch
            .checked_add(1)
            .context("artifact epoch exhausted")?;
        let mut reader = RawReader::open(
            self.relay.clone(),
            self.guard.clone(),
            reader_epoch,
            descriptor,
            limits,
            self.protected.clone(),
            self.cancel.clone(),
        )?;
        reader.attach(&self.health)?;
        ensure!(
            !stop() && !self.health.failed(),
            "artifact source admission canceled"
        );
        Ok(Box::new(reader))
    }
}
