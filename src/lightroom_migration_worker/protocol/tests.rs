use super::*;
use crate::catalog_writer::{Priority, Writers};
use std::{
    io::Cursor,
    sync::{atomic::AtomicBool, mpsc},
};
fn guard() -> Guard {
    Guard {
        session: "session".into(),
        generation: "generation".into(),
        operation: "operation".into(),
    }
}
struct Output(mpsc::Sender<Vec<u8>>);
impl Publish for Output {
    fn publish(&self, frame: &ChildFrame) -> Result<()> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, frame)?;
        self.0.send(bytes)?;
        Ok(())
    }
}
#[test]
fn frame_admission_precedes_body_allocation_and_keeps_exact_strings() -> Result<()> {
    let raw = " {\"n\":900719925474099312345,\"__proto__\":{},\"n\":1e9} \n";
    let frame = ParentFrame::Input {
        guard: guard(),
        offset: U64(u64::MAX),
        text: raw.into(),
    };
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &frame)?;
    let result: ParentFrame = read_frame(&mut Cursor::new(bytes))?;
    match result {
        ParentFrame::Input { offset, text, .. } => {
            assert_eq!(offset.0, u64::MAX);
            assert_eq!(text, raw);
        }
        _ => panic!("different frame"),
    }
    let mut prefix = Cursor::new((FRAME_BYTES as u32 + 1).to_be_bytes());
    assert!(
        read_frame::<ParentFrame>(&mut prefix)
            .unwrap_err()
            .to_string()
            .contains("byte admission")
    );
    let large = ParentFrame::Input {
        guard: guard(),
        offset: U64(0),
        text: "\0".repeat(FRAME_BYTES / 2),
    };
    let mut output = Vec::new();
    assert!(write_frame(&mut output, &large).is_err());
    assert!(output.is_empty());
    let unknown=b"{\"kind\":\"Cancel\",\"guard\":{\"session\":\"session\",\"generation\":\"generation\",\"operation\":\"operation\"},\"unexpected\":true}";
    let mut bytes = (unknown.len() as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(unknown);
    assert!(read_frame::<ParentFrame>(&mut Cursor::new(bytes)).is_err());
    Ok(())
}
#[test]
fn exact_grant_controls_entire_writer_lease_and_rejects_replay() -> Result<()> {
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let controls = Controls::new(
        guard(),
        audit.clone(),
        Instant::now() + Duration::from_secs(5),
    )?;
    let (tx, rx) = mpsc::channel();
    let grants = Grants {
        controls: controls.clone(),
        output: Arc::new(Output(tx)),
        write: WriteKind::Bootstrap,
        target_token: "reviewed-root".into(),
        lock: None,
        verify: Arc::new(|| Ok(())),
    };
    let w = Writers::with_external(Arc::new(grants));
    let (held, waiting) = mpsc::channel();
    let (release, done) = mpsc::channel();
    let worker = std::thread::spawn(move || -> Result<()> {
        let _permit = w.enter(Priority::Background)?;
        held.send(())?;
        done.recv()?;
        Ok(())
    });
    let need: ChildFrame = read_frame(&mut Cursor::new(rx.recv_timeout(Duration::from_secs(2))?))?;
    let sequence = match need {
        ChildFrame::NeedWrite {
            sequence, write, ..
        } => {
            assert_eq!(write, WriteKind::Bootstrap);
            sequence
        }
        _ => panic!("grant was not requested"),
    };
    assert!(waiting.try_recv().is_err());
    controls.accept(ParentFrame::Grant {
        guard: guard(),
        sequence,
        write: WriteKind::Bootstrap,
    })?;
    waiting.recv_timeout(Duration::from_secs(2))?;
    release.send(())?;
    worker.join().unwrap()?;
    let released: ChildFrame =
        read_frame(&mut Cursor::new(rx.recv_timeout(Duration::from_secs(2))?))?;
    assert!(matches!(released,ChildFrame::ReleaseWrite{sequence:s,..} if s==sequence));
    assert!(
        controls
            .accept(ParentFrame::Grant {
                guard: guard(),
                sequence,
                write: WriteKind::Bootstrap
            })
            .is_err()
    );
    assert!(audit.is_poisoned());
    Ok(())
}
#[test]
fn cancellation_releases_waiting_helper_without_a_grant() -> Result<()> {
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let controls = Controls::new(guard(), audit, Instant::now() + Duration::from_secs(5))?;
    let (tx, rx) = mpsc::channel();
    let grants = Grants {
        controls: controls.clone(),
        output: Arc::new(Output(tx)),
        write: WriteKind::Bootstrap,
        target_token: "reviewed-root".into(),
        lock: None,
        verify: Arc::new(|| Ok(())),
    };
    let worker = std::thread::spawn(move || grants.acquire().is_err());
    rx.recv_timeout(Duration::from_secs(2))?;
    controls.accept(ParentFrame::Cancel { guard: guard() })?;
    assert!(worker.join().unwrap());
    assert!(rx.try_recv().is_err());
    Ok(())
}

