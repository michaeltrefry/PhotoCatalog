//! Native service fence coverage; public Lightroom custody/translation coverage
//! lives in catalog_storage::relink_review::tests.
use super::*;
use anyhow::{Result, ensure};
use photocatalog::{
    catalog_edits::VariantKey,
    catalog_storage::RelinkScope,
    edit::{Recipe, RecipeV1},
};
use rusqlite::params;
fn encoded(path: &NativePath) -> Vec<u8> {
    match path {
        NativePath::UnixBytes(v) => v.clone(),
        NativePath::WindowsWide(v) => v.iter().flat_map(|v| v.to_le_bytes()).collect(),
    }
}
#[test]
fn reviewed_initial_source_renders_requested_copy_preserves_recipe_and_undo_authority() -> Result<()>
{
    let t = tempfile::tempdir()?;
    let root = t.path().join("catalog");
    let old = t.path().join("missing/original.png");
    let new = t.path().join("new/original.png");
    std::fs::create_dir(new.parent().unwrap())?;
    image(&new, [40, 70, 100]);
    let new = new.canonicalize()?;
    let bytes = std::fs::read(&new)?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let mut c = Catalog::open(&root)?;
    let db = rusqlite::Connection::open(root.join("catalog.sqlite3"))?;
    db.execute("INSERT INTO assets(id,location,path_display,state) VALUES('pending-service-source',?1,?2,'pending')",params![encoded(&NativePath::from_path(&old)),old.to_string_lossy()])?;
    c.record_storage_path("pending-service-source", &NativePath::from_path(&old))?;
    let master = VariantKey::master("pending-service-source");
    let copy = c
        .create_edit_variant(&master, 0, "independent selected copy")?
        .key;
    let edit = c.save_edit_recipe(
        &copy,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 1.25,
            ..Default::default()
        }),
    )?;
    let p = c.begin_relink_review(RelinkScope::Asset {
        asset_id: master.asset_id.clone(),
        destinations: vec![NativePath::from_path(&new)],
    })?;
    let p = c.prepare_relink_batch(&p.id, 1)?;
    assert_eq!(p.unverified, 1);
    let p = c.confirm_relink_associations(
        &p.id,
        p.revision,
        p.confirmation_token.as_deref().unwrap(),
        "no_retained_original_digest",
    )?;
    c.apply_relink(&p.id)?;
    let mut service = service(
        &t.path().join("cache"),
        new.parent().unwrap(),
        ServiceLimits::default(),
    );
    let consumer = service.submit_hydration(
        &mut c,
        HydrationRequest {
            variant: &copy,
            source: &NativePath::from_path(&new),
            fingerprint: &hash,
            tier: Tier::Large,
            priority: Priority::Foreground,
            interactive: false,
        },
    )?;
    ensure!(
        matches!(
            await_result(&mut service, &mut c, consumer),
            ServiceCompletion::Ready
        ),
        "selected hydration did not complete"
    );
    let selected = service
        .cached_variant(&c, &copy, Tier::Large, false)?
        .unwrap();
    assert_eq!(selected.key.as_ref().unwrap().variant_id, copy.variant_id);
    assert_eq!(c.edit_variant(&copy)?.recipe_digest, edit.recipe_digest);
    assert_eq!(c.edit_variant(&master)?.revision, 0);
    assert!(
        c.preview(&master.asset_id).is_err(),
        "edited-copy bytes must never be stored as legacy master JPEG"
    );
    // Every publication path, including direct legacy updates, must honor fence.
    assert!(
        db.execute(
            "UPDATE assets SET fingerprint=?2 WHERE id=?1",
            params![master.asset_id, "c".repeat(64)]
        )
        .is_err()
    );
    c.undo_relink(&p.id)?;
    assert_eq!(
        c.get(&master.asset_id)?.original_path,
        old.to_string_lossy()
    );
    assert_eq!(std::fs::read(&new)?, bytes);
    assert_eq!(c.edit_variant(&copy)?.recipe_digest, edit.recipe_digest);
    drop(service);
    Ok(())
}
#[test]
fn changed_reviewed_source_cannot_be_adopted_by_new_hydration_request() -> Result<()> {
    let t = tempfile::tempdir()?;
    let (source, key) = hydration_fixture(t.path());
    let mut c = Catalog::open(t.path().join("catalog"))?;
    let p = c.begin_relink_review(RelinkScope::Asset {
        asset_id: key.asset_id.clone(),
        destinations: vec![NativePath::from_path(&source)],
    })?;
    let p = c.prepare_relink_batch(&p.id, 1)?;
    let p = if p.unverified > 0 {
        c.confirm_relink_associations(
            &p.id,
            p.revision,
            p.confirmation_token.as_deref().unwrap(),
            "no_retained_original_digest",
        )?
    } else {
        p
    };
    c.apply_relink(&p.id)?;
    image(&source, [99, 10, 30]);
    let changed = std::fs::read(&source)?;
    let mut service = service(
        &t.path().join("cache"),
        source.parent().unwrap(),
        ServiceLimits::default(),
    );
    assert!(
        service
            .submit_hydration(
                &mut c,
                HydrationRequest {
                    variant: &key,
                    source: &NativePath::from_path(&source),
                    fingerprint: blake3::hash(&changed).to_hex().as_str(),
                    tier: Tier::Large,
                    priority: Priority::Foreground,
                    interactive: false
                }
            )
            .is_err()
    );
    assert_eq!(c.get(&key.asset_id)?.state, "pending");
    assert_eq!(std::fs::read(&source)?, changed);
    assert!(service.is_drained());
    Ok(())
}
