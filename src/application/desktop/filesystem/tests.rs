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
