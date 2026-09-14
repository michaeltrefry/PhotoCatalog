//! Exact selected evidence through the existing review connection. This never
//! opens retained captures, original photos, or a second inspection descriptor.
use super::*;
use rusqlite::params;

pub const PREPARATION_CHUNK_BYTES: usize = 64 * 1024;
pub const PREPARATION_PAGE_ROWS: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum PreparationDocument {
    Manifest { revision: String },
    OriginalEvidence { revision: String, source_id: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparationChunk {
    pub review_token: String,
    pub capture: CaptureEvidence,
    pub document: PreparationDocument,
    pub state: String,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub next: Option<u64>,
}
impl SelectionReview {
    /// This is an explicit source-dependent read, unlike cached review pages.
    /// Both source CAS checks must pass before the chunk is returned. The caller
    /// binds every chunk and derived artifact/supplement preparation to this
    /// review token; a later Seal still performs its own final source CAS.
    pub fn preparation_chunk(
        &self,
        expected: &str,
        document: PreparationDocument,
        offset: u64,
        limit: usize,
        cancel: Arc<AtomicBool>,
    ) -> Result<PreparationChunk> {
        ensure!(
            (1..=PREPARATION_CHUNK_BYTES).contains(&limit),
            "selection preparation chunk byte admission"
        );
        let sql = SqlBudget::new(&self.plan.db, self.summary.limits, cancel);
        sql.check()?;
        self.current(expected)?;
        let revision = match &document {
            PreparationDocument::Manifest { revision }
            | PreparationDocument::OriginalEvidence { revision, .. } => revision,
        };
        let capture = self
            .evidence
            .captures
            .iter()
            .find(|c| c.selected && &c.revision == revision)
            .context("preparation requires a currently selected capture")?;
        // Source IDs are generated identifiers; this bound precedes SQLite bind
        // copies. Historical native path units live inside opaque evidence.
        let (total, state, bytes): (i64, Option<String>, Vec<u8>) = match &document {
            PreparationDocument::Manifest { .. } => self.plan.db.query_row(
                "SELECT length(CAST(manifest AS BLOB)), CASE WHEN length(CAST(stage AS BLOB))<=256 THEN stage END, substr(CAST(manifest AS BLOB),?2,?3) FROM captures WHERE revision=?1",
                params![revision, i64::try_from(offset)?.checked_add(1).context("preparation chunk offset overflow")?, i64::try_from(limit)?],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?,
            PreparationDocument::OriginalEvidence { source_id, .. } => {
                ensure!(!source_id.is_empty() && source_id.len() <= 4096 && !source_id.contains('\0'), "preparation source identifier bounds");
                self.plan.db.query_row(
                    "SELECT length(CAST(evidence AS BLOB)), CASE WHEN length(CAST(state AS BLOB))<=256 THEN state END, substr(CAST(evidence AS BLOB),?3,?4) FROM paths WHERE revision=?1 AND source_id=?2 AND evidence IS NOT NULL",
                    params![revision, source_id, i64::try_from(offset)?.checked_add(1).context("preparation chunk offset overflow")?, i64::try_from(limit)?],
                    |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?
            }
        };
        let total = u64::try_from(total)?;
        ensure!(
            offset <= total,
            "selection preparation offset exceeds evidence"
        );
        let state = state.context("selection preparation state exceeds byte admission")?;
        let next = offset
            .checked_add(bytes.len() as u64)
            .context("preparation offset overflow")?;
        ensure!(
            next <= total && (next == total || !bytes.is_empty()),
            "selection preparation chunk made no progress"
        );
        sql.check()?;
        self.current(expected)?;
        Ok(PreparationChunk {
            review_token: expected.into(),
            capture: capture.clone(),
            document,
            state,
            offset,
            bytes,
            total_bytes: total,
            next: (next < total).then_some(next),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparationSource {
    pub sequence: i64,
    pub source_id: String,
    pub state: String,
    pub evidence_bytes: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparationSources {
    pub review_token: String,
    pub capture: CaptureEvidence,
    pub sources: Vec<PreparationSource>,
    pub next: Option<i64>,
}
impl SelectionReview {
    /// Indexed selected-source roster for guided supplemental proof selection.
    /// Evidence itself is fetched with preparation_chunk; no source is opened.
    pub fn preparation_sources(
        &self,
        expected: &str,
        revision: &str,
        after: i64,
        limit: usize,
        cancel: Arc<AtomicBool>,
    ) -> Result<PreparationSources> {
        ensure!(
            after >= 0 && (1..=PREPARATION_PAGE_ROWS).contains(&limit),
            "selection preparation source page bounds"
        );
        let sql = SqlBudget::new(&self.plan.db, self.summary.limits, cancel);
        sql.check()?;
        self.current(expected)?;
        let capture = self
            .evidence
            .captures
            .iter()
            .find(|c| c.selected && c.revision == revision)
            .context("preparation requires a currently selected capture")?;
        let mut statement = self.plan.db.prepare("SELECT sequence,CASE WHEN length(CAST(source_id AS BLOB))<=4096 THEN source_id END,CASE WHEN length(CAST(state AS BLOB))<=256 THEN state END,length(CAST(evidence AS BLOB)) FROM paths WHERE revision=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?;
        let mut query = statement.query(params![revision, after, i64::try_from(limit + 1)?])?;
        let maximum = self.summary.limits.page_bytes;
        let mut used = bounded_json(capture, maximum)?.len() + expected.len() + 256;
        let mut sources: Vec<PreparationSource> = Vec::new();
        let mut next = None;
        while let Some(row) = query.next()? {
            sql.check()?;
            if sources.len() == limit {
                next = sources.last().map(|s| s.sequence);
                break;
            }
            let value = PreparationSource {
                sequence: row.get(0)?,
                source_id: row
                    .get::<_, Option<String>>(1)?
                    .context("preparation source ID exceeds byte admission")?,
                state: row
                    .get::<_, Option<String>>(2)?
                    .context("preparation source state exceeds byte admission")?,
                evidence_bytes: row
                    .get::<_, Option<i64>>(3)?
                    .map(u64::try_from)
                    .transpose()?,
            };
            let bytes = bounded_json(&value, maximum)?.len() + 1;
            if used + bytes > maximum {
                ensure!(
                    !sources.is_empty(),
                    "selection preparation source exceeds page byte admission"
                );
                next = sources.last().map(|s| s.sequence);
                break;
            }
            used += bytes;
            sources.push(value);
        }
        drop(query);
        drop(statement);
        sql.check()?;
        self.current(expected)?;
        Ok(PreparationSources {
            review_token: expected.into(),
            capture: capture.clone(),
            sources,
            next,
        })
    }
}
