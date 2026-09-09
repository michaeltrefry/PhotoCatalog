use anyhow::{Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use photocatalog::{
    Catalog,
    catalog_storage::{PathReference, RelinkScope, StorageEncoding},
    storage_volume::{self, NativePath},
};
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
#[derive(Clone, Copy, ValueEnum)]
enum OriginEncoding {
    Unix,
    Windows,
}
#[derive(Subcommand)]
enum Command {
    /// Inspect mounted-volume identities and ambiguity without changing originals.
    StorageVolumes,
    /// Explicitly tag a bounded page of legacy, untagged catalog paths by their origin OS.
    DeclareStorageEncoding {
        #[arg(value_enum)]
        encoding: OriginEncoding,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    StorageLocate {
        path: PathBuf,
    },
    StorageStatus {
        id: String,
    },
    /// Start a reviewed folder/root remap, even while its old location is online.
    RelinkFolder {
        from: PathBuf,
        #[arg(required = true)]
        destinations: Vec<PathBuf>,
    },
    RelinkOriginal {
        id: String,
        #[arg(required = true)]
        destinations: Vec<PathBuf>,
    },
    /// Start an explicit native/legacy/volume request from a bounded JSON file.
    RelinkPlan {
        request: PathBuf,
    },
    RelinkPlans {
        #[arg(long, default_value = "")]
        after: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    RelinkShow {
        plan: String,
    },
    RelinkPrepare {
        plan: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    RelinkItems {
        plan: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    RelinkSources {
        plan: String,
        sequence: i64,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Set per-original destinations before starting plan preparation.
    RelinkCandidates {
        plan: String,
        id: String,
        #[arg(required = true)]
        destinations: Vec<PathBuf>,
    },
    RelinkSourceCandidates {
        plan: String,
        source: i64,
        #[arg(required = true)]
        destinations: Vec<PathBuf>,
    },
    RelinkExclude {
        plan: String,
        sequence: i64,
    },
    RelinkExcludeSource {
        plan: String,
        source: i64,
    },
    /// Commit only a fully prepared, resolved plan; original files are never moved.
    RelinkApply {
        plan: String,
    },
    /// Atomically restore catalog paths if no conflicting changes occurred.
    RelinkUndo {
        plan: String,
    },
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
    /// Browse retained copy/relocation file-instance provenance for unchanged XMP.
    MetadataFileInstances {
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
        Command::StorageVolumes => print_json(&storage_volume::mounted_volumes()?)?,
        Command::StorageLocate { path } => print_json(&storage_volume::locate(&path))?,
        Command::StorageStatus { id } => {
            print_json(&catalog.storage_status(&id, &storage_volume::mounted_volumes()?)?)?
        }
        Command::DeclareStorageEncoding {
            encoding,
            after,
            limit,
        } => {
            let encoding = match encoding {
                OriginEncoding::Unix => StorageEncoding::Unix,
                OriginEncoding::Windows => StorageEncoding::Windows,
            };
            print_json(&catalog.declare_storage_encoding(encoding, after, limit)?)?;
        }
        Command::RelinkFolder { from, destinations } => {
            print_json(&catalog.begin_relink(RelinkScope::Prefix {
                from: PathReference::native(&from),
                destinations: native_paths(destinations),
            })?)?
        }
        Command::RelinkOriginal { id, destinations } => {
            print_json(&catalog.begin_relink(RelinkScope::Asset {
                asset_id: id,
                destinations: native_paths(destinations),
            })?)?
        }
        Command::RelinkPlan { request } => {
            let mut bytes = Vec::new();
            std::fs::File::open(request)?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 1024 * 1024, "relink request exceeds 1 MiB");
            print_json(&catalog.begin_relink(serde_json::from_slice(&bytes)?)?)?;
        }
        Command::RelinkPlans { after, limit } => print_json(&catalog.relink_plans(&after, limit)?)?,
        Command::RelinkShow { plan } => print_json(&catalog.relink_plan(&plan)?)?,
        Command::RelinkPrepare { plan, limit } => {
            print_json(&catalog.prepare_relink_batch(&plan, limit)?)?
        }
        Command::RelinkItems { plan, after, limit } => {
            print_json(&catalog.relink_items(&plan, after, limit)?)?
        }
        Command::RelinkSources {
            plan,
            sequence,
            after,
            limit,
        } => print_json(&catalog.relink_sources(&plan, sequence, after, limit)?)?,
        Command::RelinkCandidates {
            plan,
            id,
            destinations,
        } => {
            catalog.set_relink_candidates(&plan, &id, native_paths(destinations))?;
            print_json(&catalog.relink_plan(&plan)?)?;
        }
        Command::RelinkSourceCandidates {
            plan,
            source,
            destinations,
        } => {
            catalog.set_relink_source_candidates(&plan, source, native_paths(destinations))?;
            print_json(&catalog.relink_plan(&plan)?)?;
        }
        Command::RelinkExclude { plan, sequence } => {
            catalog.exclude_relink_item(&plan, sequence)?;
            print_json(&catalog.relink_plan(&plan)?)?;
        }
        Command::RelinkExcludeSource { plan, source } => {
            catalog.exclude_relink_source(&plan, source)?;
            print_json(&catalog.relink_plan(&plan)?)?;
        }
        Command::RelinkApply { plan } => print_json(&catalog.apply_relink(&plan)?)?,
        Command::RelinkUndo { plan } => print_json(&catalog.undo_relink(&plan)?)?,
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
        Command::MetadataFileInstances { id, after, limit } => {
            print_json(&catalog.metadata_file_instances(&id, after, limit)?)?
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
fn native_paths(paths: Vec<PathBuf>) -> Vec<NativePath> {
    paths.iter().map(|p| NativePath::from_path(p)).collect()
}
