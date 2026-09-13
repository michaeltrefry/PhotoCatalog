use super::*;
use crate::{
    catalog_storage::RelinkWorkerHandle,
    export_service::{ExportEvent, ExportService, ExportServiceLimits},
    image_export::{OutputFormat, OutputProfile, OutputSpec},
};
use anyhow::{Context as _, ensure};
use std::io::Read;
pub(super) fn options(c: &Config) -> Options {
    let worker = c.preview_limits.per_worker_bytes;
    let render = RenderLimits {
        max_pixels: U64(100_000_000),
        max_allocation_bytes: U64((2 * 1024 * 1024 * 1024).min(worker)),
        max_live_bytes: U64(worker),
    };
    Options {
        budgets: Budgets {
            max_original_bytes: U64(256 * 1024 * 1024),
            max_payload_bytes: U64(2 * 1024 * 1024 * 1024),
            alias_limits: AliasLimits {
                directories: U64(4096),
                candidates: U64(256),
            },
        },
        execution: ExecutionLimits {
            worker_bytes: U64(worker),
            working_bytes: U64(c.preview_limits.working_bytes),
            render: PhotoLimits {
                decode: DecodeLimits {
                    max_encoded_bytes: U64(c.preview_limits.decode_limits.max_encoded_bytes),
                    max_intermediate_pixels: U64(c
                        .preview_limits
                        .decode_limits
                        .max_intermediate_pixels),
                    max_allocation_bytes: U64(c.preview_limits.decode_limits.max_allocation_bytes),
                },
                render: render.clone(),
                encode: EncodeLimits {
                    render,
                    max_metadata_bytes: U64(PROFILE_BYTES as u64),
                    row_buffer_bytes: U64(PROFILE_BYTES as u64),
                },
                max_encoded_extent: U64(2 * 1024 * 1024 * 1024),
            },
        },
        page_rows: U64(u64::from(c.limits.page_rows)),
        page_bytes: U64(c.limits.page_bytes as u64),
        path_bytes: U64(PATH_BYTES as u64),
        profile_bytes: U64(PROFILE_BYTES as u64),
        profile_tokens: U64(8),
        profile_total_bytes: U64(2 * PROFILE_BYTES as u64),
        result_rows: U64(100),
    }
}
fn render(v: &RenderLimits) -> crate::edit::RenderLimits {
    crate::edit::RenderLimits {
        max_pixels: v.max_pixels.0,
        max_allocation_bytes: v.max_allocation_bytes.0,
        max_live_bytes: v.max_live_bytes.0,
    }
}
fn execution(v: &ExecutionLimits, config: &Config) -> anyhow::Result<ExportServiceLimits> {
    let limits = ExportServiceLimits {
        worker_bytes: v.worker_bytes.0,
        working_bytes: v.working_bytes.0,
        render: crate::photo_render::PhotoRenderLimits {
            decode: crate::media::DecodeLimits {
                max_encoded_bytes: v.render.decode.max_encoded_bytes.0,
                max_intermediate_pixels: v.render.decode.max_intermediate_pixels.0,
                max_allocation_bytes: v.render.decode.max_allocation_bytes.0,
            },
            render: render(&v.render.render),
            encode: crate::image_export::EncodeLimits {
                render: render(&v.render.encode.render),
                max_metadata_bytes: v.render.encode.max_metadata_bytes.0,
                row_buffer_bytes: v.render.encode.row_buffer_bytes.0,
            },
            max_encoded_extent: v.render.max_encoded_extent.0,
        },
    };
    ensure!(
        limits.working_bytes <= config.preview_limits.working_bytes,
        "export exceeds shared native memory allowance"
    );
    for v in [&v.render.render, &v.render.encode.render] {
        ensure!(
            v.max_pixels.0 > 0
                && v.max_pixels.0 <= 100_000_000
                && v.max_allocation_bytes.0 > 0
                && v.max_live_bytes.0 > 0,
            "positive bounded export pixel allowances required"
        );
    }
    ensure!(
        v.render.encode.max_metadata_bytes.0 > 0
            && v.render.encode.max_metadata_bytes.0 <= PROFILE_BYTES as u64
            && v.render.encode.row_buffer_bytes.0 > 0
            && v.render.encode.row_buffer_bytes.0 <= limits.worker_bytes,
        "export metadata/row allowance"
    );
    limits.validate()?;
    Ok(limits)
}
pub(super) fn validate(r: &Request, c: &Config, o: &Options) -> Result<()> {
    let bad = |m: &str| error(ErrorCode::InvalidRequest, m);
    match r {
        Request::Profile { path: p } => {
            path(p)?;
        }
        Request::Paths { limit } => {
            if !(1..=512).contains(&limit.0) {
                return Err(bad("alias projection limit1..512"));
            }
        }
        Request::Destinations {
            directory,
            targets,
            format,
            naming,
        } => {
            path(directory)?;
            if targets.is_empty() || targets.len() > 100 {
                return Err(bad("choose1..100 destination targets"));
            }
            for t in targets {
                t.key.validate().map_err(native)?;
                if t.expected_revision.0 < 0 {
                    return Err(bad("negative selected revision"));
                }
            }
            for s in [&naming.prefix, &naming.suffix] {
                if s.len() > 128
                    || s.chars().any(|c| {
                        c.is_control()
                            || ['/', '\\', ':', '*', '?', '"', '<', '>', '|'].contains(&c)
                    })
                {
                    return Err(bad("invalid destination prefix/suffix"));
                }
            }
            if let OutputFormat::Jpeg { quality } = format
                && !(1..=100).contains(quality)
            {
                return Err(bad("JPEG quality1..100"));
            }
        }
        Request::Append {
            expected_total,
            target,
            output,
            budgets,
            ..
        } => {
            path(&target.destination)?;
            target.key.validate().map_err(native)?;
            if expected_total.0 < 0 || target.expected_revision.0 < 0 {
                return Err(bad("negative export CAS revision"));
            }
            let b = budgets.as_ref().unwrap_or(&o.budgets);
            if b.max_original_bytes.0 == 0
                || b.max_payload_bytes.0 == 0
                || b.alias_limits.directories.0 > 65536
                || !(1..=4096).contains(&b.alias_limits.candidates.0)
            {
                return Err(bad("invalid export original/payload/alias budgets"));
            }
            if let Profile::Icc { token } = &output.profile {
                identity(token)?;
            }
            if let Metadata::Resolved {
                expected_revision,
                base_model,
            } = &target.metadata
                && (expected_revision.0 < 0 || base_model.is_some_and(|m| m.0 <= 0))
            {
                return Err(bad("invalid metadata revision/base model"));
            }
        }
        Request::Run {
            limits,
            max_items,
            max_seconds,
            ..
        } => {
            if !(1..=1_000_000).contains(&max_items.0) || !(1..=86400).contains(&max_seconds.0) {
                return Err(bad("export run requires bounded positive item/time limits"));
            }
            execution(limits.as_ref().unwrap_or(&o.execution), c).map_err(native)?;
        }
        Request::Recover {
            directories,
            limits,
        } => {
            if !(1..=1024).contains(&directories.0) {
                return Err(bad("recovery directories1..1024"));
            }
            execution(limits.as_ref().unwrap_or(&o.execution), c).map_err(native)?;
        }
        Request::RetrySeal { sequence, .. } | Request::Restore { sequence, .. }
            if sequence.0 <= 0 =>
        {
            return Err(bad("positive export item sequence required"));
        }
        _ => {}
    }
    Ok(())
}
pub(super) struct Context {
    pub control: Arc<Mutex<Control>>,
    pub cache: Arc<Mutex<Cache>>,
    pub config: Config,
}
impl Context {
    fn stopped(&self) -> bool {
        self.control.lock().unwrap().shutdown.is_canceled()
    }
    fn cancel(&self) -> Cancellation {
        self.control.lock().unwrap().cancel.clone()
    }
    fn update(&self, stage: &str, bytes: Option<u64>) {
        let mut c = self.control.lock().unwrap();
        if let Some(s) = &mut c.status {
            s.stage = stage.into();
            s.stream_bytes = bytes.map(U64);
        }
    }
    fn observe(&self, p: core::ExportCheckpoint) {
        use crate::catalog_exports::ExportCheckpoint::*;
        let (stage, bytes) = match p {
            Hashing { bytes } => ("hashing", Some(bytes)),
            Alias => ("alias", None),
            BeforeMutation => return,
            OriginalVerified => ("planning", None),
            IntentCommitted => ("intent_committed", None),
            Captured => ("captured", None),
            CaptureVerified => ("capture_verified", None),
            Linked => ("linked", None),
            Finalizing => ("finalizing", None),
            InstalledVerified => ("installed_verified", None),
        };
        self.update(stage, bytes);
        #[cfg(test)]
        if let Some(checkpoint) = &self.config.import_checkpoint {
            checkpoint(&format!("export_{stage}"), &self.cancel().0);
        }
    }
    fn authority(&self, allow_canceled: bool) -> anyhow::Result<Authority<'_>> {
        {
            let mut c = self.control.lock().unwrap();
            c.hold_since = Some(Instant::now());
        }
        loop {
            let c = self.control.lock().unwrap();
            if c.shutdown.is_canceled() || (!allow_canceled && c.cancel.is_canceled()) {
                drop(c);
                self.release();
                anyhow::bail!("export canceled before writer admission");
            }
            if c.hold_granted {
                break;
            }
            drop(c);
            thread::sleep(Duration::from_millis(5));
        }
        #[cfg(test)]
        if let Some(checkpoint) = &self.config.import_checkpoint {
            checkpoint("export_hold", &self.cancel().0);
        }
        if self.stopped() || (!allow_canceled && self.cancel().is_canceled()) {
            self.release();
            anyhow::bail!("export canceled before writer phase");
        }
        Ok(Authority(self))
    }
    fn release(&self) {
        let mut c = self.control.lock().unwrap();
        c.hold_since = None;
        c.hold_granted = false;
        if let Some(s) = &mut c.status {
            s.write_hold = false;
        }
    }
    fn permit(&self) -> anyhow::Result<NativeLaunchPermit> {
        self.update("waiting_for_previews", None);
        {
            let mut c = self.control.lock().unwrap();
            c.permit_requested = true;
            if let Some(s) = &mut c.status {
                s.phase = "waiting_for_previews".into();
            }
        }
        loop {
            let mut c = self.control.lock().unwrap();
            ensure!(
                !c.shutdown.is_canceled() && !c.cancel.is_canceled() && !c.yield_requested,
                "export canceled or yielded before native admission"
            );
            if let Some(p) = c.permit.take() {
                if let Some(s) = &mut c.status {
                    s.phase = "running".into();
                }
                return Ok(p);
            }
            drop(c);
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn finish(&self, result: anyhow::Result<ResultValue>, catalog: Option<&Catalog>) {
        let identity = self
            .control
            .lock()
            .unwrap()
            .status
            .as_ref()
            .map(|s| (s.id.clone(), s.job.as_ref().map(|j| j.id.clone())));
        // Database reads (and their I/O) never execute while the cached control
        // mutex is held. Status, cancellation and close signal independently.
        let current = identity
            .as_ref()
            .and_then(|(_, job)| job.as_ref())
            .and_then(|job| {
                #[cfg(test)]
                if let Some(checkpoint) = &self.config.import_checkpoint {
                    checkpoint("export_final_job_read", &self.cancel().0);
                }
                catalog.and_then(|catalog| read::job(catalog, job).ok())
            });
        self.release();
        let mut c = self.control.lock().unwrap();
        if c.status.as_ref().map(|s| s.id.as_str()) != identity.as_ref().map(|(id, _)| id.as_str())
        {
            return;
        }
        c.active = false;
        c.permit_requested = false;
        c.permit = None;
        let canceled = c.cancel.is_canceled();
        let yielded = c.yield_requested;
        let Some(s) = &mut c.status else { return };
        if let Some(current) = current {
            s.job = Some(current);
        }
        // A completed owned phase retains its published result even if cancel
        // arrived just after its mutation. A canceled Run keeps its job outcome.
        let winning = result.is_ok()
            && (s.kind != "run" || s.job.as_ref().is_some_and(|j| j.state == "complete"));
        s.phase = if yielded && result.is_ok() {
            "paused"
        } else if canceled && !winning {
            "canceled"
        } else if result.is_ok() {
            "complete"
        } else {
            "failed"
        }
        .into();
        s.stage = "finished".into();
        s.stream_bytes = None;
        match result {
            Ok(value) => s.result = Some(Box::new(value)),
            Err(e) => s.error = Some(format!("{e:#}").chars().take(2048).collect()),
        };
    }
}
struct Authority<'a>(&'a Context);
impl Drop for Authority<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}
fn ae(e: BridgeError) -> anyhow::Error {
    anyhow::anyhow!("{}", e.message)
}
pub(super) fn profile(ctx: &Context, p: &NativePath) -> anyhow::Result<ResultValue> {
    let path = path(p).map_err(ae)?;
    {
        let c = ctx.cache.lock().unwrap();
        ensure!(
            c.profiles.len() < 8,
            "ICC token count exceeded; release unused profile"
        );
    }
    let parent = path
        .parent()
        .context("profile parent required")?
        .canonicalize()?;
    let normalized = parent.join(path.file_name().context("profile filename required")?);
    let mut source = crate::lightroom::source::Source::open(&normalized, PROFILE_BYTES as u64)?;
    {
        let c = ctx.cache.lock().unwrap();
        let retained: u64 = c.profiles.values().map(|p| p.info.bytes.0).sum();
        ensure!(
            retained + source.before.bytes <= 2 * PROFILE_BYTES as u64,
            "ICC byte quota exceeded; release unused profile"
        );
    }
    let mut bytes = vec![0; usize::try_from(source.before.bytes)?];
    let cancel = ctx.cancel();
    for chunk in bytes.chunks_mut(64 * 1024) {
        ensure!(
            !cancel.is_canceled() && !ctx.stopped(),
            "ICC admission canceled"
        );
        source.file.read_exact(chunk)?;
    }
    let mut extra = [0];
    ensure!(source.file.read(&mut extra)? == 0, "ICC changed size");
    source.verify()?;
    ensure!(!cancel.is_canceled(), "ICC admission canceled");
    let (_, _, linear) = crate::image_export::output_profile(&OutputProfile::Icc {
        bytes: bytes.clone(),
    })?;
    let info = ProfileAdmission {
        token: uuid::Uuid::new_v4().to_string(),
        name: path.file_name().unwrap().to_string_lossy().into_owned(),
        bytes: U64(bytes.len() as u64),
        blake3: blake3::hash(&bytes).to_hex().to_string(),
        linear,
    };
    ctx.cache.lock().unwrap().profiles.insert(
        info.token.clone(),
        ProfileEntry {
            info: info.clone(),
            bytes: Arc::new(bytes),
        },
    );
    Ok(ResultValue::Profile(info))
}
fn component(label: &str) -> String {
    let text: String = label
        .chars()
        .map(|c| {
            if c.is_control() || ['/', '\\', ':', '*', '?', '"', '<', '>', '|'].contains(&c) {
                '_'
            } else {
                c
            }
        })
        .take(80)
        .collect();
    let text = text.trim_matches([' ', '.']);
    if text.is_empty() {
        "variant".into()
    } else {
        text.into()
    }
}
pub(super) fn destinations(
    ctx: &Context,
    catalog: &Catalog,
    directory: &NativePath,
    targets: Vec<TargetKey>,
    format: OutputFormat,
    naming: Naming,
) -> anyhow::Result<ResultValue> {
    let directory = path(directory).map_err(ae)?.canonicalize()?;
    ensure!(directory.is_dir(), "existing output directory required");
    let cancel = ctx.cancel();
    let mut rows = Vec::new();
    for (index, target) in targets.into_iter().enumerate() {
        ensure!(
            !cancel.is_canceled() && !ctx.stopped(),
            "destination preparation canceled"
        );
        let name = read::name(catalog, &target.key).map_err(ae)?;
        let computed = (|| -> anyhow::Result<NativePath> {
            let selected = catalog.edit_variant(&target.key)?;
            ensure!(
                selected.revision == target.expected_revision.0,
                "selected variant changed"
            );
            let encoded: Option<String> = catalog.db.query_row(
                "SELECT CASE WHEN length(CAST(native_path AS BLOB))<=32768 THEN native_path END FROM storage_bindings WHERE asset_id=?1",
                [&target.key.asset_id], |r| r.get(0))?;
            let original = path(&serde_json::from_str::<NativePath>(
                &encoded.context("original path exceeds destination byte allowance")?,
            )?)
            .map_err(ae)?;
            let stem = original
                .file_stem()
                .context("source filename unavailable")?;
            let mut filename = std::ffi::OsString::from(&naming.prefix);
            filename.push(stem);
            if naming.variant_suffix {
                filename.push(format!("-{}", component(&name.variant_label)));
            }
            filename.push(&naming.suffix);
            if let Some(start) = naming.sequence_start {
                filename.push(format!(
                    "-{}",
                    start
                        .0
                        .checked_add(index as u64)
                        .context("destination sequence overflow")?
                ));
            }
            filename.push(match format {
                OutputFormat::Jpeg { .. } => ".jpg",
                OutputFormat::Png { .. } => ".png",
                OutputFormat::Tiff { .. } => ".tif",
            });
            let result = NativePath::from_path(&directory.join(filename));
            path(&result).map_err(ae)?;
            Ok(result)
        })();
        let (destination, error) = match computed {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(format!("{e:#}").chars().take(2048).collect())),
        };
        rows.push(Destination {
            target,
            name,
            destination,
            error,
        });
        bounded(&rows, 8 * 1024 * 1024).map_err(ae)?;
    }
    let mut cache = ctx.cache.lock().unwrap();
    ensure!(
        cache.destinations.len() < 4,
        "destination result quota; release unused results"
    );
    let used: usize = cache
        .destinations
        .values()
        .map(|r| serde_json::to_vec(r).map(|b| b.len()))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .sum();
    ensure!(
        used + serde_json::to_vec(&rows)?.len() <= 8 * 1024 * 1024,
        "destination result bytes exceeded"
    );
    let token = uuid::Uuid::new_v4().to_string();
    let total = U64(rows.len() as u64);
    cache.destinations.insert(token.clone(), rows);
    Ok(ResultValue::Destinations { token, total })
}
pub(super) fn output(
    ctx: &Context,
    value: Output,
    snapshot: Option<Arc<Vec<u8>>>,
) -> anyhow::Result<OutputSpec> {
    Ok(OutputSpec {
        size: value.size,
        format: value.format,
        alpha: value.alpha,
        profile: match value.profile {
            Profile::Srgb => OutputProfile::Srgb,
            Profile::LinearSrgb => OutputProfile::LinearSrgb,
            Profile::Icc { token } => {
                let bytes = match snapshot {
                    Some(bytes) => bytes,
                    None => ctx
                        .cache
                        .lock()
                        .unwrap()
                        .profiles
                        .get(&token)
                        .context("ICC token unavailable; admit profile again")?
                        .bytes
                        .clone(),
                };
                OutputProfile::Icc {
                    bytes: (*bytes).clone(),
                }
            }
        },
    })
}
fn service<'a>(
    slot: &'a mut Option<(String, ExportService)>,
    catalog: &Catalog,
    ctx: &Context,
    limits: &ExecutionLimits,
    replace: bool,
) -> anyhow::Result<&'a mut ExportService> {
    let signature = serde_json::to_string(limits)?;
    if slot
        .as_ref()
        .is_some_and(|(current, _)| current != &signature)
    {
        ensure!(
            replace,
            "execution limits changed; explicitly recover with these limits first"
        );
        slot.take();
    }
    if slot.is_none() {
        ensure!(replace, "explicit export recovery required before Run");
        *slot = Some((
            signature,
            ExportService::open(
                catalog,
                &ctx.config.worker_executable,
                execution(limits, &ctx.config)?,
            )?,
        ));
    }
    Ok(&mut slot.as_mut().unwrap().1)
}
pub(super) struct Task {
    pub request: Request,
    pub profile_snapshot: Option<Arc<Vec<u8>>>,
    pub reply: Option<mpsc::SyncSender<super::super::Reply>>,
}
pub(super) fn run(handle: RelinkWorkerHandle, rx: mpsc::Receiver<Task>, ctx: Context) {
    let opened = handle.open();
    let mut catalog = match opened {
        Ok(c) => c,
        Err(e) => {
            // Admission publishes status before sending its task. A failed open
            // must not race that publication or strand a deferred Cancel reply.
            if let Ok(task) = rx.recv() {
                let message = format!("{e:#}");
                ctx.finish(Err(e), None);
                if let Some(tx) = task.reply {
                    let _ = tx.send(super::super::reply(Err(error(ErrorCode::Native, message))));
                }
            }
            return;
        }
    };
    let mut owner = None;
    loop {
        if ctx.stopped() {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(task) => {
                let mut result = execute(
                    &ctx,
                    &mut catalog,
                    &mut owner,
                    task.request,
                    task.profile_snapshot,
                );
                // Terminal means native ownership is drained, including Close.
                // A failed reap keeps the service and its permit for retry.
                while let Some((_, service)) = owner.as_mut() {
                    let cleanup = if ctx.stopped() || !service.is_active() {
                        ctx.update("draining", None);
                        #[cfg(test)]
                        if service.is_active()
                            && let Some(checkpoint) = &ctx.config.import_checkpoint
                        {
                            checkpoint("export_before_reap", &ctx.cancel().0);
                        }
                        service.drain_native()
                    } else {
                        ctx.authority(true)
                            .and_then(|_hold| service.yield_to_previews(&mut catalog).map(|_| ()))
                    };
                    match cleanup {
                        Ok(()) => break,
                        Err(e) => {
                            ctx.update("draining", None);
                            if let Some(s) = &mut ctx.control.lock().unwrap().status {
                                s.error = Some(
                                    format!("export drain: {e:#}").chars().take(2048).collect(),
                                );
                            }
                            result = Err(e);
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
                if ctx.stopped() {
                    let id = ctx.control.lock().unwrap().cancel_job_on_shutdown.take();
                    if let Some(id) = id
                        && read::job(&catalog, &id).is_ok_and(|j| j.state != "complete")
                    {
                        result = catalog
                            .cancel_photo_export_job(&id)
                            .map(|job| ResultValue::Job(job.into()));
                    }
                }
                let reply = task.reply.map(|tx| {
                    let value = match &result {
                        Ok(ResultValue::Job(job)) => Ok(super::super::Response::Export(Box::new(
                            Response::Job(job.clone()),
                        ))),
                        Ok(_) => Err(error(
                            ErrorCode::Native,
                            "unexpected inactive cancel result",
                        )),
                        Err(e) => Err(error(
                            if ctx.cancel().is_canceled() {
                                ErrorCode::Canceled
                            } else {
                                ErrorCode::Native
                            },
                            format!("{e:#}"),
                        )),
                    };
                    (tx, super::super::reply(value))
                });
                ctx.finish(result, Some(&catalog));
                if let Some((tx, reply)) = reply {
                    let _ = tx.send(reply);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    if let Some((_, service)) = &mut owner {
        let _ = service.yield_to_previews(&mut catalog);
    }
    {
        let id = ctx.control.lock().unwrap().cancel_job_on_shutdown.clone();
        if let Some(id) = id
            && read::job(&catalog, &id).is_ok_and(|j| j.state != "complete")
        {
            let _ = catalog.cancel_photo_export_job(&id);
        }
    }
    drop(owner);
}
fn execute(
    ctx: &Context,
    catalog: &mut Catalog,
    owner: &mut Option<(String, ExportService)>,
    request: Request,
    profile_snapshot: Option<Arc<Vec<u8>>>,
) -> anyhow::Result<ResultValue> {
    let default = options(&ctx.config);
    let cancel = ctx.cancel();
    let mut observer = |p| ctx.observe(p);
    let mut control = core::ExportControl::observed(&cancel.0, &mut observer);
    match request {
        Request::Cancel {
            job: Some(job),
            operation: None,
        } => {
            let _hold = ctx.authority(false)?;
            let signal = cancel.clone();
            catalog
                .db
                .progress_handler(1000, Some(move || signal.is_canceled()))?;
            let result = catalog.cancel_photo_export_job(&job);
            catalog.db.progress_handler(0, None::<fn() -> bool>)?;
            result?;
            Ok(ResultValue::Job(read::job(catalog, &job).map_err(ae)?))
        }
        Request::Profile { path } => profile(ctx, &path),
        Request::Destinations {
            directory,
            targets,
            format,
            naming,
        } => destinations(ctx, catalog, &directory, targets, format, naming),
        Request::Paths { limit } => {
            let _hold = ctx.authority(false)?;
            let p = catalog.reconcile_export_paths(limit.0 as usize)?;
            Ok(ResultValue::Paths(Paths {
                projected: U64(p.projected as u64),
                pending: p.pending,
                unbound: U64(p.unbound),
            }))
        }
        Request::Append {
            job,
            expected_total,
            target,
            output: value,
            budgets,
        } => {
            let output = output(ctx, value, profile_snapshot)?;
            let b = budgets.unwrap_or(default.budgets);
            let target = core::ExportTarget {
                key: target.key,
                expected_revision: target.expected_revision.0,
                destination: path(&target.destination).map_err(ae)?,
                overwrite: target.overwrite,
                metadata: match target.metadata {
                    Metadata::Omit => core::MetadataSelection::Omit,
                    Metadata::Resolved {
                        expected_revision,
                        base_model,
                    } => core::MetadataSelection::Resolved {
                        expected_revision: expected_revision.0,
                        base_model: base_model.map(|v| v.0),
                    },
                },
            };
            ctx.update("planning", None);
            let _hold = ctx.authority(false)?;
            let item = catalog.append_photo_export_cancellable(
                &job,
                expected_total.0,
                &target,
                &output,
                b.max_original_bytes.0,
                b.max_payload_bytes.0,
                crate::catalog_export_alias::AliasLimits {
                    directories: b.alias_limits.directories.0 as usize,
                    candidates: b.alias_limits.candidates.0 as usize,
                },
                &mut control,
            )?;
            Ok(ResultValue::Appended {
                job: read::job(catalog, &job).map_err(ae)?,
                item: Box::new(
                    read::item(catalog, &job, item.sequence, ctx.config.limits.page_bytes)
                        .map_err(ae)?,
                ),
            })
        }
        Request::Recover {
            directories,
            limits,
        } => {
            let limits = limits.unwrap_or(default.execution);
            let _hold = ctx.authority(false)?;
            ctx.update("recovering", None);
            let s = service(owner, catalog, ctx, &limits, true)?;
            let result = s.recover_cancellable(catalog, directories.0 as usize, &mut control)?;
            Ok(ResultValue::Recovery(Recovery {
                fenced: U64(result.fenced as u64),
                complete: result.complete,
            }))
        }
        Request::RetrySeal {
            job,
            sequence,
            authority,
        } => {
            let _hold = ctx.authority(false)?;
            ensure!(
                read::document(catalog, &job, sequence.0).map_err(ae)?.1 == authority,
                "reviewed export authority changed"
            );
            catalog.retry_sealed_photo_export(&job, sequence.0)?;
            Ok(ResultValue::Job(read::job(catalog, &job).map_err(ae)?))
        }
        Request::Restore {
            job,
            sequence,
            authority,
        } => {
            let _hold = ctx.authority(false)?;
            ensure!(
                read::document(catalog, &job, sequence.0).map_err(ae)?.1 == authority,
                "reviewed export authority changed"
            );
            ctx.update("restoring", None);
            let receipt =
                catalog.restore_photo_export_item_cancellable(&job, sequence.0, &mut control)?;
            Ok(ResultValue::Receipt {
                job: read::job(catalog, &job).map_err(ae)?,
                sequence,
                authority,
                receipt: receipt.into(),
            })
        }
        Request::Run {
            job,
            limits,
            max_items,
            max_seconds,
        } => {
            let limits = limits.unwrap_or(default.execution);
            let service = service(owner, catalog, ctx, &limits, false)?;
            let started = Instant::now();
            let mut processed = 0;
            loop {
                if ctx.stopped() {
                    break;
                }
                let (yielded, foreground) = {
                    let c = ctx.control.lock().unwrap();
                    (c.yield_requested, c.foreground_yield)
                };
                if cancel.is_canceled() || yielded || foreground {
                    let _hold = ctx.authority(true)?;
                    ctx.update("yielding", None);
                    service.yield_to_previews(catalog)?;
                    if cancel.is_canceled() {
                        if read::job(catalog, &job).map_err(ae)?.state != "complete" {
                            catalog.cancel_photo_export_job(&job)?;
                        }
                        break;
                    }
                    if yielded {
                        break;
                    }
                    {
                        let mut c = ctx.control.lock().unwrap();
                        c.foreground_yield = false;
                    }
                }
                if processed >= max_items.0 || started.elapsed().as_secs() >= max_seconds.0 {
                    let _hold = ctx.authority(true)?;
                    service.yield_to_previews(catalog)?;
                    break;
                }
                let event = {
                    let _hold = ctx.authority(true)?;
                    service.tick_detached(catalog, &job, &mut control)?
                };
                match event {
                    ExportEvent::WaitingForPreviews => match ctx.permit() {
                        Ok(permit) => service.admit_native(catalog, permit)?,
                        Err(e) => {
                            if ctx.stopped() {
                                return Err(e);
                            }
                            let _hold = ctx.authority(true)?;
                            service.yield_to_previews(catalog)?;
                            if cancel.is_canceled()
                                && read::job(catalog, &job).map_err(ae)?.state != "complete"
                            {
                                catalog.cancel_photo_export_job(&job)?;
                            }
                            break;
                        }
                    },
                    ExportEvent::Started { sequence, pid } => {
                        ctx.update("rendering", None);
                        if let Some(s) = &mut ctx.control.lock().unwrap().status {
                            s.sequence = Some(I64(sequence));
                        }
                        #[cfg(test)]
                        if let Some(checkpoint) = &ctx.config.import_checkpoint {
                            checkpoint(&format!("export_native_started:{pid}"), &ctx.cancel().0);
                        }
                        #[cfg(not(test))]
                        let _ = pid;
                    }
                    ExportEvent::Rendering { sequence } => {
                        ctx.update("rendering", None);
                        if let Some(s) = &mut ctx.control.lock().unwrap().status {
                            s.sequence = Some(I64(sequence));
                        }
                    }
                    ExportEvent::Published { sequence, .. }
                    | ExportEvent::Failed { sequence, .. } => {
                        processed += 1;
                        let current = read::job(catalog, &job).map_err(ae)?;
                        if let Some(s) = &mut ctx.control.lock().unwrap().status {
                            s.sequence = Some(I64(sequence));
                            s.processed = U64(processed);
                            s.job = Some(current);
                        }
                    }
                    ExportEvent::Idle => break,
                    ExportEvent::Yielded { .. } => {}
                }
                thread::sleep(Duration::from_millis(20));
            }
            Ok(ResultValue::Job(read::job(catalog, &job).map_err(ae)?))
        }
        _ => anyhow::bail!("unsupported long export request"),
    }
}
