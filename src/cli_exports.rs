use super::{print_json, read_request};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use photocatalog::{
    Catalog,
    catalog_exports::ExportTarget,
    export_service::{ExportService, ExportServiceLimits},
    image_export::{AlphaPolicy, OutputFormat, OutputProfile, OutputSize, OutputSpec},
};
use serde::Deserialize;
use std::{
    io::Read,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Profile {
    Srgb,
    LinearSrgb,
    Icc { path: PathBuf },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    size: OutputSize,
    format: OutputFormat,
    profile: Profile,
    alpha: AlphaPolicy,
}
impl Output {
    fn resolve(self) -> Result<OutputSpec> {
        let profile = match self.profile {
            Profile::Srgb => OutputProfile::Srgb,
            Profile::LinearSrgb => OutputProfile::LinearSrgb,
            Profile::Icc { path } => {
                let f = std::fs::File::open(path)?;
                ensure!(
                    f.metadata()?.len() <= 16 * 1024 * 1024,
                    "ICC profile exceeds16MiB"
                );
                let mut bytes = Vec::new();
                f.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= 16 * 1024 * 1024,
                    "ICC profile grew beyond16MiB"
                );
                OutputProfile::Icc { bytes }
            }
        };
        Ok(OutputSpec {
            size: self.size,
            format: self.format,
            profile,
            alpha: self.alpha,
        })
    }
}
#[derive(Subcommand)]
pub(super) enum ExportCommand {
    /// Create a durable batch; append and seal before running it.
    #[command(name = "photo-export-begin")]
    Begin,
    /// Index at most512 original paths without reading original image bytes.
    #[command(name = "photo-export-paths")]
    Paths {
        #[arg(long, default_value_t = 512)]
        limit: usize,
    },
    /// Append one variant and explicit output settings with compare-and-set total.
    #[command(name = "photo-export-add")]
    Add {
        job: String,
        target: PathBuf,
        output: PathBuf,
        #[arg(long)]
        expected_total: i64,
        #[arg(long)]
        max_original_bytes: u64,
        #[arg(long)]
        max_payload_bytes: u64,
        #[arg(long, default_value_t = 4096)]
        alias_directories: usize,
        #[arg(long, default_value_t = 256)]
        alias_candidates: usize,
    },
    #[command(name = "photo-export-seal")]
    Seal {
        job: String,
        #[arg(long)]
        expected_total: i64,
    },
    #[command(name = "photo-export-status")]
    Status { job: String },
    #[command(name = "photo-export-jobs")]
    Jobs {
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    #[command(name = "photo-export-items")]
    Items {
        job: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    #[command(name = "photo-export-plan")]
    Plan { job: String, sequence: i64 },
    #[command(name = "photo-export-cancel")]
    Cancel { job: String },
    /// Retry a previously accepted failed seal; all publication guards still apply.
    #[command(name = "photo-export-retry-seal")]
    RetrySeal { job: String, sequence: i64 },
    /// Restore captured destination bytes without publishing new pixels.
    #[command(name = "photo-export-restore")]
    Restore { job: String, sequence: i64 },
    /// Run a bounded batch slice with explicit native memory limits and preview ownership.
    #[command(name = "photo-export-run")]
    Run {
        job: String,
        limits: PathBuf,
        #[arg(long, default_value_t = 100)]
        max_items: usize,
        #[arg(long, default_value_t = 300)]
        max_seconds: u64,
        #[arg(long, default_value_t = 256)]
        recovery_directories: usize,
    },
    /// Retire abandoned transports and fence attempts in bounded pages.
    #[command(name = "photo-export-recover")]
    Recover {
        limits: PathBuf,
        #[arg(long, default_value_t = 256)]
        directories: usize,
    },
}
pub(super) fn run(
    catalog: &mut Catalog,
    preview_config: Option<PathBuf>,
    command: ExportCommand,
) -> Result<()> {
    match command {
        ExportCommand::Begin => print_json(&catalog.begin_photo_export()?),
        ExportCommand::Paths { limit } => print_json(&catalog.reconcile_export_paths(limit)?),
        ExportCommand::Add {
            job,
            target,
            output,
            expected_total,
            max_original_bytes,
            max_payload_bytes,
            alias_directories,
            alias_candidates,
        } => {
            let target: ExportTarget = read_request(&target)?;
            let output: Output = read_request(&output)?;
            print_json(&catalog.append_photo_export_with_alias_limits(
                &job,
                expected_total,
                &target,
                &output.resolve()?,
                max_original_bytes,
                max_payload_bytes,
                photocatalog::catalog_export_alias::AliasLimits {
                    directories: alias_directories,
                    candidates: alias_candidates,
                },
            )?)
        }
        ExportCommand::Seal {
            job,
            expected_total,
        } => print_json(&catalog.seal_photo_export_job(&job, expected_total)?),
        ExportCommand::Status { job } => print_json(&catalog.photo_export_job(&job)?),
        ExportCommand::Jobs { after, limit } => {
            print_json(&catalog.photo_export_jobs(after, limit)?)
        }
        ExportCommand::Items { job, after, limit } => {
            print_json(&catalog.photo_export_items(&job, after, limit)?)
        }
        ExportCommand::Plan { job, sequence } => {
            print_json(&catalog.photo_export_plan(&job, sequence)?)
        }
        ExportCommand::Cancel { job } => print_json(&catalog.cancel_photo_export_job(&job)?),
        ExportCommand::RetrySeal { job, sequence } => {
            catalog.retry_sealed_photo_export(&job, sequence)?;
            print_json(&catalog.photo_export_job(&job)?)
        }
        ExportCommand::Restore { job, sequence } => {
            print_json(&catalog.restore_photo_export_item(&job, sequence)?)
        }
        ExportCommand::Recover {
            limits,
            directories,
        } => {
            let limits: ExportServiceLimits = read_request(&limits)?;
            let mut service = ExportService::open(catalog, &std::env::current_exe()?, limits)?;
            print_json(&service.recover(catalog, directories)?)
        }
        ExportCommand::Run {
            job,
            limits,
            max_items,
            max_seconds,
            recovery_directories,
        } => {
            ensure!(
                max_items > 0 && max_items <= 10000 && max_seconds > 0 && max_seconds <= 86400,
                "export slice bounds"
            );
            let limits: ExportServiceLimits = read_request(&limits)?;
            let configuration=photocatalog::preview::PreviewConfiguration::read(&preview_config.context("--preview-config is required to coordinate native export and preview ownership")?)?;
            let mut previews = configuration.open(std::env::current_exe()?, None)?;
            let mut service = ExportService::open(catalog, &std::env::current_exe()?, limits)?;
            let recovered = service.recover(catalog, recovery_directories)?;
            print_json(&recovered)?;
            ensure!(
                recovered.complete,
                "more rendering attempts require another bounded recovery page"
            );
            let started = Instant::now();
            let canceled = AtomicBool::new(false);
            let mut completed = 0;
            while completed < max_items && started.elapsed() < Duration::from_secs(max_seconds) {
                previews.tick(catalog)?;
                let event = service.tick(catalog, &mut previews, &job, &canceled)?;
                use photocatalog::export_service::ExportEvent;
                match event {
                    ExportEvent::Idle => break,
                    ExportEvent::Published { .. } | ExportEvent::Failed { .. } => {
                        completed += 1;
                        print_json(&event)?;
                    }
                    ExportEvent::Started { .. } => print_json(&event)?,
                    _ => {}
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if service.is_active() {
                print_json(&service.yield_to_previews(catalog)?)?;
            }
            print_json(&catalog.photo_export_job(&job)?)
        }
    }
}
