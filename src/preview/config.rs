use super::*;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
/// Explicit application settings. Cache manifest locations/budgets supersede
/// stale copies of these initial values after a durable relocation/config change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewConfiguration {
    pub store: StoreConfig,
    pub policy: PreviewPolicy,
    pub limits: ServiceLimits,
    pub original_roots: Vec<PathBuf>,
}
impl PreviewConfiguration {
    pub fn read(path: &Path) -> Result<Self> {
        let mut file = std::fs::File::open(path).context("open preview configuration")?;
        let length = file.metadata()?.len();
        ensure!(length <= 64 * 1024, "preview configuration size limit");
        let mut bytes = vec![0; length as usize];
        file.read_exact(&mut bytes)?;
        let mut extra = [0];
        ensure!(file.read(&mut extra)? == 0, "preview configuration grew");
        let value: Self = serde_json::from_slice(&bytes)?;
        ensure!(value.original_roots.len() <= 1024, "original root limit");
        Ok(value)
    }
    pub fn open(
        &self,
        worker: PathBuf,
        additional_original: Option<&Path>,
    ) -> Result<PreviewService> {
        let mut roots = self.original_roots.clone();
        if let Some(path) = additional_original {
            roots.push(path.to_path_buf());
        }
        PreviewService::open(
            self.store.clone(),
            &roots,
            worker,
            self.policy.clone(),
            self.limits.clone(),
        )
    }
}
