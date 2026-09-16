//! Root-owned disposable qualification of the installed managed catalog route.
//!
//! This is ignored because it consumes a previously verified portable working
//! set and an exact separately built executable. It never fabricates a worker
//! protocol: every operation enters through `DesktopBridge::submit`.

use anyhow::{Context, Result, bail, ensure};
use photocatalog::{
    application::{
        Config, ImportPhase, Limits, Phase, Reply, Request, Response, U64,
        desktop::{DesktopBridge, TransportPhase},
        lightroom as app_lightroom, lightroom_bridge as wb, lightroom_migration as migration,
        metadata, organization,
    },
    catalog_migration::importer::{KeywordOverlap, OverlapPolicy},
    filesystem_worker::wire::{
        LightroomArtifactPreparation, LightroomArtifactPreparationReply, LightroomSealedDocument,
        LightroomSealedRead,
    },
    lightroom::{
        self,
        plan::{FamilyReport, Report as InspectionReport},
        selection::{FamilyDecision, SelectionRequest},
    },
    preview::{PreviewPolicy, ServiceLimits},
    storage_volume::NativePath,
};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const BASELINE_MANIFEST_SHA256: &str =
    "2dcba80e52f0c2698766be5c67a0af8ff582de3d8f06c7423a48e462a5870b3b";
const BUILDER_COMMIT: &str = "f8e11c7ff597884e2c0d6a8642f9ef5cee65c9fe";
const SELECTED: [&str; 2] = ["2014-v13-2.lrcat", "2015-v13.lrcat"];
const EXCLUDED: [&str; 2] = ["2014-v13.lrcat", "2015-v13-3.lrcat"];
const DEADLINE: Duration = Duration::from_secs(300);
const INSPECTION_COMPLETE: &str = "inspection_complete_with_reported_gaps";

#[derive(Deserialize)]
struct WorkingManifest {
    format_version: u32,
    working_set_id: String,
    baseline: Baseline,
    bound_originals_root: String,
    source_reconciliation: Value,
    readiness: BTreeMap<String, bool>,
}

#[derive(Deserialize)]
struct Baseline {
    pack_id: String,
    manifest_sha256: String,
}

#[derive(Clone)]
struct ExactPart {
    role: migration::InputRole,
    text: String,
    blake3: String,
}

#[derive(Serialize)]
struct Counts {
    assets: u64,
    variants: u64,
    xmp_sources: u64,
    virtual_copies: u64,
    collections: u64,
    excluded_catalog_rows_imported: u64,
}

struct PublicCounts {
    assets: u64,
    variants: u64,
    xmp_sources: u64,
    collections: u64,
}

struct Harness {
    bridge: DesktopBridge,
    output: PathBuf,
    catalog: PathBuf,
    working: PathBuf,
    evidence: Vec<String>,
}

fn env_absolute(name: &str) -> Result<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name).with_context(|| format!("{name} required"))?);
    ensure!(path.is_absolute(), "{name} must be absolute");
    Ok(path)
}

fn hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes * 2 && value.bytes().all(|v| v.is_ascii_hexdigit())
}

fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take((maximum + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= maximum,
        "{} exceeds byte limit",
        path.display()
    );
    Ok(bytes)
}

