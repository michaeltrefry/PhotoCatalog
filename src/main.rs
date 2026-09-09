use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use photocatalog::Catalog;
use std::{
    io::{Read, Write},
    path::PathBuf,
};
#[derive(Parser)]
#[command(version, about = "PhotoCatalog Rust catalog and metadata tools")]
struct Cli {
    #[arg(long)]
    catalog: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Import supported photos recursively; originals are read only. Repeat to resume.
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
    /// Inspect effective fields, source revisions, and unresolved conflicts.
    Metadata {
        id: String,
    },
    /// Browse retained source observations and packet/model descriptors.
    MetadataHistory {
        id: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    MetadataDecisions {
        id: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Choose a current field candidate, guarded by the displayed metadata revision.
    MetadataResolve {
        id: String,
        field: String,
        model: i64,
        #[arg(long)]
        expected_revision: i64,
    },
    /// Apply explicit JSON edit operations to a complete base model inside the catalog.
    MetadataEdit {
        id: String,
        edits: PathBuf,
        #[arg(long)]
        base_model: Option<i64>,
        #[arg(long)]
        expected_revision: i64,
    },
    /// Retain exact original carrier evidence as JSON in a new output file.
    MetadataPackets {
        id: String,
        observation: i64,
        output: PathBuf,
    },
    /// Prepare a reviewed export; reads the destination and stores the plan in the catalog.
    MetadataExportPlan {
        id: String,
        base_model: i64,
        destination: PathBuf,
        #[arg(long)]
        expected_revision: i64,
    },
    /// Explicitly publish the previously displayed operation, with revision checks.
    MetadataExportApply {
        operation: String,
    },
    /// List retained interrupted/completed export recovery directories.
    MetadataExportDiscover {
        directory: PathBuf,
    },
    /// Explicitly resume filesystem publication/recovery of a known export operation.
    MetadataExportRecover {
        directory: PathBuf,
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
        Command::Metadata { id } => print_json(&catalog.metadata(&id)?)?,
        Command::MetadataHistory { id, after, limit } => {
            print_json(&catalog.metadata_history(&id, after, limit)?)?
        }
        Command::MetadataDecisions { id, after, limit } => {
            print_json(&catalog.metadata_decisions(&id, after, limit)?)?
        }
        Command::MetadataResolve {
            id,
            field,
            model,
            expected_revision,
        } => print_json(&catalog.resolve_metadata(&id, expected_revision, &field, model)?)?,
        Command::MetadataEdit {
            id,
            edits,
            base_model,
            expected_revision,
        } => {
            let mut bytes = Vec::new();
            std::fs::File::open(edits)?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 1024 * 1024,
                "edit instruction file exceeds 1 MiB"
            );
            let edits: Vec<photocatalog::xmp::Edit> = serde_json::from_slice(&bytes)?;
            print_json(&catalog.edit_metadata(&id, expected_revision, base_model, &edits)?)?;
        }
        Command::MetadataPackets {
            id,
            observation,
            output,
        } => {
            let evidence = catalog.metadata_packets(&id, observation)?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            serde_json::to_writer(&mut file, &evidence)?;
            file.sync_all()?;
        }
        Command::MetadataExportPlan {
            id,
            base_model,
            destination,
            expected_revision,
        } => print_json(&catalog.plan_metadata_export(
            &id,
            expected_revision,
            base_model,
            &destination,
        )?)?,
        Command::MetadataExportApply { operation } => {
            let receipt = catalog.apply_metadata_export(&operation)?;
            print_json(&receipt)?;
            ensure!(
                receipt.state == photocatalog::metadata_export::ExportState::Published,
                "export requires recovery/review; see retained receipt"
            );
        }
        Command::MetadataExportDiscover { directory } => print_json(
            &photocatalog::metadata_export::discover_exports(&directory)?,
        )?,
        Command::MetadataExportRecover { directory } => {
            print_json(&catalog.recover_metadata_export(&directory)?)?
        }
    }
    Ok(())
}
fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
