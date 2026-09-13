//! One owned source-I/O preparer. It never opens the destination catalog or renders.
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
    receiver: Option<mpsc::Receiver<Event>>,
    cancel: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Preparation {
    pub(crate) fn spawn(
        catalog: &Catalog,
        source: &Path,
        cancel: Arc<AtomicBool>,
        #[cfg(test)] checkpoint: Option<Checkpoint>,
    ) -> Result<Self> {
        crate::catalog_backup::require_jobs_released(&catalog.root)?;
        ensure!(source.is_dir(), "import source must be a directory");
        ensure!(
            !source.starts_with(&catalog.root) && !catalog.root.starts_with(source),
            "catalog and originals must be separate directories"
        );
        let (sender, receiver) = mpsc::sync_channel(0);
        let source = source.to_path_buf();
        let stop = cancel.clone();
        let worker = thread::Builder::new()
            .name("catalog-source-preparation".into())
            .spawn(move || {
                let result = prepare_walk(
                    &source,
                    &sender,
                    &stop,
                    #[cfg(test)]
                    checkpoint,
                );
                if let Err(e) = result
                    && !stop.load(Ordering::Acquire)
                {
                    let _ = sender.send(Event::Failed {
                        source: NativePath::from_path(&source),
                        message: format!("{e:#}").chars().take(2048).collect(),
                    });
                }
            })?;
        Ok(Self {
            receiver: Some(receiver),
            cancel,
            thread: Some(worker),
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
    pub(crate) fn finish(mut self) {
        self.receiver.take();
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
    pub(crate) fn stop(&mut self) {
        if self.thread.is_some() {
            self.cancel.store(true, Ordering::Release);
        }
        self.receiver.take(); // Unblock a rendezvous send before joining.
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Preparation {
    fn drop(&mut self) {
        self.stop();
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
    root: &Path,
    sender: &mpsc::SyncSender<Event>,
    cancel: &AtomicBool,
    #[cfg(test)] checkpoint: Option<Checkpoint>,
) -> Result<()> {
    let discovery = catalog_metadata::ImportDiscovery::new()?;
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
        crate::catalog_backup::require_jobs_released(&catalog.root)?;
        let location = location_bytes(&header.path);
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
}
