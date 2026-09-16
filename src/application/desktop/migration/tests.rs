use super::*;
use crate::{application::Request, storage_volume::NativePath};
#[cfg(unix)]
use crate::{
    application::{Config, Limits, Reply, Response},
    lightroom_migration_worker::input,
};

fn header(
    destination: &std::path::Path,
    operation: api::Operation,
    documents: &[(api::InputRole, String)],
) -> api::Header {
    api::Header {
        catalog: None,
        destination: NativePath::from_path(destination),
        operation,
        parts: documents
            .iter()
            .map(|(role, text)| api::PartDescriptor {
                role: *role,
                bytes: U64(text.len() as u64),
                blake3: blake3::hash(text.as_bytes()).to_hex().to_string(),
            })
            .collect(),
        timeout_ms: U64(180_000),
    }
}
fn snap(response: api::Response) -> Result<api::Snapshot> {
    match response {
        api::Response::Status(snapshot) => Ok(snapshot),
        _ => anyhow::bail!("expected migration status"),
    }
}
fn pure(pool: &ByteBudget) -> Result<(Arc<Shared>, Coordinator)> {
    let shared = super::super::tests::shared(1024 * 1024);
    let coordinator = Coordinator::new(&shared, std::env::current_exe()?, Some(pool.clone()));
    Ok((shared, coordinator))
}
fn upload(
    coordinator: &Coordinator,
    guard: &Guard,
    documents: &[(api::InputRole, String)],
) -> Result<()> {
    for (role, text) in documents {
        let mut offset = 0;
        while offset < text.len() {
            let mut end = (offset + TEXT_CHUNK).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            coordinator.request(api::Request::Upload {
                guard: guard.clone(),
                role: *role,
                offset: U64(offset as u64),
                text: text[offset..end].into(),
            })?;
            offset = end;
        }
        coordinator.request(api::Request::Finish {
            guard: guard.clone(),
            role: *role,
            blake3: blake3::hash(text.as_bytes()).to_hex().to_string(),
        })?;
    }
    Ok(())
}
#[test]
fn lm_facade_exact_independent_raw_uploads_preserve_bytes_and_refuse_reorder() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let pool = ByteBudget::new(4 * 1024 * 1024 * 1024)?;
    let (shared, coordinator) = pure(&pool)?;
    let baseline = pool.used();
    let mut documents = vec![];
    for role in [
        api::InputRole::Seal,
        api::InputRole::Approval,
        api::InputRole::Policy,
        api::InputRole::ExecutionAuthorization,
    ] {
        let maximum = role.maximum_bytes();
        let text = format!("{}{{\"value\":\"\\u0061\"}}", " ".repeat(maximum - 18));
        assert_eq!(text.len(), maximum);
        documents.push((role, text));
    }
    let command = api::Operation::Run {
        approval_blake3: "a".repeat(64),
        max_steps: U64(1),
        max_seconds: U64(1),
        source_open_ms: U64(1),
        artifact_open_ms: U64(1),
        max_artifact_bytes: U64(1),
    };
    let header = header(
        &temp.path().canonicalize()?.join("absent"),
        command,
        &documents,
    );
    let guard = snap(coordinator.request(api::Request::Begin {
        operation: "exact-raw".into(),
        header: header.clone(),
    })?)?
    .guard;
    assert!(shared.state.lock().unwrap().pending.is_empty());
    assert!(
        coordinator
            .request(api::Request::Upload {
                guard: guard.clone(),
                role: api::InputRole::Approval,
                offset: U64(0),
                text: "x".into()
            })
            .is_err()
    );
    assert!(
        coordinator
            .request(api::Request::Act {
                guard: guard.clone()
            })
            .is_err()
    );
    upload(&coordinator, &guard, &documents)?;
    let status = snap(coordinator.request(api::Request::Status {
        guard: guard.clone(),
    })?)?;
    assert_eq!(status.phase, api::Phase::Ready);
    assert_eq!(status.uploaded, U64(56 * 1024 * 1024));
    let slot = coordinator.slot.lock().unwrap();
    let EntryState::Upload(upload) = &slot.as_ref().unwrap().state else {
        unreachable!()
    };
    for ((_, raw), actual) in documents.iter().zip(&upload.texts) {
        assert_eq!(raw.as_bytes(), actual.as_bytes());
    }
    drop(slot);
    coordinator.request(api::Request::Discard { guard })?;
    assert_eq!(pool.used(), baseline);
    let mut oversized = header;
    oversized.parts[0].bytes.0 += 1;
    assert!(
        coordinator
            .request(api::Request::Begin {
                operation: "oversize".into(),
                header: oversized
            })
            .is_err()
    );
    assert_eq!(pool.used(), baseline);
    assert!(shared.state.lock().unwrap().pending.is_empty());
    Ok(())
}
#[test]
fn lm_facade_same_pool_required_minus_one_retries_without_authority_or_storage_leak() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let documents = vec![(api::InputRole::SupplementRequests, "[]".into())];
    let h = header(
        &temp.path().canonicalize()?.join("target"),
        api::Operation::PrepareSupplements,
        &documents,
    );
    let pool = ByteBudget::new(2 * 1024 * 1024 * 1024)?;
    let (_shared, coordinator) = pure(&pool)?;
    let baseline = pool.used();
    let guard = snap(coordinator.request(api::Request::Begin {
        operation: "measure".into(),
        header: h.clone(),
    })?)?
    .guard;
    let required = pool.used();
    coordinator.request(api::Request::Discard { guard })?;
    assert_eq!(pool.used(), baseline);
    let exact = ByteBudget::new(required)?;
    let (shared, coordinator) = pure(&exact)?;
    let exact_baseline = exact.used();
    let competitor = exact.reserve_exact(1)?;
    let error = coordinator
        .request(api::Request::Begin {
            operation: "measure".into(),
            header: h.clone(),
        })
        .unwrap_err();
    assert!(matches!(error.code, ErrorCode::ResourceLimit));
    assert_eq!(exact.used(), exact_baseline + 1);
    assert!(coordinator.slot.lock().unwrap().is_none());
    assert!(shared.state.lock().unwrap().pending.is_empty());
    drop(competitor);
    let guard = snap(coordinator.request(api::Request::Begin {
        operation: "measure".into(),
        header: h,
    })?)?
    .guard;
    assert_eq!(exact.used(), required);
    coordinator.request(api::Request::Cancel { guard })?;
    assert_eq!(exact.used(), exact_baseline);
    Ok(())
}
#[test]
fn lm_facade_public_json_seven_headers_guards_and_existing_commands_are_strict() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let cases = [
        (
            api::Operation::Run {
                approval_blake3: "a".repeat(64),
                max_steps: U64(1),
                max_seconds: U64(1),
                source_open_ms: U64(1),
                artifact_open_ms: U64(1),
                max_artifact_bytes: U64(1),
            },
            vec![
                api::InputRole::Seal,
                api::InputRole::Approval,
                api::InputRole::Policy,
            ],
        ),
        (
            api::Operation::Status {
                run: "a".repeat(64),
            },
            vec![],
        ),
        (
            api::Operation::PrepareSupplements,
            vec![api::InputRole::SupplementRequests],
        ),
        (
            api::Operation::RepairCurrent {
                max_steps: U64(1),
                max_seconds: U64(1),
                source_open_ms: U64(1),
            },
            vec![
                api::InputRole::Seal,
                api::InputRole::Approval,
                api::InputRole::RepairRequest,
            ],
        ),
        (
            api::Operation::RepairStatus {
                repair: "a".repeat(64),
            },
            vec![],
        ),
        (
            api::Operation::RepairKeywords {
                max_steps: U64(1),
                max_seconds: U64(1),
                source_open_ms: U64(1),
            },
            vec![
                api::InputRole::Seal,
                api::InputRole::Approval,
                api::InputRole::RepairRequest,
            ],
        ),
        (
            api::Operation::KeywordRepairStatus {
                repair: "a".repeat(64),
            },
            vec![],
        ),
    ];
    let pool = ByteBudget::new(2 * 1024 * 1024 * 1024)?;
    let (shared, coordinator) = pure(&pool)?;
    let baseline = pool.used();
    for (index, (operation, roles)) in cases.into_iter().enumerate() {
        let docs = roles
            .into_iter()
            .map(|r| (r, "{}".into()))
            .collect::<Vec<_>>();
        let request = Request::LightroomMigration {
            request: Box::new(api::Request::Begin {
                operation: format!("operation-{index}"),
                header: header(
                    &temp.path().canonicalize()?.join("target"),
                    operation,
                    &docs,
                ),
            }),
        };
        let json = serde_json::to_string(&request)?;
        let Request::LightroomMigration { request } = serde_json::from_str(&json)? else {
            unreachable!()
        };
        let guard = snap(coordinator.request(*request)?)?.guard;
        let mut stale = guard.clone();
        stale.generation = "f".repeat(64);
        assert!(matches!(
            coordinator
                .request(api::Request::Cancel { guard: stale })
                .unwrap_err()
                .code,
            ErrorCode::StaleSession
        ));
        coordinator.request(api::Request::Discard { guard })?;
    }
    assert_eq!(
        serde_json::to_string(&Request::Status)?,
        "{\"command\":\"status\"}"
    );
    assert!(serde_json::from_str::<Request>(r#"{"command":"lightroom_migration","args":{"request":{"action":"begin","operation":"x","header":{},"extra":true}}}"#).is_err());
    assert!(shared.state.lock().unwrap().pending.is_empty());
    assert_eq!(pool.used(), baseline);
    Ok(())
}

#[test]
fn lm_facade_begin_replay_and_small_reply_refusal_keep_guard_and_admission_exact() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let pool = ByteBudget::new(2 * 1024 * 1024 * 1024)?;
    let (shared, coordinator) = pure(&pool)?;
    let documents = vec![(api::InputRole::SupplementRequests, "[]".into())];
    let h = header(
        &temp.path().canonicalize()?.join("target"),
        api::Operation::PrepareSupplements,
        &documents,
    );
    let first = snap(coordinator.request(api::Request::Begin {
        operation: "replay".into(),
        header: h.clone(),
    })?)?;
    let used = pool.used();
    let repeated = snap(coordinator.request(api::Request::Begin {
        operation: "replay".into(),
        header: h.clone(),
    })?)?;
    assert_eq!(first.guard, repeated.guard);
    assert_eq!(pool.used(), used);
    assert!(shared.state.lock().unwrap().pending.is_empty());
    coordinator.request(api::Request::Discard { guard: first.guard })?;
    drop(coordinator);
    assert_eq!(pool.used(), 0);
    let mut shared = super::super::tests::shared(1024 * 1024);
    Arc::get_mut(&mut shared).unwrap().limits.reply_bytes = 1;
    let coordinator = Coordinator::new(&shared, std::env::current_exe()?, Some(pool.clone()));
    let baseline = pool.used();
    assert!(matches!(
        coordinator
            .request(api::Request::Begin {
                operation: "no-reply-space".into(),
                header: h
            })
            .unwrap_err()
            .code,
        ErrorCode::ResourceLimit
    ));
    assert!(coordinator.slot.lock().unwrap().is_none());
    assert!(shared.state.lock().unwrap().pending.is_empty());
    assert_eq!(pool.used(), baseline);
    Ok(())
}

