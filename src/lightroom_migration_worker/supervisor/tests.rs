use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
struct Admit {
    writers: Arc<Writers>,
    releases: Arc<Mutex<Vec<(u64, WriteKind)>>>,
}
impl Admission for Admit {
    fn lock(&mut self, _: &str, _: &DestinationPin, _: &FileKey) -> Result<()> {
        Ok(())
    }
    fn writer(
        &mut self,
        _: u64,
        _: WriteKind,
        _: &str,
        _: Option<&FileKey>,
        _: &Stop,
        _: Instant,
    ) -> Result<Arc<Writers>> {
        Ok(self.writers.clone())
    }
    fn release(&mut self, sequence: u64, kind: WriteKind) -> Result<()> {
        self.releases.lock().unwrap().push((sequence, kind));
        Ok(())
    }
    fn progress(&mut self, _: &str, _: u64, _: Option<u64>) -> Result<()> {
        Ok(())
    }
}
fn guard() -> Guard {
    Guard {
        session: "session".into(),
        generation: "generation".into(),
        operation: "operation".into(),
    }
}
fn state(writers: Arc<Writers>, releases: Arc<Mutex<Vec<(u64, WriteKind)>>>) -> State<Admit> {
    State::new(
        Admit { writers, releases },
        guard(),
        "a".repeat(64),
        MemoryBudget::new(2 * 1024 * 1024 * 1024)
            .unwrap()
            .reservation(),
    )
}
fn admitted<A: Admission>(state: &mut State<A>, stop: &Arc<Stop>) -> Result<()> {
    state.accept_wait(
        ChildFrame::Admitted {
            guard: guard(),
            request_blake3: "a".repeat(64),
            build: super::super::worker::build_identity().into(),
        },
        stop,
        Instant::now() + Duration::from_secs(5),
    )?;
    Ok(())
}
fn need() -> ChildFrame {
    ChildFrame::NeedWrite {
        guard: guard(),
        sequence: U64(1),
        write: WriteKind::Bootstrap,
        target_token: "b".repeat(64),
        lock: None,
    }
}
#[test]
fn a_stale_release_cannot_free_the_parent_permit() -> Result<()> {
    let writers = Arc::new(Writers::default());
    let releases = Arc::new(Mutex::new(Vec::new()));
    let mut state = state(writers.clone(), releases.clone());
    let stop = Arc::new(Stop::default());
    admitted(&mut state, &stop)?;
    assert!(matches!(
        state.accept_wait(need(), &stop, Instant::now() + Duration::from_secs(5))?,
        Some(ParentFrame::Grant {
            sequence: U64(1),
            ..
        })
    ));
    let completed = Arc::new(AtomicBool::new(false));
    let done = completed.clone();
    let other = writers.clone();
    let waiter = thread::spawn(move || -> Result<()> {
        let _permit = other.enter(Priority::Foreground)?;
        done.store(true, Ordering::Release);
        Ok(())
    });
    writers.wait_until_queued(1, 0);
    assert!(
        state
            .accept_wait(
                ChildFrame::ReleaseWrite {
                    guard: guard(),
                    sequence: U64(2),
                    write: WriteKind::Bootstrap
                },
                &stop,
                Instant::now() + Duration::from_secs(5)
            )
            .is_err()
    );
    assert!(!completed.load(Ordering::Acquire));
    assert!(releases.lock().unwrap().is_empty());
    state.accept_wait(
        ChildFrame::ReleaseWrite {
            guard: guard(),
            sequence: U64(1),
            write: WriteKind::Bootstrap,
        },
        &stop,
        Instant::now() + Duration::from_secs(5),
    )?;
    waiter.join().expect("waiter panicked")?;
    assert_eq!(*releases.lock().unwrap(), [(1, WriteKind::Bootstrap)]);
    assert!(completed.load(Ordering::Acquire));
    Ok(())
}
#[test]
fn cancellation_retires_writer_wait_without_grant_or_parent_hold_leak() -> Result<()> {
    let writers = Arc::new(Writers::default());
    let held = writers.enter(Priority::Foreground)?;
    let stop = Arc::new(Stop::default());
    let releases = Arc::new(Mutex::new(Vec::new()));
    let worker_gate = writers.clone();
    let worker_stop = stop.clone();
    let worker_releases = releases.clone();
    let worker = thread::spawn(move || -> Result<()> {
        let mut state = state(worker_gate, worker_releases);
        admitted(&mut state, &worker_stop)?;
        assert!(
            state
                .accept_wait(
                    need(),
                    &worker_stop,
                    Instant::now() + Duration::from_secs(5)
                )
                .is_err()
        );
        assert!(state.held.is_none());
        Ok(())
    });
    writers.wait_until_queued(0, 1);
    stop.cancel();
    writers.wake_waiters();
    worker.join().expect("supervisor panicked")?;
    // The current writer is still held: cancellation did not depend on it.
    assert_eq!(*releases.lock().unwrap(), [(1, WriteKind::Bootstrap)]);
    drop(held);
    let _next = writers.enter(Priority::Background)?;
    Ok(())
}
#[test]
fn input_guard_result_digest_and_terminal_order_are_enforced() -> Result<()> {
    let mut state = state(
        Arc::new(Writers::default()),
        Arc::new(Mutex::new(Vec::new())),
    );
    let stop = Arc::new(Stop::default());
    let until = Instant::now() + Duration::from_secs(5);
    assert!(state.accept_wait(need(), &stop, until).is_err());
    admitted(&mut state, &stop)?;
    let text = " \n{\"id\":9007199254740993,\"id\":1e99}\n";
    let mut wrong = guard();
    wrong.generation = "old".into();
    assert!(
        state
            .accept_wait(
                ChildFrame::Result {
                    guard: wrong,
                    offset: U64(0),
                    text: text.into()
                },
                &stop,
                until
            )
            .is_err()
    );
    assert!(state.result.is_empty());
    state.accept_wait(
        ChildFrame::Result {
            guard: guard(),
            offset: U64(0),
            text: text.into(),
        },
        &stop,
        until,
    )?;
    assert!(
        state
            .accept_wait(
                ChildFrame::Finished {
                    guard: guard(),
                    bytes: U64(text.len() as u64),
                    result_blake3: "c".repeat(64)
                },
                &stop,
                until
            )
            .is_err()
    );
    assert!(state.terminal.is_none());
    state.accept_wait(
        ChildFrame::Finished {
            guard: guard(),
            bytes: U64(text.len() as u64),
            result_blake3: blake3::hash(text.as_bytes()).to_hex().to_string(),
        },
        &stop,
        until,
    )?;
    assert!(state.terminal.as_ref().unwrap().is_ok());
    assert_eq!(state.result, text);
    assert!(state.accept_wait(need(), &stop, until).is_err());
    Ok(())
}

