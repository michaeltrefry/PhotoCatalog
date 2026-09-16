use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

#[test]
fn detached_planning_shares_pinned_writer_registry_and_leaves_wal_reads_and_edits_available()
-> Result<()> {
    for cancel_requested in [false, true] {
        let (temp, mut catalog, original) = fixture()?;
        let bytes = vec![7; 192 * 1024];
        std::fs::write(&original, &bytes)?;
        catalog.db.execute(
            "UPDATE assets SET fingerprint=?1 WHERE id='a'",
            [blake3::hash(&bytes).to_hex().to_string()],
        )?;
        catalog.reconcile_export_paths(512)?;
        let job = catalog.begin_photo_export()?;
        let selected = target(temp.path().join("detached.png"));
        let handle = catalog.relink_worker_handle()?;
        let writers = catalog.writers.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        let (entered, blocked) = mpsc::sync_channel(1);
        let (release, resume) = mpsc::sync_channel(1);
        let id = job.id.clone();
        let worker = std::thread::spawn(move || -> Result<()> {
            let mut detached = handle.open()?;
            assert!(Arc::ptr_eq(&writers, &detached.writers));
            let mut held = false;
            let mut observe = |event| {
                if matches!(event, ExportCheckpoint::Hashing { bytes } if bytes > 0) && !held {
                    held = true;
                    entered.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(10)).unwrap();
                }
            };
            let result = detached.append_photo_export_cancellable(
                &id,
                0,
                &selected,
                &output(),
                1024 * 1024,
                1024 * 1024,
                Default::default(),
                &mut ExportControl::observed(&signal, &mut observe),
            );
            assert!(
                result.is_err(),
                "cancel or selected edit must stop authority publication"
            );
            Ok(())
        });
        blocked.recv_timeout(Duration::from_secs(10))?;
        assert_eq!(catalog.photo_export_job(&job.id)?.total, 0);
        assert_eq!(catalog.edit_variant(&VariantKey::master("a"))?.revision, 0);
        if cancel_requested {
            cancel.store(true, Ordering::Release);
        } else {
            catalog.save_edit_recipe(
                &VariantKey::master("a"),
                0,
                &Recipe::V1(RecipeV1 {
                    exposure_ev: 1.,
                    ..Default::default()
                }),
            )?;
        }
        release.send(())?;
        worker.join().unwrap()?;
        assert_eq!(catalog.photo_export_job(&job.id)?.total, 0);
        assert_eq!(std::fs::read(&original)?, bytes);
    }
    Ok(())
}

