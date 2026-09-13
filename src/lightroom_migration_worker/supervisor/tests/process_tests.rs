use super::*;
use crate::{
    Catalog,
    lightroom_migration_worker::{
        identity::Audit,
        input,
        lease::{DestinationLease, DestinationReview},
        protocol::{Controls, Grants, Publish, read_frame},
    },
    storage_volume::NativePath,
};
use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    process::Command,
    sync::mpsc,
};
const HELPER: &str =
    "lightroom_migration_worker::supervisor::tests::process_tests::owned_executor_fixture";
const FIXTURE: &str = "PHOTOCATALOG_LM_OWNED_EXECUTOR_FIXTURE";

/// Inert during an ordinary test run. A dedicated instance of this executable
/// combines the actual lock, SQL connection and writer-control primitives.
#[test]
fn owned_executor_fixture() -> Result<()> {
    let Ok(mode) = std::env::var(FIXTURE) else {
        return Ok(());
    };
    let mut stdin = std::io::stdin();
    let request = input::receive(&mut stdin, Instant::now() + Duration::from_secs(20))?;
    let root: NativePath = serde_json::from_str(&request.text)?;
    let output: Arc<dyn Publish> = Arc::new(Mutex::new(std::io::stderr()));
    output.publish(&ChildFrame::Admitted {
        guard: request.guard.clone(),
        request_blake3: request.digest,
    })?;
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let scope = audit.install()?;
    let review = DestinationReview::existing(&root, None, &audit)?;
    let lease = Arc::new(DestinationLease::acquire(
        review,
        None,
        Instant::now() + Duration::from_secs(10),
    )?);
    let target = "b".repeat(64);
    output.publish(&ChildFrame::LockAcquired {
        guard: request.guard.clone(),
        lock: lease.lock_key().clone(),
        destination: lease.pin().clone(),
        target_token: target.clone(),
    })?;
    let controls = Controls::new(
        request.guard.clone(),
        audit.clone(),
        Instant::now() + Duration::from_secs(20),
    )?;
    let listener = controls.clone();
    thread::spawn(move || {
        loop {
            let accepted =
                read_frame::<ParentFrame>(&mut stdin).and_then(|frame| listener.accept(frame));
            if accepted.is_err() {
                listener.poison();
                // Owner loss is process termination, not a thread-only cancel that
                // could leave the only SQL executor working without ownership.
                std::process::exit(74);
            }
        }
    });
    let verified = lease.clone();
    let writers = Writers::with_external(Arc::new(Grants {
        controls,
        output: output.clone(),
        write: WriteKind::Catalog,
        target_token: target,
        lock: Some(lease.lock_key().clone()),
        verify: Arc::new(move || verified.verify()),
    }));
    {
        let mut catalog = lease.open_current(writers.clone())?;
        let _permit = writers.enter(Priority::Background)?;
        let tx = catalog.db.transaction()?;
        tx.execute_batch("CREATE TABLE lm_owned_process_probe(value TEXT NOT NULL); INSERT INTO lm_owned_process_probe VALUES('committed by sole lock owner');")?;
        tx.commit()?;
        if mode == "lost_release" {
            // Deliberately bypass destructors/ReleaseWrite after a real commit.
            // The parent must retain its permit until it has reaped this PID.
            std::process::exit(75);
        }
        ensure!(mode == "normal", "unknown synthetic helper mode");
    }
    drop(writers);
    drop(lease);
    drop(scope);
    let result = "{\"committed\":true}";
    output.publish(&ChildFrame::Result {
        guard: request.guard.clone(),
        offset: U64(0),
        text: result.into(),
    })?;
    output.publish(&ChildFrame::Finished {
        guard: request.guard,
        result_blake3: blake3::hash(result.as_bytes()).to_hex().to_string(),
        bytes: U64(result.len() as u64),
    })?;
    std::process::exit(0);
}
struct Parent {
    writers: Arc<Writers>,
    root: NativePath,
    database: FileKey,
    contender: File,
    needs: mpsc::Sender<u64>,
    released: Arc<Mutex<Vec<(u64, bool)>>>,
}
impl Admission for Parent {
    fn lock(&mut self, _: &str, destination: &DestinationPin, _: &FileKey) -> Result<()> {
        ensure!(
            destination.root == self.root && destination.database_key == self.database,
            "helper fixture destination differs"
        );
        Ok(())
    }
    fn writer(
        &mut self,
        sequence: u64,
        _: WriteKind,
        _: &str,
        _: Option<&FileKey>,
        _: &Stop,
        _: Instant,
    ) -> Result<Arc<Writers>> {
        self.needs.send(sequence)?;
        Ok(self.writers.clone())
    }
    fn release(&mut self, sequence: u64, _: WriteKind) {
        let unlocked = self.contender.try_lock_exclusive().is_ok();
        if unlocked {
            FileExt::unlock(&self.contender).unwrap();
        }
        self.released.lock().unwrap().push((sequence, unlocked));
    }
    fn progress(&mut self, _: &str, _: u64, _: Option<u64>) -> Result<()> {
        Ok(())
    }
}
fn setup() -> Result<(tempfile::TempDir, Catalog, Parent, mpsc::Receiver<u64>)> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?.join("destination");
    let catalog = Catalog::open(&root)?;
    // Same path/open flags as the retained Lightroom CLI lock contract.
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(root.join(".lightroom-import.lock"))?;
    let (needs, receiver) = mpsc::channel();
    let parent = Parent {
        writers: catalog.writers.clone(),
        root: NativePath::from_path(&root),
        database: FileKey::of(&catalog.relink_file)?,
        contender,
        needs,
        released: Arc::new(Mutex::new(Vec::new())),
    };
    Ok((temp, catalog, parent, receiver))
}
fn command(mode: &str) -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", HELPER, "--nocapture"])
        .env(FIXTURE, mode);
    Ok(command)
}
#[test]
fn lost_release_keeps_parent_permit_until_actual_executor_reap() -> Result<()> {
    let (_temp, catalog, parent, _needs) = setup()?;
    let released = parent.released.clone();
    let stop = Arc::new(Stop::default());
    let request = serde_json::to_string(&parent.root)?;
    let mut pid = None;
    let result = execute_owned(
        |stop| {
            let process = Process::spawn_test_command(command("lost_release")?, stop)?;
            pid = Some(process.pid());
            Ok(process)
        },
        guard(),
        &request,
        stop,
        Instant::now() + Duration::from_secs(20),
        parent,
    );
    assert!(result.is_err());
    assert_eq!(*released.lock().unwrap(), [(1, false), (2, true)]);
    assert_eq!(
        catalog
            .db
            .query_row("SELECT value FROM lm_owned_process_probe", [], |r| r
                .get::<_, String>(0))?,
        "committed by sole lock owner"
    );
    #[cfg(unix)]
    {
        assert_eq!(unsafe { libc::kill(pid.unwrap() as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    let _permit = catalog.writers.enter(Priority::Foreground)?;
    Ok(())
}
#[test]
fn cancel_while_parent_writer_held_reaps_without_waiting_for_that_writer() -> Result<()> {
    let (_temp, catalog, parent, needs) = setup()?;
    let held = catalog.writers.enter(Priority::Foreground)?;
    let stop = Arc::new(Stop::default());
    let worker_stop = stop.clone();
    let request = serde_json::to_string(&parent.root)?;
    let (pid_tx, pid_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        execute_owned(
            |stop| {
                let process = Process::spawn_test_command(command("normal")?, stop)?;
                pid_tx.send(process.pid())?;
                Ok(process)
            },
            guard(),
            &request,
            worker_stop,
            Instant::now() + Duration::from_secs(20),
            parent,
        )
    });
    // Always cancel and join the owned helper before reporting a readiness
    // failure, including an assertion from the bounded queue fixture.
    let readiness = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        ensure!(
            needs.recv_timeout(Duration::from_secs(5))? == 1,
            "unexpected first writer request"
        );
        catalog.writers.wait_until_queued(0, 1);
        Ok(())
    }));
    stop.cancel();
    catalog.writers.wake_waiters();
    let result = worker.join().expect("supervisor panicked");
    match readiness {
        Ok(ready) => ready?,
        Err(panic) => std::panic::resume_unwind(panic),
    }
    assert!(result.is_err());
    let pid = pid_rx.recv_timeout(Duration::from_secs(1))?;
    #[cfg(unix)]
    {
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    #[cfg(not(unix))]
    let _ = pid;
    assert!(
        catalog
            .db
            .prepare("SELECT * FROM lm_owned_process_probe")
            .is_err()
    );
    // This release occurs after join: cancel could not have depended on it.
    drop(held);
    let _next = catalog.writers.enter(Priority::Background)?;
    Ok(())
}