#[test]
fn lm_transport_batch1_legacy_supervisor_rejects_streamed_result_without_accepting_bytes()
-> Result<()> {
    let mut state = state(
        Arc::new(Writers::default()),
        Arc::new(Mutex::new(Vec::new())),
    );
    let stop = Arc::new(Stop::default());
    admitted(&mut state, &stop)?;
    let error = state
        .accept_wait(
            ChildFrame::BeginResult {
                guard: guard(),
                bytes: U64(9 * 1024 * 1024),
                blake3: "b".repeat(64),
            },
            &stop,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unsupported by legacy supervisor")
    );
    assert!(state.result.is_empty());
    assert!(state.terminal.is_none());
    Ok(())
}

mod process_tests;

#[test]
fn memory_grant_denial_and_lost_reply_keep_exact_parent_charge() -> Result<()> {
    let budget = MemoryBudget::new(150)?;
    let mut owner = State::new(
        Admit {
            writers: Arc::new(Writers::default()),
            releases: Arc::new(Mutex::new(Vec::new())),
        },
        guard(),
        "a".repeat(64),
        budget.reservation(),
    );
    let stop = Arc::new(Stop::default());
    let until = Instant::now() + Duration::from_secs(5);
    let frame = || ChildFrame::NeedMemory {
        guard: guard(),
        sequence: U64(1),
        bytes: U64(100),
    };
    // This may precede input semantic admission; it never admits writes.
    let grant = owner.accept_wait(frame(), &stop, until)?;
    assert!(matches!(
        grant,
        Some(ParentFrame::MemoryGrant {
            sequence: U64(1),
            bytes: U64(100),
            ..
        })
    ));
    drop(grant); // Simulate lost acknowledgement.
    assert_eq!(budget.used(), 100);
    assert!(owner.accept_wait(frame(), &stop, until).is_err());
    assert!(
        owner
            .accept_wait(
                ChildFrame::NeedMemory {
                    guard: guard(),
                    sequence: U64(2),
                    bytes: U64(51)
                },
                &stop,
                until
            )
            .is_err()
    );
    assert_eq!(owner.next_memory, 2);
    assert_eq!(budget.used(), 100);
    assert!(owner.accept_wait(need(), &stop, until).is_err());
    drop(owner); // In production Owned first terminates/reaps/joins Process.
    assert_eq!(budget.used(), 0);
    Ok(())
}

