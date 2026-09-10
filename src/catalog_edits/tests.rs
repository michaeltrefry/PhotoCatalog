use super::*;
use crate::edit::{AdjustmentGroup, NormalizedRect, RecipeV1};
use std::sync::atomic::{AtomicBool, Ordering};

fn fixture() -> Result<(tempfile::TempDir, Catalog)> {
    let temp = tempfile::tempdir()?;
    let catalog = Catalog::open(temp.path().join("catalog"))?;
    for asset in ["a", "b", "c", "tiny"] {
        let size = if asset == "tiny" { 2 } else { 1000 };
        let metadata = serde_json::json!({"format":"PNG","width":size,"height":size,"orientation":1,"camera_make":null,"camera_model":null,"captured_at":null,"lens":null,"preview_source":"fixture"});
        catalog.db.execute("INSERT INTO assets(id,location,path_display,state,metadata) VALUES(?1,?2,?1,'pending',?3)",params![asset,asset.as_bytes(),metadata.to_string()])?;
    }
    Ok((temp, catalog))
}
fn exposed(ev: f32) -> Recipe {
    Recipe::V1(RecipeV1 {
        exposure_ev: ev,
        ..RecipeV1::default()
    })
}
fn exposure(view: &VariantView) -> f32 {
    let Recipe::V1(r) = &view.recipe;
    r.exposure_ev
}

