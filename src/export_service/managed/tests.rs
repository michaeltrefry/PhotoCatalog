use super::*;
use crate::{
    catalog_edits::VariantKey,
    catalog_exports::{ExportTarget, MetadataSelection},
    image_export::{AlphaPolicy, IntegerDepth, OutputFormat, OutputProfile, OutputSize},
    preview::{Layout, PreviewPolicy, ServiceLimits, StoreConfig},
    storage_volume::NativePath,
};

pub(crate) fn limits() -> ExportServiceLimits {
    let render = crate::edit::RenderLimits {
        max_pixels: 1024,
        max_allocation_bytes: 1024 * 1024,
        max_live_bytes: 1024 * 1024,
    };
    ExportServiceLimits {
        worker_bytes: 2 * 1024 * 1024,
        working_bytes: 2 * 1024 * 1024,
        render: crate::photo_render::PhotoRenderLimits {
            decode: crate::media::DecodeLimits {
                max_encoded_bytes: 1024 * 1024,
                max_intermediate_pixels: 1024,
                max_allocation_bytes: 1024 * 1024,
            },
            render,
            encode: crate::image_export::EncodeLimits {
                render,
                ..Default::default()
            },
            max_encoded_extent: 1024 * 1024,
        },
    }
}

pub(crate) fn previews(base: &std::path::Path) -> Result<PreviewService> {
    PreviewService::open(
        StoreConfig {
            manifest_root: base.join("preview-manifest"),
            thumbnail_root: base.join("preview-thumbnail"),
            large_root: base.join("preview-large"),
            layout: Layout::Flat,
            thumbnail_bytes: 1024 * 1024,
            large_bytes: 1024 * 1024,
        },
        &[],
        std::env::current_exe()?,
        PreviewPolicy::default(),
        ServiceLimits::default(),
    )
}

pub(crate) fn enqueue(catalog: &mut Catalog, base: &std::path::Path, name: &str) -> Result<String> {
    let original = base.join(format!("{name}-original.bin"));
    std::fs::write(&original, b"managed C original")?;
    let fingerprint = blake3::hash(b"managed C original").to_hex().to_string();
    catalog.db.execute("INSERT INTO assets(id,location,path_display,state,fingerprint,preview_hash,metadata) VALUES(?1,?2,?3,'ready',?4,'fixture','{\"format\":\"PNG\",\"width\":1,\"height\":1,\"orientation\":1,\"camera_make\":null,\"camera_model\":null,\"captured_at\":null,\"preview_source\":\"fixture\"}')", rusqlite::params![name, crate::location_bytes(&original), original.to_string_lossy(), fingerprint])?;
    catalog.record_storage_path(name, &NativePath::from_path(&original))?;
    let job = catalog.begin_photo_export()?;
    catalog.append_photo_export(
        &job.id,
        0,
        &ExportTarget {
            key: VariantKey::master(name),
            expected_revision: 0,
            destination: base.join(format!("{name}-published.png")),
            overwrite: false,
            metadata: MetadataSelection::Omit,
        },
        &crate::image_export::OutputSpec {
            size: OutputSize::Original,
            format: OutputFormat::Png {
                depth: IntegerDepth::Eight,
            },
            profile: OutputProfile::Srgb,
            alpha: AlphaPolicy::Preserve,
        },
        1024,
        1024,
    )?;
    catalog.seal_photo_export_job(&job.id, 1)?;
    Ok(job.id)
}

#[test]
fn managed_export_c_success_and_unknown_register_begin_spawn_start_replay() -> Result<()> {
    for (index, fault) in [
        None,
        Some("register"),
        Some("begin"),
        Some("spawn"),
        Some("start"),
    ]
    .into_iter()
    .enumerate()
    {
        let temp = tempfile::tempdir()?;
        let (mut session, control) =
            crate::catalog_session::managed_export_runtime_session(temp.path())?;
        control.drain_native_on_status(true);
        match fault {
            Some("begin") => control.lose_stage(),
            Some(action) => control.lose_native(action),
            None => {}
        }
        let catalog = session.catalog.as_mut().unwrap();
        let name = format!("managed-{index}");
        let job = enqueue(catalog, temp.path(), &name)?;
        let mut previews = previews(temp.path())?;
        let mut service = ExportService::open(catalog, limits())?;
        assert!(service.recover(catalog, 32)?.complete);
        let canceled = AtomicBool::new(false);
        let mut started = false;
        let mut published = false;
        let mut injected_errors = 0;
        for _ in 0..64 {
            match service.tick(catalog, &mut previews, &job, &canceled) {
                Ok(ExportEvent::Started { pid: 42, .. }) => started = true,
                Ok(ExportEvent::Published { .. }) => {
                    published = true;
                    break;
                }
                Ok(ExportEvent::Failed { detail, .. }) => {
                    anyhow::bail!("managed C fixture failed: {detail}")
                }
                Ok(_) => {}
                Err(_) => {
                    injected_errors += 1;
                    if fault == Some("register") {
                        assert_eq!(service.reserved_bytes(), limits().worker_bytes);
                    }
                }
            }
        }
        assert!(started && published);
        assert_eq!(injected_errors, usize::from(fault.is_some()));
        assert!(!service.is_active());
        assert_eq!(service.reserved_bytes(), 0);
        assert_eq!(service.take_completion_metrics().unwrap().worker_pid, 42);
        assert_eq!(
            std::fs::read(temp.path().join(format!("{name}-published.png")))?,
            b"x"
        );
        assert_eq!(catalog.photo_export_job(&job)?.state, "complete");
        if matches!(fault, Some("register" | "spawn" | "start")) {
            assert!(
                control
                    .native_request_bytes()?
                    .windows(2)
                    .any(|pair| pair[0] == pair[1])
            );
        }
        if fault == Some("begin") {
            assert!(
                control
                    .stage_request_digests()?
                    .windows(2)
                    .any(|pair| pair[0] == pair[1])
            );
        }
        service.close()?;
        drop(service);
        drop(previews);
        session.close()?;
    }
    Ok(())
}