#[test]
fn lm_facade_bookkeeping_refusal_precedes_owned_graph_and_retries_same_pool() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let pool = ByteBudget::new(1024 * 1024 * 1024)?;
    let (shared, coordinator) = pure(&pool)?;
    let destination = temp.path().canonicalize()?.join("target");
    let documents = vec![(api::InputRole::SupplementRequests, "[]".into())];
    let name = "bookkeeping-order";
    let bytes = coordinator.bookkeeping_bytes(name, shared.limits.reply_bytes)?;
    let (limit, used) = pool.snapshot();
    let competitor = pool.reserve_exact(limit - used - (bytes - 1))?;
    let held = pool.used();
    let begin = || api::Request::Begin {
        operation: name.into(),
        header: header(&destination, api::Operation::PrepareSupplements, &documents),
    };
    let failure = coordinator.request(begin()).unwrap_err();
    assert!(matches!(failure.code, ErrorCode::ResourceLimit));
    assert_eq!(coordinator.faults.lock().unwrap().graphs_built, 0);
    assert!(coordinator.slot.lock().unwrap().is_none());
    assert_eq!(pool.used(), held);
    assert!(shared.state.lock().unwrap().pending.is_empty());
    drop(competitor);
    let guard = snap(coordinator.request(begin())?)?.guard;
    assert_eq!(coordinator.faults.lock().unwrap().graphs_built, 1);
    coordinator.request(api::Request::Discard { guard })?;
    assert_eq!(pool.used(), used);
    Ok(())
}

