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
    request: lightroom_bridge::Request,
    reply: mpsc::SyncSender<Reply>,
    cancel: Cancellation,
}
#[derive(Default)]
struct Queue {
    data: VecDeque<Entry>,
    control: VecDeque<Entry>,
    stopping: bool,
}
struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    limits: Limits,
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
        let target = if control {
            &mut queue.control
        } else {
            &mut queue.data
        };
        if target.len() >= maximum {
            return Err(error(
                ErrorCode::ResourceLimit,
                "Workbench request queue is full",
            ));
        }
        let cancel = Cancellation::default();
        let (reply, receiver) = mpsc::sync_channel(1);
        target.push_back(Entry {
            request: *request,
            reply,
            cancel: cancel.clone(),
        });
        self.shared.wake.notify_one();
        Ok(Pending { receiver, cancel })
    }

    pub(super) fn signal_shutdown(&self) {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.stopping = true;
        self.shared.wake.notify_all();
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
            queue
                .control
                .pop_front()
                .or_else(|| queue.data.pop_front())
                .unwrap()
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
        let _ = entry.reply.send(reply);
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
