use super::*;
use crate::{
    catalog_session::PhysicalObjectId,
    preview::{Layout, Tier},
};

fn physical(index: u64) -> PhysicalObjectId {
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
fn authority() -> (Binding, RootCapability) {
    let binding = Binding {
        nonce: LeaseId::new(),
        epoch: LeaseId::new(),
    };
    let root = RootCapability {
        epoch: binding.epoch.clone(),
        token: LeaseId::new(),
        session: LeaseId::new(),
        canonical_root: NativePath::from_path(&std::env::temp_dir().join("relay-fs6")),
        root_physical: physical(u64::MAX - 1),
        catalog_physical: physical(u64::MAX - 2),
    };
    (binding, root)
}
fn request(root: &RootCapability, action: store::Action) -> store::Request {
    store::Request {
        root: root.clone(),
        operation: U64(u64::MAX),
        action,
    }
}
fn query(root: &RootCapability) -> store::StatusQuery {
    store::StatusQuery::from(&store::Query {
        root: root.clone(),
        operation: U64(u64::MAX),
        selected: None,
    })
}
fn status(query: &store::StatusQuery) -> store::Status {
    store::Status {
        operation: query.operation,
        group: None,
        stage: None,
        slots: 0,
        owned_bytes: 0,
        selected: None,
    }
}
#[test]
fn store_action_surface_preserves_identity_and_cleanup_cancellation_rules() -> Result<()> {
    let (binding, root) = authority();
    let group = LeaseId::new();
    let token = LeaseId::new();
    let actions = [
        store::Action::Acquire(store::Descriptor {
            identity: "opaque-\"identity\"".into(),
            layout: Layout::HashPrefix,
            roots: [root.canonical_root.clone(), root.canonical_root.clone()],
            relocation: None,
        }),
        store::Action::Reserve {
            group: group.clone(),
            tier: Tier::Large,
            destination: root.canonical_root.clone(),
        },
        store::Action::Lock {
            group: group.clone(),
            reservation: token.clone(),
        },
        store::Action::Promote {
            group: group.clone(),
            target: token.clone(),
            tier: Tier::Large,
        },
        store::Action::Retire {
            group: group.clone(),
            old: token.clone(),
        },
        store::Action::Abandon {
            group,
            reservation: token,
        },
    ];
    for action in actions {
        let request = request(&root, action);
        let call = Call::PreviewStore(Box::new(request.clone()));
        call.validate()?;
        assert_eq!(call.cleanup(), request.is_cleanup());
        assert_eq!(call.cancellable(), !request.is_cleanup());
        let bytes = encode(
            &Packet {
                binding: binding.clone(),
                body: Body::Call {
                    id: U64(u64::MAX),
                    call,
                },
            },
            BYTES,
        )?;
        let Body::Call {
            id,
            call: Call::PreviewStore(decoded),
        } = decode(&binding, &bytes, Lane::Data)?
        else {
            panic!("store call shape")
        };
        assert_eq!(id.0, u64::MAX);
        assert_eq!(*decoded, request);
        assert_eq!(decoded.operation.0, u64::MAX);
        let mut zero = request;
        zero.operation = U64(0);
        assert!(Call::PreviewStore(Box::new(zero)).validate().is_err());
    }
    let config = Call::ReadPreviewConfiguration(root.canonical_root.clone());
    config.validate()?;
    assert!(config.cancellable());
    assert!(!config.cleanup());
    validate_reply(
        &config,
        &Value::Configuration(vec![0; store::CONFIG_BYTES]),
        &binding,
    )?;
    assert!(
        validate_reply(
            &config,
            &Value::Configuration(vec![0; store::CONFIG_BYTES + 1]),
            &binding
        )
        .is_err()
    );
    let over = encode(&vec![0u8; 32], 1).unwrap_err();
    assert_eq!(
        over.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::ResourceLimit
    );
    Ok(())
}
#[test]
fn store_status_snapshot_is_independent_of_admission_and_ordinary_results() -> Result<()> {
    let (binding, root) = authority();
    let mut query = query(&root);
    let mut zero = query.clone();
    zero.operation = U64(0);
    assert!(zero.validate().is_err());
    query.selected = Some(LeaseId::new());
    let mut value = status(&query);
    value.selected = Some(store::Lease {
        token: query.selected.clone().unwrap(),
        tier: Tier::Large,
        path: NativePath::from_path(&std::env::temp_dir().join("x".repeat(20_000))),
    });
    value.validate(&query)?;
    let mut output = Output::default();
    for id in 1..=2 {
        output.push(
            &binding,
            Body::Reply {
                id: U64(id),
                outcome: Ok(Value::Unit),
            },
        )?;
    }
    output.push(
        &binding,
        Body::AdmissionReply {
            id: U64(1),
            value: Ok(None),
        },
    )?;
    output.push(
        &binding,
        Body::Control(Control::Admission {
            id: U64(1),
            operation: U64(1),
            session: root.session.clone(),
        }),
    )?;
    output.push(
        &binding,
        Body::Control(Control::StoreStatus {
            id: U64(1),
            query: query.clone(),
        }),
    )?;
    output.push(
        &binding,
        Body::StoreReply {
            id: U64(1),
            value: Ok(value.clone()),
        },
    )?;
    let reply = output.next(Lane::Store).context("reserved store lane")?;
    assert!(
        reply.bytes.len() > CONTROL_BYTES,
        "selected root must exercise fragmented status, not a tiny control"
    );
    assert!(
        matches!(decode(&binding, &reply.bytes, Lane::Store)?, Body::StoreReply { value: Ok(ref got), .. } if got == &value)
    );
    assert!(decode(&binding, &reply.bytes, Lane::Admission).is_err());
    assert!(output.next(Lane::Admission).is_some());
    assert_eq!(output.data.len(), 2);
    let mut controls = 0;
    while output.next(Lane::Control).is_some() {
        controls += 1;
    }
    assert_eq!(controls, 2);
    Ok(())
}
#[test]
fn store_status_rejects_wrong_operation_selection_epoch_and_unknown_reply() -> Result<()> {
    let (binding, root) = authority();
    let query = query(&root);
    let proxy = Proxy::new(binding.clone());
    proxy.state.lock().unwrap().store_query = Some(StoreQuery {
        id: 7,
        query: query.clone(),
        result: None,
    });
    let send = |binding: Binding, id, value| {
        proxy.receive(
            &encode(
                &Packet {
                    binding,
                    body: Body::StoreReply {
                        id: U64(id),
                        value: Ok(value),
                    },
                },
                BYTES,
            )?,
            Lane::Store,
        )
    };
    let mut bad = status(&query);
    bad.operation = U64(u64::MAX - 1);
    assert!(send(binding.clone(), 7, bad).is_err());
    let mut bad = status(&query);
    bad.selected = Some(store::Lease {
        token: LeaseId::new(),
        tier: Tier::Thumbnail,
        path: root.canonical_root.clone(),
    });
    assert!(send(binding.clone(), 7, bad).is_err());
    let mut stale = binding.clone();
    stale.epoch = LeaseId::new();
    assert!(send(stale, 7, status(&query)).is_err());
    assert!(send(binding.clone(), 8, status(&query)).is_err());
    assert!(
        proxy
            .state
            .lock()
            .unwrap()
            .store_query
            .as_ref()
            .unwrap()
            .result
            .is_none()
    );
    send(binding.clone(), 7, status(&query))?;
    assert!(matches!(
        proxy
            .state
            .lock()
            .unwrap()
            .store_query
            .as_ref()
            .unwrap()
            .result,
        Some(Ok(_))
    ));
    assert!(
        send(binding, 7, status(&query)).is_err(),
        "duplicate adoption must not replace a delivered snapshot"
    );
    Ok(())
}
#[test]
fn delivered_failure_categories_survive_relay_even_after_owner_uncertainty() -> Result<()> {
    let (binding, root) = authority();
    for kind in [
        FailureKind::ResourceLimit,
        FailureKind::Canceled,
        FailureKind::Unknown,
    ] {
        let proxy = Proxy::new(binding.clone());
        proxy.state.lock().unwrap().calls.push(ChildCall {
            id: 1,
            call: Call::ReadPreviewConfiguration(root.canonical_root.clone()),
            outcome: None,
            canceled: false,
            queried: false,
        });
        let fault = Fault::from_error(Failure::new(kind, "é".repeat(4096)).into(), true);
        assert_eq!(fault.kind, kind);
        assert_eq!(fault.unknown, kind == FailureKind::Unknown);
        assert!(fault.message.len() <= 4096);
        let packet = encode(
            &Packet {
                binding: binding.clone(),
                body: Body::Reply {
                    id: U64(1),
                    outcome: Err(fault),
                },
            },
            BYTES,
        )?;
        proxy.receive(&packet, Lane::Data)?;
        let mut state = proxy.state.lock().unwrap();
        let Err(fault) = state.calls[0].outcome.take().unwrap() else {
            panic!("typed failure required")
        };
        let error = fault.into_error();
        assert_eq!(error.downcast_ref::<Failure>().unwrap().kind, kind);
    }
    let dead = Proxy::new(binding);
    dead.fail("real transport loss");
    let error = dead
        .read_preview_configuration(&root.canonical_root, &AtomicBool::new(false))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::Unknown
    );
    let fault = Fault::from_error(store::ResourceLimit("local admission limit").into(), false);
    assert_eq!(fault.kind, FailureKind::ResourceLimit);
    assert!(!fault.unknown);
    Ok(())
}
