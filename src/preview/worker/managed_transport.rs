//! Retained C transport tasks; no catalog SQL enters these closures.
use super::managed_process::Job;
use super::*;
use crate::preview::transport_task::RetainedError;
use crate::preview::{stage_io::Calls, transport_task::Task};
use std::sync::{Arc, Mutex};

enum Outcome {
    Work(std::result::Result<Box<RenderedPreviewBatch>, RetainedError>),
    Drain(std::result::Result<(), RetainedError>),
}
pub(crate) struct Render {
    owner: Arc<Mutex<Option<Job>>>,
    task: Option<Task<Outcome>>,
    cancel: Arc<AtomicBool>,
    calls: Arc<Calls>,
    request: RenderWork,
    limits: crate::preview::ServiceLimits,
    cost: u64,
    complete: bool,
    failure: Option<RetainedError>,
    result: Option<std::result::Result<Box<RenderedPreviewBatch>, RetainedError>>,
    #[cfg(test)]
    cleanup_spawn_failures: usize,
}
impl Render {
    pub fn new(
        calls: Arc<Calls>,
        request: RenderWork,
        limits: &crate::preview::ServiceLimits,
        cost: u64,
    ) -> Self {
        Self {
            owner: Arc::new(Mutex::new(None)),
            task: None,
            cancel: Arc::new(AtomicBool::new(false)),
            calls,
            request,
            limits: limits.clone(),
            cost,
            complete: false,
            failure: None,
            result: None,
            #[cfg(test)]
            cleanup_spawn_failures: 0,
        }
    }
    pub fn start(&mut self) -> Result<()> {
        if self.task.is_some() || self.complete {
            return Ok(());
        }
        let owner = self.owner.clone();
        let calls = self.calls.clone();
        let request = self.request.clone();
        let limits = self.limits.clone();
        let cost = self.cost;
        self.task = Some(Task::spawn(
            "preview-render-transport",
            self.cancel.clone(),
            move |cancel| {
                let result = (|| -> Result<RenderedPreviewBatch> {
                    if cancel.load(Ordering::Acquire) {
                        bail!("preview canceled before stage admission");
                    }
                    let mut state = owner.lock().unwrap_or_else(|p| p.into_inner());
                    *state = Some(Job::admit(
                        calls,
                        crate::catalog_session::native::Work::Render(Box::new(request.clone())),
                        &limits,
                        cost,
                    )?);
                    let job = state.as_mut().unwrap();
                    loop {
                        if let Some(result) = job.poll_render(&request, &cancel)? {
                            return Ok(result);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                })();
                Ok(Outcome::Work(
                    result.map(Box::new).map_err(RetainedError::new),
                ))
            },
        )?);
        Ok(())
    }
    /// Collect only completed transport work. A terminal batch or error stays
    /// owned here until the actor can enter its separate publication phase.
    pub fn progress(&mut self, canceled: &AtomicBool) {
        if self.result.is_some() || self.complete {
            return;
        }
        match self.poll_task(canceled) {
            Ok(Some(batch)) => self.result = Some(Ok(batch)),
            Err(error) if self.complete => self.result = Some(Err(RetainedError::new(error))),
            Err(error) => {
                // Thread admission or another pre-drain error is retryable
                // ownership, not a terminal publication result.
                self.failure
                    .get_or_insert_with(|| RetainedError::new(error));
            }
            Ok(None) => {}
        }
    }
    pub fn poll(&mut self, canceled: &AtomicBool) -> Result<Option<RenderedPreviewBatch>> {
        self.progress(canceled);
        self.result
            .take()
            .map(|result| {
                result
                    .map(|batch| *batch)
                    .map_err(RetainedError::into_error)
            })
            .transpose()
    }
    fn poll_task(&mut self, canceled: &AtomicBool) -> Result<Option<Box<RenderedPreviewBatch>>> {
        if canceled.load(Ordering::Acquire) {
            self.signal_stop();
        }
        if self.task.is_none() && self.failure.is_some() {
            self.start_cleanup()?;
        } else {
            self.start()?;
        }
        let result = match self.task.as_mut().context("render result consumed")?.poll() {
            Ok(None) => return Ok(None),
            Ok(Some(result)) => result,
            Err(error) => {
                self.task = None;
                self.failure
                    .get_or_insert_with(|| RetainedError::new(error));
                self.start_cleanup()?;
                return Ok(None);
            }
        };
        self.task = None;
        match result {
            Outcome::Work(Ok(batch)) => {
                self.complete = true;
                Ok(Some(batch))
            }
            Outcome::Work(Err(error)) => {
                self.failure = Some(error);
                self.start_cleanup()?;
                Ok(None)
            }
            Outcome::Drain(Ok(())) => {
                self.complete = true;
                Err(self
                    .failure
                    .take()
                    .context("render failure absent")?
                    .into_error())
            }
            Outcome::Drain(Err(error)) => {
                self.failure.get_or_insert(error);
                self.start_cleanup()?;
                Ok(None)
            }
        }
    }
    #[cfg(test)]
    pub(super) fn fail_next_cleanup_spawn(&mut self) {
        self.cleanup_spawn_failures += 1;
    }
    fn start_cleanup(&mut self) -> Result<()> {
        self.signal_stop();
        #[cfg(test)]
        if self.cleanup_spawn_failures > 0 {
            self.cleanup_spawn_failures -= 1;
            bail!("injected cleanup thread spawn failure");
        }
        let owner = self.owner.clone();
        let calls = self.calls.clone();
        self.task = Some(Task::spawn(
            "preview-render-drain",
            self.cancel.clone(),
            move |_| {
                let result = (|| -> Result<()> {
                    let mut owner = owner.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(job) = owner.as_mut() {
                        while !job.is_retired() && !job.drain()? {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        job.retire()?;
                        job.stage.release()?;
                    }
                    if owner.is_none() {
                        super::managed_process::cleanup_unadmitted(&calls)?;
                    }
                    Ok(())
                })();
                Ok(Outcome::Drain(result.map_err(RetainedError::new)))
            },
        )?);
        Ok(())
    }
    pub fn busy(&self) -> bool {
        self.task.as_ref().is_some_and(Task::running)
    }
    pub fn signal_stop(&self) {
        self.cancel.store(true, Ordering::Release);
        self.calls.signal_native_stop();
    }
    pub fn pid(&self) -> u32 {
        self.owner
            .try_lock()
            .ok()
            .and_then(|s| {
                s.as_ref()
                    .and_then(|j| j.status.as_ref().and_then(|s| s.pid))
            })
            .unwrap_or(0)
    }
    /// Explicit service shutdown only. Actor polling never joins a live task.
    pub fn stop(&mut self) -> Result<()> {
        self.signal_stop();
        if let Some(task) = &mut self.task {
            let _ = task.shutdown()?;
        }
        self.task = None;
        self.complete = true;
        let mut owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(job) = owner.as_mut() {
            while !job.is_retired() && !job.drain()? {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            job.retire()?;
            // The returned prepared artifact may still own this stage.
            // Stage release follows publication/last owner drop.
        }
        if owner.is_none() {
            super::managed_process::cleanup_unadmitted(&self.calls)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