#[test]
fn managed_export_c_lost_seal_reply_is_replayed_by_worker_yield_cleanup() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (mut session, control) =
        crate::catalog_session::managed_export_runtime_session(temp.path())?;
    control.drain_native_on_status(true);
    let catalog = session.catalog.as_mut().unwrap();
    let job = enqueue(catalog, temp.path(), "lost-seal")?;
    let mut previews = previews(temp.path())?;
    let mut service = ExportService::open(catalog, limits())?;
    assert!(service.recover(catalog, 32)?.complete);
    let canceled = AtomicBool::new(false);

    for _ in 0..32 {
        let event = service.tick(catalog, &mut previews, &job, &canceled)?;
        assert!(!matches!(event, ExportEvent::Failed { .. }));
        if service
            .active
            .as_ref()
            .is_some_and(|active| active.phase == Phase::Seal)
        {
            break;
        }
    }
    assert_eq!(service.active.as_ref().unwrap().phase, Phase::Seal);

    control.lose_stage();
    assert!(
        service
            .tick(catalog, &mut previews, &job, &canceled)
            .is_err()
    );
    assert!(service.active.as_ref().unwrap().attempt.pending_stage());

    // This is the exact cleanup call made by application::exports::worker
    // after tick_detached propagates the unknown reply.
    let mut published = false;
    for _ in 0..16 {
        match service.yield_to_previews(catalog) {
            Ok(ExportEvent::Published { sequence: 1, .. }) => {
                published = true;
                break;
            }
            Err(error)
                if error
                    .downcast_ref::<super::super::PendingExport>()
                    .is_some() => {}
            other => anyhow::bail!("unexpected yield result: {other:?}"),
        }
    }
    assert!(published);
    let stage_digests = control.stage_request_digests()?;
    assert!(stage_digests.len() >= 2);
    assert!(stage_digests.windows(2).any(|pair| pair[0] == pair[1]));
    assert_eq!(catalog.photo_export_job(&job)?.state, "complete");
    assert_eq!(
        std::fs::read(temp.path().join("lost-seal-published.png"))?,
        b"x"
    );
    assert!(!service.is_active());
    assert_eq!(service.reserved_bytes(), 0);

    service.close()?;
    drop(service);
    drop(previews);
    session.close()?;
    Ok(())
}

fn until_idle(
    service: &mut ExportService,
    catalog: &mut Catalog,
    previews: &mut PreviewService,
    job: &str,
    cancel: &AtomicBool,
) -> Result<ExportEvent> {
    for _ in 0..100 {
        match service.tick(catalog, previews, job, cancel) {
            Ok(
                event @ (ExportEvent::Published { .. }
                | ExportEvent::Failed { .. }
                | ExportEvent::Yielded { .. }),
            ) => return Ok(event),
            Ok(_) => {}
            Err(error) if crate::export_service::pending_export(&error) => {}
            Err(error) => return Err(error),
        }
    }
    anyhow::bail!("managed C fixture did not settle")
}

#[test]
fn managed_export_c_terminal_release_failure_never_readmits_seal() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (mut session, facts) = crate::catalog_session::managed_export_runtime_session(temp.path())?;
    facts.drain_native_on_status(true);
    facts.reject_release();
    let catalog = session.catalog.as_mut().unwrap();
    let job = enqueue(catalog, temp.path(), "failed-release")?;
    let mut previews = previews(temp.path())?;
    let mut service = ExportService::open(catalog, limits())?;
    service.recover(catalog, 32)?;
    assert!(matches!(
        until_idle(
            &mut service,
            catalog,
            &mut previews,
            &job,
            &AtomicBool::new(false)
        )?,
        ExportEvent::Published { .. }
    ));
    let requests = facts.stage_requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(r.action, export_stage::Action::ResultAndSeal))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(r.action, export_stage::Action::Release))
            .count(),
        2
    );
    assert_eq!(
        facts
            .native_requests()
            .iter()
            .filter(|r| matches!(r.action, export_native::Action::RetryDrain))
            .count(),
        1
    );
    assert_eq!(catalog.photo_export_job(&job)?.state, "complete");
    service.close()?;
    drop(service);
    drop(previews);
    session.close()?;
    Ok(())
}

#[test]
fn managed_export_c_failed_spawn_and_rejected_registration_settle_and_close() -> Result<()> {
    for refused_register in [false, true] {
        for cancel in [false, true] {
            let temp = tempfile::tempdir()?;
            let (mut session, facts) =
                crate::catalog_session::managed_export_runtime_session(temp.path())?;
            if refused_register {
                facts.reject_register();
            } else {
                facts.fail_spawn();
            }
            facts.drain_native_on_status(true);
            let catalog = session.catalog.as_mut().unwrap();
            let job = enqueue(catalog, temp.path(), "refused")?;
            let mut previews = previews(temp.path())?;
            let mut service = ExportService::open(catalog, limits())?;
            service.recover(catalog, 32)?;
            let canceled = AtomicBool::new(false);
            service.tick(catalog, &mut previews, &job, &canceled)?;
            service.tick(catalog, &mut previews, &job, &canceled)?;
            if cancel {
                canceled.store(true, std::sync::atomic::Ordering::Release);
            }
            assert!(matches!(
                until_idle(&mut service, catalog, &mut previews, &job, &canceled)?,
                ExportEvent::Failed { .. }
            ));
            assert!(
                !facts
                    .native_requests()
                    .iter()
                    .any(|r| matches!(r.action, export_native::Action::Start))
            );
            assert_eq!(service.reserved_bytes(), 0);
            assert!(ExportService::open(catalog, limits()).is_err());
            service.close()?;
            let mut successor = ExportService::open(catalog, limits())?;
            successor.close()?;
            drop((successor, service, previews));
            session.close()?;
        }
    }
    Ok(())
}

