use super::*;
use crate::edit::RecipeV1;
use crate::image_export::IntegerDepth;

fn fixture() -> Result<(tempfile::TempDir, Catalog, PathBuf)> {
    let temp = tempfile::tempdir()?;
    let original = temp.path().join("original.png");
    std::fs::write(&original, b"source bytes unchanged")?;
    let mut c = Catalog::open(temp.path().join("catalog"))?;
    let fingerprint = blake3::hash(b"source bytes unchanged").to_hex().to_string();
    c.db.execute("INSERT INTO assets(id,location,path_display,state,fingerprint,preview_hash,metadata) VALUES('a',?1,'original','ready',?2,'fixture','{\"format\":\"PNG\",\"width\":1000,\"height\":1000,\"orientation\":1,\"camera_make\":null,\"camera_model\":null,\"captured_at\":null,\"preview_source\":\"fixture\"}')",params![crate::location_bytes(&original),fingerprint])?;
    c.record_storage_path("a", &NativePath::from_path(&original))?;
    Ok((temp, c, original))
}
fn output() -> OutputSpec {
    OutputSpec {
        size: OutputSize::Original,
        format: OutputFormat::Png {
            depth: IntegerDepth::Sixteen,
        },
        profile: OutputProfile::Srgb,
        alpha: AlphaPolicy::Preserve,
    }
}
fn target(path: PathBuf) -> ExportTarget {
    ExportTarget {
        key: VariantKey::master("a"),
        expected_revision: 0,
        destination: path,
        overwrite: false,
        metadata: MetadataSelection::Omit,
    }
}
fn queued(c: &mut Catalog, path: PathBuf) -> Result<(ExportJob, ExportWork)> {
    let j = c.begin_photo_export()?;
    c.append_photo_export(&j.id, 0, &target(path), &output(), 1024, 1024)?;
    c.seal_photo_export_job(&j.id, 1)?;
    let w = c.claim_photo_export(&j.id)?.unwrap();
    Ok((j, w))
}
fn seal(temp: &Path, w: &ExportWork) -> Result<SealedPhotoExport> {
    let stage = temp.join(format!("{}.stage", w.attempt));
    std::fs::write(&stage, b"completed encoded derivative")?;
    metadata_export::seal_photo_export(&w.plan.destination, &stage, 1024, &w.authority, |_| Ok(()))
}
#[test]
fn frozen_job_survives_restart_and_only_accepted_seal_publishes() -> Result<()> {
    let (temp, mut c, original) = fixture()?;
    let destination = temp.path().join("export.png");
    let (j, w) = queued(&mut c, destination.clone())?;
    let sealed = seal(temp.path(), &w)?;
    assert!(c.publish_photo_export_item(&j.id, 1).is_err());
    assert!(!destination.exists());
    c.accept_photo_export_seal(&w, &sealed)?;
    drop(c);
    let mut c = Catalog::open(temp.path().join("catalog"))?;
    let r = c.publish_photo_export_item(&j.id, 1)?;
    assert_eq!(r.state, metadata_export::ExportState::Published);
    assert_eq!(c.photo_export_job(&j.id)?.state, "complete");
    assert_eq!(c.photo_export_job(&j.id)?.completed, 1);
    assert_eq!(std::fs::read(destination)?, b"completed encoded derivative");
    assert_eq!(std::fs::read(original)?, b"source bytes unchanged");
    Ok(())
}
#[test]
fn cancel_blocks_orphan_and_accepted_seal_without_publishing() -> Result<()> {
    for accepted in [false, true] {
        let (temp, mut c, _) = fixture()?;
        let destination = temp.path().join("export.png");
        let (j, w) = queued(&mut c, destination.clone())?;
        let sealed = seal(temp.path(), &w)?;
        if accepted {
            c.accept_photo_export_seal(&w, &sealed)?;
        }
        c.cancel_photo_export_job(&j.id)?;
        assert!(c.accept_photo_export_seal(&w, &sealed).is_err());
        assert!(c.publish_photo_export_item(&j.id, 1).is_err());
        assert!(!destination.exists());
        assert_eq!(
            metadata_export::read_photo_seal(&w.plan.destination, &w.authority)?,
            sealed
        );
    }
    Ok(())
}
#[test]
fn edit_undo_aba_changed_original_and_forged_work_cannot_accept() -> Result<()> {
    for change in ["undo", "source", "forged"] {
        let (temp, mut c, original) = fixture()?;
        let (j, mut w) = queued(&mut c, temp.path().join("export.png"))?;
        let sealed = seal(temp.path(), &w)?;
        match change {
            "undo" => {
                c.save_edit_recipe(
                    &w.plan.identity.key,
                    0,
                    &Recipe::V1(RecipeV1 {
                        exposure_ev: 1.,
                        ..RecipeV1::default()
                    }),
                )?;
                c.undo_edit(&w.plan.identity.key, 1)?;
            }
            "source" => std::fs::write(original, b"external changed bytes")?,
            _ => {
                w.plan.metadata = MetadataSelection::Resolved {
                    expected_revision: 0,
                    base_model: None,
                }
            }
        }
        assert!(c.accept_photo_export_seal(&w, &sealed).is_err());
        assert_eq!(c.photo_export_items(&j.id, 0, 1)?[0].state, "rendering");
    }
    Ok(())
}
#[test]
fn batch_cas_collision_overwrite_and_original_alias_are_rejected_atomically() -> Result<()> {
    let (temp, mut c, original) = fixture()?;
    let j = c.begin_photo_export()?;
    let dest = temp.path().join("export.png");
    c.append_photo_export(&j.id, 0, &target(dest.clone()), &output(), 1024, 1024)?;
    assert!(
        c.append_photo_export(
            &j.id,
            0,
            &target(temp.path().join("other.png")),
            &output(),
            1024,
            1024
        )
        .is_err()
    );
    assert!(
        c.append_photo_export(&j.id, 1, &target(dest), &output(), 1024, 1024)
            .is_err()
    );
    let mut t = target(original.clone());
    t.overwrite = true;
    assert!(
        c.append_photo_export(&j.id, 1, &t, &output(), 1024, 1024)
            .is_err()
    );
    let alias = temp.path().join("alias.png");
    std::fs::hard_link(&original, &alias)?;
    t.destination = alias;
    assert!(
        c.append_photo_export(&j.id, 1, &t, &output(), 1024, 1024)
            .is_err()
    );
    t.destination = temp.path().join("existing.png");
    std::fs::write(&t.destination, b"old")?;
    t.overwrite = false;
    assert!(
        c.append_photo_export(&j.id, 1, &t, &output(), 1024, 1024)
            .is_err()
    );
    assert_eq!(c.photo_export_job(&j.id)?.total, 1);
    assert_eq!(c.photo_export_items(&j.id, 0, 200)?.len(), 1);
    Ok(())
}
#[test]
fn publication_conflict_is_not_reported_as_published() -> Result<()> {
    let (temp, mut c, _) = fixture()?;
    let dest = temp.path().join("export.png");
    let (j, w) = queued(&mut c, dest.clone())?;
    let sealed = seal(temp.path(), &w)?;
    c.accept_photo_export_seal(&w, &sealed)?;
    std::fs::write(&dest, b"external file")?;
    let receipt = c.publish_photo_export_item(&j.id, 1)?;
    assert_eq!(receipt.state, metadata_export::ExportState::Conflict);
    let items = c.photo_export_items(&j.id, 0, 1)?;
    assert_eq!(items[0].state, "failed");
    assert!(items[0].error.is_some());
    assert_eq!(std::fs::read(dest)?, b"external file");
    Ok(())
}

