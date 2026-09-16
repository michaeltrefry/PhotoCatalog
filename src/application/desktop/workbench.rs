//! One retained G dispatcher for the independent managed Workbench generation.
use super::{CONTROL_SLOTS, Limits, Result, validate_public_request};
use crate::application::{
    Cancellation, ErrorCode, Pending, Reply, Request, Response, error, failure, lightroom_bridge,
    lightroom_managed,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::{self, JoinHandle},
};

struct Entry {
    id: u64,
    request: lightroom_bridge::Request,
    reply: mpsc::SyncSender<Reply>,
    cancel: Cancellation,
    _completion: Arc<Completion>,
}
#[derive(Default)]
struct Queue {
    data: VecDeque<Entry>,
    control: VecDeque<Entry>,
    stopping: bool,
    next: u64,
    active: Option<u64>,
}
struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    limits: Limits,
    data_inflight: std::sync::atomic::AtomicUsize,
    control_inflight: std::sync::atomic::AtomicUsize,
}
struct Completion {
    shared: Arc<Shared>,
    // Keep the admission owner alive through delivery, including after shutdown.
    _generation: Arc<lightroom_managed::Generation>,
    control: bool,
}
impl Drop for Completion {
    fn drop(&mut self) {
        let used = if self.control {
            &self.shared.control_inflight
        } else {
            &self.shared.data_inflight
        };
        used.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.shared.wake.notify_all();
    }
}
fn cancellation_callback(
    shared: std::sync::Weak<Shared>,
    generation: std::sync::Weak<lightroom_managed::Generation>,
    id: u64,
) -> impl Fn() + Send + Sync {
    move || {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let interrupt = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            if queue.active == Some(id) {
                queue.stopping = true;
                shared.wake.notify_all();
                true
            } else {
                false
            }
        };
        if interrupt && let Some(generation) = generation.upgrade() {
            let _ = generation.interrupt();
        }
    }
}

pub(super) struct Dispatcher {
    shared: Arc<Shared>,
    generation: Arc<lightroom_managed::Generation>,
    worker: Mutex<Option<JoinHandle<()>>>,
    shutdown: Mutex<()>,
}
impl Dispatcher {
    /// The caller retains the generation if creating the dispatcher fails.
    pub(super) fn start(
        generation: &Arc<lightroom_managed::Generation>,
        limits: Limits,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            limits,
            data_inflight: std::sync::atomic::AtomicUsize::new(0),
            control_inflight: std::sync::atomic::AtomicUsize::new(0),
        });
        let owner = generation.clone();
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("desktop-workbench-dispatch".into())
            .spawn(move || run(&state, &owner))?;
        Ok(Self {
            shared,
            generation: generation.clone(),
            worker: Mutex::new(Some(worker)),
            shutdown: Mutex::new(()),
        })
    }

    pub(super) fn submit(&self, request: lightroom_bridge::Request) -> Result<Pending> {
        let request = Request::Lightroom {
            request: Box::new(request),
        };
        validate_public_request(&request, self.shared.limits.request_bytes.min(128 * 1024))?;
        let Request::Lightroom { request } = request else {
            unreachable!()
        };
        let control = matches!(
            request.as_ref(),
            lightroom_bridge::Request::Status { .. }
                | lightroom_bridge::Request::Cancel { .. }
                | lightroom_bridge::Request::Close { .. }
        );
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.stopping {
            return Err(error(ErrorCode::Closed, "Workbench dispatcher is stopping"));
        }
        let maximum = if control {
            CONTROL_SLOTS
        } else {
            self.shared.limits.queued
        };
        if control && queue.active.is_some() {
            return Err(error(
                ErrorCode::Busy,
                "Workbench transport request is in progress",
            ));
        }
        let inflight = if control {
            &self.shared.control_inflight
        } else {
            &self.shared.data_inflight
        };
        let used = inflight.load(std::sync::atomic::Ordering::Acquire);
        if used >= maximum {
            return Err(error(
                ErrorCode::ResourceLimit,
                "Workbench request queue is full",
            ));
        }
        queue.next = queue.next.checked_add(1).ok_or_else(|| {
            error(
                ErrorCode::ResourceLimit,
                "Workbench request identity exhausted",
            )
        })?;
        let id = queue.next;
        let shared = Arc::downgrade(&self.shared);
        let generation = Arc::downgrade(&self.generation);
        let cancel = Cancellation(
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Some(Arc::new(cancellation_callback(shared, generation, id))),
        );
        let (reply, receiver) = mpsc::sync_channel(1);
        inflight.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let completion = Arc::new(Completion {
            shared: self.shared.clone(),
            _generation: self.generation.clone(),
            control,
        });
        let target = if control {
            &mut queue.control
        } else {
            &mut queue.data
        };
        target.push_back(Entry {
            id,
            request: *request,
            reply,
            cancel: cancel.clone(),
            _completion: completion.clone(),
        });
        self.shared.wake.notify_one();
        Ok(Pending {
            receiver,
            cancel,
            completion: Some(Box::new(completion)),
        })
    }

    pub(super) fn signal_shutdown(&self) {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.stopping = true;
        let active = queue.active.is_some();
        self.shared.wake.notify_all();
        drop(queue);
        if active {
            let _ = self.generation.interrupt();
        }
    }

    pub(super) fn shutdown_checked(&self) -> Result<()> {
        let _attempt = self.shutdown.lock().unwrap_or_else(|e| e.into_inner());
        self.signal_shutdown();
        let joined = self
            .worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_none_or(|worker| worker.join().is_ok());
        // Even a dispatcher panic must reach checked W/S cleanup. Retain the
        // same generation in this façade when cleanup needs another attempt.
        self.generation
            .shutdown_checked()
            .map_err(crate::application::native)?;
        if !joined {
            return Err(error(ErrorCode::Native, "Workbench dispatcher panicked"));
        }
        Ok(())
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        let _ = self.shutdown_checked();
    }
}

