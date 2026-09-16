//! Managed C export adapter. Root/path/filesystem/process and native-memory
//! custody remain behind the session and G/F protocols.
use super::{
    ExportCompletionMetrics, ExportEvent, ExportRecovery, ExportServiceLimits, detail, unix_ms,
};
use crate::{
    Catalog,
    catalog_exports::{ExportCheckpoint, ExportControl, ExportWork},
    catalog_session::{
        ManagedExportAttempt, ManagedExportExecutor, export_executor, export_native, export_stage,
    },
    image_export::OutputProfile,
    preview::{NativeLaunchPause, NativeLaunchPermit, PreviewService},
};
use anyhow::{Context, Result, ensure};
use std::sync::{Arc, atomic::AtomicBool};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Register,
    Begin,
    Icc,
    Xmp,
    Ready,
    Spawn,
    Start,
    Running,
    Seal,
    Release,
    Retire,
    CompletionReady,
    Stop,
    Drain,
    RetryDrain,
}

#[derive(Debug)]
enum Disposition {
    Failed(String),
    Canceled,
    Yielded,
    DrainOnly,
}

struct Active {
    attempt: ManagedExportAttempt,
    phase: Phase,
    icc: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
    icc_offset: usize,
    xmp_offset: usize,
    completion: Option<export_stage::Completion>,
    disposition: Option<Disposition>,
    registered: bool,
    begun: bool,
    armed: bool,
    pending_native: bool,
    pid: Option<u32>,
    started: std::time::Instant,
    started_unix_ms: u128,
}

pub(super) struct ExportService {
    catalog_pin: Arc<crate::catalog_session::CatalogSessionAuthority>,
    limits: ExportServiceLimits,
    executor: Option<ManagedExportExecutor>,
    active: Option<Active>,
    preparing: Option<(ExportWork, Option<String>)>,
    pause: Option<NativeLaunchPause>,
    permit: Option<NativeLaunchPermit>,
    recovery_candidate: Option<export_executor::Candidate>,
    recovery_inventory_open: bool,
    recovery_discard_pending: bool,
    recovery_fenced: usize,
    recovery_complete: bool,
    completion_metrics: Option<ExportCompletionMetrics>,
}

fn blob(bytes: Option<&[u8]>) -> Option<export_stage::Blob> {
    bytes.map(|bytes| export_stage::Blob {
        bytes: crate::application::U64(bytes.len() as u64),
        digest: blake3::hash(bytes).to_hex().to_string(),
    })
}

impl ExportService {
    pub(super) fn open(catalog: &Catalog, limits: ExportServiceLimits) -> Result<Self> {
        let executor = catalog
            .session
            .open_managed_export(&AtomicBool::new(false))?
            .context("managed export owner was not selected")?;
        Ok(Self {
            catalog_pin: catalog.session.clone(),
            limits,
            executor: Some(executor),
            active: None,
            preparing: None,
            pause: None,
            permit: None,
            recovery_candidate: None,
            recovery_inventory_open: false,
            recovery_discard_pending: false,
            recovery_fenced: 0,
            recovery_complete: false,
            completion_metrics: None,
        })
    }

    fn check_catalog(&self, catalog: &Catalog) -> Result<()> {
        catalog.require_jobs_released()?;
        ensure!(
            crate::catalog_session::CatalogSessionAuthority::export_matches(
                &self.catalog_pin,
                &catalog.session,
            )?,
            "export executor selected database changed"
        );
        Ok(())
    }

    fn ensure_executor(&mut self) -> Result<&mut ManagedExportExecutor> {
        if self.executor.is_none() {
            self.executor = Some(
                self.catalog_pin
                    .open_managed_export(&AtomicBool::new(false))?
                    .context("managed export owner was not selected")?,
            );
        }
        Ok(self.executor.as_mut().unwrap())
    }

    fn handle_stage_failure(active: &mut Active, error: anyhow::Error) -> Result<()> {
        let Some(message) = active.attempt.take_terminal_stage_failure() else {
            return Err(error);
        };
        active.disposition = Some(Disposition::Failed(detail(message)));
        if active.phase == Phase::Begin
            && ManagedExportAttempt::stage_failure_without_custody(&error)
        {
            active.begun = false;
        }
        active.phase = Phase::Stop;
        Ok(())
    }

