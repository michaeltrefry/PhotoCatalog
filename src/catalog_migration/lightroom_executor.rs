//! The seven Lightroom migration operations shared by the direct CLI and the
//! isolated managed worker. Path/transport admission stays in those adapters;
//! this module owns the single business-operation implementation.
use super::{
    artifacts::ArtifactLimits,
    current_repair,
    import_artifacts::{ArtifactFactory, LocalArtifacts, Worker},
    importer::{Policy, Progress},
    keyword_repair, supplements,
};
use crate::{
    Catalog,
    lightroom::migration_source::{MigrationRead, MigrationSource},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

pub const DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;

/// Resolve only the existing ancestor, preserving a not-yet-created target's
/// lexical suffix. Both direct and managed adapters use this before mutation.
pub fn destination_path(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "destination must be absolute");
    ensure!(
        path.components()
            .all(|c| !matches!(c, Component::ParentDir | Component::CurDir)),
        "destination cannot contain relative components"
    );
    let mut ancestor = path;
    let mut remaining = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remaining.push(
                    ancestor
                        .file_name()
                        .context("destination has no ancestor")?,
                );
                ancestor = ancestor.parent().context("destination has no parent")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut resolved = fs::canonicalize(ancestor)?;
    ensure!(
        fs::metadata(&resolved)?.is_dir(),
        "destination ancestor must be a directory"
    );
    for part in remaining.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

pub fn disjoint(destination: &Path, source: &Path) -> Result<()> {
    let source = fs::canonicalize(source)?;
    ensure!(
        !destination.starts_with(&source) && !source.starts_with(destination),
        "destination and source custody must be separate directories"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub struct WorkLimit {
    pub steps: u64,
    pub seconds: u64,
}
impl WorkLimit {
    pub fn validate(self) -> Result<()> {
        ensure!(
            self.steps > 0 && (1..=86_400).contains(&self.seconds),
            "invalid work budget"
        );
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct RunResult {
    protocol: u8,
    status: &'static str,
    progress: Progress,
    steps: u64,
    elapsed_seconds: f64,
    needs_decision: Option<String>,
    adobe_rendering_equivalent: bool,
    native_collection_order_equivalent: bool,
}

#[derive(Debug, Serialize)]
pub struct CurrentRepairResult {
    protocol: u8,
    status: &'static str,
    repair: current_repair::Progress,
    steps: u64,
    elapsed_seconds: f64,
    adobe_rendering_equivalent: bool,
}

#[derive(Debug, Serialize)]
pub struct KeywordRepairResult {
    protocol: u8,
    status: &'static str,
    repair: keyword_repair::Progress,
    steps: u64,
    elapsed_seconds: f64,
    adobe_rendering_equivalent: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Output {
    Run(RunResult),
    Status(Progress),
    Supplements(Vec<supplements::Prepared>),
    CurrentRepair(CurrentRepairResult),
    CurrentRepairStatus(current_repair::Progress),
    KeywordRepair(KeywordRepairResult),
    KeywordRepairStatus(keyword_repair::Progress),
}

fn status(complete: bool, stopped: bool) -> &'static str {
    if complete {
        "complete"
    } else if stopped {
        "stopped"
    } else {
        "paused"
    }
}

type ProgressReporter<'a> = dyn FnMut(&str, u64, Option<u64>) -> Result<()> + 'a;

pub(crate) struct RunAuthority<'a> {
    pub(crate) source: &'a dyn MigrationRead,
    pub(crate) approval: &'a [u8],
    pub(crate) policy: &'a Policy,
}

fn run_with_factory<'a>(
    catalog: &mut Catalog,
    authority: RunAuthority<'a>,
    limits: ArtifactLimits,
    work: WorkLimit,
    stop: &dyn Fn() -> Result<bool>,
    report: &mut ProgressReporter<'_>,
    factory: Box<dyn ArtifactFactory + 'a>,
) -> Result<Output> {
    work.validate()?;
    limits.validate()?;
    let mut progress = {
        let _phase = super::repair_memory::phase();
        catalog.begin_selected_import_reader(
            authority.source,
            authority.approval,
            authority.policy,
        )?
    };
    let mut worker = {
        let _phase = super::repair_memory::phase();
        Worker::with_readers(authority.source, &progress.id, limits, factory)?
    };
    let started = Instant::now();
    let deadline = started + Duration::from_secs(work.seconds);
    let mut stopped = stop()?;
    let mut last_report = Instant::now();
    let mut steps = 0;
    let mut needs_decision = None;
    while !progress.complete && steps < work.steps && Instant::now() < deadline && !stopped {
        let step = {
            let _phase = super::repair_memory::phase();
            worker.step(catalog, &|| false)?
        };
        steps += 1;
        progress = step.progress;
        needs_decision = step.needs_decision;
        stopped = stop()?;
        if last_report.elapsed() >= Duration::from_secs(5) || needs_decision.is_some() {
            eprintln!(
                "{:?}: capture {}, processed {}, steps {}",
                progress.stage, progress.capture_index, progress.processed, steps
            );
            report(&format!("{:?}", progress.stage), progress.processed, None)?;
            last_report = Instant::now();
        }
        if needs_decision.is_some() {
            break;
        }
    }
    let state = if progress.complete {
        "complete"
    } else if needs_decision.is_some() {
        "needs_decision"
    } else if stopped {
        "stopped"
    } else {
        "paused"
    };
    Ok(Output::Run(RunResult {
        protocol: 1,
        status: state,
        progress,
        steps,
        elapsed_seconds: started.elapsed().as_secs_f64(),
        needs_decision,
        adobe_rendering_equivalent: false,
        native_collection_order_equivalent: false,
    }))
}

pub fn run_local(
    catalog: &mut Catalog,
    source: &MigrationSource,
    approval: &[u8],
    policy: &Policy,
    limits: ArtifactLimits,
    work: WorkLimit,
    stop: &dyn Fn() -> Result<bool>,
) -> Result<Output> {
    let mut report = |_: &str, _: u64, _: Option<u64>| Ok(());
    run_with_factory(
        catalog,
        RunAuthority {
            source,
            approval,
            policy,
        },
        limits,
        work,
        stop,
        &mut report,
        Box::new(LocalArtifacts),
    )
}

pub(crate) fn run_managed<'a>(
    catalog: &mut Catalog,
    authority: RunAuthority<'a>,
    limits: ArtifactLimits,
    work: WorkLimit,
    stop: &dyn Fn() -> Result<bool>,
    report: &mut ProgressReporter<'_>,
    factory: Box<dyn ArtifactFactory + 'a>,
) -> Result<Output> {
    run_with_factory(catalog, authority, limits, work, stop, report, factory)
}

fn repair_current_reader(
    catalog: &mut Catalog,
    source: &dyn MigrationRead,
    request: &current_repair::Request,
    work: WorkLimit,
    stop: &dyn Fn() -> Result<bool>,
    report: &mut ProgressReporter<'_>,
) -> Result<Output> {
    work.validate()?;
    let mut progress = {
        let _phase = super::repair_memory::phase();
        catalog.begin_current_develop_repair_reader(source, request)?
    };
    let started = Instant::now();
    let deadline = started + Duration::from_secs(work.seconds);
    let mut steps = 0;
    let mut stopped = stop()?;
    let mut last_report = Instant::now();
    while !progress.complete && steps < work.steps && Instant::now() < deadline && !stopped {
        progress = {
            let _phase = super::repair_memory::phase();
            catalog
                .step_current_develop_repair_reader(source, &progress.id)?
                .progress
        };
        steps += 1;
        stopped = stop()?;
        if last_report.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "{:?}: examined {}, repaired {}, steps {}",
                progress.phase, progress.examined, progress.repaired, steps
            );
            report(&format!("{:?}", progress.phase), progress.examined, None)?;
            last_report = Instant::now();
        }
    }
    Ok(Output::CurrentRepair(CurrentRepairResult {
        protocol: 1,
        status: status(progress.complete, stopped),
        repair: progress,
        steps,
        elapsed_seconds: started.elapsed().as_secs_f64(),
        adobe_rendering_equivalent: false,
    }))
}

