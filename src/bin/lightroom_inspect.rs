//! Standalone inspection CLI: never opens an application catalog.
use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use photocatalog::{
    lightroom::{
        self, Limits,
        capture::{self, Request},
        discovery,
        plan::{PLAN_SCHEMA_VERSION, Plan},
    },
    storage_volume::NativePath,
};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::PathBuf,
};

#[derive(Parser)]
#[command(about = "Read-only Lightroom preservation and migration dry-run tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Discover explicit candidates. Filename grouping remains provisional.
    Discover {
        root: PathBuf,
        #[arg(long, default_value_t = 16384)]
        max_entries: usize,
        #[arg(long, default_value_t = 16)]
        max_depth: usize,
    },
    /// Capture into a new directory outside the source tree; raw bytes are retained.
    Capture {
        source: PathBuf,
        output: PathBuf,
        #[arg(long)]
        main_only: bool,
        #[arg(long)]
        closed_application_evidence: Option<String>,
    },
    #[command(hide = true)]
    CaptureWorker,
    /// Create a separate inspection plan, never a PhotoCatalog catalog.
    Create {
        plan: PathBuf,
    },
    RegisterInventory {
        plan: PathBuf,
        inventory: PathBuf,
    },
    Add {
        plan: PathBuf,
        capture: PathBuf,
    },
    Resume {
        plan: PathBuf,
        revision: String,
        #[arg(long, default_value_t = 1000)]
        max_rows: usize,
    },
    Rows {
        plan: PathBuf,
        revision: String,
        #[arg(long)]
        table: Option<String>,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Report {
        plan: PathBuf,
        revision: String,
    },
    CheckPaths {
        plan: PathBuf,
        revision: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        packets: bool,
    },
    Paths {
        plan: PathBuf,
        revision: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Issues {
        plan: PathBuf,
        revision: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Packets {
        plan: PathBuf,
        revision: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    PacketBytes {
        plan: PathBuf,
        revision: String,
        sequence: i64,
        #[arg(long)]
        decoded: bool,
        #[arg(long, default_value_t = 0)]
        offset: i64,
        #[arg(long, default_value_t = 65536)]
        limit: usize,
    },
    MetadataConflicts {
        plan: PathBuf,
        revision: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Page shared global-ID conflicts for an explicitly selected revision pair.
    GlobalIdConflicts {
        plan: PathBuf,
        left: String,
        right: String,
        #[arg(long, default_value = "")]
        after_left: String,
        #[arg(long, default_value = "")]
        after_right: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    PathCollisions {
        plan: PathBuf,
        left: String,
        right: String,
        #[arg(long, default_value_t = 0)]
        after_left: i64,
        #[arg(long, default_value_t = 0)]
        after_right: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Families {
        plan: PathBuf,
    },
    AssignFamily {
        plan: PathBuf,
        revision: String,
        family: String,
        #[arg(long)]
        reason: String,
    },
    Choose {
        plan: PathBuf,
        family: String,
        revision: String,
        #[arg(long)]
        expected_evidence: String,
        #[arg(long)]
        reason: String,
    },
}
fn emit(value: &impl serde::Serialize) -> Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    // Page accounting uses compact JSON and reserves the final newline.
    serde_json::to_writer(&mut lock, value)?;
    writeln!(lock)?;
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(mut reader: impl Read) -> Result<T> {
    let mut bytes = vec![];
    reader
        .by_ref()
        .take(lightroom::MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= lightroom::MANIFEST_BYTES,
        "JSON input exceeds shared byte budget"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Discover {
            root,
            max_entries,
            max_depth,
        } => emit(&discovery::discover(
            &root,
            &Limits {
                max_files: max_entries,
                max_depth,
                ..Default::default()
            },
        )?)?,
        Command::Capture {
            source,
            output,
            main_only,
            closed_application_evidence,
        } => {
            let request = Request {
                source: NativePath::from_path(&source),
                output: NativePath::from_path(&output),
                include_auxiliary: !main_only,
                closed_application_evidence,
                limits: Limits::default(),
            };
            let report = capture::spawn(&std::env::current_exe()?, &request)?;
            emit(&report)?;
            ensure!(
                report.state == "captured",
                "capture failed; retained diagnostics are in the exclusive output directory"
            );
        }
        Command::CaptureWorker => {
            // This process has no SQLite connections before acquiring source locks.
            let request: Request = read_json(io::stdin().lock())?;
            emit(&capture::run_isolated(request)?)?;
        }
        Command::Create { plan } => {
            Plan::create(&plan)?;
            emit(
                &serde_json::json!({"plan":NativePath::from_path(&plan),"schema":PLAN_SCHEMA_VERSION}),
            )?;
        }
        Command::RegisterInventory { plan, inventory } => emit(
            &serde_json::json!({"inventory_digest":Plan::open(&plan)?.register_inventory(&read_json(File::open(inventory)?)?)?}),
        )?,
        Command::Add { plan, capture } => {
            emit(&serde_json::json!({"revision":Plan::open(&plan)?.add_capture(&capture)?}))?
        }
        Command::Resume {
            plan,
            revision,
            max_rows,
        } => emit(&Plan::open(&plan)?.resume(&revision, max_rows)?)?,
        Command::Rows {
            plan,
            revision,
            table,
            after,
            limit,
        } => emit(&Plan::open(&plan)?.rows(&revision, table.as_deref(), after, limit)?)?,
        Command::Report { plan, revision } => emit(&Plan::open(&plan)?.report(&revision)?)?,
        Command::CheckPaths {
            plan,
            revision,
            limit,
            packets,
        } => emit(
            &serde_json::json!({"checked":Plan::open(&plan)?.check_paths(&revision,limit,packets)?}),
        )?,
        Command::Paths {
            plan,
            revision,
            after,
            limit,
        } => emit(&Plan::open(&plan)?.paths(&revision, after, limit)?)?,
        Command::Issues {
            plan,
            revision,
            after,
            limit,
        } => emit(&Plan::open(&plan)?.issues(&revision, after, limit)?)?,
        Command::Packets {
            plan,
            revision,
            after,
            limit,
        } => emit(&Plan::open(&plan)?.packets(&revision, after, limit)?)?,
        Command::PacketBytes {
            plan,
            revision,
            sequence,
            decoded,
            offset,
            limit,
        } => emit(&Plan::open(&plan)?.packet_bytes(&revision, sequence, decoded, offset, limit)?)?,
        Command::MetadataConflicts {
            plan,
            revision,
            after,
            limit,
        } => emit(&Plan::open(&plan)?.metadata_conflicts(&revision, after, limit)?)?,
        Command::GlobalIdConflicts {
            plan,
            left,
            right,
            after_left,
            after_right,
            limit,
        } => emit(&Plan::open(&plan)?.global_id_conflicts(
            &left,
            &right,
            &after_left,
            &after_right,
            limit,
        )?)?,
        Command::PathCollisions {
            plan,
            left,
            right,
            after_left,
            after_right,
            limit,
        } => emit(&Plan::open(&plan)?.path_collisions(
            &left,
            &right,
            after_left,
            after_right,
            limit,
        )?)?,
        Command::Families { plan } => emit(&Plan::open(&plan)?.families()?)?,
        Command::AssignFamily {
            plan,
            revision,
            family,
            reason,
        } => {
            Plan::open(&plan)?.assign_family(&revision, &family, &reason)?;
            emit(&serde_json::json!({"assigned":family}))?;
        }
        Command::Choose {
            plan,
            family,
            revision,
            expected_evidence,
            reason,
        } => {
            Plan::open(&plan)?.choose(&family, &revision, &expected_evidence, &reason)?;
            emit(&serde_json::json!({"selected":revision}))?;
        }
    }
    Ok(())
}
