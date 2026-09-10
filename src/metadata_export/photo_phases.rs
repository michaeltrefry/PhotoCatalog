//! Photo-only publication phases. Bulk reads and durability barriers belong to
//! the executor; callers put only capture/link operations inside catalog guards.
use super::*;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    object: (u64, u128),
    bytes: u64,
    modified: std::time::SystemTime,
    changed: i128,
}
fn stamp(file: &File) -> Result<Stamp> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "verified object is not an ordinary file"
    );
    let changed = super::content_change_stamp(file)?;
    Ok(Stamp {
        object: crate::storage_volume::held_object_key(file)?,
        bytes: metadata.len(),
        modified: metadata.modified()?,
        changed,
    })
}

/// Opaque OS content-change value read from this exact held handle. Never
/// substitute mtime or creation time when the OS query is unsupported.
pub(crate) fn content_change_stamp(file: &File) -> Result<i128> {
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec())
    };
    #[cfg(windows)]
    let changed = {
        use std::os::windows::io::AsRawHandle;
        #[repr(C)]
        struct BasicInfo {
            creation: i64,
            access: i64,
            write: i64,
            change: i64,
            attributes: u32,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetFileInformationByHandleEx(
                handle: *mut std::ffi::c_void,
                class: i32,
                info: *mut std::ffi::c_void,
                bytes: u32,
            ) -> i32;
        }
        let mut info = std::mem::MaybeUninit::<BasicInfo>::uninit();
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                0,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<BasicInfo>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let info = unsafe { info.assume_init() };
        ensure!(
            info.change != 0,
            "filesystem does not expose a content-change stamp"
        );
        i128::from(info.change)
    };
    Ok(changed)
}