#[test]
fn export_state_seeks_do_not_scan_pending_or_completed_prefixes() -> Result<()> {
    for count in [100, 5000] {
        let (_temp, mut c, _) = fixture()?;
        let job = c.begin_photo_export()?;
        let tx = c.db.transaction()?;
        for sequence in 1..=count {
            tx.execute("INSERT INTO photo_export_items(job,sequence,destination,plan,authority,state) VALUES(?1,?2,?3,'{}','fixture',?4)",params![job.id,sequence,sequence.to_string(),if sequence%2==0{"pending"}else{"published"}])?;
        }
        tx.commit()?;
        assert_eq!(c.next_sealed_photo_export(&job.id)?, None);
        let mut query = c.db.prepare(NEXT_SEALED)?;
        assert!(query.query([&job.id])?.next()?.is_none());
        assert!(query.get_status(rusqlite::StatementStatus::VmStep) < 50);
        assert_eq!(query.get_status(rusqlite::StatementStatus::FullscanStep), 0);
        c.db.execute(
            "UPDATE photo_export_items SET state='published' WHERE job=?1",
            [&job.id],
        )?;
        c.db.execute(
            "UPDATE photo_export_items SET state='pending' WHERE job=?1 AND sequence=?2",
            params![job.id, count],
        )?;
        let mut query = c.db.prepare(NEXT_PENDING)?;
        assert_eq!(query.query_row([&job.id], |r| r.get::<_, i64>(0))?, count);
        assert!(query.get_status(rusqlite::StatementStatus::VmStep) < 50);
        assert_eq!(query.get_status(rusqlite::StatementStatus::FullscanStep), 0);
        assert_eq!(query.get_status(rusqlite::StatementStatus::Sort), 0);
    }
    Ok(())
}

