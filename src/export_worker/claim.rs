//! Durable, bounded Discard claims. Original/inventory-visible names are never
//! selected by deletion. After rename, physical proof is verified inside an
//! F-created 0700 container before any artifact is removed. The closed private
//! namespace is exclusive to F: malicious same-UID mutation of its control files
//! or claimed internals is outside this contract (POSIX has no inode-conditional
//! unlink). Ordinary substitutions at the original name remain protected.
use super::*;
const PREFIX: &str = ".f-";
pub(crate) const RECORD_BYTES: usize = 8192;
type Key = (u64, u128);
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    version: u32,
    original: String,
    claim: String,
    parent: Key,
    token: crate::catalog_session::LeaseId,
    executor: crate::catalog_session::LeaseId,
    operation: u64,
    request_digest: String,
    attempt: crate::catalog_session::export_executor::Attempt,
    directory: Key,
    active: Key,
    request: Key,
    digest: [u8; 32],
    files: [Option<Key>; 8],
    moved: bool,
    pub(super) removing: u16,
    pub(super) removing_directory: bool,
}
impl Record {
    fn validate(&self, wrapper: &Path) -> Result<()> {
        ensure!(
            self.version == 1 && self.removing < 256,
            "unknown Discard claim version/progress"
        );
        ensure!(
            self.moved || (self.removing == 0 && !self.removing_directory),
            "unclaimed removal intent"
        );
        ensure!(
            !self.removing_directory
                || self
                    .files
                    .iter()
                    .enumerate()
                    .all(|(index, key)| key.is_none() || self.removing & (1 << index) != 0),
            "incomplete claim directory removal intent"
        );
        let mut gap = false;
        for index in [3, 4, 5, 6, 7, 0, 2, 1] {
            let started = self.removing & (1 << index) != 0;
            if self.files[index].is_none() {
                ensure!(!started, "claim intent for absent artifact");
                continue;
            }
            ensure!(!started || !gap, "claim removal intents are not contiguous");
            gap |= !started;
        }
        self.attempt.validate()?;
        ensure!(
            self.operation > 1
                && self.request_digest.len() == 64
                && self
                    .request_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "claim Discard authority bound"
        );
        ensure!(
            self.original.len() <= 128
                && retired_name(Path::new(&self.original))
                && Path::new(&self.original).components().count() == 1,
            "invalid claim source name"
        );
        ensure!(
            wrapper.file_name().and_then(|name| name.to_str()) == Some(self.claim.as_str())
                && is_claim(wrapper),
            "claim control name mismatch"
        );
        ensure!(
            self.files[1] == Some(self.active) && self.files[2] == Some(self.request),
            "claim proof identity mismatch"
        );
        ensure!(
            lease_identity(&crate::filesystem_worker::open_directory(
                wrapper.parent().context("claim parent")?
            )?)? == self.parent,
            "claim namespace identity changed"
        );
        Ok(())
    }
    pub(super) fn save(&self, wrapper: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(bytes.len() <= RECORD_BYTES, "claim control bound");
        // This fixed scratch name is inside the F-exclusive container. A crash
        // leaves either the prior committed record or an incomplete scratch;
        // target mutation never begins before the committed intent is synced.
        let scratch = wrapper.join("claim.next");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&scratch)?;
        ensure!(file.metadata()?.is_file(), "claim scratch type");
        let half = bytes.len() / 2;
        file.write_all(&bytes[..half])?;
        #[cfg(test)]
        compact_discard_checkpoint("claim-scratch", wrapper)?;
        file.write_all(&bytes[half..])?;
        file.sync_all()?;
        drop(file);
        fs::rename(&scratch, wrapper.join("claim.json"))?;
        sync(wrapper)
    }
    fn candidate(&self, path: PathBuf) -> CompactRetiredExportTransport {
        CompactRetiredExportTransport {
            token: self.token.clone(),
            staging: path,
            directory_identity: self.directory,
            active_identity: self.active,
            request_identity: self.request,
            request_digest: self.digest,
            attempt: self.attempt.clone(),
        }
    }
}
fn sync(path: &Path) -> Result<()> {
    #[cfg(unix)]
    crate::filesystem_worker::open_directory(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
pub(super) fn is_claim(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(PREFIX))
        .is_some_and(|name| uuid::Uuid::parse_str(name).is_ok_and(|id| id.to_string() == name))
}
fn inspect(wrapper: &Path) -> Result<(Option<Record>, bool)> {
    ensure!(
        fs::symlink_metadata(wrapper)?.file_type().is_dir(),
        "claim container type"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(wrapper)?;
        ensure!(
            metadata.mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
            "claim container is not private to F user"
        );
    }
    let mut count = 0;
    for entry in fs::read_dir(wrapper)?.take(4) {
        let entry = entry?;
        count += 1;
        let name = entry.file_name();
        ensure!(
            count <= 3 && (name == "transport" || name == "claim.json" || name == "claim.next"),
            "unknown claim artifact"
        );
        ensure!(
            if name == "transport" {
                entry.file_type()?.is_dir()
            } else {
                entry.file_type()?.is_file()
            },
            "claim artifact type"
        );
    }
    let target = wrapper.join("transport").try_exists()?;
    let record = match fs::symlink_metadata(wrapper.join("claim.json")) {
        Ok(_) => {
            let bytes = read(&wrapper.join("claim.json"), RECORD_BYTES as u64)?;
            let record: Record = serde_json::from_slice(&bytes)?;
            record.validate(wrapper)?;
            Some(record)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        record.is_some() || !target,
        "claimed transport has no durable provenance"
    );
    Ok((record, target))
}
/// Exactly one empty wrapper may coexist with its original transport while
/// preparing a serialized claim, or while removing its completed control. It is
/// bounded control metadata, not a second transport allowance.
pub(super) fn empty_control(wrapper: &Path) -> Result<bool> {
    let (record, target) = inspect(wrapper)?;
    if target {
        return Ok(false);
    }
    if let Some(record) = record {
        ensure!(
            !record.moved || record.removing_directory,
            "claimed transport disappeared without removal intent"
        );
    }
    Ok(true)
}

pub(crate) struct CompactDiscard {
    wrapper: PathBuf,
    wrapper_file: Option<File>,
    parent: File,
    record: Record,
    inner: Option<ClaimedCleanup>,
    created: bool,
    complete: bool,
}
impl CompactDiscard {
    pub(crate) fn begin(
        retired: &CompactRetiredExportTransport,
        request: &crate::catalog_session::export_executor::Request,
    ) -> Result<Self> {
        request.validate()?;
        ensure!(
            matches!(&request.action, crate::catalog_session::export_executor::Action::Discard { token } if token == &retired.token),
            "claim Discard authority mismatch"
        );
        Self::prepare(
            retired,
            request.executor.clone(),
            request.operation.0,
            request.digest()?,
        )
    }
    #[cfg(test)]
    pub(crate) fn test_begin(retired: &CompactRetiredExportTransport) -> Result<Self> {
        Self::prepare(
            retired,
            crate::catalog_session::LeaseId::new(),
            3,
            blake3::hash(b"synthetic authorized Discard")
                .to_hex()
                .to_string(),
        )
    }
    fn prepare(
        retired: &CompactRetiredExportTransport,
        executor: crate::catalog_session::LeaseId,
        operation: u64,
        request_digest: String,
    ) -> Result<Self> {
        let inner = ClaimedCleanup::begin(retired)?;
        let name = format!("{PREFIX}{}", uuid::Uuid::new_v4());
        let parent_path = retired.staging.parent().context("claim source parent")?;
        let mut files = [None; 8];
        for (index, file) in inner.files.iter().enumerate() {
            if let Some(file) = file {
                files[index] = Some(lease_identity(file)?);
            }
        }
        let record = Record {
            version: 1,
            original: retired
                .staging
                .file_name()
                .and_then(|name| name.to_str())
                .context("claim source name")?
                .to_owned(),
            claim: name.clone(),
            parent: lease_identity(&inner.parent)?,
            token: retired.token.clone(),
            executor,
            operation,
            request_digest,
            attempt: retired.attempt.clone(),
            directory: retired.directory_identity,
            active: retired.active_identity,
            request: retired.request_identity,
            digest: retired.request_digest,
            files,
            moved: false,
            removing: 0,
            removing_directory: false,
        };
        let wrapper = parent_path.join(name);
        crate::catalog_session::validate_path(&crate::storage_volume::NativePath::from_path(
            &wrapper.join("transport"),
        ))?;
        Ok(Self {
            wrapper,
            wrapper_file: None,
            parent: inner.parent.try_clone()?,
            record,
            inner: Some(inner),
            created: false,
            complete: false,
        })
    }
    fn verify_wrapper(&self) -> Result<()> {
        ensure!(
            lease_identity(&crate::filesystem_worker::open_directory(&self.wrapper)?)?
                == lease_identity(
                    self.wrapper_file
                        .as_ref()
                        .context("claim container handle")?
                )?,
            "claim container identity changed"
        );
        ensure!(
            lease_identity(&crate::filesystem_worker::open_directory(
                self.wrapper.parent().context("claim parent")?
            )?)? == lease_identity(&self.parent)?,
            "claim parent changed"
        );
        inspect(&self.wrapper)?;
        Ok(())
    }
    pub(crate) fn resume(&mut self, retired: &CompactRetiredExportTransport) -> Result<()> {
        if self.complete {
            #[cfg(unix)]
            self.parent.sync_all()?;
            return Ok(());
        }
        if !self.created {
            #[cfg(test)]
            compact_discard_checkpoint("before-claim-create", &retired.staging)?;
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&self.wrapper)?;
            self.created = true;
            #[cfg(test)]
            compact_discard_checkpoint("claim-created", &retired.staging)?;
        }
        if self.wrapper_file.is_none() {
            self.wrapper_file = Some(discard_directory(&self.wrapper)?);
        }
        // The initial record is durable before any source move; a failed initial
        // write is retryable because no transport effect has occurred.
        if !self.wrapper.join("claim.json").try_exists()? {
            self.record.save(&self.wrapper)?;
        }
        self.verify_wrapper()?;
        if !self.record.moved {
            let target = self.wrapper.join("transport");
            if !target.try_exists()? {
                self.inner
                    .as_mut()
                    .context("unclaimed cleanup custody")?
                    .verify(retired)?;
                self.record.save(&self.wrapper)?;
                #[cfg(test)]
                compact_discard_checkpoint("before-claim", &retired.staging)?;
                #[cfg(unix)]
                {
                    use std::os::fd::AsRawFd;
                    let source = std::ffi::CString::new(self.record.original.as_bytes())?;
                    let destination = c"transport";
                    ensure!(
                        unsafe {
                            libc::renameat(
                                self.parent.as_raw_fd(),
                                source.as_ptr(),
                                self.wrapper_file.as_ref().unwrap().as_raw_fd(),
                                destination.as_ptr(),
                            )
                        } == 0,
                        "atomic export claim: {}",
                        std::io::Error::last_os_error()
                    );
                }
                #[cfg(not(unix))]
                fs::rename(&retired.staging, &target)?;
                // Record selection before fallible barriers/checks. Even if the
                // move selected a replacement, retries never select source again.
                self.record.moved = true;
                #[cfg(test)]
                compact_discard_checkpoint("after-claim", &retired.staging)?;
                self.record.save(&self.wrapper)?;
                #[cfg(unix)]
                self.parent.sync_all()?;
            } else {
                self.record.moved = true;
            }
        }
        let claimed = self.record.candidate(self.wrapper.join("transport"));
        let inner = self.inner.as_mut().context("claimed cleanup custody")?;
        inner.parent = self.wrapper_file.as_ref().unwrap().try_clone()?;
        if !inner.directory_removed {
            inner.verify(&claimed).with_context(|| {
                format!(
                    "Discard claim original={} claimed={}",
                    self.record.original, self.record.claim
                )
            })?;
        }
        #[cfg(test)]
        compact_discard_checkpoint("claim-verified", &retired.staging)?;
        inner.resume(&claimed, &mut self.record, &self.wrapper, &retired.staging)?;
        self.verify_wrapper()?;
        remove_control(&self.wrapper)?;
        self.complete = true;
        #[cfg(test)]
        compact_discard_checkpoint("after-claim-control", &retired.staging)?;
        #[cfg(unix)]
        self.parent.sync_all()?;
        Ok(())
    }
}
fn remove_control(wrapper: &Path) -> Result<()> {
    let (_, target) = inspect(wrapper)?;
    ensure!(!target, "claim still owns transport");
    for name in ["claim.next", "claim.json"] {
        match fs::remove_file(wrapper.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    sync(wrapper)?;
    fs::remove_dir(wrapper)?;
    Ok(())
}
/// Restore only the identities already authorized by the durable Discard. A
/// per-artifact intent makes absence after crash attributable to that operation;
/// an extant different object is never adopted. The whole record stays bounded.
fn recover_inner(wrapper: &Path, mutate: bool) -> Result<bool> {
    let (record, target) = inspect(wrapper)?;
    let Some(mut record) = record else {
        if mutate {
            remove_control(wrapper)?;
        }
        return Ok(false);
    };
    let parent_path = wrapper.parent().context("claim parent")?;
    if !target && record.removing_directory {
        if mutate {
            remove_control(wrapper)?;
        }
        return Ok(false);
    }
    if !target {
        ensure!(!record.moved, "claimed transport absent");
        // Pre-move intent has not consumed its public source. Cancel only this
        // empty control object; ordinary inventory still owns that source.
        if mutate {
            remove_control(wrapper)?;
        }
        return Ok(false);
    }
    record.moved = true;
    let candidate = record.candidate(wrapper.join("transport"));
    let directory = discard_directory(&candidate.staging)?;
    ensure!(
        lease_identity(&directory)? == record.directory,
        "claim directory mismatch; original and claimed objects retained"
    );
    let mut inner = ClaimedCleanup {
        directory,
        parent: discard_directory(wrapper)?,
        files: std::array::from_fn(|_| None),
        removed: [false; 8],
        directory_removed: false,
    };
    for (index, name) in TRANSPORT_FILES.iter().enumerate() {
        match fs::symlink_metadata(candidate.staging.join(name)) {
            Ok(_) => {
                let file = discard_file(&candidate.staging.join(name))?;
                ensure!(
                    Some(lease_identity(&file)?) == record.files[index],
                    "claim artifact identity mismatch"
                );
                if index < 2 {
                    file.try_lock_exclusive().context("claimed lease busy")?;
                }
                inner.files[index] = Some(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    record.files[index].is_none() || record.removing & (1 << index) != 0,
                    "claim proof absent without durable removal intent"
                );
                inner.removed[index] = record.files[index].is_some();
            }
            Err(error) => return Err(error.into()),
        }
    }
    inner.verify(&candidate)?;
    let original = parent_path.join(&record.original);
    if mutate {
        inner.resume(&candidate, &mut record, wrapper, &original)?;
        remove_control(wrapper)?;
    }
    Ok(true)
}
pub(super) fn preflight(wrapper: &Path) -> Result<()> {
    let original = inspect(wrapper)?
        .0
        .map(|record| record.original)
        .unwrap_or_else(|| "uncommitted control".into());
    recover_inner(wrapper, false).map(|_| ()).with_context(|| {
        format!(
            "Discard claim original={original} claimed={}",
            wrapper.display()
        )
    })
}
pub(super) fn recover(wrapper: &Path) -> Result<bool> {
    let original = inspect(wrapper)?
        .0
        .map(|record| record.original)
        .unwrap_or_else(|| "uncommitted control".into());
    recover_inner(wrapper, true).with_context(|| {
        format!(
            "Discard claim original={original} claimed={}",
            wrapper.display()
        )
    })
}
