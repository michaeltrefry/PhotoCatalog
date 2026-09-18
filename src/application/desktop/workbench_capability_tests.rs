use super::workbench::Dispatcher;
use crate::{
    application::{
        Config, Limits, Reply, U64, lightroom as lw, lightroom_bridge as bridge, lightroom_managed,
    },
    filesystem_worker::wire::{
        LightroomArtifactPreparation, LightroomArtifactPreparationReply, LightroomSealedDocument,
        LightroomSealedRead,
    },
    lightroom::{
        capture,
        migration_source::tests::Fixture as InspectionFixture,
        selection::{FamilyDecision, SelectionRequest},
        source::Source,
    },
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

fn config() -> Config {
    Config {
        worker_executable: std::env::current_exe().unwrap(),
        cache_root: None,
        original_roots: vec![],
        preview_policy: Default::default(),
        preview_limits: Default::default(),
        limits: Limits::default(),
        import_checkpoint: None,
    }
}

fn call(dispatcher: &Dispatcher, request: bridge::Request) -> Result<bridge::Response> {
    match dispatcher.submit(request)?.recv() {
        Reply::Ok {
            value: crate::application::Response::Lightroom(value),
        } => Ok(*value),
        Reply::Error { error } => Err(anyhow::anyhow!("{:?}: {}", error.code, error.message)),
        _ => anyhow::bail!("managed Workbench reply kind"),
    }
}

fn status(dispatcher: &Dispatcher) -> Result<bridge::Status> {
    match call(
        dispatcher,
        bridge::Request::Status {
            workbench: None,
            attempt: None,
        },
    )? {
        bridge::Response::Status(Some(value)) => Ok(value),
        _ => anyhow::bail!("managed Workbench status is absent"),
    }
}

fn guard(status: &bridge::Status) -> bridge::Guard {
    bridge::Guard {
        workbench: status.workbench.clone(),
        generation: status.generation.clone(),
        operation: status.operation.clone(),
    }
}

fn wait(dispatcher: &Dispatcher) -> Result<bridge::Status> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let value = status(dispatcher)?;
        if matches!(
            value.phase,
            lw::Phase::Complete | lw::Phase::Failed | lw::Phase::Canceled | lw::Phase::Closed
        ) {
            return Ok(value);
        }
        ensure!(
            Instant::now() < deadline,
            "managed Workbench timed out: {value:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn result_json(dispatcher: &Dispatcher) -> Result<serde_json::Value> {
    let value = wait(dispatcher)?;
    ensure!(
        value.phase == lw::Phase::Complete,
        "managed action failed: {value:?}"
    );
    let token = value.result_token.clone().context("result token")?;
    let mut offset = U64(0);
    let mut json = String::new();
    loop {
        let bridge::Response::Result(page) = call(
            dispatcher,
            bridge::Request::Result {
                guard: guard(&value),
                token: token.clone(),
                offset,
                limit: U64(u64::MAX),
            },
        )?
        else {
            anyhow::bail!("managed Workbench result page kind")
        };
        json.push_str(&page.page.json_fragment);
        match page.page.next {
            Some(next) => offset = next,
            None => break,
        }
    }
    Ok(serde_json::from_str(&json)?)
}

fn action(dispatcher: &Dispatcher, action: bridge::Action) -> Result<serde_json::Value> {
    call(
        dispatcher,
        bridge::Request::Action {
            guard: guard(&status(dispatcher)?),
            action,
        },
    )?;
    result_json(dispatcher)
}

#[test]
fn managed_resume_cancel_close_reopen_preserves_rows_and_releases_custody() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let directory = std::fs::canonicalize(temp.path())?;
    std::fs::create_dir(directory.join("sources"))?;
    let source = directory.join("sources/synthetic.lrcat");
    {
        let db = rusqlite::Connection::open(&source)?;
        db.execute_batch(
            "CREATE TABLE Opaque(id INTEGER PRIMARY KEY, value TEXT);
            WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<4096)
            INSERT INTO Opaque SELECT x,'retained opaque data' FROM n;",
        )?;
    }
    let source_before = std::fs::read(&source)?;
    let captured = directory.join("capture");
    let manifest = capture::run_isolated(capture::Request {
        source: NativePath::from_path(&source),
        output: NativePath::from_path(&captured),
        include_auxiliary: false,
        closed_application_evidence: Some("synthetic fixture closed".into()),
        limits: crate::lightroom::Limits::default(),
    })?;
    ensure!(
        manifest.state == "captured",
        "fixture capture: {manifest:?}"
    );
    let capture_before = std::fs::read(captured.join("logical.sqlite3"))?;
    let fixture = lightroom_managed::tests::ManagedFixture::start(&directory)?;
    let config = config();
    let generation = Arc::new(lightroom_managed::Generation::start_fixture(
        &fixture.owner,
        &config.worker_executable,
    )?);
    let dispatcher = Dispatcher::start(&generation, config.limits)?;
    let root = directory.join("inspection");
    let open = |mode| -> Result<()> {
        call(
            &dispatcher,
            bridge::Request::Open {
                attempt: uuid::Uuid::new_v4().to_string(),
                root: NativePath::from_path(&root),
                mode,
                capture_staging: NativePath::from_path(&directory),
                limits: lw::Limits::default().into(),
            },
        )?;
        result_json(&dispatcher)?;
        Ok(())
    };
    open(lw::OpenMode::Create).context("initial open")?;
    let added = action(
        &dispatcher,
        bridge::Action::AddCapture {
            directory: NativePath::from_path(&captured),
        },
    )?;
    let revision = added["revision"]
        .as_str()
        .context("capture revision")?
        .to_owned();
    call(
        &dispatcher,
        bridge::Request::Action {
            guard: guard(&status(&dispatcher)?),
            action: bridge::Action::Resume {
                revision: revision.clone(),
                max_rows: U64(100_000),
            },
        },
    )?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let current = status(&dispatcher)?;
        ensure!(
            current.phase == lw::Phase::Running && Instant::now() < deadline,
            "resume did not reach cancellable progress: {current:?}"
        );
        if current.processed.0 > 0 {
            call(
                &dispatcher,
                bridge::Request::Cancel {
                    guard: guard(&current),
                },
            )?;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    ensure!(
        wait(&dispatcher)?.phase == lw::Phase::Canceled,
        "resume did not cancel"
    );
    let report = || -> Result<serde_json::Value> {
        call(
            &dispatcher,
            bridge::Request::Read {
                guard: guard(&status(&dispatcher)?),
                query: bridge::Query::Report {
                    revision: revision.clone(),
                },
            },
        )?;
        result_json(&dispatcher)
    };
    let partial = report()?;
    call(
        &dispatcher,
        bridge::Request::Close {
            workbench: status(&dispatcher)?.workbench,
        },
    )?;
    ensure!(
        wait(&dispatcher)?.phase == lw::Phase::Closed,
        "close did not settle"
    );
    open(lw::OpenMode::OpenExisting)?;
    ensure!(
        report()?["tables"] == partial["tables"],
        "reopen changed retained rows"
    );
    let resumed = action(
        &dispatcher,
        bridge::Action::Resume {
            revision: revision.clone(),
            max_rows: U64(100_000),
        },
    )?;
    ensure!(
        resumed["stage"] != "pending",
        "resume did not finish: {resumed}"
    );
    let complete = report()?;
    ensure!(
        complete["tables"]
            .as_array()
            .context("table report")?
            .iter()
            .all(|table| table["state"] == "complete"),
        "incomplete tables: {complete}"
    );
    call(
        &dispatcher,
        bridge::Request::Close {
            workbench: status(&dispatcher)?.workbench,
        },
    )?;
    ensure!(
        wait(&dispatcher)?.phase == lw::Phase::Closed,
        "final close did not settle"
    );
    dispatcher.shutdown_checked()?;
    fixture.owner.drain_checked()?;
    fixture.filesystem.try_shutdown()?;
    ensure!(generation.pid().is_none(), "W remains retained");
    let db = rusqlite::Connection::open(root.join("inspection.sqlite3"))?;
    let (count, distinct): (i64, i64) = db.query_row(
        "SELECT count(*),count(DISTINCT key_json) FROM rows WHERE revision=? AND table_name='Opaque'",
        [&revision], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        (count, distinct) == (4096, 4096),
        "lost or duplicate rows: {count}/{distinct}"
    );
    ensure!(std::fs::read(source)? == source_before, "source changed");
    ensure!(
        std::fs::read(captured.join("logical.sqlite3"))? == capture_before,
        "capture changed"
    );
    Ok(())
}

