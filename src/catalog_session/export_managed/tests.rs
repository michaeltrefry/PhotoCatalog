use super::*;
use crate::export_service::{managed_test_enqueue, managed_test_limits};

#[test]
fn managed_export_c_identity_overflows_fail_without_dispatch_or_registry_mutation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (mut session, facts) = crate::catalog_session::managed_export_runtime_session(temp.path())?;
    let catalog = session.catalog.as_mut().unwrap();
    let authority = catalog.session.clone();
    authority
        .managed_export
        .0
        .lock()
        .unwrap()
        .generation_high_water = u64::MAX;
    assert!(
        authority
            .open_managed_export(&AtomicBool::new(false))
            .is_err()
    );
    {
        let state = authority.managed_export.0.lock().unwrap();
        assert_eq!(state.generation_high_water, u64::MAX);
        assert!(!state.service_claimed && state.active.is_none() && state.pending_close.is_none());
    }
    assert!(facts.executor_requests().is_empty());
    authority
        .managed_export
        .0
        .lock()
        .unwrap()
        .generation_high_water = 0;
    let mut executor = authority
        .open_managed_export(&AtomicBool::new(false))?
        .unwrap();
    let initial = facts.executor_requests();
    authority
        .managed_export
        .0
        .lock()
        .unwrap()
        .active
        .as_mut()
        .unwrap()
        .high_water = u64::MAX;
    assert!(executor.recover(32, &AtomicBool::new(false)).is_err());
    assert!(executor.close().is_err());
    {
        let state = authority.managed_export.0.lock().unwrap();
        let active = state.active.as_ref().unwrap();
        assert_eq!(active.high_water, u64::MAX);
        assert!(!active.closing && active.pending.is_none() && state.pending_close.is_none());
        assert!(state.service_claimed);
    }
    assert_eq!(facts.executor_requests(), initial);
    authority
        .managed_export
        .0
        .lock()
        .unwrap()
        .active
        .as_mut()
        .unwrap()
        .high_water = 1;
    let job = managed_test_enqueue(catalog, temp.path(), "overflow")?;
    let work = catalog.claim_photo_export(&job)?.unwrap();
    authority.managed_export.0.lock().unwrap().native_high_water = u64::MAX;
    assert!(
        executor
            .prepare_attempt(work.clone(), managed_test_limits())
            .is_err()
    );
    assert_eq!(
        authority.managed_export.0.lock().unwrap().native_high_water,
        u64::MAX
    );
    assert!(facts.native_requests().is_empty() && facts.stage_requests().is_empty());
    assert_eq!(
        catalog
            .photo_export_attempt_if_rendering(&job, 1)?
            .unwrap()
            .attempt,
        work.attempt
    );
    authority.managed_export.0.lock().unwrap().native_high_water = 0;
    let mut attempt = executor.prepare_attempt(work.clone(), managed_test_limits())?;
    attempt.stage_high_water = u64::MAX;
    assert!(
        attempt
            .stage(
                export_stage::Action::Ready {
                    icc: None,
                    xmp: None
                },
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert_eq!(attempt.stage_high_water, u64::MAX);
    assert!(attempt.pending_stage.is_none());
    assert!(facts.native_requests().is_empty() && facts.stage_requests().is_empty());
    assert_eq!(facts.executor_requests(), initial);
    drop(attempt);
    executor.close()?;
    catalog.requeue_photo_export_attempt(&work)?;
    drop(executor);
    session.close()?;
    Ok(())
}

#[test]
fn managed_export_c_native_counter_overflow_settles_service_rendering_claim() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (mut session, facts) = crate::catalog_session::managed_export_runtime_session(temp.path())?;
    let catalog = session.catalog.as_mut().unwrap();
    let job = managed_test_enqueue(catalog, temp.path(), "service-overflow")?;
    let mut previews = crate::export_service::managed_test_previews(temp.path())?;
    let mut service = crate::export_service::ExportService::open(
        catalog,
        &std::env::current_exe()?,
        managed_test_limits(),
    )?;
    service.recover(catalog, 32)?;
    catalog
        .session
        .managed_export
        .0
        .lock()
        .unwrap()
        .native_high_water = u64::MAX;
    let event = service.tick(catalog, &mut previews, &job, &AtomicBool::new(false))?;
    assert!(
        matches!(event, crate::export_service::ExportEvent::Failed { ref detail, .. } if detail.contains("managed export native identity exhausted")),
        "{event:?}"
    );
    assert!(!service.is_active());
    assert_eq!(service.reserved_bytes(), 0);
    assert!(facts.native_requests().is_empty() && facts.stage_requests().is_empty());
    assert!(catalog.rendering_photo_export_attempts(200)?.is_empty());
    assert_eq!(catalog.photo_export_items(&job, 0, 2)?[0].state, "failed");
    assert_eq!(
        catalog
            .session
            .managed_export
            .0
            .lock()
            .unwrap()
            .native_high_water,
        u64::MAX
    );
    service.close()?;
    drop((service, previews));
    session.close()?;
    Ok(())
}
