//! Transient retained-image requests owned by the same application service.
//! One bounded read/decode per owner iteration; native original jobs stay isolated.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct ReadTicket(pub u64);
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ReadQueueUsage {
    pub queued: usize,
    pub completed: usize,
}
pub enum ReadOutcome {
    Ready(Box<PreviewView>),
    Missing,
    Stale,
    Failed {
        resource_limit: bool,
        message: String,
    },
}
pub struct ReadCompletion {
    pub outcome: ReadOutcome,
    pub queue_ms: f64,
    pub owner_read_ms: f64,
    pub metrics: CacheReadMetrics,
}
struct PendingRead {
    expected: EditRenderIdentity,
    native: Option<super::super::scheduler::NativeReadRequest>,
    interactive: bool,
    tier: Tier,
    allow_stale: bool,
    priority: Priority,
    enqueued: Instant,
    started: Option<Instant>,
    owner_ms: f64,
    metrics: CacheReadMetrics,
}
#[derive(Default)]
pub(super) struct ReadQueue {
    next: u64,
    active: Option<(
        ReadTicket,
        PendingRead,
        Box<managed_reads::ManagedRead>,
        bool,
    )>,
    pending: HashMap<ReadTicket, PendingRead>,
    completed: HashMap<ReadTicket, ReadCompletion>,
}
impl ReadQueue {
    pub(super) fn transport_busy(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|(_, _, read, _)| read.transport_busy())
    }
    pub(super) fn active(&self) -> bool {
        self.active.is_some()
    }
    pub(super) fn len(&self) -> usize {
        self.pending.len() + self.completed.len() + usize::from(self.active.is_some())
    }
}
impl PreviewService {
    /// Queue a retained cache lookup without entering the original renderer.
    /// Missing entries are explicit; callers may separately request native work.
    /// Tickets and native consumers share the configured descriptor allowance.
    pub fn queue_read(
        &mut self,
        catalog: &Catalog,
        asset: &str,
        tier: Tier,
        allow_stale: bool,
        priority: Priority,
    ) -> Result<ReadTicket> {
        self.queue_read_variant(
            catalog,
            &VariantKey::master(asset),
            tier,
            allow_stale,
            priority,
            false,
        )
    }
    pub fn queue_read_variant(
        &mut self,
        catalog: &Catalog,
        variant: &VariantKey,
        tier: Tier,
        allow_stale: bool,
        priority: Priority,
        interactive: bool,
    ) -> Result<ReadTicket> {
        ensure!(
            self.available_request_slots() > 0,
            "preview request allowance exhausted"
        );
        let expected = catalog.edit_render_identity(variant)?;
        if interactive {
            self.interactive_key(&expected, tier)?;
        } else {
            self.variant_key(&expected, tier)?;
        }
        let next = self
            .reads
            .next
            .checked_add(1)
            .context("read ticket overflow")?;
        self.reads.next = next;
        let ticket = ReadTicket(next);
        let native = if self.store.stage_calls().is_some() {
            Some(self.scheduler.queue_native_read(priority)?)
        } else {
            None
        };
        if priority == Priority::Foreground
            && let Some((_, pending, active, abandoned)) = &mut self.reads.active
            && pending.priority == Priority::Background
            && !*abandoned
        {
            active.preempt();
        }
        self.reads.pending.insert(
            ticket,
            PendingRead {
                expected,
                native,
                interactive,
                tier,
                allow_stale,
                priority,
                enqueued: Instant::now(),
                started: None,
                owner_ms: 0.0,
                metrics: CacheReadMetrics::default(),
            },
        );
        Ok(ticket)
    }
    /// Cancellation removes queued work or drops a completed view immediately.
    /// Managed transfers signal their retained task and reserved native Stop;
    /// their stage and budgets remain owned until checked task/native drain.
    /// Legacy synchronous codec calls complete at their owner-call boundary.
    pub fn cancel_read(&mut self, ticket: ReadTicket) -> bool {
        if let Some((active, _, read, abandoned)) = &mut self.reads.active
            && *active == ticket
        {
            *abandoned = true;
            read.preempting = false;
            read.signal_cancel();
            return true;
        }
        if let Some(pending) = self.reads.pending.remove(&ticket) {
            if let Some(native) = pending.native {
                let _ = self.scheduler.cancel_queued_native_read(native.id);
            }
            return true;
        }
        self.reads.completed.remove(&ticket).is_some()
    }
    /// Process at most one retained decode; foreground wins, then arrival order.
    /// An actor can receive viewport cancellation between each call. Existing
    /// pixel identity is checked before any cache work and again by cached_inner.
    pub fn tick_read(&mut self, catalog: &Catalog) -> Option<ReadTicket> {
        if self.delivery_io.load(std::sync::atomic::Ordering::Acquire)
            || (self
                .delivery_pending
                .load(std::sync::atomic::Ordering::Acquire)
                && self.reads.active.is_none())
        {
            return None;
        }
        if self.store.stage_calls().is_some() {
            return self.tick_managed_read_queue(catalog);
        }
        let ticket = *self
            .reads
            .pending
            .iter()
            .min_by_key(|(ticket, read)| (std::cmp::Reverse(read.priority), ticket.0))
            .map(|(ticket, _)| ticket)?;
        let read = self.reads.pending.remove(&ticket).unwrap();
        let queue_ms = read.enqueued.elapsed().as_secs_f64() * 1000.0;
        let owner_started = Instant::now();
        let mut metrics = CacheReadMetrics::default();
        let result = (|| -> Result<ReadOutcome> {
            let current = catalog.edit_render_identity(&read.expected.key)?;
            if !same_edit(&current, &read.expected) {
                return Ok(ReadOutcome::Stale);
            }
            let before = self.decoded.access_counts();
            let start = Instant::now();
            let result = self.cached_inner(
                catalog,
                &read.expected.key,
                read.tier,
                read.allow_stale,
                read.interactive,
                Some(&mut metrics),
            );
            metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;
            let after = self.decoded.access_counts();
            metrics.decoded_hits = after.0.saturating_sub(before.0);
            metrics.decoded_misses = after.1.saturating_sub(before.1);
            metrics.returned_pixels = matches!(&result, Ok(Some(_)));
            Ok(match result? {
                Some(view) => ReadOutcome::Ready(Box::new(view)),
                None => ReadOutcome::Missing,
            })
        })();
        let outcome = result.unwrap_or_else(|error| ReadOutcome::Failed {
            resource_limit: error.downcast_ref::<DecodedBudgetExceeded>().is_some()
                || error.downcast_ref::<EncodedBudgetExceeded>().is_some(),
            message: format!("{error:#}"),
        });
        self.reads.completed.insert(
            ticket,
            ReadCompletion {
                outcome,
                queue_ms,
                owner_read_ms: owner_started.elapsed().as_secs_f64() * 1000.0,
                metrics,
            },
        );
        Some(ticket)
    }
    fn tick_managed_read_queue(&mut self, catalog: &Catalog) -> Option<ReadTicket> {
        let owner_started = Instant::now();
        let (ticket, mut read, result, abandoned) =
            if let Some((ticket, mut read, mut active, abandoned)) = self.reads.active.take() {
                let polled = self.poll_managed_read(catalog, &mut active);
                if matches!(polled, Ok(Some(_))) && active.preempting && !abandoned {
                    read.native = active.request.take();
                    read.started = None;
                    self.reads.pending.insert(ticket, read);
                    return None;
                }
                read.owner_ms += owner_started.elapsed().as_secs_f64() * 1000.0;
                match polled {
                    Ok(None) => {
                        self.reads.active = Some((ticket, read, active, abandoned));
                        return None;
                    }
                    result => (ticket, read, result.map(|v| v.flatten()), abandoned),
                }
            } else {
                let ticket = *self
                    .reads
                    .pending
                    .iter()
                    .min_by_key(|(ticket, read)| (std::cmp::Reverse(read.priority), ticket.0))
                    .map(|(ticket, _)| ticket)?;
                let mut read = self.reads.pending.remove(&ticket).unwrap();
                read.started = Some(owner_started);
                let before = self.decoded.access_counts();
                let result = (|| -> Result<managed_reads::Begin> {
                    let current = catalog.edit_render_identity(&read.expected.key)?;
                    if !same_edit(&current, &read.expected) {
                        return Ok(managed_reads::Begin::Ready(None));
                    }
                    self.begin_managed_read(
                        catalog,
                        &read.expected.key,
                        read.tier,
                        read.allow_stale,
                        read.interactive,
                        false,
                    )
                })();
                let after = self.decoded.access_counts();
                read.metrics.decoded_hits = after.0.saturating_sub(before.0);
                read.metrics.decoded_misses = after.1.saturating_sub(before.1);
                read.owner_ms += owner_started.elapsed().as_secs_f64() * 1000.0;
                match result {
                    Ok(managed_reads::Begin::Pending(mut active)) => {
                        active.request = read.native.take();
                        self.reads.active = Some((ticket, read, active, false));
                        return None;
                    }
                    Ok(managed_reads::Begin::Ready(view)) => {
                        (ticket, read, Ok(view.map(|v| *v)), false)
                    }
                    Err(error) => (ticket, read, Err(error), false),
                }
            };
        if let Some(native) = read.native.take() {
            let _ = self.scheduler.cancel_queued_native_read(native.id);
        }
        if abandoned {
            return None;
        }
        let outcome = match result {
            Ok(Some(view)) => ReadOutcome::Ready(Box::new(view)),
            Ok(None) => ReadOutcome::Missing,
            Err(error) => ReadOutcome::Failed {
                resource_limit: error.downcast_ref::<DecodedBudgetExceeded>().is_some()
                    || error.downcast_ref::<EncodedBudgetExceeded>().is_some()
                    || error
                        .downcast_ref::<crate::catalog_session::store::ResourceLimit>()
                        .is_some(),
                message: crate::filesystem_worker::wire::Failure::new(
                    crate::filesystem_worker::wire::FailureKind::Unknown,
                    format_args!("{error:#}"),
                )
                .message,
            },
        };
        read.metrics.total_ms = read
            .started
            .unwrap_or(owner_started)
            .elapsed()
            .as_secs_f64()
            * 1000.0;
        read.metrics.header_decode_ms = (read.metrics.total_ms
            - read.metrics.catalog_identity_ms
            - read.metrics.store_read_checksum_ms)
            .max(0.0);
        read.metrics.returned_pixels = matches!(&outcome, ReadOutcome::Ready(_));
        self.reads.completed.insert(
            ticket,
            ReadCompletion {
                outcome,
                queue_ms: read
                    .started
                    .unwrap_or(owner_started)
                    .duration_since(read.enqueued)
                    .as_secs_f64()
                    * 1000.0,
                owner_read_ms: read.owner_ms,
                metrics: read.metrics,
            },
        );
        Some(ticket)
    }
    pub(super) fn try_read_shutdown(&mut self) -> Result<()> {
        if let Some((_, _, active, _)) = &mut self.reads.active {
            while !active.drain()? {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        Ok(())
    }
    pub(super) fn finish_read_shutdown(&mut self) -> Result<()> {
        if let Some((_, _, active, _)) = &mut self.reads.active {
            active.release_scheduler(&mut self.scheduler)?;
        }
        self.reads.active.take();
        Ok(())
    }
    pub(super) fn signal_read_shutdown(&mut self) {
        for (_, pending) in self.reads.pending.drain() {
            if let Some(native) = pending.native {
                let _ = self.scheduler.cancel_queued_native_read(native.id);
            }
        }
        if let Some((_, _, active, abandoned)) = &mut self.reads.active {
            *abandoned = true;
            active.preempting = false;
            active.signal_cancel();
        }
    }
    pub fn take_read(&mut self, ticket: ReadTicket) -> Option<ReadCompletion> {
        self.reads.completed.remove(&ticket)
    }
    pub fn read_queue_usage(&self) -> ReadQueueUsage {
        ReadQueueUsage {
            queued: self.reads.pending.len() + usize::from(self.reads.active.is_some()),
            completed: self.reads.completed.len(),
        }
    }
}
