use super::*;
use crate::{
    application::U64,
    catalog_session::{PhysicalObjectId, RootCapability},
};

struct Fixture {
    _temporary: tempfile::TempDir,
    manifest: PathBuf,
    root: RootCapability,
    owner: Owner,
    next: u64,
    cancel: AtomicBool,
}
impl Fixture {
    fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let manifest = temporary.path().canonicalize()?;
        #[cfg(unix)]
        let physical = PhysicalObjectId::Unix {
            device: U64(1),
            inode: U64(2),
        };
        #[cfg(windows)]
        let physical = PhysicalObjectId::Windows {
            volume_serial: U64(1),
            file_index: U64(2),
        };
        Ok(Self {
            root: RootCapability {
                epoch: LeaseId::new(),
                token: LeaseId::new(),
                session: LeaseId::new(),
                canonical_root: NativePath::from_path(&manifest),
                root_physical: physical,
                catalog_physical: physical,
            },
            _temporary: temporary,
            manifest,
            owner: Owner::default(),
            next: 0,
            cancel: AtomicBool::new(false),
        })
    }
    fn request(&mut self, action: Action, supervisor: bool) -> Request {
        self.next += 1;
        Request {
            root: self.root.clone(),
            operation: U64(self.next),
            supervisor,
            action,
        }
    }
    fn call(&mut self, action: Action, supervisor: bool) -> Result<Value> {
        let request = self.request(action, supervisor);
        let reply = self.owner.call(&self.manifest, &request, &self.cancel)?;
        reply.validate(&request)?;
        Ok(reply.value)
    }
    fn admit(&mut self) -> Result<LeaseId> {
        match self.call(
            Action::Admit {
                limits: Limits {
                    workers: 2,
                    encoded: U64(1024 * 1024),
                    rgb: U64(1024 * 1024),
                    prepared: U64(1024 * 1024),
                },
            },
            false,
        )? {
            Value::Admitted {
                stage,
                ready: true,
                error: None,
            } => Ok(stage),
            other => anyhow::bail!("unexpected admission: {other:?}"),
        }
    }
    fn path(&self, stage: &LeaseId) -> PathBuf {
        self.owner.stage(stage).unwrap().path.to_path_buf()
    }
    fn arm(&mut self, stage: &LeaseId, native: u64) -> Result<Value> {
        self.call(
            Action::Arm {
                stage: stage.clone(),
                native: U64(native),
            },
            true,
        )
    }
    fn drain(&mut self, stage: &LeaseId, native: u64) -> Result<Value> {
        self.call(
            Action::NativeDrained {
                stage: stage.clone(),
                native: U64(native),
            },
            true,
        )
    }
    fn begin(&mut self, stage: &LeaseId, bytes: &[u8], digest: String) -> Result<Value> {
        fs::write(self.path(stage).join("0.rgb"), bytes)?;
        self.call(
            Action::BeginRead {
                stage: stage.clone(),
                artifact: Artifact::Rgb(0),
                bytes: U64(bytes.len() as u64),
                digest,
            },
            false,
        )
    }
}

