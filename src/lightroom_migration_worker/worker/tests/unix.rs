use super::*;
use crate::{
    Catalog,
    catalog_migration::{evidence, importer_tests::ImportFixture},
    catalog_writer::Writers,
    lightroom::selection::{ApprovalDocument, ApprovalScope},
    lightroom_migration_worker::{
        memory::MemoryBudget,
        process::Stop,
        protocol::{InputRole, WriteKind},
        supervisor::{Admission, Drained, FailureCause, InputPart, execute_operation},
    },
};
use anyhow::{Result, ensure};
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const CHILD_ROLE: &str = "PHOTOCATALOG_LM_BATCH3_CHILD_ROLE";
const CHILD_PIDS: &str = "PHOTOCATALOG_LM_BATCH3_CHILD_PIDS";

/// One libtest entrypoint stands in for the installed executable while retaining
/// the production LM/SQL/Raw dispatch bodies and framed pipes.
#[test]
fn lm_executor_batch3_actual_role_fixture() -> Result<()> {
    let Some(role) = std::env::var_os(CHILD_ROLE) else {
        return Ok(());
    };
    if let Some(path) = std::env::var_os(CHILD_PIDS) {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{} {}", std::process::id(), role.to_string_lossy())?;
    }
    use std::os::fd::FromRawFd;
    // The shell fixture reserves fd 3 for framed protocol. Libtest and the
    // shared executor remain free to write diagnostics elsewhere.
    let protocol = unsafe { fs::File::from_raw_fd(3) };
    match role.to_str() {
        Some("--lightroom-migration-worker") => serve(std::io::stdin(), protocol),
        Some("--lightroom-source-reader-sql") => {
            super::super::super::source_reader::test_source_reader_main(false, protocol)
        }
        Some("--lightroom-source-reader-raw") => {
            super::super::super::source_reader::test_source_reader_main(true, protocol)
        }
        _ => anyhow::bail!("unexpected managed migration role"),
    }
}

struct Parent {
    root: PathBuf,
    writers: Option<Arc<Writers>>,
    events: Arc<Mutex<Vec<String>>>,
}
impl Admission for Parent {
    fn lock(&mut self, _: &str, destination: &DestinationPin, _: &FileKey) -> Result<()> {
        ensure!(
            destination.root == NativePath::from_path(&self.root),
            "destination pin differs"
        );
        self.events.lock().unwrap().push("lock".into());
        Ok(())
    }
    fn writer(
        &mut self,
        _: u64,
        kind: WriteKind,
        _: &str,
        _: Option<&FileKey>,
        _: &Stop,
        _: Instant,
    ) -> Result<Arc<Writers>> {
        if kind == WriteKind::Bootstrap {
            ensure!(self.writers.is_none(), "bootstrap repeated");
            let existed = self.root.join("catalog.sqlite3").exists();
            let lock_existed = self.root.join(".lightroom-import.lock").exists();
            let catalog = Catalog::open(&self.root)?;
            crate::catalog_migration::importer::install(&catalog.db)?;
            crate::catalog_migration::reconciliation::install(&catalog.db)?;
            self.writers = Some(catalog.writers.clone());
            self.events.lock().unwrap().push(
                if !existed {
                    "bootstrap"
                } else if !lock_existed {
                    "first_use"
                } else {
                    "upgrade"
                }
                .into(),
            );
        } else if self.writers.is_none() {
            let catalog = Catalog::open(&self.root)?;
            self.writers = Some(catalog.writers.clone());
        }
        self.writers
            .clone()
            .context("catalog writer requested before bootstrap")
    }
    fn release(&mut self, _: u64, kind: WriteKind) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(format!("release:{kind:?}"));
        Ok(())
    }
    fn progress(&mut self, phase: &str, completed: u64, total: Option<u64>) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(format!("progress:{phase}:{completed}:{total:?}"));
        Ok(())
    }
}

fn wrapper(root: &Path, pids: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join("managed-lightroom-fixture");
    let current = std::env::current_exe()?;
    let body = format!(
        "#!/bin/sh\nrole=\"$1\"\nexec env {CHILD_ROLE}=\"$role\" {CHILD_PIDS}='{}' '{}' --exact lightroom_migration_worker::worker::tests::unix::lm_executor_batch3_actual_role_fixture --nocapture 3>&1 1>/dev/null 2>/dev/null\n",
        pids.display(),
        current.display()
    );
    fs::write(&path, body)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

fn drive<A: Admission>(
    operation: &mut super::super::super::supervisor::Operation<A>,
) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(180);
    loop {
        if operation.retry_drain().is_some() {
            return Ok(());
        }
        ensure!(Instant::now() < until, "managed worker fixture deadline");
        thread::sleep(Duration::from_millis(2));
    }
}

fn saved_text(saved: &super::super::super::supervisor::SavedResult) -> String {
    (0..saved.page_count())
        .filter_map(|index| saved.page(index))
        .collect()
}

fn completed<'a>(
    drained: Option<&'a Drained>,
    label: &str,
) -> Result<&'a super::super::super::supervisor::SavedResult> {
    match drained.with_context(|| format!("managed operation {label} did not drain"))? {
        Drained::Complete(saved) => Ok(saved),
        Drained::Failed(failure) => {
            anyhow::bail!("managed operation {label} failed: {failure:?}")
        }
    }
}

fn test_budget() -> Result<MemoryBudget> {
    let allowance = crate::catalog_migration::file_metadata::tests::Test::managed_preprojection()?
        .managed_preprojection_allowance_bytes()?;
    let limit = crate::lightroom_migration_worker::memory::layout::add(
        crate::lightroom_migration_worker::memory::layout::add(2 * 1024 * 1024 * 1024, allowance)?,
        crate::lightroom_migration_worker::memory::core::worker_repair_execution()?,
    )?;
    MemoryBudget::new(limit)
}

fn managed_before_document_bytes(executable: &Path, encoded: &str) -> Result<usize> {
    use crate::lightroom_migration_worker::{
        input::INPUT_BYTES,
        memory::{core, layout::add, transport},
        process::Process,
        source_reader::relay::broker::Broker,
    };
    let mut prior = add(INPUT_BYTES, core::worker_envelope(encoded.len())?)?;
    prior = add(prior, Process::<ChildFrame>::allocation_backing()?)?;
    prior = add(prior, Broker::allocation_backing()?)?;
    prior = add(prior, executable.as_os_str().len())?;
    add(prior, transport::payloads(true)?.total()?)
}

fn actual_operation(
    executable: &Path,
    destination: &Path,
    operation: Operation,
    documents: &[(InputRole, String)],
    label: &str,
    budget: MemoryBudget,
    events: Arc<Mutex<Vec<String>>>,
) -> Result<serde_json::Value> {
    let envelope = base(
        operation,
        documents
            .iter()
            .map(|(role, text)| descriptor(*role, text))
            .collect(),
        destination,
    );
    let encoded = serde_json::to_string(&envelope)?;
    let parts: Vec<_> = documents
        .iter()
        .map(|(role, text)| InputPart { role: *role, text })
        .collect();
    let mut running = execute_operation(
        executable,
        Guard {
            session: "batch3".into(),
            generation: label.into(),
            operation: label.into(),
        },
        &encoded,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(180),
        Parent {
            root: destination.into(),
            writers: None,
            events,
        },
        budget.clone(),
        &parts,
        envelope.operation.result_maximum()?,
    );
    drive(&mut running)?;
    let saved = completed(running.retry_drain(), label)?;
    Ok(serde_json::from_str(&saved_text(saved))?)
}

