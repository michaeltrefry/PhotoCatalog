use super::*;
use crate::lightroom::{migration_source::tests::Fixture, plan::Plan};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

fn config(root: &Path, mode: OpenMode) -> Config {
    Config {
        root: NativePath::from_path(root),
        mode,
        capture_executable: NativePath::from_path(&root.with_file_name("synthetic-capture-worker")),
        capture_staging: NativePath::from_path(root.parent().unwrap()),
        limits: Limits::default(),
    }
}
fn terminal(workbench: &Workbench) -> Status {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let status = workbench.status();
        if matches!(
            status.phase,
            Phase::Complete | Phase::Failed | Phase::Canceled | Phase::Closed
        ) {
            return status;
        }
        assert!(Instant::now() < until, "workbench timed out: {status:?}");
        thread::sleep(Duration::from_millis(2));
    }
}
fn value(workbench: &Workbench) -> serde_json::Value {
    let s = terminal(workbench);
    assert_eq!(s.phase, Phase::Complete, "{s:?}");
    let token = s.result_token.as_deref().unwrap();
    let mut offset = U64(0);
    let mut json = String::new();
    loop {
        let page = workbench
            .result(&s.generation, &s.operation, token, offset, U64(97))
            .unwrap();
        assert_eq!(
            (&page.workbench, &page.generation, &page.operation),
            (&s.workbench, &s.generation, &s.operation)
        );
        json.push_str(&page.json_fragment);
        match page.next {
            Some(next) => offset = next,
            None => break,
        }
    }
    serde_json::from_str(&json).unwrap()
}
fn read(w: &Workbench, q: Query) -> serde_json::Value {
    w.read(&w.status().generation, q).unwrap();
    value(w)
}
fn act(w: &Workbench, a: Action) -> serde_json::Value {
    w.start(&w.status().generation, a).unwrap();
    value(w)
}
fn close(w: &mut Workbench) {
    w.request_close();
    let until = Instant::now() + Duration::from_secs(30);
    while !w.poll_closed().unwrap() {
        assert!(
            Instant::now() < until,
            "close failed to drain: {:?}",
            w.status()
        );
        thread::sleep(Duration::from_millis(2));
    }
    assert!(w.status().closed);
}

#[test]
fn held_initialization_has_queue_bypass_cancel_close_and_no_replay() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plan");
    let (release, held) = mpsc::channel();
    let mut w = Workbench::spawn_inner(
        config(&root, OpenMode::Create),
        move || {
            held.recv().unwrap();
        },
        None,
    )
    .unwrap();
    let initial = w.status();
    assert_eq!(initial.phase, Phase::Opening);
    assert!(w.cancel("different operation").is_err());
    assert_eq!(
        w.cancel(&initial.operation).unwrap().phase,
        Phase::CancelRequested
    );
    assert!(w.read(&initial.generation, Query::Families).is_err());
    w.request_close();
    assert!(!w.poll_closed().unwrap());
    assert!(!root.exists());
    release.send(()).unwrap();
    close(&mut w);
    assert!(!root.exists());
    let mut fresh = Workbench::spawn(config(&root, OpenMode::Create)).unwrap();
    value(&fresh);
    assert_ne!(initial.workbench, fresh.status().workbench);
    assert!(fresh.cancel(&initial.operation).is_err());
    close(&mut fresh);
}

#[test]
fn exact_result_tokens_decimal_cursors_and_bounded_cache_replace() {
    let fixture = Fixture::new();
    let mut w = Workbench::spawn(config(
        fixture.path.parent().unwrap(),
        OpenMode::OpenExisting,
    ))
    .unwrap();
    value(&w);
    let old = w.status();
    let revision = fixture.revision().to_owned();
    let rows = read(
        &w,
        Query::Rows {
            revision,
            table: None,
            after: I64(0),
            limit: U64(1),
        },
    );
    assert_eq!(rows["next"], "1");
    assert_eq!(rows["rows"][0]["cells"][0]["type"], "Blob");
    assert!(
        w.result(
            &old.generation,
            &old.operation,
            old.result_token.as_deref().unwrap(),
            U64(0),
            U64(100)
        )
        .is_err()
    );
    let saved = w.status();
    assert!(
        w.result(
            &saved.generation,
            &saved.operation,
            saved.result_token.as_deref().unwrap(),
            U64(0),
            U64((core::PAGE_BYTES + 1) as u64)
        )
        .is_err()
    );
    assert!(w.read("stale", Query::Families).is_err());
    assert_eq!(w.status().operation, saved.operation);
    close(&mut w);
    let mut reopened = Workbench::spawn(config(
        fixture.path.parent().unwrap(),
        OpenMode::OpenExisting,
    ))
    .unwrap();
    let opening = value(&reopened);
    assert_eq!(opening["schema"], 3);
    assert!(opening.get("rows").is_none());
    close(&mut reopened);
}

