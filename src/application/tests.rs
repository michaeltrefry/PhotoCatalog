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

pub(super) fn disconnected() -> Bridge {
    let shared = Arc::new(Shared {
        managed_catalog: false,
        lightroom: Arc::new(Mutex::new(lightroom_bridge::Control::default())),
        exports: Arc::new(Mutex::new(exports::Control::default())),
        relink: Arc::new(Mutex::new(relink::Control::default())),
        copy: Arc::new(Mutex::new(copy::Control::default())),
        backups: Mutex::new(backup::Coordinator::new(Default::default()).unwrap()),
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
            import_status: None,
            import_cancel: None,
        }),
        wake: Condvar::new(),
        limits: Limits::default(),
        binary: Arc::new(AtomicUsize::new(0)),
    });
    Bridge(Arc::new(Handle {
        shared,
        thread: Mutex::new(None),
        shutdown_failure: Mutex::new(None),
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

#[test]
fn import_status_and_cancel_bypass_pending_foreground_work_and_bind_attempt() {
    let b = disconnected();
    let cancel = Cancellation::default();
    {
        let mut q = b.0.shared.queue.lock().unwrap();
        q.status.catalog = Some("catalog".into());
        q.import_cancel = Some(cancel.clone());
        q.import_status = Some(ImportStatus {
            id: "attempt".into(),
            source: NativePath::from_path(std::path::Path::new("/photos")),
            phase: ImportPhase::Discovering,
            imported: U64(3),
            unchanged: U64(2),
            failed: U64(0),
            skipped: U64(1),
            metadata_updated: U64(4),
            metadata_warnings: U64(0),
            awaiting_resources: U64(0),
            pending_previews: 2,
            error: None,
            error_source: None,
        });
    }
    let _pending = b
        .submit(Request::Undo {
            catalog: "catalog".into(),
            key: VariantKey::master("image"),
            expected_revision: I64(0),
        })
        .unwrap();
    assert!(
        b.submit(Request::ImportCancel {
            catalog: "catalog".into(),
            import: "old".into()
        })
        .is_err()
    );
    assert!(!cancel.is_canceled());
    let Reply::Ok {
        value: Response::Import(Some(s)),
    } = b
        .submit(Request::ImportCancel {
            catalog: "catalog".into(),
            import: "attempt".into(),
        })
        .unwrap()
        .recv()
    else {
        panic!("wrong cancellation status")
    };
    assert_eq!(s.phase, ImportPhase::CancelRequested);
    assert_eq!(s.imported, U64(3));
    assert!(cancel.is_canceled());
    assert_eq!(b.0.shared.queue.lock().unwrap().pending.len(), 1);
}

#[test]
fn held_source_hash_allows_foreground_edit_query_cancel_and_close_reopen() -> Result<()> {
    use crate::edit::{Recipe, RecipeV1};
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    let path = originals.join("seed.png");
    image::RgbImage::from_pixel(32, 24, image::Rgb([20u8, 40, 70])).save(&path)?;
    let source_before = std::fs::read(&path)?;
    let root = temp.path().join("catalog");
    let mut catalog = Catalog::open(&root)?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let key = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
    let identity_before = catalog.render_identity(&key.asset_id)?;
    drop(catalog);
    let (entered, events) = std::sync::mpsc::channel();
    let chunks = Arc::new(AtomicUsize::new(0));
    let counted = chunks.clone();
    let bridge = Bridge::spawn(Config {
        worker_executable: std::env::current_exe()?,
        cache_root: None,
        original_roots: vec![originals.clone()],
        preview_policy: preview::PreviewPolicy::default(),
        preview_limits: preview::ServiceLimits::default(),
        limits: Limits::default(),
        import_checkpoint: Some(Arc::new(move |stage, cancel| {
            if stage == "hash_chunk" {
                counted.fetch_add(1, Ordering::AcqRel);
                let _ = entered.send(thread::current().name().unwrap_or_default().to_owned());
                // Preparation cannot proceed until an actual cancel or close signal.
                while !cancel.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        })),
    })?;
    let call = |request| -> Result<Response> {
        let pending = bridge.submit(request)?;
        match pending.receiver.recv_timeout(Duration::from_secs(5))? {
            Reply::Ok { value } => Ok(value),
            Reply::Error { error } => Err(error.into()),
        }
    };
    let Response::Status(opened) = call(Request::OpenExisting {
        path: NativePath::from_path(&root),
    })?
    else {
        anyhow::bail!("open reply")
    };
    let token = opened.catalog.context("catalog token")?;
    let Response::Import(Some(start)) = call(Request::ImportStart {
        catalog: token.clone(),
        source: NativePath::from_path(&originals),
    })?
    else {
        anyhow::bail!("start reply")
    };
    assert_eq!(
        events.recv_timeout(Duration::from_secs(5))?,
        "catalog-source-preparation"
    );
    assert!(crate::ImportLock::acquire(&root.join("import.lock")).is_err());
    assert!(matches!(
        call(Request::Image {
            catalog: token.clone(),
            key: key.clone()
        })?,
        Response::Image(_)
    ));
    let recipe = Recipe::V1(RecipeV1 {
        exposure_ev: 1.0,
        ..RecipeV1::default()
    });
    let Response::Variant(saved) = call(Request::SaveRecipe {
        catalog: token.clone(),
        key: key.clone(),
        expected_revision: I64(0),
        recipe,
    })?
    else {
        anyhow::bail!("save reply")
    };
    assert_eq!(chunks.load(Ordering::Acquire), 1);
    assert!(
        matches!(call(Request::Variant { catalog: token.clone(), key: key.clone() })?, Response::Variant(v) if v.recipe_digest == saved.recipe_digest)
    );
    call(Request::ImportCancel {
        catalog: token.clone(),
        import: start.id,
    })?;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(call(Request::ImportStatus { catalog: token.clone() })?, Response::Import(Some(s)) if s.phase == ImportPhase::Canceled)
        {
            break;
        }
        ensure!(Instant::now() < until, "cancel did not stop source hash");
        thread::sleep(Duration::from_millis(2));
    }
    drop(crate::ImportLock::acquire(&root.join("import.lock"))?);
    call(Request::ImportResume {
        catalog: token.clone(),
        source: NativePath::from_path(&originals),
    })?;
    events.recv_timeout(Duration::from_secs(5))?;
    call(Request::Close { catalog: token })?;
    drop(crate::ImportLock::acquire(&root.join("import.lock"))?);
    let Response::Status(opened) = call(Request::OpenExisting {
        path: NativePath::from_path(&root),
    })?
    else {
        anyhow::bail!("reopen reply")
    };
    assert!(matches!(
        call(Request::ImportStatus {
            catalog: opened.catalog.context("reopened token")?
        })?,
        Response::Import(None)
    ));
    bridge.shutdown();
    let catalog = Catalog::open(&root)?;
    let after = catalog.render_identity(&key.asset_id)?;
    assert_eq!(after.fingerprint, identity_before.fingerprint);
    assert_eq!(after.state, "ready");
    assert_eq!(
        catalog.edit_variant(&key)?.recipe_digest,
        saved.recipe_digest
    );
    assert_eq!(std::fs::read(path)?, source_before);
    assert_eq!(chunks.load(Ordering::Acquire), 2);
    Ok(())
}

#[cfg(unix)]
#[test]
fn close_holds_import_lock_until_owned_native_child_is_drained() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    image::RgbImage::from_pixel(32, 24, image::Rgb([20u8, 40, 70]))
        .save(originals.join("source.png"))?;
    let root = temp.path().join("catalog");
    drop(Catalog::open(&root)?);
    // A non-returning owned executable makes the worker's lifetime deterministic.
    // exec replaces the shell, so cancellation owns and reaps the only child.
    let worker = temp.path().join("held-worker");
    let pid_path = temp.path().join("held-worker.pid");
    std::fs::write(
        &worker,
        br#"#!/bin/sh
printf '%s' "$$" > "$0.pid"
exec /bin/sleep 60
"#,
    )?;
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700))?;
    let released = Arc::new(AtomicBool::new(false));
    let release = released.clone();
    let (entered, events) = std::sync::mpsc::channel();
    let bridge = Bridge::spawn(Config {
        worker_executable: worker,
        cache_root: None,
        original_roots: vec![originals.clone()],
        preview_policy: preview::PreviewPolicy::default(),
        preview_limits: preview::ServiceLimits::default(),
        limits: Limits::default(),
        import_checkpoint: Some(Arc::new(move |stage, _| {
            if stage == "before_service_drop" {
                let _ = entered.send(());
                let until = Instant::now() + Duration::from_secs(5);
                while !release.load(Ordering::Acquire) && Instant::now() < until {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        })),
    })?;
    let call = |request| -> Result<Response> {
        match bridge
            .submit(request)?
            .receiver
            .recv_timeout(Duration::from_secs(5))?
        {
            Reply::Ok { value } => Ok(value),
            Reply::Error { error } => Err(error.into()),
        }
    };
    let Response::Status(opened) = call(Request::OpenExisting {
        path: NativePath::from_path(&root),
    })?
    else {
        anyhow::bail!("open reply")
    };
    let token = opened.catalog.context("catalog token")?;
    call(Request::ImportStart {
        catalog: token.clone(),
        source: NativePath::from_path(&originals),
    })?;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(call(Request::Status)?, Response::Status(s) if s.active_previews > 0) {
            break;
        }
        ensure!(Instant::now() < until, "owned child never became active");
        thread::sleep(Duration::from_millis(2));
    }
    let child: i32 = loop {
        if let Some(child) = std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|text| text.parse().ok())
        {
            break child;
        }
        ensure!(Instant::now() < until, "owned child did not report its pid");
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(unsafe { libc::kill(child, 0) }, 0);
    let close = bridge.submit(Request::Close { catalog: token })?;
    events.recv_timeout(Duration::from_secs(5))?;
    assert!(crate::ImportLock::acquire(&root.join("import.lock")).is_err());
    assert_eq!(unsafe { libc::kill(child, 0) }, 0);
    released.store(true, Ordering::Release);
    assert!(
        matches!(close.receiver.recv_timeout(Duration::from_secs(5))?, Reply::Ok { value: Response::Status(s) } if matches!(s.phase, Phase::Closed) && s.active_previews == 0)
    );
    assert_eq!(unsafe { libc::kill(child, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    drop(crate::ImportLock::acquire(&root.join("import.lock"))?);
    bridge.shutdown();
    Ok(())
}

#[test]
fn canceled_hydration_keeps_blocked_reader_owned_without_blocking_foreground() -> Result<()> {
    use crate::edit::{Recipe, RecipeV1};
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    let path = originals.join("source.png");
    image::RgbImage::from_pixel(32, 24, image::Rgb([20u8, 40, 70])).save(&path)?;
    let root = temp.path().join("catalog");
    let mut c = Catalog::open(&root)?;
    c.import(&originals, None, |_| Ok(()))?;
    let key = VariantKey::master(c.browse(0, 1)?[0].id.clone());
    c.db.execute(
        "UPDATE assets SET state='pending',fingerprint=NULL,metadata=NULL,preview_hash=NULL",
        [],
    )?;
    drop(c);
    let release = Arc::new(AtomicBool::new(false));
    let released = release.clone();
    let (entered, events) = mpsc::channel();
    let bridge = Bridge::spawn(Config {
        worker_executable: std::env::current_exe()?,
        cache_root: None,
        original_roots: vec![originals],
        preview_policy: preview::PreviewPolicy::default(),
        preview_limits: preview::ServiceLimits::default(),
        limits: Limits::default(),
        import_checkpoint: Some(Arc::new(move |stage, _| {
            if stage == "hydration_hash_chunk" {
                let _ = entered.send(());
                let until = Instant::now() + Duration::from_secs(10);
                // Simulate an OS read that does not return immediately on cancellation.
                while !released.load(Ordering::Acquire) && Instant::now() < until {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        })),
    })?;
    let call = |request| -> Result<Response> {
        match bridge
            .submit(request)?
            .receiver
            .recv_timeout(Duration::from_secs(3))?
        {
            Reply::Ok { value } => Ok(value),
            Reply::Error { error } => Err(error.into()),
        }
    };
    let Response::Status(opened) = call(Request::OpenExisting {
        path: NativePath::from_path(&root),
    })?
    else {
        anyhow::bail!("open")
    };
    let token = opened.catalog.context("token")?;
    let Response::Preview(ticket) = call(Request::Preview {
        catalog: token.clone(),
        key: key.clone(),
        tier: PreviewTier::Large,
        interactive: false,
        viewport: "selected".into(),
        generation: U64(1),
        foreground: true,
    })?
    else {
        anyhow::bail!("preview")
    };
    events.recv_timeout(Duration::from_secs(3))?;
    call(Request::CancelPreview {
        catalog: token.clone(),
        ticket: ticket.ticket,
    })?;
    let Response::Variant(saved) = call(Request::SaveRecipe {
        catalog: token.clone(),
        key: key.clone(),
        expected_revision: I64(0),
        recipe: Recipe::V1(RecipeV1 {
            exposure_ev: 1.0,
            ..RecipeV1::default()
        }),
    })?
    else {
        anyhow::bail!("save")
    };
    assert!(matches!(call(Request::Status)?, Response::Status(_)));
    assert!(!release.load(Ordering::Acquire));
    let close = bridge.submit(Request::Close { catalog: token })?;
    assert!(matches!(
        close.receiver.recv_timeout(Duration::from_millis(30)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    release.store(true, Ordering::Release);
    assert!(matches!(
        close.receiver.recv_timeout(Duration::from_secs(3))?,
        Reply::Ok { .. }
    ));
    bridge.shutdown();
    let c = Catalog::open(&root)?;
    assert_eq!(c.edit_variant(&key)?.recipe_digest, saved.recipe_digest);
    assert_eq!(c.render_identity(&key.asset_id)?.state, "pending");
    assert!(c.render_identity(&key.asset_id)?.fingerprint.is_none());
    Ok(())
}

pub(super) fn drain_actor(
    root: &std::path::Path,
) -> Result<(Bridge, Actor, String, Arc<AtomicUsize>)> {
    let originals = root.join("originals");
    std::fs::create_dir(&originals)?;
    let source = originals.join("source.png");
    image::RgbImage::from_pixel(8, 6, image::Rgb([20u8, 40, 70])).save(&source)?;
    let mut catalog = Catalog::open(root.join("catalog"))?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let asset = catalog.browse(0, 1)?[0].id.clone();
    drop(catalog);
    let bridge = disconnected();
    let mut actor = Actor::new(
        Config {
            worker_executable: std::env::current_exe()?,
            cache_root: None,
            original_roots: vec![originals.clone()],
            preview_policy: Default::default(),
            preview_limits: Default::default(),
            limits: Default::default(),
            import_checkpoint: None,
        },
        bridge.0.shared.clone(),
    );
    actor.command(
        Request::OpenExisting {
            path: NativePath::from_path(&root.join("catalog")),
        },
        &Cancellation::default(),
    )?;
    let token = actor.open.as_ref().unwrap().token.clone();
    actor.command(
        Request::ImportStart {
            catalog: token.clone(),
            source: NativePath::from_path(&originals),
        },
        &Cancellation::default(),
    )?;
    let failures = Arc::new(AtomicUsize::new(1));
    let open = actor.open.as_mut().unwrap();
    open.service
        .test_owned_worker(&open.catalog, &asset, &source, failures.clone());
    Ok((bridge, actor, token, failures))
}

#[test]
fn failed_close_retains_catalog_import_and_cache_until_retry() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (bridge, mut actor, token, _) = drain_actor(root.path())?;
    let config = actor
        .open
        .as_ref()
        .unwrap()
        .service
        .cache_configuration()
        .clone();
    let close = || Request::Close {
        catalog: token.clone(),
    };
    assert!(actor.command(close(), &Cancellation::default()).is_err());
    assert!(matches!(actor.status().phase, Phase::Closing));
    let open = actor.open.as_ref().unwrap();
    assert!(open.closing && open.import.as_ref().unwrap().import_lock.is_some());
    assert_eq!(open.service.active_worker_pids().len(), 1);
    assert_eq!(open.service.scheduler_usage().reserved_bytes, 4096);
    assert!(crate::ImportLock::acquire(&root.path().join("catalog/import.lock")).is_err());
    assert!(preview::PreviewStore::open(config.clone(), &[]).is_err());
    let mutation = Request::Create {
        path: NativePath::from_path(&root.path().join("other")),
    };
    assert!(matches!(
        bridge.submit(mutation.clone()),
        Err(BridgeError {
            code: ErrorCode::Busy,
            ..
        })
    ));
    assert!(matches!(
        actor.command(mutation, &Cancellation::default()),
        Err(BridgeError {
            code: ErrorCode::Busy,
            ..
        })
    ));
    actor.maintain();
    assert!(matches!(actor.status().phase, Phase::Closing));
    assert!(
        matches!(actor.command(close(), &Cancellation::default())?, Response::Status(s) if matches!(s.phase, Phase::Closed))
    );
    assert!(actor.open.is_none());
    drop(crate::ImportLock::acquire(
        &root.path().join("catalog/import.lock"),
    )?);
    drop(preview::PreviewStore::open(config, &[])?);
    Ok(())
}

#[test]
fn checked_shutdown_returns_failed_drain_and_retries_same_actor() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (bridge, actor, _, _) = drain_actor(root.path())?;
    *bridge.0.thread.lock().unwrap() = Some(thread::spawn(move || actor.run()));
    assert!(bridge.try_shutdown().is_err());
    assert!(bridge.0.thread.lock().unwrap().is_some());
    let q = bridge.0.shared.queue.lock().unwrap();
    assert!(!q.stopping);
    assert!(matches!(q.status.phase, Phase::Closing));
    drop(q);
    assert!(crate::ImportLock::acquire(&root.path().join("catalog/import.lock")).is_err());
    bridge.try_shutdown()?;
    assert!(bridge.0.thread.lock().unwrap().is_none());
    assert!(matches!(
        bridge.0.shared.queue.lock().unwrap().status.phase,
        Phase::Closed
    ));
    drop(crate::ImportLock::acquire(
        &root.path().join("catalog/import.lock"),
    )?);
    bridge.try_shutdown()?;
    Ok(())
}

fn publication_actor(
    base: &std::path::Path,
    event: crate::import_preparation::Event,
    behavior: crate::import_preparation::TestPublication,
    reference: crate::import_preparation::Reference,
) -> Result<(Bridge, Actor)> {
    let bridge = disconnected();
    let mut actor = Actor::new(
        Config {
            worker_executable: std::env::current_exe()?,
            cache_root: None,
            original_roots: vec![base.join("originals")],
            preview_policy: Default::default(),
            preview_limits: Default::default(),
            limits: Default::default(),
            import_checkpoint: None,
        },
        bridge.0.shared.clone(),
    );
    actor.command(
        Request::OpenExisting {
            path: NativePath::from_path(&base.join("catalog")),
        },
        &Cancellation::default(),
    )?;
    let open = actor.open.as_mut().unwrap();
    let preparation =
        crate::import_preparation::Preparation::publication_test(&open.catalog, event, behavior)?;
    let id = uuid::Uuid::new_v4().to_string();
    open.import = Some(ImportTask {
        import_lock: None,
        preparation: Some(preparation),
        reference: Some(reference),
        status: ImportStatus {
            id: id.clone(),
            source: NativePath::from_path(&base.join("originals")),
            phase: ImportPhase::Discovering,
            imported: U64(0),
            unchanged: U64(0),
            failed: U64(0),
            skipped: U64(0),
            metadata_updated: U64(0),
            metadata_warnings: U64(0),
            awaiting_resources: U64(0),
            pending_previews: 0,
            error: None,
            error_source: None,
        },
        consumers: Vec::new(),
        cancel: Cancellation::default(),
        discovery_finished: false,
        failure: false,
    });
    Ok((bridge, actor))
}

fn publication_fixture(
    base: &std::path::Path,
) -> Result<(crate::import_preparation::Reference, PathBuf, String)> {
    let originals = base.join("originals");
    std::fs::create_dir(&originals)?;
    let originals = originals.canonicalize()?;
    let path = originals.join("source.png");
    image::RgbImage::from_pixel(16, 12, image::Rgb([20u8, 40, 70])).save(&path)?;
    let mut catalog = Catalog::open(base.join("catalog"))?;
    let mut volumes = crate::import_storage::ImportVolumes::new();
    let observation = volumes.observe(&path)?;
    let reference = crate::import_preparation::Reference::begin(
        &mut catalog,
        crate::import_preparation::Header {
            path: path.clone(),
            fingerprint: crate::fingerprint(&path)?,
            observation,
        },
    )?;
    let asset = catalog.db.query_row(
        "SELECT id FROM assets WHERE location=?1",
        [crate::location_bytes(&path)],
        |row| row.get(0),
    )?;
    Ok((reference, path, asset))
}

fn publication_counts(catalog: &Catalog, asset: &str) -> Result<(i64, i64, i64, i64)> {
    Ok((
        catalog.db.query_row(
            "SELECT count(*) FROM metadata_sources WHERE asset_id=?1",
            [asset],
            |row| row.get(0),
        )?,
        catalog.db.query_row(
            "SELECT count(*) FROM metadata_image_observations WHERE image_id=?1",
            [asset],
            |row| row.get(0),
        )?,
        catalog.db.query_row(
            "SELECT count(*) FROM edit_variants WHERE asset_id=?1",
            [asset],
            |row| row.get(0),
        )?,
        catalog.db.query_row(
            "SELECT count(*) FROM metadata_history WHERE asset_id=?1",
            [asset],
            |row| row.get(0),
        )?,
    ))
}

#[test]
fn actor_rejects_stale_sidecar_and_original_before_publication() -> Result<()> {
    for sidecar in [true, false] {
        let root = tempfile::tempdir()?;
        let (reference, path, asset) = publication_fixture(root.path())?;
        let catalog = Catalog::open(root.path().join("catalog"))?;
        let before = publication_counts(&catalog, &asset)?;
        drop(catalog);
        let (event, behavior) = if sidecar {
            let sidecar_path = path.with_extension("xmp");
            let prepared = crate::catalog_metadata::prepare_import_failure(
                crate::catalog_metadata::Source {
                    kind: "sidecar".into(),
                    locator: crate::location_bytes(&sidecar_path),
                    display: sidecar_path.to_string_lossy().into_owned(),
                    ambiguous: false,
                    provenance: serde_json::json!({"fixture":"stale-before-commit"}),
                },
                "unreadable fixture".into(),
                &AtomicBool::new(false),
            )?;
            (
                crate::import_preparation::Event::Source(Box::new(prepared)),
                crate::import_preparation::TestPublication::RejectInspection,
            )
        } else {
            (
                crate::import_preparation::Event::End,
                crate::import_preparation::TestPublication::RejectFile,
            )
        };
        let (_bridge, mut actor) = publication_actor(root.path(), event, behavior, reference)?;
        actor.maintain();
        let open = actor.open.as_ref().unwrap();
        let import = open.import.as_ref().unwrap();
        assert!(import.failure && import.consumers.is_empty());
        assert_eq!(publication_counts(&open.catalog, &asset)?, before);
    }
    Ok(())
}

#[test]
fn actor_lost_postcommit_release_cancels_consumer_and_keeps_receipt() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (reference, _path, asset) = publication_fixture(root.path())?;
    let (_bridge, mut actor) = publication_actor(
        root.path(),
        crate::import_preparation::Event::End,
        crate::import_preparation::TestPublication::LoseFileRelease,
        reference,
    )?;
    actor.maintain();
    let open = actor.open.as_ref().unwrap();
    let import = open.import.as_ref().unwrap();
    assert!(import.failure && import.consumers.is_empty());
    let receipts: i64 = open.catalog.db.query_row(
        "SELECT count(*) FROM metadata_history WHERE asset_id=?1 AND action='import_filesystem_receipt'",
        [&asset],
        |row| row.get(0),
    )?;
    assert_eq!(receipts, 1);
    let state: String =
        open.catalog
            .db
            .query_row("SELECT state FROM assets WHERE id=?1", [&asset], |row| {
                row.get(0)
            })?;
    assert_eq!(state, "pending");
    assert_eq!(open.service.scheduler_usage().reserved_bytes, 0);
    Ok(())
}

#[test]
fn checked_shutdown_reports_owner_panic_on_every_retry() {
    let bridge = disconnected();
    let (tx, rx) = mpsc::sync_channel(1);
    *bridge.0.thread.lock().unwrap() = Some(thread::spawn(move || {
        let _ = tx.send(());
        panic!("injected catalog owner panic");
    }));
    rx.recv().unwrap();
    assert!(bridge.try_shutdown().is_err());
    assert!(bridge.try_shutdown().is_err());
    assert!(bridge.0.shutdown_failure.lock().unwrap().is_some());
}

#[test]
fn owner_unwind_retains_whole_open_when_worker_wait_fails() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (bridge, mut actor, _, _) = drain_actor(root.path())?;
    let config = actor
        .open
        .as_ref()
        .unwrap()
        .service
        .cache_configuration()
        .clone();
    let (tx, rx) = mpsc::sync_channel(1);
    actor.retained_on_drop = Some(tx);
    let t = thread::spawn(move || {
        let _owner = actor;
        panic!("injected actor unwind with active child and import lock");
    });
    assert!(t.join().is_err());
    // The hook transfers the exact whole owner that production deliberately
    // retains, allowing this test to retry cleanup without permanent leaks.
    let mut retained = rx.recv_timeout(Duration::from_secs(5))?;
    assert!(retained.closing);
    assert!(matches!(
        bridge.0.shared.queue.lock().unwrap().status.phase,
        Phase::Closing
    ));
    assert!(retained.import.as_ref().unwrap().import_lock.is_some());
    assert_eq!(retained.service.active_worker_pids().len(), 1);
    assert_eq!(retained.service.scheduler_usage().reserved_bytes, 4096);
    assert!(crate::ImportLock::acquire(&root.path().join("catalog/import.lock")).is_err());
    assert!(preview::PreviewStore::open(config.clone(), &[]).is_err());
    assert_eq!(retained.catalog.browse(0, 1)?.len(), 1);
    retained.service.try_shutdown()?;
    drop(retained);
    drop(crate::ImportLock::acquire(
        &root.path().join("catalog/import.lock"),
    )?);
    drop(preview::PreviewStore::open(config, &[])?);
    Ok(())
}

#[test]
fn managed_catalog_refuses_independent_lightroom_at_submit_and_dispatch() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let mut bridge = disconnected();
    Arc::get_mut(&mut Arc::get_mut(&mut bridge.0).unwrap().shared)
        .unwrap()
        .managed_catalog = true;
    let request = || Request::Lightroom {
        request: Box::new(lightroom_bridge::Request::Open {
            attempt: "synthetic-independent-w".into(),
            root: NativePath::from_path(&temp.path().join("must-not-create")),
            mode: lightroom::OpenMode::Create,
            capture_staging: NativePath::from_path(&temp.path().join("must-not-stage")),
            limits: lightroom::Limits::default().into(),
        }),
    };
    let failure = bridge.submit(request()).err().unwrap();
    assert!(matches!(failure.code, ErrorCode::InvalidRequest));
    assert!(bridge.0.shared.queue.lock().unwrap().pending.is_empty());
    let mut actor = Actor::new(
        Config {
            worker_executable: std::env::current_exe()?,
            cache_root: None,
            original_roots: vec![],
            preview_policy: Default::default(),
            preview_limits: Default::default(),
            limits: Default::default(),
            import_checkpoint: None,
        },
        bridge.0.shared.clone(),
    );
    actor.managed = Some(ManagedCatalogConfig {
        filesystem: crate::catalog_session::unused_filesystem(temp.path())?,
    });
    assert!(matches!(
        actor
            .command(request(), &Cancellation::default())
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    ));
    assert!(!temp.path().join("must-not-create").exists());
    assert!(!temp.path().join("must-not-stage").exists());
    Ok(())
}

#[test]
fn retained_failed_admission_blocks_global_writes_until_explicit_close() -> anyhow::Result<()> {
    for initialized in [false, true] {
        let temp = tempfile::tempdir()?;
        let bridge = disconnected();
        let mut actor = Actor::new(
            Config {
                worker_executable: std::env::current_exe()?,
                cache_root: None,
                original_roots: vec![],
                preview_policy: Default::default(),
                preview_limits: Default::default(),
                limits: Default::default(),
                import_checkpoint: None,
            },
            bridge.0.shared.clone(),
        );
        match crate::catalog_session::retained_admission(temp.path(), initialized)? {
            Ok(session) => actor.failed_session = Some(session),
            Err(cleanup) => actor.failed_admission = Some(cleanup),
        }
        actor.publish_failed_admission("synthetic retained cleanup".into());
        let failed = actor.status();
        assert!(matches!(failed.phase, Phase::Closing));
        assert!(
            failed
                .message
                .as_deref()
                .unwrap()
                .contains("cleanup owner retained")
        );
        let token = failed.catalog.unwrap();
        assert!(
            actor.open.is_none(),
            "cleanup token must not create a usable Open"
        );
        assert!(matches!(
            actor
                .command(
                    Request::Close {
                        catalog: "wrong-admission".into()
                    },
                    &Cancellation::default()
                )
                .unwrap_err()
                .code,
            ErrorCode::StaleSession
        ));
        let bundle = NativePath::from_path(&temp.path().join("must-not-open"));
        for request in [
            Request::BackupInspect {
                bundle: bundle.clone(),
            },
            Request::OpenExisting { path: bundle },
        ] {
            assert!(matches!(
                actor
                    .command(request, &Cancellation::default())
                    .unwrap_err()
                    .code,
                ErrorCode::Busy
            ));
        }
        assert!(actor.shared.backups.lock().unwrap().status()?.is_none());
        assert!(
            matches!(actor.command(Request::Status,&Cancellation::default())?,Response::Status(s) if matches!(s.phase,Phase::Closing) && s.catalog.as_ref()==Some(&token) && s.message.as_deref().unwrap().contains("cleanup owner retained"))
        );
        assert!(matches!(
            bridge
                .submit(Request::BackupInspect {
                    bundle: NativePath::from_path(&temp.path().join("never-open"))
                })
                .err()
                .unwrap()
                .code,
            ErrorCode::Busy
        ));
        assert!(
            matches!(actor.command(Request::Close {catalog:token.clone()},&Cancellation::default())?,Response::Status(s) if matches!(s.phase,Phase::Closed))
        );
        assert!(actor.failed_admission.is_none() && actor.failed_session.is_none());
        assert!(actor.status().catalog.is_none());
        assert!(matches!(
            actor
                .command(Request::Close { catalog: token }, &Cancellation::default())
                .unwrap_err()
                .code,
            ErrorCode::StaleSession
        ));
    }
    Ok(())
}
