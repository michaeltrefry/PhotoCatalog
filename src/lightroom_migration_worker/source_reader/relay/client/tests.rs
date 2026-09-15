//! Pure LM listener/proxy fixtures; no G process or filesystem is opened.
use super::*;
use std::sync::{atomic::AtomicUsize, mpsc};

struct Published(mpsc::SyncSender<ChildFrame>);
impl Publish for Published {
    fn publish(&self, frame: &ChildFrame) -> Result<()> {
        let copy = serde_json::from_slice(&serde_json::to_vec(frame)?)?;
        self.0
            .try_send(copy)
            .map_err(|e| anyhow::anyhow!("fixture output queue: {e}"))
    }
}
struct Fixture {
    client: Arc<Client>,
    output: mpsc::Receiver<ChildFrame>,
    aborts: Arc<AtomicUsize>,
    budget: MemoryBudget,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.client.revoke();
    }
}
struct Opened {
    fixture: Fixture,
    remote: Remote,
    stop: Arc<Stop>,
    token: String,
}
impl Drop for Opened {
    fn drop(&mut self) {
        // Proxy revocation is not a claim that G's actual process was reaped.
        self.fixture.client.revoke();
    }
}
fn epoch() -> Epoch {
    Epoch {
        guard: Guard {
            session: "fixture".into(),
            generation: "1".into(),
            operation: "operation".into(),
        },
        reader: "sql-reader".into(),
    }
}
fn fixture() -> Result<Fixture> {
    let (tx, rx) = mpsc::sync_channel(8);
    let aborts = Arc::new(AtomicUsize::new(0));
    let count = aborts.clone();
    let budget = MemoryBudget::new(1024 * 1024)?;
    let client = Client::new(
        epoch().guard,
        Arc::new(Published(tx)),
        Arc::new(move || {
            count.fetch_add(1, Ordering::AcqRel);
        }),
        budget.clone(),
    )?;
    Ok(Fixture {
        client,
        output: rx,
        aborts,
        budget,
    })
}
fn command(f: &Fixture) -> Result<Command> {
    match f.output.recv_timeout(Duration::from_secs(2))? {
        ChildFrame::Source { guard, command } => {
            ensure!(guard == f.client.guard, "fixture outgoing guard mismatch");
            Ok(command)
        }
        _ => anyhow::bail!("unexpected non-Source fixture frame"),
    }
}
fn opened() -> Result<Opened> {
    let fixture = fixture()?;
    let client = fixture.client.clone();
    let stop = Arc::new(Stop::default());
    let stopped = stop.clone();
    let opener = thread::spawn(move || {
        client.open(
            Kind::Sql,
            epoch(),
            stopped,
            Instant::now() + Duration::from_secs(2),
        )
    });
    let start = command(&fixture);
    let token = "a".repeat(64);
    let acknowledgement = match &start {
        Ok(Command::Start { sequence, .. }) => fixture.client.accept(Event::Started {
            sequence: *sequence,
            token: token.clone(),
        }),
        _ => {
            fixture.client.revoke();
            Err(anyhow::anyhow!("fixture expected Start"))
        }
    };
    if acknowledgement.is_err() {
        fixture.client.revoke();
    }
    let result = opener.join().expect("Source proxy opener panicked");
    acknowledgement?;
    let remote = result?;
    let Command::Start {
        sequence,
        kind,
        reader,
    } = start?
    else {
        unreachable!()
    };
    assert_eq!(sequence, U64(1));
    assert_eq!(kind, Kind::Sql);
    assert_eq!(reader, epoch().reader);
    Ok(Opened {
        fixture,
        remote,
        stop,
        token,
    })
}
fn output(client: &Client, token: &str, frame: u64, reply: &Reply) -> Result<()> {
    let mut encoded = Vec::new();
    crate::lightroom_migration_worker::protocol::write_frame(&mut encoded, reply)?;
    for (part, bytes) in encoded.chunks(super::super::CHUNK).enumerate() {
        client.accept(Event::Output {
            token: token.into(),
            frame: U64(frame),
            offset: U64((part * super::super::CHUNK) as u64),
            total: U64(encoded.len() as u64),
            bytes: bytes.to_vec(),
        })?;
    }
    Ok(())
}
#[test]
fn exact_start_and_pending_input_reject_changed_retries_and_duplicate_acknowledgements()
-> Result<()> {
    let open = opened()?;
    let client = &open.fixture.client;
    assert!(
        client
            .accept(Event::Started {
                sequence: U64(1),
                token: open.token.clone()
            })
            .is_err()
    );
    assert!(
        client
            .accept(Event::Started {
                sequence: U64(999),
                token: "b".repeat(64)
            })
            .is_err()
    );
    assert!(
        client
            .accept(Event::Started {
                sequence: U64(1),
                token: "not-a-token".into()
            })
            .is_err()
    );
    assert!(
        open.remote
            .try_send(Request::Open { epoch: epoch() })?
            .is_some()
    );
    let Command::Input {
        token,
        frame,
        offset,
        total,
        bytes,
    } = command(&open.fixture)?
    else {
        anyhow::bail!("fixture expected Input")
    };
    assert_eq!(token, open.token);
    assert_eq!(frame, U64(1));
    assert_eq!(offset, U64(0));
    assert_eq!(total.0, bytes.len() as u64);
    assert!(
        open.remote
            .try_send(Request::Cancel { epoch: epoch() })
            .is_err()
    );
    assert!(
        open.remote
            .try_send(Request::Open { epoch: epoch() })?
            .is_some()
    );
    assert!(matches!(
        open.fixture.output.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert!(
        client
            .accept(Event::Accepted {
                token: token.clone(),
                frame,
                offset: U64(total.0 - 1)
            })
            .is_err()
    );
    client.accept(Event::Accepted {
        token: token.clone(),
        frame,
        offset: total,
    })?;
    assert!(
        client
            .accept(Event::Accepted {
                token,
                frame,
                offset: total
            })
            .is_err()
    );
    assert!(
        open.remote
            .try_send(Request::Open { epoch: epoch() })?
            .is_none()
    );
    assert_eq!(open.remote.slot.state.lock().unwrap().next_input, 2);
    Ok(())
}
#[test]
fn opaque_multichunk_reply_is_consumed_once_and_output_replay_is_rejected() -> Result<()> {
    let open = opened()?;
    let expected: Vec<_> = (0..20_000).map(|n| (n % 256) as u8).collect();
    let reply = Reply::Chunk {
        epoch: epoch(),
        sequence: U64(3),
        offset: U64(9),
        bytes: expected.clone(),
    };
    output(&open.fixture.client, &open.token, 1, &reply)?;
    assert!(output(&open.fixture.client, &open.token, 1, &reply).is_err());
    assert!(matches!(
        open.fixture.output.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    let Output::Frame(Reply::Chunk {
        bytes,
        sequence,
        offset,
        ..
    }) = open.remote.try_receive()?
    else {
        anyhow::bail!("fixture expected intact chunk result")
    };
    assert_eq!(bytes, expected);
    assert_eq!(sequence, U64(3));
    assert_eq!(offset, U64(9));
    let Command::Consumed { token, frame } = command(&open.fixture)? else {
        anyhow::bail!("fixture expected Consumed")
    };
    assert_eq!(token, open.token);
    assert_eq!(frame, U64(1));
    assert!(matches!(open.remote.try_receive()?, Output::Pending));
    assert!(matches!(
        open.fixture.output.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert!(output(&open.fixture.client, &open.token, 1, &reply).is_err());
    assert!(!open.stop.requested());
    Ok(())
}
#[test]
fn failed_control_cancels_epoch_with_complete_unconsumed_reply_still_queued() -> Result<()> {
    let open = opened()?;
    output(
        &open.fixture.client,
        &open.token,
        1,
        &Reply::Ready {
            epoch: epoch(),
            binding: "c".repeat(64),
        },
    )?;
    assert!(open.remote.slot.state.lock().unwrap().reply.is_some());
    open.fixture.client.accept(Event::Failed {
        token: open.token.clone(),
        detail: "Source pipe lost".into(),
    })?;
    assert!(open.stop.requested());
    assert_eq!(open.fixture.aborts.load(Ordering::Acquire), 1);
    assert!(open.remote.slot.state.lock().unwrap().reply.is_some());
    assert!(!open.remote.slot.state.lock().unwrap().closed);
    assert!(open.remote.try_receive().is_err());
    assert!(open.remote.reaped().is_err());
    assert!(matches!(
        open.fixture.output.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    Ok(())
}
#[test]
fn malformed_offsets_tokens_and_foreign_reply_epochs_are_rejected() -> Result<()> {
    let open = opened()?;
    let make = |token: String, offset| Event::Output {
        token,
        frame: U64(1),
        offset: U64(offset),
        total: U64(8),
        bytes: vec![0],
    };
    assert!(open.fixture.client.accept(make("b".repeat(64), 0)).is_err());
    assert!(
        open.fixture
            .client
            .accept(make(open.token.clone(), 1))
            .is_err()
    );
    assert!(open.remote.slot.state.lock().unwrap().incoming.is_none());
    let mut foreign = epoch();
    foreign.reader = "raw-reader".into();
    assert!(
        output(
            &open.fixture.client,
            &open.token,
            1,
            &Reply::Ready {
                epoch: foreign,
                binding: "c".repeat(64),
            }
        )
        .is_err()
    );
    let state = open.remote.slot.state.lock().unwrap();
    assert!(state.reply.is_none());
    assert_eq!(state.next_output, 1);
    Ok(())
}
#[test]
fn lost_start_acknowledgement_revoke_releases_proxy_without_claiming_g_reap() -> Result<()> {
    let fixture = fixture()?;
    let client = fixture.client.clone();
    let stop = Arc::new(Stop::default());
    let stopped = stop.clone();
    let opener = thread::spawn(move || {
        client.open(
            Kind::Raw,
            epoch(),
            stopped,
            Instant::now() + Duration::from_secs(2),
        )
    });
    let start = command(&fixture);
    fixture.client.revoke();
    let result = opener.join().expect("revoked proxy opener panicked");
    assert!(matches!(
        start?,
        Command::Start {
            kind: Kind::Raw,
            ..
        }
    ));
    assert!(result.is_err());
    assert!(stop.requested());
    assert!(
        fixture
            .client
            .slots
            .lock()
            .unwrap()
            .iter()
            .all(Option::is_none)
    );
    assert!(fixture.aborts.load(Ordering::Acquire) >= 1);
    assert!(matches!(
        fixture.output.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert!(fixture.client.check().is_err());
    Ok(())
}

#[test]
fn sql_and_raw_live_slots_cannot_share_a_start_token() -> Result<()> {
    let open = opened()?;
    let client = open.fixture.client.clone();
    let raw_stop = Arc::new(Stop::default());
    let stopped = raw_stop.clone();
    let mut raw_epoch = epoch();
    raw_epoch.reader = "raw-reader".into();
    let opener = thread::spawn(move || {
        client.open(
            Kind::Raw,
            raw_epoch,
            stopped,
            Instant::now() + Duration::from_secs(2),
        )
    });
    let start = command(&open.fixture);
    let (duplicate, unique) = match &start {
        Ok(Command::Start { sequence, .. }) => (
            open.fixture.client.accept(Event::Started {
                sequence: *sequence,
                token: open.token.clone(),
            }),
            open.fixture.client.accept(Event::Started {
                sequence: *sequence,
                token: "b".repeat(64),
            }),
        ),
        _ => (
            Err(anyhow::anyhow!("expected raw Start")),
            Err(anyhow::anyhow!("raw Start missing")),
        ),
    };
    if unique.is_err() {
        open.fixture.client.revoke();
    }
    let raw = opener.join().expect("raw proxy opener panicked");
    // Revoke before unwrapping or asserting so any unexpected accepted token
    // cannot cause Remote::drop to wait for a mock G which is not running.
    open.fixture.client.revoke();
    assert!(matches!(
        start?,
        Command::Start {
            kind: Kind::Raw,
            sequence: U64(2),
            ..
        }
    ));
    assert!(duplicate.is_err());
    unique?;
    let raw = raw?;
    assert_eq!(
        raw.slot.state.lock().unwrap().token.as_deref(),
        Some("b".repeat(64).as_str())
    );
    assert!(open.stop.requested());
    assert!(raw_stop.requested());
    drop(raw);
    Ok(())
}

#[test]
fn scoped_allocation_requires_exact_ack_and_rejects_replay() -> Result<()> {
    let open = opened()?;
    let budget = open.remote.allocation_budget();
    let worker = thread::spawn(move || {
        let mut reservation = budget.reservation();
        reservation.grow(37)?;
        Ok::<_, anyhow::Error>(reservation)
    });
    let result = (|| -> Result<()> {
        assert!(
            matches!(command(&open.fixture)?, Command::Reserve { token, sequence: U64(1), bytes: U64(37) } if token == open.token)
        );
        assert!(
            open.fixture
                .client
                .accept(Event::Reserved {
                    token: open.token.clone(),
                    sequence: U64(2),
                    bytes: U64(37)
                })
                .is_err()
        );
        assert!(
            open.fixture
                .client
                .accept(Event::Reserved {
                    token: open.token.clone(),
                    sequence: U64(1),
                    bytes: U64(36)
                })
                .is_err()
        );
        open.fixture.client.accept(Event::Reserved {
            token: open.token.clone(),
            sequence: U64(1),
            bytes: U64(37),
        })?;
        Ok(())
    })();
    if result.is_err() {
        open.fixture.client.revoke();
    }
    let reservation = worker.join().expect("allocation waiter panicked");
    result?;
    let reservation = reservation?;
    assert!(
        open.fixture
            .client
            .accept(Event::Reserved {
                token: open.token.clone(),
                sequence: U64(1),
                bytes: U64(37)
            })
            .is_err()
    );
    drop(reservation);
    assert!(
        open.fixture.output.try_recv().is_err(),
        "dropping child reservation cannot publish a scope release"
    );
    Ok(())
}

#[test]
fn worker_quiescence_clears_queued_payloads_and_waits_for_exact_ack() -> Result<()> {
    let open = opened()?;
    assert!(
        open.remote
            .try_send(Request::Cancel { epoch: epoch() })?
            .is_some()
    );
    assert!(matches!(command(&open.fixture)?, Command::Input { .. }));
    output(
        &open.fixture.client,
        &open.token,
        1,
        &Reply::Retired { epoch: epoch() },
    )?;
    open.fixture.client.accept(Event::Drained {
        token: open.token.clone(),
    })?;
    {
        let state = open.remote.slot.state.lock().unwrap();
        assert!(state.sending.is_some() && state.reply.is_some());
        assert!(!state.quiesced);
    }
    thread::scope(|scope| -> Result<()> {
        let worker = scope.spawn(|| open.remote.quiesce());
        let result = (|| -> Result<()> {
            assert!(
                matches!(command(&open.fixture)?, Command::Quiesced { token } if token == open.token)
            );
            {
                let state = open.remote.slot.state.lock().unwrap();
                assert!(
                    state.sending.is_none() && state.reply.is_none() && state.incoming.is_none()
                );
                assert!(!state.quiesced);
            }
            assert!(
                open.fixture
                    .client
                    .open(
                        Kind::Sql,
                        epoch(),
                        Arc::new(Stop::default()),
                        Instant::now()
                    )
                    .is_err()
            );
            assert!(
                open.fixture
                    .client
                    .accept(Event::Quiesced {
                        token: "f".repeat(64)
                    })
                    .is_err()
            );
            open.fixture.client.accept(Event::Quiesced {
                token: open.token.clone(),
            })?;
            Ok(())
        })();
        if result.is_err() {
            open.fixture.client.revoke();
        }
        let joined = worker.join().expect("quiescence worker panicked");
        result?;
        joined?;
        Ok(())
    })?;
    assert!(
        open.fixture
            .client
            .accept(Event::Quiesced {
                token: open.token.clone()
            })
            .is_err()
    );
    Ok(())
}

#[test]
fn operation_admission_retains_all_opening_roles_and_result_high_water() -> Result<()> {
    let fixture = fixture()?;
    let client = &fixture.client;
    let baseline = fixture.budget.used();
    assert!(baseline > 0);
    client.admit_opening(Kind::Sql, 100)?;
    client.admit_opening(Kind::Raw, 50)?;
    client.admit_opening(Kind::CaptureSql, 25)?;
    client.admit_result(20, 10)?;
    assert_eq!(fixture.budget.used(), baseline + 225);
    client.admit_result(5, 20)?;
    client.admit_opening(Kind::Raw, 10)?;
    assert_eq!(fixture.budget.used(), baseline + 255);
    // A denied growth must not update any of the successful owner maxima.
    let denied = client.admit_result(1024 * 1024, 1).unwrap_err();
    let denied = denied
        .downcast_ref::<crate::lightroom_migration_worker::memory::ResourceLimit>()
        .unwrap();
    assert_eq!(denied.available, 1024 * 1024 - baseline - 255);
    assert!(client.admit_result(usize::MAX, 1).is_err());
    let owner = client.operation_memory.lock().unwrap();
    assert_eq!(fixture.budget.used(), baseline + 255);
    assert_eq!(owner.transient, 20);
    assert_eq!(owner.retained_graph, 20);
    assert_eq!(owner.opening, [100, 50, 25]);
    drop(owner);
    client.admit_producer(Kind::Sql, 30)?;
    client.admit_producer(Kind::Raw, 40)?;
    client.admit_producer(Kind::CaptureSql, 20)?;
    assert_eq!(fixture.budget.used(), baseline + 345);
    client.admit_producer(Kind::Sql, 10)?;
    assert_eq!(fixture.budget.used(), baseline + 345);
    assert!(client.admit_producer(Kind::Raw, usize::MAX).is_err());
    assert_eq!(fixture.budget.used(), baseline + 345);
    assert_eq!(
        client.operation_memory.lock().unwrap().producer,
        [30, 40, 20]
    );
    Ok(())
}

#[test]
fn core_phase_grants_add_to_source_owners_and_denial_keeps_high_water() -> Result<()> {
    let f = fixture()?;
    let baseline = f.budget.used();
    f.client.admit_opening(Kind::Sql, 101)?;
    f.client.admit_producer(Kind::Raw, 103)?;
    f.client.admit_result(107, 109)?;
    let source = 101 + 103 + 107 + 3 * 109;
    assert_eq!(f.budget.used(), baseline + source);
    f.client.admit_core(113)?;
    f.client.admit_core(11)?;
    assert_eq!(f.budget.used(), baseline + source + 113);
    let available = f.budget.snapshot()?.available;
    let denied = f.client.admit_core(114 + available).unwrap_err();
    let limit = denied
        .downcast_ref::<crate::lightroom_migration_worker::memory::ResourceLimit>()
        .expect("structured core resource limit");
    assert_eq!(limit.required, available + 1);
    assert_eq!(limit.available, available);
    assert_eq!(f.budget.used(), baseline + source + 113);
    f.client.admit_result(127, 131)?;
    assert_eq!(f.budget.used(), baseline + 101 + 103 + 127 + 3 * 131 + 113);
    let pool = f.budget.clone();
    drop(f);
    assert_eq!(pool.used(), 0);
    println!(
        "CORE_PHASE_ADMISSION additive=true denied_before_growth=true retained_until_client_drop=true"
    );
    Ok(())
}
