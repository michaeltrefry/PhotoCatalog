//! One owned import preparer. Legacy mode performs source I/O here; managed mode
//! keeps every source read in F and only associates facts in C's Discovery SQL.
//! Rendezvous delivery bounds unpublished work; the catalog actor alone commits it.
use crate::{
    Catalog,
    catalog_edits::VariantKey,
    catalog_metadata::{self, PreparedImportSource, Source},
    import_storage::ImportVolumes,
    location_bytes,
    preview::{self, PreviewService},
    storage_volume::{LocationState, NativePath, VolumeLocation},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

#[cfg(test)]
pub(crate) type Checkpoint = Arc<dyn Fn(&str, &AtomicBool) + Send + Sync>;

pub(crate) struct Header {
    pub(crate) path: PathBuf,
    fingerprint: String,
    observation: VolumeLocation,
}
pub(crate) enum Event {
    Header(Box<Header>),
    Source(Box<PreparedImportSource>),
    End,
    Skipped,
    Failed { source: NativePath, message: String },
    Finished,
}
pub(crate) struct Preparation {
    session: Arc<crate::catalog_session::CatalogSessionAuthority>,
    receiver: Option<mpsc::Receiver<Event>>,
    cancel: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    commands: Option<mpsc::SyncSender<Command>>,
    managed: bool,
    cleanup: Arc<std::sync::Mutex<Option<RemoteImport>>>,
}
enum Command {
    InspectionCommitted(mpsc::SyncSender<Result<()>>),
    Recheck(mpsc::SyncSender<Result<()>>),
    FileCommitted(mpsc::SyncSender<Result<()>>),
    Retire(mpsc::SyncSender<Result<()>>),
}
impl Preparation {
    pub(crate) fn spawn(
        catalog: &Catalog,
        source: &Path,
        cancel: Arc<AtomicBool>,
        #[cfg(test)] checkpoint: Option<Checkpoint>,
    ) -> Result<Self> {
        catalog.require_jobs_released()?;
        let managed = catalog.session.managed_import_root();
        if managed.is_none() {
            ensure!(source.is_dir(), "import source must be a directory");
            ensure!(
                !source.starts_with(&catalog.root) && !catalog.root.starts_with(source),
                "catalog and originals must be separate directories"
            );
        }
        let discovery = if let Some(pool) = catalog.session.pool() {
            Some(pool.lease(crate::catalog_session::DISCOVERY_ROLE)?)
        } else {
            None
        };
        let (sender, receiver) = mpsc::sync_channel(0);
        let (command_sender, command_receiver) = mpsc::sync_channel(0);
        let source = source.to_path_buf();
        let stop = cancel.clone();
        let is_managed = managed.is_some();
        let session = catalog.session.clone();
        let cleanup = Arc::new(std::sync::Mutex::new(None));
        let failed_cleanup = cleanup.clone();
        let worker = thread::Builder::new()
            .name("catalog-source-preparation".into())
            .spawn(move || {
                let result = if let Some(root) = managed {
                    prepare_managed(
                        session,
                        discovery,
                        root,
                        &source,
                        &sender,
                        &command_receiver,
                        &stop,
                        &failed_cleanup,
                    )
                } else {
                    prepare_walk(
                        discovery,
                        &source,
                        &sender,
                        &stop,
                        #[cfg(test)]
                        checkpoint,
                    )
                };
                if let Err(e) = result
                    && !stop.load(Ordering::Acquire)
                {
                    let _ = sender.send(Event::Failed {
                        source: NativePath::from_path(&source),
                        message: format!("{e:#}").chars().take(2048).collect(),
                    });
                }
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                // Failed spawn drops the closure and returns its admitted role;
                // no thread exists to join, so this concrete owner acknowledges it.
                if let Some(pool) = catalog.session.pool() {
                    pool.joined(crate::catalog_session::DISCOVERY_ROLE, true)?;
                }
                return Err(error.into());
            }
        };
        Ok(Self {
            session: catalog.session.clone(),
            receiver: Some(receiver),
            cancel,
            thread: Some(worker),
            commands: Some(command_sender),
            managed: is_managed,
            cleanup,
        })
    }
    pub(crate) fn poll(&self) -> Result<Option<Event>> {
        match self
            .receiver
            .as_ref()
            .context("preparation already stopped")?
            .try_recv()
        {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                anyhow::bail!("source preparation ended without terminal event")
            }
        }
    }
    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.managed && self.thread.is_some() {
            if let Err(error) = self.command(CommandKind::Retire) {
                self.request_cancel();
                self.join_thread()?;
                if let Err(cleanup) = self.retry_cleanup() {
                    return Err(error).context(format!(
                        "managed import retirement cleanup retained: {cleanup:#}"
                    ));
                }
                return Err(error);
            }
        }
        self.receiver.take();
        self.commands.take();
        self.join_thread()?;
        self.retry_cleanup()
    }
    fn command(&self, kind: CommandKind) -> Result<()> {
        ensure!(
            self.managed,
            "managed import command used by legacy preparation"
        );
        let (tx, rx) = mpsc::sync_channel(0);
        let command = match kind {
            CommandKind::InspectionCommitted => Command::InspectionCommitted(tx),
            CommandKind::Recheck => Command::Recheck(tx),
            CommandKind::FileCommitted => Command::FileCommitted(tx),
            CommandKind::Retire => Command::Retire(tx),
        };
        self.commands
            .as_ref()
            .context("managed import commands stopped")?
            .send(command)
            .map_err(|_| anyhow::anyhow!("managed import owner stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("managed import acknowledgement lost"))?
    }
    pub(crate) fn inspection_committed(&self) -> Result<()> {
        if self.managed {
            self.command(CommandKind::InspectionCommitted)
        } else {
            Ok(())
        }
    }
    pub(crate) fn recheck_current(&self) -> Result<()> {
        if self.managed {
            self.command(CommandKind::Recheck)
        } else {
            Ok(())
        }
    }
    pub(crate) fn file_committed(&self) -> Result<()> {
        if self.managed {
            self.command(CommandKind::FileCommitted)
        } else {
            Ok(())
        }
    }
    pub(crate) fn request_cancel(&mut self) {
        if self.thread.is_some() {
            self.cancel.store(true, Ordering::Release);
        }
        self.commands.take(); // Unblock a managed custody acknowledgement wait.
        self.receiver.take(); // Unblock a rendezvous send before joining.
    }
    pub(crate) fn cancel_and_finish(&mut self) -> Result<()> {
        self.request_cancel();
        self.join_thread()?;
        self.retry_cleanup()
    }
    fn join_thread(&mut self) -> Result<()> {
        if let Some(worker) = self.thread.take() {
            let healthy = worker.join().is_ok();
            if let Some(pool) = self.session.pool() {
                let _ = pool.joined(crate::catalog_session::DISCOVERY_ROLE, healthy);
            }
            ensure!(healthy, "source preparation owner panicked");
        }
        Ok(())
    }
    fn retry_cleanup(&self) -> Result<()> {
        let mut slot = self.cleanup.lock().unwrap();
        let Some(mut remote) = slot.take() else {
            return Ok(());
        };
        match remote.abort() {
            Ok(()) => Ok(()),
            Err(error) => {
                *slot = Some(remote);
                Err(error).context("managed import cleanup remains retained")
            }
        }
    }
}
impl Drop for Preparation {
    fn drop(&mut self) {
        if self.cancel_and_finish().is_err()
            && self
                .cleanup
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some()
        {
            // An unexpected last-owner drop is not evidence that F released the
            // source/lock. Quarantine the exact request/session owner rather than
            // letting Arc drop guess at retirement. Normal Close retains `self`
            // and retries this cleanup instead of reaching this branch.
            std::mem::forget(self.cleanup.clone());
        }
    }
}
fn canceled(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "source preparation canceled"
    );
    Ok(())
}
fn send(sender: &mpsc::SyncSender<Event>, cancel: &AtomicBool, event: Event) -> Result<()> {
    canceled(cancel)?;
    sender
        .send(event)
        .map_err(|_| anyhow::anyhow!("source preparation receiver closed"))
}
#[derive(PartialEq, Eq)]
struct Stamp {
    identity: (u64, u64),
    length: u64,
    modified: std::time::SystemTime,
}
fn stamp(file: &File) -> Result<Stamp> {
    let m = file.metadata()?;
    Ok(Stamp {
        identity: crate::metadata_export::held_file_identity(file)?,
        length: m.len(),
        modified: m.modified()?,
    })
}
fn fingerprint(
    path: &Path,
    cancel: &AtomicBool,
    #[cfg(test)] checkpoint: Option<&Checkpoint>,
) -> Result<(String, File, Stamp)> {
    canceled(cancel)?;
    let metadata = fs::symlink_metadata(path)?;
    let mut file = crate::xmp_packets::open_regular(path, &metadata)?;
    let before = stamp(&file)?;
    let mut hash = blake3::Hasher::new();
    let mut bytes = [0u8; 65536];
    let mut total = 0u64;
    loop {
        canceled(cancel)?;
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .context("source length overflow")?;
        ensure!(total <= before.length, "source grew during preparation");
        hash.update(&bytes[..n]);
        #[cfg(test)]
        if let Some(checkpoint) = checkpoint {
            checkpoint("hash_chunk", cancel);
        }
    }
    ensure!(
        total == before.length && stamp(&file)? == before,
        "source changed during preparation"
    );
    recheck(path, &file, &before)?;
    Ok((hash.finalize().to_hex().to_string(), file, before))
}
fn recheck(path: &Path, file: &File, before: &Stamp) -> Result<()> {
    let current = crate::xmp_packets::open_regular(path, &fs::symlink_metadata(path)?)?;
    ensure!(
        stamp(file)? == *before && stamp(&current)? == *before,
        "source changed before prepared publication"
    );
    Ok(())
}
fn prepare_walk(
    admitted: Option<crate::catalog_session::SqlConnection>,
    root: &Path,
    sender: &mpsc::SyncSender<Event>,
    cancel: &Arc<AtomicBool>,
    #[cfg(test)] checkpoint: Option<Checkpoint>,
) -> Result<()> {
    let discovery = match admitted {
        Some(db) => catalog_metadata::ImportDiscovery::from_admitted(db, cancel.clone())?,
        None => catalog_metadata::ImportDiscovery::new()?,
    };
    let mut volumes = ImportVolumes::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false).max_open(16) {
        canceled(cancel)?;
        let entry = entry.context("discover source folder")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !crate::media::supported_extension(&extension) {
            send(sender, cancel, Event::Skipped)?;
            continue;
        }
        #[cfg(test)]
        if let Some(checkpoint) = &checkpoint {
            checkpoint("source", cancel);
        }
        let result = (|| -> Result<()> {
            let (fingerprint, file, before) = fingerprint(
                path,
                cancel,
                #[cfg(test)]
                checkpoint.as_ref(),
            )?;
            let observation = volumes.observe(path)?;
            canceled(cancel)?;
            let sidecars = discovery.sidecars(path, cancel)?;
            send(
                sender,
                cancel,
                Event::Header(Box::new(Header {
                    path: path.into(),
                    fingerprint,
                    observation,
                })),
            )?;
            let embedded = Source {
                kind: "embedded".into(),
                locator: location_bytes(path),
                display: path.to_string_lossy().into_owned(),
                ambiguous: false,
                provenance: serde_json::json!({"discovery":"original file"}),
            };
            for source in std::iter::once(embedded).chain(sidecars) {
                let prepared = catalog_metadata::prepare_import_source(source, cancel)?;
                send(sender, cancel, Event::Source(Box::new(prepared)))?;
            }
            canceled(cancel)?;
            recheck(path, &file, &before)?;
            send(sender, cancel, Event::End)
        })();
        if let Err(e) = result {
            canceled(cancel)?;
            send(
                sender,
                cancel,
                Event::Failed {
                    source: NativePath::from_path(path),
                    message: format!("{e:#}").chars().take(2048).collect(),
                },
            )?;
            return Ok(());
        }
    }
    send(sender, cancel, Event::Finished)
}

