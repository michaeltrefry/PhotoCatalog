use super::{print_json, read_request};
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use photocatalog::catalog_backup::{self, CancellationToken, Limits, Progress};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Args)]
pub(super) struct OperationOptions {
    /// JSON limits for this operation; backup-limits prints the default template.
    #[arg(long)]
    limits: PathBuf,
    /// Cancel at the next checkpoint when this file exists.
    #[arg(long)]
    cancel_file: Option<PathBuf>,
}

#[derive(Subcommand)]
pub(super) enum BackupCommand {
    /// Print the backup/restore limits template without opening a catalog.
    #[command(name = "backup-limits")]
    Limits,
    /// Copy one consistent catalog snapshot into a new backup bundle.
    #[command(name = "backup-create")]
    Create {
        bundle: PathBuf,
        #[command(flatten)]
        options: OperationOptions,
    },
    /// Verify a completed backup without opening or upgrading its catalog.
    #[command(name = "backup-inspect")]
    Inspect {
        bundle: PathBuf,
        #[command(flatten)]
        options: OperationOptions,
    },
    /// Restore a verified backup into the new --catalog destination.
    #[command(name = "backup-restore")]
    Restore {
        bundle: PathBuf,
        #[command(flatten)]
        options: OperationOptions,
    },
    /// Read the restored catalog's receipt and pending-job hold.
    #[command(name = "restore-status")]
    Status,
    /// Release restored jobs for later explicit execution; starts no jobs.
    #[command(name = "restore-resume")]
    Resume {
        restore_id: String,
        /// Acknowledge that jobs captured in the backup may now be resumed.
        #[arg(long, required = true)]
        acknowledge_pending_jobs: bool,
    },
}

impl OperationOptions {
    fn with_control<T>(
        &self,
        operation: impl FnOnce(&CancellationToken, &mut dyn FnMut(Progress) -> Result<()>) -> Result<T>,
    ) -> Result<T> {
        let token = CancellationToken::default();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            // Also retire the watcher if an operation panics while the scope unwinds.
            struct StopOnDrop<'a>(&'a AtomicBool);
            impl Drop for StopOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let _stop_on_drop = StopOnDrop(&stop);
            let watcher = self
                .cancel_file
                .as_ref()
                .map(|path| {
                    let stop = &stop;
                    let token = &token;
                    std::thread::Builder::new()
                        .name("backup-cancel".into())
                        .spawn_scoped(scope, move || {
                            while !stop.load(Ordering::Acquire) {
                                match path.try_exists() {
                                    Ok(false) => {}
                                    Ok(true) | Err(_) => {
                                        token.cancel();
                                        break;
                                    }
                                }
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        })
                })
                .transpose()
                .context("start backup cancellation watcher")?;
            let result = operation(&token, &mut self.progress());
            stop.store(true, Ordering::Release);
            if let Some(watcher) = watcher {
                watcher
                    .join()
                    .map_err(|_| anyhow::anyhow!("backup cancellation watcher failed"))?;
            }
            result
        })
    }

    fn read_limits(&self) -> Result<Limits> {
        let limits: Limits = read_request(&self.limits)?;
        limits.validate()?;
        self.check_cancel()?;
        Ok(limits)
    }

    fn check_cancel(&self) -> Result<()> {
        if let Some(path) = &self.cancel_file {
            ensure!(
                !path
                    .try_exists()
                    .context("check backup cancellation file")?,
                "backup operation cancelled by {}",
                path.display()
            );
        }
        Ok(())
    }

    fn progress(&self) -> impl FnMut(Progress) -> Result<()> + '_ {
        let mut last_phase = None;
        let mut last_emission = Instant::now();
        move |progress| {
            self.check_cancel()?;
            let phase = serde_json::to_value(progress.phase)?;
            if last_phase.as_ref() != Some(&phase)
                || last_emission.elapsed() >= Duration::from_millis(250)
            {
                writeln!(
                    std::io::stderr().lock(),
                    "{}",
                    serde_json::to_string(&progress)?
                )?;
                last_phase = Some(phase);
                last_emission = Instant::now();
            }
            Ok(())
        }
    }
}

// This dispatch deliberately runs before Catalog::open: inspection must not
// initialize missing roots, and restore requires an unused destination.
pub(super) fn run(root: &Path, command: BackupCommand) -> Result<()> {
    match command {
        BackupCommand::Limits => print_json(&Limits::default()),
        BackupCommand::Create { bundle, options } => {
            let limits = options.read_limits()?;
            print_json(&options.with_control(|token, progress| {
                catalog_backup::backup_catalog_with_control(root, &bundle, &limits, token, progress)
            })?)
        }
        BackupCommand::Inspect { bundle, options } => {
            let limits = options.read_limits()?;
            print_json(&options.with_control(|token, progress| {
                catalog_backup::inspect_backup_with_control(&bundle, &limits, token, progress)
            })?)
        }
        BackupCommand::Restore { bundle, options } => {
            let limits = options.read_limits()?;
            print_json(&options.with_control(|token, progress| {
                catalog_backup::restore_catalog_with_control(
                    &bundle, root, &limits, token, progress,
                )
            })?)
        }
        BackupCommand::Status => print_json(&catalog_backup::restore_status(root)?),
        BackupCommand::Resume {
            restore_id,
            acknowledge_pending_jobs,
        } => print_json(&catalog_backup::resume_restored_jobs(
            root,
            &restore_id,
            acknowledge_pending_jobs,
        )?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_cancellation_does_not_replace_a_completed_operation_result() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cancel = temp.path().join("cancel");
        let options = OperationOptions {
            limits: temp.path().join("unused-limits"),
            cancel_file: Some(cancel.clone()),
        };
        let receipt = options.with_control(|_, _| {
            // The operation has finished; cancellation arrives as it returns.
            std::fs::write(&cancel, [])?;
            Ok("completed-receipt")
        })?;
        assert_eq!(receipt, "completed-receipt");
        std::fs::remove_file(&cancel)?;
        let failure = options.with_control::<()>(|_, _| {
            std::fs::write(&cancel, [])?;
            anyhow::bail!("original storage failure")
        });
        assert_eq!(failure.unwrap_err().to_string(), "original storage failure");
        Ok(())
    }
}
