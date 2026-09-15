use super::*;
use crate::lightroom::{
    self as core,
    capture::CaptureProcess,
    plan::{Plan, desktop::InspectionPin},
};
use crate::{
    filesystem_worker::wire::{
        LightroomWorkbenchIo, LightroomWorkbenchIoReply, LightroomWorkbenchSealDocument,
        LightroomWorkbenchSealState,
    },
    lightroom_migration_worker::{
        identity::FileKey,
        source_reader::capture_wire::{self, TableValue},
    },
};
use std::time::Duration;

// Rust drops these fields in declaration order: all SQLite/source handles
// disappear before the long-lived same-inode identity descriptor closes.
struct Owner {
    plan: Option<Plan>,
    review: Option<selection::SelectionReview>,
    pin: Option<InspectionPin>,
    managed: Option<Arc<dyn ManagedIo>>,
    root_operation: String,
    workbench: String,
    generation: String,
    root: NativePath,
    root_identity: Option<FileKey>,
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
    managed: Option<Arc<dyn ManagedIo>>,
) -> Result<()> {
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
        let (root, pin, root_identity, plan) = if let Some(io) = &managed {
            let opening_workbench = shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .status
                .workbench
                .clone();
            let request = LightroomWorkbenchIo::RootBegin {
                operation: initial.clone(),
                workbench: opening_workbench.clone(),
                generation: initial_generation.clone(),
                root: config.root.clone(),
                create: matches!(config.mode, OpenMode::Create),
            };
            let reply = io.filesystem(request.clone(), &control.cancel)?;
            reply.validate_for(&request)?;
            let LightroomWorkbenchIoReply::Root { root, physical, .. } = reply else {
                anyhow::bail!("Workbench root reply kind")
            };
            let setup = (|| -> Result<Plan> {
                let local = native(&root, config.limits.native_path_units)?;
                let mut plan = if matches!(config.mode, OpenMode::Create) {
                    Plan::create_managed(&local, &physical)?
                } else {
                    Plan::open_managed(&local, &physical)?
                };
                plan.set_execution(Some(control.clone()));
                Ok(plan)
            })();
            let plan = match setup {
                Ok(plan) => plan,
                Err(primary) => {
                    let release = LightroomWorkbenchIo::RootRelease {
                        operation: initial.clone(),
                        workbench: opening_workbench,
                        generation: initial_generation.clone(),
                    };
                    return match io.filesystem(release.clone(), &AtomicBool::new(false)) {
                        Ok(reply) => match reply.validate_for(&release) {
                            Ok(())
                                if matches!(reply, LightroomWorkbenchIoReply::Released { .. }) =>
                            {
                                Err(primary)
                            }
                            Ok(()) => {
                                Err(primary
                                    .context("managed root startup cleanup reply kind differs"))
                            }
                            Err(cleanup) => Err(primary.context(format!(
                                "managed root startup cleanup receipt invalid: {cleanup:#}"
                            ))),
                        },
                        Err(cleanup) => Err(primary.context(format!(
                            "managed root startup cleanup also failed: {cleanup:#}"
                        ))),
                    };
                }
            };
            (root, None, Some(physical), plan)
        } else {
            let root = native(&config.root, config.limits.native_path_units)?;
            if matches!(config.mode, OpenMode::Create) {
                drop(Plan::create(&root)?);
            }
            control.check()?;
            let pin = InspectionPin::open(&root)?;
            let plan = pin.open_plan(control.clone())?;
            (NativePath::from_path(pin.root()), Some(pin), None, plan)
        };
        let version = plan.data_version()?;
        let workbench = shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .workbench
            .clone();
        Ok(Owner {
            plan: Some(plan),
            review: None,
            pin,
            managed,
            root_operation: initial.clone(),
            workbench,
            generation: initial_generation.clone(),
            root,
            root_identity,
            version,
            config,
        })
    })();
    let mut owner = match opened {
        Ok(owner) => {
            let root = owner.root.clone();
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
            let detail = error_text(&error);
            finish(
                &shared,
                &initial,
                &initial_generation,
                Err(anyhow::anyhow!(detail.clone())),
                None,
                &closing,
            );
            return Err(anyhow::anyhow!(detail));
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
            owner.verify_root(&control)?;
            if let Some(plan) = &mut owner.plan {
                plan.set_execution(Some(control.clone()));
                if let Some(pin) = &owner.pin {
                    pin.verify_plan(plan)?;
                } else if let Some(identity) = &owner.root_identity {
                    plan.verify_managed_identity(identity)?;
                }
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
                Err(query) => owner.query(query, &control, &operation),
            }
        })();
        let review = owner.review.as_ref().map(|r| r.summary().token.clone());
        finish(&shared, &operation, &generation, result, review, &closing);
    }
    owner.close()
    // Owner drops Plan/Review first, then identity pin. Any capture subprocess
    // has already dropped/reaped inside action before this point.
}
impl Owner {
    fn current_operation(&self, shared: &Arc<Mutex<Shared>>) -> String {
        shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .operation
            .clone()
    }
    fn capture_limits(&self) -> capture_wire::Limits {
        capture_wire::Limits {
            open_deadline_ms: U64(self.config.limits.deadline_ms.clamp(1, 120_000)),
            total_deadline_ms: U64(self.config.limits.deadline_ms.max(1)),
            vm_steps: U64(self.config.limits.vm_steps.max(1)),
            schema_objects: U64(capture_wire::MAX_SCHEMA_OBJECTS as u64),
            schema_bytes: U64(core::PAGE_BYTES as u64),
            page_bytes: U64(self.config.limits.page_bytes.min(core::PAGE_BYTES) as u64),
            max_cell_bytes: U64(self.config.limits.row_bytes.min(64 * 1024 * 1024) as u64),
            result_bytes: U64(self.config.limits.result_bytes.min(
                crate::lightroom_migration_worker::source_reader::capture_wire::MAX_RESULT_BYTES,
            ) as u64),
            inline_bytes: U64(self.config.limits.page_bytes.min(core::PAGE_BYTES) as u64),
            chunk_bytes: U64(
                crate::lightroom_migration_worker::source_reader::capture_wire::MAX_CHUNK_BYTES
                    as u64,
            ),
            max_rows: U64(capture_wire::MAX_ROWS as u64),
        }
    }
    fn begin_evidence(
        &self,
        operation: &str,
        directory: NativePath,
        control: &Control,
    ) -> Result<(
        String,
        NativePath,
        core::capture::Manifest,
        crate::lightroom_migration_worker::source_reader::CaptureSqlAuthority,
    )> {
        let io = self
            .managed
            .as_ref()
            .context("managed filesystem owner absent")?;
        let capture_generation = token();
        let mut protected = Vec::new();
        if let Some(identity) = &self.root_identity {
            protected.push(identity.clone());
        }
        let request = LightroomWorkbenchIo::EvidenceBegin {
            operation: operation.into(),
            workbench: self.workbench.clone(),
            generation: self.generation.clone(),
            capture_generation: capture_generation.clone(),
            directory,
            source_generation: token(),
            protected,
            limits: self.capture_limits(),
        };
        let reply = io.filesystem(request.clone(), &control.cancel)?;
        reply.validate_for(&request)?;
        let LightroomWorkbenchIoReply::Evidence {
            capture_generation: actual,
            directory,
            manifest,
            authority,
            ..
        } = reply
        else {
            anyhow::bail!("capture evidence reply kind")
        };
        ensure!(
            actual == capture_generation,
            "capture evidence generation differs"
        );
        Ok((capture_generation, directory, manifest, authority))
    }
    fn evidence_current(
        &self,
        operation: &str,
        capture_generation: &str,
        control: &Control,
    ) -> Result<()> {
        let io = self.managed.as_ref().context("managed owner absent")?;
        let request = LightroomWorkbenchIo::EvidenceCurrent {
            operation: operation.into(),
            workbench: self.workbench.clone(),
            generation: self.generation.clone(),
            capture_generation: capture_generation.into(),
        };
        let reply = io.filesystem(request.clone(), &control.cancel)?;
        reply.validate_for(&request)?;
        ensure!(
            matches!(reply, LightroomWorkbenchIoReply::Evidence { .. }),
            "capture evidence current reply kind"
        );
        Ok(())
    }
    fn release_evidence(&self, operation: &str, capture_generation: &str) -> Result<()> {
        let io = self.managed.as_ref().context("managed owner absent")?;
        let request = LightroomWorkbenchIo::EvidenceRelease {
            operation: operation.into(),
            workbench: self.workbench.clone(),
            generation: self.generation.clone(),
            capture_generation: capture_generation.into(),
        };
        let reply = io.filesystem(request.clone(), &AtomicBool::new(false))?;
        reply.validate_for(&request)?;
        ensure!(
            matches!(reply, LightroomWorkbenchIoReply::Released { .. }),
            "capture evidence release reply kind"
        );
        Ok(())
    }
    fn seal_upload(
        &self,
        operation: &str,
        token: &str,
        document: LightroomWorkbenchSealDocument,
        bytes: &[u8],
        control: &Control,
    ) -> Result<()> {
        let io = self.managed.as_ref().context("managed owner absent")?;
        let mut offset = 0usize;
        while offset < bytes.len() {
            control.check()?;
            let end = offset
                .checked_add(crate::filesystem_worker::wire::CHUNK_BYTES)
                .context("seal upload offset overflow")?
                .min(bytes.len());
            let request = LightroomWorkbenchIo::SealChunk {
                operation: operation.into(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
                token: token.into(),
                document,
                offset: U64(offset as u64),
                bytes: bytes[offset..end].to_vec(),
            };
            let reply = io.filesystem(request.clone(), &control.cancel)?;
            reply.validate_for(&request)?;
            offset = end;
        }
        Ok(())
    }
    fn seal_managed(
        &mut self,
        operation: &str,
        review_token: &str,
        approval_blake3: &str,
        approval_json: &str,
        output: NativePath,
        control: &Control,
    ) -> Result<selection::SealedSelection> {
        let io = self
            .managed
            .as_ref()
            .context("managed owner absent")?
            .clone();
        let mut review = self
            .review
            .take()
            .context("prepare an explicit selection review first")?;
        let mut retained_token = None;
        let mut publication_started = false;
        let result = (|| -> Result<selection::SealedSelection> {
            let preparation = review.prepare_managed_seal(
                review_token,
                approval_blake3,
                approval_json.as_bytes(),
                output,
                control.cancel.clone(),
            )?;
            let seal_token = token();
            retained_token = Some(seal_token.clone());
            let request = LightroomWorkbenchIo::SealBegin {
                operation: operation.into(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
                token: seal_token.clone(),
                output: preparation.output.clone(),
                approval_bytes: U64(preparation.approval_bytes.len() as u64),
                approval_blake3: core::digest(&preparation.approval_bytes),
                review_bytes: U64(preparation.review_bytes.len() as u64),
                review_blake3: core::digest(&preparation.review_bytes),
            };
            let reply = io.filesystem(request.clone(), &control.cancel)?;
            reply.validate_for(&request)?;
            self.seal_upload(
                operation,
                &seal_token,
                LightroomWorkbenchSealDocument::Approval,
                &preparation.approval_bytes,
                control,
            )?;
            self.seal_upload(
                operation,
                &seal_token,
                LightroomWorkbenchSealDocument::Review,
                &preparation.review_bytes,
                control,
            )?;
            let request = LightroomWorkbenchIo::SealStage {
                operation: operation.into(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
                token: seal_token.clone(),
            };
            let reply = io.filesystem(request.clone(), &control.cancel)?;
            reply.validate_for(&request)?;
            let LightroomWorkbenchIoReply::SealStaged {
                directory,
                database,
                physical,
                ..
            } = reply
            else {
                anyhow::bail!("seal stage reply kind")
            };
            let progress = control.processed.clone();
            review.backup_managed(
                review_token,
                &database,
                &physical,
                control.cancel.clone(),
                move |value| progress.store(value.completed, Ordering::Release),
            )?;
            let request = LightroomWorkbenchIo::SealSyncHash {
                operation: operation.into(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
                token: seal_token.clone(),
                maximum_bytes: U64(preparation.snapshot_bytes),
            };
            let reply = io.filesystem(request.clone(), &control.cancel)?;
            reply.validate_for(&request)?;
            let LightroomWorkbenchIoReply::SealHashed {
                identity,
                blake3,
                database,
                ..
            } = reply
            else {
                anyhow::bail!("seal hash reply kind")
            };
            let seal = review.managed_input_seal(&preparation, database, identity, blake3)?;
            let remaining = self.config.limits.deadline_ms.clamp(1, 120_000);
            let limits = core::migration_source::ReadLimits {
                open_deadline_ms: self.config.limits.deadline_ms.max(1),
                deadline_ms: remaining,
                vm_steps: self.config.limits.vm_steps.min(1_000_000_000),
                ..Default::default()
            };
            let mut protected = vec![physical];
            if let Some(root) = &self.root_identity {
                protected.push(root.clone());
            }
            let source =
                io.source_sql_open(seal.clone(), limits, protected, control.cancel.clone())?;
            io.source_retire(&source)
                .context("SQL13 source did not drain")?;
            review.current_managed_seal(review_token)?;
            self.verify_root(control)?;
            let seal_bytes = core::bounded_json(&seal, core::MANIFEST_BYTES)?;
            let seal_digest = core::digest(&seal_bytes);
            let request = LightroomWorkbenchIo::SealPublishBegin {
                operation: operation.into(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
                token: seal_token.clone(),
                bytes: U64(seal_bytes.len() as u64),
                blake3: seal_digest.clone(),
            };
            let reply = io.filesystem(request.clone(), &control.cancel)?;
            reply.validate_for(&request)?;
            self.seal_upload(
                operation,
                &seal_token,
                LightroomWorkbenchSealDocument::Seal,
                &seal_bytes,
                control,
            )?;
            // Publication is the selection point. After this request begins,
            // success wins over cancellation and an unknown reply is reconciled
            // only with the exact status operation, never by replaying publish.
            control.check()?;
            let publish = LightroomWorkbenchIo::SealPublish {
                operation: operation.into(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
                token: seal_token.clone(),
            };
            publication_started = true;
            let published = match io.filesystem(publish.clone(), &AtomicBool::new(false)) {
                Ok(reply) => {
                    reply.validate_for(&publish)?;
                    reply
                }
                Err(unknown) => {
                    let status = LightroomWorkbenchIo::SealStatus {
                        operation: operation.into(),
                        workbench: self.workbench.clone(),
                        generation: self.generation.clone(),
                        token: seal_token.clone(),
                    };
                    let reply = io
                        .filesystem(status.clone(), &AtomicBool::new(false))
                        .with_context(|| {
                            format!("seal publication outcome unknown: {unknown:#}")
                        })?;
                    reply.validate_for(&status)?;
                    reply
                }
            };
            let LightroomWorkbenchIoReply::SealState {
                state,
                seal_path,
                approval_path,
                seal_blake3,
                ..
            } = published
            else {
                anyhow::bail!("seal publication reply kind")
            };
            ensure!(
                state == LightroomWorkbenchSealState::Published
                    && seal_blake3.as_deref() == Some(seal_digest.as_str()),
                "seal publication did not reach exact published receipt"
            );
            Ok(selection::SealedSelection {
                seal,
                approval: preparation.approval.clone(),
                approval_bytes: preparation.approval_bytes.clone(),
                directory,
                seal_path,
                approval_path,
            })
        })();
        let result = match (result, retained_token, publication_started) {
            (Err(primary), Some(token), false) => {
                let abort = LightroomWorkbenchIo::SealAbort {
                    operation: operation.into(),
                    workbench: self.workbench.clone(),
                    generation: self.generation.clone(),
                    token,
                };
                match io.filesystem(abort.clone(), &AtomicBool::new(false)) {
                    Ok(reply) => {
                        if let Err(cleanup) = reply.validate_for(&abort) {
                            Err(primary.context(format!(
                                "seal abort receipt validation also failed: {cleanup:#}"
                            )))
                        } else {
                            Err(primary)
                        }
                    }
                    Err(cleanup) => Err(primary.context(format!(
                        "seal abort also failed and remains retained: {cleanup:#}"
                    ))),
                }
            }
            (result, _, _) => result,
        };
        self.review = Some(review);
        result
    }
    fn add_capture_managed(
        &mut self,
        operation: &str,
        directory: NativePath,
        control: &Control,
    ) -> Result<String> {
        let io = self
            .managed
            .as_ref()
            .context("managed owner absent")?
            .clone();
        let (capture_generation, directory, manifest, authority) =
            self.begin_evidence(operation, directory, control)?;
        let binding = authority.binding_blake3.clone();
        let source = io.source_open(authority, control.cancel.clone())?;
        let source_result = (|| -> Result<capture_wire::SchemaObjects> {
            let schema = io.source_schema(&source)?;
            ensure!(
                schema.authority_binding == binding,
                "CaptureSql schema authority differs"
            );
            let current = io.source_current(&source)?;
            ensure!(
                current.authority_binding == binding
                    && current.schema_roster_blake3 == schema.schema_roster_blake3,
                "CaptureSql current binding differs"
            );
            Ok(schema)
        })();
        let retired = io.source_retire(&source);
        let schema = source_result?;
        retired.context("CaptureSql did not drain")?;
        // This is the final fallible filesystem observation before the short
        // plan transaction. The evidence lease remains retained through commit.
        self.evidence_current(operation, &capture_generation, control)?;
        self.verify_root(control)?;
        let revision = self
            .plan(control)?
            .add_capture_managed(&directory, &manifest, &schema)?;
        self.release_evidence(operation, &capture_generation)?;
        Ok(revision)
    }
    fn resume_managed(
        &mut self,
        operation: &str,
        revision: &str,
        maximum: usize,
        control: &Control,
    ) -> Result<core::plan::Progress> {
        let io = self
            .managed
            .as_ref()
            .context("managed owner absent")?
            .clone();
        let directory = self.plan(control)?.managed_capture(revision)?.0;
        let (capture_generation, _, manifest, authority) =
            self.begin_evidence(operation, directory, control)?;
        ensure!(
            manifest.revision_id.as_deref() == Some(revision),
            "capture revision differs"
        );
        let binding = authority.binding_blake3.clone();
        let source = io.source_open(authority, control.cancel.clone())?;
        let source_result = (|| -> Result<usize> {
            let schema = io.source_schema(&source)?;
            ensure!(
                schema.authority_binding == binding,
                "CaptureSql schema authority differs"
            );
            let (stable_digest, pending) = self.plan(control)?.managed_resume_roster(revision)?;
            ensure!(
                schema.schema_roster_blake3 == stable_digest,
                "captured schema roster changed"
            );
            let mut retained = 0usize;
            for mut stable in pending {
                if retained >= maximum {
                    break;
                }
                control.check()?;
                let fresh = schema
                    .tables
                    .get(usize::try_from(stable.descriptor.ordinal.0)?)
                    .context("fresh CaptureSql table ordinal missing")?;
                let mut comparable = fresh.clone();
                let handle = std::mem::take(&mut comparable.table_handle);
                ensure!(
                    comparable == stable.descriptor,
                    "captured stable table descriptor changed"
                );
                let mut cursor = stable.cursor.clone();
                loop {
                    let request_rows = (maximum - retained).min(capture_wire::MAX_ROWS);
                    if request_rows == 0 {
                        break;
                    }
                    let value =
                        io.source_rows(&source, handle.clone(), cursor.clone(), request_rows)?;
                    let next_cursor = match &value {
                        TableValue::Batch(batch) => {
                            ensure!(
                                batch.authority_binding == binding
                                    && batch.schema_roster_blake3 == stable_digest
                                    && batch.table_handle == handle,
                                "CaptureSql batch binding differs"
                            );
                            batch.next_cursor.clone()
                        }
                        TableValue::Failure(failure) => {
                            ensure!(
                                failure.authority_binding == binding
                                    && failure.schema_roster_blake3 == stable_digest
                                    && failure.table_handle == handle,
                                "CaptureSql failure binding differs"
                            );
                            failure.cursor.clone()
                        }
                    };
                    self.verify_root(control)?;
                    let added = self
                        .plan(control)?
                        .apply_managed_table(revision, &stable, value)?;
                    cursor = next_cursor;
                    stable.cursor.clone_from(&cursor);
                    retained = retained.checked_add(added).context("retained row count")?;
                    control.progress(retained as u64);
                    if added == 0 || retained >= maximum {
                        break;
                    }
                }
            }
            let current = io.source_current(&source)?;
            ensure!(
                current.authority_binding == binding
                    && current.schema_roster_blake3 == stable_digest,
                "CaptureSql terminal current binding differs"
            );
            Ok(retained)
        })();
        let retired = io.source_retire(&source);
        let retained = source_result?;
        retired.context("CaptureSql did not drain")?;
        self.evidence_current(operation, &capture_generation, control)?;
        self.verify_root(control)?;
        let stage = self.plan(control)?.finish_managed_resume(revision)?;
        self.release_evidence(operation, &capture_generation)?;
        Ok(core::plan::Progress {
            revision_id: revision.into(),
            retained_this_call: retained,
            stage,
        })
    }
    fn inspect_originals_managed(
        &mut self,
        operation: &str,
        revision: &str,
        maximum: usize,
        packets: bool,
        control: &Control,
    ) -> Result<usize> {
        let io = self
            .managed
            .as_ref()
            .context("managed owner absent")?
            .clone();
        let candidates = self
            .plan(control)?
            .managed_original_candidates(revision, maximum, packets)?;
        let mut processed = 0usize;
        for candidate in candidates {
            control.check()?;
            let begin = LightroomWorkbenchIo::OriginalBegin {
                operation: operation.into(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
                candidate: candidate.clone(),
                maximum_result_bytes: U64(self.config.limits.result_bytes as u64),
            };
            let reply = io.filesystem(begin.clone(), &control.cancel)?;
            reply.validate_for(&begin)?;
            let LightroomWorkbenchIoReply::OriginalReady {
                token,
                bytes,
                blake3,
                ..
            } = reply
            else {
                anyhow::bail!("original begin reply kind")
            };
            ensure!(token == candidate.token, "original begin token differs");
            let length = usize::try_from(bytes.0)?;
            let mut encoded = Vec::new();
            encoded.try_reserve_exact(length)?;
            while encoded.len() < length {
                control.check()?;
                let page = LightroomWorkbenchIo::OriginalPage {
                    operation: operation.into(),
                    workbench: self.workbench.clone(),
                    generation: self.generation.clone(),
                    token: token.clone(),
                    offset: U64(encoded.len() as u64),
                    limit: U64((length - encoded.len()).min(16 * 1024) as u64),
                };
                let part = io.filesystem(page.clone(), &control.cancel)?;
                part.validate_for(&page)?;
                let LightroomWorkbenchIoReply::OriginalChunk {
                    token: actual,
                    offset,
                    bytes,
                    ..
                } = part
                else {
                    anyhow::bail!("original page reply kind")
                };
                ensure!(
                    actual == token && offset.0 == encoded.len() as u64,
                    "original page continuity"
                );
                encoded.extend_from_slice(&bytes);
            }
            ensure!(
                crate::lightroom::digest(&encoded) == blake3,
                "original result digest differs"
            );
            let observation: core::plan::OriginalObservation = serde_json::from_slice(&encoded)?;
            let current = LightroomWorkbenchIo::OriginalCurrent {
                operation: operation.into(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
                token: token.clone(),
            };
            let current_reply = io.filesystem(current.clone(), &control.cancel)?;
            current_reply.validate_for(&current)?;
            ensure!(
                matches!(current_reply, LightroomWorkbenchIoReply::OriginalReady { ref token, ref blake3, .. } if token == &candidate.token && blake3 == &crate::lightroom::digest(&encoded)),
                "original current evidence differs"
            );
            self.verify_root(control)?;
            self.plan(control)?
                .apply_managed_original(&candidate, observation)?;
            let release = LightroomWorkbenchIo::OriginalRelease {
                operation: operation.into(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
                token,
            };
            let released = io.filesystem(release.clone(), &AtomicBool::new(false))?;
            released.validate_for(&release)?;
            processed += 1;
            control.progress(processed as u64);
        }
        self.verify_root(control)?;
        self.plan(control)?.finish_managed_originals(revision)?;
        Ok(processed)
    }
    fn verify_root(&mut self, control: &Control) -> Result<()> {
        if let Some(io) = &self.managed {
            let request = LightroomWorkbenchIo::RootCurrent {
                operation: self.root_operation.clone(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
            };
            let reply = io.filesystem(request.clone(), &control.cancel)?;
            reply.validate_for(&request)?;
            let LightroomWorkbenchIoReply::Root { physical, root, .. } = reply else {
                anyhow::bail!("Workbench root current reply kind")
            };
            ensure!(root == self.root, "Workbench root changed");
            ensure!(
                self.root_identity.as_ref() == Some(&physical),
                "Workbench inspection object changed"
            );
            if let Some(plan) = &self.plan {
                plan.verify_managed_identity(&physical)?;
            }
        } else {
            self.pin
                .as_ref()
                .context("inspection pin absent")?
                .verify()?;
        }
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        if let Some(review) = self.review.take()
            && let Err(error) = review.close_checked()
        {
            failures.push(format!("selection review close: {error:#}"));
        }
        if let Some(plan) = self.plan.take()
            && let Err(error) = plan.close_checked()
        {
            failures.push(format!("inspection plan close: {error:#}"));
        }
        if let Some(io) = &self.managed {
            let request = LightroomWorkbenchIo::RootRelease {
                operation: self.root_operation.clone(),
                workbench: self.workbench.clone(),
                generation: self.generation.clone(),
            };
            match io.filesystem(request.clone(), &AtomicBool::new(false)) {
                Ok(reply) => {
                    if let Err(error) = reply.validate_for(&request) {
                        failures.push(format!("Workbench root release receipt: {error:#}"));
                    } else if !matches!(reply, LightroomWorkbenchIoReply::Released { .. }) {
                        failures.push("Workbench root release reply kind".into());
                    }
                }
                Err(error) => failures.push(format!("Workbench root release: {error:#}")),
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("Workbench close failed: {}", failures.join("; "));
        }
        Ok(())
    }
    fn plan(&mut self, control: &Control) -> Result<&mut Plan> {
        ensure!(
            self.review.is_none(),
            "ReleaseReview required before opening inspection writer"
        );
        if self.plan.is_none() {
            let mut plan = if let Some(identity) = &self.root_identity {
                Plan::open_managed(&self.root.to_path()?, identity)?
            } else {
                self.pin
                    .as_ref()
                    .context("inspection pin absent")?
                    .open_plan(control.clone())?
            };
            plan.set_execution(Some(control.clone()));
            self.version = plan.data_version()?;
            self.plan = Some(plan);
        }
        let plan = self.plan.as_mut().context("inspection owner unavailable")?;
        plan.set_execution(Some(control.clone()));
        if let Some(pin) = &self.pin {
            pin.verify_plan(plan)?;
        } else if let Some(identity) = &self.root_identity {
            plan.verify_managed_identity(identity)?;
        }
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
                if let Some(io) = &self.managed {
                    let request_f = LightroomWorkbenchIo::CaptureStart {
                        operation: self.current_operation(shared),
                        workbench: self.workbench.clone(),
                        generation: self.generation.clone(),
                        executable: self.config.capture_executable.clone(),
                        staging: self.config.capture_staging.clone(),
                        request,
                    };
                    let mut reply = io.filesystem(request_f.clone(), &control.cancel)?;
                    reply.validate_for(&request_f)?;
                    loop {
                        match reply {
                            LightroomWorkbenchIoReply::CaptureRunning { pid, staging, .. } => {
                                let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                                state.status.capture_pid = Some(u32::try_from(pid.0)?);
                                state.status.capture_staging = Some(staging);
                            }
                            LightroomWorkbenchIoReply::CaptureComplete { manifest, .. } => {
                                let retire = LightroomWorkbenchIo::CaptureRetire {
                                    operation: self.current_operation(shared),
                                    workbench: self.workbench.clone(),
                                    generation: self.generation.clone(),
                                };
                                let retired = io.filesystem(retire.clone(), &control.cancel)?;
                                retired.validate_for(&retire)?;
                                return encode(&manifest, limit);
                            }
                            _ => anyhow::bail!("capture filesystem reply kind"),
                        }
                        if let Err(error) = control.check() {
                            let cancel = LightroomWorkbenchIo::CaptureCancel {
                                operation: self.current_operation(shared),
                                workbench: self.workbench.clone(),
                                generation: self.generation.clone(),
                            };
                            let _ = io.filesystem(cancel, &AtomicBool::new(false));
                            return Err(error);
                        }
                        let poll = LightroomWorkbenchIo::CapturePoll {
                            operation: self.current_operation(shared),
                            workbench: self.workbench.clone(),
                            generation: self.generation.clone(),
                        };
                        thread::sleep(Duration::from_millis(10));
                        reply = io.filesystem(poll.clone(), &control.cancel)?;
                        reply.validate_for(&poll)?;
                    }
                }
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
                let revision = if self.managed.is_some() {
                    self.add_capture_managed(&self.current_operation(shared), directory, control)?
                } else {
                    let directory = native(&directory, path_limit)?;
                    self.plan(control)?.add_capture(&directory)?
                };
                encode(&serde_json::json!({"revision":revision}), limit)
            }
            Action::Resume { revision, max_rows } => {
                let rows = count(max_rows, 100_000)?;
                let progress = if self.managed.is_some() {
                    self.resume_managed(&self.current_operation(shared), &revision, rows, control)?
                } else {
                    self.plan(control)?.resume(&revision, rows)?
                };
                encode(&progress, limit)
            }
            Action::InspectOriginals {
                revision,
                limit: rows,
                inspection,
            } => {
                let rows = count(rows, 1000)?;
                let packets = matches!(inspection, OriginalInspection::Packets);
                let processed = if self.managed.is_some() {
                    self.inspect_originals_managed(
                        &self.current_operation(shared),
                        &revision,
                        rows,
                        packets,
                        control,
                    )?
                } else {
                    self.plan(control)?.check_paths(&revision, rows, packets)?
                };
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
                    requested == self.root.to_path()?.join("inspection.sqlite3"),
                    "selection inspection differs from pinned workbench"
                );
                if let Some(plan) = self.plan.take() {
                    plan.close_checked()?;
                }
                self.verify_root(control)?;
                let progress = control.processed.clone();
                let review = if let Some(identity) = &self.root_identity {
                    selection::SelectionReview::open_managed(
                        request,
                        limits,
                        control.cancel.clone(),
                        identity,
                        move |p| progress.store(p.completed, Ordering::Release),
                    )?
                } else {
                    let review = selection::SelectionReview::open(
                        request,
                        limits,
                        control.cancel.clone(),
                        move |p| progress.store(p.completed, Ordering::Release),
                    )?;
                    self.pin
                        .as_ref()
                        .context("inspection pin absent")?
                        .verify_review(&review)?;
                    review
                };
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
                let result = if self.managed.is_some() {
                    let operation = self.current_operation(shared);
                    self.seal_managed(
                        &operation,
                        &review_token,
                        &approval_blake3,
                        &approval_json,
                        output,
                        control,
                    )?
                } else {
                    let review = self
                        .review
                        .as_mut()
                        .context("prepare an explicit selection review first")?;
                    let progress = control.processed.clone();
                    review.seal(
                        &review_token,
                        &approval_blake3,
                        approval_json.as_bytes(),
                        output,
                        control.cancel.clone(),
                        move |p| {
                            progress.store(p.completed, Ordering::Release);
                        },
                    )?
                };
                // The immutable approval bytes remain exact strings; no integer
                // or NativePath reserialization changes approval authority.
                encode(
                    &serde_json::json!({"seal":result.seal,"approval_json":String::from_utf8(result.approval_bytes)?,"approval":result.approval,"directory":result.directory,"seal_path":result.seal_path,"approval_path":result.approval_path}),
                    limit,
                )
            }
            Action::ApprovalDocuments { draft_json } => {
                let review = self
                    .review
                    .as_ref()
                    .context("prepare an explicit selection review first")?;
                encode(
                    &review.approval_documents(draft_json.as_bytes(), control.cancel.clone())?,
                    limit,
                )
            }
            Action::ReleaseReview => {
                if let Some(review) = self.review.take() {
                    review.close_checked()?;
                }
                self.verify_root(control)?;
                self.plan(control)?;
                encode(&serde_json::json!({"review_released":true}), limit)
            }
        }
    }
    fn query(&mut self, query: Query, control: &Control, operation: &str) -> Result<String> {
        let maximum = self.config.limits.result_bytes;
        match query {
            Query::CaptureManifest { directory } => {
                if self.managed.is_some() {
                    let (capture_generation, _, manifest, _) =
                        self.begin_evidence(operation, directory, control)?;
                    self.release_evidence(operation, &capture_generation)?;
                    encode(&manifest, maximum)
                } else {
                    let directory = native(&directory, self.config.limits.native_path_units)?;
                    encode(&core::capture::read_manifest(&directory)?, maximum)
                }
            }
            Query::SelectionSummary => encode(
                self.review
                    .as_ref()
                    .context("no live selection review")?
                    .summary(),
                maximum,
            ),
            Query::SelectionSources {
                review_token,
                revision,
                after,
                limit,
            } => {
                let review = self.review.as_ref().context("no live selection review")?;
                encode(
                    &review.preparation_sources(
                        &review_token,
                        &revision,
                        after.0,
                        count(limit, 256)?,
                        control.cancel.clone(),
                    )?,
                    maximum,
                )
            }
            Query::SelectionPreparation {
                review_token,
                document,
                offset,
                limit,
            } => {
                let review = self.review.as_ref().context("no live selection review")?;
                encode(
                    &review.preparation_chunk(
                        &review_token,
                        document,
                        offset.0,
                        count(limit, 64 * 1024)?,
                        control.cancel.clone(),
                    )?,
                    maximum,
                )
            }
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
