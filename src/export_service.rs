//! Incremental export actor: one admitted process, bounded catalog pages, and
//! preview launch suspension held until the export process has been reaped.
use crate::{
    Catalog,
    catalog_exports::{ExportPublicationMetrics, ExportWork},
    export_worker::ExportWorkerProcess,
    photo_render::PhotoRenderLimits,
    preview::{ByteBudget, ByteReservation, NativeLaunchPause, PreviewService},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportServiceLimits {
    /// Measured whole-worker reservation including native scratch and margin.
    /// Pixel limits below are additional allocation checks, not an RSS promise.
    pub worker_bytes: u64,
    pub working_bytes: u64,
    pub render: PhotoRenderLimits,
}
impl ExportServiceLimits {
    pub fn validate(&self) -> Result<()> {
        self.render.decode.validate()?;
        ensure!(
            self.worker_bytes > 0 && self.worker_bytes <= self.working_bytes,
            "export worker exceeds memory admission"
        );
        ensure!(
            self.render.decode.max_allocation_bytes <= self.worker_bytes
                && self.render.render.max_live_bytes <= self.worker_bytes
                && self.render.encode.render.max_live_bytes <= self.worker_bytes,
            "export allocation ceilings exceed worker reservation"
        );
        ensure!(
            self.render.max_encoded_extent > 0,
            "zero export staging bound"
        );
        Ok(())
    }
}
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExportEvent {
    WaitingForPreviews,
    Started {
        sequence: i64,
        pid: u32,
    },
    Rendering {
        sequence: i64,
    },
    Published {
        sequence: i64,
        cleanup_warning: Option<String>,
    },
    Failed {
        sequence: i64,
        detail: String,
        cleanup_warning: Option<String>,
    },
    Yielded {
        sequence: i64,
        cleanup_warning: Option<String>,
    },
    Idle,
}
#[derive(Debug, Serialize)]
pub struct ExportRecovery {
    pub fenced: usize,
    pub complete: bool,
}
/// Actual successful-worker phase times, including failed later authorization.
/// Publication details are absent if it failed before returning those details;
/// publication_elapsed_ms still measures that complete attempted call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportCompletionMetrics {
    pub job: String,
    pub sequence: i64,
    pub attempt: String,
    pub started_unix_ms: u128,
    pub finished_unix_ms: u128,
    /// Owner wall interval from input preparation/spawn through observed child
    /// completion. Includes polling; render fields carry the child phase timings.
    pub worker_elapsed_ms: f64,
    pub worker_pid: u32,
    pub worker_peak_resident_bytes: Option<u64>,
    pub worker_peak_method: String,
    pub render: crate::photo_render::PhotoRenderTimings,
    pub seal_ms: f64,
    pub accept_ms: f64,
    pub publication_elapsed_ms: f64,
    pub publication: Option<ExportPublicationMetrics>,
}
fn unix_ms() -> Result<u128> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis())
}
struct Active {
    process: ExportWorkerProcess,
    _reservation: ByteReservation,
    started: std::time::Instant,
    started_unix_ms: u128,
}
pub struct ExportService {
    catalog: PathBuf,
    executable: PathBuf,
    staging: PathBuf,
    limits: ExportServiceLimits,
    budget: ByteBudget,
    active: Option<Active>,
    pause: Option<NativeLaunchPause>,
    _lock: File,
    recovery_complete: bool,
    completion_metrics: Option<ExportCompletionMetrics>,
}
fn detail(error: impl std::fmt::Display) -> String {
    let mut value = error.to_string();
    if value.len() > 8192 {
        let mut n = 8192;
        while !value.is_char_boundary(n) {
            n -= 1;
        }
        value.truncate(n);
    }
    value
}
impl ExportService {
    /// The application owns its preview service for the same catalog. CLI callers
    /// also open the configured preview store, whose process lock prevents a second
    /// application from running that service concurrently.
    pub fn open(catalog: &Catalog, executable: &Path, limits: ExportServiceLimits) -> Result<Self> {
        limits.validate()?;
        ensure!(
            executable.is_absolute(),
            "absolute export executable required"
        );
        let root = catalog.root.canonicalize()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("photo-export.lock"))?;
        lock.try_lock_exclusive()
            .context("another export executor owns this catalog")?;
        let staging = root.join("photo-export-workers");
        std::fs::create_dir_all(&staging)?;
        ensure!(
            !std::fs::symlink_metadata(&staging)?
                .file_type()
                .is_symlink(),
            "export staging must not be a symlink"
        );
        Ok(Self {
            catalog: root,
            executable: executable.to_owned(),
            staging,
            limits,
            budget: ByteBudget::new(limits.working_bytes)?,
            active: None,
            pause: None,
            _lock: lock,
            recovery_complete: false,
            completion_metrics: None,
        })
    }
    fn check_catalog(&self, catalog: &Catalog) -> Result<()> {
        ensure!(
            catalog.root.canonicalize()? == self.catalog,
            "export executor belongs to another catalog"
        );
        Ok(())
    }
    /// Call before executing jobs after every service open. Complete staging
    /// inventory precedes any catalog-only fencing, including a crash between
    /// claiming an item and creating its transport. Busy/unknown entries block it.
    pub fn recover(
        &mut self,
        catalog: &mut Catalog,
        max_directories: usize,
    ) -> Result<ExportRecovery> {
        self.check_catalog(catalog)?;
        ensure!(
            self.active.is_none(),
            "cannot recover while export worker is active"
        );
        self.recovery_complete = false;
        let result =
            crate::export_worker::recover_export_transports(&self.staging, max_directories)?;
        ensure!(
            result.retained.is_empty(),
            "export transports require attention: {:?}",
            result.retained
        );
        let mut fenced = 0;
        for retired in &result.retired {
            if let Some(current) = catalog
                .photo_export_attempt_if_rendering(&retired.work.job, retired.work.sequence)?
                && current.attempt == retired.work.attempt
                && current.authority == retired.work.authority
            {
                catalog.requeue_photo_export_attempt(&current)?;
                fenced += 1;
            }
            crate::export_worker::discard_retired_export_transport(retired)?;
        }
        let page = catalog.rendering_photo_export_attempts(200)?;
        for work in &page {
            catalog.requeue_photo_export_attempt(work)?;
            fenced += 1;
        }
        // Committed pre-link intents can have installed bytes even when their
        // job was canceled or edited after the crash. Reconcile installed results
        // by that intent; uninstalled stale results fail and retain captures.
        let intents = catalog.photo_export_publication_intents(200)?;
        for (job, sequence) in &intents {
            if let Err(error) = catalog.publish_photo_export_item(job, *sequence) {
                catalog.fail_sealed_photo_export(
                    job,
                    *sequence,
                    &detail(format!("publication recovery: {error:#}")),
                )?;
            }
        }
        self.recovery_complete = page.len() < 200 && intents.len() < 200;
        Ok(ExportRecovery {
            fenced,
            complete: self.recovery_complete,
        })
    }
    pub fn take_completion_metrics(&mut self) -> Option<ExportCompletionMetrics> {
        self.completion_metrics.take()
    }
    pub fn reserved_bytes(&self) -> u64 {
        self.budget.used()
    }
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }
    /// One bounded execution step on the export executor thread. Completion may
    /// hash full source/payload bytes and run durability barriers: never invoke
    /// this function directly on the UI or foreground catalog actor. Native
    /// rendering is polled rather than synchronously joined here.
    pub fn tick(
        &mut self,
        catalog: &mut Catalog,
        previews: &mut PreviewService,
        job: &str,
        canceled: &AtomicBool,
    ) -> Result<ExportEvent> {
        self.check_catalog(catalog)?;
        ensure!(
            self.recovery_complete,
            "export transport recovery must complete before launch"
        );
        if let Some(active) = self.active.as_ref() {
            ensure!(
                active.process.work().job == job,
                "another export job is active"
            );
            let work = active.process.work().clone();
            if canceled.load(std::sync::atomic::Ordering::Acquire) {
                catalog.cancel_photo_export_job(job)?;
            }
            let current = catalog.photo_export_work_current(&work)?;
            let cancellation = AtomicBool::new(!current);
            let outcome = self.active.as_mut().unwrap().process.poll(&cancellation);
            if matches!(outcome, Ok(None)) {
                return Ok(ExportEvent::Rendering {
                    sequence: work.sequence,
                });
            }
            // On any transport error stop/reap before releasing either native
            // admission or preview suspension. Failed stop leaves ownership held.
            self.active.as_mut().unwrap().process.stop()?;
            let active = self.active.take().unwrap();
            let worker_pid = active.process.pid();
            let cleanup_warning = active.process.retire_transport().err().map(detail);
            let worker_elapsed_ms = active.started.elapsed().as_secs_f64() * 1000.;
            let started_unix_ms = active.started_unix_ms;
            drop(active);
            self.pause.take();
            match outcome {
                Ok(Some(result)) => {
                    let accept_start = std::time::Instant::now();
                    let accepted = catalog.accept_photo_export_seal(&work, &result.sealed);
                    self.completion_metrics = Some(ExportCompletionMetrics {
                        job: work.job.clone(),
                        sequence: work.sequence,
                        attempt: work.attempt.clone(),
                        started_unix_ms,
                        finished_unix_ms: unix_ms()?,
                        worker_elapsed_ms,
                        worker_pid,
                        worker_peak_resident_bytes: result.peak_resident_bytes,
                        worker_peak_method: result.peak_method.clone(),
                        render: result.rendered.timings.clone(),
                        seal_ms: result.seal_ms,
                        accept_ms: accept_start.elapsed().as_secs_f64() * 1000.,
                        publication_elapsed_ms: 0.,
                        publication: None,
                    });
                    if let Err(error) = accepted {
                        let message = detail(format!("{error:#}"));
                        catalog.fail_photo_export(&work, &message)?;
                        return Ok(ExportEvent::Failed {
                            sequence: work.sequence,
                            detail: message,
                            cleanup_warning,
                        });
                    }
                    let publish_start = std::time::Instant::now();
                    let published =
                        catalog.publish_photo_export_item_with_metrics(job, work.sequence);
                    if let Some(metrics) = self.completion_metrics.as_mut() {
                        metrics.publication_elapsed_ms =
                            publish_start.elapsed().as_secs_f64() * 1000.;
                        metrics.finished_unix_ms = unix_ms()?;
                        if let Ok((_, publication)) = &published {
                            metrics.publication = Some(publication.clone());
                        }
                    }
                    match published {
                        Ok((receipt, _))
                            if receipt.state == crate::metadata_export::ExportState::Published =>
                        {
                            Ok(ExportEvent::Published {
                                sequence: work.sequence,
                                cleanup_warning,
                            })
                        }
                        Ok((receipt, _)) => Ok(ExportEvent::Failed {
                            sequence: work.sequence,
                            detail: receipt.detail,
                            cleanup_warning,
                        }),
                        Err(error) => {
                            let message = detail(format!("{error:#}"));
                            catalog.fail_sealed_photo_export(job, work.sequence, &message)?;
                            Ok(ExportEvent::Failed {
                                sequence: work.sequence,
                                detail: message,
                                cleanup_warning,
                            })
                        }
                    }
                }
                Err(error) => {
                    let message = detail(format!("{error:#}"));
                    catalog.fail_photo_export(&work, &message)?;
                    Ok(ExportEvent::Failed {
                        sequence: work.sequence,
                        detail: message,
                        cleanup_warning,
                    })
                }
                Ok(None) => unreachable!(),
            }
        } else {
            if canceled.load(std::sync::atomic::Ordering::Acquire) {
                if catalog.photo_export_job(job)?.state != "complete" {
                    catalog.cancel_photo_export_job(job)?;
                }
                self.pause.take();
                return Ok(ExportEvent::Idle);
            }
            if catalog.photo_export_job(job)?.state != "queued" {
                self.pause.take();
                return Ok(ExportEvent::Idle);
            }
            if let Some(sequence) = catalog.next_sealed_photo_export(job)? {
                return match catalog.publish_photo_export_item(job, sequence) {
                    Ok(receipt)
                        if receipt.state == crate::metadata_export::ExportState::Published =>
                    {
                        Ok(ExportEvent::Published {
                            sequence,
                            cleanup_warning: None,
                        })
                    }
                    Ok(receipt) => Ok(ExportEvent::Failed {
                        sequence,
                        detail: receipt.detail,
                        cleanup_warning: None,
                    }),
                    Err(error) => {
                        let message = detail(format!("{error:#}"));
                        catalog.fail_sealed_photo_export(job, sequence, &message)?;
                        Ok(ExportEvent::Failed {
                            sequence,
                            detail: message,
                            cleanup_warning: None,
                        })
                    }
                };
            }
            if self.pause.is_none() {
                self.pause = Some(previews.pause_native_launches()?);
            }
            if !previews.native_work_drained() {
                return Ok(ExportEvent::WaitingForPreviews);
            }
            let Some(reservation) = self.budget.try_reserve(self.limits.worker_bytes) else {
                return Ok(ExportEvent::Idle);
            };
            let Some(work) = catalog.claim_photo_export(job)? else {
                self.pause.take();
                return Ok(ExportEvent::Idle);
            };
            let sequence = work.sequence;
            let worker_started = std::time::Instant::now();
            let started_unix_ms = unix_ms()?;
            self.completion_metrics = None;
            let spawn = (|| -> Result<ExportWorkerProcess> {
                let (output, xmp) = catalog.photo_export_inputs(&work.plan)?;
                let mut limits = self.limits.render;
                ensure!(
                    work.plan.max_payload_bytes <= limits.max_encoded_extent,
                    "export plan exceeds configured staging allowance"
                );
                limits.max_encoded_extent = work.plan.max_payload_bytes;
                limits.decode.max_encoded_bytes = limits
                    .decode
                    .max_encoded_bytes
                    .min(work.plan.max_original_bytes);
                ExportWorkerProcess::spawn(
                    &self.executable,
                    &self.staging,
                    work.clone(),
                    &output,
                    xmp.as_deref(),
                    limits,
                )
            })();
            match spawn {
                Ok(process) => {
                    let pid = process.pid();
                    self.active = Some(Active {
                        process,
                        _reservation: reservation,
                        started: worker_started,
                        started_unix_ms,
                    });
                    Ok(ExportEvent::Started { sequence, pid })
                }
                Err(error) => {
                    self.pause.take();
                    let message = detail(format!("{error:#}"));
                    catalog.fail_photo_export(&work, &message)?;
                    Ok(ExportEvent::Failed {
                        sequence,
                        detail: message,
                        cleanup_warning: None,
                    })
                }
            }
        }
    }
    /// Foreground development can preempt background export. Reap first, fence
    /// the old token, then resume preview launch admission. The item stays queued.
    pub fn yield_to_previews(&mut self, catalog: &mut Catalog) -> Result<ExportEvent> {
        self.check_catalog(catalog)?;
        let Some(active) = self.active.as_mut() else {
            self.pause.take();
            return Ok(ExportEvent::Idle);
        };
        active.process.stop()?;
        let work: ExportWork = active.process.work().clone();
        catalog.requeue_photo_export_attempt(&work)?;
        let cleanup_warning = active.process.retire_transport().err().map(detail);
        self.active.take();
        self.pause.take();
        Ok(ExportEvent::Yielded {
            sequence: work.sequence,
            cleanup_warning,
        })
    }
}
impl Drop for ExportService {
    fn drop(&mut self) {
        // Active's process drop waits too; the pause and reservation outlive it.
        if let Some(active) = self.active.as_mut() {
            let _ = active.process.stop();
        }
        self.active.take();
        self.pause.take();
    }
}