/// Authority is stored catalog identity, never the worker's guess at a row ID.
pub(crate) struct Reference {
    asset: String,
    path: PathBuf,
    fingerprint: String,
    previous_fingerprint: Option<String>,
    ready: bool,
    seen: BTreeSet<Vec<u8>>,
    changed: bool,
    warnings: u64,
}
impl Reference {
    pub(crate) fn source_path(&self) -> NativePath {
        NativePath::from_path(&self.path)
    }
    fn check(&self, db: &Connection) -> Result<()> {
        let current: (Vec<u8>, Option<String>, String) = db.query_row(
            "SELECT location,fingerprint,state FROM assets WHERE id=?",
            [&self.asset],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        ensure!(
            current.0 == location_bytes(&self.path)
                && current.1 == self.previous_fingerprint
                && current.2 == if self.ready { "ready" } else { "pending" },
            "original reference changed during preparation; retry required"
        );
        Ok(())
    }
    pub(crate) fn begin(catalog: &mut Catalog, header: Header) -> Result<Self> {
        catalog.require_jobs_released()?;
        let location = location_bytes(&header.path);
        crate::catalog_storage::verify_location_fence(
            &catalog.db,
            &location,
            Some(&header.fingerprint),
        )?;
        ensure!(
            header.observation.requested_path == NativePath::from_path(&header.path),
            "volume observation path differs"
        );
        let identity = header
            .observation
            .volume
            .as_ref()
            .and_then(|v| v.persistent_identity.as_ref())
            .map(serde_json::to_string)
            .transpose()?;
        let existing:Option<(String,Option<String>,String,i64)>=catalog.db.query_row("SELECT a.id,a.fingerprint,a.state,EXISTS(SELECT 1 FROM edit_variants WHERE asset_id=a.id AND revision>0) FROM assets a WHERE location=?",[&location],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let bound:Option<String>=catalog.db.query_row("SELECT v.identity FROM assets a JOIN storage_bindings b ON b.asset_id=a.id JOIN storage_volumes v ON v.id=b.volume_id WHERE a.location=?",[&location],|r|r.get(0)).optional()?.flatten();
        if let Some(bound) = bound {
            ensure!(
                identity.as_ref() == Some(&bound)
                    || existing.as_ref().and_then(|v| v.1.as_ref()) == Some(&header.fingerprint),
                "relink-required: source volume identity changed; explicit relink review required"
            );
        }
        if let (Some(identity), Some(relative)) =
            (&identity, &header.observation.relative_in_volume)
        {
            let matches:Vec<Vec<u8>>=catalog.db.prepare("SELECT a.location FROM assets a JOIN storage_bindings b ON b.asset_id=a.id JOIN storage_volumes v ON v.id=b.volume_id WHERE v.identity=?1 AND b.relative=?2 LIMIT 2")?.query_map(params![identity,serde_json::to_string(relative)?],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
            ensure!(
                matches.len() < 2 && matches.iter().all(|p| p == &location),
                "relink-required: existing volume-relative original uses another path; explicit relink review required"
            );
        }
        if let Some((_, fp, state, revision)) = &existing {
            ensure!(
                *revision == 0 || (fp.as_ref() == Some(&header.fingerprint) && state == "ready"),
                "source-changed: content or availability changed on an edited image; explicit source review required; edits retained"
            );
        }
        let ready = existing.as_ref().is_some_and(|(_, fp, state, _)| {
            fp.as_ref() == Some(&header.fingerprint) && state == "ready"
        });
        if !ready {
            catalog.reserve(&header.path, &location)?;
        }
        let asset: String =
            catalog
                .db
                .query_row("SELECT id FROM assets WHERE location=?", [&location], |r| {
                    r.get(0)
                })?;
        catalog.record_import_path(&header.path)?;
        if header.observation.state == LocationState::Available {
            catalog.bind_storage(&asset, &header.observation)?;
        }
        Ok(Self {
            asset,
            path: header.path,
            fingerprint: header.fingerprint,
            previous_fingerprint: existing.and_then(|v| v.1),
            ready,
            seen: BTreeSet::new(),
            changed: false,
            warnings: 0,
        })
    }
    pub(crate) fn source(
        &mut self,
        catalog: &mut Catalog,
        source: &PreparedImportSource,
    ) -> Result<()> {
        let _write = catalog
            .writers
            .enter(crate::catalog_writer::Priority::Background)?;
        let tx = catalog
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        self.check(&tx)?;
        if source.source.kind == "embedded" {
            ensure!(
                source.source.locator == location_bytes(&self.path),
                "embedded source path differs"
            );
        } else {
            self.seen.insert(source.source.locator.clone());
        }
        let (changed, warning) = catalog_metadata::apply_import_source(&tx, &self.asset, source)?;
        tx.commit()?;
        self.changed |= changed;
        self.warnings += u64::from(warning);
        Ok(())
    }
    pub(crate) fn finish(
        mut self,
        catalog: &mut Catalog,
        service: &mut PreviewService,
    ) -> Result<(Option<preview::Consumer>, bool, u64)> {
        {
            let _write = catalog
                .writers
                .enter(crate::catalog_writer::Priority::Background)?;
            let tx = catalog
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            self.check(&tx)?;
            self.changed |= catalog_metadata::finish_import_sources(&tx, &self.asset, &self.seen)?;
            tx.commit()?;
        }
        let consumer = if self.ready {
            let key = VariantKey::master(&self.asset);
            if service
                .cached_variant(catalog, &key, preview::Tier::Thumbnail, false)?
                .is_some()
            {
                None
            } else {
                Some(service.request_variant(
                    catalog,
                    &key,
                    preview::Tier::Thumbnail,
                    preview::Priority::Background,
                )?)
            }
        } else {
            Some(service.submit_import(catalog, &self.asset, &self.path, &self.fingerprint)?)
        };
        Ok((consumer, self.changed, self.warnings))
    }
    pub(crate) fn fail(&self, catalog: &mut Catalog, reason: &str) -> Result<()> {
        if !self.ready {
            self.check(&catalog.db)?;
            catalog.fail(&location_bytes(&self.path), &anyhow::anyhow!("{reason}"))?;
        }
        Ok(())
    }
}

/// One selected-file read slot, independent of the folder walk. No directory
/// association, XMP refresh, renderer, or catalog writer runs on this thread.
pub(crate) struct SinglePreparation {
    receiver: Option<mpsc::Receiver<std::result::Result<String, String>>>,
    cancel: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    result: Option<std::result::Result<String, String>>,
}
impl SinglePreparation {
    pub(crate) fn spawn(
        path: PathBuf,
        session: Arc<crate::catalog_session::CatalogSessionAuthority>,
        #[cfg(test)] checkpoint: Option<Checkpoint>,
    ) -> Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let (sender, receiver) = mpsc::sync_channel(0);
        let worker = thread::Builder::new()
            .name("catalog-original-preparation".into())
            .spawn(move || {
                #[cfg(test)]
                let checkpoint: Option<Checkpoint> = checkpoint.map(|callback| {
                    Arc::new(move |stage: &str, cancel: &AtomicBool| {
                        if stage == "hash_chunk" {
                            callback("hydration_hash_chunk", cancel);
                        }
                    }) as Checkpoint
                });
                let result = if session.pool().is_some() {
                    crate::catalog_session::storage::Observer(session)
                        .evidence(&path, &stop)
                        .map(|evidence| evidence.hash)
                } else {
                    fingerprint(
                        &path,
                        &stop,
                        #[cfg(test)]
                        checkpoint.as_ref(),
                    )
                    .map(|(digest, _, _)| digest)
                }
                .map_err(|e| format!("prepare original: {e:#}"));
                if !stop.load(Ordering::Acquire) {
                    let _ = sender.send(result);
                }
            })?;
        Ok(Self {
            receiver: Some(receiver),
            cancel,
            worker: Some(worker),
            result: None,
        })
    }
    pub(crate) fn request_cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.receiver.take();
    }
    pub(crate) fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
    }
    pub(crate) fn poll(&mut self) -> Result<Option<std::result::Result<String, String>>> {
        if self.result.is_none() {
            match self
                .receiver
                .as_ref()
                .context("original preparation stopped")?
                .try_recv()
            {
                Ok(result) => {
                    self.result = Some(result);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result = Some(Err("original preparation stopped without a result".into()));
                }
            }
        }
        if self.is_finished() {
            self.receiver.take();
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            return Ok(self.result.take());
        }
        Ok(None)
    }
}
#[derive(Clone, Copy)]
enum CommandKind {
    InspectionCommitted,
    Recheck,
    FileCommitted,
    Retire,
}

