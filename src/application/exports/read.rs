use super::*;
use rusqlite::{OptionalExtension, params};
const PLAN_BYTES: usize = 128 * 1024;
fn sized(value: Option<String>, what: &str) -> Result<String> {
    value.ok_or_else(|| {
        error(
            ErrorCode::ResourceLimit,
            format!("export {what} exceeds byte allowance"),
        )
    })
}
pub(super) fn job(c: &Catalog, id: &str) -> Result<Job> {
    identity(id)?;
    let row:Option<(i64,Option<String>,i64,i64)>=c.db.query_row("SELECT sequence,CASE WHEN length(CAST(state AS BLOB))<=16 THEN state END,total,completed FROM photo_export_jobs WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|e|native(e.into()))?;
    let (s, state, total, completed) =
        row.ok_or_else(|| error(ErrorCode::InvalidRequest, "export job unavailable"))?;
    Ok(Job {
        sequence: I64(s),
        id: id.into(),
        state: sized(state, "job state")?,
        total: I64(total),
        completed: I64(completed),
    })
}
pub(super) fn document(
    c: &Catalog,
    id: &str,
    sequence: i64,
) -> Result<(core::PhotoPlanDocument, String)> {
    identity(id)?;
    if sequence <= 0 {
        return Err(error(
            ErrorCode::InvalidRequest,
            "positive export item sequence required",
        ));
    }
    let row:Option<(Option<String>,Option<String>)>=c.db.query_row("SELECT CASE WHEN length(CAST(plan AS BLOB))<=?3 THEN plan END,CASE WHEN length(CAST(authority AS BLOB))=64 THEN authority END FROM photo_export_items WHERE job=?1 AND sequence=?2",params![id,sequence,PLAN_BYTES as i64],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|native(e.into()))?;
    let (raw, hash) =
        row.ok_or_else(|| error(ErrorCode::InvalidRequest, "export item unavailable"))?;
    let raw = sized(raw, "plan")?;
    let hash = sized(hash, "authority")?;
    Ok((core::checked_plan(&raw, &hash).map_err(native)?, hash))
}
pub(super) fn name(c: &Catalog, key: &VariantKey) -> Result<Name> {
    key.validate().map_err(native)?;
    let row:Option<(Option<String>,Option<String>,bool)>=c.db.query_row("SELECT CASE WHEN length(CAST(a.path_display AS BLOB))<=32768 THEN a.path_display END,CASE WHEN length(CAST(COALESCE(v.label,'Master') AS BLOB))<=32768 THEN COALESCE(v.label,'Master') END,(?2='master' OR v.id IS NOT NULL) FROM assets a LEFT JOIN edit_variants v ON v.asset_id=a.id AND v.id=?2 WHERE a.id=?1",params![key.asset_id,key.variant_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|e|native(e.into()))?;
    let Some((path, label, available)) = row else {
        return Ok(Name {
            filename: "Unavailable photo".into(),
            variant_label: "Unavailable variant".into(),
            available: false,
        });
    };
    let path = sized(path, "filename")?;
    Ok(Name {
        filename: path.rsplit(['/', '\\']).next().unwrap_or(&path).into(),
        variant_label: sized(label, "variant label")?,
        available,
    })
}
pub(super) fn item(c: &Catalog, id: &str, sequence: i64, limit: usize) -> Result<Item> {
    let (plan, authority) = document(c, id, sequence)?;
    let(state,attempt,error_value,receipt): (Option<String>,Option<String>,Option<String>,Option<String>)=c.db.query_row("SELECT CASE WHEN length(CAST(state AS BLOB))<=16 THEN state END,CASE WHEN attempt IS NULL THEN '' WHEN length(CAST(attempt AS BLOB))<=256 THEN attempt END,CASE WHEN error IS NULL THEN '' WHEN length(CAST(error AS BLOB))<=?3 THEN error END,CASE WHEN receipt IS NULL THEN '' WHEN length(CAST(receipt AS BLOB))<=?3 THEN receipt END FROM photo_export_items WHERE job=?1 AND sequence=?2",params![id,sequence,limit as i64],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|e|native(e.into()))?;
    let attempt = sized(attempt, "attempt")?;
    let error_value = sized(error_value, "item error")?;
    let receipt = sized(receipt, "receipt")?;
    let value = Item {
        sequence: I64(sequence),
        key: plan.identity.key.clone(),
        name: name(c, &plan.identity.key)?,
        destination: NativePath::from_path(&plan.destination.destination),
        state: sized(state, "state")?,
        attempt: (!attempt.is_empty()).then_some(attempt),
        authority,
        error: (!error_value.is_empty()).then_some(error_value),
        receipt: if receipt.is_empty() {
            None
        } else {
            Some(
                serde_json::from_str::<crate::metadata_export::ExportReceipt>(&receipt)
                    .map_err(|e| native(e.into()))?
                    .into(),
            )
        },
    };
    bounded(&value, limit)?;
    Ok(value)
}
fn blob(c: &Catalog, digest: &str) -> Result<Blob> {
    let bytes:i64=c.db.query_row("SELECT raw_length FROM photo_export_blobs WHERE hash=?1 AND raw_length BETWEEN 0 AND 16777216",[digest],|r|r.get(0)).map_err(|e|native(e.into()))?;
    Ok(Blob {
        digest: digest.into(),
        bytes: U64(bytes as u64),
    })
}
fn render_identity(v: &crate::catalog_edits::EditRenderIdentity) -> RenderIdentity {
    RenderIdentity {
        image_identity: v.image_identity.as_ref().map(|i| ImageIdentity {
            image_id: i.image_id.clone(),
            key: i.key.clone(),
            metadata_revision: I64(i.metadata_revision),
            pixel_generation: I64(i.pixel_generation),
            shared_source_epoch: I64(i.shared_source_epoch),
            physical_generation: I64(i.physical_generation),
        }),
        source: SourceIdentity {
            asset_id: v.source.asset_id.clone(),
            generation: I64(v.source.generation),
            fingerprint: v.source.fingerprint.clone(),
            state: v.source.state.clone(),
            metadata_revision: I64(v.source.metadata_revision),
        },
        key: v.key.clone(),
        revision: I64(v.revision),
        recipe_digest: v.recipe_digest.clone(),
    }
}
pub(super) fn plan(
    c: &Catalog,
    id: &str,
    sequence: i64,
    limit: usize,
    cache: &Cache,
) -> Result<Plan> {
    let (p, authority) = document(c, id, sequence)?;
    let profile = match &p.output.profile {
        core::StoredProfile::Srgb => StoredProfile::Srgb,
        core::StoredProfile::LinearSrgb => StoredProfile::LinearSrgb,
        core::StoredProfile::Icc { blob: digest } => StoredProfile::Icc {
            blob: digest.clone(),
            bytes: blob(c, digest)?.bytes,
            linear: cache
                .profiles
                .values()
                .find(|v| v.info.blake3 == *digest)
                .map(|v| v.info.linear),
        },
    };
    let value = Plan {
        job: id.into(),
        sequence: I64(sequence),
        authority,
        plan_bytes: U64(p.raw().len() as u64),
        version: p.version,
        renderer_identity: p.renderer_identity.clone(),
        name: name(c, &p.identity.key)?,
        identity: render_identity(&p.identity),
        original: p.original.clone(),
        original_revision: p.original_revision.clone().into(),
        output: StoredOutput {
            size: p.output.size,
            format: p.output.format,
            profile,
            alpha: p.output.alpha,
        },
        metadata: metadata(&p.metadata),
        xmp_blob: p.xmp_blob.as_ref().map(|s| blob(c, s)).transpose()?,
        destination: DestinationSnapshot {
            version: p.destination.version,
            operation: p.destination.operation.clone(),
            destination: NativePath::from_path(&p.destination.destination),
            expected: p.destination.expected.clone().map(Into::into),
            max_existing_bytes: U64(p.destination.max_existing_bytes),
        },
        budgets: Budgets {
            max_original_bytes: U64(p.max_original_bytes),
            max_payload_bytes: U64(p.max_payload_bytes),
            alias_limits: AliasLimits {
                directories: U64(p.alias_limits.directories as u64),
                candidates: U64(p.alias_limits.candidates as u64),
            },
        },
        item: item(c, id, sequence, limit)?,
    };
    bounded(&value, limit)?;
    Ok(value)
}
fn metadata(v: &core::MetadataSelection) -> Metadata {
    match v {
        core::MetadataSelection::Omit => Metadata::Omit,
        core::MetadataSelection::Resolved {
            expected_revision,
            base_model,
        } => Metadata::Resolved {
            expected_revision: I64(*expected_revision),
            base_model: base_model.map(I64),
        },
    }
}
pub(super) fn execute(
    c: &Catalog,
    r: Request,
    limits: &Limits,
    cache: &Mutex<Cache>,
    options: &Options,
) -> Result<Response> {
    let response = match r {
        Request::Options => Response::Options(options.clone()),
        Request::Job { job: id } => Response::Job(job(c, &id)?),
        Request::Plan { job: id, sequence } => Response::Plan(Box::new(plan(
            c,
            &id,
            sequence.0,
            limits.page_bytes,
            &cache.lock().unwrap(),
        )?)),
        Request::PlanChunk {
            job: id,
            sequence,
            authority,
            offset,
            bytes,
        } => {
            let (p, hash) = document(c, &id, sequence.0)?;
            if hash != authority {
                return Err(error(ErrorCode::Superseded, "export authority changed"));
            }
            if bytes.0 == 0 || bytes.0 > 32768 {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "plan chunk allowance1..32768",
                ));
            }
            let start = usize::try_from(offset.0).map_err(|e| native(e.into()))?;
            if start > p.raw().len() || !p.raw().is_char_boundary(start) {
                return Err(error(ErrorCode::InvalidRequest, "invalid plan byte cursor"));
            }
            let mut end = start.saturating_add(bytes.0 as usize).min(p.raw().len());
            while !p.raw().is_char_boundary(end) {
                end -= 1;
            }
            if end == start && start < p.raw().len() {
                return Err(error(
                    ErrorCode::ResourceLimit,
                    "chunk cannot hold next UTF-8 character",
                ));
            }
            Response::PlanChunk(PlanChunk {
                job: id,
                sequence,
                authority,
                offset,
                next: (end < p.raw().len()).then_some(U64(end as u64)),
                total_bytes: U64(p.raw().len() as u64),
                text: p.raw()[start..end].into(),
            })
        }
        Request::Jobs { after, limit } => {
            let n = page(after.0, limit, limits)?;
            let mut rows = Vec::new();
            let mut cursor = after.0;
            let mut more = false;
            loop {
                let v:Option<(i64,Option<String>)>=c.db.query_row("SELECT sequence,CASE WHEN length(CAST(id AS BLOB))<=256 THEN id END FROM photo_export_jobs WHERE sequence>?1 ORDER BY sequence LIMIT 1",[cursor],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|native(e.into()))?;
                let Some((seq, id)) = v else { break };
                if rows.len() == n {
                    more = true;
                    break;
                }
                rows.push(job(c, &sized(id, "job ID")?)?);
                if bounded(&rows, limits.page_bytes).is_err() {
                    rows.pop();
                    if rows.is_empty() {
                        return Err(error(
                            ErrorCode::ResourceLimit,
                            "single export job exceeds page bytes",
                        ));
                    }
                    more = true;
                    break;
                }
                cursor = seq;
            }
            Response::Jobs {
                rows,
                next: more.then_some(I64(cursor)),
            }
        }
        Request::Items {
            job: id,
            after,
            limit,
        } => {
            job(c, &id)?;
            let n = page(after.0, limit, limits)?;
            let mut rows = Vec::new();
            let mut cursor = after.0;
            let mut more = false;
            loop {
                let seq:Option<i64>=c.db.query_row("SELECT sequence FROM photo_export_items WHERE job=?1 AND sequence>?2 ORDER BY sequence LIMIT 1",params![id,cursor],|r|r.get(0)).optional().map_err(|e|native(e.into()))?;
                let Some(seq) = seq else { break };
                if rows.len() == n {
                    more = true;
                    break;
                }
                rows.push(item(c, &id, seq, limits.page_bytes)?);
                if bounded(&rows, limits.page_bytes).is_err() {
                    rows.pop();
                    if rows.is_empty() {
                        return Err(error(
                            ErrorCode::ResourceLimit,
                            "single export item exceeds page bytes",
                        ));
                    }
                    more = true;
                    break;
                }
                cursor = seq;
            }
            Response::Items {
                rows,
                next: more.then_some(I64(cursor)),
            }
        }
        Request::ProfileRelease { token } => {
            cache
                .lock()
                .unwrap()
                .profiles
                .remove(&token)
                .ok_or_else(|| error(ErrorCode::StaleSession, "profile token unavailable"))?;
            Response::Released { token }
        }
        Request::ResultRelease { token } => {
            cache
                .lock()
                .unwrap()
                .destinations
                .remove(&token)
                .ok_or_else(|| error(ErrorCode::StaleSession, "destination result unavailable"))?;
            Response::Released { token }
        }
        Request::DestinationRows {
            token,
            after,
            limit,
        } => {
            let n = page(
                i64::try_from(after.0).map_err(|e| native(e.into()))?,
                limit,
                limits,
            )?;
            let cache = cache.lock().unwrap();
            let saved = cache
                .destinations
                .get(&token)
                .ok_or_else(|| error(ErrorCode::StaleSession, "destination result unavailable"))?;
            let start = usize::try_from(after.0).map_err(|e| native(e.into()))?;
            if start > saved.len() {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "destination cursor outside result",
                ));
            }
            let mut rows = Vec::new();
            let mut end = start;
            for row in saved.iter().skip(start).take(n) {
                rows.push(row.clone());
                if bounded(&rows, limits.page_bytes).is_err() {
                    rows.pop();
                    break;
                }
                end += 1;
            }
            if rows.is_empty() && end < saved.len() {
                return Err(error(
                    ErrorCode::ResourceLimit,
                    "single destination exceeds page bytes",
                ));
            }
            Response::Destinations {
                rows,
                next: (end < saved.len()).then_some(U64(end as u64)),
                total: U64(saved.len() as u64),
            }
        }
        _ => {
            return Err(error(
                ErrorCode::InvalidRequest,
                "not an export read request",
            ));
        }
    };
    bounded(&response, limits.reply_bytes)?;
    Ok(response)
}