fn accepted_overwrite(
    c: &mut Catalog,
    temp: &Path,
) -> Result<(ExportJob, ExportWork, SealedPhotoExport)> {
    let destination = temp.join("replacement.png");
    std::fs::write(&destination, b"old destination bytes")?;
    let j = c.begin_photo_export()?;
    let mut target = target(destination);
    target.overwrite = true;
    c.append_photo_export(&j.id, 0, &target, &output(), 1024, 1024)?;
    c.seal_photo_export_job(&j.id, 1)?;
    let work = c.claim_photo_export(&j.id)?.unwrap();
    let sealed = seal(temp, &work)?;
    c.accept_photo_export_seal(&work, &sealed)?;
    Ok((j, work, sealed))
}

#[test]
fn queued_export_rechecks_same_inode_restored_mtime_and_atomic_replacement() -> Result<()> {
    use std::{fs, sync::mpsc, time::Duration};
    for publication in [false, true] {
        for replacement in [false, true] {
            let (temp, mut c, original) = fixture()?;
            let destination = temp.path().join("result.png");
            let (j, w) = queued(&mut c, destination.clone())?;
            let sealed = seal(temp.path(), &w)?;
            if publication {
                c.accept_photo_export_seal(&w, &sealed)?;
            }
            let original_bytes = fs::read(&original)?;
            let modified = fs::metadata(&original)?.modified()?;
            let mut worker = Catalog::open(&c.root)?;
            let gate = c.writers.clone();
            let permit = gate.enter(Priority::Background)?;
            let (sent, ready) = mpsc::channel();
            let job = j.id.clone();
            let child = std::thread::spawn(move || -> Result<()> {
                let mut signal = Some(sent);
                let hook = |phase| -> Result<()> {
                    if phase == PhotoExportBoundary::OriginalVerified
                        && let Some(sent) = signal.take()
                    {
                        sent.send(())?;
                    }
                    Ok(())
                };
                if publication {
                    worker
                        .publish_photo_export_item_with_hook(&job, 1, hook)
                        .map(|_| ())
                } else {
                    worker.accept_photo_export_seal_with_hook(&w, &sealed, hook)
                }
            });
            ready.recv_timeout(Duration::from_secs(5))?;
            gate.wait_until_queued(1, 0);
            if replacement {
                fs::rename(&original, temp.path().join("previous-original"))?;
            }
            let mut changed = original_bytes.clone();
            changed[0] ^= 1;
            fs::write(&original, &changed)?;
            fs::File::options()
                .write(true)
                .open(&original)?
                .set_times(fs::FileTimes::new().set_modified(modified))?;
            drop(permit);
            let error = child
                .join()
                .unwrap()
                .expect_err("stale verified file was authorized");
            assert!(
                format!("{error:#}").contains("verified file changed"),
                "{error:#}"
            );
            assert!(!destination.exists());
            assert_eq!(fs::read(&original)?, changed);
        }
    }
    Ok(())
}

#[test]
fn every_bulk_hash_checkpoint_allows_an_unrelated_catalog_writer() -> Result<()> {
    let (temp, mut c, _) = fixture()?;
    let (j, w, sealed) = accepted_overwrite(&mut c, temp.path())?;
    let mut unrelated = Catalog::open(&c.root)?;
    let mut writes = 0;
    let (receipt, metrics) = c.publish_photo_export_item_with_hook(&j.id, 1, |phase| {
        if phase == PhotoExportBoundary::Hashing {
            // Fails with recursive admission/SQLITE_BUSY if any whole-file scan
            // is moved back inside the global catalog writer transaction.
            unrelated.begin_photo_export()?;
            writes += 1;
        }
        Ok(())
    })?;
    assert_eq!(receipt.state, metadata_export::ExportState::Published);
    assert!(writes >= 12, "missing bulk verification checkpoints");
    assert_eq!(metrics.authority_intervals_ms.len(), 4);
    assert!(metrics.total_ms >= metrics.original_hash_ms);
    assert_eq!(
        std::fs::read(w.plan.destination.destination)?,
        b"completed encoded derivative"
    );
    assert_eq!(
        metadata_export::read_photo_seal(&sealed.snapshot, &sealed.authority_digest)?,
        sealed
    );
    Ok(())
}

