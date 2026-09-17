//! Actual C-side driver. Uses the same relay and R0 admission as production.
use super::*;
use anyhow::Context;
use std::sync::{atomic::AtomicUsize, mpsc};
static CLOSES: AtomicUsize = AtomicUsize::new(0);
static EXPECT_CLOSES: AtomicBool = AtomicBool::new(false);
unsafe extern "C" fn close_observer(
    _: u32,
    _: *mut std::ffi::c_void,
    _: *mut std::ffi::c_void,
    _: *mut std::ffi::c_void,
) -> i32 {
    CLOSES.fetch_add(1, Ordering::AcqRel);
    0
}
fn observe(db: &Connection) -> Result<()> {
    let result = unsafe {
        rusqlite::ffi::sqlite3_trace_v2(
            db.handle(),
            rusqlite::ffi::SQLITE_TRACE_CLOSE,
            Some(close_observer),
            std::ptr::null_mut(),
        )
    };
    ensure!(
        result == rusqlite::ffi::SQLITE_OK,
        "close observer installation"
    );
    Ok(())
}
pub(crate) fn before_release() -> Result<()> {
    if EXPECT_CLOSES.load(Ordering::Acquire) {
        ensure!(
            CLOSES.load(Ordering::Acquire) == 9,
            "F ReleaseRoot preceded all nine SQL closes"
        );
    }
    Ok(())
}
pub(crate) fn run(
    filesystem: Arc<dyn CatalogFilesystem>,
    request: PrepareCatalog,
    lost_confirm: bool,
    alias: bool,
    before_final_close: impl FnOnce() -> Result<()>,
) -> Result<()> {
    CLOSES.store(0, Ordering::Release);
    EXPECT_CLOSES.store(true, Ordering::Release);
    let admitted = ManagedSession::admit_observed(
        filesystem.clone(),
        &request,
        &AtomicBool::new(false),
        |db| observe(db).expect("fixture trace installation"),
    );
    let mut session = match admitted {
        Ok(s) => s,
        Err(failure) => {
            ensure!(
                lost_confirm && failure.is_poisoned(),
                "unexpected admission failure: {}",
                failure
            );
            ensure!(
                CLOSES.load(Ordering::Acquire) == 0,
                "poison admission closed SQL"
            );
            failure.retire_poisoned()
        }
    };
    ensure!(!lost_confirm, "lost Confirm fixture unexpectedly admitted");
    // Exercise the real session-bound C adapter through G's sibling F. Dropping
    // PreviewStore returns its SQL role; it cannot release any F tier lock.
    let base = request
        .manifest_root
        .to_path()?
        .parent()
        .context("fixture preview parent")?
        .to_path_buf();
    let config = crate::preview::StoreConfig {
        manifest_root: request.manifest_root.to_path()?,
        thumbnail_root: base.join("custody-thumb"),
        large_root: base.join("custody-large"),
        layout: crate::preview::Layout::Flat,
        thumbnail_bytes: 8 * 1024 * 1024,
        large_bytes: 8 * 1024 * 1024,
    };
    let origin = if session.bootstrap.manifest.created {
        crate::preview::ManifestOrigin::CreatedByAdmission
    } else {
        crate::preview::ManifestOrigin::Existing
    };
    let files = session.store_files(Arc::new(AtomicBool::new(false)))?;
    let mut preview = crate::preview::PreviewStore::open_admitted(
        config.clone(),
        session.manifest()?,
        origin,
        files.clone(),
    )?;
    // Cache bytes are deliberately larger than either complete IPC envelope.
    // This fixture exercises regular-file custody only: no decode or native child.
    let cache_key = |generation| crate::preview::PreviewKey {
        image_pixel_generation: None,
        asset_id: "custody-asset".into(),
        variant_id: "master".into(),
        generation,
        fingerprint: "a".repeat(64),
        edit_revision: generation,
        renderer_version: "custody-test-1".into(),
        preparation_version: crate::preview::PREPARATION_VERSION.into(),
        tier: crate::preview::Tier::Thumbnail,
        edge: 256,
        encoding: crate::preview::CodecSettings {
            codec: crate::preview::Codec::Jpeg,
            quality: 65,
        },
    };
    let first = cache_key(1);
    let bytes = vec![0x93; 1024 * 1024 + 7];
    preview.desire(&first, || Ok(true))?;
    ensure!(
        preview.publish(&first, &bytes, |attach| attach())?
            == crate::preview::Publication::Attached,
        "managed publication did not attach"
    );
    ensure!(
        preview
            .read_limited(&first, false, bytes.len() as u64)?
            .context("managed cache miss")?
            .bytes
            == bytes,
        "managed raw byte transport changed bytes"
    );
    ensure!(
        preview
            .read_limited(&first, false, 1)
            .unwrap_err()
            .is::<crate::preview::EncodedBudgetExceeded>(),
        "cache budget admitted full object"
    );
    let current = cache_key(2);
    preview.desire(&current, || Ok(true))?;
    ensure!(
        preview.read(&current, false)?.is_none(),
        "stale cache exposed without fallback"
    );
    ensure!(
        preview
            .read(&current, true)?
            .context("missing stale fallback")?
            .stale,
        "thumbnail fallback lost stale tag"
    );
    ensure!(
        preview.publish(&current, &bytes, |_| Ok(crate::preview::Publication::Stale))?
            == crate::preview::Publication::Stale,
        "stale authorizer attached"
    );
    ensure!(
        preview
            .read(&current, true)?
            .context("stale authorizer removed old current")?
            .bytes
            == bytes,
        "stale compensation changed old bytes"
    );
    let error = preview.publish(&current, &bytes, |attach| {
        attach()?;
        anyhow::bail!("injected catalog commit failure after attachment")
    });
    ensure!(
        error.is_err() && preview.current_is_intact(&current)?,
        "post-attachment failure removed current bytes"
    );
    preview.recover(128)?;
    ensure!(
        preview
            .read(&current, false)?
            .context("post-attachment cache miss")?
            .bytes
            == bytes,
        "recovery changed attached bytes"
    );
    let status = files.cache_status()?;
    ensure!(
        status.kind == store::StatusKind::Objects && status.object.is_some(),
        "object receipt unavailable independently of locks"
    );
    let relocated = base.join("custody-relocated");
    preview.begin_relocation(crate::preview::Tier::Thumbnail, &relocated, &[])?;
    while !preview
        .relocation_step(crate::preview::Tier::Thumbnail, 1, 2 * 1024 * 1024)?
        .complete
    {}
    ensure!(
        preview
            .read(&current, false)?
            .context("relocated cache miss")?
            .bytes
            == bytes,
        "relocation changed immutable cache bytes"
    );
    preview.invalidate(&current)?;
    ensure!(
        preview.read(&current, false)?.is_none(),
        "managed eviction retained current cache"
    );
    drop(preview);
    let tier_locked = |path: &std::path::Path| -> Result<bool> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join(".photocatalog-preview-owner"))?;
        let locked = fs2::FileExt::try_lock_exclusive(&file).is_err();
        if !locked {
            fs2::FileExt::unlock(&file)?;
        }
        Ok(locked)
    };
    ensure!(
        tier_locked(&config.thumbnail_root)?
            && tier_locked(&config.large_root)?
            && tier_locked(&relocated)?,
        "dropping C store released F custody before SQL drain"
    );
    let pool = session.authority.pool().unwrap();
    let discovery = pool.lease(DISCOVERY_ROLE)?;
    observe(&discovery)?;
    drop(discovery);
    pool.joined(DISCOVERY_ROLE, true)?;
    let catalog = session.catalog.as_ref().unwrap();
    ensure!(
        catalog
            .db
            .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?
            == "wal",
        "fixture must test production WAL lock"
    );
    catalog.db.execute_batch("CREATE TABLE relay_overlap_fixture(n INTEGER); INSERT INTO relay_overlap_fixture VALUES(1);")?;
    let handle = catalog.sql_worker_handle(SqlRole::Export)?;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (end_tx, end_rx) = mpsc::sync_channel::<()>(1);
    let worker = std::thread::spawn(move || -> Result<()> {
        let catalog = handle.open()?;
        catalog
            .db
            .execute_batch("BEGIN IMMEDIATE; UPDATE relay_overlap_fixture SET n=2;")?;
        ready_tx.send(())?;
        let _ = end_rx.recv();
        catalog.db.execute_batch("ROLLBACK")?;
        drop(catalog);
        Ok(())
    });
    // Closing the sender on any assertion/error releases the fixture thread.
    let result = (|| -> Result<()> {
        ready_rx.recv_timeout(std::time::Duration::from_secs(15))?;
        let root = session.bootstrap.root_capability();
        if alias {
            let observer = storage::Observer(session.authority.clone());
            let shm = request.root.to_path()?.join("catalog.sqlite3-shm");
            let evidence = observer.evidence(&shm, &AtomicBool::new(false))?;
            ensure!(
                evidence.length > 0,
                "storage F reader did not consume the lock-bearing alias"
            );
            ensure!(
                observer.quick(&shm, &evidence, &AtomicBool::new(false))?,
                "storage quick identity differs"
            );
            ensure!(
                observer.object(&shm, &AtomicBool::new(false))? == evidence.object,
                "storage object identity differs"
            );
            let error = filesystem
                .restore_status(&root)
                .err()
                .context("binary alias must reject")?;
            ensure!(
                error
                    .to_string()
                    .contains("invalid backup/restore control document"),
                "alias did not reach JSON decode: {error}"
            );
            ensure!(session.close().is_err(), "close escaped held export role");
            return Ok(());
        }
        let status = filesystem
            .restore_status(&root)?
            .context("missing private restore marker")?;
        ensure!(status.jobs_held, "fixture restore must begin held");
        ensure!(
            filesystem
                .resume_restored_jobs(&root, &status.receipt.restore_id, false)
                .is_err(),
            "acknowledgment required"
        );
        let resumed = filesystem.resume_restored_jobs(&root, &status.receipt.restore_id, true)?;
        ensure!(!resumed.jobs_held, "release receipt stayed held");
        ensure!(
            !filesystem.restore_status(&root)?.unwrap().jobs_held,
            "release not durable"
        );
        ensure!(session.close().is_err(), "close escaped held export role");
        Ok(())
    })();
    drop(end_tx);
    let joined = worker
        .join()
        .map_err(|_| anyhow::anyhow!("fixture worker panicked"))?;
    joined?;
    ensure!(
        session.close().is_err(),
        "close escaped returned but unjoined export role"
    );
    session.authority.joined(SqlRole::Export, true)?;
    before_final_close()?;
    ensure!(
        tier_locked(&config.thumbnail_root)? && tier_locked(&relocated)?,
        "F released retired or active tier before final SQL close"
    );
    session.close()?;
    ensure!(
        !tier_locked(&config.thumbnail_root)?
            && !tier_locked(&config.large_root)?
            && !tier_locked(&relocated)?,
        "final verified drain retained a tier lock"
    );
    result?;
    Ok(())
}
