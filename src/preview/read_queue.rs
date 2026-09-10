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
    interactive: bool,
    tier: Tier,
    allow_stale: bool,
    priority: Priority,
    enqueued: Instant,
}
#[derive(Default)]
pub(super) struct ReadQueue {
    next: u64,
    pending: HashMap<ReadTicket, PendingRead>,
    completed: HashMap<ReadTicket, ReadCompletion>,
}
impl ReadQueue {
    pub(super) fn len(&self) -> usize {
        self.pending.len() + self.completed.len()
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
        self.reads.pending.insert(
            ticket,
            PendingRead {
                expected,
                interactive,
                tier,
                allow_stale,
                priority,
                enqueued: Instant::now(),
            },
        );
        Ok(ticket)
    }
    /// Cancellation removes queued work or drops a completed view immediately.
    /// A retained in-process JPEG decode completes at its owner-call boundary;
    /// this does not claim interruption inside that decoder. Full-original worker
    /// cancellation uses the separate kill-and-wait lease implementation.
    pub fn cancel_read(&mut self, ticket: ReadTicket) -> bool {
        self.reads.pending.remove(&ticket).is_some()
            || self.reads.completed.remove(&ticket).is_some()
    }
    /// Process at most one retained decode; foreground wins, then arrival order.
    /// An actor can receive viewport cancellation between each call. Existing
    /// pixel identity is checked before any cache work and again by cached_inner.
    pub fn tick_read(&mut self, catalog: &Catalog) -> Option<ReadTicket> {
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
    pub fn take_read(&mut self, ticket: ReadTicket) -> Option<ReadCompletion> {
        self.reads.completed.remove(&ticket)
    }
    pub fn read_queue_usage(&self) -> ReadQueueUsage {
        ReadQueueUsage {
            queued: self.reads.pending.len(),
            completed: self.reads.completed.len(),
        }
    }
}