pub fn repair_current_local(
    catalog: &mut Catalog,
    source: &MigrationSource,
    request: &current_repair::Request,
    work: WorkLimit,
    stop: &dyn Fn() -> Result<bool>,
) -> Result<Output> {
    let mut report = |_: &str, _: u64, _: Option<u64>| Ok(());
    repair_current_reader(catalog, source, request, work, stop, &mut report)
}
pub(crate) fn repair_current_managed(
    catalog: &mut Catalog,
    source: &dyn MigrationRead,
    request: &current_repair::Request,
    work: WorkLimit,
    stop: &dyn Fn() -> Result<bool>,
    report: &mut ProgressReporter<'_>,
) -> Result<Output> {
    repair_current_reader(catalog, source, request, work, stop, report)
}

fn repair_keywords_reader(
    catalog: &mut Catalog,
    source: &dyn MigrationRead,
    request: &keyword_repair::Request,
    work: WorkLimit,
    stop: &dyn Fn() -> Result<bool>,
    report: &mut ProgressReporter<'_>,
) -> Result<Output> {
    work.validate()?;
    let mut progress = {
        let _phase = super::repair_memory::phase();
        catalog.begin_keyword_repair_reader(source, request)?
    };
    let started = Instant::now();
    let deadline = started + Duration::from_secs(work.seconds);
    let mut steps = 0;
    let mut stopped = stop()?;
    let mut last_report = Instant::now();
    while !progress.complete && steps < work.steps && Instant::now() < deadline && !stopped {
        progress = {
            let _phase = super::repair_memory::phase();
            catalog
                .step_keyword_repair_reader(source, &progress.id)?
                .progress
        };
        steps += 1;
        stopped = stop()?;
        if last_report.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "{:?}: examined {}, repaired {}, steps {}",
                progress.phase, progress.examined, progress.repaired, steps
            );
            report(
                &format!("{:?}", progress.phase),
                progress.examined as u64,
                None,
            )?;
            last_report = Instant::now();
        }
    }
    Ok(Output::KeywordRepair(KeywordRepairResult {
        protocol: 1,
        status: status(progress.complete, stopped),
        repair: progress,
        steps,
        elapsed_seconds: started.elapsed().as_secs_f64(),
        adobe_rendering_equivalent: false,
    }))
}

