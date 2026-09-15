use super::*;

fn approval(bytes: &[u8], token: &str, limits: SelectionLimits) -> Result<ApprovalDocument> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MANIFEST_BYTES && bytes.len() <= limits.review_bytes,
        "selection approval byte admission exceeded"
    );
    let value: ApprovalDocument =
        serde_json::from_slice(bytes).context("selection approval document")?;
    ensure!(
        value.protocol == 1 && value.review_token == token,
        "approval protocol or exact review token differs"
    );
    ensure!(
        !value.authorization.trim().is_empty() && value.authorization.len() <= 4096,
        "explicit bounded authorization text required"
    );
    local(&value.destination, limits.native_path_units)?;
    ensure!(
        !value.policy.import_source.is_empty() && value.policy.import_source.len() <= 4096,
        "approval import source bound"
    );
    ensure!(
        value.policy.artifacts.len() <= APPROVAL_ROSTER_LIMIT
            && value.policy.supplements.len() <= APPROVAL_ROSTER_LIMIT
            && value.supplements.len() <= APPROVAL_ROSTER_LIMIT,
        "approval artifact/supplement roster bound"
    );
    for artifact in &value.policy.artifacts {
        native(&artifact.mapping.root, limits.native_path_units)?;
        native(&artifact.mapping.relative, limits.native_path_units)?;
    }
    Ok(value)
}
fn write_exact(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    Ok(())
}
struct Backup(*mut rusqlite::ffi::sqlite3_backup);
impl Backup {
    fn finish(mut self) -> Result<()> {
        let pointer = self.0;
        self.0 = std::ptr::null_mut();
        let code = unsafe { rusqlite::ffi::sqlite3_backup_finish(pointer) };
        ensure!(
            code == rusqlite::ffi::SQLITE_OK,
            "selection SQLite backup finish failed ({code})"
        );
        Ok(())
    }
}
impl Drop for Backup {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                rusqlite::ffi::sqlite3_backup_finish(self.0);
            }
        }
    }
}

pub(crate) struct ManagedSealPreparation {
    pub(crate) approval: ApprovalDocument,
    pub(crate) approval_bytes: Vec<u8>,
    pub(crate) review_bytes: Vec<u8>,
    pub(crate) output: NativePath,
    pub(crate) snapshot_bytes: u64,
}

fn close_managed_destination(slot: &mut Option<Connection>) -> Result<()> {
    let Some(connection) = slot.take() else {
        return Ok(());
    };
    match connection.close() {
        Ok(()) => Ok(()),
        Err((connection, error)) => {
            *slot = Some(connection);
            Err(error.into())
        }
    }
}

impl SelectionReview {
    pub(crate) fn close_managed_destination(&mut self) -> Result<()> {
        close_managed_destination(&mut self.managed_destination)
    }
    pub(crate) fn prepare_managed_seal(
        &self,
        expected_review_token: &str,
        expected_approval_blake3: &str,
        approval_json: &[u8],
        output: NativePath,
        cancel: Arc<AtomicBool>,
    ) -> Result<ManagedSealPreparation> {
        let limits = self.summary.limits;
        let until = Instant::now() + Duration::from_millis(limits.deadline_ms);
        check(&cancel, until)?;
        self.current(expected_review_token)?;
        ensure!(
            controlled_digest(approval_json, &cancel, until)? == expected_approval_blake3,
            "exact approval bytes changed after approval"
        );
        let approval = approval(approval_json, expected_review_token, limits)?;
        local(&output, limits.native_path_units)?;
        let selected_revisions: BTreeSet<_> = self
            .evidence
            .captures
            .iter()
            .filter(|capture| capture.selected)
            .map(|capture| capture.revision.as_str())
            .collect();
        for artifact in &approval.policy.artifacts {
            ensure!(
                selected_revisions.contains(artifact.capture_revision.as_str()),
                "approval artifact is outside selected roster"
            );
        }
        for supplement in &approval.policy.supplements {
            ensure!(
                selected_revisions.contains(supplement.capture_revision.as_str()),
                "approval supplement policy is outside selected roster"
            );
        }
        for supplement in &approval.supplements {
            ensure!(
                selected_revisions.contains(supplement.revision.as_str()),
                "approval supplement pin is outside selected roster"
            );
        }
        let snapshot_bytes = database_bytes(&self.plan.db, limits.snapshot_bytes)?;
        let review_bytes = bounded_json(&self.evidence, limits.review_bytes)?;
        Ok(ManagedSealPreparation {
            approval,
            approval_bytes: approval_json.to_vec(),
            review_bytes,
            output,
            snapshot_bytes,
        })
    }