fn actual_rejection(
    executable: &Path,
    destination: &Path,
    operation: Operation,
    documents: &[(InputRole, String)],
    label: &str,
    budget: MemoryBudget,
    events: Arc<Mutex<Vec<String>>>,
) -> Result<String> {
    let envelope = base(
        operation,
        documents
            .iter()
            .map(|(role, text)| descriptor(*role, text))
            .collect(),
        destination,
    );
    let encoded = serde_json::to_string(&envelope)?;
    let parts: Vec<_> = documents
        .iter()
        .map(|(role, text)| InputPart { role: *role, text })
        .collect();
    let mut running = execute_operation(
        executable,
        Guard {
            session: "batch3-negative".into(),
            generation: label.into(),
            operation: label.into(),
        },
        &encoded,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(180),
        Parent {
            root: destination.into(),
            writers: None,
            events,
        },
        budget.clone(),
        &parts,
        envelope.operation.result_maximum()?,
    );
    drive(&mut running)?;
    let detail = match running.retry_drain().context("rejection did not drain")? {
        Drained::Failed(failure) => match &failure.cause {
            FailureCause::Rejected(detail) => detail.clone(),
            _ => anyhow::bail!("unexpected managed rejection: {failure:?}"),
        },
        Drained::Complete(_) => anyhow::bail!("invalid managed operation completed"),
    };
    drop(running);
    assert_eq!(budget.used(), 0);
    Ok(detail)
}

fn complete_evidence_bytes(catalog: &Catalog, id: &str) -> Result<Vec<u8>> {
    let state = catalog.migration_evidence(id)?;
    ensure!(state.complete, "fixture evidence {id} is incomplete");
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < state.length {
        bytes.extend(catalog.migration_evidence_chunk(id, bytes.len() as u64)?);
    }
    assert_eq!(bytes.len() as u64, state.length);
    let descriptor: serde_json::Value =
        serde_json::from_slice(&catalog.migration_evidence_descriptor(id)?)?;
    assert_eq!(descriptor["adapter"], "qualified-psd-supplement-v1");
    assert_eq!(descriptor["blake3"], blake3::hash(&bytes).to_hex().as_str());
    assert_eq!(descriptor["bytes"], bytes.len() as u64);
    Ok(bytes)
}

