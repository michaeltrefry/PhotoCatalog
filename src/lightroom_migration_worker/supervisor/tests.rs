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
    State::new(Admit { writers, releases }, guard(), "a".repeat(64))
}
fn admitted(state: &mut State<Admit>, stop: &Stop) -> Result<()> {
    state.accept(
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
    let stop = Stop::default();
    admitted(&mut state, &stop)?;
    assert!(matches!(
        state.accept(need(), &stop, Instant::now() + Duration::from_secs(5))?,
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
            .accept(
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
    state.accept(
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
                .accept(
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
    let stop = Stop::default();
    let until = Instant::now() + Duration::from_secs(5);
    assert!(state.accept(need(), &stop, until).is_err());
    admitted(&mut state, &stop)?;
    let text = " \n{\"id\":9007199254740993,\"id\":1e99}\n";
    let mut wrong = guard();
    wrong.generation = "old".into();
    assert!(
        state
            .accept(
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
    state.accept(
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
            .accept(
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
    state.accept(
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
    assert!(state.accept(need(), &stop, until).is_err());
    Ok(())
}

mod process_tests;