/// In-memory evidence only: callers cannot construct, deserialize or reset the
/// stamp. A full bounded hash is tied to a retained handle and exact current path.
/// This is not a lock on external writers; every namespace action revalidates it.
pub struct VerifiedFile {
    file: File,
    path: PathBuf,
    stamp: Stamp,
    revision: FileRevision,
}
impl VerifiedFile {
    pub fn read(path: &Path, max_bytes: u64) -> Result<Self> {
        Self::read_with_checkpoint(path, max_bytes, &mut |_| Ok(()))
    }
    #[doc(hidden)]
    pub fn read_with_checkpoint(
        path: &Path,
        max_bytes: u64,
        checkpoint: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> Result<Self> {
        let file = open_regular(path)?;
        let before = stamp(&file)?;
        // The outer held handle/change stamp also catches same-length in-place
        // writes whose mtime was restored during stream_revision's scan.
        let revision = stream_revision(path, max_bytes, checkpoint, None)?;
        ensure!(
            revision.identity == super::held_file_identity(&file)? && stamp(&file)? == before,
            "file changed during verification"
        );
        let result = Self {
            file,
            path: path.to_owned(),
            stamp: before,
            revision,
        };
        result.recheck()?;
        Ok(result)
    }
    pub fn revision(&self) -> &FileRevision {
        &self.revision
    }
    /// Constant byte work: handles/path identity and fresh change metadata only.
    pub fn recheck(&self) -> Result<()> {
        let current = open_regular(&self.path)?;
        ensure!(
            stamp(&self.file)? == self.stamp && stamp(&current)? == self.stamp,
            "verified file changed while waiting for authority"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PhotoPublicationTimings {
    pub hash_ms: f64,
    pub capture_ms: f64,
    pub link_ms: f64,
    pub durability_ms: f64,
}
/// Live operation lease plus checked bytes. Never persisted as authorization.
/// A separate catalog intent must commit before capture or link is invoked.
pub struct PhotoPublication {
    seal: SealedPhotoExport,
    directory: PathBuf,
    _lock: OperationLock,
    payload: Option<VerifiedFile>,
    destination: Option<VerifiedFile>,
    captured: Option<VerifiedFile>,
    timings: PhotoPublicationTimings,
}
impl PhotoPublication {
    pub fn prepare(seal: &SealedPhotoExport) -> Result<Self> {
        Self::prepare_with_checkpoint(seal, &mut |_| Ok(()))
    }
    #[doc(hidden)]
    pub fn prepare_with_checkpoint(
        seal: &SealedPhotoExport,
        checkpoint: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> Result<Self> {
        Self::prepare_mode(seal, checkpoint, false)
    }
    pub fn prepare_restore(seal: &SealedPhotoExport) -> Result<Self> {
        Self::prepare_mode(seal, &mut |_| Ok(()), true)
    }
    fn prepare_mode(
        seal: &SealedPhotoExport,
        checkpoint: &mut dyn FnMut(u64) -> io::Result<()>,
        restore: bool,
    ) -> Result<Self> {
        validate_photo_seal(seal)?;
        let directory = seal.recovery_directory();
        ensure!(
            fs::symlink_metadata(&directory)?.is_dir(),
            "photo operation directory changed"
        );
        let directory = directory.canonicalize()?;
        ensure!(
            directory == seal.recovery_directory(),
            "photo operation path changed"
        );
        let lock = OperationLock(open_regular(&directory.join("operation.lock"))?);
        lock.0
            .try_lock_exclusive()
            .context("photo publication already active")?;
        let stored: SealedPhotoExport = read_journal(&directory.join("photo-seal.json"))?;
        let plan: ExportPlan = read_journal(&directory.join("plan.json"))?;
        ensure!(
            stored == *seal && plan == seal.plan(),
            "photo publication authority differs"
        );
        let start = Instant::now();
        let payload = if restore {
            None
        } else {
            let verified = VerifiedFile::read_with_checkpoint(
                &directory.join("payload"),
                seal.max_payload_bytes,
                checkpoint,
            )?;
            ensure!(
                verified.revision() == &seal.payload,
                "sealed payload changed"
            );
            Some(verified)
        };
        let maximum = seal.max_payload_bytes.max(seal.snapshot.max_existing_bytes);
        let destination = if restore {
            // A foreign/special/oversized current destination must not prevent
            // retaining/reporting the capture; no-clobber restore still refuses it.
            optional_verified(&seal.snapshot.destination, maximum, checkpoint).unwrap_or(None)
        } else {
            optional_verified(&seal.snapshot.destination, maximum, checkpoint)?
        };
        let captured = optional_verified(
            &directory.join("original"),
            seal.snapshot.max_existing_bytes,
            checkpoint,
        )?;
        Ok(Self {
            seal: seal.clone(),
            directory,
            _lock: lock,
            payload,
            destination,
            captured,
            timings: PhotoPublicationTimings {
                hash_ms: start.elapsed().as_secs_f64() * 1000.,
                ..Default::default()
            },
        })
    }
    pub fn recheck_payload(&self) -> Result<()> {
        self.payload
            .as_ref()
            .context("payload is not verified")?
            .recheck()
    }
    pub fn timings(&self) -> &PhotoPublicationTimings {
        &self.timings
    }
    pub fn failure_receipt(&self, detail: String) -> ExportReceipt {
        let state = match fs::symlink_metadata(&self.seal.snapshot.destination) {
            Ok(metadata) => {
                let owned = open_regular(&self.seal.snapshot.destination)
                    .ok()
                    .and_then(|file| identity(&file, &metadata).ok())
                    == Some(self.seal.payload.identity);
                if owned {
                    ExportState::Recoverable
                } else {
                    ExportState::Conflict
                }
            }
            Err(_) => ExportState::Recoverable,
        };
        receipt(&self.seal.plan(), &self.directory, state, detail)
    }

    pub fn installed(&self) -> bool {
        self.destination.as_ref().is_some_and(|file| {
            file.revision().identity == self.seal.payload.identity
                && file.revision().digest == self.seal.payload.digest
                && file.revision().bytes == self.seal.payload.bytes
        })
    }
    /// Called under current catalog/source/job authority, after intent commit.
    /// Namespace only; capture is fully rehashed by verify_capture after release.
    pub fn capture(&mut self) -> Result<()> {
        self.recheck_payload()?;
        if let Some(captured) = &self.captured {
            captured.recheck()?;
            ensure!(
                Some(captured.revision()) == self.seal.snapshot.expected.as_ref(),
                "captured original differs from plan"
            );
            return Ok(());
        }
        if let Some(expected) = &self.seal.snapshot.expected {
            let destination = self
                .destination
                .as_ref()
                .context("expected destination is absent")?;
            ensure!(
                destination.revision() == expected,
                "destination changed since planning"
            );
            destination.recheck()?;
            let start = Instant::now();
            move_to_private(
                &self.seal.snapshot.destination,
                &self.directory.join("original"),
            )?;
            self.timings.capture_ms += start.elapsed().as_secs_f64() * 1000.;
            self.destination = None;
        }
        Ok(())
    }
    /// No catalog writer may be held. Rename changes ctime: rehash the captured
    /// bytes instead of accepting/resetting a changed stamp from a cached proof.
    pub fn verify_capture(&mut self) -> Result<()> {
        self.verify_capture_with_checkpoint(&mut |_| Ok(()))
    }
    #[doc(hidden)]
    pub fn verify_capture_with_checkpoint(
        &mut self,
        checkpoint: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> Result<()> {
        if let Some(expected) = &self.seal.snapshot.expected {
            let start = Instant::now();
            let captured = VerifiedFile::read_with_checkpoint(
                &self.directory.join("original"),
                self.seal.snapshot.max_existing_bytes,
                checkpoint,
            )?;
            self.timings.hash_ms += start.elapsed().as_secs_f64() * 1000.;
            ensure!(
                captured.revision() == expected,
                "captured original differs from plan"
            );
            self.captured = Some(captured);
            let start = Instant::now();
            sync_directory(&self.directory)?;
            sync_directory(self.seal.snapshot.destination.parent().unwrap())?;
            self.timings.durability_ms += start.elapsed().as_secs_f64() * 1000.;
        }
        Ok(())
    }
    /// Called under freshly reacquired authority. No full reads or explicit
    /// fsync here; Windows retains the existing write-through rename primitive.
    /// Its actual latency is included in link_ms and the authority interval.
    pub fn link(&mut self) -> Result<()> {
        self.recheck_payload()?;
        if self.seal.snapshot.expected.is_some() {
            let captured = self.captured.as_ref().context("capture was not verified")?;
            captured.recheck()?;
            ensure!(
                Some(captured.revision()) == self.seal.snapshot.expected.as_ref(),
                "capture changed"
            );
        }
        let start = Instant::now();
        publish_noclobber(
            &self.directory.join("payload"),
            &self.seal.snapshot.destination,
        )?;
        self.timings.link_ms += start.elapsed().as_secs_f64() * 1000.;
        Ok(())
    }
    /// Outside catalog writer: verify both names after our hardlink changed ctime,
    /// then flush and recheck. This also handles link-before-catalog-commit crashes.
    pub fn verify_installed(&mut self) -> Result<ExportReceipt> {
        self.verify_installed_with_checkpoint(&mut |_| Ok(()))
    }
    #[doc(hidden)]
    pub fn verify_installed_with_checkpoint(
        &mut self,
        checkpoint: &mut dyn FnMut(u64) -> io::Result<()>,
    ) -> Result<ExportReceipt> {
        let start = Instant::now();
        self.payload = Some(VerifiedFile::read_with_checkpoint(
            &self.directory.join("payload"),
            self.seal.max_payload_bytes,
            checkpoint,
        )?);
        let destination = VerifiedFile::read_with_checkpoint(
            &self.seal.snapshot.destination,
            self.seal.max_payload_bytes,
            checkpoint,
        )?;
        self.timings.hash_ms += start.elapsed().as_secs_f64() * 1000.;
        ensure!(
            self.payload
                .as_ref()
                .context("payload is not verified")?
                .revision()
                == &self.seal.payload
                && destination.revision() == &self.seal.payload,
            "installed payload changed; retained evidence requires recovery"
        );
        self.destination = Some(destination);
        let start = Instant::now();
        sync_regular(&self.seal.snapshot.destination)?;
        sync_directory(self.seal.snapshot.destination.parent().unwrap())?;
        self.timings.durability_ms += start.elapsed().as_secs_f64() * 1000.;
        self.recheck_installed()?;
        Ok(receipt(
            &self.seal.plan(),
            &self.directory,
            ExportState::Published,
            "intent-authorized payload verified; durability barriers completed".into(),
        ))
    }
    pub fn recheck_installed(&self) -> Result<()> {
        ensure!(self.installed(), "owned payload is not installed");
        self.recheck_payload()?;
        let payload = self.payload.as_ref().context("payload is not verified")?;
        let destination = self
            .destination
            .as_ref()
            .context("destination is not verified")?;
        ensure!(
            payload.stamp.object == destination.stamp.object,
            "installed full file identity differs"
        );
        destination.recheck()
    }
    /// Restoration only: capture may contain externally changed bytes; preserve
    /// them rather than overwriting/removing any independently created destination.
    /// Caller authorizes this short namespace step under the current alias guard.
    pub fn restore_link(&mut self) -> Result<()> {
        ensure!(
            !self.installed(),
            "owned output is already installed; finalize its intent"
        );
        let captured = self
            .captured
            .as_ref()
            .context("no captured original to restore")?;
        captured.recheck()?;
        if let Some(destination) = &self.destination {
            // A prior restore may have linked successfully before its owner
            // crashed. Only the exact held captured object can make this a
            // no-op; equal bytes in a different object still conflict.
            destination.recheck()?;
            if destination.stamp.object == captured.stamp.object
                && destination.revision() == captured.revision()
            {
                return Ok(());
            }
        }
        let start = Instant::now();
        publish_noclobber(
            &self.directory.join("original"),
            &self.seal.snapshot.destination,
        )?;
        self.timings.link_ms += start.elapsed().as_secs_f64() * 1000.;
        Ok(())
    }
    pub fn verify_restored(&mut self) -> Result<ExportReceipt> {
        let expected = self
            .captured
            .as_ref()
            .context("missing capture proof")?
            .revision()
            .clone();
        let start = Instant::now();
        let captured = VerifiedFile::read(
            &self.directory.join("original"),
            self.seal.snapshot.max_existing_bytes,
        )?;
        let destination = VerifiedFile::read(
            &self.seal.snapshot.destination,
            self.seal.snapshot.max_existing_bytes,
        )?;
        self.timings.hash_ms += start.elapsed().as_secs_f64() * 1000.;
        ensure!(
            captured.revision() == &expected && destination.revision() == &expected,
            "restored object changed; retained evidence requires recovery"
        );
        self.captured = Some(captured);
        self.destination = Some(destination);
        let start = Instant::now();
        sync_regular(&self.seal.snapshot.destination)?;
        sync_directory(self.seal.snapshot.destination.parent().unwrap())?;
        self.timings.durability_ms += start.elapsed().as_secs_f64() * 1000.;
        self.recheck_restored()?;
        Ok(receipt(
            &self.seal.plan(),
            &self.directory,
            ExportState::Restored,
            "captured bytes restored without clobber and verified durable".into(),
        ))
    }
    pub fn recheck_restored(&self) -> Result<()> {
        let captured = self.captured.as_ref().context("missing capture")?;
        let destination = self
            .destination
            .as_ref()
            .context("missing restored destination")?;
        captured.recheck()?;
        destination.recheck()?;
        ensure!(
            captured.stamp.object == destination.stamp.object
                && captured.revision() == destination.revision(),
            "restored full file identity differs"
        );
        Ok(())
    }
}
fn optional_verified(
    path: &Path,
    maximum: u64,
    checkpoint: &mut dyn FnMut(u64) -> io::Result<()>,
) -> Result<Option<VerifiedFile>> {
    match fs::symlink_metadata(path) {
        Ok(_) => VerifiedFile::read_with_checkpoint(path, maximum, checkpoint).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
