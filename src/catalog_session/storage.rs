//! Read-only storage observations execute in F while C retains SQL authority.
use super::{AuthorityMode, CatalogSessionAuthority, RootCapability, validate_path};
use crate::{
    application::U64,
    storage_volume::{self, MountSnapshot, NativePath, VolumeLocation},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Action {
    Evidence(NativePath),
    Quick {
        path: NativePath,
        expected: Evidence,
    },
    Object(NativePath),
    Locate(NativePath),
    Mounts,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub root: RootCapability,
    pub action: Action,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.root.canonical_root)?;
        self.root.root_physical.validate()?;
        self.root.catalog_physical.validate()?;
        match &self.action {
            Action::Evidence(p) | Action::Object(p) | Action::Locate(p) => validate_path(p)?,
            Action::Quick { path, expected } => {
                validate_path(path)?;
                expected.clone().into_core()?;
            }
            Action::Mounts => {}
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub hash: String,
    pub length: U64,
    pub modified_ns: String,
    pub device: U64,
    pub object: U64,
}
impl Evidence {
    pub(crate) fn into_core(self) -> Result<crate::catalog_storage::Evidence> {
        ensure!(
            self.hash.len() == 64
                && self
                    .hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "storage evidence digest"
        );
        let modified_ns: u128 = self.modified_ns.parse()?;
        ensure!(
            modified_ns.to_string() == self.modified_ns,
            "storage evidence timestamp encoding"
        );
        Ok(crate::catalog_storage::Evidence {
            hash: self.hash,
            length: self.length.0,
            modified_ns,
            object: (self.device.0, self.object.0),
        })
    }
}
impl From<crate::catalog_storage::Evidence> for Evidence {
    fn from(v: crate::catalog_storage::Evidence) -> Self {
        Self {
            hash: v.hash,
            length: U64(v.length),
            modified_ns: v.modified_ns.to_string(),
            device: U64(v.object.0),
            object: U64(v.object.1),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[expect(
    clippy::large_enum_variant,
    reason = "inline observations are included in the bounded relay response root"
)]
pub enum Value {
    Evidence(Evidence),
    Quick(bool),
    Object { device: U64, object: U64 },
    Locate(VolumeLocation),
    Mounts(MountSnapshot),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub request: Request,
    pub value: Value,
}
impl Reply {
    pub fn validate(&self, request: &Request) -> Result<()> {
        request.validate()?;
        ensure!(
            serde_json::to_vec(&self.request)? == serde_json::to_vec(request)?,
            "storage observation authority differs"
        );
        match (&request.action, &self.value) {
            (Action::Evidence(_), Value::Evidence(e)) => {
                e.clone().into_core()?;
            }
            (Action::Object(_), Value::Object { .. }) => {}
            (Action::Quick { .. }, Value::Quick(_)) => {}
            (Action::Locate(path), Value::Locate(v)) => {
                ensure!(&v.requested_path == path, "storage location path differs")
            }
            (Action::Mounts, Value::Mounts(_)) => {}
            _ => anyhow::bail!("storage observation response kind differs"),
        }
        crate::filesystem_worker::wire::encode(
            self,
            crate::filesystem_worker::wire::MESSAGE_BYTES,
        )?;
        Ok(())
    }
}

/// A snapshot carries its original owner, including across background preparation.
#[derive(Clone)]
pub(crate) struct Observer(pub(crate) Arc<CatalogSessionAuthority>);
impl std::fmt::Debug for Observer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StorageObserver")
    }
}
impl Observer {
    fn call(&self, action: Action, cancel: &AtomicBool) -> Result<Value> {
        match &self.0.mode {
            AuthorityMode::Legacy(_) => execute(&action, cancel),
            AuthorityMode::Managed {
                filesystem, root, ..
            } => {
                let request = Request {
                    root: root.clone(),
                    action,
                };
                let reply = filesystem.storage_call(&request, cancel)?;
                reply.validate(&request)?;
                Ok(reply.value)
            }
        }
    }
    pub(crate) fn evidence(
        &self,
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_storage::Evidence> {
        match self.call(Action::Evidence(NativePath::from_path(path)), cancel)? {
            Value::Evidence(v) => v.into_core(),
            _ => anyhow::bail!("storage evidence reply"),
        }
    }
    pub(crate) fn quick(
        &self,
        path: &Path,
        expected: &crate::catalog_storage::Evidence,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        match self.call(
            Action::Quick {
                path: NativePath::from_path(path),
                expected: expected.clone().into(),
            },
            cancel,
        )? {
            Value::Quick(v) => Ok(v),
            _ => anyhow::bail!("storage quick evidence reply"),
        }
    }
    pub(crate) fn object(&self, path: &Path, cancel: &AtomicBool) -> Result<(u64, u64)> {
        match self.call(Action::Object(NativePath::from_path(path)), cancel)? {
            Value::Object { device, object } => Ok((device.0, object.0)),
            _ => anyhow::bail!("storage object reply"),
        }
    }
    pub(crate) fn locate(&self, path: &Path, cancel: &AtomicBool) -> Result<VolumeLocation> {
        match self.call(Action::Locate(NativePath::from_path(path)), cancel)? {
            Value::Locate(v) => Ok(v),
            _ => anyhow::bail!("storage location reply"),
        }
    }
    pub(crate) fn mounts(&self, cancel: &AtomicBool) -> Result<MountSnapshot> {
        match self.call(Action::Mounts, cancel)? {
            Value::Mounts(v) => Ok(v),
            _ => anyhow::bail!("storage mounts reply"),
        }
    }
}
pub(crate) fn execute(action: &Action, cancel: &AtomicBool) -> Result<Value> {
    ensure!(
        !cancel.load(std::sync::atomic::Ordering::Acquire),
        "storage observation canceled"
    );
    let value = match action {
        Action::Evidence(p) => {
            Value::Evidence(crate::catalog_storage::read_evidence(&p.to_path()?, cancel)?.into())
        }
        Action::Quick { path, expected } => {
            Value::Quick(crate::catalog_storage::quick_evidence_matches(
                &path.to_path()?,
                &expected.clone().into_core()?,
            )?)
        }
        Action::Object(p) => {
            let file = crate::catalog_storage::open_regular(&p.to_path()?)?;
            let (d, o) = crate::catalog_storage::object_key(&file)?;
            Value::Object {
                device: U64(d),
                object: U64(o),
            }
        }
        Action::Locate(p) => Value::Locate(storage_volume::locate(&p.to_path()?)),
        Action::Mounts => Value::Mounts(storage_volume::mounted_volumes()?),
    };
    ensure!(
        !cancel.load(std::sync::atomic::Ordering::Acquire),
        "storage observation canceled"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_keeps_exact_numbers_and_refuses_noncanonical_timestamps() -> Result<()> {
        let core = crate::catalog_storage::Evidence {
            hash: "a".repeat(64),
            length: u64::MAX,
            modified_ns: u128::MAX,
            object: (u64::MAX, u64::MAX - 1),
        };
        let wire: Evidence = core.clone().into();
        let bytes = serde_json::to_vec(&wire)?;
        let decoded: Evidence = serde_json::from_slice(&bytes)?;
        assert_eq!(decoded.into_core()?, core);
        let mut invalid = wire;
        invalid.modified_ns = "01".into();
        assert!(invalid.into_core().is_err());
        Ok(())
    }

    #[test]
    fn changed_file_fails_quick_observation_and_canceled_read_does_not_begin() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("original.raw");
        std::fs::write(&path, b"original")?;
        let native = NativePath::from_path(&path);
        let Value::Evidence(expected) =
            execute(&Action::Evidence(native.clone()), &AtomicBool::new(false))?
        else {
            panic!("evidence")
        };
        std::fs::write(&path, b"changed and longer")?;
        assert!(matches!(
            execute(
                &Action::Quick {
                    path: native.clone(),
                    expected
                },
                &AtomicBool::new(false)
            )?,
            Value::Quick(false)
        ));
        assert!(execute(&Action::Evidence(native), &AtomicBool::new(true)).is_err());
        Ok(())
    }
}
