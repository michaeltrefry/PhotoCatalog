//! Isolated, resumable selected-catalog migration worker.
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use photocatalog::{
    Catalog,
    catalog_migration::{
        artifacts::ArtifactLimits,
        current_repair,
        importer::{Policy, Progress},
        keyword_repair,
        lightroom_executor::{self, WorkLimit},
    },
    lightroom::migration_source::{InputSeal, MigrationSource, ReadLimits},
};
use serde::de::DeserializeOwned;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Parser)]
#[command(about = "Import an explicitly sealed Lightroom selection into one LensWorks catalog")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Correct current-settings container selection in an explicitly pinned completed import.
    RepairCurrent {
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        seal: PathBuf,
        #[arg(long)]
        approval: PathBuf,
        #[arg(long)]
        request: PathBuf,
        #[arg(long, default_value_t = 1000)]
        max_steps: u64,
        #[arg(long, default_value_t = 60)]
        max_seconds: u64,
        #[arg(long)]
        stop_file: Option<PathBuf>,
        #[arg(long, default_value_t = 600)]
        source_open_seconds: u64,
    },
    /// Read a repair checkpoint without opening the inspection or upgrading the catalog.
    RepairStatus {
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        repair: String,
    },
    /// Correct keyword hierarchy and memberships in an explicitly pinned completed import.
    RepairKeywords {
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        seal: PathBuf,
        #[arg(long)]
        approval: PathBuf,
        #[arg(long)]
        request: PathBuf,
        #[arg(long, default_value_t = 1000)]
        max_steps: u64,
        #[arg(long, default_value_t = 60)]
        max_seconds: u64,
        #[arg(long)]
        stop_file: Option<PathBuf>,
        #[arg(long, default_value_t = 600)]
        source_open_seconds: u64,
    },
    /// Read a repair checkpoint without opening the inspection or upgrading the catalog.
    KeywordRepairStatus {
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        repair: String,
    },
    /// Run bounded work; repeat with identical inputs to resume its saved checkpoint.
    Run {
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        seal: PathBuf,
        #[arg(long)]
        approval: PathBuf,
        #[arg(long)]
        policy: PathBuf,
        #[arg(long, default_value_t = 1000)]
        max_steps: u64,
        /// Work budget after source admission. An active step finishes atomically.
        #[arg(long, default_value_t = 60)]
        max_seconds: u64,
        /// Create this file to request a clean stop at a bounded step boundary.
        #[arg(long)]
        stop_file: Option<PathBuf>,
        #[arg(long, default_value_t = 600)]
        source_open_seconds: u64,
        #[arg(long, default_value_t = 600)]
        artifact_open_seconds: u64,
        #[arg(long, default_value_t = 64 * 1024 * 1024 * 1024)]
        max_artifact_bytes: u64,
    },
    /// Read a destination checkpoint without opening the inspection or originals.
    Status {
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        run: String,
    },
    /// Retain qualified successor proof copies; returns pins for a separately approved seal.
    PrepareSupplements {
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        requests: PathBuf,
    },
}

fn bytes(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "input must be a regular file"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000).share_mode(1);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= DOCUMENT_BYTES,
        "input must be a regular file of at most 16 MiB"
    );
    let mut value = Vec::new();
    file.take(DOCUMENT_BYTES + 1).read_to_end(&mut value)?;
    ensure!(
        value.len() as u64 <= DOCUMENT_BYTES,
        "input document exceeds 16 MiB"
    );
    Ok(value)
}
fn document<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&bytes(path)?).with_context(|| format!("read {}", path.display()))
}

/// Resolve only the existing ancestor. A source directory need not be online.
fn destination_path(path: &Path) -> Result<PathBuf> {
    lightroom_executor::destination_path(path)
}

fn disjoint(destination: &Path, source: &Path) -> Result<()> {
    lightroom_executor::disjoint(destination, source)
}

fn lock_destination(destination: &Path) -> Result<File> {
    fs::create_dir_all(destination)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(destination.join(".lightroom-import.lock"))?;
    lock.try_lock_exclusive()
        .context("another migration worker owns this destination")?;
    Ok(lock)
}

fn emit(value: &impl serde::Serialize) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

fn status_database(destination: &Path) -> Result<rusqlite::Connection> {
    lightroom_executor::status_database(destination)
}

