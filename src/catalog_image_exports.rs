//! Image-scoped XMP exports alongside unchanged legacy export authorities.
use crate::{
    Catalog, catalog_edits::VariantKey, catalog_images::ImageMetadataIdentity,
    catalog_metadata::MetadataExportPlan, catalog_writer::Priority,
};
use anyhow::{Result, ensure};
use flate2::{Compression, write::ZlibEncoder};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use std::{io::Write, path::Path};

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS metadata_image_export_authorities(
        operation TEXT PRIMARY KEY REFERENCES metadata_export_plans(operation),
        image_identity TEXT NOT NULL CHECK(length(image_identity)<=32768));",
    )?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct ImageMetadataExportPlan {
    pub image_identity: ImageMetadataIdentity,
    pub export: MetadataExportPlan,
}

/// Compare metadata authority without interpreting an unavailable image as a DB
/// error. Real SQLite/JSON errors propagate and never trigger an automatic restore.
pub(crate) fn current(
    db: &Connection,
    operation: &str,
    asset: &str,
    revision: i64,
) -> Result<bool> {
    let encoded: Option<String> = db
        .query_row(
            "SELECT image_identity FROM metadata_image_export_authorities WHERE operation=?1",
            [operation],
            |r| r.get(0),
        )
        .optional()?;
    let Some(encoded) = encoded else {
        let current:i64=db.query_row("SELECT COALESCE(m.revision,0) FROM assets a LEFT JOIN metadata_assets m ON m.asset_id=a.id WHERE a.id=?1",[asset],|r|r.get(0))?;
        return Ok(current == revision);
    };
    ensure!(encoded.len() <= 32768, "image export identity size limit");
    let expected: ImageMetadataIdentity = serde_json::from_str(&encoded)?;
    expected.key.validate()?;
    ensure!(
        expected.key.asset_id == asset && expected.metadata_revision == revision,
        "image export authority differs from plan"
    );
    let actual:Option<(ImageMetadataIdentity,bool)>=db.query_row(
        "SELECT i.id,i.asset_id,i.variant_id,COALESCE(m.revision,0),i.pixel_generation,s.epoch,a.physical_generation,i.applied_shared_epoch=s.epoch FROM catalog_images i JOIN assets a ON a.id=i.asset_id JOIN image_shared_state s ON s.asset_id=i.asset_id LEFT JOIN metadata_assets m ON m.asset_id=i.id WHERE i.id=?1",
        [&expected.image_id],|r|Ok((ImageMetadataIdentity{image_id:r.get(0)?,key:VariantKey{asset_id:r.get(1)?,variant_id:r.get(2)?},metadata_revision:r.get(3)?,pixel_generation:r.get(4)?,shared_source_epoch:r.get(5)?,physical_generation:r.get(6)?},r.get(7)?)),
    ).optional()?;
    Ok(actual.is_some_and(|(identity, ready)| ready && identity == expected))
}

impl Catalog {
    /// Freeze this image's selected XMP model and resolved fields. Older master
    /// plans remain readable and are never reserialized or assigned new authority.
    pub fn plan_image_metadata_export(
        &mut self,
        key: &VariantKey,
        expected_revision: i64,
        base_model: i64,
        destination: &Path,
    ) -> Result<ImageMetadataExportPlan> {
        let identity = self.image_metadata_identity(key)?;
        ensure!(
            identity.metadata_revision == expected_revision,
            "image metadata changed before export planning"
        );
        let (payload, projected) =
            self.resolved_export_xmp(&identity.image_id, expected_revision, base_model)?;
        let plan = crate::metadata_export::plan_export(destination, &payload)?;
        let hash = blake3::hash(&payload).to_hex().to_string();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&payload)?;
        let compressed = encoder.finish()?;
        let encoded = serde_json::to_string(&identity)?;
        ensure!(encoded.len() <= 32768, "image export identity size limit");
        let _permit = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        crate::catalog_images::require_image_metadata_identity(&tx, &identity)?;
        tx.execute(
            "INSERT OR IGNORE INTO metadata_blobs VALUES(?1,?2,?3)",
            params![hash, i64::try_from(payload.len())?, compressed],
        )?;
        tx.execute(
            "INSERT INTO metadata_export_plans VALUES(?1,?2,?3,?4,?5,?6,NULL)",
            params![
                plan.operation,
                key.asset_id,
                expected_revision,
                base_model,
                serde_json::to_string(&plan)?,
                hash
            ],
        )?;
        tx.execute(
            "INSERT INTO metadata_image_export_authorities VALUES(?1,?2)",
            params![plan.operation, encoded],
        )?;
        tx.commit()?;
        Ok(ImageMetadataExportPlan {
            image_identity: identity,
            export: MetadataExportPlan {
                asset_id: key.asset_id.clone(),
                metadata_revision: expected_revision,
                base_model,
                destination: plan,
                projected,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp::{Edit, Value};

    #[test]
    fn image_xmp_export_survives_reopen_and_sibling_edits_but_rejects_own_stale_revision()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("catalog");
        let mut catalog = Catalog::open(&root)?;
        let original = temp.path().join("missing.dng");
        catalog.db.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES('a',?1,'missing','pending')",
            [crate::location_bytes(&original)],
        )?;
        let master = VariantKey::master("a");
        let copy = catalog.create_edit_variant(&master, 0, "copy")?.key;
        let rating = |value: &str| Edit::Set {
            namespace: crate::xmp::XMP.into(),
            path: "Rating".into(),
            value: value.into(),
        };
        let own = catalog.edit_metadata_for_image(&copy, 0, None, &[rating("5")])?;
        let destination = temp.path().join("copy.xmp");
        let export = catalog.plan_image_metadata_export(
            &copy,
            own.revision,
            own.model_ids[0],
            &destination,
        )?;
        catalog.edit_metadata_for_image(&master, 0, None, &[rating("1")])?;
        drop(catalog);
        let mut catalog = Catalog::open(&root)?;
        assert_eq!(
            catalog
                .apply_metadata_export(&export.export.destination.operation)?
                .state,
            crate::metadata_export::ExportState::Published
        );
        assert_eq!(
            crate::xmp::project(&std::fs::read(&destination)?)?
                .fields
                .get("rating"),
            Some(&Value::Text("5".into()))
        );
        let stale = catalog.plan_image_metadata_export(
            &copy,
            own.revision,
            own.model_ids[0],
            &temp.path().join("stale.xmp"),
        )?;
        catalog.edit_metadata_for_image(
            &copy,
            own.revision,
            Some(own.model_ids[0]),
            &[rating("4")],
        )?;
        assert!(
            catalog
                .apply_metadata_export(&stale.export.destination.operation)
                .is_err()
        );
        assert!(!temp.path().join("stale.xmp").exists());
        Ok(())
    }
}
