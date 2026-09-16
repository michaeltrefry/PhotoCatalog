use super::*;
use crate::{
    catalog_session::{BootstrapMode, PhysicalObjectId, PinnedDatabase},
    storage_volume::NativePath,
};
fn request() -> PrepareCatalog {
    PrepareCatalog {
        operation: U64(u64::MAX - 3),
        session: LeaseId::new(),
        mode: BootstrapMode::DesktopCreate,
        root: NativePath::from_path(&std::env::temp_dir().join("relay-catalog")),
        manifest_root: NativePath::from_path(&std::env::temp_dir().join("relay-cache")),
        import_source: None,
    }
}
fn physical_object(index: u64) -> PhysicalObjectId {
    #[cfg(unix)]
    {
        PhysicalObjectId::Unix {
            device: U64(u64::MAX),
            inode: U64(index),
        }
    }
    #[cfg(windows)]
    {
        PhysicalObjectId::Windows {
            volume_serial: U64(u32::MAX as u64),
            file_index: U64(index),
        }
    }
}
fn bootstrap(r: &PrepareCatalog, b: &Binding) -> CatalogBootstrap {
    let physical = physical_object(u64::MAX - 1);
    CatalogBootstrap {
        version: 1,
        operation: r.operation,
        epoch: b.epoch.clone(),
        token: LeaseId::new(),
        session: r.session.clone(),
        canonical_root: r.root.clone(),
        root_physical: physical,
        catalog: PinnedDatabase {
            path: NativePath::from_path(&r.root.to_path().unwrap().join("catalog.sqlite3")),
            physical,
            created: true,
        },
        manifest: PinnedDatabase {
            path: NativePath::from_path(
                &r.manifest_root.to_path().unwrap().join("previews.sqlite3"),
            ),
            physical: physical_object(u64::MAX - 2),
            created: true,
        },
    }
}

#[test]
fn storage_observation_reply_cannot_cross_root_or_action() -> Result<()> {
    use crate::catalog_session::storage;
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let r = request();
    let request = storage::Request {
        root: bootstrap(&r, &binding).root_capability(),
        action: storage::Action::Object(r.root),
    };
    let reply = storage::Reply {
        request: request.clone(),
        value: storage::Value::Object {
            device: U64(u64::MAX),
            object: U64(u64::MAX - 1),
        },
    };
    reply.validate(&request)?;
    let mut other = request.clone();
    other.root.token = LeaseId::new();
    assert!(reply.validate(&other).is_err());
    other = request.clone();
    other.action = storage::Action::Mounts;
    assert!(reply.validate(&other).is_err());
    let call = Call::Storage(Box::new(request.clone()));
    let value = Value::Storage(reply);
    validate_reply(&call, &value, &binding)?;
    Ok(())
}
#[test]
fn exact_encoding_and_epoch_bounds_precede_reply_adoption() -> Result<()> {
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let r = request();
    let packet = Packet {
        binding: binding.clone(),
        body: Body::Call {
            id: U64(u64::MAX),
            call: Call::Prepare(r.clone()),
        },
    };
    let encoded = encode(&packet, BYTES)?;
    assert_eq!(encoded.capacity(), encoded.len());
    let Body::Call {
        id,
        call: Call::Prepare(decoded),
    } = decode(&binding, &encoded, Lane::Data)?
    else {
        panic!("wire shape")
    };
    assert_eq!(id.0, u64::MAX);
    assert_eq!(decoded.operation, r.operation);
    assert_eq!(decoded.root, r.root);
    assert!(encode(&packet, encoded.len() - 1).is_err());
    assert!(
        decode(
            &Binding {
                nonce: LeaseId::new(),
                epoch: binding.epoch.clone()
            },
            &encoded,
            Lane::Data
        )
        .is_err()
    );
    assert!(decode(&binding, &encoded, Lane::Control).is_err());
    Ok(())
}

#[test]
fn lm_desktop_relay_identity_fact_roundtrips_private_f_lane_and_rejects_changed_binding()
-> Result<()> {
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let request = MigrationIdentityRequest {
        root: bootstrap(&request(), &binding).root_capability(),
        lock: Some(physical_object(u64::MAX - 7)),
    };
    let call = Call::MigrationIdentity(Box::new(request.clone()));
    call.validate()?;
    assert!(call.cancellable());
    let reply = MigrationIdentityReply {
        root: request.root.clone(),
        lock: request.lock,
    };
    validate_reply(&call, &Value::MigrationIdentity(reply.clone()), &binding)?;
    let packet = Packet {
        binding: binding.clone(),
        body: Body::Call {
            id: U64(9),
            call: call.clone(),
        },
    };
    let bytes = encode(&packet, BYTES)?;
    assert!(encode(&packet, bytes.len() - 1).is_err());
    let Body::Call {
        call: Call::MigrationIdentity(decoded),
        ..
    } = decode(&binding, &bytes, Lane::Data)?
    else {
        panic!("migration identity wire shape changed")
    };
    assert_eq!(*decoded, request);
    let mut changed_binding = binding.clone();
    changed_binding.epoch = LeaseId::new();
    assert!(decode(&changed_binding, &bytes, Lane::Data).is_err());
    assert!(decode(&binding, &bytes, Lane::Control).is_err());
    let mut changed = reply.clone();
    changed.lock = None;
    assert!(validate_reply(&call, &Value::MigrationIdentity(changed), &binding).is_err());
    let mut changed = reply;
    changed.root.catalog_physical = physical_object(1);
    assert!(validate_reply(&call, &Value::MigrationIdentity(changed), &binding).is_err());
    Ok(())
}

