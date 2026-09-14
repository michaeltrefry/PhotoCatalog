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
        let budget = MemoryBudget::from_parent(Arc::new(MemoryGrants {
            controls: controls.clone(),
            output: Arc::new(Output(tx)),
        }));
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