#[test]
fn cooperative_publication_cancel_retains_intents_and_capture_and_link_wins_cancel() -> Result<()> {
    for boundary in [
        ExportCheckpoint::OriginalVerified,
        ExportCheckpoint::IntentCommitted,
        ExportCheckpoint::Captured,
        ExportCheckpoint::CaptureVerified,
        ExportCheckpoint::Linked,
    ] {
        let (temp, mut catalog, original) = fixture()?;
        let destination = temp.path().join("overwrite.png");
        std::fs::write(&destination, b"prior destination")?;
        let job = catalog.begin_photo_export()?;
        let mut selected = target(destination.clone());
        selected.overwrite = true;
        catalog.append_photo_export(&job.id, 0, &selected, &output(), 1024, 1024)?;
        catalog.seal_photo_export_job(&job.id, 1)?;
        let work = catalog.claim_photo_export(&job.id)?.unwrap();
        let sealed = seal(temp.path(), &work)?;
        catalog.accept_photo_export_seal(&work, &sealed)?;
        let frozen = work.plan.raw().to_owned();
        let cancel = AtomicBool::new(false);
        let mut observed = Vec::new();
        let mut observer = |event| {
            observed.push(event);
            if event == boundary {
                cancel.store(true, Ordering::Release);
            }
        };
        let result = catalog.publish_photo_export_item_cancellable(
            &job.id,
            1,
            &mut ExportControl::observed(&cancel, &mut observer),
        );
        assert!(cancel.load(Ordering::Acquire));
        if boundary == ExportCheckpoint::Linked {
            assert_eq!(result?.0.state, metadata_export::ExportState::Published);
            assert!(observed.contains(&ExportCheckpoint::InstalledVerified));
            assert_eq!(catalog.photo_export_job(&job.id)?.state, "complete");
            assert_eq!(
                std::fs::read(&destination)?,
                b"completed encoded derivative"
            );
        } else {
            assert!(result.is_err());
            assert_eq!(
                catalog.photo_export_items(&job.id, 0, 1)?[0].state,
                "sealed"
            );
            if matches!(
                boundary,
                ExportCheckpoint::Captured | ExportCheckpoint::CaptureVerified
            ) {
                assert!(!destination.exists());
                assert_eq!(
                    std::fs::read(sealed.recovery_directory().join("original"))?,
                    b"prior destination"
                );
                // A separately requested restore also stops during verification,
                // without losing capture or inventing restoration success.
                cancel.store(false, Ordering::Release);
                let mut interrupt = |event| {
                    if matches!(event, ExportCheckpoint::Hashing { bytes } if bytes > 0) {
                        cancel.store(true, Ordering::Release);
                    }
                };
                assert!(
                    catalog
                        .restore_photo_export_item_cancellable(
                            &job.id,
                            1,
                            &mut ExportControl::observed(&cancel, &mut interrupt)
                        )
                        .is_err()
                );
                assert!(!destination.exists());
                cancel.store(false, Ordering::Release);
                let mut after_link = |event| {
                    if event == ExportCheckpoint::Linked {
                        cancel.store(true, Ordering::Release);
                    }
                };
                let restored = catalog.restore_photo_export_item_cancellable(
                    &job.id,
                    1,
                    &mut ExportControl::observed(&cancel, &mut after_link),
                )?;
                assert_eq!(restored.state, metadata_export::ExportState::Restored);
                assert_eq!(std::fs::read(&destination)?, b"prior destination");
            } else {
                assert_eq!(std::fs::read(&destination)?, b"prior destination");
                cancel.store(false, Ordering::Release);
                assert_eq!(
                    catalog
                        .publish_photo_export_item_cancellable(
                            &job.id,
                            1,
                            &mut ExportControl::new(&cancel)
                        )?
                        .0
                        .state,
                    metadata_export::ExportState::Published
                );
            }
        }
        assert_eq!(catalog.photo_export_plan(&job.id, 1)?.0.raw(), frozen);
        assert_eq!(std::fs::read(original)?, b"source bytes unchanged");
    }
    Ok(())
}

#[test]
fn canceled_acceptance_and_alias_candidate_leave_authority_unmodified() -> Result<()> {
    let (temp, mut catalog, original) = fixture()?;
    let (job, work) = queued(&mut catalog, temp.path().join("result.png"))?;
    let sealed = seal(temp.path(), &work)?;
    let cancel = AtomicBool::new(false);
    let mut observer = |event| {
        if matches!(event, ExportCheckpoint::Hashing { bytes } if bytes > 0) {
            cancel.store(true, Ordering::Release);
        }
    };
    assert!(
        catalog
            .accept_photo_export_seal_cancellable(
                &work,
                &sealed,
                &mut ExportControl::observed(&cancel, &mut observer)
            )
            .is_err()
    );
    assert_eq!(
        catalog.photo_export_items(&job.id, 0, 1)?[0].state,
        "rendering"
    );
    let unicode = original.with_file_name("original-é.png");
    std::fs::copy(&original, &unicode)?;
    catalog.db.execute(
        "INSERT INTO assets(id,location,path_display,state,fingerprint,preview_hash,metadata) SELECT 'unicode',?1,'unicode fixture',state,fingerprint,preview_hash,metadata FROM assets WHERE id='a'",
        [crate::location_bytes(&unicode)],
    )?;
    catalog.record_storage_path("unicode", &NativePath::from_path(&unicode))?;
    catalog.reconcile_export_paths(512)?;
    let tx = catalog.db.unchecked_transaction()?;
    let mut candidates = 0;
    assert!(
        crate::catalog_export_alias::protect_destination_with_checkpoint(
            &tx,
            &temp.path().join("different.png"),
            Default::default(),
            &mut || {
                candidates += 1;
                ensure!(
                    candidates < 2,
                    "cancel before candidate filesystem observation"
                );
                Ok(())
            }
        )
        .is_err()
    );
    assert_eq!(candidates, 2);
    Ok(())
}
