use super::*;
use crate::catalog_backup::RestoreStatus;
use crate::catalog_session::{
    CatalogBootstrap, CatalogFilesystem, ConfirmSqlAdmission, PhysicalObjectId, PrepareCatalog,
    RootCapability, SqlAdmissionConfirmed,
};
use crate::filesystem_worker::wire::{Failure, FailureKind};
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::time::Duration;
fn root() -> RootCapability {
    // Synthetic identities only; these tests do not admit or open a catalog.
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
    #[cfg(unix)]
    let path = std::path::Path::new("/synthetic");
    #[cfg(windows)]
    let path = std::path::Path::new(r"C:\synthetic");
    RootCapability {
        epoch: LeaseId::new(),
        token: LeaseId::new(),
        session: LeaseId::new(),
        canonical_root: NativePath::from_path(path),
        root_physical: physical,
        catalog_physical: physical,
    }
}

struct Fake {
    stage: LeaseId,
    calls: Mutex<Vec<n::Request>>,
    actions: Mutex<Vec<f::Action>>,
    lost: AtomicUsize,
    stops: Mutex<Vec<n::Key>>,
    hold_spawn: Mutex<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>>,
}
impl CatalogFilesystem for Fake {
    fn native(&self) -> Option<&dyn n::CatalogNative> {
        Some(self)
    }
    fn preview_stage_call(&self, r: &f::Request, _: &AtomicBool) -> Result<f::Reply> {
        self.actions.lock().unwrap().push(r.action.clone());
        Ok(f::Reply {
            epoch: r.root.epoch.clone(),
            session: r.root.session.clone(),
            operation: r.operation,
            value: if matches!(r.action, f::Action::Admit { .. }) {
                f::Value::Admitted {
                    stage: self.stage.clone(),
                    ready: true,
                    error: None,
                }
            } else {
                f::Value::Unit
            },
        })
    }
    fn prepare_catalog(&self, _: &PrepareCatalog, _: &AtomicBool) -> Result<CatalogBootstrap> {
        anyhow::bail!("unexpected catalog admission")
    }
    fn abandon_prepare(&self, _: U64, _: &LeaseId) -> Result<()> {
        anyhow::bail!("unexpected abandon")
    }
    fn confirm_sql_admission(
        &self,
        _: &ConfirmSqlAdmission,
        _: &AtomicBool,
    ) -> Result<SqlAdmissionConfirmed> {
        anyhow::bail!("unexpected SQL admission")
    }
    fn restore_status(&self, _: &RootCapability) -> Result<Option<RestoreStatus>> {
        anyhow::bail!("unexpected restore")
    }
    fn resume_restored_jobs(&self, _: &RootCapability, _: &str, _: bool) -> Result<RestoreStatus> {
        anyhow::bail!("unexpected resume")
    }
    fn release_root(&self, _: &RootCapability) -> Result<()> {
        anyhow::bail!("unexpected root release")
    }
}
impl n::CatalogNative for Fake {
    fn signal_stop(&self, root: &RootCapability, operation: U64) -> Result<()> {
        self.stops
            .lock()
            .unwrap()
            .push(n::Key::new(root, operation));
        Ok(())
    }
    fn call(&self, r: &n::Request, _: &AtomicBool) -> Result<n::Status> {
        self.calls.lock().unwrap().push(r.clone());
        if matches!(r.action, n::Action::Spawn { .. }) {
            let held = self.hold_spawn.lock().unwrap().take();
            if let Some((entered, release)) = held {
                entered.send(())?;
                release.recv()?;
            }
        }
        if matches!(r.action, n::Action::Spawn { .. })
            && self
                .lost
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1))
                .is_ok()
        {
            return Err(Failure::new(FailureKind::Unknown, "lost initial spawn reply").into());
        }
        Ok(n::Status {
            epoch: r.root.epoch.clone(),
            session: r.root.session.clone(),
            operation: r.operation,
            stage: self.stage.clone(),
            pid: Some(7),
            phase: if matches!(r.action, n::Action::Drain | n::Action::Retire) {
                n::Phase::Drained
            } else {
                n::Phase::Spawned
            },
            initial_sent: false,
            encode_sent: false,
            exit_code: None,
            success: Some(false),
            error: None,
        })
    }
    fn status(&self, _: &RootCapability, _: U64) -> Result<n::Status> {
        Err(Failure::new(FailureKind::Unknown, "status not yet delivered").into())
    }
}
fn job(lost: usize) -> Result<(Job, Arc<Fake>)> {
    let fake = Arc::new(Fake {
        stage: LeaseId::new(),
        calls: Mutex::new(Vec::new()),
        actions: Mutex::new(Vec::new()),
        lost: AtomicUsize::new(lost),
        stops: Mutex::new(Vec::new()),
        hold_spawn: Mutex::new(None),
    });
    let calls = crate::preview::stage_io::Calls::new(fake.clone(), root(), false);
    let work = n::Work::DecodeEncoded {
        codec: Codec::Jpeg,
        encoded_bytes: U64(5),
        encoded_digest: "a".repeat(64),
        expected_dimensions: None,
    };
    let limits = crate::preview::ServiceLimits::default();
    let cost = n::header_cost(5, limits.cache_header_scratch_bytes)?;
    Ok((Job::admit(calls, work, &limits, cost)?, fake))
}
#[test]
fn cancel_before_upload_or_spawn_only_releases_f_stage() -> Result<()> {
    let (mut job, fake) = job(0)?;
    job.signal_stop();
    assert!(job.drain()?);
    assert!(job.is_retired());
    job.stage.release()?;
    assert!(fake.calls.lock().unwrap().is_empty());
    let actions = fake.actions.lock().unwrap();
    assert!(matches!(actions[0], f::Action::Admit { .. }));
    assert!(matches!(actions[1], f::Action::AbortRead { .. }));
    assert!(matches!(actions[2], f::Action::Release { .. }));
    Ok(())
}
#[test]
fn uncertain_spawn_recovers_from_bound_stop_status_before_drain() -> Result<()> {
    let (mut job, fake) = job(1)?;
    assert!(job.start().is_err());
    assert!(job.spawn_attempted);
    assert!(!job.is_retired());
    assert!(
        !fake
            .actions
            .lock()
            .unwrap()
            .iter()
            .any(|a| matches!(a, f::Action::Release { .. }))
    );
    assert!(job.drain()?);
    job.retire()?;
    job.stage.release()?;
    let calls = fake.calls.lock().unwrap();
    let spawn: Vec<_> = calls
        .iter()
        .filter(|r| matches!(r.action, n::Action::Spawn { .. }))
        .collect();
    assert_eq!(spawn.len(), 1);
    // The validated Stop response authoritatively recovers the original slot.
    // Exact initial-Spawn replay when status is unknown is covered separately.
    assert!(calls.iter().any(|r| matches!(r.action, n::Action::Stop)));
    assert!(calls.iter().all(|r| r.operation == job.operation));
    assert!(matches!(calls.last().unwrap().action, n::Action::Retire));
    Ok(())
}

