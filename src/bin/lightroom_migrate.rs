//! Isolated, resumable selected-catalog migration worker.
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use photocatalog::{
    Catalog,
    catalog_migration::{
        artifacts::ArtifactLimits,
        import_artifacts::Worker,
        importer::{Policy, Progress},
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
#[command(about = "Import an explicitly sealed Lightroom selection into one PhotoCatalog catalog")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
            Ok(meta) => {
                ensure!(meta.is_dir(), "destination ancestor must be a directory");
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                remaining.push(
                    ancestor
                        .file_name()
                        .context("destination has no ancestor")?,
                );
                ancestor = ancestor.parent().context("destination has no parent")?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let mut resolved = fs::canonicalize(ancestor)?;
    for part in remaining.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn disjoint(destination: &Path, source: &Path) -> Result<()> {
    let source = fs::canonicalize(source)?;
    ensure!(
        !destination.starts_with(&source) && !source.starts_with(destination),
        "destination and source custody must be separate directories"
    );
    Ok(())
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

fn read_status(destination: &Path, run: &str) -> Result<Progress> {
    ensure!(
        run.len() == 64 && run.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid run identity"
    );
    let db = rusqlite::Connection::open_with_flags(
        destination.join("catalog.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    let app: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        app == 0x50484341 && schema == photocatalog::CURRENT_SCHEMA_VERSION,
        "status requires a current PhotoCatalog catalog; no schema migration was performed"
    );
    let raw: Vec<u8> = db.query_row(
        "SELECT progress FROM migration_runs WHERE id=?1 AND length(progress)<=?2",
        rusqlite::params![run, i64::try_from(DOCUMENT_BYTES)?],
        |r| r.get(0),
    )?;
    let progress: Progress = serde_json::from_slice(&raw)?;
    ensure!(progress.id == run, "stored run identity differs");
    Ok(progress)
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
            let mut prepared = Vec::with_capacity(requests.len());
            for request in &requests {
                prepared.push(catalog.prepare_migration_supplement(request, &stop)?);
                eprintln!(
                    "Prepared supplemental proof {}/{}",
                    prepared.len(),
                    requests.len()
                );
            }
            emit(&prepared)
        }
        Command::Status { destination, run } => {
            let destination = destination_path(&destination)?;
            ensure!(
                destination.join("catalog.sqlite3").is_file(),
                "destination catalog is absent"
            );
            let progress = read_status(&destination, &run)?;
            emit(&progress)
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
            )?;
            let _lock = lock_destination(&destination)?;
            let mut catalog = Catalog::open(&destination)?;
            let mut progress: Progress =
                catalog.begin_selected_import(&source, &approval, &policy)?;
            let mut worker = Worker::new(
                &source,
                &progress.id,
                ArtifactLimits {
                    maximum_bytes: max_artifact_bytes,
                    open_deadline_ms: artifact_open_seconds * 1000,
                    chunk_deadline_ms: 120_000,
                    chunk_bytes: 1024 * 1024,
                },
            )?;
            let started = Instant::now();
            let deadline = started + Duration::from_secs(max_seconds);
            let mut stopped = stop_requested(stop_file.as_deref())?;
            let mut last_report = Instant::now();
            let mut steps = 0;
            let mut needs_decision = None;
            while !progress.complete && steps < max_steps && Instant::now() < deadline && !stopped {
                // CLI cancellation is sampled between steps, so an admitted
                // step finishes and real read/checksum errors still propagate.
                let step = worker.step(&mut catalog, &|| false)?;
                steps += 1;
                progress = step.progress;
                needs_decision = step.needs_decision;
                stopped = stop_requested(stop_file.as_deref())?;
                if last_report.elapsed() >= Duration::from_secs(5) || needs_decision.is_some() {
                    eprintln!(
                        "{:?}: capture {}, processed {}, steps {}",
                        progress.stage, progress.capture_index, progress.processed, steps
                    );
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
            emit(&serde_json::json!({
                "protocol": 1, "status": state, "progress": progress,
                "steps": steps, "elapsed_seconds": started.elapsed().as_secs_f64(),
                "needs_decision": needs_decision,
                "adobe_rendering_equivalent": false,
                "native_collection_order_equivalent": false
            }))
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
        assert!(destination_path(&root.join("missing/../catalog")).is_err());
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
            db.execute_batch("PRAGMA application_id=1346913089; PRAGMA user_version=7; CREATE TABLE migration_runs(id TEXT PRIMARY KEY,progress BLOB);")?;
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
}