#[test]
fn managed_export_c_service_claim_survives_success_yield_drain_and_idle() -> Result<()> {
    for finish in ["success", "yield", "drain"] {
        let temp = tempfile::tempdir()?;
        let (mut session, facts) =
            crate::catalog_session::managed_export_runtime_session(temp.path())?;
        facts.drain_native_on_status(true);
        let catalog = session.catalog.as_mut().unwrap();
        let job = enqueue(catalog, temp.path(), finish)?;
        let mut previews = previews(temp.path())?;
        let mut service = ExportService::open(catalog, limits())?;
        service.recover(catalog, 32)?;
        assert!(ExportService::open(catalog, limits()).is_err());
        service.tick(catalog, &mut previews, &job, &AtomicBool::new(false))?;
        if finish == "success" {
            assert!(matches!(
                until_idle(
                    &mut service,
                    catalog,
                    &mut previews,
                    &job,
                    &AtomicBool::new(false)
                )?,
                ExportEvent::Published { .. }
            ));
        } else {
            for _ in 0..32 {
                let result = if finish == "yield" {
                    service.yield_to_previews(catalog).map(|_| ())
                } else {
                    service.drain_native()
                };
                match result {
                    Ok(()) => break,
                    Err(e) if crate::export_service::pending_export(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        assert!(!service.is_active());
        assert_eq!(service.reserved_bytes(), 0);
        assert!(ExportService::open(catalog, limits()).is_err());
        if finish == "drain" {
            assert!(
                catalog
                    .photo_export_attempt_if_rendering(&job, 1)?
                    .is_some()
            );
            assert_eq!(service.recover(catalog, 32)?.fenced, 1);
        }
        assert_eq!(
            facts
                .executor_requests()
                .iter()
                .filter(|r| matches!(r.action, export_executor::Action::Acquire))
                .count(),
            1
        );
        assert!(
            !facts
                .executor_requests()
                .iter()
                .any(|r| matches!(r.action, export_executor::Action::Release))
        );
        service.close()?;
        drop((service, previews));
        session.close()?;
    }
    Ok(())
}

#[test]
fn managed_export_c_plan_limit_failure_settles_exact_claim_before_register() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (mut session, facts) = crate::catalog_session::managed_export_runtime_session(temp.path())?;
    let catalog = session.catalog.as_mut().unwrap();
    let job = enqueue(catalog, temp.path(), "over-limit")?;
    let mut bounded = limits();
    bounded.render.max_encoded_extent = 512;
    let mut service = ExportService::open(catalog, bounded)?;
    service.recover(catalog, 32)?;
    let mut previews = previews(temp.path())?;
    let failed = until_idle(
        &mut service,
        catalog,
        &mut previews,
        &job,
        &AtomicBool::new(false),
    )?;
    assert!(
        matches!(failed, ExportEvent::Failed { ref detail, .. } if detail.contains("export plan exceeds configured staging allowance")),
        "{failed:?}"
    );
    assert!(facts.native_requests().is_empty());
    assert!(facts.stage_requests().is_empty());
    assert!(catalog.rendering_photo_export_attempts(200)?.is_empty());
    assert!(!service.is_active());
    service.close()?;
    drop((service, previews));
    session.close()?;
    Ok(())
}

pub(crate) fn set_blobs(
    catalog: &mut Catalog,
    job: &str,
    icc: Option<&[u8]>,
    xmp: Option<&[u8]>,
    linear: bool,
) -> Result<()> {
    use std::io::Write;
    let raw: String = catalog.db.query_row(
        "SELECT plan FROM photo_export_items WHERE job=?1 AND sequence=1",
        [job],
        |r| r.get(0),
    )?;
    let mut plan: crate::catalog_exports::PhotoExportPlan = serde_json::from_str(&raw)?;
    let mut store = |bytes: &[u8]| -> Result<String> {
        let hash = blake3::hash(bytes).to_hex().to_string();
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes)?;
        catalog.db.execute(
            "INSERT OR IGNORE INTO photo_export_blobs(hash,raw_length,compressed) VALUES(?1,?2,?3)",
            rusqlite::params![hash, bytes.len() as i64, encoder.finish()?],
        )?;
        Ok(hash)
    };
    plan.output.profile = match icc {
        Some(bytes) => crate::catalog_exports::StoredProfile::Icc {
            blob: store(bytes)?,
        },
        None if linear => crate::catalog_exports::StoredProfile::LinearSrgb,
        None => crate::catalog_exports::StoredProfile::Srgb,
    };
    plan.xmp_blob = xmp.map(&mut store).transpose()?;
    let raw = serde_json::to_string(&plan)?;
    let authority = blake3::hash(raw.as_bytes()).to_hex().to_string();
    crate::catalog_exports::checked_plan(&raw, &authority)?;
    catalog.db.execute(
        "UPDATE photo_export_items SET plan=?1,authority=?2 WHERE job=?3 AND sequence=1",
        rusqlite::params![raw, authority, job],
    )?;
    Ok(())
}

#[test]
fn managed_export_c_blob_boundaries_and_linear_profile_preserve_exact_requests() -> Result<()> {
    for (icc_size, xmp_size, linear) in [
        (None, None, false),
        (None, None, true),
        (Some(0), Some(0), false),
        (Some(16_384), Some(16_385), false),
        (Some(16_385), Some(16_384), false),
        (Some(export_stage::BLOB_BYTES as usize), None, false),
        (None, Some(export_stage::BLOB_BYTES as usize), false),
        (
            Some(export_stage::BLOB_BYTES as usize),
            Some(export_stage::BLOB_BYTES as usize),
            false,
        ),
    ] {
        let temp = tempfile::tempdir()?;
        let (mut session, facts) =
            crate::catalog_session::managed_export_runtime_session(temp.path())?;
        facts.drain_native_on_status(true);
        let catalog = session.catalog.as_mut().unwrap();
        let job = enqueue(catalog, temp.path(), "blobs")?;
        let icc = icc_size.map(|n| vec![0x5a; n]);
        let xmp = xmp_size.map(|n| vec![0x6b; n]);
        set_blobs(catalog, &job, icc.as_deref(), xmp.as_deref(), linear)?;
        let mut service = ExportService::open(catalog, limits())?;
        service.recover(catalog, 32)?;
        let mut previews = previews(temp.path())?;
        let mut published = false;
        for _ in 0..(2 * export_stage::BLOB_BYTES as usize / export_stage::CHUNK_BYTES + 64) {
            match service.tick(catalog, &mut previews, &job, &AtomicBool::new(false))? {
                ExportEvent::Published { .. } => {
                    published = true;
                    break;
                }
                ExportEvent::Failed { detail, .. } => {
                    anyhow::bail!("blob transport failed: {detail}")
                }
                _ => {}
            }
        }
        assert!(published);
        let requests = facts.stage_requests();
        for (is_icc, expected) in [(true, icc.as_deref()), (false, xmp.as_deref())] {
            let mut offset = 0usize;
            for request in &requests {
                let chunk = match &request.action {
                    export_stage::Action::UploadIcc { offset, bytes } if is_icc => {
                        Some((offset.0 as usize, bytes))
                    }
                    export_stage::Action::UploadXmp { offset, bytes } if !is_icc => {
                        Some((offset.0 as usize, bytes))
                    }
                    _ => None,
                };
                if let Some((actual, bytes)) = chunk {
                    assert_eq!(actual, offset);
                    assert!(bytes.len() <= export_stage::CHUNK_BYTES);
                    assert_eq!(
                        bytes.as_slice(),
                        &expected.unwrap()[offset..offset + bytes.len()]
                    );
                    offset += bytes.len();
                }
            }
            assert_eq!(offset, expected.map_or(0, |b| b.len()));
        }
        let ready = requests
            .iter()
            .find_map(|r| {
                if let export_stage::Action::Ready { icc, xmp } = &r.action {
                    Some((icc, xmp))
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(ready.0, &blob(icc.as_deref()));
        assert_eq!(ready.1, &blob(xmp.as_deref()));
        let begin = requests
            .iter()
            .find(|r| matches!(r.action, export_stage::Action::Begin { .. }))
            .unwrap();
        let registrations = facts.native_requests();
        let export_native::Action::Register {
            begin: registered, ..
        } = &registrations[0].action
        else {
            unreachable!()
        };
        assert_eq!(begin.digest()?, registered.digest()?);
        service.close()?;
        drop((service, previews));
        session.close()?;
    }
    Ok(())
}

fn persisted_recovery_transport(
    root: &std::path::Path,
    work: &crate::catalog_exports::ExportWork,
    version: u64,
) -> Result<std::path::PathBuf> {
    let path = root.join(format!("photo-worker-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path)?;
    std::fs::write(path.join("active.lock"), [])?;
    let mut render = limits().render;
    render.max_encoded_extent = work.plan.max_payload_bytes;
    render.decode.max_encoded_bytes = work.plan.max_original_bytes;
    let bytes = crate::export_worker::prepare_managed_request(work, render, &path)?;
    let bytes = if version == 1 {
        // V1 embeds ExportWork; V2 uses work.plan_json. Serialize the original
        // RawValue directly so its authority digest retains exact plan bytes.
        #[derive(serde::Serialize)]
        struct Legacy<'a> {
            version: u64,
            work: &'a crate::catalog_exports::ExportWork,
            limits: crate::photo_render::PhotoRenderLimits,
        }
        serde_json::to_vec(&Legacy {
            version,
            work,
            limits: render,
        })?
    } else {
        assert_eq!(version, 2);
        bytes
    };
    std::fs::write(path.join("request.json"), bytes)?;
    Ok(path)
}

#[test]
fn managed_export_c_real_recovery_combines_retained_stale_matching_and_exact_discard() -> Result<()>
{
    for (drop_reopen, legacy_matches) in
        [(false, true), (false, false), (true, true), (true, false)]
    {
        let temp = tempfile::tempdir()?;
        let (mut session, facts) =
            crate::catalog_session::managed_export_runtime_session(temp.path())?;
        facts.real_executor();
        let catalog = session.catalog.as_mut().unwrap();
        let job = enqueue(catalog, temp.path(), "recovery")?;
        let work = catalog.claim_photo_export(&job)?.unwrap();
        let mut service = ExportService::open(catalog, limits())?;
        let before_bounds = facts.executor_requests();
        assert!(service.recover(catalog, 0).is_err());
        assert!(service.recover(catalog, 1025).is_err());
        assert_eq!(facts.executor_requests(), before_bounds);
        let legacy = temp.path().join("catalog/photo-export-workers");
        let managed = temp.path().join("cache/export-workers");
        let mut legacy_plan: crate::catalog_exports::PhotoExportPlan =
            serde_json::from_str(work.plan.raw())?;
        legacy_plan.version = 1;
        legacy_plan.destination.version = 1;
        legacy_plan.identity.image_identity = None;
        let legacy_raw = serde_json::to_string(&legacy_plan)?;
        let legacy_authority = blake3::hash(legacy_raw.as_bytes()).to_hex().to_string();
        let mut legacy_work = work.clone();
        legacy_work.plan = crate::catalog_exports::checked_plan(&legacy_raw, &legacy_authority)?;
        legacy_work.authority = legacy_authority;
        let mut modern_work = work.clone();
        if legacy_matches {
            catalog.db.execute(
                "UPDATE photo_export_items SET plan=?1,authority=?2 WHERE job=?3 AND sequence=1",
                rusqlite::params![legacy_raw, legacy_work.authority, job],
            )?;
            modern_work.attempt = uuid::Uuid::new_v4().to_string();
        } else {
            legacy_work.attempt = uuid::Uuid::new_v4().to_string();
        }
        let legacy_path = persisted_recovery_transport(&legacy, &legacy_work, 1)?;
        let modern_path = persisted_recovery_transport(&managed, &modern_work, 2)?;
        let retained = managed.join("unrecognized-directory");
        std::fs::create_dir(&retained)?;
        let retained_error = service
            .recover(catalog, 32)
            .err()
            .context("retained inventory unexpectedly succeeded")?;
        assert!(
            format!("{retained_error:#}").contains("export transports require attention"),
            "{retained_error:#}"
        );
        assert!(
            catalog
                .photo_export_attempt_if_rendering(&job, 1)?
                .is_some()
        );
        assert!(
            !facts
                .executor_requests()
                .iter()
                .any(|r| matches!(r.action, export_executor::Action::Discard { .. }))
        );
        std::fs::remove_dir(&retained)?;
        let cancel = Arc::new(AtomicBool::new(false));
        facts.lose_executor("discard");
        facts.cancel_discard(cancel.clone());
        let lost_error = service
            .recover_cancellable(catalog, 32, &mut ExportControl::new(&cancel))
            .err()
            .context("expected lost Discard acknowledgement")?;
        assert!(
            format!("{lost_error:#}")
                .contains("injected lost real executor discard acknowledgement"),
            "{lost_error:#}"
        );
        assert_eq!(
            service.recovery_candidate.as_ref().unwrap().attempt.attempt,
            legacy_work.attempt
        );
        assert_eq!(
            catalog
                .photo_export_attempt_if_rendering(&job, 1)?
                .is_none(),
            legacy_matches
        );
        let first = facts
            .executor_requests()
            .into_iter()
            .find(|r| matches!(r.action, export_executor::Action::Discard { .. }))
            .unwrap();
        if drop_reopen {
            // Drop closes by reconciling the retained exact Discard first. A
            // successor inventories what remains and must not requeue twice.
            drop(service);
            service = ExportService::open(catalog, limits())?;
            cancel.store(false, std::sync::atomic::Ordering::Release);
            let result = service.recover(catalog, 32)?;
            assert!(result.complete);
            assert_eq!(
                result.fenced,
                usize::from(!legacy_matches),
                "successor counts only a still-matching candidate"
            );
        } else {
            // The canceled retry must still replay the admitted Discard before
            // stopping ahead of the next candidate's SQL decision.
            assert!(
                service
                    .recover_cancellable(catalog, 32, &mut ExportControl::new(&cancel))
                    .is_err()
            );
            let requests = facts.executor_requests();
            let discards: Vec<_> = requests
                .iter()
                .filter(|r| matches!(r.action, export_executor::Action::Discard { .. }))
                .collect();
            assert_eq!(discards[0], discards[1]);
            cancel.store(false, std::sync::atomic::Ordering::Release);
            let result = service.recover(catalog, 32)?;
            assert!(result.complete);
            assert_eq!(result.fenced, 1);
        }
        let requests = facts.executor_requests();
        assert!(requests.iter().filter(|r| **r == first).count() >= 2);
        assert!(!legacy_path.exists() && !modern_path.exists());
        assert!(catalog.rendering_photo_export_attempts(200)?.is_empty());
        assert_eq!(catalog.photo_export_items(&job, 0, 2)?[0].state, "pending");
        service.close()?;
        drop(service);
        session.close()?;
    }
    Ok(())
}

fn recovery_population(
    catalog: &mut Catalog,
    base: &std::path::Path,
    count: i64,
) -> Result<(String, Vec<crate::catalog_exports::ExportWork>)> {
    let job = enqueue(catalog, base, "page")?;
    let template = catalog.claim_photo_export(&job)?.unwrap();
    let template_plan: crate::catalog_exports::PhotoExportPlan =
        serde_json::from_str(template.plan.raw())?;
    let mut rows = Vec::new();
    for sequence in 1..=count {
        let mut plan = template_plan.clone();
        plan.destination = crate::metadata_export::snapshot_photo_destination(
            &base.join(format!("page-{sequence}.png")),
            plan.max_payload_bytes,
        )?;
        let raw = serde_json::to_string(&plan)?;
        let authority = blake3::hash(raw.as_bytes()).to_hex().to_string();
        let work = crate::catalog_exports::ExportWork {
            job: job.clone(),
            sequence,
            attempt: uuid::Uuid::new_v4().to_string(),
            authority: authority.clone(),
            plan: crate::catalog_exports::checked_plan(&raw, &authority)?,
        };
        catalog.db.execute("INSERT OR REPLACE INTO photo_export_items(job,sequence,destination,plan,authority,state,attempt) VALUES(?1,?2,?3,?4,?5,'rendering',?6)", rusqlite::params![job,sequence,serde_json::to_string(&NativePath::from_path(&plan.destination.destination))?,raw,authority,work.attempt])?;
        rows.push(work);
    }
    catalog.db.execute(
        "UPDATE photo_export_jobs SET total=?1 WHERE id=?2",
        rusqlite::params![count, job],
    )?;
    Ok((job, rows))
}

#[test]
fn managed_export_c_real_recovery_pages_200_201_401_rendering_and_publication() -> Result<()> {
    for count in [200, 201, 401] {
        for publication in [false, true] {
            let temp = tempfile::tempdir()?;
            let (mut session, facts) =
                crate::catalog_session::managed_export_runtime_session(temp.path())?;
            facts.real_executor();
            let catalog = session.catalog.as_mut().unwrap();
            let (job, works) = recovery_population(catalog, temp.path(), count)?;
            if publication {
                let payload = temp.path().join("payload");
                std::fs::write(&payload, b"x")?;
                for work in &works {
                    let seal = crate::metadata_export::seal_photo_export(
                        &work.plan.destination,
                        &payload,
                        work.plan.max_payload_bytes,
                        &work.authority,
                        |_| Ok(()),
                    )?;
                    catalog.accept_photo_export_seal(work, &seal)?;
                    let interrupted =
                        catalog.publish_photo_export_item_with_hook(&job, work.sequence, |point| {
                            anyhow::ensure!(
                                point
                                    != crate::catalog_exports::PhotoExportBoundary::IntentCommitted,
                                "fixture interrupted after intent commit"
                            );
                            Ok(())
                        });
                    assert!(interrupted.is_err());
                }
            }
            let mut service = ExportService::open(catalog, limits())?;
            let mut remaining = count;
            let mut calls = 0;
            loop {
                let page = service.recover(catalog, 32)?;
                calls += 1;
                let handled = remaining.min(200);
                assert_eq!(page.fenced, if publication { 0 } else { handled as usize });
                remaining -= handled;
                assert_eq!(page.complete, handled < 200);
                if page.complete {
                    break;
                }
                assert!(calls <= 3);
            }
            assert_eq!(calls, count / 200 + 1);
            assert_eq!(remaining, 0);
            assert!(catalog.rendering_photo_export_attempts(200)?.is_empty());
            assert!(catalog.photo_export_publication_intents(200)?.is_empty());
            assert_eq!(
                facts
                    .executor_requests()
                    .iter()
                    .filter(|r| matches!(r.action, export_executor::Action::Recover { .. }))
                    .count(),
                calls as usize
            );
            if publication {
                assert_eq!(catalog.photo_export_job(&job)?.state, "complete");
                for work in &works {
                    assert_eq!(std::fs::read(&work.plan.destination.destination)?, b"x");
                }
            }
            service.close()?;
            drop(service);
            session.close()?;
        }
    }
    Ok(())
}

#[test]
fn managed_export_c_real_executor_unknown_acquire_recover_close_survive_reopen() -> Result<()> {
    for fault in ["acquire", "recover", "close"] {
        let temp = tempfile::tempdir()?;
        let (mut session, facts) =
            crate::catalog_session::managed_export_runtime_session(temp.path())?;
        facts.real_executor();
        let catalog = session.catalog.as_mut().unwrap();
        facts.lose_executor(fault);
        if fault == "acquire" {
            assert!(ExportService::open(catalog, limits()).is_err());
        }
        let mut service = ExportService::open(catalog, limits())?;
        if fault == "recover" {
            assert!(service.recover(catalog, 32).is_err());
        } else {
            assert!(service.recover(catalog, 32)?.complete);
        }
        if fault == "close" {
            assert!(service.close().is_err());
            assert!(ExportService::open(catalog, limits()).is_err());
        }
        drop(service);
        let mut reopened = ExportService::open(catalog, limits())?;
        assert!(reopened.recover(catalog, 32)?.complete);
        reopened.close()?;
        drop(reopened);
        let requests = facts.executor_requests();
        let selected: Vec<_> = requests
            .iter()
            .filter(|r| {
                matches!(
                    (&r.action, fault),
                    (export_executor::Action::Acquire, "acquire")
                        | (export_executor::Action::Recover { .. }, "recover")
                        | (export_executor::Action::Release, "close")
                )
            })
            .collect();
        assert!(
            selected.windows(2).any(|pair| pair[0] == pair[1]),
            "{fault}: exact request not replayed"
        );
        session.close()?;
    }
    Ok(())
}

#[test]
fn managed_export_c_reserved_stop_retry_drain_and_retire_replay_keep_custody() -> Result<()> {
    for action in ["stop", "retry_drain", "retire"] {
        let temp = tempfile::tempdir()?;
        let (mut session, facts) =
            crate::catalog_session::managed_export_runtime_session(temp.path())?;
        facts.drain_native_on_status(true);
        let catalog = session.catalog.as_mut().unwrap();
        let job = enqueue(catalog, temp.path(), action)?;
        let mut previews = previews(temp.path())?;
        let mut service = ExportService::open(catalog, limits())?;
        service.recover(catalog, 32)?;
        let cancel = AtomicBool::new(false);
        for _ in 0..32 {
            service.tick(catalog, &mut previews, &job, &cancel)?;
            if service
                .active
                .as_ref()
                .is_some_and(|a| a.phase == Phase::Running)
            {
                break;
            }
        }
        assert_eq!(service.active.as_ref().unwrap().phase, Phase::Running);
        facts.lose_native(action);
        if action == "retry_drain" {
            facts.reject_release();
        }
        if action == "stop" {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        let mut unknown = 0;
        let mut settled = false;
        for _ in 0..64 {
            match service.tick(catalog, &mut previews, &job, &cancel) {
                Err(error) if crate::export_service::pending_export(&error) => {
                    unknown += 1;
                    assert_eq!(service.reserved_bytes(), limits().worker_bytes);
                    assert!(
                        catalog
                            .photo_export_attempt_if_rendering(&job, 1)?
                            .is_some()
                    );
                }
                Err(error) => return Err(error),
                Ok(ExportEvent::Published { .. } | ExportEvent::Failed { .. }) => {
                    settled = true;
                    break;
                }
                Ok(_) => {}
            }
        }
        assert!(settled);
        // RetryDrain setup first reports a terminal Release failure, then the
        // selected RetryDrain action loses its own acknowledgement.
        assert_eq!(unknown, if action == "retry_drain" { 2 } else { 1 });
        let requests = facts.native_requests();
        let selected: Vec<_> = requests
            .iter()
            .filter(|r| {
                matches!(
                    (&r.action, action),
                    (export_native::Action::Stop, "stop")
                        | (export_native::Action::RetryDrain, "retry_drain")
                        | (export_native::Action::Retire, "retire")
                )
            })
            .collect();
        assert_eq!(
            selected.len(),
            2,
            "selected action must replay once: {action}"
        );
        assert_eq!(selected[0].digest()?, selected[1].digest()?);
        assert_eq!(service.reserved_bytes(), 0);
        let calls = facts.native_request_bytes()?;
        assert!(calls.windows(2).any(|pair| pair[0] == pair[1]));
        let seals = facts
            .stage_requests()
            .iter()
            .filter(|r| matches!(r.action, export_stage::Action::ResultAndSeal))
            .count();
        assert_eq!(seals, usize::from(action != "stop"));
        assert_eq!(
            catalog.photo_export_job(&job)?.state,
            if action == "stop" {
                "canceled"
            } else {
                "complete"
            }
        );
        service.close()?;
        drop((service, previews));
        session.close()?;
    }
    Ok(())
}

#[test]
fn managed_export_c_real_busy_inventory_and_interrupted_discard_preserve_sql_custody() -> Result<()>
{
    use fs2::FileExt;
    struct HookReset;
    impl Drop for HookReset {
        fn drop(&mut self) {
            crate::export_worker::set_compact_discard_hook(|_, _| Ok(()));
        }
    }
    for phase in [
        "before-sql",
        "after-request",
        "after-active",
        "directory",
        "after-directory",
    ] {
        let _reset = HookReset;
        let temp = tempfile::tempdir()?;
        let (mut session, facts) =
            crate::catalog_session::managed_export_runtime_session(temp.path())?;
        facts.real_executor();
        let catalog = session.catalog.as_mut().unwrap();
        let job = enqueue(catalog, temp.path(), "busy")?;
        let work = catalog.claim_photo_export(&job)?.unwrap();
        let mut service = ExportService::open(catalog, limits())?;
        let path =
            persisted_recovery_transport(&temp.path().join("cache/export-workers"), &work, 2)?;
        let busy = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join("active.lock"))?;
        busy.try_lock_exclusive()?;
        let error = service
            .recover(catalog, 32)
            .err()
            .context("busy inventory unexpectedly admitted")?;
        assert!(format!("{error:#}").contains("export transports require attention"));
        assert!(
            catalog
                .photo_export_attempt_if_rendering(&job, 1)?
                .is_some()
        );
        FileExt::unlock(&busy)?;
        drop(busy);
        let cancel = AtomicBool::new(false);
        let mut checkpoints = 0;
        let mut observer = |point| {
            if point == ExportCheckpoint::BeforeMutation {
                checkpoints += 1;
                if phase == "before-sql" && checkpoints == 2 {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        };
        let mut fired = false;
        crate::export_worker::set_compact_discard_hook(move |point, _| {
            if point == phase && !fired {
                fired = true;
                anyhow::bail!("interrupted real F Discard at {phase}");
            }
            Ok(())
        });
        let error = service
            .recover_cancellable(
                catalog,
                32,
                &mut ExportControl::observed(&cancel, &mut observer),
            )
            .err()
            .context("expected recovery interruption")?;
        assert!(
            format!("{error:#}").contains(if phase == "before-sql" {
                "export cancellation requested"
            } else {
                "interrupted real F Discard"
            }),
            "{error:#}"
        );
        assert_eq!(
            catalog
                .photo_export_attempt_if_rendering(&job, 1)?
                .is_some(),
            phase == "before-sql"
        );
        cancel.store(false, std::sync::atomic::Ordering::Release);
        if matches!(phase, "before-sql" | "after-directory") {
            // Same facade reuse after checked Close must inventory afresh; its
            // previous candidate belongs to the retired F executor generation.
            service.close()?;
        }
        if phase == "after-active" {
            drop(service);
            service = ExportService::open(catalog, limits())?;
        }
        let resumed = service.recover(catalog, 32)?;
        assert_eq!(resumed.fenced, usize::from(phase != "after-active"));
        assert!(resumed.complete && !path.exists());
        assert!(catalog.rendering_photo_export_attempts(200)?.is_empty());
        service.close()?;
        drop(service);
        session.close()?;
    }
    Ok(())
}

#[test]
fn managed_export_c_mixed_recovery_orders_rendering_before_publication_and_resumes_cancel()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let (mut session, facts) = crate::catalog_session::managed_export_runtime_session(temp.path())?;
    facts.real_executor();
    let catalog = session.catalog.as_mut().unwrap();
    let (job, works) = recovery_population(catalog, temp.path(), 203)?;
    let payload = temp.path().join("mixed-payload");
    std::fs::write(&payload, b"x")?;
    for work in &works[201..] {
        let seal = crate::metadata_export::seal_photo_export(
            &work.plan.destination,
            &payload,
            work.plan.max_payload_bytes,
            &work.authority,
            |_| Ok(()),
        )?;
        catalog.accept_photo_export_seal(work, &seal)?;
        let error = catalog
            .publish_photo_export_item_with_hook(&job, work.sequence, |point| {
                anyhow::ensure!(
                    point != crate::catalog_exports::PhotoExportBoundary::IntentCommitted,
                    "mixed fixture intent interruption"
                );
                Ok(())
            })
            .err()
            .context("expected committed intent")?;
        assert!(format!("{error:#}").contains("mixed fixture intent interruption"));
    }
    let read = rusqlite::Connection::open_with_flags(
        temp.path().join("catalog/catalog.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let canceled = AtomicBool::new(false);
    let mut observed = false;
    let mut observer = |point| {
        if matches!(point, ExportCheckpoint::Hashing { .. }) && !observed {
            let rendering: i64 = read
                .query_row(
                    "SELECT COUNT(*) FROM photo_export_items WHERE state='rendering'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                rendering, 1,
                "first 200-row rendering page must precede publication"
            );
            observed = true;
            canceled.store(true, std::sync::atomic::Ordering::Release);
        }
    };
    let mut service = ExportService::open(catalog, limits())?;
    assert!(
        service
            .recover_cancellable(
                catalog,
                32,
                &mut ExportControl::observed(&canceled, &mut observer)
            )
            .is_err()
    );
    assert!(observed);
    assert_eq!(catalog.photo_export_publication_intents(200)?.len(), 2);
    canceled.store(false, std::sync::atomic::Ordering::Release);
    let result = service.recover(catalog, 32)?;
    assert!(result.complete);
    assert_eq!(
        result.fenced, 201,
        "resumed result includes committed prior page exactly once"
    );
    for work in &works[201..] {
        assert_eq!(std::fs::read(&work.plan.destination.destination)?, b"x");
    }
    assert!(catalog.rendering_photo_export_attempts(200)?.is_empty());
    assert!(catalog.photo_export_publication_intents(200)?.is_empty());
    service.close()?;
    drop((service, read));
    session.close()?;
    Ok(())
}

#[test]
fn managed_export_c_phase_interruption_matrix_retains_cleanup_and_sql_policy() -> Result<()> {
    for phase in [
        Phase::Register,
        Phase::Ready,
        Phase::Running,
        Phase::Seal,
        Phase::CompletionReady,
    ] {
        for finish in ["cancel", "invalidate", "yield", "drain", "close", "drop"] {
            let temp = tempfile::tempdir()?;
            let (mut session, facts) =
                crate::catalog_session::managed_export_runtime_session(temp.path())?;
            facts.drain_native_on_status(true);
            let catalog = session.catalog.as_mut().unwrap();
            let job = enqueue(catalog, temp.path(), "interrupt")?;
            let mut previews = previews(temp.path())?;
            let mut service = ExportService::open(catalog, limits())?;
            service.recover(catalog, 32)?;
            let canceled = AtomicBool::new(false);
            for _ in 0..48 {
                service.tick(catalog, &mut previews, &job, &canceled)?;
                if service.active.as_ref().is_some_and(|a| a.phase == phase) {
                    break;
                }
            }
            assert_eq!(
                service
                    .active
                    .as_ref()
                    .context("fixture phase absent")?
                    .phase,
                phase
            );
            if phase == Phase::Seal {
                facts.lose_stage_action("seal");
                assert!(
                    service
                        .tick(catalog, &mut previews, &job, &canceled)
                        .is_err()
                );
                assert!(service.active.as_ref().unwrap().attempt.pending_stage());
            }
            if finish == "cancel" {
                canceled.store(true, std::sync::atomic::Ordering::Release);
            }
            if finish == "invalidate" {
                catalog.db.execute("UPDATE assets SET physical_generation=physical_generation+1 WHERE id='interrupt'", [])?;
            }
            let sealed = matches!(phase, Phase::Seal | Phase::CompletionReady);
            if finish == "drop" {
                drop(service);
                service = ExportService::open(catalog, limits())?;
            } else {
                let mut settled = false;
                for _ in 0..48 {
                    let result = match finish {
                        "yield" => service.yield_to_previews(catalog).map(|_| ()),
                        "drain" => service.drain_native(),
                        "close" => service.close(),
                        _ => service
                            .tick(catalog, &mut previews, &job, &canceled)
                            .map(|_| ()),
                    };
                    match result {
                        Err(error) if crate::export_service::pending_export(&error) => {}
                        Err(error) => return Err(error.context(format!("{phase:?}/{finish}"))),
                        Ok(()) if !service.is_active() => {
                            settled = true;
                            break;
                        }
                        Ok(()) => {}
                    }
                    if service.is_active() {
                        assert!(
                            catalog
                                .photo_export_attempt_if_rendering(&job, 1)?
                                .is_some()
                        );
                    }
                }
                assert!(settled, "{phase:?}/{finish}");
            }
            assert_eq!(service.reserved_bytes(), 0);
            let item = catalog.photo_export_items(&job, 0, 2)?.remove(0);
            let expected = match finish {
                "cancel" | "invalidate" => "failed",
                "yield" if sealed => "published",
                "yield" => "pending",
                _ => "rendering",
            };
            assert_eq!(item.state, expected, "{phase:?}/{finish}");
            let published = temp.path().join("interrupt-published.png");
            assert_eq!(
                published.exists(),
                finish == "yield" && sealed,
                "{phase:?}/{finish}"
            );
            let seals = facts
                .stage_requests()
                .into_iter()
                .filter(|r| matches!(r.action, export_stage::Action::ResultAndSeal))
                .map(|r| r.digest())
                .collect::<Result<std::collections::HashSet<_>>>()?;
            assert_eq!(
                seals.len(),
                usize::from(sealed),
                "no additional logical seal: {phase:?}/{finish}"
            );
            if matches!(finish, "drain" | "close" | "drop") {
                assert_eq!(service.recover(catalog, 32)?.fenced, 1);
            }
            service.close()?;
            drop((service, previews));
            session.close()?;
        }
    }
    Ok(())
}

#[test]
fn managed_export_c_failed_wait_and_pipe_join_retry_before_failing_sql() -> Result<()> {
    for failure in [
        export_native::Phase::WaitFailed,
        export_native::Phase::PipeJoinFailed,
    ] {
        let temp = tempfile::tempdir()?;
        let (mut session, facts) =
            crate::catalog_session::managed_export_runtime_session(temp.path())?;
        let catalog = session.catalog.as_mut().unwrap();
        let job = enqueue(catalog, temp.path(), "native-failure")?;
        let mut previews = previews(temp.path())?;
        let mut service = ExportService::open(catalog, limits())?;
        service.recover(catalog, 32)?;
        let cancel = AtomicBool::new(false);
        for _ in 0..32 {
            service.tick(catalog, &mut previews, &job, &cancel)?;
            if service
                .active
                .as_ref()
                .is_some_and(|a| a.phase == Phase::Running)
            {
                break;
            }
        }
        facts.native_failure(failure);
        facts.lose_native("retry_drain");
        assert!(matches!(
            until_idle(&mut service, catalog, &mut previews, &job, &cancel)?,
            ExportEvent::Failed { .. }
        ));
        let natives = facts.native_requests();
        assert_eq!(
            natives
                .iter()
                .filter(|r| matches!(r.action, export_native::Action::RetryDrain))
                .count(),
            2
        );
        assert!(
            natives
                .iter()
                .any(|r| matches!(r.action, export_native::Action::Retire))
        );
        assert!(
            !facts
                .stage_requests()
                .iter()
                .any(|r| matches!(r.action, export_stage::Action::ResultAndSeal))
        );
        assert_eq!(service.reserved_bytes(), 0);
        assert!(catalog.rendering_photo_export_attempts(200)?.is_empty());
        service.close()?;
        drop((service, previews));
        session.close()?;
    }
    Ok(())
}

#[test]
fn managed_export_c_recovery_rejects_retained_preparation_before_executor_dispatch() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (mut session, facts) = crate::catalog_session::managed_export_runtime_session(temp.path())?;
    let catalog = session.catalog.as_mut().unwrap();
    let job = enqueue(catalog, temp.path(), "preparation")?;
    let mut service = ExportService::open(catalog, limits())?;
    service.recover(catalog, 32)?;
    let work = catalog.claim_photo_export(&job)?.unwrap();
    service.preparing = Some((work.clone(), Some("retained preparation failure".into())));
    let before = facts.executor_requests();
    let error = service
        .recover(catalog, 32)
        .err()
        .context("recovery took preparation custody")?;
    assert!(format!("{error:#}").contains("cannot recover while export worker is active"));
    assert_eq!(facts.executor_requests(), before);
    assert_eq!(
        catalog
            .photo_export_attempt_if_rendering(&job, 1)?
            .unwrap()
            .attempt,
        work.attempt
    );
    service.drain_native()?;
    assert_eq!(service.recover(catalog, 32)?.fenced, 1);
    service.close()?;
    drop(service);
    session.close()?;
    Ok(())
}
