//! Operation-scoped remote source ownership. All filesystem work stays in the
//! child; this owner retains its process until the caller's SQL/permit stack has
//! drained. It is never stored on or joined by the foreground actor.
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
        process::{Output, Process, Stop},
        protocol::Guard,
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    path::Path,
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
struct Session {
    process: Process<Reply>,
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
}
impl Session {
    fn open(
        executable: &Path,
        epoch: Epoch,
        authority: Authority,
        cancel: Arc<AtomicBool>,
        open_ms: u64,
        read_ms: u64,
    ) -> Result<Self> {
        epoch.validate()?;
        let budget = Budget::from_authority(&authority)?;
        let encoded = exact_json(&authority, AUTHORITY_BYTES, &cancel)?;
        let binding = authority.binding()?;
        let process_stop = Arc::new(Stop::default());
        let process = Process::spawn_role(
            executable,
            "--lightroom-source-reader",
            process_stop.clone(),
        )?;
        Self::admit(
            process,
            process_stop,
            epoch,
            binding,
            encoded,
            cancel,
            open_ms,
            read_ms,
            budget,
        )
    }
    fn admit(
        process: Process<Reply>,
        process_stop: Arc<Stop>,
        epoch: Epoch,
        binding: String,
        encoded: Vec<u8>,
        cancel: Arc<AtomicBool>,
        open_ms: u64,
        read_ms: u64,
        budget: Budget,
    ) -> Result<Self> {
        ensure!(
            (1..=3_600_000).contains(&open_ms) && (1..=120_000).contains(&read_ms),
            "source process deadline bounds"
        );
        let until = Instant::now() + Duration::from_millis(open_ms);
        let mut session = Self {
            process,
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
        let Reply::Ready { binding, .. } = session.receive(until, true)? else {
            anyhow::bail!("source Ready required");
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
        let until = Instant::now() + self.deadline;
        let sequence = self.next;
        self.next = self
            .next
            .checked_add(1)
            .context("source sequence exhausted")?;
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
        let mut encoded = Vec::with_capacity(length);
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
        self.process.terminate();
        self.retired = true;
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        // Scope construction requires this owner outside all consumer SQL and
        // permits. Every error/unwind retains it until that stack has retired.
        let _ = self.retire();
        self.process.terminate();
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
        executable: &Path,
        guard: Guard,
        reader: String,
        seal: InputSeal,
        limits: ReadLimits,
        protected: Vec<FileKey>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        let session = Session::open(
            executable,
            Epoch { guard, reader },
            Authority::Sql {
                seal: seal.clone(),
                limits: limits.into(),
                protected,
            },
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
            Value::Manifest(v) => Ok(v),
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
        executable: &Path,
        guard: Guard,
        reader: String,
        descriptor: ArtifactDescriptor,
        limits: ArtifactLimits,
        protected: Vec<FileKey>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        let encoded = crate::lightroom::bounded_json(&descriptor, 64 * 1024)?;
        let session = Session::open(
            executable,
            Epoch { guard, reader },
            Authority::Artifact {
                descriptor: descriptor.clone(),
                limits: limits.into(),
                protected,
            },
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