pub fn repair_keywords_local(
    catalog: &mut Catalog,
    source: &MigrationSource,
    request: &keyword_repair::Request,
    work: WorkLimit,
    stop: &dyn Fn() -> Result<bool>,
) -> Result<Output> {
    let mut report = |_: &str, _: u64, _: Option<u64>| Ok(());
    repair_keywords_reader(catalog, source, request, work, stop, &mut report)
}
pub(crate) fn repair_keywords_managed(
    catalog: &mut Catalog,
    source: &dyn MigrationRead,
    request: &keyword_repair::Request,
    work: WorkLimit,
    stop: &dyn Fn() -> Result<bool>,
    report: &mut ProgressReporter<'_>,
) -> Result<Output> {
    repair_keywords_reader(catalog, source, request, work, stop, report)
}

fn prepare_supplements_with_report(
    catalog: &mut Catalog,
    requests: &[supplements::Request],
    stop: &std::sync::atomic::AtomicBool,
    report: &mut ProgressReporter<'_>,
) -> Result<Output> {
    ensure!(
        !requests.is_empty() && requests.len() <= 1024,
        "supplement request roster bound"
    );
    let mut prepared = Vec::with_capacity(requests.len());
    for request in requests {
        prepared.push(catalog.prepare_migration_supplement(request, stop)?);
        eprintln!(
            "Prepared supplemental proof {}/{}",
            prepared.len(),
            requests.len()
        );
        report(
            "supplements",
            prepared.len() as u64,
            Some(requests.len() as u64),
        )?;
    }
    Ok(Output::Supplements(prepared))
}

