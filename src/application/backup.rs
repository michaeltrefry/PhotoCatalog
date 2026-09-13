//! One owned background backup operation. Core backup/restore owns filesystem,
//! snapshot, integrity and publication checks; this adapter owns only lifecycle.
use super::{I64, U64};
use crate::{catalog_backup as core, storage_volume::NativePath};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fmt::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

const PATH_UNITS: usize = 32 * 1024;
const ERROR_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    Create {
        source: NativePath,
        bundle: NativePath,
    },
    Inspect {
        bundle: NativePath,
    },
    Restore {
        bundle: NativePath,
        destination: NativePath,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Create,
    Inspect,
    Restore,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    CancelRequested,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub phase: core::Phase,
    pub pages_copied: U64,
    pub total_pages: U64,
    pub bytes_processed: U64,
}
impl From<core::Progress> for Progress {
    fn from(p: core::Progress) -> Self {
        Self {
            phase: p.phase,
            pages_copied: U64(p.pages_copied),
            total_pages: U64(p.total_pages),
            bytes_processed: U64(p.bytes_processed),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupReceipt {
    pub protocol: U64,
    pub backup_id: String,
    pub application_id: I64,
    pub schema_version: I64,
    pub database_bytes: U64,
    pub database_blake3: String,
}
impl From<core::BackupReceipt> for BackupReceipt {
    fn from(r: core::BackupReceipt) -> Self {
        Self {
            protocol: U64(u64::from(r.protocol)),
            backup_id: r.backup_id,
            application_id: I64(r.application_id),
            schema_version: I64(r.schema_version),
            database_bytes: U64(r.database_bytes),
            database_blake3: r.database_blake3,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreReceipt {
    pub protocol: U64,
    pub restore_id: String,
    pub backup: BackupReceipt,
    pub schema_version: I64,
}
impl From<core::RestoreReceipt> for RestoreReceipt {
    fn from(r: core::RestoreReceipt) -> Self {
        Self {
            protocol: U64(u64::from(r.protocol)),
            restore_id: r.restore_id,
            backup: r.backup.into(),
            schema_version: I64(r.schema_version),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Receipt {
    Backup(BackupReceipt),
    Restore(RestoreReceipt),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub message: String,
    pub truncated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub operation: String,
    pub kind: Kind,
    pub state: State,
    pub cancellation_requested: bool,
    pub progress: Option<Progress>,
    pub receipt: Option<Receipt>,
    pub error: Option<Failure>,
}

struct Active {
    cancel: core::CancellationToken,
    progress: Arc<Mutex<Option<Progress>>>,
    worker: JoinHandle<std::result::Result<Receipt, Failure>>,
}
/// Own this on one actor or behind one mutex. `status` never waits for a running
/// worker. Only `join`, `shutdown`, and Drop may block through a core boundary.
/// A new start explicitly replaces the one previous terminal snapshot. Operation
/// IDs are minted here, not supplied/replayed by callers; stale cancel IDs fail.
pub struct Coordinator {
    limits: core::Limits,
    latest: Option<Snapshot>,
    active: Option<Active>,
}
impl Coordinator {
    pub fn new(limits: core::Limits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            limits,
            latest: None,
            active: None,
        })
    }
    pub fn start(&mut self, request: Request) -> Result<Snapshot> {
        self.start_observing(request, |_| {})
    }
    fn start_observing<F: FnMut(&Progress) + Send + 'static>(
        &mut self,
        request: Request,
        mut observer: F,
    ) -> Result<Snapshot> {
        self.reap(false)?;
        ensure!(
            self.active.is_none(),
            "backup operation is already running; cancel and join it before starting another"
        );
        let prepared = Prepared::new(request)?;
        let kind = prepared.kind();
        let operation = uuid::Uuid::new_v4().to_string();
        let cancel = core::CancellationToken::default();
        let worker_cancel = cancel.clone();
        let progress = Arc::new(Mutex::new(None));
        let worker_progress = Arc::clone(&progress);
        let limits = self.limits.clone();
        let worker = thread::Builder::new()
            .name("catalog-backup".into())
            .spawn(move || {
                let callback = move |p: core::Progress| -> Result<()> {
                    let p = Progress::from(p);
                    *worker_progress
                        .lock()
                        .map_err(|_| anyhow::anyhow!("backup progress state unavailable"))? =
                        Some(p.clone());
                    observer(&p);
                    Ok(())
                };
                prepared
                    .run(&limits, &worker_cancel, callback)
                    .map_err(failure)
            })
            .context("start owned backup worker")?;
        let snapshot = Snapshot {
            operation,
            kind,
            state: State::Running,
            cancellation_requested: false,
            progress: None,
            receipt: None,
            error: None,
        };
        self.latest = Some(snapshot.clone());
        self.active = Some(Active {
            cancel,
            progress,
            worker,
        });
        Ok(snapshot)
    }
    pub fn status(&mut self) -> Result<Option<Snapshot>> {
        self.reap(false)?;
        self.read_progress()?;
        Ok(self.latest.clone())
    }
    pub fn cancel(&mut self, operation: &str) -> Result<Snapshot> {
        self.reap(false)?;
        ensure!(
            self.latest
                .as_ref()
                .is_some_and(|s| s.operation == operation),
            "backup operation is stale or unavailable"
        );
        if let Some(active) = &self.active {
            active.cancel.cancel();
            let snapshot = self.latest.as_mut().unwrap();
            snapshot.cancellation_requested = true;
            snapshot.state = State::CancelRequested;
        }
        self.status()?.context("backup operation unavailable")
    }
    pub fn join(&mut self) -> Result<Option<Snapshot>> {
        self.reap(true)?;
        Ok(self.latest.clone())
    }
    pub fn shutdown(&mut self) -> Result<Option<Snapshot>> {
        if let Some(active) = &self.active {
            active.cancel.cancel();
            if let Some(snapshot) = &mut self.latest {
                snapshot.cancellation_requested = true;
                snapshot.state = State::CancelRequested;
            }
        }
        self.join()
    }
    fn read_progress(&mut self) -> Result<()> {
        if let (Some(active), Some(snapshot)) = (&self.active, &mut self.latest) {
            snapshot.progress = active
                .progress
                .lock()
                .map_err(|_| anyhow::anyhow!("backup progress state unavailable"))?
                .clone();
        }
        Ok(())
    }
    fn reap(&mut self, wait: bool) -> Result<()> {
        if !self
            .active
            .as_ref()
            .is_some_and(|a| wait || a.worker.is_finished())
        {
            return Ok(());
        }
        let active = self.active.take().unwrap();
        // Join even if progress reporting was poisoned. Worker ownership must
        // never escape because obtaining its last optional progress failed.
        let progress = active.progress.lock().ok().and_then(|p| p.clone());
        let result = active.worker.join().unwrap_or_else(|_| {
            Err(Failure {
                message: "backup worker panicked; inspect the destination before retrying".into(),
                truncated: false,
            })
        });
        let snapshot = self
            .latest
            .as_mut()
            .context("owned backup snapshot unavailable")?;
        snapshot.progress = progress;
        match result {
            Ok(receipt) => {
                snapshot.state = State::Complete;
                snapshot.receipt = Some(receipt);
            }
            Err(error) => {
                snapshot.state = State::Failed;
                snapshot.error = Some(error);
            }
        }
        Ok(())
    }
}
impl Drop for Coordinator {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

enum Prepared {
    Create(PathBuf, PathBuf),
    Inspect(PathBuf),
    Restore(PathBuf, PathBuf),
}
impl Prepared {
    fn new(request: Request) -> Result<Self> {
        fn path(p: NativePath) -> Result<PathBuf> {
            let units = match &p {
                NativePath::UnixBytes(v) => v.len(),
                NativePath::WindowsWide(v) => v.len(),
            };
            ensure!(
                (1..=PATH_UNITS).contains(&units),
                "backup path exceeds unit allowance"
            );
            let p = p.to_path()?;
            ensure!(
                p.is_absolute(),
                "backup paths must be absolute native paths"
            );
            Ok(p)
        }
        Ok(match request {
            Request::Create { source, bundle } => Self::Create(path(source)?, path(bundle)?),
            Request::Inspect { bundle } => Self::Inspect(path(bundle)?),
            Request::Restore {
                bundle,
                destination,
            } => Self::Restore(path(bundle)?, path(destination)?),
        })
    }
    fn kind(&self) -> Kind {
        match self {
            Self::Create(..) => Kind::Create,
            Self::Inspect(..) => Kind::Inspect,
            Self::Restore(..) => Kind::Restore,
        }
    }
    fn run<F: FnMut(core::Progress) -> Result<()>>(
        self,
        limits: &core::Limits,
        cancel: &core::CancellationToken,
        callback: F,
    ) -> Result<Receipt> {
        match self {
            Self::Create(source, bundle) => {
                core::backup_catalog_with_control(source, bundle, limits, cancel, callback)
                    .map(|r| Receipt::Backup(r.into()))
            }
            Self::Inspect(bundle) => {
                core::inspect_backup_with_control(bundle, limits, cancel, callback)
                    .map(|r| Receipt::Backup(r.into()))
            }
            Self::Restore(bundle, destination) => {
                core::restore_catalog_with_control(bundle, destination, limits, cancel, callback)
                    .map(|r| Receipt::Restore(r.into()))
            }
        }
    }
}
fn failure(error: anyhow::Error) -> Failure {
    struct Bounded {
        text: String,
        truncated: bool,
    }
    impl std::fmt::Write for Bounded {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            let mut n = s.len().min(ERROR_BYTES - self.text.len());
            while !s.is_char_boundary(n) {
                n -= 1;
            }
            self.text.push_str(&s[..n]);
            self.truncated |= n != s.len();
            Ok(())
        }
    }
    let mut result = Bounded {
        text: String::new(),
        truncated: false,
    };
    let _ = write!(result, "{error:#}");
    Failure {
        message: result.text,
        truncated: result.truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Catalog;
    use std::sync::mpsc;
    fn native(p: &std::path::Path) -> NativePath {
        NativePath::from_path(p)
    }
    fn coordinator() -> Result<Coordinator> {
        Coordinator::new(core::Limits {
            max_seconds: 20,
            ..Default::default()
        })
    }

    #[test]
    fn backup_inspect_restore_keep_receipts_and_hold() -> Result<()> {
        let t = tempfile::tempdir()?;
        let source = t.path().join("source");
        let bundle = t.path().join("bundle");
        let destination = t.path().join("restored");
        drop(Catalog::open(&source)?);
        let mut c = coordinator()?;
        assert!(c.status()?.is_none());
        let started = c.start(Request::Create {
            source: native(&source),
            bundle: native(&bundle),
        })?;
        let done = c.join()?.unwrap();
        assert_eq!(done.state, State::Complete);
        assert_eq!(done.operation, started.operation);
        let Receipt::Backup(receipt) = done.receipt.unwrap() else {
            panic!()
        };
        assert_eq!(c.cancel(&started.operation)?.state, State::Complete);
        assert!(!c.status()?.unwrap().cancellation_requested);
        let inspect = c.start(Request::Inspect {
            bundle: native(&bundle),
        })?;
        assert_ne!(inspect.operation, started.operation);
        assert!(c.cancel(&started.operation).is_err());
        let inspected = c.join()?.unwrap();
        assert_eq!(inspected.state, State::Complete);
        let Receipt::Backup(checked) = inspected.receipt.unwrap() else {
            panic!()
        };
        assert_eq!(checked.database_blake3, receipt.database_blake3);
        c.start(Request::Restore {
            bundle: native(&bundle),
            destination: native(&destination),
        })?;
        let restored = c.join()?.unwrap();
        assert_eq!(restored.state, State::Complete);
        let Receipt::Restore(r) = restored.receipt.unwrap() else {
            panic!()
        };
        assert_eq!(r.backup.backup_id, receipt.backup_id);
        assert!(core::restore_status(&destination)?.unwrap().jobs_held);
        drop(Catalog::open(&destination)?);
        Ok(())
    }
    #[test]
    fn duplicate_start_and_cancel_during_actual_copy_do_not_publish() -> Result<()> {
        let t = tempfile::tempdir()?;
        let source = t.path().join("source");
        let bundle = t.path().join("bundle");
        drop(Catalog::open(&source)?);
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (go, go_rx) = mpsc::sync_channel(1);
        let mut c = coordinator()?;
        let mut once = false;
        let start = c.start_observing(
            Request::Create {
                source: native(&source),
                bundle: native(&bundle),
            },
            move |p| {
                if p.phase == core::Phase::Copy && !once {
                    once = true;
                    ready_tx.send(()).unwrap();
                    go_rx
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                }
            },
        )?;
        ready.recv_timeout(std::time::Duration::from_secs(10))?;
        assert!(
            c.start(Request::Inspect {
                bundle: native(&bundle)
            })
            .is_err()
        );
        let snapshot = c.cancel(&start.operation)?;
        assert_eq!(snapshot.state, State::CancelRequested);
        assert!(snapshot.cancellation_requested);
        assert!(snapshot.receipt.is_none());
        go.send(())?;
        let done = c.join()?.unwrap();
        assert_eq!(done.state, State::Failed);
        assert!(done.cancellation_requested);
        assert!(done.error.unwrap().message.contains("cancel"));
        assert!(!bundle.join("photocatalog-backup.json").exists());
        assert!(Catalog::open(&bundle).is_err());
        drop(Catalog::open(&source)?);
        Ok(())
    }
    #[test]
    fn drop_cancels_and_joins_before_releasing_worker_ownership() -> Result<()> {
        let t = tempfile::tempdir()?;
        let source = t.path().join("source");
        let bundle = t.path().join("bundle");
        drop(Catalog::open(&source)?);
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (go, go_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished) = mpsc::sync_channel(1);
        let mut c = coordinator()?;
        let mut once = false;
        c.start_observing(
            Request::Create {
                source: native(&source),
                bundle: native(&bundle),
            },
            move |p| {
                if p.phase == core::Phase::Copy && !once {
                    once = true;
                    ready_tx.send(()).unwrap();
                    go_rx
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                }
            },
        )?;
        ready.recv_timeout(std::time::Duration::from_secs(10))?;
        let cancel = c.active.as_ref().unwrap().cancel.clone();
        let dropping = thread::spawn(move || {
            drop(c);
            finished_tx.send(()).unwrap();
        });
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !cancel.is_cancelled() && std::time::Instant::now() < until {
            thread::yield_now();
        }
        assert!(cancel.is_cancelled());
        assert!(finished.try_recv().is_err());
        go.send(())?;
        finished.recv_timeout(std::time::Duration::from_secs(10))?;
        dropping.join().unwrap();
        assert!(!bundle.join("photocatalog-backup.json").exists());
        drop(Catalog::open(&source)?);
        Ok(())
    }
    #[test]
    fn successful_worker_result_wins_a_late_cancel_race() -> Result<()> {
        let mut c = coordinator()?;
        let (go, ready) = mpsc::sync_channel(1);
        c.latest = Some(Snapshot {
            operation: "owned".into(),
            kind: Kind::Inspect,
            state: State::Running,
            cancellation_requested: false,
            progress: None,
            receipt: None,
            error: None,
        });
        c.active = Some(Active {
            cancel: core::CancellationToken::default(),
            progress: Arc::new(Mutex::new(None)),
            worker: thread::spawn(move || {
                ready
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                Ok(Receipt::Backup(BackupReceipt {
                    protocol: U64(1),
                    backup_id: "published".into(),
                    application_id: I64(1),
                    schema_version: I64(10),
                    database_bytes: U64(1),
                    database_blake3: "digest".into(),
                }))
            }),
        });
        assert_eq!(c.cancel("owned")?.state, State::CancelRequested);
        go.send(())?;
        let done = c.join()?.unwrap();
        assert_eq!(done.state, State::Complete);
        assert!(done.cancellation_requested);
        assert!(done.receipt.is_some());
        assert!(done.error.is_none());
        Ok(())
    }
    #[test]
    fn admission_and_failed_destination_preserve_prior_state() -> Result<()> {
        let t = tempfile::tempdir()?;
        let source = t.path().join("source");
        let target = t.path().join("existing");
        drop(Catalog::open(&source)?);
        std::fs::create_dir(&target)?;
        std::fs::write(target.join("keep"), b"unchanged")?;
        let mut c = coordinator()?;
        assert!(
            c.start(Request::Inspect {
                bundle: native(std::path::Path::new("relative"))
            })
            .is_err()
        );
        assert!(c.status()?.is_none());
        c.start(Request::Create {
            source: native(&source),
            bundle: native(&target),
        })?;
        assert_eq!(c.join()?.unwrap().state, State::Failed);
        assert_eq!(std::fs::read(target.join("keep"))?, b"unchanged");
        assert_eq!(std::fs::read_dir(&target)?.count(), 1);
        Ok(())
    }
    #[test]
    fn wire_integers_and_error_storage_are_bounded() {
        let r = BackupReceipt::from(core::BackupReceipt {
            protocol: u32::MAX,
            backup_id: "id".into(),
            application_id: i64::MIN,
            schema_version: i64::MAX,
            database_bytes: u64::MAX,
            database_blake3: "hash".into(),
        });
        let j = serde_json::to_value(r).unwrap();
        assert_eq!(j["protocol"], u32::MAX.to_string());
        assert_eq!(j["application_id"], i64::MIN.to_string());
        assert_eq!(j["database_bytes"], u64::MAX.to_string());
        let p = serde_json::to_value(Progress::from(core::Progress {
            phase: core::Phase::Hash,
            pages_copied: u64::MAX,
            total_pages: u64::MAX,
            bytes_processed: u64::MAX,
        }))
        .unwrap();
        assert_eq!(p["bytes_processed"], u64::MAX.to_string());
        let f = failure(anyhow::anyhow!("雪".repeat(ERROR_BYTES)));
        assert!(f.truncated);
        assert!(f.message.len() <= ERROR_BYTES);
        assert!(f.message.is_char_boundary(f.message.len()));
    }
}
