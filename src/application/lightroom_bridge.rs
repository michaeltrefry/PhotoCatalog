//! Explicit inspection lifetime, independent of the foreground catalog.
//! Cached controls own no SQLite or joins. All source work remains on Workbench.
use super::{I64, U64, lightroom as lw};
use crate::storage_volume::NativePath;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
mod wire;
pub use wire::*;
static INSPECTION_OWNER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
struct OwnerLease;
impl Drop for OwnerLease {
    fn drop(&mut self) {
        INSPECTION_OWNER.store(false, std::sync::atomic::Ordering::Release);
    }
}
const ENVELOPE: usize = 128 * 1024;
// JSON may escape a single source byte to six bytes. Leave envelope headroom.
const CHUNK: usize = (ENVELOPE - 16 * 1024) / 6;
fn identity(s: &str) -> Result<()> {
    ensure!(
        !s.is_empty() && s.len() <= 128,
        "inspection identity byte limit"
    );
    Ok(())
}
fn guard(control: &lw::WorkbenchControl, g: &Guard) -> Result<lw::Status> {
    for s in [&g.workbench, &g.generation, &g.operation] {
        identity(s)?;
    }
    let s = control.status();
    ensure!(
        s.workbench == g.workbench && s.generation == g.generation && s.operation == g.operation,
        "stale inspection session/generation/operation"
    );
    Ok(s)
}
fn idle(s: &lw::Status) -> Result<()> {
    ensure!(
        s.initialized
            && !s.closed
            && matches!(
                s.phase,
                lw::Phase::Complete | lw::Phase::Failed | lw::Phase::Canceled
            ),
        "inspection operation is busy or closed"
    );
    Ok(())
}
struct Upload {
    guard: Guard,
    id: String,
    purpose: InputPurpose,
    total: usize,
    digest: Option<String>,
    expected_digest: Option<String>,
    chunks: Vec<String>,
    received: usize,
    owned: usize,
    hash: blake3::Hasher,
    sealed: Option<Arc<Vec<String>>>,
}
pub(super) struct MetadataLayouts {
    pub upload: usize,
    pub control: usize,
    pub coordinator: usize,
}
impl Upload {
    fn status(&self, attempt: &str) -> InputStatus {
        InputStatus {
            attempt: attempt.into(),
            guard: self.guard.clone(),
            input: self.id.clone(),
            purpose: self.purpose,
            total_bytes: U64(self.total as u64),
            received_bytes: U64(self.received as u64),
            blake3: self.digest.clone(),
            expected_blake3: self.expected_digest.clone(),
            complete: self.sealed.is_some(),
        }
    }
    fn bytes(&self) -> Result<Vec<u8>> {
        let chunks = self.sealed.as_ref().context("input is incomplete")?;
        let mut bytes = Vec::with_capacity(self.total);
        for chunk in chunks.iter() {
            bytes.extend_from_slice(chunk.as_bytes());
        }
        ensure!(bytes.len() == self.total, "sealed input length differs");
        ensure!(
            self.digest.as_deref() == Some(blake3::hash(&bytes).to_hex().as_str()),
            "sealed input digest differs"
        );
        Ok(bytes)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactReceipt {
    receipt: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalReceiptDraft {
    protocol: u32,
    review_token: String,
    destination: NativePath,
    import_source: String,
    overlap: crate::catalog_migration::importer::OverlapPolicy,
    keyword_overlap: crate::catalog_migration::importer::KeywordOverlap,
    artifacts: Vec<ArtifactReceipt>,
    supplements: Vec<crate::lightroom::selection::ExactDocument>,
    authorization: String,
}

fn resolve_approval_receipts(
    bytes: &[u8],
    review_token: &str,
    cancel: &std::sync::atomic::AtomicBool,
    mut resolve: impl FnMut(&str) -> Result<crate::lightroom::selection::ExactDocument>,
) -> Result<crate::lightroom::selection::ApprovalDraft> {
    let public: ApprovalReceiptDraft = serde_json::from_slice(bytes)?;
    ensure!(
        public.protocol == 1 && public.review_token == review_token,
        "approval receipt draft protocol or review token differs"
    );
    ensure!(
        public.artifacts.len() <= crate::lightroom::selection::APPROVAL_ROSTER_LIMIT
            && public.supplements.len() <= crate::lightroom::selection::APPROVAL_ROSTER_LIMIT,
        "approval roster bound"
    );
    let mut artifacts = Vec::with_capacity(public.artifacts.len());
    for reference in public.artifacts {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "approval receipt resolution canceled"
        );
        uuid::Uuid::parse_str(&reference.receipt)?;
        artifacts.push(resolve(&reference.receipt)?);
    }
    Ok(crate::lightroom::selection::ApprovalDraft {
        protocol: public.protocol,
        review_token: public.review_token,
        destination: public.destination,
        import_source: public.import_source,
        overlap: public.overlap,
        keyword_overlap: public.keyword_overlap,
        artifacts,
        supplements: public.supplements,
        authorization: public.authorization,
    })
}
#[derive(Default)]
pub(crate) struct Control {
    attempt: Option<String>,
    workbench: Option<lw::WorkbenchControl>,
    upload: Option<Upload>,
    drained: bool,
}
impl Control {
    pub(crate) fn signal_shutdown(&self) {
        if let Some(w) = &self.workbench {
            w.request_close();
        }
    }
    fn status(&self) -> Option<Status> {
        self.workbench.as_ref().map(|w| {
            let mut status = w.status();
            // Worker completion precedes the actor's join and lease release.
            // Cached Closed must certify that a new Bridge can acquire ownership.
            if status.closed && !self.drained {
                status.closed = false;
                status.phase = lw::Phase::Closing;
            }
            Status::new(self.attempt.clone().unwrap_or_default(), status)
        })
    }
    fn checked(&self, g: &Guard) -> Result<lw::WorkbenchControl> {
        let w = self.workbench.as_ref().context("no inspection workbench")?;
        guard(w, g)?;
        Ok(w.clone())
    }
    fn input(&self, g: &Guard, id: &str) -> Result<&Upload> {
        identity(id)?;
        let u = self.upload.as_ref().context("no inspection input")?;
        ensure!(
            u.id == id
                && u.guard.workbench == g.workbench
                && u.guard.generation == g.generation
                && u.guard.operation == g.operation,
            "stale inspection input"
        );
        Ok(u)
    }
    pub(crate) fn approval_documents(
        &mut self,
        g: &Guard,
        input: &str,
        review_token: &str,
        cancel: &std::sync::atomic::AtomicBool,
        mut resolve: impl FnMut(&str) -> Result<crate::lightroom::selection::ExactDocument>,
    ) -> Result<Response> {
        let w = self.checked(g)?;
        idle(&w.status())?;
        let upload = self.input(g, input)?;
        ensure!(
            upload.purpose == InputPurpose::ApprovalDraft,
            "input purpose differs"
        );
        ensure!(
            self.status()
                .as_ref()
                .and_then(|s| s.review_token.as_deref())
                == Some(review_token),
            "approval factory review token differs"
        );
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "approval receipt resolution canceled"
        );
        let trusted =
            resolve_approval_receipts(&upload.bytes()?, review_token, cancel, &mut resolve)?;
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "approval receipt resolution canceled"
        );
        let bytes = crate::lightroom::bounded_json(&trusted, w.status().limits.request_bytes)?;
        let json = lw::Payload {
            bytes: bytes.len(),
            chunks: Arc::new(vec![String::from_utf8(bytes)?]),
        };
        w.bridge_deferred(
            &g.generation,
            &g.operation,
            lw::DeferredAction::ApprovalDocuments { json },
        )?;
        self.upload = None;
        Ok(Response::Status(self.status()))
    }
    pub(crate) fn direct(&mut self, request: Request, envelope: usize) -> Result<Response> {
        match request {
            Request::Options {} => Ok(Response::Options(options(envelope))),
            Request::Status { workbench, attempt } => {
                if let Some(id) = workbench {
                    identity(&id)?;
                    ensure!(
                        self.workbench
                            .as_ref()
                            .is_some_and(|w| w.status().workbench == id),
                        "inspection session changed"
                    );
                }
                if let Some(id) = attempt {
                    identity(&id)?;
                    ensure!(
                        self.attempt.as_ref() == Some(&id),
                        "inspection open attempt changed"
                    );
                }
                Ok(Response::Status(self.status()))
            }
            Request::Cancel { guard: g } => {
                let w = self.checked(&g)?;
                w.bridge_cancel(&g.generation, &g.operation)?;
                Ok(Response::Status(self.status()))
            }
            Request::Close { workbench } => {
                identity(&workbench)?;
                let w = self.workbench.as_ref().context("no inspection workbench")?;
                ensure!(
                    w.status().workbench == workbench,
                    "inspection session changed"
                );
                w.request_close();
                self.upload = None;
                Ok(Response::Status(self.status()))
            }
            Request::Result {
                guard: g,
                token,
                offset,
                limit,
            } => {
                identity(&token)?;
                let w = self.checked(&g)?;
                ensure!(limit.0 > 0, "result chunk limit must be positive");
                let safe = ((envelope.saturating_sub(16 * 1024)) / 6)
                    .min(CHUNK)
                    .min(w.status().limits.page_bytes);
                ensure!(safe >= 4, "inspection envelope budget too small");
                let page = w.result(
                    &g.generation,
                    &g.operation,
                    &token,
                    offset,
                    U64(limit.0.min(safe as u64)),
                )?;
                Ok(Response::Result(ResultPage {
                    attempt: self.attempt.clone().unwrap_or_default(),
                    page,
                }))
            }
            Request::InputStatus { guard: g, input } => {
                self.checked(&g)?;
                let value = if let Some(id) = input {
                    Some(
                        self.input(&g, &id)?
                            .status(self.attempt.as_deref().unwrap_or_default()),
                    )
                } else {
                    self.upload
                        .as_ref()
                        .map(|u| u.status(self.attempt.as_deref().unwrap_or_default()))
                };
                Ok(Response::Input(value))
            }
            Request::SealedDocument { .. } => {
                anyhow::bail!("sealed document reads require the managed filesystem owner")
            }
            Request::ArtifactPreparation { .. } => {
                anyhow::bail!("artifact preparation requires the managed filesystem owner")
            }
            _ => anyhow::bail!("inspection request requires actor admission"),
        }
    }
}
fn options(envelope: usize) -> Options {
    Options {
        envelope_bytes: U64(envelope.min(ENVELOPE) as u64),
        chunk_bytes: U64(CHUNK.min(envelope.saturating_sub(16 * 1024) / 6) as u64),
        input_slots: U64(1),
        input_owned_factor: U64(3),
        minimum_nonfinal_chunk_bytes: U64(4096),
        selection_preparation_chunk_bytes: U64(
            crate::lightroom::selection::PREPARATION_CHUNK_BYTES as u64,
        ),
        selection_preparation_page_rows: U64(
            crate::lightroom::selection::PREPARATION_PAGE_ROWS as u64
        ),
        workbench: lw::Limits::default().into(),
        inspection: crate::lightroom::Limits::default().into(),
        selection: crate::lightroom::selection::SelectionLimits::default().into(),
    }
}
pub(crate) struct Coordinator {
    owner: Option<lw::Workbench>,
    lease: Option<OwnerLease>,
    control: Arc<Mutex<Control>>,
    managed: Option<Arc<dyn lw::ManagedIo>>,
}
pub(super) fn metadata_layouts() -> MetadataLayouts {
    MetadataLayouts {
        upload: std::mem::size_of::<Upload>(),
        control: std::mem::size_of::<Control>(),
        coordinator: std::mem::size_of::<Coordinator>(),
    }
}
impl Coordinator {
    pub(crate) fn fatal(&self) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|owner| owner.control().fatal())
    }

    pub(crate) fn new(control: Arc<Mutex<Control>>) -> Self {
        Self {
            owner: None,
            lease: None,
            control,
            managed: None,
        }
    }
    pub(crate) fn new_managed(
        control: Arc<Mutex<Control>>,
        managed: Arc<dyn lw::ManagedIo>,
    ) -> Self {
        Self {
            owner: None,
            lease: None,
            control,
            managed: Some(managed),
        }
    }
    pub(crate) fn maintain(&mut self) -> Result<()> {
        if let Some(w) = &mut self.owner {
            let joined = w.poll_closed()?;
            if joined {
                self.owner = None;
                self.lease = None;
                self.control
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drained = true;
            }
        }
        Ok(())
    }
    pub(crate) fn shutdown(&mut self) -> Result<()> {
        self.control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .signal_shutdown();
        if let Some(owner) = &mut self.owner {
            owner.request_close();
            while !owner.poll_closed()? {
                let status = owner.control().status();
                ensure!(
                    !owner.control().fatal(),
                    "managed Workbench close failed: {}",
                    status.error.as_deref().unwrap_or("retained close failure")
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        self.owner = None;
        self.lease = None;
        self.control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drained = true;
        Ok(())
    }

    /// A terminal application actor has no caller left that can retry checked
    /// shutdown. Keep every remaining child and dependency owner alive rather
    /// than allowing field destruction to counterfeit a successful reap.
    pub(crate) fn retain_failed_shutdown(&mut self) {
        std::mem::forget(self.owner.take());
        std::mem::forget(self.lease.take());
        std::mem::forget(self.managed.take());
    }
    pub(crate) fn request(
        &mut self,
        request: Request,
        executable: &std::path::Path,
        envelope: usize,
    ) -> Result<Response> {
        if request.direct() {
            return self
                .control
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .direct(request, envelope);
        }
        if let Request::Open {
            attempt,
            root,
            mode,
            capture_staging,
            limits,
        } = request
        {
            ensure!(
                envelope >= 48 * 1024,
                "inspection transport requires at least 48KiB envelopes"
            );
            identity(&attempt)?;
            self.maintain()?;
            ensure!(
                self.owner.is_none(),
                "close and drain the current inspection workbench before opening another"
            );
            let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                c.attempt.as_ref() != Some(&attempt),
                "inspection open attempt already used; reconcile cached status or use an explicit new attempt"
            );
            ensure!(
                INSPECTION_OWNER
                    .compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire
                    )
                    .is_ok(),
                "another desktop inspection owner is active in this process"
            );
            let lease = OwnerLease;
            let config = lw::Config {
                root,
                mode,
                capture_executable: NativePath::from_path(executable),
                capture_staging,
                limits: limits.try_into()?,
            };
            let w = if let Some(managed) = &self.managed {
                lw::Workbench::spawn_managed(config, managed)?
            } else {
                lw::Workbench::spawn(config)?
            };
            c.attempt = Some(attempt);
            c.workbench = Some(w.control());
            c.upload = None;
            c.drained = false;
            self.owner = Some(w);
            self.lease = Some(lease);
            return Ok(Response::Status(c.status()));
        }
        let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
        match request {
            Request::Action { guard: g, action } => {
                let w = c.checked(&g)?;
                idle(&w.status())?;
                match action {
                    Action::RegisterInventory { input } => {
                        let u = c.input(&g, &input)?;
                        ensure!(
                            u.purpose == InputPurpose::Inventory,
                            "input purpose differs"
                        );
                        let json = lw::Payload {
                            chunks: u.sealed.clone().context("input is incomplete")?,
                            bytes: u.total,
                        };
                        w.bridge_deferred(
                            &g.generation,
                            &g.operation,
                            lw::DeferredAction::Inventory(json),
                        )?;
                    }
                    Action::PrepareSelection { input, limits } => {
                        let u = c.input(&g, &input)?;
                        ensure!(
                            u.purpose == InputPurpose::SelectionRequest,
                            "input purpose differs"
                        );
                        let json = lw::Payload {
                            chunks: u.sealed.clone().context("input is incomplete")?,
                            bytes: u.total,
                        };
                        w.bridge_deferred(
                            &g.generation,
                            &g.operation,
                            lw::DeferredAction::Selection {
                                json,
                                limits: limits.try_into()?,
                            },
                        )?;
                    }
                    Action::Seal {
                        input,
                        review_token,
                        approval_blake3,
                        output,
                    } => {
                        let u = c.input(&g, &input)?;
                        ensure!(
                            u.purpose == InputPurpose::Approval
                                && u.digest.as_ref() == Some(&approval_blake3),
                            "approval input purpose/digest differs"
                        );
                        let json = lw::Payload {
                            chunks: u.sealed.clone().context("input is incomplete")?,
                            bytes: u.total,
                        };
                        w.bridge_deferred(
                            &g.generation,
                            &g.operation,
                            lw::DeferredAction::Seal {
                                json,
                                review_token,
                                approval_blake3,
                                output,
                            },
                        )?;
                    }
                    Action::ApprovalDocuments {
                        input: _,
                        review_token: _,
                    } => {
                        anyhow::bail!(
                            "approval documents require filesystem receipts resolved by the managed desktop owner"
                        )
                    }
                    value => {
                        w.bridge_start(&g.generation, &g.operation, typed_action(value)?)?;
                    }
                };
                c.upload = None;
                Ok(Response::Status(c.status()))
            }
            Request::Read { guard: g, query } => {
                let w = c.checked(&g)?;
                w.bridge_read(&g.generation, &g.operation, query.into())?;
                c.upload = None;
                Ok(Response::Status(c.status()))
            }
            Request::InputBegin {
                guard: g,
                purpose,
                total_bytes,
                expected_blake3,
            } => {
                let w = c.checked(&g)?;
                let s = w.status();
                idle(&s)?;
                ensure!(
                    c.upload.is_none(),
                    "discard or consume the current inspection input before replacement"
                );
                let total = usize::try_from(total_bytes.0)?;
                ensure!(
                    total > 0 && total <= s.limits.request_bytes,
                    "inspection input aggregate byte limit"
                );
                ensure!(
                    expected_blake3.as_ref().is_none_or(|d| d.len() == 64
                        && d.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
                    "invalid input digest"
                );
                c.upload = Some(Upload {
                    guard: g,
                    id: uuid::Uuid::new_v4().to_string(),
                    purpose,
                    total,
                    digest: None,
                    expected_digest: expected_blake3,
                    chunks: Vec::new(),
                    received: 0,
                    owned: 0,
                    hash: blake3::Hasher::new(),
                    sealed: None,
                });
                Ok(Response::Input(c.upload.as_ref().map(|u| {
                    u.status(c.attempt.as_deref().unwrap_or_default())
                })))
            }
            Request::InputAppend {
                guard: g,
                input,
                offset,
                fragment,
            } => {
                c.checked(&g)?;
                let u = c.input(&g, &input)?;
                ensure!(
                    u.sealed.is_none() && offset.0 == u.received as u64,
                    "input already sealed or offset differs"
                );
                ensure!(
                    !fragment.is_empty()
                        && fragment.len() <= CHUNK
                        && u.received
                            .checked_add(fragment.len())
                            .is_some_and(|n| n <= u.total),
                    "input chunk/aggregate byte limit"
                );
                ensure!(
                    fragment.len() >= 4096 || u.received + fragment.len() == u.total,
                    "non-final input chunks must contain at least 4096 UTF-8 bytes"
                );
                let charge = fragment
                    .capacity()
                    .checked_add(std::mem::size_of::<String>() * 2)
                    .context("input allocation overflow")?;
                ensure!(
                    u.owned
                        .checked_add(charge)
                        .is_some_and(|n| n <= u.total.saturating_mul(3).saturating_add(256)),
                    "input owned allocation budget"
                );
                let u = c.upload.as_mut().unwrap();
                u.hash.update(fragment.as_bytes());
                u.received += fragment.len();
                u.owned += charge;
                u.chunks.push(fragment);
                Ok(Response::Input(c.upload.as_ref().map(|u| {
                    u.status(c.attempt.as_deref().unwrap_or_default())
                })))
            }
            Request::InputFinish { guard: g, input } => {
                c.checked(&g)?;
                let u = c.input(&g, &input)?;
                ensure!(
                    u.sealed.is_none() && u.received == u.total,
                    "input incomplete or already sealed"
                );
                let digest = u.hash.finalize().to_hex().to_string();
                ensure!(
                    u.expected_digest.as_ref().is_none_or(|d| d == &digest),
                    "input digest mismatch; discard explicitly"
                );
                let u = c.upload.as_mut().unwrap();
                u.digest = Some(digest);
                u.sealed = Some(Arc::new(std::mem::take(&mut u.chunks)));
                Ok(Response::Input(c.upload.as_ref().map(|u| {
                    u.status(c.attempt.as_deref().unwrap_or_default())
                })))
            }
            Request::InputDiscard { guard: g, input } => {
                c.checked(&g)?;
                c.input(&g, &input)?;
                c.upload = None;
                Ok(Response::Input(None))
            }
            _ => anyhow::bail!("unsupported inspection admission"),
        }
    }
}
fn typed_action(a: Action) -> Result<lw::Action> {
    Ok(match a {
        Action::Discover { root, limits } => lw::Action::Discover {
            root,
            limits: limits.try_into()?,
        },
        Action::Capture {
            source,
            output,
            include_auxiliary,
            closed_application_evidence,
            limits,
        } => lw::Action::Capture {
            request: crate::lightroom::capture::Request {
                source,
                output,
                include_auxiliary,
                closed_application_evidence,
                limits: limits.try_into()?,
            },
        },
        Action::AddCapture { directory } => lw::Action::AddCapture { directory },
        Action::Resume { revision, max_rows } => lw::Action::Resume { revision, max_rows },
        Action::InspectOriginals {
            revision,
            limit,
            inspection,
        } => lw::Action::InspectOriginals {
            revision,
            limit,
            inspection,
        },
        Action::AssignFamily {
            revision,
            family,
            reason,
        } => lw::Action::AssignFamily {
            revision,
            family,
            reason,
        },
        Action::Choose {
            family,
            revision,
            expected_evidence,
            reason,
        } => lw::Action::Choose {
            family,
            revision,
            expected_evidence,
            reason,
        },
        Action::ReleaseReview {} => lw::Action::ReleaseReview,
        _ => anyhow::bail!("action requires immutable inspection input"),
    })
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests;
