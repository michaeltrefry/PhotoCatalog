//! Operation-scoped remote source ownership. All filesystem work stays in the
//! child; this owner retains its process until the caller's SQL/permit stack has
//! drained. It is never stored on or joined by the foreground actor.
mod transport;
#[cfg(test)]
use crate::lightroom_migration_worker::memory::MemoryBudget;

use super::relay::{Kind, client::Client};
use transport::Transport;

use super::{
    transport::*,
    wire::{Budget, Expected, Query, Value},
};
use crate::{
    application::U64,
    catalog_migration::artifacts::{ArtifactDescriptor, ArtifactLimits, ArtifactRead},
    lightroom::{
        capture::Manifest,
        migration_source::{
            ByteRef, Collection, Cursor, ImageLinks, InputSeal, MigrationRead, Page, ReadLimits,
            Resolution, StableSource,
        },
    },
    lightroom_migration_worker::{
        identity::FileKey,
        memory::Reservation,
        process::{Output, Process, Stop},
        protocol::Guard,
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Atomic-only single-epoch observation for writer admission and the privately owned
/// destination connection's precommit hook. Observation never sends IPC or opens
/// a file. A commit that wins before observed loss remains durable authority.
#[derive(Clone)]
pub(crate) struct Health {
    process: Arc<Stop>,
    cancel: Arc<AtomicBool>,
    poisoned: Arc<AtomicBool>,
}
impl Health {
    pub(crate) fn failed(&self) -> bool {
        self.process.admission().load(Ordering::Acquire)
            || self.cancel.load(Ordering::Acquire)
            || self.poisoned.load(Ordering::Acquire)
    }
}
fn reserve_opening(
    memory: &mut Reservation,
    completed: &mut u64,
    sequence: u64,
    bytes: u64,
) -> Result<usize> {
    let next = completed
        .checked_add(1)
        .context("source allocation sequence exhausted")?;
    ensure!(
        sequence == next,
        "source allocation request sequence differs"
    );
    let bytes = usize::try_from(bytes)?;
    memory.grow(bytes)?;
    *completed = next;
    Ok(bytes)
}
struct SourceAdmission {
    epoch: Epoch,
    binding: String,
    encoded: Vec<u8>,
    cancel: Arc<AtomicBool>,
    open_ms: u64,
    read_ms: u64,
    budget: Budget,
    memory: Reservation,
}

struct Session {
    process: Transport,
    epoch: Epoch,
    binding: String,
    health: Health,
    next: u64,
    completed: u64,
    chain: String,
    deadline: Duration,
    canceled: bool,
    retired: bool,
    budget: Budget,
    // Retired only after Process::terminate has verified reap and joined I/O.
    memory: Reservation,
}
impl Session {
    fn open(
        relay: Arc<Client>,
        epoch: Epoch,
        authority: Authority,
        opening_graph: usize,
        cancel: Arc<AtomicBool>,
        open_ms: u64,
        read_ms: u64,
    ) -> Result<Self> {
        epoch.validate()?;
        ensure!(
            (1..=3_600_000).contains(&open_ms) && (1..=120_000).contains(&read_ms),
            "source process deadline bounds"
        );
        let kind = match &authority {
            Authority::Sql { .. } => Kind::Sql,
            Authority::Artifact { .. } => Kind::Raw,
        };
        let budget = Budget::from_authority(&authority)?;
        let encoded_length = exact_json_length(&authority, AUTHORITY_BYTES, &cancel)?;
        relay.admit_opening(
            kind,
            crate::lightroom_migration_worker::memory::layout::add(
                opening_graph,
                crate::lightroom_migration_worker::memory::layout::mul(3, encoded_length)?,
            )?,
        )?;
        let encoded = exact_json(&authority, AUTHORITY_BYTES, &cancel)?;
        #[cfg(all(test, feature = "internal-capacity-probes"))]
        crate::capacity_probes::observe(crate::capacity_probes::OPEN_AUTHORITY, encoded.capacity());

        let binding = authority.binding()?;
        let process_stop = Arc::new(Stop::default());
        let process = relay.open(
            kind,
            epoch.clone(),
            process_stop.clone(),
            Instant::now() + Duration::from_millis(open_ms),
        )?;
        let scoped_memory = process.allocation_budget();
        // Authority/result/caller graphs use the operation's allowance. This
        // budget is only for Source-owned opening grants requested before locks.
        Self::admit(
            process,
            process_stop,
            SourceAdmission {
                epoch,
                binding,
                encoded,
                cancel,
                open_ms,
                read_ms,
                budget,
                memory: scoped_memory.reservation(),
            },
        )
    }
    fn admit(
        process: impl Into<Transport>,
        process_stop: Arc<Stop>,
        admission: SourceAdmission,
    ) -> Result<Self> {
        let SourceAdmission {
            epoch,
            binding,
            encoded,
            cancel,
            open_ms,
            read_ms,
            budget,
            memory,
        } = admission;
        ensure!(
            (1..=3_600_000).contains(&open_ms) && (1..=120_000).contains(&read_ms),
            "source process deadline bounds"
        );
        let until = Instant::now() + Duration::from_millis(open_ms);
        let mut session = Self {
            process: process.into(),
            epoch,
            chain: binding.clone(),
            binding,
            health: Health {
                process: process_stop,
                cancel,
                poisoned: Arc::new(AtomicBool::new(false)),
            },
            next: 1,
            completed: 0,
            deadline: Duration::from_millis(read_ms),
            canceled: false,
            retired: false,
            budget,
            memory,
        };
        let digest = crate::lightroom::digest(&encoded);
        session.send(
            Request::Begin {
                epoch: session.epoch.clone(),
                bytes: U64(encoded.len() as u64),
                blake3: digest,
            },
            until,
        )?;
        for (index, bytes) in encoded.chunks(CHUNK_BYTES).enumerate() {
            session.check()?;
            session.send(
                Request::Authority {
                    epoch: session.epoch.clone(),
                    offset: U64((index * CHUNK_BYTES) as u64),
                    bytes: bytes.to_vec(),
                },
                until,
            )?;
        }
        session.send(
            Request::Open {
                epoch: session.epoch.clone(),
            },
            until,
        )?;
        let mut allocation_sequence = 0u64;
        let binding = loop {
            match session.receive(until, true)? {
                Reply::Reserve {
                    sequence, bytes, ..
                } => {
                    session.check()?;
                    // Charge before acknowledgement. The exact epoch/sequence/byte
                    // echo cannot grant a different allocation or replay an old one.
                    let bytes = reserve_opening(
                        &mut session.memory,
                        &mut allocation_sequence,
                        sequence.0,
                        bytes.0,
                    )?;
                    session.send(
                        Request::Reserved {
                            epoch: session.epoch.clone(),
                            sequence,
                            bytes: crate::application::U64(bytes.try_into()?),
                        },
                        until,
                    )?;
                }
                Reply::Ready { binding, .. } => break binding,
                _ => anyhow::bail!("source Ready or opening allocation required"),
            }
        };
        ensure!(
            binding == session.binding,
            "source admitted binding mismatch"
        );
        session.check()?;
        Ok(session)
    }
    fn check(&self) -> Result<()> {
        ensure!(
            !self.retired && !self.health.failed(),
            "source epoch unavailable/canceled; explicit full re-admission required"
        );
        Ok(())
    }
    fn send(&mut self, mut request: Request, until: Instant) -> Result<()> {
        loop {
            ensure!(Instant::now() < until, "source input deadline");
            match self.process.try_send(request)? {
                None => return Ok(()),
                Some(pending) => request = pending,
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn signal_cancel(&mut self, until: Instant) -> Result<()> {
        if self.health.cancel.load(Ordering::Acquire) && !self.canceled {
            self.send(
                Request::Cancel {
                    epoch: self.epoch.clone(),
                },
                until,
            )?;
            self.canceled = true;
        }
        Ok(())
    }
    fn receive(&mut self, until: Instant, observe_cancel: bool) -> Result<Reply> {
        loop {
            ensure!(Instant::now() < until, "source output deadline");
            if observe_cancel {
                self.signal_cancel(until)?;
            }
            match self.process.try_receive()? {
                Output::Frame(reply) => {
                    ensure!(reply.epoch() == &self.epoch, "source reply epoch mismatch");
                    if let Reply::Failed { detail, .. } = reply {
                        self.health.poisoned.store(true, Ordering::Release);
                        anyhow::bail!("source reader: {detail}");
                    }
                    return Ok(reply);
                }
                Output::End => anyhow::bail!("source reader ended before expected reply"),
                Output::Pending => thread::sleep(Duration::from_millis(5)),
            }
        }
    }
    fn query(&mut self, query: Read) -> Result<Value> {
        self.check()?;
        let result = self.query_inner(query);
        if result.is_err() {
            self.health.poisoned.store(true, Ordering::Release);
        }
        result
    }
    fn query_inner(&mut self, query: Read) -> Result<Value> {
        #[cfg(all(test, feature = "internal-capacity-probes"))]
        let _capacity_phase = crate::capacity_probes::phase(crate::capacity_probes::SOURCE_DECODED);
        let until = Instant::now() + self.deadline;
        let sequence = self.next;
        self.next = self
            .next
            .checked_add(1)
            .context("source sequence exhausted")?;
        self.process
            .admit_producer(Value::producer_allocation(Expected {
                read: &query,
                budget: self.budget,
                binding: &self.binding,
            })?)?;
        self.check()?;
        let query_digest =
            crate::lightroom::digest(&crate::lightroom::bounded_json(&query, 120 * 1024)?);
        let expected = query.clone();
        self.send(
            Request::Read {
                epoch: self.epoch.clone(),
                sequence: U64(sequence),
                binding: self.binding.clone(),
                query,
            },
            until,
        )?;
        let Reply::Result {
            sequence: got,
            query_blake3,
            bytes,
            blake3,
            ..
        } = self.receive(until, true)?
        else {
            anyhow::bail!("source result header required");
        };
        let length: usize = bytes.0.try_into()?;
        ensure!(
            got.0 == sequence
                && query_blake3 == query_digest
                && digest_valid(&blake3)
                && (1..=RESULT_BYTES).contains(&length),
            "source result admission identity/bounds"
        );
        let (transient, graph) = Value::allocation(
            length,
            Expected {
                read: &expected,
                budget: self.budget,
                binding: &self.binding,
            },
        )?;
        self.process.admit_result(transient, graph)?;
        let mut encoded = Vec::with_capacity(length);
        #[cfg(all(test, feature = "internal-capacity-probes"))]
        crate::capacity_probes::observe(crate::capacity_probes::SOURCE_ENCODED, encoded.capacity());

        while encoded.len() < length {
            let Reply::Chunk {
                sequence: got,
                offset,
                bytes,
                ..
            } = self.receive(until, true)?
            else {
                anyhow::bail!("source result chunk required");
            };
            ensure!(
                got.0 == sequence
                    && offset.0 == encoded.len() as u64
                    && !bytes.is_empty()
                    && bytes.len() <= CHUNK_BYTES
                    && bytes.len() <= length - encoded.len(),
                "source result continuity/bounds"
            );
            encoded.extend_from_slice(&bytes);
        }
        let Reply::Ticket {
            sequence: got,
            binding,
            blake3: ticket_digest,
            chain,
            ..
        } = self.receive(until, true)?
        else {
            anyhow::bail!("source consumption ticket required");
        };
        ensure!(
            got.0 == sequence
                && binding == self.binding
                && ticket_digest == blake3
                && crate::lightroom::digest(&encoded) == blake3
                && next_chain(&self.chain, sequence, &query_digest, &blake3)? == chain,
            "source result/ticket authority mismatch"
        );
        self.chain = chain;
        self.completed = sequence;
        // Even when cancellation arrived during streaming, consume its complete
        // ticket before returning the cancellation. Keep the process alive.
        self.check()?;
        #[cfg(all(test, feature = "internal-capacity-probes"))]
        crate::capacity_probes::observe(crate::capacity_probes::SOURCE_DECODED, encoded.capacity());
        Value::decode_checked(
            &encoded,
            Expected {
                read: &expected,
                budget: self.budget,
                binding: &self.binding,
            },
            &|| self.health.failed(),
        )
    }
    fn retire(&mut self) -> Result<()> {
        if self.retired {
            return Ok(());
        }
        let until = Instant::now() + Duration::from_secs(5);
        self.send(
            Request::Retire {
                epoch: self.epoch.clone(),
                completed: U64(self.completed),
                chain: self.chain.clone(),
            },
            until,
        )?;
        ensure!(
            matches!(self.receive(until, false)?, Reply::Retired { .. }),
            "source retirement acknowledgement required"
        );
        // Retired means its roster is gone; process terminal still requires reap.
        while self.process.try_reap()?.is_none() {
            ensure!(Instant::now() < until, "source process exit deadline");
            thread::sleep(Duration::from_millis(5));
        }
        self.process.terminate()?;
        self.retired = true;
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        // Scope construction requires this owner outside all consumer SQL and
        // permits. Every error/unwind retains it until that stack has retired.
        let _ = self.retire();
        let _ = self.process.terminate();
    }
}

pub(crate) struct SqlReader {
    session: RefCell<Session>,
    seal: InputSeal,
    binding: String,
    chunk_bytes: usize,
}
impl SqlReader {
    pub(crate) fn open(
        relay: Arc<Client>,
        guard: Guard,
        reader: String,
        seal: InputSeal,
        limits: ReadLimits,
        protected: Vec<FileKey>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        use crate::lightroom::migration_source::{SelectedCapture, SupplementPin};
        use crate::lightroom_migration_worker::memory::layout::{
            add, mul, seal_dynamic, seal_validation,
        };
        limits.validate()?;
        // Counting uses the already-owned seal and allocates no JSON buffer.
        // Clone payload bytes are bounded by its canonical encoded length;
        // repeated struct/Vec roots require their actual member counts too.
        let length = exact_json_length(&seal, crate::lightroom::MANIFEST_BYTES, &cancel)?;
        let members = add(
            mul(seal.selected.len(), std::mem::size_of::<SelectedCapture>())?,
            add(
                mul(seal.excluded_revisions.len(), std::mem::size_of::<String>())?,
                mul(seal.supplements.len(), std::mem::size_of::<SupplementPin>())?,
            )?,
        )?;
        // Source::admit validates one complete selected Manifest at a time
        // while both capture partitions remain live. Admit that opening phase
        // before Open; later reads reuse this per-role producer high water.
        let opening_work = Value::sql_opening_allocation(limits)?;
        relay.admit_producer(Kind::Sql, opening_work)?;
        let opening_graph = add(
            add(length, members)?,
            add(seal_dynamic()?, seal_validation()?)?,
        )?;
        relay.admit_opening(Kind::Sql, opening_graph)?;
        let session = Session::open(
            relay,
            Epoch { guard, reader },
            Authority::Sql {
                seal: seal.clone(),
                limits: limits.into(),
                protected,
            },
            opening_graph,
            cancel,
            limits.open_deadline_ms,
            limits.deadline_ms,
        )?;
        Ok(Self {
            binding: session.binding.clone(),
            session: RefCell::new(session),
            seal,
            chunk_bytes: limits.chunk_bytes,
        })
    }
    pub(crate) fn health(&self) -> Health {
        self.session.borrow().health.clone()
    }
    fn query(&self, query: Query) -> Result<Value> {
        self.session.borrow_mut().query(Read::Sql(query))
    }
}
impl MigrationRead for SqlReader {
    fn admit_retention(&self, cursor_bytes: usize) -> Result<()> {
        self.session.borrow().process.admit_core(
            crate::lightroom_migration_worker::memory::core::retention(cursor_bytes)?,
        )
    }

    fn admit_file_metadata(&self, bytes: usize) -> Result<()> {
        self.session.borrow().process.admit_core(bytes)
    }

    fn seal(&self) -> &InputSeal {
        &self.seal
    }
    fn binding_blake3(&self) -> &str {
        &self.binding
    }
    fn max_chunk_bytes(&self) -> usize {
        self.chunk_bytes
    }
    fn capture_manifest(&self, revision: &str) -> Result<Manifest> {
        match self.query(Query::CaptureManifest {
            revision: revision.into(),
        })? {
            Value::Manifest(v) => Ok(*v),
            _ => anyhow::bail!("source manifest reply kind"),
        }
    }
    fn stable_source(&self, revision: &str, source_id: &str) -> Result<StableSource> {
        match self.query(Query::StableSource {
            revision: revision.into(),
            source_id: source_id.into(),
        })? {
            Value::StableSource(v) => Ok(v),
            _ => anyhow::bail!("source stable reply kind"),
        }
    }
    fn origin_packet_roster(
        &self,
        revision: &str,
        source_id: &str,
        origin: &str,
    ) -> Result<Vec<i64>> {
        match self.query(Query::OriginPacketRoster {
            revision: revision.into(),
            source_id: source_id.into(),
            origin: origin.into(),
        })? {
            Value::OriginPacketRoster(v) => Ok(v.into_iter().map(|v| v.0).collect()),
            _ => anyhow::bail!("source packet roster reply kind"),
        }
    }
    fn page(
        &self,
        revision: &str,
        collection: Collection,
        after: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page> {
        match self.query(Query::Page {
            revision: revision.into(),
            collection,
            after: after.cloned(),
            limit: U64(limit as u64),
        })? {
            Value::Page(v) => Ok(v),
            _ => anyhow::bail!("source page reply kind"),
        }
    }
    fn read_chunk(&self, reference: &ByteRef, offset: u64, limit: usize) -> Result<Vec<u8>> {
        match self.query(Query::ReadChunk {
            reference: reference.clone(),
            offset: U64(offset),
            limit: U64(limit as u64),
        })? {
            Value::Chunk(v) => Ok(v),
            _ => anyhow::bail!("source chunk reply kind"),
        }
    }
    fn count(&self, revision: &str, collection: Collection) -> Result<u64> {
        match self.query(Query::Count {
            revision: revision.into(),
            collection,
        })? {
            Value::Count(v) => Ok(v.0),
            _ => anyhow::bail!("source count reply kind"),
        }
    }
    fn resolve(
        &self,
        revision: &str,
        source_id: &str,
        field: &str,
        target_table: &str,
    ) -> Result<Resolution> {
        match self.query(Query::Resolve {
            revision: revision.into(),
            source_id: source_id.into(),
            field: field.into(),
            target_table: target_table.into(),
        })? {
            Value::Resolution(v) => Ok(v),
            _ => anyhow::bail!("source resolution reply kind"),
        }
    }
    fn image_links(&self, revision: &str, source_id: &str) -> Result<ImageLinks> {
        match self.query(Query::ImageLinks {
            revision: revision.into(),
            source_id: source_id.into(),
        })? {
            Value::ImageLinks(v) => Ok(v),
            _ => anyhow::bail!("source image links reply kind"),
        }
    }
}

pub(crate) struct RawReader {
    session: Session,
    descriptor: ArtifactDescriptor,
    encoded: Vec<u8>,
    ticket: Option<super::commit::RawTicket>,
}
impl RawReader {
    pub(crate) fn open(
        relay: Arc<Client>,
        guard: Guard,
        reader: String,
        descriptor: ArtifactDescriptor,
        limits: ArtifactLimits,
        protected: Vec<FileKey>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        limits.validate()?;
        let length = exact_json_length(&descriptor, 64 * 1024, &cancel)?;
        // Descriptor clone has four native unit vectors and strings, all paid
        // by canonical bytes. Preserve the separate retained encoded descriptor.
        let opening_graph = crate::lightroom_migration_worker::memory::layout::mul(3, length)?;
        relay.admit_opening(Kind::Raw, opening_graph)?;
        let encoded = exact_json(&descriptor, 64 * 1024, &cancel)?;
        let session = Session::open(
            relay,
            Epoch { guard, reader },
            Authority::Artifact {
                descriptor: descriptor.clone(),
                limits: limits.into(),
                protected,
            },
            opening_graph,
            cancel,
            limits.open_deadline_ms,
            limits.chunk_deadline_ms,
        )?;
        Ok(Self {
            session,
            descriptor,
            encoded,
            ticket: None,
        })
    }
    pub(crate) fn health(&self) -> Health {
        self.session.health.clone()
    }
    pub(crate) fn attach(&mut self, health: &Arc<super::CommitHealth>) -> Result<()> {
        ensure!(self.ticket.is_none(), "raw source already attached");
        self.ticket = Some(health.attach_raw(self.health())?);
        Ok(())
    }
}
impl Drop for RawReader {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            let healthy = !self.session.health.failed();
            if self.session.retire().is_ok() && healthy {
                ticket.finish();
            }
            // Otherwise RawTicket drop poisons the operation; never silently
            // replace a failed source with a new epoch and continue.
        }
    }
}
impl ArtifactRead for RawReader {
    fn descriptor(&self) -> &ArtifactDescriptor {
        &self.descriptor
    }
    fn encoded(&self) -> &[u8] {
        &self.encoded
    }
    fn length(&self) -> u64 {
        self.descriptor.artifact.revision.bytes
    }
    fn verify(&mut self) -> Result<()> {
        match self.session.query(Read::ArtifactVerify)? {
            Value::Verified => Ok(()),
            _ => anyhow::bail!("artifact verify reply kind"),
        }
    }
    fn chunk(&mut self, offset: u64, stop: &dyn Fn() -> bool) -> Result<Vec<u8>> {
        ensure!(!stop(), "artifact custody stopped");
        match self.session.query(Read::ArtifactChunk {
            offset: U64(offset),
        })? {
            Value::Chunk(v) => {
                ensure!(!stop(), "artifact custody stopped");
                Ok(v)
            }
            _ => anyhow::bail!("artifact chunk reply kind"),
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod budget_tests;
