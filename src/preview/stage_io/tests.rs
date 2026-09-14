use super::*;
use crate::catalog_backup::RestoreStatus;
use crate::catalog_session::{
    CatalogBootstrap, ConfirmSqlAdmission, PhysicalObjectId, PrepareCatalog, SqlAdmissionConfirmed,
};
use std::collections::VecDeque;

enum Response {
    Unit,
    Admitted(LeaseId),
    LegacyRead(LeaseId, u64),
    Chunk(Vec<u8>),
    Lost,
    WrongSession,
    Terminal,
    ForgedTerminal,
    Canceled,
    HeldChunk {
        bytes: Vec<u8>,
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    },
}
struct Script {
    responses: Mutex<VecDeque<Response>>,
    requests: Mutex<Vec<Request>>,
}
impl CatalogFilesystem for Script {
    fn preview_stage_call(&self, request: &Request, cancel: &AtomicBool) -> Result<Reply> {
        self.requests.lock().unwrap().push(request.clone());
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected dispatch");
        let mut reply = Reply {
            epoch: request.root.epoch.clone(),
            session: request.root.session.clone(),
            operation: request.operation,
            value: Value::Unit,
        };
        match response {
            Response::Unit => {}
            Response::Admitted(stage) => {
                reply.value = Value::Admitted {
                    stage,
                    ready: true,
                    error: None,
                }
            }
            Response::LegacyRead(transfer, bytes) => {
                reply.value = Value::LegacyRead {
                    transfer: Some(transfer),
                    bytes: U64(bytes),
                };
            }
            Response::Chunk(bytes) => reply.value = Value::Chunk { bytes },
            Response::HeldChunk {
                bytes,
                entered,
                release,
            } => {
                entered.send(())?;
                release.recv()?;
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(Failure::new(
                        FailureKind::Canceled,
                        "held read canceled before effects",
                    )
                    .into());
                }
                reply.value = Value::Chunk { bytes };
            }
            Response::Lost => {
                return Err(Failure::new(FailureKind::Unknown, "reply lost after effects").into());
            }
            Response::WrongSession => reply.session = LeaseId::new(),
            Response::Terminal | Response::ForgedTerminal => {
                let mut error =
                    Failure::new(FailureKind::Unknown, "authenticated operation failure");
                let mut request_digest = digest(request)?;
                if matches!(response, Response::ForgedTerminal) {
                    request_digest[0] ^= 1;
                }
                error.object_receipt = Some(crate::catalog_session::preview_io::FailureReceipt {
                    operation: request.operation,
                    step: U64(0),
                    request_digest,
                });
                return Err(error.into());
            }
            Response::Canceled => {
                return Err(Failure::new(FailureKind::Canceled, "canceled before effects").into());
            }
        }
        Ok(reply)
    }
    fn prepare_catalog(&self, _: &PrepareCatalog, _: &AtomicBool) -> Result<CatalogBootstrap> {
        anyhow::bail!("unexpected catalog admission")
    }
    fn abandon_prepare(&self, _: U64, _: &LeaseId) -> Result<()> {
        anyhow::bail!("unexpected abandon")
    }
    fn confirm_sql_admission(
        &self,
        _: &ConfirmSqlAdmission,
        _: &AtomicBool,
    ) -> Result<SqlAdmissionConfirmed> {
        anyhow::bail!("unexpected SQL admission")
    }
    fn restore_status(&self, _: &RootCapability) -> Result<Option<RestoreStatus>> {
        anyhow::bail!("unexpected restore")
    }
    fn resume_restored_jobs(&self, _: &RootCapability, _: &str, _: bool) -> Result<RestoreStatus> {
        anyhow::bail!("unexpected resume")
    }
    fn release_root(&self, _: &RootCapability) -> Result<()> {
        anyhow::bail!("unexpected root release")
    }
}

fn admission() -> Action {
    Action::Admit {
        limits: Limits {
            workers: 2,
            encoded: U64(1024),
            rgb: U64(1024),
            prepared: U64(1024),
        },
    }
}

