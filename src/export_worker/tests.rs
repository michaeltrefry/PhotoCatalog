use super::*;
use crate::{
    catalog_edits::{EditRenderIdentity, VariantKey},
    catalog_exports::{MetadataSelection, PhotoExportPlan, StoredOutput},
    catalog_metadata::RenderIdentity,
    edit::{Recipe, RenderLimits},
    image_export::{
        AlphaPolicy, EncodeLimits, EncodingReport, IntegerDepth, OutputDescriptor, OutputFormat,
        OutputSize,
    },
    media::DecodeLimits,
    metadata_export::{DestinationSnapshot, FileRevision},
    storage_volume::NativePath,
};

fn rendering_facts(request: &Request, staging: PathBuf, bytes: u64) -> ExportRenderingFacts {
    ExportRenderingFacts {
        job: request.work.job.clone(),
        sequence: request.work.sequence,
        authority: request.work.authority.clone(),
        attempt: request.work.attempt.clone(),
        output: request.work.plan.output.clone(),
        output_revision: metadata_export::inspect_file_revision(&staging, 16 * 1024).unwrap(),
        rendered: StagedPhoto {
            staging,
            encoding: EncodingReport {
                output: OutputDescriptor {
                    width: 1,
                    height: 1,
                    channels: 4,
                    bits_per_sample: 8,
                    floating_point: false,
                    orientation: 1,
                    icc_blake3: "fixture".into(),
                    integer_clips_to_unit_range: true,
                    alpha: AlphaPolicy::Preserve,
                },
                encoded_extent: bytes,
                source_fingerprint: request.work.plan.original_revision.digest.clone(),
                recipe_digest: request.work.plan.identity.recipe_digest.clone(),
                metadata_blake3: "fixture".into(),
                compression: "fixture".into(),
            },
            renderer_identity: request.work.plan.renderer_identity.clone(),
            metadata_notes: Vec::new(),
            timings: crate::photo_render::PhotoRenderTimings {
                source_verification_before_ms: 0.,
                staging_setup_ms: 0.,
                decode_ms: 0.,
                recipe_ms: 0.,
                metadata_ms: 0.,
                encode_ms: 0.,
                source_verification_after_ms: 0.,
                sync_ms: 0.,
                total_ms: 0.,
            },
        },
        peak_resident_bytes: None,
        peak_method: "fixture".into(),
    }
}

