//! Versioned review and detached filesystem preparation. Candidate workers own
//! only bounded values: no database handle, writer permit, renderer, or publisher.
use super::*;
use std::path::PathBuf;

pub(crate) const REVIEW_SCHEMA: &str = "
ALTER TABLE storage_plans ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE storage_plans ADD COLUMN review_token TEXT;
ALTER TABLE storage_plans ADD COLUMN rules TEXT NOT NULL DEFAULT '[]';
CREATE TABLE storage_review_summary(plan TEXT PRIMARY KEY REFERENCES storage_plans(id), total INTEGER NOT NULL DEFAULT 0, matched INTEGER NOT NULL DEFAULT 0, excluded INTEGER NOT NULL DEFAULT 0, unverified INTEGER NOT NULL DEFAULT 0, confirmed INTEGER NOT NULL DEFAULT 0, unresolved INTEGER NOT NULL DEFAULT 0, unresolved_sources INTEGER NOT NULL DEFAULT 0);
CREATE TRIGGER storage_review_plan AFTER INSERT ON storage_plans BEGIN INSERT INTO storage_review_summary(plan) VALUES(new.id); END;
CREATE TRIGGER storage_review_item_insert AFTER INSERT ON storage_items BEGIN UPDATE storage_review_summary SET total=total+1, matched=matched+(new.status='matched'), excluded=excluded+(new.status='excluded'), unverified=unverified+(new.status='unverified'), confirmed=confirmed+(new.status='user_confirmed'), unresolved=unresolved+(new.status NOT IN ('matched','user_confirmed','excluded')) WHERE plan=new.plan; END;
CREATE TRIGGER storage_review_item_update AFTER UPDATE OF status ON storage_items BEGIN UPDATE storage_review_summary SET matched=matched+(new.status='matched')-(old.status='matched'), excluded=excluded+(new.status='excluded')-(old.status='excluded'), unverified=unverified+(new.status='unverified')-(old.status='unverified'), confirmed=confirmed+(new.status='user_confirmed')-(old.status='user_confirmed'), unresolved=unresolved+(new.status NOT IN ('matched','user_confirmed','excluded'))-(old.status NOT IN ('matched','user_confirmed','excluded')), unresolved_sources=unresolved_sources+((new.status!='excluded')-(old.status!='excluded'))*(SELECT COUNT(*) FROM storage_source_items WHERE plan=new.plan AND sequence=new.sequence AND status NOT IN ('matched','excluded','historical')) WHERE plan=new.plan; END;
CREATE TRIGGER storage_review_source_insert AFTER INSERT ON storage_source_items BEGIN UPDATE storage_review_summary SET unresolved_sources=unresolved_sources+(new.status NOT IN ('matched','excluded','historical') AND EXISTS(SELECT 1 FROM storage_items WHERE plan=new.plan AND sequence=new.sequence AND status!='excluded')) WHERE plan=new.plan; END;
CREATE TRIGGER storage_review_source_update AFTER UPDATE OF status ON storage_source_items BEGIN UPDATE storage_review_summary SET unresolved_sources=unresolved_sources+((new.status NOT IN ('matched','excluded','historical'))-(old.status NOT IN ('matched','excluded','historical')))*EXISTS(SELECT 1 FROM storage_items WHERE plan=new.plan AND sequence=new.sequence AND status!='excluded') WHERE plan=new.plan; END;
CREATE TABLE storage_source_fences(asset_id TEXT PRIMARY KEY REFERENCES assets(id), hash TEXT NOT NULL, authority_plan TEXT NOT NULL REFERENCES storage_plans(id));
CREATE TRIGGER storage_reviewed_fingerprint BEFORE UPDATE OF fingerprint ON assets WHEN new.fingerprint IS NOT NULL AND EXISTS(SELECT 1 FROM storage_source_fences WHERE asset_id=new.id AND hash!=new.fingerprint) BEGIN SELECT RAISE(ABORT,'original differs from user-reviewed source fence'); END;
CREATE TABLE storage_hydration_transitions(asset_id TEXT PRIMARY KEY REFERENCES assets(id), plan TEXT NOT NULL REFERENCES storage_plans(id), before_state TEXT NOT NULL, after_state TEXT NOT NULL);
";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelinkOverride {
    Asset {
        asset_id: String,
        candidates: Vec<NativePath>,
    },
    Source {
        source_id: i64,
        candidates: Vec<NativePath>,
    },
    Prefix {
        from: PathReference,
        destinations: Vec<NativePath>,
    },
}
#[derive(Debug)]
pub struct RelinkPreparation {
    observer: crate::catalog_session::storage::Observer,
    plan: String,
    revision: i64,
    epoch: i64,
    cursor: i64,
    next: i64,
    finished: bool,
    root: PathBuf,
    rows: Vec<PreparationRow>,
}
#[derive(Debug)]
struct PreparationRow {
    sequence: i64,
    asset: String,
    request: RelinkScope,
    paths: Vec<NativePath>,
    excluded: bool,
    allow_unverified: bool,
    data: ItemData,
    sources: Vec<SourceInput>,
}
#[derive(Debug)]
struct SourceInput {
    id: i64,
    data: SourceData,
    expected: Option<String>,
    override_paths: Option<Vec<NativePath>>,
    historical: bool,
}
#[derive(Debug)]
pub struct PreparedRelinkBatch {
    snapshot: RelinkPreparation,
    rows: Vec<PreparedRow>,
}
#[derive(Debug)]
struct PreparedRow {
    sequence: i64,
    asset: String,
    status: MatchStatus,
    detail: String,
    data: ItemData,
    sources: Vec<(i64, MatchStatus, String, SourceData)>,
}
const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const MAX_SOURCES: usize = 1024;
const MAX_BATCH_ASSETS: usize = 64;
// Preflight counts and byte lengths before Rust materializes source values.
const SOURCE_BYTES_SQL: &str = "length(s.locator)+length(CAST(s.display AS BLOB))+length(CAST(s.kind AS BLOB))+length(CAST(s.availability AS BLOB))+COALESCE(length(CAST(o.provenance AS BLOB)),0)+COALESCE(length(CAST(l.tag AS BLOB)),0)";
fn source_snapshot_usage(db: &Connection, asset: &str) -> Result<(usize, usize)> {
    let sql = format!(
        "SELECT COUNT(*),COALESCE(SUM({SOURCE_BYTES_SQL}),0) FROM (SELECT id FROM metadata_sources WHERE asset_id=? AND kind IN ('embedded','sidecar') LIMIT 1025) ids JOIN metadata_sources s ON s.id=ids.id LEFT JOIN metadata_observations o ON o.id=s.current_observation LEFT JOIN storage_source_locators l ON l.source_id=s.id"
    );
    let (count, bytes): (i64, i64) = db.query_row(&sql, [asset], |r| Ok((r.get(0)?, r.get(1)?)))?;
    ensure!(
        count <= MAX_SOURCES as i64,
        "relink source snapshot exceeds 1024 sources per asset; review source custody"
    );
    ensure!(
        bytes <= MAX_SNAPSHOT_BYTES as i64,
        "relink source snapshot exceeds 64 MiB per asset; review source custody"
    );
    Ok((count as usize, bytes as usize))
}
pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "relink canceled");
    Ok(())
}
pub(super) fn require_revision(db: &Connection, plan: &str, revision: i64) -> Result<()> {
    let current: i64 = db.query_row(
        "SELECT revision FROM storage_plans WHERE id=?",
        [plan],
        |r| r.get(0),
    )?;
    ensure!(
        current == revision,
        "relink review changed; reload before deciding"
    );
    Ok(())
}
pub(super) fn bump_revision(db: &Connection, plan: &str) -> Result<()> {
    db.execute(
        "UPDATE storage_plans SET revision=revision+1,review_token=?2 WHERE id=?1",
        params![plan, uuid::Uuid::new_v4().to_string()],
    )?;
    Ok(())
}
pub(super) fn review_counts(
    db: &Connection,
    plan: &str,
) -> Result<(i64, i64, i64, Option<String>)> {
    Ok(db.query_row("SELECT p.revision,COALESCE(s.unverified,0),COALESCE(s.confirmed,0),CASE WHEN p.state='ready' AND s.unverified>0 THEN p.review_token END FROM storage_plans p LEFT JOIN storage_review_summary s ON s.plan=p.id WHERE p.id=?", [plan], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?)
}
fn valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn historical(
    db: &Connection,
    asset: &str,
    provenance: &serde_json::Value,
    locator: &[u8],
) -> Result<bool> {
    if !locator.starts_with(b"lightroom:")
        || provenance
            .pointer("/source/adapter")
            .and_then(|v| v.as_str())
            != Some("lightroom-file-metadata-v1")
    {
        return Ok(false);
    }
    let key: crate::catalog_migration::originals::SourceKey = serde_json::from_value(
        provenance
            .pointer("/source/file")
            .cloned()
            .context("retained Lightroom source key missing")?,
    )?;
    let identity = key.identity()?;
    ensure!(
        key.table == "AgLibraryFile"
            && locator.starts_with(format!("lightroom:{identity}:").as_bytes()),
        "retained Lightroom locator/source identity differs"
    );
    let owner: Option<String> = db
        .query_row(
            "SELECT asset_id FROM migration_originals WHERE source_identity=?",
            [identity],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        owner.as_deref() == Some(asset),
        "retained Lightroom source does not belong to this original"
    );
    Ok(true)
}
pub(super) fn expected_identity(
    db: &Connection,
    asset: &str,
    fingerprint: Option<&str>,
) -> Result<Option<String>> {
    let fence: Option<String> = db
        .query_row(
            "SELECT hash FROM storage_source_fences WHERE asset_id=?",
            [asset],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(fp) = fingerprint {
        ensure!(
            fence.as_deref().is_none_or(|hash| hash == fp),
            "source identity contradicts reviewed association"
        );
        return Ok(Some(fp.into()));
    }
    if fence.is_some() {
        return Ok(fence);
    }
    // Lightroom row IDs and packet hashes are not full original fingerprints.
    // A retained complete embedded-file observation can carry an actual one.
    source_snapshot_usage(db, asset)?;
    let mut hashes = std::collections::BTreeSet::new();
    let current = get_binding(db, asset)?.map(|b| b.native_path);
    let mut stmt = db.prepare("SELECT s.locator,CASE WHEN length(CAST(o.provenance AS BLOB))<=67108864 THEN o.provenance END,length(CAST(o.provenance AS BLOB)),o.status,s.id FROM metadata_sources s JOIN metadata_observations o ON o.id=s.current_observation WHERE s.asset_id=? AND s.kind='embedded' ORDER BY s.id LIMIT 1025")?;
    let mut count = 0;
    let mut bytes = 0usize;
    for row in stmt.query_map([asset], |r| {
        let length = r.get::<_, i64>(2)? as usize;
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            if length <= MAX_SNAPSHOT_BYTES {
                Some(r.get::<_, String>(1)?)
            } else {
                None
            },
            length,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })? {
        count += 1;
        let (locator, raw, length, status, source_id) = row?;
        bytes = bytes.saturating_add(length);
        ensure!(
            count <= MAX_SOURCES && bytes <= MAX_SNAPSHOT_BYTES,
            "retained source evidence exceeds relink limit"
        );
        let value: serde_json::Value =
            serde_json::from_str(&raw.context("source evidence too large")?)?;
        let retained = historical(db, asset, &value, &locator)?;
        if retained
            && value
                .pointer("/source/historical_observation/status")
                .and_then(|v| v.as_str())
                == Some("Complete")
        {
            let observed: Option<NativePath> = value
                .pointer("/source/source_path")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok());
            if current.is_some()
                && current == observed
                && let Some(hash) = value
                    .pointer("/file_revision/blake3")
                    .and_then(|v| v.as_str())
                    .filter(|v| valid_hash(v))
            {
                ensure!(
                    value
                        .pointer("/source/historical_observation/revision/blake3")
                        .and_then(|v| v.as_str())
                        == Some(hash),
                    "retained original revision differs from projected file evidence"
                );
                hashes.insert(hash.to_owned());
            }
        } else if !retained
            && matches!(status.as_str(), "Complete" | "Absent")
            && current
                .as_ref()
                .is_some_and(|native| encoded_bytes(native) == locator)
            && get_source_tag(db, source_id)?.is_none_or(|tag| tag.native == current)
            && value
                .pointer("/source_location/kind")
                .and_then(|v| v.as_str())
                == Some("embedded")
        {
            let observed: Option<Vec<u8>> = value
                .pointer("/source_location/locator")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok());
            if observed.as_ref() == Some(&locator)
                && let Some(hash) = value
                    .pointer("/file_revision/blake3")
                    .and_then(|v| v.as_str())
                    .filter(|v| valid_hash(v))
            {
                hashes.insert(hash.to_owned());
            }
        }
    }
    ensure!(
        hashes.len() <= 1,
        "retained original digests conflict; explicit evidence reconciliation required"
    );
    Ok(hashes.into_iter().next())
}
fn load_sources(
    db: &Connection,
    plan: &str,
    asset: &str,
    item: &ItemData,
    budget: &mut usize,
) -> Result<Vec<SourceInput>> {
    let query = format!(
        "SELECT s.id,s.kind,s.locator,s.display,s.availability,s.current_observation,CASE WHEN length(CAST(o.provenance AS BLOB))<=67108864 THEN o.provenance END,{SOURCE_BYTES_SQL} FROM metadata_sources s LEFT JOIN metadata_observations o ON o.id=s.current_observation LEFT JOIN storage_source_locators l ON l.source_id=s.id WHERE s.asset_id=?1 AND s.kind IN ('embedded','sidecar') ORDER BY s.id LIMIT 1025"
    );
    let mut stmt = db.prepare(&query)?;
    let mut out = Vec::new();
    for row in stmt.query_map([asset], |r| {
        let bytes = r.get::<_, i64>(7)? as usize;
        if bytes > MAX_SNAPSHOT_BYTES {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Vec<u8>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, Option<String>>(6)?,
            bytes,
        ))
    })? {
        let (id, kind, locator, display, availability, observation, provenance, bytes) = row?;
        *budget = budget.saturating_add(bytes);
        ensure!(
            out.len() < MAX_SOURCES && *budget <= MAX_SNAPSHOT_BYTES,
            "relink source snapshot exceeds 1024 sources per asset or 64 MiB per batch; reduce batch or review source custody"
        );
        let provenance: serde_json::Value = provenance
            .map(|v| serde_json::from_str(&v))
            .transpose()?
            .unwrap_or_default();
        let is_historical = historical(db, asset, &provenance, &locator)?;
        let expected = provenance
            .pointer("/file_revision/blake3")
            .and_then(|v| v.as_str())
            .filter(|s| valid_hash(s))
            .map(str::to_owned)
            .or_else(|| {
                if kind == "embedded" && observation.is_none() {
                    item.expected_identity.clone()
                } else {
                    None
                }
            });
        let old_native = if is_historical {
            None
        } else {
            source_native(db, id, item, &locator)?
        };
        out.push(SourceInput {
            id,
            expected,
            historical: is_historical,
            override_paths: exception(db, plan, "source", &id.to_string())?,
            data: SourceData {
                embedded: kind == "embedded",
                historical: is_historical,
                old_native,
                old_tag: get_source_tag(db, id)?,
                old_locator: locator,
                old_display: display,
                old_availability: availability,
                observation,
                candidates: vec![],
                destination: None,
                evidence: None,
            },
        });
    }
    Ok(out)
}
impl Catalog {
    /// Bounded read snapshot; no filesystem observation or writer admission.
    pub fn relink_preparation(&self, plan: &str, limit: usize) -> Result<RelinkPreparation> {
        ensure!((1..=1000).contains(&limit), "batch limit must be 1..1000");
        let limit = limit.min(MAX_BATCH_ASSETS);
        let tx = self.db.unchecked_transaction()?;
        let (request,snapshot,cursor,high,state,revision,rules): (String,i64,i64,i64,String,i64,String) = tx.query_row("SELECT request,epoch,cursor,high_water,state,revision,rules FROM storage_plans WHERE id=?", [plan], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
        ensure!(state == "preparing", "plan is not preparing");
        ensure!(
            snapshot == epoch(&tx)?,
            "stale relink plan: catalog locations or sources changed"
        );
        let request: RelinkScope = serde_json::from_str(&request)?;
        let rules: Vec<RelinkOverride> = serde_json::from_str(&rules)?;
        let (filter, value) = match &request {
            RelinkScope::Asset { asset_id, .. } => ("AND a.id=?4", asset_id.as_str()),
            RelinkScope::Volume { logical_volume, .. } => (
                "AND a.id IN (SELECT asset_id FROM storage_bindings WHERE volume_id=?4)",
                logical_volume.as_str(),
            ),
            _ => ("AND ?4 IS NOT NULL", ""),
        };
        let query = format!(
            "SELECT a.sequence,a.id FROM assets a WHERE a.sequence>?1 AND a.sequence<=?2 {filter} ORDER BY a.sequence LIMIT ?3"
        );
        let rows = tx
            .prepare(&query)?
            .query_map(params![cursor, high, limit as i64, value], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut finished = rows.len() < limit
            || matches!(request, RelinkScope::Asset { .. })
            || rows.last().is_some_and(|r| r.0 == high);
        let mut next = if finished {
            high
        } else {
            rows.last().map_or(high, |r| r.0)
        };
        let mut work = Vec::new();
        let mut budget = 0usize;
        let mut source_count = 0usize;
        let mut through = cursor;
        for (sequence, asset) in rows {
            let item_bytes:i64=tx.query_row("SELECT length(a.location)+length(CAST(a.path_display AS BLOB))+COALESCE(length(CAST(b.reference AS BLOB)),0)+COALESCE(length(CAST(b.native_path AS BLOB)),0)+COALESCE(length(CAST(b.relative AS BLOB)),0)+COALESCE(length(CAST(b.file_key AS BLOB)),0)+COALESCE(length(CAST(b.volume_id AS BLOB)),0)+COALESCE(length(CAST(a.fingerprint AS BLOB)),0) FROM assets a LEFT JOIN storage_bindings b ON b.asset_id=a.id WHERE a.id=?",[&asset],|r|r.get(0))?;
            ensure!(
                item_bytes <= MAX_SNAPSHOT_BYTES as i64,
                "relink asset snapshot exceeds 64 MiB; review oversized locator custody"
            );
            let mut data = load_item(&tx, &asset)?;
            // A nested folder rule cannot pull unrelated assets into this scope.
            let base =
                mapped_candidates(&request, &asset, &data.reference, data.old_binding.as_ref())?;
            let explicit = exception(&tx, plan, "asset", &asset)?;
            if base.is_none() && explicit.is_none() {
                through = sequence;
                continue;
            }
            // Admit complete asset/source sets before expanding retained evidence.
            // The actor publishes at most 64 assets and 1024 sources per batch.
            let excluded = explicit.as_ref().is_some_and(Vec::is_empty);
            let (sources_needed, source_bytes) = if excluded {
                (0, 0)
            } else {
                source_snapshot_usage(&tx, &asset)?
            };
            if source_count + sources_needed > MAX_SOURCES
                || (!work.is_empty()
                    && budget
                        .saturating_add(source_bytes)
                        .saturating_add(item_bytes as usize)
                        > MAX_SNAPSHOT_BYTES)
            {
                finished = false;
                next = through;
                break;
            }
            ensure!(
                (item_bytes as usize).saturating_add(source_bytes) <= MAX_SNAPSHOT_BYTES,
                "relink asset and source snapshot exceeds 64 MiB; review source custody"
            );
            data.expected_identity = expected_identity(&tx, &asset, data.fingerprint.as_deref())?;
            let fenced: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM storage_source_fences WHERE asset_id=?)",
                [&asset],
                |r| r.get(0),
            )?;
            data.identity_basis = if fenced {
                "user_confirmed_fence"
            } else if data.fingerprint.is_some() {
                "catalog_fingerprint"
            } else if data.expected_identity.is_some() {
                "retained_original_digest"
            } else {
                "unverified"
            }
            .into();

            let mut effective = request.clone();
            let mut longest = None;
            for rule in &rules {
                if let RelinkOverride::Prefix { from, destinations } = rule {
                    let scope = RelinkScope::Prefix {
                        from: from.clone(),
                        destinations: destinations.clone(),
                    };
                    if mapped_candidates(
                        &scope,
                        &asset,
                        &data.reference,
                        data.old_binding.as_ref(),
                    )?
                    .is_some()
                    {
                        let n = components(from)?.names.len();
                        if longest.is_none_or(|old| n > old) {
                            longest = Some(n);
                            effective = scope;
                        }
                    }
                }
            }
            let paths = explicit.unwrap_or(
                mapped_candidates(
                    &effective,
                    &asset,
                    &data.reference,
                    data.old_binding.as_ref(),
                )?
                .unwrap_or_default(),
            );
            let allow_unverified:bool=tx.query_row("SELECT state='pending' AND fingerprint IS NULL AND metadata IS NULL FROM assets WHERE id=?",[&asset],|r|r.get(0))?;
            budget = budget
                .saturating_add(json(&data)?.len())
                .saturating_add(json(&paths)?.len());
            ensure!(
                budget <= MAX_SNAPSHOT_BYTES,
                "relink snapshot exceeds 64 MiB; reduce batch"
            );
            let sources = if excluded {
                vec![]
            } else {
                load_sources(&tx, plan, &asset, &data, &mut budget)?
            };
            source_count += sources.len();
            through = sequence;
            work.push(PreparationRow {
                sequence,
                asset,
                request: effective,
                paths,
                excluded,
                allow_unverified,
                data,
                sources,
            });
        }
        tx.commit()?;
        Ok(RelinkPreparation {
            observer: crate::catalog_session::storage::Observer(self.session.clone()),
            plan: plan.into(),
            revision,
            epoch: snapshot,
            cursor,
            next,
            finished,
            root: self.root.clone(),
            rows: work,
        })
    }
    pub fn publish_relink_preparation(&mut self, batch: PreparedRelinkBatch) -> Result<RelinkPlan> {
        let work = batch.snapshot;
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        require_revision(&tx, &work.plan, work.revision)?;
        let (cursor, state): (i64, String) = tx.query_row(
            "SELECT cursor,state FROM storage_plans WHERE id=?",
            [&work.plan],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            cursor == work.cursor && state == "preparing" && epoch(&tx)? == work.epoch,
            "prepared relink snapshot is stale"
        );
        for row in batch.rows {
            tx.execute(
                "INSERT INTO storage_items VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    work.plan,
                    row.sequence,
                    row.asset,
                    row.status.name(),
                    row.detail,
                    row.data.destination.as_ref().map(encoded_bytes),
                    row.data.evidence.as_ref().map(Evidence::key),
                    json(&row.data)?
                ],
            )?;
            for (id, status, detail, data) in row.sources {
                tx.execute(
                    "INSERT INTO storage_source_items VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        work.plan,
                        row.sequence,
                        id,
                        status.name(),
                        detail,
                        data.destination.as_ref().map(encoded_bytes),
                        json(&data)?
                    ],
                )?;
            }
        }
        tx.execute(
            "UPDATE storage_plans SET cursor=?2,state=?3 WHERE id=?1",
            params![
                work.plan,
                work.next,
                if work.finished {
                    "checking"
                } else {
                    "preparing"
                }
            ],
        )?;
        bump_revision(&tx, &work.plan)?;
        tx.commit()?;
        drop(_write);
        self.relink_plan(&work.plan)
    }
    /// Full-plan collision reconciliation is deliberately an explicit worker
    /// phase, never hidden inside the last bounded actor publication.
    pub fn finalize_relink_review_cancellable(
        &mut self,
        plan: &str,
        revision: i64,
        cancel: &AtomicBool,
    ) -> Result<RelinkPlan> {
        check_cancel(cancel)?;
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let sql_cancel = SqlCancellation::new(&tx, cancel);
        require_revision(&tx, plan, revision)?;
        let (state, snapshot): (String, i64) = tx.query_row(
            "SELECT state,epoch FROM storage_plans WHERE id=?",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            state == "checking" && snapshot == epoch(&tx)?,
            "relink collision review is incomplete or stale"
        );
        mark_collisions(&tx, plan)?;
        tx.execute("UPDATE storage_plans SET state='ready' WHERE id=?", [plan])?;
        bump_revision(&tx, plan)?;
        check_cancel(cancel)?;
        drop(sql_cancel);
        tx.commit()?;
        drop(_write);
        self.relink_plan(plan)
    }
    /// Token binds every currently prepared eligible candidate. No future row or
    /// revised path inherits this acknowledgement. Confirmed is never 'matched'.
    pub fn confirm_relink_associations(
        &mut self,
        plan: &str,
        revision: i64,
        token: &str,
        acknowledgement: &str,
    ) -> Result<RelinkPlan> {
        self.confirm_relink_associations_cancellable(
            plan,
            revision,
            token,
            acknowledgement,
            &AtomicBool::new(false),
        )
    }
    pub fn confirm_relink_associations_cancellable(
        &mut self,
        plan: &str,
        revision: i64,
        token: &str,
        acknowledgement: &str,
        cancel: &AtomicBool,
    ) -> Result<RelinkPlan> {
        check_cancel(cancel)?;
        ensure!(
            acknowledgement == "no_retained_original_digest",
            "explicit no-retained-original-digest acknowledgement required"
        );
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let sql_cancel = SqlCancellation::new(&tx, cancel);
        require_revision(&tx, plan, revision)?;
        let (state, stored, snapshot): (String, Option<String>, i64) = tx.query_row(
            "SELECT state,review_token,epoch FROM storage_plans WHERE id=?",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        ensure!(
            state == "ready" && stored.as_deref() == Some(token) && snapshot == epoch(&tx)?,
            "association review token is stale"
        );
        let count=tx.execute("UPDATE storage_items SET status='user_confirmed',detail='User confirmed association without retained original digest; candidate bytes are fenced' WHERE plan=? AND status='unverified'",[plan])?;
        ensure!(count > 0, "no unverified associations in this review");
        bump_revision(&tx, plan)?;
        check_cancel(cancel)?;
        drop(sql_cancel);
        tx.commit()?;
        drop(_write);
        self.relink_plan(plan)
    }
    pub fn revise_relink(
        &mut self,
        plan: &str,
        revision: i64,
        changes: Vec<RelinkOverride>,
    ) -> Result<RelinkPlan> {
        self.revise_relink_cancellable(plan, revision, changes, &AtomicBool::new(false))
    }
    pub fn revise_relink_cancellable(
        &mut self,
        plan: &str,
        revision: i64,
        changes: Vec<RelinkOverride>,
        cancel: &AtomicBool,
    ) -> Result<RelinkPlan> {
        check_cancel(cancel)?;
        ensure!(
            changes.len() <= 1024 && json(&changes)?.len() <= 1024 * 1024,
            "relink override limit"
        );
        for change in &changes {
            match change {
                RelinkOverride::Asset { candidates, .. }
                | RelinkOverride::Source { candidates, .. } => validate_candidates(candidates)?,
                RelinkOverride::Prefix { from, destinations } => {
                    validate_request(&RelinkScope::Prefix {
                        from: from.clone(),
                        destinations: destinations.clone(),
                    })?
                }
            }
        }
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let sql_cancel = SqlCancellation::new(&tx, cancel);
        require_revision(&tx, plan, revision)?;
        let (request, rules): (String, String) = tx.query_row(
            "SELECT request,rules FROM storage_plans WHERE id=?",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut rules: Vec<RelinkOverride> = serde_json::from_str(&rules)?;
        let id = uuid::Uuid::new_v4().to_string();
        let high: i64 = tx.query_row("SELECT COALESCE(MAX(sequence),0) FROM assets", [], |r| {
            r.get(0)
        })?;
        tx.execute("INSERT INTO storage_plans(id,request,epoch,high_water,state) VALUES(?1,?2,?3,?4,'preparing')",params![id,request,epoch(&tx)?,high])?;
        tx.execute("INSERT INTO storage_exceptions SELECT ?2,kind,entity,candidates FROM storage_exceptions WHERE plan=?1",params![plan,id])?;
        // Ready exclusions must survive a revision as explicit exceptions.
        tx.execute("INSERT INTO storage_exceptions SELECT ?2,'asset',asset_id,'[]' FROM storage_items WHERE plan=?1 AND status='excluded' ON CONFLICT(plan,kind,entity) DO UPDATE SET candidates='[]'",params![plan,id])?;
        tx.execute("INSERT INTO storage_exceptions SELECT ?2,'source',CAST(source_id AS TEXT),'[]' FROM storage_source_items WHERE plan=?1 AND status='excluded' ON CONFLICT(plan,kind,entity) DO UPDATE SET candidates='[]'",params![plan,id])?;
        for change in changes {
            match change {
                RelinkOverride::Asset {
                    asset_id,
                    candidates,
                } => {
                    ensure!(
                        tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM assets WHERE id=?)",
                            [&asset_id],
                            |r| r.get::<_, bool>(0)
                        )?,
                        "override asset does not exist"
                    );
                    tx.execute("INSERT INTO storage_exceptions VALUES(?1,'asset',?2,?3) ON CONFLICT(plan,kind,entity) DO UPDATE SET candidates=excluded.candidates",params![id,asset_id,json(&candidates)?])?;
                }
                RelinkOverride::Source {
                    source_id,
                    candidates,
                } => {
                    ensure!(
                        tx.query_row(
                            "SELECT kind='sidecar' FROM metadata_sources WHERE id=?",
                            [source_id],
                            |r| r.get::<_, bool>(0)
                        )?,
                        "only sidecar candidate overrides are allowed"
                    );
                    tx.execute("INSERT INTO storage_exceptions VALUES(?1,'source',?2,?3) ON CONFLICT(plan,kind,entity) DO UPDATE SET candidates=excluded.candidates",params![id,source_id.to_string(),json(&candidates)?])?;
                }
                RelinkOverride::Prefix { from, destinations } => {
                    rules
                        .retain(|r| !matches!(r,RelinkOverride::Prefix{from:old,..} if old==&from));
                    rules.push(RelinkOverride::Prefix { from, destinations });
                }
            }
        }
        ensure!(
            rules.len() <= 1024 && json(&rules)?.len() <= 1024 * 1024,
            "relink prefix rule limit"
        );
        tx.execute(
            "UPDATE storage_plans SET rules=?2 WHERE id=?1",
            params![id, json(&rules)?],
        )?;
        check_cancel(cancel)?;
        drop(sql_cancel);
        tx.commit()?;
        drop(_write);
        self.relink_plan(&id)
    }
}
impl RelinkPreparation {
    pub fn prepare(mut self, cancel: &AtomicBool) -> Result<PreparedRelinkBatch> {
        let mut rows = Vec::new();
        for mut row in std::mem::take(&mut self.rows) {
            check_cancel(cancel)?;
            let (status, detail, candidates, selected) = if row.excluded {
                (
                    MatchStatus::Excluded,
                    "Explicitly excluded".into(),
                    vec![],
                    None,
                )
            } else {
                evaluate(
                    &self.observer,
                    &row.paths,
                    row.data.expected_identity.as_deref(),
                    &self.root,
                    row.allow_unverified,
                    cancel,
                )
            };
            check_cancel(cancel)?;
            row.data.candidates = candidates;
            if let Some((path, evidence)) = selected {
                row.data.binding = Some(binding_draft(&self.observer, &path.to_path()?, cancel)?);
                row.data.destination = Some(path);
                row.data.evidence = Some(evidence);
            }
            let mut sources = Vec::new();
            for source in row.sources {
                check_cancel(cancel)?;
                let (id, status, detail, data) = prepare_source(
                    &self.observer,
                    source,
                    &row.data,
                    &row.request,
                    &self.root,
                    cancel,
                )?;
                sources.push((id, status, detail, data));
            }
            rows.push(PreparedRow {
                sequence: row.sequence,
                asset: row.asset,
                status,
                detail,
                data: row.data,
                sources,
            });
        }
        check_cancel(cancel)?;
        Ok(PreparedRelinkBatch {
            snapshot: self,
            rows,
        })
    }
}
fn prepare_source(
    observer: &crate::catalog_session::storage::Observer,
    source: SourceInput,
    item: &ItemData,
    request: &RelinkScope,
    root: &Path,
    cancel: &AtomicBool,
) -> Result<(i64, MatchStatus, String, SourceData)> {
    let mut data = source.data;
    let (status, detail, candidates, selected) = if source.historical {
        (
            MatchStatus::Historical,
            "Retained historical Lightroom evidence; locator and packets remain unchanged".into(),
            vec![],
            None,
        )
    } else if source.override_paths.as_ref().is_some_and(Vec::is_empty) {
        (
            MatchStatus::Excluded,
            "Explicitly excluded".into(),
            vec![],
            None,
        )
    } else if data.embedded {
        match (item.destination.as_ref(), item.evidence.as_ref()) {
            (Some(path), Some(evidence))
                if data.old_locator == item.old_location
                    && source.expected.as_deref() == Some(&evidence.hash) =>
            {
                (
                    MatchStatus::Matched,
                    "Embedded source follows its verified original".into(),
                    vec![],
                    Some((path.clone(), evidence.clone())),
                )
            }
            _ => (
                MatchStatus::Mismatch,
                "Embedded observation does not match planned original".into(),
                vec![],
                None,
            ),
        }
    } else {
        let paths =
            source
                .override_paths
                .unwrap_or(sidecar_paths(request, item, data.old_native.clone())?);
        evaluate(
            observer,
            &paths,
            source.expected.as_deref(),
            root,
            false,
            cancel,
        )
    };
    check_cancel(cancel)?;
    data.candidates = candidates;
    if let Some((path, evidence)) = selected {
        data.destination = Some(path);
        data.evidence = Some(evidence);
    }
    Ok((source.id, status, detail, data))
}
pub(super) fn record_association(
    db: &Connection,
    plan: &str,
    asset: &str,
    data: &ItemData,
) -> Result<()> {
    if data.fingerprint.is_none() {
        let hash = &data
            .evidence
            .as_ref()
            .context("missing reviewed source evidence")?
            .hash;
        hydration_fence(db, asset, hash)?;
        db.execute(
            "INSERT INTO storage_source_fences VALUES(?1,?2,?3) ON CONFLICT(asset_id) DO NOTHING",
            params![asset, hash, plan],
        )?;
    }
    Ok(())
}
/// Exact raw location identity, before import can reserve or refresh metadata.
pub(crate) fn verify_location_fence(
    db: &Connection,
    location: &[u8],
    hash: Option<&str>,
) -> Result<()> {
    let fence: Option<String> = db.query_row(
        "SELECT f.hash FROM assets a JOIN storage_source_fences f ON f.asset_id=a.id WHERE a.location=?",
        [location], |r| r.get(0),
    ).optional()?;
    ensure!(
        fence.as_deref().is_none_or(|v| Some(v) == hash),
        "original bytes differ from user-reviewed relink candidate or cannot be read; import refused before metadata changes"
    );
    Ok(())
}
pub(crate) fn hydration_fence(db: &Connection, asset: &str, hash: &str) -> Result<()> {
    let fence: Option<String> = db
        .query_row(
            "SELECT hash FROM storage_source_fences WHERE asset_id=?",
            [asset],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        fence.as_deref().is_none_or(|v| v == hash),
        "original bytes differ from user-reviewed relink candidate; preview refused"
    );
    Ok(())
}
pub(crate) fn record_hydration(db: &Connection, asset: &str, before: Option<String>) -> Result<()> {
    let Some(before) = before else {
        return Ok(());
    };
    let current = state_of(db, asset)?;
    let mut expected: AppliedState = serde_json::from_str(&before)?;
    ensure!(
        expected.fingerprint.is_none(),
        "hydration lineage requires initial source"
    );
    let hash = current
        .fingerprint
        .as_deref()
        .context("hydration fingerprint missing")?;
    hydration_fence(db, asset, hash)?;
    expected.fingerprint = Some(hash.into());
    ensure!(
        expected == current,
        "hydration changed fields outside exact source readiness transition"
    );
    let head: Option<String> = db
        .query_row(
            "SELECT plan FROM storage_heads WHERE asset_id=?",
            [asset],
            |r| r.get(0),
        )
        .optional()?;
    let Some(head) = head else {
        return Ok(());
    };
    let stored: String = db.query_row(
        "SELECT after_state FROM storage_applied_items WHERE plan=?1 AND asset_id=?2",
        params![head, asset],
        |r| r.get(0),
    )?;
    if serde_json::from_str::<AppliedState>(&stored)?
        != serde_json::from_str::<AppliedState>(&before)?
    {
        return Ok(());
    }
    // Only an exact authorized readiness transition advances the valid undo
    // ancestry. No location, binding, edit, or metadata counter is forgiven.
    db.execute("WITH RECURSIVE lineage(plan) AS (SELECT ?1 UNION ALL SELECT a.previous FROM storage_applied_items a JOIN lineage l ON l.plan=a.plan WHERE a.asset_id=?2 AND a.predecessor_valid=1 AND a.previous IS NOT NULL) UPDATE storage_applied_items SET after_state=json_set(after_state,'$.fingerprint',?3) WHERE asset_id=?2 AND plan IN (SELECT plan FROM lineage) AND json_extract(after_state,'$.fingerprint') IS NULL",params![head,asset,hash])?;
    db.execute(
        "INSERT INTO storage_hydration_transitions VALUES(?1,?2,?3,?4)",
        params![asset, head, before, json(&current)?],
    )?;
    Ok(())
}

