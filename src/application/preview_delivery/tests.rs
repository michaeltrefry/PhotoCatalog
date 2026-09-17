use super::*;

fn shared(limit: usize) -> Shared {
    Shared {
        managed_catalog: true,
        lightroom: Arc::new(Mutex::new(lightroom_bridge::Control::default())),
        exports: Arc::new(Mutex::new(exports::Control::default())),
        relink: Arc::new(Mutex::new(relink::Control::default())),
        copy: Arc::new(Mutex::new(copy::Control::default())),
        backups: Mutex::new(backup::Coordinator::new(Default::default()).unwrap()),
        queue: Mutex::new(super::super::Queue {
            pending: VecDeque::new(),
            stopping: false,
            viewport: HashMap::new(),
            ticket_foreground: HashMap::new(),
            status: Status {
                phase: Phase::Closed,
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
        limits: Limits {
            binary_bytes: limit,
            ..Limits::default()
        },
        binary: Arc::new(AtomicUsize::new(0)),
    }
}

#[test]
fn denied_binary_reservation_preserves_sibling_charge_and_retry_capacity() {
    let shared = shared(8);
    let first = BinaryGrant::reserve(&shared, 3).unwrap();
    let sibling = BinaryGrant::reserve(&shared, 3).unwrap();
    assert!(matches!(
        BinaryGrant::reserve(&shared, 3),
        Err(BridgeError {
            code: ErrorCode::ResourceLimit,
            ..
        })
    ));
    assert_eq!(shared.binary.load(Ordering::Acquire), 6);
    drop(first);
    let retry = BinaryGrant::reserve(&shared, 5).unwrap();
    assert_eq!(shared.binary.load(Ordering::Acquire), 8);
    drop(retry);
    assert_eq!(shared.binary.load(Ordering::Acquire), 3);
    drop(sibling);
    assert_eq!(shared.binary.load(Ordering::Acquire), 0);

    shared.binary.store(usize::MAX, Ordering::Release);
    assert!(BinaryGrant::reserve(&shared, 1).is_err());
    assert_eq!(shared.binary.load(Ordering::Acquire), usize::MAX);
    shared.binary.store(0, Ordering::Release);
}

#[test]
fn rejected_payload_shape_releases_only_its_own_binary_grant() {
    let shared = shared(8);
    let sibling = BinaryGrant::reserve(&shared, 2).unwrap();
    let wrong_length = BinaryGrant::reserve(&shared, 3).unwrap();
    assert!(
        wrong_length
            .payload(vec![7, 8], "image/jpeg".into())
            .is_err()
    );
    assert_eq!(shared.binary.load(Ordering::Acquire), 2);

    let mut excess_capacity = Vec::with_capacity(4);
    excess_capacity.extend_from_slice(&[1, 2, 3]);
    let wrong_capacity = BinaryGrant::reserve(&shared, 3).unwrap();
    assert!(
        wrong_capacity
            .payload(excess_capacity, "image/jpeg".into())
            .is_err()
    );
    assert_eq!(shared.binary.load(Ordering::Acquire), 2);
    drop(sibling);
    assert_eq!(shared.binary.load(Ordering::Acquire), 0);
}

#[test]
fn transferred_binary_charge_survives_until_last_payload_owner_drops() {
    let shared = shared(3);
    let grant = BinaryGrant::reserve(&shared, 3).unwrap();
    let bytes = vec![1, 2, 3].into_boxed_slice().into_vec();
    let payload = Arc::new(grant.payload(bytes, "image/jpeg".into()).unwrap());
    assert_eq!(payload.bytes(), &[1, 2, 3]);
    assert_eq!(payload.mime, "image/jpeg");
    let transport = payload.clone();
    drop(payload);
    assert_eq!(shared.binary.load(Ordering::Acquire), 3);
    assert!(BinaryGrant::reserve(&shared, 1).is_err());
    drop(transport);
    assert_eq!(shared.binary.load(Ordering::Acquire), 0);
    let retry = BinaryGrant::reserve(&shared, 3).unwrap();
    drop(retry);
    assert_eq!(shared.binary.load(Ordering::Acquire), 0);
}

#[test]
fn canceled_ready_ticket_retires_queued_delivery_before_any_transfer() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let originals = dir.path().join("originals");
    std::fs::create_dir(&originals)?;
    image::RgbImage::from_pixel(2, 2, image::Rgb([1u8, 2, 3])).save(originals.join("one.png"))?;
    let mut catalog = Catalog::open(dir.path().join("catalog"))?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let key = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
    let identity = catalog.edit_render_identity(&key)?;
    let policy = preview::PreviewPolicy::default();
    let mut service = PreviewService::open(
        preview::StoreConfig {
            manifest_root: dir.path().join("manifest"),
            thumbnail_root: dir.path().join("thumb"),
            large_root: dir.path().join("large"),
            layout: preview::Layout::Flat,
            thumbnail_bytes: 1024 * 1024,
            large_bytes: 1024 * 1024,
        },
        &[originals],
        std::env::current_exe()?,
        policy.clone(),
        preview::ServiceLimits::default(),
    )?;
    let shared = shared(1024);
    let id = "ticket".to_owned();
    let token = "catalog";
    let viewport = "page".to_owned();
    shared
        .queue
        .lock()
        .unwrap()
        .viewport
        .insert((token.into(), viewport.clone()), 1);
    shared.queue.lock().unwrap().ticket_foreground.insert(
        (token.into(), id.clone()),
        TicketPriority {
            foreground: true,
            viewport: viewport.clone(),
            generation: 1,
        },
    );
    let mut tickets = HashMap::from([(
        id.clone(),
        Ticket {
            read: None,
            dto: PreviewStatus {
                ticket: id.clone(),
                key: key.clone(),
                revision: I64(identity.revision),
                recipe_digest: identity.recipe_digest.clone(),
                viewport,
                generation: U64(1),
                state: PreviewState::Ready,
                message: None,
                diagnostic: None,
            },
            identity: identity.clone(),
            consumer: None,
            tier: preview::Tier::Thumbnail,
            interactive: false,
            foreground: true,
            hydration: false,
            diagnostic_started: None,
            original_started: None,
            touched: Instant::now(),
            cancel: Cancellation::default(),
        },
    )]);
    let (tx, rx) = mpsc::sync_channel(1);
    let mut queue = Queue::default();
    let mut ctx = Context {
        service: &mut service,
        catalog: &catalog,
        tickets: &mut tickets,
        token,
        shared: &shared,
        policy: &policy,
    };
    queue.enqueue(&mut ctx, id.clone(), Cancellation::default(), tx);
    assert_eq!(queue.pending.len(), 1);
    // CancelPreview transitions an already-ready ticket without a render consumer.
    ctx.tickets.get_mut(&id).unwrap().dto.state = PreviewState::Canceled;
    queue.advance(&mut ctx);
    assert!(matches!(
        rx.try_recv()?,
        Err(BridgeError {
            code: ErrorCode::Canceled,
            ..
        })
    ));
    assert!(queue.pending.is_empty() && queue.active.is_none());
    assert_eq!(shared.binary.load(Ordering::Acquire), 0);
    assert!(ctx.service.active_worker_pids().is_empty());
    queue.shutdown(ctx.service)?;
    ctx.service.try_shutdown()?;
    Ok(())
}

#[test]
fn typed_transfer_failures_keep_category_through_context_and_release_only_owned_grant() {
    use crate::filesystem_worker::wire::{Failure, FailureKind};
    for (kind, expected) in [
        (FailureKind::Canceled, ErrorCode::Canceled),
        (FailureKind::ResourceLimit, ErrorCode::ResourceLimit),
        (FailureKind::Unknown, ErrorCode::Native),
        (FailureKind::Rejected, ErrorCode::Native),
    ] {
        let shared = shared(8);
        let sibling = BinaryGrant::reserve(&shared, 3).unwrap();
        let owned = BinaryGrant::reserve(&shared, 4).unwrap();
        let failure = anyhow::Error::new(Failure::new(kind, "typed transport result"))
            .context("encoded read");
        let result: std::result::Result<(), BridgeError> = {
            let _owned = owned;
            Err(transfer_error(failure))
        };
        let observed = result.unwrap_err();
        assert_eq!(
            serde_json::to_string(&observed.code).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
        assert_eq!(shared.binary.load(Ordering::Acquire), 3);
        drop(sibling);
        assert_eq!(shared.binary.load(Ordering::Acquire), 0);
    }
    let identity = transfer_error(anyhow::anyhow!("foreign root/session receipt"));
    assert!(matches!(identity.code, ErrorCode::Native));
}
