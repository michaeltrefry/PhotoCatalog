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
fn bootstrap(r: &PrepareCatalog, b: &Binding) -> CatalogBootstrap {
    let physical = PhysicalObjectId::Unix {
        device: U64(u64::MAX),
        inode: U64(u64::MAX - 1),
    };
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
            physical: PhysicalObjectId::Unix {
                device: U64(u64::MAX),
                inode: U64(u64::MAX - 2),
            },
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
    Ok(())
}
#[test]
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