fn upload(dispatcher: &Dispatcher, purpose: bridge::InputPurpose, json: &str) -> Result<String> {
    let guard = guard(&status(dispatcher)?);
    let bridge::Response::Input(Some(input)) = call(
        dispatcher,
        bridge::Request::InputBegin {
            guard: guard.clone(),
            purpose,
            total_bytes: U64(json.len() as u64),
            expected_blake3: Some(blake3::hash(json.as_bytes()).to_hex().to_string()),
        },
    )?
    else {
        anyhow::bail!("managed input admission reply kind")
    };
    call(
        dispatcher,
        bridge::Request::InputAppend {
            guard: guard.clone(),
            input: input.input.clone(),
            offset: U64(0),
            fragment: json.into(),
        },
    )?;
    call(
        dispatcher,
        bridge::Request::InputFinish {
            guard,
            input: input.input.clone(),
        },
    )?;
    Ok(input.input)
}

fn write_capture(directory: &Path) -> Result<(String, String, String)> {
    std::fs::create_dir_all(directory.join("raw"))?;
    let artifact_path = directory.join("raw/original.bin");
    let bytes = b"exact retained artifact bytes";
    std::fs::write(&artifact_path, bytes)?;
    let source = Source::open(&artifact_path, u64::MAX)?;
    let revision = source.before.clone();
    drop(source);
    let artifact = capture::Artifact {
        source: NativePath::from_path(&artifact_path),
        role: "main".into(),
        relative: NativePath::from_path(Path::new("original.bin")),
        stored: "raw/original.bin".into(),
        revision,
        blake3: blake3::hash(bytes).to_hex().to_string(),
    };
    let capture_revision = crate::lightroom::json_digest(&vec![artifact.clone()])?;
    let manifest = capture::Manifest {
        protocol: 1,
        request: capture::Request {
            source: NativePath::from_path(&artifact_path),
            output: NativePath::from_path(directory),
            include_auxiliary: false,
            closed_application_evidence: Some("test fixture closed".into()),
            limits: crate::lightroom::Limits::default(),
        },
        state: "captured".into(),
        raw_byte_retention: "complete".into(),
        sqlite_consistency: "consistent_default_sqlite".into(),
        application_consistency: "closed_by_test".into(),
        cooperative_lock_protocol: "test".into(),
        artifacts: vec![artifact],
        companion_inventory: vec![],
        absent_companions: vec![],
        issues: vec![],
        wal: None,
        logical_blake3: None,
        logical_revision: None,
        revision_id: Some(capture_revision.clone()),
    };
    let manifest = crate::lightroom::bounded_json(&manifest, crate::lightroom::MANIFEST_BYTES)?;
    let digest = blake3::hash(&manifest).to_hex().to_string();
    let manifest_json = String::from_utf8(manifest.clone())?;
    std::fs::write(directory.join("manifest.json"), manifest)?;
    Ok((capture_revision, digest, manifest_json))
}