#[test]
fn cancellation_after_capture_preserves_bytes_and_allows_explicit_restore_without_payload()
-> Result<()> {
    let (temp, mut c, original) = fixture()?;
    let source = std::fs::read(&original)?;
    let (j, w, sealed) = accepted_overwrite(&mut c, temp.path())?;
    let mut other = Catalog::open(&c.root)?;
    let result = c.publish_photo_export_item_with_hook(&j.id, 1, |phase| {
        if phase == PhotoExportBoundary::Captured {
            other.cancel_photo_export_job(&j.id)?;
        }
        Ok(())
    });
    assert!(
        !result
            .as_ref()
            .is_ok_and(|r| r.0.state == metadata_export::ExportState::Published)
    );
    assert!(!w.plan.destination.destination.exists());
    assert_eq!(
        std::fs::read(sealed.recovery_directory().join("original"))?,
        b"old destination bytes"
    );
    // A corrupt/missing payload is not permission to strand intact captured bytes.
    std::fs::remove_file(sealed.recovery_directory().join("payload"))?;
    let restored = c.restore_photo_export_item(&j.id, 1)?;
    assert_eq!(restored.state, metadata_export::ExportState::Restored);
    assert_eq!(
        std::fs::read(&w.plan.destination.destination)?,
        b"old destination bytes"
    );
    assert_eq!(std::fs::read(original)?, source);
    assert_eq!(c.photo_export_job(&j.id)?.state, "canceled");
    Ok(())
}

#[test]
fn namespace_mutation_is_not_excused_as_our_rename_or_hardlink_timestamp() -> Result<()> {
    use std::fs;
    for phase in [PhotoExportBoundary::Captured, PhotoExportBoundary::Linked] {
        let (temp, mut c, _) = fixture()?;
        let (j, w, sealed) = accepted_overwrite(&mut c, temp.path())?;
        let altered = if phase == PhotoExportBoundary::Captured {
            sealed.recovery_directory().join("original")
        } else {
            w.plan.destination.destination.clone()
        };
        let result = c.publish_photo_export_item_with_hook(&j.id, 1, |at| {
            if at == phase {
                let modified = fs::metadata(&altered)?.modified()?;
                let mut bytes = fs::read(&altered)?;
                bytes[0] ^= 1;
                fs::write(&altered, bytes)?;
                fs::File::options()
                    .write(true)
                    .open(&altered)?
                    .set_times(fs::FileTimes::new().set_modified(modified))?;
            }
            Ok(())
        });
        assert!(result.is_err(), "mutation was hidden by a refreshed stamp");
        assert_ne!(c.photo_export_items(&j.id, 0, 1)?[0].state, "published");
        assert!(altered.exists(), "externally changed bytes were discarded");
    }
    Ok(())
}

#[test]
fn publication_crash_child() -> Result<()> {
    let Ok(root) = std::env::var("PHOTOCATALOG_TEST_PUBLICATION_CRASH") else {
        return Ok(());
    };
    let job = std::env::var("PHOTOCATALOG_TEST_PUBLICATION_JOB")?;
    let mut c = Catalog::open(root)?;
    c.publish_photo_export_item_with_hook(&job, 1, |phase| {
        if phase == PhotoExportBoundary::Linked {
            std::process::exit(73);
        }
        Ok(())
    })?;
    anyhow::bail!("child never reached linked boundary")
}