#[test]
fn export_directory_reply_requires_exact_root_and_requested_path() -> Result<()> {
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let catalog = request();
    let root = bootstrap(&catalog, &binding).root_capability();
    let request = PrepareExportDirectory {
        root: root.clone(),
        directory: NativePath::from_path(&std::env::temp_dir().join("selected-output")),
    };
    let call = Call::PrepareExportDirectory(Box::new(request.clone()));
    call.validate()?;
    let value = Value::ExportDirectory(PreparedExportDirectory {
        root,
        requested: request.directory.clone(),
        directory: NativePath::from_path(&std::env::temp_dir()),
    });
    validate_reply(&call, &value, &binding)?;
    let packet = Packet {
        binding: binding.clone(),
        body: Body::Call {
            id: U64(9),
            call: call.clone(),
        },
    };
    let bytes = encode(&packet, BYTES)?;
    assert!(matches!(
        decode(&binding, &bytes, Lane::Data)?,
        Body::Call {
            id: U64(9),
            call: Call::PrepareExportDirectory(_)
        }
    ));
    let Value::ExportDirectory(mut foreign) = value else {
        unreachable!()
    };
    foreign.requested = NativePath::from_path(&std::env::temp_dir().join("other-output"));
    assert!(validate_reply(&call, &Value::ExportDirectory(foreign), &binding).is_err());
    Ok(())
}

#[test]
fn export_snapshot_and_alias_fact_replies_require_exact_provenance() -> Result<()> {
    use crate::catalog_session::{
        ExportAliasFactKind, ExportAliasFactReply, ExportAliasFactRequest, ExportAliasFactValue,
        ExportDestinationSnapshotReply, ExportDestinationSnapshotRequest, ExportObjectKey,
    };
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let root = bootstrap(&request(), &binding).root_capability();
    let destination = NativePath::from_path(&std::env::temp_dir().join("selected.png"));
    let snapshot_request = ExportDestinationSnapshotRequest {
        root: root.clone(),
        destination: destination.clone(),
        max_existing_bytes: U64(1024),
    };
    let snapshot_call = Call::ExportDestinationSnapshot(Box::new(snapshot_request.clone()));
    let snapshot = ExportDestinationSnapshotReply {
        root: root.clone(),
        requested: destination.clone(),
        snapshot: crate::metadata_export::DestinationSnapshot {
            version: 2,
            operation: uuid::Uuid::new_v4().to_string(),
            destination: destination.to_path()?,
            expected: None,
            max_existing_bytes: 1024,
        },
    };
    validate_reply(
        &snapshot_call,
        &Value::ExportDestinationSnapshot(snapshot.clone()),
        &binding,
    )?;
    let encoded = encode(
        &Packet {
            binding: binding.clone(),
            body: Body::Call {
                id: U64(13),
                call: snapshot_call.clone(),
            },
        },
        BYTES,
    )?;
    assert!(matches!(
        decode(&binding, &encoded, Lane::Data)?,
        Body::Call {
            id: U64(13),
            call: Call::ExportDestinationSnapshot(_)
        }
    ));
    let mut foreign_snapshot = snapshot;
    foreign_snapshot.requested = NativePath::from_path(&std::env::temp_dir().join("other.png"));
    assert!(
        validate_reply(
            &snapshot_call,
            &Value::ExportDestinationSnapshot(foreign_snapshot),
            &binding,
        )
        .is_err()
    );

    let fact_request = ExportAliasFactRequest {
        root: root.clone(),
        path: destination,
        kind: ExportAliasFactKind::Destination,
    };
    let fact_call = Call::ExportAliasFact(Box::new(fact_request.clone()));
    let fact = ExportAliasFactReply {
        root,
        path: fact_request.path.clone(),
        kind: fact_request.kind,
        value: ExportAliasFactValue::File {
            object: ExportObjectKey {
                volume: U64(u64::MAX),
                object: u128::MAX.to_string(),
            },
            canonical: None,
        },
    };
    validate_reply(&fact_call, &Value::ExportAliasFact(fact.clone()), &binding)?;
    let encoded = encode(
        &Packet {
            binding: binding.clone(),
            body: Body::Reply {
                id: U64(14),
                outcome: Ok(Value::ExportAliasFact(fact.clone())),
            },
        },
        BYTES,
    )?;
    let Body::Reply {
        id: U64(14),
        outcome: Ok(decoded),
    } = decode(&binding, &encoded, Lane::Data)?
    else {
        panic!("alias fact reply")
    };
    validate_reply(&fact_call, &decoded, &binding)?;
    let mut wrong_kind = fact;
    wrong_kind.kind = ExportAliasFactKind::File;
    assert!(validate_reply(&fact_call, &Value::ExportAliasFact(wrong_kind), &binding).is_err());
    Ok(())
}

