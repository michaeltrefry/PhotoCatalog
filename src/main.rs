use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use photocatalog::{
    Catalog,
    catalog_storage::{PathReference, RelinkScope, StorageEncoding},
    organization::{BatchItem, KeywordKind, Operation},
    organization_search::{Cursor, Query, TextLimits},
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
    /// Explicit preview service locations, quotas and admission settings.
    #[arg(long)]
    preview_config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Clone, Copy, ValueEnum)]
enum OriginEncoding {
    Unix,
    Windows,
}
#[derive(Clone, Copy, ValueEnum)]
enum KeywordType {
    Flat,
    Hierarchical,
}
impl From<KeywordType> for KeywordKind {
    fn from(value: KeywordType) -> Self {
        match value {
            KeywordType::Flat => Self::Flat,
            KeywordType::Hierarchical => Self::Hierarchical,
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum PreviewTier {
    Thumbnail,
    Large,
}
impl From<PreviewTier> for photocatalog::preview::Tier {
    fn from(tier: PreviewTier) -> Self {
        match tier {
            PreviewTier::Thumbnail => Self::Thumbnail,
            PreviewTier::Large => Self::Large,
        }
    }
}
// Flattened families preserve the public command syntax while keeping Clap's
// generated debug-mode argument builders out of a single large stack frame.
#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Cache(CacheCommand),
    #[command(flatten)]
    Search(SearchCommand),
    #[command(flatten)]
    Taxonomy(TaxonomyCommand),
    #[command(flatten)]
    Organization(OrganizationCommand),
    #[command(flatten)]
    Storage(StorageCommand),
    #[command(flatten)]
    Relink(RelinkCommand),
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
    #[command(flatten)]
    Metadata(MetadataCommand),
}

#[derive(Subcommand)]
enum CacheCommand {
    /// List durable preview jobs, including resource/availability errors.
    #[command(name = "cache-jobs")]
    Jobs {
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Resume queued jobs; --retry-blocked retries after resources/storage recover.
    #[command(name = "cache-resume")]
    Resume {
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        retry_blocked: bool,
    },
    #[command(name = "cache-budgets")]
    Budgets {
        #[arg(long)]
        thumbnail_bytes: u64,
        #[arg(long)]
        large_bytes: u64,
    },
    #[command(name = "cache-relocate-begin")]
    RelocateBegin {
        #[arg(value_enum)]
        tier: PreviewTier,
        destination: PathBuf,
    },
    #[command(name = "cache-relocate-step")]
    RelocateStep {
        #[arg(value_enum)]
        tier: PreviewTier,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 4194304)]
        bytes: u64,
    },
}

#[derive(Subcommand)]
enum SearchCommand {
    /// Resume bounded organization index construction for an upgraded catalog.
    OrganizationIndex {
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Query all combined filters/sorts from a bounded JSON request.
    Search {
        query: PathBuf,
        #[arg(long)]
        cursor: Option<PathBuf>,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long, default_value_t = 2048)]
        scan: usize,
        #[arg(long, default_value_t = 1_048_576)]
        text_document_bytes: usize,
        #[arg(long, default_value_t = 8_388_608)]
        text_page_bytes: usize,
    },
    /// Emit bounded pages from one consistent snapshot; concurrent writes remain visible to new sessions.
    SearchSession {
        query: PathBuf,
        #[arg(long, default_value_t = 1)]
        pages: usize,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long, default_value_t = 2048)]
        scan: usize,
        #[arg(long, default_value_t = 1_048_576)]
        text_document_bytes: usize,
        #[arg(long, default_value_t = 8_388_608)]
        text_page_bytes: usize,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
    },
    SearchPlan {
        query: PathBuf,
        #[arg(long)]
        cursor: Option<PathBuf>,
        #[arg(long, default_value_t = 2048)]
        scan: usize,
    },
    Folders {
        #[arg(long)]
        parent: Option<i64>,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum TaxonomyCommand {
    KeywordCreate {
        #[arg(value_enum)]
        kind: KeywordType,
        #[arg(required = true)]
        path: Vec<String>,
    },
    Keywords {
        #[arg(value_enum)]
        kind: KeywordType,
        #[arg(long)]
        parent: Option<i64>,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    KeywordDelete {
        id: i64,
    },
    CollectionCreate {
        name: String,
        #[arg(long)]
        provenance: Option<PathBuf>,
    },
    Collections {
        #[arg(long, default_value = "")]
        after: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    CollectionRename {
        id: String,
        name: String,
        #[arg(long)]
        expected_revision: i64,
    },
    CollectionDelete {
        id: String,
        #[arg(long)]
        expected_revision: i64,
    },
}

#[derive(Subcommand)]
enum OrganizationCommand {
    /// Apply a single explicit rating/flag/label/keyword/collection operation from JSON.
    Organize {
        id: String,
        operation: PathBuf,
        #[arg(long)]
        expected_revision: i64,
    },
    OrganizationBegin {
        operation: PathBuf,
    },
    OrganizationAppend {
        job: String,
        items: PathBuf,
    },
    OrganizationSeal {
        job: String,
    },
    OrganizationStep {
        job: String,
        #[arg(long, default_value_t = 1)]
        steps: usize,
    },
    OrganizationShow {
        job: String,
    },
    OrganizationJobs {
        #[arg(long, default_value = "")]
        after: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    OrganizationItems {
        job: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Retry an explicitly re-reviewed revision, or record a failed item as skipped.
    OrganizationReview {
        job: String,
        sequence: i64,
        #[arg(long)]
        new_revision: Option<i64>,
        #[arg(long)]
        skip: bool,
    },
    OrganizationCancel {
        job: String,
    },
    OrganizationEvents {
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum StorageCommand {
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
}

// Variant names intentionally preserve the existing flat CLI command names.
#[allow(clippy::enum_variant_names)]
#[derive(Subcommand)]
enum RelinkCommand {
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
}

#[derive(Subcommand)]
enum MetadataCommand {
    /// Inspect effective fields, source revisions, and unresolved conflicts.
    Metadata { id: String },
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
    MetadataExportApply { operation: String },
    /// List retained interrupted/completed export recovery directories.
    MetadataExportDiscover { directory: PathBuf },
    /// Explicitly resume filesystem publication/recovery of a known export operation.
    MetadataExportRecover { directory: PathBuf },
}

fn main() -> Result<()> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--preview-worker")
    {
        return photocatalog::preview::worker_main();
    }
    run_cli(Cli::parse())
}
// Keep image import out of the large administrative-command dispatch frame.
// Windows executable main stacks are smaller than Rust's test-thread stacks.
fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Import { folder, max_files } => {
            let mut catalog = Catalog::open_for_import(&cli.catalog, &folder)?;
            let configuration = photocatalog::preview::PreviewConfiguration::read(
                cli.preview_config
                    .as_deref()
                    .context("--preview-config is required for application imports")?,
            )?;
            let mut previews = configuration.open(std::env::current_exe()?, Some(&folder))?;
            let report =
                catalog.import_with_previews(folder, max_files, |_| Ok(()), &mut previews)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            ensure!(
                report.failed == 0 && report.awaiting_resources == 0,
                "{} imports failed, {} await resources or storage; inspect cache-jobs and resume",
                report.failed,
                report.awaiting_resources
            );
            Ok(())
        }
        command => run_catalog_command(cli.catalog, cli.preview_config, command),
    }
}
#[inline(never)]
fn run_catalog_command(
    root: PathBuf,
    preview_config: Option<PathBuf>,
    command: Command,
) -> Result<()> {
    let mut catalog = Catalog::open(root)?;
    match command {
        Command::Search(SearchCommand::OrganizationIndex { limit }) => {
            print_json(&catalog.organization_index(limit)?)?
        }
        Command::Search(SearchCommand::Search {
            query,
            cursor,
            limit,
            scan,
            text_document_bytes,
            text_page_bytes,
        }) => print_json(
            &catalog.search_with_text_limits(
                &read_request::<Query>(&query)?,
                cursor
                    .as_deref()
                    .map(read_request::<Cursor>)
                    .transpose()?
                    .as_ref(),
                limit,
                scan,
                TextLimits {
                    document_bytes: text_document_bytes,
                    page_bytes: text_page_bytes,
                },
            )?,
        )?,
        Command::Search(SearchCommand::SearchSession {
            query,
            pages,
            limit,
            scan,
            text_document_bytes,
            text_page_bytes,
            seconds,
        }) => {
            ensure!((1..=1000).contains(&pages), "session pages must be 1..1000");
            let mut session = catalog.search_session_with_text_limits(
                read_request(&query)?,
                seconds,
                TextLimits {
                    document_bytes: text_document_bytes,
                    page_bytes: text_page_bytes,
                },
            )?;
            for _ in 0..pages {
                let page = session.next_page(limit, scan)?;
                let done = page.exhausted;
                println!("{}", serde_json::to_string(&page)?);
                if done {
                    break;
                }
            }
            session.close()?;
        }
        Command::Search(SearchCommand::SearchPlan {
            query,
            cursor,
            scan,
        }) => print_json(
            &catalog.explain_search(
                &read_request::<Query>(&query)?,
                cursor
                    .as_deref()
                    .map(read_request::<Cursor>)
                    .transpose()?
                    .as_ref(),
                scan,
            )?,
        )?,
        Command::Search(SearchCommand::Folders {
            parent,
            after,
            limit,
        }) => print_json(&catalog.organization_folders(parent, after, limit)?)?,
        Command::Taxonomy(TaxonomyCommand::KeywordCreate { kind, path }) => {
            print_json(&catalog.create_keyword(kind.into(), &path)?)?
        }
        Command::Taxonomy(TaxonomyCommand::Keywords {
            kind,
            parent,
            after,
            limit,
        }) => print_json(&catalog.organization_keywords(kind.into(), parent, after, limit)?)?,
        Command::Taxonomy(TaxonomyCommand::KeywordDelete { id }) => catalog.delete_keyword(id)?,
        Command::Taxonomy(TaxonomyCommand::CollectionCreate { name, provenance }) => print_json(
            &catalog.create_collection(
                &name,
                provenance
                    .as_deref()
                    .map(read_request::<serde_json::Value>)
                    .transpose()?
                    .unwrap_or(serde_json::json!({"origin":"explicit CLI creation"})),
            )?,
        )?,
        Command::Taxonomy(TaxonomyCommand::Collections { after, limit }) => {
            print_json(&catalog.organization_collections(&after, limit)?)?
        }
        Command::Taxonomy(TaxonomyCommand::CollectionRename {
            id,
            name,
            expected_revision,
        }) => catalog.rename_collection(&id, expected_revision, &name)?,
        Command::Taxonomy(TaxonomyCommand::CollectionDelete {
            id,
            expected_revision,
        }) => catalog.delete_collection(&id, expected_revision)?,
        Command::Organization(OrganizationCommand::Organize {
            id,
            operation,
            expected_revision,
        }) => print_json(&catalog.organize_asset(
            &id,
            expected_revision,
            read_request::<Operation>(&operation)?,
        )?)?,
        Command::Organization(OrganizationCommand::OrganizationBegin { operation }) => {
            print_json(&catalog.begin_organization_batch(read_request::<Operation>(&operation)?)?)?
        }
        Command::Organization(OrganizationCommand::OrganizationAppend { job, items }) => {
            print_json(
                &catalog
                    .append_organization_batch(&job, &read_request::<Vec<BatchItem>>(&items)?)?,
            )?
        }
        Command::Organization(OrganizationCommand::OrganizationSeal { job }) => {
            print_json(&catalog.seal_organization_batch(&job)?)?
        }
        Command::Organization(OrganizationCommand::OrganizationStep { job, steps }) => {
            ensure!((1..=1000).contains(&steps), "steps must be 1..1000");
            for _ in 0..steps {
                let state = catalog.step_organization_batch(&job)?;
                if !matches!(state.state.as_str(), "ready" | "running") {
                    break;
                }
            }
            print_json(&catalog.organization_job(&job)?)?;
        }
        Command::Organization(OrganizationCommand::OrganizationShow { job }) => {
            print_json(&catalog.organization_job(&job)?)?
        }
        Command::Organization(OrganizationCommand::OrganizationJobs { after, limit }) => {
            print_json(&catalog.organization_jobs(&after, limit)?)?
        }
        Command::Organization(OrganizationCommand::OrganizationItems { job, after, limit }) => {
            print_json(&catalog.organization_job_items(&job, after, limit)?)?
        }
        Command::Organization(OrganizationCommand::OrganizationReview {
            job,
            sequence,
            new_revision,
            skip,
        }) => {
            ensure!(
                new_revision.is_some() != skip,
                "provide exactly one of --new-revision or --skip"
            );
            print_json(&catalog.review_organization_item(&job, sequence, new_revision)?)?;
        }
        Command::Organization(OrganizationCommand::OrganizationCancel { job }) => {
            print_json(&catalog.cancel_organization_batch(&job)?)?
        }
        Command::Organization(OrganizationCommand::OrganizationEvents { after, limit }) => {
            print_json(&catalog.organization_events(after, limit)?)?
        }

        Command::Storage(StorageCommand::StorageVolumes) => {
            print_json(&storage_volume::mounted_volumes()?)?
        }
        Command::Storage(StorageCommand::StorageLocate { path }) => {
            print_json(&storage_volume::locate(&path))?
        }
        Command::Storage(StorageCommand::StorageStatus { id }) => {
            print_json(&catalog.storage_status(&id, &storage_volume::mounted_volumes()?)?)?
        }
        Command::Storage(StorageCommand::DeclareStorageEncoding {
            encoding,
            after,
            limit,
        }) => {
            let encoding = match encoding {
                OriginEncoding::Unix => StorageEncoding::Unix,
                OriginEncoding::Windows => StorageEncoding::Windows,
            };
            print_json(&catalog.declare_storage_encoding(encoding, after, limit)?)?;
        }
        Command::Relink(RelinkCommand::RelinkFolder { from, destinations }) => {
            print_json(&catalog.begin_relink(RelinkScope::Prefix {
                from: PathReference::native(&from),
                destinations: native_paths(destinations),
            })?)?
        }
        Command::Relink(RelinkCommand::RelinkOriginal { id, destinations }) => {
            print_json(&catalog.begin_relink(RelinkScope::Asset {
                asset_id: id,
                destinations: native_paths(destinations),
            })?)?
        }
        Command::Relink(RelinkCommand::RelinkPlan { request }) => {
            let mut bytes = Vec::new();
            std::fs::File::open(request)?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 1024 * 1024, "relink request exceeds 1 MiB");
            print_json(&catalog.begin_relink(serde_json::from_slice(&bytes)?)?)?;
        }
        Command::Relink(RelinkCommand::RelinkPlans { after, limit }) => {
            print_json(&catalog.relink_plans(&after, limit)?)?
        }
        Command::Relink(RelinkCommand::RelinkShow { plan }) => {
            print_json(&catalog.relink_plan(&plan)?)?
        }
        Command::Relink(RelinkCommand::RelinkPrepare { plan, limit }) => {
            print_json(&catalog.prepare_relink_batch(&plan, limit)?)?
        }
        Command::Relink(RelinkCommand::RelinkItems { plan, after, limit }) => {
            print_json(&catalog.relink_items(&plan, after, limit)?)?
        }
        Command::Relink(RelinkCommand::RelinkSources {
            plan,
            sequence,
            after,
            limit,
        }) => print_json(&catalog.relink_sources(&plan, sequence, after, limit)?)?,
        Command::Relink(RelinkCommand::RelinkCandidates {
            plan,
            id,
            destinations,
        }) => {
            catalog.set_relink_candidates(&plan, &id, native_paths(destinations))?;
            print_json(&catalog.relink_plan(&plan)?)?;
        }
        Command::Relink(RelinkCommand::RelinkSourceCandidates {
            plan,
            source,
            destinations,
        }) => {
            catalog.set_relink_source_candidates(&plan, source, native_paths(destinations))?;
            print_json(&catalog.relink_plan(&plan)?)?;
        }
        Command::Relink(RelinkCommand::RelinkExclude { plan, sequence }) => {
            catalog.exclude_relink_item(&plan, sequence)?;
            print_json(&catalog.relink_plan(&plan)?)?;
        }
        Command::Relink(RelinkCommand::RelinkExcludeSource { plan, source }) => {
            catalog.exclude_relink_source(&plan, source)?;
            print_json(&catalog.relink_plan(&plan)?)?;
        }
        Command::Relink(RelinkCommand::RelinkApply { plan }) => {
            print_json(&catalog.apply_relink(&plan)?)?
        }
        Command::Relink(RelinkCommand::RelinkUndo { plan }) => {
            print_json(&catalog.undo_relink(&plan)?)?
        }
        Command::Cache(CacheCommand::Jobs { after, limit }) => {
            let settings = load_preview_settings(&preview_config)?;
            let previews = settings.open(std::env::current_exe()?, None)?;
            print_json(&previews.jobs(after, limit)?)?;
        }
        Command::Cache(CacheCommand::Resume {
            after,
            limit,
            retry_blocked,
        }) => {
            let settings = load_preview_settings(&preview_config)?;
            let mut previews = settings.open(std::env::current_exe()?, None)?;
            let (cursor, consumers) = previews.resume(&mut catalog, after, limit, retry_blocked)?;
            let mut pending = consumers;
            let mut results = Vec::new();
            while !pending.is_empty() {
                previews.tick(&mut catalog)?;
                pending.retain(|consumer| {
                    if let Some(result) = previews.take_completion(*consumer) {
                        results.push(result);
                        false
                    } else {
                        true
                    }
                });
                if !pending.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            print_json(&serde_json::json!({"cursor":cursor,"results":results}))?;
        }
        Command::Cache(CacheCommand::Budgets {
            thumbnail_bytes,
            large_bytes,
        }) => {
            let settings = load_preview_settings(&preview_config)?;
            let mut previews = settings.open(std::env::current_exe()?, None)?;
            previews.set_cache_budgets(thumbnail_bytes, large_bytes)?;
            print_json(&previews.store_usage()?)?;
        }
        Command::Cache(CacheCommand::RelocateBegin { tier, destination }) => {
            let settings = load_preview_settings(&preview_config)?;
            let mut previews = settings.open(std::env::current_exe()?, None)?;
            previews.begin_relocation(tier.into(), &destination, &settings.original_roots)?;
        }
        Command::Cache(CacheCommand::RelocateStep { tier, limit, bytes }) => {
            let settings = load_preview_settings(&preview_config)?;
            let mut previews = settings.open(std::env::current_exe()?, None)?;
            print_json(&previews.relocation_step(tier.into(), limit, bytes)?)?;
        }
        Command::Import { .. } => unreachable!("import uses its isolated dispatch path"),
        Command::Browse { after, limit } => println!(
            "{}",
            serde_json::to_string_pretty(&catalog.browse(after, limit)?)?
        ),
        Command::Get { id } => println!("{}", serde_json::to_string_pretty(&catalog.get(&id)?)?),
        Command::Preview { id, output } => {
            let configuration = photocatalog::preview::PreviewConfiguration::read(
                preview_config
                    .as_deref()
                    .context("--preview-config is required for application previews")?,
            )?;
            let mut previews = configuration.open(std::env::current_exe()?, None)?;
            let view = match previews.cached(
                &catalog,
                &id,
                photocatalog::preview::Tier::Thumbnail,
                true,
            )? {
                Some(view) => view,
                None => {
                    let consumer = previews.request(
                        &mut catalog,
                        &id,
                        photocatalog::preview::Tier::Thumbnail,
                        photocatalog::preview::Priority::Foreground,
                    )?;
                    loop {
                        previews.tick(&mut catalog)?;
                        if let Some(result) = previews.take_completion(consumer) {
                            ensure!(
                                matches!(result, photocatalog::preview::ServiceCompletion::Ready),
                                "preview unavailable: {result:?}"
                            );
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    previews
                        .cached(&catalog, &id, photocatalog::preview::Tier::Thumbnail, false)?
                        .context("completed preview missing")?
                }
            };
            drop(view);
            let preview = previews
                .encoded_cached(&catalog, &id, photocatalog::preview::Tier::Thumbnail, true)?
                .context("preview unavailable during export")?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            file.write_all(preview.bytes())?;
            file.sync_all()?;
        }
        Command::Metadata(MetadataCommand::Metadata { id }) => print_json(&catalog.metadata(&id)?)?,
        Command::Metadata(MetadataCommand::MetadataHistory { id, after, limit }) => {
            print_json(&catalog.metadata_history(&id, after, limit)?)?
        }
        Command::Metadata(MetadataCommand::MetadataFileInstances { id, after, limit }) => {
            print_json(&catalog.metadata_file_instances(&id, after, limit)?)?
        }
        Command::Metadata(MetadataCommand::MetadataDecisions { id, after, limit }) => {
            print_json(&catalog.metadata_decisions(&id, after, limit)?)?
        }
        Command::Metadata(MetadataCommand::MetadataResolve {
            id,
            field,
            model,
            expected_revision,
        }) => print_json(&catalog.resolve_metadata(&id, expected_revision, &field, model)?)?,
        Command::Metadata(MetadataCommand::MetadataEdit {
            id,
            edits,
            base_model,
            expected_revision,
        }) => {
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
        Command::Metadata(MetadataCommand::MetadataPackets {
            id,
            observation,
            output,
        }) => {
            let evidence = catalog.metadata_packets(&id, observation)?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            serde_json::to_writer(&mut file, &evidence)?;
            file.sync_all()?;
        }
        Command::Metadata(MetadataCommand::MetadataExportPlan {
            id,
            base_model,
            destination,
            expected_revision,
        }) => print_json(&catalog.plan_metadata_export(
            &id,
            expected_revision,
            base_model,
            &destination,
        )?)?,
        Command::Metadata(MetadataCommand::MetadataExportApply { operation }) => {
            let receipt = catalog.apply_metadata_export(&operation)?;
            print_json(&receipt)?;
            ensure!(
                receipt.state == photocatalog::metadata_export::ExportState::Published,
                "export requires recovery/review; see retained receipt"
            );
        }
        Command::Metadata(MetadataCommand::MetadataExportDiscover { directory }) => print_json(
            &photocatalog::metadata_export::discover_exports(&directory)?,
        )?,
        Command::Metadata(MetadataCommand::MetadataExportRecover { directory }) => {
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

fn read_request<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<T> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "request exceeds 1MiB");
    Ok(serde_json::from_slice(&bytes)?)
}

fn load_preview_settings(
    path: &Option<PathBuf>,
) -> Result<photocatalog::preview::PreviewConfiguration> {
    photocatalog::preview::PreviewConfiguration::read(
        path.as_deref()
            .context("--preview-config is required for cache commands")?,
    )
}

#[cfg(test)]
mod cli_stack_tests {
    use super::*;
    #[test]
    fn bounded_stack_parser_child() -> Result<()> {
        if std::env::var_os("PHOTOCATALOG_CLI_STACK_CHILD").is_none() {
            return Ok(());
        }
        let owned = tempfile::tempdir()?;
        let folder = owned.path().join("originals ü 日本語");
        std::fs::create_dir(&folder)?;
        let photo = folder.join("source.jpg");
        image::RgbImage::from_pixel(8, 8, image::Rgb([30, 70, 90])).save(&photo)?;
        let before = std::fs::read(&photo)?;
        let catalog = owned.path().join("catalog");
        let import_root = catalog.clone();
        std::thread::Builder::new()
            .name("bounded-cli-import".into())
            .stack_size(1024 * 1024)
            .spawn(move || {
                let cli = Cli::try_parse_from([
                    std::ffi::OsString::from("photocatalog"),
                    std::ffi::OsString::from("--catalog"),
                    import_root.into_os_string(),
                    std::ffi::OsString::from("import"),
                    folder.into_os_string(),
                ])?;
                ensure!(
                    matches!(cli.command, Command::Import { .. }),
                    "wrong parsed command"
                );
                Ok::<(), anyhow::Error>(())
            })?
            .join()
            .map_err(|_| anyhow::anyhow!("small-stack import panicked"))??;
        ensure!(!catalog.exists(), "parser-only regression must not import");
        ensure!(std::fs::read(photo)? == before);
        Ok(())
    }
    #[test]
    fn parsing_runs_on_a_one_mib_stack_in_an_actual_child() -> Result<()> {
        let child = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "cli_stack_tests::bounded_stack_parser_child",
                "--nocapture",
            ])
            .env("PHOTOCATALOG_CLI_STACK_CHILD", "1")
            .output()?;
        ensure!(
            child.status.success(),
            "small-stack child failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        Ok(())
    }
}