#[test]
fn variants_are_lazy_independent_and_undo_redo_survive_restart() -> Result<()> {
    let (temp, mut catalog) = fixture()?;
    let master = VariantKey::master("a");
    assert_eq!(catalog.edit_variant(&master)?.revision, 0);
    assert_eq!(
        catalog
            .db
            .query_row("SELECT count(*) FROM edit_variants", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    let first = catalog.save_edit_recipe(&master, 0, &exposed(1.0))?;
    let variant = catalog.create_edit_variant(&master, first.revision, "Bright version")?;
    catalog.save_edit_recipe(&master, 1, &exposed(2.0))?;
    assert_eq!(exposure(&catalog.edit_variant(&variant.key)?), 1.0);
    assert!(!catalog.edit_variant(&variant.key)?.can_undo);
    let undone = catalog.undo_edit(&master, 2)?;
    assert_eq!(undone.revision, 3);
    assert_eq!(exposure(&undone), 1.0);
    drop(catalog);
    let mut catalog = Catalog::open(temp.path().join("catalog"))?;
    let current = catalog.edit_variant(&master)?;
    assert!(current.can_redo);
    let redone = catalog.redo_edit(&master, current.revision)?;
    assert_eq!(redone.revision, 4);
    assert_eq!(exposure(&redone), 2.0);
    let undone = catalog.undo_edit(&master, 4)?;
    catalog.save_edit_recipe(&master, undone.revision, &exposed(-1.0))?;
    assert!(!catalog.edit_variant(&master)?.can_redo);
    assert!(catalog.redo_edit(&master, 6).is_err());
    assert_eq!(exposure(&catalog.edit_variant(&variant.key)?), 1.0);
    let history = catalog.edit_history(&master, -1, 200)?;
    assert_eq!(
        history.iter().map(|x| x.revision).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    assert_eq!(catalog.edit_variants("a", 0, 200)?.len(), 2);
    Ok(())
}

#[test]
fn undo_cannot_reuse_an_earlier_render_revision_and_guard_is_atomic() -> Result<()> {
    let (_temp, mut catalog) = fixture()?;
    let master = VariantKey::master("a");
    let initial = catalog.edit_render_identity(&master)?;
    catalog.save_edit_recipe(&master, 0, &exposed(1.0))?;
    let edited = catalog.edit_render_identity(&master)?;
    catalog.undo_edit(&master, 1)?;
    let undo = catalog.edit_render_identity(&master)?;
    assert_eq!(initial.recipe_digest, undo.recipe_digest);
    assert_ne!(initial.revision, undo.revision);
    let called = AtomicBool::new(false);
    assert!(
        catalog
            .with_edit_identity(&initial, || {
                called.store(true, Ordering::SeqCst);
                Ok(())
            })?
            .is_none()
    );
    assert!(
        catalog
            .with_edit_identity(&edited, || {
                called.store(true, Ordering::SeqCst);
                Ok(())
            })?
            .is_none()
    );
    assert!(!called.load(Ordering::SeqCst));
    assert!(catalog.with_edit_identity(&undo, || Ok(17))? == Some(17));
    catalog.db.execute(
        "UPDATE assets SET render_generation=render_generation+1 WHERE id='a'",
        [],
    )?;
    assert!(catalog.with_edit_identity(&undo, || Ok(17))?.is_none());
    Ok(())
}

#[test]
fn invalid_recipe_stale_cas_and_failed_undo_leave_no_partial_history() -> Result<()> {
    let (_temp, mut catalog) = fixture()?;
    let key = VariantKey::master("a");
    assert!(
        catalog
            .save_edit_recipe(&key, 0, &exposed(f32::NAN))
            .is_err()
    );
    assert!(catalog.save_edit_recipe(&key, 99, &exposed(1.0)).is_err());
    assert!(catalog.undo_edit(&key, 0).is_err());
    assert_eq!(
        catalog
            .db
            .query_row("SELECT count(*) FROM edit_variants", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    assert!(catalog.edit_history(&key, -1, 20)?.is_empty());
    Ok(())
}

#[test]
fn copied_groups_are_frozen_and_progress_conflicts_survive_restart() -> Result<()> {
    let (temp, mut catalog) = fixture()?;
    let source = VariantKey::master("a");
    let b = VariantKey::master("b");
    let c = VariantKey::master("c");
    catalog.save_edit_recipe(&source, 0, &exposed(1.0))?;
    let target_recipe = Recipe::V1(RecipeV1 {
        contrast: 0.5,
        ..RecipeV1::default()
    });
    catalog.save_edit_recipe(&b, 0, &target_recipe)?;
    let job = catalog.begin_edit_copy(&source, 1, &[AdjustmentGroup::Exposure])?;
    catalog.append_edit_copy(
        &job.id,
        0,
        &[
            EditTarget {
                key: b.clone(),
                expected_revision: 1,
            },
            EditTarget {
                key: c.clone(),
                expected_revision: 0,
            },
        ],
    )?;
    assert!(
        catalog
            .append_edit_copy(
                &job.id,
                0,
                &[EditTarget {
                    key: c.clone(),
                    expected_revision: 0
                }]
            )
            .is_err()
    );
    catalog.seal_edit_copy(&job.id, 2)?;
    catalog.save_edit_recipe(&source, 1, &exposed(3.0))?;
    catalog.save_edit_recipe(&c, 0, &exposed(-1.0))?;
    let result = catalog.apply_edit_copy_step(&job.id, 1)?;
    assert_eq!(result.completed, 1);
    assert_eq!(result.state, "queued");
    assert_eq!(exposure(&catalog.edit_variant(&b)?), 1.0);
    let Recipe::V1(settings) = catalog.edit_variant(&b)?.recipe;
    assert_eq!(settings.contrast, 0.5);
    drop(catalog);
    let mut catalog = Catalog::open(temp.path().join("catalog"))?;
    assert_eq!(
        catalog.apply_edit_copy_step(&job.id, 100)?.state,
        "complete"
    );
    let rows = catalog.edit_copy_items(&job.id, 0, 200)?;
    assert_eq!(
        rows.iter().map(|x| x.state.as_str()).collect::<Vec<_>>(),
        ["applied", "conflict"]
    );
    assert_eq!(catalog.edit_variant(&b)?.revision, 2);
    catalog.apply_edit_copy_step(&job.id, 100)?;
    assert_eq!(catalog.edit_variant(&b)?.revision, 2);
    assert_eq!(exposure(&catalog.edit_variant(&c)?), -1.0);
    Ok(())
}

#[test]
fn incompatible_crop_and_cancellation_are_explicit() -> Result<()> {
    let (_temp, mut catalog) = fixture()?;
    let source = VariantKey::master("a");
    let recipe = Recipe::V1(RecipeV1 {
        crop: Some(NormalizedRect {
            left: 0.1,
            top: 0.0,
            right: 0.11,
            bottom: 1.0,
        }),
        ..RecipeV1::default()
    });
    catalog.save_edit_recipe(&source, 0, &recipe)?;
    let job = catalog.begin_edit_copy(&source, 1, &[AdjustmentGroup::Geometry])?;
    catalog.append_edit_copy(
        &job.id,
        0,
        &[
            EditTarget {
                key: VariantKey::master("tiny"),
                expected_revision: 0,
            },
            EditTarget {
                key: VariantKey::master("b"),
                expected_revision: 0,
            },
        ],
    )?;
    catalog.seal_edit_copy(&job.id, 2)?;
    catalog.apply_edit_copy_step(&job.id, 1)?;
    assert_eq!(
        catalog.edit_copy_items(&job.id, 0, 1)?[0].state,
        "incompatible"
    );
    catalog.cancel_edit_copy(&job.id)?;
    assert_eq!(
        catalog.apply_edit_copy_step(&job.id, 100)?.state,
        "canceled"
    );
    assert_eq!(catalog.edit_variant(&VariantKey::master("b"))?.revision, 0);
    Ok(())
}

#[test]
fn copy_database_failure_rolls_back_edit_and_progress_for_safe_resume() -> Result<()> {
    let (_temp, mut catalog) = fixture()?;
    let source = VariantKey::master("a");
    let target = VariantKey::master("b");
    catalog.save_edit_recipe(&source, 0, &exposed(1.0))?;
    let job = catalog.begin_edit_copy(&source, 1, &[AdjustmentGroup::Exposure])?;
    catalog.append_edit_copy(
        &job.id,
        0,
        &[EditTarget {
            key: target.clone(),
            expected_revision: 0,
        }],
    )?;
    catalog.seal_edit_copy(&job.id, 1)?;
    catalog.db.execute_batch("CREATE TRIGGER fail_copy BEFORE INSERT ON edit_recipe_nodes WHEN NEW.asset_id='b' BEGIN SELECT RAISE(ABORT,'injected storage failure'); END;")?;
    assert!(catalog.apply_edit_copy_step(&job.id, 1).is_err());
    assert_eq!(catalog.edit_copy_job(&job.id)?.completed, 0);
    assert_eq!(catalog.edit_copy_items(&job.id, 0, 1)?[0].state, "pending");
    assert_eq!(catalog.edit_variant(&target)?.revision, 0);
    catalog.db.execute_batch("DROP TRIGGER fail_copy;")?;
    assert_eq!(catalog.apply_edit_copy_step(&job.id, 1)?.state, "complete");
    assert_eq!(catalog.edit_variant(&target)?.revision, 1);
    Ok(())
}

#[test]
fn schema_five_upgrade_is_lazy_and_keeps_existing_assets() -> Result<()> {
    let (temp, catalog) = fixture()?;
    let before: i64 = catalog
        .db
        .query_row("SELECT count(*) FROM assets", [], |r| r.get(0))?;
    catalog.db.execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE edit_copy_items; DROP TABLE edit_copy_jobs; DROP TABLE edit_changes; DROP TABLE edit_redo_nodes; DROP TABLE edit_recipe_nodes; DROP TABLE edit_variants; PRAGMA user_version=5;")?;
    drop(catalog);
    let catalog = Catalog::open(temp.path().join("catalog"))?;
    assert_eq!(
        catalog
            .db
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        6
    );
    assert_eq!(
        catalog
            .db
            .query_row("SELECT count(*) FROM assets", [], |r| r.get::<_, i64>(0))?,
        before
    );
    assert_eq!(
        catalog
            .db
            .query_row("SELECT count(*) FROM edit_variants", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    assert_eq!(catalog.edit_variant(&VariantKey::master("a"))?.revision, 0);
    Ok(())
}