fn reject_symlinks(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(
                !kind.is_symlink(),
                "working set contains symlink: {}",
                entry.path().display()
            );
            if kind.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

fn admitted_inputs() -> Result<(PathBuf, PathBuf, PathBuf, WorkingManifest, String, Value)> {
    let executable = env_absolute("PHOTOCATALOG_TEST_EXECUTABLE")?.canonicalize()?;
    ensure!(executable.is_file(), "configured executable is not a file");
    let working = env_absolute("PHOTOCATALOG_VALIDATION_WORKING_SET")?.canonicalize()?;
    ensure!(working.is_dir(), "working set is not a directory");
    ensure!(
        !working.starts_with("/Volumes"),
        "working set under /Volumes is forbidden"
    );
    ensure!(
        read_bounded(&working.join(".lensworks-validation-working-copy"), 16)? == b"1\n",
        "working marker absent or changed"
    );
    reject_symlinks(&working)?;
    let manifest_path = working.join("working-manifest.json");
    let manifest_bytes = read_bounded(&manifest_path, 1024 * 1024)?;
    let manifest: WorkingManifest = serde_json::from_slice(&manifest_bytes)?;
    ensure!(manifest.format_version == 1, "working manifest version");
    ensure!(
        manifest.working_set_id == "lensworks-cross-platform-working-v1",
        "working set identity"
    );
    ensure!(
        manifest.baseline.pack_id == "lensworks-cross-platform-disposable-v1",
        "baseline identity"
    );
    ensure!(
        manifest.baseline.manifest_sha256 == BASELINE_MANIFEST_SHA256,
        "baseline digest differs"
    );
    ensure!(
        manifest.readiness.get("destination_working_copy_prepared") == Some(&true),
        "working copy not prepared"
    );
    ensure!(
        manifest.readiness.get("all_source_sha256_recorded") == Some(&true),
        "source hashes not recorded"
    );
    let expected_root = working.join("inputs/originals").canonicalize()?;
    ensure!(
        !expected_root.starts_with("/Volumes"),
        "originals under /Volumes are forbidden"
    );
    let catalog_root = working.join("inputs/lightroom-catalogs").canonicalize()?;
    let observed_catalogs = fs::read_dir(&catalog_root)?
        .map(|entry| Ok(entry?.path()))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("lrcat"))
        .map(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
                .context("fixture catalog filename is not UTF-8")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let expected_catalogs = SELECTED
        .into_iter()
        .chain(EXCLUDED)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    ensure!(
        observed_catalogs == expected_catalogs,
        "working set contains a different Lightroom catalog roster"
    );
    let bound = PathBuf::from(&manifest.bound_originals_root).canonicalize()?;
    ensure!(bound == expected_root, "working originals binding differs");
    let checksum = String::from_utf8(read_bounded(&working.join("working-manifest.sha256"), 256)?)?;
    let fields = checksum.split_whitespace().collect::<Vec<_>>();
    ensure!(
        fields.len() == 2 && hex(fields[0], 32) && fields[1] == "working-manifest.json",
        "working manifest checksum receipt"
    );
    // The builder's --verify-working-copy command is the checksum authority. The
    // harness records its sealed digest and refuses any other manifest identity.
    let output = env_absolute("PHOTOCATALOG_VALIDATION_OUTPUT")?;
    ensure!(!output.exists(), "qualification output must not exist");
    ensure!(
        !output.starts_with("/Volumes"),
        "qualification output under /Volumes is forbidden"
    );
    let parent = output
        .parent()
        .context("qualification output has no parent")?
        .canonicalize()?;
    ensure!(
        parent == working.parent().context("working set has no parent")?,
        "working set and fresh output must share the authorized evidence parent"
    );
    fs::create_dir(&output)?;
    let commit =
        std::env::var("PHOTOCATALOG_TEST_COMMIT").context("PHOTOCATALOG_TEST_COMMIT required")?;
    ensure!(
        hex(&commit, 20),
        "tested product commit must be 40 hexadecimal digits"
    );
    let executable_blake3 = blake3::hash(&read_bounded(&executable, 1024 * 1024 * 1024)?)
        .to_hex()
        .to_string();
    let executable_identity = json!({
        "path": executable.display().to_string(),
        "bytes": fs::metadata(&executable)?.len(),
        "blake3": executable_blake3,
        "tested_product_commit": commit,
        "os": std::env::consts::OS,
        "architecture": std::env::consts::ARCH
    });
    Ok((
        executable,
        working,
        output,
        manifest,
        fields[0].into(),
        executable_identity,
    ))
}

fn app_call(bridge: &DesktopBridge, request: Request) -> Result<Response> {
    let label = format!("{request:?}");
    match bridge
        .submit(request)
        .with_context(|| format!("submit {label}"))?
        .recv()
    {
        Reply::Ok { value } => Ok(value),
        Reply::Error { error } => Err(anyhow::Error::from(error).context(label)),
    }
}

fn app_status(bridge: &DesktopBridge) -> Result<photocatalog::application::Status> {
    match app_call(bridge, Request::Status)? {
        Response::Status(status) => Ok(status),
        _ => bail!("unexpected application status response"),
    }
}

fn wait_ready(bridge: &DesktopBridge) -> Result<String> {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let status = app_status(bridge)?;
        if matches!(status.phase, Phase::Ready) {
            return status.catalog.context("ready catalog token absent");
        }
        ensure!(
            !matches!(status.phase, Phase::Failed),
            "catalog failed: {:?}",
            status.message
        );
        ensure!(
            Instant::now() < deadline,
            "catalog ready timeout: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_import(
    bridge: &DesktopBridge,
    catalog: &str,
) -> Result<photocatalog::application::ImportStatus> {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let Response::Import(Some(status)) = app_call(
            bridge,
            Request::ImportStatus {
                catalog: catalog.into(),
            },
        )?
        else {
            bail!("import status disappeared")
        };
        if matches!(
            status.phase,
            ImportPhase::Complete | ImportPhase::Canceled | ImportPhase::Failed
        ) {
            return Ok(status);
        }
        ensure!(Instant::now() < deadline, "import timeout: {status:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wb_call(bridge: &DesktopBridge, request: wb::Request) -> Result<wb::Response> {
    match app_call(
        bridge,
        Request::Lightroom {
            request: Box::new(request),
        },
    )? {
        Response::Lightroom(response) => Ok(*response),
        _ => bail!("unexpected Workbench response"),
    }
}

fn wb_status(bridge: &DesktopBridge) -> Result<wb::Status> {
    match wb_call(
        bridge,
        wb::Request::Status {
            workbench: None,
            attempt: None,
        },
    )? {
        wb::Response::Status(Some(status)) => Ok(status),
        _ => bail!("Workbench status absent"),
    }
}

fn wb_guard(status: &wb::Status) -> wb::Guard {
    wb::Guard {
        workbench: status.workbench.clone(),
        generation: status.generation.clone(),
        operation: status.operation.clone(),
    }
}

fn wb_wait(bridge: &DesktopBridge) -> Result<wb::Status> {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let status = wb_status(bridge)?;
        if matches!(
            status.phase,
            app_lightroom::Phase::Complete
                | app_lightroom::Phase::Failed
                | app_lightroom::Phase::Canceled
                | app_lightroom::Phase::Closed
        ) {
            return Ok(status);
        }
        ensure!(Instant::now() < deadline, "Workbench timeout: {status:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wb_result(bridge: &DesktopBridge) -> Result<(wb::Status, String)> {
    let status = wb_wait(bridge)?;
    ensure!(
        matches!(status.phase, app_lightroom::Phase::Complete),
        "Workbench failed: {:?}",
        status.error
    );
    let token = status
        .result_token
        .clone()
        .context("Workbench result token absent")?;
    let mut offset = U64(0);
    let mut result = String::new();
    loop {
        let wb::Response::Result(page) = wb_call(
            bridge,
            wb::Request::Result {
                guard: wb_guard(&status),
                token: token.clone(),
                offset,
                limit: U64(16 * 1024),
            },
        )?
        else {
            bail!("unexpected Workbench result page")
        };
        ensure!(
            page.page.offset == offset,
            "Workbench result cursor changed"
        );
        result.push_str(&page.page.json_fragment);
        match page.page.next {
            Some(next) => offset = next,
            None => break,
        }
    }
    ensure!(
        result.len() as u64 == status.result_bytes.0,
        "Workbench result length differs"
    );
    Ok((status, result))
}

fn wb_action(bridge: &DesktopBridge, action: wb::Action) -> Result<Value> {
    let status = wb_status(bridge)?;
    wb_call(
        bridge,
        wb::Request::Action {
            guard: wb_guard(&status),
            action,
        },
    )?;
    Ok(serde_json::from_str(&wb_result(bridge)?.1)?)
}

fn wb_query(bridge: &DesktopBridge, query: wb::Query) -> Result<Value> {
    let status = wb_status(bridge)?;
    wb_call(
        bridge,
        wb::Request::Read {
            guard: wb_guard(&status),
            query,
        },
    )?;
    Ok(serde_json::from_str(&wb_result(bridge)?.1)?)
}

fn wb_upload(
    bridge: &DesktopBridge,
    purpose: wb::InputPurpose,
    text: &str,
) -> Result<(String, String)> {
    let status = wb_status(bridge)?;
    let guard = wb_guard(&status);
    let digest = blake3::hash(text.as_bytes()).to_hex().to_string();
    let wb::Response::Input(Some(upload)) = wb_call(
        bridge,
        wb::Request::InputBegin {
            guard: guard.clone(),
            purpose,
            total_bytes: U64(text.len() as u64),
            expected_blake3: Some(digest.clone()),
        },
    )?
    else {
        bail!("Workbench upload admission absent")
    };
    let mut offset = 0;
    while offset < text.len() {
        let mut end = (offset + 16 * 1024).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        wb_call(
            bridge,
            wb::Request::InputAppend {
                guard: guard.clone(),
                input: upload.input.clone(),
                offset: U64(offset as u64),
                fragment: text[offset..end].into(),
            },
        )?;
        offset = end;
    }
    wb_call(
        bridge,
        wb::Request::InputFinish {
            guard,
            input: upload.input.clone(),
        },
    )?;
    Ok((upload.input, digest))
}

fn artifact_receipts(
    bridge: &DesktopBridge,
    directory: &Path,
    revision: &str,
    manifest_blake3: &str,
) -> Result<Vec<String>> {
    let session = uuid::Uuid::new_v4().to_string();
    let wb::Response::ArtifactPreparation(Some(LightroomArtifactPreparationReply::Begun {
        members,
        ..
    })) = wb_call(
        bridge,
        wb::Request::ArtifactPreparation {
            request: LightroomArtifactPreparation::Begin {
                session: session.clone(),
                directory: NativePath::from_path(directory),
                capture_revision: revision.into(),
                manifest_blake3: manifest_blake3.into(),
                maximum_bytes: U64(8 * 1024 * 1024 * 1024),
                open_deadline_ms: U64(120_000),
            },
        },
    )?
    else {
        bail!("artifact preparation begin reply")
    };
    let mut receipts = Vec::new();
    for member_index in 0..members.0 {
        let wb::Response::ArtifactPreparation(Some(LightroomArtifactPreparationReply::Prepared {
            receipt,
            ..
        })) = wb_call(
            bridge,
            wb::Request::ArtifactPreparation {
                request: LightroomArtifactPreparation::Member {
                    session: session.clone(),
                    member_index: U64(member_index),
                },
            },
        )?
        else {
            bail!("artifact preparation member reply")
        };
        receipts.push(receipt);
    }
    ensure!(
        matches!(
            wb_call(
                bridge,
                wb::Request::ArtifactPreparation {
                    request: LightroomArtifactPreparation::Discard { session }
                },
            )?,
            wb::Response::ArtifactPreparation(None)
        ),
        "artifact preparation session did not discard"
    );
    Ok(receipts)
}

fn sealed_document(
    bridge: &DesktopBridge,
    directory: &Path,
    document: LightroomSealedDocument,
) -> Result<ExactPart> {
    let session = uuid::Uuid::new_v4().to_string();
    let wb::Response::SealedDocument(Some(first)) = wb_call(
        bridge,
        wb::Request::SealedDocument {
            request: LightroomSealedRead::Begin {
                session: session.clone(),
                directory: NativePath::from_path(directory),
                document,
            },
        },
    )?
    else {
        bail!("sealed document begin reply")
    };
    ensure!(
        first.bytes.is_empty() && first.offset.0 == 0,
        "sealed document admission bytes"
    );
    let mut bytes = Vec::with_capacity(first.total_bytes.0.try_into()?);
    let mut offset = 0u64;
    while offset < first.total_bytes.0 {
        let wb::Response::SealedDocument(Some(page)) = wb_call(
            bridge,
            wb::Request::SealedDocument {
                request: LightroomSealedRead::Page {
                    session: session.clone(),
                    offset: U64(offset),
                    limit: U64((first.total_bytes.0 - offset).min(16 * 1024)),
                },
            },
        )?
        else {
            bail!("sealed document page reply")
        };
        ensure!(
            page.blake3 == first.blake3 && page.offset.0 == offset,
            "sealed document identity changed"
        );
        bytes.extend_from_slice(&page.bytes);
        offset = page.next.map_or(first.total_bytes.0, |next| next.0);
    }
    ensure!(
        blake3::hash(&bytes).to_hex().as_str() == first.blake3,
        "sealed document digest differs"
    );
    ensure!(
        matches!(
            wb_call(
                bridge,
                wb::Request::SealedDocument {
                    request: LightroomSealedRead::Discard { session }
                }
            )?,
            wb::Response::SealedDocument(None)
        ),
        "sealed document read did not discard"
    );
    Ok(ExactPart {
        role: match document {
            LightroomSealedDocument::Seal => migration::InputRole::Seal,
            LightroomSealedDocument::Approval => migration::InputRole::Approval,
        },
        text: String::from_utf8(bytes)?,
        blake3: first.blake3,
    })
}

fn migration_call(
    bridge: &DesktopBridge,
    request: migration::Request,
) -> Result<migration::Response> {
    match app_call(
        bridge,
        Request::LightroomMigration {
            request: Box::new(request),
        },
    )? {
        Response::LightroomMigration(response) => Ok(*response),
        _ => bail!("unexpected migration response"),
    }
}

fn migration_status(response: migration::Response) -> Result<migration::Snapshot> {
    match response {
        migration::Response::Status(snapshot) => Ok(snapshot),
        _ => bail!("migration status expected"),
    }
}

fn run_migration(
    bridge: &DesktopBridge,
    catalog: &str,
    destination: &Path,
    parts: &[ExactPart],
) -> Result<Value> {
    let approval = parts
        .iter()
        .find(|part| part.role == migration::InputRole::Approval)
        .context("approval part")?;
    let mut snapshot = migration_status(migration_call(
        bridge,
        migration::Request::Begin {
            operation: format!("qualification-{}", uuid::Uuid::new_v4().simple()),
            header: migration::Header {
                catalog: Some(catalog.into()),
                destination: NativePath::from_path(destination),
                operation: migration::Operation::Run {
                    approval_blake3: approval.blake3.clone(),
                    max_steps: U64(1_000_000),
                    max_seconds: U64(300),
                    source_open_ms: U64(120_000),
                    artifact_open_ms: U64(120_000),
                    max_artifact_bytes: U64(8 * 1024 * 1024 * 1024),
                },
                parts: parts
                    .iter()
                    .map(|part| migration::PartDescriptor {
                        role: part.role,
                        bytes: U64(part.text.len() as u64),
                        blake3: part.blake3.clone(),
                    })
                    .collect(),
                timeout_ms: U64(300_000),
            },
        },
    )?)?;
    let mut completed = 0usize;
    while snapshot.phase == migration::Phase::Uploading {
        let role = snapshot.next_role.context("migration upload role absent")?;
        let part = parts
            .iter()
            .find(|part| part.role == role)
            .context("migration requested unknown role")?;
        let local = usize::try_from(snapshot.uploaded.0)?
            .checked_sub(completed)
            .context("migration upload offset")?;
        if local < part.text.len() {
            let mut end = (local + 16 * 1024).min(part.text.len());
            while !part.text.is_char_boundary(end) {
                end -= 1;
            }
            snapshot = migration_status(migration_call(
                bridge,
                migration::Request::Upload {
                    guard: snapshot.guard.clone(),
                    role,
                    offset: U64(local as u64),
                    text: part.text[local..end].into(),
                },
            )?)?;
        } else {
            snapshot = migration_status(migration_call(
                bridge,
                migration::Request::Finish {
                    guard: snapshot.guard.clone(),
                    role,
                    blake3: part.blake3.clone(),
                },
            )?)?;
            completed = completed
                .checked_add(part.text.len())
                .context("migration upload total")?;
        }
    }
    ensure!(
        snapshot.phase == migration::Phase::Ready,
        "migration upload did not become ready: {snapshot:?}"
    );
    snapshot = migration_status(migration_call(
        bridge,
        migration::Request::Act {
            guard: snapshot.guard.clone(),
        },
    )?)?;
    let deadline = Instant::now() + DEADLINE;
    loop {
        if matches!(
            snapshot.phase,
            migration::Phase::Complete | migration::Phase::Failed
        ) {
            break;
        }
        ensure!(Instant::now() < deadline, "migration timeout: {snapshot:?}");
        let request = if snapshot.phase == migration::Phase::DrainPending {
            migration::Request::RetryDrain {
                guard: snapshot.guard.clone(),
            }
        } else {
            migration::Request::Status {
                guard: snapshot.guard.clone(),
            }
        };
        snapshot = migration_status(migration_call(bridge, request)?)?;
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(
        snapshot.phase == migration::Phase::Complete,
        "migration failed: {:?}",
        snapshot.failure
    );
    let result = snapshot
        .result
        .clone()
        .context("migration result identity absent")?;
    let mut text = String::new();
    for page in 0..result.pages.0 {
        let mut offset = U64(0);
        loop {
            let migration::Response::Page {
                text: fragment,
                next_offset,
                offset: actual,
                ..
            } = migration_call(
                bridge,
                migration::Request::ResultPage {
                    guard: snapshot.guard.clone(),
                    page: U64(page),
                    offset,
                    maximum_bytes: U64(16 * 1024),
                },
            )?
            else {
                bail!("migration result page expected")
            };
            ensure!(actual == offset, "migration result cursor changed");
            text.push_str(&fragment);
            match next_offset {
                Some(next) => offset = next,
                None => break,
            }
        }
    }
    ensure!(
        text.len() as u64 == result.bytes.0,
        "migration result length differs"
    );
    ensure!(
        blake3::hash(text.as_bytes()).to_hex().as_str() == result.blake3,
        "migration result digest differs"
    );
    ensure!(
        matches!(
            migration_call(
                bridge,
                migration::Request::Discard {
                    guard: snapshot.guard
                }
            )?,
            migration::Response::Discarded { .. }
        ),
        "migration owner did not discard"
    );
    Ok(serde_json::from_str(&text)?)
}

fn create_new_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn inspection_path(value: &Value) -> Result<PathBuf> {
    Ok(serde_json::from_value::<NativePath>(value.clone())?.to_path()?)
}

fn validate_path_evidence(
    catalog: &str,
    row: &Value,
    originals: &Path,
    observed_sidecars: &mut BTreeSet<String>,
) -> Result<()> {
    let state = row["state"].as_str().context("inspection path state")?;
    let path = inspection_path(&row["inspection_path"])?;
    let relative = path
        .strip_prefix(originals)
        .with_context(|| {
            format!(
                "inspection path escaped fixture originals: {}",
                path.display()
            )
        })?
        .to_string_lossy();
    let evidence = &row["evidence"];
    let expected_origins = [
        "embedded",
        "sidecar_xmp",
        "sidecar_XMP",
        "sidecar_appended_xmp",
        "sidecar_appended_XMP",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if !SELECTED.contains(&catalog) {
        let expected = match catalog {
            "2014-v13.lrcat" => "2014/January/2014-01-02/excluded-only-2014.jpg",
            "2015-v13-3.lrcat" => "2015/February/2015-02-03/excluded-only-2015.jpg",
            _ => bail!("unexpected excluded fixture catalog: {catalog}"),
        };
        let inspections = evidence["inspections"]
            .as_array()
            .context("missing original inspection evidence")?;
        let origins = inspections
            .iter()
            .map(|inspection| {
                inspection["origin"]
                    .as_str()
                    .context("missing original inspection origin")
            })
            .collect::<Result<BTreeSet<_>>>()?;
        ensure!(
            state == "missing"
                && relative == expected
                && evidence["packet_gaps"] == false
                && evidence["metadata"]["missing"] == true
                && inspections.len() == expected_origins.len()
                && origins == expected_origins
                && inspections.iter().all(|inspection| {
                    inspection["state"] == "absent"
                        && inspection["error"].is_null()
                        && inspection["status"].is_null()
                        && inspection["revision"].is_null()
                        && inspection["issues"].is_null()
                        && inspection["packets"] == 0
                        && inspection["parse_inputs"] == 0
                }),
            "excluded fixture path evidence differs for {catalog}: {relative} {state}"
        );
        return Ok(());
    }

    let inspections = evidence["inspections"]
        .as_array()
        .context("original inspection evidence")?;
    let origins = inspections
        .iter()
        .map(|inspection| {
            inspection["origin"]
                .as_str()
                .context("original inspection origin")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    ensure!(
        inspections.len() == expected_origins.len() && origins == expected_origins,
        "original inspection origin roster differs for {relative}"
    );
    match state {
        "available_packets_retained" => {
            ensure!(
                evidence["packet_gaps"] == false,
                "retained path reports packet gaps for {relative}"
            );
            for inspection in inspections {
                let status = inspection["status"].as_str();
                let inspected = inspection["state"] == "inspected";
                let absent = inspection["state"] == "absent";
                ensure!(
                    inspection["error"].is_null(),
                    "path inspection error for {relative}"
                );
                ensure!(
                    (inspected
                        && matches!(status, Some("Complete" | "Absent"))
                        && inspection["issues"].as_array().is_some_and(Vec::is_empty))
                        || (absent
                            && status.is_none()
                            && inspection["issues"].is_null()
                            && inspection["packets"] == 0
                            && inspection["parse_inputs"] == 0),
                    "unexpected retained inspection status for {relative}: {}",
                    inspection["status"]
                );
            }
        }
        "available_packet_gaps" => {
            ensure!(
                path.extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.eq_ignore_ascii_case("bmp"))
                    && evidence["packet_gaps"] == true
                    && inspections.len() == 5,
                "unexpected packet gap for {relative}"
            );
            let mut unsupported = 0usize;
            let mut absent_sidecars = 0usize;
            for inspection in inspections {
                if inspection["origin"] == "embedded" {
                    let issues = inspection["issues"]
                        .as_array()
                        .context("BMP Unsupported issue roster")?;
                    ensure!(
                        inspection["state"] == "inspected"
                            && inspection["error"].is_null()
                            && inspection["status"] == "Unsupported"
                            && inspection["packets"] == 0
                            && inspection["parse_inputs"] == 0
                            && !issues.is_empty()
                            && issues.iter().all(|issue| issue["status"] == "Unsupported"),
                        "BMP gap evidence differs"
                    );
                    unsupported += 1;
                } else {
                    ensure!(
                        inspection["origin"]
                            .as_str()
                            .is_some_and(|origin| origin.starts_with("sidecar"))
                            && inspection["state"] == "absent"
                            && inspection["error"].is_null()
                            && inspection["status"].is_null()
                            && inspection["packets"] == 0
                            && inspection["parse_inputs"] == 0,
                        "BMP sidecar evidence differs"
                    );
                    absent_sidecars += 1;
                }
            }
            ensure!(
                unsupported == 1 && absent_sidecars == 4,
                "BMP gap roster differs"
            );
        }
        _ => bail!("unexpected selected original path state for {catalog}: {state}"),
    }
    for inspection in inspections {
        if inspection["origin"]
            .as_str()
            .is_some_and(|origin| origin.starts_with("sidecar"))
            && inspection["state"] == "inspected"
            && inspection["error"].is_null()
            && inspection["status"] == "Complete"
            && inspection["packets"]
                .as_u64()
                .is_some_and(|packets| packets > 0)
        {
            let sidecar = inspection_path(&inspection["path"])?;
            ensure!(
                sidecar.starts_with(originals)
                    && sidecar
                        .extension()
                        .and_then(|value| value.to_str())
                        .is_some_and(|value| value.eq_ignore_ascii_case("xmp")),
                "inspected sidecar escaped fixture originals"
            );
            observed_sidecars.insert(sidecar.to_string_lossy().to_ascii_lowercase());
        }
    }
    Ok(())
}

impl Harness {
    fn new(executable: PathBuf, working: PathBuf, output: PathBuf) -> Result<Self> {
        let catalog = output.join("catalog");
        let bridge = DesktopBridge::spawn(Config {
            worker_executable: executable.clone(),
            cache_root: Some(output.join("cache")),
            original_roots: vec![],
            preview_policy: PreviewPolicy::default(),
            preview_limits: ServiceLimits::default(),
            limits: Limits::default(),
        })?;
        Ok(Self {
            bridge,
            output,
            catalog,
            working,
            evidence: Vec::new(),
        })
    }

    fn record(&mut self, name: &str, value: &impl Serialize) -> Result<()> {
        let path = self.output.join(name);
        create_new_json(&path, value)?;
        self.evidence.push(name.into());
        Ok(())
    }

    fn import(&mut self) -> Result<String> {
        let Response::Status(_) = app_call(
            &self.bridge,
            Request::Create {
                path: NativePath::from_path(&self.catalog),
            },
        )?
        else {
            bail!("catalog create response")
        };
        let mut token = wait_ready(&self.bridge)?;
        let originals = self.working.join("inputs/originals");
        let Response::Import(Some(started)) = app_call(
            &self.bridge,
            Request::ImportStart {
                catalog: token.clone(),
                source: NativePath::from_path(&originals),
            },
        )?
        else {
            bail!("import start response")
        };
        let Response::Import(Some(canceling)) = app_call(
            &self.bridge,
            Request::ImportCancel {
                catalog: token.clone(),
                import: started.id.clone(),
            },
        )?
        else {
            bail!("import cancel response")
        };
        ensure!(
            matches!(
                canceling.phase,
                ImportPhase::CancelRequested | ImportPhase::Canceled
            ),
            "cancel was not admitted"
        );
        let canceled = wait_import(&self.bridge, &token)?;
        ensure!(
            canceled.phase == ImportPhase::Canceled,
            "fixture completed before cancellation; use a fresh working/output pair"
        );
        self.record("import-canceled.json", &canceled)?;
        app_call(&self.bridge, Request::Close { catalog: token })?;
        app_call(
            &self.bridge,
            Request::OpenExisting {
                path: NativePath::from_path(&self.catalog),
            },
        )?;
        token = wait_ready(&self.bridge)?;
        app_call(
            &self.bridge,
            Request::ImportResume {
                catalog: token.clone(),
                source: NativePath::from_path(&originals),
            },
        )?;
        let complete = wait_import(&self.bridge, &token)?;
        ensure!(
            complete.phase == ImportPhase::Complete && complete.failed.0 == 0,
            "import failed: {complete:?}"
        );
        self.record("import-complete.json", &complete)?;
        app_call(&self.bridge, Request::Close { catalog: token })?;
        app_call(
            &self.bridge,
            Request::OpenExisting {
                path: NativePath::from_path(&self.catalog),
            },
        )?;
        token = wait_ready(&self.bridge)?;
        app_call(
            &self.bridge,
            Request::ImportResume {
                catalog: token.clone(),
                source: NativePath::from_path(&originals),
            },
        )?;
        let repeated = wait_import(&self.bridge, &token)?;
        ensure!(
            repeated.phase == ImportPhase::Complete
                && repeated.imported.0 == 0
                && repeated.failed.0 == 0,
            "repeat import was not idempotent: {repeated:?}"
        );
        self.record("import-repeat.json", &repeated)?;
        Ok(token)
    }

    fn inspect_and_seal(
        &mut self,
        expected_sidecars: usize,
    ) -> Result<(Vec<ExactPart>, Vec<String>, Value)> {
        let inspection = self.output.join("inspection");
        let wb::Response::Status(_) = wb_call(
            &self.bridge,
            wb::Request::Open {
                attempt: uuid::Uuid::new_v4().to_string(),
                root: NativePath::from_path(&inspection),
                mode: app_lightroom::OpenMode::Create,
                capture_staging: NativePath::from_path(&self.output),
                limits: app_lightroom::Limits::default().into(),
            },
        )?
        else {
            bail!("Workbench open response")
        };
        ensure!(
            matches!(wb_wait(&self.bridge)?.phase, app_lightroom::Phase::Complete),
            "Workbench open failed"
        );
        let catalogs = self.working.join("inputs/lightroom-catalogs");
        let mut revisions = HashMap::new();
        let mut reports = HashMap::new();
        let captures = self.output.join("captures");
        let originals = self.working.join("inputs/originals");
        let mut observed_sidecars = BTreeSet::new();
        fs::create_dir(&captures)?;
        for name in SELECTED.into_iter().chain(EXCLUDED) {
            let source = catalogs.join(name);
            let capture = captures.join(name);
            let manifest = wb_action(
                &self.bridge,
                wb::Action::Capture {
                    source: NativePath::from_path(&source),
                    output: NativePath::from_path(&capture),
                    include_auxiliary: true,
                    closed_application_evidence: Some(
                        "generated qualification catalog; no Lightroom process owns it".into(),
                    ),
                    limits: lightroom::Limits::default().into(),
                },
            )?;
            self.record(&format!("capture-{name}.json"), &manifest)?;
            let added = wb_action(
                &self.bridge,
                wb::Action::AddCapture {
                    directory: NativePath::from_path(&capture),
                },
            )?;
            let revision = added["revision"]
                .as_str()
                .context("capture revision")?
                .to_owned();
            let mut row_stage = None;
            for _ in 0..100 {
                let progress = wb_action(
                    &self.bridge,
                    wb::Action::Resume {
                        revision: revision.clone(),
                        max_rows: U64(100_000),
                    },
                )?;
                let stage = progress["stage"].as_str().context("inspection row stage")?;
                if stage == INSPECTION_COMPLETE || stage == "rows_reconciled_paths_pending" {
                    row_stage = Some(stage.to_owned());
                    break;
                }
                ensure!(
                    stage == "pending",
                    "unexpected inspection row stage: {stage}"
                );
            }
            let row_stage = row_stage.context("inspection rows did not finish within 100 pages")?;
            let mut original_pages = Vec::new();
            if row_stage != INSPECTION_COMPLETE {
                for _ in 0..100 {
                    let page = wb_action(
                        &self.bridge,
                        wb::Action::InspectOriginals {
                            revision: revision.clone(),
                            limit: U64(1_000),
                            inspection: app_lightroom::OriginalInspection::Packets,
                        },
                    )?;
                    let processed = page["processed"]
                        .as_str()
                        .context("original inspection processed count")?
                        .parse::<u64>()?;
                    original_pages.push(page);
                    let interim: InspectionReport = serde_json::from_value(wb_query(
                        &self.bridge,
                        wb::Query::Report {
                            revision: revision.clone(),
                        },
                    )?)?;
                    if interim.stage == INSPECTION_COMPLETE {
                        break;
                    }
                    ensure!(
                        interim.stage == "rows_reconciled_paths_pending" && processed > 0,
                        "original inspection stalled for {name}: {}",
                        interim.stage
                    );
                }
            }
            let report: InspectionReport = serde_json::from_value(wb_query(
                &self.bridge,
                wb::Query::Report {
                    revision: revision.clone(),
                },
            )?)?;
            ensure!(
                report.stage == INSPECTION_COMPLETE,
                "inspection incomplete for {name}: {}",
                report.stage
            );
            let mut paths = Vec::new();
            let mut after = 0i64;
            let mut paths_exhausted = false;
            for _ in 0..100 {
                let page = wb_query(
                    &self.bridge,
                    wb::Query::Paths {
                        revision: revision.clone(),
                        after: photocatalog::application::I64(after),
                        limit: U64(1_000),
                    },
                )?;
                let rows = page["rows"].as_array().context("inspection path rows")?;
                for row in rows {
                    validate_path_evidence(name, row, &originals, &mut observed_sidecars)?;
                    paths.push(row.clone());
                }
                if page["next"].is_null() {
                    paths_exhausted = true;
                    break;
                }
                let next = page["next"]
                    .as_str()
                    .context("inspection path cursor")?
                    .parse()?;
                ensure!(next > after, "inspection path cursor did not advance");
                after = next;
            }
            ensure!(
                paths_exhausted,
                "inspection paths exceed qualification bound"
            );
            let reported_paths = report
                .counts
                .iter()
                .filter(|(name, _)| name.starts_with("paths_"))
                .map(|(_, count)| *count)
                .sum::<i64>();
            let files = *report
                .counts
                .get("files")
                .context("inspection report file count")?;
            ensure!(
                files > 0 && reported_paths == files && paths.len() as i64 == files,
                "inspection path roster differs for {name}: paged {}, reported {reported_paths}, files {files}",
                paths.len()
            );
            self.record(
                &format!("inspection-{name}.json"),
                &json!({"row_stage": row_stage, "original_pages": original_pages, "paths": paths, "report": &report}),
            )?;
            let year = &name[..4];
            wb_action(
                &self.bridge,
                wb::Action::AssignFamily {
                    revision: revision.clone(),
                    family: format!("qualification-{year}"),
                    reason: "fixture itinerary family".into(),
                },
            )?;
            revisions.insert(name.to_owned(), revision.clone());
            reports.insert(revision, report);
        }
        ensure!(
            observed_sidecars.len() == expected_sidecars,
            "inspected sidecar roster differs: observed {}, expected {expected_sidecars}",
            observed_sidecars.len()
        );
        let family_report: FamilyReport =
            serde_json::from_value(wb_query(&self.bridge, wb::Query::Families {})?)?;
        ensure!(
            family_report.families.len() == 2,
            "expected two fixture families"
        );
        let selected_revisions = SELECTED
            .iter()
            .map(|name| revisions[*name].clone())
            .collect::<BTreeSet<_>>();
        let mut decisions = Vec::new();
        for family in &family_report.families {
            let selected = family
                .members
                .iter()
                .find(|member| selected_revisions.contains(&member.revision_id))
                .context("fixture family has no selected current catalog")?;
            wb_action(
                &self.bridge,
                wb::Action::Choose {
                    family: family.id.clone(),
                    revision: selected.revision_id.clone(),
                    expected_evidence: family.evidence_digest.clone(),
                    reason: "exact itinerary selection".into(),
                },
            )?;
            decisions.push(FamilyDecision::Select {
                family: family.id.clone(),
                revision: selected.revision_id.clone(),
                expected_evidence_digest: family.evidence_digest.clone(),
            });
        }
        let selection = serde_json::to_string(&SelectionRequest {
            inspection: NativePath::from_path(&inspection),
            families: decisions,
        })?;
        let (input, _) = wb_upload(&self.bridge, wb::InputPurpose::SelectionRequest, &selection)?;
        let summary = wb_action(
            &self.bridge,
            wb::Action::PrepareSelection {
                input,
                limits: lightroom::selection::SelectionLimits::default().into(),
            },
        )?;
        ensure!(
            summary["selected"] == 2 && summary["excluded"] == 2,
            "selection roster differs: {summary}"
        );
        let review_token = summary["token"]
            .as_str()
            .context("selection review token")?
            .to_owned();
        let capture_page = wb_query(
            &self.bridge,
            wb::Query::SelectionPage {
                review_token: review_token.clone(),
                collection: app_lightroom::ReviewCollection::Captures,
                after: U64(0),
                limit: U64(16),
            },
        )?;
        let rows = capture_page["rows"]
            .as_array()
            .context("selection capture rows")?;
        let mut receipts = Vec::new();
        let mut virtual_copies = 0u64;
        let mut catalog_packets = 0u64;
        for row in rows.iter().filter(|row| row["selected"] == true) {
            let revision = row["revision"].as_str().context("selected revision")?;
            let manifest_blake3 = row["manifest_blake3"]
                .as_str()
                .context("selected manifest digest")?;
            let (name, _) = revisions
                .iter()
                .find(|(_, value)| value.as_str() == revision)
                .context("selected capture directory")?;
            receipts.extend(artifact_receipts(
                &self.bridge,
                &captures.join(name),
                revision,
                manifest_blake3,
            )?);
            let report = reports
                .get(revision)
                .context("selected inspection report")?;
            virtual_copies += u64::try_from(
                *report
                    .counts
                    .get("retained_virtual_copies")
                    .context("virtual-copy source count")?,
            )?;
            catalog_packets += u64::try_from(
                *report
                    .counts
                    .get("catalog_xmp_packets")
                    .context("catalog packet source count")?,
            )?;
        }
        ensure!(
            virtual_copies == 2 && catalog_packets == 4,
            "selected source reconciliation differs"
        );
        let draft = json!({
            "protocol": 1,
            "review_token": review_token,
            "destination": NativePath::from_path(&self.catalog),
            "import_source": "lensworks-cross-platform-disposable-v1 root qualification",
            "overlap": OverlapPolicy::ReuseExactPath { reason: "direct import precedes migration into this same reviewed catalog".into() },
            "keyword_overlap": KeywordOverlap::ReuseExactHierarchy { reason: "same selected Lightroom hierarchy".into() },
            "artifacts": receipts.iter().map(|receipt| json!({"receipt": receipt})).collect::<Vec<_>>(),
            "supplements": [],
            "authorization": "root-owned disposable qualification of the exact selected fixture catalogs"
        });
        let draft = serde_json::to_string(&draft)?;
        let (input, _) = wb_upload(&self.bridge, wb::InputPurpose::ApprovalDraft, &draft)?;
        let documents = wb_action(
            &self.bridge,
            wb::Action::ApprovalDocuments {
                input,
                review_token: review_token.clone(),
            },
        )?;
        let approval = documents["approval_json"]
            .as_str()
            .context("approval document")?
            .to_owned();
        let policy = documents["policy_json"]
            .as_str()
            .context("policy document")?
            .to_owned();
        let approval_blake3 = documents["approval_blake3"]
            .as_str()
            .context("approval digest")?
            .to_owned();
        let policy_blake3 = documents["policy_blake3"]
            .as_str()
            .context("policy digest")?
            .to_owned();
        let (input, digest) = wb_upload(&self.bridge, wb::InputPurpose::Approval, &approval)?;
        ensure!(digest == approval_blake3, "approval upload digest differs");
        let sealed = self.output.join("sealed");
        let seal_receipt = wb_action(
            &self.bridge,
            wb::Action::Seal {
                review_token,
                approval_blake3: approval_blake3.clone(),
                input,
                output: NativePath::from_path(&sealed),
            },
        )?;
        self.record("selection-and-seal.json", &json!({"family_report": family_report, "summary": summary, "documents": documents, "seal_receipt": seal_receipt}))?;
        wb_action(&self.bridge, wb::Action::ReleaseReview {})?;
        let workbench = wb_status(&self.bridge)?.workbench;
        wb_call(&self.bridge, wb::Request::Close { workbench })?;
        ensure!(wb_wait(&self.bridge)?.closed, "Workbench did not close");
        for receipt in receipts {
            ensure!(
                matches!(
                    wb_call(
                        &self.bridge,
                        wb::Request::ArtifactPreparation {
                            request: LightroomArtifactPreparation::DiscardReceipt { receipt }
                        }
                    )?,
                    wb::Response::ArtifactPreparation(None)
                ),
                "artifact receipt did not discard"
            );
        }
        let seal = sealed_document(&self.bridge, &sealed, LightroomSealedDocument::Seal)?;
        let exact_approval =
            sealed_document(&self.bridge, &sealed, LightroomSealedDocument::Approval)?;
        ensure!(
            exact_approval.blake3 == approval_blake3 && exact_approval.text == approval,
            "sealed approval differs"
        );
        let input_seal: lightroom::migration_source::InputSeal = serde_json::from_str(&seal.text)?;
        let expected_selected = SELECTED
            .iter()
            .map(|name| revisions[*name].clone())
            .collect::<BTreeSet<_>>();
        let expected_excluded = EXCLUDED
            .iter()
            .map(|name| revisions[*name].clone())
            .collect::<BTreeSet<_>>();
        ensure!(
            input_seal
                .selected
                .iter()
                .map(|capture| capture.revision.clone())
                .collect::<BTreeSet<_>>()
                == expected_selected
                && input_seal
                    .excluded_revisions
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    == expected_excluded,
            "sealed selected/excluded roster differs from the exact fixture choices"
        );
        Ok((
            vec![
                seal,
                exact_approval,
                ExactPart {
                    role: migration::InputRole::Policy,
                    text: policy,
                    blake3: policy_blake3,
                },
            ],
            input_seal.excluded_revisions,
            serde_json::to_value(family_report)?,
        ))
    }

    fn public_counts(&self, catalog: &str) -> Result<(PublicCounts, Value)> {
        let mut cursor = None;
        let mut images = Vec::new();
        loop {
            let Response::Images {
                rows,
                next,
                has_more,
                ..
            } = app_call(
                &self.bridge,
                Request::Images {
                    catalog: catalog.into(),
                    folder: None,
                    recursive: true,
                    text: None,
                    cursor,
                    limit: 100,
                },
            )?
            else {
                bail!("images response")
            };
            images.extend(rows);
            if !has_more {
                break;
            }
            cursor = next;
            ensure!(cursor.is_some(), "images continuation absent");
        }
        let assets = images
            .iter()
            .map(|image| image.key.asset_id.clone())
            .collect::<BTreeSet<_>>();
        let mut source_ids = BTreeSet::new();
        for image in &images {
            let Response::Metadata(identity) = app_call(
                &self.bridge,
                Request::Metadata {
                    catalog: catalog.into(),
                    request: Box::new(metadata::Request::Identity {
                        key: image.key.clone(),
                    }),
                },
            )?
            else {
                bail!("metadata identity response")
            };
            let metadata::Response::Identity(identity) = *identity else {
                bail!("metadata identity payload")
            };
            let mut after = None;
            loop {
                let Response::Metadata(response) = app_call(
                    &self.bridge,
                    Request::Metadata {
                        catalog: catalog.into(),
                        request: Box::new(metadata::Request::Sources {
                            identity: identity.clone(),
                            after,
                            limit: 100,
                        }),
                    },
                )?
                else {
                    bail!("metadata sources response")
                };
                let metadata::Response::Sources(page) = *response else {
                    bail!("metadata sources payload")
                };
                source_ids.extend(page.rows.iter().map(|source| source.id.0));
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        let mut after = String::new();
        let mut collections = Vec::new();
        loop {
            let Response::Organization(response) = app_call(
                &self.bridge,
                Request::Organization {
                    catalog: catalog.into(),
                    request: Box::new(organization::Request::Collections {
                        after: after.clone(),
                        limit: 100,
                    }),
                },
            )?
            else {
                bail!("collections response")
            };
            let organization::Response::Collections(page) = *response else {
                bail!("collections payload")
            };
            collections.extend(page.rows);
            match page.next {
                Some(next) => after = next,
                None => break,
            }
        }
        let counts = PublicCounts {
            assets: u64::try_from(assets.len())?,
            variants: u64::try_from(images.len())?,
            xmp_sources: u64::try_from(source_ids.len())?,
            collections: u64::try_from(collections.len())?,
        };
        Ok((
            counts,
            json!({"images":images,"unique_asset_ids":assets,"metadata_source_ids":source_ids,"collections":collections}),
        ))
    }
}

fn closed_sqlite_counts(
    catalog: &Path,
    public: PublicCounts,
    excluded_revisions: &[String],
    run: &str,
) -> Result<(Counts, Value)> {
    ensure!(
        excluded_revisions.len() == 2,
        "expected two sealed excluded revisions"
    );
    ensure!(
        excluded_revisions.iter().all(|revision| hex(revision, 32)),
        "excluded revision identity bounds"
    );
    ensure!(hex(run, 32), "migration run identity bounds");
    let database = catalog.join("catalog.sqlite3");
    let connection = Connection::open_with_flags(
        &database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", true)?;
    let scalar = |sql: &str| -> Result<u64> {
        Ok(u64::try_from(
            connection.query_row(sql, [], |row| row.get::<_, i64>(0))?,
        )?)
    };
    let sqlite_assets = scalar("SELECT count(*) FROM assets")?;
    let sqlite_variants = scalar("SELECT count(*) FROM catalog_images")?;
    let sqlite_xmp_sources = scalar("SELECT count(*) FROM metadata_sources")?;
    let sqlite_collections = scalar("SELECT count(*) FROM organization_collections")?;
    ensure!(
        (
            sqlite_assets,
            sqlite_variants,
            sqlite_xmp_sources,
            sqlite_collections
        ) == (
            public.assets,
            public.variants,
            public.xmp_sources,
            public.collections
        ),
        "closed SQLite counts differ from fully paged public API counts"
    );
    let virtual_copies = u64::try_from(connection.query_row(
        "SELECT count(*) FROM catalog_images WHERE role='virtual'",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    let excluded_catalog_rows_imported = u64::try_from(connection.query_row(
        "SELECT count(*) FROM migration_retained_records WHERE revision IN (?1,?2)",
        params![excluded_revisions[0], excluded_revisions[1]],
        |row| row.get::<_, i64>(0),
    )?)?;
    ensure!(
        public.assets == 8
            && virtual_copies == 2
            && public.collections == 5
            && excluded_catalog_rows_imported == 0,
        "destination fixture acceptance counts differ"
    );
    let complete_run_receipts = u64::try_from(connection.query_row(
        "SELECT count(*) FROM migration_runs WHERE id=?1 AND json_extract(progress,'$.complete')=1",
        [run],
        |row| row.get::<_, i64>(0),
    )?)?;
    ensure!(
        complete_run_receipts == 1,
        "durable completed migration run receipt absent"
    );
    let counts = Counts {
        assets: public.assets,
        variants: public.variants,
        xmp_sources: public.xmp_sources,
        virtual_copies,
        collections: public.collections,
        excluded_catalog_rows_imported,
    };
    let receipt = json!({
        "database": database,
        "open_flags": ["SQLITE_OPEN_READ_ONLY", "SQLITE_OPEN_NO_MUTEX"],
        "query_only": true,
        "migration_run": run,
        "excluded_revisions": excluded_revisions,
        "queries": {
            "assets": {"sql": "SELECT count(*) FROM assets", "value": sqlite_assets, "public_api_value": public.assets},
            "variants": {"sql": "SELECT count(*) FROM catalog_images", "value": sqlite_variants, "public_api_value": public.variants},
            "xmp_sources": {"sql": "SELECT count(*) FROM metadata_sources", "value": sqlite_xmp_sources, "public_api_value": public.xmp_sources},
            "virtual_copies": {"sql": "SELECT count(*) FROM catalog_images WHERE role='virtual'", "value": virtual_copies},
            "collections": {"sql": "SELECT count(*) FROM organization_collections", "value": sqlite_collections, "public_api_value": public.collections},
            "excluded_catalog_rows_imported": {"sql": "SELECT count(*) FROM migration_retained_records WHERE revision IN (?1,?2)", "value": excluded_catalog_rows_imported},
            "complete_run_receipts": {"sql": "SELECT count(*) FROM migration_runs WHERE id=?1 AND json_extract(progress,'$.complete')=1", "value": complete_run_receipts}
        }
    });
    Ok((counts, receipt))
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.bridge.try_shutdown();
    }
}

#[test]
#[ignore = "requires verified disposable working set, fresh evidence output and exact built PHOTOCATALOG_TEST_EXECUTABLE"]
fn root_native_fixture_import_migration_qualification() -> Result<()> {
    let (executable, working, output, manifest, working_manifest_sha256, binary_identity) =
        admitted_inputs()?;
    let commit = std::env::var("PHOTOCATALOG_TEST_COMMIT")?;
    let mut harness = Harness::new(executable, working, output)?;
    let result = (|| -> Result<()> {
        let mut token = harness.import()?;
        let expected_sidecars = usize::try_from(
            manifest.source_reconciliation["selected_source_reconciliation"]
                ["sidecar_xmp_packets_available"]
                .as_u64()
                .context("verified fixture sidecar packet count")?,
        )?;
        let (parts, excluded_revisions, family_evidence) =
            harness.inspect_and_seal(expected_sidecars)?;
        let first = run_migration(&harness.bridge, &token, &harness.catalog, &parts)?;
        ensure!(
            first["status"] == "complete" && first["progress"]["complete"] == true,
            "migration did not complete: {first}"
        );
        let run = first["progress"]["id"]
            .as_str()
            .context("migration run identity")?
            .to_owned();
        harness.record("migration-first.json", &first)?;
        app_call(&harness.bridge, Request::Close { catalog: token })?;
        app_call(
            &harness.bridge,
            Request::OpenExisting {
                path: NativePath::from_path(&harness.catalog),
            },
        )?;
        token = wait_ready(&harness.bridge)?;
        let repeated = run_migration(&harness.bridge, &token, &harness.catalog, &parts)?;
        ensure!(
            repeated["status"] == "complete" && repeated["progress"]["complete"] == true,
            "repeat migration did not complete: {repeated}"
        );
        ensure!(
            repeated["progress"]["id"].as_str() == Some(run.as_str()),
            "repeat migration did not resume exact durable run"
        );
        harness.record("migration-repeat.json", &repeated)?;
        let (public_counts, public_queries) = harness.public_counts(&token)?;
        harness.record("destination-public-queries.json", &public_queries)?;
        app_call(&harness.bridge, Request::Close { catalog: token })?;
        harness.bridge.try_shutdown()?;
        let shutdown = harness.bridge.status();
        ensure!(
            shutdown.phase == TransportPhase::Closed
                && shutdown.pending == 0
                && !shutdown.outcome_unknown,
            "desktop did not reach checked fully drained shutdown: {shutdown:?}"
        );
        harness.record(
            "desktop-closed.json",
            &json!({
                "phase": "closed",
                "pending": shutdown.pending,
                "outcome_unknown": shutdown.outcome_unknown,
                "message": shutdown.message,
                "pid": shutdown.pid
            }),
        )?;
        let (counts, sqlite_receipt) =
            closed_sqlite_counts(&harness.catalog, public_counts, &excluded_revisions, &run)?;
        harness.record("destination-closed-sqlite-counts.json", &sqlite_receipt)?;
        let qualification = json!({
            "format_version": 1,
            "status": "root_native_qualification_complete_pending_review",
            "fixture_builder_commit": BUILDER_COMMIT,
            "baseline_manifest_sha256": BASELINE_MANIFEST_SHA256,
            "working_manifest_sha256": working_manifest_sha256,
            "tested_product_commit": commit,
            "qualification_environment": binary_identity,
            "workflow": "direct import followed by selected Lightroom migration into the same new catalog",
            "destination_counts": counts,
            "reconciliation": manifest.source_reconciliation["selected_source_reconciliation"],
            "selection_evidence": family_evidence,
            "count_methods": {
                "assets": "unique asset_id values from fully paged public Images replies",
                "variants": "logical image rows from fully paged public Images replies",
                "xmp_sources": "unique source IDs from fully paged public Metadata Identity/Sources replies for every logical image",
                "virtual_copies": "actual destination catalog_images rows with role=virtual from read-only SQLite after public Close and checked full DesktopBridge shutdown",
                "collections": "rows from fully paged public Organization Collections replies",
                "excluded_catalog_rows_imported": "actual destination migration_retained_records rows matching either exact sealed excluded revision, from read-only SQLite after public Close and checked full DesktopBridge shutdown"
            },
            "evidence_paths": harness.evidence,
            "approved_for_platform_handoff": false
        });
        create_new_json(
            &harness.output.join("target-qualification.json"),
            &qualification,
        )?;
        println!("{}", serde_json::to_string_pretty(&qualification)?);
        Ok(())
    })();
    if let Err(error) = &result {
        eprintln!(
            "qualification failed before cleanup: {error:#}; transport: {:?}",
            harness.bridge.status()
        );
    }
    result
}