pub fn prepare_supplements(
    catalog: &mut Catalog,
    requests: &[supplements::Request],
    stop: &std::sync::atomic::AtomicBool,
) -> Result<Output> {
    let mut report = |_: &str, _: u64, _: Option<u64>| Ok(());
    prepare_supplements_with_report(catalog, requests, stop, &mut report)
}

pub(crate) fn prepare_supplements_managed(
    catalog: &mut Catalog,
    requests: &[supplements::Request],
    stop: &std::sync::atomic::AtomicBool,
    report: &mut ProgressReporter<'_>,
) -> Result<Output> {
    prepare_supplements_with_report(catalog, requests, stop, report)
}

pub fn status_database(destination: &std::path::Path) -> Result<Connection> {
    let db = Connection::open_with_flags(
        destination.join("catalog.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let app: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        app == 0x50484341 && schema == crate::CURRENT_SCHEMA_VERSION,
        "status requires a current LensWorks catalog; no schema migration was performed"
    );
    Ok(db)
}

pub fn run_status(db: &Connection, run: &str) -> Result<Output> {
    Ok(Output::Status(run_progress(db, run)?))
}
pub fn run_progress(db: &Connection, run: &str) -> Result<Progress> {
    ensure!(
        run.len() == 64 && run.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid run identity"
    );
    let raw: Vec<u8> = db.query_row(
        "SELECT progress FROM migration_runs WHERE id=?1 AND length(CAST(progress AS BLOB))<=?2",
        rusqlite::params![run, i64::try_from(DOCUMENT_BYTES)?],
        |r| r.get(0),
    )?;
    let progress: Progress = serde_json::from_slice(&raw)?;
    ensure!(progress.id == run, "stored run identity differs");
    Ok(progress)
}
pub fn current_repair_status(db: &Connection, repair: &str) -> Result<Output> {
    Ok(Output::CurrentRepairStatus(current_repair::read_progress(
        db, repair,
    )?))
}
pub fn keyword_repair_status(db: &Connection, repair: &str) -> Result<Output> {
    Ok(Output::KeywordRepairStatus(keyword_repair::read_progress(
        db, repair,
    )?))
}

fn validate_current_request(request: &current_repair::Request) -> Result<()> {
    let hash = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    ensure!(
        hash(&request.run)
            && hash(&request.expected_complete_progress_blake3)
            && request.expected_mapping_epoch >= 0
            && !request.reason.trim().is_empty()
            && request.reason.len() <= 4096,
        "invalid repair request"
    );
    Ok(())
}

pub fn preflight_current_upgrade(
    destination: &std::path::Path,
    input: &str,
    request: &current_repair::Request,
) -> Result<()> {
    validate_current_request(request)?;
    let path = destination.join("catalog.sqlite3");
    ensure!(
        std::fs::symlink_metadata(&path)?.is_file(),
        "repair database must be a regular file"
    );
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    preflight_current_database(&db, input, request)
}

pub(crate) fn preflight_current_database(
    db: &Connection,
    input: &str,
    request: &current_repair::Request,
) -> Result<()> {
    validate_current_request(request)?;
    let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let schema: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        app == 0x50484341 && (7..=crate::CURRENT_SCHEMA_VERSION).contains(&schema),
        "repair requires a completed-import catalog schema"
    );
    if schema == 7 {
        let raw: Vec<u8> = db.query_row(
            "SELECT progress FROM migration_runs WHERE id=?1 AND length(CAST(progress AS BLOB))<=?2",
            rusqlite::params![request.run, i64::try_from(DOCUMENT_BYTES)?],
            |r| r.get(0),
        )?;
        let progress: Progress = serde_json::from_slice(&raw)?;
        let epoch: i64 = db.query_row(
            "SELECT epoch FROM migration_mapping_epoch WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            progress.id == request.run
                && progress.input == input
                && progress.complete
                && progress.stage == super::importer::Stage::Complete
                && blake3::hash(&raw).to_hex().as_str()
                    == request.expected_complete_progress_blake3
                && epoch == request.expected_mapping_epoch,
            "legacy repair predecessor differs"
        );
    }
    Ok(())
}