    pub(crate) fn backup_managed(
        &mut self,
        expected_review_token: &str,
        database: &NativePath,
        expected_physical: &crate::lightroom_migration_worker::identity::FileKey,
        cancel: Arc<AtomicBool>,
        mut progress: impl FnMut(SelectionProgress),
    ) -> Result<()> {
        let limits = self.summary.limits;
        let until = Instant::now() + Duration::from_millis(limits.deadline_ms);
        check(&cancel, until)?;
        self.current(expected_review_token)?;
        ensure!(
            self.managed_identity.as_ref() != Some(&super::super::physical(expected_physical)),
            "seal destination aliases inspection source"
        );
        let path = local(database, limits.native_path_units)?;
        let budget = SqlBudget::new(&self.plan.db, limits, cancel.clone());
        ensure!(
            self.managed_destination.is_none(),
            "seal destination still retained"
        );
        // On failure, retain the source snapshot as well as the destination.
        // The W owner poisons the operation and closes both before releasing F.
        self.plan.db.execute_batch("BEGIN DEFERRED")?;
        database_bytes(&self.plan.db, limits.snapshot_bytes)?;
        let _: i64 = self
            .plan
            .db
            .query_row("SELECT count(*) FROM captures", [], |row| row.get(0))?;
        self.managed_destination = Some(Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?);
        let target = self.managed_destination.as_ref().unwrap();
        crate::catalog_storage::verify_database_identity(
            target,
            &super::super::physical(expected_physical),
        )?;
        target.busy_timeout(Duration::ZERO)?;
        let backup = unsafe {
            rusqlite::ffi::sqlite3_backup_init(
                target.handle(),
                c"main".as_ptr(),
                self.plan.db.handle(),
                c"main".as_ptr(),
            )
        };
        ensure!(
            !backup.is_null(),
            "selection SQLite backup initialization failed"
        );
        let backup = Backup(backup);
        loop {
            check(&cancel, until)?;
            let code = unsafe { rusqlite::ffi::sqlite3_backup_step(backup.0, 64) };
            let total = unsafe { rusqlite::ffi::sqlite3_backup_pagecount(backup.0) };
            let remaining = unsafe { rusqlite::ffi::sqlite3_backup_remaining(backup.0) };
            ensure!(
                total >= 0 && remaining >= 0,
                "selection SQLite backup progress invalid"
            );
            progress(SelectionProgress {
                phase: "copying_snapshot".into(),
                completed: (total - remaining) as u64,
                total: Some(total as u64),
            });
            if code == rusqlite::ffi::SQLITE_DONE {
                break;
            }
            ensure!(
                code == rusqlite::ffi::SQLITE_OK,
                "selection SQLite backup interrupted or busy ({code})"
            );
        }
        backup.finish()?;
        crate::catalog_storage::verify_database_identity(
            target,
            &super::super::physical(expected_physical),
        )?;
        target.pragma_update(None, "journal_mode", "DELETE")?;
        close_managed_destination(&mut self.managed_destination)?;
        budget.check()?;
        drop(budget);
        self.current(expected_review_token)?;
        self.plan.db.execute_batch("COMMIT")?;
        Ok(())
    }

