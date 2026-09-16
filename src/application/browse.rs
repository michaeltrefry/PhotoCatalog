//! Bounded desktop filters over the existing organization search contract.
use super::*;
use crate::organization_search::{Direction, Sort};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub text: Option<String>,
    pub keyword: Option<I64>,
    pub keyword_direct: bool,
    pub folder: Option<I64>,
    pub folder_recursive: bool,
    pub collection: Option<String>,
    pub date_from: Option<String>,
    pub date_until: Option<String>,
    pub camera_make: Option<String>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub format: Option<String>,
    pub rating: Option<u8>,
    pub flag: Option<crate::organization::Flag>,
    pub label: Option<String>,
    pub only_conflicted: bool,
    pub sort: Sort,
    pub direction: Direction,
}
impl Options {
    pub fn query(self) -> std::result::Result<Query, BridgeError> {
        for value in [
            &self.text,
            &self.collection,
            &self.date_from,
            &self.date_until,
            &self.camera_make,
            &self.camera,
            &self.lens,
            &self.format,
            &self.label,
        ]
        .into_iter()
        .flatten()
        {
            if value.len() > 1024 || value.contains('\0') {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "search field exceeds 1024 bytes or contains NUL",
                ));
            }
        }
        if self.keyword.is_some_and(|v| v.0 <= 0)
            || self.folder.is_some_and(|v| v.0 <= 0)
            || self.rating.is_some_and(|v| v > 5)
        {
            return Err(error(
                ErrorCode::InvalidRequest,
                "invalid search identifier or rating",
            ));
        }
        Ok(Query {
            include_variants: true,
            text: self.text,
            keyword: self.keyword.map(|v| v.0),
            keyword_direct: self.keyword_direct,
            folder: self.folder.map(|v| v.0),
            folder_recursive: self.folder_recursive,
            collection: self.collection,
            date_from: self.date_from,
            date_until: self.date_until,
            camera_make: self.camera_make,
            camera: self.camera,
            lens: self.lens,
            format: self.format,
            rating: self.rating,
            flag: self.flag,
            label: self.label,
            only_conflicted: self.only_conflicted,
            sort: self.sort,
            direction: self.direction,
        })
    }
}
impl Actor {
    pub(super) fn search_page(
        &mut self,
        catalog: &str,
        query: Query,
        cursor: Option<String>,
        limit: u16,
    ) -> std::result::Result<Response, BridgeError> {
        let limits = self.config.limits.clone();
        if limit == 0 || limit > limits.page_rows {
            return Err(error(ErrorCode::InvalidRequest, "page row allowance"));
        }
        let old = cursor
            .as_deref()
            .map(|token| decode_cursor(token, catalog))
            .transpose()?;
        let page = self
            .current(catalog)?
            .catalog
            .search_grid(
                &query,
                old.as_ref(),
                usize::from(limit),
                limits.scan_rows,
                limits.page_bytes,
            )
            .map_err(native)?;
        let next = page
            .next
            .as_ref()
            .map(|cursor| encode_cursor(catalog, cursor))
            .transpose()?;
        Ok(Response::Images {
            rows: page
                .rows
                .into_iter()
                .map(|row| {
                    let key = VariantKey {
                        asset_id: row.asset_id.clone(),
                        variant_id: row.variant_id.clone(),
                    };
                    let image = self.current(catalog)?.catalog.image(&key).map_err(native)?;
                    grid_image(row, image)
                })
                .collect::<std::result::Result<Vec<_>, BridgeError>>()?,
            next,
            has_more: page.has_more,
            page_complete: page.page_complete,
            scanned: page.scanned as u32,
        })
    }
}

/// Enrich only admitted page rows, using the exact logical variant's catalog state.
/// Origin and translation state are fixed vocabulary fields in the catalog schema.
pub(super) fn grid_image(
    row: crate::organization_search::SearchRow,
    image: crate::catalog_images::Image,
) -> std::result::Result<GridImage, BridgeError> {
    if row.image_id != image.id
        || row.asset_id != image.key.asset_id
        || row.variant_id != image.key.variant_id
        || row.sequence != image.sequence
    {
        return Err(error(ErrorCode::Native, "logical image identity changed"));
    }
    Ok(GridImage {
        image_id: row.image_id,
        origin: image.origin,
        translation_state: image.translation_state,
        key: image.key,
        sequence: I64(row.sequence),
        metadata_revision: I64(row.metadata_revision),
        metadata_pending: row.metadata_pending,
        state: row.state,
        filename: row.filename,
        rating: row.rating.map(I64),
        flag: row.flag,
        label: row.label,
        conflicts: row.conflicts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> anyhow::Result<(tempfile::TempDir, Catalog, VariantKey, VariantKey)> {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        std::fs::create_dir(&originals)?;
        image::RgbImage::from_pixel(8, 8, image::Rgb([20u8, 40, 70]))
            .save(originals.join("photo.png"))?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        catalog.import(&originals, None, |_| Ok(()))?;
        let master = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
        let copy = catalog
            .create_edit_variant(
                &master,
                catalog.edit_variant(&master)?.revision,
                "Imported copy",
            )?
            .key;
        while catalog.organization_index(20)?.pending {}
        Ok((temp, catalog, master, copy))
    }

    #[test]
    fn compatibility_projection_preserves_each_logical_images_catalog_state() -> anyhow::Result<()>
    {
        let (_temp, catalog, master, copy) = fixture()?;
        // Exercise the mapper against every persisted state independently of the
        // translator's own classification tests. The shared original stays native.
        for state in ["native", "translated", "retained_only", "untranslated"] {
            catalog.db.execute(
                "UPDATE catalog_images SET origin='import',translation_state=?1 WHERE asset_id=?2 AND variant_id=?3",
                rusqlite::params![state, copy.asset_id, copy.variant_id],
            )?;
            let mapped = grid_image(catalog.grid_image(&copy, 16384)?, catalog.image(&copy)?)
                .map_err(|e| anyhow::anyhow!(e.message))?;
            assert_eq!(mapped.key, copy);
            assert_eq!(mapped.origin, "import");
            assert_eq!(mapped.translation_state, state);
            let native = grid_image(catalog.grid_image(&master, 16384)?, catalog.image(&master)?)
                .map_err(|e| anyhow::anyhow!(e.message))?;
            assert_eq!(native.key, master);
            assert_eq!(native.origin, "native");
            assert_eq!(native.translation_state, "native");
            let wire = serde_json::to_value(mapped)?;
            assert_eq!(wire["translation_state"], state);
        }
        Ok(())
    }

    #[test]
    fn compatibility_projection_rejects_another_variant_identity() -> anyhow::Result<()> {
        let (_temp, catalog, master, copy) = fixture()?;
        assert!(grid_image(catalog.grid_image(&copy, 16384)?, catalog.image(&master)?).is_err());
        let mut same_key_wrong_image = catalog.image(&copy)?;
        same_key_wrong_image.id = "another logical image".into();
        assert!(grid_image(catalog.grid_image(&copy, 16384)?, same_key_wrong_image).is_err());
        Ok(())
    }
}
