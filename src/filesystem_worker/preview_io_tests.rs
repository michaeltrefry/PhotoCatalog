// Included in store::tests to reuse its real, isolated held-root fixture.
use crate::catalog_session::preview_io as io;
use crate::filesystem_worker::preview_io::ObjectOwner;
fn object_request(
    f: &Fixture,
    group: &Acquired,
    op: u64,
    step: u64,
    action: io::Action,
) -> io::Request {
    io::Request {
        root: f.bootstrap.root_capability(),
        group: group.group.clone(),
        operation: U64(op),
        step: U64(step),
        action,
    }
}
fn object_expected(group: &Acquired, bytes: &[u8]) -> io::Expected {
    io::Expected {
        object: io::Object {
            root: group.tiers[0].token.clone(),
            key: "a".repeat(64),
        },
        bytes: U64(bytes.len() as u64),
        checksum: blake3::hash(bytes).to_hex().to_string(),
    }
}
fn object_execute(
    owner: &mut ObjectOwner,
    f: &Fixture,
    request: &io::Request,
) -> Result<io::Reply> {
    owner.execute(&f.owner, request, &AtomicBool::new(false), |_| Ok(()))
}
#[test]
fn cache_chunks_cross_metadata_envelope_without_full_f_object_and_reject_replay() -> Result<()> {
    let mut f = Fixture::new()?;
    let group = f.acquire()?;
    let mut owner = ObjectOwner::default();
    let bytes = vec![0x8f; 1024 * 1024 + 7];
    let expected = object_expected(&group, &bytes);
    let temporary = format!("{}.{}.pending", expected.object.key, uuid::Uuid::new_v4());
    let first = object_request(
        &f,
        &group,
        1,
        0,
        io::Action::BeginWrite {
            expected: expected.clone(),
            temporary,
        },
    );
    object_execute(&mut owner, &f, &first)?;
    let mut step = 0;
    for (index, chunk) in bytes.chunks(io::CHUNK_BYTES).enumerate() {
        step += 1;
        let request = object_request(
            &f,
            &group,
            1,
            step,
            io::Action::Write {
                offset: U64((index * io::CHUNK_BYTES) as u64),
                checksum: blake3::hash(chunk).to_hex().to_string(),
                bytes: chunk.to_vec(),
            },
        );
        let encoded = crate::filesystem_worker::wire::encode_operation(
            &crate::filesystem_worker::wire::Operation::PreviewIo(request.clone()),
        )?;
        assert!(encoded.len() < 2 * io::CHUNK_BYTES);
        let crate::filesystem_worker::wire::Operation::PreviewIo(decoded) =
            crate::filesystem_worker::wire::decode_operation(&encoded)?
        else {
            unreachable!()
        };
        assert_eq!(decoded, request);
        let reply = object_execute(&mut owner, &f, &request)?;
        assert_eq!(reply, object_execute(&mut owner, &f, &request)?);
    }
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 1, step + 1, io::Action::Finish),
    )?;
    assert_eq!(
        fs::read(group.tiers[0].path.to_path()?.join(&expected.object.key))?,
        bytes
    );
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 8, 0, io::Action::Check(expected.clone())),
    )?;
    assert!(object_execute(&mut owner, &f, &first).is_err());
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            9,
            0,
            io::Action::BeginRead {
                expected,
                allowance: U64(bytes.len() as u64),
            },
        ),
    )?;
    let mut output = vec![];
    let mut step = 0;
    while output.len() < bytes.len() {
        step += 1;
        let request = object_request(
            &f,
            &group,
            9,
            step,
            io::Action::Read {
                offset: U64(output.len() as u64),
            },
        );
        let reply = object_execute(&mut owner, &f, &request)?;
        let encoded = crate::filesystem_worker::wire::encode_outcome(&Ok(
            crate::filesystem_worker::wire::Response::PreviewIo(reply),
        ))?;
        assert!(encoded.len() < 2 * io::CHUNK_BYTES);
        let Ok(crate::filesystem_worker::wire::Response::PreviewIo(reply)) =
            crate::filesystem_worker::wire::decode_outcome(&encoded)?
        else {
            unreachable!()
        };
        reply.validate(&request)?;
        let io::Value::Chunk { bytes, .. } = reply.value else {
            unreachable!()
        };
        output.extend_from_slice(&bytes);
    }
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 9, step + 1, io::Action::Finish),
    )?;
    assert_eq!(output, bytes);
    owner.drain();
    Ok(())
}
#[test]
fn cache_lost_finish_receipt_replays_without_overwriting_and_status_is_independent() -> Result<()> {
    let mut f = Fixture::new()?;
    let group = f.acquire()?;
    let mut owner = ObjectOwner::default();
    let bytes = b"immutable bytes";
    let expected = object_expected(&group, bytes);
    let name = format!("{}.{}.pending", expected.object.key, uuid::Uuid::new_v4());
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            1,
            0,
            io::Action::BeginWrite {
                expected: expected.clone(),
                temporary: name,
            },
        ),
    )?;
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            1,
            1,
            io::Action::Write {
                offset: U64(0),
                checksum: expected.checksum.clone(),
                bytes: bytes.to_vec(),
            },
        ),
    )?;
    let request = object_request(&f, &group, 1, 2, io::Action::Finish);
    let mut published = 0;
    let mut status = None;
    let result = owner.execute(&f.owner, &request, &AtomicBool::new(false), |snapshot| {
        published += 1;
        status = Some(snapshot);
        ensure!(published != 2, "injected lost final snapshot delivery");
        Ok(())
    });
    assert!(result.is_err());
    let value = object_execute(&mut owner, &f, &request)?;
    assert_eq!(value.value, io::Value::Unit);
    let query = StatusQuery::from(&Query {
        kind: crate::catalog_session::store::StatusKind::Objects,
        root: f.bootstrap.root_capability(),
        operation: U64(1),
        selected: None,
    });
    let value = status.unwrap().status(&query)?;
    value.validate(&query)?;
    assert_eq!(value.stage, Some(Stage::Complete));
    assert!(value.object.is_some());
    let mut wrong = query.clone();
    wrong.kind = crate::catalog_session::store::StatusKind::Locks;
    assert!(value.validate(&wrong).is_err());
    assert_eq!(
        fs::read(group.tiers[0].path.to_path()?.join(&expected.object.key))?,
        bytes
    );
    owner.drain();
    Ok(())
}
#[test]
fn cache_read_checks_length_before_budget_and_terminal_resource_failure_replays() -> Result<()> {
    let mut f = Fixture::new()?;
    let group = f.acquire()?;
    let mut owner = ObjectOwner::default();
    let bytes = b"original cached";
    let expected = object_expected(&group, bytes);
    fs::write(
        group.tiers[0].path.to_path()?.join(&expected.object.key),
        bytes,
    )?;
    let request = object_request(
        &f,
        &group,
        1,
        0,
        io::Action::BeginRead {
            expected: expected.clone(),
            allowance: U64(1),
        },
    );
    let first = object_execute(&mut owner, &f, &request).unwrap_err();
    let replay = object_execute(&mut owner, &f, &request).unwrap_err();
    assert_eq!(
        first
            .downcast_ref::<crate::filesystem_worker::wire::Failure>()
            .unwrap()
            .kind,
        crate::filesystem_worker::wire::FailureKind::ResourceLimit
    );
    assert_eq!(format!("{first}"), format!("{replay}"));
    fs::write(
        group.tiers[0].path.to_path()?.join(&expected.object.key),
        b"changed length",
    )?;
    let request = object_request(
        &f,
        &group,
        2,
        0,
        io::Action::BeginRead {
            expected,
            allowance: U64(0),
        },
    );
    assert_eq!(
        object_execute(&mut owner, &f, &request)?.value,
        io::Value::Integrity(io::Integrity::Corrupt)
    );
    owner.drain();
    Ok(())
}
#[test]
fn cache_canceled_chunk_and_changed_group_have_no_effect_before_checked_abort() -> Result<()> {
    let mut f = Fixture::new()?;
    let group = f.acquire()?;
    let mut owner = ObjectOwner::default();
    let bytes = b"upload";
    let expected = object_expected(&group, bytes);
    let name = format!("{}.{}.pending", expected.object.key, uuid::Uuid::new_v4());
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            1,
            0,
            io::Action::BeginWrite {
                expected: expected.clone(),
                temporary: name.clone(),
            },
        ),
    )?;
    let request = object_request(
        &f,
        &group,
        1,
        1,
        io::Action::Write {
            offset: U64(0),
            checksum: expected.checksum,
            bytes: bytes.to_vec(),
        },
    );
    assert!(
        owner
            .execute(&f.owner, &request, &AtomicBool::new(true), |_| Ok(()))
            .is_err()
    );
    assert_eq!(
        fs::metadata(group.tiers[0].path.to_path()?.join(&name))?.len(),
        0
    );
    let mut wrong = object_request(&f, &group, 1, 1, io::Action::Abort);
    wrong.group = LeaseId::new();
    assert!(object_execute(&mut owner, &f, &wrong).is_err());
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 1, 1, io::Action::Abort),
    )?;
    assert!(!group.tiers[0].path.to_path()?.join(name).exists());
    assert!(locked(&group.tiers[0].path)?);
    owner.drain();
    Ok(())
}
#[test]
fn cache_truncation_during_stream_is_corruption_not_lost_reply() -> Result<()> {
    let mut f = Fixture::new()?;
    let group = f.acquire()?;
    let mut owner = ObjectOwner::default();
    let expected = object_expected(&group, b"stable cache bytes");
    let path = group.tiers[0].path.to_path()?.join(&expected.object.key);
    fs::write(&path, b"stable cache bytes")?;
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            1,
            0,
            io::Action::BeginRead {
                expected,
                allowance: U64(100),
            },
        ),
    )?;
    fs::write(&path, b"short")?;
    let request = object_request(&f, &group, 1, 1, io::Action::Read { offset: U64(0) });
    let reply = object_execute(&mut owner, &f, &request)?;
    reply.validate(&request)?;
    assert_eq!(reply.value, io::Value::Integrity(io::Integrity::Corrupt));
    assert_eq!(reply, object_execute(&mut owner, &f, &request)?);
    owner.drain();
    Ok(())
}
#[test]
fn cache_relocation_reconciles_copy_and_cleanup_but_retains_root_locks() -> Result<()> {
    let mut f = Fixture::new()?;
    let group = f.acquire()?;
    let mut owner = ObjectOwner::default();
    let bytes = vec![0x79; io::CHUNK_BYTES + 3];
    let expected = object_expected(&group, &bytes);
    let old = group.tiers[0].path.to_path()?.join(&expected.object.key);
    fs::write(&old, &bytes)?;
    let reserve = f.request(Action::Reserve {
        group: group.group.clone(),
        tier: Tier::Thumbnail,
        destination: f.path("relocation-object"),
    });
    let Value::Reserved(target) = f.execute(&reserve)?.value else {
        unreachable!()
    };
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            1,
            0,
            io::Action::InspectRelocation {
                target: target.token.clone(),
            },
        ),
    )?;
    let lock = f.request(Action::Lock {
        group: group.group.clone(),
        reservation: target.token.clone(),
    });
    f.execute(&lock)?;
    let id = match object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            2,
            0,
            io::Action::AdmitRelocation {
                target: target.token.clone(),
            },
        ),
    )?
    .value
    {
        io::Value::Relocation(id) => id,
        _ => unreachable!(),
    };
    let relocation = |cleanup| io::Action::Relocate {
        source: expected.object.root.clone(),
        target: target.token.clone(),
        id: id.clone(),
        key: expected.object.key.clone(),
        bytes: expected.bytes,
        checksum: expected.checksum.clone(),
        cleanup,
    };
    let copy = object_request(&f, &group, 3, 0, relocation(false));
    object_execute(&mut owner, &f, &copy)?;
    assert_eq!(
        object_execute(&mut owner, &f, &copy)?.value,
        io::Value::Unit
    );
    let new = target.path.to_path()?.join(&expected.object.key);
    assert_eq!(fs::read(&old)?, bytes);
    assert_eq!(fs::read(&new)?, bytes);
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 4, 0, relocation(false)),
    )?;
    let promote = f.request(Action::Promote {
        group: group.group.clone(),
        target: target.token.clone(),
        tier: Tier::Thumbnail,
    });
    f.execute(&promote)?;
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 5, 0, relocation(true)),
    )?;
    assert!(!old.exists());
    assert_eq!(fs::read(&new)?, bytes);
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 6, 0, relocation(true)),
    )?;
    let retire = f.request(Action::Retire {
        group: group.group.clone(),
        old: group.tiers[0].token.clone(),
    });
    f.execute(&retire)?;
    assert!(locked(&group.tiers[0].path)? && locked(&target.path)?);
    assert!(
        object_execute(
            &mut owner,
            &f,
            &object_request(&f, &group, 7, 0, io::Action::Check(expected))
        )
        .is_err()
    );
    owner.drain();
    Ok(())
}
#[test]
fn cache_owner_capacity_admission_has_no_effect_and_does_not_consume_identity() -> Result<()> {
    let mut f = Fixture::new()?;
    let group = f.acquire()?;
    let expected = object_expected(&group, b"bytes");
    let temporary = format!("{}.{}.pending", expected.object.key, uuid::Uuid::new_v4());
    let request = object_request(
        &f,
        &group,
        1,
        0,
        io::Action::BeginWrite {
            expected,
            temporary: temporary.clone(),
        },
    );
    let mut owner = ObjectOwner::with_budget(0);
    assert!(
        object_execute(&mut owner, &f, &request)
            .unwrap_err()
            .is::<crate::catalog_session::store::ResourceLimit>()
    );
    assert!(!group.tiers[0].path.to_path()?.join(&temporary).exists());
    owner.restore_budget();
    object_execute(&mut owner, &f, &request)?;
    let (actual, reserved) = owner.capacity();
    assert!(actual <= reserved);
    eprintln!(
        "FS7 object owner actual retained={actual} reserved including status replacement={reserved}; request/codec/path scratch separate"
    );
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 1, 1, io::Action::Abort),
    )?;
    owner.drain();
    Ok(())
}
#[test]
fn cache_unknown_effect_reconcile_abort_preserves_publication_and_relocation_journal_target()
-> Result<()> {
    let mut f = Fixture::new()?;
    let group = f.acquire()?;
    let mut owner = ObjectOwner::default();
    let bytes = b"already published";
    let expected = object_expected(&group, bytes);
    let temporary = format!("{}.{}.pending", expected.object.key, uuid::Uuid::new_v4());
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            1,
            0,
            io::Action::BeginWrite {
                expected: expected.clone(),
                temporary,
            },
        ),
    )?;
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            1,
            1,
            io::Action::Write {
                offset: U64(0),
                checksum: expected.checksum.clone(),
                bytes: bytes.to_vec(),
            },
        ),
    )?;
    owner.fail_next_effect();
    let finish = object_request(&f, &group, 1, 2, io::Action::Finish);
    for _ in 0..2 {
        let error = object_execute(&mut owner, &f, &finish).unwrap_err();
        let failure = super::super::filesystem_failure(error);
        assert_eq!(
            failure.kind,
            crate::filesystem_worker::wire::FailureKind::Unknown
        );
        assert!(failure.object_receipt.unwrap().matches(&finish)?);
        let bytes = crate::filesystem_worker::wire::encode_outcome(&Err(failure))?;
        let decoded = crate::filesystem_worker::wire::decode_outcome(&bytes)?.unwrap_err();
        assert!(decoded.object_receipt.unwrap().matches(&finish)?);
    }
    let original = group.tiers[0].path.to_path()?.join(&expected.object.key);
    assert_eq!(fs::read(&original)?, bytes);
    assert!(
        object_execute(
            &mut owner,
            &f,
            &object_request(&f, &group, 2, 0, io::Action::Check(expected.clone()))
        )
        .is_err()
    );
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 1, 3, io::Action::Abort),
    )?;
    assert_eq!(
        fs::read(&original)?,
        bytes,
        "Abort removed a published object"
    );
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 2, 0, io::Action::Check(expected.clone())),
    )?;
    let reserve = f.request(Action::Reserve {
        group: group.group.clone(),
        tier: Tier::Thumbnail,
        destination: f.path("unknown-relocation"),
    });
    let Value::Reserved(target) = f.execute(&reserve)?.value else {
        unreachable!()
    };
    let lock = f.request(Action::Lock {
        group: group.group.clone(),
        reservation: target.token.clone(),
    });
    f.execute(&lock)?;
    let io::Value::Relocation(id) = object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            3,
            0,
            io::Action::AdmitRelocation {
                target: target.token.clone(),
            },
        ),
    )?
    .value
    else {
        unreachable!()
    };
    let action = io::Action::Relocate {
        source: expected.object.root.clone(),
        target: target.token.clone(),
        id,
        key: expected.object.key.clone(),
        bytes: expected.bytes,
        checksum: expected.checksum.clone(),
        cleanup: false,
    };
    owner.fail_next_effect();
    let request = object_request(&f, &group, 4, 0, action.clone());
    let first = object_execute(&mut owner, &f, &request).unwrap_err();
    let replay = object_execute(&mut owner, &f, &request).unwrap_err();
    assert_eq!(first.to_string(), replay.to_string());
    let destination = target.path.to_path()?.join(&expected.object.key);
    assert_eq!(fs::read(&destination)?, bytes);
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 4, 1, io::Action::Abort),
    )?;
    assert_eq!(fs::read(&destination)?, bytes);
    object_execute(&mut owner, &f, &object_request(&f, &group, 5, 0, action))?;
    assert_eq!(fs::read(&original)?, bytes);
    assert_eq!(fs::read(&destination)?, bytes);
    assert!(object_execute(&mut owner, &f, &request).is_err());
    owner.drain();
    Ok(())
}
#[test]
fn cache_hash_prefix_publication_and_existing_destination_failure_keep_immutable_bytes()
-> Result<()> {
    let mut f = Fixture::new()?;
    let acquire = f.request(Action::Acquire(Descriptor {
        identity: "prefix identity".into(),
        layout: Layout::HashPrefix,
        roots: [f.path("prefix-thumb"), f.path("prefix-large")],
        relocation: None,
    }));
    let Value::Acquired(group) = f.execute(&acquire)?.value else {
        unreachable!()
    };
    let mut owner = ObjectOwner::default();
    let bytes = b"hash prefix cache";
    let expected = object_expected(&group, bytes);
    let temporary = format!("{}.{}.pending", expected.object.key, uuid::Uuid::new_v4());
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            1,
            0,
            io::Action::BeginWrite {
                expected: expected.clone(),
                temporary: temporary.clone(),
            },
        ),
    )?;
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            1,
            1,
            io::Action::Write {
                offset: U64(0),
                checksum: expected.checksum.clone(),
                bytes: bytes.to_vec(),
            },
        ),
    )?;
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 1, 2, io::Action::Finish),
    )?;
    let destination = group.tiers[0]
        .path
        .to_path()?
        .join("aa/aa")
        .join(&expected.object.key);
    assert_eq!(fs::read(&destination)?, bytes);
    assert!(
        object_execute(
            &mut owner,
            &f,
            &object_request(
                &f,
                &group,
                2,
                0,
                io::Action::BeginWrite {
                    expected: expected.clone(),
                    temporary
                }
            )
        )
        .is_err()
    );
    object_execute(
        &mut owner,
        &f,
        &object_request(&f, &group, 2, 1, io::Action::Abort),
    )?;
    assert_eq!(fs::read(&destination)?, bytes);
    object_execute(
        &mut owner,
        &f,
        &object_request(
            &f,
            &group,
            3,
            0,
            io::Action::Remove {
                object: expected.object,
                temporary: None,
            },
        ),
    )?;
    assert!(!destination.exists());
    owner.drain();
    Ok(())
}
#[test]
fn cache_long_legacy_temporary_removal_uses_aggregate_admission_without_generated_cap() -> Result<()>
{
    let mut f = Fixture::new()?;
    let group = f.acquire()?;
    let mut owner = ObjectOwner::default();
    let expected = object_expected(&group, b"existing");
    let name = format!("{}.{}.pending", expected.object.key, "legacy".repeat(20));
    assert!(name.len() > 128 && name.len() < 255);
    let root = group.tiers[0].path.to_path()?;
    fs::write(root.join(&name), b"pending")?;
    fs::write(root.join(&expected.object.key), b"existing")?;
    let removal = object_request(
        &f,
        &group,
        1,
        0,
        io::Action::Remove {
            object: expected.object.clone(),
            temporary: Some(name.clone()),
        },
    );
    removal.validate()?;
    object_execute(&mut owner, &f, &removal)?;
    assert!(!root.join(&name).exists() && !root.join(&expected.object.key).exists());
    let generated = object_request(
        &f,
        &group,
        2,
        0,
        io::Action::BeginWrite {
            expected: expected.clone(),
            temporary: name,
        },
    );
    assert!(
        generated
            .validate()
            .unwrap_err()
            .is::<crate::catalog_session::store::ResourceLimit>()
    );
    let escaped = format!(
        "{}{}.pending",
        expected.object.key,
        "\"".repeat(crate::catalog_session::ENVELOPE_BYTES / 2)
    );
    let over = object_request(
        &f,
        &group,
        2,
        0,
        io::Action::Remove {
            object: expected.object,
            temporary: Some(escaped),
        },
    );
    over.validate()?;
    let error = crate::filesystem_worker::wire::encode_operation(
        &crate::filesystem_worker::wire::Operation::PreviewIo(over),
    )
    .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<crate::filesystem_worker::wire::Failure>()
            .unwrap()
            .kind,
        crate::filesystem_worker::wire::FailureKind::ResourceLimit
    );
    owner.drain();
    Ok(())
}