#[test]
fn interrupted_transfer_requires_exact_abort_and_releases_capacity() -> Result<()> {
    let mut f = Fixture::new()?;
    let stage = f.admit()?;
    f.arm(&stage, 1)?;
    f.drain(&stage, 1)?;
    let bytes = vec![37; CHUNK + 7];
    f.begin(&stage, &bytes, blake3::hash(&bytes).to_hex().to_string())?;
    let request = f.request(
        Action::Read {
            stage: stage.clone(),
            offset: U64(0),
        },
        false,
    );
    let first = f.owner.call(&f.manifest, &request, &f.cancel)?;
    let repeated = f.owner.call(&f.manifest, &request, &f.cancel)?;
    assert_eq!(first.binary(), &bytes[..CHUNK]);
    assert_eq!(repeated.binary(), first.binary());
    assert_eq!(f.owner.stream.as_ref().unwrap().offset, CHUNK as u64);
    f.cancel.store(true, Ordering::Release);
    assert!(
        f.call(
            Action::Read {
                stage: stage.clone(),
                offset: U64(CHUNK as u64)
            },
            false
        )
        .is_err()
    );
    assert!(
        f.call(
            Action::FinishRead {
                stage: stage.clone()
            },
            false
        )
        .is_err()
    );
    assert!(
        f.call(
            Action::Release {
                stage: stage.clone()
            },
            false
        )
        .is_err()
    );
    assert!(
        f.call(
            Action::AbortRead {
                stage: LeaseId::new()
            },
            false
        )
        .is_err()
    );
    assert!(f.owner.stream.is_some());
    f.call(
        Action::AbortRead {
            stage: stage.clone(),
        },
        false,
    )?;
    let path = f.path(&stage);
    f.call(Action::Release { stage }, false)?;
    assert!(!path.exists());
    assert!(f.owner.empty());
    f.cancel.store(false, Ordering::Release);
    let replacement = f.admit()?;
    f.call(Action::Release { stage: replacement }, false)?;
    Ok(())
}

#[test]
fn corrupt_transfer_never_finishes_but_can_be_aborted() -> Result<()> {
    let mut f = Fixture::new()?;
    let stage = f.admit()?;
    f.arm(&stage, 1)?;
    f.drain(&stage, 1)?;
    f.begin(&stage, b"invalid", "a".repeat(64))?;
    f.call(
        Action::Read {
            stage: stage.clone(),
            offset: U64(0),
        },
        false,
    )?;
    assert!(
        f.call(
            Action::FinishRead {
                stage: stage.clone()
            },
            false
        )
        .is_err()
    );
    assert!(f.owner.stream.is_some());
    f.call(
        Action::AbortRead {
            stage: stage.clone(),
        },
        false,
    )?;
    f.call(Action::Release { stage }, false)?;
    assert!(f.owner.empty());
    Ok(())
}

fn receipt() -> crate::edit::PreparedProxyReceipt {
    crate::edit::PreparedProxyReceipt {
        bytes: 1,
        blake3: "b".repeat(64),
        identity: crate::edit::PreparedProxyIdentity {
            source_fingerprint: "c".repeat(64),
            white_balance: crate::edit::WhiteBalance::AsShot,
            renderer_identity: crate::edit::renderer_identity().into(),
            original_dimensions: (1, 1),
            longest_edge: 1,
            width: 1,
            height: 1,
        },
    }
}
fn reject_replaced_prepared_root(f: &mut Fixture, foreign: &Path) -> Result<()> {
    let key = "a".repeat(64);
    let file = foreign.join(format!("{key}.linear"));
    fs::write(&file, b"foreign cache file")?;
    assert!(
        f.call(Action::PreparedRemove { key: key.clone() }, false)
            .is_err()
    );
    assert!(f.call(Action::PreparedInitialize, false).is_err());
    // An invalid stage makes the check order observable: root authority must
    // fail before source receipt processing or destination mutation.
    let error = f
        .call(
            Action::PreparedAdopt {
                stage: LeaseId::new(),
                key,
                receipt: receipt(),
            },
            false,
        )
        .unwrap_err();
    assert!(error.to_string().contains("prepared root"), "{error:#}");
    assert_eq!(fs::read(file)?, b"foreign cache file");
    Ok(())
}

