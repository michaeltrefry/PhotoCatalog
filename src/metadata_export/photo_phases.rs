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

/// OS change metadata from the exact held handle. On Windows this is not a
/// monotonic content counter: callers requiring content stability must retain
/// a deny-write handle. Never substitute mtime or creation time on query failure.
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
        // Zero is legitimate on filesystems without this timestamp. Windows
        // content authority comes from the held deny-write lease, not this value.
        i128::from(info.change)
    };
    Ok(changed)
}

/// In-memory evidence only: callers cannot construct, deserialize or reset the
/// stamp. A full bounded hash is tied to a retained handle and exact current path.
/// Windows retains a deny-write lease (read/delete sharing remains enabled).
/// Unix uses change metadata. Every namespace action revalidates the path.
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
        #[cfg(not(windows))]
        let file = open_regular(path)?;
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            let file = OpenOptions::new()
                .read(true)
                .share_mode(1 | 4) // FILE_SHARE_READ | FILE_SHARE_DELETE; never WRITE.
                .custom_flags(0x0020_0000)
                .open(path)?;
            ensure!(
                file.metadata()?.file_type().is_file(),
                "not an ordinary file"
            );
            file
        };
        let before = stamp(&file)?;
        // The held Windows share mode prevents writes throughout the scan. Unix
        // change metadata also detects in-place writes with restored mtime.
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
            // A missing/damaged payload cannot prevent restoring the capture.
            // Retain a valid live payload, however, so installed-output recovery
            // never confuses a reused historical file ID with current ownership.
            VerifiedFile::read_with_checkpoint(
                &directory.join("payload"),
                seal.max_payload_bytes,
                checkpoint,
            )
            .ok()
            .filter(|verified| verified.revision() == &seal.payload)
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
    /// No catalog writer may be held: after a failed namespace operation acquire
    /// fresh bounded byte proofs. A successful link changes the payload's ctime,
    /// so the pre-link proof cannot establish ownership here.
    pub fn failure_receipt(&self, detail: String) -> ExportReceipt {
        let state = match fs::symlink_metadata(&self.seal.snapshot.destination) {
            Ok(_) => {
                let owned = (|| -> Result<bool> {
                    let payload = VerifiedFile::read(
                        &self.directory.join("payload"),
                        self.seal.max_payload_bytes,
                    )?;
                    let destination = VerifiedFile::read(
                        &self.seal.snapshot.destination,
                        self.seal.max_payload_bytes,
                    )?;
                    Ok(self.owns_installed_pair(&payload, &destination))
                })()
                .unwrap_or(false);
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
        self.payload
            .as_ref()
            .zip(self.destination.as_ref())
            .is_some_and(|(payload, destination)| self.owns_installed_pair(payload, destination))
    }
    fn owns_installed_pair(&self, payload: &VerifiedFile, destination: &VerifiedFile) -> bool {
        payload.revision() == &self.seal.payload
            && destination.revision() == &self.seal.payload
            && payload.stamp.object == destination.stamp.object
            && payload.recheck().is_ok()
            && destination.recheck().is_ok()
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
        #[cfg(windows)]
        {
            // Outside catalog authority only. FlushFileBuffers requires a write
            // handle, incompatible with our deny-write proofs. Retire those
            // proofs, flush, then fully verify both names before re-admission.
            self.payload = None;
            self.destination = None;
            let start = Instant::now();
            sync_regular(&self.seal.snapshot.destination)?;
            self.timings.durability_ms += start.elapsed().as_secs_f64() * 1000.;
        }
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
        #[cfg(not(windows))]
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
        Ok(())
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
        #[cfg(windows)]
        {
            // Same outside-authority barrier as verify_installed. Any retained
            // name may alias the restored object; none may keep write exclusion
            // while FlushFileBuffers opens it. Fresh hashes bind expected below.
            self.payload = None;
            self.captured = None;
            self.destination = None;
            let start = Instant::now();
            sync_regular(&self.seal.snapshot.destination)?;
            self.timings.durability_ms += start.elapsed().as_secs_f64() * 1000.;
        }
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
        #[cfg(not(windows))]
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

#[cfg(test)]
mod ownership_tests {
    use super::*;

    fn fixture(existing: bool) -> Result<(tempfile::TempDir, SealedPhotoExport)> {
        let root = tempfile::tempdir()?;
        let destination = root.path().join("output.jpg");
        if existing {
            fs::write(&destination, b"original bytes")?;
        }
        let input = root.path().join("encoded");
        fs::write(&input, b"encoded bytes")?;
        let snapshot = snapshot_photo_destination(&destination, 1024)?;
        let authority = blake3::hash(b"ownership regression").to_hex().to_string();
        let seal = seal_photo_export(&snapshot, &input, 1024, &authority, |_| Ok(()))?;
        Ok((root, seal))
    }

    #[test]
    fn historical_payload_identity_without_live_payload_is_not_ownership() -> Result<()> {
        let (_root, seal) = fixture(false)?;
        fs::remove_file(seal.recovery_directory().join("payload"))?;
        fs::write(&seal.snapshot.destination, b"encoded bytes")?;
        let mut publication = PhotoPublication::prepare_restore(&seal)?;
        // Deterministically emulate a filesystem reusing the deleted payload's
        // ID for this foreign object. Matching bytes do not convey ownership.
        publication.seal.payload.identity =
            publication.destination.as_ref().unwrap().revision.identity;
        assert!(!publication.installed());
        assert_eq!(
            publication
                .failure_receipt("foreign replacement".into())
                .state,
            ExportState::Conflict
        );
        assert_eq!(fs::read(&seal.snapshot.destination)?, b"encoded bytes");
        Ok(())
    }

    #[test]
    fn live_payload_does_not_authorize_a_different_destination_object() -> Result<()> {
        let (_root, seal) = fixture(false)?;
        fs::write(&seal.snapshot.destination, b"encoded bytes")?;
        let mut publication = PhotoPublication::prepare(&seal)?;
        // Also exercise classification when a valid payload is retained, but
        // the historical serialized ID happens to name a different object.
        publication.seal.payload.identity =
            publication.destination.as_ref().unwrap().revision.identity;
        assert_eq!(
            publication
                .failure_receipt("foreign replacement".into())
                .state,
            ExportState::Conflict
        );
        assert!(!publication.installed());
        Ok(())
    }

    #[test]
    fn installed_link_can_be_verified_and_finalized_after_restart() -> Result<()> {
        let (_root, seal) = fixture(false)?;
        let mut publication = PhotoPublication::prepare(&seal)?;
        publication.link()?;
        // Linking changes ctime: classification must acquire fresh evidence,
        // rather than treating the pre-link payload stamp as unchanged.
        assert_eq!(
            publication
                .failure_receipt("interrupted after link".into())
                .state,
            ExportState::Recoverable
        );
        drop(publication);
        let mut restarted = PhotoPublication::prepare_restore(&seal)?;
        assert!(restarted.installed());
        assert_eq!(restarted.verify_installed()?.state, ExportState::Published);
        restarted.recheck_installed()?;
        Ok(())
    }

    #[test]
    fn original_restore_survives_missing_or_corrupt_payload() -> Result<()> {
        for missing in [true, false] {
            let (_root, seal) = fixture(true)?;
            let mut publication = PhotoPublication::prepare(&seal)?;
            publication.capture()?;
            publication.verify_capture()?;
            drop(publication);
            let payload = seal.recovery_directory().join("payload");
            if missing {
                fs::remove_file(payload)?;
            } else {
                fs::write(payload, b"damaged bytes")?;
            }
            let mut restore = PhotoPublication::prepare_restore(&seal)?;
            assert!(!restore.installed());
            restore.restore_link()?;
            assert_eq!(restore.verify_restored()?.state, ExportState::Restored);
            assert_eq!(fs::read(&seal.snapshot.destination)?, b"original bytes");
        }
        Ok(())
    }
}
