//! Stateful import walk and source custody owned entirely by F.
use crate::{
    application::U64,
    catalog_session::{LeaseId, import as protocol},
    import_storage::ImportVolumes,
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions, ReadDir},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::UNIX_EPOCH,
};

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    physical: crate::catalog_session::PhysicalObjectId,
    length: u64,
    modified_ns: u128,
}
fn stamp(file: &File) -> Result<Stamp> {
    let metadata = file.metadata()?;
    Ok(Stamp {
        physical: crate::catalog_storage::physical_object_id(file)?,
        length: metadata.len(),
        modified_ns: metadata.modified()?.duration_since(UNIX_EPOCH)?.as_nanos(),
    })
}
fn open_regular(path: &Path) -> Result<(File, Stamp)> {
    let metadata = fs::symlink_metadata(path)?;
    let file = crate::xmp_packets::open_regular(path, &metadata)?;
    let stamp = stamp(&file)?;
    Ok((file, stamp))
}
fn recheck(path: &Path, held: &File, expected: &Stamp) -> Result<()> {
    let (_current, current_stamp) = open_regular(path)?;
    ensure!(
        stamp(held)? == *expected && current_stamp == *expected,
        "source changed before catalog commit"
    );
    Ok(())
}
fn canceled(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "managed import canceled");
    Ok(())
}
fn failure_detail(error: impl std::fmt::Display) -> String {
    let mut value = format!("inspection failed: {error}");
    let mut end = value.len().min(2048);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

struct Current {
    path: PathBuf,
    file: File,
    stamp: Stamp,
    directory_path: PathBuf,
    directory_guard: File,
    directory_identity: DirectoryIdentity,
}
struct Inspection {
    path: PathBuf,
    guard: Option<(File, Stamp)>,
    encoded: Vec<u8>,
    checksum: String,
    offset: usize,
    delivered: bool,
}
struct Directory {
    path: PathBuf,
    guard: File,
    identity: DirectoryIdentity,
    entries: ReadDir,
    deferred: Option<protocol::DirectoryFact>,
    count: usize,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DirectoryIdentity {
    Unix(u64, u64, Option<u64>, [u8; 32]),
    Windows(u64, u64, [u8; 32]),
}
fn directory_identity(path: &Path, file: &File) -> Result<DirectoryIdentity> {
    let native = NativePath::from_path(path);
    let mut hasher = blake3::Hasher::new();
    match &native {
        NativePath::UnixBytes(bytes) => {
            hasher.update(b"unix");
            hasher.update(bytes);
        }
        NativePath::WindowsWide(units) => {
            hasher.update(b"windows");
            for unit in units {
                hasher.update(&unit.to_le_bytes());
            }
        }
    }
    let path_digest = *hasher.finalize().as_bytes();
    Ok(match crate::catalog_storage::physical_object_id(file)? {
        crate::catalog_session::PhysicalObjectId::Unix { device, inode } => {
            DirectoryIdentity::Unix(
                device.0,
                inode.0,
                crate::import_storage::mount_instance(path),
                path_digest,
            )
        }
        crate::catalog_session::PhysicalObjectId::Windows {
            volume_serial,
            file_index,
        } => DirectoryIdentity::Windows(volume_serial.0, file_index.0, path_digest),
    })
}
fn recheck_directory(path: &Path, held: &File, expected: DirectoryIdentity) -> Result<()> {
    let current = super::bootstrap::open_directory(path)?;
    ensure!(
        directory_identity(path, held)? == expected
            && directory_identity(path, &current)? == expected,
        "import source directory changed"
    );
    Ok(())
}
struct Active {
    transfer: LeaseId,
    source: PathBuf,
    source_directory: File,
    source_identity: DirectoryIdentity,
    lock: File,
    walk: walkdir::IntoIter,
    seen_directories: BTreeSet<DirectoryIdentity>,
    pending: Option<PathBuf>,
    pending_directory: Option<(PathBuf, File, DirectoryIdentity)>,
    directory: Option<Directory>,
    current: Option<Current>,
    inspection: Option<Inspection>,
    volumes: ImportVolumes,
    walked: u64,
    walk_finished: bool,
    next_step: u64,
    last: Option<(u64, String, protocol::Reply)>,
}
#[derive(Clone)]
struct Terminal {
    transfer: LeaseId,
    last: protocol::Reply,
}
#[derive(Default)]
pub(super) struct Owner {
    active: Option<Active>,
    terminal: Option<Terminal>,
}
pub(crate) fn owner_layout() -> (usize, usize) {
    (std::mem::size_of::<Owner>(), std::mem::align_of::<Owner>())
}
impl Owner {
    pub fn empty(&self) -> bool {
        self.active.is_none()
    }

    pub fn call(
        &mut self,
        catalog_root: &Path,
        original_roots: &[NativePath],
        request: &protocol::Request,
        cancel: &AtomicBool,
    ) -> Result<protocol::Reply> {
        request.validate()?;
        let digest = request.digest()?;
        if let Some(active) = &self.active
            && let Some((step, previous, reply)) = &active.last
            && *step == request.step.0
        {
            ensure!(previous == &digest, "altered duplicate import request");
            return Ok(reply.clone());
        }
        if let Some(terminal) = &self.terminal
            && terminal.transfer == request.transfer
            && terminal.last.step == request.step
        {
            ensure!(
                terminal.last.request_digest == digest,
                "altered duplicate terminal import request"
            );
            return Ok(terminal.last.clone());
        }

        let value = match &request.action {
            protocol::Action::Begin { source } => {
                let result = (|| -> Result<protocol::Value> {
                    ensure!(
                        request.step.0 == 0 && self.active.is_none(),
                        "import owner already active"
                    );
                    canceled(cancel)?;
                    let source = source.to_path()?.canonicalize()?;
                    ensure!(source.is_dir(), "import source must be a directory");
                    ensure!(
                        !source.starts_with(catalog_root) && !catalog_root.starts_with(&source),
                        "catalog and originals must be separate directories"
                    );
                    let admitted = original_roots.iter().any(|root| {
                        root.to_path()
                            .ok()
                            .and_then(|p| p.canonicalize().ok())
                            .is_some_and(|root| source.starts_with(root))
                    });
                    ensure!(admitted, "import source is outside admitted original roots");
                    let source_directory = super::bootstrap::open_directory(&source)?;
                    let source_identity = directory_identity(&source, &source_directory)?;
                    let lock = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(catalog_root.join("import.lock"))?;
                    lock.try_lock_exclusive()
                        .context("another import owns this catalog")?;
                    self.terminal = None;
                    self.active = Some(Active {
                        transfer: request.transfer.clone(),
                        source: source.clone(),
                        source_directory,
                        source_identity,
                        lock,
                        walk: walkdir::WalkDir::new(&source)
                            .follow_links(false)
                            .max_open(16)
                            .into_iter(),
                        seen_directories: BTreeSet::new(),
                        pending: None,
                        pending_directory: None,
                        directory: None,
                        current: None,
                        inspection: None,
                        volumes: ImportVolumes::new(),
                        walked: 0,
                        walk_finished: false,
                        next_step: 0,
                        last: None,
                    });
                    Ok(protocol::Value::Begun)
                })();
                match result {
                    Ok(value) => value,
                    Err(error) => {
                        protocol::Value::Failed(crate::filesystem_worker::wire::Failure::new(
                            if cancel.load(Ordering::Acquire) {
                                crate::filesystem_worker::wire::FailureKind::Canceled
                            } else {
                                crate::filesystem_worker::wire::FailureKind::Rejected
                            },
                            error,
                        ))
                    }
                }
            }
            action => {
                let active = self
                    .active
                    .as_mut()
                    .context("managed import custody is not retained")?;
                ensure!(
                    active.next_step == request.step.0
                        && active.last.as_ref().is_none_or(|v| v.0 != request.step.0),
                    "managed import step mismatch"
                );
                ensure!(
                    request.transfer == active.transfer,
                    "managed import transfer mismatch"
                );
                let result = (|| -> Result<protocol::Value> {
                    if !matches!(action, protocol::Action::Abort) {
                        recheck_directory(
                            &active.source,
                            &active.source_directory,
                            active.source_identity,
                        )?;
                    }
                    Ok(match action {
                        protocol::Action::Next => next(active, cancel)?,
                        protocol::Action::Inspect { source } => inspect(active, source, cancel)?,
                        protocol::Action::Read { offset } => read(active, offset.0, cancel)?,
                        protocol::Action::FinishInspection => finish_inspection(active, cancel)?,
                        protocol::Action::CommitInspection => commit_inspection(active)?,
                        protocol::Action::Recheck => {
                            let current = active
                                .current
                                .as_ref()
                                .context("no current import source")?;
                            recheck(&current.path, &current.file, &current.stamp)?;
                            recheck_directory(
                                &current.directory_path,
                                &current.directory_guard,
                                current.directory_identity,
                            )?;
                            protocol::Value::Rechecked
                        }
                        protocol::Action::CommitFile => {
                            ensure!(
                                active.inspection.is_none(),
                                "metadata inspection remains uncommitted"
                            );
                            let current = active
                                .current
                                .as_ref()
                                .context("no current import source")?;
                            recheck(&current.path, &current.file, &current.stamp)?;
                            recheck_directory(
                                &current.directory_path,
                                &current.directory_guard,
                                current.directory_identity,
                            )?;
                            active.current = None;
                            protocol::Value::FileCommitted
                        }
                        protocol::Action::Finish => {
                            ensure!(
                                active.walk_finished
                                    && active.pending.is_none()
                                    && active.pending_directory.is_none()
                                    && active.directory.is_none()
                                    && active.current.is_none()
                                    && active.inspection.is_none(),
                                "import cannot finish with retained work"
                            );
                            protocol::Value::Finished
                        }
                        protocol::Action::Abort => protocol::Value::Aborted,
                        protocol::Action::Begin { .. } => unreachable!(),
                    })
                })();
                match result {
                    Ok(value) => value,
                    Err(error) => {
                        protocol::Value::Failed(crate::filesystem_worker::wire::Failure::new(
                            if cancel.load(Ordering::Acquire) {
                                crate::filesystem_worker::wire::FailureKind::Canceled
                            } else {
                                crate::filesystem_worker::wire::FailureKind::Rejected
                            },
                            error,
                        ))
                    }
                }
            }
        };
        let mut reply = protocol::Reply {
            root: request.root.clone(),
            transfer: request.transfer.clone(),
            step: request.step,
            request_digest: digest.clone(),
            value,
        };
        if let Err(error) = reply.validate(request) {
            reply.value = protocol::Value::Failed(crate::filesystem_worker::wire::Failure::new(
                crate::filesystem_worker::wire::FailureKind::ResourceLimit,
                error,
            ));
            reply.validate(request)?;
        }
        let terminal = matches!(
            (&request.action, &reply.value),
            (protocol::Action::Finish, protocol::Value::Finished)
                | (protocol::Action::Abort, protocol::Value::Aborted)
        );
        let failed_without_custody = matches!(
            (&request.action, &reply.value),
            (protocol::Action::Begin { .. }, protocol::Value::Failed(_))
        ) && self.active.is_none();
        let rejected_while_active = matches!(
            (&request.action, &reply.value),
            (protocol::Action::Begin { .. }, protocol::Value::Failed(_))
        ) && self.active.is_some();
        if failed_without_custody {
            self.terminal = Some(Terminal {
                transfer: request.transfer.clone(),
                last: reply.clone(),
            });
        } else if rejected_while_active {
            // A second transfer cannot advance or replace the retained owner's
            // sequence. The rejection itself performs no owner-state mutation.
        } else if terminal {
            fs2::FileExt::unlock(&self.active.as_ref().expect("active import").lock)
                .context("release managed import lock")?;
            let _active = self.active.take().expect("active import");
            self.terminal = Some(Terminal {
                transfer: request.transfer.clone(),
                last: reply.clone(),
            });
        } else if let Some(active) = &mut self.active {
            active.next_step = active
                .next_step
                .checked_add(1)
                .context("import step exhausted")?;
            active.last = Some((request.step.0, digest, reply.clone()));
        }
        Ok(reply)
    }
}

fn next(active: &mut Active, cancel: &AtomicBool) -> Result<protocol::Value> {
    canceled(cancel)?;
    ensure!(
        active.current.is_none() && active.inspection.is_none(),
        "commit current import source before advancing"
    );
    if let Some(directory) = &mut active.directory {
        let mut facts = Vec::with_capacity(protocol::DIRECTORY_FACTS);
        let mut path_units = 0usize;
        while facts.len() < protocol::DIRECTORY_FACTS {
            canceled(cancel)?;
            let fact = match directory.deferred.take() {
                Some(fact) => Some(fact),
                None => match directory.entries.next() {
                    Some(entry) => {
                        let entry = entry?;
                        directory.count = directory
                            .count
                            .checked_add(1)
                            .context("directory entry count overflow")?;
                        ensure!(
                            directory.count <= crate::xmp_packets::Limits::default().max_entries,
                            "directory metadata association entry limit exceeded; no truncated associations admitted"
                        );
                        Some(protocol::DirectoryFact {
                            path: NativePath::from_path(&entry.path()),
                            regular: entry.file_type()?.is_file(),
                        })
                    }
                    None => None,
                },
            };
            let Some(fact) = fact else { break };
            let units = match &fact.path {
                NativePath::UnixBytes(value) => value.len(),
                NativePath::WindowsWide(value) => value.len(),
            };
            if !facts.is_empty()
                && path_units
                    .checked_add(units)
                    .is_none_or(|total| total > protocol::DIRECTORY_FACT_PATH_UNITS)
            {
                directory.deferred = Some(fact);
                break;
            }
            path_units = path_units
                .checked_add(units)
                .context("directory fact path units overflow")?;
            facts.push(fact);
        }
        if !facts.is_empty() {
            return Ok(protocol::Value::DirectoryFacts {
                directory: NativePath::from_path(&directory.path),
                facts,
            });
        }
        recheck_directory(&directory.path, &directory.guard, directory.identity)?;
        let directory = active.directory.take().unwrap();
        active.pending_directory =
            Some((directory.path.clone(), directory.guard, directory.identity));
        return Ok(protocol::Value::DirectoryEnd {
            directory: NativePath::from_path(&directory.path),
        });
    }
    if let Some(path) = active.pending.take() {
        return header(active, path, cancel);
    }
    ensure!(!active.walk_finished, "import walk already finished");
    loop {
        canceled(cancel)?;
        let Some(entry) = active.walk.next() else {
            active.walk_finished = true;
            return Ok(protocol::Value::WalkFinished);
        };
        active.walked = active
            .walked
            .checked_add(1)
            .context("import walk count overflow")?;
        ensure!(
            active.walked <= protocol::MAX_WALK_ENTRIES,
            "import walk entry limit exceeded"
        );
        let entry = entry.context("discover source folder")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !crate::media::supported_extension(&extension) {
            return Ok(protocol::Value::Skipped);
        }
        let directory = path
            .parent()
            .context("original has no parent")?
            .to_path_buf();
        let guard = super::bootstrap::open_directory(&directory)?;
        let identity = directory_identity(&directory, &guard)?;
        let key = NativePath::from_path(&directory);
        if active.seen_directories.insert(identity) {
            ensure!(
                active.seen_directories.len() <= protocol::MAX_DIRECTORIES,
                "import directory limit exceeded"
            );
            active.pending = Some(path);
            active.directory = Some(Directory {
                path: directory.clone(),
                guard,
                identity,
                entries: fs::read_dir(&directory)?,
                deferred: None,
                count: 0,
            });
            return Ok(protocol::Value::DirectoryStart { directory: key });
        }
        active.pending_directory = Some((directory, guard, identity));
        return header(active, path, cancel);
    }
}

fn header(active: &mut Active, path: PathBuf, cancel: &AtomicBool) -> Result<protocol::Value> {
    canceled(cancel)?;
    let parent = path.parent().context("original has no parent")?;
    let (directory_path, directory_guard, directory_identity) =
        match active.pending_directory.take() {
            Some(value) => value,
            None => {
                let guard = super::bootstrap::open_directory(parent)?;
                let identity = directory_identity(parent, &guard)?;
                (parent.to_path_buf(), guard, identity)
            }
        };
    ensure!(
        directory_path == parent,
        "import directory custody belongs to another source"
    );
    recheck_directory(&directory_path, &directory_guard, directory_identity)?;
    let (mut file, before) = open_regular(&path)?;
    ensure!(
        before.length <= crate::xmp_packets::Limits::default().max_source_bytes,
        "import source byte limit"
    );
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        canceled(cancel)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .context("import source length overflow")?;
        ensure!(total <= before.length, "source grew during preparation");
        hasher.update(&buffer[..count]);
    }
    ensure!(total == before.length, "source shrank during preparation");
    recheck(&path, &file, &before)?;
    let fingerprint = hasher.finalize().to_hex().to_string();
    let observation = active.volumes.observe(&path)?;
    recheck_directory(&directory_path, &directory_guard, directory_identity)?;
    active.current = Some(Current {
        path: path.clone(),
        file,
        stamp: before,
        directory_path,
        directory_guard,
        directory_identity,
    });
    Ok(protocol::Value::Header {
        path: NativePath::from_path(&path),
        fingerprint,
        observation,
    })
}

fn inspect(
    active: &mut Active,
    source: &crate::catalog_metadata::Source,
    cancel: &AtomicBool,
) -> Result<protocol::Value> {
    canceled(cancel)?;
    ensure!(
        active.inspection.is_none(),
        "previous metadata inspection remains retained"
    );
    let current = active
        .current
        .as_ref()
        .context("metadata inspection has no current original")?;
    let path = protocol::source_path(source)?;
    recheck_directory(
        &current.directory_path,
        &current.directory_guard,
        current.directory_identity,
    )?;
    if source.kind == "embedded" {
        ensure!(
            path == current.path,
            "embedded metadata source differs from current original"
        );
    } else {
        ensure!(
            path.parent() == current.path.parent()
                && path
                    .extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.eq_ignore_ascii_case("xmp")),
            "sidecar is outside current original directory or is not XMP"
        );
    }
    let guarded = open_regular(&path);
    if source.kind == "embedded" {
        let current = active.current.as_ref().expect("current import source");
        recheck(&current.path, &current.file, &current.stamp)?;
    }
    let (guard, stamp) = match guarded {
        Ok(value) => value,
        Err(error) if source.kind == "sidecar" && !cancel.load(Ordering::Acquire) => {
            let message = failure_detail(error);
            active.inspection = Some(Inspection {
                path,
                guard: None,
                encoded: Vec::new(),
                checksum: String::new(),
                offset: 0,
                delivered: true,
            });
            return Ok(protocol::Value::InspectionFailed {
                source: source.clone(),
                message,
            });
        }
        Err(error) => return Err(error),
    };
    let inspection = match crate::xmp_packets::inspect_cancellable(
        &path,
        &crate::xmp_packets::Limits::default(),
        source.kind == "sidecar",
        cancel,
    ) {
        Ok(inspection) => inspection,
        Err(error) if source.kind == "sidecar" && !cancel.load(Ordering::Acquire) => {
            let message = failure_detail(error);
            active.inspection = Some(Inspection {
                path,
                guard: Some((guard, stamp)),
                encoded: Vec::new(),
                checksum: String::new(),
                offset: 0,
                delivered: true,
            });
            return Ok(protocol::Value::InspectionFailed {
                source: source.clone(),
                message,
            });
        }
        Err(error) => return Err(error),
    };
    recheck(&path, &guard, &stamp)?;
    let encoded = protocol::encode_inspection(inspection)?;
    let checksum = blake3::hash(&encoded).to_hex().to_string();
    let bytes = encoded.len() as u64;
    active.inspection = Some(Inspection {
        path,
        guard: Some((guard, stamp)),
        encoded,
        checksum: checksum.clone(),
        offset: 0,
        delivered: false,
    });
    Ok(protocol::Value::Inspection {
        source: source.clone(),
        bytes: U64(bytes),
        checksum,
    })
}
fn read(active: &mut Active, offset: u64, cancel: &AtomicBool) -> Result<protocol::Value> {
    canceled(cancel)?;
    let inspection = active
        .inspection
        .as_mut()
        .context("metadata inspection is not retained")?;
    ensure!(
        !inspection.delivered
            && offset == inspection.offset as u64
            && inspection.offset < inspection.encoded.len(),
        "metadata inspection read offset mismatch"
    );
    let end = inspection
        .encoded
        .len()
        .min(inspection.offset + protocol::CHUNK_BYTES);
    let bytes = inspection.encoded[inspection.offset..end].to_vec();
    let start = inspection.offset;
    inspection.offset = end;
    Ok(protocol::Value::Chunk {
        offset: U64(start as u64),
        checksum: blake3::hash(&bytes).to_hex().to_string(),
        bytes,
    })
}
fn finish_inspection(active: &mut Active, cancel: &AtomicBool) -> Result<protocol::Value> {
    canceled(cancel)?;
    let inspection = active
        .inspection
        .as_mut()
        .context("metadata inspection is not retained")?;
    ensure!(
        inspection.offset == inspection.encoded.len()
            && blake3::hash(&inspection.encoded).to_hex().as_str() == inspection.checksum,
        "metadata inspection transfer incomplete"
    );
    let (guard, stamp) = inspection
        .guard
        .as_ref()
        .context("successful inspection lost its source guard")?;
    recheck(&inspection.path, guard, stamp)?;
    inspection.delivered = true;
    Ok(protocol::Value::InspectionFinished)
}
fn commit_inspection(active: &mut Active) -> Result<protocol::Value> {
    let inspection = active
        .inspection
        .as_ref()
        .context("metadata inspection is not retained")?;
    ensure!(
        inspection.delivered,
        "metadata inspection was not delivered"
    );
    if let Some((guard, stamp)) = &inspection.guard {
        recheck(&inspection.path, guard, stamp)?;
    } else {
        ensure!(
            open_regular(&inspection.path).is_err(),
            "unavailable sidecar became readable before catalog acknowledgement"
        );
    }
    active.inspection = None;
    Ok(protocol::Value::InspectionCommitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_session::RootCapability;
    use std::sync::atomic::AtomicBool;

    struct Fixture {
        _temp: tempfile::TempDir,
        catalog: PathBuf,
        originals: PathBuf,
        root: RootCapability,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let temp = tempfile::tempdir()?;
            let catalog = temp.path().join("catalog");
            let originals = temp.path().join("originals");
            fs::create_dir(&catalog)?;
            fs::create_dir(&originals)?;
            let database = catalog.join("catalog.sqlite3");
            fs::write(&database, b"fixture")?;
            let root_file = super::super::bootstrap::open_directory(&catalog)?;
            let database_file = File::open(&database)?;
            let root = RootCapability {
                epoch: LeaseId::new(),
                token: LeaseId::new(),
                session: LeaseId::new(),
                canonical_root: NativePath::from_path(&catalog),
                root_physical: crate::catalog_storage::physical_object_id(&root_file)?,
                catalog_physical: crate::catalog_storage::physical_object_id(&database_file)?,
            };
            Ok(Self {
                _temp: temp,
                catalog,
                originals,
                root,
            })
        }
        fn request(
            &self,
            transfer: &LeaseId,
            step: u64,
            action: protocol::Action,
        ) -> protocol::Request {
            protocol::Request {
                root: self.root.clone(),
                transfer: transfer.clone(),
                step: U64(step),
                action,
            }
        }
        fn originals(&self) -> Vec<NativePath> {
            vec![NativePath::from_path(&self.originals)]
        }
    }

    fn call(
        owner: &mut Owner,
        fixture: &Fixture,
        transfer: &LeaseId,
        step: &mut u64,
        action: protocol::Action,
    ) -> Result<protocol::Value> {
        let request = fixture.request(transfer, *step, action);
        let reply = owner.call(
            &fixture.catalog,
            &fixture.originals(),
            &request,
            &AtomicBool::new(false),
        )?;
        reply.validate(&request)?;
        *step += 1;
        Ok(reply.value)
    }

    fn begin(
        owner: &mut Owner,
        fixture: &Fixture,
        transfer: &LeaseId,
        step: &mut u64,
    ) -> Result<()> {
        ensure!(matches!(
            call(
                owner,
                fixture,
                transfer,
                step,
                protocol::Action::Begin {
                    source: NativePath::from_path(&fixture.originals)
                },
            )?,
            protocol::Value::Begun
        ));
        Ok(())
    }

    fn next_header(
        owner: &mut Owner,
        fixture: &Fixture,
        transfer: &LeaseId,
        step: &mut u64,
    ) -> Result<PathBuf> {
        loop {
            match call(owner, fixture, transfer, step, protocol::Action::Next)? {
                protocol::Value::Header { path, .. } => return Ok(path.to_path()?),
                protocol::Value::DirectoryStart { .. }
                | protocol::Value::DirectoryFacts { .. }
                | protocol::Value::DirectoryEnd { .. }
                | protocol::Value::Skipped => {}
                value => anyhow::bail!("header unavailable: {value:?}"),
            }
        }
    }

    #[test]
    fn malformed_begin_has_no_filesystem_effect_and_actual_lock_is_exclusive() -> Result<()> {
        let fixture = Fixture::new()?;
        let mut owner = Owner::default();
        let transfer = LeaseId::new();
        let malformed = fixture.request(
            &transfer,
            0,
            protocol::Action::Begin {
                source: NativePath::from_path(Path::new("relative")),
            },
        );
        assert!(
            owner
                .call(
                    &fixture.catalog,
                    &fixture.originals(),
                    &malformed,
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        assert!(!fixture.catalog.join("import.lock").exists());

        let mut step = 0;
        begin(&mut owner, &fixture, &transfer, &mut step)?;
        let mut alias = Owner::default();
        let alias_transfer = LeaseId::new();
        let request = fixture.request(
            &alias_transfer,
            0,
            protocol::Action::Begin {
                source: NativePath::from_path(&fixture.originals),
            },
        );
        assert!(matches!(
            alias
                .call(
                    &fixture.catalog,
                    &fixture.originals(),
                    &request,
                    &AtomicBool::new(false)
                )?
                .value,
            protocol::Value::Failed(_)
        ));
        let intruder = fixture.request(
            &LeaseId::new(),
            0,
            protocol::Action::Begin {
                source: NativePath::from_path(&fixture.originals),
            },
        );
        assert!(matches!(
            owner
                .call(
                    &fixture.catalog,
                    &fixture.originals(),
                    &intruder,
                    &AtomicBool::new(false)
                )?
                .value,
            protocol::Value::Failed(_)
        ));
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::Abort
            )?,
            protocol::Value::Aborted
        ));
        assert!(matches!(
            alias
                .call(
                    &fixture.catalog,
                    &fixture.originals(),
                    &request,
                    &AtomicBool::new(false)
                )?
                .value,
            protocol::Value::Failed(_)
        ));
        let alias_transfer = LeaseId::new();
        let mut alias_step = 0;
        begin(&mut alias, &fixture, &alias_transfer, &mut alias_step)?;
        ensure!(matches!(
            call(
                &mut alias,
                &fixture,
                &alias_transfer,
                &mut alias_step,
                protocol::Action::Abort
            )?,
            protocol::Value::Aborted
        ));
        Ok(())
    }

    #[test]
    fn packet_chunks_are_bounded_replayable_and_failed_steps_do_not_mutate_offset() -> Result<()> {
        let fixture = Fixture::new()?;
        let original = fixture.originals.join("image.jpg");
        let sidecar = fixture.originals.join("image.xmp");
        fs::write(&original, b"not a decoded image")?;
        fs::write(&sidecar, vec![b'x'; protocol::CHUNK_BYTES + 4096])?;
        let mut owner = Owner::default();
        let transfer = LeaseId::new();
        let mut step = 0;
        begin(&mut owner, &fixture, &transfer, &mut step)?;
        assert_eq!(
            next_header(&mut owner, &fixture, &transfer, &mut step)?,
            original
        );
        let source = crate::catalog_metadata::Source {
            kind: "sidecar".into(),
            locator: crate::location_bytes(&sidecar),
            display: sidecar.to_string_lossy().into_owned(),
            ambiguous: false,
            provenance: serde_json::json!({"fixture":true}),
        };
        let length = match call(
            &mut owner,
            &fixture,
            &transfer,
            &mut step,
            protocol::Action::Inspect { source },
        )? {
            protocol::Value::Inspection { bytes, .. } => usize::try_from(bytes.0)?,
            value => anyhow::bail!("unexpected inspection: {value:?}"),
        };
        assert!(length > protocol::CHUNK_BYTES);
        let request = fixture.request(&transfer, step, protocol::Action::Read { offset: U64(0) });
        let first = owner.call(
            &fixture.catalog,
            &fixture.originals(),
            &request,
            &AtomicBool::new(false),
        )?;
        let replay = owner.call(
            &fixture.catalog,
            &fixture.originals(),
            &request,
            &AtomicBool::new(false),
        )?;
        assert_eq!(first.request_digest, replay.request_digest);
        let first_bytes = match first.value {
            protocol::Value::Chunk { bytes, .. } => bytes,
            _ => anyhow::bail!("missing first chunk"),
        };
        assert_eq!(first_bytes.len(), protocol::CHUNK_BYTES);
        step += 1;
        let premature = fixture.request(&transfer, step, protocol::Action::FinishInspection);
        assert!(matches!(
            owner
                .call(
                    &fixture.catalog,
                    &fixture.originals(),
                    &premature,
                    &AtomicBool::new(false)
                )?
                .value,
            protocol::Value::Failed(_)
        ));
        step += 1;
        let canceled = AtomicBool::new(true);
        let next = fixture.request(
            &transfer,
            step,
            protocol::Action::Read {
                offset: U64(protocol::CHUNK_BYTES as u64),
            },
        );
        assert!(matches!(
            owner
                .call(&fixture.catalog, &fixture.originals(), &next, &canceled)?
                .value,
            protocol::Value::Failed(_)
        ));
        step += 1;
        let next = fixture.request(
            &transfer,
            step,
            protocol::Action::Read {
                offset: U64(protocol::CHUNK_BYTES as u64),
            },
        );
        let reply = owner.call(
            &fixture.catalog,
            &fixture.originals(),
            &next,
            &AtomicBool::new(false),
        )?;
        assert!(matches!(reply.value, protocol::Value::Chunk { .. }));
        step += 1;
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::Abort
            )?,
            protocol::Value::Aborted
        ));
        Ok(())
    }

    #[test]
    fn source_change_before_commit_is_rejected_and_abort_releases_custody() -> Result<()> {
        let fixture = Fixture::new()?;
        let original = fixture.originals.join("image.jpg");
        fs::write(&original, b"original revision")?;
        let mut owner = Owner::default();
        let transfer = LeaseId::new();
        let mut step = 0;
        begin(&mut owner, &fixture, &transfer, &mut step)?;
        assert_eq!(
            next_header(&mut owner, &fixture, &transfer, &mut step)?,
            original
        );
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::Recheck
            )?,
            protocol::Value::Rechecked
        ));
        fs::write(&original, b"changed revision")?;
        let commit = fixture.request(&transfer, step, protocol::Action::CommitFile);
        assert!(matches!(
            owner
                .call(
                    &fixture.catalog,
                    &fixture.originals(),
                    &commit,
                    &AtomicBool::new(false)
                )?
                .value,
            protocol::Value::Failed(_)
        ));
        step += 1;
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::Abort
            )?,
            protocol::Value::Aborted
        ));
        assert!(owner.empty());
        Ok(())
    }

    #[test]
    fn source_root_alias_replacement_is_rejected_but_abort_still_retires_lock() -> Result<()> {
        let fixture = Fixture::new()?;
        fs::write(fixture.originals.join("image.jpg"), b"original")?;
        let mut owner = Owner::default();
        let transfer = LeaseId::new();
        let mut step = 0;
        begin(&mut owner, &fixture, &transfer, &mut step)?;
        let retained = fixture.originals.with_extension("retained");
        fs::rename(&fixture.originals, &retained)?;
        fs::create_dir(&fixture.originals)?;
        fs::write(fixture.originals.join("image.jpg"), b"replacement")?;
        let request = fixture.request(&transfer, step, protocol::Action::Next);
        assert!(matches!(
            owner
                .call(
                    &fixture.catalog,
                    &fixture.originals(),
                    &request,
                    &AtomicBool::new(false)
                )?
                .value,
            protocol::Value::Failed(_)
        ));
        step += 1;
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::Abort
            )?,
            protocol::Value::Aborted
        ));
        assert!(owner.empty());
        Ok(())
    }

    #[test]
    fn unavailable_sidecar_is_a_committable_observation_and_outside_path_is_rejected() -> Result<()>
    {
        let fixture = Fixture::new()?;
        let original = fixture.originals.join("image.jpg");
        fs::write(&original, b"original")?;
        let mut owner = Owner::default();
        let transfer = LeaseId::new();
        let mut step = 0;
        begin(&mut owner, &fixture, &transfer, &mut step)?;
        next_header(&mut owner, &fixture, &transfer, &mut step)?;
        let outside = fixture.catalog.join("outside.xmp");
        fs::write(&outside, b"outside")?;
        let outside_source = crate::catalog_metadata::Source {
            kind: "sidecar".into(),
            locator: crate::location_bytes(&outside),
            display: outside.to_string_lossy().into_owned(),
            ambiguous: false,
            provenance: serde_json::json!({}),
        };
        let malformed = fixture.request(
            &transfer,
            step,
            protocol::Action::Inspect {
                source: outside_source,
            },
        );
        assert!(matches!(
            owner
                .call(
                    &fixture.catalog,
                    &fixture.originals(),
                    &malformed,
                    &AtomicBool::new(false)
                )?
                .value,
            protocol::Value::Failed(_)
        ));
        step += 1;
        let missing = fixture.originals.join("missing.xmp");
        let missing_source = crate::catalog_metadata::Source {
            kind: "sidecar".into(),
            locator: crate::location_bytes(&missing),
            display: missing.to_string_lossy().into_owned(),
            ambiguous: false,
            provenance: serde_json::json!({"discovered":true}),
        };
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::Inspect {
                    source: missing_source
                },
            )?,
            protocol::Value::InspectionFailed { .. }
        ));
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::CommitInspection
            )?,
            protocol::Value::InspectionCommitted
        ));
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::Abort
            )?,
            protocol::Value::Aborted
        ));
        Ok(())
    }

    #[test]
    fn unsupported_files_are_counted_without_opening_an_import_source() -> Result<()> {
        let fixture = Fixture::new()?;
        fs::write(fixture.originals.join("unsupported.txt"), b"not imported")?;
        let mut owner = Owner::default();
        let transfer = LeaseId::new();
        let mut step = 0;
        begin(&mut owner, &fixture, &transfer, &mut step)?;
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::Next
            )?,
            protocol::Value::Skipped
        ));
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::Next
            )?,
            protocol::Value::WalkFinished
        ));
        ensure!(matches!(
            call(
                &mut owner,
                &fixture,
                &transfer,
                &mut step,
                protocol::Action::Finish
            )?,
            protocol::Value::Finished
        ));
        Ok(())
    }
}