struct RemoteImport {
    session: Arc<crate::catalog_session::CatalogSessionAuthority>,
    root: crate::catalog_session::RootCapability,
    transfer: crate::catalog_session::LeaseId,
    step: u64,
    pending: Option<crate::catalog_session::import::Request>,
    active: bool,
}
struct RemoteCustody {
    remote: Option<RemoteImport>,
    cleanup: Arc<std::sync::Mutex<Option<RemoteImport>>>,
}
impl RemoteCustody {
    fn new(remote: RemoteImport, cleanup: Arc<std::sync::Mutex<Option<RemoteImport>>>) -> Self {
        Self {
            remote: Some(remote),
            cleanup,
        }
    }
    fn remote(&mut self) -> &mut RemoteImport {
        self.remote.as_mut().expect("managed import custody")
    }
    fn retired(&mut self) {
        self.remote = None;
    }
}
impl Drop for RemoteCustody {
    fn drop(&mut self) {
        if let Some(remote) = self.remote.take() {
            let mut cleanup = self
                .cleanup
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            cleanup.get_or_insert(remote);
        }
    }
}
impl RemoteImport {
    fn call(
        &mut self,
        action: crate::catalog_session::import::Action,
        cancel: &AtomicBool,
    ) -> Result<crate::catalog_session::import::Value> {
        ensure!(
            self.pending.is_none(),
            "managed import has an unreconciled request"
        );
        let request = crate::catalog_session::import::Request {
            root: self.root.clone(),
            transfer: self.transfer.clone(),
            step: crate::application::U64(self.step),
            action,
        };
        self.pending = Some(request.clone());
        let reply = match self.session.import_call(&request, cancel) {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                self.pending = None;
                anyhow::bail!("managed catalog did not route import to F")
            }
            Err(error) => {
                if error
                    .downcast_ref::<crate::filesystem_worker::wire::Failure>()
                    .is_some_and(|failure| {
                        failure.kind != crate::filesystem_worker::wire::FailureKind::Unknown
                    })
                {
                    self.pending = None;
                }
                return Err(error);
            }
        };
        reply.validate(&request)?;
        self.pending = None;
        self.step = self
            .step
            .checked_add(1)
            .context("managed import step exhausted")?;
        self.observe(&reply.value);
        match reply.value {
            crate::catalog_session::import::Value::Failed(failure) => {
                Err(anyhow::Error::new(failure))
            }
            value => Ok(value),
        }
    }
    /// An outer cancellation or lost reply never establishes whether F consumed
    /// a step. Replay only the exact retained request and validate its digest.
    fn reconcile(&mut self) -> Result<Option<crate::catalog_session::import::Value>> {
        let Some(request) = self.pending.clone() else {
            return Ok(None);
        };
        let reply = match self.session.import_call(&request, &AtomicBool::new(false)) {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                self.pending = None;
                anyhow::bail!("managed catalog did not route retained import request")
            }
            Err(error) => {
                if error
                    .downcast_ref::<crate::filesystem_worker::wire::Failure>()
                    .is_some_and(|failure| {
                        failure.kind != crate::filesystem_worker::wire::FailureKind::Unknown
                    })
                {
                    self.pending = None;
                }
                return Err(error);
            }
        };
        reply.validate(&request)?;
        self.pending = None;
        self.step = self
            .step
            .checked_add(1)
            .context("managed import step exhausted")?;
        self.observe(&reply.value);
        Ok(Some(reply.value))
    }
    fn abort(&mut self) -> Result<()> {
        if self.pending.is_none() && !self.active {
            return Ok(());
        }
        if matches!(
            self.reconcile()?,
            Some(
                crate::catalog_session::import::Value::Finished
                    | crate::catalog_session::import::Value::Aborted
            )
        ) {
            return Ok(());
        }
        match self.call(
            crate::catalog_session::import::Action::Abort,
            &AtomicBool::new(false),
        )? {
            crate::catalog_session::import::Value::Aborted => Ok(()),
            _ => anyhow::bail!("managed import abort reply mismatch"),
        }
    }
    fn observe(&mut self, value: &crate::catalog_session::import::Value) {
        match value {
            crate::catalog_session::import::Value::Begun => self.active = true,
            crate::catalog_session::import::Value::Finished
            | crate::catalog_session::import::Value::Aborted => self.active = false,
            _ => {}
        }
    }
}