    pub(super) fn recover(
        &mut self,
        catalog: &mut Catalog,
        max_directories: usize,
    ) -> Result<ExportRecovery> {
        self.recover_cancellable(
            catalog,
            max_directories,
            &mut ExportControl::new(&AtomicBool::new(false)),
        )
    }

    pub(super) fn recover_cancellable(
        &mut self,
        catalog: &mut Catalog,
        max_directories: usize,
        control: &mut ExportControl<'_>,
    ) -> Result<ExportRecovery> {
        // A committed SQL decision owns exact Discard cleanup even when a
        // retry arrives canceled. Observe cancellation again after that replay.
        if !self.recovery_discard_pending {
            control.begin()?;
        }
        self.check_catalog(catalog)?;
        ensure!(
            !self.is_active(),
            "cannot recover while export worker is active"
        );
        self.recovery_complete = false;
        if !self.recovery_inventory_open {
            let reply = self
                .ensure_executor()?
                .recover(max_directories, control.cancellation())?;
            let export_executor::Value::Recovery {
                retained,
                retained_example,
                candidate,
                ..
            } = reply.value
            else {
                anyhow::bail!("managed export recovery reply")
            };
            ensure!(
                retained.0 == 0,
                "export transports require attention: {}",
                retained_example.unwrap_or_else(|| "retained transport".into())
            );
            self.recovery_candidate = candidate;
            self.recovery_inventory_open = self.recovery_candidate.is_some();
        }
        while let Some(current) = self.recovery_candidate.clone() {
            if !self.recovery_discard_pending {
                control.check(ExportCheckpoint::BeforeMutation)?;
                if let Some(work) = catalog.photo_export_attempt_if_rendering(
                    &current.attempt.job,
                    current.attempt.sequence,
                )? && work.attempt == current.attempt.attempt
                    && work.authority == current.attempt.authority
                {
                    let next = self
                        .recovery_fenced
                        .checked_add(1)
                        .context("export recovery count exhausted")?;
                    catalog.requeue_photo_export_attempt(&work)?;
                    self.recovery_fenced = next;
                }
                self.recovery_discard_pending = true;
            }
            // Once the SQL decision is committed, discard is admitted cleanup.
            let reply = self.ensure_executor()?.discard(current.token)?;
            let export_executor::Value::Discarded { candidate: next } = reply.value else {
                anyhow::bail!("managed export discard reply")
            };
            self.recovery_discard_pending = false;
            self.recovery_candidate = next;
            self.recovery_inventory_open = self.recovery_candidate.is_some();
        }
        control.begin()?;
        let page = catalog.rendering_photo_export_attempts(200)?;
        for work in &page {
            control.check(ExportCheckpoint::BeforeMutation)?;
            let next = self
                .recovery_fenced
                .checked_add(1)
                .context("export recovery count exhausted")?;
            catalog.requeue_photo_export_attempt(work)?;
            self.recovery_fenced = next;
        }
        let intents = catalog.photo_export_publication_intents(200)?;
        for (job, sequence) in &intents {
            if let Err(error) =
                catalog.publish_photo_export_item_cancellable(job, *sequence, control)
            {
                if control.cancel_requested() {
                    return Err(error);
                }
                catalog.fail_sealed_photo_export(
                    job,
                    *sequence,
                    &detail(format!("publication recovery: {error:#}")),
                )?;
            }
        }
        self.recovery_complete = page.len() < 200 && intents.len() < 200;
        Ok(ExportRecovery {
            fenced: std::mem::take(&mut self.recovery_fenced),
            complete: self.recovery_complete,
        })
    }

    pub(super) fn take_completion_metrics(&mut self) -> Option<ExportCompletionMetrics> {
        self.completion_metrics.take()
    }

    pub(super) fn reserved_bytes(&self) -> u64 {
        if self.active.as_ref().is_some_and(|active| active.registered) {
            self.limits.worker_bytes
        } else {
            0
        }
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.is_some() || self.preparing.is_some()
    }

