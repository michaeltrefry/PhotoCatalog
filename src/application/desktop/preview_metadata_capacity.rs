//! Checked requested-backing assembly for managed preview metadata in C.
//!
//! This is not an allocator or RSS ceiling. It excludes encoded/RGB image
//! payloads, native/codec reservations, caller-held decoded pixel allocations,
//! SQLite/native caches, allocator metadata, TLS and thread stacks. The existing
//! `working_bytes` policy remains the native-work admission and is not reduced.

use super::filesystem::{Body, Call, Packet};
use crate::{
    application::{Config, U64},
    catalog_edits::EditRenderIdentity,
    catalog_metadata::RenderIdentity,
    catalog_session::{PATH_UNITS, native},
    preview::{
        ByteReservation, CacheReadMetrics, Consumer, JobState, PreviewKey, PreviewService,
        PreviewView, Priority, ReadCompletion, RenderRecord, RenderWork, RetainedPixels,
        ServiceCompletion, WorkLease, WorkerProcess,
    },
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{HashMap, VecDeque},
    mem::{align_of, size_of},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Instant,
};

const SAVED_DESCRIPTOR_BYTES: u64 = 64 * 1024;
const FAILURE_CHAR_BYTES: u64 = 4 * 4096;
const FAILURE_BYTES: u64 = 4096;
const RELAY_BYTES: u64 = 1024 * 1024;
const CHUNK_BYTES: u64 = 16 * 1024;
const PACKET_ARC_ALLOCATIONS: u64 = 30;
const LEASE_ID_BYTES: u64 = 36;

