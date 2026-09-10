use super::{print_json, read_request};
use anyhow::Result;
use clap::{Args, Subcommand};
use photocatalog::{Catalog, catalog_edits::VariantKey};
use std::path::PathBuf;

#[derive(Args)]
pub(super) struct VariantArgs {
    asset: String,
    #[arg(long, default_value = "master")]
    variant: String,
}
impl VariantArgs {
    fn key(self) -> VariantKey {
        VariantKey {
            asset_id: self.asset,
            variant_id: self.variant,
        }
    }
}

#[derive(Subcommand)]
pub(super) enum EditCommand {
    /// Inspect the current recipe, revision and persistent undo/redo availability.
    #[command(name = "edit-view")]
    View {
        #[command(flatten)]
        target: VariantArgs,
    },
    /// List persisted variants; edit-view also exposes an untouched implicit master.
    #[command(name = "edit-variants")]
    Variants {
        asset: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Create an independent variant from the specified source revision.
    #[command(name = "edit-variant-create")]
    Create {
        #[command(flatten)]
        target: VariantArgs,
        #[arg(long)]
        revision: i64,
        #[arg(long)]
        label: String,
    },
    /// Save a versioned recipe JSON; a stale expected revision is rejected.
    #[command(name = "edit-save")]
    Save {
        #[command(flatten)]
        target: VariantArgs,
        #[arg(long)]
        revision: i64,
        recipe: PathBuf,
    },
    #[command(name = "edit-undo")]
    Undo {
        #[command(flatten)]
        target: VariantArgs,
        #[arg(long)]
        revision: i64,
    },
    #[command(name = "edit-redo")]
    Redo {
        #[command(flatten)]
        target: VariantArgs,
        #[arg(long)]
        revision: i64,
    },
    #[command(name = "edit-history")]
    History {
        #[command(flatten)]
        target: VariantArgs,
        #[arg(long, default_value_t = -1)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Freeze source settings and selected adjustment groups from a JSON array.
    #[command(name = "edit-copy-begin")]
    CopyBegin {
        #[command(flatten)]
        source: VariantArgs,
        #[arg(long)]
        revision: i64,
        groups: PathBuf,
    },
    /// Append up to 200 target identities/revisions from a JSON array.
    #[command(name = "edit-copy-targets")]
    CopyTargets {
        job: String,
        #[arg(long)]
        expected_total: i64,
        targets: PathBuf,
    },
    #[command(name = "edit-copy-seal")]
    CopySeal {
        job: String,
        #[arg(long)]
        expected_total: i64,
    },
    /// Process a bounded page. Each edit and its result commit together.
    #[command(name = "edit-copy-step")]
    CopyStep {
        job: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    #[command(name = "edit-copy-cancel")]
    CopyCancel { job: String },
    #[command(name = "edit-copy-status")]
    CopyStatus { job: String },
    #[command(name = "edit-copy-jobs")]
    CopyJobs {
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    #[command(name = "edit-copy-items")]
    CopyItems {
        job: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

pub(super) fn run(catalog: &mut Catalog, command: EditCommand) -> Result<()> {
    match command {
        EditCommand::View { target } => print_json(&catalog.edit_variant(&target.key())?),
        EditCommand::Variants {
            asset,
            after,
            limit,
        } => print_json(&catalog.edit_variants(&asset, after, limit)?),
        EditCommand::Create {
            target,
            revision,
            label,
        } => print_json(&catalog.create_edit_variant(&target.key(), revision, &label)?),
        EditCommand::Save {
            target,
            revision,
            recipe,
        } => print_json(&catalog.save_edit_recipe(
            &target.key(),
            revision,
            &read_request(&recipe)?,
        )?),
        EditCommand::Undo { target, revision } => {
            print_json(&catalog.undo_edit(&target.key(), revision)?)
        }
        EditCommand::Redo { target, revision } => {
            print_json(&catalog.redo_edit(&target.key(), revision)?)
        }
        EditCommand::History {
            target,
            after,
            limit,
        } => print_json(&catalog.edit_history(&target.key(), after, limit)?),
        EditCommand::CopyBegin {
            source,
            revision,
            groups,
        } => print_json(&catalog.begin_edit_copy(
            &source.key(),
            revision,
            &read_request::<Vec<_>>(&groups)?,
        )?),
        EditCommand::CopyTargets {
            job,
            expected_total,
            targets,
        } => print_json(&catalog.append_edit_copy(
            &job,
            expected_total,
            &read_request::<Vec<_>>(&targets)?,
        )?),
        EditCommand::CopySeal {
            job,
            expected_total,
        } => print_json(&catalog.seal_edit_copy(&job, expected_total)?),
        EditCommand::CopyStep { job, limit } => {
            print_json(&catalog.apply_edit_copy_step(&job, limit)?)
        }
        EditCommand::CopyCancel { job } => print_json(&catalog.cancel_edit_copy(&job)?),
        EditCommand::CopyStatus { job } => print_json(&catalog.edit_copy_job(&job)?),
        EditCommand::CopyJobs { after, limit } => {
            print_json(&catalog.edit_copy_jobs(after, limit)?)
        }
        EditCommand::CopyItems { job, after, limit } => {
            print_json(&catalog.edit_copy_items(&job, after, limit)?)
        }
    }
}
