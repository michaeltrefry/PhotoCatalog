use super::*;
use crate::application::{self as app, I64};
use std::time::{Duration, Instant};
static SERIAL: Mutex<()> = Mutex::new(());
fn config() -> app::Config {
    app::Config {
        worker_executable: std::env::current_exe().unwrap(),
        cache_root: None,
        original_roots: vec![],
        preview_policy: Default::default(),
        preview_limits: Default::default(),
        limits: Default::default(),
        import_checkpoint: None,
    }
}
fn call(b: &app::Bridge, r: Request) -> Result<Response> {
    let reply = b
        .submit(app::Request::Lightroom {
            request: Box::new(r),
        })?
        .receiver
        .recv_timeout(Duration::from_secs(10))?;
    ensure!(
        serde_json::to_vec(&reply)?.len() <= ENVELOPE,
        "wire envelope overflow"
    );
    match reply {
        app::Reply::Ok {
            value: app::Response::Lightroom(r),
        } => Ok(*r),
        app::Reply::Error { error } => Err(error.into()),
        _ => anyhow::bail!("wrong response"),
    }
}
fn status(b: &app::Bridge) -> Status {
    match call(
        b,
        Request::Status {
            workbench: None,
            attempt: None,
        },
    )
    .unwrap()
    {
        Response::Status(Some(s)) => s,
        _ => panic!(),
    }
}
fn g(s: &Status) -> Guard {
    Guard {
        workbench: s.workbench.clone(),
        generation: s.generation.clone(),
        operation: s.operation.clone(),
    }
}
fn wait(b: &app::Bridge) -> Status {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let s = status(b);
        if matches!(
            s.phase,
            lw::Phase::Complete | lw::Phase::Failed | lw::Phase::Canceled | lw::Phase::Closed
        ) {
            return s;
        }
        assert!(Instant::now() < deadline, "{s:?}");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn raw(b: &app::Bridge) -> String {
    let s = wait(b);
    assert_eq!(s.phase, lw::Phase::Complete, "{s:?}");
    let mut out = String::new();
    let mut offset = U64(0);
    loop {
        let Response::Result(p) = call(
            b,
            Request::Result {
                guard: g(&s),
                token: s.result_token.clone().unwrap(),
                offset,
                limit: U64(u64::MAX),
            },
        )
        .unwrap() else {
            panic!()
        };
        out.push_str(&p.page.json_fragment);
        match p.page.next {
            Some(n) => offset = n,
            None => break,
        }
    }
    out
}
fn value(b: &app::Bridge) -> serde_json::Value {
    serde_json::from_str(&raw(b)).unwrap()
}
fn read(b: &app::Bridge, q: Query) -> serde_json::Value {
    call(
        b,
        Request::Read {
            guard: g(&status(b)),
            query: q,
        },
    )
    .unwrap();
    value(b)
}
fn act(b: &app::Bridge, a: Action) -> serde_json::Value {
    call(
        b,
        Request::Action {
            guard: g(&status(b)),
            action: a,
        },
    )
    .unwrap();
    value(b)
}
fn upload(b: &app::Bridge, purpose: InputPurpose, json: &str) -> String {
    let guard = g(&status(b));
    let Response::Input(Some(u)) = call(
        b,
        Request::InputBegin {
            guard: guard.clone(),
            purpose,
            total_bytes: U64(json.len() as u64),
            expected_blake3: Some(blake3::hash(json.as_bytes()).to_hex().to_string()),
        },
    )
    .unwrap() else {
        panic!()
    };
    let mut offset = 0;
    while offset < json.len() {
        let mut end = (offset + CHUNK).min(json.len());
        while !json.is_char_boundary(end) {
            end -= 1;
        }
        call(
            b,
            Request::InputAppend {
                guard: guard.clone(),
                input: u.input.clone(),
                offset: U64(offset as u64),
                fragment: json[offset..end].into(),
            },
        )
        .unwrap();
        offset = end;
    }
    call(
        b,
        Request::InputFinish {
            guard,
            input: u.input.clone(),
        },
    )
    .unwrap();
    u.input
}
fn open(b: &app::Bridge, root: &std::path::Path, mode: lw::OpenMode) -> Status {
    call(
        b,
        Request::Open {
            attempt: uuid::Uuid::new_v4().to_string(),
            root: NativePath::from_path(root),
            mode,
            capture_staging: NativePath::from_path(root.parent().unwrap()),
            limits: lw::Limits::default().into(),
        },
    )
    .unwrap();
    let s = wait(b);
    assert_eq!(s.phase, lw::Phase::Complete, "{s:?}");
    s
}
#[test]
fn complete_retained_surface_selection_seal_and_catalog_lifetime_are_independent() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use crate::catalog_migration::importer::{KeywordOverlap, OverlapPolicy, Policy};
    use crate::lightroom::{
        migration_source::{InputSeal, MigrationSource, ReadLimits, tests::Fixture},
        selection::{ApprovalDocument, ApprovalScope, FamilyDecision, SelectionRequest},
    };
    let fixture = Fixture::new();
    let root = fixture.path.parent().unwrap();
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
    let b = app::Bridge::spawn(config()).unwrap();
    open(&b, root, lw::OpenMode::OpenExisting);
    let revision = fixture.revision().to_owned();
    let excluded = fixture.seal.excluded_revisions[0].clone();
    for query in [
        Query::Rows {
            revision: revision.clone(),
            table: None,
            after: I64(0),
            limit: U64(1),
        },
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
        read(&b, query);
    }
    for (r, family) in [(&revision, "kept"), (&excluded, "excluded")] {
        act(
            &b,
            Action::AssignFamily {
                revision: r.clone(),
                family: family.into(),
                reason: "explicit synthetic review".into(),
            },
        );
    }
    let report = read(&b, Query::Families {});
    let mut decisions = vec![];
    for family in report["families"].as_array().unwrap() {
        let id = family["id"].as_str().unwrap().to_owned();
        let evidence = family["evidence_digest"].as_str().unwrap().to_owned();
        if id == "explicit:kept" {
            act(
                &b,
                Action::Choose {
                    family: id.clone(),
                    revision: revision.clone(),
                    expected_evidence: evidence.clone(),
                    reason: "TEST only".into(),
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
    let input = upload(
        &b,
        InputPurpose::SelectionRequest,
        &serde_json::to_string(&SelectionRequest {
            inspection: NativePath::from_path(&fixture.path),
            families: decisions,
        })
        .unwrap(),
    );
    let summary = act(
        &b,
        Action::PrepareSelection {
            input,
            limits: crate::lightroom::selection::SelectionLimits::default().into(),
        },
    );
    let token = summary["token"].as_str().unwrap().to_owned();
    assert_eq!(summary["selected"], 1);
    assert_eq!(summary["excluded"], 1);
    let prepared_sources = read(
        &b,
        Query::SelectionSources {
            review_token: token.clone(),
            revision: revision.clone(),
            after: I64(0),
            limit: U64(1),
        },
    );
    assert_eq!(prepared_sources["sources"][0]["source_id"], "file-selected");
    let prepared_evidence = read(
        &b,
        Query::SelectionPreparation {
            review_token: token.clone(),
            document: crate::lightroom::selection::PreparationDocument::OriginalEvidence {
                revision: revision.clone(),
                source_id: "file-selected".into(),
            },
            offset: U64(0),
            limit: U64(65536),
        },
    );
    let exact: Vec<u8> = serde_json::from_value(prepared_evidence["bytes"].clone()).unwrap();
    assert_eq!(exact, b"{\"missing\":true}");
    assert_eq!(prepared_evidence["review_token"], token);

    assert!(
        call(
            &b,
            Request::Read {
                guard: g(&status(&b)),
                query: Query::Families {}
            }
        )
        .is_err()
    );
    for collection in [
        lw::ReviewCollection::Families,
        lw::ReviewCollection::Captures,
        lw::ReviewCollection::UninspectedCandidates,
        lw::ReviewCollection::ConflictSample,
        lw::ReviewCollection::PathCollisionSample,
    ] {
        read(
            &b,
            Query::SelectionPage {
                review_token: token.clone(),
                collection,
                after: U64(0),
                limit: U64(1),
            },
        );
    }
    read(&b, Query::SelectionSummary {});
    let doc = ApprovalDocument {
        protocol: 1,
        review_token: token.clone(),
        scope: ApprovalScope::SelectedMigrationTest,
        destination: NativePath::from_path(&root.join("never-created-destination")),
        policy: Policy {
            import_source: "TEST bridge".into(),
            overlap: OverlapPolicy::RequireDecision,
            keyword_overlap: KeywordOverlap::RequireDecision,
            artifacts: vec![],
            supplements: vec![],
        },
        supplements: vec![],
        authorization: "Synthetic explicit TEST approval".into(),
    };
    let approval = format!(" \n{}\n\t", serde_json::to_string_pretty(&doc).unwrap());
    let input = upload(&b, InputPurpose::Approval, &approval);
    let sealed = act(
        &b,
        Action::Seal {
            review_token: token,
            approval_blake3: blake3::hash(approval.as_bytes()).to_hex().to_string(),
            input,
            output: NativePath::from_path(&root.join("sealed")),
        },
    );
    assert_eq!(sealed["approval_json"], approval);
    let seal: InputSeal = serde_json::from_value(sealed["seal"].clone()).unwrap();
    drop(MigrationSource::open(seal, ReadLimits::default()).unwrap());
    assert!(!root.join("never-created-destination").exists());
    act(&b, Action::ReleaseReview {});
    // Closing an independently selected destination catalog cannot close inspection.
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("destination");
    let reply = b
        .submit(app::Request::Create {
            path: NativePath::from_path(&destination),
        })
        .unwrap()
        .recv();
    let app::Reply::Ok {
        value: app::Response::Status(catalog),
    } = reply
    else {
        panic!("{reply:?}")
    };
    let before = status(&b);
    b.submit(app::Request::Close {
        catalog: catalog.catalog.unwrap(),
    })
    .unwrap()
    .recv();
    assert_eq!(status(&b).workbench, before.workbench);
    assert!(!status(&b).closed);
    read(&b, Query::Families {});
    b.shutdown();
}
#[test]
fn uploads_envelopes_stale_guards_and_foreign_evidence_are_exact() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("inspection");
    let b = app::Bridge::spawn(config()).unwrap();
    let s = open(&b, &root, lw::OpenMode::Create);
    let Response::Options(o) = call(&b, Request::Options {}).unwrap() else {
        panic!()
    };
    assert_eq!(o.envelope_bytes, U64(ENVELOPE as u64));
    assert_eq!(o.selection_preparation_chunk_bytes, U64(64 * 1024));
    assert_eq!(o.selection_preparation_page_rows, U64(256));
    let stale = g(&s);
    read(&b, Query::Families {});
    assert!(
        call(
            &b,
            Request::Read {
                guard: stale,
                query: Query::Families {}
            }
        )
        .is_err()
    );
    #[cfg(unix)]
    let foreign = NativePath::WindowsWide(vec![67, 58, 92, 0xd800]);
    #[cfg(windows)]
    let foreign = NativePath::UnixBytes(vec![47, 255]);
    let inventory = crate::lightroom::discovery::Inventory {
        protocol: crate::lightroom::PROTOCOL,
        root: foreign.clone(),
        candidates: vec![crate::lightroom::discovery::Candidate {
            path: foreign.clone(),
            bytes: 9007199254740993,
            modified_ns: Some(u128::MAX),
            filename_hint: None,
            version_hint: None,
        }],
        exclusions: vec![],
        issues: vec![],
        complete: true,
        entries: 1,
    };
    let exact = serde_json::to_string(&inventory).unwrap();
    let input = upload(&b, InputPurpose::Inventory, &exact);
    act(&b, Action::RegisterInventory { input });
    read(&b, Query::Families {});
    let guard = g(&status(&b));
    let escaped = "\\\"\n😀".repeat(20000);
    assert!(
        b.submit(app::Request::Lightroom {
            request: Box::new(Request::InputAppend {
                guard: guard.clone(),
                input: "none".into(),
                offset: U64(0),
                fragment: escaped
            })
        })
        .is_err()
    );
    let Response::Input(Some(u)) = call(
        &b,
        Request::InputBegin {
            guard: guard.clone(),
            purpose: InputPurpose::Inventory,
            total_bytes: U64(3),
            expected_blake3: Some(blake3::hash(b"abc").to_hex().to_string()),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert!(
        call(
            &b,
            Request::InputAppend {
                guard: guard.clone(),
                input: u.input.clone(),
                offset: U64(1),
                fragment: "abc".into()
            }
        )
        .is_err()
    );
    call(
        &b,
        Request::InputAppend {
            guard: guard.clone(),
            input: u.input.clone(),
            offset: U64(0),
            fragment: "xyz".into(),
        },
    )
    .unwrap();
    assert!(
        call(
            &b,
            Request::InputFinish {
                guard: guard.clone(),
                input: u.input.clone()
            }
        )
        .is_err()
    );
    call(
        &b,
        Request::InputDiscard {
            guard: guard.clone(),
            input: u.input,
        },
    )
    .unwrap();
    let invalid = upload(&b, InputPurpose::Inventory, "{broken");
    call(
        &b,
        Request::Action {
            guard: g(&status(&b)),
            action: Action::RegisterInventory { input: invalid },
        },
    )
    .unwrap();
    assert_eq!(wait(&b).phase, lw::Phase::Failed);
    read(&b, Query::Families {});
    call(
        &b,
        Request::Action {
            guard: g(&status(&b)),
            action: Action::Capture {
                source: foreign,
                output: NativePath::from_path(&temp.path().join("never-created")),
                include_auxiliary: false,
                closed_application_evidence: None,
                limits: crate::lightroom::Limits::default().into(),
            },
        },
    )
    .unwrap();
    let failed = wait(&b);
    assert_eq!(failed.phase, lw::Phase::Failed);
    assert!(failed.capture_pid.is_none());
    assert!(!temp.path().join("never-created").exists());
    b.shutdown();
}
#[test]
fn held_owner_has_bypass_cancel_close_and_foreground_turns_without_replay() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("held-inspection");
    let (release, held) = std::sync::mpsc::channel();
    let w = lw::Workbench::spawn_held(
        lw::Config {
            root: NativePath::from_path(&root),
            mode: lw::OpenMode::Create,
            capture_executable: NativePath::from_path(&std::env::current_exe().unwrap()),
            capture_staging: NativePath::from_path(temp.path()),
            limits: lw::Limits::default(),
        },
        move || {
            held.recv().unwrap();
        },
    )
    .unwrap();
    let b = app::Bridge::spawn(config()).unwrap();
    b.shutdown();
    {
        let mut q = b.0.shared.queue.lock().unwrap();
        q.stopping = false;
    }
    {
        let mut c = b.0.shared.lightroom.lock().unwrap();
        c.attempt = Some("held-attempt".into());
        c.workbench = Some(w.control());
    }
    let mut actor = app::Actor::new(config(), b.0.shared.clone());
    actor.lightroom.owner = Some(w);
    let thread = std::thread::spawn(move || actor.run());
    *b.0.thread.lock().unwrap() = Some(thread);
    let s = status(&b);
    assert_eq!(s.phase, lw::Phase::Opening);
    // Even while initialization is held, a foreground catalog query receives a turn.
    assert!(matches!(
        b.submit(app::Request::Status)
            .unwrap()
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        app::Reply::Ok { .. }
    ));
    let started = Instant::now();
    call(&b, Request::Cancel { guard: g(&s) }).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        call(
            &b,
            Request::Cancel {
                guard: Guard {
                    operation: "stale".into(),
                    ..g(&s)
                }
            }
        )
        .is_err()
    );
    call(
        &b,
        Request::Close {
            workbench: s.workbench.clone(),
        },
    )
    .unwrap();
    assert_eq!(status(&b).phase, lw::Phase::Closing);
    assert!(!status(&b).closed);
    assert!(!root.exists());
    assert!(
        call(
            &b,
            Request::Open {
                attempt: "cannot-overtake".into(),
                root: NativePath::from_path(&root),
                mode: lw::OpenMode::Create,
                capture_staging: NativePath::from_path(temp.path()),
                limits: lw::Limits::default().into()
            }
        )
        .is_err()
    );
    release.send(()).unwrap();
    assert!(wait(&b).closed);
    assert!(!root.exists());
    let new = open(&b, &root, lw::OpenMode::Create);
    assert_ne!(new.workbench, s.workbench);
    assert!(
        call(
            &b,
            Request::Close {
                workbench: s.workbench
            }
        )
        .is_err()
    );
    let guard = g(&new);
    let pending = b
        .submit(app::Request::Lightroom {
            request: Box::new(Request::Read {
                guard: guard.clone(),
                query: Query::Families {},
            }),
        })
        .unwrap();
    drop(pending);
    let until = Instant::now() + Duration::from_secs(5);
    while status(&b).operation == guard.operation {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
    let observed = wait(&b);
    assert_ne!(observed.operation, guard.operation);
    assert!(
        call(
            &b,
            Request::Read {
                guard,
                query: Query::Families {}
            }
        )
        .is_err()
    );
    b.shutdown();
}
#[test]
fn wire_canonical_decimals_unknown_fields_and_request_queue_cancellation() {
    use serde_json::json;
    for number in ["0", "18446744073709551615"] {
        let r:Request=serde_json::from_value(json!({"kind":"Read","guard":{"workbench":"w","generation":"g","operation":"o"},"query":{"kind":"Rows","revision":"r","table":null,"after":"9223372036854775807","limit":number}})).unwrap();
        let encoded = serde_json::to_value(r).unwrap();
        assert_eq!(encoded["query"]["limit"], number);
    }
    for bad in [json!(1), json!("01"), json!("18446744073709551616")] {
        assert!(serde_json::from_value::<Request>(json!({"kind":"Read","guard":{"workbench":"w","generation":"g","operation":"o"},"query":{"kind":"Rows","revision":"r","table":null,"after":"0","limit":bad}})).is_err());
    }
    assert!(serde_json::from_value::<Request>(json!({"kind":"Options","hidden":true})).is_err());
    assert!(
        serde_json::from_value::<Action>(json!({"kind":"ReleaseReview","hidden":true})).is_err()
    );
    for kind in ["Families", "SelectionSummary"] {
        assert!(serde_json::from_value::<Query>(json!({"kind":kind,"hidden":true})).is_err());
    }
    // Stop the normal owner, then install a held actor so a canceled queued Open
    // cannot start filesystem work. Its cancellation is checked at dequeue.
    let b = app::Bridge::spawn(config()).unwrap();
    b.shutdown();
    b.0.shared.queue.lock().unwrap().stopping = false;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("canceled-before-dequeue");
    let p = b
        .submit(app::Request::Lightroom {
            request: Box::new(Request::Open {
                attempt: "queued".into(),
                root: NativePath::from_path(&root),
                mode: lw::OpenMode::Create,
                capture_staging: NativePath::from_path(temp.path()),
                limits: lw::Limits::default().into(),
            }),
        })
        .unwrap();
    p.cancellation().cancel();
    let actor = app::Actor::new(config(), b.0.shared.clone());
    *b.0.thread.lock().unwrap() = Some(std::thread::spawn(move || actor.run()));
    assert!(matches!(
        p.receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
        app::Reply::Error { .. }
    ));
    assert!(!root.exists());
    assert!(matches!(
        call(
            &b,
            Request::Status {
                workbench: None,
                attempt: None
            }
        )
        .unwrap(),
        Response::Status(None)
    ));
    b.shutdown();
}
#[test]
fn large_utf8_authority_crosses_transport_pages_without_truncation() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = crate::lightroom::migration_source::tests::Fixture::new();
    let text = "😀\\\"\n\t".repeat(25000);
    let db = rusqlite::Connection::open(&fixture.path).unwrap();
    db.execute("UPDATE rows SET source_id=? WHERE sequence=(SELECT min(sequence) FROM rows WHERE revision=?)",rusqlite::params![text,fixture.revision()]).unwrap();
    drop(db);
    let b = app::Bridge::spawn(config()).unwrap();
    open(
        &b,
        fixture.path.parent().unwrap(),
        lw::OpenMode::OpenExisting,
    );
    call(
        &b,
        Request::Read {
            guard: g(&status(&b)),
            query: Query::Rows {
                revision: fixture.revision().into(),
                table: None,
                after: I64(0),
                limit: U64(1),
            },
        },
    )
    .unwrap();
    let s = wait(&b);
    assert!(s.result_bytes.0 > ENVELOPE as u64);
    let exact = raw(&b);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&exact).unwrap()["rows"][0]["source_id"],
        text
    );
    let bad = call(
        &b,
        Request::Result {
            guard: g(&s),
            token: "not-the-token".into(),
            offset: U64(0),
            limit: U64(1),
        },
    );
    assert!(bad.is_err());
    let inventory = crate::lightroom::discovery::Inventory {
        protocol: crate::lightroom::PROTOCOL,
        root: NativePath::from_path(fixture.path.parent().unwrap()),
        candidates: vec![],
        exclusions: vec![],
        issues: vec![crate::lightroom::Issue {
            code: "synthetic-retained-detail".into(),
            source_id: None,
            detail: text,
        }],
        complete: false,
        entries: 0,
    };
    let json = serde_json::to_string(&inventory).unwrap();
    assert!(json.len() > ENVELOPE);
    let input = upload(&b, InputPurpose::Inventory, &json);
    act(&b, Action::RegisterInventory { input });
    b.shutdown();
}
#[cfg(unix)]
#[test]
fn bridge_cancel_and_close_reap_live_configured_capture_pid_and_retain_diagnostics() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("configured-owned-helper");
    std::fs::write(&script, b"#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut cfg = config();
    cfg.worker_executable = script;
    let b = app::Bridge::spawn(cfg).unwrap();
    open(&b, &temp.path().join("inspection"), lw::OpenMode::Create);
    for close in [false, true] {
        call(
            &b,
            Request::Action {
                guard: g(&status(&b)),
                action: Action::Capture {
                    source: NativePath::from_path(&temp.path().join("never-opened-original.lrcat")),
                    output: NativePath::from_path(&temp.path().join("capture")),
                    include_auxiliary: false,
                    closed_application_evidence: None,
                    limits: crate::lightroom::Limits::default().into(),
                },
            },
        )
        .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        let running = loop {
            let s = status(&b);
            if s.capture_pid.is_some() {
                break s;
            }
            assert!(
                Instant::now() < until && s.phase == lw::Phase::Running,
                "{s:?}"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        let pid = running.capture_pid.unwrap().0 as i32;
        let staging = running.capture_staging.unwrap().to_path().unwrap();
        assert!(staging.join("request.json").is_file());
        if close {
            call(
                &b,
                Request::Close {
                    workbench: running.workbench,
                },
            )
            .unwrap();
            assert!(wait(&b).closed);
        } else {
            call(
                &b,
                Request::Cancel {
                    guard: g(&status(&b)),
                },
            )
            .unwrap();
            assert_eq!(wait(&b).phase, lw::Phase::Canceled);
        }
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert!(staging.join("request.json").is_file());
        eprintln!(
            "bridge owned capture PID {pid} reaped before terminal; close={close}; retained request diagnostic"
        );
    }
    assert!(!temp.path().join("never-opened-original.lrcat").exists());
    b.shutdown();
}
