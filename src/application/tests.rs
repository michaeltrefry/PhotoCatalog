use super::*;
#[test]
fn decimal_and_native_path_wire_are_lossless() {
    for n in [i64::MIN, 0, i64::MAX] {
        let j = serde_json::to_string(&I64(n)).unwrap();
        assert_eq!(serde_json::from_str::<I64>(&j).unwrap(), I64(n));
    }
    assert_eq!(
        serde_json::from_str::<U64>(&format!("\"{}\"", u64::MAX)).unwrap(),
        U64(u64::MAX)
    );
    for bad in [
        "1",
        "\"01\"",
        "\"+1\"",
        "\"-0\"",
        "\"18446744073709551616\"",
    ] {
        assert!(serde_json::from_str::<U64>(bad).is_err());
    }
    for path in [
        NativePath::UnixBytes(vec![47, 255, 128]),
        NativePath::WindowsWide(vec![92, 0xd800, 65]),
    ] {
        let request = Request::OpenExisting { path: path.clone() };
        let wire = serde_json::to_vec(&request).unwrap();
        let Request::OpenExisting { path: restored } = serde_json::from_slice(&wire).unwrap()
        else {
            panic!()
        };
        assert_eq!(restored, path);
    }
}

fn disconnected() -> Bridge {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            pending: VecDeque::new(),
            stopping: false,
            viewport: HashMap::new(),
            ticket_foreground: HashMap::new(),
            status: Status {
                phase: Phase::Opening,
                catalog: None,
                jobs_held: false,
                pending_commands: 0,
                active_previews: 0,
                cancel_requested: false,
                message: None,
            },
            active_cancel: None,
        }),
        wake: Condvar::new(),
        limits: Limits::default(),
        binary: Arc::new(AtomicUsize::new(0)),
    });
    Bridge(Arc::new(Handle {
        shared,
        thread: Mutex::new(None),
    }))
}
fn preview_request(generation: u64, key: &str) -> Request {
    Request::Preview {
        catalog: "catalog".into(),
        key: VariantKey::master(key),
        tier: PreviewTier::Thumbnail,
        interactive: false,
        viewport: "grid".into(),
        generation: U64(generation),
        foreground: false,
    }
}
#[test]
fn viewport_coalescing_priority_and_cancel_status_are_bounded() {
    let b = disconnected();
    let old = b.submit(preview_request(1, "old")).unwrap();
    let _new = b.submit(preview_request(2, "new")).unwrap();
    assert!(matches!(
        old.recv(),
        Reply::Error {
            error: BridgeError {
                code: ErrorCode::Superseded,
                ..
            }
        }
    ));
    for i in 0..40 {
        let _ = b.submit(preview_request(2, &format!("asset{i}"))).unwrap();
    }
    let _save = b
        .submit(Request::Undo {
            catalog: "catalog".into(),
            key: VariantKey::master("new"),
            expected_revision: I64(1),
        })
        .unwrap();
    let q = b.0.shared.queue.lock().unwrap();
    let top = q.pending.iter().min_by_key(|e| e.priority()).unwrap();
    assert!(matches!(top.work, Work::Command(Request::Undo { .. }, _)));
    assert_eq!(q.pending.len(), 42);
    drop(q);
    let c = Cancellation::default();
    b.0.shared.queue.lock().unwrap().active_cancel = Some(c.clone());
    c.cancel();
    let Reply::Ok {
        value: Response::Status(s),
    } = b.submit(Request::Status).unwrap().recv()
    else {
        panic!()
    };
    assert!(s.cancel_requested);
    assert_eq!(s.pending_commands, 42);
    let pending = b
        .submit(Request::Folders {
            catalog: "catalog".into(),
            parent: None,
            after: I64(0),
            limit: 1,
        })
        .unwrap();
    let handle = pending.cancellation();
    std::thread::spawn(move || handle.cancel()).join().unwrap();
    assert!(pending.cancel.is_canceled());
    // The transport cannot raise a background ticket's native priority.
    b.0.shared.queue.lock().unwrap().ticket_foreground.insert(
        ("catalog".into(), "t".into()),
        TicketPriority {
            foreground: false,
            viewport: "grid".into(),
            generation: 2,
        },
    );
    let _bytes = b.preview_bytes("catalog".into(), "t".into(), true).unwrap();
    assert_eq!(
        b.0.shared
            .queue
            .lock()
            .unwrap()
            .pending
            .back()
            .unwrap()
            .priority(),
        4
    );
}
#[test]
fn compact_search_omits_huge_provenance_and_preserves_bounds_and_cursor() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("photos");
    std::fs::create_dir(&originals)?;
    for n in 0..3 {
        image::RgbImage::from_pixel(8, 8, image::Rgb([20u8, 40, 70]))
            .save(originals.join(format!("{n}.png")))?;
    }
    let mut c = Catalog::open(temp.path().join("catalog"))?;
    c.import(&originals, None, |_| Ok(()))?;
    while c.organization_index(20)?.pending {}
    let huge = serde_json::json!({"opaque":"x".repeat(2*1024*1024)}).to_string();
    c.db.execute("UPDATE organization_assets SET provenance=?1", [huge])?;
    let q = Query {
        include_variants: true,
        ..Query::default()
    };
    let full = c.search(&q, None, 1, 3)?;
    let compact = c.search_grid(&q, None, 1, 3, 16384)?;
    assert_eq!(full.rows[0].image_id, compact.rows[0].image_id);
    assert!(compact.rows[0].provenance.is_null());
    assert!(!full.rows[0].provenance.is_null());
    assert_eq!(
        serde_json::to_value(full.next)?,
        serde_json::to_value(compact.next.clone())?
    );
    let next = c.search_grid(&q, compact.next.as_ref(), 2, 3, 16384)?;
    assert_eq!(next.rows.len(), 2);
    c.db.execute(
        "UPDATE organization_assets SET filename=?1",
        ["z".repeat(20000)],
    )?;
    assert!(
        c.search_grid(&q, None, 1, 3, 16384)
            .unwrap_err()
            .to_string()
            .contains("byte admission")
    );
    Ok(())
}