#[test]
fn export_publication_relay_requires_exact_step_authority_and_bounded_detail() -> Result<()> {
    use crate::catalog_session::{
        ExportPublicationAction, ExportPublicationMode, ExportPublicationRequest,
        ExportPublicationSource, ExportPublicationValue,
    };

    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let root = bootstrap(&request(), &binding).root_capability();
    let seal = crate::metadata_export::SealedPhotoExport {
        version: 2,
        snapshot: crate::metadata_export::DestinationSnapshot {
            version: 2,
            operation: uuid::Uuid::new_v4().to_string(),
            destination: std::env::temp_dir().join("publication-relay.jpg"),
            expected: None,
            max_existing_bytes: 4096,
        },
        authority_digest: "ab".repeat(32),
        max_payload_bytes: 4096,
        payload: crate::metadata_export::FileRevision {
            bytes: 7,
            digest: "cd".repeat(32),
            modified_ns: 11,
            identity: (12, 13),
        },
    };
    let request = ExportPublicationRequest {
        root: root.clone(),
        transfer: LeaseId::new(),
        step: U64(0),
        mode: ExportPublicationMode::Publish,
        source: ExportPublicationSource::Sealed(seal.clone()),
        action: ExportPublicationAction::Begin,
    };
    let call = Call::ExportPublication(Box::new(request.clone()));
    call.validate()?;
    let reply = ExportPublicationReply {
        mode: request.mode,
        request_digest: request.digest()?,
        root,
        transfer: request.transfer.clone(),
        step: request.step,
        seal,
        value: ExportPublicationValue::Begun { installed: false },
        timings: Default::default(),
        hashed_bytes: U64(0),
    };
    validate_reply(&call, &Value::ExportPublication(reply.clone()), &binding)?;
    let encoded = encode(
        &Packet {
            binding: binding.clone(),
            body: Body::Reply {
                id: U64(15),
                outcome: Ok(Value::ExportPublication(reply.clone())),
            },
        },
        BYTES,
    )?;
    let Body::Reply {
        outcome: Ok(decoded),
        ..
    } = decode(&binding, &encoded, Lane::Data)?
    else {
        panic!("publication reply")
    };
    validate_reply(&call, &decoded, &binding)?;

    let mut wrong_mode = reply.clone();
    wrong_mode.mode = ExportPublicationMode::Restore;
    assert!(validate_reply(&call, &Value::ExportPublication(wrong_mode), &binding).is_err());
    let mut wrong_digest = reply.clone();
    wrong_digest.request_digest = "00".repeat(32);
    assert!(validate_reply(&call, &Value::ExportPublication(wrong_digest), &binding).is_err());
    let step = ExportPublicationRequest {
        step: U64(1),
        action: ExportPublicationAction::Capture,
        ..request.clone()
    };
    let mut failed = ExportPublicationReply {
        step: step.step,
        request_digest: step.digest()?,
        value: ExportPublicationValue::Failed(crate::filesystem_worker::wire::Failure::new(
            crate::filesystem_worker::wire::FailureKind::Rejected,
            "retained failure",
        )),
        ..reply.clone()
    };
    validate_reply(
        &Call::ExportPublication(Box::new(step.clone())),
        &Value::ExportPublication(failed.clone()),
        &binding,
    )?;
    if let ExportPublicationValue::Failed(failure) = &mut failed.value {
        failure.message = "x".repeat(crate::filesystem_worker::wire::ERROR_BYTES + 1);
    }
    assert!(
        validate_reply(
            &Call::ExportPublication(Box::new(step)),
            &Value::ExportPublication(failed),
            &binding
        )
        .is_err()
    );
    let mut foreign = reply;
    foreign.transfer = LeaseId::new();
    assert!(validate_reply(&call, &Value::ExportPublication(foreign), &binding).is_err());
    let mut oversized = request;
    oversized.step = U64(1);
    oversized.action = ExportPublicationAction::FailureReceipt {
        detail: "x".repeat(8193),
    };
    assert!(
        Call::ExportPublication(Box::new(oversized))
            .validate()
            .is_err()
    );
    Ok(())
}

#[test]
fn export_profile_chunk_uses_bounded_binary_and_exact_provenance() -> Result<()> {
    use crate::catalog_session::{
        EXPORT_PROFILE_BYTES, ExportProfileAction, ExportProfileReply, ExportProfileRequest,
        ExportProfileValue,
    };

    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let catalog = request();
    let root = bootstrap(&catalog, &binding).root_capability();
    let request = ExportProfileRequest {
        root: root.clone(),
        requested: NativePath::from_path(&std::env::temp_dir().join("selected.icc")),
        transfer: LeaseId::new(),
        step: U64(1),
        allowance: U64(EXPORT_PROFILE_BYTES as u64),
        action: ExportProfileAction::Read { offset: U64(7) },
    };
    let bytes = vec![0xa5; crate::catalog_session::preview_io::CHUNK_BYTES];
    let reply = ExportProfileReply {
        root,
        requested: request.requested.clone(),
        transfer: request.transfer.clone(),
        step: request.step,
        value: ExportProfileValue::Chunk {
            offset: U64(7),
            checksum: blake3::hash(&bytes).to_hex().to_string(),
            bytes: bytes.clone(),
        },
    };
    reply.validate(&request)?;
    let packet = Packet {
        binding: binding.clone(),
        body: Body::Reply {
            id: U64(21),
            outcome: Ok(Value::ExportProfile(reply.clone())),
        },
    };
    let encoded = encode_packet(&packet, BYTES)?;
    assert!(encoded.len() < bytes.len() + 2048);
    let Body::Reply {
        outcome: Ok(Value::ExportProfile(decoded)),
        ..
    } = decode(&binding, &encoded, Lane::Data)?
    else {
        panic!("profile reply")
    };
    decoded.validate(&request)?;
    assert_eq!(decoded.binary(), Some(bytes.as_slice()));

    let mut foreign = reply;
    foreign.requested = NativePath::from_path(&std::env::temp_dir().join("other.icc"));
    assert!(foreign.validate(&request).is_err());
    let mut corrupt = decoded;
    if let ExportProfileValue::Chunk { checksum, .. } = &mut corrupt.value {
        *checksum = "0".repeat(64);
    }
    assert!(corrupt.validate(&request).is_err());
    assert!(
        corrupt
            .set_binary(&vec![
                0;
                crate::catalog_session::preview_io::CHUNK_BYTES + 1
            ])
            .is_err()
    );
    Ok(())
}
#[test]
fn wrong_reply_is_not_acknowledged_and_cancel_does_not_erase_publication() -> Result<()> {
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let proxy = Proxy::new(binding.clone());
    let r = request();
    proxy.state.lock().unwrap().calls.push(ChildCall {
        id: 1,
        call: Call::Prepare(r.clone()),
        outcome: None,
        canceled: true,
        queried: false,
    });
    let wrong = encode(
        &Packet {
            binding: binding.clone(),
            body: Body::Reply {
                id: U64(1),
                outcome: Ok(Value::Unit),
            },
        },
        BYTES,
    )?;
    assert!(proxy.receive(&wrong, Lane::Data).is_err());
    assert!(proxy.next(Lane::Data).is_none());
    let expected = bootstrap(&r, &binding);
    let correct = encode(
        &Packet {
            binding: binding.clone(),
            body: Body::Reply {
                id: U64(1),
                outcome: Ok(Value::Bootstrap(expected.clone())),
            },
        },
        BYTES,
    )?;
    proxy.receive(&correct, Lane::Data)?;
    let state = proxy.state.lock().unwrap();
    assert!(matches!(
        state.calls[0].outcome,
        Some(Ok(Value::Bootstrap(_)))
    ));
    drop(state);
    let out = proxy.next(Lane::Control).unwrap();
    let Body::Control(Control::Ack { id, digest }) = decode(&binding, &out.bytes, Lane::Control)?
    else {
        panic!("ack")
    };
    assert_eq!(id, U64(1));
    assert_eq!(digest, blake3::hash(&correct).to_hex().as_str());
    Ok(())
}