fn prepare_managed(
    session: Arc<crate::catalog_session::CatalogSessionAuthority>,
    admitted: Option<crate::catalog_session::SqlConnection>,
    root: crate::catalog_session::RootCapability,
    source_root: &Path,
    sender: &mpsc::SyncSender<Event>,
    commands: &mpsc::Receiver<Command>,
    cancel: &Arc<AtomicBool>,
    cleanup: &Arc<std::sync::Mutex<Option<RemoteImport>>>,
) -> Result<()> {
    let db = admitted.context("managed import discovery SQL role is unavailable")?;
    let discovery = catalog_metadata::ImportDiscovery::from_admitted(db, cancel.clone())?;
    let mut custody = RemoteCustody::new(
        RemoteImport {
            session,
            root,
            transfer: crate::catalog_session::LeaseId::new(),
            step: 0,
            pending: None,
            active: false,
        },
        cleanup.clone(),
    );
    let run = (|| -> Result<()> {
        let remote = custody.remote();
        match remote.call(
            crate::catalog_session::import::Action::Begin {
                source: NativePath::from_path(source_root),
            },
            cancel,
        )? {
            crate::catalog_session::import::Value::Begun => {}
            _ => anyhow::bail!("managed import begin reply mismatch"),
        }
        loop {
            match remote.call(crate::catalog_session::import::Action::Next, cancel)? {
                crate::catalog_session::import::Value::DirectoryStart { directory } => {
                    discovery.begin_directory(&directory.to_path()?)?
                }
                crate::catalog_session::import::Value::DirectoryFacts { directory, facts } => {
                    let directory = directory.to_path()?;
                    discovery.directory_facts(
                        &directory,
                        facts.into_iter().map(|fact| {
                            Ok(catalog_metadata::DirectoryFact {
                                path: fact.path.to_path()?,
                                regular: fact.regular,
                            })
                        }),
                        cancel,
                    )?;
                }
                crate::catalog_session::import::Value::DirectoryEnd { directory } => {
                    discovery.finish_directory(&directory.to_path()?)?
                }
                crate::catalog_session::import::Value::Skipped => {
                    send(sender, cancel, Event::Skipped)?
                }
                crate::catalog_session::import::Value::Header {
                    path,
                    fingerprint,
                    observation,
                } => {
                    let path = path.to_path()?;
                    send(
                        sender,
                        cancel,
                        Event::Header(Box::new(Header {
                            path: path.clone(),
                            fingerprint,
                            observation,
                        })),
                    )?;
                    let embedded = Source {
                        kind: "embedded".into(),
                        locator: location_bytes(&path),
                        display: path.to_string_lossy().into_owned(),
                        ambiguous: false,
                        provenance: serde_json::json!({"discovery":"original file"}),
                    };
                    for source in
                        std::iter::once(embedded).chain(discovery.sidecars_indexed(&path, cancel)?)
                    {
                        let prepared = match remote.call(
                            crate::catalog_session::import::Action::Inspect {
                                source: source.clone(),
                            },
                            cancel,
                        )? {
                            crate::catalog_session::import::Value::Inspection {
                                source: expected_source,
                                bytes,
                                checksum,
                            } => {
                                ensure!(
                                    expected_source.kind == source.kind
                                        && expected_source.locator == source.locator
                                        && expected_source.display == source.display
                                        && expected_source.ambiguous == source.ambiguous
                                        && expected_source.provenance == source.provenance,
                                    "managed inspection source changed"
                                );
                                let length = usize::try_from(bytes.0)?;
                                ensure!(
                                    length <= crate::catalog_session::import::MAX_INSPECTION_BYTES,
                                    "managed inspection byte limit"
                                );
                                let mut encoded = Vec::new();
                                encoded.try_reserve_exact(length)?;
                                while encoded.len() < length {
                                    let offset = encoded.len() as u64;
                                    match remote.call(
                                        crate::catalog_session::import::Action::Read {
                                            offset: crate::application::U64(offset),
                                        },
                                        cancel,
                                    )? {
                                        crate::catalog_session::import::Value::Chunk {
                                            offset: actual,
                                            bytes,
                                            ..
                                        } => {
                                            ensure!(
                                                actual.0 == offset
                                                    && bytes.len() <= length - encoded.len(),
                                                "managed inspection chunk range"
                                            );
                                            encoded.extend_from_slice(&bytes);
                                        }
                                        _ => {
                                            anyhow::bail!("managed inspection chunk reply mismatch")
                                        }
                                    }
                                }
                                ensure!(
                                    blake3::hash(&encoded).to_hex().as_str() == checksum,
                                    "managed inspection transfer checksum"
                                );
                                match remote.call(
                                    crate::catalog_session::import::Action::FinishInspection,
                                    cancel,
                                )? {
                                    crate::catalog_session::import::Value::InspectionFinished => {}
                                    _ => anyhow::bail!("managed inspection finish reply mismatch"),
                                }
                                let inspection =
                                    crate::catalog_session::import::decode_inspection(&encoded)?;
                                catalog_metadata::prepare_import_inspection(
                                    source.clone(),
                                    inspection,
                                    cancel,
                                )?
                            }
                            crate::catalog_session::import::Value::InspectionFailed {
                                source: expected_source,
                                message,
                            } => {
                                ensure!(
                                    expected_source.kind == source.kind
                                        && expected_source.locator == source.locator
                                        && expected_source.display == source.display
                                        && expected_source.ambiguous == source.ambiguous
                                        && expected_source.provenance == source.provenance,
                                    "managed failed-inspection source changed"
                                );
                                catalog_metadata::prepare_import_failure(
                                    source.clone(),
                                    message,
                                    cancel,
                                )?
                            }
                            _ => anyhow::bail!("managed inspection begin reply mismatch"),
                        };
                        send(sender, cancel, Event::Source(Box::new(prepared)))?;
                        let Command::InspectionCommitted(reply) =
                            commands.recv().map_err(|_| {
                                anyhow::anyhow!(
                                    "managed import inspection acknowledgement channel closed"
                                )
                            })?
                        else {
                            anyhow::bail!("managed import command ordering mismatch")
                        };
                        let result = remote
                            .call(
                                crate::catalog_session::import::Action::CommitInspection,
                                &AtomicBool::new(false),
                            )
                            .and_then(|value| {
                                ensure!(
                                    matches!(
                                        value,
                                        crate::catalog_session::import::Value::InspectionCommitted
                                    ),
                                    "managed inspection commit reply mismatch"
                                );
                                Ok(())
                            });
                        let failed = result.is_err();
                        let _ = reply.send(result);
                        ensure!(!failed, "managed inspection commit failed");
                    }
                    send(sender, cancel, Event::End)?;
                    let Command::Recheck(reply) = commands
                        .recv()
                        .map_err(|_| anyhow::anyhow!("managed import recheck channel closed"))?
                    else {
                        anyhow::bail!("managed import command ordering mismatch")
                    };
                    let result = remote
                        .call(
                            crate::catalog_session::import::Action::Recheck,
                            &AtomicBool::new(false),
                        )
                        .and_then(|value| {
                            ensure!(
                                matches!(value, crate::catalog_session::import::Value::Rechecked),
                                "managed import recheck reply mismatch"
                            );
                            Ok(())
                        });
                    let failed = result.is_err();
                    let _ = reply.send(result);
                    ensure!(!failed, "managed import recheck failed");
                    let Command::FileCommitted(reply) = commands
                        .recv()
                        .map_err(|_| anyhow::anyhow!("managed import commit channel closed"))?
                    else {
                        anyhow::bail!("managed import command ordering mismatch")
                    };
                    let result = remote
                        .call(
                            crate::catalog_session::import::Action::CommitFile,
                            &AtomicBool::new(false),
                        )
                        .and_then(|value| {
                            ensure!(
                                matches!(
                                    value,
                                    crate::catalog_session::import::Value::FileCommitted
                                ),
                                "managed import commit reply mismatch"
                            );
                            Ok(())
                        });
                    let failed = result.is_err();
                    let _ = reply.send(result);
                    ensure!(!failed, "managed import commit failed");
                }
                crate::catalog_session::import::Value::WalkFinished => {
                    send(sender, cancel, Event::Finished)?;
                    let Command::Retire(reply) = commands
                        .recv()
                        .map_err(|_| anyhow::anyhow!("managed import retirement channel closed"))?
                    else {
                        anyhow::bail!("managed import command ordering mismatch")
                    };
                    let result = remote
                        .call(
                            crate::catalog_session::import::Action::Finish,
                            &AtomicBool::new(false),
                        )
                        .and_then(|value| {
                            ensure!(
                                matches!(value, crate::catalog_session::import::Value::Finished),
                                "managed import finish reply mismatch"
                            );
                            Ok(())
                        });
                    let failed = result.is_err();
                    let _ = reply.send(result);
                    ensure!(!failed, "managed import retirement failed");
                    return Ok(());
                }
                _ => anyhow::bail!("managed import walk reply mismatch"),
            }
        }
    })();
    if let Err(error) = run {
        if let Err(cleanup_error) = custody.remote().abort() {
            return Err(error).context(format!(
                "managed import cleanup retained after failure: {cleanup_error:#}"
            ));
        }
        custody.retired();
        return Err(error);
    }
    custody.retired();
    Ok(())
}
impl Drop for SinglePreparation {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.receiver.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::{Recipe, RecipeV1};
    use crate::storage_volume::{IdentityScheme, MountedVolume, PersistentVolumeId};

