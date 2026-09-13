use super::*;
use crate::lightroom::{
    self as core,
    capture::CaptureProcess,
    plan::{Plan, desktop::InspectionPin},
};
use std::time::Duration;

// Rust drops these fields in declaration order: all SQLite/source handles
// disappear before the long-lived same-inode identity descriptor closes.
struct Owner {
    plan: Option<Plan>,
    review: Option<selection::SelectionReview>,
    pin: InspectionPin,
    version: i64,
    config: Config,
}
fn encode(value: &impl Serialize, limit: usize) -> Result<String> {
    Ok(String::from_utf8(core::bounded_json(value, limit)?)?)
}
fn count(value: U64, maximum: usize) -> Result<usize> {
    let n = usize::try_from(value.0)?;
    ensure!((1..=maximum).contains(&n), "workbench row/chunk admission");
    Ok(n)
}
fn error_text(error: &anyhow::Error) -> String {
    format!("{error:#}").chars().take(4096).collect()
}
fn finish(
    shared: &Arc<Mutex<Shared>>,
    operation: &str,
    generation: &str,
    result: Result<String>,
    review: Option<String>,
    closing: &AtomicBool,
) {
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    if s.status.operation != operation {
        return;
    }
    s.status.review_token = review;
    s.status.capture_pid = None;
    match result {
        Ok(json) => {
            let result_token = token();
            s.status.result_token = Some(result_token.clone());
            s.status.result_bytes = U64(json.len() as u64);
            s.result = Some(Arc::new(Cached {
                generation: generation.into(),
                operation: operation.into(),
                token: result_token,
                json,
            }));
            s.status.phase = if closing.load(Ordering::Acquire) {
                Phase::Closing
            } else {
                Phase::Complete
            };
        }
        Err(error) => {
            s.status.error = Some(error_text(&error));
            s.status.result_token = None;
            s.status.result_bytes = U64(0);
            s.result = None;
            s.status.phase = if closing.load(Ordering::Acquire) {
                Phase::Closing
            } else if s.control.cancel.load(Ordering::Acquire) {
                Phase::Canceled
            } else {
                Phase::Failed
            };
        }
    }
}
pub(super) fn run(
    config: Config,
    receiver: mpsc::Receiver<Message>,
    shared: Arc<Mutex<Shared>>,
    closing: Arc<AtomicBool>,
) {
    let (initial, initial_generation, control) = {
        let s = shared.lock().unwrap_or_else(|e| e.into_inner());
        (
            s.status.operation.clone(),
            s.status.generation.clone(),
            s.control.clone(),
        )
    };
    let opened = (|| -> Result<Owner> {
        control.check()?;
        let root = native(&config.root, config.limits.native_path_units)?;
        if matches!(config.mode, OpenMode::Create) {
            drop(Plan::create(&root)?);
        }
        control.check()?;
        let pin = InspectionPin::open(&root)?;
        let plan = pin.open_plan(control.clone())?;
        let version = plan.data_version()?;
        Ok(Owner {
            plan: Some(plan),
            review: None,
            pin,
            version,
            config,
        })
    })();
    let mut owner = match opened {
        Ok(owner) => {
            let root = NativePath::from_path(owner.pin.root());
            {
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                s.status.initialized = true;
                s.status.root = root.clone();
            }
            finish(
                &shared,
                &initial,
                &initial_generation,
                encode(
                    &serde_json::json!({"root":root,"schema":core::plan::PLAN_SCHEMA_VERSION}),
                    owner.config.limits.result_bytes,
                ),
                None,
                &closing,
            );
            owner
        }
        Err(error) => {
            finish(
                &shared,
                &initial,
                &initial_generation,
                Err(error),
                None,
                &closing,
            );
            return;
        }
    };
    while !closing.load(Ordering::Acquire) {
        let Ok(message) = receiver.recv() else {
            break;
        };
        if closing.load(Ordering::Acquire) {
            break;
        }
        let (operation, generation, control, request) = match message {
            Message::Action {
                operation,
                generation,
                action,
                control,
            } => (operation, generation, control, Ok(action)),
            Message::Read {
                operation,
                generation,
                query,
                control,
            } => (operation, generation, control, Err(query)),
            Message::Close => break,
        };
        // submit holds this same mutex until it publishes the new identity;
        // worker results therefore cannot overtake caller admission state.
        {
            let s = shared.lock().unwrap_or_else(|e| e.into_inner());
            if s.status.operation != operation {
                continue;
            }
        }
        let result = (|| -> Result<String> {
            control.check()?;
            owner.pin.verify()?;
            if let Some(plan) = &mut owner.plan {
                plan.set_execution(Some(control.clone()));
                owner.pin.verify_plan(plan)?;
                let observed = plan.data_version()?;
                if observed != owner.version {
                    owner.version = observed;
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.status.generation = token();
                    s.result = None;
                    anyhow::bail!(
                        "inspection changed externally; generation and cursors invalidated, request a fresh page"
                    );
                }
            }
            match request {
                Ok(action) => owner.action(action.decode(&control)?, &control, &shared),
                Err(query) => owner.query(query, &control),
            }
        })();
        let review = owner.review.as_ref().map(|r| r.summary().token.clone());
        finish(&shared, &operation, &generation, result, review, &closing);
    }
    // Owner drops Plan/Review first, then identity pin. Any capture subprocess
    // has already dropped/reaped inside action before this point.
}
impl Owner {
    fn plan(&mut self, control: &Control) -> Result<&mut Plan> {
        ensure!(
            self.review.is_none(),
            "ReleaseReview required before opening inspection writer"
        );
        if self.plan.is_none() {
            let plan = self.pin.open_plan(control.clone())?;
            self.version = plan.data_version()?;
            self.plan = Some(plan);
        }
        let plan = self.plan.as_mut().context("inspection owner unavailable")?;
        plan.set_execution(Some(control.clone()));
        self.pin.verify_plan(plan)?;
        Ok(plan)
    }
    fn action(
        &mut self,
        action: Action,
        control: &Control,
        shared: &Arc<Mutex<Shared>>,
    ) -> Result<String> {
        let limit = self.config.limits.result_bytes;
        let path_limit = self.config.limits.native_path_units;
        match action {
            Action::Discover { root, limits } => {
                let root = native(&root, path_limit)?;
                let inventory = core::discovery::discover_controlled(
                    &root,
                    &limits,
                    Some((limit, path_limit)),
                    || control.check(),
                )?;
                encode(&inventory, limit)
            }
            Action::Capture { request } => {
                native(&request.source, path_limit)?;
                native(&request.output, path_limit)?;
                let executable = native(&self.config.capture_executable, path_limit)?;
                let staging = native(&self.config.capture_staging, path_limit)?;
                let mut child = CaptureProcess::spawn(&executable, &staging, &request)?;
                {
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.status.capture_pid = Some(child.pid());
                    s.status.capture_staging =
                        Some(NativePath::from_path(child.staging_directory()));
                }
                loop {
                    if let Err(error) = control.check() {
                        child.cancel_and_wait()?;
                        return Err(error);
                    }
                    if let Some(manifest) = child.poll()? {
                        return encode(&manifest, limit);
                    }
                    // Bounded polling lives only on this worker. Status and
                    // cancel use cached/atomic state and never wait here.
                    thread::sleep(Duration::from_millis(10));
                }
            }
            Action::RegisterInventory { inventory } => {
                native_units(&inventory.root, path_limit)?;
                for candidate in &inventory.candidates {
                    native_units(&candidate.path, path_limit)?;
                }
                for excluded in &inventory.exclusions {
                    native_units(excluded, path_limit)?;
                }
                let digest = self.plan(control)?.register_inventory(&inventory)?;
                encode(&serde_json::json!({"inventory_digest":digest}), limit)
            }
            Action::AddCapture { directory } => {
                let directory = native(&directory, path_limit)?;
                let revision = self.plan(control)?.add_capture(&directory)?;
                encode(&serde_json::json!({"revision":revision}), limit)
            }
            Action::Resume { revision, max_rows } => {
                let rows = count(max_rows, 100_000)?;
                let progress = self.plan(control)?.resume(&revision, rows)?;
                encode(&progress, limit)
            }
            Action::InspectOriginals {
                revision,
                limit: rows,
                inspection,
            } => {
                let rows = count(rows, 1000)?;
                let packets = matches!(inspection, OriginalInspection::Packets);
                let processed = self.plan(control)?.check_paths(&revision, rows, packets)?;
                control.progress(processed as u64);
                encode(
                    &serde_json::json!({"revision":revision,"processed":U64(processed as u64),"inspection":inspection}),
                    limit,
                )
            }
            Action::AssignFamily {
                revision,
                family,
                reason,
            } => {
                self.plan(control)?
                    .assign_family(&revision, &family, &reason)?;
                encode(
                    &serde_json::json!({"revision":revision,"family":family}),
                    limit,
                )
            }
            Action::Choose {
                family,
                revision,
                expected_evidence,
                reason,
            } => {
                let plan = self.plan(control)?;
                plan.desktop_choose(&family, &revision, &expected_evidence, &reason)?;
                encode(
                    &serde_json::json!({"family":family,"revision":revision,"evidence":expected_evidence}),
                    limit,
                )
            }
            Action::PrepareSelection { request, limits } => {
                ensure!(
                    self.review.is_none(),
                    "ReleaseReview required before replacing live review"
                );
                let requested = native(&request.inspection, path_limit)?;
                ensure!(
                    requested == self.pin.root().join("inspection.sqlite3"),
                    "selection inspection differs from pinned workbench"
                );
                drop(self.plan.take());
                let progress = control.processed.clone();
                let review = selection::SelectionReview::open(
                    request,
                    limits,
                    control.cancel.clone(),
                    move |p| {
                        progress.store(p.completed, Ordering::Release);
                    },
                )?;
                self.pin.verify_review(&review)?;
                let encoded = encode(review.summary(), limit)?;
                self.review = Some(review);
                Ok(encoded)
            }
            Action::Seal {
                review_token,
                approval_blake3,
                approval_json,
                output,
            } => {
                native(&output, path_limit)?;
                // Refuse an insufficient response budget BEFORE immutable
                // publication, so completion cannot become a serialization
                // failure afterward. Approval appears escaped plus in its seal
                // subset; bounded roster/path overhead is reserved explicitly.
                let path_bytes =
                    core::bounded_json(&output, self.config.limits.request_bytes)?.len();
                let response_bound = approval_json
                    .len()
                    .checked_mul(8)
                    .and_then(|n| n.checked_add(6 * (path_bytes + 4096)))
                    .and_then(|n| n.checked_add(1024 * 1024))
                    .context("seal response admission overflow")?;
                ensure!(
                    response_bound <= limit,
                    "seal response requires a larger result_bytes budget before publication"
                );
                let review = self
                    .review
                    .as_mut()
                    .context("prepare an explicit selection review first")?;
                let progress = control.processed.clone();
                let result = review.seal(
                    &review_token,
                    &approval_blake3,
                    approval_json.as_bytes(),
                    output,
                    control.cancel.clone(),
                    move |p| {
                        progress.store(p.completed, Ordering::Release);
                    },
                )?;
                // The immutable approval bytes remain exact strings; no integer
                // or NativePath reserialization changes approval authority.
                encode(
                    &serde_json::json!({"seal":result.seal,"approval_json":String::from_utf8(result.approval_bytes)?,"approval":result.approval,"directory":result.directory,"seal_path":result.seal_path,"approval_path":result.approval_path}),
                    limit,
                )
            }
            Action::ReleaseReview => {
                drop(self.review.take());
                self.plan(control)?;
                encode(&serde_json::json!({"review_released":true}), limit)
            }
        }
    }
    fn query(&mut self, query: Query, control: &Control) -> Result<String> {
        let maximum = self.config.limits.result_bytes;
        match query {
            Query::CaptureManifest { directory } => {
                let directory = native(&directory, self.config.limits.native_path_units)?;
                encode(&core::capture::read_manifest(&directory)?, maximum)
            }
            Query::SelectionSummary => encode(
                self.review
                    .as_ref()
                    .context("no live selection review")?
                    .summary(),
                maximum,
            ),
            Query::SelectionPage {
                review_token,
                collection,
                after,
                limit,
            } => {
                let review = self.review.as_ref().context("no live selection review")?;
                ensure!(
                    review.summary().token == review_token,
                    "selection page review token differs"
                );
                encode(
                    &review.page(
                        collection.into(),
                        usize::try_from(after.0)?,
                        count(limit, 256)?,
                    )?,
                    maximum,
                )
            }
            Query::Rows {
                revision,
                table,
                after,
                limit,
            } => {
                let rows = self.plan(control)?.rows(
                    &revision,
                    table.as_deref(),
                    after.0,
                    count(limit, 1000)?,
                )?;
                encode(
                    &serde_json::json!({"next":rows.last().map(|r|I64(r.sequence)),"exhausted":rows.is_empty(),"rows":rows}),
                    maximum,
                )
            }
            Query::Report { revision } => {
                encode(&self.plan(control)?.desktop_report(&revision)?, maximum)
            }
            Query::Families => encode(&self.plan(control)?.desktop_families()?, maximum),
            Query::Paths {
                revision,
                after,
                limit,
            } => sequence_page(
                self.plan(control)?
                    .paths(&revision, after.0, count(limit, 1000)?)?,
                maximum,
            ),
            Query::Issues {
                revision,
                after,
                limit,
            } => sequence_page(
                self.plan(control)?
                    .issues(&revision, after.0, count(limit, 1000)?)?,
                maximum,
            ),
            Query::Packets {
                revision,
                after,
                limit,
            } => sequence_page(
                self.plan(control)?
                    .packets(&revision, after.0, count(limit, 1000)?)?,
                maximum,
            ),
            Query::PacketBytes {
                revision,
                sequence,
                decoded,
                offset,
                limit,
            } => encode(
                &self.plan(control)?.packet_bytes(
                    &revision,
                    sequence.0,
                    decoded,
                    offset.0,
                    count(limit, 1024 * 1024)?,
                )?,
                maximum,
            ),
            Query::MetadataConflicts {
                revision,
                after,
                limit,
            } => sequence_page(
                self.plan(control)?
                    .metadata_conflicts(&revision, after.0, count(limit, 1000)?)?,
                maximum,
            ),
            Query::GlobalIdConflicts {
                left,
                right,
                after_left,
                after_right,
                limit,
            } => {
                let rows = self.plan(control)?.global_id_conflicts(
                    &left,
                    &right,
                    &after_left,
                    &after_right,
                    count(limit, 1000)?,
                )?;
                let next=rows.last().map(|r|serde_json::json!({"left":r["left_source_id"],"right":r["right_source_id"]}));
                encode(
                    &serde_json::json!({"next":next,"exhausted":rows.is_empty(),"rows":rows}),
                    maximum,
                )
            }
            Query::PathCollisions {
                left,
                right,
                after_left,
                after_right,
                limit,
            } => {
                let rows = self.plan(control)?.path_collisions(
                    &left,
                    &right,
                    after_left.0,
                    after_right.0,
                    count(limit, 1000)?,
                )?;
                let next = rows
                    .last()
                    .map(|r| {
                        Ok::<_, anyhow::Error>((
                            I64(r["left_sequence"]
                                .as_i64()
                                .context("left collision cursor")?),
                            I64(r["right_sequence"]
                                .as_i64()
                                .context("right collision cursor")?),
                        ))
                    })
                    .transpose()?;
                encode(
                    &serde_json::json!({"next":next,"exhausted":rows.is_empty(),"rows":rows}),
                    maximum,
                )
            }
        }
    }
}
fn sequence_page(rows: Vec<serde_json::Value>, maximum: usize) -> Result<String> {
    let next = rows
        .last()
        .map(|r| {
            r["sequence"]
                .as_i64()
                .map(I64)
                .context("inspection sequence cursor missing")
        })
        .transpose()?;
    encode(
        &serde_json::json!({"next":next,"exhausted":rows.is_empty(),"rows":rows}),
        maximum,
    )
}