#[test]
fn managed_dispatcher_preserves_sealed_artifact_and_approval_capabilities() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = config();
    let directory = std::fs::canonicalize(temp.path())?;
    let fixture = lightroom_managed::tests::ManagedFixture::start(&directory)?;
    let filesystem = fixture.filesystem.clone();
    let owner = fixture.owner.clone();
    let generation = Arc::new(lightroom_managed::Generation::start_fixture(
        &owner,
        &config.worker_executable,
    )?);
    let dispatcher = Dispatcher::start(&generation, config.limits.clone())?;

    let sealed = directory.as_path().join("sealed");
    std::fs::create_dir(&sealed)?;
    std::fs::write(sealed.join("approval.json"), b"{\"approved\":true}")?;
    let sealed_session = uuid::Uuid::new_v4().to_string();
    let bridge::Response::SealedDocument(Some(begin)) = call(
        &dispatcher,
        bridge::Request::SealedDocument {
            request: LightroomSealedRead::Begin {
                session: sealed_session.clone(),
                directory: NativePath::from_path(&sealed),
                document: LightroomSealedDocument::Approval,
            },
        },
    )?
    else {
        anyhow::bail!("sealed begin reply kind")
    };
    ensure!(
        begin.bytes.is_empty() && begin.next == Some(U64(0)),
        "sealed begin receipt"
    );
    let bridge::Response::SealedDocument(Some(page)) = call(
        &dispatcher,
        bridge::Request::SealedDocument {
            request: LightroomSealedRead::Page {
                session: sealed_session.clone(),
                offset: U64(0),
                limit: U64(1024),
            },
        },
    )?
    else {
        anyhow::bail!("sealed page reply kind")
    };
    ensure!(page.bytes == b"{\"approved\":true}", "sealed page changed");
    ensure!(matches!(
        call(
            &dispatcher,
            bridge::Request::SealedDocument {
                request: LightroomSealedRead::Discard {
                    session: sealed_session
                },
            },
        )?,
        bridge::Response::SealedDocument(None)
    ));

    let capture = directory.as_path().join("capture");
    let (capture_revision, manifest_blake3, manifest_json) = write_capture(&capture)?;
    let artifact_session = uuid::Uuid::new_v4().to_string();
    ensure!(matches!(
        call(
            &dispatcher,
            bridge::Request::ArtifactPreparation {
                request: LightroomArtifactPreparation::Begin {
                    session: artifact_session.clone(),
                    directory: NativePath::from_path(&capture),
                    capture_revision: capture_revision.clone(),
                    manifest_blake3,
                    maximum_bytes: U64(1024 * 1024),
                    open_deadline_ms: U64(10_000),
                },
            },
        )?,
        bridge::Response::ArtifactPreparation(Some(
            LightroomArtifactPreparationReply::Begun { .. }
        ))
    ));
    let bridge::Response::ArtifactPreparation(Some(LightroomArtifactPreparationReply::Prepared {
        receipt,
        ..
    })) = call(
        &dispatcher,
        bridge::Request::ArtifactPreparation {
            request: LightroomArtifactPreparation::Member {
                session: artifact_session.clone(),
                member_index: U64(0),
            },
        },
    )?
    else {
        anyhow::bail!("artifact member reply kind")
    };

    let inspection = InspectionFixture::new();
    let inspection_root = inspection.path.parent().context("inspection root")?;
    let db = rusqlite::Connection::open(&inspection.path)?;
    let former_selected = inspection.revision().to_owned();
    db.execute(
        "UPDATE captures SET revision=?1,manifest=?2 WHERE revision=?3",
        rusqlite::params![capture_revision, manifest_json, former_selected],
    )?;
    for table in ["tables", "rows", "paths", "packets"] {
        db.execute(
            &format!("UPDATE {table} SET revision=?1 WHERE revision=?2"),
            rusqlite::params![capture_revision, former_selected],
        )?;
    }
    let captures = {
        let mut statement = db.prepare("SELECT revision,manifest FROM captures")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (revision, manifest) in captures {
        let manifest: capture::Manifest = serde_json::from_str(&manifest)?;
        db.execute(
            "UPDATE captures SET path=?1 WHERE revision=?2",
            rusqlite::params![serde_json::to_string(&manifest.request.output)?, revision],
        )?;
    }
    db.execute("DELETE FROM family_choices", [])?;
    drop(db);
    let attempt = uuid::Uuid::new_v4().to_string();
    call(
        &dispatcher,
        bridge::Request::Open {
            attempt,
            root: NativePath::from_path(inspection_root),
            mode: lw::OpenMode::OpenExisting,
            capture_staging: NativePath::from_path(directory.as_path()),
            limits: lw::Limits::default().into(),
        },
    )?;
    ensure!(
        wait(&dispatcher)?.initialized,
        "inspection did not initialize"
    );
    let selected = capture_revision;
    let excluded = inspection.seal.excluded_revisions[0].clone();
    action(
        &dispatcher,
        bridge::Action::AssignFamily {
            revision: selected.clone(),
            family: "selected".into(),
            reason: "managed capability test".into(),
        },
    )?;
    action(
        &dispatcher,
        bridge::Action::AssignFamily {
            revision: excluded.clone(),
            family: "excluded".into(),
            reason: "managed capability test".into(),
        },
    )?;
    call(
        &dispatcher,
        bridge::Request::Read {
            guard: guard(&status(&dispatcher)?),
            query: bridge::Query::Families {},
        },
    )?;
    let families = result_json(&dispatcher)?;
    let mut decisions = Vec::new();
    for family in families["families"].as_array().context("families")? {
        let id = family["id"].as_str().context("family id")?.to_owned();
        let evidence = family["evidence_digest"]
            .as_str()
            .context("family evidence")?
            .to_owned();
        if id == "explicit:selected" {
            action(
                &dispatcher,
                bridge::Action::Choose {
                    family: id.clone(),
                    revision: selected.clone(),
                    expected_evidence: evidence.clone(),
                    reason: "managed capability test".into(),
                },
            )?;
            decisions.push(FamilyDecision::Select {
                family: id,
                revision: selected.clone(),
                expected_evidence_digest: evidence,
            });
        } else {
            decisions.push(FamilyDecision::Exclude {
                family: id,
                expected_evidence_digest: evidence,
            });
        }
    }
    let selection = serde_json::to_string(&SelectionRequest {
        inspection: NativePath::from_path(&inspection.path),
        families: decisions,
    })?;
    let input = upload(
        &dispatcher,
        bridge::InputPurpose::SelectionRequest,
        &selection,
    )?;
    let summary = action(
        &dispatcher,
        bridge::Action::PrepareSelection {
            input,
            limits: crate::lightroom::selection::SelectionLimits::default().into(),
        },
    )?;
    let review_token = summary["token"].as_str().context("review token")?;
    let draft = serde_json::to_string(&serde_json::json!({
        "protocol": 1,
        "review_token": review_token,
        "destination": NativePath::from_path(&directory.as_path().join("destination.sqlite3")),
        "import_source": "managed callback fixture",
        "overlap": crate::catalog_migration::importer::OverlapPolicy::RequireDecision,
        "keyword_overlap": crate::catalog_migration::importer::KeywordOverlap::RequireDecision,
        "artifacts": [{"receipt": receipt}],
        "supplements": [],
        "authorization": "explicit managed callback test authority"
    }))?;
    let input = upload(&dispatcher, bridge::InputPurpose::ApprovalDraft, &draft)?;
    let documents = action(
        &dispatcher,
        bridge::Action::ApprovalDocuments {
            input,
            review_token: review_token.into(),
        },
    )?;
    ensure!(
        documents.to_string().contains("managed callback fixture"),
        "approval documents omitted resolved policy"
    );

    ensure!(matches!(
        call(
            &dispatcher,
            bridge::Request::ArtifactPreparation {
                request: LightroomArtifactPreparation::DiscardReceipt {
                    receipt: receipt.clone(),
                },
            },
        )?,
        bridge::Response::ArtifactPreparation(None)
    ));
    ensure!(
        call(
            &dispatcher,
            bridge::Request::ArtifactPreparation {
                request: LightroomArtifactPreparation::Resolve { receipt },
            },
        )
        .is_err(),
        "discarded artifact receipt resolved again"
    );
    ensure!(matches!(
        call(
            &dispatcher,
            bridge::Request::ArtifactPreparation {
                request: LightroomArtifactPreparation::Discard {
                    session: artifact_session,
                },
            },
        )?,
        bridge::Response::ArtifactPreparation(None)
    ));
    dispatcher.shutdown_checked()?;
    owner.drain_checked()?;
    filesystem.try_shutdown()?;
    ensure!(
        generation.pid().is_none(),
        "managed Workbench process was not checked-reaped"
    );
    Ok(())
}