/// Pins the already selected database object and writer admission registry.
/// Creating a handle performs no filesystem work on the application actor.
#[derive(Clone)]
pub struct RelinkWorkerHandle {
    root: PathBuf,
    session: std::sync::Arc<crate::catalog_session::CatalogSessionAuthority>,
    role: crate::catalog_session::SqlRole,
    writers: std::sync::Arc<crate::catalog_writer::Writers>,
}
impl Catalog {
    pub fn relink_worker_handle(&self) -> Result<RelinkWorkerHandle> {
        self.sql_worker_handle(crate::catalog_session::SqlRole::Relink)
    }
    pub(crate) fn sql_worker_handle(
        &self,
        role: crate::catalog_session::SqlRole,
    ) -> Result<RelinkWorkerHandle> {
        ensure!(
            matches!(
                role,
                crate::catalog_session::SqlRole::Relink | crate::catalog_session::SqlRole::Export
            ),
            "invalid catalog worker role"
        );
        Ok(RelinkWorkerHandle {
            root: self.root.clone(),
            session: self.session.clone(),
            role,
            writers: self.writers.clone(),
        })
    }
}
impl RelinkWorkerHandle {
    pub fn open(self) -> Result<Catalog> {
        self.open_with(|_| Ok(()))
    }
    fn open_with(self, mut boundary: impl FnMut(bool) -> Result<()>) -> Result<Catalog> {
        if let Some(pool) = self.session.pool() {
            let db = pool.lease(crate::catalog_session::role_index(self.role))?;
            return Ok(Catalog {
                db,
                root: self.root,
                writers: self.writers,
                session: self.session,
            });
        }
        let file = self.session.legacy_file()?.clone();
        let path = self.root.join("catalog.sqlite3");
        let expected = object_key(&file)?;
        let before = open_regular(&path)?;
        ensure!(
            object_key(&before)? == expected,
            "selected catalog database was replaced"
        );
        boundary(false)?;
        let db = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        boundary(true)?;
        let after = open_regular(&path)?;
        ensure!(
            object_key(&after)? == expected,
            "selected catalog database changed while opening relink worker"
        );
        verify_database_object(&db, &file)?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let application: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        ensure!(
            version == crate::CURRENT_SCHEMA_VERSION && application == 0x50484341,
            "relink worker requires the selected current catalog; migration is not allowed"
        );
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        crate::configure_catalog_connection(&db)?;
        verify_database_object(&db, &file)?;
        ensure!(
            object_key(&open_regular(&path)?)? == expected,
            "selected catalog changed during worker setup"
        );
        Ok(Catalog {
            db: db.into(),
            root: self.root,
            writers: self.writers,
            session: self.session,
        })
    }
}

/// Scoped SQLite cancellation, borrowing both the connection and flag. The
/// callback cannot outlive either; Drop clears it before transaction rollback.
pub(super) struct SqlCancellation<'a> {
    db: &'a Connection,
    _cancel: &'a AtomicBool,
}
unsafe extern "C" fn sql_cancel(context: *mut std::ffi::c_void) -> i32 {
    // Installed only by SqlCancellation, whose borrow pins this AtomicBool.
    i32::from(unsafe { &*context.cast::<AtomicBool>() }.load(Ordering::Acquire))
}
impl<'a> SqlCancellation<'a> {
    pub(super) fn new(db: &'a Connection, cancel: &'a AtomicBool) -> Self {
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                db.handle(),
                1000,
                Some(sql_cancel),
                (cancel as *const AtomicBool).cast_mut().cast(),
            );
        }
        Self {
            db,
            _cancel: cancel,
        }
    }
}
impl Drop for SqlCancellation<'_> {
    fn drop(&mut self) {
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                self.db.handle(),
                0,
                None,
                std::ptr::null_mut(),
            );
        }
    }
}

#[cfg(test)]
mod tests;
