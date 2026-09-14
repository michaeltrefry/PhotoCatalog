//! Private, descriptor-free preview ownership protocol. F owns every lock.
use super::{LeaseId, RootCapability, validate_path};
use crate::{
    preview::{Layout, Tier},
    storage_volume::NativePath,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const ROOT_SLOTS: usize = 16;
pub const OWNED_BYTES: usize = 2 * 1024 * 1024;
pub const CONFIG_BYTES: usize = 64 * 1024;
pub const MARKER_BYTES: usize = 256;

#[derive(Debug)]
pub struct ResourceLimit(pub &'static str);
impl std::fmt::Display for ResourceLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ResourceLimit {}

pub fn marker(identity: &str, tier: Tier, layout: Layout) -> Result<Vec<u8>> {
    ensure!(
        identity.len() <= MARKER_BYTES,
        ResourceLimit(
            "Preview ownership identity exceeds the 256-byte serialized marker limit; no preview ownership marker was written"
        )
    );
    crate::filesystem_worker::wire::encode(&(identity, tier, layout), MARKER_BYTES)
        .map_err(|_| ResourceLimit("Preview ownership identity exceeds the 256-byte serialized marker limit; no preview ownership marker was written").into())
}

pub fn path(path: &NativePath) -> Result<()> {
    let units = match path {
        NativePath::UnixBytes(v) => v.len(),
        NativePath::WindowsWide(v) => v.len(),
    };
    ensure!(
        units <= super::PATH_UNITS,
        ResourceLimit(
            "Resolved preview path exceeds the managed filesystem limit of 32768 native path units; no path was truncated or changed"
        )
    );
    validate_path(path)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relocation {
    pub id: String,
    pub tier: Tier,
    pub source: NativePath,
    pub target: NativePath,
    pub cleanup: bool,
}
impl Relocation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty() && self.id.len() <= 128,
            "invalid preview relocation identity"
        );
        path(&self.source)?;
        path(&self.target)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub identity: String,
    pub layout: Layout,
    pub roots: [NativePath; 2],
    pub relocation: Option<Relocation>,
}
impl Descriptor {
    pub fn validate(&self) -> Result<()> {
        for (root, tier) in self.roots.iter().zip([Tier::Thumbnail, Tier::Large]) {
            path(root)?;
            marker(&self.identity, tier, self.layout)?;
        }
        if let Some(value) = &self.relocation {
            value.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Action {
    Acquire(Descriptor),
    Reserve {
        group: LeaseId,
        tier: Tier,
        destination: NativePath,
    },
    Lock {
        group: LeaseId,
        reservation: LeaseId,
    },
    Promote {
        group: LeaseId,
        target: LeaseId,
        tier: Tier,
    },
    Retire {
        group: LeaseId,
        old: LeaseId,
    },
    Abandon {
        group: LeaseId,
        reservation: LeaseId,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: RootCapability,
    pub operation: crate::application::U64,
    pub action: Action,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.operation.0 > 0, "preview operation sequence is zero");
        path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        match &self.action {
            Action::Acquire(value) => value.validate()?,
            Action::Reserve { destination, .. } => path(destination)?,
            _ => {}
        }
        Ok(())
    }
    pub fn is_cleanup(&self) -> bool {
        matches!(self.action, Action::Abandon { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub token: LeaseId,
    pub tier: Tier,
    pub path: NativePath,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acquired {
    pub group: LeaseId,
    pub tiers: [Lease; 2],
    pub extra: Option<Lease>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Value {
    Acquired(Acquired),
    Reserved(Lease),
    Locked(Lease),
    Unit,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub operation: crate::application::U64,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub root: RootCapability,
    pub operation: crate::application::U64,
    pub selected: Option<LeaseId>,
}
/// A control query carries no path and cannot grant filesystem authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusQuery {
    pub epoch: LeaseId,
    pub token: LeaseId,
    pub session: LeaseId,
    pub root_physical: super::PhysicalObjectId,
    pub catalog_physical: super::PhysicalObjectId,
    pub operation: crate::application::U64,
    pub selected: Option<LeaseId>,
}
impl From<&Query> for StatusQuery {
    fn from(q: &Query) -> Self {
        Self {
            epoch: q.root.epoch.clone(),
            token: q.root.token.clone(),
            session: q.root.session.clone(),
            root_physical: q.root.root_physical,
            catalog_physical: q.root.catalog_physical,
            operation: q.operation,
            selected: q.selected.clone(),
        }
    }
}
impl StatusQuery {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.operation.0 > 0,
            "preview status operation sequence is zero"
        );
        self.root_physical.validate()?;
        self.catalog_physical.validate()
    }
    pub fn matches(&self, root: &RootCapability) -> bool {
        self.epoch == root.epoch
            && self.token == root.token
            && self.session == root.session
            && self.root_physical == root.root_physical
            && self.catalog_physical == root.catalog_physical
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Reserved,
    Acquiring,
    Held,
    Failed,
    Complete,
    Abandoned,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub operation: crate::application::U64,
    pub group: Option<LeaseId>,
    pub stage: Option<Stage>,
    pub slots: usize,
    pub owned_bytes: usize,
    pub selected: Option<Lease>,
}

pub fn validate_reply(request: &Request, reply: &Reply) -> Result<()> {
    ensure!(
        request.operation == reply.operation,
        "preview ownership reply operation mismatch"
    );
    match (&request.action, &reply.value) {
        (Action::Acquire(request), Value::Acquired(value)) => {
            ensure!(
                value.tiers[0].token != value.tiers[1].token,
                "preview tiers share a lock capability"
            );
            for (actual, expected) in value.tiers.iter().zip(&request.roots) {
                ensure!(&actual.path == expected, "preview admitted root mismatch");
            }
            for (lease, tier) in value.tiers.iter().zip([Tier::Thumbnail, Tier::Large]) {
                ensure!(lease.tier == tier, "preview ownership tier mismatch");
                path(&lease.path)?;
            }
            match (&request.relocation, &value.extra) {
                (None, None) => {}
                (Some(relocation), Some(extra)) => {
                    path(&extra.path)?;
                    let expected = if relocation.cleanup {
                        &relocation.source
                    } else {
                        &relocation.target
                    };
                    ensure!(
                        &extra.path == expected
                            && extra.tier == relocation.tier
                            && value.tiers.iter().all(|tier| tier.token != extra.token),
                        "preview recovery capability mismatch"
                    );
                }
                _ => anyhow::bail!("preview recovery capability presence mismatch"),
            }
        }
        (
            Action::Reserve {
                tier, destination, ..
            },
            Value::Reserved(value),
        ) => {
            ensure!(
                &value.tier == tier && &value.path == destination,
                "preview reservation facts mismatch"
            );
        }
        (Action::Lock { reservation, .. }, Value::Locked(value)) => {
            ensure!(
                &value.token == reservation,
                "preview lock capability mismatch"
            );
            path(&value.path)?;
        }
        (Action::Promote { .. } | Action::Retire { .. } | Action::Abandon { .. }, Value::Unit) => {}
        _ => anyhow::bail!("unexpected preview ownership response"),
    }
    Ok(())
}

impl super::ManagedSession {
    pub(crate) fn store_files(
        &self,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<std::sync::Arc<dyn crate::preview::AdmittedStoreFiles>> {
        let super::AuthorityMode::Managed {
            filesystem, root, ..
        } = &self.authority.mode
        else {
            anyhow::bail!("preview files require a managed catalog session")
        };
        let manifest = self.bootstrap.manifest.path.to_path()?;
        let manifest = NativePath::from_path(
            manifest
                .parent()
                .ok_or_else(|| anyhow::anyhow!("preview manifest has no parent"))?,
        );
        Ok(crate::preview::ManagedStoreFiles::new(
            filesystem.clone(),
            root.clone(),
            manifest,
            cancel,
        ))
    }
}

impl Status {
    pub fn validate(&self, query: &StatusQuery) -> Result<()> {
        ensure!(
            self.operation == query.operation,
            "preview status operation mismatch"
        );
        ensure!(
            self.slots <= ROOT_SLOTS && self.owned_bytes <= OWNED_BYTES,
            "preview status admission mismatch"
        );
        if let Some(selected) = &self.selected {
            ensure!(
                query.selected.as_ref() == Some(&selected.token),
                "preview status selected capability mismatch"
            );
            path(&selected.path)?;
        }
        Ok(())
    }
}