#[test]
fn all_retained_pages_selection_release_and_immutable_seal() {
    use crate::{
        catalog_migration::importer::{KeywordOverlap, OverlapPolicy, Policy},
        lightroom::migration_source::{InputSeal, MigrationSource, ReadLimits},
    };
    use selection::{
        ApprovalDocument, ApprovalScope, FamilyDecision, SelectionLimits, SelectionRequest,
    };
    let fixture = Fixture::new();
    let root = fixture.path.parent().unwrap();
    // Fix only synthetic custody locator syntax; no artifact at this path exists.
    let db = rusqlite::Connection::open(&fixture.path).unwrap();
    db.execute(
        "UPDATE captures SET path=?",
        [
            serde_json::to_string(&NativePath::from_path(&root.join("never-opened-capture")))
                .unwrap(),
        ],
    )
    .unwrap();
    db.execute("DELETE FROM family_choices", []).unwrap();
    drop(db);
    let mut w = Workbench::spawn(config(root, OpenMode::OpenExisting)).unwrap();
    value(&w);
    let revision = fixture.revision().to_owned();
    let excluded = fixture.seal.excluded_revisions[0].clone();
    for query in [
        Query::Report {
            revision: revision.clone(),
        },
        Query::Paths {
            revision: revision.clone(),
            after: I64(0),
            limit: U64(1),
        },
        Query::Issues {
            revision: revision.clone(),
            after: I64(0),
            limit: U64(1),
        },
        Query::Packets {
            revision: revision.clone(),
            after: I64(0),
            limit: U64(1),
        },
        Query::PacketBytes {
            revision: revision.clone(),
            sequence: I64(1),
            decoded: false,
            offset: I64(0),
            limit: U64(2),
        },
        Query::MetadataConflicts {
            revision: revision.clone(),
            after: I64(0),
            limit: U64(1),
        },
        Query::GlobalIdConflicts {
            left: revision.clone(),
            right: excluded.clone(),
            after_left: String::new(),
            after_right: String::new(),
            limit: U64(1),
        },
        Query::PathCollisions {
            left: revision.clone(),
            right: excluded.clone(),
            after_left: I64(0),
            after_right: I64(0),
            limit: U64(1),
        },
    ] {
        read(&w, query);
    }
    for (r, family) in [(&revision, "kept"), (&excluded, "excluded")] {
        act(
            &w,
            Action::AssignFamily {
                revision: r.clone(),
                family: family.into(),
                reason: "explicit synthetic review".into(),
            },
        );
    }
    let report = read(&w, Query::Families);
    let mut decisions = vec![];
    for family in report["families"].as_array().unwrap() {
        let id = family["id"].as_str().unwrap().to_owned();
        let evidence = family["evidence_digest"].as_str().unwrap().to_owned();
        if id == "explicit:kept" {
            act(
                &w,
                Action::Choose {
                    family: id.clone(),
                    revision: revision.clone(),
                    expected_evidence: evidence.clone(),
                    reason: "TEST selection".into(),
                },
            );
            decisions.push(FamilyDecision::Select {
                family: id,
                revision: revision.clone(),
                expected_evidence_digest: evidence,
            });
        } else {
            decisions.push(FamilyDecision::Exclude {
                family: id,
                expected_evidence_digest: evidence,
            });
        }
    }
    let summary = act(
        &w,
        Action::PrepareSelection {
            request: SelectionRequest {
                inspection: NativePath::from_path(&fixture.path),
                families: decisions,
            },
            limits: SelectionLimits::default(),
        },
    );
    let review_token = summary["token"].as_str().unwrap().to_owned();
    assert_eq!(summary["selected"], 1);
    assert_eq!(summary["excluded"], 1);
    assert!(w.read(&w.status().generation, Query::Families).is_err());
    assert!(
        w.start(
            &w.status().generation,
            Action::AssignFamily {
                revision,
                family: "forbidden".into(),
                reason: "review still owned".into()
            }
        )
        .is_err()
    );
    for collection in [
        ReviewCollection::Families,
        ReviewCollection::Captures,
        ReviewCollection::UninspectedCandidates,
        ReviewCollection::ConflictSample,
        ReviewCollection::PathCollisionSample,
    ] {
        read(
            &w,
            Query::SelectionPage {
                review_token: review_token.clone(),
                collection,
                after: U64(0),
                limit: U64(1),
            },
        );
    }
    let document = ApprovalDocument {
        protocol: 1,
        review_token: review_token.clone(),
        scope: ApprovalScope::SelectedMigrationTest,
        destination: NativePath::from_path(&root.join("never-opened-destination.sqlite3")),
        policy: Policy {
            import_source: "TEST workbench".into(),
            overlap: OverlapPolicy::RequireDecision,
            keyword_overlap: KeywordOverlap::RequireDecision,
            artifacts: vec![],
            supplements: vec![],
        },
        supplements: vec![],
        authorization: "Synthetic TEST authority only".into(),
    };
    let approval_json = format!(
        " \n{}\n\t",
        serde_json::to_string_pretty(&document).unwrap()
    );
    let sealed = act(
        &w,
        Action::Seal {
            review_token,
            approval_blake3: blake3::hash(approval_json.as_bytes()).to_hex().to_string(),
            approval_json: approval_json.clone(),
            output: NativePath::from_path(&root.join("sealed")),
        },
    );
    assert_eq!(sealed["approval_json"], approval_json);
    let seal: InputSeal = serde_json::from_value(sealed["seal"].clone()).unwrap();
    drop(MigrationSource::open(seal, ReadLimits::default()).unwrap());
    assert!(!root.join("never-opened-destination.sqlite3").exists());
    act(&w, Action::ReleaseReview);
    assert!(w.status().review_token.is_none());
    read(&w, Query::Families);
    close(&mut w);
}