fn run(shared: &Shared, generation: &lightroom_managed::Generation) {
    // A panicking dispatcher must reject retained and future requests too.
    struct Stop<'a>(&'a Shared);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let mut queue = self.0.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.stopping = true;
            for entry in queue.control.drain(..) {
                let _ = entry
                    .reply
                    .send(failure(ErrorCode::Closed, "Workbench dispatcher stopped"));
            }
            for entry in queue.data.drain(..) {
                let _ = entry
                    .reply
                    .send(failure(ErrorCode::Closed, "Workbench dispatcher stopped"));
            }
            self.0.wake.notify_all();
        }
    }
    let _stop = Stop(shared);
    loop {
        let entry = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            while !queue.stopping && queue.control.is_empty() && queue.data.is_empty() {
                queue = shared.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
            if queue.stopping {
                return;
            }
            let entry = queue
                .control
                .pop_front()
                .or_else(|| queue.data.pop_front())
                .unwrap();
            queue.active = Some(entry.id);
            entry
        };
        let reply = if entry.cancel.is_canceled() {
            failure(
                ErrorCode::Canceled,
                "Workbench request canceled before execution",
            )
        } else {
            match generation.call(entry.request) {
                Ok(value) => Reply::Ok {
                    value: Response::Lightroom(Box::new(value)),
                },
                Err(error) => Reply::Error {
                    error: crate::application::native(error),
                },
            }
        };
        let maximum = shared
            .limits
            .reply_bytes
            .min(shared.limits.request_bytes)
            .min(128 * 1024);
        let reply = if crate::lightroom::bounded_json(&reply, maximum).is_ok() {
            reply
        } else {
            failure(ErrorCode::ResourceLimit, "Workbench response byte limit")
        };
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        // One producer sends once into a one-slot channel, so this cannot wait
        // for the receiver. Publish completion atomically with the active id.
        let _ = entry.reply.send(reply);
        queue.active = None;
        shared.wake.notify_all();
    }
}

pub(crate) fn metadata_layouts() -> [(usize, usize); 3] {
    [
        (
            std::mem::size_of::<Dispatcher>(),
            std::mem::align_of::<Dispatcher>(),
        ),
        (
            std::mem::size_of::<Shared>(),
            std::mem::align_of::<Shared>(),
        ),
        (std::mem::size_of::<Entry>(), std::mem::align_of::<Entry>()),
    ]
}

