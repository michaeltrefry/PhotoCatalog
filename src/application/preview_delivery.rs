//! Managed ready-ticket delivery. Validation uses the shared read queue; only
//! the admitted encoded filesystem transfer runs off the catalog actor.
use super::*;
use crate::preview::encoded_delivery::{Gate, Transfer};
use anyhow::Context as _;

struct BinaryGrant {
    usage: Arc<AtomicUsize>,
    bytes: usize,
}
impl BinaryGrant {
    fn reserve(shared: &Shared, bytes: usize) -> std::result::Result<Self, BridgeError> {
        shared
            .binary
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|n| *n <= shared.limits.binary_bytes)
            })
            .map_err(|_| error(ErrorCode::ResourceLimit, "binary transport byte allowance"))?;
        Ok(Self {
            usage: shared.binary.clone(),
            bytes,
        })
    }
    fn payload(
        mut self,
        bytes: Vec<u8>,
        mime: String,
    ) -> std::result::Result<PreviewBytes, BridgeError> {
        if bytes.len() != self.bytes || bytes.capacity() != self.bytes {
            return Err(error(
                ErrorCode::Native,
                "encoded delivery length/capacity changed",
            ));
        }
        self.bytes = 0;
        Ok(PreviewBytes {
            bytes,
            mime,
            usage: self.usage.clone(),
        })
    }
}
impl Drop for BinaryGrant {
    fn drop(&mut self) {
        self.usage.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct Entry {
    id: String,
    key: VariantKey,
    identity: crate::catalog_edits::EditRenderIdentity,
    tier: preview::Tier,
    foreground: bool,
    read: Option<preview::ReadTicket>,
    view: Option<Box<preview::PreviewView>>,
    cancel: Cancellation,
    reply: mpsc::SyncSender<std::result::Result<PreviewBytes, BridgeError>>,
    enqueued: Option<Instant>,
}
struct Active {
    entry: Entry,
    _gate: Gate,
    transfer: Option<Transfer>,
    binary: Option<BinaryGrant>,
    started: Option<Instant>,
}
#[derive(Default)]
pub(super) struct Queue {
    pending: VecDeque<Entry>,
    active: Option<Active>,
}
pub(super) struct Context<'a> {
    pub service: &'a mut PreviewService,
    pub catalog: &'a Catalog,
    pub tickets: &'a mut HashMap<String, Ticket>,
    pub token: &'a str,
    pub shared: &'a Shared,
    pub policy: &'a preview::PreviewPolicy,
}
fn live(entry: &Entry, ctx: &Context<'_>) -> bool {
    let Some(ticket) = ctx.tickets.get(&entry.id) else {
        return false;
    };
    let current = {
        let queue = ctx.shared.queue.lock().unwrap();
        queue
            .viewport
            .get(&(ctx.token.to_owned(), ticket.dto.viewport.clone()))
            == Some(&ticket.dto.generation.0)
            && queue
                .ticket_foreground
                .contains_key(&(ctx.token.to_owned(), entry.id.clone()))
    };
    current
        && matches!(ticket.dto.state, PreviewState::Ready)
        && !entry.cancel.is_canceled()
        && !ticket.cancel.is_canceled()
        && ctx
            .catalog
            .edit_render_identity(&entry.key)
            .is_ok_and(|now| identity_equal(&now, &entry.identity))
}
fn canceled() -> BridgeError {
    error(ErrorCode::Canceled, "preview changed or delivery canceled")
}
fn transfer_error(e: anyhow::Error) -> BridgeError {
    if e.downcast_ref::<crate::filesystem_worker::wire::Failure>()
        .is_some_and(|failure| {
            failure.kind == crate::filesystem_worker::wire::FailureKind::Canceled
        })
    {
        return error(ErrorCode::Canceled, e.to_string());
    }
    if e.is::<preview::EncodedBudgetExceeded>() || e.is::<preview::DecodedBudgetExceeded>() {
        error(ErrorCode::ResourceLimit, e.to_string())
    } else {
        native(e)
    }
}
impl Queue {
    pub fn enqueue(
        &mut self,
        ctx: &mut Context<'_>,
        id: String,
        cancel: Cancellation,
        reply: mpsc::SyncSender<std::result::Result<PreviewBytes, BridgeError>>,
    ) {
        let result = (|| -> std::result::Result<Entry, BridgeError> {
            if self.pending.len() + usize::from(self.active.is_some()) >= ctx.shared.limits.queued {
                return Err(error(ErrorCode::Busy, "encoded delivery queue full"));
            }
            let ticket = ctx
                .tickets
                .get(&id)
                .ok_or_else(|| error(ErrorCode::StaleSession, "preview ticket expired"))?;
            if !matches!(ticket.dto.state, PreviewState::Ready) {
                return Err(error(ErrorCode::Busy, "preview is not ready"));
            }
            let current = ctx
                .catalog
                .edit_render_identity(&ticket.dto.key)
                .map_err(native)?;
            if !identity_equal(&current, &ticket.identity) {
                return Err(error(
                    ErrorCode::Superseded,
                    "preview edit identity changed",
                ));
            }
            let read = ctx
                .service
                .queue_read_variant(
                    ctx.catalog,
                    &ticket.dto.key,
                    ticket.tier,
                    false,
                    if ticket.foreground {
                        preview::Priority::Foreground
                    } else {
                        preview::Priority::Background
                    },
                    ticket.interactive,
                )
                .map_err(transfer_error)?;
            Ok(Entry {
                id: id.clone(),
                key: ticket.dto.key.clone(),
                identity: ticket.identity.clone(),
                tier: ticket.tier,
                foreground: ticket.foreground,
                read: Some(read),
                view: None,
                cancel,
                reply: reply.clone(),
                enqueued: ticket.dto.diagnostic.as_ref().map(|_| Instant::now()),
            })
        })();
        match result {
            Ok(entry) => self.pending.push_back(entry),
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    }
    pub fn advance(&mut self, ctx: &mut Context<'_>) {
        let count = self.pending.len();
        for _ in 0..count {
            let mut entry = self.pending.pop_front().unwrap();
            if !live(&entry, ctx) {
                if let Some(read) = entry.read.take() {
                    ctx.service.cancel_read(read);
                }
                let _ = entry.reply.send(Err(canceled()));
            } else {
                self.pending.push_back(entry);
            }
        }
        if let Some(active) = &mut self.active {
            if !live(&active.entry, ctx) {
                active.entry.cancel.cancel();
                if let Some(transfer) = &active.transfer {
                    transfer.signal_cancel();
                }
            }
            if let Some(transfer) = &mut active.transfer {
                let result = match transfer.poll() {
                    Ok(None) => return,
                    result => result,
                };
                let mut active = self.active.take().unwrap();
                let result = result
                    .map_err(transfer_error)
                    .and_then(|output| {
                        output
                            .context("encoded delivery result missing")
                            .map_err(native)
                    })
                    .and_then(|output| output.finish(ctx.service).map_err(transfer_error))
                    .and_then(|bytes| {
                        if !live(&active.entry, ctx) {
                            return Err(canceled());
                        }
                        let codec = match active.entry.tier {
                            preview::Tier::Thumbnail => ctx.policy.thumbnail.encoding.codec,
                            preview::Tier::Large => ctx.policy.large.encoding.codec,
                        };
                        let mime = match codec {
                            preview::Codec::Jpeg => "image/jpeg",
                            preview::Codec::Webp => "image/webp",
                            preview::Codec::Avif => "image/avif",
                        }
                        .into();
                        active
                            .binary
                            .take()
                            .expect("admitted binary guard")
                            .payload(bytes, mime)
                    });
                if result.is_ok()
                    && let Some(diagnostic) = ctx
                        .tickets
                        .get_mut(&active.entry.id)
                        .and_then(|ticket| ticket.dto.diagnostic.as_mut())
                    && let Some(delivery) = &mut diagnostic.delivery
                {
                    delivery.transfer_ms = active
                        .started
                        .map(|started| started.elapsed().as_secs_f64() * 1000.0)
                        .unwrap_or(0.0);
                    delivery.total_ms = active
                        .entry
                        .enqueued
                        .map(|started| started.elapsed().as_secs_f64() * 1000.0)
                        .unwrap_or(0.0);
                }
                if result.is_ok()
                    && let Some(ticket) = ctx.tickets.get_mut(&active.entry.id)
                {
                    ticket.touched = Instant::now();
                }
                let _ = active.entry.reply.send(result);
                return;
            }
        }
        if self.active.is_none() {
            let Some(index) = self
                .pending
                .iter()
                .position(|e| e.foreground)
                .or_else(|| (!self.pending.is_empty()).then_some(0))
            else {
                return;
            };
            let entry = &mut self.pending[index];
            if let Some(read) = entry.read {
                let Some(done) = ctx.service.take_read(read) else {
                    return;
                };
                entry.read = None;
                if let Some(diagnostic) = ctx
                    .tickets
                    .get_mut(&entry.id)
                    .and_then(|ticket| ticket.dto.diagnostic.as_mut())
                {
                    let (outcome, selected) = match &done.outcome {
                        preview::ReadOutcome::Ready(view) => {
                            ("ready", view.key.as_ref().and_then(|key| key.digest().ok()))
                        }
                        preview::ReadOutcome::Missing => ("missing", None),
                        preview::ReadOutcome::Stale => ("stale", None),
                        preview::ReadOutcome::Failed { .. } => ("failed", None),
                    };
                    let read = read_diagnostic(&done, outcome);
                    if let Some(selected) = selected {
                        diagnostic.current_key_matches_selected = diagnostic
                            .expected_key_digest
                            .as_ref()
                            .map(|expected| expected == &selected);
                        diagnostic.selected_key_digest = Some(selected);
                    }
                    diagnostic.delivery = Some(PreviewDeliveryDiagnostic {
                        ready_for_transfer_ms: entry
                            .enqueued
                            .map(|started| started.elapsed().as_secs_f64() * 1000.0)
                            .unwrap_or(0.0),
                        retained_read: read,
                        transfer_ms: 0.0,
                        total_ms: 0.0,
                    });
                }
                match done.outcome {
                    preview::ReadOutcome::Ready(view) => entry.view = Some(view),
                    outcome => {
                        let entry = self.pending.remove(index).unwrap();
                        let error = match outcome {
                            preview::ReadOutcome::Failed {
                                resource_limit,
                                message,
                            } => error(
                                if resource_limit {
                                    ErrorCode::ResourceLimit
                                } else {
                                    ErrorCode::Native
                                },
                                message,
                            ),
                            _ => error(ErrorCode::Superseded, "current preview no longer cached"),
                        };
                        let _ = entry.reply.send(Err(error));
                        return;
                    }
                }
            }
            let gate = match ctx.service.pause_for_encoded_delivery() {
                Ok(gate) => gate,
                Err(e) if e.is::<preview::stage_io::Busy>() => return,
                Err(e) => {
                    let entry = self.pending.remove(index).unwrap();
                    let _ = entry.reply.send(Err(transfer_error(e)));
                    return;
                }
            };
            self.active = Some(Active {
                entry: self.pending.remove(index).unwrap(),
                _gate: gate,
                transfer: None,
                binary: None,
                started: None,
            });
        }
        let active = self.active.as_mut().unwrap();
        if active.entry.cancel.is_canceled() {
            let active = self.active.take().unwrap();
            let _ = active.entry.reply.send(Err(canceled()));
            return;
        }
        let plan = match ctx
            .service
            .prepare_encoded_delivery(active.entry.view.as_deref().unwrap())
        {
            Ok(plan) => plan,
            Err(e) if e.is::<preview::stage_io::Busy>() => return,
            Err(e) => {
                let active = self.active.take().unwrap();
                let _ = active.entry.reply.send(Err(transfer_error(e)));
                return;
            }
        };
        let result = (|| -> std::result::Result<(Transfer, BinaryGrant), BridgeError> {
            let grant = BinaryGrant::reserve(
                ctx.shared,
                usize::try_from(plan.bytes())
                    .map_err(|_| error(ErrorCode::ResourceLimit, "encoded delivery size"))?,
            )?;
            let transfer = plan
                .start(active.entry.cancel.0.clone())
                .map_err(transfer_error)?;
            Ok((transfer, grant))
        })();
        match result {
            Ok((transfer, grant)) => {
                active.entry.view = None;
                active.transfer = Some(transfer);
                active.binary = Some(grant);
                active.started = active.entry.enqueued.map(|_| Instant::now());
            }
            Err(e) => {
                let active = self.active.take().unwrap();
                let _ = active.entry.reply.send(Err(e));
            }
        }
    }
    pub fn signal_shutdown(&mut self, service: &mut PreviewService) {
        for entry in &mut self.pending {
            entry.cancel.cancel();
            if let Some(read) = entry.read.take() {
                service.cancel_read(read);
            }
        }
        if let Some(active) = &mut self.active {
            active.entry.cancel.cancel();
            if let Some(transfer) = &active.transfer {
                transfer.signal_cancel();
            }
        }
    }
    pub fn shutdown(
        &mut self,
        service: &mut PreviewService,
    ) -> std::result::Result<(), BridgeError> {
        self.signal_shutdown(service);
        if let Some(active) = &mut self.active
            && let Some(transfer) = &mut active.transfer
        {
            let _ = transfer.shutdown().map_err(transfer_error)?;
        }
        if let Some(active) = self.active.take() {
            let _ = active.entry.reply.send(Err(canceled()));
        }
        for entry in self.pending.drain(..) {
            let _ = entry.reply.send(Err(canceled()));
        }
        Ok(())
    }
}

impl Actor {
    pub(super) fn enqueue_encoded_delivery(
        &mut self,
        token: String,
        id: String,
        cancel: Cancellation,
        reply: mpsc::SyncSender<std::result::Result<PreviewBytes, BridgeError>>,
    ) {
        let shared = self.shared.clone();
        let policy = self.config.preview_policy.clone();
        match self.current(&token) {
            Ok(open) => {
                let mut ctx = Context {
                    service: &mut open.service,
                    catalog: &open.catalog,
                    tickets: &mut open.tickets,
                    token: &open.token,
                    shared: &shared,
                    policy: &policy,
                };
                open.deliveries.enqueue(&mut ctx, id, cancel, reply);
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }
}
pub(super) fn advance(open: &mut Open, shared: &Shared, policy: &preview::PreviewPolicy) {
    let mut ctx = Context {
        service: &mut open.service,
        catalog: &open.catalog,
        tickets: &mut open.tickets,
        token: &open.token,
        shared,
        policy,
    };
    open.deliveries.advance(&mut ctx);
}

#[cfg(test)]
mod tests;
