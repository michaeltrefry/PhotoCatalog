//! Backup-specific regular-file custody for the managed filesystem process.
//! The owner is stateful so immutable bundle files remain pinned across hashing
//! and SQL verification. It never opens SQLite.
use super::{
    APPLICATION_ID, BackupReceipt, COMPLETED, DB, FileStamp, Limits, MANIFEST, PENDING, RESTORE,
    RestoreReceipt, check_catalog_root, exclusive_root, exists, file_stamp, manifest, no_journal,
    publish, regular, root, write_document,
};
use crate::{
    CURRENT_SCHEMA_VERSION,
    application::{I64, U64},
    catalog_session::PhysicalObjectId,
    storage_volume::NativePath,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    PrepareCreate {
        operation: String,
        source: NativePath,
        bundle: NativePath,
        expected_source: PhysicalObjectId,
        limits: Limits,
    },
    CheckCreate {
        operation: String,
        database_bytes: U64,
    },
    HashCreate {
        operation: String,
    },
    FinishCreate {
        operation: String,
        schema_version: I64,
    },
    PrepareInspect {
        operation: String,
        bundle: NativePath,
        limits: Limits,
    },
    HashInspect {
        operation: String,
    },
    FinishInspect {
        operation: String,
    },
    PrepareRestore {
        operation: String,
        destination: NativePath,
    },
    CopyRestore {
        operation: String,
    },
    FinishRestore {
        operation: String,
        schema_version: I64,
    },
    Abort {
        operation: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Reply {
    CreatePrepared {
        source: NativePath,
        source_physical: PhysicalObjectId,
        target: NativePath,
        target_physical: PhysicalObjectId,
    },
    CreateChecked,
    InspectPrepared {
        database: NativePath,
        physical: PhysicalObjectId,
        receipt: BackupReceipt,
    },
    HashProgress {
        bytes: U64,
        done: bool,
    },
    RestorePrepared {
        target: NativePath,
        target_physical: PhysicalObjectId,
        receipt: BackupReceipt,
    },
    CopyProgress {
        bytes: U64,
        done: bool,
    },
    Backup(BackupReceipt),
    Restore(RestoreReceipt),
    Aborted,
}

impl Request {
    pub fn validate(&self) -> Result<()> {
        let (operation, paths): (&str, Vec<&NativePath>) = match self {
            Self::PrepareCreate {
                operation,
                source,
                bundle,
                limits,
                expected_source,
            } => {
                limits.validate()?;
                expected_source.validate()?;
                (operation, vec![source, bundle])
            }
            Self::PrepareInspect {
                operation,
                bundle,
                limits,
            } => {
                limits.validate()?;
                (operation, vec![bundle])
            }
            Self::PrepareRestore {
                operation,
                destination,
            } => (operation, vec![destination]),
            Self::CheckCreate { operation, .. }
            | Self::HashCreate { operation }
            | Self::FinishCreate { operation, .. }
            | Self::HashInspect { operation }
            | Self::FinishInspect { operation }
            | Self::CopyRestore { operation }
            | Self::FinishRestore { operation, .. }
            | Self::Abort { operation } => (operation, vec![]),
        };
        ensure!(
            uuid::Uuid::parse_str(operation)?.to_string() == operation,
            "invalid backup operation identity"
        );
        for path in paths {
            crate::catalog_session::validate_path(path)?;
            ensure!(
                path.to_path()?.is_absolute(),
                "managed backup path must be absolute"
            );
        }
        Ok(())
    }

    pub fn cleanup(&self) -> bool {
        matches!(self, Self::Abort { .. })
    }
}

struct Held {
    path: PathBuf,
    file: File,
    physical: PhysicalObjectId,
}
impl Held {
    fn open(path: PathBuf) -> Result<Self> {
        let file = crate::metadata_export::open_regular(&path)?;
        let physical = crate::catalog_storage::physical_object_id(&file)?;
        Ok(Self {
            path,
            file,
            physical,
        })
    }
    fn created(path: PathBuf) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let physical = crate::catalog_storage::physical_object_id(&file)?;
        Ok(Self {
            path,
            file,
            physical,
        })
    }
    fn recheck(&self) -> Result<()> {
        let current = crate::metadata_export::open_regular(&self.path)?;
        ensure!(
            crate::catalog_storage::physical_object_id(&current)? == self.physical,
            "backup filesystem object changed while held"
        );
        Ok(())
    }
}

struct Hashing {
    hasher: blake3::Hasher,
    bytes: u64,
    expected: u64,
}
impl Hashing {
    fn new(expected: u64) -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            bytes: 0,
            expected,
        }
    }
    fn step(&mut self, file: &mut File, cancel: &AtomicBool) -> Result<(u64, bool)> {
        check_cancel(cancel)?;
        let mut buffer = vec![0; 1024 * 1024];
        let n = file.read(&mut buffer)?;
        if n == 0 {
            ensure!(
                self.bytes == self.expected,
                "backup database length changed"
            );
            return Ok((self.bytes, true));
        }
        self.bytes = self
            .bytes
            .checked_add(n as u64)
            .context("backup byte count overflow")?;
        ensure!(
            self.bytes <= self.expected,
            "backup database grew while hashing"
        );
        self.hasher.update(&buffer[..n]);
        check_cancel(cancel)?;
        Ok((self.bytes, false))
    }
    fn digest(&self) -> String {
        self.hasher.clone().finalize().to_hex().to_string()
    }
}