#[test]
fn managed_import_cleanup_waits_for_one_of_two_bounded_relay_slots() -> Result<()> {
    use crate::catalog_session::import;

    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let proxy = Proxy::new(binding.clone());
    let root = bootstrap(&request(), &binding).root_capability();
    let mut state = proxy.state.lock().unwrap();
    for id in [u64::MAX - 1, u64::MAX] {
        state.calls.push(ChildCall {
            id,
            call: Call::Prepare(request()),
            outcome: None,
            canceled: false,
            queried: false,
        });
    }
    drop(state);

    let import_request = import::Request {
        root: root.clone(),
        transfer: LeaseId::new(),
        step: U64(0),
        action: import::Action::Abort,
    };
    let waiting_request = import_request.clone();
    let waiting_proxy = proxy.clone();
    let waiting = thread::spawn(move || {
        waiting_proxy.call(Call::Import(waiting_request), &AtomicBool::new(false))
    });
    let held_until = Instant::now() + Duration::from_millis(50);
    while Instant::now() < held_until && !waiting.is_finished() {
        assert!(proxy.next(Lane::Data).is_none());
        thread::sleep(Duration::from_millis(1));
    }
    let rejected = waiting.is_finished();
    assert_eq!(
        proxy.state.lock().unwrap().calls.len(),
        2,
        "waiting import must not enlarge the retained relay root"
    );
    let import_id = if rejected {
        0
    } else {
        proxy.state.lock().unwrap().calls.pop();
        let deadline = Instant::now() + Duration::from_secs(2);
        let (id, call) = loop {
            if let Some(out) = proxy.next(Lane::Data) {
                let Body::Call { id, call } = decode(&binding, &out.bytes, Lane::Data)? else {
                    anyhow::bail!("expected relay call")
                };
                break (id.0, call);
            }
            ensure!(
                Instant::now() < deadline,
                "waiting import admission timeout"
            );
            thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(call, Call::Import(_)));
        assert_eq!(
            proxy.state.lock().unwrap().calls.len(),
            2,
            "admitted import replaces the retired call within the same bound"
        );
        id
    };
    if import_id != 0 {
        proxy.receive(
            &encode(
                &Packet {
                    binding: binding.clone(),
                    body: Body::Reply {
                        id: U64(import_id),
                        outcome: Ok(Value::Import(import::Reply {
                            root,
                            transfer: import_request.transfer.clone(),
                            step: import_request.step,
                            request_digest: import_request.digest()?,
                            value: import::Value::Aborted,
                        })),
                    },
                },
                BYTES,
            )?,
            Lane::Data,
        )?;
        assert!(proxy.next(Lane::Control).is_some());
    }
    let waiting_result = waiting.join().unwrap();
    proxy.state.lock().unwrap().calls.clear();
    ensure!(
        !rejected,
        "managed import cleanup was rejected at transient relay capacity"
    );
    assert!(matches!(waiting_result?, Value::Import(_)));
    Ok(())
}

#[test]
fn independent_control_capacity_survives_full_data_queue() -> Result<()> {
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let mut output = Output::default();
    for id in 1..=2 {
        output.push(
            &binding,
            Body::Call {
                id: U64(id),
                call: Call::Prepare(request()),
            },
        )?;
    }
    assert!(
        output
            .push(
                &binding,
                Body::Call {
                    id: U64(3),
                    call: Call::Prepare(request())
                }
            )
            .is_err()
    );
    output.push(&binding, Body::Control(Control::Cancel { id: U64(1) }))?;
    output.push(
        &binding,
        Body::Control(Control::Admission {
            id: U64(1),
            operation: U64(u64::MAX - 3),
            session: LeaseId::new(),
        }),
    )?;
    assert_eq!(output.next(Lane::Control).unwrap().lane, Lane::Control);
    assert_eq!(output.next(Lane::Control).unwrap().lane, Lane::Control);
    assert_eq!(output.next(Lane::Data).unwrap().lane, Lane::Data);
    Ok(())
}