#[test]
fn memory_grant_exact_echo_cancel_and_parent_owned_retirement() -> Result<()> {
    use crate::lightroom_migration_worker::memory::MemoryBudget;
    for cancel in [false, true] {
        let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
        let controls = Controls::new(
            guard(),
            audit.clone(),
            Instant::now() + Duration::from_secs(5),
        )?;
        let (tx, rx) = mpsc::channel();
        let budget = MemoryBudget::from_parent(Arc::new(MemoryGrants::new(
            controls.clone(),
            Arc::new(Output(tx)),
        )));
        assert!(budget.snapshot().is_err());
        let worker = std::thread::spawn(move || -> Result<()> {
            let mut reservation = budget.reservation();
            reservation.grow(123)?;
            drop(reservation);
            Ok(())
        });
        let frame: ChildFrame =
            read_frame(&mut Cursor::new(rx.recv_timeout(Duration::from_secs(2))?))?;
        let ChildFrame::NeedMemory {
            sequence, bytes, ..
        } = frame
        else {
            anyhow::bail!("memory request required");
        };
        assert_eq!(bytes.0, 123);
        if cancel {
            controls.accept(ParentFrame::Cancel { guard: guard() })?;
            assert!(worker.join().unwrap().is_err());
        } else {
            controls.accept(ParentFrame::MemoryGrant {
                guard: guard(),
                sequence,
                bytes,
            })?;
            worker.join().unwrap()?;
            // No implicit release when a child-side reservation drops. Parent
            // retirement is bound to its verified process/consumer ownership.
            assert!(rx.try_recv().is_err());
            assert!(
                controls
                    .accept(ParentFrame::MemoryGrant {
                        guard: guard(),
                        sequence,
                        bytes
                    })
                    .is_err()
            );
            assert!(audit.is_poisoned());
        }
    }
    Ok(())
}

#[test]
fn lm_executor_batch3_startup_memory_pump_precedes_content_and_hands_off_exactly() -> Result<()> {
    use crate::lightroom_migration_worker::memory::MemoryBudget;
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let controls = Controls::new(guard(), audit, Instant::now() + Duration::from_secs(5))?;
    let mut startup = Vec::new();
    write_frame(
        &mut startup,
        &ParentFrame::MemoryGrant {
            guard: guard(),
            sequence: U64(1),
            bytes: U64(123),
        },
    )?;
    let mut startup = Cursor::new(startup);
    let (tx, rx) = mpsc::channel();
    let grants = Arc::new(MemoryGrants::with_startup(
        controls.clone(),
        Arc::new(Output(tx)),
        move || read_frame(&mut startup),
    ));
    let budget = MemoryBudget::from_parent(grants.clone());
    let mut retained = budget.reservation();
    retained.grow(123)?;
    let need: ChildFrame = read_frame(&mut Cursor::new(rx.recv()?))?;
    assert!(matches!(
        need,
        ChildFrame::NeedMemory {
            sequence: U64(1),
            bytes: U64(123),
            ..
        }
    ));
    grants.finish_startup()?;

    // The next request is serviced through the listener/Condvar path. A
    // successful handoff cannot strand it in the removed startup pump.
    let mut later = budget.reservation();
    let worker = std::thread::spawn(move || later.grow(7));
    let need: ChildFrame = read_frame(&mut Cursor::new(rx.recv()?))?;
    let ChildFrame::NeedMemory {
        sequence, bytes, ..
    } = need
    else {
        anyhow::bail!("post-startup memory request required")
    };
    assert_eq!((sequence.0, bytes.0), (2, 7));
    controls.accept(ParentFrame::MemoryGrant {
        guard: guard(),
        sequence,
        bytes,
    })?;
    worker.join().unwrap()?;

    let mut refused = Vec::new();
    for frame in [
        ParentFrame::Cancel { guard: guard() },
        ParentFrame::Part {
            guard: guard(),
            role: InputRole::Policy,
            offset: U64(0),
            text: "content-before-grant".into(),
        },
    ] {
        let mut encoded = Vec::new();
        write_frame(&mut encoded, &frame)?;
        refused.push(encoded);
    }
    refused.push(Vec::new());
    for encoded in refused {
        let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
        let controls = Controls::new(guard(), audit, Instant::now() + Duration::from_secs(5))?;
        let mut encoded = Cursor::new(encoded);
        let (tx, rx) = mpsc::channel();
        let grants = Arc::new(MemoryGrants::with_startup(
            controls,
            Arc::new(Output(tx)),
            move || read_frame(&mut encoded),
        ));
        let budget = MemoryBudget::from_parent(grants);
        assert!(budget.reservation().grow(1).is_err());
        assert!(matches!(
            read_frame::<ChildFrame>(&mut Cursor::new(rx.recv()?))?,
            ChildFrame::NeedMemory { bytes: U64(1), .. }
        ));
    }
    Ok(())
}

