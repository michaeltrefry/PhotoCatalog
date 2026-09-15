//! Image-scoped XMP exports alongside unchanged legacy export authorities.
use crate::{
    Catalog, catalog_edits::VariantKey, catalog_images::ImageMetadataIdentity,
    catalog_metadata::MetadataExportPlan, catalog_writer::Priority,
};
use anyhow::{Result, ensure};
use flate2::{Compression, write::ZlibEncoder};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use std::{io::Write, path::Path, sync::atomic::AtomicBool};

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
    let encoded: Option<Option<String>> = db
        .query_row(
            "SELECT CASE WHEN length(CAST(image_identity AS BLOB))<=32768 THEN image_identity END FROM metadata_image_export_authorities WHERE operation=?1",
            [operation],
            |r| r.get(0),
        )
        .optional()?;
    let encoded = match encoded {
        Some(Some(encoded)) => encoded,
        Some(None) => anyhow::bail!("image export identity size limit"),
        None => {
            let current:i64=db.query_row("SELECT COALESCE(m.revision,0) FROM assets a LEFT JOIN metadata_assets m ON m.asset_id=a.id WHERE a.id=?1",[asset],|r|r.get(0))?;
            return Ok(current == revision);
        }
    };
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

pub(crate) fn owner(
    db: &Connection,
    operation: &str,
    asset: &str,
    revision: i64,
) -> Result<crate::catalog_metadata_write::Owner> {
    let encoded: Option<Option<String>> = db
        .query_row(
            "SELECT CASE WHEN length(CAST(image_identity AS BLOB))<=32768 THEN image_identity END FROM metadata_image_export_authorities WHERE operation=?1",
            [operation],
            |row| row.get(0),
        )
        .optional()?;
    match encoded {
        Some(Some(encoded)) => Ok(crate::catalog_metadata_write::Owner::Image {
            identity: serde_json::from_str(&encoded)?,
        }),
        Some(None) => anyhow::bail!("image export identity size limit"),
        None => Ok(crate::catalog_metadata_write::Owner::LegacyAsset {
            asset_id: asset.to_owned(),
            revision,
        }),
    }
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
        self.plan_image_metadata_export_with_cancel(
            key,
            expected_revision,
            base_model,
            destination,
            &AtomicBool::new(false),
        )
    }
    pub(crate) fn plan_image_metadata_export_with_cancel(
        &mut self,
        key: &VariantKey,
        expected_revision: i64,
        base_model: i64,
        destination: &Path,
        cancel: &AtomicBool,
    ) -> Result<ImageMetadataExportPlan> {
        self.plan_image_metadata_export_controlled(
            key,
            expected_revision,
            base_model,
            destination,
            cancel,
            crate::catalog_session::metadata_files::EVIDENCE_BYTES,
            Default::default(),
            None,
        )
    }
    pub(crate) fn plan_image_metadata_export_with_receipt(
        &mut self,
        key: &VariantKey,
        expected_revision: i64,
        base_model: i64,
        destination: &Path,
        cancel: &AtomicBool,
        max_existing_bytes: u64,
        alias_limits: crate::catalog_export_alias::AliasLimits,
        attempt: &str,
        request_digest: &str,
    ) -> Result<ImageMetadataExportPlan> {
        self.plan_image_metadata_export_controlled(
            key,
            expected_revision,
            base_model,
            destination,
            cancel,
            max_existing_bytes,
            alias_limits,
            Some((attempt, request_digest)),
        )
    }
    fn plan_image_metadata_export_controlled(
        &mut self,
        key: &VariantKey,
        expected_revision: i64,
        base_model: i64,
        destination: &Path,
        cancel: &AtomicBool,
        max_existing_bytes: u64,
        alias_limits: crate::catalog_export_alias::AliasLimits,
        receipt: Option<(&str, &str)>,
    ) -> Result<ImageMetadataExportPlan> {
        self.require_jobs_released()?;
        self.reconcile_export_paths(512)?;
        ensure!(max_existing_bytes > 0, "existing file byte limit");
        alias_limits.validate()?;
        let identity = self.image_metadata_identity(key)?;
        ensure!(
            identity.metadata_revision == expected_revision,
            "image metadata changed before export planning"
        );
        let (payload, projected) =
            self.resolved_export_xmp(&identity.image_id, expected_revision, base_model)?;
        let native = crate::storage_volume::NativePath::from_path(destination);
        let _permit = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((attempt, digest)) = receipt {
            ensure!(
                crate::catalog_metadata_write::existing(&tx, attempt, digest)?.is_none(),
                "metadata attempt already committed"
            );
        }
        crate::catalog_images::require_image_metadata_identity(&tx, &identity)?;
        let mut control = crate::catalog_exports::ExportControl::new(cancel);
        crate::catalog_exports::protect_catalog_original_destination_controlled(
            &tx,
            &self.session,
            destination,
            alias_limits,
            &mut control,
        )?;
        let plan = match self.session.plan_metadata_file(
            &native,
            &payload,
            max_existing_bytes,
            alias_limits,
            cancel,
        )? {
            Some(plan) => plan,
            None => {
                let mut checkpoint = |_| {
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "metadata planning canceled",
                        ))
                    } else {
                        Ok(())
                    }
                };
                crate::metadata_export::plan_export_controlled(
                    destination,
                    &payload,
                    max_existing_bytes,
                    alias_limits,
                    &mut checkpoint,
                )?
            }
        };
        let hash = blake3::hash(&payload).to_hex().to_string();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&payload)?;
        let compressed = encoder.finish()?;
        let encoded = serde_json::to_string(&identity)?;
        ensure!(encoded.len() <= 32768, "image export identity size limit");
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
        if let Some((attempt, digest)) = receipt {
            crate::catalog_metadata_write::insert(
                &tx,
                attempt,
                digest,
                "sidecar_plan",
                &crate::catalog_metadata_write::Owner::Image {
                    identity: identity.clone(),
                },
                &serde_json::json!({"operation": plan.operation}),
            )?;
        }
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
        std::fs::write(&original, b"original")?;
        catalog.db.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES('a',?1,'missing','pending')",
            [crate::location_bytes(&original)],
        )?;
        catalog.record_storage_path(
            "a",
            &crate::storage_volume::NativePath::from_path(&original),
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
    #[cfg(unix)]
    #[test]
    fn native_and_legacy_image_xmp_authorities_keep_raw_bytes_and_stale_restore_only() -> Result<()>
    {
        use std::os::unix::ffi::OsStringExt;
        for legacy in [false, true] {
            let temp = tempfile::tempdir()?;
            let root = temp.path().join("catalog");
            let mut catalog = Catalog::open(&root)?;
            let original = temp.path().join("original.dng");
            std::fs::write(&original, b"original")?;
            catalog.db.execute("INSERT INTO assets(id,location,path_display,state) VALUES('a',?1,'synthetic','pending')",[crate::location_bytes(&original)])?;
            catalog.record_storage_path(
                "a",
                &crate::storage_volume::NativePath::from_path(&original),
            )?;
            let key = crate::storage_volume::object_key(&original, &std::fs::metadata(&original)?)?;
            catalog.db.execute(
                "UPDATE storage_bindings SET file_key=?1 WHERE asset_id='a'",
                [format!("{}:{}", key.0, key.1)],
            )?;
            let copy = catalog
                .create_edit_variant(&VariantKey::master("a"), 0, "copy")?
                .key;
            let rating = |v: &str| Edit::Set {
                namespace: crate::xmp::XMP.into(),
                path: "Rating".into(),
                value: v.into(),
            };
            let own = catalog.edit_metadata_for_image(&copy, 0, None, &[rating("5")])?;
            let parent = temp.path().join(if legacy {
                std::ffi::OsString::from("legacy")
            } else {
                std::ffi::OsString::from_vec(vec![b'p', 255])
            });
            if let Err(error) = std::fs::create_dir(&parent) {
                #[cfg(target_os = "macos")]
                if error.raw_os_error() == Some(92) {
                    assert!(!parent.exists());
                    eprintln!(
                        "non-UTF filesystem probe rejected before export: EILSEQ92; byte-wire custody remains tested"
                    );
                    continue;
                }
                return Err(error.into());
            }
            let destination = parent.join(if legacy {
                std::ffi::OsString::from("old.xmp")
            } else {
                std::ffi::OsString::from_vec(b"image\xff.xmp".to_vec())
            });
            std::fs::write(&destination, b"original retained sidecar")?;
            let mut export = catalog.plan_image_metadata_export(
                &copy,
                own.revision,
                own.model_ids[0],
                &destination,
            )?;
            if legacy {
                export.export.destination.version = 1;
            }
            let raw = format!(
                " \n{}\n ",
                serde_json::to_string_pretty(&export.export.destination)?
            );
            let operation = export.export.destination.operation.clone();
            catalog.db.execute(
                "UPDATE metadata_export_plans SET plan=?1 WHERE operation=?2",
                rusqlite::params![raw, operation],
            )?;
            drop(catalog);
            let mut catalog = Catalog::open(&root)?;
            let receipt = catalog.apply_metadata_export(&operation)?;
            assert_eq!(
                receipt.state,
                crate::metadata_export::ExportState::Published
            );
            assert_eq!(receipt.destination, export.export.destination.destination);
            let retained: String = catalog.db.query_row(
                "SELECT plan FROM metadata_export_plans WHERE operation=?1",
                [&operation],
                |r| r.get(0),
            )?;
            assert_eq!(retained, raw);
            assert_eq!(
                std::fs::read(receipt.captured_original.as_ref().unwrap())?,
                b"original retained sidecar"
            );
            assert_eq!(
                catalog
                    .recover_metadata_export(&receipt.recovery_directory)?
                    .state,
                crate::metadata_export::ExportState::Published
            );
            let stale_path = parent.join("stale.xmp");
            std::fs::write(&stale_path, b"stale original")?;
            let stale = catalog.plan_image_metadata_export(
                &copy,
                own.revision,
                own.model_ids[0],
                &stale_path,
            )?;
            let (payload, _) = catalog.resolved_export_xmp(
                &stale.image_identity.image_id,
                own.revision,
                own.model_ids[0],
            )?;
            let interrupted = crate::metadata_export::apply_export_with_hook(
                &stale.export.destination,
                &payload,
                |boundary| {
                    if boundary == crate::metadata_export::ExportBoundary::BeforePublish {
                        Err(std::io::Error::other("interrupted"))
                    } else {
                        Ok(())
                    }
                },
            )?;
            catalog.edit_metadata_for_image(
                &copy,
                own.revision,
                Some(own.model_ids[0]),
                &[rating("4")],
            )?;
            let restored = catalog.recover_metadata_export(&interrupted.recovery_directory)?;
            assert_ne!(
                restored.state,
                crate::metadata_export::ExportState::Published
            );
            assert_eq!(std::fs::read(stale_path)?, b"stale original");
            assert!(
                !catalog
                    .metadata_model(&stale.image_identity.image_id, own.model_ids[0])?
                    .is_empty()
            );
        }
        Ok(())
    }

    #[test]
    fn controlled_sidecar_plan_rejects_original_and_enforces_selected_existing_limit() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("catalog");
        let original = temp.path().canonicalize()?.join("original.xmp");
        std::fs::write(&original, b"catalog original")?;
        let mut catalog = Catalog::open(&root)?;
        catalog.db.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES('a',?1,'original','pending')",
            [crate::location_bytes(&original)],
        )?;
        catalog.record_storage_path(
            "a",
            &crate::storage_volume::NativePath::from_path(&original),
        )?;
        let object = crate::storage_volume::object_key(&original, &std::fs::metadata(&original)?)?;
        catalog.db.execute(
            "UPDATE storage_bindings SET file_key=?1 WHERE asset_id='a'",
            [format!("{}:{}", object.0, object.1)],
        )?;
        let key = VariantKey::master("a");
        let change = catalog.edit_metadata_for_image(
            &key,
            0,
            None,
            &[Edit::Set {
                namespace: crate::xmp::XMP.into(),
                path: "Rating".into(),
                value: "5".into(),
            }],
        )?;
        let identity = catalog.image_metadata_identity(&key)?;
        let digest = "0".repeat(64);
        assert!(
            catalog
                .plan_image_metadata_export_with_receipt(
                    &key,
                    identity.metadata_revision,
                    change.model_ids[0],
                    &original,
                    &AtomicBool::new(false),
                    1024,
                    Default::default(),
                    &uuid::Uuid::new_v4().to_string(),
                    &digest,
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&original)?, b"catalog original");

        let alias = temp.path().canonicalize()?.join("hardlink-alias.xmp");
        std::fs::hard_link(&original, &alias)?;
        assert!(
            catalog
                .plan_image_metadata_export_with_receipt(
                    &key,
                    identity.metadata_revision,
                    change.model_ids[0],
                    &alias,
                    &AtomicBool::new(false),
                    1024,
                    Default::default(),
                    &uuid::Uuid::new_v4().to_string(),
                    &digest,
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&original)?, b"catalog original");

        let oversized = temp.path().canonicalize()?.join("oversized.xmp");
        std::fs::write(&oversized, b"12345678901")?;
        assert!(
            catalog
                .plan_image_metadata_export_with_receipt(
                    &key,
                    identity.metadata_revision,
                    change.model_ids[0],
                    &oversized,
                    &AtomicBool::new(false),
                    10,
                    Default::default(),
                    &uuid::Uuid::new_v4().to_string(),
                    &digest,
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&oversized)?, b"12345678901");

        catalog.db.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES('unbound',?1,'unbound','pending')",
            [b"unbound".as_slice()],
        )?;
        let new_destination = temp.path().canonicalize()?.join("new.xmp");
        assert!(
            catalog
                .plan_image_metadata_export_with_receipt(
                    &key,
                    identity.metadata_revision,
                    change.model_ids[0],
                    &new_destination,
                    &AtomicBool::new(false),
                    1024,
                    Default::default(),
                    &uuid::Uuid::new_v4().to_string(),
                    &digest,
                )
                .is_err()
        );
        assert!(!new_destination.exists());
        assert_eq!(
            catalog
                .db
                .query_row("SELECT count(*) FROM metadata_export_plans", [], |row| row
                    .get::<_, i64>(
                    0
                ))?,
            0
        );
        Ok(())
    }
}
