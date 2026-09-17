use super::*;

// This isolates hydration ownership from transport timing. The actual configured
// reopen fixture separately covers a running managed read holding staging bytes.
#[test]
fn hydration_leaves_read_owned_ticket_for_read_queue() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let originals = dir.path().join("originals");
    std::fs::create_dir(&originals)?;
    image::RgbImage::from_pixel(2, 2, image::Rgb([1u8, 2, 3])).save(originals.join("one.png"))?;
    let mut catalog = Catalog::open(dir.path().join("catalog"))?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let key = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
    let mut identity = catalog.edit_render_identity(&key)?;
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
        preview::PreviewPolicy::default(),
        preview::ServiceLimits::default(),
    )?;
    let read = service.queue_read_variant(
        &catalog,
        &key,
        preview::Tier::Thumbnail,
        false,
        preview::Priority::Foreground,
        false,
    )?;
    // A stale identity must be reconciled by the existing read owner, not by
    // hydration. If hydration enters this candidate, its existing check fails.
    identity.revision = identity.revision.checked_add(1).unwrap();
    let id = "read-owned".to_owned();
    let touched = Instant::now();
    let mut tickets = HashMap::from([(
        id.clone(),
        Ticket {
            read: Some(read),
            dto: PreviewStatus {
                ticket: id.clone(),
                key: key.clone(),
                revision: I64(identity.revision),
                recipe_digest: identity.recipe_digest.clone(),
                viewport: "grid".into(),
                generation: U64(1),
                state: PreviewState::Queued,
                message: Some("queued cache read".into()),
                diagnostic: None,
            },
            identity,
            consumer: None,
            tier: preview::Tier::Thumbnail,
            interactive: false,
            foreground: true,
            hydration: false,
            diagnostic_started: None,
            original_started: None,
            touched,
            cancel: Cancellation::default(),
        },
    )]);
    let before = serde_json::to_value(&tickets[&id].dto)?;
    let mut hydration = State::default();
    hydration.advance(&mut catalog, &mut service, &mut tickets, None);
    let ticket = &tickets[&id];
    assert_eq!(serde_json::to_value(&ticket.dto)?, before);
    assert_eq!(ticket.read, Some(read));
    assert!(ticket.consumer.is_none());
    assert_eq!(ticket.touched, touched);
    assert!(!ticket.hydration);
    assert_eq!(service.read_queue_usage().queued, 1);
    assert_eq!(service.read_queue_usage().completed, 0);
    assert!(service.active_worker_pids().is_empty());
    assert!(hydration.preparing.is_none());

    // The same unowned candidate still goes through hydration's established
    // identity validation. Excluding every Queued ticket would fail this check.
    assert!(service.cancel_read(read));
    tickets.get_mut(&id).unwrap().read = None;
    hydration.advance(&mut catalog, &mut service, &mut tickets, None);
    let ticket = &tickets[&id];
    assert!(matches!(ticket.dto.state, PreviewState::Stale));
    assert!(
        ticket
            .dto
            .message
            .as_deref()
            .unwrap()
            .contains("recipe or source changed during preparation")
    );
    assert!(ticket.consumer.is_none());
    assert_eq!(service.read_queue_usage().queued, 0);
    assert_eq!(service.read_queue_usage().completed, 0);
    assert!(service.active_worker_pids().is_empty());
    service.try_shutdown()?;
    Ok(())
}

// Exercise the exact managed hydration admission helper with the real shared
// read queue. No transport or native helper is started by this fixture.
#[test]
fn hydrated_read_waits_for_shared_capacity_then_retries() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let originals = dir.path().join("originals");
    std::fs::create_dir(&originals)?;
    image::RgbImage::from_pixel(2, 2, image::Rgb([1u8, 2, 3])).save(originals.join("one.png"))?;
    let mut catalog = Catalog::open(dir.path().join("catalog"))?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let key = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
    let identity = catalog.edit_render_identity(&key)?;
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
        preview::PreviewPolicy::default(),
        preview::ServiceLimits {
            requests: 1,
            ..preview::ServiceLimits::default()
        },
    )?;
    let occupied = service.queue_read_variant(
        &catalog,
        &key,
        preview::Tier::Thumbnail,
        false,
        preview::Priority::Background,
        false,
    )?;
    let id = "waiting-for-capacity".to_owned();
    let touched = Instant::now();
    let mut tickets = HashMap::from([(
        id.clone(),
        Ticket {
            read: None,
            dto: PreviewStatus {
                ticket: id.clone(),
                key: key.clone(),
                revision: I64(identity.revision),
                recipe_digest: identity.recipe_digest.clone(),
                viewport: "grid".into(),
                generation: U64(1),
                state: PreviewState::Queued,
                message: Some("queued cache read".into()),
                diagnostic: None,
            },
            identity,
            consumer: None,
            tier: preview::Tier::Thumbnail,
            interactive: false,
            foreground: true,
            hydration: false,
            diagnostic_started: None,
            original_started: None,
            touched,
            cancel: Cancellation::default(),
        },
    )]);
    assert_eq!(service.available_request_slots(), 0);
    let before = serde_json::to_value(&tickets[&id].dto)?;
    let ticket = tickets.get_mut(&id).unwrap();
    queue_managed_read(
        &catalog,
        &mut service,
        ticket,
        preview::Priority::Foreground,
    )?;
    assert_eq!(serde_json::to_value(&ticket.dto)?, before);
    assert!(matches!(ticket.dto.state, PreviewState::Queued));
    assert!(ticket.read.is_none());
    assert!(ticket.consumer.is_none());
    assert_eq!(ticket.touched, touched);
    assert_eq!(service.read_queue_usage().queued, 1);
    assert!(service.cancel_read(occupied));
    assert_eq!(service.available_request_slots(), 1);
    queue_managed_read(
        &catalog,
        &mut service,
        ticket,
        preview::Priority::Foreground,
    )?;
    let admitted = ticket
        .read
        .expect("capacity release admits the retained ticket");
    assert_ne!(admitted, occupied);
    assert!(matches!(ticket.dto.state, PreviewState::Queued));
    assert_eq!(service.read_queue_usage().queued, 1);
    assert_eq!(service.available_request_slots(), 0);
    assert!(service.cancel_read(admitted));
    assert_eq!(service.available_request_slots(), 1);
    assert!(service.active_worker_pids().is_empty());
    service.try_shutdown()?;
    Ok(())
}