    pub(super) fn admit_native(
        &mut self,
        catalog: &Catalog,
        permit: NativeLaunchPermit,
    ) -> Result<()> {
        self.check_catalog(catalog)?;
        ensure!(
            self.recovery_complete,
            "explicit export recovery must complete before native admission"
        );
        ensure!(
            permit.matches_catalog(catalog),
            "export native permit belongs to another catalog handle"
        );
        ensure!(
            self.permit.is_none() && self.active.is_none(),
            "export already owns native admission"
        );
        self.permit = Some(permit);
        Ok(())
    }

    pub(super) fn tick(
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
        if self.active.is_none()
            && self.permit.is_none()
            && !canceled.load(std::sync::atomic::Ordering::Acquire)
            && catalog.photo_export_job(job)?.state == "queued"
            && catalog.next_sealed_photo_export(job)?.is_none()
        {
            if self.pause.is_none() {
                self.pause = Some(previews.pause_native_launches()?);
            }
            if previews.native_work_drained() {
                let permit = previews.native_launch_permit(catalog, self.pause.take().unwrap())?;
                self.admit_native(catalog, permit)?;
            }
        }
        self.tick_detached(catalog, job, &mut ExportControl::new(canceled))
    }

    fn stopped_event(
        &mut self,
        catalog: &mut Catalog,
        work: ExportWork,
        disposition: Disposition,
    ) -> Result<ExportEvent> {
        self.active.take();
        self.pause.take();
        self.permit.take();
        match disposition {
            Disposition::Yielded => {
                catalog.requeue_photo_export_attempt(&work)?;
                Ok(ExportEvent::Yielded {
                    sequence: work.sequence,
                    cleanup_warning: None,
                })
            }
            Disposition::Canceled => {
                if catalog.photo_export_job(&work.job)?.state != "complete" {
                    catalog.cancel_photo_export_job(&work.job)?;
                }
                let message = detail("export worker canceled");
                catalog.fail_photo_export(&work, &message)?;
                Ok(ExportEvent::Failed {
                    sequence: work.sequence,
                    detail: message,
                    cleanup_warning: None,
                })
            }
            Disposition::Failed(message) => {
                catalog.fail_photo_export(&work, &message)?;
                Ok(ExportEvent::Failed {
                    sequence: work.sequence,
                    detail: message,
                    cleanup_warning: None,
                })
            }
            Disposition::DrainOnly => Ok(ExportEvent::Idle),
        }
    }

