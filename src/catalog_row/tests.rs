use super::*;
use crate::{Catalog, catalog_edits::VariantKey};
use anyhow::Result;
use rusqlite::{Connection, params};

#[test]
fn borrowed_preview_fields_enforce_byte_bound_and_storage_class() -> Result<()> {
    let db = Connection::open_in_memory()?;
    for maximum in [64, 256, 1024, 64 * 1024, 1024 * 1024] {
        let exact = "x".repeat(maximum);
        assert_eq!(
            db.query_row("SELECT ?", [&exact], |r| text(r, 0, maximum).map(str::len))?,
            maximum
        );
        assert!(
            db.query_row("SELECT ? || 'x'", [&exact], |r| text(r, 0, maximum)
                .map(str::len))
                .is_err()
        );
        assert_eq!(
            db.query_row("SELECT zeroblob(?)", [i64::try_from(maximum)?], |r| blob(
                r, 0, maximum
            )
            .map(<[u8]>::len))?,
            maximum
        );
        assert!(
            db.query_row("SELECT zeroblob(?)", [i64::try_from(maximum + 1)?], |r| {
                blob(r, 0, maximum).map(<[u8]>::len)
            })
            .is_err()
        );
    }
    for expression in ["zeroblob(1)", "42", "NULL", "CAST(x'ff' AS TEXT)"] {
        assert!(
            db.query_row(&format!("SELECT {expression}"), [], |r| owned_text(
                r, 0, 64
            ))
            .is_err()
        );
    }
    assert!(
        db.query_row("SELECT 'x'", [], |r| blob(r, 0, 64).map(<[u8]>::len))
            .is_err()
    );
    assert_eq!(
        db.query_row("SELECT NULL", [], |r| optional_text(r, 0, 64))?,
        None
    );
    assert!(
        db.query_row("SELECT 'é'", [], |r| owned_text(r, 0, 1))
            .is_err()
    );
    assert_eq!(
        db.query_row("SELECT 'é'", [], |r| owned_text(r, 0, 2))?,
        "é"
    );
    Ok(())
}
fn fixture() -> Result<(tempfile::TempDir, Catalog, VariantKey)> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    image::RgbImage::from_pixel(2, 2, image::Rgb([1u8, 2, 3])).save(originals.join("one.png"))?;
    let mut catalog = Catalog::open(temp.path().join("catalog"))?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let master = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
    let variant =
        catalog.create_edit_variant(&master, catalog.edit_variant(&master)?.revision, "Copy")?;
    Ok((temp, catalog, variant.key))
}
fn bound_error(error: &anyhow::Error) {
    let invalid_utf8 = matches!(
        error.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::FromSqlConversionFailure(_, rusqlite::types::Type::Text, cause))
            if cause.downcast_ref::<std::str::Utf8Error>().is_some()
    );
    assert!(
        format!("{error:#}").contains("stored preview field") || invalid_utf8,
        "{error:#}"
    );
}

