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
                .map(|row| GridImage {
                    image_id: row.image_id,
                    key: VariantKey {
                        asset_id: row.asset_id,
                        variant_id: row.variant_id,
                    },
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
                .collect(),
            next,
            has_more: page.has_more,
            page_complete: page.page_complete,
            scanned: page.scanned as u32,
        })
    }
}
