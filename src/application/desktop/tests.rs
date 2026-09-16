use super::*;
use std::io::Cursor;
use wire::{BinaryHeader, Frame};

pub(super) fn shared(cap: usize) -> Arc<Shared> {
    Arc::new(Shared {
        session: [9; 16],
        limits: Limits {
            binary_bytes: cap,
            ..Limits::default()
        },
        state: Mutex::new(State {
            phase: TransportPhase::Ready,
            message: None,
            unknown: false,
            next: 1,
            pending: HashMap::new(),
            control: VecDeque::new(),
            data: VecDeque::new(),
            ready: true,
            stopping: false,
            shutdown_attempt: 0,
            shutdown_sent: 0,
            drain_error: None,
            reaped: false,
            child_finished: false,
            local_verified: false,
            filesystem_verified: true,
            child_exit: None,
            catalog_retiring: false,
            catalog_epoch: 0,
            backup_admitting: false,
            close_admitting: false,
        }),
        wake: Condvar::new(),
        binary: Arc::new(AtomicUsize::new(0)),
        filesystem: None,
        backup: None,
        metadata: Default::default(),
        migration_stop: Mutex::new(None),
        fixture: Mutex::new(None),
    })
}

#[test]
fn managed_backup_routes_share_pre_effect_public_byte_boundary() {
    let path = || NativePath::from_path(std::path::Path::new("/fixture/backup"));
    let requests = [
        Request::BackupCreate {
            catalog: "catalog".into(),
            bundle: path(),
        },
        Request::BackupInspect { bundle: path() },
        Request::BackupRestore {
            bundle: path(),
            destination: NativePath::from_path(std::path::Path::new("/fixture/restore")),
        },
        Request::BackupStatus,
        Request::BackupCancel {
            operation: "operation".into(),
        },
        Request::Close {
            catalog: "catalog".into(),
        },
    ];
    for request in requests {
        assert!(matches!(
            validate_public_request(&request, 0).unwrap_err().code,
            ErrorCode::ResourceLimit
        ));
    }
}

#[test]
fn drain_attempts_retain_unknown_pending_and_ignore_stale_failure() {
    let state = shared(8);
    let (_, rx) = bytes_pending(&state, 1);
    state.stop();
    let first = process::next_outgoing(&state, &mut None).unwrap();
    assert_eq!((first.kind, first.id), (Kind::Shutdown, 1));
    state.drain_failed(1, "native wait failed".into()).unwrap();
    assert_eq!(state.state.lock().unwrap().phase, TransportPhase::Draining);
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(!state.state.lock().unwrap().reaped);
    state.stop(); // Explicit retry, same owner and pending receipt.
    let retry = process::next_outgoing(&state, &mut None).unwrap();
    assert_eq!((retry.kind, retry.id), (Kind::Shutdown, 2));
    state.drain_failed(1, "late old failure".into()).unwrap();
    assert!(state.state.lock().unwrap().drain_error.is_none());
    assert!(state.drain_failed(3, "unowned future".into()).is_err());
    state.complete_failure();
    assert!(rx.recv().unwrap().is_err());
    assert!(state.state.lock().unwrap().unknown);
}

