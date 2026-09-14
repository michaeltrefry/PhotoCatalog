use super::*;
use crate::{
    application::U64,
    lightroom_migration_worker::{
        protocol::{read_frame, write_frame},
        source_reader::transport::{Epoch, Request},
    },
};
use std::{process::Command as OsCommand, time::Instant};
const HELPER: &str =
    "lightroom_migration_worker::source_reader::relay::broker::tests::owned_broker_source_fixture";
const ENV: &str = "PHOTOCATALOG_OWNED_BROKER_SOURCE_FIXTURE";

#[test]
fn owned_broker_source_fixture() -> Result<()> {
    if std::env::var_os(ENV).is_none() {
        return Ok(());
    }
    if std::env::var_os("PHOTOCATALOG_BROKER_TYPED_SQL").is_some() {
        crate::lightroom_migration_worker::source_reader::owner::serve_mode(
            std::io::stdin(),
            std::io::stderr(),
            Some(Kind::Sql),
        )?;
        std::process::exit(0);
    }
    if let Ok(address) = std::env::var("PHOTOCATALOG_BROKER_ERROR_BARRIER") {
        use std::io::{Read, Write};
        let mut barrier = std::net::TcpStream::connect(address)?;
        barrier.set_read_timeout(Some(Duration::from_secs(15)))?;
        let role: u8 = std::env::var("PHOTOCATALOG_BROKER_ERROR_ROLE")?.parse()?;
        barrier.write_all(&[role])?;
        let mut mode = [0u8];
        barrier.read_exact(&mut mode)?;
        if mode[0] == 1 {
            // Detection may immediately revoke this helper; acknowledge the
            // trigger before emitting malformed bytes. Reserved Failed proves
            // that the subsequent bytes were actually observed by G.
            barrier.write_all(&[99])?;
            std::io::stderr().write_all(&[0, 0, 0, 1, b'!'])?;
            std::io::stderr().flush()?;
        } else {
            #[cfg(unix)]
            {
                ensure!(
                    mode[0] == 2 && unsafe { libc::close(0) } == 0,
                    "close fixture input"
                );
                // Parent injects its write only after this completed close.
                barrier.write_all(&[99])?;
            }
            #[cfg(not(unix))]
            anyhow::bail!("input-close fixture requires Unix");
        }
        // Keep this actual Source alive after the observed transport failure.
        // G's checked kill/reap must retire it; fixture EOF is not our trigger.
        barrier.read_exact(&mut mode)?;
        anyhow::bail!("unexpected fixture barrier release")
    }
    loop {
        let request: Request = read_frame(&mut std::io::stdin())?;
        match request {
            Request::Cancel { .. } => std::process::exit(42),
            Request::Retire { epoch, .. } => {
                write_frame(&mut std::io::stderr(), &Reply::Retired { epoch })?;
                std::process::exit(0);
            }
            _ => {}
        }
    }
}
fn guard() -> Guard {
    Guard {
        session: "broker-fixture".into(),
        generation: "1".into(),
        operation: "operation".into(),
    }
}
fn epoch(reader: &str) -> Epoch {
    Epoch {
        guard: guard(),
        reader: reader.into(),
    }
}
fn broker() -> Result<(Broker, Arc<Stop>, Arc<Mutex<Vec<u32>>>)> {
    broker_with_budget(MemoryBudget::new(1024 * 1024)?)
}
fn broker_with_budget(budget: MemoryBudget) -> Result<(Broker, Arc<Stop>, Arc<Mutex<Vec<u32>>>)> {
    let stop = Arc::new(Stop::default());
    let pids = Arc::new(Mutex::new(Vec::with_capacity(2)));
    let seen = pids.clone();
    let broker = Broker::start_with(
        guard(),
        stop.clone(),
        budget,
        move |_, stop, before_wait| {
            let mut command = OsCommand::new(std::env::current_exe()?);
            command
                .args(["--exact", HELPER, "--nocapture"])
                .env(ENV, "1");
            crate::lightroom_migration_worker::process::source_environment(&mut command);
            let process =
                Process::spawn_test_command_with_cleanup(command, stop, Some(before_wait))?;
            seen.lock().unwrap().push(process.pid());
            Ok(process)
        },
    )?;
    Ok((broker, stop, pids))
}
fn send(broker: &Broker, mut command: Command) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match broker.try_send(command)? {
            None => return Ok(()),
            Some(next) => command = next,
        }
        ensure!(Instant::now() < until, "broker fixture send deadline");
        thread::sleep(Duration::from_millis(2));
    }
}
fn event(broker: &Broker) -> Result<Event> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(event) = broker.try_receive()? {
            return Ok(event);
        }
        ensure!(Instant::now() < until, "broker fixture event deadline");
        thread::sleep(Duration::from_millis(2));
    }
}
fn start(broker: &Broker, sequence: u64, kind: Kind, reader: &str) -> Result<String> {
    send(
        broker,
        Command::Start {
            sequence: U64(sequence),
            kind,
            reader: reader.into(),
        },
    )?;
    match event(broker)? {
        Event::Started {
            sequence: actual,
            token,
        } => {
            assert_eq!(actual, U64(sequence));
            Ok(token)
        }
        _ => anyhow::bail!("expected exact Started"),
    }
}
fn input(broker: &Broker, token: &str, request: &Request) -> Result<()> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, request)?;
    assert!(bytes.len() < super::super::CHUNK);
    send(
        broker,
        Command::Input {
            token: token.into(),
            frame: U64(1),
            offset: U64(0),
            total: U64(bytes.len() as u64),
            bytes,
        },
    )
}
fn absent(pids: &Arc<Mutex<Vec<u32>>>) {
    let pids = pids.lock().unwrap();
    assert!(!pids.is_empty());
    for &pid in pids.iter() {
        #[cfg(unix)]
        {
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        println!("BROKER_CHILD_REAPED pid={pid} checked_broker_join=true");
    }
}
#[test]
fn both_actual_sources_revoke_before_broker_join() -> Result<()> {
    let (mut broker, _, pids) = broker()?;
    let sql = start(&broker, 1, Kind::Sql, "sql")?;
    let raw = start(&broker, 2, Kind::Raw, "raw")?;
    assert_ne!(sql, raw);
    assert!(broker.ensure_idle().is_err());
    // This isolated broker fixture has no LM child; the caller supplies the
    // revocation acknowledgement only after its own executor is absent.
    broker.revoke_after_lm();
    broker.wait_revoked();
    assert!(broker.shared.state.lock().unwrap().revoked);
    broker.finish()?;
    assert_eq!(pids.lock().unwrap().len(), 2);
    absent(&pids);
    Ok(())
}
#[test]
fn unexpected_source_death_has_reserved_control_and_waits_for_lm_revoke() -> Result<()> {
    let (mut broker, stop, pids) = broker()?;
    let sql = start(&broker, 1, Kind::Sql, "sql")?;
    // Leave ordinary events unconsumed; the exact failure has a separate slot.
    send(
        &broker,
        Command::Start {
            sequence: U64(2),
            kind: Kind::Raw,
            reader: "raw".into(),
        },
    )?;
    input(
        &broker,
        &sql,
        &Request::Cancel {
            epoch: epoch("sql"),
        },
    )?;
    let until = Instant::now() + Duration::from_secs(5);
    while !stop.requested() {
        ensure!(
            Instant::now() < until,
            "broker fixture Source death deadline"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let failure = broker.try_urgent().context("reserved failure absent")?;
    assert!(matches!(failure, Event::Failed { .. }));
    assert!(
        !broker.worker.as_ref().unwrap().is_finished(),
        "must retain Source owners before G's executor-revoked acknowledgement"
    );
    broker.revoke_after_lm();
    broker.wait_revoked();
    assert!(broker.finish().is_err());
    absent(&pids);
    Ok(())
}
#[test]
fn orderly_retirement_keeps_reply_until_consumed_then_checks_actual_reap() -> Result<()> {
    let (mut broker, stop, pids) = broker()?;
    let token = start(&broker, 1, Kind::Sql, "sql")?;
    input(
        &broker,
        &token,
        &Request::Retire {
            epoch: epoch("sql"),
            completed: U64(0),
            chain: "a".repeat(64),
        },
    )?;
    assert!(matches!(event(&broker)?, Event::Accepted { .. }));
    let Event::Output {
        token: actual,
        frame,
        offset,
        total,
        bytes,
    } = event(&broker)?
    else {
        anyhow::bail!("Retired output missing")
    };
    assert_eq!(actual, token);
    assert_eq!(offset, U64(0));
    assert_eq!(total.0, bytes.len() as u64);
    let reply: Reply = read_frame(&mut std::io::Cursor::new(bytes))?;
    assert!(matches!(reply, Reply::Retired { .. }));
    assert!(broker.try_receive()?.is_none());
    assert!(!stop.requested());
    send(
        &broker,
        Command::Consumed {
            token: token.clone(),
            frame,
        },
    )?;
    assert!(matches!(event(&broker)?, Event::Drained { token: actual } if actual == token));
    assert!(
        broker.ensure_idle().is_err(),
        "Drained alone cannot release the epoch"
    );
    send(
        &broker,
        Command::Quiesced {
            token: token.clone(),
        },
    )?;
    assert!(matches!(event(&broker)?, Event::Quiesced { token: actual } if actual == token));
    broker.ensure_idle()?;
    broker.revoke_after_lm();
    broker.finish()?;
    absent(&pids);
    Ok(())
}

#[test]
fn observed_full_event_queue_cannot_delay_revoking_both_sources() -> Result<()> {
    let (mut broker, stop, pids) = broker()?;
    send(
        &broker,
        Command::Start {
            sequence: U64(1),
            kind: Kind::Sql,
            reader: "sql".into(),
        },
    )?;
    send(
        &broker,
        Command::Start {
            sequence: U64(2),
            kind: Kind::Raw,
            reader: "raw".into(),
        },
    )?;
    let token = crate::lightroom::digest(&crate::lightroom::bounded_json(
        &(epoch("sql"), U64(1), Kind::Sql),
        4096,
    )?);
    input(
        &broker,
        &token,
        &Request::Begin {
            epoch: epoch("sql"),
            bytes: U64(0),
            blake3: blake3::hash(b"").to_hex().to_string(),
        },
    )?;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let state = broker.shared.state.lock().unwrap();
        if state.event_full {
            // No event has been received by this fixture, so the observed Full
            // remains full at the immediately following cancellation.
            stop.cancel();
            drop(state);
            break;
        }
        drop(state);
        ensure!(
            Instant::now() < until,
            "normal Source queue did not report Full"
        );
        thread::sleep(Duration::from_millis(2));
    }
    broker.wait_revoked();
    assert!(!broker.worker.as_ref().unwrap().is_finished());
    broker.revoke_after_lm();
    broker.finish()?;
    assert_eq!(pids.lock().unwrap().len(), 2);
    absent(&pids);
    println!("BROKER_FULL_CANCEL observed_full=true sources=2 revoke_before_drain=true");
    Ok(())
}

#[test]
fn live_transport_errors_bypass_observed_full_events_before_any_dequeue() -> Result<()> {
    use std::io::{Read, Write};
    let modes: &[u8] = if cfg!(unix) { &[1, 2] } else { &[1] };
    for &mode in modes {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(Stop::default());
        let pids = Arc::new(Mutex::new(Vec::with_capacity(2)));
        let seen = pids.clone();
        let injection = Arc::new(Mutex::new(None));
        let captured = injection.clone();
        let mut broker = Broker::start_with(
            guard(),
            stop.clone(),
            MemoryBudget::new(1024 * 1024)?,
            move |kind, child_stop, before_wait| {
                let mut command = OsCommand::new(std::env::current_exe()?);
                command
                    .args(["--exact", HELPER, "--nocapture"])
                    .env(ENV, "1")
                    .env("PHOTOCATALOG_BROKER_ERROR_BARRIER", address.to_string())
                    .env(
                        "PHOTOCATALOG_BROKER_ERROR_ROLE",
                        if kind == Kind::Sql { "1" } else { "2" },
                    );
                crate::lightroom_migration_worker::process::source_environment(&mut command);
                let process = Process::spawn_test_command_with_cleanup(
                    command,
                    child_stop,
                    Some(before_wait),
                )?;
                seen.lock().unwrap().push(process.pid());
                if kind == Kind::Sql {
                    *captured.lock().unwrap() = Some(process.input_failure_probe());
                }
                Ok(process)
            },
        )?;
        send(
            &broker,
            Command::Start {
                sequence: U64(1),
                kind: Kind::Sql,
                reader: "sql".into(),
            },
        )?;
        send(
            &broker,
            Command::Start {
                sequence: U64(2),
                kind: Kind::Raw,
                reader: "raw".into(),
            },
        )?;
        let token = crate::lightroom::digest(&crate::lightroom::bounded_json(
            &(epoch("sql"), U64(1), Kind::Sql),
            4096,
        )?);
        input(
            &broker,
            &token,
            &Request::Begin {
                epoch: epoch("sql"),
                bytes: U64(0),
                blake3: blake3::hash(b"").to_hex().to_string(),
            },
        )?;
        let until = Instant::now() + Duration::from_secs(10);
        let mut sql = None;
        let mut raw = None;
        while sql.is_none() || raw.is_none() {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Accepted sockets may inherit the listener's nonblocking
                    // mode. This barrier uses bounded blocking reads explicitly.
                    stream
                        .set_nonblocking(false)
                        .context("fixture barrier blocking mode")?;
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    let mut role = [0u8];
                    stream
                        .read_exact(&mut role)
                        .context("fixture barrier role")?;
                    match role[0] {
                        1 => sql = Some(stream),
                        2 => raw = Some(stream),
                        _ => anyhow::bail!("unknown fixture role"),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            ensure!(Instant::now() < until, "fixture connection deadline");
            thread::sleep(Duration::from_millis(2));
        }
        while !broker.shared.state.lock().unwrap().event_full {
            ensure!(Instant::now() < until, "fixture never observed Full");
            thread::sleep(Duration::from_millis(2));
        }
        let mut sql = sql.unwrap();
        sql.write_all(&[mode]).context("fixture barrier trigger")?;
        let mut ack = [0u8];
        sql.read_exact(&mut ack)
            .context("fixture barrier trigger acknowledgement")?;
        assert_eq!(ack, [99]);
        if mode == 2 {
            let probe = injection
                .lock()
                .unwrap()
                .clone()
                .context("SQL input probe absent")?;
            let mut bytes = Vec::new();
            write_frame(
                &mut bytes,
                &Request::Cancel {
                    epoch: epoch("sql"),
                },
            )?;
            *probe.lock().unwrap() = Some(bytes);
        }
        while !stop.requested() {
            ensure!(
                Instant::now() < until,
                "live transport error hidden by ordinary events"
            );
            thread::sleep(Duration::from_millis(2));
        }
        assert!(matches!(broker.try_urgent(), Some(Event::Failed { .. })));
        assert!(!broker.worker.as_ref().unwrap().is_finished());
        broker.revoke_after_lm();
        broker.wait_revoked();
        assert!(broker.finish().is_err());
        absent(&pids);
        println!(
            "LIVE_SOURCE_TRANSPORT_ERROR mode={mode} observed_full=true dequeued_events=0 reserved_failure=true checked_drain=true"
        );
        drop(sql);
        drop(raw);
    }
    Ok(())
}

#[test]
fn source_charge_needs_quiescence_and_reuses_only_exact_retired_epoch() -> Result<()> {
    let budget = MemoryBudget::new(100)?;
    let mut caller = budget.reservation();
    caller.grow(10)?;
    let (mut broker, _, pids) = broker_with_budget(budget.clone())?;
    let sql = start(&broker, 1, Kind::Sql, "sql")?;
    let raw = start(&broker, 2, Kind::Raw, "raw")?;
    for (token, bytes) in [(&sql, 30), (&raw, 20)] {
        send(
            &broker,
            Command::Reserve {
                token: token.clone(),
                sequence: U64(1),
                bytes: U64(bytes),
            },
        )?;
        assert!(
            matches!(event(&broker)?, Event::Reserved { token: actual, sequence: U64(1), bytes: amount } if actual == *token && amount == U64(bytes))
        );
    }
    assert_eq!(budget.used(), 60);
    send(&broker, Command::Drain { token: sql.clone() })?;
    assert!(matches!(event(&broker)?, Event::Drained { token } if token == sql));
    assert_eq!(
        budget.used(),
        60,
        "Source wait/joins alone must retain its charge"
    );
    assert!(broker.ensure_idle().is_err());
    send(&broker, Command::Quiesced { token: sql.clone() })?;
    assert!(matches!(event(&broker)?, Event::Quiesced { token } if token == sql));
    assert_eq!(
        budget.used(),
        30,
        "only SQL charge retires; caller and raw remain"
    );
    let next = start(&broker, 3, Kind::Sql, "sql-new")?;
    assert_ne!(next, sql);
    send(
        &broker,
        Command::Reserve {
            token: next,
            sequence: U64(1),
            bytes: U64(30),
        },
    )?;
    assert!(matches!(
        event(&broker)?,
        Event::Reserved { bytes: U64(30), .. }
    ));
    assert_eq!(budget.used(), 60);
    broker.revoke_after_lm();
    broker.wait_revoked();
    broker.finish()?;
    absent(&pids);
    assert_eq!(
        budget.used(),
        60,
        "broker thread exit is not LM/operation drain"
    );
    drop(broker); // production G drops this owner only after LM and all I/O wait.
    assert_eq!(budget.used(), 10);
    drop(caller);
    assert_eq!(budget.used(), 0);
    println!(
        "SOURCE_SCOPE_RETIRED after_wait=60 after_quiesced=30 after_broker_thread=60 after_owner=10 final=0"
    );
    Ok(())
}

#[test]
fn stale_quiescence_early_reuse_and_denial_retain_source_charge() -> Result<()> {
    for invalid in 0..4 {
        let budget = MemoryBudget::new(100)?;
        let (mut broker, stop, pids) = broker_with_budget(budget.clone())?;
        let token = start(&broker, 1, Kind::Sql, "sql")?;
        send(
            &broker,
            Command::Reserve {
                token: token.clone(),
                sequence: U64(1),
                bytes: U64(40),
            },
        )?;
        assert!(matches!(event(&broker)?, Event::Reserved { .. }));
        let bad = if invalid == 3 {
            Command::Reserve {
                token: token.clone(),
                sequence: U64(2),
                bytes: U64(61),
            }
        } else if invalid == 0 {
            Command::Quiesced {
                token: token.clone(),
            }
        } else {
            send(
                &broker,
                Command::Drain {
                    token: token.clone(),
                },
            )?;
            assert!(matches!(event(&broker)?, Event::Drained { .. }));
            if invalid == 1 {
                Command::Quiesced {
                    token: "f".repeat(64),
                }
            } else {
                Command::Start {
                    sequence: U64(2),
                    kind: Kind::Sql,
                    reader: "too-early".into(),
                }
            }
        };
        send(&broker, bad)?;
        let until = Instant::now() + Duration::from_secs(5);
        while !stop.requested() {
            ensure!(Instant::now() < until, "invalid quiescence did not revoke");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(budget.used(), 40);
        if invalid == 3 {
            assert!(
                matches!(broker.try_urgent(), Some(Event::Failed { detail, .. })
                if detail.contains("requested 61 additional bytes, 60 available"))
            );
        }
        broker.revoke_after_lm();
        broker.wait_revoked();
        assert!(broker.finish().is_err());
        absent(&pids);
        assert_eq!(budget.used(), 40);
        drop(broker);
        assert_eq!(budget.used(), 0);
    }
    Ok(())
}

#[test]
fn typed_source_opening_uses_g_pool_and_quiesces_after_public_result_moves() -> Result<()> {
    use crate::lightroom::migration_source::{MigrationRead, ReadLimits, tests::Fixture};
    use crate::lightroom_migration_worker::{
        protocol::{ChildFrame, Publish, SourceListener},
        source_reader::{SqlReader, relay::client::Client},
    };
    use std::sync::atomic::AtomicBool;
    struct Published(mpsc::SyncSender<ChildFrame>);
    impl Publish for Published {
        fn publish(&self, frame: &ChildFrame) -> Result<()> {
            let copy = serde_json::from_slice(&serde_json::to_vec(frame)?)?;
            self.0
                .try_send(copy)
                .map_err(|e| anyhow::anyhow!("typed relay fixture output: {e}"))
        }
    }
    let fixture = Fixture::new();
    let revision = fixture.revision().to_owned();
    let budget = MemoryBudget::new(1024 * 1024)?;
    let mut caller = budget.reservation();
    caller.grow(17)?;
    let stop = Arc::new(Stop::default());
    let pids = Arc::new(Mutex::new(Vec::new()));
    let seen = pids.clone();
    let mut broker = Broker::start_with(
        guard(),
        stop.clone(),
        budget.clone(),
        move |kind, child_stop, before_wait| {
            assert_eq!(kind, Kind::Sql);
            let mut command = OsCommand::new(std::env::current_exe()?);
            command
                .args(["--exact", HELPER, "--nocapture"])
                .env(ENV, "1")
                .env("PHOTOCATALOG_BROKER_TYPED_SQL", "1");
            crate::lightroom_migration_worker::process::source_environment(&mut command);
            let process =
                Process::spawn_test_command_with_cleanup(command, child_stop, Some(before_wait))?;
            seen.lock().unwrap().push(process.pid());
            Ok(process)
        },
    )?;
    let (output, incoming) = mpsc::sync_channel(4);
    let abort = stop.clone();
    let client = Client::new(
        guard(),
        Arc::new(Published(output)),
        Arc::new(move || abort.cancel()),
    )?;
    let worker_client = client.clone();
    let seal = fixture.seal.clone();
    let observed = budget.clone();
    let worker = thread::spawn(move || -> Result<_> {
        let source = SqlReader::open(
            worker_client,
            guard(),
            "typed-sql".into(),
            seal,
            ReadLimits::default(),
            vec![],
            Arc::new(AtomicBool::new(false)),
        )?;
        let admitted = observed.used();
        ensure!(
            admitted > 17,
            "typed opening made no parent scope reservation"
        );
        let manifest = source.capture_manifest(&revision)?;
        drop(source);
        ensure!(
            observed.used() == 17,
            "typed Source Drop omitted checked quiescence"
        );
        Ok((manifest, admitted))
    });
    let pumped = (|| -> Result<()> {
        let until = Instant::now() + Duration::from_secs(15);
        let mut pending = None;
        while !worker.is_finished() {
            ensure!(
                !stop.requested() && Instant::now() < until,
                "typed relay stopped/deadline"
            );
            if let Some(event) = broker.try_urgent() {
                client.accept(event)?;
            }
            if let Some(event) = broker.try_receive()? {
                client.accept(event)?;
            }
            if pending.is_none() {
                match incoming.try_recv() {
                    Ok(ChildFrame::Source {
                        guard: actual,
                        command,
                    }) => {
                        ensure!(actual == guard(), "typed guard");
                        pending = Some(command);
                    }
                    Ok(_) => anyhow::bail!("unexpected typed relay frame"),
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => anyhow::bail!("typed worker output lost"),
                }
            }
            if let Some(command) = pending.take() {
                pending = broker.try_send(command)?;
            }
            thread::sleep(Duration::from_millis(1));
        }
        broker.ensure_idle()?;
        Ok(())
    })();
    if pumped.is_err() {
        client.revoke();
        stop.cancel();
    }
    // This fixture substitutes a joined worker thread for LM, not an LM Child.
    // The actual Source Child still has the production G wait/pipe owner.
    broker.revoke_after_lm();
    broker.wait_revoked();
    let joined = worker.join();
    let drained = broker.finish();
    absent(&pids);
    drop(broker);
    pumped?;
    let (manifest, admitted) = joined.expect("typed Source consumer panicked")?;
    drained?;
    assert_eq!(budget.used(), 17);
    drop(manifest); // Public result storage belongs to caller/operation admission.
    drop(caller);
    assert_eq!(budget.used(), 0);
    println!(
        "TYPED_G_SCOPE admitted={admitted} source_quiesced=17 caller_released=0 worker_thread_joined=true"
    );
    Ok(())
}