fn assert_supplement_fixture_evidence(
    destination: &Path,
    output: &serde_json::Value,
    request: &crate::catalog_migration::supplements::Request,
) -> Result<()> {
    use crate::catalog_migration::{file_metadata::SupplementalProof, supplements::Prepared};

    let prepared: Vec<Prepared> = serde_json::from_value(output.clone())?;
    ensure!(
        prepared.len() == 1,
        "fixture prepared supplement roster differs"
    );
    let prepared = &prepared[0];
    let catalog = Catalog::open(destination)?;
    let normalized = complete_evidence_bytes(&catalog, &prepared.evidence)?;
    assert_eq!(
        blake3::hash(&normalized).to_hex().as_str(),
        prepared.pin.proof_blake3
    );
    let proof: SupplementalProof = serde_json::from_slice(&normalized)?;
    ensure!(
        proof.packets.len() == 1 && proof.parse_inputs.len() == 1,
        "fixture proof payload roster differs"
    );

    let validation = complete_evidence_bytes(&catalog, &proof.validation_document.evidence)?;
    assert_eq!(
        blake3::hash(&validation).to_hex().as_str(),
        request.inspection_blake3
    );
    assert_eq!(proof.validation_document.length, validation.len() as u64);
    assert_eq!(
        proof.validation_document.blake3,
        blake3::hash(&validation).to_hex().as_str()
    );
    let packet = complete_evidence_bytes(&catalog, &proof.packets[0].payload.evidence)?;
    assert_eq!(
        packet.as_slice(),
        b"<x:xmpmeta>retained exact PSD packet</x:xmpmeta>"
    );
    assert_eq!(proof.packets[0].payload.length, packet.len() as u64);
    assert_eq!(
        proof.packets[0].payload.blake3,
        blake3::hash(&packet).to_hex().as_str()
    );
    assert_eq!(
        proof.packets[0].payload.evidence, proof.parse_inputs[0].payload.evidence,
        "identity transform must reuse the byte-identical retained payload"
    );
    assert_eq!(
        proof.packets[0].payload.length,
        proof.parse_inputs[0].payload.length
    );
    assert_eq!(
        proof.packets[0].payload.blake3,
        proof.parse_inputs[0].payload.blake3
    );

    let mut expected = vec![
        prepared.evidence.clone(),
        proof.validation_document.evidence,
        proof.packets[0].payload.evidence.clone(),
    ];
    expected.sort();
    expected.dedup();
    ensure!(
        expected.len() == 3,
        "fixture evidence classes must have three distinct identities"
    );
    let mut statement = catalog
        .db
        .prepare("SELECT id FROM migration_evidence ORDER BY id")?;
    let actual = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn lm_executor_batch3_startup_rejection_preserves_child_detail_before_admission() -> Result<()> {
    let root = tempfile::tempdir()?;
    let destination = root.path().canonicalize()?.join("never-created");
    let pids = root.path().join("pids");
    let executable = wrapper(root.path(), &pids)?;
    let envelope = base(
        Operation::PrepareSupplements,
        vec![descriptor(InputRole::SupplementRequests, "[")],
        &destination,
    );
    let encoded = serde_json::to_string(&envelope)?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let budget = test_budget()?;
    // Valid envelope, deliberately mismatched exact multipart descriptor. The
    // child refuses BeginPart before requesting retained storage or Admitted.
    let mut running = execute_operation(
        &executable,
        Guard {
            session: "startup".into(),
            generation: "mismatch".into(),
            operation: "supplement".into(),
        },
        &encoded,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(60),
        Parent {
            root: destination.clone(),
            writers: None,
            events: events.clone(),
        },
        budget.clone(),
        &[InputPart {
            role: InputRole::SupplementRequests,
            text: "[]",
        }],
        envelope.operation.result_maximum()?,
    );
    drive(&mut running)?;
    let failure = match running
        .retry_drain()
        .context("startup failure did not drain")?
    {
        Drained::Failed(failure) => failure,
        Drained::Complete(_) => anyhow::bail!("invalid startup completed"),
    };
    let FailureCause::Rejected(detail) = &failure.cause else {
        anyhow::bail!("startup rejection lost its cause: {failure:?}")
    };
    assert_eq!(detail, "migration input part descriptor differs");
    assert!(!failure.poisoned);
    assert!(!failure.outcome_unknown);
    assert!(events.lock().unwrap().is_empty());
    assert!(!destination.exists());
    let lines = fs::read_to_string(&pids)?;
    assert_eq!(lines.lines().count(), 1);
    for line in lines.lines() {
        assert!(line.ends_with("--lightroom-migration-worker"), "{line}");
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    drop(running);
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn lm_executor_batch3_actual_lm_sql_raw_bootstrap_run_and_status() -> Result<()> {
    let fixture = ImportFixture::new(false)?;
    assert!(!fixture.destination.join("catalog.sqlite3").exists());
    let root = tempfile::tempdir()?;
    let pids = root.path().join("pids");
    let executable = wrapper(root.path(), &pids)?;
    let destination = NativePath::from_path(&fixture.destination);
    let policy_json = serde_json::to_string(&fixture.policy)?;
    let approval = ApprovalDocument {
        protocol: 1,
        review_token: "e".repeat(64),
        scope: ApprovalScope::SelectedMigrationTest,
        destination: destination.clone(),
        policy: fixture.policy.clone(),
        supplements: vec![],
        authorization: "explicit synthetic managed migration".into(),
    };
    let approval_json = serde_json::to_string(&approval)?;
    let approval_blake3 = blake3::hash(approval_json.as_bytes()).to_hex().to_string();
    let mut seal = fixture.inspection.seal.clone();
    seal.approval.document_blake3 = approval_blake3.clone();
    let seal_json = serde_json::to_string(&seal)?;
    let descriptors = [
        descriptor(InputRole::Seal, &seal_json),
        descriptor(InputRole::Approval, &approval_json),
        descriptor(InputRole::Policy, &policy_json),
    ];
    let envelope = base(
        Operation::Run {
            approval_blake3,
            max_steps: U64(4000),
            max_seconds: U64(120),
            source_open_ms: U64(30_000),
            artifact_open_ms: U64(ImportFixture::limits().open_deadline_ms),
            max_artifact_bytes: U64(ImportFixture::limits().maximum_bytes),
        },
        descriptors.to_vec(),
        &fixture.destination,
    );
    let encoded = serde_json::to_string(&envelope)?;
    let parts = [
        InputPart {
            role: InputRole::Seal,
            text: &seal_json,
        },
        InputPart {
            role: InputRole::Approval,
            text: &approval_json,
        },
        InputPart {
            role: InputRole::Policy,
            text: &policy_json,
        },
    ];
    let events = Arc::new(Mutex::new(Vec::new()));
    let parent = Parent {
        root: fixture.destination.clone(),
        writers: None,
        events: events.clone(),
    };
    let budget = test_budget()?;
    let stop = Arc::new(Stop::default());
    let mut operation = execute_operation(
        &executable,
        Guard {
            session: "batch3".into(),
            generation: "one".into(),
            operation: "run".into(),
        },
        &encoded,
        stop,
        Instant::now() + Duration::from_secs(180),
        parent,
        budget.clone(),
        &parts,
        envelope.operation.result_maximum()?,
    );
    drive(&mut operation)?;
    let saved = completed(operation.retry_drain(), "run")?;
    let run_json = saved_text(saved);
    let run: serde_json::Value = serde_json::from_str(&run_json)?;
    assert_eq!(run["status"], "complete");
    let run_id = run["progress"]["id"]
        .as_str()
        .context("run id absent")?
        .to_owned();
    assert_eq!(
        events.lock().unwrap().first().map(String::as_str),
        Some("bootstrap")
    );
    assert!(fixture.destination.join(".lightroom-import.lock").is_file());

    let catalog = fixture.open()?;
    assert_eq!(
        catalog
            .db
            .query_row("SELECT count(*) FROM migration_runs", [], |row| row
                .get::<_, i64>(0))?,
        1
    );
    for (index, expected) in fixture.raw().iter().enumerate() {
        let revision = &fixture.inspection.seal.selected[index].revision;
        let captures = catalog.retained_migration_records(
            run["progress"]["input"]
                .as_str()
                .context("run input absent")?,
            revision,
            crate::lightroom::migration_source::Collection::Captures,
            0,
            2,
        )?;
        assert_eq!(captures.len(), 1, "capture roster for revision {revision}");
        let (_, state) = catalog.migration_artifact(captures[0].0, 0)?;
        let mut actual = Vec::new();
        while actual.len() < expected.len() {
            actual.extend(evidence::read(&catalog.db, &state.id, actual.len() as u64)?);
        }
        assert_eq!(&actual, expected);
    }
    drop(catalog);

    // A stale reviewed schema pin must request the same Bootstrap grant. The
    // parent publishes the already-upgraded current object before Grant, after
    // which LM reopens and pins that current object itself.
    let root_file = fs::File::open(&fixture.destination)?;
    let database_file = fs::File::open(fixture.destination.join("catalog.sqlite3"))?;
    let mut upgrade_envelope = envelope.clone();
    upgrade_envelope.expected_destination = Some(DestinationPin {
        root: destination.clone(),
        root_key: FileKey::of(&root_file)?,
        database_key: FileKey::of(&database_file)?,
        schema: crate::application::I64(crate::CURRENT_SCHEMA_VERSION - 1),
    });
    let upgrade_json = serde_json::to_string(&upgrade_envelope)?;
    let upgrade_events = Arc::new(Mutex::new(Vec::new()));
    let mut upgrade = execute_operation(
        &executable,
        Guard {
            session: "batch3".into(),
            generation: "upgrade".into(),
            operation: "run".into(),
        },
        &upgrade_json,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(180),
        Parent {
            root: fixture.destination.clone(),
            writers: None,
            events: upgrade_events.clone(),
        },
        budget.clone(),
        &parts,
        upgrade_envelope.operation.result_maximum()?,
    );
    drive(&mut upgrade)?;
    completed(upgrade.retry_drain(), "reviewed_upgrade")?;
    assert_eq!(
        upgrade_events.lock().unwrap().first().map(String::as_str),
        Some("upgrade")
    );

    let status_envelope = base(
        Operation::Status { run: run_id },
        vec![],
        &fixture.destination,
    );
    let status_json = serde_json::to_string(&status_envelope)?;
    let parent = Parent {
        root: fixture.destination.clone(),
        writers: None,
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let mut status = execute_operation(
        &executable,
        Guard {
            session: "batch3".into(),
            generation: "two".into(),
            operation: "status".into(),
        },
        &status_json,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(60),
        parent,
        budget.clone(),
        &[],
        status_envelope.operation.result_maximum()?,
    );
    drive(&mut status)?;
    let status_saved = completed(status.retry_drain(), "status")?;
    let status_value: serde_json::Value = serde_json::from_str(&saved_text(status_saved))?;
    assert_eq!(status_value, run["progress"]);

    let lines = fs::read_to_string(&pids)?;
    assert_eq!(lines.matches("--lightroom-migration-worker").count(), 3);
    assert_eq!(lines.matches("--lightroom-source-reader-sql").count(), 2);
    assert_eq!(
        lines.matches("--lightroom-source-reader-raw").count(),
        fixture.raw().len()
    );
    for line in lines.lines() {
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "managed child {pid} remains live"
        );
    }
    drop(operation);
    drop(upgrade);
    drop(status);
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn lm_executor_batch3_actual_remaining_five_operations_preserve_rows_and_readonly_status()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let pids = root.path().join("pids");
    let executable = wrapper(root.path(), &pids)?;
    let budget = test_budget()?;

    let supplement = crate::catalog_migration::supplements::tests::Fixture::new()?;
    let supplement_destination = root.path().canonicalize()?.join("supplement-target");
    drop(Catalog::open(&supplement_destination)?);
    let supplement_events = Arc::new(Mutex::new(Vec::new()));
    let supplement_output = actual_operation(
        &executable,
        &supplement_destination,
        Operation::PrepareSupplements,
        &[(
            InputRole::SupplementRequests,
            serde_json::to_string(&vec![supplement.request.clone()])?,
        )],
        "supplements",
        budget.clone(),
        supplement_events.clone(),
    )?;
    assert_eq!(supplement_output.as_array().map(Vec::len), Some(1));
    assert_supplement_fixture_evidence(
        &supplement_destination,
        &supplement_output,
        &supplement.request,
    )?;
    assert_eq!(
        supplement_events
            .lock()
            .unwrap()
            .first()
            .map(String::as_str),
        Some("first_use")
    );
    assert!(
        supplement_destination
            .join(".lightroom-import.lock")
            .is_file()
    );

    let current =
        crate::catalog_migration::importer_tests::current_repair_tests::managed_fixture()?;
    let current_documents = [
        (
            InputRole::Seal,
            serde_json::to_string(&current.fixture.inspection.seal)?,
        ),
        (InputRole::Approval, current.approval.clone()),
        (
            InputRole::RepairRequest,
            serde_json::to_string(&current.request)?,
        ),
    ];
    let current_output = actual_operation(
        &executable,
        &current.fixture.destination,
        Operation::RepairCurrent {
            max_steps: U64(1000),
            max_seconds: U64(120),
            source_open_ms: U64(30_000),
        },
        &current_documents,
        "current",
        budget.clone(),
        Arc::new(Mutex::new(Vec::new())),
    )?;
    assert_eq!(current_output["repair"]["complete"], true);
    let current_id = current_output["repair"]["id"]
        .as_str()
        .context("current repair id absent")?
        .to_owned();
    let current_status_events = Arc::new(Mutex::new(Vec::new()));
    let current_status = actual_operation(
        &executable,
        &current.fixture.destination,
        Operation::RepairStatus { repair: current_id },
        &[],
        "current_status",
        budget.clone(),
        current_status_events.clone(),
    )?;
    assert_eq!(current_status, current_output["repair"]);
    assert!(current_status_events.lock().unwrap().is_empty());
    assert_eq!(
        current.fixture.open()?.db.query_row(
            "SELECT count(*) FROM migration_current_repair_items",
            [],
            |row| row.get::<_, i64>(0),
        )?,
        6
    );

    let keywords =
        crate::catalog_migration::importer_tests::keyword_repair_tests::managed_fixture()?;
    let keyword_documents = [
        (
            InputRole::Seal,
            serde_json::to_string(&keywords.fixture.inspection.seal)?,
        ),
        (InputRole::Approval, keywords.approval.clone()),
        (
            InputRole::RepairRequest,
            serde_json::to_string(&keywords.request)?,
        ),
    ];
    let keyword_output = actual_operation(
        &executable,
        &keywords.fixture.destination,
        Operation::RepairKeywords {
            max_steps: U64(1000),
            max_seconds: U64(120),
            source_open_ms: U64(30_000),
        },
        &keyword_documents,
        "keywords",
        budget.clone(),
        Arc::new(Mutex::new(Vec::new())),
    )?;
    assert_eq!(keyword_output["repair"]["complete"], true);
    let keyword_id = keyword_output["repair"]["id"]
        .as_str()
        .context("keyword repair id absent")?
        .to_owned();
    let keyword_status_events = Arc::new(Mutex::new(Vec::new()));
    let keyword_status = actual_operation(
        &executable,
        &keywords.fixture.destination,
        Operation::KeywordRepairStatus { repair: keyword_id },
        &[],
        "keyword_status",
        budget.clone(),
        keyword_status_events.clone(),
    )?;
    assert_eq!(keyword_status, keyword_output["repair"]);
    assert!(keyword_status_events.lock().unwrap().is_empty());
    assert_eq!(
        keywords.fixture.open()?.db.query_row(
            "SELECT count(*) FROM migration_keyword_repair_items",
            [],
            |row| row.get::<_, i64>(0),
        )?,
        14
    );

    let lines = fs::read_to_string(&pids)?;
    assert_eq!(lines.matches("--lightroom-migration-worker").count(), 5);
    assert_eq!(lines.matches("--lightroom-source-reader-sql").count(), 2);
    assert_eq!(lines.matches("--lightroom-source-reader-raw").count(), 0);
    for line in lines.lines() {
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "managed child {pid} remains live"
        );
    }
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn lm_executor_batch3_preflight_disjointness_and_status_refuse_before_mutation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let canonical = root.path().canonicalize()?;
    let pids = canonical.join("pids");
    let executable = wrapper(&canonical, &pids)?;
    let budget = test_budget()?;

    let fixture = ImportFixture::new(false)?;
    let policy_json = serde_json::to_string(&fixture.policy)?;
    let source_parent = fixture
        .inspection
        .seal
        .database
        .to_path()?
        .parent()
        .context("inspection parent")?
        .canonicalize()?;
    let destination = source_parent.join("nested-managed-target");
    let approved_destination = NativePath::from_path(&destination);
    let approval = ApprovalDocument {
        protocol: 1,
        review_token: "e".repeat(64),
        scope: ApprovalScope::SelectedMigrationTest,
        destination: approved_destination,
        policy: fixture.policy.clone(),
        supplements: vec![],
        authorization: "explicit synthetic managed migration".into(),
    };
    let approval_json = serde_json::to_string(&approval)?;
    let approval_blake3 = blake3::hash(approval_json.as_bytes()).to_hex().to_string();
    let mut seal = fixture.inspection.seal.clone();
    seal.approval.document_blake3 = approval_blake3.clone();
    let run_documents = [
        (InputRole::Seal, serde_json::to_string(&seal)?),
        (InputRole::Approval, approval_json),
        (InputRole::Policy, policy_json),
    ];
    let events = Arc::new(Mutex::new(Vec::new()));
    let error = actual_rejection(
        &executable,
        &destination,
        Operation::Run {
            approval_blake3,
            max_steps: U64(1),
            max_seconds: U64(30),
            source_open_ms: U64(30_000),
            artifact_open_ms: U64(30_000),
            max_artifact_bytes: U64(1024),
        },
        &run_documents,
        "disjoint",
        budget.clone(),
        events.clone(),
    )?;
    assert!(error.contains("separate directories"), "{error}");
    assert!(!destination.exists());
    assert!(events.lock().unwrap().is_empty());

    let current =
        crate::catalog_migration::importer_tests::current_repair_tests::managed_fixture()?;
    let valid_repair_documents = [
        (
            InputRole::Seal,
            serde_json::to_string(&current.fixture.inspection.seal)?,
        ),
        (InputRole::Approval, current.approval.clone()),
        (
            InputRole::RepairRequest,
            serde_json::to_string(&current.request)?,
        ),
    ];
    let missing_repair = canonical.join("missing-repair");
    for (steps, expected) in [(0, "invalid work budget"), (1, "existing catalog")] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = actual_rejection(
            &executable,
            &missing_repair,
            Operation::RepairCurrent {
                max_steps: U64(steps),
                max_seconds: U64(30),
                source_open_ms: U64(30_000),
            },
            &valid_repair_documents,
            if steps == 0 {
                "invalid_work"
            } else {
                "missing_repair"
            },
            budget.clone(),
            events.clone(),
        )?;
        assert!(error.contains(expected), "{error}");
        assert!(!missing_repair.exists());
        assert!(events.lock().unwrap().is_empty());
    }
    let missing_supplement = canonical.join("missing-supplement");
    let supplement_events = Arc::new(Mutex::new(Vec::new()));
    let error = actual_rejection(
        &executable,
        &missing_supplement,
        Operation::PrepareSupplements,
        &[(InputRole::SupplementRequests, "[]".into())],
        "empty_supplement",
        budget.clone(),
        supplement_events.clone(),
    )?;
    assert!(error.contains("roster bound"), "{error}");
    assert!(!missing_supplement.exists());
    assert!(supplement_events.lock().unwrap().is_empty());

    let mut request = current.request.clone();
    request.expected_mapping_epoch += 1;
    {
        let db = rusqlite::Connection::open(current.fixture.destination.join("catalog.sqlite3"))?;
        db.pragma_update(None, "user_version", 7)?;
    }
    let before = fs::read(current.fixture.destination.join("catalog.sqlite3"))?;
    let repair_documents = [
        (
            InputRole::Seal,
            serde_json::to_string(&current.fixture.inspection.seal)?,
        ),
        (InputRole::Approval, current.approval),
        (InputRole::RepairRequest, serde_json::to_string(&request)?),
    ];
    let repair_events = Arc::new(Mutex::new(Vec::new()));
    let error = actual_rejection(
        &executable,
        &current.fixture.destination,
        Operation::RepairCurrent {
            max_steps: U64(1),
            max_seconds: U64(30),
            source_open_ms: U64(30_000),
        },
        &repair_documents,
        "predecessor",
        budget.clone(),
        repair_events.clone(),
    )?;
    assert!(error.contains("predecessor differs"), "{error}");
    assert_eq!(
        before,
        fs::read(current.fixture.destination.join("catalog.sqlite3"))?
    );
    assert!(
        !current
            .fixture
            .destination
            .join(".lightroom-import.lock")
            .exists()
    );
    assert!(repair_events.lock().unwrap().is_empty());

    let legacy = canonical.join("legacy-status");
    fs::create_dir(&legacy)?;
    let database = legacy.join("catalog.sqlite3");
    {
        let db = rusqlite::Connection::open(&database)?;
        db.execute_batch(&format!(
            "PRAGMA application_id=1346913089; PRAGMA user_version={}; CREATE TABLE preserved(value); INSERT INTO preserved VALUES('unchanged');",
            crate::CURRENT_SCHEMA_VERSION - 1
        ))?;
    }
    let before = fs::read(&database)?;
    let status_events = Arc::new(Mutex::new(Vec::new()));
    let error = actual_rejection(
        &executable,
        &legacy,
        Operation::Status {
            run: "a".repeat(64),
        },
        &[],
        "legacy_status",
        budget,
        status_events.clone(),
    )?;
    assert_eq!(
        error,
        "status requires a current LensWorks catalog; no schema migration was performed"
    );
    assert_eq!(before, fs::read(&database)?);
    assert!(status_events.lock().unwrap().is_empty());

    for line in fs::read_to_string(&pids)?.lines() {
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "managed child remains live"
        );
    }
    Ok(())
}