#[test]
fn actual_crash_after_link_finalizes_committed_intent_despite_later_edit_cancel_and_offline_source()
-> Result<()> {
    let (temp, mut c, original) = fixture()?;
    let (j, w, _) = accepted_overwrite(&mut c, temp.path())?;
    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "catalog_exports::tests::publication_crash_child",
            "--nocapture",
        ])
        .env("PHOTOCATALOG_TEST_PUBLICATION_CRASH", &c.root)
        .env("PHOTOCATALOG_TEST_PUBLICATION_JOB", &j.id)
        .status()?;
    assert_eq!(status.code(), Some(73));
    assert_eq!(c.photo_export_items(&j.id, 0, 1)?[0].state, "sealed");
    let intent: Option<String> = c.db.query_row(
        "SELECT publication FROM photo_export_items WHERE job=?1 AND sequence=1",
        [&j.id],
        |r| r.get(0),
    )?;
    assert!(intent.is_some(), "link happened before durable intent");
    let linked = std::fs::read(&w.plan.destination.destination)?;
    c.save_edit_recipe(
        &w.plan.identity.key,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 2.,
            ..Default::default()
        }),
    )?;
    c.cancel_photo_export_job(&j.id)?;
    std::fs::rename(original, temp.path().join("offline-original"))?;
    drop(c);
    let mut c = Catalog::open(temp.path().join("catalog"))?;
    let receipt = c.publish_photo_export_item(&j.id, 1)?;
    assert_eq!(receipt.state, metadata_export::ExportState::Published);
    assert_eq!(std::fs::read(&w.plan.destination.destination)?, linked);
    assert_eq!(c.photo_export_job(&j.id)?.completed, 1);
    assert_eq!(c.photo_export_job(&j.id)?.state, "canceled");
    c.publish_photo_export_item(&j.id, 1)?;
    assert_eq!(
        c.photo_export_job(&j.id)?.completed,
        1,
        "recovery counted twice"
    );
    Ok(())
}

#[test]
fn uninstalled_intent_rechecks_stale_recipe_and_never_uses_it_as_publication_permission()
-> Result<()> {
    let (temp, mut c, _) = fixture()?;
    let (j, w, _) = accepted_overwrite(&mut c, temp.path())?;
    let result = c.publish_photo_export_item_with_hook(&j.id, 1, |phase| {
        if phase == PhotoExportBoundary::IntentCommitted {
            anyhow::bail!("simulated interruption before namespace");
        }
        Ok(())
    });
    assert!(result.is_err());
    let before = std::fs::read(&w.plan.destination.destination)?;
    c.save_edit_recipe(
        &w.plan.identity.key,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 1.,
            ..Default::default()
        }),
    )?;
    assert!(c.publish_photo_export_item(&j.id, 1).is_err());
    assert_eq!(std::fs::read(&w.plan.destination.destination)?, before);
    Ok(())
}

#[test]
fn interrupted_restore_and_repeated_restore_converge_without_clobber_or_double_count() -> Result<()>
{
    let (temp, mut c, original) = fixture()?;
    let (j, w, sealed) = accepted_overwrite(&mut c, temp.path())?;
    let mut other = Catalog::open(&c.root)?;
    let result = c.publish_photo_export_item_with_hook(&j.id, 1, |phase| {
        if phase == PhotoExportBoundary::Captured {
            other.cancel_photo_export_job(&j.id)?;
        }
        Ok(())
    });
    assert!(
        !result
            .as_ref()
            .is_ok_and(|r| r.0.state == metadata_export::ExportState::Published)
    );
    // Inject loss of the executor after the no-clobber restore link, before
    // durability/final catalog publication. No cleanup or receipt is performed.
    {
        let mut interrupted = metadata_export::PhotoPublication::prepare_restore(&sealed)?;
        interrupted.restore_link()?;
    }
    assert_eq!(c.photo_export_items(&j.id, 0, 1)?[0].state, "failed");
    std::fs::remove_file(sealed.recovery_directory().join("payload"))?;
    drop(other);
    drop(c);
    let mut c = Catalog::open(temp.path().join("catalog"))?;
    for _ in 0..2 {
        let receipt = c.restore_photo_export_item(&j.id, 1)?;
        assert_eq!(receipt.state, metadata_export::ExportState::Restored);
        assert_eq!(c.photo_export_items(&j.id, 0, 1)?[0].state, "restored");
        assert_eq!(c.photo_export_job(&j.id)?.completed, 1);
        assert_eq!(
            std::fs::read(&w.plan.destination.destination)?,
            b"old destination bytes"
        );
    }
    // Equal bytes from a replacement inode are not evidence of our restored link.
    std::fs::rename(
        &w.plan.destination.destination,
        temp.path().join("restored-owned"),
    )?;
    std::fs::write(&w.plan.destination.destination, b"old destination bytes")?;
    let receipt = c.restore_photo_export_item(&j.id, 1)?;
    assert_eq!(receipt.state, metadata_export::ExportState::Conflict);
    assert_eq!(
        std::fs::read(&w.plan.destination.destination)?,
        b"old destination bytes"
    );
    assert_eq!(std::fs::read(original)?, b"source bytes unchanged");
    Ok(())
}
