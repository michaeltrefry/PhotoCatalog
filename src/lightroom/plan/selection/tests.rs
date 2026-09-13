use super::*;
use crate::{
    catalog_migration::importer::{KeywordOverlap, OverlapPolicy},
    lightroom::migration_source::tests::Fixture,
};

struct Case {
    fixture: Fixture,
    request: SelectionRequest,
}
impl Case {
    fn new() -> Self {
        let fixture = Fixture::new();
        let db = Connection::open(&fixture.path).unwrap();
        db.execute(
            "UPDATE captures SET path=?",
            [serde_json::to_string(&NativePath::from_path(
                &fixture.path.with_file_name("never-open-capture"),
            ))
            .unwrap()],
        )
        .unwrap();
        db.execute("DELETE FROM family_choices", []).unwrap();
        drop(db);
        let mut plan = Plan::open(fixture.path.parent().unwrap()).unwrap();
        let a = &fixture.seal.selected[0].revision;
        let b = &fixture.seal.excluded_revisions[0];
        plan.assign_family(a, "kept", "synthetic explicit group")
            .unwrap();
        plan.assign_family(b, "omitted", "synthetic omitted whole family")
            .unwrap();
        let report = plan.families().unwrap();
        for family in &report.families {
            plan.choose(
                &family.id,
                &family.members[0].revision_id,
                &family.evidence_digest,
                "synthetic explicit choice",
            )
            .unwrap();
        }
        let report = plan.families().unwrap();
        let families = report
            .families
            .iter()
            .map(|f| {
                if f.id == "explicit:kept" {
                    FamilyDecision::Select {
                        family: f.id.clone(),
                        revision: a.clone(),
                        expected_evidence_digest: f.evidence_digest.clone(),
                    }
                } else {
                    FamilyDecision::Exclude {
                        family: f.id.clone(),
                        expected_evidence_digest: f.evidence_digest.clone(),
                    }
                }
            })
            .collect();
        drop(plan);
        let request = SelectionRequest {
            inspection: NativePath::from_path(&fixture.path),
            families,
        };
        Self { fixture, request }
    }
    fn review(&self) -> SelectionReview {
        SelectionReview::open(
            self.request.clone(),
            SelectionLimits::default(),
            flag(),
            |_| {},
        )
        .unwrap()
    }
    fn output(&self, tag: &str) -> NativePath {
        NativePath::from_path(&self.fixture.path.parent().unwrap().join(tag))
    }
    fn edit(&self, sql: &str) {
        Connection::open(&self.fixture.path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }
}
fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
fn document(review: &SelectionReview, scope: ApprovalScope) -> Vec<u8> {
    let doc = ApprovalDocument {
        protocol: 1,
        review_token: review.summary().token.clone(),
        scope,
        destination: NativePath::from_path(
            &review
                .guard
                .path
                .with_file_name("destination-never-opened.sqlite3"),
        ),
        policy: Policy {
            import_source: "synthetic source".into(),
            overlap: OverlapPolicy::RequireDecision,
            keyword_overlap: KeywordOverlap::RequireDecision,
            artifacts: vec![],
            supplements: vec![],
        },
        supplements: vec![],
        authorization: "Explicit synthetic TEST fixture authority, never a real migration".into(),
    };
    let mut bytes = b" \n".to_vec();
    bytes.extend(serde_json::to_vec_pretty(&doc).unwrap());
    bytes.extend(b"\n \t");
    bytes
}
fn seal(review: &mut SelectionReview, bytes: &[u8], output: NativePath) -> Result<SealedSelection> {
    review.seal(
        &review.summary.token.clone(),
        &digest(bytes),
        bytes,
        output,
        flag(),
        |_| {},
    )
}
fn hash(path: &Path) -> String {
    digest(&fs::read(path).unwrap())
}

#[test]
fn exact_approval_roster_scope_and_immutable_snapshot_roundtrip() {
    let case = Case::new();
    let before = hash(&case.fixture.path);
    let mut review = case.review();
    assert_eq!((review.summary.selected, review.summary.excluded), (1, 1));
    let page = review.page(ReviewCollection::Captures, 0, 1).unwrap();
    assert_eq!(page.next, Some(1));
    assert_eq!(
        review.page(ReviewCollection::Captures, 1, 1).unwrap().next,
        None
    );
    for (tag, scope) in [
        ("test-seal", ApprovalScope::SelectedMigrationTest),
        ("canonical-synthetic-seal", ApprovalScope::SelectedMigration),
    ] {
        let bytes = document(&review, scope);
        let sealed = seal(&mut review, &bytes, case.output(tag)).unwrap();
        assert_eq!(sealed.approval.scope, scope);
        assert_eq!(sealed.approval_bytes, bytes);
        assert_eq!(
            fs::read(sealed.approval_path.to_path().unwrap()).unwrap(),
            bytes
        );
        assert_eq!(sealed.seal.approval.document_blake3, digest(&bytes));
        assert_eq!(sealed.seal.approval.scope, scope.wire());
        assert_eq!(
            sealed.seal.excluded_revisions,
            case.fixture.seal.excluded_revisions
        );
        assert!(MigrationSource::open(sealed.seal.clone(), ReadLimits::default()).is_ok());
        let frozen = sealed.seal.database.to_path().unwrap();
        assert_ne!(frozen, case.fixture.path);
        let db = Connection::open_with_flags(&frozen, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM family_choices", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM captures", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert!(!sealed.approval.destination.to_path().unwrap().exists());
    }
    assert_eq!(hash(&case.fixture.path), before);
    let next = case.review();
    assert_ne!(
        next.summary.token, review.summary.token,
        "restart requires a fresh review"
    );
}

#[test]
fn explicit_partition_and_stale_family_choice_are_required() {
    let case = Case::new();
    for variant in 0..4 {
        let mut request = case.request.clone();
        match variant {
            0 => {
                request.families.pop();
            }
            1 => {
                request.families.push(request.families[0].clone());
            }
            2 => {
                for decision in &mut request.families {
                    if let FamilyDecision::Select {
                        expected_evidence_digest,
                        ..
                    } = decision
                    {
                        *expected_evidence_digest = "0".repeat(64);
                    }
                }
            }
            _ => {
                for d in &mut request.families {
                    if let FamilyDecision::Select { revision, .. } = d {
                        *revision = "f".repeat(64);
                    }
                }
            }
        }
        assert!(
            SelectionReview::open(request, SelectionLimits::default(), flag(), |_| {}).is_err(),
            "variant {variant}"
        );
    }
    case.edit("UPDATE captures SET evidence_revision=evidence_revision+1");
    assert!(
        SelectionReview::open(case.request, SelectionLimits::default(), flag(), |_| {}).is_err()
    );
}

#[test]
fn any_live_source_commit_invalidates_review_including_excluded_and_unrelated_rows() {
    for sql in [
        "UPDATE family_choices SET reason='changed reason'",
        "UPDATE captures SET manifest=manifest||' '",
        "UPDATE captures SET evidence_revision=evidence_revision+1",
        "UPDATE captures SET stage='changed' WHERE revision IN (SELECT revision FROM family_assignments WHERE family='omitted')",
        "DELETE FROM family_choices WHERE family='explicit:omitted'",
        "UPDATE rows SET cells_json='[]'",
    ] {
        let case = Case::new();
        let mut review = case.review();
        let bytes = document(&review, ApprovalScope::SelectedMigrationTest);
        case.edit(sql);
        let output = case.output("stale");
        assert!(seal(&mut review, &bytes, output.clone()).is_err(), "{sql}");
        assert!(!output.to_path().unwrap().exists());
    }
}

#[test]
fn approval_digest_rejects_scope_destination_policy_authorization_and_whitespace_tamper() {
    let case = Case::new();
    let mut review = case.review();
    let original = document(&review, ApprovalScope::SelectedMigrationTest);
    for variant in 0..5 {
        let mut parsed: serde_json::Value = serde_json::from_slice(&original).unwrap();
        match variant {
            0 => parsed["scope"] = "selected_migration".into(),
            1 => {
                parsed["destination"] =
                    serde_json::to_value(case.output("wrong-destination")).unwrap()
            }
            2 => parsed["policy"]["import_source"] = "other".into(),
            3 => parsed["authorization"] = "other approval".into(),
            _ => {}
        }
        let tampered = serde_json::to_vec(&parsed).unwrap();
        let output = case.output(&format!("tamper-{variant}"));
        assert!(
            review
                .seal(
                    &review.summary.token.clone(),
                    &digest(&original),
                    &tampered,
                    output.clone(),
                    flag(),
                    |_| {}
                )
                .is_err()
        );
        assert!(!output.to_path().unwrap().exists());
    }
}

#[test]
fn cancel_before_output_and_during_partial_backup_retains_only_diagnostics() {
    let case = Case::new();
    case.edit("CREATE TABLE synthetic_padding AS SELECT zeroblob(4194304) AS bytes;");
    let mut review = case.review();
    let bytes = document(&review, ApprovalScope::SelectedMigrationTest);
    let cancel = flag();
    cancel.store(true, Ordering::Relaxed);
    let output = case.output("before");
    assert!(
        review
            .seal(
                &review.summary.token.clone(),
                &digest(&bytes),
                &bytes,
                output.clone(),
                cancel,
                |_| {}
            )
            .is_err()
    );
    assert!(!output.to_path().unwrap().exists());
    let output = case.output("partial");
    let cancel = flag();
    let stop = cancel.clone();
    let result = review.seal(
        &review.summary.token.clone(),
        &digest(&bytes),
        &bytes,
        output.clone(),
        cancel,
        move |p| {
            if p.phase == "copying_snapshot" {
                assert!(p.completed < p.total.unwrap());
                stop.store(true, Ordering::Relaxed);
            }
        },
    );
    assert!(result.is_err());
    let output = output.to_path().unwrap();
    assert!(output.join("pending.json").exists());
    assert!(output.join("failure.json").exists());
    assert!(output.join("inspection.sqlite3").exists());
    assert_eq!(fs::read(output.join("approval.json")).unwrap(), bytes);
    assert!(!output.join("input-seal.json").exists());
    assert!(seal(&mut review, &bytes, NativePath::from_path(&output)).is_err());
}

#[test]
fn wal_snapshot_consistency_pre_cas_rejection_and_commit_wins() {
    let case = Case::new();
    let writer = Connection::open(&case.fixture.path).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    let mut review = case.review();
    let bytes = document(&review, ApprovalScope::SelectedMigrationTest);
    let output = case.output("stale-during-copy");
    let mut changed = false;
    let result = review.seal(
        &review.summary.token.clone(),
        &digest(&bytes),
        &bytes,
        output.clone(),
        flag(),
        |p| {
            if p.phase == "copying_snapshot" && !changed {
                writer
                    .execute("UPDATE rows SET cells_json='[]'", [])
                    .unwrap();
                changed = true;
            }
        },
    );
    assert!(result.is_err());
    assert!(changed);
    assert!(!output.to_path().unwrap().join("input-seal.json").exists());
    // Restore bytes through an explicit new commit before a fresh review.
    writer
        .execute("UPDATE rows SET cells_json='[1]'", [])
        .unwrap();
    let mut review = case.review();
    let bytes = document(&review, ApprovalScope::SelectedMigrationTest);
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE rows SET cells_json='[2]';")
        .unwrap();
    let cancel = flag();
    let stop = cancel.clone();
    let sealed = review
        .seal(
            &review.summary.token.clone(),
            &digest(&bytes),
            &bytes,
            case.output("consistent"),
            cancel,
            |p| {
                if p.phase == "sealed" {
                    writer.execute_batch("COMMIT;").unwrap();
                    stop.store(true, Ordering::Relaxed);
                }
            },
        )
        .unwrap();
    assert!(stop.load(Ordering::Relaxed));
    assert!(MigrationSource::open(sealed.seal.clone(), ReadLimits::default()).is_ok());
    let db = Connection::open_with_flags(
        sealed.seal.database.to_path().unwrap(),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        db.query_row("SELECT cells_json FROM rows LIMIT 1", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "[1]"
    );
    assert_eq!(
        writer
            .query_row("SELECT cells_json FROM rows LIMIT 1", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "[2]"
    );
    assert!(seal(&mut review, &bytes, case.output("old-review-again")).is_err());
}

#[test]
fn old_schema_limits_native_paths_and_sql_cancel_fail_without_source_mutation() {
    let case = Case::new();
    case.edit("PRAGMA user_version=2;");
    let before = hash(&case.fixture.path);
    assert!(
        SelectionReview::open(
            case.request.clone(),
            SelectionLimits::default(),
            flag(),
            |_| {}
        )
        .is_err()
    );
    assert_eq!(hash(&case.fixture.path), before);
    case.edit("PRAGMA user_version=3;");
    let mut request = case.request.clone();
    request.inspection = NativePath::UnixBytes(vec![b'a'; 32769]);
    assert!(SelectionReview::open(request, SelectionLimits::default(), flag(), |_| {}).is_err());
    let limits = SelectionLimits {
        snapshot_bytes: 1,
        ..SelectionLimits::default()
    };
    assert!(SelectionReview::open(case.request.clone(), limits, flag(), |_| {}).is_err());
    let mut request = case.request.clone();
    request.inspection = NativePath::UnixBytes(vec![0]);
    assert!(SelectionReview::open(request, SelectionLimits::default(), flag(), |_| {}).is_err());
    let cancel = flag();
    let stop = cancel.clone();
    assert!(
        SelectionReview::open(
            case.request.clone(),
            SelectionLimits::default(),
            cancel,
            move |_| stop.store(true, Ordering::Relaxed)
        )
        .is_err()
    );
    case.edit("UPDATE family_choices SET reason=replace(hex(zeroblob(4096)),'0','é');");
    let limits = SelectionLimits {
        row_bytes: 10 * 1024,
        ..SelectionLimits::default()
    };
    assert!(
        SelectionReview::open(case.request.clone(), limits, flag(), |_| {}).is_err(),
        "UTF-8 byte admission must reject before JSON/row allocation"
    );
}

#[test]
fn generated_input_seal_rejects_snapshot_tamper_and_reader_cancel() {
    let case = Case::new();
    let mut review = case.review();
    let bytes = document(&review, ApprovalScope::SelectedMigrationTest);
    let sealed = seal(&mut review, &bytes, case.output("tamper-snapshot")).unwrap();
    let cancel = flag();
    cancel.store(true, Ordering::Relaxed);
    assert!(
        MigrationSource::open_cancellable(sealed.seal.clone(), ReadLimits::default(), cancel)
            .is_err()
    );
    Connection::open(sealed.seal.database.to_path().unwrap())
        .unwrap()
        .execute("UPDATE family_choices SET reason='tampered'", [])
        .unwrap();
    assert!(MigrationSource::open(sealed.seal, ReadLimits::default()).is_err());
}

#[test]
fn closed_wal_header_creates_no_companions_and_never_switches_to_live_wal() {
    let case = Case::new();
    assert_eq!(
        companions(&case.fixture.path).unwrap(),
        InspectionReadMode::ClosedMain
    );
    let mut review = case.review();
    assert_eq!(review.summary.read_mode, InspectionReadMode::ClosedMain);
    assert_eq!(
        companions(&case.fixture.path).unwrap(),
        InspectionReadMode::ClosedMain
    );
    let bytes = document(&review, ApprovalScope::SelectedMigrationTest);
    let writer = Connection::open(&case.fixture.path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) SELECT revision,'new-source','new-table','[]','[]' FROM captures LIMIT 1;").unwrap();
    assert_eq!(
        companions(&case.fixture.path).unwrap(),
        InspectionReadMode::LiveWal
    );
    assert!(seal(&mut review, &bytes, case.output("late-wal")).is_err());
    writer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(writer);
    assert!(seal(&mut review, &bytes, case.output("checkpointed-change")).is_err());
}

#[test]
fn live_companion_replacement_invalidates_without_claiming_shm_cache_immutability() {
    let case = Case::new();
    let writer = Connection::open(&case.fixture.path).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; UPDATE rows SET cells_json=cells_json;")
        .unwrap();
    let review = case.review();
    assert_eq!(review.summary.read_mode, InspectionReadMode::LiveWal);
    let mut shm = case.fixture.path.as_os_str().to_os_string();
    shm.push("-shm");
    let shm = PathBuf::from(shm);
    let saved = shm.with_extension("held-shm");
    match fs::rename(&shm, &saved) {
        Ok(()) => {
            fs::copy(&saved, &shm).unwrap();
            let failure = review.current(&review.summary.token).unwrap_err();
            assert!(format!("{failure:#}").contains("companion object changed"));
            fs::remove_file(&shm).unwrap();
            fs::rename(&saved, &shm).unwrap();
        }
        Err(error) => {
            // Native Windows sharing may prevent replacement in the first
            // place; that is a separate enforced outcome, not a guard hit.
            #[cfg(not(windows))]
            panic!("unexpected rename failure: {error}");
            #[cfg(windows)]
            {
                // ERROR_SHARING_VIOLATION (32) is Uncategorized in current Rust;
                // check the native refusal, not its unstable broad category.
                assert!(matches!(error.raw_os_error(), Some(5 | 32)), "{error:?}");
                review.current(&review.summary.token).unwrap();
            }
        }
    }
}

#[test]
fn sqlite_progress_hook_honors_atomic_cancel_during_vm_execution() {
    let db = Connection::open_in_memory().unwrap();
    let cancel = flag();
    let budget = SqlBudget::new(&db, SelectionLimits::default(), cancel.clone());
    cancel.store(true, Ordering::Relaxed);
    let error=db.query_row("WITH RECURSIVE many(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM many WHERE n<100000000) SELECT sum(n) FROM many",[],|r|r.get::<_,i64>(0)).unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::OperationInterrupted)
    );
    assert!(budget.check().is_err());
    drop(budget);
    assert_eq!(
        db.query_row("SELECT 7", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
}
