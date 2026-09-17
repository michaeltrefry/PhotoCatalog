//! Inspection-only connection ownership for the serialized desktop workbench.
use super::*;
use crate::lightroom::control::{self, Control};

pub(crate) struct InspectionPin {
    source: Source,
    root: PathBuf,
}
impl InspectionPin {
    pub fn open(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let source = Source::open(&root.join("inspection.sqlite3"), u64::MAX)?;
        Ok(Self { source, root })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn verify(&self) -> Result<()> {
        crate::lightroom::source::reject_links(&self.source.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = fs::symlink_metadata(&self.source.path)?;
            ensure!(
                meta.is_file()
                    && format!("{}:{}", meta.dev(), meta.ino()) == self.source.before.object,
                "inspection path object changed; reopen explicitly"
            );
        }
        #[cfg(windows)]
        {
            let current = Source::open(&self.source.path, u64::MAX)?;
            ensure!(
                current.before.object == self.source.before.object,
                "inspection path object changed; reopen explicitly"
            );
        }
        Ok(())
    }
    pub fn open_plan(&self, execution: Control) -> Result<Plan> {
        self.verify()?;
        // No writable connection (or hot-journal recovery) before old-schema
        // preflight. This main-header check follows the existing CLI discipline;
        // the actual writable connection is independently checked below.
        let check = snapshot(&self.source.path, &Limits::default())?;
        let budget = super::super::control::SqlControl::new(&check, execution.clone());
        let app: i64 = check.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let version: i64 = check.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(app == 0x50434c49, "not an inspection database");
        require_current_plan(version)?;
        validate_paging_indexes(&check)?;
        drop(budget);
        drop(check);
        self.verify()?;
        // READ_WRITE without CREATE, no migration, and actual opened-object
        // identity checked before any configuration capable of writing.
        let db = Connection::open_with_flags(
            &self.source.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        crate::catalog_storage::verify_database_object(&db, &self.source.file)?;
        let mut plan = Plan {
            db,
            root: self.root.clone(),
            execution: None,
        };
        plan.set_execution(Some(execution));
        let app: i64 = plan
            .db
            .query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let version: i64 = plan.db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(app == 0x50434c49, "not an inspection database");
        require_current_plan(version)?;
        for table in [
            "captures",
            "schema_objects",
            "tables",
            "rows",
            "entities",
            "issues",
            "packets",
            "references_out",
            "metadata_facts",
            "paths",
            "inventories",
            "family_assignments",
            "family_choices",
        ] {
            let count: i64 = plan.db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name=?",
                [table],
                |r| r.get(0),
            )?;
            ensure!(count == 1, "incomplete inspection schema: {table}");
        }
        plan.db
            .prepare("SELECT evidence_revision FROM captures LIMIT 0")?;
        validate_paging_indexes(&plan.db)?;
        self.verify()?;
        crate::catalog_storage::verify_database_object(&plan.db, &self.source.file)?;
        crate::configure_catalog_connection(&plan.db)?;
        Ok(plan)
    }
    pub fn verify_review(&self, review: &selection::SelectionReview) -> Result<()> {
        self.verify()?;
        review.verify_inspection_owner(&self.source)
    }
    pub fn verify_plan(&self, plan: &Plan) -> Result<()> {
        self.verify()?;
        crate::catalog_storage::verify_database_object(&plan.db, &self.source.file)
    }
}
impl Plan {
    pub(crate) fn desktop_choose(
        &mut self,
        family: &str,
        revision: &str,
        expected: &str,
        reason: &str,
        commit_authority: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            !reason.trim().is_empty() && reason.len() <= 4096,
            "explicit selection reason required"
        );
        // Same read snapshot covers preadmission, exact family evidence, and
        // choice publication. External commits cause a failed upgrade rather
        // than attaching this choice to evidence which was never reviewed.
        let tx = self.db.unchecked_transaction()?;
        let report = self.families_in_snapshot()?;
        let value = report
            .families
            .iter()
            .find(|f| f.id == family)
            .context("family absent")?;
        ensure!(
            value.evidence_digest == expected,
            "family evidence changed; inspect again"
        );
        ensure!(
            value.members.iter().any(|m| m.revision_id == revision),
            "selected revision is outside family"
        );
        self.db.execute("INSERT INTO family_choices VALUES(?,?,?,?) ON CONFLICT(family) DO UPDATE SET revision=excluded.revision,evidence_digest=excluded.evidence_digest,reason=excluded.reason", params![family,revision,expected,reason])?;
        commit_authority()?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn desktop_families(&self) -> Result<FamilyReport> {
        let tx = self.db.unchecked_transaction()?;
        let result = self.families_in_snapshot()?;
        tx.commit()?;
        Ok(result)
    }
    pub(crate) fn desktop_report(&self, revision: &str) -> Result<Report> {
        let tx = self.db.unchecked_transaction()?;
        self.admit_report()?;
        let result = self.report(revision)?;
        tx.commit()?;
        Ok(result)
    }
    pub(crate) fn set_execution(&mut self, execution: Option<Control>) {
        control::install(&self.db, None);
        self.execution = execution.map(Box::new);
        // SQLite itself rejects oversized values/rows before Rust decodes or
        // copies retained TEXT/BLOBs. A budget refusal rolls back the current
        // publication batch and never marks a source table as corrupt.
        unsafe {
            rusqlite::ffi::sqlite3_limit(
                self.db.handle(),
                rusqlite::ffi::SQLITE_LIMIT_LENGTH,
                self.execution
                    .as_ref()
                    .map_or(i32::MAX, |c| c.row_bytes as i32),
            );
        }
        control::install(&self.db, self.execution.as_deref());
    }
    pub(crate) fn data_version(&self) -> Result<i64> {
        Ok(self.db.query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }
    /// Used only for deliberate aggregate report/family preparation, never for
    /// page polling. Streaming row queries perform their own row admission.
    pub(crate) fn admit_report(&self) -> Result<()> {
        let Some(control) = self.execution.as_ref() else {
            return Ok(());
        };
        let mut total = 0i64;
        for (table, columns, aggregate) in [
            (
                "captures",
                &[
                    "revision",
                    "manifest",
                    "path",
                    "stage",
                    "provider",
                    "schema_version",
                ][..],
                true,
            ),
            ("inventories", &["digest", "json"][..], true),
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
            ("tables", &["name", "category", "issue"][..], true),
            (
                "entities",
                &["source_id", "table_name", "global_key", "fields_json"][..],
                false,
            ),
            (
                "paths",
                &["source_id", "original", "inspection_path"][..],
                false,
            ),
        ] {
            control.check()?;
            let size = columns
                .iter()
                .map(|c| format!("coalesce(length(CAST({c} AS BLOB)),0)"))
                .collect::<Vec<_>>()
                .join("+");
            let sql =
                format!("SELECT coalesce(max({size}),0),coalesce(sum({size}+1024),0) FROM {table}");
            let (max, sum): (i64, i64) = self
                .db
                .query_row(&sql, [], |r| Ok((r.get(0)?, r.get(1)?)))?;
            ensure!(
                max >= 0 && max as usize <= control.row_bytes,
                "inspection {table} row byte admission exceeded; evidence retained"
            );
            if aggregate {
                total = total
                    .checked_add(sum)
                    .context("inspection report byte overflow")?;
                ensure!(
                    total >= 0 && total as usize <= control.result_bytes,
                    "inspection aggregate report byte admission exceeded; evidence retained"
                );
            }
        }
        Ok(())
    }
}
