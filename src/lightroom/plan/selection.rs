//! Explicit, immutable migration authority built from an owned read-only review.
//!
//! Run review and sealing on an owned worker. A review is intentionally not
//! restartable: its connection-local change counter is valid only for this live
//! handle. Completed approval/snapshot/seal artifacts are restart-readable.
//!
//! The desktop coordinator must serialize inspection SQLite operations in one
//! owned worker, excluding other same-process writers/readers while review,
//! sealing or source handles operate, or use an isolated worker process. On
//! POSIX, closing another descriptor of the same database inode can release
//! process-scoped SQLite locks. This API does not provide generic cross-thread
//! lock safety. Live external WAL writers remain subject to the documented CAS.
use super::{FamilyReport, PLAN_SCHEMA_VERSION, Plan};
use crate::lightroom::{
    MANIFEST_BYTES, PAGE_BYTES, bounded_json, digest,
    migration_source::{
        InputSeal, MigrationSource, ReadLimits, SelectedCapture, SelectionApproval, SupplementPin,
    },
    source::Source,
    write_new_json,
};
use crate::{catalog_migration::importer::Policy, storage_volume::NativePath};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
mod snapshot;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionLimits {
    pub review_bytes: usize,
    pub row_bytes: usize,
    pub page_bytes: usize,
    pub native_path_units: usize,
    /// Admission budget, not a product catalog-size ceiling.
    pub snapshot_bytes: u64,
    pub vm_steps: u64,
    pub deadline_ms: u64,
}
impl Default for SelectionLimits {
    fn default() -> Self {
        Self {
            review_bytes: 64 * 1024 * 1024,
            row_bytes: MANIFEST_BYTES,
            page_bytes: PAGE_BYTES,
            native_path_units: 32768,
            snapshot_bytes: 8 * 1024 * 1024 * 1024,
            vm_steps: 1_000_000_000,
            deadline_ms: 600_000,
        }
    }
}
impl SelectionLimits {
    fn validate(self) -> Result<()> {
        ensure!(
            (1024..=256 * 1024 * 1024).contains(&self.review_bytes),
            "selection review byte limit"
        );
        ensure!(
            (1024..=MANIFEST_BYTES).contains(&self.row_bytes)
                && self.row_bytes <= self.review_bytes,
            "selection row byte limit"
        );
        ensure!(
            (1024..=PAGE_BYTES).contains(&self.page_bytes),
            "selection page byte limit"
        );
        ensure!(
            (1..=1024 * 1024).contains(&self.native_path_units),
            "selection native-path limit"
        );
        ensure!(
            self.snapshot_bytes > 0 && self.snapshot_bytes <= i64::MAX as u64,
            "selection snapshot byte limit"
        );
        ensure!(
            (1000..=100_000_000_000).contains(&self.vm_steps)
                && (1..=3_600_000).contains(&self.deadline_ms),
            "selection execution limits"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum FamilyDecision {
    Select {
        family: String,
        revision: String,
        expected_evidence_digest: String,
    },
    Exclude {
        family: String,
        expected_evidence_digest: String,
    },
}
impl FamilyDecision {
    fn family(&self) -> &str {
        match self {
            Self::Select { family, .. } | Self::Exclude { family, .. } => family,
        }
    }
    fn evidence(&self) -> &str {
        match self {
            Self::Select {
                expected_evidence_digest,
                ..
            }
            | Self::Exclude {
                expected_evidence_digest,
                ..
            } => expected_evidence_digest,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionRequest {
    pub inspection: NativePath,
    pub families: Vec<FamilyDecision>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    SelectedMigration,
    SelectedMigrationTest,
}
impl ApprovalScope {
    fn wire(self) -> &'static str {
        match self {
            Self::SelectedMigration => "selected_migration",
            Self::SelectedMigrationTest => "selected_migration_test",
        }
    }
}
/// Exact input JSON bytes are retained; this parsed descriptor is returned so
/// the importer coordinator can compare its actual destination and policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDocument {
    pub protocol: u32,
    pub review_token: String,
    pub scope: ApprovalScope,
    pub destination: NativePath,
    pub policy: Policy,
    pub supplements: Vec<SupplementPin>,
    pub authorization: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureEvidence {
    pub revision: String,
    pub family: String,
    pub selected: bool,
    pub manifest_blake3: String,
    pub evidence_revision: i64,
    pub stage: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub protocol: u32,
    pub token: String,
    pub inspection: NativePath,
    pub limits: SelectionLimits,
    pub read_mode: InspectionReadMode,
    pub families: usize,
    pub selected: usize,
    pub excluded: usize,
    pub inventory_complete: bool,
    pub uninspected_candidates: usize,
    pub conflicts: i64,
    pub path_collisions: i64,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InspectionReadMode {
    /// No companions may appear. Descriptor identity/size/change metadata is
    /// fenced; this does not detect hostile restoration of identical metadata.
    ClosedMain,
    /// Existing WAL and SHM, genuine read-only main/WAL and data_version CAS.
    /// SQLite may update its derived SHM cache. Pre/post identity checks reject
    /// observed replacement; they do not atomically prevent concurrent cleanup
    /// and SHM recreation between checks. Originals are never opened.
    LiveWal,
}
#[derive(Clone, Copy, Debug)]
pub enum ReviewCollection {
    Families,
    Captures,
    UninspectedCandidates,
    ConflictSample,
    PathCollisionSample,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewPage {
    pub token: String,
    pub rows: Vec<serde_json::Value>,
    pub next: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelectionProgress {
    pub phase: String,
    pub completed: u64,
    pub total: Option<u64>,
}
#[derive(Debug)]
pub struct SealedSelection {
    pub seal: InputSeal,
    pub approval: ApprovalDocument,
    pub approval_bytes: Vec<u8>,
    pub directory: NativePath,
    pub seal_path: NativePath,
    pub approval_path: NativePath,
}
#[derive(Serialize)]
struct ReviewEvidence {
    request: SelectionRequest,
    report: FamilyReport,
    captures: Vec<CaptureEvidence>,
    choices: Vec<(String, String, String, String)>,
    inventories: Vec<String>,
}
pub struct SelectionReview {
    // Close SQLite before the source descriptor. No writable Plan escapes.
    plan: Plan,
    guard: Source,
    version: i64,
    companion_objects: Vec<String>,
    summary: ReviewSummary,
    evidence: ReviewEvidence,
}

struct Budget {
    cancel: Arc<AtomicBool>,
    until: Instant,
    left: u64,
}
struct SqlBudget<'a> {
    db: &'a Connection,
    state: Box<Budget>,
}
unsafe extern "C" fn progress_hook(context: *mut std::ffi::c_void) -> i32 {
    let state = unsafe { &mut *context.cast::<Budget>() };
    if state.cancel.load(Ordering::Relaxed) || state.left < 1000 || Instant::now() >= state.until {
        return 1;
    }
    state.left -= 1000;
    0
}
impl<'a> SqlBudget<'a> {
    fn new(db: &'a Connection, limits: SelectionLimits, cancel: Arc<AtomicBool>) -> Self {
        let mut state = Box::new(Budget {
            cancel,
            until: Instant::now() + Duration::from_millis(limits.deadline_ms),
            left: limits.vm_steps,
        });
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                db.handle(),
                1000,
                Some(progress_hook),
                (&mut *state as *mut Budget).cast(),
            );
        }
        Self { db, state }
    }
    fn check(&self) -> Result<()> {
        check(&self.state.cancel, self.state.until)
    }
}
impl Drop for SqlBudget<'_> {
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
fn check(cancel: &AtomicBool, until: Instant) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "selection canceled; no completed seal published"
    );
    ensure!(Instant::now() < until, "selection deadline exceeded");
    Ok(())
}
fn controlled_digest(bytes: &[u8], cancel: &AtomicBool, until: Instant) -> Result<String> {
    let mut hash = blake3::Hasher::new();
    for chunk in bytes.chunks(128 * 1024) {
        check(cancel, until)?;
        hash.update(chunk);
    }
    check(cancel, until)?;
    Ok(hash.finalize().to_hex().to_string())
}
fn native(path: &NativePath, limit: usize) -> Result<()> {
    let (length, nul) = match path {
        NativePath::UnixBytes(v) => (v.len(), v.contains(&0)),
        NativePath::WindowsWide(v) => (v.len(), v.contains(&0)),
    };
    ensure!(
        length > 0 && length <= limit && !nul,
        "selection native path exceeds limits or contains NUL"
    );
    Ok(())
}
fn local(path: &NativePath, limit: usize) -> Result<PathBuf> {
    native(path, limit)?;
    let path = path.to_path()?;
    ensure!(path.is_absolute(), "selection path must be absolute");
    Ok(path)
}
fn version(db: &Connection) -> Result<i64> {
    Ok(db.query_row("PRAGMA data_version", [], |r| r.get(0))?)
}
fn companions(path: &Path) -> Result<InspectionReadMode> {
    let companion = |suffix: &str| {
        let mut p = path.as_os_str().to_os_string();
        p.push(suffix);
        PathBuf::from(p)
    };
    let mut present = BTreeSet::new();
    for suffix in ["-wal", "-shm", "-journal"] {
        let p = companion(suffix);
        match fs::symlink_metadata(&p) {
            Ok(_) => {
                crate::lightroom::source::reject_links(&p)?;
                present.insert(suffix);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if present.is_empty() {
        return Ok(InspectionReadMode::ClosedMain);
    }
    ensure!(
        present == BTreeSet::from(["-wal", "-shm"]),
        "live review requires both existing WAL and SHM with no journal; close its writer before review"
    );
    Ok(InspectionReadMode::LiveWal)
}
fn companion_objects(path: &Path, mode: InspectionReadMode) -> Result<Vec<String>> {
    if mode == InspectionReadMode::ClosedMain {
        return Ok(vec![]);
    }
    let mut objects = vec![];
    for suffix in ["-wal", "-shm"] {
        let mut companion = path.as_os_str().to_os_string();
        companion.push(suffix);
        let companion = PathBuf::from(companion);
        crate::lightroom::source::reject_links(&companion)?;
        #[cfg(unix)]
        {
            // Do not open/close another Unix SHM descriptor: POSIX locks are
            // process-scoped and that close could release SQLite's live locks.
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::symlink_metadata(&companion)?;
            ensure!(metadata.is_file(), "inspection companion is not regular");
            objects.push(format!("{}:{}", metadata.dev(), metadata.ino()));
        }
        #[cfg(windows)]
        {
            // Windows locks are handle-scoped; the existing platform file key
            // excludes cache byte/timestamp changes made by normal SQLite.
            objects.push(Source::open(&companion, u64::MAX)?.before.object.clone());
        }
    }
    Ok(objects)
}
fn database_bytes(db: &Connection, limit: u64) -> Result<u64> {
    let pages = u64::try_from(db.query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))?)?;
    let size = u64::try_from(db.query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))?)?;
    let bytes = pages
        .checked_mul(size)
        .context("inspection size overflow")?;
    ensure!(
        bytes <= limit,
        "inspection exceeds snapshot_bytes admission; increase the explicit budget"
    );
    Ok(bytes)
}
/// Pre-admit UTF-8 byte lengths before families() materializes retained JSON.
/// Entity evidence is streamed by the existing engine, so only its largest row
/// is bounded; aggregate capture/inventory/assignment storage is bounded too.
fn preadmit(db: &Connection, limits: SelectionLimits) -> Result<()> {
    let mut aggregate = 0u64;
    for (table, columns, collect) in [
        (
            "captures",
            &[
                "revision",
                "path",
                "manifest",
                "schema_version",
                "provider",
                "stage",
            ][..],
            true,
        ),
        (
            "family_assignments",
            &["revision", "family", "reason"][..],
            true,
        ),
        (
            "family_choices",
            &["family", "revision", "evidence_digest", "reason"][..],
            true,
        ),
        ("inventories", &["digest", "json"][..], true),
        (
            "entities",
            &["source_id", "table_name", "global_key", "fields_json"][..],
            false,
        ),
        (
            "paths",
            &["source_id", "inspection_path", "original"][..],
            false,
        ),
    ] {
        let kind: String = db.query_row(
            "SELECT type FROM sqlite_schema WHERE name=?",
            [table],
            |r| r.get(0),
        )?;
        ensure!(
            kind == "table",
            "selection evidence must be an ordinary table"
        );
        let expression = columns
            .iter()
            .map(|c| format!("coalesce(length(CAST({c} AS BLOB)),0)"))
            .collect::<Vec<_>>()
            .join("+");
        let sql = format!(
            "SELECT count(*),coalesce(max({expression}),0),coalesce(sum({expression}),0) FROM {table}"
        );
        let (count, max, sum): (i64, i64, i64) =
            db.query_row(&sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let (count, max, sum) = (
            u64::try_from(count)?,
            u64::try_from(max)?,
            u64::try_from(sum)?,
        );
        ensure!(
            max <= limits.row_bytes as u64,
            "selection {table} evidence row exceeds byte admission"
        );
        if collect {
            aggregate = aggregate
                .checked_add(sum)
                .and_then(|n| n.checked_add(count.checked_mul(64)?))
                .context("selection evidence size overflow")?;
            ensure!(
                aggregate <= limits.review_bytes as u64,
                "selection aggregate review byte admission exceeded"
            );
            ensure!(
                count <= 16_384,
                "selection retained roster row admission exceeded"
            );
        }
        if table == "captures" {
            ensure!(
                (1..=256).contains(&count),
                "selection requires 1..256 inspected captures"
            );
        }
    }
    database_bytes(db, limits.snapshot_bytes)?;
    Ok(())
}

impl SelectionReview {
    /// Bind both this review's held source and SQLite's actually opened object
    /// to the workbench's existing descriptor. No incidental same-inode FD is
    /// opened/closed here (important for POSIX process-scoped SQLite locks).
    pub(crate) fn verify_inspection_owner(&self, expected: &Source) -> Result<()> {
        ensure!(
            self.guard.before.object == expected.before.object,
            "selection opened a different inspection object; reopen workbench explicitly"
        );
        crate::catalog_storage::verify_database_object(&self.plan.db, &expected.file)
    }
    pub fn open(
        request: SelectionRequest,
        limits: SelectionLimits,
        cancel: Arc<AtomicBool>,
        mut progress: impl FnMut(SelectionProgress),
    ) -> Result<Self> {
        limits.validate()?;
        let until = Instant::now() + Duration::from_millis(limits.deadline_ms);
        check(&cancel, until)?;
        ensure!(
            !request.families.is_empty() && request.families.len() <= 256,
            "explicit complete family decision roster required"
        );
        bounded_json(&request, limits.review_bytes)?;
        let path = local(&request.inspection, limits.native_path_units)?;
        let guard = Source::open(&path, limits.snapshot_bytes)?;
        let read_mode = companions(&path)?;
        let initial_companions = companion_objects(&path, read_mode)?;
        // immutable is safe only for fenced main-only custody, never live WAL.
        // Genuine RO SQLite may otherwise create companions for a closed file
        // whose header retains WAL mode, which review must not do.
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let db = match read_mode {
            InspectionReadMode::ClosedMain => {
                Connection::open_with_flags(super::uri(&path)?, flags | OpenFlags::SQLITE_OPEN_URI)?
            }
            InspectionReadMode::LiveWal => Connection::open_with_flags(&path, flags)?,
        };
        crate::catalog_storage::verify_database_object(&db, &guard.file)?;
        db.busy_timeout(Duration::ZERO)?;
        db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; PRAGMA mmap_size=0; PRAGMA cache_size=-8192; PRAGMA temp_store=FILE;")?;
        let app: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            app == 0x50434c49 && schema == PLAN_SCHEMA_VERSION,
            "selection requires existing schema3 inspection; old evidence was not migrated"
        );
        let baseline = version(&db)?;
        let plan = Plan {
            db,
            execution: None,
            root: path
                .parent()
                .context("inspection parent absent")?
                .to_path_buf(),
        };
        let budget = SqlBudget::new(&plan.db, limits, cancel.clone());
        let tx = plan.db.unchecked_transaction()?;
        preadmit(&plan.db, limits)?;
        progress(SelectionProgress {
            phase: "reviewing".into(),
            completed: 0,
            total: None,
        });
        check(&cancel, until)?;
        let report = plan.families_in_snapshot()?;
        let decisions: BTreeMap<_, _> = request.families.iter().map(|d| (d.family(), d)).collect();
        ensure!(
            decisions.len() == request.families.len() && decisions.len() == report.families.len(),
            "family decisions must partition every family exactly once"
        );
        let mut captures = Vec::new();
        let mut selected = 0;
        for family in &report.families {
            check(&cancel, until)?;
            let decision = decisions
                .get(family.id.as_str())
                .context("family has no explicit selection or exclusion")?;
            ensure!(
                decision.evidence() == family.evidence_digest,
                "family evidence changed; review again"
            );
            let chosen = match decision {
                FamilyDecision::Select { revision, .. } => {
                    ensure!(
                        family.selected.as_ref() == Some(revision),
                        "selection differs from current explicit family choice"
                    );
                    selected += 1;
                    Some(revision.as_str())
                }
                FamilyDecision::Exclude { .. } => None,
            };
            for member in &family.members {
                native(&member.source, limits.native_path_units)?;
                let manifest: String = plan.db.query_row(
                    "SELECT manifest FROM captures WHERE revision=?",
                    [&member.revision_id],
                    |r| r.get(0),
                )?;
                captures.push(CaptureEvidence {
                    revision: member.revision_id.clone(),
                    family: family.id.clone(),
                    selected: chosen == Some(member.revision_id.as_str()),
                    manifest_blake3: controlled_digest(manifest.as_bytes(), &cancel, until)?,
                    evidence_revision: member.inspection_evidence_revision,
                    stage: member.row_stage.clone(),
                });
            }
        }
        ensure!(
            selected > 0,
            "selection must include at least one explicitly chosen capture"
        );
        captures.sort_by(|a, b| a.revision.cmp(&b.revision));
        let choices = plan
            .db
            .prepare(
                "SELECT family,revision,evidence_digest,reason FROM family_choices ORDER BY family",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let inventories = plan
            .db
            .prepare("SELECT digest FROM inventories ORDER BY digest")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        let evidence = ReviewEvidence {
            request,
            report,
            captures,
            choices,
            inventories,
        };
        let bytes = bounded_json(&evidence, limits.review_bytes)?;
        let token = digest(&bounded_json(
            &(
                uuid::Uuid::new_v4().to_string(),
                controlled_digest(&bytes, &cancel, until)?,
            ),
            1024,
        )?);
        tx.commit()?;
        budget.check()?;
        drop(budget);
        ensure!(
            version(&plan.db)? == baseline,
            "inspection committed changes during review; review again"
        );
        guard.verify()?;
        ensure!(
            companions(&path)? == read_mode,
            "inspection companion mode changed during review"
        );
        ensure!(
            companion_objects(&path, read_mode)? == initial_companions,
            "inspection companion object changed during review"
        );
        let summary = ReviewSummary {
            protocol: 1,
            token,
            inspection: evidence.request.inspection.clone(),
            limits,
            read_mode,
            families: evidence.report.families.len(),
            selected,
            excluded: evidence.captures.len() - selected,
            inventory_complete: evidence.report.inventory_complete,
            uninspected_candidates: evidence.report.uninspected_candidates.len(),
            conflicts: evidence.report.conflict_count,
            path_collisions: evidence.report.possible_path_collision_count,
        };
        Ok(Self {
            plan,
            guard,
            version: baseline,
            companion_objects: initial_companions,
            summary,
            evidence,
        })
    }
    pub fn summary(&self) -> &ReviewSummary {
        &self.summary
    }
    pub fn page(
        &self,
        collection: ReviewCollection,
        after: usize,
        limit: usize,
    ) -> Result<ReviewPage> {
        ensure!((1..=256).contains(&limit), "selection page row limit");
        fn page<T: Serialize>(
            rows: &[T],
            after: usize,
            limit: usize,
            maximum: usize,
        ) -> Result<(Vec<serde_json::Value>, Option<usize>)> {
            ensure!(after <= rows.len(), "selection page cursor out of range");
            let mut values = vec![];
            let mut bytes = 256;
            for row in rows.iter().skip(after).take(limit) {
                let value = bounded_json(row, maximum.saturating_sub(256))?;
                if bytes + value.len() + 1 > maximum {
                    break;
                }
                bytes += value.len() + 1;
                values.push(serde_json::from_slice(&value)?);
            }
            let next = after + values.len();
            Ok((values, (next < rows.len()).then_some(next)))
        }
        let max = self.summary.limits.page_bytes;
        let (rows, next) = match collection {
            ReviewCollection::Families => page(&self.evidence.report.families, after, limit, max)?,
            ReviewCollection::Captures => page(&self.evidence.captures, after, limit, max)?,
            ReviewCollection::UninspectedCandidates => page(
                &self.evidence.report.uninspected_candidates,
                after,
                limit,
                max,
            )?,
            ReviewCollection::ConflictSample => page(
                &self.evidence.report.cross_catalog_conflicts,
                after,
                limit,
                max,
            )?,
            ReviewCollection::PathCollisionSample => page(
                &self.evidence.report.possible_path_collisions,
                after,
                limit,
                max,
            )?,
        };
        Ok(ReviewPage {
            token: self.summary.token.clone(),
            rows,
            next,
        })
    }
    fn current(&self, expected: &str) -> Result<()> {
        ensure!(
            expected == self.summary.token,
            "selection review token differs"
        );
        self.guard
            .verify()
            .context("inspection identity changed; review again")?;
        crate::catalog_storage::verify_database_object(&self.plan.db, &self.guard.file)?;
        ensure!(
            companions(&self.guard.path)? == self.summary.read_mode,
            "inspection companion mode changed; review again"
        );
        ensure!(
            companion_objects(&self.guard.path, self.summary.read_mode)? == self.companion_objects,
            "inspection companion object changed; review again"
        );
        ensure!(
            version(&self.plan.db)? == self.version,
            "inspection committed changes; review again"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