    fn observation(path: &Path, relative: &Path) -> VolumeLocation {
        VolumeLocation {
            requested_path: NativePath::from_path(path),
            state: LocationState::Available,
            canonical_path: Some(NativePath::from_path(path)),
            volume: Some(MountedVolume {
                mount_path: NativePath::from_path(path.parent().unwrap()),
                volume_subpath: NativePath::from_path(Path::new("")),
                persistent_identity: Some(
                    PersistentVolumeId::new(
                        IdentityScheme::MacVolumeUuid,
                        "12345678-1234-1234-1234-123456789abc",
                    )
                    .unwrap(),
                ),
                filesystem: "fixture".into(),
                device_number: None,
                issues: vec![],
            }),
            relative_in_volume: Some(NativePath::from_path(relative)),
            existing_ancestor: None,
            issues: vec![],
        }
    }

    #[test]
    fn edited_copy_changed_source_and_known_binding_reject_before_mutation() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        fs::create_dir(&originals)?;
        let originals = originals.canonicalize()?;
        let path = originals.join("one.png");
        image::RgbImage::from_pixel(24, 16, image::Rgb([20u8, 40, 70])).save(&path)?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        catalog.import(&originals, None, |_| Ok(()))?;
        let key = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
        let before = catalog.render_identity(&key.asset_id)?;
        let copy = catalog.create_edit_variant(&key, 0, "edited copy")?;
        let saved = catalog.save_edit_recipe(
            &copy.key,
            0,
            &Recipe::V1(RecipeV1 {
                exposure_ev: 1.0,
                ..RecipeV1::default()
            }),
        )?;
        let relative = Path::new("DCIM/one.png");
        catalog.bind_storage(&key.asset_id, &observation(&path, relative))?;
        let mutations = catalog.db.total_changes();
        let changed = Reference::begin(
            &mut catalog,
            Header {
                path: path.clone(),
                fingerprint: "different-content".into(),
                observation: observation(&path, relative),
            },
        );
        assert!(
            changed
                .err()
                .context("changed content admitted")?
                .to_string()
                .contains("source-changed")
        );
        assert_eq!(catalog.db.total_changes(), mutations);
        let other = originals.join("other.png");
        fs::copy(&path, &other)?;
        let changed = Reference::begin(
            &mut catalog,
            Header {
                path: other.clone(),
                fingerprint: before.fingerprint.clone().unwrap(),
                observation: observation(&other, relative),
            },
        );
        assert!(
            changed
                .err()
                .context("duplicate admitted")?
                .to_string()
                .contains("relink-required")
        );
        assert_eq!(catalog.db.total_changes(), mutations);
        assert_eq!(catalog.browse(0, 10)?.len(), 1);
        assert_eq!(
            catalog.edit_variant(&copy.key)?.recipe_digest,
            saved.recipe_digest
        );
        let after = catalog.render_identity(&key.asset_id)?;
        assert_eq!(
            serde_json::to_value(&after)?,
            serde_json::to_value(&before)?
        );
        Ok(())
    }
    #[test]
    fn reviewed_initial_source_fence_rejects_both_import_paths_before_any_mutation() -> Result<()> {
        use crate::catalog_storage::RelinkScope;
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        fs::create_dir(&originals)?;
        let path = originals.canonicalize()?.join("one.png");
        image::RgbImage::from_pixel(24, 16, image::Rgb([20u8, 40, 70])).save(&path)?;
        let old = temp.path().join("missing/one.png");
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        catalog.db.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES('pending',?1,?2,'pending')",
            params![location_bytes(&old), old.to_string_lossy()],
        )?;
        let key = VariantKey::master("pending");
        let copy = catalog.create_edit_variant(&key, 0, "retained copy")?;
        catalog.save_edit_recipe(
            &copy.key,
            0,
            &Recipe::V1(RecipeV1 {
                exposure_ev: 1.5,
                ..Default::default()
            }),
        )?;
        let plan = catalog.begin_relink_review(RelinkScope::Asset {
            asset_id: key.asset_id.clone(),
            destinations: vec![NativePath::from_path(&path)],
        })?;
        let plan = catalog.prepare_relink_batch(&plan.id, 1)?;
        catalog.confirm_relink_associations(
            &plan.id,
            plan.revision,
            plan.confirmation_token.as_deref().unwrap(),
            "no_retained_original_digest",
        )?;
        catalog.apply_relink(&plan.id)?;
        image::RgbImage::from_pixel(24, 16, image::Rgb([70u8, 40, 20])).save(&path)?;
        let changed_bytes = fs::read(&path)?;
        let changes = catalog.db.total_changes();
        let error = Reference::begin(
            &mut catalog,
            Header {
                path: path.clone(),
                fingerprint: crate::fingerprint(&path)?,
                observation: observation(&path, Path::new("one.png")),
            },
        )
        .err()
        .context("changed reviewed source admitted")?;
        assert!(error.to_string().contains("user-reviewed"), "{error:#}");
        assert_eq!(catalog.db.total_changes(), changes);
        let mut volumes = crate::import_storage::ImportVolumes::new();
        let mut report = crate::ImportReport::default();
        let error = catalog
            .import_file(&path, &mut volumes, &mut report, &mut |_| Ok(()), &mut None)
            .unwrap_err();
        assert!(error.to_string().contains("user-reviewed"), "{error:#}");
        assert_eq!(catalog.db.total_changes(), changes);
        assert_eq!(fs::read(&path)?, changed_bytes);
        fs::remove_file(&path)?;
        let error = catalog
            .import_file(&path, &mut volumes, &mut report, &mut |_| Ok(()), &mut None)
            .unwrap_err();
        assert!(error.to_string().contains("user-reviewed"), "{error:#}");
        assert_eq!(catalog.db.total_changes(), changes);
        Ok(())
    }
}