#[derive(Clone, Copy)]
struct Layout {
    size: u64,
    align: u64,
}
impl Layout {
    fn of<T>() -> Self {
        Self {
            size: size_of::<T>() as u64,
            align: align_of::<T>() as u64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Retained,
    Active,
    Startup,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Contribution {
    pub name: &'static str,
    pub phase: Phase,
    pub count: u64,
    pub each: u64,
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub retained: u64,
    pub active: u64,
    pub startup: u64,
    pub requested: u64,
    pub contributions: Vec<Contribution>,
}

#[derive(Clone, Copy)]
struct Checked;
impl Checked {
    fn value(self, value: u128) -> Result<u64> {
        ensure!(
            value <= isize::MAX as u128 && value <= u64::MAX as u128,
            "preview metadata requested capacity exceeds target isize"
        );
        Ok(value as u64)
    }
    fn add(self, values: &[u64]) -> Result<u64> {
        self.value(values.iter().try_fold(0u128, |sum, value| {
            sum.checked_add(u128::from(*value))
                .context("preview metadata addition overflow")
        })?)
    }
    fn mul(self, left: u64, right: u64) -> Result<u64> {
        self.value(
            u128::from(left)
                .checked_mul(u128::from(right))
                .context("preview metadata multiplication overflow")?,
        )
    }
    fn align(self, value: u64, alignment: u64) -> Result<u64> {
        ensure!(
            alignment.is_power_of_two(),
            "invalid metadata layout alignment"
        );
        let added = self.add(&[value, alignment - 1])?;
        Ok(added & !(alignment - 1))
    }
    fn layout_upper(self, fields: &[Layout]) -> Result<Layout> {
        let align = fields.iter().map(|field| field.align).max().unwrap_or(1);
        let mut size = 0;
        // Rounding every field to the aggregate alignment is independent of the
        // compiler's permitted field order and is an upper bound on padding.
        for field in fields {
            size = self.add(&[size, self.align(field.size, align)?])?;
        }
        Ok(Layout {
            size: self.align(size, align)?,
            align,
        })
    }
    fn tuple(self, left: Layout, right: Layout) -> Result<Layout> {
        self.layout_upper(&[left, right])
    }
    fn vec(self, element: u64, potential: u64) -> Result<u64> {
        self.mul(self.add(&[self.mul(2, potential)?, 8])?, element)
    }
    fn vec_growth(self, element: u64, potential: u64) -> Result<u64> {
        self.mul(self.add(&[self.mul(3, potential)?, 8])?, element)
    }
    fn table(self, item: Layout, demand: u64) -> Result<u64> {
        if demand == 0 {
            return Ok(0);
        }
        let scaled = self.mul(16, demand)? / 7;
        let minimum = 16u64.max(scaled);
        let buckets = minimum
            .checked_next_power_of_two()
            .context("preview metadata hash bucket overflow")?;
        let alignment = 16u64.max(item.align);
        let one = self.add(&[
            self.align(self.mul(buckets, item.size)?, alignment)?,
            buckets,
            16,
        ])?;
        // A long-lived table may retain old and replacement blocks together.
        self.mul(2, one)
    }
    fn arc(self, value: Layout) -> Result<u64> {
        self.layout_upper(&[Layout::of::<usize>(), Layout::of::<usize>(), value])
            .map(|layout| layout.size)
    }
    fn content(self, raw: u64) -> Result<(u64, u64)> {
        let nodes = self.add(&[raw, 1])? / 2;
        let content = Layout::of::<serde::__private229::de::Content<'static>>().size;
        let pair = Layout::of::<(
            serde::__private229::de::Content<'static>,
            serde::__private229::de::Content<'static>,
        )>()
        .size;
        let per_node = self.add(&[
            self.mul(2, content)?.max(pair),
            self.mul(8, content.max(pair))?,
        ])?;
        let backing = self.mul(nodes, per_node)?;
        Ok((backing, self.add(&[content, backing, raw])?))
    }
    fn parse(self, raw: u64, layers: u64, partial_typed: u64) -> Result<u64> {
        let (old_backing, graph) = self.content(raw)?;
        self.add(&[
            raw,
            self.mul(layers, graph)?,
            old_backing,
            self.mul(3, raw)?,
            8,
            partial_typed,
        ])
    }
}

struct Assembly {
    checked: Checked,
    contributions: Vec<Contribution>,
}
impl Assembly {
    fn new() -> Self {
        Self {
            checked: Checked,
            contributions: Vec::new(),
        }
    }
    fn push(&mut self, name: &'static str, phase: Phase, count: u64, each: u64) -> Result<()> {
        ensure!(
            !self.contributions.iter().any(|entry| entry.name == name),
            "duplicate preview metadata owner group"
        );
        let total = self.checked.mul(count, each)?;
        self.contributions.push(Contribution {
            name,
            phase,
            count,
            each,
            total,
        });
        Ok(())
    }
    fn finish(self) -> Result<Report> {
        let sum = |phase| {
            self.checked.add(
                &self
                    .contributions
                    .iter()
                    .filter(|entry| entry.phase == phase)
                    .map(|entry| entry.total)
                    .collect::<Vec<_>>(),
            )
        };
        let retained = sum(Phase::Retained)?;
        let active = sum(Phase::Active)?;
        let startup = sum(Phase::Startup)?;
        let requested = self.checked.add(&[retained, active.max(startup)])?;
        Ok(Report {
            retained,
            active,
            startup,
            requested,
            contributions: self.contributions,
        })
    }
}

fn work_backing(c: Checked, raw: u64, cloned: bool) -> Result<u64> {
    let potential_path_units = raw / 2;
    let paths = if cloned {
        c.mul(3, c.mul(2, potential_path_units)?)?
    } else {
        c.mul(3, c.vec(2, potential_path_units)?)?
    };
    let keys = if cloned {
        c.mul(2, Layout::of::<PreviewKey>().size)?
    } else {
        c.vec(Layout::of::<PreviewKey>().size, 2)?
    };
    // All owned String payloads together fit inside the admitted complete JSON.
    c.add(&[raw, paths, keys])
}

fn work_graph(c: Checked, raw: u64, cloned: bool) -> Result<u64> {
    c.add(&[
        Layout::of::<RenderWork>().size,
        work_backing(c, raw, cloned)?,
    ])
}

fn record_graph(c: Checked) -> Result<u64> {
    c.add(&[
        Layout::of::<RenderRecord>().size,
        SAVED_DESCRIPTOR_BYTES,
        c.vec(
            Layout::of::<String>().size,
            (SAVED_DESCRIPTOR_BYTES + 1) / 3,
        )?,
    ])
}

fn preview_key_backing(c: Checked) -> Result<u64> {
    c.add(&[
        256,
        256,
        64,
        128,
        u64::try_from(crate::preview::PREPARATION_VERSION.len())?,
    ])
}

fn wire_typed_graph(c: Checked, raw: u64) -> Result<u64> {
    // Packet alternatives may reach RenderWork before semantic validation. One
    // JSON numeric path can consume raw/2 units; three independent NativePath
    // fields and the keys Vec are charged separately, as in the reviewed walk.
    c.add(&[
        Layout::of::<Packet>().size,
        u64::try_from(super::filesystem::maximum_boxed_call_root_bytes())?,
        raw,
        c.vec_growth(1, raw)?,
        c.mul(3, c.vec_growth(2, raw / 2)?)?,
        c.vec_growth(Layout::of::<PreviewKey>().size, raw / 2)?,
    ])
}

fn source_layouts(c: Checked) -> Result<SourceLayouts> {
    let saved_job = c.layout_upper(&[
        Layout::of::<RenderWork>(),
        Layout::of::<RenderIdentity>(),
        Layout::of::<Option<EditRenderIdentity>>(),
        Layout::of::<Option<EditRenderIdentity>>(),
        Layout::of::<Option<EditRenderIdentity>>(),
        Layout::of::<bool>(),
        Layout::of::<JobState>(),
    ])?;
    let active_job = c.layout_upper(&[
        Layout::of::<WorkLease>(),
        Layout::of::<WorkerProcess>(),
        Layout::of::<ByteReservation>(),
    ])?;
    let native_read_request =
        c.layout_upper(&[Layout::of::<u64>(), Layout::of::<Arc<AtomicBool>>()])?;
    let pending_read = c.layout_upper(&[
        Layout::of::<EditRenderIdentity>(),
        // Option's discriminant is conservatively represented by one usize.
        c.layout_upper(&[native_read_request, Layout::of::<usize>()])?,
        Layout::of::<bool>(),
        Layout::of::<crate::preview::Tier>(),
        Layout::of::<bool>(),
        Layout::of::<Priority>(),
        Layout::of::<Instant>(),
        Layout::of::<Option<Instant>>(),
        Layout::of::<f64>(),
        Layout::of::<CacheReadMetrics>(),
    ])?;
    let scheduler_job = c.layout_upper(&[
        Layout::of::<String>(),
        Layout::of::<u64>(),
        Layout::of::<HashMap<Consumer, Priority>>(),
        Layout::of::<bool>(),
        Layout::of::<Arc<AtomicBool>>(),
        Layout::of::<bool>(),
        Layout::of::<Option<u64>>(),
    ])?;
    let native_read = c.layout_upper(&[
        Layout::of::<Priority>(),
        Layout::of::<u64>(),
        Layout::of::<Option<u64>>(),
        Layout::of::<Arc<AtomicBool>>(),
    ])?;
    let prepared_reference = Layout::of::<crate::preview::prepared_cache::PreparedReference>();
    let prepared_entry = c.layout_upper(&[prepared_reference, Layout::of::<u64>()])?;
    Ok(SourceLayouts {
        service_job: c.tuple(Layout::of::<String>(), saved_job)?,
        service_active: c.tuple(Layout::of::<u64>(), active_job)?,
        service_consumer: c.tuple(Layout::of::<Consumer>(), Layout::of::<String>())?,
        service_completion: c.tuple(Layout::of::<Consumer>(), Layout::of::<ServiceCompletion>())?,
        read_pending: c.tuple(Layout::of::<crate::preview::ReadTicket>(), pending_read)?,
        read_completed: c.tuple(
            Layout::of::<crate::preview::ReadTicket>(),
            Layout::of::<ReadCompletion>(),
        )?,
        scheduler_job: c.tuple(Layout::of::<u64>(), scheduler_job)?,
        scheduler_key: c.tuple(Layout::of::<String>(), Layout::of::<u64>())?,
        scheduler_consumer: c.tuple(Layout::of::<Consumer>(), Layout::of::<u64>())?,
        scheduler_external: c.tuple(Layout::of::<u64>(), native_read)?,
        scheduler_job_consumer: c.tuple(Layout::of::<Consumer>(), Layout::of::<Priority>())?,
        decoded_entry: c.tuple(
            Layout::of::<String>(),
            Layout::of::<(Arc<RetainedPixels>, u64)>(),
        )?,
        prepared_entry: c.tuple(Layout::of::<String>(), prepared_entry)?,
    })
}

struct SourceLayouts {
    service_job: Layout,
    service_active: Layout,
    service_consumer: Layout,
    service_completion: Layout,
    read_pending: Layout,
    read_completed: Layout,
    scheduler_job: Layout,
    scheduler_key: Layout,
    scheduler_consumer: Layout,
    scheduler_external: Layout,
    scheduler_job_consumer: Layout,
    decoded_entry: Layout,
    prepared_entry: Layout,
}

fn add_table(
    assembly: &mut Assembly,
    name: &'static str,
    layout: Layout,
    demand: u64,
) -> Result<()> {
    assembly.push(
        name,
        Phase::Retained,
        1,
        assembly.checked.table(layout, demand)?,
    )
}

pub(crate) fn report(config: &Config) -> Result<Report> {
    let c = Checked;
    let limits = &config.preview_limits;
    let n = u64::try_from(limits.requests)?;
    let w = u64::try_from(limits.workers)?;
    let d = u64::try_from(limits.decoded_entries)?;
    let p = u64::try_from(limits.prepared_cache_entries)?;
    let jobs = c.add(&[n, w])?;
    let active_fs = c.add(&[w, 1])?;
    let q_jobs = c.add(&[jobs, 1])?;
    let q_requests = c.add(&[n, 1])?;
    let layouts = source_layouts(c)?;
    let mut a = Assembly::new();

    a.push(
        "service.fixed_owned_root",
        Phase::Retained,
        1,
        Layout::of::<PreviewService>().size,
    )?;
    add_table(&mut a, "service.jobs.table", layouts.service_job, q_jobs)?;
    add_table(
        &mut a,
        "service.active.table",
        layouts.service_active,
        c.add(&[w, 1])?,
    )?;
    add_table(
        &mut a,
        "service.consumers.table",
        layouts.service_consumer,
        q_requests,
    )?;
    add_table(
        &mut a,
        "service.completed.table",
        layouts.service_completion,
        q_requests,
    )?;
    add_table(
        &mut a,
        "reads.pending.table",
        layouts.read_pending,
        q_requests,
    )?;
    add_table(
        &mut a,
        "reads.completed.table",
        layouts.read_completed,
        q_requests,
    )?;
    add_table(
        &mut a,
        "scheduler.jobs.table",
        layouts.scheduler_job,
        q_jobs,
    )?;
    add_table(
        &mut a,
        "scheduler.keys.table",
        layouts.scheduler_key,
        q_jobs,
    )?;
    add_table(
        &mut a,
        "scheduler.consumers.table",
        layouts.scheduler_consumer,
        q_requests,
    )?;
    add_table(
        &mut a,
        "scheduler.external.table",
        layouts.scheduler_external,
        q_requests,
    )?;
    a.push(
        "scheduler.each_job_consumers.table",
        Phase::Retained,
        jobs,
        c.table(layouts.scheduler_job_consumer, q_requests)?,
    )?;
    add_table(
        &mut a,
        "decoded.entries.table",
        layouts.decoded_entry,
        if d == 0 { 0 } else { c.add(&[d, 1])? },
    )?;
    add_table(
        &mut a,
        "prepared.entries.table",
        layouts.prepared_entry,
        if p == 0 { 0 } else { c.add(&[p, 1])? },
    )?;
    add_table(
        &mut a,
        "store.touches.table",
        c.tuple(Layout::of::<String>(), Layout::of::<i64>())?,
        257,
    )?;

    a.push(
        "service.jobs.saved_descriptor_backings",
        Phase::Retained,
        jobs,
        c.add(&[
            64,
            work_backing(c, SAVED_DESCRIPTOR_BYTES, false)?,
            SAVED_DESCRIPTOR_BYTES,
        ])?,
    )?;
    a.push("service.consumer_digest_backings", Phase::Retained, n, 64)?;
    a.push(
        "service.completion_failure_backings",
        Phase::Retained,
        n,
        FAILURE_CHAR_BYTES,
    )?;
    a.push(
        "reads.pending_identity_backings",
        Phase::Retained,
        n,
        SAVED_DESCRIPTOR_BYTES,
    )?;
    a.push(
        "reads.completed_view_backings",
        Phase::Retained,
        n,
        c.add(&[
            Layout::of::<PreviewView>().size,
            preview_key_backing(c)?,
            64,
            record_graph(c)?,
        ])?,
    )?;
    a.push(
        "reads.completed_failure_backings",
        Phase::Retained,
        n,
        FAILURE_BYTES,
    )?;
    a.push("scheduler.job_key_backings", Phase::Retained, jobs, 64)?;
    a.push(
        "scheduler.cancel_arc_backings",
        Phase::Retained,
        c.add(&[jobs, n])?,
        c.arc(Layout::of::<AtomicBool>())?,
    )?;
    a.push("decoded.key_backings", Phase::Retained, d, 64)?;
    a.push("store.touch_key_backings", Phase::Retained, 256, 64)?;
    a.push(
        "decoded.cache_and_completed_pixels_arc_roots",
        Phase::Retained,
        c.add(&[d, n])?,
        c.arc(Layout::of::<RetainedPixels>())?,
    )?;
    a.push(
        "prepared.key_and_reference_backings",
        Phase::Retained,
        p,
        c.add(&[
            64,
            SAVED_DESCRIPTOR_BYTES,
            c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
        ])?,
    )?;
    a.push(
        "service.tick_active_ids_vec",
        Phase::Active,
        1,
        c.vec_growth(Layout::of::<u64>().size, w)?,
    )?;
    a.push(
        "scheduler.finished_consumers_vec",
        Phase::Active,
        1,
        c.vec_growth(Layout::of::<Consumer>().size, n)?,
    )?;
    a.push(
        "store.saved_job_page_graph",
        Phase::Active,
        1,
        c.add(&[
            c.vec_growth(Layout::of::<(i64, String, String)>().size, 1000)?,
            c.mul(1000, c.add(&[64, SAVED_DESCRIPTOR_BYTES])?)?,
        ])?,
    )?;
    a.push(
        "store.saved_job_view_page_graph",
        Phase::Active,
        1,
        c.add(&[
            c.vec_growth(Layout::of::<crate::preview::JobView>().size, 1000)?,
            c.mul(1000, c.add(&[64, 256, SAVED_DESCRIPTOR_BYTES])?)?,
        ])?,
    )?;
    a.push(
        "store.cache_recovery_row_graph",
        Phase::Active,
        1,
        RELAY_BYTES,
    )?;
    let recovery_key_partial = c.add(&[Layout::of::<PreviewKey>().size, RELAY_BYTES])?;
    a.push(
        "store.cache_recovery_key_parser_without_raw",
        Phase::Active,
        1,
        c.parse(RELAY_BYTES, 2, recovery_key_partial)?
            .checked_sub(RELAY_BYTES)
            .context("cache recovery parse accounting")?,
    )?;

    a.push(
        "active.render_additional_work_graphs",
        Phase::Active,
        c.mul(4, w)?,
        work_graph(c, SAVED_DESCRIPTOR_BYTES, true)?,
    )?;
    a.push(
        "active.read_native_work_graph",
        Phase::Active,
        1,
        work_graph(c, SAVED_DESCRIPTOR_BYTES, true)?,
    )?;
    let [_, job_owner, stage] = crate::preview::metadata_owner_layouts();
    let job_owner = Layout {
        size: u64::try_from(job_owner.0)?,
        align: u64::try_from(job_owner.1)?,
    };
    a.push(
        "active.render_and_read_job_owner_arc_backings",
        Phase::Active,
        active_fs,
        c.arc(job_owner)?,
    )?;
    a.push(
        "active.transport_cancel_arc_backings",
        Phase::Active,
        active_fs,
        c.arc(Layout::of::<AtomicBool>())?,
    )?;
    let managed_read = crate::preview::managed_read_metadata_root_layout();
    let managed_read = Layout {
        size: u64::try_from(managed_read.0)?,
        align: u64::try_from(managed_read.1)?,
    };
    a.push(
        "active.managed_read_box_and_nested_metadata",
        Phase::Active,
        1,
        c.add(&[
            managed_read.size,
            SAVED_DESCRIPTOR_BYTES,
            preview_key_backing(c)?,
            64,
            record_graph(c)?,
            64,
            LEASE_ID_BYTES,
            64,
        ])?,
    )?;
    a.push(
        "active.job_admission_failure_backings",
        Phase::Active,
        active_fs,
        RELAY_BYTES,
    )?;
    a.push(
        "active.job_status_error_backings",
        Phase::Active,
        active_fs,
        crate::catalog_session::native::ERROR_BYTES as u64,
    )?;
    a.push(
        "active.transport_result_error_string_backings",
        Phase::Active,
        active_fs,
        c.mul(2, FAILURE_BYTES)?,
    )?;
    a.push(
        "active.ready_batch_metadata_graphs",
        Phase::Active,
        w,
        c.add(&[
            Layout::of::<crate::preview::RenderedPreviewBatch>().size,
            c.vec(Layout::of::<crate::preview::ProducedPreview>().size, 2)?,
            native::REQUEST_BYTES as u64,
            c.vec(
                Layout::of::<String>().size,
                (native::REQUEST_BYTES as u64 + 1) / 3,
            )?,
            c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
        ])?,
    )?;
    a.push(
        "active.publication_record_clone",
        Phase::Active,
        1,
        record_graph(c)?,
    )?;
    let [receipt, object] = crate::preview::receipt_metadata_layouts();
    let receipt_root = Layout {
        size: u64::try_from(receipt.0)?,
        align: u64::try_from(receipt.1)?,
    };
    let object_root = Layout {
        size: u64::try_from(object.0)?,
        align: u64::try_from(object.1)?,
    };
    let receipt_partial = c.add(&[
        receipt_root.size,
        native::REQUEST_BYTES as u64,
        c.vec_growth(object_root.size, native::REQUEST_BYTES as u64 / 2)?,
        c.vec_growth(
            Layout::of::<String>().size,
            (native::REQUEST_BYTES as u64 + 1) / 3,
        )?,
        c.vec_growth(2, native::REQUEST_BYTES as u64 / 2)?,
    ])?;
    a.push(
        "active.native_receipt_parsers",
        Phase::Active,
        active_fs,
        c.parse(native::REQUEST_BYTES as u64, 2, receipt_partial)?,
    )?;
    let potential = SAVED_DESCRIPTOR_BYTES / 2;
    let saved_job_partial = c.add(&[
        layouts.service_job.size,
        SAVED_DESCRIPTOR_BYTES,
        c.mul(3, c.vec_growth(2, potential)?)?,
        c.vec_growth(Layout::of::<PreviewKey>().size, potential)?,
    ])?;
    let record_partial = c.add(&[
        Layout::of::<RenderRecord>().size,
        SAVED_DESCRIPTOR_BYTES,
        c.vec_growth(
            Layout::of::<String>().size,
            (SAVED_DESCRIPTOR_BYTES + 1) / 3,
        )?,
    ])?;
    let actor_partial = saved_job_partial.max(record_partial);
    a.push(
        "active.actor_saved_descriptor_parser",
        Phase::Active,
        1,
        c.parse(SAVED_DESCRIPTOR_BYTES, 2, actor_partial)?,
    )?;

    let packet_partial = wire_typed_graph(c, RELAY_BYTES)?;
    let data_parse = c.parse(RELAY_BYTES, 6, packet_partial)?;
    // Three assembly/current buffers retain the raw parse input, so subtract the
    // parse formula's one raw owner before adding that exact envelope.
    a.push(
        "relay.incoming_assemblies_and_chunk_copies",
        Phase::Active,
        1,
        c.add(&[c.mul(3, RELAY_BYTES)?, c.mul(2, CHUNK_BYTES)?])?,
    )?;
    a.push(
        "relay.data_packet_content_parse_without_raw",
        Phase::Active,
        1,
        data_parse
            .checked_sub(RELAY_BYTES)
            .context("relay parse accounting")?,
    )?;
    a.push(
        "relay.reserved_control_packet_parse",
        Phase::Active,
        1,
        c.parse(CHUNK_BYTES, 6, wire_typed_graph(c, CHUNK_BYTES)?)?,
    )?;
    a.push(
        "relay.outgoing_packet_backings",
        Phase::Active,
        1,
        c.add(&[c.mul(4, RELAY_BYTES)?, c.mul(26, CHUNK_BYTES)?])?,
    )?;
    a.push(
        "relay.packet_arc_allocation_roots",
        Phase::Active,
        PACKET_ARC_ALLOCATIONS,
        c.arc(Layout::of::<Vec<u8>>())?,
    )?;
    let slot_item = Layout::of::<(u64, Arc<Vec<u8>>)>().size;
    let data_item = Layout::of::<Arc<Vec<u8>>>().size;
    let output_queues = c.add(&[
        c.mul(3, c.vec_growth(slot_item, 2)?)?,
        c.mul(3, c.vec_growth(slot_item, 1)?)?,
        c.vec_growth(slot_item, 16)?,
        c.vec_growth(data_item, 2)?,
    ])?;
    a.push(
        "relay.output_queue_backings",
        Phase::Active,
        1,
        output_queues,
    )?;
    a.push(
        "relay.proxy_accepted_call_graphs",
        Phase::Active,
        4,
        wire_typed_graph(c, RELAY_BYTES)?,
    )?;
    a.push(
        "relay.proxy_result_graphs",
        Phase::Active,
        2,
        wire_typed_graph(c, RELAY_BYTES)?,
    )?;
    a.push(
        "stage.waiting_action_graphs_before_mutex",
        Phase::Active,
        active_fs,
        wire_typed_graph(c, native::REQUEST_BYTES as u64)?,
    )?;
    a.push(
        "stage.shared_pending_and_proposed_requests",
        Phase::Active,
        2,
        wire_typed_graph(c, native::REQUEST_BYTES as u64)?,
    )?;
    a.push(
        "stage.upload_chunk_backings",
        Phase::Active,
        active_fs,
        CHUNK_BYTES,
    )?;
    a.push(
        "relay.query_and_native_status_graphs",
        Phase::Active,
        6,
        wire_typed_graph(c, CHUNK_BYTES)?,
    )?;
    a.push(
        "serialization.native_envelopes",
        Phase::Active,
        active_fs,
        native::REQUEST_BYTES as u64,
    )?;
    a.push(
        "serialization.render_records",
        Phase::Active,
        w,
        SAVED_DESCRIPTOR_BYTES,
    )?;
    a.push(
        "serialization.saved_job",
        Phase::Active,
        1,
        SAVED_DESCRIPTOR_BYTES,
    )?;
    for (name, count) in [
        ("relay.parent_fault_backing", 1),
        ("relay.child_fault_backing", 1),
        ("relay.child_call_fault_backings", 2),
        ("relay.admission_query_fault_backing", 1),
        ("relay.store_query_fault_backing", 1),
        ("relay.native_pending_fault_backing", 1),
        ("relay.native_result_fault_backing", 1),
    ] {
        a.push(name, Phase::Active, count, FAILURE_BYTES)?;
    }

    // Startup/configuration parsing precedes PreviewService construction. Keep
    // it as a phase maximum instead of multiplying it by request/job counts.
    let config_bytes = super::wire::CONFIG_BYTES as u64;
    let path_nodes = c.add(&[config_bytes, 1])? / 2;
    // Across every NativePath in the frame, numeric units sum to at most the
    // byte-derived node count. Each independently empty/growing Vec can also
    // request its eight-element minimum, and there are at most path_nodes
    // such vectors. This retains the per-vector overhead instead of folding all
    // nested paths into one imaginary Vec.
    let nested_native_paths = c.add(&[
        c.mul(c.mul(3, path_nodes)?, Layout::of::<u16>().size)?,
        c.mul(c.mul(8, path_nodes)?, Layout::of::<u16>().size)?,
    ])?;
    let config_partial = c.add(&[
        Layout::of::<super::wire::ConfigWire>().size,
        config_bytes,
        c.vec_growth(Layout::of::<NativePath>().size, path_nodes)?,
        nested_native_paths,
    ])?;
    a.push(
        "startup.config_parser",
        Phase::Startup,
        1,
        c.parse(config_bytes, 3, config_partial)?,
    )?;
    a.push(
        "startup.config_typed_clone",
        Phase::Startup,
        1,
        config_partial,
    )?;

    // Keep these roots explicit. Mutex/channel/thread implementations and their
    // stacks remain runtime storage outside this requested-backing claim.
    let stage_root = Layout {
        size: u64::try_from(stage.0)?,
        align: u64::try_from(stage.1)?,
    };
    a.push(
        "fixed.active_stage_arc_backings",
        Phase::Active,
        active_fs,
        c.arc(stage_root)?,
    )?;
    let [calls, calls_state] = crate::preview::stage_io::metadata_layouts();
    let calls_root = Layout {
        size: u64::try_from(calls.0)?,
        align: u64::try_from(calls.1)?,
    };
    a.push(
        "fixed.stage_calls_arc_backings",
        Phase::Active,
        c.add(&[active_fs, 1])?,
        c.arc(calls_root)?,
    )?;
    a.push(
        "fixed.stage_calls_owned_identity_backings",
        Phase::Active,
        c.add(&[active_fs, 1])?,
        c.add(&[c.mul(5, LEASE_ID_BYTES)?, c.vec(2, PATH_UNITS as u64)?])?,
    )?;
    let calls_state = Layout {
        size: u64::try_from(calls_state.0)?,
        align: u64::try_from(calls_state.1)?,
    };
    a.push(
        "fixed.stage_calls_shared_arc_backings",
        Phase::Active,
        1,
        c.add(&[
            c.mul(2, c.arc(Layout::of::<std::sync::atomic::AtomicU64>())?)?,
            c.arc(Layout::of::<Mutex<Option<U64>>>())?,
            c.arc(calls_state)?,
        ])?,
    )?;
    let proxy = super::filesystem::metadata_proxy_layout();
    let proxy_root = Layout {
        size: u64::try_from(proxy.0)?,
        align: u64::try_from(proxy.1)?,
    };
    a.push(
        "fixed.proxy_arc_backing",
        Phase::Active,
        1,
        c.arc(proxy_root)?,
    )?;
    a.push(
        "fixed.active_job_and_stage_identity_backings",
        Phase::Active,
        active_fs,
        c.add(&[c.mul(8, LEASE_ID_BYTES)?, c.vec(2, PATH_UNITS as u64)?])?,
    )?;
    a.push(
        "fixed.proxy_binding_and_completed_backings",
        Phase::Active,
        1,
        c.add(&[c.mul(2, LEASE_ID_BYTES)?, 64])?,
    )?;
    let budget = crate::preview::budget_state_layout();
    let budget_root = Layout {
        size: u64::try_from(budget.0)?,
        align: u64::try_from(budget.1)?,
    };
    let admission = super::preview_metadata_admission::owned_layout();
    a.push(
        "fixed.metadata_admission_owner_backing",
        Phase::Retained,
        1,
        c.add(&[
            c.arc(Layout { size: u64::try_from(admission.0)?, align: u64::try_from(admission.1)? })?,
            std::mem::size_of::<super::preview_metadata_admission::ProcessReservation>() as u64,
            std::mem::size_of::<Mutex<Option<super::preview_metadata_admission::ProcessReservation>>>() as u64,
        ])?,
    )?;
    a.push(
        "fixed.preview_byte_budget_arc_backings",
        Phase::Retained,
        2,
        c.arc(budget_root)?,
    )?;
    a.push(
        "fixed.service_atomic_arc_backings",
        Phase::Retained,
        1,
        c.add(&[
            c.arc(Layout::of::<std::sync::atomic::AtomicUsize>())?,
            c.mul(3, c.arc(Layout::of::<AtomicBool>())?)?,
        ])?,
    )?;
    a.push(
        "fixed.service_and_store_path_backings",
        Phase::Retained,
        6,
        c.vec(2, PATH_UNITS as u64)?,
    )?;
    a.push(
        "fixed.store_identity_backing",
        Phase::Retained,
        1,
        LEASE_ID_BYTES,
    )?;
    a.push(
        "fixed.proxy_packet_type_roots",
        Phase::Active,
        1,
        c.add(&[
            Layout::of::<Packet>().size,
            Layout::of::<Call>().size,
            Layout::of::<Body>().size,
            Layout::of::<VecDeque<Arc<Vec<u8>>>>().size,
            Layout::of::<HashMap<u64, U64>>().size,
        ])?,
    )?;

    let report = a.finish()?;
    ensure!(report.requested > 0, "empty preview metadata capacity");
    // Layout arithmetic above is tied to this target. Accepted public ranges are
    // still enforced by their owning constructors; this function does not widen
    // any request/cache setting.
    ensure!(
        (1..=100_000).contains(&limits.requests)
            && (1..=16).contains(&limits.workers)
            && (1..=100_000).contains(&limits.decoded_entries)
            && limits.prepared_cache_entries <= 1024,
        "invalid preview metadata owner limits"
    );
    Ok(report)
}

pub(crate) fn requested_bytes(config: &Config) -> Result<u64> {
    Ok(report(config)?.requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            worker_executable: std::path::PathBuf::from("/fixture/worker"),
            cache_root: None,
            original_roots: Vec::new(),
            preview_policy: Default::default(),
            preview_limits: Default::default(),
            limits: Default::default(),
            import_checkpoint: None,
        }
    }

    #[test]
    fn default_and_supported_boundaries_have_complete_named_assemblies() -> Result<()> {
        let mut default = config();
        let default_report = report(&default)?;
        println!(
            "default retained={} active={} startup={} requested={}",
            default_report.retained,
            default_report.active,
            default_report.startup,
            default_report.requested
        );
        assert_eq!(
            default_report.requested,
            default_report
                .retained
                .checked_add(default_report.active.max(default_report.startup))
                .unwrap()
        );
        assert!(default_report.contributions.len() >= 40);
        let mut names = std::collections::HashSet::new();
        assert!(
            default_report
                .contributions
                .iter()
                .all(|entry| names.insert(entry.name))
        );
        default.preview_limits.working_bytes = 1;
        assert_eq!(report(&default)?.requested, default_report.requested);
        let mut maximum = config();
        maximum.preview_limits.requests = 100_000;
        maximum.preview_limits.workers = 16;
        maximum.preview_limits.decoded_entries = 100_000;
        maximum.preview_limits.prepared_cache_entries = 1024;
        let maximum_report = report(&maximum)?;
        println!(
            "maximum retained={} active={} startup={} requested={}",
            maximum_report.retained,
            maximum_report.active,
            maximum_report.startup,
            maximum_report.requested
        );
        assert!(maximum_report.requested > report(&config())?.requested);
        let mut minimum = config();
        minimum.preview_limits.requests = 1;
        minimum.preview_limits.workers = 1;
        minimum.preview_limits.decoded_entries = 1;
        minimum.preview_limits.prepared_cache_entries = 0;
        let minimum_report = report(&minimum)?;
        println!(
            "minimum retained={} active={} startup={} requested={}",
            minimum_report.retained,
            minimum_report.active,
            minimum_report.startup,
            minimum_report.requested
        );
        assert!(minimum_report.requested < report(&config())?.requested);
        Ok(())
    }

    #[test]
    fn checked_arithmetic_rejects_overflow_and_duplicate_owners() {
        let c = Checked;
        assert!(c.mul(isize::MAX as u64, 2).is_err());
        assert!(
            c.vec(Layout::of::<String>().size, isize::MAX as u64)
                .is_err()
        );
        assert!(c.table(Layout::of::<String>(), isize::MAX as u64).is_err());
        let mut assembly = Assembly::new();
        assembly.push("one", Phase::Retained, 1, 1).unwrap();
        assert!(assembly.push("one", Phase::Active, 1, 1).is_err());
    }

    #[test]
    fn owner_formula_keeps_image_and_native_budgets_separate() -> Result<()> {
        let base = config();
        let expected = report(&base)?.requested;
        let mutators: [fn(&mut crate::preview::ServiceLimits); 5] = [
            |limits| limits.working_bytes = 1,
            |limits| limits.encoded_staging_bytes = 1,
            |limits| limits.decoded_cache_bytes = 1,
            |limits| limits.decoded_live_bytes = 1,
            |limits| limits.prepared_cache_bytes = 1,
        ];
        for mutate in mutators {
            let mut changed = base.clone();
            mutate(&mut changed.preview_limits);
            assert_eq!(report(&changed)?.requested, expected);
        }
        Ok(())
    }
}