#[test]
fn identical_admission_from_another_lane_cannot_replay_or_share_the_stage() -> Result<()> {
    let first_stage = LeaseId::new();
    let second_stage = LeaseId::new();
    let (calls, script, _) = fixture([
        Response::Lost,
        Response::Admitted(first_stage.clone()),
        Response::Admitted(second_stage.clone()),
    ]);
    let first = calls.lane()?;
    let second = calls.lane()?;
    let cancel = AtomicBool::new(false);
    assert!(first.call(admission(), &cancel).is_err());
    let original = {
        let state = calls.state.lock().unwrap();
        assert_eq!(state.pending_lane, first.lane);
        digest(state.pending.as_ref().unwrap())?
    };
    let error = second.call(admission(), &cancel).unwrap_err();
    assert!(error.downcast_ref::<Busy>().is_some(), "{error:#}");
    assert!(second.reconcile_value().is_err());
    assert!(second.reconcile_cleanup().is_err());
    assert_eq!(script.requests.lock().unwrap().len(), 1);
    assert_eq!(
        digest(calls.state.lock().unwrap().pending.as_ref().unwrap())?,
        original
    );
    let Some(Value::Admitted {
        stage,
        ready: true,
        error: None,
    }) = first.reconcile_value()?
    else {
        anyhow::bail!("original admission reply missing")
    };
    assert_eq!(stage, first_stage);
    let Value::Admitted {
        stage,
        ready: true,
        error: None,
    } = second.call(admission(), &cancel)?
    else {
        anyhow::bail!("second admission reply missing")
    };
    assert_eq!(stage, second_stage);
    assert_ne!(first_stage, second_stage);
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(digest(&requests[0])?, digest(&requests[1])?);
    assert!(requests[2].operation.0 > requests[1].operation.0);
    assert_eq!(
        serde_json::to_value(&requests[0].action)?,
        serde_json::to_value(&requests[2].action)?
    );
    assert!(calls.state.lock().unwrap().pending.is_none());
    // Lanes share the native identity allocator too; a new lane cannot reuse
    // an earlier worker identity in G's retained high-water fence.
    assert_eq!(first.native_operation()?, U64(1));
    assert_eq!(second.native_operation()?, U64(2));
    assert_eq!(calls.native_operation()?, U64(3));
    Ok(())
}