fn cancel_after_capability_effect(
    fixture: &lightroom_managed::tests::ManagedFixture,
    request: bridge::Request,
    target: lightroom_managed::tests::CapabilityAck,
) -> Result<()> {
    let config = config();
    let generation = Arc::new(lightroom_managed::Generation::start_fixture(
        &fixture.owner,
        &config.worker_executable,
    )?);
    let dispatcher = Dispatcher::start(&generation, config.limits)?;
    let probe = lightroom_managed::tests::CapabilityAckProbe::new(target);
    fixture.owner.install_capability_ack_probe(probe.clone());
    let pending = dispatcher.submit(request)?;
    probe.wait_reached(Duration::from_secs(20))?;
    pending.cancel();
    probe.release();
    ensure!(
        matches!(pending.recv(), Reply::Error { .. }),
        "canceled capability unexpectedly returned a reply"
    );
    dispatcher.shutdown_checked()?;
    ensure!(generation.pid().is_none(), "canceled W was not reaped");
    ensure!(
        fixture.owner.capability_custody_empty(),
        "capability custody remained after checked W/F drain"
    );
    Ok(())
}

#[test]
fn canceled_sealed_begin_reconciles_lost_ack_before_f_release() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let directory = std::fs::canonicalize(temp.path())?;
    let fixture = lightroom_managed::tests::ManagedFixture::start(&directory)?;
    let sealed = directory.join("sealed-cancel");
    std::fs::create_dir(&sealed)?;
    std::fs::write(sealed.join("approval.json"), b"{\"approved\":true}")?;
    cancel_after_capability_effect(
        &fixture,
        bridge::Request::SealedDocument {
            request: LightroomSealedRead::Begin {
                session: uuid::Uuid::new_v4().to_string(),
                directory: NativePath::from_path(&sealed),
                document: LightroomSealedDocument::Approval,
            },
        },
        lightroom_managed::tests::CapabilityAck::SealedBegin,
    )?;

    let replacement = uuid::Uuid::new_v4().to_string();
    let cancel = AtomicBool::new(false);
    ensure!(
        fixture
            .filesystem
            .lightroom_sealed_read(
                &LightroomSealedRead::Begin {
                    session: replacement.clone(),
                    directory: NativePath::from_path(&sealed),
                    document: LightroomSealedDocument::Approval,
                },
                &cancel,
            )?
            .is_some(),
        "replacement sealed session was not admitted"
    );
    fixture.filesystem.lightroom_sealed_read(
        &LightroomSealedRead::Discard {
            session: replacement,
        },
        &cancel,
    )?;
    Ok(())
}