fn sibling(job: &Job) -> Result<Job> {
    let n::Action::Spawn { work, .. } = &job.spawn.action else {
        unreachable!()
    };
    Job::admit(
        job.stage.calls.lane()?,
        work.clone(),
        &crate::preview::ServiceLimits::default(),
        job.cost,
    )
}

#[test]
fn native_identity_follows_first_dispatch_not_stage_admission_order() -> Result<()> {
    let (mut earlier, fake) = job(0)?;
    let mut later = sibling(&earlier)?;
    assert_eq!(earlier.operation, U64(0));
    assert_eq!(later.operation, U64(0));
    // The earlier F stage can wait for input while a later stage becomes ready.
    later.start()?;
    earlier.start()?;
    assert_eq!(later.operation, U64(1));
    assert_eq!(earlier.operation, U64(2));
    let operations: Vec<_> = fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|r| matches!(r.action, n::Action::Spawn { .. }))
        .map(|r| r.operation)
        .collect();
    assert_eq!(operations, vec![U64(1), U64(2)]);
    assert!(later.drain()?);
    later.retire()?;
    assert!(earlier.drain()?);
    earlier.retire()?;
    Ok(())
}

#[test]
fn never_spawned_cancellation_does_not_consume_native_identity() -> Result<()> {
    let (mut canceled, fake) = job(0)?;
    let mut next = sibling(&canceled)?;
    canceled.stage.calls.signal_native_stop();
    canceled.signal_stop();
    assert!(canceled.drain()?);
    canceled.stage.release()?;
    assert_eq!(canceled.operation, U64(0));
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(fake.stops.lock().unwrap().is_empty());
    next.start()?;
    assert_eq!(next.operation, U64(1));
    assert!(next.drain()?);
    next.retire()?;
    Ok(())
}