#[test]
fn old_schema_and_replaced_database_are_rejected_before_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plan");
    drop(Plan::create(&root).unwrap());
    let path = root.join("inspection.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA user_version=2;")
        .unwrap();
    drop(db);
    let before = fs::read(&path).unwrap();
    let mut old = Workbench::spawn(config(&root, OpenMode::OpenExisting)).unwrap();
    let s = terminal(&old);
    assert!(!s.initialized && s.error.unwrap().contains("schema 2"));
    close(&mut old);
    assert_eq!(before, fs::read(&path).unwrap());
    assert!(!path.with_extension("sqlite3-wal").exists());
    let root2 = temp.path().join("current");
    let mut w = Workbench::spawn(config(&root2, OpenMode::Create)).unwrap();
    value(&w);
    let path2 = root2.join("inspection.sqlite3");
    let detached = root2.join("detached.sqlite3");
    #[cfg(windows)]
    {
        assert!(
            fs::rename(&path2, &detached).is_err(),
            "Windows inspection descriptor must exclude replacement"
        );
        read(&w, Query::Families);
        close(&mut w);
    }
    #[cfg(unix)]
    fs::rename(&path2, &detached).unwrap();
    #[cfg(unix)]
    fs::write(&path2, b"replacement authority must not be touched").unwrap();
    #[cfg(unix)]
    {
        w.read(&w.status().generation, Query::Families).unwrap();
        assert!(terminal(&w).error.unwrap().contains("changed"));
        assert_eq!(
            fs::read(&path2).unwrap(),
            b"replacement authority must not be touched"
        );
        close(&mut w);
    }
}

#[test]
fn unicode_byte_and_discovery_memory_admission_preserve_evidence() {
    let fixture = Fixture::new();
    let mut cfg = config(fixture.path.parent().unwrap(), OpenMode::OpenExisting);
    cfg.limits.row_bytes = 4096;
    let mut w = Workbench::spawn(cfg).unwrap();
    value(&w);
    // An external write changes generation first; it cannot be hidden behind a
    // stale page token. Only inspect stored synthetic SQLite, never originals.
    let db = rusqlite::Connection::open(&fixture.path).unwrap();
    db.execute("UPDATE captures SET provider=?", ["界".repeat(3000)])
        .unwrap();
    drop(db);
    w.read(&w.status().generation, Query::Families).unwrap();
    assert!(terminal(&w).error.unwrap().contains("changed externally"));
    w.read(&w.status().generation, Query::Families).unwrap();
    assert!(terminal(&w).error.is_some());
    close(&mut w);
    let temp = tempfile::tempdir().unwrap();
    for i in 0..5 {
        fs::write(temp.path().join(format!("{i}.lrcat")), b"no database read").unwrap();
    }
    let mut cfg = config(&temp.path().join("plan"), OpenMode::Create);
    cfg.limits.result_bytes = 4096;
    let mut w = Workbench::spawn(cfg).unwrap();
    value(&w);
    w.start(
        &w.status().generation,
        Action::Discover {
            root: NativePath::from_path(&fs::canonicalize(temp.path()).unwrap()),
            limits: core::Limits::default(),
        },
    )
    .unwrap();
    assert!(
        terminal(&w)
            .error
            .unwrap()
            .contains("discovery aggregate byte admission")
    );
    close(&mut w);
}

