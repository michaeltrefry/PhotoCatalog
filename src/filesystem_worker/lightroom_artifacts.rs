use super::wire::{LightroomArtifactPreparation, LightroomArtifactPreparationReply};
use crate::{
    application::U64,
    catalog_migration::{
        artifacts::{ArtifactLimits, prepare_manifest_artifact},
        importer::ArtifactInput,
    },
    catalog_storage::physical_object_id,
    lightroom::capture::Manifest,
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

struct Snapshot {
    session: String,
    directory: NativePath,
    manifest_path: NativePath,
    manifest_physical: crate::catalog_session::PhysicalObjectId,
    capture_revision: String,
    manifest_blake3: String,
    bytes: Vec<u8>,
    manifest: Manifest,
    limits: ArtifactLimits,
    prepared: BTreeMap<usize, String>,
}
struct Receipt {
    input_json: String,
    input_blake3: String,
}

#[derive(Default)]
pub(super) struct Owner {
    active: Option<Snapshot>,
    discarded: Option<String>,
    receipts: BTreeMap<String, Receipt>,
    // A bounded retry window makes explicit discard idempotent without growing
    // custody metadata for the lifetime of F.
    discarded_receipts: VecDeque<String>,
}

fn canceled(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "artifact preparation canceled"
    );
    Ok(())
}

