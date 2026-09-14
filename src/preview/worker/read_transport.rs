//! Pollable C-side filesystem/native phases. The owner survives task failure.
use super::managed_process::Job;
use super::*;
use crate::{
    application::U64,
    catalog_session::{native as n, preview_stage as f},
    preview::{stage_io::Calls, transport_task::Task},
};
use std::sync::{Arc, Mutex};

pub(crate) enum Event {
    Header(n::Header),
    Pixels(PreparedRgb),
    Drained,
}
pub(crate) struct Read {
    owner: Arc<Mutex<Option<Job>>>,
    calls: Arc<Calls>,
    task: Option<Task<Result<Event>>>,
    pub cancel: Arc<AtomicBool>,
}
impl Read {
    pub fn new(calls: Arc<Calls>, cancel: Arc<AtomicBool>) -> Self {
        Self {
            owner: Arc::new(Mutex::new(None)),
            calls,
            task: None,
            cancel,
        }
    }
    pub fn legacy_read(
        &self,
        hash: String,
        allowance: u64,
    ) -> Result<Task<(crate::catalog_session::preview_io::Integrity, Vec<u8>)>> {
        let calls = self.calls.clone();
        Task::spawn(
            "preview-legacy-transfer",
            self.cancel.clone(),
            move |cancel| {
                use crate::catalog_session::preview_io::Integrity;
                Ok(match calls.legacy_read(&hash, allowance, &cancel).map_err(|error| {
                if error.downcast_ref::<crate::filesystem_worker::wire::Failure>().is_some_and(|f| f.kind == crate::filesystem_worker::wire::FailureKind::ResourceLimit) {
                    error.context(crate::preview::EncodedBudgetExceeded)
                } else { error }
            })? {
                    Some(bytes) => (Integrity::Intact, bytes),
                    None => (Integrity::Missing, vec![]),
                })
            },
        )
    }
    fn launch(
        &mut self,
        work: impl FnOnce(&mut Option<Job>, Arc<AtomicBool>) -> Result<Event> + Send + 'static,
    ) -> Result<()> {
        ensure!(self.task.is_none(), "native read transport already pending");
        let owner = self.owner.clone();
        self.task = Some(Task::spawn(
            "preview-read-transport",
            self.cancel.clone(),
            move |cancel| {
                let mut state = owner.lock().unwrap_or_else(|p| p.into_inner());
                Ok(work(&mut state, cancel))
            },
        )?);
        Ok(())
    }
    pub fn header(
        &mut self,
        work: n::Work,
        limits: crate::preview::ServiceLimits,
        cost: u64,
        encoded: Vec<u8>,
    ) -> Result<()> {
        let calls = self.calls.clone();
        self.launch(move |owner, cancel| {
            if cancel.load(Ordering::Acquire) {
                return Ok(Event::Drained);
            }
            *owner = Some(Job::admit(calls, work, &limits, cost)?);
            let job = owner.as_mut().unwrap();
            job.stage.calls.upload(&job.stage.id, &encoded, &cancel)?;
            drop(encoded);
            wait_header(job, &cancel)
        })
    }
    pub fn rearm(&mut self, cost: u64, dimensions: (u32, u32)) -> Result<()> {
        self.launch(move |owner, cancel| {
            let next = owner
                .as_ref()
                .context("header predecessor absent")?
                .successor(cost, dimensions)?;
            *owner = Some(next);
            wait_header(owner.as_mut().unwrap(), &cancel)
        })
    }
    pub fn finish(&mut self, header: n::Header, cost: u64) -> Result<()> {
        self.launch(move |owner, cancel| {
            let job = owner.as_mut().context("native read owner absent")?;
            if cancel.load(Ordering::Acquire) {
                return drain(job, true);
            }
            job.call(n::Action::Encode {
                header: Some(header.clone()),
                working_bytes: U64(cost),
                rgb_bytes: U64(n::rgb_bytes(header.width, header.height)?),
            })?;
            loop {
                if cancel.load(Ordering::Acquire) {
                    return drain(job, true);
                }
                let status = job.refresh()?;
                if matches!(
                    status.phase,
                    n::Phase::WaitFailed | n::Phase::PipeJoinFailed
                ) {
                    job.call(n::Action::Drain)?;
                }
                if status.phase == n::Phase::Drained {
                    if !status.success.unwrap_or(false) {
                        let failure = job.failure(&status)?;
                        job.retire()?;
                        return Err(failure.into());
                    }
                    let bytes = job
                        .stage
                        .calls
                        .metadata(&job.stage.id, f::Artifact::Receipt)?
                        .context("managed decode receipt absent")?;
                    let receipt: super::managed::DecodedReceipt = serde_json::from_slice(&bytes)?;
                    ensure!(
                        receipt.header == header
                            && receipt.rgb.bytes.0 == n::rgb_bytes(header.width, header.height)?,
                        "managed RGB receipt identity"
                    );
                    let pixels = job.read(
                        f::Artifact::Rgb(0),
                        receipt.rgb.bytes.0,
                        &receipt.rgb.digest,
                        &cancel,
                    )?;
                    let pixels = PreparedRgb::new(header.width, header.height, pixels)?;
                    job.retire()?;
                    job.stage.release()?;
                    return Ok(Event::Pixels(pixels));
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        })
    }
    pub fn stop(&mut self, release: bool) -> Result<()> {
        let calls = self.calls.clone();
        self.launch(move |owner, _| match owner.as_mut() {
            Some(job) => drain(job, release),
            None => {
                super::managed_process::cleanup_unadmitted(&calls)?;
                Ok(Event::Drained)
            }
        })
    }
    pub fn busy(&self) -> bool {
        self.task.as_ref().is_some_and(Task::running)
    }
    pub fn pending(&self) -> bool {
        self.task.is_some()
    }
    pub fn poll(&mut self) -> Result<Option<Result<Event>>> {
        let Some(task) = &mut self.task else {
            return Ok(None);
        };
        let result = task.poll();
        match result {
            Ok(None) => Ok(None),
            Ok(Some(value)) => {
                self.task = None;
                Ok(Some(value))
            }
            Err(error) => {
                self.task = None;
                Err(error)
            }
        }
    }
    pub fn signal_cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        self.calls.signal_native_stop();
    }
    /// Explicit shutdown, outside an actor tick. Errors keep the Job owner.
    pub fn shutdown(&mut self) -> Result<()> {
        self.signal_cancel();
        if let Some(task) = &mut self.task {
            let _ = task.shutdown()?;
        }
        self.task = None;
        let mut owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(job) = owner.as_mut() {
            drain(job, true)?;
        }
        if owner.is_none() {
            super::managed_process::cleanup_unadmitted(&self.calls)?;
        }
        Ok(())
    }
}
fn wait_header(job: &mut Job, cancel: &AtomicBool) -> Result<Event> {
    loop {
        if cancel.load(Ordering::Acquire) {
            return drain(job, true);
        }
        job.start()?;
        let status = job.refresh()?;
        if status.phase == n::Phase::Drained {
            let error = job.failure(&status)?;
            job.retire()?;
            return Err(error.into());
        }
        if matches!(
            status.phase,
            n::Phase::WaitFailed | n::Phase::PipeJoinFailed
        ) {
            job.call(n::Action::Drain)?;
        }
        if let Some(header) = job.header()? {
            return Ok(Event::Header(header));
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
fn drain(job: &mut Job, release: bool) -> Result<Event> {
    while !job.is_retired() && !job.drain()? {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    job.retire()?;
    if release {
        job.stage.release()?;
    }
    Ok(Event::Drained)
}