    fn advance_transport(&mut self, _control: &mut ExportControl<'_>) -> Result<ExportEvent> {
        let active = self.active.as_mut().unwrap();
        if active.disposition.is_some()
            && !active.attempt.pending_stage()
            && !active.pending_native
            && !matches!(
                active.phase,
                Phase::Release
                    | Phase::Retire
                    | Phase::CompletionReady
                    | Phase::Stop
                    | Phase::Drain
                    | Phase::RetryDrain
            )
        {
            active.phase = if active.registered {
                Phase::Stop
            } else {
                Phase::CompletionReady
            };
        }
        let phase = self.active.as_ref().unwrap().phase;
        let sequence = self.active.as_ref().unwrap().attempt.work().sequence;
        match phase {
            Phase::Register => {
                // Registration admission may have reached G even if its outer
                // acknowledgement is lost. Charge the reservation first and
                // keep it charged until checked Retire releases the slot.
                self.active.as_mut().unwrap().registered = true;
                self.active.as_mut().unwrap().pending_native = true;
                let result = self
                    .active
                    .as_ref()
                    .unwrap()
                    .attempt
                    .register(&AtomicBool::new(false));
                let status = match result {
                    Ok(status) => status,
                    Err(error)
                        if self
                            .active
                            .as_ref()
                            .unwrap()
                            .attempt
                            .registration_rejected(&error) =>
                    {
                        // Register has no fallible step after publishing a slot.
                        // A G receipt for this exact Register proves negative
                        // custody; an unknown/malformed reply stays pending.
                        let active = self.active.as_mut().unwrap();
                        active.registered = false;
                        active.pending_native = false;
                        active.disposition.get_or_insert_with(|| {
                            Disposition::Failed(detail(format!("{error:#}")))
                        });
                        active.phase = Phase::CompletionReady;
                        return Ok(ExportEvent::Rendering { sequence });
                    }
                    Err(error) => return Err(error),
                };
                ensure!(
                    status.phase == export_native::Phase::Registered,
                    "managed export registration state"
                );
                self.active.as_mut().unwrap().pending_native = false;
                self.active.as_mut().unwrap().phase = Phase::Begin;
            }
            Phase::Begin => {
                let active = self.active.as_mut().unwrap();
                active.begun = true; // conservative until a bound rejection proves no custody
                if let Err(error) = active.attempt.begin(&AtomicBool::new(false)) {
                    Self::handle_stage_failure(active, error)?;
                } else {
                    active.phase = Phase::Icc;
                }
            }
            Phase::Icc => {
                let active = self.active.as_mut().unwrap();
                if let Some(bytes) = &active.icc
                    && active.icc_offset < bytes.len()
                {
                    let end = (active.icc_offset + export_stage::CHUNK_BYTES).min(bytes.len());
                    if let Err(error) = active.attempt.stage(
                        export_stage::Action::UploadIcc {
                            offset: crate::application::U64(active.icc_offset as u64),
                            bytes: bytes[active.icc_offset..end].to_vec(),
                        },
                        &AtomicBool::new(false),
                    ) {
                        Self::handle_stage_failure(active, error)?;
                    } else {
                        active.icc_offset = end;
                    }
                } else {
                    active.phase = Phase::Xmp;
                }
            }
            Phase::Xmp => {
                let active = self.active.as_mut().unwrap();
                if let Some(bytes) = &active.xmp
                    && active.xmp_offset < bytes.len()
                {
                    let end = (active.xmp_offset + export_stage::CHUNK_BYTES).min(bytes.len());
                    if let Err(error) = active.attempt.stage(
                        export_stage::Action::UploadXmp {
                            offset: crate::application::U64(active.xmp_offset as u64),
                            bytes: bytes[active.xmp_offset..end].to_vec(),
                        },
                        &AtomicBool::new(false),
                    ) {
                        Self::handle_stage_failure(active, error)?;
                    } else {
                        active.xmp_offset = end;
                    }
                } else {
                    active.phase = Phase::Ready;
                }
            }
            Phase::Ready => {
                let active = self.active.as_mut().unwrap();
                if let Err(error) = active.attempt.stage(
                    export_stage::Action::Ready {
                        icc: blob(active.icc.as_deref()),
                        xmp: blob(active.xmp.as_deref()),
                    },
                    &AtomicBool::new(false),
                ) {
                    Self::handle_stage_failure(active, error)?;
                } else {
                    active.phase = Phase::Spawn;
                }
            }
            Phase::Spawn => {
                let active = self.active.as_mut().unwrap();
                active.pending_native = true;
                let status = active
                    .attempt
                    .native(export_native::Action::Spawn, &AtomicBool::new(false))?;
                // A bound WaitFailed without a PID is the real G failed-launch
                // result. It owns an Arm/drain obligation, not a Start command.
                match status.phase {
                    export_native::Phase::Spawned | export_native::Phase::Running
                        if status.pid.is_some() =>
                    {
                        active.armed = true;
                        active.pid = status.pid;
                        active.phase = Phase::Start;
                    }
                    export_native::Phase::WaitFailed
                    | export_native::Phase::PipeJoinFailed
                    | export_native::Phase::StopRequested
                    | export_native::Phase::Drained => {
                        active.armed = true;
                        active.pid = status.pid;
                        active.disposition.get_or_insert_with(|| {
                            Disposition::Failed(detail(
                                status
                                    .error
                                    .unwrap_or_else(|| "export native launch stopped".into()),
                            ))
                        });
                        active.phase = Phase::Stop;
                    }
                    _ => anyhow::bail!("managed export Spawn outcome is unresolved"),
                }
                active.pending_native = false;
            }
            Phase::Start => {
                let active = self.active.as_mut().unwrap();
                active.pending_native = true;
                let status = active
                    .attempt
                    .native(export_native::Action::Start, &AtomicBool::new(false))?;
                let pid = status
                    .pid
                    .context("managed export native PID unavailable")?;
                active.pending_native = false;
                active.pid = Some(pid);
                active.phase = Phase::Running;
                return Ok(ExportEvent::Started { sequence, pid });
            }
            Phase::Running => {
                let status = self.active.as_ref().unwrap().attempt.status()?;
                match status.phase {
                    export_native::Phase::Drained => {
                        if status.success == Some(true) {
                            self.active.as_mut().unwrap().phase = Phase::Seal;
                        } else {
                            let message = detail(status.error.unwrap_or_else(|| {
                                format!("export worker failed ({:?})", status.exit_code)
                            }));
                            let active = self.active.as_mut().unwrap();
                            active.disposition = Some(Disposition::Failed(message));
                            active.phase = Phase::Release;
                        }
                    }
                    export_native::Phase::WaitFailed | export_native::Phase::PipeJoinFailed => {
                        self.active.as_mut().unwrap().phase = Phase::Drain;
                    }
                    _ => {}
                }
            }
            Phase::Seal => {
                let result = self
                    .active
                    .as_mut()
                    .unwrap()
                    .attempt
                    .stage(export_stage::Action::ResultAndSeal, &AtomicBool::new(false));
                let reply = match result {
                    Ok(reply) => reply,
                    Err(error) => {
                        Self::handle_stage_failure(self.active.as_mut().unwrap(), error)?;
                        return Ok(ExportEvent::Rendering { sequence });
                    }
                };
                let export_stage::Value::Completed { completion, .. } = reply.value else {
                    anyhow::bail!("managed export completion reply")
                };
                let active = self.active.as_mut().unwrap();
                active.completion = Some(completion);
                active.phase = Phase::Release;
            }
            Phase::Stop => {
                let active = self.active.as_mut().unwrap();
                if active.registered {
                    active.pending_native = true;
                    active
                        .attempt
                        .native(export_native::Action::Stop, &AtomicBool::new(false))?;
                    active.pending_native = false;
                }
                active.phase = if active.armed {
                    Phase::Drain
                } else if active.begun {
                    Phase::Release
                } else if active.registered {
                    Phase::Retire
                } else {
                    Phase::CompletionReady
                };
            }
            Phase::Drain => {
                let active = self.active.as_mut().unwrap();
                // A status observation cannot acknowledge a lost RetryDrain.
                // Replay that action first until its bound reply returns.
                if active.pending_native {
                    active
                        .attempt
                        .native(export_native::Action::RetryDrain, &AtomicBool::new(false))?;
                    active.pending_native = false;
                } else {
                    let status = active.attempt.status()?;
                    if status.phase == export_native::Phase::Drained {
                        if active.disposition.is_none()
                            && active.completion.is_none()
                            && status.success == Some(true)
                        {
                            active.phase = Phase::Seal;
                        } else {
                            if active.completion.is_none() {
                                active.disposition.get_or_insert_with(|| {
                                    Disposition::Failed(detail(
                                        status.error.unwrap_or_else(|| {
                                            "export native worker failed".into()
                                        }),
                                    ))
                                });
                            }
                            active.phase = Phase::Release;
                        }
                    } else if matches!(
                        status.phase,
                        export_native::Phase::WaitFailed | export_native::Phase::PipeJoinFailed
                    ) {
                        active.pending_native = true;
                        active
                            .attempt
                            .native(export_native::Action::RetryDrain, &AtomicBool::new(false))?;
                        active.pending_native = false;
                    }
                }
            }
            Phase::RetryDrain => {
                let active = self.active.as_mut().unwrap();
                active.pending_native = true;
                active
                    .attempt
                    .native(export_native::Action::RetryDrain, &AtomicBool::new(false))?;
                active.pending_native = false;
                active.phase = Phase::Drain;
            }
            Phase::Release => {
                let active = self.active.as_mut().unwrap();
                let action = if active.armed {
                    export_stage::Action::Release
                } else {
                    export_stage::Action::Abort
                };
                if let Err(error) = active.attempt.stage(action, &AtomicBool::new(false)) {
                    // Terminal cleanup failure proves neither absence nor drain.
                    // Retry checked drain, then admit the next Release only once
                    // the previous logical operation is known terminal.
                    if active.attempt.take_terminal_stage_failure().is_some() {
                        active.phase = if active.armed {
                            Phase::RetryDrain
                        } else {
                            Phase::Release
                        };
                    }
                    return Err(error);
                }
                active.phase = Phase::Retire;
            }
            Phase::Retire => {
                let active = self.active.as_mut().unwrap();
                active.pending_native = true;
                active
                    .attempt
                    .native(export_native::Action::Retire, &AtomicBool::new(false))?;
                active.pending_native = false;
                active.registered = false;
                active.phase = Phase::CompletionReady;
                self.pause.take();
                self.permit.take();
            }
            Phase::CompletionReady => {}
        }
        Ok(ExportEvent::Rendering { sequence })
    }