fn request(root: &Path) -> Request {
    // DestinationSnapshot compares a normalized parent with the planned path.
    // macOS temp directories may be reached through the /var -> /private/var
    // alias, so fixtures must plan with the canonical spelling used at seal.
    let root = root.canonicalize().unwrap();
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
            plan: crate::catalog_exports::checked_plan(
                &serde_json::to_string(&plan).unwrap(),
                &authority,
            )
            .unwrap(),
            authority,
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

fn native_request(root: &Path) -> Result<(Request, OutputSpec, FileRevision)> {
    const NATIVE_FIXTURE_BYTES: u64 = 8 * 1024 * 1024;
    let original = root.join("original.png");
    image::RgbaImage::from_pixel(8, 6, image::Rgba([20, 40, 80, 255])).save(&original)?;
    let original_revision =
        metadata_export::inspect_file_revision(&original, NATIVE_FIXTURE_BYTES)?;
    let mut request = request(root);
    let mut plan = (*request.work.plan).clone();
    plan.renderer_identity = photo_render::output_renderer_identity().into();
    plan.original = NativePath::from_path(&original);
    plan.original_revision = original_revision.clone();
    plan.identity.source.fingerprint = Some(original_revision.digest.clone());
    // Match the proven PhotoExportService fixture ceiling. The rendered PNG
    // carries profile and metadata chunks, so the synthetic 1 KiB recovery
    // ceiling is not a valid bound for this actual-render fixture.
    plan.max_original_bytes = NATIVE_FIXTURE_BYTES;
    plan.max_payload_bytes = NATIVE_FIXTURE_BYTES;
    let raw = serde_json::to_string(&plan)?;
    let authority = blake3::hash(raw.as_bytes()).to_hex().to_string();
    request.version = 2;
    request.work.authority = authority.clone();
    request.work.plan = crate::catalog_exports::checked_plan(&raw, &authority)?;
    request.limits.decode.max_encoded_bytes = NATIVE_FIXTURE_BYTES;
    request.limits.max_encoded_extent = NATIVE_FIXTURE_BYTES;
    Ok((
        request,
        OutputSpec {
            size: OutputSize::Original,
            format: OutputFormat::Png {
                depth: IntegerDepth::Eight,
            },
            profile: OutputProfile::Srgb,
            alpha: AlphaPolicy::Preserve,
        },
        original_revision,
    ))
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
fn compact_recovery_combines_legacy_and_current_namespaces_before_discard() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let catalog = temp.path().join("catalog-workers");
    let manifest = temp.path().join("manifest-workers");
    fs::create_dir(&catalog)?;
    fs::create_dir(&manifest)?;
    let legacy = request(temp.path());
    let mut current = request(temp.path());
    current.version = 2;
    stage(&catalog, &legacy, true);
    stage(&manifest, &current, true);
    let mut checkpoints = 0;
    let mut checkpoint = || {
        checkpoints += 1;
        Ok(())
    };
    let recovered = recover_export_transports_compact_with_checkpoint(
        &[catalog.as_path(), manifest.as_path()],
        2,
        &mut checkpoint,
    )?;
    assert_eq!(
        (recovered.scanned, recovered.cleaned, recovered.retained),
        (2, 0, 0)
    );
    assert_eq!(recovered.retired.len(), 2);
    assert!(checkpoints >= 3);
    let versions: Vec<_> = recovered
        .retired
        .iter()
        .map(|retired| retired.attempt.attempt.clone())
        .collect();
    assert!(versions.contains(&legacy.work.attempt));
    assert!(versions.contains(&current.work.attempt));
    for retired in &recovered.retired {
        discard_compact_retired_export_transport(retired)?;
    }
    assert_eq!(fs::read_dir(&catalog)?.count(), 0);
    assert_eq!(fs::read_dir(&manifest)?.count(), 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn compact_discard_revalidates_directory_lock_and_request_identity() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("workers");
    fs::create_dir(&root)?;
    let request = request(temp.path());
    stage(&root, &request, true);
    let recovered =
        recover_export_transports_compact_with_checkpoint(&[root.as_path()], 1, &mut || Ok(()))?;
    let retired = &recovered.retired[0];

    let durable_request = retired.staging.join("request.json");
    let request_bytes = fs::read(&durable_request)?;
    fs::write(&durable_request, b"changed")?;
    assert!(discard_compact_retired_export_transport(retired).is_err());
    assert!(retired.staging.exists());
    fs::write(&durable_request, &request_bytes)?;

    let active = retired.staging.join("active.lock");
    let retained_active = retired.staging.join("active.lock.retained");
    fs::rename(&active, &retained_active)?;
    fs::write(&active, [1])?;
    assert!(discard_compact_retired_export_transport(retired).is_err());
    assert!(retired.staging.exists());
    fs::remove_file(&active)?;
    fs::rename(&retained_active, &active)?;

    let staging = retired.staging.clone();
    let retained_staging = root.join("retained-physical-directory");
    fs::rename(&staging, &retained_staging)?;
    fs::create_dir(&staging)?;
    assert!(discard_compact_retired_export_transport(retired).is_err());
    assert!(staging.exists());
    fs::remove_dir(&staging)?;
    fs::rename(&retained_staging, &staging)?;

    discard_compact_retired_export_transport(retired)?;
    assert!(!staging.exists());
    Ok(())
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
fn parent_seal_requires_exact_fresh_orphan_bytes_and_preserves_strict_readback() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let request = request(temp.path());
    let canceled = AtomicBool::new(false);
    let bytes = b"aaaaaaaa";

    let first_stage = temp.path().join("first-stage");
    fs::create_dir(&first_stage)?;
    write(&first_stage.join("output"), bytes)?;
    let first = rendering_facts(&request, first_stage.join("output"), bytes.len() as u64);
    validate_rendering(&request.work, &first_stage, &first)?;
    let completed = complete_rendering(&request.work, first, &canceled)?;
    assert_eq!(
        read_seal_checked(&request.work, &canceled)?,
        completed.sealed
    );

    let equal_stage = temp.path().join("equal-stage");
    fs::create_dir(&equal_stage)?;
    write(&equal_stage.join("output"), bytes)?;
    let equal = rendering_facts(&request, equal_stage.join("output"), bytes.len() as u64);
    validate_rendering(&request.work, &equal_stage, &equal)?;
    assert_eq!(
        complete_rendering(&request.work, equal, &canceled)?.sealed,
        completed.sealed
    );

    let different_stage = temp.path().join("different-stage");
    fs::create_dir(&different_stage)?;
    write(&different_stage.join("output"), b"bbbbbbbb")?;
    let different = rendering_facts(&request, different_stage.join("output"), bytes.len() as u64);
    validate_rendering(&request.work, &different_stage, &different)?;
    let error = complete_rendering(&request.work, different, &canceled)
        .unwrap_err()
        .to_string();
    assert!(error.contains("orphan seal differs from fresh render"));

    let oversized_stage = temp.path().join("oversized-stage");
    fs::create_dir(&oversized_stage)?;
    write(&oversized_stage.join("output"), &vec![0; 1025])?;
    let oversized = rendering_facts(&request, oversized_stage.join("output"), 1025);
    assert!(validate_rendering(&request.work, &oversized_stage, &oversized).is_err());
    assert_eq!(
        read_seal_checked(&request.work, &canceled)?,
        completed.sealed
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn parent_lease_admission_is_nofollow_nonblocking_and_rechecks_exact_object() -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let temp = tempfile::tempdir()?;
    let stage = temp.path().join("stage");
    fs::create_dir(&stage)?;
    let lease_path = stage.join("parent.lock");
    write(&lease_path, b"")?;
    let held = metadata_export::open_regular(&lease_path)?;
    held.try_lock_exclusive()?;
    fs::remove_file(&lease_path)?;
    write(&lease_path, b"")?;
    assert!(check_parent_lease(&stage, &held).is_err());
    FileExt::unlock(&held)?;
    drop(held);

    fs::remove_file(&lease_path)?;
    let target = stage.join("target");
    write(&target, b"")?;
    std::os::unix::fs::symlink(&target, &lease_path)?;
    assert!(acquire_parent_lease(&stage).is_err());
    fs::remove_file(&lease_path)?;

    let encoded = std::ffi::CString::new(lease_path.as_os_str().as_bytes())?;
    ensure!(
        unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) } == 0,
        "mkfifo failed"
    );
    assert!(acquire_parent_lease(&stage).is_err());
    fs::remove_file(&lease_path)?;
    fs::create_dir(&lease_path)?;
    assert!(acquire_parent_lease(&stage).is_err());
    Ok(())
}

#[test]
#[ignore = "requires a fresh CLI via PHOTOCATALOG_TEST_EXECUTABLE"]
fn actual_render_exit_retains_parent_stage_and_rejects_output_substitution() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    for case in 0..3 {
        let temp = tempfile::tempdir()?;
        let (request, output, original_revision) = native_request(temp.path())?;
        let workers = temp.path().join("workers");
        let recovery = request
            .work
            .plan
            .destination
            .destination
            .parent()
            .unwrap()
            .join(format!(
                ".photocatalog-photo-export-{}",
                request.work.plan.destination.operation
            ));
        let mut worker = ExportWorkerProcess::spawn(
            &executable,
            &workers,
            request.work.clone(),
            &output,
            None,
            request.limits,
        )?;
        worker.wait_for_rendering_exit_for_test()?;
        let receipt: serde_json::Value =
            serde_json::from_slice(&read(&worker.staging.join("result.json"), RECEIPT_LIMIT)?)?;
        assert_eq!(receipt["version"], 3);
        assert!(!recovery.exists());
        let recovered = recover_export_transports(&workers, 1)?;
        assert_eq!(recovered.retained.len(), 1);
        assert!(recovered.retired.is_empty());

        if case == 0 {
            // A valid completed render is still ineligible for sealing after Stop.
            worker.stop()?;
            let error = worker
                .poll(&AtomicBool::new(false))
                .unwrap_err()
                .to_string();
            assert!(error.contains("export worker already consumed"));
        } else {
            let staged = worker.staging.join("output");
            let original_output = fs::read(&staged)?;
            let substituted = vec![original_output[0] ^ 0xff; original_output.len()];
            if case == 2 {
                fs::remove_file(&staged)?;
                write(&staged, &substituted)?;
            } else {
                let mut file = OpenOptions::new().write(true).open(&staged)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&substituted)?;
                file.sync_all()?;
            }
            let error = worker
                .poll(&AtomicBool::new(false))
                .unwrap_err()
                .to_string();
            assert!(error.contains("native output changed before parent sealing"));
        }
        assert!(!recovery.exists());
        worker.retire_transport()?;
        assert!(!worker.staging.exists());
        assert_eq!(
            metadata_export::inspect_file_revision(&temp.path().join("original.png"), 1024)?,
            original_revision
        );
    }
    Ok(())
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
        parent_lease: RefCell::new(None),
        staging: staging.clone(),
        request,
        exited: false,
        consumed: false,
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

