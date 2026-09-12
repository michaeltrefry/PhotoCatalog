use super::*;
use crate::{
    catalog_edits::{EditRenderIdentity, VariantKey},
    catalog_exports::{MetadataSelection, PhotoExportPlan, StoredOutput},
    catalog_metadata::RenderIdentity,
    edit::{Recipe, RenderLimits},
    image_export::{AlphaPolicy, EncodeLimits, IntegerDepth, OutputFormat, OutputSize},
    media::DecodeLimits,
    metadata_export::{DestinationSnapshot, FileRevision},
    storage_volume::NativePath,
};

fn request(root: &Path) -> Request {
    let recipe = Recipe::default();
    let fingerprint = "a".repeat(64);
    let plan = PhotoExportPlan {
        version: 1,
        // Recovery must not execute or silently upgrade an old renderer's plan.
        renderer_identity: "prior-export-renderer".into(),
        identity: EditRenderIdentity {
            image_identity: None,
            source: RenderIdentity {
                asset_id: "a".into(),
                generation: 1,
                fingerprint: Some(fingerprint.clone()),
                state: "ready".into(),
                metadata_revision: 0,
            },
            key: VariantKey::master("a"),
            revision: 2,
            recipe_digest: recipe.validate().unwrap().digest().into(),
        },
        original: NativePath::from_path(&root.join("original.png")),
        original_revision: FileRevision {
            bytes: 10,
            digest: fingerprint,
            modified_ns: 1,
            identity: (1, 2),
        },
        recipe,
        output: StoredOutput {
            size: OutputSize::Original,
            format: OutputFormat::Png {
                depth: IntegerDepth::Eight,
            },
            profile: StoredProfile::Srgb,
            alpha: AlphaPolicy::Preserve,
        },
        metadata: MetadataSelection::Omit,
        xmp_blob: None,
        destination: DestinationSnapshot {
            version: 1,
            operation: uuid::Uuid::new_v4().to_string(),
            destination: root.join("destination.png"),
            expected: None,
            max_existing_bytes: 1024,
        },
        max_original_bytes: 1024,
        max_payload_bytes: 1024,
        alias_limits: Default::default(),
    };
    let authority = blake3::hash(&serde_json::to_vec(&plan).unwrap())
        .to_hex()
        .to_string();
    Request {
        version: 1,
        work: ExportWork {
            job: "job".into(),
            sequence: 1,
            attempt: uuid::Uuid::new_v4().to_string(),
            authority,
            plan,
        },
        limits: PhotoRenderLimits {
            decode: DecodeLimits {
                max_encoded_bytes: 1024,
                ..DecodeLimits::default()
            },
            render: RenderLimits::default(),
            encode: EncodeLimits::default(),
            max_encoded_extent: 1024,
        },
    }
}
fn stage(root: &Path, request: &Request, with_lock: bool) -> PathBuf {
    let path = root.join(format!("photo-worker-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&path).unwrap();
    write(
        &path.join("request.json"),
        &serde_json::to_vec(request).unwrap(),
    )
    .unwrap();
    if with_lock {
        write(&path.join("active.lock"), b"").unwrap();
    }
    path
}
#[test]
fn recovery_fences_prior_renderer_before_catalog_reconciliation_and_retains_seal() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workers");
    fs::create_dir(&root).unwrap();
    let request = request(temp.path());
    let path = stage(&root, &request, false); // Original process died before lock creation.
    write(&path.join("output"), b"partial output").unwrap();
    let orphan = temp.path().join("durable-orphan-seal");
    write(&orphan, b"parent-owned evidence").unwrap();
    let recovered = recover_export_transports(&root, 2).unwrap();
    assert_eq!(recovered.scanned, 1);
    assert!(recovered.retained.is_empty());
    assert_eq!(recovered.retired.len(), 1);
    let retired = &recovered.retired[0];
    assert!(same_work(&retired.work, &request.work));
    assert!(!path.exists());
    assert!(retired.staging.join("request.json").is_file());
    assert!(retired.staging.join("output").is_file());
    // A crash before catalog fencing preserves the same attempt proof on retry.
    let repeated = recover_export_transports(&root, 2).unwrap();
    assert!(same_work(&repeated.retired[0].work, &request.work));
    discard_retired_export_transport(retired).unwrap();
    assert!(!retired.staging.exists());
    assert_eq!(fs::read(orphan).unwrap(), b"parent-owned evidence");
    assert!(!request.work.plan.destination.destination.exists());
}
#[test]
fn busy_worker_is_retained_and_delayed_open_cannot_cross_retirement() {
    let temp = tempfile::tempdir().unwrap();
    let request = request(temp.path());
    let path = stage(temp.path(), &request, true);
    let live = open_lease(&path).unwrap();
    live.try_lock_exclusive().unwrap();
    let recovery = recover_export_transports(temp.path(), 2).unwrap();
    assert_eq!(recovery.retained.len(), 1);
    assert!(recovery.retired.is_empty());
    assert_eq!(live.metadata().unwrap().len(), 0);
    let contender = open_lease(&path).unwrap();
    assert!(
        contender.try_lock_exclusive().is_err(),
        "failed recovery must not unlock the active owner"
    );
    drop(contender);
    // The simulated worker has completed. Release its authority even if a
    // concurrently spawned test child inherited this description before exec.
    FileExt::unlock(&live).unwrap();
    drop(live);
    // Hold the exact pre-opened handle a delayed child would later lock.
    let delayed = open_lease(&path).unwrap();
    let recovered = recover_export_transports(temp.path(), 2).unwrap();
    delayed.try_lock_exclusive().unwrap();
    assert_eq!(delayed.metadata().unwrap().len(), 1);
    assert!(check_live_lease(&path, &delayed).is_err());
    FileExt::unlock(&delayed).unwrap();
    drop(delayed);
    // Windows may retain a tombstoned path while the delayed handle is open.
    let recovered = if recovered.retired.is_empty() {
        recover_export_transports(temp.path(), 2).unwrap()
    } else {
        recovered
    };
    assert_eq!(recovered.retired.len(), 1);
    discard_retired_export_transport(&recovered.retired[0]).unwrap();
}
#[cfg(unix)]
#[test]
fn completed_recovery_releases_duplicate_description() {
    let temp = tempfile::tempdir().unwrap();
    let request = request(temp.path());
    let path = stage(temp.path(), &request, true);
    let lease = open_lease(&path).unwrap();
    lease.try_lock_exclusive().unwrap();
    let mut lease = AcquiredLease(lease);
    // dup retains the same open-file description as fork before CLOEXEC runs.
    // No process timing, sleep or unsafe fork is needed to preserve that owner.
    let inherited = lease.0.try_clone().unwrap();
    let delayed = open_lease(&path).unwrap();
    assert!(delayed.try_lock_exclusive().is_err());
    mark_retired(&mut lease.0).unwrap();
    drop(lease);
    delayed.try_lock_exclusive().unwrap();
    assert!(check_live_lease(&path, &delayed).is_err());
    assert_eq!(inherited.metadata().unwrap().len(), 1);
    drop(inherited);
    let contender = open_lease(&path).unwrap();
    assert!(
        contender.try_lock_exclusive().is_err(),
        "closing the old description must not release the new owner"
    );
    FileExt::unlock(&delayed).unwrap();
}
#[cfg(unix)]
#[test]
fn recovery_error_and_unwind_release_inherited_description() {
    for unwind in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let request = request(temp.path());
        let path = stage(temp.path(), &request, true);
        let mut inherited = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            let file = open_lease(&path)?;
            file.try_lock_exclusive()?;
            let _lease = AcquiredLease(file);
            inherited = Some(_lease.0.try_clone()?);
            if unwind {
                panic!("injected recovery unwind");
            }
            bail!("injected recovery validation failure")
        }));
        if unwind {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_err());
        }
        let contender = open_lease(&path).unwrap();
        contender.try_lock_exclusive().unwrap();
        assert_eq!(contender.metadata().unwrap().len(), 0);
        FileExt::unlock(&contender).unwrap();
        drop(inherited);
    }
}
#[test]
fn partial_unknown_and_wrong_authority_transports_are_never_fenced() {
    for kind in ["missing-request", "unknown-file", "bad-authority"] {
        let temp = tempfile::tempdir().unwrap();
        let mut request = request(temp.path());
        if kind == "bad-authority" {
            request.work.authority = "b".repeat(64);
        }
        let path = stage(temp.path(), &request, true);
        if kind == "missing-request" {
            fs::remove_file(path.join("request.json")).unwrap();
        }
        if kind == "unknown-file" {
            write(&path.join("user-file"), b"preserve").unwrap();
        }
        let result = recover_export_transports(temp.path(), 1).unwrap();
        assert_eq!(result.retained.len(), 1);
        assert!(result.retired.is_empty());
        assert!(path.exists());
        assert_eq!(fs::metadata(path.join("active.lock")).unwrap().len(), 0);
    }
}
#[test]
fn recovery_bound_and_interrupted_discard_never_invent_work() {
    let temp = tempfile::tempdir().unwrap();
    let request = request(temp.path());
    let first = stage(temp.path(), &request, true);
    let second = stage(temp.path(), &request, true);
    assert!(recover_export_transports(temp.path(), 1).is_err());
    assert!(first.exists() && second.exists());
    let result = recover_export_transports(temp.path(), 2).unwrap();
    assert_eq!(result.retired.len(), 2);
    // Simulate interruption after parent fencing and request removal, then after
    // lock removal. Both paths remain non-launchable by their retired names.
    for (index, retired) in result.retired.iter().enumerate() {
        fs::remove_file(retired.staging.join("request.json")).unwrap();
        if index == 1 {
            fs::remove_file(retired.staging.join("active.lock")).unwrap();
        }
    }
    let resumed = recover_export_transports(temp.path(), 2).unwrap();
    assert_eq!(resumed.cleaned, 2);
    assert!(resumed.retired.is_empty() && resumed.retained.is_empty());
}
#[test]
fn transport_read_admits_exact_length_and_rejects_excess() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("blob");
    write(&path, b"12345678").unwrap();
    assert_eq!(read(&path, 8).unwrap(), b"12345678");
    assert!(read(&path, 7).is_err());
}