#[test]
fn lm_transport_batch1_result_grant_precedes_exact_stream_and_rejects_unadmitted_bound()
-> Result<()> {
    let value = serde_json::json!({
        "qualified": true,
        "mixed": format!("é🦀{}", "x".repeat(40 * 1024)),
    });
    let expected = serde_json::to_string(&value)?;
    let expected_output = expected.clone();
    let measured = result::measure(&value, expected.len())?;
    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let controls = Controls::new(
        guard(),
        audit.clone(),
        Instant::now() + Duration::from_secs(5),
    )?;
    let (tx, rx) = mpsc::channel();
    let output = Arc::new(Output(tx));
    let worker_controls = controls.clone();
    let worker_output = output.clone();
    let worker_measured = measured.clone();
    let worker_guard = guard();
    let worker = std::thread::spawn(move || -> Result<()> {
        let grant = worker_controls.request_result(
            &worker_measured,
            expected.len(),
            worker_output.as_ref(),
        )?;
        result::publish(&value, grant, &worker_guard, worker_output.as_ref())
    });
    let begin: ChildFrame = read_frame(&mut Cursor::new(rx.recv_timeout(Duration::from_secs(2))?))?;
    let ChildFrame::BeginResult {
        guard: begin_guard,
        bytes,
        blake3,
    } = begin
    else {
        anyhow::bail!("result admission request required");
    };
    assert_eq!(begin_guard, guard());
    assert_eq!(bytes.0, measured.bytes as u64);
    assert_eq!(blake3, measured.blake3);
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    controls.accept(ParentFrame::ResultGrant {
        guard: guard(),
        bytes,
        blake3: blake3.clone(),
    })?;
    worker.join().expect("result worker panicked")?;
    drop(output);
    let mut reconstructed = String::new();
    let mut finished = false;
    for encoded in rx {
        let frame: ChildFrame = read_frame(&mut Cursor::new(encoded))?;
        ensure!(!finished, "frame followed terminal result");
        match frame {
            ChildFrame::Result {
                guard: frame_guard,
                offset,
                text,
            } => {
                assert_eq!(frame_guard, guard());
                assert_eq!(offset.0, reconstructed.len() as u64);
                reconstructed.push_str(&text);
            }
            ChildFrame::Finished {
                guard: frame_guard,
                bytes: finished_bytes,
                result_blake3,
            } => {
                assert_eq!(frame_guard, guard());
                assert_eq!(finished_bytes, bytes);
                assert_eq!(result_blake3, blake3);
                finished = true;
            }
            _ => anyhow::bail!("result chunk/terminal required after grant"),
        }
    }
    assert!(finished);
    assert_eq!(reconstructed, expected_output);
    assert_eq!(
        blake3::hash(reconstructed.as_bytes()).to_hex().as_str(),
        blake3
    );

    assert!(
        controls
            .accept(ParentFrame::ResultGrant {
                guard: guard(),
                bytes,
                blake3,
            })
            .is_err()
    );
    assert!(audit.is_poisoned());

    let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
    let controls = Controls::new(guard(), audit, Instant::now() + Duration::from_secs(1))?;
    let (tx, rx) = mpsc::channel();
    let denied = controls.request_result(&measured, measured.bytes - 1, &Output(tx));
    assert!(
        denied
            .err()
            .context("result bound should refuse")?
            .to_string()
            .contains("accepted-type byte bound")
    );
    assert!(rx.try_recv().is_err());
    Ok(())
}