impl<A: Admission> State<A> {
    fn accept_wait(
        &mut self,
        frame: ChildFrame,
        stop: &Arc<Stop>,
        until: Instant,
    ) -> Result<Option<ParentFrame>> {
        let immediate = self.accept(frame, stop, until)?;
        if immediate.is_some() {
            return Ok(immediate);
        }
        while self.pending.is_some() {
            if let Some(grant) = self.poll_admission()? {
                return Ok(Some(grant));
            }
            ensure!(Instant::now() < until, "test admission deadline");
            thread::sleep(Duration::from_millis(2));
        }
        Ok(None)
    }
}

#[test]
fn writer_wait_keeps_dispatch_available_for_memory_and_cancel() -> Result<()> {
    let writers = Arc::new(Writers::default());
    let foreground = writers.enter(Priority::Foreground)?;
    let releases = Arc::new(Mutex::new(Vec::new()));
    let mut owner = state(writers.clone(), releases.clone());
    let stop = Arc::new(Stop::default());
    let until = Instant::now() + Duration::from_secs(5);
    admitted(&mut owner, &stop)?;
    assert!(owner.accept(need(), &stop, until)?.is_none());
    writers.wait_until_queued(0, 1);
    // The foreground permit is intentionally still held. This must complete on
    // this same dispatch thread; it cannot depend on admission finishing.
    assert!(matches!(
        owner.accept(
            ChildFrame::NeedMemory {
                guard: guard(),
                sequence: U64(1),
                bytes: U64(7)
            },
            &stop,
            until
        )?,
        Some(ParentFrame::MemoryGrant { bytes: U64(7), .. })
    ));
    assert!(owner.poll_admission()?.is_none());
    stop.cancel();
    drop(owner);
    assert_eq!(*releases.lock().unwrap(), [(1, WriteKind::Bootstrap)]);
    drop(foreground);
    let _next = writers.enter(Priority::Foreground)?;
    Ok(())
}

#[test]
fn real_thread_affine_permit_stays_on_waiter_until_explicit_retirement() -> Result<()> {
    use crate::catalog_writer::{ExternalAdmission, ExternalLease};
    struct External(Arc<Mutex<Vec<(thread::ThreadId, bool)>>>);
    struct AffineLease {
        owner: thread::ThreadId,
        events: Arc<Mutex<Vec<(thread::ThreadId, bool)>>>,
        _local: std::rc::Rc<()>,
    }
    impl ExternalAdmission for External {
        fn acquire(&self) -> Result<Box<dyn ExternalLease>> {
            let owner = thread::current().id();
            self.0.lock().unwrap().push((owner, false));
            Ok(Box::new(AffineLease {
                owner,
                events: self.0.clone(),
                _local: std::rc::Rc::new(()),
            }))
        }
    }
    impl ExternalLease for AffineLease {
        fn release(&mut self) {
            assert_eq!(self.owner, thread::current().id());
            self.events
                .lock()
                .unwrap()
                .push((thread::current().id(), true));
        }
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    let writers = Writers::with_external(Arc::new(External(events.clone())));
    let releases = Arc::new(Mutex::new(Vec::new()));
    let mut state = state(writers, releases.clone());
    let stop = Arc::new(Stop::default());
    admitted(&mut state, &stop)?;
    assert!(matches!(
        state.accept_wait(need(), &stop, Instant::now() + Duration::from_secs(5))?,
        Some(ParentFrame::Grant { .. })
    ));
    let acquiring = events.lock().unwrap()[0].0;
    assert_ne!(acquiring, thread::current().id());
    stop.cancel();
    assert_eq!(
        events.lock().unwrap().len(),
        1,
        "cancel alone cannot retire a granted permit"
    );
    assert!(releases.lock().unwrap().is_empty());
    // State retirement models the post-drain release boundary. It joins the
    // actual permit owner before returning the actor-hold release callback.
    state.release()?;
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[(acquiring, false), (acquiring, true)]
    );
    assert_eq!(releases.lock().unwrap().len(), 1);
    Ok(())
}

