use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
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
    fn release(&mut self, sequence: u64, kind: WriteKind) {
        self.releases.lock().unwrap().push((sequence, kind));
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
fn admitted(state: &mut State<Admit>, stop: &Arc<Stop>) -> Result<()> {
    state.accept_wait(
        ChildFrame::Admitted {
            guard: guard(),
            request_blake3: "a".repeat(64),
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