#[test]
fn lm_executor_batch3_operation_graph_denial_is_typed_before_parse_or_target_use() -> Result<()> {
    use crate::lightroom_migration_worker::memory::{core, layout::add};
    let root = tempfile::tempdir()?;
    let pids = root.path().join("pids");
    let executable = wrapper(root.path(), &pids)?;
    let destination = root.path().join("never-created");
    let document = "[]".to_string();
    let envelope = base(
        Operation::PrepareSupplements,
        vec![descriptor(InputRole::SupplementRequests, &document)],
        &destination,
    );
    let encoded = serde_json::to_string(&envelope)?;
    let required = core::worker_supplement_documents(document.len())?;
    let mut prior = managed_before_document_bytes(&executable, &encoded)?;
    prior = add(prior, document.len())?;
    let budget = MemoryBudget::new(add(prior, required - 1)?)?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut running = execute_operation(
        &executable,
        Guard {
            session: "batch3-pressure".into(),
            generation: "typed".into(),
            operation: "typed".into(),
        },
        &encoded,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(60),
        Parent {
            root: destination.clone(),
            writers: None,
            events: events.clone(),
        },
        budget.clone(),
        &[InputPart {
            role: InputRole::SupplementRequests,
            text: &document,
        }],
        envelope.operation.result_maximum()?,
    );
    drive(&mut running)?;
    let Drained::Failed(failure) = running.retry_drain().context("denial did not drain")? else {
        anyhow::bail!("one-byte-short operation graph completed")
    };
    let FailureCause::ResourceLimit(limit) = &failure.cause else {
        anyhow::bail!("operation graph denial lost typed cause: {failure:?}")
    };
    assert_eq!((limit.required, limit.available), (required, required - 1));
    assert!(!destination.exists());
    assert!(events.lock().unwrap().is_empty());
    drop(running);
    assert_eq!(budget.used(), 0);

    let status_destination = root.path().join("status-never-opened");
    let status_envelope = base(
        Operation::Status {
            run: "a".repeat(64),
        },
        vec![],
        &status_destination,
    );
    let status_encoded = serde_json::to_string(&status_envelope)?;
    let status_required = core::worker_status()?;
    let status_prior = managed_before_document_bytes(&executable, &status_encoded)?;
    let status_budget = MemoryBudget::new(add(status_prior, status_required - 1)?)?;
    let mut status = execute_operation(
        &executable,
        Guard {
            session: "batch3-pressure".into(),
            generation: "status".into(),
            operation: "status".into(),
        },
        &status_encoded,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(60),
        Parent {
            root: status_destination.clone(),
            writers: None,
            events: Arc::new(Mutex::new(Vec::new())),
        },
        status_budget.clone(),
        &[],
        status_envelope.operation.result_maximum()?,
    );
    drive(&mut status)?;
    let Drained::Failed(failure) = status
        .retry_drain()
        .context("status denial did not drain")?
    else {
        anyhow::bail!("one-byte-short status graph completed")
    };
    let FailureCause::ResourceLimit(limit) = &failure.cause else {
        anyhow::bail!("status graph denial lost typed cause: {failure:?}")
    };
    assert_eq!(
        (limit.required, limit.available),
        (status_required, status_required - 1)
    );
    assert!(!status_destination.exists());
    drop(status);
    assert_eq!(status_budget.used(), 0);
    for line in fs::read_to_string(&pids)?.lines() {
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    Ok(())
}

#[test]
fn lm_executor_batch3_supplement_phase_denial_retries_same_pool_without_early_write() -> Result<()>
{
    use crate::lightroom_migration_worker::memory::{core, layout::add};
    let root = tempfile::tempdir()?;
    let pids = root.path().join("pids");
    let executable = wrapper(root.path(), &pids)?;
    let supplement = crate::catalog_migration::supplements::tests::Fixture::new()?;
    let destination = root.path().canonicalize()?.join("supplement-target");
    drop(Catalog::open(&destination)?);
    let document = serde_json::to_string(&vec![supplement.request.clone()])?;
    let envelope = base(
        Operation::PrepareSupplements,
        vec![descriptor(InputRole::SupplementRequests, &document)],
        &destination,
    );
    let encoded = serde_json::to_string(&envelope)?;
    let initial = core::worker_supplement_documents(document.len())?;
    let decoded: Vec<supplements::Request> = serde_json::from_str(&document)?;
    let phase = add(
        supplement_requests_retained(&decoded)?,
        core::worker_supplement_execution(decoded.len())?,
    )?;
    ensure!(
        phase > initial,
        "supplement fixture must exercise phase growth"
    );
    let prior = add(
        managed_before_document_bytes(&executable, &encoded)?,
        document.len(),
    )?;
    let result_room = envelope.operation.result_maximum()?;
    let budget = MemoryBudget::new(add(add(prior, phase)?, result_room)?)?;
    let mut competing = budget.reservation();
    competing.grow(add(result_room, 1)?)?;
    let before = fs::read(destination.join("catalog.sqlite3"))?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut running = execute_operation(
        &executable,
        Guard {
            session: "batch3-pressure".into(),
            generation: "supplement".into(),
            operation: "supplement".into(),
        },
        &encoded,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(60),
        Parent {
            root: destination.clone(),
            writers: None,
            events: events.clone(),
        },
        budget.clone(),
        &[InputPart {
            role: InputRole::SupplementRequests,
            text: &document,
        }],
        envelope.operation.result_maximum()?,
    );
    drive(&mut running)?;
    let Drained::Failed(failure) = running.retry_drain().context("denial did not drain")? else {
        anyhow::bail!("one-byte-short supplement phase completed")
    };
    let FailureCause::ResourceLimit(limit) = &failure.cause else {
        anyhow::bail!("supplement phase denial lost typed cause: {failure:?}")
    };
    assert_eq!(
        (limit.required, limit.available),
        (phase - initial, phase - initial - 1)
    );
    assert_eq!(before, fs::read(destination.join("catalog.sqlite3"))?);
    assert!(!destination.join(".lightroom-import.lock").exists());
    assert!(events.lock().unwrap().is_empty());
    drop(running);
    assert_eq!(budget.used(), add(result_room, 1)?);
    drop(competing);
    assert_eq!(budget.used(), 0);
    let retry_events = Arc::new(Mutex::new(Vec::new()));
    let retry = actual_operation(
        &executable,
        &destination,
        Operation::PrepareSupplements,
        &[(InputRole::SupplementRequests, document)],
        "supplement_retry",
        budget.clone(),
        retry_events,
    )?;
    assert_eq!(retry.as_array().map(Vec::len), Some(1));
    assert_eq!(budget.used(), 0);
    for line in fs::read_to_string(&pids)?.lines() {
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    Ok(())
}

#[test]
fn lm_executor_batch3_managed_destination_alias_refuses_with_full_failure_detail() -> Result<()> {
    let root = tempfile::tempdir()?;
    let pids = root.path().join("pids");
    let executable = wrapper(root.path(), &pids)?;
    let canonical = root.path().canonicalize()?;
    let target = canonical.join("target");
    drop(Catalog::open(&target)?);
    let alias = canonical.join("target-alias");
    std::os::unix::fs::symlink(&target, &alias)?;
    // The shared resolver admits aliases for custody comparisons. The managed
    // filesystem audit still refuses the original envelope path before writing.
    assert_eq!(lightroom_executor::destination_path(&alias)?, target);
    let fixture = crate::catalog_migration::supplements::tests::Fixture::new()?;
    let before = fs::read(target.join("catalog.sqlite3"))?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let budget = test_budget()?;
    let error = actual_operation(
        &executable,
        &alias,
        Operation::PrepareSupplements,
        &[(
            InputRole::SupplementRequests,
            serde_json::to_string(&vec![fixture.request])?,
        )],
        "destination_alias",
        budget.clone(),
        events.clone(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("cause: Rejected("), "{error}");
    assert!(error.contains("source path contains a link:"), "{error}");
    assert!(error.contains("target-alias"), "{error}");
    assert!(error.contains("poisoned: true"), "{error}");
    assert!(error.contains("outcome_unknown: false"), "{error}");
    assert_eq!(before, fs::read(target.join("catalog.sqlite3"))?);
    assert!(!target.join(".lightroom-import.lock").exists());
    assert!(events.lock().unwrap().is_empty());
    for line in fs::read_to_string(&pids)?.lines() {
        assert!(line.ends_with("--lightroom-migration-worker"), "{line}");
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn lm_executor_batch3_first_use_aliases_refuse_and_missing_canonical_destination_bootstraps()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let canonical = root.path().canonicalize()?;
    let pids = canonical.join("pids");
    let executable = wrapper(&canonical, &pids)?;
    let fixture = crate::catalog_migration::supplements::tests::Fixture::new()?;
    let documents = [(
        InputRole::SupplementRequests,
        serde_json::to_string(std::slice::from_ref(&fixture.request))?,
    )];
    let budget = test_budget()?;
    for missing_suffix in [false, true] {
        let label = if missing_suffix { "missing" } else { "empty" };
        let target = canonical.join(format!("{label}-target"));
        fs::create_dir(&target)?;
        let alias = canonical.join(format!("{label}-alias"));
        std::os::unix::fs::symlink(&target, &alias)?;
        let destination = if missing_suffix {
            alias.join("new/catalog")
        } else {
            alias.clone()
        };
        let resolved = if missing_suffix {
            target.join("new/catalog")
        } else {
            target.clone()
        };
        assert_eq!(
            lightroom_executor::destination_path(&destination)?,
            resolved
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = actual_operation(
            &executable,
            &destination,
            Operation::PrepareSupplements,
            &documents,
            label,
            budget.clone(),
            events.clone(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cause: Rejected("), "{error}");
        assert!(error.contains("source path contains a link:"), "{error}");
        assert!(error.contains(&format!("{label}-alias")), "{error}");
        assert!(error.contains("poisoned: true"), "{error}");
        assert!(error.contains("outcome_unknown: false"), "{error}");
        assert!(
            events.lock().unwrap().is_empty(),
            "Bootstrap/write before refusal"
        );
        for name in ["catalog.sqlite3", "previews", ".lightroom-import.lock"] {
            assert!(!resolved.join(name).exists(), "created {name}");
        }
        assert_eq!(fs::read_dir(&target)?.count(), 0, "target contents changed");
        assert_eq!(budget.used(), 0);
    }

    // Genuinely missing suffixes under a canonical directory must still work.
    let destination = canonical.join("fresh/nested/catalog");
    assert!(!canonical.join("fresh").exists());
    let events = Arc::new(Mutex::new(Vec::new()));
    let result = actual_operation(
        &executable,
        &destination,
        Operation::PrepareSupplements,
        &documents,
        "canonical_first_use",
        budget.clone(),
        events.clone(),
    )?;
    assert_eq!(result.as_array().map(Vec::len), Some(1));
    assert_supplement_fixture_evidence(&destination, &result, &fixture.request)?;
    assert_eq!(
        events.lock().unwrap().first().map(String::as_str),
        Some("bootstrap")
    );
    assert!(destination.join("catalog.sqlite3").is_file());
    assert!(destination.join("previews").is_dir());
    assert!(destination.join(".lightroom-import.lock").is_file());
    assert_eq!(budget.used(), 0);

    // lstat must preserve an existing dangling final link, and a non-directory
    // ancestor must never be treated as a merely missing destination suffix.
    let broken = canonical.join("broken-alias");
    std::os::unix::fs::symlink(canonical.join("absent"), &broken)?;
    let file = canonical.join("file");
    fs::write(&file, b"unchanged")?;
    for path in [broken.clone(), file.clone(), file.join("child")] {
        let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
        assert!(check_destination_path(&path, &audit).is_err());
        assert!(audit.is_poisoned());
    }
    assert!(fs::symlink_metadata(&broken)?.file_type().is_symlink());
    assert!(!canonical.join("absent").exists());
    assert_eq!(fs::read(&file)?, b"unchanged");

    let lines = fs::read_to_string(&pids)?;
    assert_eq!(lines.lines().count(), 3);
    for line in lines.lines() {
        assert!(line.ends_with("--lightroom-migration-worker"), "{line}");
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    Ok(())
}

#[test]
fn lm_executor_batch3_prepared_errors_are_bounded_rejections_before_source() -> Result<()> {
    let root = tempfile::tempdir()?;
    let pids = root.path().join("pids");
    let executable = wrapper(root.path(), &pids)?;
    let budget = test_budget()?;
    let destination = root.path().join("unopened");
    let events = Arc::new(Mutex::new(Vec::new()));
    let error = actual_rejection(
        &executable,
        &destination,
        Operation::PrepareSupplements,
        &[(InputRole::SupplementRequests, "[".into())],
        "bad_json",
        budget.clone(),
        events.clone(),
    )?;
    assert!(error.contains("EOF"), "{error}");
    assert!(!destination.exists());

    let fixture =
        crate::catalog_migration::importer_tests::current_repair_tests::managed_fixture()?;
    let before = fs::read(fixture.fixture.destination.join("catalog.sqlite3"))?;
    let error = actual_rejection(
        &executable,
        &fixture.fixture.destination,
        Operation::RepairCurrent {
            max_steps: U64(1000),
            max_seconds: U64(120),
            source_open_ms: U64(30_000),
        },
        &[
            (
                InputRole::Seal,
                serde_json::to_string(&fixture.fixture.inspection.seal)?,
            ),
            (InputRole::Approval, "different approved bytes".into()),
            (
                InputRole::RepairRequest,
                serde_json::to_string(&fixture.request)?,
            ),
        ],
        "wrong_approval",
        budget.clone(),
        events.clone(),
    )?;
    assert_eq!(error, "authorization bytes differ from seal");
    assert_eq!(
        before,
        fs::read(fixture.fixture.destination.join("catalog.sqlite3"))?
    );
    assert!(events.lock().unwrap().is_empty());
    for line in fs::read_to_string(&pids)?.lines() {
        assert!(
            line.ends_with("--lightroom-migration-worker"),
            "unexpected Source: {line}"
        );
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn lm_executor_batch3_unpinned_live_schema_eleven_upgrades_with_existing_lock() -> Result<()> {
    let root = tempfile::tempdir()?;
    let pids = root.path().join("pids");
    let executable = wrapper(root.path(), &pids)?;
    let fixture = crate::catalog_migration::supplements::tests::Fixture::new()?;
    let destination = root.path().canonicalize()?.join("legacy");
    let catalog = Catalog::open(&destination)?;
    // Reverse migrations after schema 11 on an empty catalog. This is the actual
    // schema-11 shape, so Bootstrap must execute the real migration successfully.
    catalog.db.execute_batch(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/relink-v12-downgrade.sql"
    )))?;
    catalog.db.pragma_update(None, "user_version", 11)?;
    assert_eq!(
        catalog
            .db
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
        11
    );
    drop(catalog);
    let lock = destination.join(".lightroom-import.lock");
    fs::write(&lock, b"existing import lock")?;
    use std::os::unix::fs::MetadataExt;
    let lock_identity = fs::metadata(&lock)?.ino();
    let events = Arc::new(Mutex::new(Vec::new()));
    let budget = test_budget()?;
    let result = actual_operation(
        &executable,
        &destination,
        Operation::PrepareSupplements,
        &[(
            InputRole::SupplementRequests,
            serde_json::to_string(std::slice::from_ref(&fixture.request))?,
        )],
        "live_upgrade",
        budget.clone(),
        events.clone(),
    )?;
    assert_eq!(result.as_array().map(Vec::len), Some(1));
    assert_supplement_fixture_evidence(&destination, &result, &fixture.request)?;
    assert_eq!(
        events.lock().unwrap().first().map(String::as_str),
        Some("upgrade")
    );
    assert_eq!(fs::metadata(&lock)?.ino(), lock_identity);
    assert_eq!(fs::read(&lock)?, b"existing import lock");
    let db = rusqlite::Connection::open_with_flags(
        destination.join("catalog.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
        crate::CURRENT_SCHEMA_VERSION
    );
    assert_eq!(db.query_row("SELECT count(*) FROM pragma_table_info('storage_plans') WHERE name IN ('revision','review_token','rules')", [], |r| r.get::<_, i64>(0))?, 3);
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn lm_executor_batch3_repair_execution_denial_retries_same_pool_before_stored_state() -> Result<()>
{
    use crate::lightroom_migration_worker::memory::{core, layout::add};
    let root = tempfile::tempdir()?;
    let pids = root.path().join("pids");
    let executable = wrapper(root.path(), &pids)?;
    let fixture =
        crate::catalog_migration::importer_tests::current_repair_tests::managed_fixture()?;
    let destination = &fixture.fixture.destination;
    let documents = vec![
        (
            InputRole::Seal,
            serde_json::to_string(&fixture.fixture.inspection.seal)?,
        ),
        (InputRole::Approval, fixture.approval.clone()),
        (
            InputRole::RepairRequest,
            serde_json::to_string(&fixture.request)?,
        ),
    ];
    let operation = Operation::RepairCurrent {
        max_steps: U64(1000),
        max_seconds: U64(120),
        source_open_ms: U64(30_000),
    };
    let envelope = base(
        operation.clone(),
        documents.iter().map(|(r, s)| descriptor(*r, s)).collect(),
        destination,
    );
    let encoded = serde_json::to_string(&envelope)?;
    let raw = documents
        .iter()
        .try_fold(0usize, |a, (_, s)| add(a, s.len()))?;
    let initial = core::worker_repair_documents(documents[0].1.len(), documents[2].1.len())?;
    let prior = add(
        add(managed_before_document_bytes(&executable, &encoded)?, raw)?,
        initial,
    )?;
    let repair = core::worker_repair_execution()?;
    let budget = test_budget()?;
    let concurrent = budget
        .snapshot()?
        .limit
        .checked_sub(add(prior, repair)? - 1)
        .context("repair fixture allowance")?;
    let mut competitor = budget.reservation();
    competitor.grow(concurrent)?;
    let before = fs::read(destination.join("catalog.sqlite3"))?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let parts: Vec<_> = documents
        .iter()
        .map(|(role, text)| InputPart { role: *role, text })
        .collect();
    let mut running = execute_operation(
        &executable,
        Guard {
            session: "repair-pressure".into(),
            generation: "denied".into(),
            operation: "current".into(),
        },
        &encoded,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(60),
        Parent {
            root: destination.clone(),
            writers: None,
            events: events.clone(),
        },
        budget.clone(),
        &parts,
        envelope.operation.result_maximum()?,
    );
    drive(&mut running)?;
    let Drained::Failed(failure) = running
        .retry_drain()
        .context("repair denial did not drain")?
    else {
        anyhow::bail!("unadmitted repair completed")
    };
    let FailureCause::ResourceLimit(limit) = &failure.cause else {
        anyhow::bail!("repair denial lost typed cause: {failure:?}")
    };
    assert_eq!((limit.required, limit.available), (repair, repair - 1));
    assert_eq!(before, fs::read(destination.join("catalog.sqlite3"))?);
    assert!(events.lock().unwrap().is_empty());
    for line in fs::read_to_string(&pids)?.lines() {
        assert!(line.ends_with("--lightroom-migration-worker"));
    }
    drop(running);
    assert_eq!(budget.used(), concurrent);
    drop(competitor);
    assert_eq!(budget.used(), 0);
    let result = actual_operation(
        &executable,
        destination,
        operation,
        &documents,
        "repair_retry",
        budget.clone(),
        events.clone(),
    )?;
    assert_eq!(result["repair"]["complete"], true);
    assert!(result["repair"]["repaired"].as_u64().unwrap_or(0) > 0);
    assert!(events.lock().unwrap().iter().any(|event| event == "lock"));
    for line in fs::read_to_string(&pids)?.lines() {
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    assert_eq!(budget.used(), 0);
    Ok(())
}

const ADOBE_PROBE: &str = "PHOTOCATALOG_LM_BATCH3_ADOBE_PROBE";
pub(crate) fn install_adobe_observer(
    output: Arc<dyn Publish>,
    guard: Guard,
) -> Result<Option<crate::catalog_migration::repair_memory::AdobeObserver>> {
    if std::env::var_os(ADOBE_PROBE).is_none() {
        return Ok(None);
    }
    Ok(Some(
        crate::catalog_migration::repair_memory::install_adobe_observer(Box::new(
            move |admitted, delta, bytes| {
                output.publish(&ChildFrame::Progress {
                    guard: guard.clone(),
                    phase: if admitted {
                        "test_adobe_admitted"
                    } else {
                        "test_adobe_before"
                    }
                    .into(),
                    completed: U64(delta as u64),
                    total: Some(U64(bytes as u64)),
                })
            },
        ))?,
    ))
}

type AdobeCheckpoint = Arc<Mutex<Option<(Vec<u8>, i64)>>>;

struct AdobePressure {
    parent: Parent,
    budget: MemoryBudget,
    compete: bool,
    held: Arc<Mutex<Option<super::super::super::memory::Reservation>>>,
    observed: Arc<Mutex<Option<(usize, usize)>>>,
    checkpoint: AdobeCheckpoint,
}
impl Admission for AdobePressure {
    fn lock(&mut self, target: &str, destination: &DestinationPin, lock: &FileKey) -> Result<()> {
        self.parent.lock(target, destination, lock)
    }
    fn writer(
        &mut self,
        sequence: u64,
        kind: WriteKind,
        target: &str,
        lock: Option<&FileKey>,
        stop: &Stop,
        until: Instant,
    ) -> Result<Arc<Writers>> {
        self.parent
            .writer(sequence, kind, target, lock, stop, until)
    }
    fn release(&mut self, sequence: u64, kind: WriteKind) -> Result<()> {
        self.parent.release(sequence, kind)
    }
    fn progress(&mut self, phase: &str, completed: u64, total: Option<u64>) -> Result<()> {
        self.parent.progress(phase, completed, total)?;
        if self.compete
            && phase == "test_adobe_before"
            && completed > 0
            && self.held.lock().unwrap().is_none()
        {
            let delta = usize::try_from(completed)?;
            let available = self.budget.snapshot()?.available;
            ensure!(
                available >= delta,
                "fixture pool cannot admit Adobe without contention"
            );
            let mut reservation = self.budget.reservation();
            reservation.grow(available - (delta - 1))?;
            *self.held.lock().unwrap() = Some(reservation);
            *self.observed.lock().unwrap() = Some((
                delta,
                usize::try_from(total.context("Adobe input length absent")?)?,
            ));
            *self.checkpoint.lock().unwrap() = Some(current_checkpoint(&self.parent.root)?);
        }
        Ok(())
    }
}
fn current_checkpoint(destination: &Path) -> Result<(Vec<u8>, i64)> {
    let db = rusqlite::Connection::open_with_flags(
        destination.join("catalog.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    Ok((
        db.query_row("SELECT progress FROM migration_runs", [], |r| r.get(0))?,
        db.query_row(
            "SELECT count(*) FROM migration_metadata WHERE slot='current_develop'",
            [],
            |r| r.get(0),
        )?,
    ))
}

#[test]
fn lm_executor_batch3_actual_run_adobe_denial_precedes_parser_and_retries_same_pool() -> Result<()>
{
    let (fixture, payload_bytes) = ImportFixture::with_adobe_expansion()?;
    let root = tempfile::tempdir()?;
    let pids = root.path().join("pids");
    let executable = wrapper(root.path(), &pids)?;
    let body = fs::read_to_string(&executable)?
        .replace("exec env ", &format!("exec env {ADOBE_PROBE}=1 "));
    fs::write(&executable, body)?;
    let approval = ApprovalDocument {
        protocol: 1,
        review_token: "e".repeat(64),
        scope: ApprovalScope::SelectedMigrationTest,
        destination: NativePath::from_path(&fixture.destination),
        policy: fixture.policy.clone(),
        supplements: vec![],
        authorization: "explicit synthetic Adobe admission test".into(),
    };
    let approval = serde_json::to_string(&approval)?;
    let digest = blake3::hash(approval.as_bytes()).to_hex().to_string();
    let mut seal = fixture.inspection.seal.clone();
    seal.approval.document_blake3 = digest.clone();
    let documents = vec![
        (InputRole::Seal, serde_json::to_string(&seal)?),
        (InputRole::Approval, approval),
        (InputRole::Policy, serde_json::to_string(&fixture.policy)?),
    ];
    let operation = Operation::Run {
        approval_blake3: digest,
        max_steps: U64(4000),
        max_seconds: U64(120),
        source_open_ms: U64(30_000),
        artifact_open_ms: U64(ImportFixture::limits().open_deadline_ms),
        max_artifact_bytes: U64(ImportFixture::limits().maximum_bytes),
    };
    let envelope = base(
        operation.clone(),
        documents
            .iter()
            .map(|(role, text)| descriptor(*role, text))
            .collect(),
        &fixture.destination,
    );
    let encoded = serde_json::to_string(&envelope)?;
    let parts: Vec<_> = documents
        .iter()
        .map(|(role, text)| InputPart { role: *role, text })
        .collect();
    let budget = test_budget()?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let held = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(None));
    let checkpoint = Arc::new(Mutex::new(None));
    let mut running = execute_operation(
        &executable,
        Guard {
            session: "adobe-pressure".into(),
            generation: "deny".into(),
            operation: "run".into(),
        },
        &encoded,
        Arc::new(Stop::default()),
        Instant::now() + Duration::from_secs(180),
        AdobePressure {
            parent: Parent {
                root: fixture.destination.clone(),
                writers: None,
                events: events.clone(),
            },
            budget: budget.clone(),
            compete: true,
            held: held.clone(),
            observed: observed.clone(),
            checkpoint: checkpoint.clone(),
        },
        budget.clone(),
        &parts,
        envelope.operation.result_maximum()?,
    );
    drive(&mut running)?;
    let Drained::Failed(failure) = running
        .retry_drain()
        .context("Adobe denial did not drain")?
    else {
        anyhow::bail!("unadmitted Adobe parser completed")
    };
    let FailureCause::ResourceLimit(limit) = &failure.cause else {
        anyhow::bail!("Adobe denial lost typed cause: {failure:?}")
    };
    let (delta, bytes) = observed
        .lock()
        .unwrap()
        .context("Run did not reach positive Adobe admission")?;
    assert_eq!(bytes, payload_bytes);
    assert_eq!((limit.required, limit.available), (delta, delta - 1));
    let transcript = events.lock().unwrap();
    assert_eq!(
        transcript
            .iter()
            .filter(|v| v.starts_with("progress:test_adobe_before:"))
            .count(),
        transcript
            .iter()
            .filter(|v| v.starts_with("progress:test_adobe_admitted:"))
            .count()
            + 1
    );
    drop(transcript);
    assert_eq!(
        current_checkpoint(&fixture.destination)?,
        checkpoint.lock().unwrap().as_ref().unwrap().clone()
    );
    assert_eq!(current_checkpoint(&fixture.destination)?.1, 0);
    drop(running);
    assert!(budget.used() > 0);
    held.lock().unwrap().take();
    assert_eq!(budget.used(), 0);
    let retry_events = Arc::new(Mutex::new(Vec::new()));
    let result = actual_operation(
        &executable,
        &fixture.destination,
        operation,
        &documents,
        "adobe_retry",
        budget.clone(),
        retry_events.clone(),
    )?;
    assert_eq!(result["status"], "complete");
    assert!(
        retry_events
            .lock()
            .unwrap()
            .iter()
            .any(|v| v.starts_with("progress:test_adobe_admitted:"))
    );
    assert_eq!(current_checkpoint(&fixture.destination)?.1, 6);
    for line in fs::read_to_string(&pids)?.lines() {
        let pid: i32 = line.split_whitespace().next().unwrap().parse()?;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    assert_eq!(budget.used(), 0);
    Ok(())
}

#[test]
fn lm_executor_batch3_repair_preflight_replacement_refuses_before_bootstrap() -> Result<()> {
    let fixture =
        crate::catalog_migration::importer_tests::keyword_repair_tests::managed_fixture()?;
    let input = fixture.fixture.inspection.seal.binding_blake3()?;
    let original = fixture.fixture.open()?;
    let progress: Vec<u8> = original.db.query_row(
        "SELECT progress FROM migration_runs WHERE id=?",
        [&fixture.request.run],
        |r| r.get(0),
    )?;
    let current = crate::catalog_migration::current_repair::Request {
        run: fixture.request.run.clone(),
        expected_complete_progress_blake3: blake3::hash(&progress).to_hex().to_string(),
        expected_mapping_epoch: original.db.query_row(
            "SELECT epoch FROM migration_mapping_epoch WHERE id=1",
            [],
            |r| r.get(0),
        )?,
        reason: "identity replacement test".into(),
    };
    drop(original);
    for schema in [7, 9] {
        let root = tempfile::tempdir()?;
        let destination = fs::canonicalize(root.path())?;
        let database = destination.join("catalog.sqlite3");
        fs::copy(
            fixture.fixture.destination.join("catalog.sqlite3"),
            &database,
        )?;
        let db = rusqlite::Connection::open(&database)?;
        db.pragma_update(None, "user_version", schema)?;
        drop(db);
        let prepared = if schema == 7 {
            PreparedOperation::Current(fixture.fixture.inspection.seal.clone(), current.clone())
        } else {
            PreparedOperation::Keywords(
                fixture.fixture.inspection.seal.clone(),
                fixture.request.clone(),
            )
        };
        let operation = if schema == 7 {
            Operation::RepairCurrent {
                max_steps: U64(1),
                max_seconds: U64(1),
                source_open_ms: U64(1000),
            }
        } else {
            Operation::RepairKeywords {
                max_steps: U64(1),
                max_seconds: U64(1),
                source_open_ms: U64(1000),
            }
        };
        let envelope = base(operation, vec![], &destination);
        assert!(envelope.expected_destination.is_none());
        let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
        let observed = review_destination(&envelope, &audit)?;
        preflight_destination(&prepared, Some(&input), observed.as_ref())?;
        let replacement = destination.join("replacement.sqlite3");
        let db = rusqlite::Connection::open(&replacement)?;
        db.execute_batch("PRAGMA application_id=1346913089; CREATE TABLE marker(value TEXT); INSERT INTO marker VALUES('replacement must remain untouched');")?;
        db.pragma_update(None, "user_version", schema)?;
        drop(db);
        let before = fs::read(&replacement)?;
        fs::rename(&database, destination.join("preflight.sqlite3"))?;
        fs::rename(&replacement, &database)?;
        let guard = Guard {
            session: "replacement".into(),
            generation: format!("schema{schema}"),
            operation: "repair".into(),
        };
        let controls = Controls::new(
            guard,
            audit.clone(),
            Instant::now() + Duration::from_secs(1),
        )?;
        let frames = Arc::new(Mutex::new(Vec::<u8>::new()));
        let error =
            prepare_destination(&envelope, &audit, observed, controls, frames.clone()).unwrap_err();
        assert!(
            format!("{error:#}").contains("helper path names a different held object"),
            "{error:#}"
        );
        assert!(
            frames.lock().unwrap().is_empty(),
            "replacement requested write authority"
        );
        assert_eq!(before, fs::read(&database)?);
        assert!(!destination.join(".lightroom-import.lock").exists());
        let db = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
            schema
        );
    }
    Ok(())
}