#[cfg(unix)]
#[test]
fn capture_cancellation_and_close_reap_owned_process_before_closed() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plan");
    let cfg = config(&root, OpenMode::Create);
    let script = cfg.capture_executable.to_path().unwrap();
    fs::write(&script, b"#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let mut w = Workbench::spawn(cfg).unwrap();
    value(&w);
    for close_now in [false, true] {
        let op = w
            .start(
                &w.status().generation,
                Action::Capture {
                    request: core::capture::Request {
                        source: NativePath::from_path(
                            &temp.path().join("not-opened-original.lrcat"),
                        ),
                        output: NativePath::from_path(&temp.path().join("capture")),
                        include_auxiliary: false,
                        closed_application_evidence: None,
                        limits: core::Limits::default(),
                    },
                },
            )
            .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        let pid = loop {
            let s = w.status();
            if let Some(pid) = s.capture_pid {
                break pid;
            }
            assert!(Instant::now() < until && s.phase == Phase::Running, "{s:?}");
            thread::sleep(Duration::from_millis(2));
        };
        let staging = w.status().capture_staging.unwrap().to_path().unwrap();
        assert!(staging.join("request.json").is_file());
        if close_now {
            close(&mut w);
        } else {
            w.cancel(&op).unwrap();
            assert_eq!(terminal(&w).phase, Phase::Canceled);
        }
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert!(staging.join("request.json").is_file());
    }
    assert!(!temp.path().join("not-opened-original.lrcat").exists());
}

#[test]
fn native_path_admission_is_lexical_and_sql_cancel_does_not_poison_connection() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = config(&temp.path().join("never-created"), OpenMode::Create);
    cfg.root = NativePath::UnixBytes(vec![0; 32769]);
    assert!(Workbench::spawn(cfg).is_err());
    assert!(!temp.path().join("never-created").exists());
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE resumable(value); INSERT INTO resumable VALUES(1);")
        .unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let control = Control::new(flag.clone(), 100_000_000, 10_000, 4096, 4096).unwrap();
    let guard = crate::lightroom::control::SqlControl::new(&db, control);
    flag.store(true, Ordering::Release);
    let result = db.query_row("WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<1000000) SELECT sum(n) FROM x", [], |r| r.get::<_, i64>(0));
    assert!(result.is_err());
    drop(guard);
    db.execute("INSERT INTO resumable VALUES(2)", []).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM resumable", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[cfg(unix)]