#[test]
fn close_retires_only_matching_catalog_binary_and_ack_survives_drain() {
    let state = shared(8);
    let (_, old) = bytes_pending(&state, 1);
    let (new_cancel, _) = bytes_pending(&state, 2);
    if let Delivery::Bytes { request, .. } = &mut state
        .state
        .lock()
        .unwrap()
        .pending
        .get_mut(&2)
        .unwrap()
        .delivery
    {
        request.catalog = "catalog-B".into();
    }
    retire_bytes(&state.state.lock().unwrap(), "catalog-A");
    assert!(!new_cancel.is_canceled());
    let mut bytes = process::BinaryAssembly::new(1, header(1, b"old"), &state).unwrap();
    bytes.push(chunk(1, b"old".to_vec())).unwrap();
    state.stop();
    bytes.finish(&state).unwrap();
    assert!(old.recv().unwrap().is_err());
    assert_eq!(state.binary.load(Ordering::Acquire), 0);
    assert_eq!(
        process::next_outgoing(&state, &mut None).unwrap().kind,
        Kind::Ack
    );
    assert_eq!(
        process::next_outgoing(&state, &mut None).unwrap().kind,
        Kind::Shutdown
    );
}
fn bytes_pending(shared: &Shared, id: u64) -> (Cancellation, mpsc::Receiver<Result<PreviewBytes>>) {
    let (reply, rx) = mpsc::sync_channel(1);
    let cancel = Cancellation::default();
    shared.state.lock().unwrap().pending.insert(
        id,
        Entry {
            delivery: Delivery::Bytes {
                request: BytesRequest {
                    catalog: "catalog-A".into(),
                    ticket: format!("ticket-{id}"),
                    foreground: true,
                },
                reply,
            },
            cancel: cancel.clone(),
            sent_cancel: false,
            control: false,
        },
    );
    (cancel, rx)
}
fn header(id: u64, bytes: &[u8]) -> BinaryHeader {
    BinaryHeader {
        catalog: "catalog-A".into(),
        ticket: format!("ticket-{id}"),
        mime: "image/png".into(),
        bytes: bytes.len(),
        digest: blake3::hash(bytes).to_hex().to_string(),
    }
}
fn chunk(id: u64, data: Vec<u8>) -> Frame {
    Frame {
        kind: Kind::BinaryChunk,
        session: [9; 16],
        id,
        offset: 0,
        total: data.len(),
        payload: data,
    }
}
#[test]
fn frame_header_rejects_future_version_and_size_before_payload_read() {
    let mut bytes = vec![];
    Message::new(Kind::Command, 1, b"{}".to_vec())
        .write([9; 16], &mut bytes)
        .unwrap();
    let mut future = bytes.clone();
    future[4] = future[4].checked_add(1).unwrap();
    let mut input = Cursor::new(future);
    assert!(Frame::read(&mut input).is_err());
    assert_eq!(input.position(), 48);
    let mut oversized = bytes.clone();
    oversized[44..48].copy_from_slice(&((wire::CHUNK + 1) as u32).to_le_bytes());
    let mut input = Cursor::new(oversized);
    assert!(Frame::read(&mut input).is_err());
    assert_eq!(input.position(), 48);
    for cut in 1..bytes.len() {
        assert!(Frame::read(&mut Cursor::new(&bytes[..cut])).is_err());
    }
    let f = Frame::read(&mut Cursor::new(bytes)).unwrap().unwrap();
    assert_eq!(f.payload, b"{}");
}
#[test]
fn assembly_caps_and_continuity_refuse_unknown_or_repeated_frames() {
    let mut m = Message::new(Kind::Command, 1, vec![3; wire::CHUNK + 3]);
    let a = m.next([9; 16]);
    assert!(wire::Assembly::start(&a, wire::CHUNK).is_err());
    let mut assembly = wire::Assembly::start(&a, wire::CHUNK + 3).unwrap();
    assert!(!assembly.push(a).unwrap());
    let mut end = m.next([9; 16]);
    end.offset = 0;
    assert!(assembly.push(end).is_err());
    let mut seen = wire::Seen::default();
    seen.insert(2, 4).unwrap();
    seen.insert(1, 4).unwrap();
    seen.insert(4, 4).unwrap();
    seen.insert(3, 4).unwrap();
    assert!(seen.insert(2, 4).is_err());
    for id in 5..10000 {
        seen.insert(id, 4).unwrap();
    }
    assert!(seen.contains(9999));
    assert!(seen.insert(0, 4).is_err());
}
#[test]
fn binary_exact_cap_ack_and_renderer_release_are_separate_owners() {
    let shared = shared(8);
    let (_, rx) = bytes_pending(&shared, 1);
    let bytes = b"raw\0\xffBOM";
    let mut assembly = process::BinaryAssembly::new(1, header(1, bytes), &shared).unwrap();
    assert_eq!(shared.binary.load(Ordering::Acquire), 8);
    bytes_pending(&shared, 2);
    assert!(process::BinaryAssembly::new(2, header(2, b"x"), &shared).is_err());
    assembly.push(chunk(1, bytes.to_vec())).unwrap();
    assembly.finish(&shared).unwrap();
    let value = rx.recv().unwrap().unwrap();
    assert_eq!(value.bytes(), bytes);
    assert_eq!(
        shared.state.lock().unwrap().control.front().unwrap().kind,
        Kind::Ack
    );
    assert_eq!(shared.binary.load(Ordering::Acquire), 8);
    drop(value);
    assert_eq!(shared.binary.load(Ordering::Acquire), 0);
}
#[test]
fn canceled_partial_bad_checksum_and_stale_binary_do_not_leak_or_replace() {
    let shared = shared(8);
    let (cancel, rx) = bytes_pending(&shared, 1);
    let mut assembly = process::BinaryAssembly::new(1, header(1, b"hello"), &shared).unwrap();
    cancel.cancel();
    assembly.push(chunk(1, b"hello".to_vec())).unwrap();
    assembly.finish(&shared).unwrap();
    assert!(rx.recv().unwrap().is_err());
    assert_eq!(shared.binary.load(Ordering::Acquire), 0);
    bytes_pending(&shared, 2);
    let mut wrong = header(2, b"x");
    wrong.catalog = "catalog-B".into();
    assert!(process::BinaryAssembly::new(2, wrong, &shared).is_err());
    let mut bad = process::BinaryAssembly::new(2, header(2, b"x"), &shared).unwrap();
    bad.push(chunk(2, b"y".to_vec())).unwrap();
    assert!(bad.finish(&shared).is_err());
    assert_eq!(shared.binary.load(Ordering::Acquire), 0);
    assert!(shared.state.lock().unwrap().pending.contains_key(&2));
    let partial = process::BinaryAssembly::new(2, header(2, b"12345678"), &shared).unwrap();
    drop(partial);
    assert_eq!(shared.binary.load(Ordering::Acquire), 0);
    let mut too_big = header(2, b"123456789");
    too_big.ticket = "ticket-2".into();
    assert!(process::BinaryAssembly::new(2, too_big, &shared).is_err());
    assert_eq!(shared.binary.load(Ordering::Acquire), 0);
}
#[test]
fn outstanding_data_does_not_hide_control_or_early_cancellation() {
    let shared = shared(8);
    let (cancel, _) = bytes_pending(&shared, 1);
    shared.state.lock().unwrap().data.push_back(Message::new(
        Kind::Command,
        1,
        vec![1; wire::CHUNK * 2],
    ));
    let mut active = None;
    let mut data = process::next_outgoing(&shared, &mut active).unwrap();
    data.next(shared.session);
    active = Some(data);
    shared.state.lock().unwrap().control.push_back(Message::new(
        Kind::Command,
        2,
        b"status".to_vec(),
    ));
    cancel.cancel();
    assert_eq!(
        process::next_outgoing(&shared, &mut active).unwrap().kind,
        Kind::Cancel
    );
    assert_eq!(process::next_outgoing(&shared, &mut active).unwrap().id, 2);
    assert_eq!(
        process::next_outgoing(&shared, &mut active).unwrap().offset,
        wire::CHUNK
    );
    shared.stop();
    assert_eq!(
        process::next_outgoing(&shared, &mut active).unwrap().kind,
        Kind::Shutdown
    );
    shared.state.lock().unwrap().reaped = true;
    assert!(process::next_outgoing(&shared, &mut active).is_none());
}
#[test]
fn config_and_wire_families_preserve_exact_native_and_decimal_authority() {
    let key = crate::catalog_edits::VariantKey {
        asset_id: "opaque-source".into(),
        variant_id: "copy-9007199254740993".into(),
    };
    let root = NativePath::from_path(std::path::Path::new("/synthetic"));
    let cases = vec![
        Request::OpenExisting { path: root.clone() },
        Request::ImportResume {
            catalog: "C".into(),
            source: root.clone(),
        },
        Request::BackupInspect {
            bundle: root.clone(),
        },
        Request::BackupRestore {
            bundle: root.clone(),
            destination: root.clone(),
        },
        Request::Metadata {
            catalog: "C".into(),
            request: Box::new(super::super::metadata::Request::Identity { key: key.clone() }),
        },
        Request::Organization {
            catalog: "C".into(),
            request: Box::new(super::super::organization::Request::DeleteCollection {
                id: "K".into(),
                expected_revision: super::super::I64(9007199254740993),
            }),
        },
        Request::EditCopy {
            catalog: "C".into(),
            request: Box::new(super::super::copy::Request::Status { operation: None }),
        },
        Request::Relink {
            catalog: "C".into(),
            request: Box::new(super::super::relink::Request::Status { operation: None }),
        },
        Request::Export {
            catalog: "C".into(),
            request: Box::new(super::super::exports::Request::Status { operation: None }),
        },
        Request::Lightroom {
            request: Box::new(super::super::lightroom_bridge::Request::Options {}),
        },
        Request::Undo {
            catalog: "C".into(),
            key,
            expected_revision: super::super::I64(i64::MAX),
        },
        Request::CancelPreview {
            catalog: "C".into(),
            ticket: "T".into(),
        },
        Request::Status,
    ];
    for case in cases {
        let bytes = serde_json::to_vec(&case).unwrap();
        let decoded: Request = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
        assert_eq!(
            local_route(&decoded),
            matches!(decoded, Request::Lightroom { .. })
        );
    }
    let config = Config {
        worker_executable: std::env::current_exe().unwrap(),
        cache_root: None,
        original_roots: vec![],
        preview_policy: Default::default(),
        preview_limits: Default::default(),
        limits: Limits::default(),
        import_checkpoint: None,
    };
    let bytes = serde_json::to_vec(&wire::ConfigWire::from_config(&config)).unwrap();
    let recovered = serde_json::from_slice::<wire::ConfigWire>(&bytes)
        .unwrap()
        .into_config()
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&wire::ConfigWire::from_config(&recovered)).unwrap(),
        bytes
    );
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut native = config.clone();
        native.original_roots =
            vec![std::ffi::OsString::from_vec(b"/synthetic/\xff".to_vec()).into()];
        let bytes = serde_json::to_vec(&wire::ConfigWire::from_config(&native)).unwrap();
        let recovered = serde_json::from_slice::<wire::ConfigWire>(&bytes)
            .unwrap()
            .into_config()
            .unwrap();
        assert_eq!(native.original_roots, recovered.original_roots);
    }
}

#[test]
fn managed_catalog_retirement_requires_identity_reap_join_and_g_drain() {
    let shared = shared(8);
    let mut state = shared.state.lock().unwrap();
    state.child_exit = None; // An abnormal exit has no success proof of its own.
    for paired in [false, true] {
        for ready in [false, true] {
            for reaped in [false, true] {
                for joined in [false, true] {
                    for migration in [false, true] {
                        for backup in [false, true] {
                            for control in [false, true] {
                                state.ready = ready;
                                state.reaped = reaped;
                                state.child_finished = joined;
                                assert_eq!(
                                    managed_catalog_retired(
                                        &state, paired, migration, backup, control,
                                    ),
                                    paired
                                        && ready
                                        && reaped
                                        && joined
                                        && migration
                                        && backup
                                        && control
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