fn acquisition_proxy(
    writer: bool,
) -> (
    Arc<Shared>,
    Arc<Proxy>,
    Arc<Mutex<Acquisition>>,
    relay::Snapshot,
) {
    let shared = super::super::tests::shared(1024 * 1024);
    let guard = Guard {
        session: "ack-fixture".into(),
        generation: "a".repeat(64),
        operation: "acquisition".into(),
    };
    let action = if writer {
        relay::Action::AcquireWrite {
            sequence: U64(1),
            kind: WriteKind::Bootstrap,
            target: guard.generation.clone(),
            lock: None,
            request_digest: relay::write_digest(
                &guard,
                U64(1),
                WriteKind::Bootstrap,
                &guard.generation,
                None,
            )
            .unwrap(),
        }
    } else {
        relay::Action::AcquireTarget {
            catalog: None,
            destination: NativePath::from_path(std::path::Path::new("/unopened-target")),
            expected: None,
        }
    };
    let request = relay::Request {
        guard: guard.clone(),
        action,
    };
    let digest = blake3::hash(&serde_json::to_vec(&request).unwrap());
    let snapshot = relay::Snapshot {
        guard: guard.clone(),
        phase: if writer {
            relay::Phase::Held
        } else {
            relay::Phase::Target
        },
        catalog: None,
        destination: None,
        sequence: writer.then_some(U64(1)),
        request_digest: match &request.action {
            relay::Action::AcquireWrite { request_digest, .. } => Some(request_digest.clone()),
            _ => None,
        },
        write_kind: writer.then_some(WriteKind::Bootstrap),
        cancel_requested: false,
        progress: None,
        failure: None,
    };
    let acquisition = Arc::new(Mutex::new(Acquisition {
        state: Submission::Submitted(digest),
        request: Some(request),
    }));
    let proxy = Arc::new(Proxy {
        shared: Arc::downgrade(&shared),
        client: relay::Client::new(&shared),
        guard,
        job: Arc::new(Job {
            stop: Arc::new(Stop::default()),
            report: Mutex::new(Report {
                phase: api::Phase::Running,
                catalog: None,
                progress: None,
                failure: None,
                retry: 0,
            }),
            wake: Condvar::new(),
            faults: Default::default(),
        }),
        authority: Mutex::new(None),
        recovery: Mutex::new(None),
        pin: Mutex::new(None),
    });
    (shared, proxy, acquisition, snapshot)
}
fn queued_recovery_reply(shared: Arc<Shared>, reply: relay::Reply) -> JoinHandle<relay::Request> {
    thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let mut state = shared.state.lock().unwrap();
            if let Some(message) = state.control.pop_front() {
                let request = serde_json::from_slice::<relay::Request>(&message.bytes).unwrap();
                let entry = state.pending.remove(&message.id).unwrap();
                let super::super::Delivery::Migration(tx) = entry.delivery else {
                    panic!()
                };
                tx.send(reply).unwrap();
                return request;
            }
            drop(state);
            assert!(Instant::now() < until, "exact recovery never submitted");
            thread::sleep(Duration::from_millis(1));
        }
    })
}
#[test]
fn lm_facade_acquisition_refusal_provenance_never_confuses_post_action_errors() -> Result<()> {
    for writer in [false, true] {
        let (_shared, proxy, acquisition, snapshot) = acquisition_proxy(writer);
        let Submission::Submitted(digest) = acquisition.lock().unwrap().state else {
            unreachable!()
        };
        let overflow: relay::Reply =
            serde_json::from_slice(&relay::Reply::Ok(snapshot).message(1, 1).bytes)?;
        assert!(matches!(overflow, relay::Reply::Error(_)));
        for unknown in [
            overflow,
            relay::Reply::Error(application::error(ErrorCode::Closed, "actor disconnected")),
        ] {
            proxy.record_reply(Some(&acquisition), digest, true, &unknown)?;
            assert_eq!(
                acquisition.lock().unwrap().state,
                Submission::Submitted(digest)
            );
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let request = acquisition.lock().unwrap().request.clone().unwrap();
        // The actual application queue's pre-dispatch rejection producer.
        application::Envelope {
            work: application::Work::MigrationAdmission(request, tx),
            cancel: Default::default(),
            created: Instant::now(),
        }
        .reject(ErrorCode::Canceled, "before actor dispatch");
        let refusal = rx.recv()?;
        assert!(matches!(refusal, relay::Reply::Refused(_)));
        proxy.record_reply(Some(&acquisition), digest, false, &refusal)?;
        assert_eq!(
            acquisition.lock().unwrap().state,
            Submission::Submitted(digest),
            "refused replay retains original uncertainty"
        );
        let (_other_shared, other_proxy, original, _) = acquisition_proxy(writer);
        other_proxy.record_reply(Some(&original), digest, true, &refusal)?;
        assert!(other_proxy.absent(&original, Instant::now() + Duration::from_secs(1))?);
    }
    Ok(())
}
#[test]
fn lm_facade_acquisition_original_ack_survives_cancel_without_replay() -> Result<()> {
    for writer in [false, true] {
        let (shared, proxy, acquisition, snapshot) = acquisition_proxy(writer);
        let Submission::Submitted(digest) = acquisition.lock().unwrap().state else {
            unreachable!()
        };
        let (tx, receiver) = mpsc::sync_channel(1);
        tx.send(relay::Reply::Ok(snapshot))?;
        *proxy.authority.lock().unwrap() = Some(Waiting {
            digest,
            pending: relay::Pending {
                receiver,
                cancel: Default::default(),
            },
            submission: Some(acquisition.clone()),
            first_submission: true,
        });
        proxy.job.stop.cancel();
        assert!(!proxy.absent(&acquisition, Instant::now() + Duration::from_secs(1))?);
        assert_eq!(
            acquisition.lock().unwrap().state,
            Submission::Acknowledged(digest)
        );
        assert!(
            shared.state.lock().unwrap().pending.is_empty(),
            "original terminal ACK consumed without replay"
        );
    }
    Ok(())
}
#[test]
fn lm_facade_acquisition_lost_and_disconnected_slots_recover_exactly_while_stopping() -> Result<()>
{
    for writer in [false, true] {
        for disconnected in [false, true] {
            let (shared, proxy, acquisition, snapshot) = acquisition_proxy(writer);
            let Submission::Submitted(digest) = acquisition.lock().unwrap().state else {
                unreachable!()
            };
            let expected = serde_json::to_vec(&acquisition.lock().unwrap().recovery()?)?;
            if disconnected {
                let (tx, receiver) = mpsc::sync_channel(1);
                drop(tx);
                *proxy.authority.lock().unwrap() = Some(Waiting {
                    digest,
                    pending: relay::Pending {
                        receiver,
                        cancel: Default::default(),
                    },
                    submission: Some(acquisition.clone()),
                    first_submission: true,
                });
            }
            proxy.job.stop.cancel();
            shared.stop();
            if disconnected {
                assert!(
                    proxy
                        .absent(&acquisition, Instant::now() + Duration::from_secs(1))
                        .is_err()
                );
                assert!(proxy.authority.lock().unwrap().is_none());
            }
            let responder = queued_recovery_reply(
                shared.clone(),
                relay::Reply::Refused(application::error(ErrorCode::Canceled, "lookup refused")),
            );
            assert!(
                proxy
                    .absent(&acquisition, Instant::now() + Duration::from_secs(1))
                    .is_err()
            );
            assert_eq!(serde_json::to_vec(&responder.join().unwrap())?, expected);
            assert_eq!(
                acquisition.lock().unwrap().state,
                Submission::Submitted(digest)
            );
            let responder = queued_recovery_reply(shared.clone(), relay::Reply::Ok(snapshot));
            assert!(!proxy.absent(&acquisition, Instant::now() + Duration::from_secs(1))?);
            let request = responder.join().unwrap();
            assert!(request.action.recovery());
            assert_eq!(serde_json::to_vec(&request)?, expected);
            assert_eq!(
                acquisition.lock().unwrap().state,
                Submission::Acknowledged(digest)
            );
            assert!(shared.state.lock().unwrap().pending.is_empty());
        }
    }
    Ok(())
}

#[test]
fn lm_facade_release_pending_and_transport_failure_require_exact_retirement_ack() -> Result<()> {
    let (shared, proxy, acquisition, mut snapshot) = acquisition_proxy(true);
    let Submission::Submitted(digest) = acquisition.lock().unwrap().state else {
        unreachable!()
    };
    proxy.record_reply(
        Some(&acquisition),
        digest,
        true,
        &relay::Reply::Ok(snapshot.clone()),
    )?;
    let write_digest = snapshot.request_digest.clone().unwrap();
    let mut admission = CatalogAdmission {
        proxy,
        attempt: Some(WriteAttempt {
            sequence: 1,
            kind: WriteKind::Bootstrap,
            digest: write_digest.clone(),
            submission: acquisition.clone(),
        }),
    };
    for phase in [relay::Phase::Releasing, relay::Phase::Failed] {
        snapshot.phase = phase;
        let reply = queued_recovery_reply(shared.clone(), relay::Reply::Ok(snapshot.clone()));
        assert_eq!(
            admission.poll_release(1, WriteKind::Bootstrap)?,
            supervisor::ReleaseProgress::Pending
        );
        let request = reply.join().unwrap();
        assert!(
            matches!(request.action, relay::Action::ReleaseWrite { sequence: U64(1), kind: WriteKind::Bootstrap, request_digest } if request_digest == write_digest)
        );
        assert_eq!(
            acquisition.lock().unwrap().state,
            Submission::Acknowledged(digest)
        );
    }
    let reply = queued_recovery_reply(
        shared.clone(),
        relay::Reply::Error(application::error(
            ErrorCode::Closed,
            "injected release acknowledgement transport loss",
        )),
    );
    assert!(
        admission
            .poll_release(1, WriteKind::Bootstrap)
            .unwrap_err()
            .to_string()
            .contains("transport loss")
    );
    reply.join().unwrap();
    snapshot.phase = relay::Phase::Released;
    snapshot.sequence = Some(U64(2));
    let reply = queued_recovery_reply(shared.clone(), relay::Reply::Ok(snapshot.clone()));
    assert!(
        admission.poll_release(1, WriteKind::Bootstrap).is_err(),
        "wrong attempt cannot retire"
    );
    reply.join().unwrap();
    snapshot.sequence = Some(U64(1));
    let reply = queued_recovery_reply(shared, relay::Reply::Ok(snapshot));
    assert_eq!(
        admission.poll_release(1, WriteKind::Bootstrap)?,
        supervisor::ReleaseProgress::Released
    );
    reply.join().unwrap();
    Ok(())
}

#[cfg(unix)]
mod actual {
    use super::*;
    use crate::catalog_migration::{evidence, importer_tests::ImportFixture};
    use std::{fs, io::Write, os::unix::fs::PermissionsExt};
    const ROLE: &str = "PHOTOCATALOG_LM_FACADE_ROLE";
    const PIDS: &str = "PHOTOCATALOG_LM_FACADE_PIDS";
    const PAUSE: &str = "PHOTOCATALOG_LM_FACADE_PAUSE";
    const DRAIN_TIMEOUT: Duration = Duration::from_secs(120);
    #[test]
    fn lm_facade_actual_role_fixture() -> Result<()> {
        let Ok(role) = std::env::var(ROLE) else {
            return Ok(());
        };
        if let Some(path) = std::env::var_os(PIDS) {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(file, "{} {}", std::process::id(), role)?;
        }
        if role == "--lightroom-migration-worker"
            && let Some(path) = std::env::var_os(PAUSE)
        {
            while std::path::Path::new(&path).exists() {
                thread::sleep(Duration::from_millis(5));
            }
        }
        if role == "--catalog-desktop-worker" {
            ensure!(
                unsafe { libc::dup2(3, 1) } == 1,
                "restore catalog binary protocol"
            );
            let result = super::super::super::process::worker_main();
            std::process::exit(if result.is_ok() { 0 } else { 75 });
        }
        use std::os::fd::FromRawFd;
        let protocol = unsafe { fs::File::from_raw_fd(3) };
        match role.as_str() {
            "--lightroom-migration-worker" => worker::serve(std::io::stdin(), protocol),
            "--lightroom-source-reader-sql" => {
                crate::lightroom_migration_worker::source_reader::test_source_reader_main(
                    false, protocol,
                )
            }
            "--lightroom-source-reader-raw" => {
                crate::lightroom_migration_worker::source_reader::test_source_reader_main(
                    true, protocol,
                )
            }
            _ => anyhow::bail!("unexpected fixture role"),
        }
    }
    fn quote(path: &std::path::Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }
    struct Fixture {
        desktop: super::super::super::DesktopBridge,
        pool: ByteBudget,
        pids: PathBuf,
        receipts: PathBuf,
        fpid: u32,
        pause: PathBuf,
        _temp: tempfile::TempDir,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let executable = PathBuf::from(
                std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE")
                    .context("fresh exact CLI required")?,
            );
            let temp = tempfile::tempdir()?;
            let base = temp.path().canonicalize()?;
            let wrapper = base.join("migration-desktop-fixture");
            let pids = base.join("pids");
            let receipts = base.join("custody-receipts");
            let pause = base.join("pause-lm-before-input");
            let current = std::env::current_exe()?;
            fs::write(
                &wrapper,
                format!(
                    "#!/bin/sh\ncase \"$1\" in\n--catalog-desktop-worker|--lightroom-migration-worker|--lightroom-source-reader-sql|--lightroom-source-reader-raw) exec env {ROLE}=\"$1\" {PIDS}={} {PAUSE}={} PHOTOCATALOG_LM_CUSTODY_RECEIPTS={} {} --exact application::desktop::migration::tests::actual::lm_facade_actual_role_fixture --nocapture --test-threads=1 3>&1 1>/dev/null ;;\n*) exec {} \"$@\" ;;\nesac\n",
                    quote(&pids),
                    quote(&pause),
                    quote(&receipts),
                    quote(&current),
                    quote(&executable)
                ),
            )?;
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))?;
            let filesystem = Arc::new(crate::filesystem_worker::client::migration_fixture(&base)?);
            let fpid = filesystem.pid();
            let config = Config {
                worker_executable: wrapper,
                cache_root: None,
                original_roots: vec![],
                preview_policy: Default::default(),
                preview_limits: Default::default(),
                limits: Limits::default(),
                import_checkpoint: None,
            };
            // This metadata pool is shared by C/F admission and migration G
            // operation/result owners. Native preview work has a separate pool.
            use crate::lightroom_migration_worker::memory::{core, layout::add};
            let operation_allowance = add(
                add(4 * 1024 * 1024 * 1024, core::worker_repair_execution()?)?,
                add(
                    core::worker_supplement_documents(input::CLI_DOCUMENT_BYTES)?,
                    core::worker_supplement_execution(1024)?,
                )?,
            )?;
            let operation_allowance = add(
                operation_allowance,
                crate::catalog_migration::file_metadata::tests::Test::managed_preprojection()?
                    .managed_preprojection_allowance_bytes()?,
            )?;
            let allowance = config
                .requested_preview_metadata_bytes()?
                .checked_add(operation_allowance as u64)
                .context("managed fixture pool overflow")?;
            let pool = ByteBudget::new(allowance)?;
            // Future export G receives this same native pool; never fold it into
            // the requested-storage allowance above.
            let native = ByteBudget::new(config.preview_limits.working_bytes)?;
            println!(
                "LM_FACADE_POOL configured={allowance} requested_operation={operation_allowance}"
            );
            let desktop = super::super::super::DesktopBridge::spawn_with_filesystem(
                config, filesystem, &pool, &native,
            )?;
            ensure!(
                desktop.status().phase == super::super::super::TransportPhase::Ready,
                "managed desktop failed startup: {:?}",
                desktop.status()
            );
            Ok(Self {
                desktop,
                pool,
                pids,
                receipts,
                fpid,
                pause,
                _temp: temp,
            })
        }
        fn public(&self, request: api::Request) -> Result<api::Response> {
            match self
                .desktop
                .submit(Request::LightroomMigration {
                    request: Box::new(request),
                })?
                .recv()
            {
                Reply::Ok {
                    value: Response::LightroomMigration(response),
                } => Ok(*response),
                Reply::Error { error } => Err(error.into()),
                _ => anyhow::bail!("wrong public response"),
            }
        }
        fn catalog(&self, path: &std::path::Path) -> Result<String> {
            match self
                .desktop
                .submit(Request::OpenExisting {
                    path: NativePath::from_path(path),
                })?
                .recv()
            {
                Reply::Ok {
                    value: Response::Status(status),
                } => status.catalog.context("open token"),
                Reply::Error { error } => Err(error.into()),
                _ => anyhow::bail!("wrong open reply"),
            }
        }
        fn operate(
            &self,
            path: &std::path::Path,
            catalog: Option<String>,
            operation: api::Operation,
            docs: &[(api::InputRole, String)],
            label: &str,
        ) -> Result<(serde_json::Value, api::Snapshot)> {
            self.operate_with_timeout(
                path,
                catalog,
                operation,
                docs,
                label,
                Duration::from_secs(180),
            )
        }
        fn operate_with_timeout(
            &self,
            path: &std::path::Path,
            catalog: Option<String>,
            operation: api::Operation,
            docs: &[(api::InputRole, String)],
            label: &str,
            execution_timeout: Duration,
        ) -> Result<(serde_json::Value, api::Snapshot)> {
            let mut h = header(path, operation, docs);
            h.catalog = catalog;
            h.timeout_ms = U64(execution_timeout.as_millis().try_into()?);
            let guard = snap(self.public(api::Request::Begin {
                operation: label.into(),
                header: h,
            })?)?
            .guard;
            let outcome = (|| -> Result<_> {
                upload(&self.desktop.0.migration, &guard, docs)?;
                self.public(api::Request::Act {
                    guard: guard.clone(),
                })?;
                // The operation's own deadline starts G recovery; leave a separate
                // window for checked waits, joins, C release and explicit retries.
                let status =
                    self.wait_terminal(&guard, Instant::now() + execution_timeout + DRAIN_TIMEOUT)?;
                ensure!(
                    status.phase == api::Phase::Complete,
                    "managed facade failed: {status:?}"
                );
                let identity = status.result.as_ref().context("result identity")?;
                let mut output = String::new();
                for page in 0..identity.pages.0 {
                    let mut offset = U64(0);
                    loop {
                        let api::Response::Page {
                            text, next_offset, ..
                        } = self.public(api::Request::ResultPage {
                            guard: guard.clone(),
                            page: U64(page),
                            offset,
                            maximum_bytes: U64(TEXT_CHUNK as u64),
                        })?
                        else {
                            anyhow::bail!("wrong managed result-page response")
                        };
                        output.push_str(&text);
                        if let Some(next) = next_offset {
                            offset = next;
                        } else {
                            break;
                        }
                    }
                }
                ensure!(
                    output.len() as u64 == identity.bytes.0,
                    "result byte identity changed"
                );
                ensure!(
                    blake3::hash(output.as_bytes()).to_hex().as_str() == identity.blake3,
                    "result digest identity changed"
                );
                let value = serde_json::from_str(&output)?;
                self.public(api::Request::Discard {
                    guard: guard.clone(),
                })?;
                Ok((value, status))
            })();
            match outcome {
                Ok(value) => Ok(value),
                Err(primary) => {
                    // Returning Err must not abandon the still-addressable G
                    // owner. Checked cleanup never converts this failure to Ok.
                    match self.recover_operation(&guard) {
                        Ok(()) => Err(primary),
                        Err(cleanup) => {
                            Err(primary
                                .context(format!("managed fixture cleanup failed: {cleanup:#}")))
                        }
                    }
                }
            }
        }
        fn wait_terminal(&self, guard: &api::Guard, deadline: Instant) -> Result<api::Snapshot> {
            loop {
                let status = snap(self.public(api::Request::Status {
                    guard: guard.clone(),
                })?)?;
                if matches!(status.phase, api::Phase::Complete | api::Phase::Failed) {
                    return Ok(status);
                }
                ensure!(
                    Instant::now() < deadline,
                    "managed facade timeout: {status:?}"
                );
                if status.phase == api::Phase::DrainPending {
                    self.public(api::Request::RetryDrain {
                        guard: guard.clone(),
                    })?;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
        fn recover_operation(&self, guard: &api::Guard) -> Result<()> {
            let operation = (|| -> Result<()> {
                if let api::Response::Status(_) = self.public(api::Request::Cancel {
                    guard: guard.clone(),
                })? {
                    let status = self.wait_terminal(guard, Instant::now() + DRAIN_TIMEOUT)?;
                    println!("LM_FACADE_FIXTURE_RECOVERY terminal={status:?}");
                    self.public(api::Request::Discard {
                        guard: guard.clone(),
                    })?;
                    if let Some(catalog) = status.catalog {
                        self.close(catalog)?;
                    }
                }
                Ok(())
            })();
            // Always attempt checked shutdown even when a preceding cleanup
            // request failed. Preserve both errors instead of claiming retirement.
            let shutdown = self.shutdown_checked();
            match (operation, shutdown) {
                (Ok(()), result) | (result, Ok(())) => result,
                (Err(operation), Err(shutdown)) => Err(operation.context(format!(
                    "managed fixture shutdown also failed: {shutdown:#}"
                ))),
            }
        }
        fn close(&self, catalog: String) -> Result<()> {
            match self.desktop.submit(Request::Close { catalog })?.recv() {
                Reply::Ok { .. } => Ok(()),
                Reply::Error { error } => Err(error.into()),
            }
        }
        fn shutdown_checked(&self) -> Result<()> {
            let deadline = Instant::now() + DRAIN_TIMEOUT;
            loop {
                match self.desktop.try_shutdown() {
                    Ok(()) => break,
                    Err(error) if Instant::now() >= deadline => {
                        anyhow::bail!("managed fixture shutdown remains pending: {error:#}");
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
            self.check_reaped()
        }
        fn check_reaped(&self) -> Result<()> {
            for line in fs::read_to_string(&self.pids)?.lines() {
                let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
                ensure!(unsafe { libc::kill(pid, 0) } == -1, "retained child {line}");
                ensure!(
                    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
                    "child retirement not verified: {line}"
                );
                println!("LM_FACADE_REAP {line}");
            }
            ensure!(
                unsafe { libc::kill(self.fpid as i32, 0) } == -1,
                "retained F"
            );
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
                "F retirement not verified"
            );
            println!("LM_FACADE_REAP {} filesystem", self.fpid);
            Ok(())
        }
        fn finish(self) -> Result<()> {
            self.desktop.try_shutdown()?;
            self.verify_retired()
        }
        fn verify_retired(self) -> Result<()> {
            ensure!(self.desktop.0.migration.drained(), "G join remains pending");
            let state = self.desktop.0.shared.state.lock().unwrap();
            ensure!(
                state.reaped && state.child_finished && state.filesystem_verified,
                "checked C/F retirement required before fixture finalization"
            );
            drop(state);
            self.check_reaped()?;
            let pool = self.pool.clone();
            drop(self.desktop);
            assert_eq!(pool.used(), 0);
            Ok(())
        }
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_managed_run_status_preserve_raw_rows_and_reap_all_roles() -> Result<()> {
        use crate::lightroom::selection::{ApprovalDocument, ApprovalScope};
        let retained = ImportFixture::new(false)?;
        let fixture = Fixture::new()?;
        let approval = ApprovalDocument {
            protocol: 1,
            review_token: "e".repeat(64),
            scope: ApprovalScope::SelectedMigrationTest,
            destination: NativePath::from_path(&retained.destination),
            policy: retained.policy.clone(),
            supplements: vec![],
            authorization: "explicit synthetic G/C migration".into(),
        };
        let approval = serde_json::to_string(&approval)?;
        let digest = blake3::hash(approval.as_bytes()).to_hex().to_string();
        let mut seal = retained.inspection.seal.clone();
        seal.approval.document_blake3 = digest.clone();
        let documents = vec![
            (api::InputRole::Seal, serde_json::to_string(&seal)?),
            (api::InputRole::Approval, approval),
            (
                api::InputRole::Policy,
                serde_json::to_string(&retained.policy)?,
            ),
        ];
        let (run, status) = fixture.operate(
            &retained.destination,
            None,
            api::Operation::Run {
                approval_blake3: digest,
                max_steps: U64(4000),
                max_seconds: U64(120),
                source_open_ms: U64(30_000),
                artifact_open_ms: U64(ImportFixture::limits().open_deadline_ms),
                max_artifact_bytes: U64(ImportFixture::limits().maximum_bytes),
            },
            &documents,
            "run",
        )?;
        assert!(status.progress.is_some());
        let catalog = status.catalog.context("bootstrap C token")?;
        let run_id = run["progress"]["id"].as_str().context("run id")?.to_owned();
        let (read, _) = fixture.operate(
            &retained.destination,
            Some(catalog.clone()),
            api::Operation::Status { run: run_id },
            &[],
            "status",
        )?;
        assert_eq!(read, run["progress"]);
        fixture.close(catalog)?;
        let catalog = retained.open()?;
        assert_eq!(
            catalog
                .db
                .query_row("SELECT count(*) FROM migration_runs", [], |r| r
                    .get::<_, i64>(0))?,
            1
        );
        for (index, expected) in retained.raw().iter().enumerate() {
            let captures = catalog.retained_migration_records(
                run["progress"]["input"].as_str().context("input")?,
                &retained.inspection.seal.selected[index].revision,
                crate::lightroom::migration_source::Collection::Captures,
                0,
                2,
            )?;
            assert_eq!(captures.len(), 1);
            let (_, state) = catalog.migration_artifact(captures[0].0, 0)?;
            let mut actual = Vec::new();
            while actual.len() < expected.len() {
                actual.extend(evidence::read(&catalog.db, &state.id, actual.len() as u64)?);
            }
            assert_eq!(&actual, expected);
        }
        drop(catalog);
        let pids = fs::read_to_string(&fixture.pids)?;
        assert_eq!(pids.matches("--lightroom-migration-worker").count(), 2);
        assert_eq!(pids.matches("--lightroom-source-reader-sql").count(), 1);
        assert_eq!(
            pids.matches("--lightroom-source-reader-raw").count(),
            retained.raw().len()
        );
        fixture.finish()
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_remaining_five_routes_preserve_repairs_and_status() -> Result<()> {
        let fixture = Fixture::new()?;
        let supplement = crate::catalog_migration::supplements::tests::Fixture::new()?;
        let destination = fixture
            ._temp
            .path()
            .canonicalize()?
            .join("supplement-target");
        let (prepared, status) = fixture.operate(
            &destination,
            None,
            api::Operation::PrepareSupplements,
            &[(
                api::InputRole::SupplementRequests,
                serde_json::to_string(&vec![supplement.request])?,
            )],
            "supplements",
        )?;
        assert_eq!(prepared.as_array().map(Vec::len), Some(1));
        fixture.close(status.catalog.context("supplement token")?)?;
        let current =
            crate::catalog_migration::importer_tests::current_repair_tests::managed_fixture()?;
        let catalog = fixture.catalog(&current.fixture.destination)?;
        let documents = vec![
            (
                api::InputRole::Seal,
                serde_json::to_string(&current.fixture.inspection.seal)?,
            ),
            (api::InputRole::Approval, current.approval.clone()),
            (
                api::InputRole::RepairRequest,
                serde_json::to_string(&current.request)?,
            ),
        ];
        let (current_output, _) = fixture.operate(
            &current.fixture.destination,
            Some(catalog.clone()),
            api::Operation::RepairCurrent {
                max_steps: U64(1000),
                max_seconds: U64(120),
                source_open_ms: U64(30_000),
            },
            &documents,
            "current",
        )?;
        assert_eq!(current_output["repair"]["complete"], true);
        let repair = current_output["repair"]["id"]
            .as_str()
            .context("current repair id")?
            .into();
        let (current_status, _) = fixture.operate(
            &current.fixture.destination,
            Some(catalog.clone()),
            api::Operation::RepairStatus { repair },
            &[],
            "current-status",
        )?;
        assert_eq!(current_status, current_output["repair"]);
        fixture.close(catalog)?;
        assert_eq!(
            current.fixture.open()?.db.query_row(
                "SELECT count(*) FROM migration_current_repair_items",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            6
        );
        let keywords =
            crate::catalog_migration::importer_tests::keyword_repair_tests::managed_fixture()?;
        let catalog = fixture.catalog(&keywords.fixture.destination)?;
        let documents = vec![
            (
                api::InputRole::Seal,
                serde_json::to_string(&keywords.fixture.inspection.seal)?,
            ),
            (api::InputRole::Approval, keywords.approval.clone()),
            (
                api::InputRole::RepairRequest,
                serde_json::to_string(&keywords.request)?,
            ),
        ];
        let (keyword_output, _) = fixture.operate(
            &keywords.fixture.destination,
            Some(catalog.clone()),
            api::Operation::RepairKeywords {
                max_steps: U64(1000),
                max_seconds: U64(120),
                source_open_ms: U64(30_000),
            },
            &documents,
            "keywords",
        )?;
        assert_eq!(keyword_output["repair"]["complete"], true);
        let repair = keyword_output["repair"]["id"]
            .as_str()
            .context("keyword repair id")?
            .into();
        let (keyword_status, _) = fixture.operate(
            &keywords.fixture.destination,
            Some(catalog.clone()),
            api::Operation::KeywordRepairStatus { repair },
            &[],
            "keyword-status",
        )?;
        assert_eq!(keyword_status, keyword_output["repair"]);
        fixture.close(catalog)?;
        assert_eq!(
            keywords.fixture.open()?.db.query_row(
                "SELECT count(*) FROM migration_keyword_repair_items",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            14
        );
        let pids = fs::read_to_string(&fixture.pids)?;
        assert_eq!(pids.matches("--lightroom-migration-worker").count(), 5);
        // Only current and keyword repair open SQL Sources; supplements/status do not.
        assert_eq!(pids.matches("--lightroom-source-reader-sql").count(), 2);
        fixture.finish()
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_lost_writer_acks_keep_close_and_result_excluded_until_drain() -> Result<()>
    {
        let fixture = Fixture::new()?;
        let supplement = crate::catalog_migration::supplements::tests::Fixture::new()?;
        let destination = fixture._temp.path().canonicalize()?.join("target");
        let documents = vec![(
            api::InputRole::SupplementRequests,
            serde_json::to_string(&vec![supplement.request])?,
        )];
        {
            let mut faults = fixture.desktop.0.migration.faults.lock().unwrap();
            faults.lost_acquire = 1;
            faults.lost_release = 1;
            faults.pause_drain = true;
        }
        let guard = snap(fixture.public(api::Request::Begin {
            operation: "lost-acks".into(),
            header: header(&destination, api::Operation::PrepareSupplements, &documents),
        })?)?
        .guard;
        upload(&fixture.desktop.0.migration, &guard, &documents)?;
        fixture.public(api::Request::Act {
            guard: guard.clone(),
        })?;
        let deadline = Instant::now() + Duration::from_secs(180);
        let status = loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if status.phase == api::Phase::DrainPending {
                break status;
            }
            ensure!(
                Instant::now() < deadline,
                "lost ACK operation deadline: {status:?}"
            );
            thread::sleep(Duration::from_millis(5));
        };
        let held = fixture.pool.used();
        assert!(held > 0);
        assert!(
            fixture
                .public(api::Request::ResultPage {
                    guard: guard.clone(),
                    page: U64(0),
                    offset: U64(0),
                    maximum_bytes: U64(100)
                })
                .is_err()
        );
        assert!(
            fixture
                .public(api::Request::Discard {
                    guard: guard.clone()
                })
                .is_err()
        );
        let catalog = status.catalog.context("bootstrap token")?;
        assert!(matches!(
            fixture
                .desktop
                .submit(Request::Close {
                    catalog: catalog.clone()
                })
                .err()
                .context("Close must retain")?
                .code,
            ErrorCode::Busy
        ));
        assert!(fixture.pool.used() >= held);
        {
            let mut faults = fixture.desktop.0.migration.faults.lock().unwrap();
            assert_eq!(faults.lost, 2);
            faults.pause_drain = false;
        }
        fixture.public(api::Request::RetryDrain {
            guard: guard.clone(),
        })?;
        let status = loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if matches!(status.phase, api::Phase::Complete | api::Phase::Failed) {
                break status;
            }
            ensure!(Instant::now() < deadline, "retry drain deadline");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.phase, api::Phase::Complete, "{status:?}");
        assert!(fixture.pool.used() < held);
        fixture.public(api::Request::Discard { guard })?;
        fixture.close(catalog)?;
        fixture.finish()
    }

    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_large_prepared_result_pages_above_eight_mib() -> Result<()> {
        let fixture = Fixture::new()?;
        let outcome = (|| -> Result<()> {
            let mut supplement = crate::catalog_migration::supplements::tests::Fixture::new()?;
            let root = supplement.request.proof_root.to_path()?;
            let document = root.join(supplement.request.inspection_relative.to_path()?);
            let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&document)?)?;
            supplement.request.source_id = "\"".repeat(4096);
            value["source_id"] = supplement.request.source_id.clone().into();
            let bytes = serde_json::to_vec(&value)?;
            fs::write(&document, &bytes)?;
            supplement.request.inspection_blake3 = blake3::hash(&bytes).to_hex().to_string();
            let requests = vec![supplement.request; 1024];
            let documents = vec![(
                api::InputRole::SupplementRequests,
                serde_json::to_string(&requests)?,
            )];
            ensure!(
                documents[0].1.len() <= input::CLI_DOCUMENT_BYTES,
                "supplement input bound"
            );
            let destination = fixture._temp.path().canonicalize()?.join("large-prepared");
            let (prepared, status) = fixture.operate_with_timeout(
                &destination,
                None,
                api::Operation::PrepareSupplements,
                &documents,
                "large-prepared",
                // The 1024 real preparations exceeded the former 180-second limit
                // at 488 results. Keep the full roster with execution headroom.
                Duration::from_secs(600),
            )?;
            ensure!(
                prepared.as_array().map(Vec::len) == Some(1024),
                "full Prepared roster"
            );
            ensure!(
                status.result.as_ref().unwrap().bytes.0 > 8 * 1024 * 1024,
                "result must exceed 8 MiB"
            );
            ensure!(
                status.result.as_ref().unwrap().pages.0 > 1,
                "result must span pages"
            );
            fixture.close(status.catalog.context("prepared catalog token")?)?;
            Ok(())
        })();
        let cleanup = fixture.finish();
        match (outcome, cleanup) {
            (Ok(()), result) | (result, Ok(())) => result,
            (Err(primary), Err(cleanup)) => Err(primary.context(format!(
                "large-result fixture cleanup also failed: {cleanup:#}"
            ))),
        }
    }

    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_fixture_deadline_preserves_unknown_failure_and_checks_cleanup() -> Result<()>
    {
        let fixture = Fixture::new()?;
        fs::write(&fixture.pause, b"pause before LM input")?;
        fixture
            .desktop
            .0
            .migration
            .faults
            .lock()
            .unwrap()
            .wait_failures = 1;
        let destination = fixture
            ._temp
            .path()
            .canonicalize()?
            .join("deadline-no-bootstrap");
        let documents = vec![(api::InputRole::SupplementRequests, "[]".into())];
        let failure = fixture
            .operate_with_timeout(
                &destination,
                None,
                api::Operation::PrepareSupplements,
                &documents,
                "fixture-deadline",
                Duration::from_secs(5),
            )
            .unwrap_err();
        let detail = format!("{failure:#}");
        assert!(detail.contains("migration operation deadline"), "{detail}");
        assert!(detail.contains("outcome_unknown: true"), "{detail}");
        assert!(
            fixture
                .desktop
                .0
                .migration
                .faults
                .lock()
                .unwrap()
                .wait_injected
        );
        assert!(fixture.desktop.0.migration.drained());
        assert!(!destination.exists());
        let pids = fs::read_to_string(&fixture.pids)?;
        assert!(pids.contains("--lightroom-migration-worker"));
        // operate returned its original negative result only after guard recovery
        // and checked C/LM/F retirement. finish also verifies the pool reaches zero.
        fixture.check_reaped()?;
        fixture.finish()
    }
    fn interrupted_startup(abrupt: bool) -> Result<()> {
        let fixture = Fixture::new()?;
        fs::write(&fixture.pause, b"pause before LM admission")?;
        let destination = fixture
            ._temp
            .path()
            .canonicalize()?
            .join("must-not-bootstrap");
        let documents = vec![(api::InputRole::SupplementRequests, "[]".into())];
        let guard = snap(fixture.public(api::Request::Begin {
            operation: "interrupted-startup".into(),
            header: header(&destination, api::Operation::PrepareSupplements, &documents),
        })?)?
        .guard;
        upload(&fixture.desktop.0.migration, &guard, &documents)?;
        fixture.public(api::Request::Act {
            guard: guard.clone(),
        })?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let pid = loop {
            let pids = fs::read_to_string(&fixture.pids)?;
            if let Some(line) = pids
                .lines()
                .find(|line| line.ends_with("--lightroom-migration-worker"))
            {
                break line.split_whitespace().next().unwrap().parse::<i32>()?;
            }
            ensure!(Instant::now() < deadline, "LM startup PID deadline");
            thread::sleep(Duration::from_millis(5));
        };
        if abrupt {
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        } else {
            fixture.public(api::Request::Cancel {
                guard: guard.clone(),
            })?;
        }
        let status = loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if status.phase == api::Phase::Failed {
                break status;
            }
            if status.phase == api::Phase::DrainPending {
                fixture.public(api::Request::RetryDrain {
                    guard: guard.clone(),
                })?;
            }
            ensure!(
                Instant::now() < deadline,
                "startup interruption drain: {status:?}"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert!(status.failure.is_some());
        assert!(!destination.exists());
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        fixture.public(api::Request::Discard { guard })?;
        fixture.finish()
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_cancel_during_lm_input_reaps_before_target_reuse() -> Result<()> {
        interrupted_startup(false)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_abrupt_lm_reaps_before_target_reuse() -> Result<()> {
        interrupted_startup(true)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_abrupt_c_retires_managed_children_with_unknown_outcome() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture
            .desktop
            .0
            .migration
            .faults
            .lock()
            .unwrap()
            .pause_result = true;
        let supplement = crate::catalog_migration::supplements::tests::Fixture::new()?;
        let destination = fixture._temp.path().canonicalize()?.join("target");
        let documents = vec![(
            api::InputRole::SupplementRequests,
            serde_json::to_string(&vec![supplement.request])?,
        )];
        let guard = snap(fixture.public(api::Request::Begin {
            operation: "abrupt-c".into(),
            header: header(&destination, api::Operation::PrepareSupplements, &documents),
        })?)?
        .guard;
        upload(&fixture.desktop.0.migration, &guard, &documents)?;
        fixture.public(api::Request::Act {
            guard: guard.clone(),
        })?;
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if fixture
                .desktop
                .0
                .migration
                .faults
                .lock()
                .unwrap()
                .result_seen
            {
                break;
            }
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            ensure!(
                !matches!(status.phase, api::Phase::Failed | api::Phase::Complete),
                "real migration result boundary: {status:?}"
            );
            ensure!(Instant::now() < deadline, "LM result boundary deadline");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(destination.exists(), "actual Bootstrap completed");
        assert_eq!(
            unsafe { libc::kill(fixture.desktop.0.pid as i32, libc::SIGKILL) },
            0
        );
        let status = loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if status.phase == api::Phase::Failed {
                break status;
            }
            if status.phase == api::Phase::DrainPending {
                fixture.public(api::Request::RetryDrain {
                    guard: guard.clone(),
                })?;
            }
            ensure!(Instant::now() < deadline, "abrupt C G drain: {status:?}");
            thread::sleep(Duration::from_millis(5));
        };
        let failed = status.failure.context("typed unknown failure")?;
        assert!(failed.poisoned && failed.outcome_unknown);
        assert!(
            fixture.desktop.0.migration.drained(),
            "G join required before production retirement"
        );
        let state = fixture.desktop.0.shared.state.lock().unwrap();
        assert!(state.reaped && state.child_finished);
        drop(state);
        let mut state = fixture.desktop.0.shared.state.lock().unwrap();
        assert!(state.ready, "actual paired identity handshake was verified");
        assert!(super::super::super::managed_catalog_retired(
            &state, true, true
        ));
        assert!(!super::super::super::managed_catalog_retired(
            &state, false, true
        ));
        assert!(!super::super::super::managed_catalog_retired(
            &state, true, false
        ));
        state.ready = false;
        assert!(!super::super::super::managed_catalog_retired(
            &state, true, true
        ));
        drop(state);
        assert!(
            fixture.desktop.try_shutdown().is_err(),
            "unverified identity retains F"
        );
        assert_eq!(unsafe { libc::kill(fixture.fpid as i32, 0) }, 0);
        fixture.desktop.0.shared.state.lock().unwrap().ready = true;
        fixture.desktop.try_shutdown()?;
        let after = snap(fixture.public(api::Request::Status {
            guard: guard.clone(),
        })?)?;
        assert_eq!(after.phase, api::Phase::Failed);
        assert!(
            after
                .failure
                .is_some_and(|f| f.poisoned && f.outcome_unknown)
        );
        let pids = fs::read_to_string(&fixture.pids)?;
        let mut roles: Vec<_> = pids
            .lines()
            .map(|line| line.split_whitespace().nth(1).unwrap())
            .collect();
        roles.sort_unstable();
        assert_eq!(
            roles,
            ["--catalog-desktop-worker", "--lightroom-migration-worker"]
        );
        assert!(!pids.contains("--lightroom-source-reader-sql"));
        fixture.public(api::Request::Discard { guard })?;
        println!("LM_FACADE_ABRUPT_C retirement=verified catalog_outcome=unknown g_join=checked");
        // The successful shutdown above already checked physical retirement;
        // another shutdown may report the retained abnormal F outcome.
        fixture.verify_retired()
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_unknown_lm_wait_keeps_same_pool_charge_until_explicit_retry() -> Result<()>
    {
        let fixture = Fixture::new()?;
        fs::write(&fixture.pause, b"pause before LM input")?;
        fixture
            .desktop
            .0
            .migration
            .faults
            .lock()
            .unwrap()
            .wait_failures = 1;
        let destination = fixture._temp.path().canonicalize()?.join("no-bootstrap");
        let documents = vec![(api::InputRole::SupplementRequests, "[]".into())];
        let guard = snap(fixture.public(api::Request::Begin {
            operation: "unknown-wait".into(),
            header: header(&destination, api::Operation::PrepareSupplements, &documents),
        })?)?
        .guard;
        upload(&fixture.desktop.0.migration, &guard, &documents)?;
        fixture.public(api::Request::Act {
            guard: guard.clone(),
        })?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if fs::read_to_string(&fixture.pids)?
                .lines()
                .any(|line| line.ends_with("--lightroom-migration-worker"))
            {
                break;
            }
            ensure!(Instant::now() < deadline, "paused LM startup PID deadline");
            thread::sleep(Duration::from_millis(5));
        }
        // send_inputs cannot finish while LM is paused before its input-memory
        // grant. Cancel first so the retained process reaches the wait injector.
        fixture.public(api::Request::Cancel {
            guard: guard.clone(),
        })?;
        loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if status.phase == api::Phase::DrainPending
                && status.failure.as_ref().is_some_and(|f| f.outcome_unknown)
                && fixture
                    .desktop
                    .0
                    .migration
                    .faults
                    .lock()
                    .unwrap()
                    .wait_injected
            {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "unknown wait remains addressable: {status:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let held = fixture.pool.used();
        assert!(!fixture.desktop.0.migration.drained());
        assert!(
            fixture
                .public(api::Request::Discard {
                    guard: guard.clone()
                })
                .is_err()
        );
        assert_eq!(fixture.pool.used(), held);
        fixture.public(api::Request::RetryDrain {
            guard: guard.clone(),
        })?;
        loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if status.phase == api::Phase::Failed {
                assert!(status.failure.unwrap().outcome_unknown);
                break;
            }
            ensure!(Instant::now() < deadline, "checked wait retry deadline");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(fixture.pool.used() < held);
        assert!(!destination.exists());
        fixture.public(api::Request::Discard { guard })?;
        fixture.finish()
    }
    fn cancel_phase(writer: bool) -> Result<()> {
        let fixture = Fixture::new()?;
        let supplement = crate::catalog_migration::supplements::tests::Fixture::new()?;
        let destination = fixture._temp.path().canonicalize()?.join("target");
        let documents = vec![(
            api::InputRole::SupplementRequests,
            serde_json::to_string(&vec![supplement.request])?,
        )];
        {
            let mut faults = fixture.desktop.0.migration.faults.lock().unwrap();
            faults.pause_writer = writer;
            faults.pause_result = !writer;
        }
        let guard = snap(fixture.public(api::Request::Begin {
            operation: "cancel-phase".into(),
            header: header(&destination, api::Operation::PrepareSupplements, &documents),
        })?)?
        .guard;
        upload(&fixture.desktop.0.migration, &guard, &documents)?;
        fixture.public(api::Request::Act {
            guard: guard.clone(),
        })?;
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let reached = {
                let faults = fixture.desktop.0.migration.faults.lock().unwrap();
                if writer {
                    faults.writer_held
                } else {
                    faults.result_seen
                }
            };
            if reached {
                break;
            }
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            ensure!(
                !matches!(status.phase, api::Phase::Failed | api::Phase::Complete),
                "phase reached terminal early: {status:?}"
            );
            ensure!(Instant::now() < deadline, "cancel phase deadline");
            thread::sleep(Duration::from_millis(5));
        }
        if writer {
            let pending = relay::Client::new(&fixture.desktop.0.shared).submit(relay::Request {
                guard: guard.clone(),
                action: relay::Action::Status,
            })?;
            let relay::Reply::Ok(snapshot) =
                pending.receiver.recv_timeout(Duration::from_secs(5))?
            else {
                anyhow::bail!("C writer status failed")
            };
            assert_eq!(snapshot.phase, relay::Phase::Held);
            assert_eq!(snapshot.write_kind, Some(WriteKind::Catalog));
        }
        assert!(
            fixture
                .public(api::Request::ResultPage {
                    guard: guard.clone(),
                    page: U64(0),
                    offset: U64(0),
                    maximum_bytes: U64(100)
                })
                .is_err()
        );
        fixture.public(api::Request::Cancel {
            guard: guard.clone(),
        })?;
        let status = loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if status.phase == api::Phase::Failed {
                break status;
            }
            if status.phase == api::Phase::DrainPending {
                fixture.public(api::Request::RetryDrain {
                    guard: guard.clone(),
                })?;
            }
            ensure!(Instant::now() < deadline, "canceled phase drain deadline");
            thread::sleep(Duration::from_millis(5));
        };
        assert!(status.failure.is_some());
        let catalog = status.catalog.context("created catalog token")?;
        fixture.public(api::Request::Discard { guard })?;
        fixture.close(catalog)?;
        fixture.finish()
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_same_pool_required_minus_one_drains_then_retries() -> Result<()> {
        let fixture = Fixture::new()?;
        let supplement = crate::catalog_migration::supplements::tests::Fixture::new()?;
        let destination = fixture._temp.path().canonicalize()?.join("target");
        let documents = vec![(
            api::InputRole::SupplementRequests,
            serde_json::to_string(&vec![supplement.request])?,
        )];
        let guard = snap(fixture.public(api::Request::Begin {
            operation: "same-pool-refusal".into(),
            header: header(&destination, api::Operation::PrepareSupplements, &documents),
        })?)?
        .guard;
        upload(&fixture.desktop.0.migration, &guard, &documents)?;
        let (limit, used) = fixture.pool.snapshot();
        let required = input::INPUT_BYTES as u64;
        let competitor = fixture.pool.reserve_exact(limit - used - (required - 1))?;
        fixture.public(api::Request::Act {
            guard: guard.clone(),
        })?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let failed = loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if status.phase == api::Phase::Failed {
                break status.failure.context("typed refusal")?;
            }
            ensure!(
                Instant::now() < deadline,
                "refusal target drain deadline: {status:?}"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(failed.code, ErrorCode::ResourceLimit));
        assert_eq!(failed.required, Some(U64(required)));
        assert_eq!(failed.available, Some(U64(required - 1)));
        assert!(fixture.desktop.0.migration.drained());
        assert!(!destination.exists(), "refusal precedes Bootstrap");
        let pids = fs::read_to_string(&fixture.pids)?;
        assert_eq!(
            pids.lines().count(),
            1,
            "no LM or Source spawned on refusal"
        );
        drop(competitor);
        fixture.public(api::Request::Discard { guard })?;
        let (_, status) = fixture.operate(
            &destination,
            None,
            api::Operation::PrepareSupplements,
            &documents,
            "same-pool-retry",
        )?;
        fixture.close(status.catalog.context("retry catalog token")?)?;
        fixture.finish()
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_cancel_with_c_writer_held_and_g_grant_pending() -> Result<()> {
        cancel_phase(true)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_cancel_during_result_delivery_retains_unknown_until_drained() -> Result<()>
    {
        cancel_phase(false)
    }
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum AcquisitionFault {
        Lost,
        Disconnected,
        Overflow,
    }
    fn remote(fixture: &Fixture, guard: &Guard) -> Result<relay::Snapshot> {
        let reply = relay::Client::new(&fixture.desktop.0.shared)
            .submit(relay::Request {
                guard: guard.clone(),
                action: relay::Action::Status,
            })?
            .receiver
            .recv_timeout(Duration::from_secs(5))?;
        let relay::Reply::Ok(snapshot) = reply else {
            anyhow::bail!("live C status refused")
        };
        Ok(snapshot)
    }
    fn acquisition_fault(writer: bool, fault: AcquisitionFault, shutdown: bool) -> Result<()> {
        let fixture = Fixture::new()?;
        let baseline = fixture.pool.used();
        let cpid = fixture.desktop.0.pid;
        let supplement = crate::catalog_migration::supplements::tests::Fixture::new()?;
        let destination = fixture._temp.path().canonicalize()?.join("target");
        let reuse_documents = vec![(
            api::InputRole::SupplementRequests,
            serde_json::to_string(&vec![supplement.request])?,
        )];
        // Run opens a real SQL Source before NeedWrite(Catalog); the local
        // synthetic fixture stays alive through every descendant drain.
        let retained = writer.then(|| ImportFixture::new(false)).transpose()?;
        let (operation, documents) = if let Some(retained) = &retained {
            use crate::lightroom::selection::{ApprovalDocument, ApprovalScope};
            let approval = serde_json::to_string(&ApprovalDocument {
                protocol: 1,
                review_token: "e".repeat(64),
                scope: ApprovalScope::SelectedMigrationTest,
                destination: NativePath::from_path(&destination),
                policy: retained.policy.clone(),
                supplements: vec![],
                authorization: "synthetic ACK recovery fixture".into(),
            })?;
            let digest = blake3::hash(approval.as_bytes()).to_hex().to_string();
            let mut seal = retained.inspection.seal.clone();
            seal.approval.document_blake3 = digest.clone();
            (
                api::Operation::Run {
                    approval_blake3: digest,
                    max_steps: U64(4000),
                    max_seconds: U64(120),
                    source_open_ms: U64(30_000),
                    artifact_open_ms: U64(ImportFixture::limits().open_deadline_ms),
                    max_artifact_bytes: U64(ImportFixture::limits().maximum_bytes),
                },
                vec![
                    (api::InputRole::Seal, serde_json::to_string(&seal)?),
                    (api::InputRole::Approval, approval),
                    (
                        api::InputRole::Policy,
                        serde_json::to_string(&retained.policy)?,
                    ),
                ],
            )
        } else {
            (api::Operation::PrepareSupplements, reuse_documents.clone())
        };
        {
            let mut faults = fixture.desktop.0.migration.faults.lock().unwrap();
            faults.catalog_acquire_faults_only = true;
            faults.pause_after_acquire_fault = true;
            faults.pause_acquire_recovery = true;
            match (writer, fault) {
                (false, AcquisitionFault::Lost) => faults.lost_target = 1,
                (true, AcquisitionFault::Lost) => faults.lost_acquire = 1,
                (false, AcquisitionFault::Disconnected) => faults.disconnect_target = 1,
                (true, AcquisitionFault::Disconnected) => faults.disconnect_write = 1,
                (false, AcquisitionFault::Overflow) => faults.unknown_target = 1,
                (true, AcquisitionFault::Overflow) => faults.unknown_write = 1,
            }
        }
        let guard = snap(fixture.public(api::Request::Begin {
            operation: "ack-lifecycle".into(),
            header: header(&destination, operation, &documents),
        })?)?
        .guard;
        upload(&fixture.desktop.0.migration, &guard, &documents)?;
        fixture.public(api::Request::Act {
            guard: guard.clone(),
        })?;
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if fixture
                .desktop
                .0
                .migration
                .faults
                .lock()
                .unwrap()
                .acquire_fault_seen
            {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "actual acquisition ACK fault was not reached"
            );
            thread::sleep(Duration::from_millis(2));
        }
        // The fault occurs after a real C action. Wait for the installed writer
        // to be held before cancellation; G has not received its grant.
        let held_remote = loop {
            let state = remote(&fixture, &guard)?;
            if state.phase
                == if writer {
                    relay::Phase::Held
                } else {
                    relay::Phase::Target
                }
            {
                break state;
            }
            ensure!(
                Instant::now() < deadline,
                "real C authority not held: {state:?}"
            );
            thread::sleep(Duration::from_millis(2));
        };
        if writer {
            assert_eq!(held_remote.write_kind, Some(WriteKind::Catalog));
        }
        let receipts_before = fs::read_to_string(&fixture.receipts)?;
        assert_eq!(receipts_before.matches("target-installed").count(), 1);
        assert_eq!(receipts_before.matches("target-drained").count(), 0);
        assert_eq!(
            receipts_before.matches("write-installed").count(),
            if writer { 2 } else { 0 }
        );
        assert_eq!(
            receipts_before.matches("permit-held").count(),
            if writer { 2 } else { 0 }
        );
        assert_eq!(
            receipts_before.matches("permit-released").count(),
            if writer { 1 } else { 0 }
        );
        let shutdown_owner = if shutdown {
            let desktop = fixture.desktop.clone();
            Some(thread::spawn(move || desktop.try_shutdown()))
        } else {
            if fault == AcquisitionFault::Overflow {
                // Let the actual bounded-reply failure win before cancellation.
                fixture
                    .desktop
                    .0
                    .migration
                    .faults
                    .lock()
                    .unwrap()
                    .pause_after_acquire_fault = false;
            } else if writer && fault == AcquisitionFault::Lost {
                // Public Close performs real cancellation while retaining C.
                assert!(matches!(
                    fixture
                        .desktop
                        .submit(Request::Close {
                            catalog: held_remote.catalog.clone().context("held writer catalog")?,
                        })
                        .err()
                        .context("Close must retain unacknowledged writer")?
                        .code,
                    ErrorCode::Busy
                ));
            } else {
                fixture.public(api::Request::Cancel {
                    guard: guard.clone(),
                })?;
            }
            None
        };
        let primary = loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            let seen = fixture
                .desktop
                .0
                .migration
                .faults
                .lock()
                .unwrap()
                .acquire_recovery_seen;
            if seen
                && status.phase == api::Phase::DrainPending
                && let Some(failure) = status.failure
            {
                break failure;
            }
            ensure!(
                Instant::now() < deadline,
                "unknown acquisition not retained: {status:?}"
            );
            thread::sleep(Duration::from_millis(2));
        };
        assert!(
            primary.outcome_unknown,
            "typed unknown outcome: {primary:?}"
        );
        if fault != AcquisitionFault::Overflow {
            assert!(matches!(primary.code, ErrorCode::Canceled), "{primary:?}");
        } else {
            assert!(
                primary
                    .detail
                    .contains("retained authority requires recovery"),
                "{primary:?}"
            );
        }
        assert!(!fixture.desktop.0.migration.drained());
        assert!(
            fixture
                .public(api::Request::Discard {
                    guard: guard.clone()
                })
                .is_err()
        );
        assert!(
            fixture
                .public(api::Request::ResultPage {
                    guard: guard.clone(),
                    page: U64(0),
                    offset: U64(0),
                    maximum_bytes: U64(100)
                })
                .is_err()
        );
        let retained = fixture.pool.used();
        assert!(retained > baseline);
        let pids = fs::read_to_string(&fixture.pids)?;
        assert_eq!(
            pids.matches("--lightroom-migration-worker").count(),
            usize::from(writer)
        );
        if writer {
            assert!(
                pids.contains("source-reader"),
                "real Source descendants existed before Catalog write"
            );
        }
        for line in pids
            .lines()
            .filter(|line| !line.ends_with("--catalog-desktop-worker"))
        {
            let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "descendant still live before C release: {line}"
            );
            println!("LM_ACK_DESCENDANT_REAP {line}");
        }
        assert_eq!(
            unsafe { libc::kill(cpid as i32, 0) },
            0,
            "recovery uses same live C"
        );
        let pending_remote = remote(&fixture, &guard)?;
        assert_eq!(pending_remote.sequence, held_remote.sequence);
        assert_eq!(pending_remote.request_digest, held_remote.request_digest);
        assert_eq!(pending_remote.phase, held_remote.phase);
        assert_eq!(
            fs::read_to_string(&fixture.receipts)?,
            receipts_before,
            "C custody retained after descendant/local waiter joins"
        );
        fixture
            .desktop
            .0
            .migration
            .faults
            .lock()
            .unwrap()
            .pause_acquire_recovery = false;
        fixture.public(api::Request::RetryDrain {
            guard: guard.clone(),
        })?;
        let terminal = loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if status.phase == api::Phase::Failed {
                break status;
            }
            // A disconnected original slot consumes one explicit failed retry;
            // the next retry uses the durable exact identity.
            if status.phase == api::Phase::DrainPending {
                fixture.public(api::Request::RetryDrain {
                    guard: guard.clone(),
                })?;
            }
            ensure!(
                Instant::now() < deadline,
                "exact acquisition recovery did not drain: {status:?}"
            );
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(terminal.failure.as_ref().unwrap().detail, primary.detail);
        assert!(terminal.failure.as_ref().unwrap().outcome_unknown);
        assert!(
            fixture.desktop.0.migration.drained(),
            "G JoinHandle consumed"
        );
        assert!(fixture.pool.used() < retained);
        let receipts = fs::read_to_string(&fixture.receipts)?;
        assert_eq!(
            receipts.matches("target-installed").count(),
            1,
            "recovery did not reacquire"
        );
        assert_eq!(receipts.matches("target-drained").count(), 1);
        for event in [
            "write-installed",
            "permit-held",
            "permit-released",
            "permit-joined",
        ] {
            assert_eq!(
                receipts.matches(event).count(),
                if writer { 2 } else { 0 },
                "exact C {event}"
            );
        }
        for line in receipts.lines() {
            println!("LM_ACK_CUSTODY {line}");
        }
        fixture.public(api::Request::Discard { guard })?;
        if let Some(owner) = shutdown_owner {
            let _first_attempt = owner
                .join()
                .map_err(|_| anyhow::anyhow!("shutdown thread panicked"))?;
            fixture.desktop.try_shutdown()?;
            assert_eq!(
                fixture.desktop.0.shared.state.lock().unwrap().child_exit,
                Some(0),
                "normal checked shutdown; C reap was not the recovery shortcut"
            );
            fixture.finish()
        } else {
            assert_eq!(unsafe { libc::kill(cpid as i32, 0) }, 0);
            *fixture.desktop.0.migration.faults.lock().unwrap() = Faults::default();
            let (_, reused) = fixture.operate(
                &destination,
                terminal.catalog,
                api::Operation::PrepareSupplements,
                &reuse_documents,
                "same-live-c-after-ack-loss",
            )?;
            assert_eq!(fixture.desktop.0.pid, cpid);
            fixture.close(reused.catalog.context("reused catalog token")?)?;
            fixture.finish()
        }
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_lost_target_ack_cancel_retry_recovers_same_live_c() -> Result<()> {
        acquisition_fault(false, AcquisitionFault::Lost, false)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_lost_writer_ack_close_retry_joins_and_recovers_same_live_c() -> Result<()> {
        acquisition_fault(true, AcquisitionFault::Lost, false)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_disconnected_target_ack_cancel_retry_recovers_same_live_c() -> Result<()> {
        acquisition_fault(false, AcquisitionFault::Disconnected, false)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_disconnected_writer_ack_cancel_retry_recovers_same_live_c() -> Result<()> {
        acquisition_fault(true, AcquisitionFault::Disconnected, false)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_post_action_target_reply_overflow_remains_unknown_until_drain() -> Result<()>
    {
        acquisition_fault(false, AcquisitionFault::Overflow, false)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_post_action_writer_reply_overflow_remains_unknown_until_release()
    -> Result<()> {
        acquisition_fault(true, AcquisitionFault::Overflow, false)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_lost_target_ack_shutdown_uses_lookup_before_normal_c_exit() -> Result<()> {
        acquisition_fault(false, AcquisitionFault::Lost, true)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_lost_writer_ack_shutdown_uses_lookup_before_normal_c_exit() -> Result<()> {
        acquisition_fault(true, AcquisitionFault::Lost, true)
    }

    #[derive(Clone, Copy)]
    enum EarlyAdmission {
        TargetCancel,
        TargetDeadline,
        TargetEnqueued,
        WaiterSpawn,
        BeforeExternal,
        WriterEnqueued,
    }
    fn early_admission(case: EarlyAdmission) -> Result<()> {
        let fixture = Fixture::new()?;
        let baseline = fixture.pool.used();
        let cpid = fixture.desktop.0.pid;
        let supplement = crate::catalog_migration::supplements::tests::Fixture::new()?;
        let destination = fixture._temp.path().canonicalize()?.join("target");
        let documents = vec![(
            api::InputRole::SupplementRequests,
            serde_json::to_string(&vec![supplement.request])?,
        )];
        let target = matches!(
            case,
            EarlyAdmission::TargetCancel
                | EarlyAdmission::TargetDeadline
                | EarlyAdmission::TargetEnqueued
        );
        {
            let mut faults = fixture.desktop.0.migration.faults.lock().unwrap();
            faults.pause_drain = true;
            match case {
                EarlyAdmission::TargetCancel | EarlyAdmission::TargetDeadline => {
                    faults.pause_target = true
                }
                EarlyAdmission::TargetEnqueued => faults.enqueue_target = true,
                EarlyAdmission::WaiterSpawn => faults.fail_waiter_spawn = true,
                EarlyAdmission::BeforeExternal => faults.pause_external = true,
                EarlyAdmission::WriterEnqueued => faults.enqueue_writer = true,
            }
        }
        let mut h = header(&destination, api::Operation::PrepareSupplements, &documents);
        if matches!(case, EarlyAdmission::TargetDeadline) {
            h.timeout_ms = U64(20);
        }
        let guard = snap(fixture.public(api::Request::Begin {
            operation: "early-admission".into(),
            header: h,
        })?)?
        .guard;
        upload(&fixture.desktop.0.migration, &guard, &documents)?;
        fixture.public(api::Request::Act {
            guard: guard.clone(),
        })?;
        let deadline = Instant::now() + Duration::from_secs(60);
        if !matches!(case, EarlyAdmission::WaiterSpawn) {
            loop {
                let reached = {
                    let faults = fixture.desktop.0.migration.faults.lock().unwrap();
                    match case {
                        EarlyAdmission::TargetCancel | EarlyAdmission::TargetDeadline => {
                            faults.target_before_submit
                        }
                        EarlyAdmission::TargetEnqueued | EarlyAdmission::WriterEnqueued => {
                            faults.enqueued
                        }
                        EarlyAdmission::BeforeExternal => faults.external_pending,
                        EarlyAdmission::WaiterSpawn => unreachable!(),
                    }
                };
                if reached {
                    break;
                }
                ensure!(Instant::now() < deadline, "early admission seam deadline");
                thread::sleep(Duration::from_millis(2));
            }
            if matches!(case, EarlyAdmission::TargetDeadline) {
                thread::sleep(Duration::from_millis(30));
            } else {
                fixture.public(api::Request::Cancel {
                    guard: guard.clone(),
                })?;
            }
            fixture
                .desktop
                .0
                .migration
                .faults
                .lock()
                .unwrap()
                .pause_target = false;
        }
        let pending = loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if status.phase == api::Phase::DrainPending
                && status.failure.is_some()
                && fixture
                    .desktop
                    .0
                    .migration
                    .faults
                    .lock()
                    .unwrap()
                    .drain_paused
            {
                break status;
            }
            ensure!(
                Instant::now() < deadline,
                "early admission pending: {status:?}"
            );
            thread::sleep(Duration::from_millis(2));
        };
        let primary = pending
            .failure
            .context("primary failure retained during drain")?;
        if matches!(case, EarlyAdmission::WaiterSpawn) {
            assert!(
                primary
                    .detail
                    .contains("injected migration admission thread spawn failure")
            );
        } else if matches!(case, EarlyAdmission::TargetDeadline) {
            assert!(primary.detail.contains("acknowledgement remains pending"));
        } else {
            assert!(
                matches!(primary.code, ErrorCode::Canceled),
                "typed cancellation: {primary:?}"
            );
        }
        let held = fixture.pool.used();
        assert!(held > baseline);
        assert!(!fixture.desktop.0.migration.drained());
        assert!(
            fixture
                .public(api::Request::Discard {
                    guard: guard.clone()
                })
                .is_err()
        );
        assert_eq!(fixture.pool.used(), held);
        assert!(
            !destination.exists(),
            "no C writer or Bootstrap on refused admission"
        );
        fixture
            .desktop
            .0
            .migration
            .faults
            .lock()
            .unwrap()
            .pause_drain = false;
        fixture.public(api::Request::RetryDrain {
            guard: guard.clone(),
        })?;
        let terminal = loop {
            let status = snap(fixture.public(api::Request::Status {
                guard: guard.clone(),
            })?)?;
            if status.phase == api::Phase::Failed {
                break status;
            }
            ensure!(
                Instant::now() < deadline,
                "early admission terminal: {status:?}"
            );
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(terminal.failure.as_ref().unwrap().detail, primary.detail);
        assert!(
            fixture.desktop.0.migration.drained(),
            "checked local waiter and G join"
        );
        assert!(fixture.pool.used() < held && fixture.pool.used() > baseline);
        let pids = fs::read_to_string(&fixture.pids)?;
        assert_eq!(pids.lines().count(), if target { 1 } else { 2 });
        assert!(!pids.contains("source-reader"));
        for line in pids
            .lines()
            .filter(|line| !line.ends_with("--catalog-desktop-worker"))
        {
            let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        }
        assert_eq!(unsafe { libc::kill(cpid as i32, 0) }, 0);
        let response = relay::Client::new(&fixture.desktop.0.shared)
            .submit(relay::Request {
                guard: guard.clone(),
                action: relay::Action::Status,
            })?
            .receiver
            .recv_timeout(Duration::from_secs(5))?;
        let relay::Reply::Ok(remote) = response else {
            anyhow::bail!("live C recovery status")
        };
        assert!(matches!(
            remote.phase,
            relay::Phase::Unknown | relay::Phase::Drained
        ));
        assert!(
            remote.sequence.is_none(),
            "no remote writer attempt was installed"
        );
        fixture.public(api::Request::Discard { guard })?;
        assert_eq!(
            fixture.pool.used(),
            baseline,
            "operation and independent failure discarded"
        );
        *fixture.desktop.0.migration.faults.lock().unwrap() = Faults::default();
        let (_, status) = fixture.operate(
            &destination,
            None,
            api::Operation::PrepareSupplements,
            &documents,
            "same-live-c-reuse",
        )?;
        assert_eq!(fixture.desktop.0.pid, cpid);
        fixture.close(status.catalog.context("reused live C catalog")?)?;
        fixture.finish()
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_cancel_before_target_submit_retires_and_reuses_live_c() -> Result<()> {
        early_admission(EarlyAdmission::TargetCancel)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_deadline_before_target_submit_retires_and_reuses_live_c() -> Result<()> {
        early_admission(EarlyAdmission::TargetDeadline)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_cancel_enqueued_target_resolves_terminal_ack_before_reuse() -> Result<()> {
        early_admission(EarlyAdmission::TargetEnqueued)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_waiter_spawn_failure_retires_without_remote_attempt() -> Result<()> {
        early_admission(EarlyAdmission::WaiterSpawn)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_cancel_before_external_acquire_joins_before_retirement() -> Result<()> {
        early_admission(EarlyAdmission::BeforeExternal)
    }
    #[test]
    #[ignore = "requires exact freshly built PHOTOCATALOG_TEST_EXECUTABLE"]
    fn lm_facade_actual_cancel_enqueued_writer_resolves_terminal_ack_before_reuse() -> Result<()> {
        early_admission(EarlyAdmission::WriterEnqueued)
    }
}