#[test]
fn released_viewports_do_not_accumulate_and_stale_release_preserves_new_generation() {
    let b = disconnected();
    b.0.shared.queue.lock().unwrap().status.catalog = Some("catalog".into());
    for i in 0..200 {
        let viewport = format!("cell{i}");
        let mut r = preview_request(1, "asset");
        if let Request::Preview { viewport: v, .. } = &mut r {
            *v = viewport.clone();
        }
        let pending = b.submit(r).unwrap();
        let released = b
            .submit(Request::ReleaseViewport {
                catalog: "catalog".into(),
                viewport,
                generation: U64(1),
            })
            .unwrap()
            .recv();
        assert!(matches!(
            released,
            Reply::Ok {
                value: Response::Status(_)
            }
        ));
        assert!(matches!(
            pending.recv(),
            Reply::Error {
                error: BridgeError {
                    code: ErrorCode::Superseded,
                    ..
                }
            }
        ));
        let q = b.0.shared.queue.lock().unwrap();
        assert!(q.viewport.is_empty());
        assert!(q.pending.is_empty());
    }
    let _new = b.submit(preview_request(3, "new")).unwrap();
    b.0.shared.queue.lock().unwrap().ticket_foreground.insert(
        ("catalog".into(), "ready".into()),
        TicketPriority {
            foreground: false,
            viewport: "grid".into(),
            generation: 3,
        },
    );
    let bytes = b
        .preview_bytes("catalog".into(), "ready".into(), false)
        .unwrap();
    b.submit(Request::ReleaseViewport {
        catalog: "catalog".into(),
        viewport: "grid".into(),
        generation: U64(2),
    })
    .unwrap()
    .recv();
    {
        let q = b.0.shared.queue.lock().unwrap();
        assert_eq!(q.viewport.get(&("catalog".into(), "grid".into())), Some(&3));
        assert_eq!(q.pending.len(), 2);
    }
    b.submit(Request::ReleaseViewport {
        catalog: "catalog".into(),
        viewport: "grid".into(),
        generation: U64(3),
    })
    .unwrap()
    .recv();
    assert!(matches!(
        bytes.recv(),
        Err(BridgeError {
            code: ErrorCode::Superseded,
            ..
        })
    ));
    assert!(
        b.0.shared
            .queue
            .lock()
            .unwrap()
            .ticket_foreground
            .is_empty()
    );
}

#[test]
fn opaque_cursor_preserves_large_integers_and_rejects_noncanonical_or_wrong_session() {
    let c = Cursor {
        version: 1,
        query_hash: "hash".into(),
        epoch: i64::MAX,
        high_water: i64::MAX,
        sequence: i64::MAX - 1,
        key: crate::organization_search::Key::Integer(i64::MAX - 1),
    };
    let s = encode_cursor("session", &c).unwrap();
    let decoded = decode_cursor(&s, "session").unwrap();
    assert_eq!(decoded.sequence, c.sequence);
    assert!(decode_cursor(&s, "other").is_err());
    assert!(decode_cursor(&format!(" {s}"), "session").is_err());
    assert!(decode_cursor(&"x".repeat(CURSOR_BYTES + 1), "session").is_err());
    let altered = s.replace("\"version\":1", "\"unexpected\":true,\"version\":1");
    assert!(decode_cursor(&altered, "session").is_err());
}