enum Active {
    Create {
        operation: String,
        source_root: PathBuf,
        source: Held,
        target_root: PathBuf,
        target: Held,
        limits: Limits,
        hashing: Option<Hashing>,
        hash_stamp: Option<FileStamp>,
    },
    Inspect {
        operation: String,
        bundle: PathBuf,
        database: Held,
        stamp: FileStamp,
        receipt: BackupReceipt,
        limits: Limits,
        hashing: Hashing,
        hash_done: bool,
    },
    Restore {
        operation: String,
        source: Held,
        stamp: FileStamp,
        receipt: BackupReceipt,
        target_root: PathBuf,
        target: Held,
        limits: Limits,
        copied: u64,
        digest: blake3::Hasher,
        copy_done: bool,
    },
}

#[derive(Default)]
pub struct Owner {
    active: Option<Active>,
}

impl Owner {
    pub fn call(&mut self, request: &Request, cancel: &AtomicBool) -> Result<Reply> {
        request.validate()?;
        if let Request::Abort { operation } = request {
            ensure!(
                self.operation() == Some(operation.as_str()),
                "backup abort identity mismatch"
            );
            self.active.take();
            return Ok(Reply::Aborted);
        }
        check_cancel(cancel)?;
        match request {
            Request::PrepareCreate {
                operation,
                source,
                bundle,
                expected_source,
                limits,
            } => {
                ensure!(self.active.is_none(), "filesystem backup owner is busy");
                let source_root = root(&source.to_path()?)?;
                check_catalog_root(&source_root)?;
                let source = Held::open(source_root.join(DB))?;
                ensure!(
                    &source.physical == expected_source,
                    "backup source differs from active catalog session"
                );
                let target_root = exclusive_root(&bundle.to_path()?, &source_root)?;
                let target = Held::created(target_root.join(DB))?;
                ensure!(
                    source.physical != target.physical,
                    "backup source and destination are the same object"
                );
                let reply = Reply::CreatePrepared {
                    source: NativePath::from_path(&source.path),
                    source_physical: source.physical,
                    target: NativePath::from_path(&target.path),
                    target_physical: target.physical,
                };
                self.active = Some(Active::Create {
                    operation: operation.clone(),
                    source_root,
                    source,
                    target_root,
                    target,
                    limits: limits.clone(),
                    hashing: None,
                    hash_stamp: None,
                });
                Ok(reply)
            }
            Request::CheckCreate {
                operation,
                database_bytes,
            } => {
                let Active::Create {
                    operation: id,
                    source_root,
                    source,
                    target_root,
                    limits,
                    ..
                } = self.active.as_mut().context("missing create owner")?
                else {
                    bail!("wrong backup filesystem phase")
                };
                ensure!(id == operation, "backup operation identity mismatch");
                ensure!(
                    database_bytes.0 <= limits.max_database_bytes,
                    "database exceeds backup byte limit"
                );
                source.recheck()?;
                disk(limits, target_root, database_bytes.0)?;
                let wal = source_root.join("catalog.sqlite3-wal");
                if exists(&wal)? {
                    ensure!(
                        regular(&wal)?.len() <= limits.max_source_wal_bytes,
                        "source WAL pressure exceeds limit; release snapshot and retry with sufficient budget"
                    );
                }
                Ok(Reply::CreateChecked)
            }
            Request::HashCreate { operation } => {
                let Active::Create {
                    operation: id,
                    target,
                    limits,
                    hashing,
                    hash_stamp,
                    ..
                } = self.active.as_mut().context("missing create owner")?
                else {
                    bail!("wrong backup filesystem phase")
                };
                ensure!(id == operation, "backup operation identity mismatch");
                target.recheck()?;
                if hashing.is_none() {
                    let expected = target.file.metadata()?.len();
                    ensure!(
                        expected <= limits.max_database_bytes,
                        "database exceeds byte limit"
                    );
                    target.file.rewind()?;
                    *hash_stamp = Some(file_stamp(&target.file)?);
                    *hashing = Some(Hashing::new(expected));
                }
                let (bytes, done) = hashing.as_mut().unwrap().step(&mut target.file, cancel)?;
                Ok(Reply::HashProgress {
                    bytes: U64(bytes),
                    done,
                })
            }
            Request::FinishCreate {
                operation,
                schema_version,
            } => {
                let Active::Create {
                    operation: id,
                    source,
                    target_root,
                    target,
                    hashing,
                    hash_stamp,
                    ..
                } = self.active.as_mut().context("missing create owner")?
                else {
                    bail!("wrong backup filesystem phase")
                };
                ensure!(id == operation, "backup operation identity mismatch");
                let hashing = hashing.as_ref().context("backup hash was not completed")?;
                ensure!(
                    hashing.bytes == hashing.expected,
                    "backup hash was not completed"
                );
                let hash_stamp = hash_stamp
                    .as_ref()
                    .context("backup hash revision was not recorded")?;
                ensure!(
                    file_stamp(&target.file)? == *hash_stamp,
                    "backup database changed after hashing"
                );
                source.recheck()?;
                target.recheck()?;
                no_journal(&target.path)?;
                crate::metadata_export::sync_file(&target.file)?;
                let receipt = BackupReceipt {
                    protocol: 1,
                    backup_id: uuid::Uuid::new_v4().to_string(),
                    application_id: APPLICATION_ID,
                    schema_version: schema_version.0,
                    database_bytes: hashing.bytes,
                    database_blake3: hashing.digest(),
                };
                write_document(&target_root.join(MANIFEST), &receipt)?;
                publish(target_root)?;
                let result = receipt.clone();
                self.active.take();
                Ok(Reply::Backup(result))
            }
            Request::PrepareInspect {
                operation,
                bundle,
                limits,
            } => {
                ensure!(self.active.is_none(), "filesystem backup owner is busy");
                let bundle = root(&bundle.to_path()?)?;
                let receipt = manifest(&bundle)?;
                ensure!(
                    receipt.database_bytes <= limits.max_database_bytes,
                    "database exceeds byte limit"
                );
                let mut database = Held::open(bundle.join(DB))?;
                let stamp = file_stamp(&database.file)?;
                ensure!(
                    stamp.bytes == receipt.database_bytes,
                    "backup size differs from manifest"
                );
                database.file.rewind()?;
                let reply = Reply::InspectPrepared {
                    database: NativePath::from_path(&database.path),
                    physical: database.physical,
                    receipt: receipt.clone(),
                };
                self.active = Some(Active::Inspect {
                    operation: operation.clone(),
                    bundle,
                    database,
                    stamp,
                    receipt: receipt.clone(),
                    limits: limits.clone(),
                    hashing: Hashing::new(receipt.database_bytes),
                    hash_done: false,
                });
                Ok(reply)
            }
            Request::HashInspect { operation } => {
                let Active::Inspect {
                    operation: id,
                    database,
                    receipt,
                    hashing,
                    hash_done,
                    ..
                } = self.active.as_mut().context("missing inspect owner")?
                else {
                    bail!("wrong backup filesystem phase")
                };
                ensure!(id == operation, "backup operation identity mismatch");
                let (bytes, done) = hashing.step(&mut database.file, cancel)?;
                if done {
                    ensure!(
                        hashing.digest() == receipt.database_blake3,
                        "backup size/digest mismatch; source was not restored"
                    );
                    *hash_done = true;
                }
                Ok(Reply::HashProgress {
                    bytes: U64(bytes),
                    done,
                })
            }
            Request::FinishInspect { operation } => {
                let Active::Inspect {
                    operation: id,
                    database,
                    stamp,
                    receipt,
                    hash_done,
                    ..
                } = self.active.as_ref().context("missing inspect owner")?
                else {
                    bail!("wrong backup filesystem phase")
                };
                ensure!(
                    id == operation && *hash_done,
                    "backup inspect was not completed"
                );
                ensure!(
                    file_stamp(&database.file)? == *stamp,
                    "database identity or revision changed during backup verification"
                );
                database.recheck()?;
                no_journal(&database.path)?;
                let result = receipt.clone();
                self.active.take();
                Ok(Reply::Backup(result))
            }
            Request::PrepareRestore {
                operation,
                destination,
            } => {
                let active = self
                    .active
                    .take()
                    .context("missing inspected backup owner")?;
                let Active::Inspect {
                    operation: id,
                    bundle,
                    mut database,
                    stamp,
                    receipt,
                    limits,
                    hash_done,
                    ..
                } = active
                else {
                    self.active = Some(active);
                    bail!("wrong backup filesystem phase")
                };
                ensure!(
                    &id == operation && hash_done,
                    "backup restore inspect incomplete"
                );
                ensure!(
                    file_stamp(&database.file)? == stamp,
                    "database identity or revision changed during backup verification"
                );
                database.recheck()?;
                let target_root = exclusive_root(&destination.to_path()?, &bundle)?;
                disk(&limits, &target_root, receipt.database_bytes)?;
                fs::create_dir(target_root.join("previews"))?;
                let target = Held::created(target_root.join(DB))?;
                ensure!(
                    database.physical != target.physical,
                    "restore source and destination are the same object"
                );
                database.file.rewind()?;
                let reply = Reply::RestorePrepared {
                    target: NativePath::from_path(&target.path),
                    target_physical: target.physical,
                    receipt: receipt.clone(),
                };
                self.active = Some(Active::Restore {
                    operation: id,
                    source: database,
                    stamp,
                    receipt,
                    target_root,
                    target,
                    limits,
                    copied: 0,
                    digest: blake3::Hasher::new(),
                    copy_done: false,
                });
                Ok(reply)
            }
            Request::CopyRestore { operation } => {
                let Active::Restore {
                    operation: id,
                    source,
                    receipt,
                    target_root,
                    target,
                    limits,
                    copied,
                    digest,
                    copy_done,
                    ..
                } = self.active.as_mut().context("missing restore owner")?
                else {
                    bail!("wrong backup filesystem phase")
                };
                ensure!(
                    id == operation && !*copy_done,
                    "restore copy identity or phase mismatch"
                );
                check_cancel(cancel)?;
                disk(limits, target_root, 0)?;
                let mut buffer = vec![0; 1024 * 1024];
                let n = source.file.read(&mut buffer)?;
                if n == 0 {
                    ensure!(
                        *copied == receipt.database_bytes
                            && digest.clone().finalize().to_hex().as_str()
                                == receipt.database_blake3,
                        "backup changed during restore"
                    );
                    crate::metadata_export::sync_file(&target.file)?;
                    *copy_done = true;
                } else {
                    *copied = copied
                        .checked_add(n as u64)
                        .context("restore byte count overflow")?;
                    ensure!(
                        *copied <= receipt.database_bytes,
                        "backup grew during restore"
                    );
                    target.file.write_all(&buffer[..n])?;
                    digest.update(&buffer[..n]);
                }
                check_cancel(cancel)?;
                Ok(Reply::CopyProgress {
                    bytes: U64(*copied),
                    done: *copy_done,
                })
            }
            Request::FinishRestore {
                operation,
                schema_version,
            } => {
                let Active::Restore {
                    operation: id,
                    source,
                    stamp,
                    receipt,
                    target_root,
                    target,
                    copy_done,
                    ..
                } = self.active.as_ref().context("missing restore owner")?
                else {
                    bail!("wrong backup filesystem phase")
                };
                ensure!(id == operation && *copy_done, "restore was not completed");
                ensure!(
                    schema_version.0 == CURRENT_SCHEMA_VERSION,
                    "restored schema did not reach current version"
                );
                ensure!(
                    file_stamp(&source.file)? == *stamp,
                    "backup changed during restore"
                );
                source.recheck()?;
                target.recheck()?;
                no_journal(&target.path)?;
                crate::metadata_export::sync_file(&target.file)?;
                let restored = RestoreReceipt {
                    protocol: 1,
                    restore_id: uuid::Uuid::new_v4().to_string(),
                    backup: receipt.clone(),
                    schema_version: schema_version.0,
                };
                write_document(&target_root.join(RESTORE), &restored)?;
                publish(target_root)?;
                let result = restored.clone();
                self.active.take();
                Ok(Reply::Restore(result))
            }
        }
    }