#[test]
fn review_open_replacement_then_path_restoration_cannot_change_workbench_authority() {
    use crate::lightroom::plan::desktop::InspectionPin;
    use selection::{FamilyDecision, SelectionLimits, SelectionRequest, SelectionReview};
    let fixture = Fixture::new();
    let root = fixture.path.parent().unwrap();
    let db = rusqlite::Connection::open(&fixture.path).unwrap();
    db.execute(
        "UPDATE captures SET path=?",
        [serde_json::to_string(&NativePath::from_path(&root.join("never-opened"))).unwrap()],
    )
    .unwrap();
    db.execute("DELETE FROM family_choices", []).unwrap();
    drop(db);
    let mut plan = Plan::open(root).unwrap();
    for (n, r) in std::iter::once(fixture.revision())
        .chain(fixture.seal.excluded_revisions.iter().map(String::as_str))
        .enumerate()
    {
        plan.assign_family(r, &format!("group-{n}"), "synthetic exact partition")
            .unwrap();
    }
    let report = plan.families().unwrap();
    let families = report
        .families
        .into_iter()
        .map(|f| {
            if f.members
                .iter()
                .any(|m| m.revision_id == fixture.revision())
            {
                plan.choose(
                    &f.id,
                    fixture.revision(),
                    &f.evidence_digest,
                    "explicit synthetic selected family",
                )
                .unwrap();
                FamilyDecision::Select {
                    family: f.id,
                    revision: fixture.revision().to_owned(),
                    expected_evidence_digest: f.evidence_digest,
                }
            } else {
                FamilyDecision::Exclude {
                    family: f.id,
                    expected_evidence_digest: f.evidence_digest,
                }
            }
        })
        .collect();
    drop(plan);
    let pin = InspectionPin::open(root).unwrap();
    let original_bytes = fs::read(&fixture.path).unwrap();
    let held_a = root.join("held-a.sqlite3");
    fs::rename(&fixture.path, &held_a).unwrap();
    fs::write(&fixture.path, &original_bytes).unwrap();
    let review = SelectionReview::open(
        SelectionRequest {
            inspection: NativePath::from_path(&fixture.path),
            families,
        },
        SelectionLimits::default(),
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let held_b = root.join("held-b.sqlite3");
    fs::rename(&fixture.path, &held_b).unwrap();
    fs::rename(&held_a, &fixture.path).unwrap();
    pin.verify().unwrap(); // A path-only check passes after restoration.
    let error = pin.verify_review(&review).unwrap_err();
    assert!(
        error.to_string().contains("different inspection object"),
        "{error:#}"
    );
    drop(review); // Drain the wrong reader before the long-lived owner pin.
    drop(pin);
    assert_eq!(original_bytes, fs::read(&fixture.path).unwrap());
}

#[test]
fn foreign_inventory_and_capture_manifest_remain_opaque_evidence() {
    // Nonlocal units, including a non-Unicode scalar, cannot be opened on this
    // platform. Registration and manifest inspection must still retain them.
    #[cfg(unix)]
    let foreign = NativePath::WindowsWide(
        [
            r"R:\synthetic\catalog-".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
        ]
        .concat(),
    );
    #[cfg(windows)]
    let foreign = NativePath::UnixBytes([b"/synthetic/catalog-".to_vec(), vec![255]].concat());
    assert!(foreign.to_path().is_err());
    let fixture = Fixture::new();
    let db = rusqlite::Connection::open(&fixture.path).unwrap();
    let json: String = db
        .query_row("SELECT manifest FROM captures LIMIT 1", [], |r| r.get(0))
        .unwrap();
    drop(db);
    let mut manifest: core::capture::Manifest = serde_json::from_str(&json).unwrap();
    manifest.request.source = foreign.clone();
    manifest.artifacts[0].source = foreign.clone();
    manifest.revision_id = Some(core::json_digest(&manifest.artifacts).unwrap());
    let temp = tempfile::tempdir().unwrap();
    let custody = fs::canonicalize(temp.path())
        .unwrap()
        .join("opaque-capture");
    fs::create_dir(&custody).unwrap();
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(custody.join("manifest.json"), &manifest_bytes).unwrap();
    let mut w = Workbench::spawn(config(&temp.path().join("plan"), OpenMode::Create)).unwrap();
    value(&w);
    let inventory = core::discovery::Inventory {
        protocol: core::PROTOCOL,
        root: foreign.clone(),
        candidates: vec![core::discovery::Candidate {
            path: foreign.clone(),
            bytes: 42,
            modified_ns: None,
            filename_hint: None,
            version_hint: None,
        }],
        exclusions: vec![foreign.clone()],
        issues: vec![],
        complete: true,
        entries: 1,
    };
    act(&w, Action::RegisterInventory { inventory });
    let families = read(&w, Query::Families);
    assert_eq!(
        families["uninspected_candidates"][0]["path"],
        serde_json::to_value(&foreign).unwrap()
    );
    let inspected = read(
        &w,
        Query::CaptureManifest {
            directory: NativePath::from_path(&custody),
        },
    );
    assert_eq!(
        inspected["request"]["source"],
        serde_json::to_value(&foreign).unwrap()
    );
    assert_eq!(
        manifest_bytes,
        fs::read(custody.join("manifest.json")).unwrap()
    );
    // The separate action which would open a source must reject those same units.
    w.start(
        &w.status().generation,
        Action::Capture {
            request: manifest.request,
        },
    )
    .unwrap();
    assert_eq!(terminal(&w).phase, Phase::Failed);
    assert!(w.status().capture_pid.is_none());
    assert!(w.status().capture_staging.is_none());
    close(&mut w);
}