#[test]
fn each_control_class_and_admission_have_capacity_when_ordinary_output_is_held() -> Result<()> {
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let mut output = Output::default();
    for id in 1..=2 {
        output.push(
            &binding,
            Body::Reply {
                id: U64(id),
                outcome: Ok(Value::Unit),
            },
        )?;
        for control in [
            Control::Status { id: U64(id) },
            Control::State {
                id: U64(id),
                retained: false,
            },
            Control::Ack {
                id: U64(id),
                digest: format!("digest-{id}"),
            },
        ] {
            output.push(&binding, Body::Control(control.clone()))?;
            output.push(&binding, Body::Control(control))?; // identical retries coalesce
        }
    }
    assert!(
        output
            .push(&binding, Body::Control(Control::Status { id: U64(3) }))
            .is_err()
    );
    assert!(
        output
            .push(
                &binding,
                Body::Control(Control::Ack {
                    id: U64(1),
                    digest: "altered".into()
                })
            )
            .is_err()
    );
    for id in 1..=2 {
        output.push(&binding, Body::Control(Control::Cancel { id: U64(id) }))?;
    }
    output.push(
        &binding,
        Body::Control(Control::Failed(Fault::new("owner lost", true))),
    )?;
    output.push(
        &binding,
        Body::Control(Control::Admission {
            id: U64(1),
            operation: U64(42),
            session: LeaseId::new(),
        }),
    )?;
    output.push(
        &binding,
        Body::AdmissionReply {
            id: U64(1),
            value: Ok(None),
        },
    )?;
    let admission = output
        .next(Lane::Admission)
        .context("reserved admission reply")?;
    assert!(matches!(
        decode(&binding, &admission.bytes, Lane::Admission)?,
        Body::AdmissionReply { id: U64(1), .. }
    ));
    assert!(decode(&binding, &admission.bytes, Lane::Data).is_err());
    assert!(matches!(
        decode(
            &binding,
            &output.next(Lane::Control).unwrap().bytes,
            Lane::Control
        )?,
        Body::Control(Control::Failed(_))
    ));
    for id in 1..=2 {
        assert!(
            matches!(decode(&binding, &output.next(Lane::Control).unwrap().bytes, Lane::Control)?, Body::Control(Control::Cancel { id: got }) if got == U64(id))
        );
    }
    assert_eq!(
        output.data.len(),
        2,
        "reserved lanes do not consume or drop ordinary results"
    );
    assert_eq!(output.ack.entries.len(), 2);
    assert_eq!(output.status.entries.len(), 2);
    assert_eq!(output.state.entries.len(), 2);
    Ok(())
}

// This owner never creates a C/SQL/native dependent. The fixture may therefore
// explicitly retire F on every unwind; the production API retains that prerequisite.
struct EmptyOwner(Arc<Parent>);
impl Drop for EmptyOwner {
    fn drop(&mut self) {
        let _ = self.0.finish_after_dependents(true);
    }
}
fn real_empty_owner() -> Result<EmptyOwner> {
    let executable = std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE")
        .context("exact configured F executable required")?;
    let client = Arc::new(Client::spawn(std::path::Path::new(&executable), vec![])?);
    eprintln!("owned relay-only F fixture PID {}", client.pid());
    Ok(EmptyOwner(Parent::new(client)))
}
fn await_result(owner: &Parent) -> Result<Out> {
    await_lane(owner, Lane::Data)
}
fn await_lane(owner: &Parent, lane: Lane) -> Result<Out> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(out) = owner.next(lane) {
            return Ok(out);
        }
        ensure!(Instant::now() < deadline, "retained relay result timeout");
        thread::sleep(Duration::from_millis(5));
    }
}
#[test]
#[ignore = "requires the exact built CLI; scripts/test_catalog_filesystem_processes.py runs this"]
fn actual_queued_cancel_and_duplicate_ids_do_not_dispatch_f_effects() -> Result<()> {
    let owner = real_empty_owner()?;
    let invoked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = invoked.clone();
    *owner.0.observer.lock().unwrap() = Some(Arc::new(move |_, after| {
        if !after {
            counter.fetch_add(1, Ordering::AcqRel);
            anyhow::bail!("fixture stopped before F effects");
        }
        Ok(())
    }));
    let send = |body| -> Result<()> {
        let lane = if matches!(body, Body::Control(_)) {
            Lane::Control
        } else {
            Lane::Data
        };
        owner.0.receive(
            &encode(
                &Packet {
                    binding: owner.0.binding.clone(),
                    body,
                },
                BYTES,
            )?,
            lane,
        )
    };
    send(Body::Control(Control::Cancel { id: U64(1) }))?;
    send(Body::Control(Control::Cancel { id: U64(1) }))?;
    let r = request();
    let call = Body::Call {
        id: U64(1),
        call: Call::Prepare(r.clone()),
    };
    send(call.clone())?;
    let first = await_result(&owner.0)?;
    assert!(matches!(
        decode(&owner.0.binding, &first.bytes, Lane::Data)?,
        Body::Reply {
            outcome: Err(Fault { unknown: false, .. }),
            ..
        }
    ));
    assert_eq!(invoked.load(Ordering::Acquire), 0);
    // Admission reconciliation has its own retained result while the ordinary
    // canceled outcome remains unacknowledged. Exact replay returns frozen bytes.
    let query = Body::Control(Control::Admission {
        id: U64(1),
        operation: r.operation,
        session: r.session.clone(),
    });
    send(query.clone())?;
    let snapshot = await_lane(&owner.0, Lane::Admission)?;
    assert!(matches!(
        decode(&owner.0.binding, &snapshot.bytes, Lane::Admission)?,
        Body::AdmissionReply {
            value: Ok(None),
            ..
        }
    ));
    send(query.clone())?;
    assert_eq!(
        await_lane(&owner.0, Lane::Admission)?.bytes.as_slice(),
        snapshot.bytes.as_slice()
    );
    assert!(
        send(Body::Control(Control::Admission {
            id: U64(1),
            operation: r.operation,
            session: LeaseId::new()
        }))
        .is_err()
    );
    send(Body::Control(Control::Admission {
        id: U64(2),
        operation: r.operation,
        session: r.session.clone(),
    }))?;
    await_lane(&owner.0, Lane::Admission)?;
    assert!(
        send(query).is_err(),
        "old admission query cannot reopen historical authority"
    );

    // A completed duplicate returns the exact retained bytes, and never replays.
    send(call)?;
    assert_eq!(
        await_result(&owner.0)?.bytes.as_slice(),
        first.bytes.as_slice()
    );
    let mut changed = r.clone();
    changed.operation = U64(r.operation.0 - 1);
    assert!(
        send(Body::Call {
            id: U64(1),
            call: Call::Prepare(changed)
        })
        .is_err()
    );
    let digest = blake3::hash(&first.bytes).to_hex().to_string();
    send(Body::Control(Control::Ack {
        id: U64(1),
        digest: digest.clone(),
    }))?;
    send(Body::Control(Control::Ack { id: U64(1), digest }))?;
    assert!(
        send(Body::Control(Control::Ack {
            id: U64(1),
            digest: "altered".into()
        }))
        .is_err()
    );
    assert!(
        send(Body::Call {
            id: U64(1),
            call: Call::Prepare(r.clone())
        })
        .is_err()
    );
    send(Body::Call {
        id: U64(2),
        call: Call::Prepare(r.clone()),
    })?;
    let second = await_result(&owner.0)?;
    assert!(matches!(
        decode(&owner.0.binding, &second.bytes, Lane::Data)?,
        Body::Reply { id: U64(2), .. }
    ));
    assert_eq!(
        invoked.load(Ordering::Acquire),
        1,
        "admitted positive control reaches executor exactly once"
    );
    assert!(
        owner
            .0
            .client
            .admission_status(r.operation, &r.session)?
            .is_none(),
        "neither fixture path enters F Prepare"
    );
    let pid = owner.0.client.pid();
    owner.0.finish_after_dependents(true)?;
    eprintln!("queued relay fixture verified F={pid} reaped and relay joined");
    Ok(())
}
#[test]
#[ignore = "requires the exact built CLI; scripts/test_catalog_filesystem_processes.py runs this"]
fn actual_idle_f_death_is_monitored_but_normal_retirement_is_not_failure() -> Result<()> {
    let owner = real_empty_owner()?;
    // No C has ever existed; explicit external retirement safely models an idle
    // F disappearance from the relay's point of view, with actual wait/IO joins.
    owner.0.client.terminate_after_dependents_drained()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while owner.0.healthy().is_ok() {
        ensure!(Instant::now() < deadline, "idle F death was not propagated");
        thread::sleep(Duration::from_millis(5));
    }
    let failure = owner
        .0
        .next(Lane::Control)
        .context("reserved Failed notification")?;
    assert!(matches!(
        decode(&owner.0.binding, &failure.bytes, Lane::Control)?,
        Body::Control(Control::Failed(_))
    ));
    owner.0.finish_after_dependents(true)?;
    let normal = real_empty_owner()?;
    normal.0.finish_after_dependents(false)?;
    normal.0.healthy()?;
    assert!(normal.0.next(Lane::Control).is_none());
    Ok(())
}

