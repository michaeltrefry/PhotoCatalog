use super::*;

fn scheduler(workers: usize, bytes: u64) -> Result<PreviewScheduler> {
    PreviewScheduler::new(SchedulerLimits {
        requests: 8,
        workers,
        working_bytes: bytes,
    })
}
fn read(
    s: &mut PreviewScheduler,
    cost: u64,
    priority: Priority,
) -> Result<(NativeReadRequest, u64)> {
    let request = s.queue_native_read(priority)?;
    let lease = s.admit_native_read(request.id, cost)?.unwrap();
    Ok((request, lease))
}

#[test]
fn native_read_and_render_share_worker_capacity_in_both_directions() -> Result<()> {
    let mut scheduler = scheduler(1, 1024)?;
    let (_, first) = read(&mut scheduler, 128, Priority::Foreground)?;
    let consumer = scheduler.request("a".repeat(64), 256, Priority::Foreground)?;
    let queued = scheduler.queue_native_read(Priority::Foreground)?;
    assert!(scheduler.next_ready()?.is_none());
    assert!(scheduler.admit_native_read(queued.id, 128)?.is_none());
    assert_eq!(scheduler.usage().active, 1);
    assert_eq!(scheduler.usage().reserved_bytes, 128);
    assert_eq!(scheduler.usage().queued, 2);
    // The caller supplies verified drain here. This pure scheduler test does
    // not establish process exit or pipe joins itself.
    scheduler.release_native_read(first)?;
    let render = scheduler.next_ready()?.unwrap();
    assert!(scheduler.admit_native_read(queued.id, 128)?.is_none());
    assert!(scheduler.cancel(consumer));
    assert!(render.canceled.load(Ordering::Acquire));
    assert!(scheduler.admit_native_read(queued.id, 128)?.is_none());
    assert_eq!(scheduler.usage().reserved_bytes, 256);
    scheduler.finished(render.id, WorkerOutcome::Stopped)?;
    let next = scheduler.admit_native_read(queued.id, 128)?.unwrap();
    assert_ne!(next, first);
    scheduler.release_native_read(next)?;
    assert_eq!(scheduler.usage().reserved_bytes, 0);
    Ok(())
}

#[test]
fn header_upgrade_retains_arrival_but_fences_the_old_attempt_lease() -> Result<()> {
    let mut scheduler = scheduler(2, 1024)?;
    let (request, header) = read(&mut scheduler, 256, Priority::Foreground)?;
    scheduler.request("b".repeat(64), 768, Priority::Foreground)?;
    let render = scheduler.next_ready()?.unwrap();
    assert!(!scheduler.upgrade_native_read(header, 512)?);
    assert_eq!(scheduler.usage().reserved_bytes, 1024);
    assert_eq!(scheduler.usage().active, 2);
    // Requeue follows checked native/transport drain, not merely a stop signal.
    scheduler.requeue_native_read(header, 512)?;
    assert_eq!(scheduler.usage().reserved_bytes, 768);
    assert!(scheduler.admit_native_read(request.id, 512)?.is_none());
    let younger = scheduler.request("d".repeat(64), 512, Priority::Foreground)?;
    scheduler.finished(render.id, WorkerOutcome::Succeeded)?;
    assert!(
        scheduler.next_ready()?.is_none(),
        "younger Render stole retry arrival"
    );
    let decode = scheduler.admit_native_read(request.id, 512)?.unwrap();
    assert_ne!(decode, header);
    let before = scheduler.usage();
    assert!(scheduler.release_native_read(header).is_err());
    assert!(scheduler.upgrade_native_read(header, 512).is_err());
    assert!(scheduler.requeue_native_read(header, 512).is_err());
    assert_eq!(scheduler.usage(), before);
    assert!(scheduler.upgrade_native_read(decode, 768)?);
    assert!(scheduler.upgrade_native_read(decode, 768)?);
    assert_eq!(scheduler.usage().reserved_bytes, 768);
    scheduler.release_native_read(decode)?;
    assert!(scheduler.release_native_read(decode).is_err());
    assert!(scheduler.cancel(younger));
    assert_eq!(scheduler.usage().reserved_bytes, 0);
    assert_eq!(scheduler.usage().active, 0);
    Ok(())
}

#[test]
fn impossible_decode_admission_preserves_existing_owners_and_charges() -> Result<()> {
    let mut scheduler = scheduler(2, 1024)?;
    let (_, read) = read(&mut scheduler, 256, Priority::Foreground)?;
    scheduler.request("c".repeat(64), 512, Priority::Foreground)?;
    let render = scheduler.next_ready()?.unwrap();
    let queued = scheduler.queue_native_read(Priority::Foreground)?;
    let before = scheduler.usage();
    for impossible in [0, 1025, u64::MAX] {
        for error in [
            scheduler.upgrade_native_read(read, impossible).unwrap_err(),
            scheduler
                .admit_native_read(queued.id, impossible)
                .unwrap_err(),
            scheduler.requeue_native_read(read, impossible).unwrap_err(),
        ] {
            assert!(
                error
                    .downcast_ref::<crate::catalog_session::store::ResourceLimit>()
                    .is_some()
            );
            assert_eq!(scheduler.usage(), before);
        }
    }
    assert!(scheduler.release_native_read(render.id).is_err());
    assert!(scheduler.finished(read, WorkerOutcome::Stopped).is_err());
    assert_eq!(scheduler.usage(), before);
    scheduler.cancel_queued_native_read(queued.id)?;
    scheduler.release_native_read(read)?;
    assert_eq!(scheduler.usage().reserved_bytes, 512);
    scheduler.finished(render.id, WorkerOutcome::Succeeded)?;
    assert_eq!(scheduler.usage().reserved_bytes, 0);
    Ok(())
}