#[test]
fn foreign_lane_cannot_abort_release_or_reconcile_an_uncertain_transfer() -> Result<()> {
    let (calls, script, stage) = fixture([
        Response::Lost,
        Response::Terminal,
        Response::Unit,
        Response::Unit,
    ]);
    let first = calls.lane()?;
    let second = calls.lane()?;
    assert!(
        first
            .call(
                Action::Read {
                    stage: stage.clone(),
                    offset: U64(9)
                },
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert!(second.abort_read(&stage).is_err());
    assert!(
        second
            .unit(Action::Release {
                stage: stage.clone()
            })
            .is_err()
    );
    assert!(second.reconcile_cleanup().is_err());
    assert_eq!(script.requests.lock().unwrap().len(), 1);
    assert_eq!(calls.state.lock().unwrap().pending_lane, first.lane);
    first.abort_read(&stage)?;
    first.unit(Action::Release {
        stage: stage.clone(),
    })?;
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(digest(&requests[0])?, digest(&requests[1])?);
    assert!(matches!(&requests[2].action, Action::AbortRead { stage: value } if *value == stage));
    assert!(matches!(&requests[3].action, Action::Release { stage: value } if *value == stage));
    assert!(requests[2].operation.0 > requests[1].operation.0);
    assert!(requests[3].operation.0 > requests[2].operation.0);
    assert!(calls.state.lock().unwrap().pending.is_none());
    Ok(())
}
fn fixture(responses: impl IntoIterator<Item = Response>) -> (Arc<Calls>, Arc<Script>, LeaseId) {
    #[cfg(unix)]
    let physical = PhysicalObjectId::Unix {
        device: U64(1),
        inode: U64(2),
    };
    #[cfg(windows)]
    let physical = PhysicalObjectId::Windows {
        volume_serial: U64(1),
        file_index: U64(2),
    };
    #[cfg(unix)]
    let path = std::path::Path::new("/synthetic-stage-adapter");
    #[cfg(windows)]
    let path = std::path::Path::new(r"C:\synthetic-stage-adapter");
    let root = RootCapability {
        epoch: LeaseId::new(),
        token: LeaseId::new(),
        session: LeaseId::new(),
        canonical_root: crate::storage_volume::NativePath::from_path(path),
        root_physical: physical,
        catalog_physical: physical,
    };
    let script = Arc::new(Script {
        responses: Mutex::new(responses.into_iter().collect()),
        requests: Mutex::new(Vec::new()),
    });
    (
        Calls::new(script.clone(), root, false),
        script,
        LeaseId::new(),
    )
}

#[test]
fn lost_read_is_reconciled_exactly_before_abort_and_next_transfer() -> Result<()> {
    let bytes = vec![9; crate::catalog_session::preview_io::CHUNK_BYTES + 7];
    let (calls, script, stage) = fixture([
        Response::Unit,
        Response::Chunk(bytes[..bytes.len() - 7].to_vec()),
        Response::Lost,
        Response::Chunk(bytes[bytes.len() - 7..].to_vec()),
        Response::Unit,
        Response::Unit,
        Response::Unit,
        Response::Chunk(b"next".to_vec()),
        Response::Unit,
    ]);
    let mut out = vec![0; bytes.len()];
    assert!(
        calls
            .read_into(
                &stage,
                Artifact::Rgb(0),
                bytes.len() as u64,
                blake3::hash(&bytes).to_hex().as_str(),
                &mut out,
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert_eq!(&out[..out.len() - 7], &bytes[..bytes.len() - 7]);
    assert_eq!(&out[out.len() - 7..], &[0; 7]);
    assert!(calls.state.lock().unwrap().pending.is_none());
    calls.unit(Action::Release {
        stage: stage.clone(),
    })?;
    let mut next = [0; 4];
    let next_stage = LeaseId::new();
    calls.read_into(
        &next_stage,
        Artifact::Rgb(0),
        4,
        blake3::hash(b"next").to_hex().as_str(),
        &mut next,
        &AtomicBool::new(false),
    )?;
    assert_eq!(&next, b"next");
    let requests = script.requests.lock().unwrap();
    assert_eq!(digest(&requests[2])?, digest(&requests[3])?);
    assert_eq!(requests[2].operation, requests[3].operation);
    assert!(matches!(requests[4].action, Action::AbortRead { .. }));
    assert!(requests[4].operation.0 > requests[3].operation.0);
    assert!(matches!(requests[8].action, Action::FinishRead { .. }));
    assert!(script.responses.lock().unwrap().is_empty());
    Ok(())
}

#[test]
fn unresolved_read_fences_other_actions_and_terminal_reconciliation_allows_cleanup() -> Result<()> {
    let (calls, script, stage) = fixture([
        Response::Lost,
        Response::ForgedTerminal,
        Response::Terminal,
        Response::Unit,
    ]);
    let read = Action::Read {
        stage: stage.clone(),
        offset: U64(13),
    };
    assert!(calls.call(read, &AtomicBool::new(false)).is_err());
    assert!(
        calls
            .unit(Action::Release {
                stage: stage.clone()
            })
            .is_err()
    );
    assert_eq!(script.requests.lock().unwrap().len(), 1);
    assert!(calls.abort_read(&stage).is_err());
    assert!(calls.state.lock().unwrap().pending.is_some());
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    calls.abort_read(&stage)?;
    assert!(calls.state.lock().unwrap().pending.is_none());
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in &requests[..3] {
        assert_eq!(digest(request)?, digest(&requests[0])?);
    }
    assert!(matches!(requests[3].action, Action::AbortRead { .. }));
    assert!(requests[3].operation.0 > requests[2].operation.0);
    Ok(())
}

#[test]
fn wrong_reply_identity_keeps_exact_operation_owned() -> Result<()> {
    let (calls, script, stage) = fixture([Response::WrongSession, Response::Unit, Response::Unit]);
    assert!(
        calls
            .unit(Action::AbortRead {
                stage: stage.clone()
            })
            .is_err()
    );
    assert!(calls.state.lock().unwrap().pending.is_some());
    assert!(
        calls
            .unit(Action::Release {
                stage: stage.clone()
            })
            .is_err()
    );
    assert_eq!(script.requests.lock().unwrap().len(), 1);
    calls.reconcile_cleanup()?;
    calls.unit(Action::Release { stage })?;
    let requests = script.requests.lock().unwrap();
    assert_eq!(digest(&requests[0])?, digest(&requests[1])?);
    assert_eq!(requests[0].operation, requests[1].operation);
    assert!(requests[2].operation.0 > requests[1].operation.0);
    assert!(calls.state.lock().unwrap().pending.is_none());
    Ok(())
}

#[test]
fn known_preeffect_cancel_does_not_leave_an_uncertain_request() -> Result<()> {
    let (calls, script, stage) = fixture([Response::Canceled, Response::Unit]);
    assert!(
        calls
            .call(
                Action::Read {
                    stage: stage.clone(),
                    offset: U64(0)
                },
                &AtomicBool::new(true)
            )
            .is_err()
    );
    assert!(calls.state.lock().unwrap().pending.is_none());
    calls.abort_read(&stage)?;
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(matches!(requests[1].action, Action::AbortRead { .. }));
    assert!(requests[1].operation.0 > requests[0].operation.0);
    Ok(())
}

#[test]
fn held_first_and_middle_stage_chunks_keep_poll_cancel_and_lane_admission_responsive() -> Result<()>
{
    use crate::preview::{ByteBudget, transport_task::Task};
    use std::sync::mpsc;
    for held in [0, 1] {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let chunk = crate::catalog_session::preview_io::CHUNK_BYTES;
        let mut responses = vec![Response::Unit];
        if held == 1 {
            responses.push(Response::Chunk(vec![7; chunk]));
        }
        responses.push(Response::HeldChunk {
            bytes: vec![7; chunk],
            entered: entered_tx,
            release: release_rx,
        });
        responses.push(Response::Unit); // exact AbortRead after known cancellation
        let (calls, script, stage) = fixture(responses);
        let lane = calls.task_lane()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let budget = ByteBudget::new((2 * chunk) as u64)?;
        let reservation = budget.try_reserve((2 * chunk) as u64).unwrap();
        let digest = blake3::hash(&vec![7; 2 * chunk]).to_hex().to_string();
        let mut task = Task::spawn("held-stage-read", cancel.clone(), move |cancel| {
            let mut bytes = vec![0; 2 * chunk];
            Ok(lane.read_into(
                &stage,
                Artifact::Rgb(0),
                (2 * chunk) as u64,
                &digest,
                &mut bytes,
                &cancel,
            ))
        })?;
        let reached = entered_rx.recv_timeout(std::time::Duration::from_secs(5));
        // Always release and join before assertions that could fail the fixture.
        let start = std::time::Instant::now();
        let pending = task.poll();
        let extra_lane = calls.lane();
        task.signal_cancel();
        let elapsed = start.elapsed();
        let held_bytes = budget.used();
        let _ = release_tx.send(());
        let result = task.shutdown()?;
        assert!(reached.is_ok());
        assert!(matches!(pending, Ok(None)));
        assert!(extra_lane.is_ok());
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "actor operations blocked: {elapsed:?}"
        );
        assert_eq!(held_bytes, (2 * chunk) as u64);
        assert!(matches!(result, Some(Err(_))));
        assert!(matches!(
            script.requests.lock().unwrap().last().unwrap().action,
            Action::AbortRead { .. }
        ));
        drop(reservation);
        assert_eq!(budget.used(), 0);
    }
    Ok(())
}

#[test]
fn closing_foreign_lane_returns_busy_before_origin_replay_and_retains_cleanup_ownership()
-> Result<()> {
    use crate::preview::ByteBudget;
    use std::sync::mpsc;
    use std::time::Duration;
    // Exercise both ordinary call retries and the reconciliation used by Abort.
    for reconcile_first in [false, true] {
        let owned_stage = LeaseId::new();
        let (calls, script, origin_stage) = fixture([
            Response::Admitted(owned_stage.clone()),
            Response::Lost,
            Response::Unit, // A exact reply reconciliation
            Response::Unit, // B Abort after A reconciliation
            Response::Unit, // B Release after A reconciliation
        ]);
        let origin = calls.task_lane()?;
        let waiting = calls.task_lane()?;
        let Value::Admitted {
            stage, ready: true, ..
        } = waiting.call(admission(), &AtomicBool::new(false))?
        else {
            anyhow::bail!("waiting lane stage not admitted")
        };
        assert_eq!(stage, owned_stage);
        let budget = ByteBudget::new(1024)?;
        let guard = budget.try_reserve(1024).unwrap();
        assert!(
            origin
                .unit(Action::AbortRead {
                    stage: origin_stage
                })
                .is_err()
        );
        let original = digest(calls.state.lock().unwrap().pending.as_ref().unwrap())?;
        let lane = waiting.clone();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let attempt = std::thread::spawn(move || {
            let _ = entered_tx.send(());
            let result = if reconcile_first {
                lane.abort_read(&stage)
            } else {
                // Cleanup uses a false per-call cancel: lane shutdown must work
                // independently of the data transfer's cancel argument.
                lane.unit(Action::Release {
                    stage: stage.clone(),
                })
            };
            let _ = done_tx.send(());
            (stage, guard, result)
        });
        let entered = entered_rx.recv_timeout(Duration::from_secs(2));
        let open_wait = done_rx.recv_timeout(Duration::from_millis(30));
        waiting.signal_native_stop();
        let closed_result = done_rx.recv_timeout(Duration::from_secs(2));
        let held_bytes = budget.used();
        let pending_before_origin_replay = {
            let state = calls.state.lock().unwrap();
            (
                state.pending_lane,
                state.pending.as_ref().map(digest).transpose()?,
            )
        };
        let dispatch_count = script.requests.lock().unwrap().len();
        // Resolve A only after observing B's closing result. Always do so
        // before joining, allowing this fixture to terminate on the old cycle.
        let reconciled = origin.reconcile_cleanup();
        let (stage, guard, result) = attempt.join().expect("closing stage lane panicked");
        entered?;
        assert!(matches!(open_wait, Err(mpsc::RecvTimeoutError::Timeout)));
        closed_result?;
        assert!(result.unwrap_err().downcast_ref::<Busy>().is_some());
        assert_eq!(pending_before_origin_replay, (origin.lane, Some(original)));
        assert_eq!(dispatch_count, 2);
        assert_eq!(held_bytes, 1024);
        assert_eq!(budget.used(), 1024);
        assert_eq!(stage, owned_stage);
        reconciled?;
        waiting.abort_read(&stage)?;
        waiting.unit(Action::Release {
            stage: stage.clone(),
        })?;
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert_eq!(digest(&requests[1])?, digest(&requests[2])?);
        assert!(matches!(&requests[3].action, Action::AbortRead { stage: s } if s == &owned_stage));
        assert!(matches!(&requests[4].action, Action::Release { stage: s } if s == &owned_stage));
        assert!(calls.state.lock().unwrap().pending.is_none());
        assert!(script.responses.lock().unwrap().is_empty());
        drop(guard);
        assert_eq!(budget.used(), 0);
    }
    Ok(())
}

#[test]
fn lost_legacy_begin_stays_with_its_lane_until_exact_replay_and_abort() -> Result<()> {
    let transfer = LeaseId::new();
    let next_transfer = LeaseId::new();
    let bytes = b"legacy";
    let hash = blake3::hash(bytes).to_hex().to_string();
    let (calls, script, _) = fixture([
        Response::Lost,
        Response::Lost,
        Response::LegacyRead(transfer.clone(), bytes.len() as u64),
        Response::Unit,
        Response::LegacyRead(next_transfer.clone(), bytes.len() as u64),
        Response::Chunk(bytes.to_vec()),
        Response::Unit,
    ]);
    let first = calls.lane()?;
    let second = calls.lane()?;
    let cancel = AtomicBool::new(false);
    assert!(first.legacy_read(&hash, LEGACY_BYTES, &cancel).is_err());
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    let pending = digest(calls.state.lock().unwrap().pending.as_ref().unwrap())?;
    assert!(second.legacy_read(&hash, LEGACY_BYTES, &cancel).is_err());
    assert!(second.cleanup_unadmitted().is_err());
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    assert_eq!(calls.state.lock().unwrap().pending_lane, first.lane);
    assert_eq!(
        digest(calls.state.lock().unwrap().pending.as_ref().unwrap())?,
        pending
    );
    first.cleanup_unadmitted()?;
    assert!(first.legacy_transfer.lock().unwrap().is_none());
    assert!(calls.state.lock().unwrap().pending.is_none());
    assert_eq!(
        second.legacy_read(&hash, LEGACY_BYTES, &cancel)?,
        Some(bytes.to_vec())
    );
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 7);
    for request in &requests[..3] {
        assert_eq!(digest(request)?, pending);
    }
    assert!(matches!(&requests[3].action, Action::AbortRead { stage } if *stage == transfer));
    assert!(matches!(&requests[6].action, Action::FinishRead { stage } if *stage == next_transfer));
    assert!(
        requests
            .iter()
            .all(|r| !matches!(r.action, Action::Release { .. }))
    );
    assert!(script.responses.lock().unwrap().is_empty());
    Ok(())
}

#[test]
fn interrupted_legacy_middle_read_retains_lease_until_replay_and_abort_ack() -> Result<()> {
    let chunk = crate::catalog_session::preview_io::CHUNK_BYTES;
    let bytes = vec![6; chunk + 3];
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let transfer = LeaseId::new();
    let (calls, script, _) = fixture([
        Response::LegacyRead(transfer.clone(), bytes.len() as u64),
        Response::Chunk(bytes[..chunk].to_vec()),
        Response::Lost,
        Response::Lost,
        Response::Chunk(bytes[chunk..].to_vec()),
        Response::Unit,
    ]);
    let lane = calls.lane()?;
    assert!(
        lane.legacy_read(&hash, LEGACY_BYTES, &AtomicBool::new(false))
            .is_err()
    );
    assert_eq!(
        lane.legacy_transfer.lock().unwrap().as_ref(),
        Some(&transfer)
    );
    assert_eq!(calls.state.lock().unwrap().pending_lane, lane.lane);
    lane.cleanup_unadmitted()?;
    assert!(lane.legacy_transfer.lock().unwrap().is_none());
    assert!(calls.state.lock().unwrap().pending.is_none());
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    for request in &requests[2..5] {
        assert_eq!(digest(request)?, digest(&requests[2])?);
        assert!(
            matches!(&request.action, Action::Read { stage, offset } if *stage == transfer && *offset == U64(chunk as u64))
        );
    }
    assert!(matches!(&requests[5].action, Action::AbortRead { stage } if *stage == transfer));
    assert!(
        requests
            .iter()
            .all(|r| !matches!(r.action, Action::Release { .. } | Action::FinishRead { .. }))
    );
    assert!(script.responses.lock().unwrap().is_empty());
    Ok(())
}

#[test]
fn held_legacy_first_and_middle_chunks_cancel_without_releasing_unread_task_charge() -> Result<()> {
    use crate::preview::{ByteBudget, transport_task::Task};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    for held in [0, 1] {
        let chunk = crate::catalog_session::preview_io::CHUNK_BYTES;
        let bytes = vec![7; 2 * chunk];
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let transfer = LeaseId::new();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut responses = vec![Response::LegacyRead(transfer.clone(), bytes.len() as u64)];
        if held == 1 {
            responses.push(Response::Chunk(bytes[..chunk].to_vec()));
        }
        responses.push(Response::HeldChunk {
            bytes: bytes[..chunk].to_vec(),
            entered: entered_tx,
            release: release_rx,
        });
        responses.push(Response::Unit);
        let (calls, script, _) = fixture(responses);
        let lane = calls.task_lane()?;
        let worker_lane = lane.clone();
        let budget = ByteBudget::new(bytes.len() as u64)?;
        let reservation = budget.try_reserve(bytes.len() as u64).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut task = Task::spawn("held-legacy-read", cancel, move |cancel| {
            let result = worker_lane.legacy_read(&hash, LEGACY_BYTES, &cancel);
            Ok((result, reservation))
        })?;
        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        let start = Instant::now();
        let pending = task.poll();
        task.signal_cancel();
        let elapsed = start.elapsed();
        let held_bytes = budget.used();
        let released = release_tx.send(());
        let drained = task.shutdown();
        // Do not assert until the held callback is released and the task joined.
        entered?;
        released?;
        assert!(pending?.is_none());
        assert!(elapsed < Duration::from_secs(1));
        assert_eq!(held_bytes, bytes.len() as u64);
        let (result, charge) = drained?.context("joined legacy result missing")?;
        assert!(result.is_err());
        assert_eq!(budget.used(), bytes.len() as u64);
        assert!(lane.legacy_transfer.lock().unwrap().is_none());
        assert!(calls.state.lock().unwrap().pending.is_none());
        drop(charge);
        assert_eq!(budget.used(), 0);
        let requests = script.requests.lock().unwrap();
        assert!(
            matches!(&requests.last().unwrap().action, Action::AbortRead { stage } if *stage == transfer)
        );
        assert!(
            requests
                .iter()
                .all(|r| !matches!(r.action, Action::Release { .. } | Action::FinishRead { .. }))
        );
        assert!(script.responses.lock().unwrap().is_empty());
    }
    Ok(())
}