#[test]
fn transport_payload_denial_precedes_child_creation_and_retires_all_charges() -> Result<()> {
    use crate::lightroom_migration_worker::memory::{layout::add, transport};
    let payload = transport::payloads(false)?;
    assert!(
        payload.typed > 0 && payload.parser > 0 && payload.encoded > 0 && payload.diagnostics > 0
    );
    assert!(transport::payloads(true)?.total()? > payload.total()?);
    let backing = Process::<ChildFrame>::allocation_backing()?;
    let budget = MemoryBudget::new(add(add(INPUT_BYTES, backing)?, payload.total()? - 1)?)?;
    let spawned = std::cell::Cell::new(false);
    let result = execute_owned(
        |_| {
            spawned.set(true);
            anyhow::bail!("must not spawn without transport allowance")
        },
        guard(),
        "{}",
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(5),
        Admit {
            writers: Arc::new(Writers::default()),
            releases: Arc::new(Mutex::new(vec![])),
        },
        budget.clone(),
    );
    let error = result.err().context("transport budget should deny")?;
    let limit = error
        .downcast_ref::<crate::lightroom_migration_worker::memory::ResourceLimit>()
        .context("structured ResourceLimit required")?;
    assert_eq!(limit.required, payload.total()?);
    assert_eq!(limit.available, payload.total()? - 1);
    assert!(!spawned.get());
    assert_eq!(budget.used(), 0);
    println!(
        "TRANSPORT_PRESPAWN_DENIED requested={} available={} no_child=true retired=0",
        limit.required, limit.available
    );
    Ok(())
}

