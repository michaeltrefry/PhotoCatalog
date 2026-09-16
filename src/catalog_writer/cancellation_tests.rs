use super::*;
use std::sync::mpsc;

#[test]
fn canceled_waiter_retires_before_parent_writer_releases() -> Result<()> {
    let writers = Arc::new(Writers::default());
    let held = writers.enter(Priority::Foreground)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let w = writers.clone();
    let c = cancel.clone();
    let worker = thread::spawn(move || {
        tx.send(w.enter_cancellable(Priority::Background, &c, None).is_err())
            .unwrap();
    });
    writers.wait_until_queued(0, 1);
    cancel.store(true, Ordering::Release);
    writers.wake_waiters();
    assert!(rx.recv_timeout(Duration::from_secs(2))?);
    worker.join().unwrap();
    {
        let s = writers.state.lock().unwrap();
        assert!(s.owner.is_some());
        assert_eq!(s.next[1], s.serving[1]);
    }
    drop(held);
    drop(writers.enter(Priority::Foreground)?);
    drop(writers.enter(Priority::Background)?);
    Ok(())
}

#[test]
fn deadlines_retire_and_pre_cancellation_allocates_no_ticket() -> Result<()> {
    let writers = Arc::new(Writers::default());
    let held = writers.enter(Priority::Background)?;
    let w = writers.clone();
    let worker = thread::spawn(move || {
        let cancel = AtomicBool::new(false);
        w.enter_cancellable(
            Priority::Foreground,
            &cancel,
            Some(Instant::now() + Duration::from_millis(20)),
        )
        .is_err()
    });
    assert!(worker.join().unwrap());
    let cancel = AtomicBool::new(true);
    let w = writers.clone();
    assert!(
        thread::spawn(move || w
            .enter_cancellable(Priority::Foreground, &cancel, None)
            .is_err())
        .join()
        .unwrap()
    );
    let s = writers.state.lock().unwrap();
    assert_eq!(s.next[0], 1);
    assert_eq!(s.serving[0], 1);
    drop(s);
    drop(held);
    drop(writers.enter(Priority::Background)?);
    Ok(())
}

#[test]
fn retirement_ranges_preserve_live_heads_and_do_not_grow_per_cancel() {
    let mut s = State {
        next: [10002, 0],
        ..State::default()
    };
    for t in 1..10001 {
        s.retire(0, t);
    }
    assert_eq!(s.serving[0], 0);
    assert_eq!(s.retired[0].len(), 1);
    s.retire(0, 10001);
    s.retire(0, 0);
    assert_eq!(s.serving[0], 10002);
    assert!(s.retired[0].is_empty());
    let mut s = State {
        next: [9, 0],
        ..State::default()
    };
    for t in [7, 3, 5, 4, 6, 1] {
        s.retire(0, t);
    }
    assert_eq!(s.retired[0].len(), 2);
    s.retire(0, 0);
    assert_eq!(s.serving[0], 2);
    s.retire(0, 2);
    assert_eq!(s.serving[0], 8);
}

struct Admission {
    acquired: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    fail: bool,
}
struct Lease(Arc<AtomicBool>);
impl ExternalLease for Lease {
    fn release(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl ExternalAdmission for Admission {
    fn acquire(&self) -> Result<Box<dyn ExternalLease>> {
        ensure!(!self.fail, "held parent gate failed");
        self.acquired.store(true, Ordering::Release);
        Ok(Box::new(Lease(self.released.clone())))
    }
}
#[test]
fn external_admission_covers_permit_and_failure_releases_local_owner() -> Result<()> {
    let a = Arc::new(AtomicBool::new(false));
    let r = Arc::new(AtomicBool::new(false));
    let w = Writers::with_external(Arc::new(Admission {
        acquired: a.clone(),
        released: r.clone(),
        fail: false,
    }));
    let permit = w.enter(Priority::Background)?;
    assert!(a.load(Ordering::Acquire));
    assert!(!r.load(Ordering::Acquire));
    drop(permit);
    assert!(r.load(Ordering::Acquire));
    let w = Writers::with_external(Arc::new(Admission {
        acquired: a,
        released: r,
        fail: true,
    }));
    assert!(w.enter(Priority::Background).is_err());
    assert!(w.state.lock().unwrap().owner.is_none());
    Ok(())
}