#[test]
fn canceled_artifact_begin_reconciles_lost_ack_before_f_release() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let directory = std::fs::canonicalize(temp.path())?;
    let fixture = lightroom_managed::tests::ManagedFixture::start(&directory)?;
    let capture = directory.join("capture-begin-cancel");
    let (revision, manifest, _) = write_capture(&capture)?;
    cancel_after_capability_effect(
        &fixture,
        bridge::Request::ArtifactPreparation {
            request: LightroomArtifactPreparation::Begin {
                session: uuid::Uuid::new_v4().to_string(),
                directory: NativePath::from_path(&capture),
                capture_revision: revision.clone(),
                manifest_blake3: manifest.clone(),
                maximum_bytes: U64(1024 * 1024),
                open_deadline_ms: U64(10_000),
            },
        },
        lightroom_managed::tests::CapabilityAck::ArtifactBegin,
    )?;

    let replacement = uuid::Uuid::new_v4().to_string();
    let cancel = AtomicBool::new(false);
    ensure!(
        fixture
            .filesystem
            .lightroom_artifact_preparation(
                &LightroomArtifactPreparation::Begin {
                    session: replacement.clone(),
                    directory: NativePath::from_path(&capture),
                    capture_revision: revision,
                    manifest_blake3: manifest,
                    maximum_bytes: U64(1024 * 1024),
                    open_deadline_ms: U64(10_000),
                },
                &cancel,
            )?
            .is_some(),
        "replacement artifact session was not admitted"
    );
    fixture.filesystem.lightroom_artifact_preparation(
        &LightroomArtifactPreparation::Discard {
            session: replacement,
        },
        &cancel,
    )?;
    Ok(())
}

