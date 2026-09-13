//! Bounded organization requests over public, logical-image-aware catalog APIs.
//! Catalog selection and NativePath authority belong to the enclosing actor.
//! Provenance is an opaque JSON string so retained integers survive JavaScript.
use super::{BridgeError, ErrorCode, I64, Limits, U64, error, native};
use crate::{Catalog, catalog_edits::VariantKey, organization as core};
use serde::{Deserialize, Serialize};
use std::io::Write;

type Result<T> = std::result::Result<T, BridgeError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "command",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    Keywords {
        kind: core::KeywordKind,
        parent: Option<I64>,
        after: I64,
        limit: u16,
    },
    CreateKeyword {
        kind: core::KeywordKind,
        path: Vec<String>,
    },
    DeleteKeyword {
        id: I64,
    },
    Synonyms {
        keyword: I64,
        after: String,
        limit: u16,
    },
    AddSynonym {
        keyword: I64,
        synonym: String,
    },
    Collections {
        after: String,
        limit: u16,
    },
    CreateCollection {
        name: String,
    },
    RenameCollection {
        id: String,
        expected_revision: I64,
        name: String,
    },
    DeleteCollection {
        id: String,
        expected_revision: I64,
    },
    Placement {
        collection: String,
    },
    PlaceCollection {
        collection: String,
        expected_revision: I64,
        parent: Option<String>,
        position: I64,
    },
    Members {
        collection: String,
        after: Option<MembershipCursor>,
        limit: u16,
    },
    Identity {
        key: VariantKey,
    },
    SetMember {
        identity: ImageIdentity,
        collection: String,
        position: I64,
    },
    Apply {
        key: VariantKey,
        expected_revision: I64,
        operation: core::Operation,
    },
    Begin {
        operation: core::Operation,
    },
    Append {
        job: String,
        items: Vec<BatchItem>,
    },
    Seal {
        job: String,
    },
    Step {
        job: String,
    },
    Cancel {
        job: String,
    },
    Job {
        job: String,
    },
    Jobs {
        after: String,
        limit: u16,
    },
    Items {
        job: String,
        after: I64,
        limit: u16,
    },
    Review {
        job: String,
        key: VariantKey,
        new_revision: Option<I64>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchItem {
    pub key: VariantKey,
    pub expected_revision: I64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageIdentity {
    pub image_id: String,
    pub key: VariantKey,
    pub metadata_revision: I64,
    pub pixel_generation: I64,
    pub shared_source_epoch: I64,
    pub physical_generation: I64,
}
impl From<ImageIdentity> for crate::catalog_images::ImageMetadataIdentity {
    fn from(v: ImageIdentity) -> Self {
        Self {
            image_id: v.image_id,
            key: v.key,
            metadata_revision: v.metadata_revision.0,
            pixel_generation: v.pixel_generation.0,
            shared_source_epoch: v.shared_source_epoch.0,
            physical_generation: v.physical_generation.0,
        }
    }
}
impl From<crate::catalog_images::ImageMetadataIdentity> for ImageIdentity {
    fn from(v: crate::catalog_images::ImageMetadataIdentity) -> Self {
        Self {
            image_id: v.image_id,
            key: v.key,
            metadata_revision: I64(v.metadata_revision),
            pixel_generation: I64(v.pixel_generation),
            shared_source_epoch: I64(v.shared_source_epoch),
            physical_generation: I64(v.physical_generation),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberCursor {
    pub position: I64,
    pub sequence: I64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipCursor {
    pub collection: String,
    pub revision: I64,
    pub ordered: bool,
    pub position: I64,
    pub sequence: I64,
}
impl From<crate::catalog_images::organization::CollectionMemberCursor> for MembershipCursor {
    fn from(v: crate::catalog_images::organization::CollectionMemberCursor) -> Self {
        Self {
            collection: v.collection,
            revision: I64(v.revision),
            ordered: v.ordered,
            position: I64(v.position),
            sequence: I64(v.sequence),
        }
    }
}
impl From<MembershipCursor> for crate::catalog_images::organization::CollectionMemberCursor {
    fn from(v: MembershipCursor) -> Self {
        Self {
            collection: v.collection,
            revision: v.revision.0,
            ordered: v.ordered,
            position: v.position.0,
            sequence: v.sequence.0,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MembershipPage {
    pub rows: Vec<Member>,
    pub next: Option<MembershipCursor>,
    pub scanned: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T, C> {
    pub rows: Vec<T>,
    pub next: Option<C>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyword {
    pub id: I64,
    pub kind: core::KeywordKind,
    pub parent: Option<I64>,
    pub name: String,
    pub path: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub id: String,
    pub name: String,
    pub revision: I64,
    pub provenance_json: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Placement {
    pub collection: String,
    pub parent: Option<String>,
    pub position: I64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Synonym {
    pub synonym: String,
    pub provenance_json: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub key: VariantKey,
    pub cursor: MemberCursor,
    pub provenance_json: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub operation: core::Operation,
    pub state: String,
    pub pending: I64,
    pub applied: I64,
    pub failed: I64,
    pub skipped: I64,
}
impl From<core::Job> for Job {
    fn from(v: core::Job) -> Self {
        Self {
            id: v.id,
            operation: v.operation,
            state: v.state,
            pending: I64(v.pending),
            applied: I64(v.applied),
            failed: I64(v.failed),
            skipped: I64(v.skipped),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobItem {
    pub sequence: I64,
    pub key: VariantKey,
    pub image_id: String,
    pub expected_revision: I64,
    pub status: String,
    pub error: Option<String>,
    pub result_revision: Option<I64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Response {
    Keywords(Page<Keyword, I64>),
    KeywordCreated { id: I64 },
    Synonyms(Page<Synonym, String>),
    Collections(Page<Collection, String>),
    CollectionCreated { id: String },
    Placement(Placement),
    Members(MembershipPage),
    Identity(ImageIdentity),
    Changed { revision: I64 },
    Acknowledged,
    Job(Job),
    Jobs(Page<Job, String>),
    Items(Page<JobItem, I64>),
}

// Counting serializer stops at the declared bound without allocating a second
// request/provenance-sized buffer. The transport applies its own outer bound.
struct Counter {
    bytes: usize,
    limit: usize,
}
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes) {
            return Err(std::io::Error::other("organization message byte limit"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn size(value: &impl Serialize, limit: usize) -> Result<usize> {
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| error(ErrorCode::ResourceLimit, "organization message byte limit"))?;
    Ok(counter.bytes)
}
fn invalid(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(error(ErrorCode::InvalidRequest, message))
    }
}
fn text(value: &str, empty: bool) -> Result<()> {
    invalid(
        (empty || !value.is_empty()) && value.len() <= 1024 && !value.contains('\0'),
        "organization text must be at most 1024 bytes without NUL",
    )
}
fn key(value: &VariantKey) -> Result<()> {
    value
        .validate()
        .map_err(|_| error(ErrorCode::InvalidRequest, "invalid variant identity"))?;
    text(&value.asset_id, false)?;
    text(&value.variant_id, false)
}
fn count(limit: u16, bounds: &Limits) -> Result<usize> {
    invalid(
        limit > 0 && limit <= bounds.page_rows && usize::from(limit) <= bounds.scan_rows,
        "organization page row limit",
    )?;
    Ok(usize::from(limit))
}
fn path(kind: core::KeywordKind, value: &[String]) -> Result<()> {
    invalid(
        !value.is_empty()
            && value.len() <= 64
            && (kind != core::KeywordKind::Flat || value.len() == 1),
        "keyword path depth",
    )?;
    for part in value {
        text(part, false)?;
        invalid(
            kind == core::KeywordKind::Flat || !part.contains('|'),
            "keyword hierarchy separator",
        )?;
    }
    Ok(())
}
fn operation(value: &core::Operation) -> Result<()> {
    match value {
        core::Operation::Rating { value } => invalid(*value <= 5, "rating must be 0..5"),
        core::Operation::Label { value } => text(value, true),
        core::Operation::Flag { .. } => Ok(()),
        core::Operation::AddKeyword { kind, path: parts }
        | core::Operation::RemoveKeyword { kind, path: parts } => path(*kind, parts),
        core::Operation::MoveKeyword { from, to } => {
            path(core::KeywordKind::Hierarchical, from)?;
            path(core::KeywordKind::Hierarchical, to)
        }
        core::Operation::AddCollection { collection }
        | core::Operation::RemoveCollection { collection } => text(collection, false),
    }
}
fn opaque(value: serde_json::Value, bounds: &Limits) -> Result<String> {
    size(&value, bounds.page_bytes.min(65536))?;
    serde_json::to_string(&value).map_err(|e| native(e.into()))
}
fn page<T: Serialize, C: Clone + Serialize>(
    rows: Vec<T>,
    limit: usize,
    bounds: &Limits,
    cursor: impl Fn(&T) -> C,
) -> Result<Page<T, C>> {
    let budget = bounds
        .page_bytes
        .min(bounds.reply_bytes.saturating_sub(128));
    let full = rows.len() == limit;
    let mut accepted = Vec::new();
    let mut bytes: usize = 64;
    let mut more = full;
    for row in rows {
        let row_bytes = size(&row, budget)?;
        let incoming = row_bytes + size(&cursor(&row), budget)? + 2;
        if incoming > budget.saturating_sub(bytes) {
            if accepted.is_empty() {
                return Err(error(
                    ErrorCode::ResourceLimit,
                    "organization row exceeds page bytes",
                ));
            }
            more = true;
            break;
        }
        bytes += incoming;
        accepted.push(row);
    }
    let next = if more {
        accepted.last().map(cursor)
    } else {
        None
    };
    Ok(Page {
        rows: accepted,
        next,
    })
}

fn job_reply_bound(id: String, operation: core::Operation, bounds: &Limits) -> Result<()> {
    // Check the largest possible counter encoding before any job mutation;
    // a committed change must not turn into a transport-size error afterward.
    size(
        &Response::Job(Job {
            id,
            operation,
            state: "cancelled".into(),
            pending: I64(i64::MAX),
            applied: I64(i64::MAX),
            failed: I64(i64::MAX),
            skipped: I64(i64::MAX),
        }),
        bounds.reply_bytes,
    )?;
    Ok(())
}

/// Execute one bounded command. A Step advances at most one core job item;
/// the actor must schedule repeated steps as background work between UI requests.
pub fn execute(catalog: &mut Catalog, request: Request, bounds: &Limits) -> Result<Response> {
    invalid(
        (1..=100).contains(&bounds.page_rows)
            && bounds.scan_rows >= usize::from(bounds.page_rows)
            && bounds.scan_rows <= 4096
            && (1024..=1024 * 1024).contains(&bounds.page_bytes)
            && (1024..=4 * 1024 * 1024).contains(&bounds.request_bytes)
            && (1024..=4 * 1024 * 1024).contains(&bounds.reply_bytes),
        "organization limits",
    )?;
    size(&request, bounds.request_bytes)?;
    match &request {
        Request::Begin { operation } => job_reply_bound("0".repeat(36), operation.clone(), bounds)?,
        Request::Append { job, .. }
        | Request::Seal { job }
        | Request::Step { job }
        | Request::Cancel { job }
        | Request::Review { job, .. } => {
            text(job, false)?;
            let current = catalog.organization_job(job).map_err(native)?;
            job_reply_bound(current.id, current.operation, bounds)?;
        }
        _ => {}
    }
    let response = match request {
        Request::Keywords {
            kind,
            parent,
            after,
            limit,
        } => {
            invalid(
                after.0 >= 0 && parent.is_none_or(|v| v.0 > 0),
                "keyword cursor",
            )?;
            let limit = count(limit, bounds)?;
            let rows = catalog
                .organization_keywords(kind, parent.map(|v| v.0), after.0, limit)
                .map_err(native)?
                .into_iter()
                .map(|v| Keyword {
                    id: I64(v.id),
                    kind: v.kind,
                    parent: v.parent.map(I64),
                    name: v.name,
                    path: v.path,
                })
                .collect();
            Response::Keywords(page(rows, limit, bounds, |v| v.id)?)
        }
        Request::CreateKeyword { kind, path: parts } => {
            path(kind, &parts)?;
            Response::KeywordCreated {
                id: I64(catalog.create_keyword(kind, &parts).map_err(native)?),
            }
        }
        Request::DeleteKeyword { id } => {
            invalid(id.0 > 0, "keyword id")?;
            catalog.delete_keyword(id.0).map_err(native)?;
            Response::Acknowledged
        }
        Request::Synonyms {
            keyword,
            after,
            limit,
        } => {
            invalid(keyword.0 > 0, "keyword id")?;
            text(&after, true)?;
            let limit = count(limit, bounds)?;
            let rows = catalog
                .keyword_synonyms(keyword.0, &after, limit)
                .map_err(native)?
                .into_iter()
                .map(|(synonym, v)| {
                    Ok(Synonym {
                        synonym,
                        provenance_json: opaque(v, bounds)?,
                    })
                })
                .collect::<Result<_>>()?;
            Response::Synonyms(page(rows, limit, bounds, |v| v.synonym.clone())?)
        }
        Request::AddSynonym { keyword, synonym } => {
            invalid(keyword.0 > 0, "keyword id")?;
            text(&synonym, false)?;
            catalog
                .create_keyword_synonym(
                    keyword.0,
                    &synonym,
                    &serde_json::json!({"source":"desktop"}),
                )
                .map_err(native)?;
            Response::Acknowledged
        }
        Request::Collections { after, limit } => {
            text(&after, true)?;
            let limit = count(limit, bounds)?;
            let rows = catalog
                .organization_collections(&after, limit)
                .map_err(native)?
                .into_iter()
                .map(|v| {
                    Ok(Collection {
                        id: v.id,
                        name: v.name,
                        revision: I64(v.revision),
                        provenance_json: opaque(v.provenance, bounds)?,
                    })
                })
                .collect::<Result<_>>()?;
            Response::Collections(page(rows, limit, bounds, |v| v.id.clone())?)
        }
        Request::CreateCollection { name } => {
            text(&name, false)?;
            Response::CollectionCreated {
                id: catalog
                    .create_collection(
                        &name,
                        serde_json::json!({"source":"desktop","kind":"manual"}),
                    )
                    .map_err(native)?,
            }
        }
        Request::RenameCollection {
            id,
            expected_revision,
            name,
        } => {
            text(&id, false)?;
            text(&name, false)?;
            invalid(expected_revision.0 >= 0, "collection revision")?;
            catalog
                .rename_collection(&id, expected_revision.0, &name)
                .map_err(native)?;
            Response::Acknowledged
        }
        Request::DeleteCollection {
            id,
            expected_revision,
        } => {
            text(&id, false)?;
            invalid(expected_revision.0 >= 0, "collection revision")?;
            catalog
                .delete_collection(&id, expected_revision.0)
                .map_err(native)?;
            Response::Acknowledged
        }
        Request::Placement { collection } => {
            text(&collection, false)?;
            let v = catalog.collection_placement(&collection).map_err(native)?;
            Response::Placement(Placement {
                collection: v.collection,
                parent: v.parent,
                position: I64(v.position),
            })
        }
        Request::PlaceCollection {
            collection,
            expected_revision,
            parent,
            position,
        } => {
            text(&collection, false)?;
            if let Some(v) = &parent {
                text(v, false)?;
            }
            invalid(
                expected_revision.0 >= 0 && position.0 >= 0,
                "collection placement revision or position",
            )?;
            catalog
                .place_collection_checked(
                    &crate::catalog_images::organization::CollectionPlacement {
                        collection,
                        parent,
                        position: position.0,
                    },
                    expected_revision.0,
                )
                .map_err(native)?;
            Response::Acknowledged
        }
        Request::Identity { key: target } => {
            key(&target)?;
            Response::Identity(
                catalog
                    .image_metadata_identity(&target)
                    .map_err(native)?
                    .into(),
            )
        }
        Request::Members {
            collection,
            after,
            limit,
        } => {
            text(&collection, false)?;
            let limit = count(limit, bounds)?;
            if let Some(v) = &after {
                text(&v.collection, false)?;
                invalid(
                    v.position.0 >= 0 && v.sequence.0 >= 0 && v.revision.0 >= 0,
                    "membership cursor",
                )?;
            }
            let budget = bounds
                .page_bytes
                .min(bounds.reply_bytes.saturating_sub(128));
            let mut page = MembershipPage {
                rows: Vec::new(),
                next: after,
                scanned: U64(0),
            };
            while page.rows.len() < limit && page.scanned.0 < bounds.scan_rows as u64 {
                let before = page.next.clone();
                let cursor = before.clone().map(Into::into);
                let step = catalog
                    .image_collection_member_step(
                        &collection,
                        cursor.as_ref(),
                        bounds.scan_rows - page.scanned.0 as usize,
                        budget,
                    )
                    .map_err(native)?;
                page.scanned.0 += step.scanned as u64;
                page.next = step.next.map(Into::into);
                if let Some((sequence, v)) = step.member {
                    page.rows.push(Member {
                        key: v.key,
                        cursor: MemberCursor {
                            position: I64(v.position),
                            sequence: I64(sequence),
                        },
                        provenance_json: opaque(v.provenance, bounds)?,
                    });
                    if size(&page, budget).is_err() {
                        page.rows.pop();
                        page.next = before;
                        if page.rows.is_empty() {
                            return Err(error(
                                ErrorCode::ResourceLimit,
                                "organization member exceeds page bytes",
                            ));
                        }
                        break;
                    }
                }
                if page.next.is_none() {
                    break;
                }
            }
            size(&page, budget)?;
            Response::Members(page)
        }
        Request::SetMember {
            identity,
            collection,
            position,
        } => {
            key(&identity.key)?;
            text(&identity.image_id, false)?;
            text(&collection, false)?;
            invalid(
                position.0 >= 0
                    && [
                        identity.metadata_revision,
                        identity.pixel_generation,
                        identity.shared_source_epoch,
                        identity.physical_generation,
                    ]
                    .iter()
                    .all(|v| v.0 >= 0),
                "membership identity or position",
            )?;
            Response::Changed {
                revision: I64(catalog
                    .set_image_collection_membership(
                        &identity.into(),
                        &collection,
                        position.0,
                        &serde_json::json!({"source":"desktop","action":"ordered_membership"}),
                    )
                    .map_err(native)?),
            }
        }
        Request::Apply {
            key: target,
            expected_revision,
            operation: op,
        } => {
            key(&target)?;
            invalid(expected_revision.0 >= 0, "image revision")?;
            operation(&op)?;
            Response::Changed {
                revision: I64(catalog
                    .organize_image(&target, expected_revision.0, op)
                    .map_err(native)?),
            }
        }
        Request::Begin { operation: op } => {
            operation(&op)?;
            Response::Job(catalog.begin_organization_batch(op).map_err(native)?.into())
        }
        Request::Append { job, items } => {
            text(&job, false)?;
            invalid(
                !items.is_empty()
                    && items.len() <= usize::from(bounds.page_rows)
                    && items.len() <= bounds.scan_rows,
                "organization selection page limit",
            )?;
            for item in &items {
                key(&item.key)?;
                invalid(item.expected_revision.0 >= 0, "image revision")?;
            }
            let items = items
                .into_iter()
                .map(|v| (v.key, v.expected_revision.0))
                .collect::<Vec<_>>();
            Response::Job(
                catalog
                    .append_image_organization_batch(&job, &items)
                    .map_err(native)?
                    .into(),
            )
        }
        Request::Seal { job } => {
            text(&job, false)?;
            Response::Job(
                catalog
                    .seal_organization_batch(&job)
                    .map_err(native)?
                    .into(),
            )
        }
        Request::Step { job } => {
            text(&job, false)?;
            // Organization changes only catalog state. Restored external-job
            // holds intentionally permit offline editing and organization.
            Response::Job(
                catalog
                    .step_organization_batch(&job)
                    .map_err(native)?
                    .into(),
            )
        }
        Request::Cancel { job } => {
            text(&job, false)?;
            Response::Job(
                catalog
                    .cancel_organization_batch(&job)
                    .map_err(native)?
                    .into(),
            )
        }
        Request::Job { job } => {
            text(&job, false)?;
            Response::Job(catalog.organization_job(&job).map_err(native)?.into())
        }
        Request::Jobs { after, limit } => {
            text(&after, true)?;
            let limit = count(limit, bounds)?;
            invalid(limit * 2 <= bounds.scan_rows, "organization job scan limit")?;
            let rows = catalog
                .organization_jobs(&after, limit)
                .map_err(native)?
                .into_iter()
                .map(Job::from)
                .collect();
            Response::Jobs(page(rows, limit, bounds, |v| v.id.clone())?)
        }
        Request::Items { job, after, limit } => {
            text(&job, false)?;
            invalid(after.0 >= 0, "job cursor")?;
            let limit = count(limit, bounds)?;
            invalid(
                limit * 2 <= bounds.scan_rows,
                "organization item scan limit",
            )?;
            let rows = catalog
                .organization_job_items(&job, after.0, limit)
                .map_err(native)?
                .into_iter()
                .map(|v| {
                    let image = catalog
                        .browse_images(
                            v.sequence.checked_sub(1).ok_or_else(|| {
                                error(ErrorCode::Native, "invalid logical image sequence")
                            })?,
                            1,
                        )
                        .map_err(native)?
                        .into_iter()
                        .next()
                        .ok_or_else(|| {
                            error(ErrorCode::Native, "organization logical image missing")
                        })?;
                    if image.sequence != v.sequence || image.id != v.asset_id {
                        return Err(error(
                            ErrorCode::Native,
                            "organization logical image identity mismatch",
                        ));
                    }
                    Ok(JobItem {
                        sequence: I64(v.sequence),
                        key: image.key,
                        image_id: image.id,
                        expected_revision: I64(v.expected_revision),
                        status: v.status,
                        error: v.error,
                        result_revision: v.result_revision.map(I64),
                    })
                })
                .collect::<Result<_>>()?;
            Response::Items(page(rows, limit, bounds, |v| v.sequence)?)
        }
        Request::Review {
            job,
            key: target,
            new_revision,
        } => {
            text(&job, false)?;
            key(&target)?;
            invalid(new_revision.is_none_or(|v| v.0 >= 0), "review revision")?;
            let image = catalog.image(&target).map_err(native)?;
            Response::Job(
                catalog
                    .review_organization_item(&job, image.sequence, new_revision.map(|v| v.0))
                    .map_err(native)?
                    .into(),
            )
        }
    };
    size(&response, bounds.reply_bytes)?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(c: &mut Catalog, r: Request) -> anyhow::Result<Response> {
        Ok(execute(c, r, &Limits::default())?)
    }
    fn fixture() -> anyhow::Result<(tempfile::TempDir, Catalog, VariantKey)> {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("photos/2026/arbitrary nesting");
        std::fs::create_dir_all(&originals)?;
        image::RgbImage::from_pixel(8, 8, image::Rgb([30u8, 60, 90]))
            .save(originals.join("original.png"))?;
        let mut c = Catalog::open(temp.path().join("catalog"))?;
        c.import(temp.path().join("photos").as_path(), None, |_| Ok(()))?;
        while c.organization_index(20)?.pending {}
        let key = c.browse_images(0, 1)?[0].key.clone();
        Ok((temp, c, key))
    }
    fn begin(c: &mut Catalog, op: core::Operation) -> anyhow::Result<String> {
        let Response::Job(job) = call(c, Request::Begin { operation: op })? else {
            panic!()
        };
        Ok(job.id)
    }
    fn revision(c: &Catalog, k: &VariantKey) -> anyhow::Result<I64> {
        Ok(I64(c.image_metadata_identity(k)?.metadata_revision))
    }
    fn items(c: &mut Catalog, job: &str) -> anyhow::Result<Vec<JobItem>> {
        let Response::Items(page) = call(
            c,
            Request::Items {
                job: job.into(),
                after: I64(0),
                limit: 100,
            },
        )?
        else {
            panic!()
        };
        Ok(page.rows)
    }
    #[test]
    fn variant_batch_is_durable_single_unit_cancelable_and_never_becomes_master()
    -> anyhow::Result<()> {
        let (temp, mut c, master) = fixture()?;
        let original = temp
            .path()
            .join("photos/2026/arbitrary nesting/original.png");
        let original_bytes = std::fs::read(&original)?;
        let master_revision = revision(&c, &master)?;
        let edit_revision = c.edit_variant(&master)?.revision;
        let a = c.create_edit_variant(&master, edit_revision, "first")?.key;
        let b = c.create_edit_variant(&master, edit_revision, "second")?.key;
        let selection = vec![
            BatchItem {
                key: a.clone(),
                expected_revision: revision(&c, &a)?,
            },
            BatchItem {
                key: b.clone(),
                expected_revision: revision(&c, &b)?,
            },
        ];
        let job = begin(
            &mut c,
            core::Operation::Flag {
                value: core::Flag::Pick,
            },
        )?;
        call(
            &mut c,
            Request::Append {
                job: job.clone(),
                items: selection.clone(),
            },
        )?;
        let mut changed = selection.clone();
        changed[0].expected_revision.0 += 1;
        assert!(
            call(
                &mut c,
                Request::Append {
                    job: job.clone(),
                    items: changed
                }
            )
            .is_err()
        );
        call(&mut c, Request::Seal { job: job.clone() })?;
        let Response::Job(after) = call(&mut c, Request::Step { job: job.clone() })? else {
            panic!()
        };
        assert_eq!(after.applied, I64(1));
        assert_eq!(after.pending, I64(1));
        let page = items(&mut c, &job)?;
        assert_eq!(page.len(), 2);
        assert!(page.iter().any(|v| v.key == a));
        assert!(page.iter().any(|v| v.key == b));
        assert!(page.iter().all(|v| v.key != master));
        assert_eq!(revision(&c, &master)?, master_revision);
        drop(c);
        let mut c = Catalog::open(temp.path().join("catalog"))?;
        let Response::Job(after) = call(&mut c, Request::Job { job: job.clone() })? else {
            panic!()
        };
        assert_eq!(after.applied, I64(1));
        call(&mut c, Request::Cancel { job: job.clone() })?;
        assert!(call(&mut c, Request::Step { job: job.clone() }).is_err());
        let page = items(&mut c, &job)?;
        assert_eq!(page.iter().filter(|v| v.status == "applied").count(), 1);
        assert_eq!(page.iter().filter(|v| v.status == "pending").count(), 1);
        assert_eq!(std::fs::read(original)?, original_bytes);
        Ok(())
    }
    #[test]
    fn stale_variant_revision_pauses_then_explicit_review_resumes() -> anyhow::Result<()> {
        let (_temp, mut c, master) = fixture()?;
        let v = c
            .create_edit_variant(&master, c.edit_variant(&master)?.revision, "copy")?
            .key;
        let before = revision(&c, &v)?;
        let job = begin(
            &mut c,
            core::Operation::Flag {
                value: core::Flag::Pick,
            },
        )?;
        call(
            &mut c,
            Request::Append {
                job: job.clone(),
                items: vec![BatchItem {
                    key: v.clone(),
                    expected_revision: before,
                }],
            },
        )?;
        call(&mut c, Request::Seal { job: job.clone() })?;
        call(
            &mut c,
            Request::Apply {
                key: v.clone(),
                expected_revision: before,
                operation: core::Operation::Flag {
                    value: core::Flag::Reject,
                },
            },
        )?;
        let Response::Job(paused) = call(&mut c, Request::Step { job: job.clone() })? else {
            panic!()
        };
        assert_eq!(paused.state, "paused");
        assert_eq!(paused.failed, I64(1));
        assert_eq!(items(&mut c, &job)?[0].key, v);
        assert!(
            call(
                &mut c,
                Request::Review {
                    job: job.clone(),
                    key: master.clone(),
                    new_revision: None
                }
            )
            .is_err()
        );
        let current = revision(&c, &v)?;
        call(
            &mut c,
            Request::Review {
                job: job.clone(),
                key: v.clone(),
                new_revision: Some(current),
            },
        )?;
        let Response::Job(done) = call(&mut c, Request::Step { job: job.clone() })? else {
            panic!()
        };
        assert_eq!(done.state, "complete");
        assert_eq!(done.applied, I64(1));
        assert_eq!(
            items(&mut c, &job)?[0].result_revision,
            Some(I64(current.0 + 1))
        );
        Ok(())
    }
    #[test]
    fn hierarchy_collection_cas_membership_and_synonyms_preserve_evidence() -> anyhow::Result<()> {
        let (_temp, mut c, master) = fixture()?;
        let Response::KeywordCreated { id } = call(
            &mut c,
            Request::CreateKeyword {
                kind: core::KeywordKind::Hierarchical,
                path: vec!["Nature".into(), "Birds".into()],
            },
        )?
        else {
            panic!()
        };
        let provenance = serde_json::json!({"source":"Lightroom","retained_source_id":u64::MAX,"unsupported":{"rule":"kept"}});
        c.add_keyword_synonym(id.0, "aves", &provenance)?;
        assert!(
            call(
                &mut c,
                Request::AddSynonym {
                    keyword: id,
                    synonym: "aves".into()
                }
            )
            .is_err()
        );
        call(
            &mut c,
            Request::AddSynonym {
                keyword: id,
                synonym: "birds".into(),
            },
        )?;
        let Response::Synonyms(synonyms) = call(
            &mut c,
            Request::Synonyms {
                keyword: id,
                after: String::new(),
                limit: 100,
            },
        )?
        else {
            panic!()
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&synonyms.rows[0].provenance_json)?,
            provenance
        );
        let Response::Keywords(roots) = call(
            &mut c,
            Request::Keywords {
                kind: core::KeywordKind::Hierarchical,
                parent: None,
                after: I64(0),
                limit: 100,
            },
        )?
        else {
            panic!()
        };
        assert_eq!(roots.rows[0].name, "Nature");
        assert!(
            call(
                &mut c,
                Request::DeleteKeyword {
                    id: roots.rows[0].id
                }
            )
            .is_err()
        );
        let parent = c.create_collection("parent", serde_json::json!({"kind":"set"}))?;
        let child = c.create_collection("retained smart", provenance.clone())?;
        let collection_revision = c
            .organization_collections("", 100)?
            .into_iter()
            .find(|v| v.id == child)
            .unwrap()
            .revision;
        call(
            &mut c,
            Request::PlaceCollection {
                collection: child.clone(),
                expected_revision: I64(collection_revision),
                parent: Some(parent.clone()),
                position: I64(4),
            },
        )?;
        assert!(
            call(
                &mut c,
                Request::PlaceCollection {
                    collection: child.clone(),
                    expected_revision: I64(collection_revision),
                    parent: None,
                    position: I64(0)
                }
            )
            .is_err()
        );
        assert_eq!(c.collection_placement(&child)?.parent, Some(parent));
        assert!(
            call(
                &mut c,
                Request::RenameCollection {
                    id: child.clone(),
                    expected_revision: I64(collection_revision),
                    name: "stale rename".into()
                }
            )
            .is_err()
        );
        call(
            &mut c,
            Request::RenameCollection {
                id: child.clone(),
                expected_revision: I64(collection_revision + 1),
                name: "retained renamed".into(),
            },
        )?;
        let current = c
            .organization_collections("", 100)?
            .into_iter()
            .find(|v| v.id == child)
            .unwrap();
        assert_eq!(current.provenance, provenance);
        let v = c
            .create_edit_variant(&master, c.edit_variant(&master)?.revision, "member")?
            .key;
        let before_keyword = revision(&c, &v)?;
        call(
            &mut c,
            Request::Apply {
                key: v.clone(),
                expected_revision: before_keyword,
                operation: core::Operation::AddKeyword {
                    kind: core::KeywordKind::Hierarchical,
                    path: vec!["Nature".into(), "Birds".into()],
                },
            },
        )?;
        while c.organization_index(20)?.pending {}
        let matches = c.search(
            &crate::organization_search::Query {
                include_variants: true,
                keyword: Some(id.0),
                ..Default::default()
            },
            None,
            100,
            512,
        )?;
        assert_eq!(matches.rows.len(), 1);
        assert_eq!(matches.rows[0].variant_id, v.variant_id);
        assert!(call(&mut c, Request::DeleteKeyword { id }).is_err());
        let identity: ImageIdentity = c.image_metadata_identity(&v)?.into();
        call(
            &mut c,
            Request::SetMember {
                identity: identity.clone(),
                collection: child.clone(),
                position: I64(9007199254740993),
            },
        )?;
        assert!(
            call(
                &mut c,
                Request::SetMember {
                    identity,
                    collection: child.clone(),
                    position: I64(0)
                }
            )
            .is_err()
        );
        let Response::Members(page) = call(
            &mut c,
            Request::Members {
                collection: child.clone(),
                after: None,
                limit: 100,
            },
        )?
        else {
            panic!()
        };
        assert_eq!(page.rows[0].key, v);
        assert_eq!(page.rows[0].cursor.position, I64(9007199254740993));
        let expected = revision(&c, &v)?;
        call(
            &mut c,
            Request::Apply {
                key: v,
                expected_revision: expected,
                operation: core::Operation::RemoveCollection {
                    collection: child.clone(),
                },
            },
        )?;
        let latest = c
            .organization_collections("", 100)?
            .into_iter()
            .find(|v| v.id == child)
            .unwrap()
            .revision;
        assert!(
            call(
                &mut c,
                Request::DeleteCollection {
                    id: child.clone(),
                    expected_revision: I64(latest - 1)
                }
            )
            .is_err()
        );
        call(
            &mut c,
            Request::DeleteCollection {
                id: child,
                expected_revision: I64(latest),
            },
        )?;
        Ok(())
    }
    #[test]
    fn bounds_and_decimal_wire_reject_without_mutating_jobs() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let mut c = Catalog::open(temp.path().join("catalog"))?;
        let mut limits = Limits {
            reply_bytes: 1024,
            ..Default::default()
        };
        let op = core::Operation::AddKeyword {
            kind: core::KeywordKind::Hierarchical,
            path: vec!["x".repeat(1024); 4],
        };
        assert!(execute(&mut c, Request::Begin { operation: op }, &limits).is_err());
        assert!(c.organization_jobs("", 100)?.is_empty());
        assert!(
            call(
                &mut c,
                Request::Collections {
                    after: String::new(),
                    limit: 101
                }
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Request>(
                serde_json::json!({"command":"delete_keyword","args":{"id":9007199254740993u64}})
            )
            .is_err()
        );
        let dto = MemberCursor {
            position: I64(i64::MAX),
            sequence: I64(9007199254740993),
        };
        let wire = serde_json::to_string(&dto)?;
        assert!(wire.contains("\"9223372036854775807\""));
        let round: MemberCursor = serde_json::from_str(&wire)?;
        assert_eq!(round.sequence, dto.sequence);
        for n in 0..8 {
            c.create_collection(
                &format!("collection-{n}"),
                serde_json::json!({"retained":"x".repeat(200)}),
            )?;
        }
        limits.page_bytes = 1024;
        let mut after = String::new();
        let mut ids = std::collections::BTreeSet::new();
        loop {
            let Response::Collections(page) =
                execute(&mut c, Request::Collections { after, limit: 100 }, &limits)?
            else {
                panic!()
            };
            for row in page.rows {
                assert!(ids.insert(row.id));
            }
            if let Some(next) = page.next {
                after = next;
            } else {
                break;
            }
        }
        assert_eq!(ids.len(), 8);
        Ok(())
    }
    #[test]
    fn membership_progress_handles_sparse_defaults_byte_cuts_and_revision_changes()
    -> anyhow::Result<()> {
        let (_temp, mut c, master) = fixture()?;
        let collection = c.create_collection("ordered", serde_json::json!({}))?;
        let mut keys = Vec::new();
        for n in 0..12 {
            let v = c
                .create_edit_variant(&master, c.edit_variant(&master)?.revision, &format!("{n}"))?
                .key;
            c.set_image_collection_membership(
                &c.image_metadata_identity(&v)?,
                &collection,
                n + 1,
                &serde_json::json!({"retained":"x".repeat(400)}),
            )?;
            keys.push(v);
        }
        let limits = Limits {
            page_rows: 2,
            scan_rows: 3,
            page_bytes: 1024,
            reply_bytes: 1024,
            ..Default::default()
        };
        let Response::Members(first) = execute(
            &mut c,
            Request::Members {
                collection: collection.clone(),
                after: None,
                limit: 2,
            },
            &limits,
        )?
        else {
            panic!()
        };
        assert!(first.rows.is_empty());
        assert_eq!(first.scanned, U64(3));
        assert!(!first.next.as_ref().unwrap().ordered);
        let mut cursor = first.next;
        let mut found = Vec::new();
        let mut calls = 0;
        loop {
            let Response::Members(page) = execute(
                &mut c,
                Request::Members {
                    collection: collection.clone(),
                    after: cursor,
                    limit: 2,
                },
                &limits,
            )?
            else {
                panic!()
            };
            assert!(page.scanned.0 <= 3);
            assert!(
                page.rows.len() <= 1,
                "byte cap should stop before second member"
            );
            found.extend(page.rows.into_iter().map(|v| v.key));
            cursor = page.next;
            calls += 1;
            if cursor.is_none() {
                break;
            }
            assert!(calls < 100);
        }
        assert_eq!(found, keys);
        let expected = revision(&c, &master)?;
        call(
            &mut c,
            Request::Apply {
                key: master.clone(),
                expected_revision: expected,
                operation: core::Operation::AddCollection {
                    collection: collection.clone(),
                },
            },
        )?;
        c.set_image_collection_membership(
            &c.image_metadata_identity(&keys[4])?,
            &collection,
            0,
            &serde_json::json!({}),
        )?;
        let expected = c
            .image_collection_members(&collection, None, 100)?
            .into_iter()
            .map(|v| v.1.key)
            .collect::<Vec<_>>();
        let mut cursor = None;
        let mut actual = Vec::new();
        loop {
            let Response::Members(page) = execute(
                &mut c,
                Request::Members {
                    collection: collection.clone(),
                    after: cursor,
                    limit: 2,
                },
                &limits,
            )?
            else {
                panic!()
            };
            actual.extend(page.rows.into_iter().map(|v| v.key));
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(actual, expected);
        assert_eq!(actual[0], master);
        assert_eq!(actual[1], keys[4]);
        let Response::Members(page) = execute(
            &mut c,
            Request::Members {
                collection: collection.clone(),
                after: None,
                limit: 2,
            },
            &limits,
        )?
        else {
            panic!()
        };
        let cursor = page.next.unwrap();
        c.rename_collection(&collection, cursor.revision.0, "changed")?;
        assert!(
            execute(
                &mut c,
                Request::Members {
                    collection,
                    after: Some(cursor),
                    limit: 2
                },
                &limits
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn restored_external_hold_keeps_local_organization_batch_available() -> anyhow::Result<()> {
        let (temp, mut c, master) = fixture()?;
        let expected = revision(&c, &master)?;
        let job = begin(
            &mut c,
            core::Operation::Flag {
                value: core::Flag::Pick,
            },
        )?;
        call(
            &mut c,
            Request::Append {
                job: job.clone(),
                items: vec![BatchItem {
                    key: master,
                    expected_revision: expected,
                }],
            },
        )?;
        call(&mut c, Request::Seal { job: job.clone() })?;
        drop(c);
        let backup = temp.path().join("backup");
        let restored = temp.path().join("restored");
        let limits = crate::catalog_backup::Limits {
            min_free_bytes: 0,
            ..Default::default()
        };
        crate::catalog_backup::backup_catalog(
            temp.path().join("catalog"),
            &backup,
            &limits,
            |_| Ok(()),
        )?;
        crate::catalog_backup::restore_catalog(&backup, &restored, &limits, |_| Ok(()))?;
        assert!(
            crate::catalog_backup::restore_status(&restored)?
                .unwrap()
                .jobs_held
        );
        let mut c = Catalog::open(&restored)?;
        let Response::Job(done) = call(&mut c, Request::Step { job })? else {
            panic!()
        };
        assert_eq!(done.state, "complete");
        assert!(
            crate::catalog_backup::restore_status(&restored)?
                .unwrap()
                .jobs_held
        );
        Ok(())
    }
}