#[test]
fn pre_admission_child_entry() {
    let Some(ready) = std::env::var_os("PHOTOCATALOG_EXPORT_TEST_READY") else {
        return;
    };
    fs::write(ready, b"ready").unwrap();
    let mut token = [0];
    let _ = std::io::stdin().read(&mut token);
}
#[test]
fn cancellation_reaps_actual_pre_admission_child_and_retires_missing_lease() {
    let temp = tempfile::tempdir().unwrap();
    let request = request(temp.path());
    let root = temp.path().join("workers");
    fs::create_dir(&root).unwrap();
    let staging = stage(&root, &request, false);
    let ready = temp.path().join("ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "export_worker::tests::pre_admission_child_entry"])
        .env("PHOTOCATALOG_EXPORT_TEST_READY", &ready)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lease = child.stdin.take();
    let mut process = ExportWorkerProcess {
        child,
        lease,
        staging: staging.clone(),
        request,
        exited: false,
    };
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            std::time::Instant::now() < until,
            "test child admission timeout"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(process.poll(&AtomicBool::new(true)).is_err());
    assert!(process.exited);
    assert!(process.child.try_wait().unwrap().is_some());
    process.retire_transport().unwrap();
    assert!(!staging.exists());
    assert!(
        recover_export_transports(&root, 2)
            .unwrap()
            .retired
            .is_empty()
    );
}
