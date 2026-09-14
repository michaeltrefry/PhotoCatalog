//! G owns each actual N Child and stdin writer independently of C lifetime.
use crate::application::U64;
use crate::catalog_session::{LeaseId, RootCapability, native::*};
use anyhow::{Context, Result, ensure};
use std::{
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Only the G adapter implements these calls; C cannot assert native drain.
pub(super) trait Stages: Send + Sync {
    fn arm(&self, root: &RootCapability, stage: &LeaseId, operation: U64) -> Result<PathBuf>;
    fn drained(&self, root: &RootCapability, stage: &LeaseId, operation: U64) -> Result<()>;
    fn header(&self, root: &RootCapability, stage: &LeaseId, operation: U64) -> Result<Header>;
    fn abandon(&self, _root: &RootCapability) -> Result<()> {
        anyhow::bail!("stage abandonment unavailable")
    }
}
struct SendState {
    input: Option<(ChildStdin, Vec<u8>)>,
    start: bool,
    encode: bool,
    stop: bool,
    initial_sent: bool,
    encode_sent: bool,
    #[cfg(all(test, unix))]
    hold_encode: bool,
    done: bool,
    error: Option<String>,
}
struct Slot {
    root: RootCapability,
    operation: U64,
    stage: LeaseId,
    digest: [u8; 32],
    child: Mutex<Option<Child>>,
    send: Mutex<SendState>,
    wake: Condvar,
    writer: Mutex<Option<JoinHandle<()>>>,
    reaper: Mutex<Option<JoinHandle<()>>>,
    status: Mutex<Status>,
    retry: AtomicBool,
    stop: AtomicBool,
    stages: Arc<dyn Stages>,
    grant: Mutex<Option<[u8; 32]>>,
    no_child_terminal: AtomicBool,
    charged: std::sync::atomic::AtomicU64,
    work: Work,
}
fn bounded(error: impl std::fmt::Display) -> String {
    struct Prefix(String);
    impl std::fmt::Write for Prefix {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            let mut n = value.len().min(ERROR_BYTES - self.0.len());
            while !value.is_char_boundary(n) {
                n -= 1;
            }
            self.0.push_str(&value[..n]);
            Ok(())
        }
    }
    let mut output = Prefix(String::with_capacity(ERROR_BYTES));
    let _ = std::fmt::write(&mut output, format_args!("{error}"));
    output.0
}
impl Slot {
    fn status(&self) -> Status {
        let mut value = self
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let send = self.send.lock().unwrap_or_else(|p| p.into_inner());
        value.initial_sent = send.initial_sent;
        value.encode_sent = send.encode_sent;
        if value.error.is_none() {
            value.error = send.error.clone();
        }
        value
    }
    fn failure(&self, phase: Phase, error: impl std::fmt::Display) {
        let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
        s.phase = phase;
        s.error = Some(bounded(error));
        self.retry.store(false, Ordering::Release);
    }
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        {
            let mut s = self.send.lock().unwrap_or_else(|p| p.into_inner());
            s.stop = true;
        }
        self.wake.notify_all();
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(child) = child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(e) = child.kill() {
                        self.failure(Phase::WaitFailed, format_args!("native kill failed: {e}"));
                        return;
                    }
                }
                Err(e) => {
                    self.failure(
                        Phase::WaitFailed,
                        format_args!("native observation failed: {e}"),
                    );
                    return;
                }
            }
        }
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        if !matches!(
            status.phase,
            Phase::Drained | Phase::WaitFailed | Phase::PipeJoinFailed | Phase::ExitObserved
        ) {
            status.phase = Phase::StopRequested;
        }
    }
    fn start_writer(self: &Arc<Self>) -> Result<()> {
        let owner = self.clone();
        let handle = thread::Builder::new()
            .name("preview-native-input".into())
            .spawn(move || {
                let input = {
                    let mut s = owner.send.lock().unwrap_or_else(|p| p.into_inner());
                    while !s.start && !s.stop {
                        s = owner.wake.wait(s).unwrap_or_else(|p| p.into_inner());
                    }
                    if s.stop {
                        s.input.take();
                        s.done = true;
                        return;
                    }
                    s.input.take()
                };
                let result = (|| -> Result<()> {
                    use std::io::Write;
                    let (mut input, bytes) = input.context("native stdin owner missing")?;
                    {
                        let mut status = owner.status.lock().unwrap_or_else(|p| p.into_inner());
                        if status.phase == Phase::Spawned {
                            status.phase = Phase::Sending;
                        }
                    }
                    input.write_all(&bytes)?;
                    input.write_all(b"\n!")?;
                    input.flush()?;
                    drop(bytes);
                    {
                        let mut status = owner.status.lock().unwrap_or_else(|p| p.into_inner());
                        if status.phase == Phase::Sending {
                            status.phase = Phase::Running;
                        }
                    }
                    {
                        let mut s = owner.send.lock().unwrap_or_else(|p| p.into_inner());
                        s.initial_sent = true;
                        while !s.encode && !s.stop {
                            s = owner.wake.wait(s).unwrap_or_else(|p| p.into_inner());
                        }
                        #[cfg(all(test, unix))]
                        while s.hold_encode && !s.stop {
                            s = owner.wake.wait(s).unwrap_or_else(|p| p.into_inner());
                        }
                        if s.stop {
                            return Ok(());
                        }
                    }
                    input.write_all(b"E")?;
                    input.flush()?;
                    let mut s = owner.send.lock().unwrap_or_else(|p| p.into_inner());
                    s.encode_sent = true;
                    while !s.stop {
                        s = owner.wake.wait(s).unwrap_or_else(|p| p.into_inner());
                    }
                    Ok(())
                })();
                let mut s = owner.send.lock().unwrap_or_else(|p| p.into_inner());
                s.done = true;
                if let Err(e) = result {
                    s.error = Some(bounded(e));
                    owner.stop.store(true, Ordering::Release);
                }
                owner.wake.notify_all();
            })?;
        *self.writer.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
        Ok(())
    }
    fn start_reaper(self: &Arc<Self>) -> Result<()> {
        let mut retained = self.reaper.lock().unwrap_or_else(|p| p.into_inner());
        if retained.as_ref().is_some_and(|h| !h.is_finished()) {
            return Ok(());
        }
        if let Some(h) = retained.take()
            && h.join().is_err()
        {
            self.failure(
                Phase::WaitFailed,
                "native reaper panic; retry retained child",
            );
        }
        let owner = self.clone();
        *retained=Some(thread::Builder::new().name("preview-native-reap".into()).spawn(move||{
            let outcome=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||loop {
                if !owner.retry.load(Ordering::Acquire){thread::sleep(Duration::from_millis(5));continue;}
                let writer_panicked=owner.writer.lock().unwrap_or_else(|p|p.into_inner()).as_ref().is_some_and(|h|h.is_finished())&&!owner.send.lock().unwrap_or_else(|p|p.into_inner()).done;
                if writer_panicked{owner.stop.store(true,Ordering::Release);}
                if owner.stop.load(Ordering::Acquire){owner.stop();}
                // No blocking wait while the child can still run. The lock is
                // released between every try_wait so Stop can always reach kill.
                let exit={
                    let mut slot=owner.child.lock().unwrap_or_else(|p|p.into_inner());
                    match slot.as_mut(){
                        Some(child)=>match child.try_wait(){
                            Ok(Some(_))=>match child.wait(){Ok(status)=>{slot.take();Some(Ok(status))},Err(e)=>Some(Err(e))},
                            Ok(None)=>None,Err(e)=>Some(Err(e)),
                        },
                        None=>None,
                    }
                };
                if let Some(exit)=exit {
                    match exit {
                        Err(e)=>{owner.failure(Phase::WaitFailed,e);continue;}
                        Ok(exit)=>{let mut s=owner.status.lock().unwrap_or_else(|p|p.into_inner());s.phase=Phase::ExitObserved;s.exit_code=exit.code();s.success=Some(exit.success());}
                    }
                    {let mut s=owner.send.lock().unwrap_or_else(|p|p.into_inner());s.stop=true;s.input.take();}owner.wake.notify_all();
                }
                let observed=owner.no_child_terminal.load(Ordering::Acquire)||owner.status.lock().unwrap_or_else(|p|p.into_inner()).success.is_some();
                if observed {
                    let mut writer=owner.writer.lock().unwrap_or_else(|p|p.into_inner());
                    if writer.as_ref().is_some_and(|h|!h.is_finished()){drop(writer);thread::sleep(Duration::from_millis(2));continue;}
                    if let Some(h)=writer.take() && h.join().is_err(){drop(writer);owner.failure(Phase::PipeJoinFailed,"native input thread panicked; checked drain retry required");continue;}
                    drop(writer);
                    match owner.stages.drained(&owner.root,&owner.stage,owner.operation){
                        Ok(())=>{owner.status.lock().unwrap_or_else(|p|p.into_inner()).phase=Phase::Drained;break;}
                        Err(e)=>{owner.failure(Phase::PipeJoinFailed,format_args!("native reaped; stage drain acknowledgement unresolved: {e}"));continue;}
                    }
                }
                thread::sleep(Duration::from_millis(2));
            }));
            if outcome.is_err(){owner.failure(Phase::WaitFailed,"native reaper panicked; owning state retained for retry");}
        })?);
        Ok(())
    }
}
/// Slots are bounded by configured workers. Retired IDs are rejected by the
/// high-water fence; active exact identities retain their original child.
pub(super) struct Owner {
    #[cfg(all(test, unix))]
    hold_encode: AtomicBool,
    #[cfg(test)]
    before_register: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    admission: Mutex<()>,
    root_closing: AtomicBool,
    early_stop: Mutex<Vec<Key>>,
    executable: PathBuf,
    stages: Arc<dyn Stages>,
    slots: Mutex<Vec<Arc<Slot>>>,
    high_water: Mutex<u64>,
    limits: crate::preview::ServiceLimits,
    selected: Mutex<Option<RootCapability>>,
}
impl Owner {
    pub fn new(
        executable: PathBuf,
        stages: Arc<dyn Stages>,
        limits: crate::preview::ServiceLimits,
    ) -> Self {
        Self {
            #[cfg(all(test, unix))]
            hold_encode: AtomicBool::new(false),
            #[cfg(test)]
            before_register: Mutex::new(None),
            admission: Mutex::new(()),
            root_closing: AtomicBool::new(false),
            early_stop: Mutex::new(Vec::new()),
            executable,
            stages,
            slots: Mutex::new(Vec::new()),
            high_water: Mutex::new(0),
            limits,
            selected: Mutex::new(None),
        }
    }
    /// Deterministic fixture boundary: C may grant Encode, but each independent
    /// writer waits before sending E. Production Stop still bypasses the hold.
    #[cfg(all(test, unix))]
    pub(super) fn test_hold_encodes(&self) -> Result<()> {
        let _admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            self.slots
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty(),
            "test hold after native admission"
        );
        self.hold_encode.store(true, Ordering::Release);
        Ok(())
    }
    #[cfg(all(test, unix))]
    pub(super) fn test_encode_is_held(
        &self,
        root: &RootCapability,
        operation: U64,
    ) -> Result<bool> {
        let slot = self.compact(&Key::new(root, operation))?;
        let send = slot.send.lock().unwrap_or_else(|p| p.into_inner());
        Ok(send.hold_encode && send.initial_sent && send.encode && !send.encode_sent && !send.stop)
    }
    pub fn bind(&self, root: &RootCapability) -> Result<()> {
        let _admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        let mut selected = self.selected.lock().unwrap_or_else(|p| p.into_inner());
        if selected.as_ref() == Some(root) {
            return Ok(());
        }
        ensure!(
            self.slots
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty(),
            "previous catalog native ownership remains retained"
        );
        self.early_stop
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        ensure!(
            selected.is_none(),
            "previous native root binding remains retained"
        );
        self.root_closing.store(false, Ordering::Release);
        *selected = Some(root.clone());
        *self.high_water.lock().unwrap_or_else(|p| p.into_inner()) = 0;
        Ok(())
    }
    fn compact(&self, key: &Key) -> Result<Arc<Slot>> {
        key.validate()?;
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|s| s.operation == key.operation && key.matches(&s.root))
            .cloned()
            .context("native query owner not retained")
    }
    pub fn stop_key(&self, key: &Key) -> Result<()> {
        key.validate()?;
        let _admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok(slot) = self.compact(key) {
            slot.stop();
            return Ok(());
        }
        ensure!(
            self.selected
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .is_some_and(|root| key.matches(root)),
            "native stop root mismatch"
        );
        // A cleanup control can overtake the ordinary Spawn packet. Preserve
        // its exact identity until registration; never let it create a child.
        if key.operation.0 <= *self.high_water.lock().unwrap_or_else(|p| p.into_inner()) {
            return Ok(());
        }
        let mut early = self.early_stop.lock().unwrap_or_else(|p| p.into_inner());
        if !early.contains(key) {
            ensure!(
                early.len() < self.limits.workers,
                "native early-stop capacity"
            );
            early.push(key.clone());
        }
        Ok(())
    }
    pub fn query(&self, q: &Query) -> Result<Status> {
        let slot = self.compact(&q.key)?;
        match q.action {
            QueryAction::Status => Ok(slot.status()),
            QueryAction::RetryDrain => self.call(&Request {
                root: slot.root.clone(),
                operation: slot.operation,
                action: Action::Drain,
            }),
            QueryAction::Retire => self.call(&Request {
                root: slot.root.clone(),
                operation: slot.operation,
                action: Action::Retire,
            }),
        }
    }
    pub fn retire_root(&self, root: &RootCapability) -> Result<()> {
        let _admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            self.selected
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                == Some(root),
            "native root retirement mismatch"
        );
        self.root_closing.store(true, Ordering::Release);
        let slots = self.slots.lock().unwrap_or_else(|p| p.into_inner()).clone();
        for slot in slots.iter().filter(|s| s.root == *root) {
            ensure!(
                slot.status().phase == Phase::Drained,
                "catalog native owner has not drained"
            );
        }
        for slot in slots.iter().filter(|s| s.root == *root) {
            self.call(&Request {
                root: root.clone(),
                operation: slot.operation,
                action: Action::Retire,
            })?;
        }
        Ok(())
    }
    /// Only a confirmed F ReleaseRoot may discard the supervisor cleanup binding.
    pub fn forget_released_root(&self, root: &RootCapability) -> Result<()> {
        let _admission = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            self.root_closing.load(Ordering::Acquire),
            "native root was not closed"
        );
        ensure!(
            self.slots
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty(),
            "native slots remain retained"
        );
        let mut selected = self.selected.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            selected.as_ref() == Some(root),
            "released native root mismatch"
        );
        *selected = None;
        Ok(())
    }
    fn slot(&self, root: &RootCapability, operation: U64) -> Result<Arc<Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|s| s.operation == operation && s.root == *root)
            .cloned()
            .context("native operation is not retained")
    }
    #[cfg(test)]
    pub fn status(&self, root: &RootCapability, operation: U64) -> Result<Status> {
        Ok(self.slot(root, operation)?.status())
    }
    pub fn call(&self, r: &Request) -> Result<Status> {
        r.validate()?;
        // Bind/retire cannot pass an admitted-but-not-registered Spawn. The
        // guard ends after registration, before F arm or process IO.
        let admission = if matches!(r.action, Action::Spawn { .. }) {
            Some(self.admission.lock().unwrap_or_else(|p| p.into_inner()))
        } else {
            None
        };
        ensure!(
            self.selected
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                == Some(&r.root),
            "native catalog is not selected/confirmed"
        );
        if let Action::Spawn {
            stage,
            work,
            workers,
            working_bytes,
        } = &r.action
        {
            ensure!(
                !self.root_closing.load(Ordering::Acquire),
                "native root is closing; new spawn refused"
            );
            let bytes = Envelope {
                operation: r.operation,
                stage: stage.clone(),
                work: work.clone(),
            }
            .bytes()?;
            let digest = *blake3::hash(&serde_json::to_vec(r)?).as_bytes();
            if let Ok(slot) = self.slot(&r.root, r.operation) {
                ensure!(slot.digest == digest, "changed native spawn identity");
                return Ok(slot.status());
            }
            let required = match work {
                Work::Render(render) => render_cost(
                    render,
                    self.limits.per_worker_bytes,
                    self.limits.cache_codec_scratch_bytes,
                )?,
                Work::DecodeEncoded {
                    codec,
                    encoded_bytes,
                    expected_dimensions,
                    ..
                } => match expected_dimensions {
                    Some((w, h)) => decode_cost(
                        *codec,
                        *w,
                        *h,
                        encoded_bytes.0,
                        self.limits.cache_header_scratch_bytes,
                        self.limits.cache_codec_scratch_bytes,
                    )?,
                    None => header_cost(encoded_bytes.0, self.limits.cache_header_scratch_bytes)?,
                },
            };
            ensure!(
                working_bytes.0 == required && usize::from(*workers) == self.limits.workers,
                "native configured admission mismatch"
            );
            let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
            let held = slots.iter().try_fold(0u64, |n, s| {
                n.checked_add(s.charged.load(Ordering::Acquire))
                    .context("native aggregate overflow")
            })?;
            ensure!(
                required <= self.limits.working_bytes.saturating_sub(held),
                crate::catalog_session::store::ResourceLimit(
                    "Native working allowance remains owned; drain and retry"
                )
            );
            ensure!(
                slots.len() < usize::from(*workers),
                crate::catalog_session::store::ResourceLimit(
                    "Native slots remain owned; wait for checked drain and retry"
                )
            );
            let mut high = self.high_water.lock().unwrap_or_else(|p| p.into_inner());
            ensure!(r.operation.0 > *high, "stale native spawn identity");
            slots.try_reserve(1).context("native slot allocation")?;
            #[cfg(test)]
            if let Some(hook) = self.before_register.lock().unwrap().as_ref() {
                hook();
            }
            let slot = Arc::new(Slot {
                root: r.root.clone(),
                operation: r.operation,
                stage: stage.clone(),
                digest,
                child: Mutex::new(None),
                send: Mutex::new(SendState {
                    input: None,
                    start: false,
                    encode: false,
                    stop: false,
                    initial_sent: false,
                    encode_sent: false,
                    #[cfg(all(test, unix))]
                    hold_encode: self.hold_encode.load(Ordering::Acquire),
                    done: false,
                    error: None,
                }),
                wake: Condvar::new(),
                writer: Mutex::new(None),
                reaper: Mutex::new(None),
                status: Mutex::new(Status {
                    epoch: r.root.epoch.clone(),
                    session: r.root.session.clone(),
                    operation: r.operation,
                    stage: stage.clone(),
                    pid: None,
                    phase: Phase::Preparing,
                    initial_sent: false,
                    encode_sent: false,
                    exit_code: None,
                    success: None,
                    error: None,
                }),
                retry: AtomicBool::new(true),
                stop: AtomicBool::new(false),
                stages: self.stages.clone(),
                grant: Mutex::new(None),
                no_child_terminal: AtomicBool::new(false),
                charged: std::sync::atomic::AtomicU64::new(required),
                work: work.clone(),
            });
            *high = r.operation.0;
            {
                let mut early = self.early_stop.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(index) = early
                    .iter()
                    .position(|key| key.operation == r.operation && key.matches(&r.root))
                {
                    early.remove(index);
                    slot.stop.store(true, Ordering::Release);
                }
            }
            slots.push(slot.clone());
            drop(slots);
            drop(high);
            drop(admission);
            // Register the stage/operation before even F arm. A failed arm or
            // OS spawn is retained as a proven no-child terminal attempt.
            let spawned = (|| -> Result<Child> {
                let cwd = self.stages.arm(&r.root, stage, r.operation)?;
                ensure!(
                    !slot.stop.load(Ordering::Acquire),
                    "native canceled before OS spawn"
                );
                Ok(Command::new(&self.executable)
                    .arg("--preview-worker")
                    .current_dir(cwd)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .env("OMP_NUM_THREADS", "1")
                    .env("RAYON_NUM_THREADS", "1")
                    .spawn()?)
            })();
            let mut child = match spawned {
                Ok(child) => child,
                Err(e) => {
                    slot.no_child_terminal.store(true, Ordering::Release);
                    slot.failure(
                        Phase::WaitFailed,
                        format_args!("native launch failed before a Child was returned: {e}"),
                    );
                    return Ok(slot.status());
                }
            };
            let pid = child.id();
            let input = child.stdin.take().map(|input| (input, bytes));
            *slot.child.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
            slot.send.lock().unwrap_or_else(|p| p.into_inner()).input = input;
            {
                let mut status = slot.status.lock().unwrap_or_else(|p| p.into_inner());
                status.pid = Some(pid);
                status.phase = Phase::Spawned;
            }
            // The registry already owns Child, stdin and request before either
            // fallible thread creation or any send. Failed startup keeps it.
            if let Err(e) = slot.start_writer() {
                slot.failure(Phase::PipeJoinFailed, e);
                slot.stop();
                if slot.status().phase == Phase::StopRequested {
                    slot.retry.store(true, Ordering::Release);
                }
            }
            if let Err(e) = slot.start_reaper() {
                slot.failure(Phase::WaitFailed, e);
            }
            return Ok(slot.status());
        }
        let slot = self.slot(&r.root, r.operation)?;
        match &r.action {
            Action::Start => {
                ensure!(slot.status().pid.is_some(), "native child was not launched");
                let mut s = slot.send.lock().unwrap_or_else(|p| p.into_inner());
                ensure!(!s.stop, "native already stopping");
                s.start = true;
                slot.wake.notify_all();
            }
            Action::Encode {
                header,
                working_bytes,
                rgb_bytes: rgb_grant,
            } => {
                if let Some(h) = header {
                    ensure!(h.stage == slot.stage, "native header stage");
                    let observed = self.stages.header(&r.root, &slot.stage, r.operation)?;
                    ensure!(
                        *h == observed,
                        "native encode grant differs from immutable F header"
                    );
                }
                let required = match (&slot.work, header) {
                    (Work::Render(render), None) => {
                        let expected = render.keys.iter().try_fold(0u64, |sum, key| {
                            sum.checked_add(rgb_bytes(key.edge, key.edge)?)
                                .context("render RGB grant overflow")
                        })?;
                        ensure!(rgb_grant.0 == expected, "render RGB grant mismatch");
                        render_cost(
                            render,
                            self.limits.per_worker_bytes,
                            self.limits.cache_codec_scratch_bytes,
                        )?
                    }
                    (
                        Work::DecodeEncoded {
                            codec,
                            encoded_bytes,
                            encoded_digest,
                            expected_dimensions,
                        },
                        Some(h),
                    ) => {
                        ensure!(
                            *codec == h.codec
                                && *encoded_bytes == h.input_bytes
                                && *encoded_digest == h.input_digest
                                && expected_dimensions.is_none_or(|v| v == (h.width, h.height)),
                            "native header input binding"
                        );
                        ensure!(
                            rgb_grant.0 == rgb_bytes(h.width, h.height)?,
                            "native RGB grant length"
                        );
                        decode_cost(
                            *codec,
                            h.width,
                            h.height,
                            encoded_bytes.0,
                            self.limits.cache_header_scratch_bytes,
                            self.limits.cache_codec_scratch_bytes,
                        )?
                    }
                    _ => anyhow::bail!("native grant mode"),
                };
                ensure!(
                    working_bytes.0 == required,
                    "native working upgrade mismatch"
                );
                let slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
                let other =
                    slots
                        .iter()
                        .filter(|s| !Arc::ptr_eq(s, &slot))
                        .try_fold(0u64, |n, s| {
                            n.checked_add(s.charged.load(Ordering::Acquire))
                                .context("native aggregate overflow")
                        })?;
                ensure!(
                    required <= self.limits.working_bytes.saturating_sub(other),
                    crate::catalog_session::store::ResourceLimit(
                        "Native upgrade is temporarily occupied; stop/drain header owner and retry at full cost"
                    )
                );
                slot.charged.store(required, Ordering::Release);
                drop(slots);
                let digest = *blake3::hash(&serde_json::to_vec(&r.action)?).as_bytes();
                let mut grant = slot.grant.lock().unwrap_or_else(|p| p.into_inner());
                ensure!(
                    grant.as_ref().is_none_or(|old| *old == digest),
                    "changed native encode grant"
                );
                let mut s = slot.send.lock().unwrap_or_else(|p| p.into_inner());
                ensure!(
                    s.start && !s.stop,
                    "native encode before start or after stop"
                );
                *grant = Some(digest);
                s.encode = true;
                slot.wake.notify_all();
            }
            Action::Stop => slot.stop(),
            Action::Drain => {
                slot.retry.store(true, Ordering::Release);
                slot.start_reaper()?;
            }
            Action::Retire => {
                ensure!(
                    slot.status().phase == Phase::Drained,
                    "native owner is not drained"
                );
                if let Some(h) = slot.reaper.lock().unwrap_or_else(|p| p.into_inner()).take() {
                    h.join()
                        .map_err(|_| anyhow::anyhow!("native reaper join failed"))?;
                }
                self.slots
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .retain(|s| !Arc::ptr_eq(s, &slot));
            }
            Action::Spawn { .. } => unreachable!(),
        }
        Ok(slot.status())
    }
    /// Caller has checked C retirement and joined its writers. G still proves
    /// every N wait/pipe join before asking F to abandon any remaining stage.
    pub fn finish_after_catalog(&self) -> Result<()> {
        self.stop_all();
        let slots = self.slots.lock().unwrap_or_else(|p| p.into_inner()).clone();
        for slot in &slots {
            slot.retry.store(true, Ordering::Release);
            slot.start_reaper()?;
        }
        loop {
            let states: Vec<_> = slots.iter().map(|slot| slot.status()).collect();
            ensure!(
                !states
                    .iter()
                    .any(|s| matches!(s.phase, Phase::WaitFailed | Phase::PipeJoinFailed)),
                "native drain failed; exact owners and F stages retained for retry"
            );
            if states.iter().all(|s| s.phase == Phase::Drained) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let root = self
            .selected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(root) = root {
            self.stages.abandon(&root)?;
            self.retire_root(&root)?;
        }
        Ok(())
    }
    pub fn stop_all(&self) {
        for s in self.slots.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            s.stop();
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let slots = self.slots.get_mut().unwrap_or_else(|p| p.into_inner());
        for slot in slots.drain(..) {
            slot.stop();
            if slot.status().phase == Phase::Drained {
                if let Some(handle) = slot.reaper.lock().unwrap_or_else(|p| p.into_inner()).take() {
                    let _ = handle.join();
                }
            } else {
                let _ = slot.start_reaper();
                // Checked callers retain Owner and retry. Unwind must not drop
                // a Child or unjoined fallback owner after failed thread start.
                std::mem::forget(slot);
            }
        }
    }
}

#[cfg(test)]
mod tests;