#[test]
fn preview_path_and_variant_queries_reject_before_decode_then_retry() -> Result<()> {
    let (_temp, catalog, key) = fixture()?;
    let expected = catalog.edit_variant(&key)?;
    let original = catalog.preview_original_path(&key.asset_id)?;
    for (table, column, filter, expression) in [
        ("edit_variants", "label", "id", "hex(zeroblob(513))"),
        ("edit_variants", "label", "id", "CAST(x'ff' AS TEXT)"),
        (
            "edit_recipe_nodes",
            "recipe",
            "variant_id",
            "zeroblob(65537)",
        ),
        ("edit_recipe_nodes", "recipe", "variant_id", "'{}'"),
        (
            "edit_recipe_nodes",
            "digest",
            "variant_id",
            "hex(zeroblob(33))",
        ),
        ("edit_recipe_nodes", "digest", "variant_id", "zeroblob(64)"),
    ] {
        // edit_variant owns its read transaction, so commit this synthetic
        // corruption before calling it; an outer SAVEPOINT would hide the guard.
        let original: rusqlite::types::Value = catalog.db.query_row(
            &format!("SELECT {column} FROM {table} WHERE {filter}=?"),
            [&key.variant_id],
            |row| row.get(0),
        )?;
        assert_eq!(
            catalog.db.execute(
                &format!("UPDATE {table} SET {column}={expression} WHERE {filter}=?"),
                [&key.variant_id],
            )?,
            1,
        );
        bound_error(&catalog.edit_variant(&key).unwrap_err());
        assert!(
            catalog.db.is_autocommit(),
            "failed variant read retained its transaction"
        );
        assert_eq!(
            catalog.db.execute(
                &format!("UPDATE {table} SET {column}=? WHERE {filter}=?"),
                params![original, key.variant_id],
            )?,
            1,
        );
        let retry = catalog.edit_variant(&key)?;
        assert_eq!(retry.label, expected.label);
        assert_eq!(retry.recipe_digest, expected.recipe_digest);
        assert_eq!(retry.recipe, expected.recipe);
    }
    for expression in [
        "hex(zeroblob(524289))",
        "zeroblob(64)",
        "CAST(x'ff' AS TEXT)",
    ] {
        catalog.db.execute_batch("SAVEPOINT corrupt_path")?;
        catalog.db.execute(
            &format!("UPDATE storage_bindings SET native_path={expression} WHERE asset_id=?"),
            [&key.asset_id],
        )?;
        bound_error(&catalog.preview_original_path(&key.asset_id).unwrap_err());
        catalog
            .db
            .execute_batch("ROLLBACK TO corrupt_path; RELEASE corrupt_path")?;
        assert_eq!(catalog.preview_original_path(&key.asset_id)?, original);
    }
    // Drop only the immutable-identity trigger inside this savepoint so a
    // synthetic malformed stored id can reach the reader guard.
    let healthy_image = crate::catalog_images::id(&catalog.db, &key)?;
    catalog.db.execute_batch(
        "SAVEPOINT corrupt_image; PRAGMA defer_foreign_keys=ON; \
             DROP TRIGGER image_identity_immutable",
    )?;
    catalog.db.execute(
        "UPDATE catalog_images SET id=hex(zeroblob(129)) WHERE asset_id=? AND variant_id=?",
        params![key.asset_id, key.variant_id],
    )?;
    bound_error(&crate::catalog_images::id(&catalog.db, &key).unwrap_err());
    catalog
        .db
        .execute_batch("ROLLBACK TO corrupt_image; RELEASE corrupt_image")?;
    assert_eq!(crate::catalog_images::id(&catalog.db, &key)?, healthy_image);
    let immutable = catalog
        .db
        .execute(
            "UPDATE catalog_images SET id=hex(zeroblob(129)) WHERE asset_id=? AND variant_id=?",
            params![key.asset_id, key.variant_id],
        )
        .unwrap_err();
    assert!(
        immutable
            .to_string()
            .contains("logical image identity and ancestry are immutable"),
        "{immutable:#}"
    );
    assert!(catalog.edit_render_identity(&key).is_ok());
    Ok(())
}

#[test]
fn corrupt_source_identity_cannot_publish_and_failed_reads_release_transactions() -> Result<()> {
    let (_temp, mut catalog, key) = fixture()?;
    catalog
        .db
        .execute_batch("PRAGMA ignore_check_constraints=ON")?;
    for (column, expressions) in [
        (
            "fingerprint",
            ["hex(zeroblob(33))", "zeroblob(64)", "CAST(x'ff' AS TEXT)"],
        ),
        (
            "state",
            ["hex(zeroblob(4))", "zeroblob(7)", "CAST(x'ff' AS TEXT)"],
        ),
    ] {
        for expression in expressions {
            let mut expected = catalog.edit_render_identity(&key)?;
            let original: rusqlite::types::Value = catalog.db.query_row(
                &format!("SELECT {column} FROM assets WHERE id=?"),
                [&key.asset_id],
                |r| r.get(0),
            )?;
            catalog.db.execute(
                &format!("UPDATE assets SET {column}={expression} WHERE id=?"),
                [&key.asset_id],
            )?;
            bound_error(&catalog.edit_render_identity(&key).unwrap_err());
            assert!(
                catalog.db.is_autocommit(),
                "failed read retained its transaction"
            );
            bound_error(&catalog.render_identity(&key.asset_id).unwrap_err());
            // Keep the image CAS current so the physical field guard, rather
            // than an earlier stale-image check, decides this publication.
            let image_id = crate::catalog_images::id(&catalog.db, &key)?;
            let image = crate::catalog_images::identity(&catalog.db, &image_id)?;
            expected.source.generation = image.physical_generation;
            expected.image_identity = Some(image);
            let called = std::cell::Cell::new(false);
            let error = catalog
                .with_edit_identity(&expected, || {
                    called.set(true);
                    Ok(())
                })
                .unwrap_err();
            bound_error(&error);
            assert!(!called.get());
            assert!(
                catalog.db.is_autocommit(),
                "failed CAS retained its transaction"
            );
            catalog.db.execute(
                &format!("UPDATE assets SET {column}=? WHERE id=?"),
                params![original, key.asset_id],
            )?;
            let healthy = catalog.edit_render_identity(&key)?;
            assert_eq!(catalog.with_edit_identity(&healthy, || Ok(17))?, Some(17));
        }
    }
    catalog
        .db
        .execute_batch("PRAGMA ignore_check_constraints=OFF")?;
    Ok(())
}
