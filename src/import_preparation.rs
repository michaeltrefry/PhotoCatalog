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
    pub(crate) fingerprint: String,
    pub(crate) observation: VolumeLocation,
}
pub(crate) enum Event {
    Begun { source: NativePath },
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
    AcceptRoot {
        source: NativePath,
        reply: mpsc::SyncSender<Result<()>>,
    },
    ValidateInspection(mpsc::SyncSender<Result<crate::catalog_session::LeaseId>>),
    ReleaseInspection {
        grant: crate::catalog_session::LeaseId,
        reply: mpsc::SyncSender<Result<()>>,
    },
    ValidateFile(mpsc::SyncSender<Result<crate::catalog_session::LeaseId>>),
    ReleaseFile {
        grant: crate::catalog_session::LeaseId,
        reply: mpsc::SyncSender<Result<()>>,
    },
    Retire(mpsc::SyncSender<Result<()>>),
}
#[cfg(test)]
pub(crate) enum TestPublication {
    AcceptRoot,
    RejectRoot,
    RejectInspection,
    RejectFile,
    LoseFileRelease,
}
impl Preparation {
    #[cfg(test)]
    pub(crate) fn publication_test(
        catalog: &Catalog,
        event: Event,
        behavior: TestPublication,
    ) -> Result<Self> {
        // Tests make a single nonblocking actor tick, so publish the fixture
        // event before returning instead of racing the worker scheduler.
        let (sender, receiver) = mpsc::sync_channel(1);
        let (command_sender, command_receiver) = mpsc::sync_channel(0);
        let cancel = Arc::new(AtomicBool::new(false));
        let expected_root = match &event {
            Event::Begun { source } => Some(source.clone()),
            _ => None,
        };
        sender
            .send(event)
            .map_err(|_| anyhow::anyhow!("publication test receiver closed"))?;
        let worker = thread::Builder::new()
            .name("catalog-publication-test".into())
            .spawn(move || {
                let run = (|| -> Result<()> {
                    // Keep the event stream alive until command handling ends,
                    // matching the production worker's disconnect ordering.
                    let _event_sender = sender;
                    match behavior {
                        TestPublication::AcceptRoot => {
                            let Command::AcceptRoot { source, reply } = command_receiver.recv()?
                            else {
                                anyhow::bail!("expected original-root acknowledgement")
                            };
                            ensure!(
                                Some(source) == expected_root,
                                "accepted original root changed"
                            );
                            let _ = reply.send(Ok(()));
                        }
                        TestPublication::RejectRoot => {
                            ensure!(
                                command_receiver.recv().is_err(),
                                "rejected root was acknowledged"
                            );
                        }
                        TestPublication::RejectInspection => {
                            let Command::ValidateInspection(reply) = command_receiver.recv()?
                            else {
                                anyhow::bail!("expected inspection validation")
                            };
                            let _ = reply.send(Err(anyhow::anyhow!(
                                "sidecar changed before catalog commit"
                            )));
                        }
                        TestPublication::RejectFile => {
                            let Command::ValidateFile(reply) = command_receiver.recv()? else {
                                anyhow::bail!("expected file validation")
                            };
                            let _ = reply.send(Err(anyhow::anyhow!(
                                "original changed before catalog commit"
                            )));
                        }
                        TestPublication::LoseFileRelease => {
                            let grant = crate::catalog_session::LeaseId::new();
                            let Command::ValidateFile(reply) = command_receiver.recv()? else {
                                anyhow::bail!("expected file validation")
                            };
                            reply
                                .send(Ok(grant.clone()))
                                .map_err(|_| anyhow::anyhow!("validation receiver closed"))?;
                            let Command::ReleaseFile {
                                grant: actual,
                                reply,
                            } = command_receiver.recv()?
                            else {
                                anyhow::bail!("expected file release")
                            };
                            ensure!(actual == grant, "release grant changed");
                            let _ =
                                reply.send(Err(anyhow::anyhow!("lost release acknowledgement")));
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = run {
                    panic!("publication test worker failed: {error:#}");
                }
            })?;
        Ok(Self {
            session: catalog.session.clone(),
            receiver: Some(receiver),
            cancel,
            thread: Some(worker),
            commands: Some(command_sender),
            managed: true,
            cleanup: Arc::new(std::sync::Mutex::new(None)),
        })
    }
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
        if self.managed
            && self.thread.is_some()
            && let Err(error) = self.unit_command(Command::Retire)
        {
            self.request_cancel();
            self.join_thread()?;
            if let Err(cleanup) = self.retry_cleanup() {
                return Err(error).context(format!(
                    "managed import retirement cleanup retained: {cleanup:#}"
                ));
            }
            return Err(error);
        }
        self.receiver.take();
        self.commands.take();
        self.join_thread()?;
        self.retry_cleanup()
    }
    fn send_command(&self, command: Command) -> Result<()> {
        ensure!(
            self.managed,
            "managed import command used by legacy preparation"
        );
        self.commands
            .as_ref()
            .context("managed import commands stopped")?
            .send(command)
            .map_err(|_| anyhow::anyhow!("managed import owner stopped"))
    }
    fn unit_command(
        &self,
        command: impl FnOnce(mpsc::SyncSender<Result<()>>) -> Command,
    ) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(0);
        self.send_command(command(tx))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("managed import acknowledgement lost"))?
    }
    pub(crate) fn accept_root(&self, source: &NativePath) -> Result<()> {
        self.unit_command(|reply| Command::AcceptRoot {
            source: source.clone(),
            reply,
        })
    }
    pub(crate) fn validate_inspection(&self) -> Result<Option<crate::catalog_session::LeaseId>> {
        if self.managed {
            let (tx, rx) = mpsc::sync_channel(0);
            self.send_command(Command::ValidateInspection(tx))?;
            Ok(Some(rx.recv().map_err(|_| {
                anyhow::anyhow!("managed import validation acknowledgement lost")
            })??))
        } else {
            Ok(None)
        }
    }
    pub(crate) fn release_inspection(
        &self,
        grant: Option<crate::catalog_session::LeaseId>,
    ) -> Result<()> {
        match grant {
            Some(grant) => self.unit_command(|reply| Command::ReleaseInspection { grant, reply }),
            None => Ok(()),
        }
    }
    pub(crate) fn validate_file(&self) -> Result<Option<crate::catalog_session::LeaseId>> {
        if self.managed {
            let (tx, rx) = mpsc::sync_channel(0);
            self.send_command(Command::ValidateFile(tx))?;
            Ok(Some(rx.recv().map_err(|_| {
                anyhow::anyhow!("managed import validation acknowledgement lost")
            })??))
        } else {
            Ok(None)
        }
    }
    pub(crate) fn release_file(
        &self,
        grant: Option<crate::catalog_session::LeaseId>,
    ) -> Result<()> {
        match grant {
            Some(grant) => self.unit_command(|reply| Command::ReleaseFile { grant, reply }),
            None => Ok(()),
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
    storage: Option<VolumeLocation>,
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
        let Header {
            path,
            fingerprint,
            observation,
        } = header;
        catalog.require_jobs_released()?;
        let location = location_bytes(&path);
        crate::catalog_storage::verify_location_fence(&catalog.db, &location, Some(&fingerprint))?;
        ensure!(
            observation.requested_path == NativePath::from_path(&path),
            "volume observation path differs"
        );
        let identity = observation
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
                    || existing.as_ref().and_then(|v| v.1.as_ref()) == Some(&fingerprint),
                "relink-required: source volume identity changed; explicit relink review required"
            );
        }
        if let (Some(identity), Some(relative)) = (&identity, &observation.relative_in_volume) {
            let matches:Vec<Vec<u8>>=catalog.db.prepare("SELECT a.location FROM assets a JOIN storage_bindings b ON b.asset_id=a.id JOIN storage_volumes v ON v.id=b.volume_id WHERE v.identity=?1 AND b.relative=?2 LIMIT 2")?.query_map(params![identity,serde_json::to_string(relative)?],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
            ensure!(
                matches.len() < 2 && matches.iter().all(|p| p == &location),
                "relink-required: existing volume-relative original uses another path; explicit relink review required"
            );
        }
        if let Some((_, fp, state, revision)) = &existing {
            ensure!(
                *revision == 0 || (fp.as_ref() == Some(&fingerprint) && state == "ready"),
                "source-changed: content or availability changed on an edited image; explicit source review required; edits retained"
            );
        }
        let ready = existing
            .as_ref()
            .is_some_and(|(_, fp, state, _)| fp.as_ref() == Some(&fingerprint) && state == "ready");
        if !ready {
            catalog.reserve(&path, &location)?;
        }
        let asset: String =
            catalog
                .db
                .query_row("SELECT id FROM assets WHERE location=?", [&location], |r| {
                    r.get(0)
                })?;
        catalog.record_import_path(&path)?;
        let storage = (observation.state == LocationState::Available).then_some(observation);
        Ok(Self {
            asset,
            path,
            fingerprint,
            storage,
            previous_fingerprint: existing.and_then(|v| v.1),
            ready,
            seen: BTreeSet::new(),
            changed: false,
            warnings: 0,
        })
    }
    fn bind_storage_with(
        &mut self,
        bind: impl FnOnce(&str, &VolumeLocation) -> Result<()>,
    ) -> Result<()> {
        let Some(observation) = self.storage.as_ref() else {
            return Ok(());
        };
        bind(&self.asset, observation)?;
        self.storage = None;
        Ok(())
    }
    pub(crate) fn bind_storage(&mut self, catalog: &mut Catalog) -> Result<()> {
        self.bind_storage_with(|asset, observation| catalog.bind_storage(asset, observation))
    }
    pub(crate) fn source(
        &mut self,
        catalog: &mut Catalog,
        source: &PreparedImportSource,
        preparation: &Preparation,
    ) -> Result<Option<crate::catalog_session::LeaseId>> {
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
        }
        let grant = preparation.validate_inspection()?;
        let (changed, warning) = catalog_metadata::apply_import_source(&tx, &self.asset, source)?;
        persist_import_receipt(
            &tx,
            &self.asset,
            "metadata",
            grant.as_ref(),
            &source.source.locator,
        )?;
        tx.commit()?;
        if source.source.kind != "embedded" {
            self.seen.insert(source.source.locator.clone());
        }
        self.changed |= changed;
        self.warnings += u64::from(warning);
        Ok(grant)
    }
    pub(crate) fn finish(
        mut self,
        catalog: &mut Catalog,
        service: &mut PreviewService,
        preparation: &Preparation,
    ) -> Result<(
        Option<preview::Consumer>,
        bool,
        u64,
        Option<crate::catalog_session::LeaseId>,
    )> {
        let grant = {
            let _write = catalog
                .writers
                .enter(crate::catalog_writer::Priority::Background)?;
            let tx = catalog
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            self.check(&tx)?;
            let grant = preparation.validate_file()?;
            self.changed |= catalog_metadata::finish_import_sources(&tx, &self.asset, &self.seen)?;
            persist_import_receipt(
                &tx,
                &self.asset,
                "file",
                grant.as_ref(),
                &location_bytes(&self.path),
            )?;
            tx.commit()?;
            grant
        };
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
        Ok((consumer, self.changed, self.warnings, grant))
    }
    pub(crate) fn fail(&self, catalog: &mut Catalog, reason: &str) -> Result<()> {
        if !self.ready {
            self.check(&catalog.db)?;
            catalog.fail(&location_bytes(&self.path), &anyhow::anyhow!("{reason}"))?;
        }
        Ok(())
    }
}