#[test]
fn foreground_cache_progresses_ahead_of_sustained_background_renders() -> Result<()> {
    let mut scheduler = scheduler(1, 1024)?;
    let mut background = vec![scheduler.request("a".repeat(64), 512, Priority::Background)?];
    let running = scheduler.next_ready()?.unwrap();
    let request = scheduler.queue_native_read(Priority::Foreground)?;
    assert!(running.canceled.load(Ordering::Acquire));
    for key in 1..=6 {
        background.push(scheduler.request(format!("{key:064x}"), 512, Priority::Background)?);
    }
    assert!(scheduler.admit_native_read(request.id, 256)?.is_none());
    assert_eq!(scheduler.usage().reserved_bytes, 512);
    assert!(
        scheduler
            .finished(running.id, WorkerOutcome::Stopped)?
            .requeued
    );
    assert!(scheduler.next_ready()?.is_none());
    let lease = scheduler.admit_native_read(request.id, 256)?.unwrap();
    assert!(!request.preempted.load(Ordering::Acquire));
    scheduler.release_native_read(lease)?;
    for consumer in background {
        assert!(scheduler.cancel(consumer));
    }
    assert_eq!(scheduler.usage().consumers, 0);
    Ok(())
}

#[test]
fn foreground_render_preempts_background_read_without_permanent_cancellation() -> Result<()> {
    let mut scheduler = scheduler(1, 1024)?;
    let (request, old) = read(&mut scheduler, 512, Priority::Background)?;
    scheduler.request("e".repeat(64), 512, Priority::Foreground)?;
    assert!(request.preempted.load(Ordering::Acquire));
    assert!(scheduler.next_ready()?.is_none());
    assert!(scheduler.cancel_queued_native_read(request.id).is_err());
    assert_eq!(scheduler.usage().reserved_bytes, 512);
    scheduler.requeue_native_read(old, 512)?;
    assert!(!request.preempted.load(Ordering::Acquire));
    let render = scheduler.next_ready()?.unwrap();
    assert!(scheduler.admit_native_read(request.id, 512)?.is_none());
    scheduler.finished(render.id, WorkerOutcome::Succeeded)?;
    let resumed = scheduler.admit_native_read(request.id, 512)?.unwrap();
    assert_ne!(old, resumed);
    assert!(scheduler.release_native_read(old).is_err());
    scheduler.release_native_read(resumed)?;
    Ok(())
}

#[test]
fn pending_cost_resolution_keeps_queue_order_and_cancel_unblocks_render() -> Result<()> {
    let mut scheduler = scheduler(1, 1024)?;
    let request = scheduler.queue_native_read(Priority::Foreground)?;
    scheduler.request("f".repeat(64), 512, Priority::Foreground)?;
    assert_eq!(scheduler.usage().active, 0);
    assert_eq!(scheduler.usage().queued, 2);
    assert_eq!(scheduler.usage().reserved_bytes, 0);
    assert!(scheduler.next_ready()?.is_none());
    scheduler.cancel_queued_native_read(request.id)?;
    assert!(scheduler.cancel_queued_native_read(request.id).is_err());
    let render = scheduler.next_ready()?.unwrap();
    scheduler.finished(render.id, WorkerOutcome::Succeeded)?;
    Ok(())
}

#[test]
fn native_and_render_requests_share_bounded_queue_capacity() -> Result<()> {
    let mut scheduler = PreviewScheduler::new(SchedulerLimits {
        requests: 2,
        workers: 1,
        working_bytes: 1024,
    })?;
    let request = scheduler.queue_native_read(Priority::Foreground)?;
    let consumer = scheduler.request("0".repeat(64), 512, Priority::Background)?;
    let before = scheduler.usage();
    assert!(scheduler.queue_native_read(Priority::Foreground).is_err());
    assert!(
        scheduler
            .request("1".repeat(64), 512, Priority::Foreground)
            .is_err()
    );
    assert_eq!(scheduler.usage(), before);
    scheduler.cancel_queued_native_read(request.id)?;
    assert!(scheduler.cancel(consumer));
    assert_eq!(scheduler.usage().consumers, 0);
    assert_eq!(scheduler.usage().queued, 0);
    Ok(())
}

#[test]
fn foreground_cache_preempts_single_background_read_candidate_with_spare_worker() -> Result<()> {
    let mut scheduler = scheduler(2, 1024)?;
    let (background, old) = read(&mut scheduler, 128, Priority::Background)?;
    let foreground = scheduler.queue_native_read(Priority::Foreground)?;
    assert!(background.preempted.load(Ordering::Acquire));
    assert_eq!(scheduler.usage().reserved_bytes, 128);
    scheduler.requeue_native_read(old, 128)?;
    let active = scheduler.admit_native_read(foreground.id, 128)?.unwrap();
    scheduler.release_native_read(active)?;
    let resumed = scheduler.admit_native_read(background.id, 128)?.unwrap();
    assert_ne!(old, resumed);
    scheduler.release_native_read(resumed)?;
    Ok(())
}