    fn operation(&self) -> Option<&str> {
        match self.active.as_ref()? {
            Active::Create { operation, .. }
            | Active::Inspect { operation, .. }
            | Active::Restore { operation, .. } => Some(operation),
        }
    }
    pub fn is_idle(&self) -> bool {
        self.active.is_none()
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "filesystem backup operation canceled"
    );
    Ok(())
}
fn disk(limits: &Limits, path: &Path, incoming: u64) -> Result<()> {
    ensure!(
        fs2::available_space(path)?
            >= limits
                .min_free_bytes
                .checked_add(incoming)
                .context("free-space limit overflow")?,
        "insufficient free space for backup/restore; preserve or remove the marked incomplete destination before retrying elsewhere"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Catalog;

    fn limits() -> Limits {
        Limits {
            min_free_bytes: 0,
            pages_per_step: 1,
            ..Limits::default()
        }
    }
    fn call(owner: &mut Owner, request: Request) -> Result<Reply> {
        owner.call(&request, &AtomicBool::new(false))
    }

    #[test]
    fn create_admission_uses_actual_source_identity_and_retains_new_pending_root() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        let catalog = Catalog::open(&source)?;
        let expected = crate::catalog_storage::opened_database_identity(&catalog.db)?;
        drop(catalog);
        let operation = uuid::Uuid::new_v4().to_string();
        let destination = temp.path().join("bundle");
        let mut owner = Owner::default();
        let Reply::CreatePrepared {
            source_physical,
            target_physical,
            ..
        } = call(
            &mut owner,
            Request::PrepareCreate {
                operation: operation.clone(),
                source: NativePath::from_path(&source),
                bundle: NativePath::from_path(&destination),
                expected_source: expected,
                limits: limits(),
            },
        )?
        else {
            bail!("unexpected create preparation")
        };
        assert_eq!(source_physical, expected);
        assert_ne!(source_physical, target_physical);
        assert!(destination.join(PENDING).is_file());
        assert!(
            call(
                &mut owner,
                Request::Abort {
                    operation: operation.clone()
                }
            )
            .is_ok()
        );
        assert!(destination.join(PENDING).is_file());
        assert!(
            call(
                &mut owner,
                Request::PrepareCreate {
                    operation: uuid::Uuid::new_v4().to_string(),
                    source: NativePath::from_path(&source),
                    bundle: NativePath::from_path(&destination),
                    expected_source: expected,
                    limits: limits(),
                }
            )
            .is_err()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn inspect_cancel_and_post_hash_swap_keep_the_bundle_untrusted() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        drop(Catalog::open(&source)?);
        let bundle = temp.path().join("bundle");
        super::super::backup_catalog(&source, &bundle, &limits(), |_| Ok(()))?;

        let operation = uuid::Uuid::new_v4().to_string();
        let mut owner = Owner::default();
        call(
            &mut owner,
            Request::PrepareInspect {
                operation: operation.clone(),
                bundle: NativePath::from_path(&bundle),
                limits: limits(),
            },
        )?;
        let canceled = AtomicBool::new(true);
        assert!(
            owner
                .call(
                    &Request::HashInspect {
                        operation: operation.clone()
                    },
                    &canceled
                )
                .is_err()
        );
        call(
            &mut owner,
            Request::Abort {
                operation: operation.clone(),
            },
        )?;

        let operation = uuid::Uuid::new_v4().to_string();
        call(
            &mut owner,
            Request::PrepareInspect {
                operation: operation.clone(),
                bundle: NativePath::from_path(&bundle),
                limits: limits(),
            },
        )?;
        loop {
            let Reply::HashProgress { done, .. } = call(
                &mut owner,
                Request::HashInspect {
                    operation: operation.clone(),
                },
            )?
            else {
                bail!("unexpected hash step")
            };
            if done {
                break;
            }
        }
        let database = bundle.join(DB);
        let replacement = bundle.join("replacement.sqlite3");
        fs::copy(&database, &replacement)?;
        fs::rename(&replacement, &database)?;
        assert!(
            call(
                &mut owner,
                Request::FinishInspect {
                    operation: operation.clone()
                }
            )
            .is_err()
        );
        call(&mut owner, Request::Abort { operation })?;
        Ok(())
    }

    #[test]
    fn restore_copy_is_exact_new_only_and_preserves_job_hold_markers() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        drop(Catalog::open(&source)?);
        let bundle = temp.path().join("bundle");
        let receipt = super::super::backup_catalog(&source, &bundle, &limits(), |_| Ok(()))?;
        let operation = uuid::Uuid::new_v4().to_string();
        let mut owner = Owner::default();
        call(
            &mut owner,
            Request::PrepareInspect {
                operation: operation.clone(),
                bundle: NativePath::from_path(&bundle),
                limits: limits(),
            },
        )?;
        loop {
            let Reply::HashProgress { done, .. } = call(
                &mut owner,
                Request::HashInspect {
                    operation: operation.clone(),
                },
            )?
            else {
                bail!("unexpected hash step")
            };
            if done {
                break;
            }
        }
        let destination = temp.path().join("restored");
        call(
            &mut owner,
            Request::PrepareRestore {
                operation: operation.clone(),
                destination: NativePath::from_path(&destination),
            },
        )?;
        loop {
            let Reply::CopyProgress { done, .. } = call(
                &mut owner,
                Request::CopyRestore {
                    operation: operation.clone(),
                },
            )?
            else {
                bail!("unexpected copy step")
            };
            if done {
                break;
            }
        }
        assert_eq!(fs::read(bundle.join(DB))?, fs::read(destination.join(DB))?);
        let Reply::Restore(restored) = call(
            &mut owner,
            Request::FinishRestore {
                operation,
                schema_version: I64(CURRENT_SCHEMA_VERSION),
            },
        )?
        else {
            bail!("unexpected restore finish")
        };
        assert_eq!(restored.backup, receipt);
        assert!(
            super::super::restore_status(&destination)?
                .unwrap()
                .jobs_held
        );
        assert!(destination.join(COMPLETED).is_file());
        assert!(destination.join(RESTORE).is_file());
        assert!(destination.join("previews").is_dir());
        assert!(
            call(
                &mut owner,
                Request::PrepareRestore {
                    operation: uuid::Uuid::new_v4().to_string(),
                    destination: NativePath::from_path(&destination),
                }
            )
            .is_err()
        );
        Ok(())
    }
}