fn persist_import_receipt(
    tx: &rusqlite::Transaction<'_>,
    asset: &str,
    phase: &str,
    grant: Option<&crate::catalog_session::LeaseId>,
    source_locator: &[u8],
) -> Result<()> {
    let Some(grant) = grant else { return Ok(()) };
    let revision = catalog_metadata::revision(tx, asset)?;
    tx.execute(
        "INSERT INTO metadata_history(asset_id,revision,action,detail) VALUES(?1,?2,'import_filesystem_receipt',?3)",
        params![
            asset,
            revision,
            serde_json::json!({
                "version": 1,
                "phase": phase,
                "grant": grant.as_str(),
                "source_locator_digest": blake3::hash(source_locator).to_hex().to_string(),
            })
            .to_string()
        ],
    )?;
    Ok(())
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
struct RemoteImport {
    route: RemoteImportRoute,
    root: crate::catalog_session::RootCapability,
    transfer: crate::catalog_session::LeaseId,
    step: u64,
    pending: Option<crate::catalog_session::import::Request>,
    active: bool,
}
#[cfg(test)]
type ImportRouteCallback = dyn Fn(
        &crate::catalog_session::import::Request,
        &AtomicBool,
    ) -> Result<Option<crate::catalog_session::import::Reply>>
    + Send
    + Sync;

enum RemoteImportRoute {
    Catalog(Arc<crate::catalog_session::CatalogSessionAuthority>),
    #[cfg(test)]
    Callback(Arc<ImportRouteCallback>),
}
impl RemoteImportRoute {
    fn call(
        &self,
        request: &crate::catalog_session::import::Request,
        cancel: &AtomicBool,
    ) -> Result<Option<crate::catalog_session::import::Reply>> {
        match self {
            Self::Catalog(session) => session.import_call(request, cancel),
            #[cfg(test)]
            Self::Callback(call) => call(request, cancel),
        }
    }
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
        let reply = match self.route.call(&request, cancel) {
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
        let reply = match self.route.call(&request, &AtomicBool::new(false)) {
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
    /// A committed C receipt may only be followed by exact replay of its release.
    /// Recover one lost reply here so callers never mistake an acknowledged
    /// catalog commit for a fresh, fallible filesystem validation.
    fn release(
        &mut self,
        action: crate::catalog_session::import::Action,
    ) -> Result<crate::catalog_session::import::Value> {
        match self.call(action, &AtomicBool::new(false)) {
            Ok(value) => Ok(value),
            Err(first) if self.pending.is_some() => match self.reconcile() {
                Ok(Some(crate::catalog_session::import::Value::Failed(failure))) => {
                    Err(anyhow::Error::new(failure))
                }
                Ok(Some(value)) => Ok(value),
                Ok(None) => Err(first),
                Err(error) => Err(error).context(format!(
                    "reconcile committed import release after lost reply: {first:#}"
                )),
            },
            Err(error) => Err(error),
        }
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
            crate::catalog_session::import::Value::Begun { .. } => self.active = true,
            crate::catalog_session::import::Value::Finished
            | crate::catalog_session::import::Value::Aborted => self.active = false,
            _ => {}
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "explicit SQL admission and retained cleanup ownership cross the preparation thread boundary"
)]
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
            route: RemoteImportRoute::Catalog(session),
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
        let source = match remote.call(
            crate::catalog_session::import::Action::Begin {
                source: NativePath::from_path(source_root),
            },
            cancel,
        )? {
            crate::catalog_session::import::Value::Begun { source } => source,
            _ => anyhow::bail!("managed import begin reply mismatch"),
        };
        // Rendezvous delivery blocks F before its first walk step until C has
        // received the exact canonical root returned by F. The separate command
        // acknowledgement below is sent only after C's manifest transaction.
        send(
            sender,
            cancel,
            Event::Begun {
                source: source.clone(),
            },
        )?;
        let Command::AcceptRoot {
            source: accepted,
            reply,
        } = commands
            .recv()
            .map_err(|_| anyhow::anyhow!("managed root acknowledgement channel closed"))?
        else {
            anyhow::bail!("managed root acknowledgement was not first")
        };
        ensure!(accepted == source, "managed root acknowledgement changed");
        reply
            .send(Ok(()))
            .map_err(|_| anyhow::anyhow!("managed root acknowledgement receiver closed"))?;
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
                        let Command::ValidateInspection(reply) = commands.recv().map_err(|_| {
                            anyhow::anyhow!("managed import inspection validation channel closed")
                        })?
                        else {
                            anyhow::bail!("managed import command ordering mismatch")
                        };
                        let result = remote
                            .call(
                                crate::catalog_session::import::Action::ValidateInspection,
                                &AtomicBool::new(false),
                            )
                            .and_then(|value| {
                                let crate::catalog_session::import::Value::InspectionValidated {
                                    grant,
                                } = value
                                else {
                                    anyhow::bail!("managed inspection validation reply mismatch")
                                };
                                Ok(grant)
                            });
                        let failed = result.is_err();
                        let _ = reply.send(result);
                        ensure!(!failed, "managed inspection validation failed");
                        let Command::ReleaseInspection { grant, reply } =
                            commands.recv().map_err(|_| {
                                anyhow::anyhow!("managed import inspection release channel closed")
                            })?
                        else {
                            anyhow::bail!("managed import command ordering mismatch")
                        };
                        let result = remote
                            .release(crate::catalog_session::import::Action::ReleaseInspection {
                                grant: grant.clone(),
                            })
                            .and_then(|value| {
                                ensure!(
                                    matches!(
                                        value,
                                        crate::catalog_session::import::Value::InspectionReleased {
                                            grant: actual
                                        } if actual == grant
                                    ),
                                    "managed inspection release reply mismatch"
                                );
                                Ok(())
                            });
                        let failed = result.is_err();
                        let _ = reply.send(result);
                        ensure!(!failed, "managed inspection release failed");
                    }
                    send(sender, cancel, Event::End)?;
                    let Command::ValidateFile(reply) = commands
                        .recv()
                        .map_err(|_| anyhow::anyhow!("managed import validation channel closed"))?
                    else {
                        anyhow::bail!("managed import command ordering mismatch")
                    };
                    let result = remote
                        .call(
                            crate::catalog_session::import::Action::ValidateFile,
                            &AtomicBool::new(false),
                        )
                        .and_then(|value| {
                            let crate::catalog_session::import::Value::FileValidated { grant } =
                                value
                            else {
                                anyhow::bail!("managed import validation reply mismatch")
                            };
                            Ok(grant)
                        });
                    let failed = result.is_err();
                    let _ = reply.send(result);
                    ensure!(!failed, "managed import validation failed");
                    let Command::ReleaseFile { grant, reply } = commands
                        .recv()
                        .map_err(|_| anyhow::anyhow!("managed import release channel closed"))?
                    else {
                        anyhow::bail!("managed import command ordering mismatch")
                    };
                    let result = remote
                        .release(crate::catalog_session::import::Action::ReleaseFile {
                            grant: grant.clone(),
                        })
                        .and_then(|value| {
                            ensure!(
                                matches!(
                                    value,
                                    crate::catalog_session::import::Value::FileReleased {
                                        grant: actual
                                    } if actual == grant
                                ),
                                "managed import release reply mismatch"
                            );
                            Ok(())
                        });
                    let failed = result.is_err();
                    let _ = reply.send(result);
                    ensure!(!failed, "managed import release failed");
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

    #[test]
    fn remote_release_replays_unknown_acknowledgements_against_real_f_owner() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let catalog = temp.path().join("catalog");
        let originals = temp.path().join("originals");
        fs::create_dir(&catalog)?;
        fs::create_dir(&originals)?;
        let catalog = catalog.canonicalize()?;
        let originals = originals.canonicalize()?;
        let original = originals.join("image.jpg");
        fs::write(&original, b"original")?;
        let catalog_file = File::create(catalog.join("catalog.sqlite3"))?;
        let catalog_directory = crate::filesystem_worker::open_directory(&catalog)?;
        let root = crate::catalog_session::RootCapability {
            epoch: crate::catalog_session::LeaseId::new(),
            token: crate::catalog_session::LeaseId::new(),
            session: crate::catalog_session::LeaseId::new(),
            canonical_root: NativePath::from_path(&catalog),
            root_physical: crate::catalog_storage::physical_object_id(&catalog_directory)?,
            catalog_physical: crate::catalog_storage::physical_object_id(&catalog_file)?,
        };
        let owner = Arc::new(std::sync::Mutex::new(
            crate::filesystem_worker::import_test_support::Owner::default(),
        ));
        let lost = Arc::new(std::sync::Mutex::new(BTreeSet::from([
            "inspection",
            "file",
        ])));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let route = {
            let owner = owner.clone();
            let lost = lost.clone();
            let requests = requests.clone();
            let catalog = catalog.clone();
            let original_roots = vec![NativePath::from_path(&originals)];
            RemoteImportRoute::Callback(Arc::new(move |request, cancel| {
                requests.lock().unwrap().push(request.clone());
                let reply =
                    owner
                        .lock()
                        .unwrap()
                        .call(&catalog, &original_roots, request, cancel)?;
                let release = match &request.action {
                    crate::catalog_session::import::Action::ReleaseInspection { .. } => {
                        Some("inspection")
                    }
                    crate::catalog_session::import::Action::ReleaseFile { .. } => Some("file"),
                    _ => None,
                };
                if release.is_some_and(|release| lost.lock().unwrap().remove(release)) {
                    return Err(crate::filesystem_worker::wire::Failure::new(
                        crate::filesystem_worker::wire::FailureKind::Unknown,
                        "injected lost successful release acknowledgement",
                    )
                    .into());
                }
                Ok(Some(reply))
            }))
        };
        let mut remote = RemoteImport {
            route,
            root,
            transfer: crate::catalog_session::LeaseId::new(),
            step: 0,
            pending: None,
            active: false,
        };
        assert!(matches!(
            remote.call(
                crate::catalog_session::import::Action::Begin {
                    source: NativePath::from_path(&originals),
                },
                &AtomicBool::new(false),
            )?,
            crate::catalog_session::import::Value::Begun { source }
                if source == NativePath::from_path(&originals)
        ));
        loop {
            match remote.call(
                crate::catalog_session::import::Action::Next,
                &AtomicBool::new(false),
            )? {
                crate::catalog_session::import::Value::Header { path, .. } => {
                    ensure!(
                        path == NativePath::from_path(&original),
                        "unexpected original"
                    );
                    break;
                }
                crate::catalog_session::import::Value::DirectoryStart { .. }
                | crate::catalog_session::import::Value::DirectoryFacts { .. }
                | crate::catalog_session::import::Value::DirectoryEnd { .. } => {}
                value => anyhow::bail!("unexpected pre-header import value: {value:?}"),
            }
        }
        let missing = originals.join("image.xmp");
        assert!(matches!(
            remote.call(
                crate::catalog_session::import::Action::Inspect {
                    source: Source {
                        kind: "sidecar".into(),
                        locator: location_bytes(&missing),
                        display: missing.to_string_lossy().into_owned(),
                        ambiguous: false,
                        provenance: serde_json::json!({"fixture":"lost-release"}),
                    },
                },
                &AtomicBool::new(false),
            )?,
            crate::catalog_session::import::Value::InspectionFailed { .. }
        ));
        let inspection_grant = match remote.call(
            crate::catalog_session::import::Action::ValidateInspection,
            &AtomicBool::new(false),
        )? {
            crate::catalog_session::import::Value::InspectionValidated { grant } => grant,
            value => anyhow::bail!("unexpected inspection validation value: {value:?}"),
        };
        assert!(matches!(
            remote.release(
                crate::catalog_session::import::Action::ReleaseInspection {
                    grant: inspection_grant.clone(),
                }
            )?,
            crate::catalog_session::import::Value::InspectionReleased { grant }
                if grant == inspection_grant
        ));
        let file_grant = match remote.call(
            crate::catalog_session::import::Action::ValidateFile,
            &AtomicBool::new(false),
        )? {
            crate::catalog_session::import::Value::FileValidated { grant } => grant,
            value => anyhow::bail!("unexpected file validation value: {value:?}"),
        };
        assert!(matches!(
            remote.release(crate::catalog_session::import::Action::ReleaseFile {
                grant: file_grant.clone(),
            })?,
            crate::catalog_session::import::Value::FileReleased { grant } if grant == file_grant
        ));
        remote.abort()?;
        ensure!(
            owner.lock().unwrap().empty(),
            "F import owner did not retire"
        );
        ensure!(
            remote.pending.is_none() && !remote.active,
            "remote custody remained active"
        );
        ensure!(
            lost.lock().unwrap().is_empty(),
            "release loss was not exercised"
        );
        let requests = requests.lock().unwrap();
        for release in ["inspection", "file"] {
            let matching: Vec<_> = requests
                .iter()
                .filter(|request| {
                    matches!(
                        (&request.action, release),
                        (
                            crate::catalog_session::import::Action::ReleaseInspection { .. },
                            "inspection"
                        ) | (
                            crate::catalog_session::import::Action::ReleaseFile { .. },
                            "file"
                        )
                    )
                })
                .collect();
            ensure!(matching.len() == 2, "release was not replayed exactly once");
            ensure!(
                matching[0].step == matching[1].step
                    && matching[0].digest()? == matching[1].digest()?,
                "release replay changed the retained request"
            );
        }
        Ok(())
    }

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
    fn storage_busy_retry_preserves_one_reservation_and_binds_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        fs::create_dir(&originals)?;
        let path = originals.canonicalize()?.join("one.png");
        image::RgbImage::from_pixel(24, 16, image::Rgb([20u8, 40, 70])).save(&path)?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        let mut reference = Reference::begin(
            &mut catalog,
            Header {
                path: path.clone(),
                fingerprint: crate::fingerprint(&path)?,
                observation: observation(&path, Path::new("one.png")),
            },
        )?;
        let (asset, generation): (String, i64) = catalog.db.query_row(
            "SELECT id,render_generation FROM assets WHERE location=?1",
            [location_bytes(&path)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let binding = |catalog: &Catalog| -> Result<(Option<String>, Option<String>)> {
            Ok(catalog.db.query_row(
                "SELECT volume_id,file_key FROM storage_bindings WHERE asset_id=?1",
                [&asset],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        };
        assert_eq!(generation, 1);
        assert_eq!(binding(&catalog)?, (None, None));

        let mut attempts = 0;
        let error = reference
            .bind_storage_with(|_, _| {
                attempts += 1;
                Err(crate::preview::stage_io::Busy("injected storage admission").into())
            })
            .unwrap_err();
        assert!(error.is::<crate::preview::stage_io::Busy>());
        assert_eq!(attempts, 1);
        assert_eq!(binding(&catalog)?, (None, None));
        assert_eq!(
            catalog.db.query_row(
                "SELECT render_generation FROM assets WHERE id=?1",
                [&asset],
                |row| row.get::<_, i64>(0),
            )?,
            generation
        );

        reference.bind_storage(&mut catalog)?;
        let bound = binding(&catalog)?;
        assert!(bound.0.is_some() && bound.1.is_some());
        assert_eq!(
            catalog.db.query_row(
                "SELECT render_generation FROM assets WHERE id=?1",
                [&asset],
                |row| row.get::<_, i64>(0),
            )?,
            generation
        );
        reference.bind_storage_with(|_, _| panic!("completed binding retried"))?;
        assert_eq!(binding(&catalog)?, bound);
        Ok(())
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