#[test]
fn lm_supervisor_batch2_streamed_result_denies_before_retained_allocation_then_pages_exactly()
-> Result<()> {
    let bytes = RESULT_BYTES + 37;
    let (storage, expected_pages) =
        crate::lightroom_migration_worker::protocol::result::retained_storage_bytes(bytes)?;
    let operation_bytes = 17usize;
    let pool = crate::preview::ByteBudget::new((storage + operation_bytes).try_into()?)?;
    let budget = MemoryBudget::from_shared(pool.clone());
    let mut competitor = budget.reservation();
    competitor.grow(1)?;
    let mut state = State::new_streaming(
        Admit {
            writers: Arc::new(Writers::default()),
            releases: Arc::new(Mutex::new(vec![])),
        },
        guard(),
        "a".repeat(64),
        budget.reservation(),
        budget.reservation(),
        bytes,
    );
    state.memory.grow(operation_bytes)?;
    let stop = Arc::new(Stop::default());
    admitted(&mut state, &stop)?;
    let digest = {
        let mut hash = blake3::Hasher::new();
        let block = [b'x'; crate::lightroom_migration_worker::protocol::result::CHUNK];
        let mut remaining = bytes;
        while remaining != 0 {
            let length = remaining.min(block.len());
            hash.update(&block[..length]);
            remaining -= length;
        }
        hash.finalize().to_hex().to_string()
    };
    let begin = || ChildFrame::BeginResult {
        guard: guard(),
        bytes: U64(bytes as u64),
        blake3: digest.clone(),
    };
    let error = state
        .accept_wait(begin(), &stop, Instant::now() + Duration::from_secs(5))
        .unwrap_err();
    let limit = error
        .downcast_ref::<crate::lightroom_migration_worker::memory::ResourceLimit>()
        .context("typed streamed-result ResourceLimit")?;
    assert_eq!((limit.required, limit.available), (storage, storage - 1));
    let typed = Failure::from_error(error, false, false, &budget);
    assert!(matches!(
        typed.cause,
        FailureCause::ResourceLimit(crate::lightroom_migration_worker::memory::ResourceLimit {
            required,
            available
        }) if (required, available) == (storage, storage - 1)
    ));
    assert!(!typed.poisoned && !typed.outcome_unknown);
    assert!(state.streamed_result.is_none());
    assert_eq!(pool.used(), (operation_bytes + 1) as u64);
    drop(competitor);
    assert!(matches!(
        state.accept_wait(
            begin(),
            &stop,
            Instant::now() + Duration::from_secs(5)
        )?,
        Some(ParentFrame::ResultGrant { bytes: U64(actual), .. }) if actual == bytes as u64
    ));
    assert_eq!(pool.used(), (storage + operation_bytes) as u64);
    let mut offset = 0usize;
    while offset != bytes {
        let length =
            (bytes - offset).min(crate::lightroom_migration_worker::protocol::result::CHUNK);
        state.accept_wait(
            ChildFrame::Result {
                guard: guard(),
                offset: U64(offset as u64),
                text: "x".repeat(length),
            },
            &stop,
            Instant::now() + Duration::from_secs(5),
        )?;
        offset += length;
    }
    state.accept_wait(
        ChildFrame::Finished {
            guard: guard(),
            result_blake3: digest.clone(),
            bytes: U64(bytes as u64),
        },
        &stop,
        Instant::now() + Duration::from_secs(5),
    )?;
    let result = state.take_saved_result(None)?;
    assert_eq!(result.identity(), Some((bytes, digest.as_str())));
    assert_eq!(result.page_count(), expected_pages);
    assert!(result.page(0).is_some());
    assert!(result.page(expected_pages).is_none());
    drop(state);
    assert_eq!(pool.used(), storage as u64);
    drop(result);
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn lm_supervisor_batch2_failed_parent_release_retains_attempt_until_checked_retry() -> Result<()> {
    struct RetryAdmission {
        writers: Arc<Writers>,
        failures: Arc<AtomicUsize>,
        releases: Arc<AtomicUsize>,
    }
    impl Admission for RetryAdmission {
        fn lock(&mut self, _: &str, _: &DestinationPin, _: &FileKey) -> Result<()> {
            Ok(())
        }
        fn writer(
            &mut self,
            _: u64,
            _: WriteKind,
            _: &str,
            _: Option<&FileKey>,
            _: &Stop,
            _: Instant,
        ) -> Result<Arc<Writers>> {
            Ok(self.writers.clone())
        }
        fn release(&mut self, _: u64, _: WriteKind) -> Result<()> {
            self.releases.fetch_add(1, Ordering::AcqRel);
            if self
                .failures
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_sub(1)
                })
                .is_ok()
            {
                anyhow::bail!("injected parent release acknowledgement loss")
            }
            Ok(())
        }
        fn progress(&mut self, _: &str, _: u64, _: Option<u64>) -> Result<()> {
            Ok(())
        }
    }
    let failures = Arc::new(AtomicUsize::new(1));
    let releases = Arc::new(AtomicUsize::new(0));
    let budget = MemoryBudget::new(FAILURE_BYTES + 1024)?;
    let mut state = State::new_streaming(
        RetryAdmission {
            writers: Arc::new(Writers::default()),
            failures: failures.clone(),
            releases: releases.clone(),
        },
        guard(),
        "a".repeat(64),
        budget.reservation(),
        budget.reservation(),
        0,
    );
    let stop = Arc::new(Stop::default());
    admitted(&mut state, &stop)?;
    assert!(matches!(
        state.accept_wait(need(), &stop, Instant::now() + Duration::from_secs(5))?,
        Some(ParentFrame::Grant { .. })
    ));
    let owner = Owned {
        process: None,
        broker: None,
        state,
        lm_drained: false,
        broker_drained: false,
        drain_fault: None,
        primary_broker_failure: None,
    };
    let mut operation = Operation::DrainPending(DrainPending::new(owner, None, None));
    let until = Instant::now() + Duration::from_secs(5);
    while releases.load(Ordering::Acquire) == 0 {
        assert!(operation.retry_drain().is_none());
        ensure!(Instant::now() < until, "parent release fixture deadline");
        thread::sleep(Duration::from_millis(2));
    }
    let Operation::DrainPending(pending) = &operation else {
        anyhow::bail!("lost release acknowledgement retired operation")
    };
    let failure = pending
        .failure()
        .context("release failure cause retained")?;
    assert!(
        matches!(&failure.cause, FailureCause::Rejected(detail) if detail.contains("release acknowledgement"))
    );
    assert!(failure.poisoned && failure.outcome_unknown);
    assert!(operation.retry_drain().is_some());
    assert!(matches!(operation, Operation::Drained(Drained::Failed(_))));
    assert_eq!(releases.load(Ordering::Acquire), 2);
    assert_eq!(budget.used(), FAILURE_BYTES);
    drop(operation);
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn lm_supervisor_batch2_child_terminal_poison_preserves_known_outcome() -> Result<()> {
    let operation_bytes = 17usize;
    let pool = crate::preview::ByteBudget::new((FAILURE_BYTES + operation_bytes) as u64)?;
    let budget = MemoryBudget::from_shared(pool.clone());
    let mut state = State::new_streaming(
        Admit {
            writers: Arc::new(Writers::default()),
            releases: Arc::new(Mutex::new(vec![])),
        },
        guard(),
        "a".repeat(64),
        budget.reservation(),
        budget.reservation(),
        0,
    );
    state.memory.grow(operation_bytes)?;
    let stop = Arc::new(Stop::default());
    admitted(&mut state, &stop)?;
    state.accept_wait(
        ChildFrame::Failed {
            guard: guard(),
            detail: "worker rejected an approved operation".into(),
            poisoned: true,
        },
        &stop,
        Instant::now() + Duration::from_secs(5),
    )?;
    let owner = Owned {
        process: None,
        broker: None,
        state,
        lm_drained: false,
        broker_drained: false,
        drain_fault: None,
        primary_broker_failure: None,
    };
    let detail = owner
        .state
        .terminal
        .as_ref()
        .context("terminal worker failure absent")?
        .as_ref()
        .unwrap_err()
        .to_string();
    let failure = failure_for_owner(&owner, anyhow::anyhow!(detail));
    assert!(failure.poisoned && !failure.outcome_unknown);
    let mut operation = Operation::DrainPending(DrainPending::new(owner, None, Some(failure)));
    let Some(Drained::Failed(failure)) = operation.retry_drain() else {
        anyhow::bail!("known terminal worker failure did not drain")
    };
    assert!(
        matches!(&failure.cause, FailureCause::Rejected(detail) if detail.contains("approved operation"))
    );
    assert!(failure.poisoned && !failure.outcome_unknown);
    assert_eq!(pool.used(), FAILURE_BYTES as u64);
    drop(operation);
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn lm_supervisor_batch2_rejection_text_is_typed_and_utf8_byte_bounded() -> Result<()> {
    let pool = crate::preview::ByteBudget::new(FAILURE_BYTES as u64)?;
    let budget = MemoryBudget::from_shared(pool.clone());
    let failure = Failure::from_error(
        anyhow::anyhow!("request was not canceled; policy rejected it"),
        false,
        false,
        &budget,
    );
    assert!(
        matches!(&failure.cause, FailureCause::Rejected(detail) if detail.contains("not canceled"))
    );
    assert_eq!(pool.used(), FAILURE_BYTES as u64);
    drop(failure);
    assert_eq!(pool.used(), 0);

    let source = "é🦀".repeat(FAILURE_BYTES);
    let failure = Failure::from_error(anyhow::anyhow!(source), false, false, &budget);
    let FailureCause::Rejected(detail) = &failure.cause else {
        anyhow::bail!("multibyte rejection changed type")
    };
    assert_eq!(detail.len(), FAILURE_BYTES);
    assert_eq!(detail.capacity(), FAILURE_BYTES);
    assert!(std::str::from_utf8(detail.as_bytes()).is_ok());
    assert_eq!(pool.used(), FAILURE_BYTES as u64);
    drop(failure);
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn lm_supervisor_batch2_shared_pool_guard_outlives_funded_state_owner() -> Result<()> {
    struct DropAdmission {
        pool: crate::preview::ByteBudget,
        observed: Arc<AtomicUsize>,
    }
    impl Drop for DropAdmission {
        fn drop(&mut self) {
            self.observed
                .store(self.pool.used() as usize, Ordering::Release);
        }
    }
    impl Admission for DropAdmission {
        fn lock(&mut self, _: &str, _: &DestinationPin, _: &FileKey) -> Result<()> {
            Ok(())
        }
        fn writer(
            &mut self,
            _: u64,
            _: WriteKind,
            _: &str,
            _: Option<&FileKey>,
            _: &Stop,
            _: Instant,
        ) -> Result<Arc<Writers>> {
            Ok(Arc::new(Writers::default()))
        }
        fn release(&mut self, _: u64, _: WriteKind) -> Result<()> {
            Ok(())
        }
        fn progress(&mut self, _: &str, _: u64, _: Option<u64>) -> Result<()> {
            Ok(())
        }
    }
    let pool = crate::preview::ByteBudget::new(23)?;
    let observed = Arc::new(AtomicUsize::new(0));
    let budget = MemoryBudget::from_shared(pool.clone());
    let mut state = State::new_streaming(
        DropAdmission {
            pool: pool.clone(),
            observed: observed.clone(),
        },
        guard(),
        "a".repeat(64),
        budget.reservation(),
        budget.reservation(),
        0,
    );
    state.memory.grow(23)?;
    let owner = Owned {
        process: None,
        broker: None,
        lm_drained: false,
        broker_drained: false,
        drain_fault: Some(anyhow::anyhow!("funded queued diagnostic")),
        primary_broker_failure: None,
        state,
    };
    let operation = Operation::Running(Running {
        ended: false,
        command: None,
        data: Some(ParentFrame::Cancel { guard: guard() }),
        control: Some(ParentFrame::Cancel { guard: guard() }),
        stop: Arc::new(Stop::default()),
        until: Instant::now() + Duration::from_secs(5),
        owner: Some(owner),
        result_memory: Some(budget.reservation()),
    });
    drop(operation);
    assert_eq!(observed.load(Ordering::Acquire), 23);
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn lm_executor_batch3_pre_admission_failure_is_bounded_guarded_and_terminal() -> Result<()> {
    let stop = Arc::new(Stop::default());
    let until = Instant::now() + Duration::from_secs(5);
    let failure = |guard: Guard, detail: String| ChildFrame::Failed {
        guard,
        detail,
        poisoned: true,
    };
    let mut stale = state(
        Arc::new(Writers::default()),
        Arc::new(Mutex::new(Vec::new())),
    );
    let mut wrong = guard();
    wrong.generation = "stale".into();
    assert!(
        stale
            .accept(failure(wrong, "refused".into()), &stop, until)
            .is_err()
    );
    assert!(stale.terminal.is_none());
    let mut oversized = state(
        Arc::new(Writers::default()),
        Arc::new(Mutex::new(Vec::new())),
    );
    assert!(
        oversized
            .accept(failure(guard(), "x".repeat(32 * 1024 + 1)), &stop, until)
            .is_err()
    );
    assert!(oversized.terminal.is_none());
    let mut valid = state(
        Arc::new(Writers::default()),
        Arc::new(Mutex::new(Vec::new())),
    );
    assert!(valid.accept(need(), &stop, until).is_err());
    valid.accept(failure(guard(), "startup refused".into()), &stop, until)?;
    assert!(!valid.admitted);
    assert!(valid.terminal_poisoned);
    assert_eq!(
        valid
            .terminal
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .to_string(),
        "startup refused"
    );
    assert!(valid.accept(need(), &stop, until).is_err());
    assert!(
        valid
            .accept(failure(guard(), "repeated".into()), &stop, until)
            .is_err()
    );
    assert!(valid.held.is_none());
    Ok(())
}
