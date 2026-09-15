use super::wire::{
    CHUNK_BYTES, LightroomSealedDocument, LightroomSealedDocumentPage, LightroomSealedRead,
};
use crate::{application::U64, catalog_storage::physical_object_id, storage_volume::NativePath};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
};

struct Snapshot {
    session: String,
    directory: NativePath,
    path: NativePath,
    document: LightroomSealedDocument,
    physical: crate::catalog_session::PhysicalObjectId,
    blake3: String,
    bytes: Vec<u8>,
}

#[derive(Default)]
pub(super) struct Owner {
    active: Option<Snapshot>,
    discarded: Option<String>,
}

fn canceled(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "sealed document read canceled"
    );
    Ok(())
}

impl Owner {
    pub(super) fn execute(
        &mut self,
        request: LightroomSealedRead,
        cancel: &AtomicBool,
    ) -> Result<Option<LightroomSealedDocumentPage>> {
        match request {
            LightroomSealedRead::Begin {
                session,
                directory,
                document,
            } => {
                canceled(cancel)?;
                if let Some(active) = &self.active {
                    ensure!(
                        active.session == session
                            && active.directory == directory
                            && active.document == document,
                        "another sealed document read is retained; discard it explicitly"
                    );
                    return Ok(Some(Self::page(active, 0, 0)?));
                }
                ensure!(
                    self.discarded.as_deref() != Some(&session),
                    "sealed document read session was already discarded"
                );
                crate::catalog_session::validate_path(&directory)?;
                let requested = directory.to_path()?;
                let selected = fs::symlink_metadata(&requested)
                    .context("inspect sealed selection directory")?;
                ensure!(
                    selected.file_type().is_dir() && !selected.file_type().is_symlink(),
                    "sealed selection path is not a direct directory"
                );
                let canonical_directory =
                    fs::canonicalize(&requested).context("resolve sealed selection directory")?;
                let name = match document {
                    LightroomSealedDocument::Seal => "input-seal.json",
                    LightroomSealedDocument::Approval => "approval.json",
                };
                let requested_file = canonical_directory.join(name);
                let entry =
                    fs::symlink_metadata(&requested_file).context("inspect sealed document")?;
                ensure!(
                    entry.file_type().is_file() && !entry.file_type().is_symlink(),
                    "sealed document is not a direct regular file"
                );
                ensure!(
                    entry.len() > 0 && entry.len() <= crate::lightroom::MANIFEST_BYTES as u64,
                    "sealed document byte limit"
                );
                let mut file = fs::File::open(&requested_file).context("open sealed document")?;
                let physical = physical_object_id(&file)?;
                let before = file.metadata()?;
                ensure!(
                    before.is_file() && before.len() == entry.len(),
                    "sealed document identity changed before read"
                );
                let mut bytes = Vec::with_capacity(usize::try_from(before.len())?);
                let mut block = [0u8; CHUNK_BYTES];
                loop {
                    canceled(cancel)?;
                    let read = file.read(&mut block)?;
                    if read == 0 {
                        break;
                    }
                    ensure!(
                        bytes
                            .len()
                            .checked_add(read)
                            .is_some_and(|n| n <= crate::lightroom::MANIFEST_BYTES),
                        "sealed document byte limit"
                    );
                    bytes.extend_from_slice(&block[..read]);
                }
                let after = file.metadata()?;
                ensure!(
                    after.len() == before.len()
                        && after.modified().ok() == before.modified().ok()
                        && physical_object_id(&file)? == physical,
                    "sealed document changed during read"
                );
                ensure!(
                    bytes.len() == usize::try_from(before.len())?,
                    "sealed document short read"
                );
                canceled(cancel)?;
                let snapshot = Snapshot {
                    session,
                    directory: NativePath::from_path(&canonical_directory),
                    path: NativePath::from_path(&requested_file),
                    document,
                    physical,
                    blake3: blake3::hash(&bytes).to_hex().to_string(),
                    bytes,
                };
                let page = Self::page(&snapshot, 0, 0)?;
                self.active = Some(snapshot);
                Ok(Some(page))
            }
            LightroomSealedRead::Page {
                session,
                offset,
                limit,
            } => {
                canceled(cancel)?;
                let active = self
                    .active
                    .as_ref()
                    .context("no sealed document read is retained")?;
                ensure!(
                    active.session == session,
                    "sealed document read session differs"
                );
                Ok(Some(Self::page(
                    active,
                    usize::try_from(offset.0)?,
                    usize::try_from(limit.0)?,
                )?))
            }
            LightroomSealedRead::Discard { session } => {
                if let Some(active) = &self.active {
                    ensure!(
                        active.session == session,
                        "sealed document read session differs"
                    );
                    self.active = None;
                    self.discarded = Some(session);
                } else {
                    ensure!(
                        self.discarded.as_deref() == Some(&session),
                        "sealed document read is not retained"
                    );
                }
                Ok(None)
            }
        }
    }

    fn page(
        snapshot: &Snapshot,
        offset: usize,
        limit: usize,
    ) -> Result<LightroomSealedDocumentPage> {
        ensure!(
            offset <= snapshot.bytes.len(),
            "sealed document page offset"
        );
        let limit = if limit == 0 {
            0
        } else {
            ensure!(limit <= CHUNK_BYTES, "sealed document page limit");
            limit
        };
        let end = offset
            .checked_add(limit)
            .context("sealed document page overflow")?
            .min(snapshot.bytes.len());
        Ok(LightroomSealedDocumentPage {
            session: snapshot.session.clone(),
            directory: snapshot.directory.clone(),
            path: snapshot.path.clone(),
            document: snapshot.document,
            physical: snapshot.physical.clone(),
            total_bytes: U64(snapshot.bytes.len() as u64),
            blake3: snapshot.blake3.clone(),
            offset: U64(offset as u64),
            next: (end < snapshot.bytes.len()).then_some(U64(end as u64)),
            bytes: snapshot.bytes[offset..end].to_vec(),
        })
    }
}