    fn advance_active(
        &mut self,
        catalog: &mut Catalog,
        control: &mut ExportControl<'_>,
    ) -> Result<ExportEvent> {
        if self.active.as_ref().unwrap().phase != Phase::CompletionReady {
            return self
                .advance_transport(control)
                .map_err(|error| error.context(super::PendingExport));
        }
        if self.active.as_ref().unwrap().completion.is_none() {
            let active = self.active.as_mut().unwrap();
            let work = active.attempt.work().clone();
            let mut disposition = active.disposition.take().unwrap_or(Disposition::DrainOnly);
            if control.cancel_requested() && matches!(disposition, Disposition::Yielded) {
                disposition = Disposition::Canceled;
            }
            return self.stopped_event(catalog, work, disposition);
        }
        let active = self.active.take().unwrap();
        let work = active.attempt.work().clone();
        let completion = active
            .completion
            .context("managed export completion missing")?;
        let accept_start = std::time::Instant::now();
        let accepted =
            catalog.accept_photo_export_seal_cancellable(&work, &completion.sealed, control);
        self.completion_metrics = Some(ExportCompletionMetrics {
            job: work.job.clone(),
            sequence: work.sequence,
            attempt: work.attempt.clone(),
            started_unix_ms: active.started_unix_ms,
            finished_unix_ms: unix_ms()?,
            worker_elapsed_ms: active.started.elapsed().as_secs_f64() * 1000.,
            worker_pid: active.pid.context("managed export worker PID missing")?,
            worker_peak_resident_bytes: completion.peak_resident_bytes,
            worker_peak_method: completion.peak_method,
            render: completion.rendered.timings,
            seal_ms: completion.seal_ms,
            accept_ms: accept_start.elapsed().as_secs_f64() * 1000.,
            publication_elapsed_ms: 0.,
            publication: None,
        });
        self.pause.take();
        self.permit.take();
        if let Err(error) = accepted {
            if control.cancel_requested()
                && catalog.photo_export_job(&work.job)?.state != "complete"
            {
                catalog.cancel_photo_export_job(&work.job)?;
            }
            let message = detail(format!("{error:#}"));
            catalog.fail_photo_export(&work, &message)?;
            return Ok(ExportEvent::Failed {
                sequence: work.sequence,
                detail: message,
                cleanup_warning: None,
            });
        }
        let publish_start = std::time::Instant::now();
        let published =
            catalog.publish_photo_export_item_cancellable(&work.job, work.sequence, control);
        if let Some(metrics) = self.completion_metrics.as_mut() {
            metrics.publication_elapsed_ms = publish_start.elapsed().as_secs_f64() * 1000.;
            metrics.finished_unix_ms = unix_ms()?;
            if let Ok((_, publication)) = &published {
                metrics.publication = Some(publication.clone());
            }
        }
        match published {
            Ok((receipt, _)) if receipt.state == crate::metadata_export::ExportState::Published => {
                Ok(ExportEvent::Published {
                    sequence: work.sequence,
                    cleanup_warning: None,
                })
            }
            Ok((receipt, _)) => Ok(ExportEvent::Failed {
                sequence: work.sequence,
                detail: receipt.detail,
                cleanup_warning: None,
            }),
            Err(error) => {
                if control.cancel_requested() && !control.finishing() {
                    if catalog.photo_export_job(&work.job)?.state != "complete" {
                        catalog.cancel_photo_export_job(&work.job)?;
                    }
                    return Ok(ExportEvent::Idle);
                }
                let message = detail(format!("{error:#}"));
                catalog.fail_sealed_photo_export(&work.job, work.sequence, &message)?;
                Ok(ExportEvent::Failed {
                    sequence: work.sequence,
                    detail: message,
                    cleanup_warning: None,
                })
            }
        }
    }