type ExportRelayFixture = (
    EmptyOwner,
    Arc<Proxy>,
    super::super::export_native::tests::Fixture,
    crate::preview::ByteBudget,
    Arc<super::super::export_native::tests::FakeStages>,
);

fn export_relay_fixture() -> Result<ExportRelayFixture> {
    let parent = real_empty_owner()?;
    let mut f = super::super::export_native::tests::fixture()?;
    f.root.epoch = parent.0.binding.epoch.clone();
    f.begin.root = f.root.clone();
    f.executor = crate::catalog_session::export_executor::executor_id(&f.root, 1)?;
    f.begin.executor = f.executor.clone();
    let pool = crate::preview::ByteBudget::new(f.worker)?;
    let stages = super::super::export_native::tests::FakeStages::new(f._temp.path().to_owned());
    let owner = Arc::new(super::super::export_native::Owner::new(
        f._temp.path().join("missing-worker"),
        stages.clone(),
        1,
        &pool,
    )?);
    owner.bind(&f.root)?;
    super::super::export_native::tests::acquire(&owner, &f)?;
    *parent.0.export_native.lock().unwrap() = Some(owner);
    let proxy = Proxy::new(parent.0.binding.clone());
    Ok((parent, proxy, f, pool, stages))
}
fn pump_export_relay<T: Send + 'static>(
    parent: &Parent,
    proxy: &Arc<Proxy>,
    operation: impl FnOnce(Arc<Proxy>) -> Result<T> + Send + 'static,
    drop_control_reply: bool,
) -> Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = {
        let proxy = proxy.clone();
        thread::spawn(move || {
            let _ = tx.send(operation(proxy));
        })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut dropped = None;
    let mut queries = Vec::new();
    loop {
        // Deliver cancellation before its queued ordinary request.
        for lane in [Lane::Control, Lane::Data] {
            if let Some(out) = proxy.next(lane) {
                if let Body::Control(Control::ExportNativeQuery { id, query }) =
                    decode(&parent.binding, &out.bytes, lane)?
                {
                    queries.push((id, query));
                }
                parent.receive(&out.bytes, lane)?;
            }
        }
        for lane in [Lane::Control, Lane::Data] {
            if let Some(out) = parent.next(lane) {
                if drop_control_reply
                    && dropped.is_none()
                    && matches!(
                        decode(&parent.binding, &out.bytes, lane)?,
                        Body::Control(Control::ExportNativeReply { .. })
                    )
                {
                    dropped = Some(out.bytes);
                } else {
                    proxy.receive(&out.bytes, lane)?;
                }
            }
        }
        match rx.try_recv() {
            Ok(result) => {
                worker.join().unwrap();
                // Flush the ordinary ACK before the next operation.
                while let Some(out) = proxy.next(Lane::Control) {
                    parent.receive(&out.bytes, Lane::Control)?;
                }
                if drop_control_reply {
                    assert!(dropped.is_some());
                    assert!(queries.len() >= 2);
                    assert!(
                        queries.iter().all(|q| q == &queries[0]),
                        "resends must retain exact id/action"
                    );
                    // A late duplicate remains harmless after the caller consumed its result.
                    proxy.receive(&dropped.unwrap(), Lane::Control)?;
                }
                return result;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            Instant::now() < deadline,
            "export Parent/Proxy relay deadline"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
#[test]
#[ignore = "requires exact built F executable via PHOTOCATALOG_TEST_EXECUTABLE; run serialized"]
fn export_native_parent_proxy_canceled_queue_records_highwater_and_contiguous_cleanup() -> Result<()>
{
    use super::super::export_native::tests::{ordinary, register};
    use crate::catalog_session::{export_native::CatalogExportNative, export_stage};
    let (parent, proxy, f, pool, _stages) = export_relay_fixture()?;
    let registration = register(&f);
    pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::call(p.as_ref(), &registration, &AtomicBool::new(false)),
        false,
    )?;
    let begin = f.begin.clone();
    pump_export_relay(
        &parent.0,
        &proxy,
        move |p| p.call(Call::ExportStage(Box::new(begin)), &AtomicBool::new(false)),
        false,
    )?;
    let canceled = ordinary(
        &f,
        2,
        export_stage::Action::Ready {
            icc: None,
            xmp: None,
        },
    );
    let error = pump_export_relay(
        &parent.0,
        &proxy,
        move |p| {
            p.call(
                Call::ExportStage(Box::new(canceled)),
                &AtomicBool::new(true),
            )
        },
        false,
    )
    .err()
    .context("queued cancellation")?;
    assert_eq!(
        error
            .downcast_ref::<Failure>()
            .context("typed cancellation")?
            .kind,
        FailureKind::Canceled
    );
    let key = crate::catalog_session::export_native::Key::new(&f.root, U64(9), &f.stage);
    let result = pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::status(p.as_ref(), &key),
        false,
    )?;
    assert_eq!(result.stage_high_water, U64(2));
    assert_eq!(
        result.pending_dispatch,
        crate::catalog_session::export_native::DispatchState::NeverDispatched
    );
    let abort = ordinary(&f, 3, export_stage::Action::Abort);
    pump_export_relay(
        &parent.0,
        &proxy,
        move |p| p.call(Call::ExportStage(Box::new(abort)), &AtomicBool::new(false)),
        false,
    )?;
    let retire = super::super::export_native::tests::lifecycle(
        &f,
        crate::catalog_session::export_native::Action::Retire,
    );
    pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::call(p.as_ref(), &retire, &AtomicBool::new(false)),
        false,
    )?;
    assert_eq!(pool.used(), 0);
    parent.0.finish_after_dependents(true)?;
    eprintln!(
        "export canceled relay verified F={} checked reap and relay join",
        parent.0.client.pid()
    );
    Ok(())
}
#[test]
#[ignore = "requires exact built F executable via PHOTOCATALOG_TEST_EXECUTABLE; run serialized"]
fn export_native_parent_proxy_lost_status_drain_retire_replies_replay_exact_ids() -> Result<()> {
    use super::super::export_native::tests::{lifecycle, ordinary, register};
    use crate::catalog_session::{
        export_native::{Action, CatalogExportNative, Key, Phase},
        export_stage,
    };
    let (parent, proxy, f, pool, _stages) = export_relay_fixture()?;
    let request = register(&f);
    pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::call(p.as_ref(), &request, &AtomicBool::new(false)),
        false,
    )?;
    let key = Key::new(&f.root, U64(9), &f.stage);
    let status = pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::status(p.as_ref(), &key),
        true,
    )?;
    assert_eq!(status.phase, Phase::Registered);
    assert_eq!(pool.used(), f.worker);
    for request in [
        f.begin.clone(),
        ordinary(
            &f,
            2,
            export_stage::Action::Ready {
                icc: None,
                xmp: None,
            },
        ),
    ] {
        pump_export_relay(
            &parent.0,
            &proxy,
            move |p| {
                p.call(
                    Call::ExportStage(Box::new(request)),
                    &AtomicBool::new(false),
                )
            },
            false,
        )?;
    }
    let request = lifecycle(&f, Action::Spawn);
    pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::call(p.as_ref(), &request, &AtomicBool::new(false)),
        false,
    )?;
    let request = lifecycle(&f, Action::RetryDrain);
    pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::call(p.as_ref(), &request, &AtomicBool::new(false)),
        true,
    )?;
    let owner = parent.0.export_native_owner()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if owner
            .query(&crate::catalog_session::export_native::Query {
                key: Key::new(&f.root, U64(9), &f.stage),
                action: crate::catalog_session::export_native::QueryAction::Status,
            })?
            .phase
            == Phase::Drained
        {
            break;
        }
        ensure!(Instant::now() < deadline, "drain deadline");
        thread::sleep(Duration::from_millis(2));
    }
    let release = ordinary(&f, 3, export_stage::Action::Release);
    pump_export_relay(
        &parent.0,
        &proxy,
        move |p| {
            p.call(
                Call::ExportStage(Box::new(release)),
                &AtomicBool::new(false),
            )
        },
        false,
    )?;
    let request = lifecycle(&f, Action::Retire);
    let status = pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::call(p.as_ref(), &request, &AtomicBool::new(false)),
        true,
    )?;
    assert_eq!(status.phase, Phase::Released);
    assert_eq!(pool.used(), 0);
    assert_eq!(pool.reserve_exact(f.worker)?.bytes(), f.worker);
    parent.0.finish_after_dependents(true)?;
    eprintln!(
        "export lost-control relay verified F={} checked reap and relay join",
        parent.0.client.pid()
    );
    Ok(())
}

