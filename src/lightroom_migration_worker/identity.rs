//! Helper-local file-role admission. No descriptor from this module enters the
//! GUI. An alias discovered after open poisons the only executor; closing the
//! rejected descriptor may already have disturbed this process's Source locks.
use crate::{application::U64, catalog_storage, storage_volume::NativePath};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileKey {
    pub volume: U64,
    pub index: U64,
}
impl FileKey {
    pub fn of(file: &File) -> Result<Self> {
        let (volume, index) = catalog_storage::object_key(file)?;
        Ok(Self {
            volume: U64(volume),
            index: U64(index),
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Source,
    Destination,
    ImportLock,
}
struct State {
    held: BTreeMap<FileKey, Role>,
    writing: bool,
}
struct Shared {
    state: Mutex<State>,
    cancel: Arc<AtomicBool>,
    poisoned: AtomicBool,
    protected: Vec<FileKey>,
}
#[derive(Clone)]
pub(crate) struct Audit(Arc<Shared>);
thread_local! { static ACTIVE: RefCell<Option<Audit>> = const { RefCell::new(None) }; }
/// Install only on the sole helper executor thread, outside all its readers.
/// A control listener may signal cancel but cannot open Source or execute SQL.
pub(crate) struct Scope;
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|v| {
            v.borrow_mut().take();
        });
    }
}
pub(crate) struct RoleLease {
    audit: Audit,
    key: FileKey,
}
impl Drop for RoleLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.audit.0.state.lock() {
            state.held.remove(&self.key);
        } else {
            self.audit.poison();
        }
    }
}
impl Audit {
    pub(crate) fn new(cancel: Arc<AtomicBool>, protected: Vec<FileKey>) -> Result<Self> {
        ensure!(
            protected.len() <= 4096,
            "protected GUI object roster exceeds admission"
        );
        Ok(Self(Arc::new(Shared {
            state: Mutex::new(State {
                held: BTreeMap::new(),
                writing: false,
            }),
            cancel,
            poisoned: AtomicBool::new(false),
            protected,
        })))
    }
    pub(crate) fn install(&self) -> Result<Scope> {
        ACTIVE.with(|v| {
            let mut slot = v.borrow_mut();
            ensure!(slot.is_none(), "helper source audit already installed");
            *slot = Some(self.clone());
            Ok(Scope)
        })
    }
    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            !self.0.poisoned.load(Ordering::Acquire),
            "migration helper file-role authority poisoned; explicit fresh operation required"
        );
        ensure!(
            !self.0.cancel.load(Ordering::Acquire),
            "migration helper canceled"
        );
        Ok(())
    }
    pub(crate) fn cancel(&self) {
        self.0.cancel.store(true, Ordering::Release);
    }
    pub(crate) fn poison(&self) {
        self.0.poisoned.store(true, Ordering::Release);
        self.0.cancel.store(true, Ordering::Release);
    }
    pub(crate) fn is_poisoned(&self) -> bool {
        self.0.poisoned.load(Ordering::Acquire)
    }
    pub(crate) fn writing(&self, value: bool) -> Result<()> {
        if value {
            self.check()?;
        }
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("helper role state poisoned"))?;
        ensure!(!value || !state.writing, "nested helper writer lease");
        state.writing = value;
        Ok(())
    }
    fn admit(&self, file: &File, role: Role) -> Result<RoleLease> {
        self.check()?;
        let checked = (|| {
            ensure!(
                file.metadata()?.is_file(),
                "helper role requires a regular file"
            );
            let key = FileKey::of(file)?;
            let mut state = self
                .0
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("helper role state poisoned"))?;
            ensure!(
                !state.held.contains_key(&key),
                "helper file aliases an already-held source/destination/lock role"
            );
            ensure!(
                role == Role::Source || !self.0.protected.contains(&key),
                "destination/lock/transport aliases a protected GUI inspection object"
            );
            ensure!(
                state.held.len() < 8192,
                "helper file-role roster exceeds admission"
            );
            state.held.insert(key.clone(), role);
            Ok(RoleLease {
                audit: self.clone(),
                key,
            })
        })();
        if checked.is_err() {
            self.poison();
        }
        checked
    }
    pub(crate) fn open(&self, path: &Path, role: Role, create_new: bool) -> Result<OwnedFile> {
        self.check()?;
        crate::lightroom::source::reject_links(path.parent().context("file has no parent")?)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(create_new || matches!(role, Role::ImportLock))
            .create_new(create_new);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000).share_mode(1 | 2);
        }
        let file = options.open(path)?;
        let lease = self.admit(&file, role)?;
        Ok(OwnedFile {
            file,
            lease,
            path: NativePath::from_path(path),
        })
    }
    /// Create the persistent first-use import-lock inode only while the parent
    /// writer grant is held. An existing-file race reopens that exact object;
    /// neither path truncates it.
    pub(crate) fn create_import_lock(&self, path: &Path) -> Result<OwnedFile> {
        self.check()?;
        ensure!(
            self.0
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("helper role state poisoned"))?
                .writing,
            "import lock creation requires a parent writer grant"
        );
        crate::lightroom::source::reject_links(path.parent().context("file has no parent")?)?;
        let configure = |create_new| {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(create_new);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.custom_flags(0x0020_0000).share_mode(1 | 2);
            }
            options
        };
        let file = match configure(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                configure(false).open(path)?
            }
            Err(error) => return Err(error.into()),
        };
        let lease = self.admit(&file, Role::ImportLock)?;
        Ok(OwnedFile {
            file,
            lease,
            path: NativePath::from_path(path),
        })
    }
}
/// File closes before its role is retired. Outer DestinationLease owns the
/// import lock until every inner reader, connection and transaction has closed.
pub(crate) struct OwnedFile {
    pub file: File,
    lease: RoleLease,
    pub path: NativePath,
}
impl OwnedFile {
    pub(crate) fn key(&self) -> &FileKey {
        &self.lease.key
    }
    pub(crate) fn verify(&self) -> Result<()> {
        self.lease.audit.check()?;
        let checked = (|| {
            ensure!(
                FileKey::of(&self.file)? == *self.key(),
                "held helper object changed"
            );
            let path = self.path.to_path()?;
            crate::lightroom::source::reject_links(&path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let meta = std::fs::symlink_metadata(&path)?;
                ensure!(
                    meta.is_file()
                        && meta.dev() == self.key().volume.0
                        && meta.ino() == self.key().index.0,
                    "helper path names a different held object"
                );
            }
            #[cfg(windows)]
            {
                // Our retained file denies delete sharing; no second pathname
                // handle is opened for identity verification.
                ensure!(
                    std::fs::symlink_metadata(&path)?.is_file(),
                    "helper path became non-regular"
                );
            }
            Ok(())
        })();
        if checked.is_err() {
            self.lease.audit.poison();
        }
        checked
    }
}
/// Source hooks are inert outside the explicitly installed helper scope.
pub(crate) fn before_source_open() -> Result<()> {
    ACTIVE.with(|v| {
        if let Some(audit) = v.borrow().as_ref() {
            audit.check()?;
            let writing = audit
                .0
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("helper role state poisoned"))?
                .writing;
            if writing {
                audit.poison();
                anyhow::bail!(
                    "source admission attempted inside a granted writer critical section"
                );
            }
        }
        Ok(())
    })
}
pub(crate) fn source_open(file: &File) -> Result<Option<RoleLease>> {
    ACTIVE.with(|v| {
        v.borrow()
            .as_ref()
            .map(|a| a.admit(file, Role::Source))
            .transpose()
    })
}

pub(crate) fn check_source() -> Result<()> {
    ACTIVE.with(|v| v.borrow().as_ref().map_or(Ok(()), Audit::check))
}
#[cfg(test)]
mod tests;