    pub(super) fn tick_detached(
        &mut self,
        catalog: &mut Catalog,
        job: &str,
        control: &mut ExportControl<'_>,
    ) -> Result<ExportEvent> {
        self.check_catalog(catalog)?;
        ensure!(
            self.recovery_complete,
            "export transport recovery must complete before launch"
        );
        if self.active.is_some() {
            let work = self.active.as_ref().unwrap().attempt.work().clone();
            ensure!(work.job == job, "another export job is active");
            if self.active.as_ref().unwrap().disposition.is_none()
                && (control.cancel_requested() || !catalog.photo_export_work_current(&work)?)
            {
                self.active.as_mut().unwrap().disposition = Some(if control.cancel_requested() {
                    Disposition::Canceled
                } else {
                    Disposition::Failed("export work changed while rendering".into())
                });
            }
            return self.advance_active(catalog, control);
        }
        if self.preparing.is_some() {
            return self.settle_preparation(catalog, control);
        }
        if control.cancel_requested() {
            if catalog.photo_export_job(job)?.state != "complete" {
                catalog.cancel_photo_export_job(job)?;
            }
            self.pause.take();
            self.permit.take();
            return Ok(ExportEvent::Idle);
        }
        if catalog.photo_export_job(job)?.state != "queued" {
            self.pause.take();
            self.permit.take();
            return Ok(ExportEvent::Idle);
        }
        if let Some(sequence) = catalog.next_sealed_photo_export(job)? {
            return match catalog.publish_photo_export_item_cancellable(job, sequence, control) {
                Ok((receipt, _))
                    if receipt.state == crate::metadata_export::ExportState::Published =>
                {
                    Ok(ExportEvent::Published {
                        sequence,
                        cleanup_warning: None,
                    })
                }
                Ok((receipt, _)) => Ok(ExportEvent::Failed {
                    sequence,
                    detail: receipt.detail,
                    cleanup_warning: None,
                }),
                Err(error) => {
                    if control.cancel_requested() && !control.finishing() {
                        if catalog.photo_export_job(job)?.state != "complete" {
                            catalog.cancel_photo_export_job(job)?;
                        }
                        return Ok(ExportEvent::Idle);
                    }
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
        if self.permit.is_none() {
            return Ok(ExportEvent::WaitingForPreviews);
        }
        ensure!(
            self.permit.as_ref().unwrap().matches_catalog(catalog),
            "export native permit changed catalog handle"
        );
        // Acquisition and clock validation precede SQL claim. Ordinary
        // attempts retain this executor for the entire facade lifetime.
        self.ensure_executor()?;
        let started_unix_ms = unix_ms()?;
        control.check(ExportCheckpoint::BeforeMutation)?;
        let Some(work) = catalog.claim_photo_export(job)? else {
            self.pause.take();
            self.permit.take();
            return Ok(ExportEvent::Idle);
        };
        let sequence = work.sequence;
        self.preparing = Some((work.clone(), None));
        let prepared = (|| -> Result<_> {
            catalog.admit_photo_export_render_cancellable(&work, control)?;
            let (output, xmp) = catalog.photo_export_inputs(&work.plan)?;
            let icc = match output.profile {
                OutputProfile::Icc { bytes } => Some(bytes),
                OutputProfile::Srgb | OutputProfile::LinearSrgb => None,
            };
            let limits = self.limits;
            let attempt = self.ensure_executor()?.prepare_attempt(work, limits)?;
            Ok((attempt, icc, xmp))
        })();
        let (attempt, icc, xmp) = match prepared {
            Ok(value) => value,
            Err(error) => {
                self.preparing.as_mut().unwrap().1 = Some(detail(format!("{error:#}")));
                return self.settle_preparation(catalog, control);
            }
        };
        self.preparing = None;
        self.completion_metrics = None;
        self.active = Some(Active {
            attempt,
            phase: Phase::Register,
            icc,
            xmp,
            icc_offset: 0,
            xmp_offset: 0,
            completion: None,
            disposition: None,
            registered: false,
            begun: false,
            armed: false,
            pending_native: false,
            pid: None,
            started: std::time::Instant::now(),
            started_unix_ms,
        });
        Ok(ExportEvent::Rendering { sequence })
    }

    fn settle_preparation(
        &mut self,
        catalog: &mut Catalog,
        control: &mut ExportControl<'_>,
    ) -> Result<ExportEvent> {
        let (work, message) = self
            .preparing
            .as_ref()
            .context("export preparation custody missing")?;
        if control.cancel_requested() && catalog.photo_export_job(&work.job)?.state != "complete" {
            catalog.cancel_photo_export_job(&work.job)?;
        }
        let message = message
            .as_ref()
            .context("export preparation failure missing")?
            .clone();
        catalog.fail_photo_export(work, &message)?;
        let sequence = work.sequence;
        self.preparing = None;
        self.pause.take();
        self.permit.take();
        Ok(ExportEvent::Failed {
            sequence,
            detail: message,
            cleanup_warning: None,
        })
    }

    pub(super) fn drain_native(&mut self) -> Result<()> {
        if let Some(active) = self.active.as_mut() {
            active.disposition = Some(Disposition::DrainOnly);
            if active.phase != Phase::CompletionReady {
                self.advance_transport(&mut ExportControl::new(&AtomicBool::new(false)))
                    .map_err(|error| error.context(super::PendingExport))?;
            }
            ensure!(
                self.active.as_ref().unwrap().phase == Phase::CompletionReady,
                super::PendingExport
            );
        }
        if self.active.is_some() || self.preparing.is_some() {
            // SQL was intentionally untouched; require explicit recovery before
            // this same facade can admit any subsequent work.
            self.recovery_complete = false;
        }
        self.active = None;
        self.preparing = None;
        self.pause.take();
        self.permit.take();
        Ok(())
    }

    pub(super) fn yield_to_previews(&mut self, catalog: &mut Catalog) -> Result<ExportEvent> {
        self.yield_to_previews_cancellable(
            catalog,
            &mut ExportControl::new(&AtomicBool::new(false)),
        )
    }

    pub(super) fn yield_to_previews_cancellable(
        &mut self,
        catalog: &mut Catalog,
        control: &mut ExportControl<'_>,
    ) -> Result<ExportEvent> {
        self.check_catalog(catalog)?;
        if self.preparing.is_some() && control.cancel_requested() {
            return self.settle_preparation(catalog, control);
        }
        if let Some((work, _)) = self.preparing.as_ref() {
            catalog.requeue_photo_export_attempt(work)?;
            let sequence = work.sequence;
            self.preparing = None;
            self.pause.take();
            self.permit.take();
            return Ok(ExportEvent::Yielded {
                sequence,
                cleanup_warning: None,
            });
        }
        let Some(active) = self.active.as_mut() else {
            self.pause.take();
            self.permit.take();
            return Ok(ExportEvent::Idle);
        };
        active.disposition.get_or_insert(Disposition::Yielded);
        // Only admitted transport replay suppresses cancellation. SQL settlement
        // and publication observe the caller's live token throughout this call.
        let event = self.advance_active(catalog, control)?;
        ensure!(!self.is_active(), super::PendingExport);
        Ok(event)
    }

    pub(super) fn close(&mut self) -> Result<()> {
        self.drain_native()?;
        if let Some(executor) = self.executor.as_mut() {
            executor.close()?;
        }
        self.executor = None;
        self.recovery_candidate = None;
        self.recovery_inventory_open = false;
        self.recovery_discard_pending = false;
        self.recovery_complete = false;
        Ok(())
    }
}

impl Drop for ExportService {
    fn drop(&mut self) {
        if self.close().is_err()
            && let Some(executor) = self.executor.as_mut()
        {
            // Persist Close intent even when the bounded per-attempt cleanup
            // has not finished. G reconciles exact pending operations; the
            // session keeps unknown Close/Recover/Discard across destruction.
            if executor.close().is_err() {
                executor.release_claim_after_failed_drop();
            }
        }
    }
}

pub(crate) fn state_layouts() -> [(usize, usize); 4] {
    [
        (
            std::mem::size_of::<ExportService>(),
            std::mem::align_of::<ExportService>(),
        ),
        (
            std::mem::size_of::<Active>(),
            std::mem::align_of::<Active>(),
        ),
        (std::mem::size_of::<Phase>(), std::mem::align_of::<Phase>()),
        (
            std::mem::size_of::<Disposition>(),
            std::mem::align_of::<Disposition>(),
        ),
    ]
}

#[cfg(test)]
pub(crate) mod tests;