#[test]
fn lost_spawn_reply_fences_other_owner_until_exact_replay() -> Result<()> {
    let (mut first, fake) = job(1)?;
    let mut second = sibling(&first)?;
    assert!(first.start().is_err());
    assert_eq!(first.operation, U64(1));
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let _ = entered_tx.send(());
        let result = second.start();
        let _ = done_tx.send(());
        (second, result)
    });
    let entered = entered_rx.recv_timeout(Duration::from_secs(2));
    let early = done_rx.recv_timeout(Duration::from_millis(40));
    let pending_calls = fake.calls.lock().unwrap().clone();
    // Resolve the retained operation before asserting, so even a failed
    // observation does not strand the waiting test thread.
    let replay = first.start();
    let (mut second, second_result) = waiter.join().expect("native admission waiter panicked");
    entered?;
    assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
    assert_eq!(pending_calls.len(), 1);
    replay?;
    second_result?;
    assert_eq!(second.operation, U64(2));
    let requests: Vec<_> = fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|r| matches!(r.action, n::Action::Spawn { .. }))
        .cloned()
        .collect();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        serde_json::to_vec(&requests[0])?,
        serde_json::to_vec(&requests[1])?
    );
    assert_eq!(requests[2].operation, U64(2));
    assert!(first.drain()?);
    first.retire()?;
    assert!(second.drain()?);
    second.retire()?;
    Ok(())
}

#[test]
fn held_spawn_allows_reserved_stop_without_job_or_admission_lock() -> Result<()> {
    let (mut owned, fake) = job(0)?;
    let calls = owned.stage.calls.clone();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    *fake.hold_spawn.lock().unwrap() = Some((entered_tx, release_rx));
    let starter = std::thread::spawn(move || {
        let result = owned.start();
        (owned, result)
    });
    let entered = entered_rx.recv_timeout(Duration::from_secs(2));
    let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
    let stopper = std::thread::spawn(move || {
        calls.signal_native_stop();
        let _ = stopped_tx.send(());
    });
    let stopped_while_held = stopped_rx.recv_timeout(Duration::from_secs(2));
    let observed_stops = fake.stops.lock().unwrap().clone();
    // Always release and join both threads before checking timing assertions.
    let _ = release_tx.send(());
    let (mut owned, start_result) = starter.join().expect("held native spawn panicked");
    stopper.join().expect("reserved native stop panicked");
    entered?;
    stopped_while_held?;
    start_result?;
    assert_eq!(
        observed_stops,
        vec![n::Key::new(&owned.stage.calls.root, owned.operation)]
    );
    assert_eq!(owned.operation, U64(1));
    assert!(fake.stops.lock().unwrap().len() >= 2);
    assert!(
        !fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r.action, n::Action::Start))
    );
    assert!(owned.drain()?);
    owned.retire()?;
    Ok(())
}

#[test]
fn sibling_canceled_while_waiting_for_unknown_spawn_allocates_no_native_id() -> Result<()> {
    let (mut first, fake) = job(1)?;
    let mut second = sibling(&first)?;
    let control = second.stage.calls.clone();
    assert!(first.start().is_err());
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let _ = entered_tx.send(());
        let result = second.start();
        let _ = done_tx.send(());
        (second, result)
    });
    let entered = entered_rx.recv_timeout(Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(40));
    control.signal_native_stop();
    let canceled_before_replay = done_rx.recv_timeout(Duration::from_secs(2));
    let replay = first.start();
    let (mut second, result) = waiter.join().expect("native waiter panicked");
    entered?;
    replay?;
    assert!(
        canceled_before_replay.is_ok(),
        "canceled sibling waited on another owner reconciliation"
    );
    let error = result.unwrap_err();
    assert_eq!(
        error.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::Canceled
    );
    assert_eq!(second.operation, U64(0));
    assert!(!second.spawn_attempted);
    let spawns: Vec<_> = fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|r| matches!(r.action, n::Action::Spawn { .. }))
        .cloned()
        .collect();
    assert_eq!(spawns.len(), 2);
    assert_eq!(
        serde_json::to_vec(&spawns[0])?,
        serde_json::to_vec(&spawns[1])?
    );
    assert_eq!(control.native_operation()?, U64(2));
    assert!(first.drain()?);
    first.retire()?;
    assert!(second.drain()?);
    second.retire()?;
    Ok(())
}