    pub(crate) fn managed_input_seal(
        &self,
        preparation: &ManagedSealPreparation,
        database: NativePath,
        identity: crate::lightroom::source::Revision,
        blake3: String,
    ) -> Result<InputSeal> {
        let families: BTreeMap<_, _> = self
            .evidence
            .report
            .families
            .iter()
            .map(|family| (family.id.as_str(), family.evidence_digest.as_str()))
            .collect();
        let selected = self
            .evidence
            .captures
            .iter()
            .filter(|capture| capture.selected)
            .map(|capture| SelectedCapture {
                revision: capture.revision.clone(),
                family: capture.family.clone(),
                family_evidence_digest: families[capture.family.as_str()].to_owned(),
                manifest_blake3: capture.manifest_blake3.clone(),
                evidence_revision: capture.evidence_revision,
            })
            .collect();
        let excluded_revisions = self
            .evidence
            .captures
            .iter()
            .filter(|capture| !capture.selected)
            .map(|capture| capture.revision.clone())
            .collect();
        let mut seal = InputSeal {
            protocol: 1,
            database,
            identity,
            blake3,
            approval: SelectionApproval {
                document_blake3: digest(&preparation.approval_bytes),
                scope: preparation.approval.scope.wire().into(),
                roster_blake3: String::new(),
            },
            selected,
            excluded_revisions,
            supplements: preparation.approval.supplements.clone(),
        };
        seal.approval.roster_blake3 = seal.roster_blake3()?;
        Ok(seal)
    }

    pub(crate) fn current_managed_seal(&self, expected_review_token: &str) -> Result<()> {
        self.current(expected_review_token)
    }