#[allow(dead_code)] // Consumed by the complete admission integration.
pub(crate) fn completion_metadata_layouts() -> [(usize, usize); 2] {
    let callback = cancellation_callback(std::sync::Weak::new(), std::sync::Weak::new(), 0);
    [
        (
            std::mem::size_of::<Completion>(),
            std::mem::align_of::<Completion>(),
        ),
        (
            std::mem::size_of_val(&callback),
            std::mem::align_of_val(&callback),
        ),
    ]
}

#[allow(dead_code)] // Consumed by the complete admission integration.
pub(crate) fn completion_channel_backing() -> anyhow::Result<usize> {
    crate::lightroom_migration_worker::memory::channels::bounded(
        1,
        std::alloc::Layout::new::<Reply>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};
    use std::time::{Duration, Instant};

    fn wait_until(mut ready: impl FnMut() -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            ensure!(
                Instant::now() < deadline,
                "Workbench dispatcher fixture timed out"
            );
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    #[test]
    fn completed_replies_retain_admission_until_receive_or_drop() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let fixture = lightroom_managed::tests::ManagedFixture::start(temp.path())?;
        let generation = Arc::new(lightroom_managed::Generation::start_fixture(
            &fixture.owner,
            &std::env::current_exe()?,
        )?);
        let dispatcher = Dispatcher::start(
            &generation,
            Limits {
                queued: 2,
                ..Limits::default()
            },
        )?;
        let first = dispatcher.submit(lightroom_bridge::Request::Options {})?;
        let second = dispatcher.submit(lightroom_bridge::Request::Options {})?;
        wait_until(|| {
            let queue = dispatcher.shared.queue.lock().unwrap();
            queue.data.is_empty() && queue.active.is_none()
        })?;
        assert!(
            matches!(dispatcher.submit(lightroom_bridge::Request::Options {}), Err(error) if matches!(error.code, ErrorCode::ResourceLimit))
        );
        assert!(matches!(first.recv(), Reply::Ok { .. }));
        let third = dispatcher.submit(lightroom_bridge::Request::Options {})?;
        drop(second);
        let fourth = dispatcher.submit(lightroom_bridge::Request::Options {})?;
        assert!(matches!(third.recv(), Reply::Ok { .. }));
        assert!(matches!(fourth.recv(), Reply::Ok { .. }));
        dispatcher.shutdown_checked()?;
        Ok(())
    }

    fn stalled_call(shutdown: bool) -> Result<()> {
        let temp = tempfile::tempdir()?;
        let fixture = lightroom_managed::tests::ManagedFixture::start(temp.path())?;
        let generation = Arc::new(lightroom_managed::Generation::start_fixture(
            &fixture.owner,
            &std::env::current_exe()?,
        )?);
        let dispatcher = Dispatcher::start(&generation, Limits::default())?;
        let marker = temp.path().join("workbench-stalled");
        let pending = dispatcher.submit(lightroom_bridge::Request::Status {
            workbench: None,
            attempt: Some(format!("fixture-stall:{}", marker.display())),
        })?;
        wait_until(|| marker.exists())?;
        let start = Instant::now();
        assert!(
            matches!(dispatcher.submit(lightroom_bridge::Request::Status { workbench: None, attempt: None }), Err(error) if matches!(error.code, ErrorCode::Busy))
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        if shutdown {
            dispatcher.shutdown_checked()?;
        } else {
            pending.cancel();
        }
        let reply = pending.receiver.recv_timeout(Duration::from_secs(10))?;
        assert!(matches!(reply, Reply::Error { .. }));
        dispatcher.shutdown_checked()?;
        assert!(generation.pid().is_none());
        Ok(())
    }

    #[test]
    fn pending_cancellation_interrupts_a_stalled_owned_workbench() -> Result<()> {
        stalled_call(false)
    }

    #[test]
    fn shutdown_interrupts_a_stalled_owned_workbench_before_join() -> Result<()> {
        stalled_call(true)
    }
}