impl Owner {
    pub(super) fn execute(
        &mut self,
        request: LightroomArtifactPreparation,
        cancel: &AtomicBool,
    ) -> Result<Option<LightroomArtifactPreparationReply>> {
        match request {
            LightroomArtifactPreparation::Begin {
                session,
                directory,
                capture_revision,
                manifest_blake3,
                maximum_bytes,
                open_deadline_ms,
            } => {
                canceled(cancel)?;
                if let Some(active) = &self.active {
                    ensure!(
                        active.session == session
                            && active.directory == directory
                            && active.capture_revision == capture_revision
                            && active.manifest_blake3 == manifest_blake3
                            && active.limits.maximum_bytes == maximum_bytes.0
                            && active.limits.open_deadline_ms == open_deadline_ms.0,
                        "another artifact preparation is retained; discard it explicitly"
                    );
                    return Ok(Some(active.begun()));
                }
                ensure!(
                    self.discarded.as_deref() != Some(&session),
                    "artifact preparation session was already discarded"
                );
                crate::catalog_session::validate_path(&directory)?;
                let requested = directory.to_path()?;
                let entry =
                    fs::symlink_metadata(&requested).context("inspect capture directory")?;
                ensure!(
                    entry.file_type().is_dir() && !entry.file_type().is_symlink(),
                    "capture path is not a direct directory"
                );
                let canonical =
                    fs::canonicalize(&requested).context("resolve capture directory")?;
                let path = canonical.join("manifest.json");
                let entry = fs::symlink_metadata(&path).context("inspect capture manifest")?;
                ensure!(
                    entry.file_type().is_file()
                        && !entry.file_type().is_symlink()
                        && entry.len() > 0
                        && entry.len() <= crate::lightroom::MANIFEST_BYTES as u64,
                    "capture manifest is not a bounded direct regular file"
                );
                let mut file = fs::File::open(&path).context("open capture manifest")?;
                let physical = physical_object_id(&file)?;
                let before = file.metadata()?;
                let mut bytes = Vec::with_capacity(usize::try_from(before.len())?);
                let mut block = [0; super::wire::CHUNK_BYTES];
                loop {
                    canceled(cancel)?;
                    let n = file.read(&mut block)?;
                    if n == 0 {
                        break;
                    }
                    ensure!(
                        bytes
                            .len()
                            .checked_add(n)
                            .is_some_and(|v| v <= crate::lightroom::MANIFEST_BYTES),
                        "capture manifest byte limit"
                    );
                    bytes.extend_from_slice(&block[..n]);
                }
                let after = file.metadata()?;
                ensure!(
                    after.len() == before.len()
                        && after.modified().ok() == before.modified().ok()
                        && physical_object_id(&file)? == physical
                        && bytes.len() == usize::try_from(before.len())?,
                    "capture manifest changed during read"
                );
                ensure!(
                    blake3::hash(&bytes).to_hex().as_str() == manifest_blake3,
                    "capture manifest differs from selected review"
                );
                let manifest: Manifest = serde_json::from_slice(&bytes)?;
                ensure!(
                    manifest.protocol == 1
                        && manifest.state == "captured"
                        && manifest.revision_id.as_deref() == Some(capture_revision.as_str())
                        && crate::lightroom::json_digest(&manifest.artifacts)? == capture_revision
                        && !manifest.artifacts.is_empty()
                        && manifest.artifacts.len() <= 4_096,
                    "capture manifest revision or artifact roster differs"
                );
                for artifact in &manifest.artifacts {
                    let relative = Path::new(&artifact.stored);
                    ensure!(
                        !relative.is_absolute()
                            && relative
                                .components()
                                .all(|c| matches!(c, std::path::Component::Normal(_)))
                            && relative.starts_with("raw"),
                        "capture stored artifact path is not a safe relative path"
                    );
                }
                canceled(cancel)?;
                let snapshot = Snapshot {
                    session,
                    directory: NativePath::from_path(&canonical),
                    manifest_path: NativePath::from_path(&path),
                    manifest_physical: physical,
                    capture_revision,
                    manifest_blake3,
                    bytes,
                    manifest,
                    limits: ArtifactLimits {
                        maximum_bytes: maximum_bytes.0,
                        open_deadline_ms: open_deadline_ms.0,
                        chunk_deadline_ms: 120_000,
                        chunk_bytes: 1024 * 1024,
                    },
                    prepared: BTreeMap::new(),
                };
                let reply = snapshot.begun();
                self.active = Some(snapshot);
                Ok(Some(reply))
            }
            LightroomArtifactPreparation::Member {
                session,
                member_index,
            } => {
                canceled(cancel)?;
                let index = usize::try_from(member_index.0)?;
                let input = {
                    let active = self
                        .active
                        .as_ref()
                        .context("no artifact preparation is retained")?;
                    ensure!(
                        active.session == session,
                        "artifact preparation session differs"
                    );
                    if let Some(receipt) = active.prepared.get(&index) {
                        return Ok(Some(LightroomArtifactPreparationReply::Prepared {
                            session,
                            member_index: U64(index as u64),
                            receipt: receipt.clone(),
                        }));
                    }
                    let artifact = active
                        .manifest
                        .artifacts
                        .get(index)
                        .context("artifact member absent")?;
                    let relative = NativePath::from_path(Path::new(&artifact.stored));
                    let mapping = prepare_manifest_artifact(
                        artifact,
                        &active.directory,
                        &relative,
                        active.limits,
                        &|| cancel.load(Ordering::Acquire),
                    )?;
                    ArtifactInput {
                        capture_revision: active.capture_revision.clone(),
                        member_index: index,
                        mapping,
                    }
                };
                let bytes = crate::lightroom::bounded_json(&input, 65_536)?;
                ensure!(
                    self.receipts.len() < 4_096,
                    "prepared artifact receipt capacity"
                );
                let receipt = uuid::Uuid::new_v4().to_string();
                self.receipts.insert(
                    receipt.clone(),
                    Receipt {
                        input_blake3: blake3::hash(&bytes).to_hex().to_string(),
                        input_json: String::from_utf8(bytes)?,
                    },
                );
                self.active
                    .as_mut()
                    .unwrap()
                    .prepared
                    .insert(index, receipt.clone());
                Ok(Some(LightroomArtifactPreparationReply::Prepared {
                    session,
                    member_index: U64(index as u64),
                    receipt,
                }))
            }
            LightroomArtifactPreparation::Resolve { receipt } => {
                canceled(cancel)?;
                let value = self
                    .receipts
                    .get(&receipt)
                    .context("prepared artifact receipt is absent or discarded")?;
                Ok(Some(LightroomArtifactPreparationReply::Resolved {
                    receipt,
                    input_json: value.input_json.clone(),
                    input_blake3: value.input_blake3.clone(),
                }))
            }
            LightroomArtifactPreparation::DiscardReceipt { receipt } => {
                if self.receipts.remove(&receipt).is_some() {
                    if let Some(active) = &mut self.active {
                        active.prepared.retain(|_, value| value != &receipt);
                    }
                    if self.discarded_receipts.len() == 4_096 {
                        self.discarded_receipts.pop_front();
                    }
                    self.discarded_receipts.push_back(receipt);
                } else {
                    ensure!(
                        self.discarded_receipts.contains(&receipt),
                        "prepared artifact receipt is absent"
                    );
                }
                Ok(None)
            }
            LightroomArtifactPreparation::Discard { session } => {
                if let Some(active) = &self.active {
                    ensure!(
                        active.session == session,
                        "artifact preparation session differs"
                    );
                    self.active = None;
                    self.discarded = Some(session);
                } else {
                    ensure!(
                        self.discarded.as_deref() == Some(&session),
                        "artifact preparation is not retained"
                    );
                }
                Ok(None)
            }
        }
    }
}
impl Snapshot {
    fn begun(&self) -> LightroomArtifactPreparationReply {
        LightroomArtifactPreparationReply::Begun {
            session: self.session.clone(),
            directory: self.directory.clone(),
            manifest_path: self.manifest_path.clone(),
            manifest_physical: self.manifest_physical,
            capture_revision: self.capture_revision.clone(),
            manifest_blake3: self.manifest_blake3.clone(),
            manifest_bytes: U64(self.bytes.len() as u64),
            members: U64(self.manifest.artifacts.len() as u64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_receipt_replays_then_discard_is_idempotent() {
        let receipt = uuid::Uuid::new_v4().to_string();
        let input_json = r#"{"capture_revision":"fixture"}"#.to_string();
        let input_blake3 = blake3::hash(input_json.as_bytes()).to_hex().to_string();
        let mut owner = Owner::default();
        owner.receipts.insert(
            receipt.clone(),
            Receipt {
                input_json: input_json.clone(),
                input_blake3: input_blake3.clone(),
            },
        );
        let cancel = AtomicBool::new(false);
        for _ in 0..2 {
            let reply = owner
                .execute(
                    LightroomArtifactPreparation::Resolve {
                        receipt: receipt.clone(),
                    },
                    &cancel,
                )
                .unwrap();
            assert!(
                matches!(reply, Some(LightroomArtifactPreparationReply::Resolved { input_json: ref json, input_blake3: ref digest, .. }) if json == &input_json && digest == &input_blake3)
            );
        }
        for _ in 0..2 {
            assert!(
                owner
                    .execute(
                        LightroomArtifactPreparation::DiscardReceipt {
                            receipt: receipt.clone(),
                        },
                        &cancel,
                    )
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            owner
                .execute(LightroomArtifactPreparation::Resolve { receipt }, &cancel)
                .is_err(),
            "discarded receipt was resolved"
        );
    }

    #[test]
    fn altered_receipt_never_resolves() {
        let receipt = uuid::Uuid::new_v4().to_string();
        let mut owner = Owner::default();
        owner.receipts.insert(
            receipt.clone(),
            Receipt {
                input_json: "{}".into(),
                input_blake3: blake3::hash(b"{}").to_hex().to_string(),
            },
        );
        let altered = uuid::Uuid::new_v4().to_string();
        assert_ne!(receipt, altered);
        assert!(
            owner
                .execute(
                    LightroomArtifactPreparation::Resolve { receipt: altered },
                    &AtomicBool::new(false)
                )
                .is_err()
        );
    }
}
