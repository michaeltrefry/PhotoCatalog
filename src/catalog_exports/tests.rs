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
