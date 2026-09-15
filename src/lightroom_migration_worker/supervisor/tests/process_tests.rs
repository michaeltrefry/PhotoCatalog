use super::*;
use crate::{
    Catalog,
    lightroom_migration_worker::{
        identity::Audit,
        input,
        lease::{DestinationLease, DestinationReview},
        protocol::{Controls, Grants, Publish, read_frame, write_frame},
        source_reader::relay::{
            Command as SourceCommand, Event as SourceEvent, Kind as SourceKind,
        },
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
const PART_HELPER: &str = "lightroom_migration_worker::supervisor::tests::process_tests::multipart_admission_receiver_fixture";
const PART_FIXTURE: &str = "PHOTOCATALOG_LM_MULTIPART_RECEIVER_FIXTURE";

#[test]
fn multipart_admission_receiver_fixture() -> Result<()> {
    if std::env::var_os(PART_FIXTURE).is_none() {
        return Ok(());
    }
    let mut input = std::io::stdin();
    let ParentFrame::BeginPart {
        guard,
        role,
        blake3,
        bytes,
    } = read_frame(&mut input)?
    else {
        anyhow::bail!("multipart fixture expected BeginPart")
    };
    write_frame(
        &mut std::io::stderr(),
        &ChildFrame::NeedMemory {
            guard: guard.clone(),
            sequence: U64(1),
            bytes,
        },
    )?;
    let ParentFrame::MemoryGrant {
        guard: granted_guard,
        sequence: U64(1),
        bytes: granted_bytes,
    } = read_frame(&mut input)?
    else {
        anyhow::bail!("multipart fixture read content before MemoryGrant")
    };
    ensure!(
        granted_guard == guard && granted_bytes == bytes,
        "multipart fixture memory grant differs"
    );
    let mut received = String::with_capacity(usize::try_from(bytes.0)?);
    loop {
        match read_frame::<ParentFrame>(&mut input)? {
            ParentFrame::Part {
                guard: part_guard,
                role: part_role,
                offset,
                text,
            } => {
                ensure!(
                    part_guard == guard && part_role == role && offset.0 == received.len() as u64,
                    "multipart fixture content order differs"
                );
                received.push_str(&text);
            }
            ParentFrame::FinishPart {
                guard: part_guard,
                role: part_role,
                blake3: finished,
            } => {
                ensure!(
                    part_guard == guard
                        && part_role == role
                        && received.len() as u64 == bytes.0
                        && finished == blake3
                        && finished == blake3::hash(received.as_bytes()).to_hex().as_str(),
                    "multipart fixture completion differs"
                );
                break;
            }
            _ => anyhow::bail!("multipart fixture unexpected frame"),
        }
    }
    std::process::exit(0);
}

/// Inert during an ordinary test run. A dedicated instance of this executable
/// combines the actual lock, SQL connection and writer-control primitives.
#[test]
fn owned_executor_fixture() -> Result<()> {
    let Ok(mode) = std::env::var(FIXTURE) else {
        return Ok(());
    };
    if mode == "hang_before_input" {
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    let mut stdin = std::io::stdin();
    let request = input::receive(&mut stdin, Instant::now() + Duration::from_secs(20))?;
    let root: NativePath = serde_json::from_str(&request.text)?;
    let output: Arc<dyn Publish> = Arc::new(Mutex::new(std::io::stderr()));
    output.publish(&ChildFrame::Admitted {
        guard: request.guard.clone(),
        request_blake3: request.digest,
        build: crate::lightroom_migration_worker::worker::build_identity().into(),
    })?;
    if mode == "rejected" {
        output.publish(&ChildFrame::Failed {
            guard: request.guard.clone(),
            detail: "managed fixture rejected the operation".into(),
            poisoned: false,
        })?;
        std::process::exit(0);
    }
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
    let memory = MemoryBudget::from_parent(Arc::new(
        crate::lightroom_migration_worker::protocol::MemoryGrants::new(
            controls.clone(),
            output.clone(),
        ),
    ));
    let mut phase = memory.reservation();
    phase.grow(12345)?;
    let verified = lease.clone();
    let writers = Writers::with_external(Arc::new(Grants {
        controls: controls.clone(),
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
        ensure!(
            matches!(mode.as_str(), "normal" | "streamed_normal"),
            "unknown synthetic helper mode"
        );
    }
    drop(writers);
    drop(lease);
    drop(scope);
    if mode == "streamed_normal" {
        let value = serde_json::json!({"committed": true});
        let measured = crate::lightroom_migration_worker::protocol::result::measure(&value, 1024)?;
        let grant = controls.request_result(&measured, 1024, output.as_ref())?;
        crate::lightroom_migration_worker::protocol::result::publish(
            &value,
            grant,
            &request.guard,
            output.as_ref(),
        )?;
        std::process::exit(0);
    }
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
    actor_held: Arc<AtomicBool>,
    fail_admission: Option<bool>,
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
        self.actor_held.store(true, Ordering::Release);
        self.needs.send(sequence)?;
        if let Some(panic) = self.fail_admission {
            assert!(!panic, "synthetic acknowledgement panic after actor hold");
            anyhow::bail!("synthetic acknowledgement lost after actor hold");
        }
        Ok(self.writers.clone())
    }
    fn release(&mut self, sequence: u64, _: WriteKind) -> Result<()> {
        let unlocked = self.contender.try_lock_exclusive().is_ok();
        if unlocked {
            FileExt::unlock(&self.contender).unwrap();
        }
        self.released.lock().unwrap().push((sequence, unlocked));
        self.actor_held.store(false, Ordering::Release);
        Ok(())
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
        database: FileKey::of(catalog.session.legacy_file()?.as_ref())?,
        contender,
        needs,
        released: Arc::new(Mutex::new(Vec::new())),
        actor_held: Arc::new(AtomicBool::new(false)),
        fail_admission: None,
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
fn lm_supervisor_batch2_multipart_pumps_memory_grant_before_multichunk_content() -> Result<()> {
    multipart_transfer(false)
}

#[test]
fn lm_executor_batch3_startup_grant_fifo_when_writer_is_waiting_for_data() -> Result<()> {
    multipart_transfer(true)
}

fn multipart_transfer(force_receive_window: bool) -> Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", PART_HELPER, "--nocapture"])
        .env(PART_FIXTURE, "1");
    let stop = Arc::new(Stop::default());
    let process = Process::spawn_test_command(command, stop.clone())?;
    let gate = force_receive_window.then(|| process.force_startup_data_receive_window());
    let pid = process.pid();
    let text = format!("qualified-é-🦀-{}", "x".repeat(3 * TEXT_CHUNK));
    let budget = MemoryBudget::new(text.len())?;
    let mut owner = Owned {
        process: Some(process),
        broker: None,
        state: State::new_streaming(
            Admit {
                writers: Arc::new(Writers::default()),
                releases: Arc::new(Mutex::new(vec![])),
            },
            guard(),
            "a".repeat(64),
            budget.reservation(),
            budget.reservation(),
            0,
        ),
        lm_drained: false,
        broker_drained: false,
        drain_fault: None,
        primary_broker_failure: None,
    };
    send_part_admitted(
        &mut owner,
        &guard(),
        crate::lightroom_migration_worker::protocol::InputRole::Policy,
        &text,
        &stop,
        Instant::now() + Duration::from_secs(10),
    )?;
    if let Some(gate) = gate {
        assert!(gate.exercised());
    }
    assert_eq!(owner.state.next_memory, 2);
    assert_eq!(budget.used(), text.len());
    let until = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = owner.process.as_mut().unwrap().try_reap()? {
            break status;
        }
        ensure!(Instant::now() < until, "multipart receiver exit deadline");
        thread::sleep(Duration::from_millis(2));
    };
    while !owner.retry_drain()? {
        ensure!(Instant::now() < until, "multipart receiver drain deadline");
        thread::sleep(Duration::from_millis(2));
    }
    let drain_fault = owner.drain_fault.take();
    drop(owner);
    assert_eq!(budget.used(), 0);
    #[cfg(unix)]
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    ensure!(
        status.success(),
        "multipart receiver rejected input: {status}"
    );
    ensure!(
        drain_fault.is_none(),
        "multipart receiver drain fault: {drain_fault:?}"
    );
    println!("LM_MULTIPART child_pid={pid} grant_before_content=true chunks>3 retired=0");
    Ok(())
}
#[test]
fn lm_supervisor_batch2_lost_release_keeps_parent_permit_until_actual_executor_reap() -> Result<()>
{
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
        MemoryBudget::new(2 * 1024 * 1024 * 1024)?,
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
fn lm_supervisor_batch2_cancel_while_parent_writer_held_reaps_without_waiting_for_that_writer()
-> Result<()> {
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
            MemoryBudget::new(2 * 1024 * 1024 * 1024)?,
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

#[test]
fn lm_supervisor_batch2_failed_or_panicked_actor_acknowledgement_retires_hold_after_actual_reap()
-> Result<()> {
    for panic in [false, true] {
        let (_temp, catalog, mut parent, _needs) = setup()?;
        parent.fail_admission = Some(panic);
        let actor_held = parent.actor_held.clone();
        let released = parent.released.clone();
        let request = serde_json::to_string(&parent.root)?;
        let mut pid = None;
        let outcome = execute_owned(
            |stop| {
                let process = Process::spawn_test_command(command("normal")?, stop)?;
                pid = Some(process.pid());
                Ok(process)
            },
            guard(),
            &request,
            Arc::new(Stop::default()),
            Instant::now() + Duration::from_secs(20),
            parent,
            MemoryBudget::new(2 * 1024 * 1024 * 1024)?,
        );
        let error = outcome
            .err()
            .context("failed actor acknowledgement required")?;
        assert_eq!(error.to_string().contains("panicked"), panic);
        assert!(!actor_held.load(Ordering::Acquire));
        // The callback can acquire the physical lock only after the helper has
        // exited; no GrantWrite was sent, so its SQL table must not exist.
        assert_eq!(*released.lock().unwrap(), [(1, true)]);
        assert!(
            catalog
                .db
                .prepare("SELECT * FROM lm_owned_process_probe")
                .is_err()
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
    }
    Ok(())
}

#[test]
fn lm_supervisor_batch2_parent_pool_refuses_before_spawn_and_charges_saved_result_after_reap()
-> Result<()> {
    let (_temp, _catalog, parent, _needs) = setup()?;
    let request = serde_json::to_string(&parent.root)?;
    let tiny = MemoryBudget::new(1)?;
    let result = execute_owned(
        |_| panic!("resource refusal must precede spawn"),
        guard(),
        &request,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(5),
        parent,
        tiny.clone(),
    );
    assert!(result.is_err());
    assert_eq!(tiny.used(), 0);

    let (_temp, _catalog, parent, _needs) = setup()?;
    let request = serde_json::to_string(&parent.root)?;
    let budget = MemoryBudget::new(2 * 1024 * 1024 * 1024)?;
    let mut pid = None;
    let result = execute_owned(
        |stop| {
            let child = Process::spawn_test_command(command("normal")?, stop)?;
            pid = Some(child.pid());
            Ok(child)
        },
        guard(),
        &request,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(20),
        parent,
        budget.clone(),
    )?;
    assert_eq!(result.text(), "{\"committed\":true}");
    assert_eq!(budget.used(), RESULT_BYTES);
    #[cfg(unix)]
    {
        assert_eq!(unsafe { libc::kill(pid.unwrap() as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    drop(result);
    assert_eq!(budget.used(), 0);
    println!(
        "PARENT_MEMORY child_pid={} reaped=true saved_result_charge={} final_charge=0",
        pid.unwrap(),
        RESULT_BYTES
    );
    Ok(())
}

#[test]
fn lm_supervisor_batch2_failed_wait_stays_addressable_until_checked_retry() -> Result<()> {
    let (_temp, _catalog, parent, _needs) = setup()?;
    let request = serde_json::to_string(&parent.root)?;
    let budget = MemoryBudget::new(2 * 1024 * 1024 * 1024)?;
    let stop = Arc::new(Stop::default());
    let cancel = stop.clone();
    let (pids, seen) = mpsc::sync_channel(1);
    let canceler = thread::spawn(move || -> Result<()> {
        let _ = seen.recv_timeout(Duration::from_secs(5))?;
        thread::sleep(Duration::from_millis(20));
        cancel.cancel();
        Ok(())
    });
    let mut operation = execute_owned_operation(
        |stop| {
            let mut process = Process::spawn_test_command_owned(
                command("hang_before_input").map_err(|error| SpawnFailure {
                    error,
                    process: None,
                })?,
                stop,
            )?;
            process.inject_wait_failures(1);
            let _ = pids.send(process.pid());
            Ok(process)
        },
        guard(),
        &request,
        stop,
        Instant::now() + Duration::from_secs(10),
        parent,
        budget.clone(),
        None,
    );
    canceler
        .join()
        .map_err(|_| anyhow::anyhow!("canceler panicked"))??;
    assert!(operation.poll());
    let Operation::DrainPending(pending) = &operation else {
        anyhow::bail!("unconfirmed wait must return DrainPending")
    };
    assert!(matches!(
        pending.failure().map(|failure| &failure.cause),
        Some(FailureCause::Canceled)
    ));
    assert!(!pending.failure().unwrap().poisoned);
    let held = budget.used();
    assert!(held > 0);
    assert!(operation.retry_drain().is_none());
    let Operation::DrainPending(pending) = &operation else {
        anyhow::bail!("failed wait retired operation")
    };
    assert!(pending.failure().unwrap().poisoned);
    assert!(pending.failure().unwrap().outcome_unknown);
    let until = Instant::now() + Duration::from_secs(5);
    while operation.retry_drain().is_none() {
        ensure!(Instant::now() < until, "owned wait retry deadline");
        thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(operation, Operation::Drained(Drained::Failed(_))));
    assert_eq!(budget.used(), 0);
    println!(
        "LM_DRAIN_PENDING held_before_retry={held} typed=canceled poisoned=true outcome_unknown=true final_charge=0"
    );
    Ok(())
}

#[test]
fn lm_supervisor_batch2_postspawn_failure_is_owned_and_drop_drains_last_resort() -> Result<()> {
    let (_temp, _catalog, parent, _needs) = setup()?;
    let request = serde_json::to_string(&parent.root)?;
    let budget = MemoryBudget::new(2 * 1024 * 1024 * 1024)?;
    let mut child_pid = None;
    let operation = execute_owned_operation(
        |stop| {
            let configured = command("hang_before_input").map_err(|error| SpawnFailure {
                error,
                process: None,
            })?;
            let process = Process::spawn_test_command_owned(configured, stop)?;
            child_pid = Some(process.pid());
            Err(SpawnFailure {
                error: anyhow::anyhow!("injected failure after Child ownership"),
                process: Some(process),
            })
        },
        guard(),
        &request,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(10),
        parent,
        budget.clone(),
        None,
    );
    let Operation::DrainPending(pending) = &operation else {
        anyhow::bail!("postspawn failure must retain DrainPending")
    };
    assert!(
        matches!(pending.failure().map(|failure| &failure.cause), Some(FailureCause::Rejected(detail)) if detail.contains("after Child ownership"))
    );
    assert!(!pending.failure().unwrap().poisoned);
    let held = budget.used();
    assert!(held > 0);
    let pid = child_pid.context("postspawn child pid")?;
    drop(operation);
    assert_eq!(budget.used(), 0);
    #[cfg(unix)]
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    println!(
        "LM_POSTSPAWN_DROP child_pid={pid} held_before_drop={held} blocking_last_resort=true final_charge=0"
    );
    Ok(())
}

#[cfg(unix)]
fn broker_stop_fixture(
    internal_failure: bool,
    inject_lm_io_panic: bool,
) -> Result<(
    tempfile::TempDir,
    Operation<Admit>,
    crate::preview::ByteBudget,
    crate::preview::ByteReservation,
    u32,
    u32,
)> {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir()?;
    let source_pid_path = temp.path().join("source.pid");
    let source_executable = temp.path().join("source-wrapper");
    let current = std::env::current_exe()?;
    let current = current.to_str().context("non-UTF-8 test executable path")?;
    let source_pid_path_text = source_pid_path
        .to_str()
        .context("non-UTF-8 Source pid path")?;
    ensure!(
        !current.contains('\'')
            && !current.contains('\n')
            && !source_pid_path_text.contains('\'')
            && !source_pid_path_text.contains('\n'),
        "fixture path cannot be represented by its fixed shell wrapper"
    );
    std::fs::write(
        &source_executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{source_pid_path_text}'\nPHOTOCATALOG_OWNED_BROKER_SOURCE_FIXTURE=1 exec '{current}' --exact lightroom_migration_worker::source_reader::relay::broker::tests::owned_broker_source_fixture --nocapture\n"
        ),
    )?;
    std::fs::set_permissions(&source_executable, std::fs::Permissions::from_mode(0o700))?;

    let backing = crate::lightroom_migration_worker::memory::layout::add(
        crate::lightroom_migration_worker::memory::layout::add(
            Process::<ChildFrame>::allocation_backing()?,
            Broker::allocation_backing()?,
        )?,
        crate::lightroom_migration_worker::memory::layout::add(
            source_executable.as_os_str().len(),
            crate::lightroom_migration_worker::memory::transport::payloads(true)?.total()?,
        )?,
    )?;
    let pool = crate::preview::ByteBudget::new(u64::try_from(
        backing
            .checked_add(61)
            .context("typed Broker fixture allowance overflow")?,
    )?)?;
    let budget = MemoryBudget::from_shared(pool.clone());
    let mut operation_memory = budget.reservation();
    operation_memory.grow(backing)?;
    let competing = pool
        .reserve_exact(60)
        .map_err(|error| anyhow::anyhow!(error))?;

    let stop = Arc::new(Stop::default());
    let broker = Broker::start(source_executable, guard(), stop.clone(), budget.clone())?;
    let mut start = Some(SourceCommand::Start {
        sequence: U64(1),
        kind: SourceKind::Sql,
        reader: "typed-limit".into(),
    });
    let until = Instant::now() + Duration::from_secs(10);
    while let Some(command) = start.take() {
        start = broker.try_send(command)?;
        ensure!(Instant::now() < until, "Source start send deadline");
        thread::sleep(Duration::from_millis(2));
    }
    let token = loop {
        if let Some(SourceEvent::Started { token, .. }) = broker.try_receive()? {
            break token;
        }
        ensure!(
            Instant::now() < until,
            "Source start acknowledgement deadline"
        );
        thread::sleep(Duration::from_millis(2));
    };
    let source_pid = loop {
        match std::fs::read_to_string(&source_pid_path) {
            Ok(text) => {
                if let Ok(pid) = text.trim().parse::<u32>()
                    && pid != 0
                {
                    break pid;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(Instant::now() < until, "Source fixture pid deadline");
        thread::sleep(Duration::from_millis(2));
    };
    let mut process =
        match Process::spawn_test_command_owned(command("hang_before_input")?, stop.clone()) {
            Ok(process) => process,
            Err(mut failure) => {
                if let Some(process) = &mut failure.process {
                    process.revoke();
                    process.terminate();
                }
                return Err(failure.error);
            }
        };
    let lm_pid = process.pid();
    if inject_lm_io_panic {
        process.inject_io_panic_report();
    }

    if internal_failure {
        let mut reserve = Some(SourceCommand::Reserve {
            token,
            sequence: U64(1),
            bytes: U64(2),
        });
        while let Some(command) = reserve.take() {
            reserve = broker.try_send(command)?;
            ensure!(Instant::now() < until, "Source reserve send deadline");
            thread::sleep(Duration::from_millis(2));
        }
        while !stop.requested() {
            ensure!(Instant::now() < until, "typed Broker refusal deadline");
            thread::sleep(Duration::from_millis(2));
        }
    } else {
        stop.cancel();
    }

    let state = State::new_streaming(
        Admit {
            writers: Arc::new(Writers::default()),
            releases: Arc::new(Mutex::new(Vec::new())),
        },
        guard(),
        "a".repeat(64),
        operation_memory,
        budget.reservation(),
        0,
    );
    let operation = Operation::Running(Running {
        ended: false,
        command: None,
        data: None,
        control: None,
        stop,
        until,
        owner: Some(Owned {
            process: Some(process),
            broker: Some(broker),
            lm_drained: false,
            broker_drained: false,
            drain_fault: None,
            primary_broker_failure: None,
            state,
        }),
        result_memory: None,
    });
    Ok((temp, operation, pool, competing, lm_pid, source_pid))
}

#[cfg(unix)]
fn drain_broker_stop_fixture(operation: &mut Operation<Admit>, until: Instant) -> Result<&Drained> {
    assert!(operation.poll());
    loop {
        if operation.retry_drain().is_some() {
            break;
        }
        ensure!(Instant::now() < until, "Broker stop checked drain deadline");
        thread::sleep(Duration::from_millis(2));
    }
    match operation {
        Operation::Drained(drained) => Ok(drained),
        Operation::Running(_) | Operation::DrainPending(_) => unreachable!(),
    }
}

#[cfg(unix)]
fn transition_with_secondary_cancel(operation: &mut Operation<Admit>) {
    let pending = match operation {
        Operation::Running(running) => {
            let budget = running
                .owner
                .as_ref()
                .expect("running Broker owner")
                .state
                .memory
                .budget();
            running.take_pending(Some(Failure::from_error(
                OperationCanceled.into(),
                false,
                false,
                &budget,
            )))
        }
        Operation::Drained(_) | Operation::DrainPending(_) => {
            panic!("Broker fixture must begin running")
        }
    };
    *operation = Operation::DrainPending(pending);
}

#[cfg(unix)]
fn assert_processes_reaped(pids: [u32; 2]) {
    for pid in pids {
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[cfg(unix)]
#[test]
fn lm_supervisor_batch2_broker_resource_limit_precedes_internal_cancel_and_lm_io_panic()
-> Result<()> {
    let (_temp, mut operation, pool, competing, lm_pid, source_pid) =
        broker_stop_fixture(true, true)?;
    let drained =
        drain_broker_stop_fixture(&mut operation, Instant::now() + Duration::from_secs(10))?;
    let Drained::Failed(failure) = drained else {
        anyhow::bail!("typed Broker refusal completed successfully")
    };
    let FailureCause::ResourceLimit(limit) = &failure.cause else {
        anyhow::bail!("typed Broker refusal was replaced by {failure}")
    };
    assert_eq!((limit.required, limit.available), (2, 1));
    assert!(failure.poisoned);
    assert!(!failure.outcome_unknown);
    assert_eq!(pool.used(), 60);
    assert_processes_reaped([lm_pid, source_pid]);
    drop(operation);
    drop(competing);
    assert_eq!(pool.used(), 0);
    println!(
        "LM_BROKER_TYPED_LIMIT required=2 available=1 lm_pid={lm_pid} source_pid={source_pid} lm_io_panic=true checked_reap=true held_after_drain=60 final_charge=0"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn lm_supervisor_batch2_live_broker_external_cancel_remains_canceled() -> Result<()> {
    let (_temp, mut operation, pool, competing, lm_pid, source_pid) =
        broker_stop_fixture(false, false)?;
    let drained =
        drain_broker_stop_fixture(&mut operation, Instant::now() + Duration::from_secs(10))?;
    let Drained::Failed(failure) = drained else {
        anyhow::bail!("externally canceled Broker operation completed successfully")
    };
    assert!(matches!(failure.cause, FailureCause::Canceled));
    assert!(!failure.poisoned);
    assert!(!failure.outcome_unknown);
    assert_eq!(pool.used(), 60);
    assert_processes_reaped([lm_pid, source_pid]);
    drop(operation);
    drop(competing);
    assert_eq!(pool.used(), 0);
    println!(
        "LM_BROKER_EXTERNAL_CANCEL lm_pid={lm_pid} source_pid={source_pid} typed=canceled checked_reap=true held_after_drain=60 final_charge=0"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn lm_supervisor_batch2_joined_broker_primary_replaces_secondary_pending_cancel() -> Result<()> {
    let (_temp, mut operation, pool, competing, lm_pid, source_pid) =
        broker_stop_fixture(true, false)?;
    transition_with_secondary_cancel(&mut operation);
    let drained =
        drain_broker_stop_fixture(&mut operation, Instant::now() + Duration::from_secs(10))?;
    let Drained::Failed(failure) = drained else {
        anyhow::bail!("typed Broker refusal completed successfully")
    };
    let FailureCause::ResourceLimit(limit) = &failure.cause else {
        anyhow::bail!("secondary pending cause replaced joined Broker failure: {failure}")
    };
    assert_eq!((limit.required, limit.available), (2, 1));
    assert!(failure.poisoned);
    assert!(!failure.outcome_unknown);
    assert_eq!(pool.used(), 60);
    assert_processes_reaped([lm_pid, source_pid]);
    drop(operation);
    drop(competing);
    assert_eq!(pool.used(), 0);
    println!(
        "LM_BROKER_PRIMARY_AFTER_PENDING required=2 available=1 secondary=canceled lm_pid={lm_pid} source_pid={source_pid} checked_reap=true held_after_drain=60 final_charge=0"
    );
    Ok(())
}

#[test]
fn lm_supervisor_batch4_actual_executor_reap_releases_operation_grant_retains_only_result()
-> Result<()> {
    let (_temp, catalog, parent, _needs) = setup()?;
    let released = parent.released.clone();
    let pool = crate::preview::ByteBudget::new(2 * 1024 * 1024 * 1024)?;
    let grant =
        crate::lightroom_migration_worker::memory::SharedAllocationGrant::new(pool.clone())?;
    let budget = MemoryBudget::from_parent(grant.clone());
    let result_budget = MemoryBudget::from_shared(pool.clone());
    let request = serde_json::to_string(&parent.root)?;
    let mut pid = None;
    let operation = execute_operation_with_broker_and_result_budget(
        |stop| {
            let process = Process::spawn_test_command_owned(
                command("streamed_normal").map_err(|error| SpawnFailure {
                    error,
                    process: None,
                })?,
                stop,
            )?;
            pid = Some(process.pid());
            Ok(process)
        },
        None,
        guard(),
        &request,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(20),
        parent,
        budget,
        result_budget,
        &[],
        Some(1024),
    );
    drop(grant);
    let result = operation.drain_blocking().into_result()?;
    let text = "{\"committed\":true}";
    let digest = blake3::hash(text.as_bytes()).to_hex().to_string();
    assert_eq!(result.identity(), Some((text.len(), digest.as_str())));
    assert_eq!(result.page_count(), 1);
    assert_eq!(result.page(0), Some(text));
    let storage =
        crate::lightroom_migration_worker::protocol::result::retained_storage_bytes(text.len())?.0;
    assert_eq!(pool.used(), storage as u64);
    assert!(!released.lock().unwrap().is_empty());
    assert_eq!(
        catalog
            .db
            .query_row("SELECT value FROM lm_owned_process_probe", [], |row| row
                .get::<_, String>(
                0
            ))?,
        "committed by sole lock owner"
    );
    #[cfg(unix)]
    {
        assert_eq!(
            unsafe { libc::kill(pid.context("spawned PID")? as i32, 0) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    drop(result);
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn lm_supervisor_batch4_managed_rejection_releases_operation_charge_after_checked_reap()
-> Result<()> {
    managed_rejection_retention(false)
}

#[test]
fn lm_supervisor_batch4_managed_rejection_unknown_wait_retains_charge_until_checked_retry()
-> Result<()> {
    managed_rejection_retention(true)
}

fn managed_rejection_retention(unknown_wait: bool) -> Result<()> {
    let (_temp, _catalog, parent, _needs) = setup()?;
    let pool = crate::preview::ByteBudget::new(2 * 1024 * 1024 * 1024)?;
    let grant =
        crate::lightroom_migration_worker::memory::SharedAllocationGrant::new(pool.clone())?;
    let budget = MemoryBudget::from_parent(grant);
    let request = serde_json::to_string(&parent.root)?;
    let mut pid = None;
    let mut operation = execute_operation_with_broker_and_result_budget(
        |stop| {
            let mut process = Process::spawn_test_command_owned(
                command(if unknown_wait {
                    "hang_before_input"
                } else {
                    "rejected"
                })
                .map_err(|error| SpawnFailure {
                    error,
                    process: None,
                })?,
                stop,
            )?;
            pid = Some(process.pid());
            if unknown_wait {
                process.inject_wait_failures(1);
                return Err(SpawnFailure {
                    error: anyhow::anyhow!("managed fixture rejected the operation"),
                    process: Some(process),
                });
            }
            Ok(process)
        },
        None,
        guard(),
        &request,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(20),
        parent,
        budget,
        MemoryBudget::from_shared(pool.clone()),
        &[],
        Some(1024),
    );
    let until = Instant::now() + Duration::from_secs(10);
    while !operation.poll() {
        ensure!(Instant::now() < until, "managed rejection deadline");
        thread::sleep(Duration::from_millis(2));
    }
    let Operation::DrainPending(pending) = &operation else {
        anyhow::bail!("rejection skipped checked drain")
    };
    let failure = pending.failure().context("retained rejection")?;
    assert!(
        matches!(&failure.cause, FailureCause::Rejected(detail) if detail == "managed fixture rejected the operation")
    );
    assert!(!failure.poisoned && !failure.outcome_unknown);
    let retained = pool.used();
    assert!(retained > FAILURE_BYTES as u64);
    if unknown_wait {
        assert!(operation.retry_drain().is_none());
        let Operation::DrainPending(pending) = &operation else {
            unreachable!()
        };
        let failure = pending
            .failure()
            .context("unknown wait retains original rejection")?;
        assert!(failure.poisoned && failure.outcome_unknown);
        assert_eq!(pool.used(), retained);
    }
    while operation.retry_drain().is_none() {
        ensure!(Instant::now() < until, "managed rejection drain deadline");
        thread::sleep(Duration::from_millis(2));
    }
    let Operation::Drained(Drained::Failed(failure)) = &operation else {
        anyhow::bail!("rejection lost at drain")
    };
    assert!(
        matches!(&failure.cause, FailureCause::Rejected(detail) if detail == "managed fixture rejected the operation")
    );
    assert_eq!(failure.poisoned, unknown_wait);
    assert_eq!(failure.outcome_unknown, unknown_wait);
    assert_eq!(pool.used(), FAILURE_BYTES as u64);
    #[cfg(unix)]
    {
        assert_eq!(
            unsafe { libc::kill(pid.context("spawned PID")? as i32, 0) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    drop(operation);
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn lm_supervisor_batch4_setup_failure_retention_uses_same_pool_and_preserves_exact_refusal()
-> Result<()> {
    for available in [FAILURE_BYTES, FAILURE_BYTES - 1] {
        let (_temp, _catalog, parent, _needs) = setup()?;
        let pool = crate::preview::ByteBudget::new((12345 + available) as u64)?;
        let grant =
            crate::lightroom_migration_worker::memory::SharedAllocationGrant::new(pool.clone())?;
        let budget = MemoryBudget::from_parent(grant);
        let mut input_owner = budget.reservation();
        input_owner.grow(12345)?;
        drop(input_owner);
        let mut invalid = guard();
        invalid.session.clear();
        let operation = execute_operation_with_broker_and_result_budget(
            |_| panic!("invalid guard cannot spawn"),
            None,
            invalid,
            "{}",
            Arc::new(Stop::default()),
            Instant::now() + Duration::from_secs(10),
            parent,
            budget,
            MemoryBudget::from_shared(pool.clone()),
            &[],
            Some(1024),
        );
        let Operation::Drained(Drained::Failed(failure)) = &operation else {
            anyhow::bail!("setup failure must be drained")
        };
        assert!(!failure.poisoned && !failure.outcome_unknown);
        if available == FAILURE_BYTES {
            assert!(
                matches!(&failure.cause, FailureCause::Rejected(detail) if !detail.is_empty() && detail.len() <= FAILURE_BYTES)
            );
            assert_eq!(pool.used(), FAILURE_BYTES as u64);
        } else {
            assert!(
                matches!(&failure.cause, FailureCause::ResourceLimit(limit) if (limit.required, limit.available) == (FAILURE_BYTES, available))
            );
            assert_eq!(pool.used(), 0);
        }
        drop(operation);
        assert_eq!(pool.used(), 0);
    }
    Ok(())
}

#[test]
fn lm_supervisor_batch4_first_failure_during_unknown_wait_uses_retained_failure_budget()
-> Result<()> {
    let pool = crate::preview::ByteBudget::new((12345 + FAILURE_BYTES) as u64)?;
    let grant =
        crate::lightroom_migration_worker::memory::SharedAllocationGrant::new(pool.clone())?;
    let budget = MemoryBudget::from_parent(grant);
    let mut memory = budget.reservation();
    memory.grow(12345)?;
    drop(budget);
    let mut process =
        Process::spawn_test_command(command("hang_before_input")?, Arc::new(Stop::default()))?;
    let pid = process.pid();
    process.inject_wait_failures(1);
    let state = State::new_streaming(
        Admit {
            writers: Arc::new(Writers::default()),
            releases: Arc::new(Mutex::new(vec![])),
        },
        guard(),
        "a".repeat(64),
        memory,
        MemoryBudget::from_shared(pool.clone()).reservation(),
        1024,
    );
    let mut pending = DrainPending::new(
        Owned {
            process: Some(process),
            broker: None,
            state,
            lm_drained: false,
            broker_drained: false,
            drain_fault: None,
            primary_broker_failure: None,
        },
        None,
        None,
    );
    assert!(pending.retry_drain().is_none());
    assert!(
        matches!(&pending.failure().unwrap().cause, FailureCause::Rejected(detail) if detail == "injected migration child wait failure")
    );
    assert_eq!(pool.used(), (12345 + FAILURE_BYTES) as u64);
    let until = Instant::now() + Duration::from_secs(10);
    let drained = loop {
        if let Some(drained) = pending.retry_drain() {
            break drained;
        }
        ensure!(
            Instant::now() < until,
            "unknown wait checked retry deadline"
        );
        thread::sleep(Duration::from_millis(2));
    };
    let Drained::Failed(failure) = &drained else {
        anyhow::bail!("unknown wait failure disappeared")
    };
    assert!(failure.poisoned && failure.outcome_unknown);
    assert!(
        matches!(&failure.cause, FailureCause::Rejected(detail) if detail == "injected migration child wait failure")
    );
    assert_eq!(pool.used(), FAILURE_BYTES as u64);
    #[cfg(unix)]
    {
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    drop(drained);
    assert_eq!(pool.used(), 0);
    Ok(())
}