pub fn preflight_keyword_upgrade(
    destination: &std::path::Path,
    input: &str,
    request: &keyword_repair::Request,
) -> Result<()> {
    let path = destination.join("catalog.sqlite3");
    ensure!(
        std::fs::symlink_metadata(&path)?.is_file(),
        "keyword repair database must be a regular file"
    );
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    preflight_keyword_database(&db, input, request)
}

pub(crate) fn preflight_keyword_database(
    db: &Connection,
    input: &str,
    request: &keyword_repair::Request,
) -> Result<()> {
    db.busy_timeout(Duration::from_secs(5))?;
    let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let schema: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        app == 0x50484341 && (9..=crate::CURRENT_SCHEMA_VERSION).contains(&schema),
        "keyword repair requires schema9 or current schema"
    );
    keyword_repair::preflight(db, input, request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_migration::importer::Stage;

    #[test]
    fn lm_executor_batch3_direct_status_and_supplement_output_shapes_are_unchanged() -> Result<()> {
        let progress = Progress {
            id: "a".repeat(64),
            input: "b".repeat(64),
            stage: Stage::Complete,
            capture_index: 2,
            artifact_index: 3,
            cursor: None,
            processed: 4,
            complete: true,
        };
        assert_eq!(
            serde_json::to_value(Output::Status(progress.clone()))?,
            serde_json::to_value(&progress)?
        );
        assert_eq!(
            serde_json::to_value(Output::Supplements(vec![]))?,
            serde_json::json!([])
        );
        let run = Output::Run(RunResult {
            protocol: 1,
            status: "complete",
            progress,
            steps: 7,
            elapsed_seconds: 1.5,
            needs_decision: None,
            adobe_rendering_equivalent: false,
            native_collection_order_equivalent: false,
        });
        let encoded = serde_json::to_value(run)?;
        assert_eq!(encoded["protocol"], 1);
        assert_eq!(encoded["status"], "complete");
        assert_eq!(encoded["steps"], 7);
        assert_eq!(encoded["adobe_rendering_equivalent"], false);
        assert_eq!(encoded["native_collection_order_equivalent"], false);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn lm_executor_batch3_shared_destination_disjointness_resolves_aliases_and_missing_suffixes()
    -> Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        std::fs::create_dir(&source)?;
        let alias = temp.path().join("source-alias");
        symlink(&source, &alias)?;
        let nested = destination_path(&alias.join("new/catalog"))?;
        assert!(disjoint(&nested, &source).is_err());
        assert!(disjoint(&source.canonicalize()?, &alias).is_err());
        let separate = destination_path(&temp.path().join("separate/catalog"))?;
        disjoint(&separate, &source)?;
        let file = temp.path().join("file");
        fs::write(&file, b"preserved")?;
        let file_alias = temp.path().join("file-alias");
        symlink(&file, &file_alias)?;
        for target in [&file, &file_alias] {
            assert_eq!(
                destination_path(target).unwrap_err().to_string(),
                "destination ancestor must be a directory"
            );
            assert!(destination_path(&target.join("new/catalog")).is_err());
        }
        assert_eq!(fs::read(&file)?, b"preserved");
        Ok(())
    }
}
