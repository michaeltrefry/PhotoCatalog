//! Same-process import lock and destination pin. Only the configured migration
//! helper constructs these owners; GUI receives their bounded identities only.
use super::identity::{Audit, FileKey, OwnedFile, Role};
use crate::{
    Catalog,
    application::I64,
    catalog_storage,
    catalog_writer::{Priority, Writers},
    storage_volume::NativePath,
};
use anyhow::{Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationPin {
    pub root: NativePath,
    pub root_key: FileKey,
    pub database_key: FileKey,
    pub schema: I64,
}
struct RootPin {
    file: File,
    path: PathBuf,
    key: FileKey,
}
impl RootPin {
    fn open(path: &Path) -> Result<Self> {
        crate::lightroom::source::reject_links(path)?;
        let path = std::fs::canonicalize(path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options
                .custom_flags(0x0200_0000 | 0x0020_0000)
                .share_mode(1 | 2);
        }
        let file = options.open(&path)?;
        ensure!(
            file.metadata()?.is_dir(),
            "migration root must be a directory"
        );
        let key = FileKey::of(&file)?;
        let value = Self { file, path, key };
        value.verify()?;
        Ok(value)
    }
    fn verify(&self) -> Result<()> {
        ensure!(
            FileKey::of(&self.file)? == self.key,
            "held migration root changed"
        );
        crate::lightroom::source::reject_links(&self.path)?;
        let meta = std::fs::symlink_metadata(&self.path)?;
        ensure!(meta.is_dir(), "migration root became non-directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                meta.dev() == self.key.volume.0 && meta.ino() == self.key.index.0,
                "migration root path object changed"
            );
        }
        Ok(())
    }
}
/// Read admission never creates/opens a lock for writing or configures SQLite.
/// The SQLite object is checked before the first schema query and before close.
pub(crate) struct DestinationReview {
    database: OwnedFile,
    root: RootPin,
    pub pin: DestinationPin,
    audit: Audit,
}
impl DestinationReview {
    pub(crate) fn existing(
        path: &NativePath,
        expected: Option<&DestinationPin>,
        audit: &Audit,
    ) -> Result<Self> {
        let checked = (|| {
            audit.check()?;
            let root = RootPin::open(&super::authority::local_destination(path)?)?;
            let database =
                audit.open(&root.path.join("catalog.sqlite3"), Role::Destination, false)?;
            let db = Connection::open_with_flags(
                root.path.join("catalog.sqlite3"),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            catalog_storage::verify_database_object(&db, &database.file)?;
            let app: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
            let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            ensure!(
                app == 0x50484341 && (1..=crate::CURRENT_SCHEMA_VERSION).contains(&schema),
                "existing migration destination is not a supported PhotoCatalog database"
            );
            database.verify()?;
            root.verify()?;
            catalog_storage::verify_database_object(&db, &database.file)?;
            let pin = DestinationPin {
                root: NativePath::from_path(&root.path),
                root_key: root.key.clone(),
                database_key: database.key().clone(),
                schema: I64(schema),
            };
            if let Some(expected) = expected {
                ensure!(
                    &pin == expected,
                    "migration destination differs from reviewed/attached object"
                );
            }
            Ok(Self {
                database,
                root,
                pin,
                audit: audit.clone(),
            })
        })();
        if checked.is_err() {
            audit.poison();
        }
        checked
    }
    pub(crate) fn verify(&self) -> Result<()> {
        let result = (|| {
            self.audit.check()?;
            self.root.verify()?;
            self.database.verify()
        })();
        if result.is_err() {
            self.audit.poison();
        }
        result
    }
    pub(crate) fn read(&self) -> Result<Connection> {
        let checked = (|| {
            self.verify()?;
            let db = Connection::open_with_flags(
                self.root.path.join("catalog.sqlite3"),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            catalog_storage::verify_database_object(&db, &self.database.file)?;
            self.verify()?;
            Ok(db)
        })();
        if checked.is_err() {
            self.audit.poison();
        }
        checked
    }
}
/// Keep this in an outer scope. Catalog/transaction/readers belong to a nested
/// executor scope, so no cleanup path releases the lock while SQL can continue.
pub(crate) struct DestinationLease {
    review: DestinationReview,
    lock: OwnedFile,
}
impl DestinationLease {
    pub(crate) fn acquire(
        review: DestinationReview,
        expected_lock: Option<&FileKey>,
        deadline: Instant,
    ) -> Result<Self> {
        review.verify()?;
        let lock = review.audit.open(
            &review.root.path.join(".lightroom-import.lock"),
            Role::ImportLock,
            false,
        )?;
        if let Some(expected) = expected_lock {
            if lock.key() != expected {
                review.audit.poison();
                anyhow::bail!("import lock differs from reviewed object");
            }
        }
        loop {
            review.audit.check()?;
            ensure!(
                Instant::now() < deadline,
                "migration import-lock acquisition deadline"
            );
            match lock.file.try_lock_exclusive() {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) => return Err(error.into()),
            }
        }
        let lease = Self { review, lock };
        lease.verify()?;
        Ok(lease)
    }
    pub(crate) fn pin(&self) -> &DestinationPin {
        &self.review.pin
    }
    pub(crate) fn lock_key(&self) -> &FileKey {
        self.lock.key()
    }
    pub(crate) fn verify(&self) -> Result<()> {
        self.review.verify()?;
        self.lock.verify()
    }
    /// The external Writers adapter obtains an exact parent grant before even
    /// writable SQLite configuration. Existing schema is never upgraded here.
    pub(crate) fn open_current(&self, writers: Arc<Writers>) -> Result<LockedCatalog<'_>> {
        self.verify()?;
        ensure!(
            self.pin().schema.0 == crate::CURRENT_SCHEMA_VERSION,
            "explicit qualified upgrade required before migration execution"
        );
        let _permit = writers.enter(Priority::Background)?;
        let result = (|| {
            self.verify()?;
            let db = Connection::open_with_flags(
                self.review.root.path.join("catalog.sqlite3"),
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            catalog_storage::verify_database_object(&db, &self.review.database.file)?;
            self.verify()?;
            let app: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
            let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            ensure!(
                app == 0x50484341 && schema == self.pin().schema.0,
                "destination schema changed after reviewed admission"
            );
            crate::configure_catalog_connection(&db)?;
            catalog_storage::verify_database_object(&db, &self.review.database.file)?;
            self.verify()?;
            Ok(LockedCatalog {
                catalog: Catalog {
                    db,
                    root: self.review.root.path.clone(),
                    writers: writers.clone(),
                    relink_file: Arc::new(self.review.database.file.try_clone()?),
                },
                lease: self,
            })
        })();
        if result.is_err() {
            self.review.audit.poison();
        }
        result
    }
}

/// Borrowing the outer lease prevents callers from dropping its lock while this
/// executor connection can still issue SQL. No API extracts the owned Catalog.
pub(crate) struct LockedCatalog<'a> {
    catalog: Catalog,
    lease: &'a DestinationLease,
}
impl std::ops::Deref for LockedCatalog<'_> {
    type Target = Catalog;
    fn deref(&self) -> &Catalog {
        &self.catalog
    }
}
impl std::ops::DerefMut for LockedCatalog<'_> {
    fn deref_mut(&mut self) -> &mut Catalog {
        &mut self.catalog
    }
}
impl LockedCatalog<'_> {
    pub(crate) fn verify(&self) -> Result<()> {
        self.lease.verify()?;
        catalog_storage::verify_database_object(&self.catalog.db, &self.lease.review.database.file)
    }
}
#[cfg(test)]
mod tests;
