//! Upgrade recovery only: no child is launched by these tests.
use super::*;
use crate::edit::{Recipe, RecipeV1};

fn setup(root: &Path) -> (Catalog, PreviewService, String, PathBuf) {
    let originals = root.join("originals");
    std::fs::create_dir(&originals).unwrap();
    let source = originals.join("source.png");
    image::RgbImage::from_pixel(8, 6, image::Rgb([40u8, 70, 100]))
        .save(&source)
        .unwrap();
    let mut catalog = Catalog::open(root.join("catalog")).unwrap();
    catalog.import(&originals, None, |_| Ok(())).unwrap();
    let asset = catalog.browse(0, 1).unwrap()[0].id.clone();
    let previews = PreviewService::open(
        StoreConfig {
            manifest_root: root.join("manifest"),
            thumbnail_root: root.join("thumb"),
            large_root: root.join("large"),
            layout: Layout::Flat,
            thumbnail_bytes: 1024 * 1024,
            large_bytes: 1024 * 1024,
        },
        &[originals],
        std::env::current_exe().unwrap(),
        PreviewPolicy::default(),
        ServiceLimits::default(),
    )
    .unwrap();
    (catalog, previews, asset, source)
}
fn persist(previews: &PreviewService, job: &SavedJob) -> String {
    job.validate().unwrap();
    assert!(
        job.request.validate().is_err(),
        "old renderer cannot launch"
    );
    let id = blake3::hash(&serde_json::to_vec(&job.request.keys).unwrap())
        .to_hex()
        .to_string();
    previews
        .store
        .save_job(&id, &serde_json::to_string(job).unwrap(), 400)
        .unwrap();
    for key in &job.request.keys {
        previews.store.desire(key, || Ok(true)).unwrap();
    }
    id
}
fn import_job(
    catalog: &Catalog,
    previews: &PreviewService,
    asset: &str,
    source: &Path,
) -> SavedJob {
    catalog
        .db
        .execute("UPDATE assets SET state='pending' WHERE id=?1", [asset])
        .unwrap();
    let expected = catalog.render_identity(asset).unwrap();
    let mut key = previews
        .key(
            &expected,
            Tier::Thumbnail,
            &crate::fingerprint(source).unwrap(),
        )
        .unwrap();
    key.renderer_version = "previous-import-renderer".into();
    SavedJob {
        request: RenderWork {
            source: NativePath::from_path(source),
            keys: vec![key],
            encoded_limit: previews.limits.per_worker_encoded_bytes,
            decode_limits: previews.limits.decode_limits,
            edit: None,
        },
        expected,
        edit: None,
        import: true,
        state: JobState::Queued,
    }
}
#[test]
fn old_renderer_queued_import_rekeys_only_after_catalog_guard() {
    let root = tempfile::tempdir().unwrap();
    let (mut catalog, mut previews, asset, source) = setup(root.path());
    let job = import_job(&catalog, &previews, &asset, &source);
    let old_id = persist(&previews, &job);
    let consumers = previews.resume(&mut catalog, 0, 10, true).unwrap().1;
    assert_eq!(consumers.len(), 1);
    assert!(previews.native_work_drained());
    assert_eq!(catalog.render_identity(&asset).unwrap().state, "pending");
    let rows = previews.store.saved_jobs(0, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].1, old_id);
    let new: SavedJob = serde_json::from_str(&rows[0].2).unwrap();
    new.request.validate().unwrap();
    assert!(same_pixels(&job.expected, &new.expected));
    assert_eq!(
        job.request.keys[0].fingerprint,
        new.request.keys[0].fingerprint
    );
}
#[test]
fn old_renderer_edited_jobs_rekey_without_retargeting_recipe_or_undo_revision() {
    for stale in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (mut catalog, mut previews, asset, source) = setup(root.path());
        let variant = VariantKey::master(&asset);
        let recipe = Recipe::V1(RecipeV1 {
            exposure_ev: 1.0,
            ..Default::default()
        });
        catalog.save_edit_recipe(&variant, 0, &recipe).unwrap();
        let edit = catalog.edit_render_identity(&variant).unwrap();
        let mut key = previews.interactive_key(&edit, Tier::Thumbnail).unwrap();
        key.renderer_version = "previous-edit-renderer:proxy1600".into();
        let job = SavedJob {
            request: RenderWork {
                source: NativePath::from_path(&source),
                keys: vec![key],
                encoded_limit: previews.limits.per_worker_encoded_bytes,
                decode_limits: previews.limits.decode_limits,
                edit: Some(EditWork {
                    recipe,
                    recipe_digest: edit.recipe_digest.clone(),
                    limits: previews.edit_limits(),
                    interactive: true,
                    prepared_bytes: 0,
                    prepared: None,
                }),
            },
            expected: edit.source.clone(),
            edit: Some(edit.clone()),
            import: false,
            state: JobState::Queued,
        };
        let mut mismatched = job.clone();
        mismatched.request.edit.as_mut().unwrap().recipe_digest = "b".repeat(64);
        assert!(mismatched.validate().is_err());
        let mut mismatched = job.clone();
        mismatched.request.keys[0].edit_revision += 1;
        assert!(mismatched.validate().is_err());
        let mut mismatched = job.clone();
        mismatched.request.keys[0].renderer_version = "previous-refined-renderer".into();
        assert!(mismatched.validate().is_err());
        persist(&previews, &job);
        if stale {
            let next = catalog
                .save_edit_recipe(&variant, edit.revision, &Recipe::default())
                .unwrap();
            let undo = catalog.undo_edit(&variant, next.revision).unwrap();
            assert_eq!(undo.recipe_digest, edit.recipe_digest);
            assert_ne!(undo.revision, edit.revision);
        }
        let consumers = previews.resume(&mut catalog, 0, 10, true).unwrap().1;
        assert_eq!(consumers.len(), usize::from(!stale));
        let rows = previews.store.saved_jobs(0, 10).unwrap();
        assert_eq!(rows.len(), usize::from(!stale));
        if !stale {
            let new: SavedJob = serde_json::from_str(&rows[0].2).unwrap();
            new.request.validate().unwrap();
            assert!(same_edit(new.edit.as_ref().unwrap(), &edit));
            assert_eq!(new.request.edit.unwrap().recipe_digest, edit.recipe_digest);
        }
    }
}
#[test]
fn old_renderer_attached_import_recovers_before_rekey_and_cleans_committed_journal() {
    for (committed, corrupt) in [(false, false), (true, false), (false, true)] {
        let root = tempfile::tempdir().unwrap();
        let (mut catalog, mut previews, asset, source) = setup(root.path());
        let job = import_job(&catalog, &previews, &asset, &source);
        persist(&previews, &job);
        let key = &job.request.keys[0];
        let decoded = crate::media::decode_full(&source).unwrap();
        let bytes = decoded.srgb_preview(512).unwrap();
        let record = RenderRecord {
            edit_input: None,
            width: decoded.width,
            height: decoded.height,
            metadata: decoded.metadata,
            provenance: decoded.provenance,
        };
        previews
            .store
            .publish_record(key, &bytes, &record, |attach| attach())
            .unwrap();
        if corrupt {
            std::fs::write(
                previews
                    .cache_configuration()
                    .thumbnail_root
                    .join(key.digest().unwrap()),
                b"incomplete",
            )
            .unwrap();
        }
        if committed {
            assert!(
                catalog
                    .commit_preview_import(
                        &job.expected,
                        &key.fingerprint,
                        &record.metadata,
                        &key.digest().unwrap(),
                        || Ok(()),
                        || Ok(()),
                    )
                    .unwrap()
                    .is_some()
            );
        }
        let consumers = previews.resume(&mut catalog, 0, 10, true).unwrap().1;
        if corrupt {
            assert_eq!(consumers.len(), 1);
            assert_eq!(catalog.render_identity(&asset).unwrap().state, "pending");
            assert!(!previews.store.current_is_intact(key).unwrap());
            assert_eq!(previews.store.saved_jobs(0, 10).unwrap().len(), 1);
            continue;
        }
        assert!(consumers.is_empty());
        assert!(previews.store.saved_jobs(0, 10).unwrap().is_empty());
        assert_eq!(catalog.render_identity(&asset).unwrap().state, "ready");
        assert!(previews.store.current_is_intact(key).unwrap());
        let retained: String = catalog
            .db
            .query_row(
                "SELECT preview_hash FROM assets WHERE id=?1",
                [&asset],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, key.digest().unwrap());
        assert!(previews.native_work_drained());
    }
}