#[test]
#[ignore = "requires exact built F executable via PHOTOCATALOG_TEST_EXECUTABLE; run serialized"]
fn export_native_parent_proxy_held_arm_keeps_busy_stop_status_control_available() -> Result<()> {
    use super::super::export_native::tests::{lifecycle, ordinary, register};
    use crate::catalog_session::{
        export_native::{Action, CatalogExportNative, Key, Phase},
        export_stage,
    };
    let (parent, proxy, f, pool, stages) = export_relay_fixture()?;
    let request = register(&f);
    pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::call(p.as_ref(), &request, &AtomicBool::new(false)),
        false,
    )?;
    for request in [
        f.begin.clone(),
        ordinary(
            &f,
            2,
            export_stage::Action::Ready {
                icc: None,
                xmp: None,
            },
        ),
    ] {
        pump_export_relay(
            &parent.0,
            &proxy,
            move |p| {
                p.call(
                    Call::ExportStage(Box::new(request)),
                    &AtomicBool::new(false),
                )
            },
            false,
        )?;
    }
    stages.pause_arm();
    let (tx, rx) = std::sync::mpsc::channel();
    let spawn = {
        let proxy = proxy.clone();
        let request = lifecycle(&f, Action::Spawn);
        thread::spawn(move || {
            let _ = tx.send(CatalogExportNative::call(
                proxy.as_ref(),
                &request,
                &AtomicBool::new(false),
            ));
        })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(out) = proxy.next(Lane::Data) {
            parent.0.receive(&out.bytes, Lane::Data)?;
            break;
        }
        ensure!(Instant::now() < deadline, "held Arm dispatch deadline");
        thread::sleep(Duration::from_millis(2));
    }
    stages.wait_ready_entered();
    // This order formerly blocked the reader and ParentState on lifecycle.
    for action in [Action::Retire, Action::RetryDrain] {
        let request = lifecycle(&f, action);
        let error = pump_export_relay(
            &parent.0,
            &proxy,
            move |p| CatalogExportNative::call(p.as_ref(), &request, &AtomicBool::new(false)),
            false,
        )
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::preview::stage_io::Busy>()
                .is_some(),
            "reserved refusal must remain typed Busy through Proxy: {error}"
        );
        assert_eq!(pool.used(), f.worker);
    }
    let stop = lifecycle(&f, Action::Stop);
    let stopped = pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::call(p.as_ref(), &stop, &AtomicBool::new(false)),
        false,
    )?;
    assert_eq!(stopped.phase, Phase::StopRequested);
    assert!(stopped.pid.is_none());
    let key = Key::new(&f.root, U64(9), &f.stage);
    let status = pump_export_relay(
        &parent.0,
        &proxy,
        move |p| CatalogExportNative::status(p.as_ref(), &key),
        false,
    )?;
    assert_eq!(status.phase, Phase::StopRequested);
    assert_eq!(pool.used(), f.worker);
    assert!(rx.try_recv().is_err(), "Arm is still barrier-held");
    stages.resume_arm();
    let spawned = pump_export_relay(
        &parent.0,
        &proxy,
        move |_| rx.recv_timeout(Duration::from_secs(5))?,
        false,
    )?;
    spawn.join().unwrap();
    assert_eq!(spawned.phase, Phase::WaitFailed);
    assert!(spawned.pid.is_none());
    parent.0.export_native_owner()?.retire_root(&f.root)?;
    assert_eq!(pool.used(), 0);
    parent.0.finish_after_dependents(true)?;
    eprintln!(
        "held Arm reserved-control fixture verified F={} checked reap and relay join",
        parent.0.client.pid()
    );
    Ok(())
}