#[test]
fn worker_protocols_preserve_legacy_raw_authority_and_reject_future_or_mixed_shapes() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    for version in [1, 2] {
        let mut r = request(temp.path());
        r.version = version;
        let legacy = serde_json::to_string_pretty(&*r.work.plan)?
            .replace("destination.png", "destination\\u002epng");
        let raw = if version == 2 {
            format!(" \n{legacy}\n ")
        } else {
            legacy
        };
        r.work.authority = blake3::hash(raw.as_bytes()).to_hex().to_string();
        r.work.plan = crate::catalog_exports::checked_plan(&raw, &r.work.authority)?;
        let bytes = serde_json::to_vec(&r)?;
        let recovered: Request = serde_json::from_slice(&bytes)?;
        validate_persisted(&recovered)?;
        assert_eq!(recovered.work.plan.raw(), raw);
        assert_eq!(recovered.work.authority, r.work.authority);
        assert!(
            validate(&recovered).is_err(),
            "retired renderer must not gain authority"
        );
        let mut wrong = bytes.clone();
        let mut value: serde_json::Value = serde_json::from_slice(&wrong)?;
        value["version"] = 99.into();
        wrong = serde_json::to_vec(&value)?;
        assert!(serde_json::from_slice::<Request>(&wrong).is_err());
        value["version"] = (if version == 1 { 2 } else { 1 }).into();
        assert!(serde_json::from_slice::<Request>(&serde_json::to_vec(&value)?).is_err());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn native_completion_keeps_non_utf_staging_and_legacy_completion_still_reads() -> Result<()> {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir()?;
    let r = request(temp.path());
    let seal = SealedPhotoExport {
        version: 1,
        snapshot: r.work.plan.destination.clone(),
        authority_digest: r.work.authority.clone(),
        max_payload_bytes: 1024,
        payload: r.work.plan.original_revision.clone(),
    };
    let rendered = serde_json::json!({
        "staging":"legacy-output", "renderer_identity":"prior-export-renderer", "metadata_notes":[],
        "encoding":{"output":{"width":1,"height":1,"channels":3,"bits_per_sample":8,"floating_point":false,"orientation":1,"icc_blake3":"fixture","integer_clips_to_unit_range":true,"alpha":{"mode":"preserve"}},"encoded_extent":10,"source_fingerprint":"a".repeat(64),"recipe_digest":r.work.plan.identity.recipe_digest,"metadata_blake3":"fixture","compression":"png"},
        "timings":{"source_verification_before_ms":0.0,"staging_setup_ms":0.0,"decode_ms":0.0,"recipe_ms":0.0,"metadata_ms":0.0,"encode_ms":0.0,"source_verification_after_ms":0.0,"sync_ms":0.0,"total_ms":0.0}
    });
    let legacy = serde_json::json!({"authority":r.work.authority,"attempt":r.work.attempt,"sealed":seal,"rendered":rendered,"seal_ms":0.0,"peak_resident_bytes":null,"peak_method":"fixture"});
    let mut completion: CompletedExport = serde_json::from_value(legacy.clone())?;
    assert_eq!(completion.rendered.staging, PathBuf::from("legacy-output"));
    let parent = temp
        .path()
        .join(std::ffi::OsString::from_vec(vec![b's', 255]));
    completion.rendered.staging = parent.join("output");
    let json = serde_json::to_vec(&completion)?;
    assert!((json.len() as u64) < RECEIPT_LIMIT);
    write(&temp.path().join("result.json"), &json)?;
    let decoded: CompletedExport =
        serde_json::from_slice(&read(&temp.path().join("result.json"), RECEIPT_LIMIT)?)?;
    assert_eq!(decoded.rendered.staging, completion.rendered.staging);

    let facts = ExportRenderingFacts {
        job: r.work.job.clone(),
        sequence: r.work.sequence,
        authority: r.work.authority.clone(),
        attempt: r.work.attempt.clone(),
        output: r.work.plan.output.clone(),
        output_revision: r.work.plan.original_revision.clone(),
        rendered: decoded.rendered,
        peak_resident_bytes: None,
        peak_method: "fixture".into(),
    };
    validate_rendering(&r.work, &parent, &facts)?;
    let rendering_json = serde_json::to_vec(&facts)?;
    let rendering_value: serde_json::Value = serde_json::from_slice(&rendering_json)?;
    assert_eq!(rendering_value["version"], 3);
    assert!(rendering_value.get("sealed").is_none());
    assert!(rendering_value.get("seal_ms").is_none());
    let wire::DecodedCompletion::Rendering(mut decoded_facts) =
        wire::decode_completion(&rendering_json)?
    else {
        anyhow::bail!("render receipt decoded as legacy completion")
    };
    validate_rendering(&r.work, &parent, &decoded_facts)?;
    decoded_facts.rendered.staging = parent.join("foreign-output");
    assert!(validate_rendering(&r.work, &parent, &decoded_facts).is_err());

    let mut foreign = rendering_value;
    foreign["authority"] = "f".repeat(64).into();
    let wire::DecodedCompletion::Rendering(foreign) =
        wire::decode_completion(&serde_json::to_vec(&foreign)?)?
    else {
        anyhow::bail!("render receipt decoded as legacy completion")
    };
    assert!(validate_rendering(&r.work, &parent, &foreign).is_err());

    let mut invalid: serde_json::Value = serde_json::from_slice(&json)?;
    invalid["version"] = 1.into();
    assert!(serde_json::from_value::<CompletedExport>(invalid).is_err());
    let mut future = legacy;
    future["version"] = 77.into();
    assert!(serde_json::from_value::<CompletedExport>(future).is_err());
    Ok(())
}

#[test]
fn compact_inventory_retains_1024_maximum_plans_only_as_bounded_candidates() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let roots = [temp.path().join("legacy"), temp.path().join("managed")];
    for root in &roots {
        fs::create_dir(root)?;
    }
    let mut request = request(temp.path());
    request.version = 2;
    request.work.job = "j".repeat(128);
    request.work.attempt = "a".repeat(128);
    let raw = format!(
        "{}{}",
        " ".repeat(
            crate::catalog_session::export_stage::PLAN_BYTES - request.work.plan.raw().len()
        ),
        request.work.plan.raw()
    );
    request.work.authority = blake3::hash(raw.as_bytes()).to_hex().to_string();
    request.work.plan = crate::catalog_exports::checked_plan(&raw, &request.work.authority)?;
    assert_eq!(
        request.work.plan.raw().len(),
        crate::catalog_session::export_stage::PLAN_BYTES
    );
    let durable = serde_json::to_vec(&request)?;
    assert!(durable.len() as u64 <= REQUEST_LIMIT);
    for index in 0..1024 {
        stage(&roots[index % 2], &request, true);
    }
    let recovery = recover_export_transports_compact_with_checkpoint(
        &[&roots[0], &roots[1]],
        1024,
        &mut || Ok(()),
    )?;
    assert_eq!(
        (recovery.scanned, recovery.retained, recovery.retired.len()),
        (1024, 0, 1024)
    );
    let mut requested =
        recovery.retired.capacity() * std::mem::size_of::<CompactRetiredExportTransport>();
    for candidate in &recovery.retired {
        candidate.attempt.validate()?;
        assert_eq!(
            (
                candidate.attempt.job.capacity(),
                candidate.attempt.attempt.capacity(),
                candidate.attempt.authority.capacity()
            ),
            (128, 128, 64)
        );
        requested += candidate.staging.capacity()
            + candidate.attempt.job.capacity()
            + candidate.attempt.attempt.capacity()
            + candidate.attempt.authority.capacity()
            + 36;
    }
    let path_backing = 2 * (2 * crate::catalog_session::PATH_UNITS + 8);
    let bound = (3 * 1024 + 8) * std::mem::size_of::<CompactRetiredExportTransport>()
        + 1024 * (path_backing + 128 + 128 + 64 + 36);
    assert!(requested <= bound);
    eprintln!(
        "compact max inventory: entries=1024 plan_bytes={} requested_candidate_backing={requested} retained_bound={bound}",
        raw.len()
    );
    // Every returned candidate traverses both actual production envelopes.
    let fixture = crate::application::desktop::export_native_test_fixture()?;
    let request = crate::catalog_session::export_executor::Request {
        root: fixture.root.clone(),
        executor: fixture.executor.clone(),
        operation: crate::application::U64(2),
        action: crate::catalog_session::export_executor::Action::Recover {
            max_directories: crate::application::U64(1024),
        },
    };
    for candidate in &recovery.retired {
        let reply = crate::catalog_session::export_executor::Reply {
            root: request.root.clone(),
            executor: request.executor.clone(),
            operation: request.operation,
            request_digest: request.digest()?,
            value: crate::catalog_session::export_executor::Value::Recovery {
                scanned: crate::application::U64(1024),
                cleaned: crate::application::U64(0),
                retained: crate::application::U64(0),
                retained_example: None,
                candidate: Some(crate::catalog_session::export_executor::Candidate {
                    token: candidate.token.clone(),
                    attempt: candidate.attempt.clone(),
                }),
            },
        };
        reply.validate(&request)?;
        let bytes = crate::filesystem_worker::wire::encode_outcome(&Ok(
            crate::filesystem_worker::wire::Response::ExportExecutor(reply.clone()),
        ))?;
        assert!(bytes.len() <= crate::filesystem_worker::wire::MESSAGE_BYTES);
        assert!(crate::filesystem_worker::wire::decode_outcome(&bytes)?.is_ok());
        crate::application::desktop::test_export_executor_relay_admission(&request, &reply)?;
    }
    // At the maximum logical inventory, a crash preparing one serialized
    // claim leaves 1025 physical entries but only 1024 transport directories.
    let mut pending = CompactDiscard::test_begin(&recovery.retired[0])?;
    let mut fired = false;
    set_compact_discard_hook(move |phase, _| {
        if phase == "before-claim" && !fired {
            fired = true;
            bail!("maximum inventory interrupted claim intent");
        }
        Ok(())
    });
    assert!(pending.resume(&recovery.retired[0]).is_err());
    drop(pending);
    set_compact_discard_hook(|_, _| Ok(()));
    assert_eq!(
        roots
            .iter()
            .map(|root| fs::read_dir(root).unwrap().count())
            .sum::<usize>(),
        1025
    );
    let again = recover_export_transports_compact_with_checkpoint(
        &[&roots[0], &roots[1]],
        1024,
        &mut || Ok(()),
    )?;
    assert_eq!(
        (again.scanned, again.retained, again.retired.len()),
        (1024, 0, 1024)
    );
    drop(again);
    // No destructive Discard is authorized by inventory alone.
    assert_eq!(
        roots
            .iter()
            .map(|root| fs::read_dir(root).unwrap().count())
            .sum::<usize>(),
        1024
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn compact_discard_atomic_claim_retains_post_validation_substitutions_after_crash() -> Result<()> {
    for kind in ["directory", "active", "request-object", "request-bytes"] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?.join("workers");
        fs::create_dir(&root)?;
        stage(&root, &request(temp.path()), true);
        let recovery =
            recover_export_transports_compact_with_checkpoint(&[&root], 1, &mut || Ok(()))?;
        let candidate = &recovery.retired[0];
        let mut cleanup = CompactDiscard::test_begin(candidate)?;
        let saved = temp.path().canonicalize()?.join("saved-original");
        let bytes = fs::read(candidate.staging.join("request.json"))?;
        let saved_hook = saved.clone();
        let mut changed = false;
        set_compact_discard_hook(move |phase, path| {
            if phase == "before-claim" && !changed {
                changed = true;
                match kind {
                    "directory" => {
                        fs::rename(path, &saved_hook)?;
                        fs::create_dir(path)?;
                        fs::write(path.join("request.json"), &bytes)?;
                        fs::write(path.join("active.lock"), [1])?;
                    }
                    "active" => {
                        fs::rename(path.join("active.lock"), &saved_hook)?;
                        fs::write(path.join("active.lock"), [1])?;
                    }
                    "request-object" => {
                        fs::rename(path.join("request.json"), &saved_hook)?;
                        fs::write(path.join("request.json"), &bytes)?;
                    }
                    _ => {
                        let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
                        value["limits"]["decode"]["max_encoded_bytes"] = serde_json::json!(512);
                        fs::write(path.join("request.json"), serde_json::to_vec(&value)?)?;
                    }
                }
            }
            if phase == "after-claim" {
                fs::create_dir(path)?;
                fs::write(path.join("replacement"), b"preserve original name")?;
            }
            Ok(())
        });
        assert!(cleanup.resume(candidate).is_err(), "{kind}");
        let claim = fs::read_dir(&root)?
            .map(|entry| entry.unwrap().path())
            .find(|path| claim::is_claim(path))
            .unwrap();
        assert!(claim.join("transport/request.json").exists());
        assert!(claim.join("transport/active.lock").exists());
        let claimed_before = fs::read(claim.join("transport/request.json"))?;
        drop(cleanup); // simulate loss of all transient F claim handles
        set_compact_discard_hook(|_, _| Ok(()));
        let recovered =
            recover_export_transports_compact_with_checkpoint(&[&root], 2, &mut || Ok(()))?;
        assert_eq!((recovered.scanned, recovered.retained), (2, 1));
        assert!(recovered.retired.is_empty());
        let diagnostic = recovered.retained_example.unwrap();
        assert!(diagnostic.contains("original=") && diagnostic.contains("claimed="));
        assert_eq!(
            fs::read(claim.join("transport/request.json"))?,
            claimed_before
        );
        assert_eq!(
            fs::read(candidate.staging.join("replacement"))?,
            b"preserve original name"
        );
        if kind != "request-bytes" {
            assert!(saved.exists());
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn compact_discard_verified_private_claim_never_deletes_original_name_replacements() -> Result<()> {
    for phase in [
        "verified-request",
        "verified-active",
        "verified-directory",
        "after-claim-control",
    ] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?.join("workers");
        fs::create_dir(&root)?;
        stage(&root, &request(temp.path()), true);
        let recovered =
            recover_export_transports_compact_with_checkpoint(&[&root], 1, &mut || Ok(()))?;
        let candidate = &recovered.retired[0];
        let mut cleanup = CompactDiscard::test_begin(candidate)?;
        let mut changed = false;
        set_compact_discard_hook(move |at, original| {
            if at == phase && !changed {
                changed = true;
                fs::create_dir(original)?;
                fs::write(original.join("request.json"), b"replacement request")?;
                fs::write(original.join("active.lock"), b"replacement lock")?;
                if phase == "after-claim-control" {
                    bail!("lost completed cleanup acknowledgement");
                }
            }
            Ok(())
        });
        let result = cleanup.resume(candidate);
        if phase == "after-claim-control" {
            assert!(result.is_err());
            cleanup.resume(candidate)?;
        } else {
            result?;
        }
        assert_eq!(
            fs::read(candidate.staging.join("request.json"))?,
            b"replacement request"
        );
        assert_eq!(
            fs::read(candidate.staging.join("active.lock"))?,
            b"replacement lock"
        );
        assert_eq!(fs::read_dir(&root)?.count(), 1);
    }
    set_compact_discard_hook(|_, _| Ok(()));
    Ok(())
}

#[test]
fn compact_discard_claim_crash_recovery_resolves_every_durable_phase() -> Result<()> {
    for phase in [
        "claim-created",
        "claim-scratch",
        "before-claim",
        "after-claim-move",
        "after-claim",
        "claim-verified",
        "after-request",
        "after-active",
        "directory",
        "after-directory",
    ] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?.join("workers");
        fs::create_dir(&root)?;
        stage(&root, &request(temp.path()), true);
        let recovery =
            recover_export_transports_compact_with_checkpoint(&[&root], 1, &mut || Ok(()))?;
        let candidate = &recovery.retired[0];
        let mut cleanup = CompactDiscard::test_begin(candidate)?;
        let mut fired = false;
        set_compact_discard_hook(move |at, _| {
            if at == phase && !fired {
                fired = true;
                bail!("simulated crash at {phase}");
            }
            Ok(())
        });
        assert!(cleanup.resume(candidate).is_err(), "{phase}");
        drop(cleanup);
        set_compact_discard_hook(|_, _| Ok(()));
        let recovered =
            recover_export_transports_compact_with_checkpoint(&[&root], 1, &mut || Ok(()))?;
        assert_eq!(
            recovered.retained, 0,
            "{phase}: {:?}",
            recovered.retained_example
        );
        let before_move = matches!(phase, "claim-created" | "claim-scratch" | "before-claim");
        assert_eq!(recovered.retired.len(), usize::from(before_move), "{phase}");
        assert_eq!(
            fs::read_dir(&root)?.count(),
            usize::from(before_move),
            "{phase}"
        );
        assert!(fs::read_dir(&root)?.all(|entry| !claim::is_claim(&entry.unwrap().path())));
    }
    Ok(())
}

#[cfg(windows)]
#[test]
fn held_directory_rename_uses_delete_capable_handles_and_refuses_replacement() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let parent = discard_directory(&root)?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    let source_handle = discard_directory(&source)?;
    let source_identity = lease_identity(&source_handle)?;

    rename_directory_held(&source_handle, &parent, std::ffi::OsStr::new("moved"))?;
    assert!(!source.try_exists()?);
    assert_eq!(
        lease_identity(&discard_directory(&root.join("moved"))?)?,
        source_identity
    );

    let collision_source = root.join("collision-source");
    let collision_target = root.join("collision-target");
    fs::create_dir(&collision_source)?;
    fs::create_dir(&collision_target)?;
    let collision_source_handle = discard_directory(&collision_source)?;
    let collision_source_identity = lease_identity(&collision_source_handle)?;
    let collision_target_identity = lease_identity(&discard_directory(&collision_target)?)?;
    assert!(
        rename_directory_held(
            &collision_source_handle,
            &parent,
            std::ffi::OsStr::new("collision-target")
        )
        .is_err()
    );
    assert_eq!(
        lease_identity(&discard_directory(&collision_source)?)?,
        collision_source_identity
    );
    assert_eq!(
        lease_identity(&discard_directory(&collision_target)?)?,
        collision_target_identity
    );
    Ok(())
}

#[test]
fn compact_discard_unknown_or_extra_claim_controls_block_all_fencing() -> Result<()> {
    for kind in [
        "missing-record",
        "changed-record",
        "extra-artifact",
        "extra-empty-intent",
        "missing-proof",
        "oversized-record",
        "invalid-progress",
    ] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?.join("workers");
        fs::create_dir(&root)?;
        stage(&root, &request(temp.path()), true);
        let recovered =
            recover_export_transports_compact_with_checkpoint(&[&root], 1, &mut || Ok(()))?;
        let candidate = &recovered.retired[0];
        let mut cleanup = CompactDiscard::test_begin(candidate)?;
        let mut fired = false;
        set_compact_discard_hook(move |phase, _| {
            if phase == "after-claim" && !fired {
                fired = true;
                bail!("interrupt selected claim before validation");
            }
            Ok(())
        });
        assert!(cleanup.resume(candidate).is_err());
        drop(cleanup);
        set_compact_discard_hook(|_, _| Ok(()));
        let wrapper = fs::read_dir(&root)?.next().unwrap()?.path();
        match kind {
            "missing-record" => fs::remove_file(wrapper.join("claim.json"))?,
            "changed-record" => {
                fs::write(wrapper.join("claim.json"), b"broken durable provenance")?
            }
            "extra-artifact" => fs::write(wrapper.join("foreign"), b"preserve")?,
            "oversized-record" => fs::write(
                wrapper.join("claim.json"),
                vec![b' '; claim::RECORD_BYTES + 1],
            )?,
            "invalid-progress" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(wrapper.join("claim.json"))?)?;
                value["removing"] = serde_json::json!(256);
                fs::write(wrapper.join("claim.json"), serde_json::to_vec(&value)?)?;
            }
            "extra-empty-intent" => {
                let extra = root.join(format!(".f-{}", uuid::Uuid::new_v4()));
                #[cfg(unix)]
                let mut builder = fs::DirBuilder::new();
                #[cfg(not(unix))]
                let builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(extra)?;
            }
            _ => fs::remove_file(wrapper.join("transport/request.json"))?,
        }
        let live = stage(&root, &request(temp.path()), true);
        let before = fs::read(wrapper.join("transport/active.lock"))?;
        let result =
            recover_export_transports_compact_with_checkpoint(&[&root], 4, &mut || Ok(()))?;
        assert_eq!(result.retained, 1, "{kind}");
        assert!(result.retired.is_empty());
        assert!(live.exists());
        assert_eq!(fs::read(live.join("active.lock"))?, b"");
        assert_eq!(fs::read(wrapper.join("transport/active.lock"))?, before);
    }
    Ok(())
}