#[test]
fn canceled_artifact_member_retires_the_unacknowledged_receipt() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let directory = std::fs::canonicalize(temp.path())?;
    let fixture = lightroom_managed::tests::ManagedFixture::start(&directory)?;
    let config = config();
    let generation = Arc::new(lightroom_managed::Generation::start_fixture(
        &fixture.owner,
        &config.worker_executable,
    )?);
    let dispatcher = Dispatcher::start(&generation, config.limits)?;
    let capture = directory.join("capture-member-cancel");
    let (revision, manifest, _) = write_capture(&capture)?;
    let session = uuid::Uuid::new_v4().to_string();
    ensure!(matches!(
        call(
            &dispatcher,
            bridge::Request::ArtifactPreparation {
                request: LightroomArtifactPreparation::Begin {
                    session: session.clone(),
                    directory: NativePath::from_path(&capture),
                    capture_revision: revision.clone(),
                    manifest_blake3: manifest.clone(),
                    maximum_bytes: U64(1024 * 1024),
                    open_deadline_ms: U64(10_000),
                },
            },
        )?,
        bridge::Response::ArtifactPreparation(Some(
            LightroomArtifactPreparationReply::Begun { .. }
        ))
    ));
    let probe = lightroom_managed::tests::CapabilityAckProbe::new(
        lightroom_managed::tests::CapabilityAck::ArtifactMember,
    );
    fixture.owner.install_capability_ack_probe(probe.clone());
    let pending = dispatcher.submit(bridge::Request::ArtifactPreparation {
        request: LightroomArtifactPreparation::Member {
            session,
            member_index: U64(0),
        },
    })?;
    probe.wait_reached(Duration::from_secs(20))?;
    let lost_receipt = probe.receipt();
    pending.cancel();
    probe.release();
    let lost_receipt = lost_receipt.context("artifact member probe omitted the created receipt")?;
    ensure!(matches!(pending.recv(), Reply::Error { .. }));
    dispatcher.shutdown_checked()?;
    ensure!(generation.pid().is_none(), "canceled W was not reaped");
    ensure!(fixture.owner.capability_custody_empty());

    let cancel = AtomicBool::new(false);
    ensure!(
        fixture
            .filesystem
            .lightroom_artifact_preparation(
                &LightroomArtifactPreparation::Resolve {
                    receipt: lost_receipt,
                },
                &cancel,
            )
            .is_err(),
        "unacknowledged artifact receipt remained in F"
    );
    let replacement = uuid::Uuid::new_v4().to_string();
    fixture.filesystem.lightroom_artifact_preparation(
        &LightroomArtifactPreparation::Begin {
            session: replacement.clone(),
            directory: NativePath::from_path(&capture),
            capture_revision: revision,
            manifest_blake3: manifest,
            maximum_bytes: U64(1024 * 1024),
            open_deadline_ms: U64(10_000),
        },
        &cancel,
    )?;
    fixture.filesystem.lightroom_artifact_preparation(
        &LightroomArtifactPreparation::Discard {
            session: replacement,
        },
        &cancel,
    )?;
    Ok(())
}