#[test]
#[ignore = "requires exact built F executable via PHOTOCATALOG_TEST_EXECUTABLE; run serialized"]
fn export_native_parent_proxy_contiguous_query_supersedes_queued_prior_duplicate() -> Result<()> {
    use super::super::export_native::tests::register;
    use crate::catalog_session::export_native::{Key, Query, QueryAction};
    let (parent, _proxy, f, pool, _stages) = export_relay_fixture()?;
    let owner = parent.0.export_native_owner()?;
    owner.call(&register(&f))?;
    let send = |id, action| -> Result<()> {
        let body = Body::Control(Control::ExportNativeQuery {
            id: U64(id),
            query: Query {
                key: Key::new(&f.root, U64(9), &f.stage),
                action,
            },
        });
        parent.0.receive(
            &encode(
                &Packet {
                    binding: parent.0.binding.clone(),
                    body,
                },
                BYTES,
            )?,
            Lane::Control,
        )
    };
    send(1, QueryAction::Status)?;
    let first = await_lane(&parent.0, Lane::Control)?; // C can now consume reply 1.
    send(1, QueryAction::Status)?; // Duplicate reply 1 deliberately stays unsent.
    assert_eq!(
        parent
            .0
            .state
            .lock()
            .unwrap()
            .output
            .export_native_reply
            .entries
            .len(),
        1
    );
    send(2, QueryAction::Retire)?; // Advancing is the acknowledgement of reply 1.
    assert_eq!(pool.used(), 0);
    {
        let state = parent.0.state.lock().unwrap();
        let queued = &state.output.export_native_reply.entries;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, 2);
        assert_eq!(state.export_native_result.as_ref().unwrap().0, 2);
    }
    send(2, QueryAction::Retire)?; // Same identity deduplicates in the one slot.
    let second = await_lane(&parent.0, Lane::Control)?;
    assert!(matches!(
        decode(&parent.0.binding, &first.bytes, Lane::Control)?,
        Body::Control(Control::ExportNativeReply {
            id: U64(1),
            value: Ok(_),
            ..
        })
    ));
    assert!(matches!(
        decode(&parent.0.binding, &second.bytes, Lane::Control)?,
        Body::Control(Control::ExportNativeReply {
            id: U64(2),
            value: Ok(_),
            ..
        })
    ));
    send(2, QueryAction::Retire)?;
    assert_eq!(
        await_lane(&parent.0, Lane::Control)?.bytes.as_slice(),
        second.bytes.as_slice()
    );
    assert_eq!(pool.used(), 0);
    assert_eq!(pool.reserve_exact(f.worker)?.bytes(), f.worker);
    assert!(send(2, QueryAction::Status).is_err());
    assert!(send(1, QueryAction::Status).is_err());
    parent.0.finish_after_dependents(true)?;
    eprintln!(
        "queued duplicate-control fixture verified F={} checked reap and relay join",
        parent.0.client.pid()
    );
    Ok(())
}