#[test]
fn prepared_directory_identity_is_retained_across_reinitialization() -> Result<()> {
    let mut f = Fixture::new()?;
    f.call(Action::PreparedInitialize, false)?;
    let path = f.manifest.join("prepared");
    let retained = path.join(format!("{}.linear", "d".repeat(64)));
    fs::write(&retained, b"live cached data")?;
    f.call(Action::PreparedInitialize, false)?;
    assert_eq!(fs::read(&retained)?, b"live cached data");
    fs::rename(&path, f.manifest.join("prepared-original"))?;
    fs::create_dir(&path)?;
    reject_replaced_prepared_root(&mut f, &path)?;
    assert_eq!(
        fs::read(
            f.manifest
                .join("prepared-original")
                .join(retained.file_name().unwrap())
        )?,
        b"live cached data"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn prepared_symlink_replacement_does_not_delete_foreign_files() -> Result<()> {
    let mut f = Fixture::new()?;
    f.call(Action::PreparedInitialize, false)?;
    let path = f.manifest.join("prepared");
    fs::rename(&path, f.manifest.join("prepared-original"))?;
    let foreign = f.manifest.join("foreign");
    fs::create_dir(&foreign)?;
    std::os::unix::fs::symlink(&foreign, &path)?;
    reject_replaced_prepared_root(&mut f, &foreign)
}

#[test]
fn cleanup_claim_never_replaces_an_existing_directory() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    fs::create_dir(&source)?;
    fs::create_dir(&destination)?;
    let held = super::super::bootstrap::open_directory(&destination)?;
    assert!(rename_claim(&source, &destination).is_err());
    assert!(source.is_dir());
    assert_eq!(
        crate::catalog_storage::physical_object_id(&held)?,
        crate::catalog_storage::physical_object_id(&super::super::bootstrap::open_directory(
            &destination
        )?)?
    );
    let mut f = Fixture::new()?;
    let stage = f.admit()?;
    let claimed = f
        .manifest
        .join("workers")
        .join(format!("claimed-{}", stage.as_str()));
    fs::create_dir(&claimed)?;
    f.call(Action::Release { stage }, false)?;
    assert!(claimed.is_dir());
    assert!(f.owner.empty());
    Ok(())
}

#[test]
fn drained_header_rearms_only_sealed_input_without_outputs() -> Result<()> {
    let mut f = Fixture::new()?;
    let stage = f.admit()?;
    let bytes = b"synthetic encoded input";
    let digest = blake3::hash(bytes).to_hex().to_string();
    f.call(
        Action::Upload {
            stage: stage.clone(),
            offset: U64(0),
            bytes: bytes.to_vec(),
        },
        false,
    )?;
    f.call(
        Action::SealInput {
            stage: stage.clone(),
            bytes: U64(bytes.len() as u64),
            digest: digest.clone(),
        },
        false,
    )?;
    f.arm(&stage, 1)?;
    let path = f.path(&stage);
    let header = crate::catalog_session::native::Header {
        operation: U64(1),
        stage: stage.clone(),
        input_digest: digest,
        input_bytes: U64(bytes.len() as u64),
        codec: crate::preview::Codec::Jpeg,
        width: 2,
        height: 3,
    };
    fs::write(path.join("header.ready"), serde_json::to_vec(&header)?)?;
    fs::write(path.join("active.lock"), [])?;
    f.drain(&stage, 1)?;
    for output in [
        "result.json",
        "0.rgb",
        "0.preview",
        "prepared.linear",
        "unknown",
    ] {
        fs::write(path.join(output), b"retained output")?;
        assert!(f.arm(&stage, 2).is_err(), "rearmed with {output}");
        assert!(path.join("header.ready").exists());
        assert_eq!(fs::read(path.join(output))?, b"retained output");
        fs::remove_file(path.join(output))?;
    }
    assert!(f.arm(&stage, 1).is_err());
    f.arm(&stage, 2)?;
    assert_eq!(fs::read(path.join("input.encoded"))?, bytes);
    assert_eq!(fs::read(path.join("managed.stage"))?, b"managed-stage-1");
    assert!(!path.join("header.ready").exists());
    assert!(!path.join("active.lock").exists());
    assert!(f.drain(&stage, 1).is_err());
    assert!(
        f.call(
            Action::Release {
                stage: stage.clone()
            },
            false
        )
        .is_err()
    );
    f.drain(&stage, 2)?;
    f.call(Action::Release { stage }, false)?;
    assert!(!path.exists());
    Ok(())
}

#[test]
fn supervisor_abandon_requires_all_native_owners_drained_and_retains_failed_cleanup() -> Result<()>
{
    let mut f = Fixture::new()?;
    let first = f.admit()?;
    let second = f.admit()?;
    f.arm(&first, 1)?;
    f.arm(&second, 2)?;
    f.drain(&first, 1)?;
    f.begin(&first, b"RGB", blake3::hash(b"RGB").to_hex().to_string())?;
    assert!(f.call(Action::AbandonOwned, false).is_err());
    assert!(f.call(Action::AbandonOwned, true).is_err());
    assert!(f.owner.stream.is_some());
    assert_eq!(f.owner.stages.len(), 2);
    f.drain(&second, 2)?;
    let path = f.path(&first);
    fs::write(path.join("unknown"), b"preserve")?;
    assert!(f.call(Action::AbandonOwned, true).is_err());
    assert!(f.owner.stream.is_none());
    assert_eq!(f.owner.stages.len(), 2);
    assert_eq!(fs::read(path.join("unknown"))?, b"preserve");
    fs::remove_file(path.join("unknown"))?;
    f.cancel.store(true, Ordering::Release);
    f.call(Action::AbandonOwned, true)?;
    assert!(f.owner.empty());
    assert!(!path.exists());
    Ok(())
}

#[test]
fn overlimit_native_path_is_rejected_before_stage_creation() -> Result<()> {
    let mut f = Fixture::new()?;
    let original = f.manifest.clone();
    f.manifest
        .push("a".repeat(crate::catalog_session::PATH_UNITS));
    assert!(f.admit().is_err());
    assert!(f.owner.empty());
    assert!(f.owner.workers.is_none());
    assert!(!original.join("workers").exists());
    Ok(())
}

#[test]
fn stage_capacity_is_admitted_before_directory_effects() -> Result<()> {
    let mut probe = Fixture::new()?;
    let baseline = probe.owner.retained();
    let stage = probe.admit()?;
    let dynamic = probe.owner.retained() - baseline;
    let mut f = Fixture::new()?;
    let adjusted = dynamic - probe.manifest.as_os_str().as_encoded_bytes().len()
        + f.manifest.as_os_str().as_encoded_bytes().len();
    f.owner.budget = f.owner.retained() + adjusted - 1;
    let failure = f.admit().unwrap_err();
    assert_eq!(
        failure.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::ResourceLimit
    );
    assert!(!f.manifest.join("workers").exists());
    assert!(f.owner.stages.is_empty());
    f.owner.budget += 1;
    let accepted = f.admit()?;
    assert!(f.owner.retained() <= f.owner.budget);
    f.call(Action::Release { stage: accepted }, false)?;
    probe.call(Action::Release { stage }, false)?;
    Ok(())
}

#[test]
fn prepared_path_denial_has_no_effect_and_exact_failure_replays() -> Result<()> {
    let mut f = Fixture::new()?;
    let prepared = f.manifest.join("prepared");
    f.owner.budget = f.owner.retained() + prepared.as_os_str().as_encoded_bytes().len() - 1;
    let request = f.request(Action::PreparedInitialize, false);
    let first = f.owner.call(&f.manifest, &request, &f.cancel).unwrap_err();
    assert_eq!(
        first.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::ResourceLimit
    );
    assert!(!prepared.exists());
    assert!(f.owner.prepared.is_none());
    f.owner.budget += 1;
    let replay = f.owner.call(&f.manifest, &request, &f.cancel).unwrap_err();
    assert_eq!(first.to_string(), replay.to_string());
    assert!(!prepared.exists());
    f.call(Action::PreparedInitialize, false)?;
    assert!(prepared.is_dir());
    assert!(f.owner.retained() <= f.owner.budget);
    Ok(())
}

#[test]
fn exhausted_metadata_budget_still_allows_owned_cleanup_and_retry() -> Result<()> {
    let mut f = Fixture::new()?;
    let stage = f.admit()?;
    let path = f.path(&stage);
    f.owner.budget = f.owner.retained() - 1;
    assert!(f.admit().is_err());
    f.call(Action::Release { stage }, false)?;
    assert!(!path.exists());
    assert!(f.owner.empty());
    assert!(f.owner.retained() <= f.owner.budget);
    f.owner.budget += 1;
    let retry = f.admit()?;
    assert!(f.owner.retained() <= f.owner.budget);
    f.call(Action::Release { stage: retry }, false)?;
    Ok(())
}

fn legacy_file(f: &Fixture, hash: &str, bytes: &[u8]) -> Result<PathBuf> {
    let root = f.root.canonical_root.to_path()?.join("previews");
    fs::create_dir_all(&root)?;
    let path = root.join(format!("{hash}.jpg"));
    fs::write(&path, bytes)?;
    Ok(path)
}
fn legacy_value(value: Value) -> Result<(LeaseId, u64)> {
    match value {
        Value::LegacyRead {
            transfer: Some(transfer),
            bytes,
        } => Ok((transfer, bytes.0)),
        other => anyhow::bail!("expected retained legacy transfer: {other:?}"),
    }
}

#[test]
fn legacy_catalog_read_replays_held_chunks_and_finishes_without_stage_release() -> Result<()> {
    let mut f = Fixture::new()?;
    let bytes = vec![73; CHUNK + 7];
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let path = legacy_file(&f, &hash, &bytes)?;
    f.manifest = f._temporary.path().join("different-manifest-root");
    let request = f.request(
        Action::BeginLegacyRead {
            hash,
            allowance: U64(bytes.len() as u64),
        },
        false,
    );
    let first = f.owner.call(&f.manifest, &request, &f.cancel)?;
    first.validate(&request)?;
    let (transfer, length) = legacy_value(first.value)?;
    let repeated = f.owner.call(&f.manifest, &request, &f.cancel)?;
    let (again, repeated_length) = legacy_value(repeated.value)?;
    assert_eq!(transfer, again);
    assert_eq!(length, repeated_length);
    assert_eq!(length, bytes.len() as u64);
    assert!(f.owner.stages.is_empty());
    assert!(!f.owner.empty());
    let read = f.request(
        Action::Read {
            stage: transfer.clone(),
            offset: U64(0),
        },
        false,
    );
    let chunk = f.owner.call(&f.manifest, &read, &f.cancel)?;
    let replay = f.owner.call(&f.manifest, &read, &f.cancel)?;
    assert_eq!(chunk.binary(), &bytes[..CHUNK]);
    assert_eq!(replay.binary(), chunk.binary());
    assert_eq!(f.owner.stream.as_ref().unwrap().offset, CHUNK as u64);
    match f.call(
        Action::Read {
            stage: transfer.clone(),
            offset: U64(CHUNK as u64),
        },
        false,
    )? {
        Value::Chunk { bytes: tail } => assert_eq!(tail, bytes[CHUNK..]),
        other => anyhow::bail!("expected legacy tail: {other:?}"),
    }
    f.call(Action::FinishRead { stage: transfer }, false)?;
    assert!(f.owner.empty());
    assert_eq!(fs::read(path)?, bytes);
    assert!(!f.manifest.exists());
    Ok(())
}

#[test]
fn legacy_and_stage_reads_share_one_stream_and_cancel_requires_exact_abort() -> Result<()> {
    let mut f = Fixture::new()?;
    let stage = f.admit()?;
    f.arm(&stage, 1)?;
    f.drain(&stage, 1)?;
    f.begin(&stage, b"rgb", blake3::hash(b"rgb").to_hex().to_string())?;
    let bytes = vec![9; CHUNK + 1];
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let path = legacy_file(&f, &hash, &bytes)?;
    assert!(
        f.call(
            Action::BeginLegacyRead {
                hash: hash.clone(),
                allowance: U64(LEGACY_BYTES)
            },
            false
        )
        .is_err()
    );
    f.call(
        Action::AbortRead {
            stage: stage.clone(),
        },
        false,
    )?;
    let (transfer, _) = legacy_value(f.call(
        Action::BeginLegacyRead {
            hash,
            allowance: U64(LEGACY_BYTES),
        },
        false,
    )?)?;
    assert!(
        f.begin(&stage, b"rgb", blake3::hash(b"rgb").to_hex().to_string())
            .is_err()
    );
    f.call(
        Action::Read {
            stage: transfer.clone(),
            offset: U64(0),
        },
        false,
    )?;
    f.cancel.store(true, Ordering::Release);
    assert!(
        f.call(
            Action::Read {
                stage: transfer.clone(),
                offset: U64(CHUNK as u64)
            },
            false
        )
        .is_err()
    );
    assert!(
        f.call(
            Action::FinishRead {
                stage: transfer.clone()
            },
            false
        )
        .is_err()
    );
    assert!(
        f.call(
            Action::AbortRead {
                stage: LeaseId::new()
            },
            false
        )
        .is_err()
    );
    assert_eq!(f.owner.stream.as_ref().unwrap().stage, transfer);
    f.call(Action::AbortRead { stage: transfer }, false)?;
    f.call(Action::Release { stage }, false)?;
    assert!(f.owner.empty());
    assert_eq!(fs::read(path)?, bytes);
    Ok(())
}

#[test]
fn legacy_missing_invalid_and_oversize_inputs_never_install_a_stream() -> Result<()> {
    let mut f = Fixture::new()?;
    let hash = blake3::hash(b"content").to_hex().to_string();
    match f.call(
        Action::BeginLegacyRead {
            hash: hash.clone(),
            allowance: U64(LEGACY_BYTES),
        },
        false,
    )? {
        Value::LegacyRead {
            transfer: None,
            bytes: U64(0),
        } => {}
        other => anyhow::bail!("expected missing legacy receipt: {other:?}"),
    }
    assert!(!f.root.canonical_root.to_path()?.join("previews").exists());
    assert!(
        f.call(
            Action::BeginLegacyRead {
                hash: "../outside".into(),
                allowance: U64(LEGACY_BYTES)
            },
            false
        )
        .is_err()
    );
    let path = legacy_file(&f, &hash, b"content")?;
    let denied = f
        .call(
            Action::BeginLegacyRead {
                hash: hash.clone(),
                allowance: U64(6),
            },
            false,
        )
        .unwrap_err();
    assert_eq!(
        denied.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::ResourceLimit
    );
    assert!(f.owner.stream.is_none());
    File::options()
        .write(true)
        .open(&path)?
        .set_len(LEGACY_BYTES + 1)?;
    assert!(
        f.call(
            Action::BeginLegacyRead {
                hash: hash.clone(),
                allowance: U64(LEGACY_BYTES + 1)
            },
            false
        )
        .is_err()
    );
    assert!(f.owner.stream.is_none());
    #[cfg(unix)]
    {
        fs::remove_file(&path)?;
        let target = f._temporary.path().join("outside-preview");
        fs::write(&target, b"content")?;
        std::os::unix::fs::symlink(&target, &path)?;
        assert!(
            f.call(
                Action::BeginLegacyRead {
                    hash,
                    allowance: U64(LEGACY_BYTES)
                },
                false
            )
            .is_err()
        );
        assert!(f.owner.stream.is_none());
    }
    Ok(())
}

#[test]
fn legacy_corruption_or_growth_keeps_transfer_owned_until_abort() -> Result<()> {
    for grow in [false, true] {
        let mut f = Fixture::new()?;
        let hash = blake3::hash(b"right").to_hex().to_string();
        let path = legacy_file(&f, &hash, if grow { b"right" } else { b"wrong" })?;
        let (transfer, _) = legacy_value(f.call(
            Action::BeginLegacyRead {
                hash,
                allowance: U64(LEGACY_BYTES),
            },
            false,
        )?)?;
        f.call(
            Action::Read {
                stage: transfer.clone(),
                offset: U64(0),
            },
            false,
        )?;
        if grow {
            OpenOptions::new()
                .append(true)
                .open(&path)?
                .write_all(b"!")?;
        }
        assert!(
            f.call(
                Action::FinishRead {
                    stage: transfer.clone()
                },
                false
            )
            .is_err()
        );
        assert!(f.owner.stream.is_some());
        f.call(Action::AbortRead { stage: transfer }, false)?;
        assert!(f.owner.empty());
        assert!(path.is_file());
    }
    Ok(())
}

mod prepared_authority_tests;
