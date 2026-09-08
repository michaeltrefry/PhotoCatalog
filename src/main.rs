use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use photocatalog::Catalog;
use std::{io::Write, path::PathBuf};
#[derive(Parser)]
#[command(
    version,
    about = "PhotoCatalog Rust foundation (provisional SQLite catalog)"
)]
struct Cli {
    #[arg(long)]
    catalog: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Import CR2/JPEG/PNG recursively; originals are read only. Repeat to resume.
    Import {
        folder: PathBuf,
        #[arg(long)]
        max_files: Option<usize>,
    },
    /// Browse stable keyset pages, including pending and failed entries.
    Browse {
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Get {
        id: String,
    },
    /// Write an intact cached JPEG to a new output file, even with originals offline.
    Preview {
        id: String,
        output: PathBuf,
    },
}
fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut catalog = match &cli.command {
        Command::Import { folder, .. } => Catalog::open_for_import(&cli.catalog, folder)?,
        _ => Catalog::open(&cli.catalog)?,
    };
    match cli.command {
        Command::Import { folder, max_files } => {
            let report = catalog.import(folder, max_files, |_| Ok(()))?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            ensure!(
                report.failed == 0,
                "{} imports failed; inspect browse output and retry",
                report.failed
            );
        }
        Command::Browse { after, limit } => println!(
            "{}",
            serde_json::to_string_pretty(&catalog.browse(after, limit)?)?
        ),
        Command::Get { id } => println!("{}", serde_json::to_string_pretty(&catalog.get(&id)?)?),
        Command::Preview { id, output } => {
            let preview = catalog.preview(&id)?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            file.write_all(&preview)?;
            file.sync_all()?;
        }
    }
    Ok(())
}