fn read_status(destination: &Path, run: &str) -> Result<Progress> {
    ensure!(
        run.len() == 64 && run.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid run identity"
    );
    let db = status_database(destination)?;
    lightroom_executor::run_progress(&db, run)
}

// Reject a stale repair request before upgrading an existing schema7 destination.
// The repair API repeats its state checks under the writer transaction.
fn preflight_repair_upgrade(
    destination: &Path,
    input: &str,
    request: &current_repair::Request,
) -> Result<()> {
    lightroom_executor::preflight_current_upgrade(destination, input, request)
}

fn preflight_keyword_upgrade(
    destination: &Path,
    input: &str,
    request: &keyword_repair::Request,
) -> Result<()> {
    lightroom_executor::preflight_keyword_upgrade(destination, input, request)
}

fn stop_requested(path: Option<&Path>) -> Result<bool> {
    let Some(path) = path else {
        return Ok(false);
    };
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("read migration stop request"),
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::RepairStatus {
            destination,
            repair,
        } => {
            let destination = destination_path(&destination)?;
            let db = status_database(&destination)?;
            emit(&lightroom_executor::current_repair_status(&db, &repair)?)
        }
        Command::RepairCurrent {
            destination,
            seal,
            approval,
            request,
            max_steps,
            max_seconds,
            stop_file,
            source_open_seconds,
        } => {
            ensure!(
                max_steps > 0 && (1..=86400).contains(&max_seconds),
                "invalid work budget"
            );
            ensure!(
                (1..=3600).contains(&source_open_seconds),
                "invalid source admission budget"
            );
            let seal: InputSeal = document(&seal)?;
            let approval = bytes(&approval)?;
            ensure!(
                blake3::hash(&approval).to_hex().as_str() == seal.approval.document_blake3,
                "authorization bytes differ from seal"
            );
            let request: current_repair::Request = document(&request)?;
            let destination = destination_path(&destination)?;
            ensure!(
                destination.join("catalog.sqlite3").is_file(),
                "repair requires an existing catalog"
            );
            let database = seal.database.to_path()?;
            disjoint(
                &destination,
                database.parent().context("inspection has no parent")?,
            )?;
            eprintln!("Verifying sealed inspection before current-settings repair...");
            let source = MigrationSource::open(
                seal,
                ReadLimits {
                    open_deadline_ms: source_open_seconds * 1000,
                    ..ReadLimits::default()
                },
            )
            .context("open and admit sealed inspection source")?;
            let _lock = lock_destination(&destination)?;
            preflight_repair_upgrade(&destination, source.binding_blake3(), &request)
                .context("preflight current-settings repair destination")?;
            let mut catalog =
                Catalog::open(&destination).context("open current-settings repair destination")?;
            emit(
                &lightroom_executor::repair_current_local(
                    &mut catalog,
                    &source,
                    &request,
                    WorkLimit {
                        steps: max_steps,
                        seconds: max_seconds,
                    },
                    &|| stop_requested(stop_file.as_deref()),
                )
                .context("execute current-settings repair")?,
            )
        }
        Command::KeywordRepairStatus {
            destination,
            repair,
        } => {
            let destination = destination_path(&destination)?;
            let db = status_database(&destination)?;
            emit(&lightroom_executor::keyword_repair_status(&db, &repair)?)
        }
        Command::RepairKeywords {
            destination,
            seal,
            approval,
            request,
            max_steps,
            max_seconds,
            stop_file,
            source_open_seconds,
        } => {
            ensure!(
                max_steps > 0 && (1..=86400).contains(&max_seconds),
                "invalid work budget"
            );
            ensure!(
                (1..=3600).contains(&source_open_seconds),
                "invalid source admission budget"
            );
            let seal: InputSeal = document(&seal)?;
            let approval = bytes(&approval)?;
            ensure!(
                blake3::hash(&approval).to_hex().as_str() == seal.approval.document_blake3,
                "authorization bytes differ from seal"
            );
            let request: keyword_repair::Request = document(&request)?;
            let destination = destination_path(&destination)?;
            ensure!(
                destination.join("catalog.sqlite3").is_file(),
                "repair requires an existing catalog"
            );
            let database = seal.database.to_path()?;
            disjoint(
                &destination,
                database.parent().context("inspection has no parent")?,
            )?;
            eprintln!("Verifying sealed inspection before keyword repair...");
            let source = MigrationSource::open(
                seal,
                ReadLimits {
                    open_deadline_ms: source_open_seconds * 1000,
                    ..ReadLimits::default()
                },
            )
            .context("open and admit sealed inspection source")?;
            let _lock = lock_destination(&destination)?;
            preflight_keyword_upgrade(&destination, source.binding_blake3(), &request)
                .context("preflight keyword repair destination")?;
            let mut catalog =
                Catalog::open(&destination).context("open keyword repair destination")?;
            emit(
                &lightroom_executor::repair_keywords_local(
                    &mut catalog,
                    &source,
                    &request,
                    WorkLimit {
                        steps: max_steps,
                        seconds: max_seconds,
                    },
                    &|| stop_requested(stop_file.as_deref()),
                )
                .context("execute keyword repair")?,
            )
        }
        Command::PrepareSupplements {
            destination,
            requests,
        } => {
            use photocatalog::catalog_migration::supplements::Request;
            let requests: Vec<Request> = document(&requests)?;
            ensure!(
                !requests.is_empty() && requests.len() <= 1024,
                "supplement request roster bound"
            );
            let destination = destination_path(&destination)?;
            for request in &requests {
                disjoint(&destination, &request.proof_root.to_path()?)?;
            }
            let _lock = lock_destination(&destination)?;
            let mut catalog = Catalog::open(&destination)?;
            let stop = std::sync::atomic::AtomicBool::new(false);
            emit(&lightroom_executor::prepare_supplements(
                &mut catalog,
                &requests,
                &stop,
            )?)
        }
        Command::Status { destination, run } => {
            let destination = destination_path(&destination)?;
            ensure!(
                destination.join("catalog.sqlite3").is_file(),
                "destination catalog is absent"
            );
            let db = status_database(&destination)?;
            emit(&lightroom_executor::run_status(&db, &run)?)
        }
        Command::Run {
            destination,
            seal,
            approval,
            policy,
            max_steps,
            max_seconds,
            stop_file,
            source_open_seconds,
            artifact_open_seconds,
            max_artifact_bytes,
        } => {
            ensure!(
                max_steps > 0 && (1..=86400).contains(&max_seconds),
                "invalid work budget"
            );
            ensure!(
                (1..=3600).contains(&source_open_seconds),
                "invalid source admission budget"
            );
            ensure!(
                (1..=3600).contains(&artifact_open_seconds),
                "invalid artifact admission budget"
            );
            ensure!(
                (1..=i64::MAX as u64).contains(&max_artifact_bytes),
                "invalid artifact byte bound"
            );
            let seal: InputSeal = document(&seal)?;
            let approval = bytes(&approval)?;
            ensure!(
                blake3::hash(&approval).to_hex().as_str() == seal.approval.document_blake3,
                "authorization bytes differ from seal"
            );
            let policy: Policy = document(&policy)?;
            let destination = destination_path(&destination)?;
            let database = seal.database.to_path()?;
            disjoint(
                &destination,
                database.parent().context("inspection has no parent")?,
            )?;
            for artifact in &policy.artifacts {
                disjoint(&destination, &artifact.mapping.root.to_path()?)?;
            }
            // Take no destination write authority until exact selected input admission succeeds.
            eprintln!("Verifying sealed inspection bytes and selected catalog authorization...");
            let source = MigrationSource::open(
                seal,
                ReadLimits {
                    open_deadline_ms: source_open_seconds * 1000,
                    ..ReadLimits::default()
                },
            )
            .context("open and admit sealed inspection source")?;
            let _lock = lock_destination(&destination)?;
            let mut catalog = Catalog::open(&destination)?;
            emit(&lightroom_executor::run_local(
                &mut catalog,
                &source,
                &approval,
                &policy,
                ArtifactLimits {
                    maximum_bytes: max_artifact_bytes,
                    open_deadline_ms: artifact_open_seconds * 1000,
                    chunk_deadline_ms: 120_000,
                    chunk_bytes: 1024 * 1024,
                },
                WorkLimit {
                    steps: max_steps,
                    seconds: max_seconds,
                },
                &|| stop_requested(stop_file.as_deref()),
            )?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_cannot_enclose_or_enter_source_custody() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let source = root.join("capture");
        fs::create_dir(&source)?;
        assert!(disjoint(&root, &source).is_err());
        assert!(disjoint(&source.join("catalog"), &source).is_err());
        disjoint(&root.join("test-catalog"), &source)?;
        assert!(destination_path(Path::new("relative/catalog")).is_err());
        // PathBuf::join normalizes `..` under a Windows verbatim root. Append
        // literal units so the rejection test actually supplies traversal.
        let mut traversal = root.as_os_str().to_owned();
        traversal.push(std::path::MAIN_SEPARATOR_STR);
        traversal.push("missing");
        traversal.push(std::path::MAIN_SEPARATOR_STR);
        traversal.push("..");
        traversal.push(std::path::MAIN_SEPARATOR_STR);
        traversal.push("catalog");
        let traversal = PathBuf::from(traversal);
        assert!(traversal.components().any(|c| c == Component::ParentDir));
        assert!(destination_path(&traversal).is_err());
        Ok(())
    }

    #[test]
    fn migration_worker_lock_is_exclusive_and_released_on_close() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let first = lock_destination(temp.path())?;
        assert!(lock_destination(temp.path()).is_err());
        drop(first);
        let _second = lock_destination(temp.path())?;
        Ok(())
    }

    #[test]
    fn status_does_not_upgrade_or_create_a_catalog() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("catalog.sqlite3");
        {
            let db = rusqlite::Connection::open(&path)?;
            db.execute_batch("PRAGMA application_id=1346913089; PRAGMA user_version=6; CREATE TABLE preserved(value); INSERT INTO preserved VALUES('unchanged');")?;
        }
        let before = fs::read(&path)?;
        assert!(read_status(temp.path(), &"a".repeat(64)).is_err());
        assert_eq!(before, fs::read(&path)?);
        assert!(!temp.path().join("previews").exists());
        assert!(read_status(&temp.path().join("absent"), &"a".repeat(64)).is_err());
        assert!(!temp.path().join("absent").exists());
        Ok(())
    }

    #[test]
    fn legacy_repair_preflight_rejects_stale_inputs_without_upgrading() -> Result<()> {
        use photocatalog::catalog_migration::importer::Stage;
        let temp = tempfile::tempdir()?;
        let progress = Progress {
            id: "a".repeat(64),
            input: "b".repeat(64),
            stage: Stage::Complete,
            capture_index: 1,
            artifact_index: 0,
            cursor: None,
            processed: 19,
            complete: true,
        };
        let raw = serde_json::to_vec(&progress)?;
        let path = temp.path().join("catalog.sqlite3");
        {
            let db = rusqlite::Connection::open(&path)?;
            db.execute_batch(
                "PRAGMA application_id=1346913089; PRAGMA user_version=7;
                CREATE TABLE migration_runs(id TEXT PRIMARY KEY,progress BLOB);
                CREATE TABLE migration_mapping_epoch(id INTEGER PRIMARY KEY,epoch INTEGER);
                INSERT INTO migration_mapping_epoch VALUES(1,3);",
            )?;
            db.execute(
                "INSERT INTO migration_runs VALUES(?1,?2)",
                rusqlite::params![progress.id, raw],
            )?;
        }
        let mut request = current_repair::Request {
            run: progress.id,
            expected_complete_progress_blake3: blake3::hash(&raw).to_hex().to_string(),
            expected_mapping_epoch: 3,
            reason: "Correct parsed container".into(),
        };
        // Both pre-repair schema 7 and completed-repair schema 8 must be
        // admitted read-only before an upgrade. Schema 7 also checks the original
        // Complete predecessor; later schemas use the resume-aware core guards.
        for version in [7, 8] {
            {
                let db = rusqlite::Connection::open(&path)?;
                db.pragma_update(None, "user_version", version)?;
            }
            let snapshot = fs::read(&path)?;
            preflight_repair_upgrade(temp.path(), &progress.input, &request)?;
            if version == 7 {
                assert!(preflight_repair_upgrade(temp.path(), &"c".repeat(64), &request).is_err());
            }
            assert_eq!(snapshot, fs::read(&path)?);
        }
        {
            let db = rusqlite::Connection::open(&path)?;
            db.pragma_update(None, "user_version", 7)?;
        }
        let before = fs::read(&path)?;
        preflight_repair_upgrade(temp.path(), &progress.input, &request)?;
        assert!(preflight_repair_upgrade(temp.path(), &"c".repeat(64), &request).is_err());
        request.expected_mapping_epoch = 4;
        assert!(preflight_repair_upgrade(temp.path(), &progress.input, &request).is_err());
        request.expected_mapping_epoch = 3;
        request.expected_complete_progress_blake3 = "d".repeat(64);
        assert!(preflight_repair_upgrade(temp.path(), &progress.input, &request).is_err());
        assert_eq!(before, fs::read(&path)?);
        assert!(!temp.path().join("previews").exists());
        assert!(status_database(temp.path()).is_err());
        Ok(())
    }

    #[test]
    fn status_reads_bounded_checkpoint_and_stop_request_is_explicit() -> Result<()> {
        use photocatalog::catalog_migration::importer::Stage;
        let temp = tempfile::tempdir()?;
        let progress = Progress {
            id: "a".repeat(64),
            input: "b".repeat(64),
            stage: Stage::Files,
            capture_index: 0,
            artifact_index: 0,
            cursor: None,
            processed: 19,
            complete: false,
        };
        {
            let db = rusqlite::Connection::open(temp.path().join("catalog.sqlite3"))?;
            db.execute_batch("PRAGMA application_id=1346913089; CREATE TABLE migration_runs(id TEXT PRIMARY KEY,progress BLOB);")?;
            db.pragma_update(None, "user_version", photocatalog::CURRENT_SCHEMA_VERSION)?;
            db.execute(
                "INSERT INTO migration_runs VALUES(?1,?2)",
                rusqlite::params![progress.id, serde_json::to_vec(&progress)?],
            )?;
        }
        assert_eq!(read_status(temp.path(), &progress.id)?.processed, 19);
        let stop = temp.path().join("STOP");
        assert!(!stop_requested(Some(&stop))?);
        fs::write(&stop, b"")?;
        assert!(stop_requested(Some(&stop))?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn document_input_refuses_fifo_and_symlink() -> Result<()> {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let temp = tempfile::tempdir()?;
        let pipe = temp.path().join("pipe");
        let name = std::ffi::CString::new(pipe.as_os_str().as_bytes())?;
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(bytes(&pipe).is_err());
        let file = temp.path().join("document");
        fs::write(&file, b"{}")?;
        let link = temp.path().join("link");
        symlink(&file, &link)?;
        assert!(bytes(&link).is_err());
        assert_eq!(bytes(&file)?, b"{}");
        Ok(())
    }
    #[test]
    fn keyword_cli_has_bounded_repair_and_readonly_status_routes() -> Result<()> {
        let cli = Cli::try_parse_from([
            "worker",
            "repair-keywords",
            "--destination",
            "/synthetic/catalog",
            "--seal",
            "seal.json",
            "--approval",
            "approval.json",
            "--request",
            "request.json",
        ])?;
        assert!(matches!(
            cli.command,
            Command::RepairKeywords {
                max_steps: 1000,
                max_seconds: 60,
                source_open_seconds: 600,
                ..
            }
        ));
        let cli = Cli::try_parse_from([
            "worker",
            "keyword-repair-status",
            "--destination",
            "/synthetic/catalog",
            "--repair",
            &"a".repeat(64),
        ])?;
        assert!(matches!(cli.command, Command::KeywordRepairStatus { .. }));
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("catalog.sqlite3");
        {
            let db = rusqlite::Connection::open(&path)?;
            db.execute_batch("PRAGMA application_id=1346913089; PRAGMA user_version=9; CREATE TABLE preserved(v); INSERT INTO preserved VALUES('prior state');")?;
        }
        let before = fs::read(&path)?;
        assert!(status_database(temp.path()).is_err());
        let request = keyword_repair::Request {
            expected_dictionaries: 1,
            expected_memberships: 0,
            expected_synonyms: 0,
            expected_captures: 1,
            run: "a".repeat(64),
            expected_complete_progress_blake3: "b".repeat(64),
            expected_mapping_epoch: 1,
            current_repair: "c".repeat(64),
            expected_current_repair_progress_blake3: "d".repeat(64),
            expected_roster_blake3: "e".repeat(64),
            predecessor_evidence_blake3: "f".repeat(64),
            roots: vec![],
            reason: "Bounded test".into(),
        };
        assert!(preflight_keyword_upgrade(temp.path(), &"a".repeat(64), &request).is_err());
        assert_eq!(before, fs::read(&path)?);
        assert!(!temp.path().join("previews").exists());
        Ok(())
    }
}
