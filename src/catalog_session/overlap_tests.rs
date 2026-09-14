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
        thumbnail_bytes: 1024,
        large_bytes: 1024,
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
        files,
    )?;
    let relocated = base.join("custody-relocated");
    preview.begin_relocation(crate::preview::Tier::Thumbnail, &relocated, &[])?;
    while !preview
        .relocation_step(crate::preview::Tier::Thumbnail, 1, 1024)?
        .complete
    {}
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