    /// Produces new immutable authority; never opens the migration destination,
    /// original paths or capture artifact paths retained in the approval.
    ///
    /// The last source CAS is the selection point, immediately before atomic
    /// create-new publication. Later source commits describe newer inspection
    /// evidence, not a revocation of this reviewed immutable snapshot. Once the
    /// seal is published, completion wins over later cancellation.
    /// Files are synced before publication. The containing directory entry is
    /// not explicitly fsynced; this is not a power-loss directory durability claim.
    pub fn seal(
        &mut self,
        expected_review_token: &str,
        expected_approval_blake3: &str,
        approval_json: &[u8],
        output_directory: NativePath,
        cancel: Arc<AtomicBool>,
        mut progress: impl FnMut(SelectionProgress),
    ) -> Result<SealedSelection> {
        let limits = self.summary.limits;
        let until = Instant::now() + Duration::from_millis(limits.deadline_ms);
        check(&cancel, until)?;
        self.current(expected_review_token)?;
        ensure!(
            approval_json.len() <= MANIFEST_BYTES && approval_json.len() <= limits.review_bytes,
            "selection approval byte admission exceeded"
        );
        ensure!(
            controlled_digest(approval_json, &cancel, until)? == expected_approval_blake3,
            "exact approval bytes changed after approval"
        );
        let approval = approval(approval_json, expected_review_token, limits)?;
        let output = local(&output_directory, limits.native_path_units)?;
        let parent = output.parent().context("selection output parent absent")?;
        crate::lightroom::source::reject_links(parent)?;
        let selected_revisions: BTreeSet<_> = self
            .evidence
            .captures
            .iter()
            .filter(|c| c.selected)
            .map(|c| c.revision.as_str())
            .collect();
        for artifact in &approval.policy.artifacts {
            ensure!(
                selected_revisions.contains(artifact.capture_revision.as_str()),
                "approval artifact is outside selected roster"
            );
        }
        for supplement in &approval.policy.supplements {
            ensure!(
                selected_revisions.contains(supplement.capture_revision.as_str()),
                "approval supplement policy is outside selected roster"
            );
        }
        for supplement in &approval.supplements {
            ensure!(
                selected_revisions.contains(supplement.revision.as_str()),
                "approval supplement pin is outside selected roster"
            );
        }
        let size = database_bytes(&self.plan.db, limits.snapshot_bytes)?;
        check(&cancel, until)?;
        // Existing outputs, including interrupted outputs, are never replaced.
        fs::create_dir(&output).context("selection output must be a new directory")?;
        let output = fs::canonicalize(output)?;
        let database = output.join("inspection.sqlite3");
        let seal_path = output.join("input-seal.json");
        let approval_path = output.join("approval.json");
        let mut last = SelectionProgress {
            phase: "creating_snapshot".into(),
            completed: 0,
            total: Some(size),
        };
        let result = (|| -> Result<SealedSelection> {
            write_new_json(
                &output.join("pending.json"),
                &serde_json::json!({"protocol":1,"review":self.summary,"approval_blake3":digest(approval_json),"state":"pending","admission":"Only the atomic final input-seal.json is completed authority; pending artifacts are diagnostic custody"}),
            )?;
            write_exact(&approval_path, approval_json)?;
            let review_bytes = bounded_json(&self.evidence, limits.review_bytes)?;
            write_exact(&output.join("review.json"), &review_bytes)?;
            progress(last.clone());
            check(&cancel, until)?;
            self.current(expected_review_token)?;
            let budget = SqlBudget::new(&self.plan.db, limits, cancel.clone());
            let transaction = self.plan.db.unchecked_transaction()?;
            // Pin the read snapshot before backup starts, including WAL pages.
            database_bytes(&self.plan.db, limits.snapshot_bytes)?;
            let _: i64 = self
                .plan
                .db
                .query_row("SELECT count(*) FROM captures", [], |r| r.get(0))?;
            write_exact(&database, &[])?;
            let target = Connection::open_with_flags(
                &database,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            target.busy_timeout(Duration::ZERO)?;
            let backup = unsafe {
                rusqlite::ffi::sqlite3_backup_init(
                    target.handle(),
                    c"main".as_ptr(),
                    self.plan.db.handle(),
                    c"main".as_ptr(),
                )
            };
            ensure!(
                !backup.is_null(),
                "selection SQLite backup initialization failed"
            );
            let backup = Backup(backup);
            loop {
                check(&cancel, until)?;
                let code = unsafe { rusqlite::ffi::sqlite3_backup_step(backup.0, 64) };
                let total = unsafe { rusqlite::ffi::sqlite3_backup_pagecount(backup.0) };
                let remaining = unsafe { rusqlite::ffi::sqlite3_backup_remaining(backup.0) };
                ensure!(
                    total >= 0 && remaining >= 0,
                    "selection SQLite backup progress invalid"
                );
                last = SelectionProgress {
                    phase: "copying_snapshot".into(),
                    completed: (total - remaining) as u64,
                    total: Some(total as u64),
                };
                progress(last.clone());
                check(&cancel, until)?;
                if code == rusqlite::ffi::SQLITE_DONE {
                    break;
                }
                ensure!(
                    code == rusqlite::ffi::SQLITE_OK,
                    "selection SQLite backup interrupted or busy ({code})"
                );
            }
            drop(backup);
            // Source remains read-only. Only our separate owned copy changes its
            // journal mode, and its SQLite handle closes before sealed hashing.
            target.pragma_update(None, "journal_mode", "DELETE")?;
            drop(target);
            transaction.commit()?;
            budget.check()?;
            drop(budget);
            self.current(expected_review_token)?;
            fs::OpenOptions::new()
                .write(true)
                .open(&database)?
                .sync_all()?;
            let mut source = Source::open(&database, limits.snapshot_bytes)?;
            let identity = source.before.clone();
            source.file.seek(SeekFrom::Start(0))?;
            let mut hash = blake3::Hasher::new();
            let mut left = identity.bytes;
            let mut chunk = [0; 128 * 1024];
            while left > 0 {
                check(&cancel, until)?;
                let n = left.min(chunk.len() as u64) as usize;
                source.file.read_exact(&mut chunk[..n])?;
                hash.update(&chunk[..n]);
                left -= n as u64;
                last = SelectionProgress {
                    phase: "hashing_snapshot".into(),
                    completed: identity.bytes - left,
                    total: Some(identity.bytes),
                };
                progress(last.clone());
            }
            source.verify()?;
            let blake3 = hash.finalize().to_hex().to_string();
            drop(source);
            let families: BTreeMap<_, _> = self
                .evidence
                .report
                .families
                .iter()
                .map(|f| (f.id.as_str(), f.evidence_digest.as_str()))
                .collect();
            let selected = self
                .evidence
                .captures
                .iter()
                .filter(|c| c.selected)
                .map(|c| SelectedCapture {
                    revision: c.revision.clone(),
                    family: c.family.clone(),
                    family_evidence_digest: families[c.family.as_str()].to_owned(),
                    manifest_blake3: c.manifest_blake3.clone(),
                    evidence_revision: c.evidence_revision,
                })
                .collect();
            let excluded_revisions = self
                .evidence
                .captures
                .iter()
                .filter(|c| !c.selected)
                .map(|c| c.revision.clone())
                .collect();
            let mut seal = InputSeal {
                protocol: 1,
                database: NativePath::from_path(&database),
                identity,
                blake3,
                approval: SelectionApproval {
                    document_blake3: digest(approval_json),
                    scope: approval.scope.wire().into(),
                    roster_blake3: String::new(),
                },
                selected,
                excluded_revisions,
                supplements: approval.supplements.clone(),
            };
            seal.approval.roster_blake3 = seal.roster_blake3()?;
            check(&cancel, until)?;
            last = SelectionProgress {
                phase: "validating_seal".into(),
                completed: 0,
                total: None,
            };
            progress(last.clone());
            let remaining = until
                .saturating_duration_since(Instant::now())
                .as_millis()
                .max(1) as u64;
            let read_limits = ReadLimits {
                open_deadline_ms: remaining,
                deadline_ms: remaining.min(120_000),
                vm_steps: limits.vm_steps.min(1_000_000_000),
                ..ReadLimits::default()
            };
            drop(MigrationSource::open_cancellable(
                seal.clone(),
                read_limits,
                cancel.clone(),
            )?);
            check(&cancel, until)?;
            // Sync a private staging artifact first. Hard-link publication is
            // atomic and fails if the final name already exists; no replace.
            let staged = output.join("input-seal.pending.json");
            write_new_json(&staged, &seal)?;
            check(&cancel, until)?;
            // This is the operation's selection point, not a writer lock or a
            // promise to stay current after the immutable snapshot is sealed.
            self.current(expected_review_token)?;
            fs::hard_link(&staged, &seal_path)
                .context("atomic create-new selection seal publication")?;
            progress(SelectionProgress {
                phase: "sealed".into(),
                completed: 1,
                total: Some(1),
            });
            Ok(SealedSelection {
                seal,
                approval: approval.clone(),
                approval_bytes: approval_json.to_vec(),
                directory: NativePath::from_path(&output),
                seal_path: NativePath::from_path(&seal_path),
                approval_path: NativePath::from_path(&approval_path),
            })
        })();
        if let Err(error) = &result {
            let message = format!("{error:#}");
            let bounded = message.chars().take(4096).collect::<String>();
            let _ = write_new_json(
                &output.join("failure.json"),
                &serde_json::json!({"state":"failed_or_canceled","review_token":expected_review_token,"progress":last,"error":bounded}),
            );
        }
        result
    }
}

#[cfg(test)]
mod managed_destination_tests {
    use super::*;

    #[test]
    fn busy_destination_close_retains_exact_connection_until_statement_finalized() -> Result<()> {
        let mut destination = Some(Connection::open_in_memory()?);
        let handle = unsafe { destination.as_ref().unwrap().handle() };
        let mut statement = std::ptr::null_mut();
        let code = unsafe {
            rusqlite::ffi::sqlite3_prepare_v2(
                handle,
                c"SELECT 1".as_ptr(),
                -1,
                &mut statement,
                std::ptr::null_mut(),
            )
        };
        ensure!(code == rusqlite::ffi::SQLITE_OK, "prepare held statement");
        let failed = close_managed_destination(&mut destination);
        let retained = destination
            .as_ref()
            .map(|connection| unsafe { connection.handle() })
            == Some(handle);
        let finalized = unsafe { rusqlite::ffi::sqlite3_finalize(statement) };
        ensure!(failed.is_err(), "busy close unexpectedly succeeded");
        ensure!(retained, "failed close discarded destination ownership");
        ensure!(
            finalized == rusqlite::ffi::SQLITE_OK,
            "finalize held statement"
        );
        close_managed_destination(&mut destination)?;
        ensure!(
            destination.is_none(),
            "successful close retained destination"
        );
        Ok(())
    }
}
