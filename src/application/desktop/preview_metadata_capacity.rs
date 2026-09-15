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
    catalog_session::{EXPORT_PROFILE_BYTES, PATH_UNITS, native},
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
// `Source::revision` formats a Unix u64:u64 object and i64:i64 change pair,
// or a Windows u64:u128 object and i64 change value. These are the portable
// maximum decimal characters, including separators.
const SOURCE_REVISION_OBJECT_BYTES: u64 = 20 + 1 + 39;
const SOURCE_REVISION_CHANGED_BYTES: u64 = 20 + 1 + 20;

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
    let (import_owner_size, import_owner_align) = crate::filesystem_worker::import_owner_layout();
    let import_path = c.vec(2, PATH_UNITS as u64)?;
    let import_fact_chunk = c.add(&[
        c.vec_growth(
            Layout::of::<crate::catalog_session::import::DirectoryFact>().size,
            crate::catalog_session::import::DIRECTORY_FACTS as u64,
        )?,
        c.vec(
            2,
            crate::catalog_session::import::DIRECTORY_FACT_PATH_UNITS as u64,
        )?,
    ])?;
    // F retains one bounded inspection envelope until C has committed its exact
    // observation. Directory identities are fixed-width; paths live only in the
    // current source/directory and one bounded fact reply. Exact retry retains
    // one request/reply envelope pair alongside those domain objects.
    a.push(
        "retained.import_f_source_custody_and_transfer",
        Phase::Retained,
        1,
        c.add(&[
            c.align(
                u64::try_from(import_owner_size)?,
                u64::try_from(import_owner_align)?,
            )?,
            c.mul(4, LEASE_ID_BYTES)?,
            c.mul(8, import_path)?,
            c.mul(2, RELAY_BYTES)?,
            c.mul(crate::catalog_session::import::MAX_DIRECTORIES as u64, 128)?,
            import_fact_chunk,
            crate::catalog_session::import::MAX_INSPECTION_BYTES as u64,
        ])?,
    )?;
    // Packet extraction, compact-envelope construction, and its metadata JSON
    // can coexist in F before the source packet/input vectors are released.
    a.push(
        "active.import_f_inspection_parse_and_encoding",
        Phase::Active,
        1,
        c.add(&[
            c.mul(
                2,
                crate::catalog_session::import::MAX_INSPECTION_BYTES as u64,
            )?,
            crate::catalog_session::import::MAX_INSPECTION_METADATA_BYTES as u64,
        ])?,
    )?;
    // At peak F's retained envelope coexists with C's exact assembly, decoded
    // packet/input bytes, and Prepared's copied blob graph. Relay envelopes and
    // a maximum directory fact chunk are charged independently.
    a.push(
        "active.import_c_transfer_decode_and_preparation",
        Phase::Active,
        1,
        c.add(&[
            c.mul(
                3,
                crate::catalog_session::import::MAX_INSPECTION_BYTES as u64,
            )?,
            c.parse(
                crate::catalog_session::import::MAX_INSPECTION_METADATA_BYTES as u64,
                6,
                crate::catalog_session::import::MAX_INSPECTION_METADATA_BYTES as u64,
            )?,
            c.mul(4, RELAY_BYTES)?,
            c.mul(4, import_fact_chunk)?,
            c.mul(8, CHUNK_BYTES)?,
        ])?,
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

    // Private migration uses the same process reservation/shared ByteBudget.
    // One authority request and sixteen recovery requests have independent
    // bounded custody in G/C. Include both encodings, both incoming parsers,
    // queues, complete pins and snapshots; CHUNK only bounds individual frames.
    let migration_slots = super::CONTROL_SLOTS as u64 + 1;
    let migration_request = (config.limits.request_bytes as u64).max(CHUNK_BYTES);
    let migration_reply = (config.limits.reply_bytes as u64).max(CHUNK_BYTES);
    a.push(
        "relay.migration_complete_message_backings",
        Phase::Active,
        1,
        c.add(&[
            c.mul(4, migration_request)?,
            c.mul(3 * super::CONTROL_SLOTS as u64, CHUNK_BYTES)?,
            c.mul(c.add(&[c.mul(2, migration_slots)?, 4])?, migration_reply)?,
            c.mul(4, CHUNK_BYTES)?,
        ])?,
    )?;
    let migration_root = Layout::of::<super::lightroom_migration::Request>()
        .size
        .max(Layout::of::<super::lightroom_migration::Reply>().size);
    // After validation there are at most two NativePaths and bounded strings.
    let migration_typed = c.add(&[
        migration_root,
        // Guard, catalog, target/digest, progress and bounded refusal strings.
        c.vec_growth(1, 3 * 64 + 128 + 2 * 64 + 128 + 1024)?,
        c.mul(2, c.vec_growth(2, PATH_UNITS as u64)?)?,
    ])?;
    let pending_demand = c.add(&[
        config.limits.queued as u64,
        2 * super::CONTROL_SLOTS as u64,
        1,
    ])?;
    let child_pending = super::process::migration_pending_layout();
    a.push(
        "relay.migration_retained_typed_graphs",
        Phase::Active,
        c.add(&[c.mul(3, migration_slots)?, 8])?,
        migration_typed,
    )?;
    // Malformed JSON can allocate before semantic native-unit validation.
    // Each parser retains its complete raw input and bounded serde Content tree.
    for (name, bytes) in [
        ("relay.migration_request_parser", migration_request),
        ("relay.migration_reply_parser", migration_reply),
        ("relay.migration_recovery_classifier", CHUNK_BYTES),
    ] {
        let partial = c.add(&[
            migration_root,
            bytes,
            c.mul(2, c.vec_growth(2, bytes / 2)?)?,
        ])?;
        a.push(name, Phase::Active, 1, c.parse(bytes, 6, partial)?)?;
    }
    a.push(
        "relay.migration_queue_backings",
        Phase::Active,
        1,
        c.add(&[
            c.vec_growth(Layout::of::<super::wire::Message>().size, migration_slots)?,
            c.table(Layout::of::<(u64, super::Entry)>(), pending_demand)?,
            c.table(
                Layout {
                    size: child_pending.0 as u64,
                    align: child_pending.1 as u64,
                },
                pending_demand,
            )?,
            c.table(
                Layout::of::<(u64, super::super::Cancellation)>(),
                pending_demand,
            )?,
            c.table(Layout::of::<u64>(), pending_demand)?,
            c.table(Layout::of::<(u64, bool)>(), migration_slots)?,
            c.vec_growth(Layout::of::<(u64, u64)>().size, pending_demand + 1)?,
        ])?,
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
    // The serial export worker runs directory preparation, destination snapshot,
    // and alias fact queries exclusively. G retains its encoded F Operation while
    // F owns the decoded request; on return, the request coexists with one decoded
    // Response. The C relay Call graph is independently charged above.
    let export_directory_request = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Operation>().size,
        Layout::of::<crate::catalog_session::PrepareExportDirectory>().size,
        c.mul(3, LEASE_ID_BYTES)?,
        c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
    ])?;
    let export_directory_reply = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Response>().size,
        c.mul(3, LEASE_ID_BYTES)?,
        c.mul(3, c.vec(2, PATH_UNITS as u64)?)?,
    ])?;
    let export_snapshot_request = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Operation>().size,
        Layout::of::<crate::catalog_session::ExportDestinationSnapshotRequest>().size,
        c.mul(3, LEASE_ID_BYTES)?,
        c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
    ])?;
    let export_snapshot_reply = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Response>().size,
        c.mul(4, LEASE_ID_BYTES)?,
        c.mul(3, c.vec(2, PATH_UNITS as u64)?)?,
        64,
    ])?;
    let export_alias_request = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Operation>().size,
        Layout::of::<crate::catalog_session::ExportAliasFactRequest>().size,
        c.mul(3, LEASE_ID_BYTES)?,
        c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
    ])?;
    let export_alias_reply = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Response>().size,
        c.mul(3, LEASE_ID_BYTES)?,
        c.mul(3, c.vec(2, PATH_UNITS as u64)?)?,
        c.vec(1, 39)?,
    ])?;
    let inspect_original_request = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Operation>().size,
        Layout::of::<crate::catalog_session::InspectExportOriginal>().size,
        c.mul(3, LEASE_ID_BYTES)?,
        c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
    ])?;
    let inspect_original_reply = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Response>().size,
        c.mul(3, LEASE_ID_BYTES)?,
        c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
        c.vec(1, 64)?,
    ])?;
    let original_lease_request = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Operation>().size,
        Layout::of::<crate::catalog_session::ExportOriginalRequest>().size,
        c.mul(4, LEASE_ID_BYTES)?,
        c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
    ])?;
    let original_lease_reply = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Response>().size,
        c.mul(4, LEASE_ID_BYTES)?,
        c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
        c.vec(1, 64)?,
    ])?;
    // Publication request/reply typed roots and full prevalidation backings.
    // The independent F parser scratch is charged below; these graphs coexist
    // with the retained C Call and active/terminal publication graphs.
    let publication_request = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Operation>().size,
        Layout::of::<crate::catalog_session::ExportPublicationRequest>().size,
        c.vec_growth(1, RELAY_BYTES)?,
        c.mul(2, c.vec_growth(2, RELAY_BYTES / 2)?)?,
    ])?;
    let publication_reply = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Response>().size,
        Layout::of::<crate::catalog_session::ExportPublicationReply>().size,
        c.vec_growth(1, RELAY_BYTES)?,
        c.mul(5, c.vec_growth(2, RELAY_BYTES / 2)?)?,
    ])?;
    let export_typed_graph = [
        c.add(&[
            export_directory_request,
            export_directory_request.max(export_directory_reply),
        ])?,
        c.add(&[
            export_snapshot_request,
            export_snapshot_request.max(export_snapshot_reply),
        ])?,
        c.add(&[
            export_alias_request,
            export_alias_request.max(export_alias_reply),
        ])?,
        c.add(&[
            inspect_original_request,
            inspect_original_request.max(inspect_original_reply),
        ])?,
        c.add(&[
            original_lease_request,
            original_lease_request.max(original_lease_reply),
        ])?,
        c.add(&[
            publication_request,
            publication_request.max(publication_reply),
        ])?,
    ]
    .into_iter()
    .max()
    .unwrap();
    a.push(
        "active.export_destination_f_typed_graph_envelope",
        Phase::Active,
        1,
        export_typed_graph,
    )?;
    // CatalogSessionAuthority retains its original request while Proxy owns an
    // independent deep clone in the C relay Call charged above. Only the
    // original request's separately allocated backings belong here; its inline
    // stack root is not an allocation.
    a.push(
        "active.export_destination_c_caller_request_backing",
        Phase::Active,
        1,
        c.add(&[
            c.mul(3, LEASE_ID_BYTES)?,
            c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
        ])?,
    )?;
    // The snapshot result remains live while append runs alias validation.
    a.push(
        "active.export_destination_snapshot_result_backing",
        Phase::Active,
        1,
        c.add(&[LEASE_ID_BYTES, c.vec(2, PATH_UNITS as u64)?, 64])?,
    )?;
    // Snapshot construction reuses metadata_export's durable 64 KiB plan and
    // receipt validation. Before an oversized path is rejected, receipt serde
    // may repeat the request-bounded destination in three path fields. Four
    // relay envelopes therefore bound that output, including the 8 KiB detail
    // and derived-name syntax. Fourteen path vectors name the request-derived
    // local/snapshot paths (2), validation plan/recovery paths (2), receipt
    // paths (3), serde clone/native conversions (6), and the C reply validator's
    // bounded NativePath conversion (1). Only one serializer output lives at a
    // time.
    let export_derived_path_units = c.add(&[PATH_UNITS as u64, 64])?;
    let export_snapshot_validation_scratch = c.add(&[
        c.mul(14, c.vec(2, export_derived_path_units)?)?,
        c.mul(2, c.vec(1, 8192)?)?,
        c.mul(4, c.vec(1, 64)?)?,
        c.vec_growth(1, c.mul(4, RELAY_BYTES)?)?,
    ])?;
    a.push(
        "active.export_destination_resolution_and_validation_scratch",
        Phase::Active,
        1,
        export_snapshot_validation_scratch,
    )?;
    // The alias phase retains the destination NativePath and its projected
    // parent/ASCII-prefix/filename strings. One admitted 256 KiB directory row
    // can remain live while a second 256 KiB candidate row, their parsed paths,
    // and a fact request coexist. The fact request is charged by the caller term
    // above. The five paths are destination, projected parent, current parent,
    // indexed directory and candidate. Projection construction has two more
    // temporary path-unit vectors but no SQL rows, so this query phase is the
    // maximum under the same Vec-capacity rule.
    let alias_projection_and_candidate = c.add(&[
        c.mul(2, 256 * 1024)?,
        c.mul(5, c.vec(2, PATH_UNITS as u64)?)?,
        c.mul(3, c.vec(1, c.add(&[PATH_UNITS as u64, 4])?)?)?,
        c.vec(1, 39)?,
    ])?;
    // Exact source exclusion runs after the alias phase. It retains original,
    // canonical-original and destination NativePaths plus the destination's
    // bounded serialized SQL key. The managed snapshot was already admitted by
    // the 64 KiB durable-plan contract in F.
    let exact_source_exclusion = c.add(&[
        c.vec(1, 64 * 1024)?,
        c.mul(3, c.vec(2, PATH_UNITS as u64)?)?,
        c.vec(1, 39)?,
        c.vec(1, 60)?,
    ])?;
    a.push(
        "active.export_alias_projection_candidate_and_source_backing_envelope",
        Phase::Active,
        1,
        alias_projection_and_candidate.max(exact_source_exclusion),
    )?;
    // The export actor is serial: one planning inspection or one held-original
    // operation is active. C records custody before Begin dispatch. F retains
    // one VerifiedFile through every existing SQL/publication recheck, plus one
    // terminal record while constructing the next candidate after a lost ack.
    let (custody_size, custody_align) = crate::catalog_session::export_original_custody_layout();
    a.push(
        "retained.export_original_c_custody_arc_root",
        Phase::Retained,
        1,
        c.arc(Layout {
            size: u64::try_from(custody_size)?,
            align: u64::try_from(custody_align)?,
        })?,
    )?;
    a.push(
        "active.export_original_c_custody_and_lease_backings",
        Phase::Active,
        1,
        c.add(&[
            c.mul(5, LEASE_ID_BYTES)?,
            c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
            c.vec(1, 64)?,
        ])?,
    )?;
    a.push(
        "active.export_original_c_caller_request_backing",
        Phase::Active,
        1,
        c.add(&[
            c.mul(4, LEASE_ID_BYTES)?,
            c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
        ])?,
    )?;
    let (original_transfer_size, _) = crate::filesystem_worker::export_original_transfer_layout();
    a.push(
        "active.export_original_f_active_terminal_overlap",
        Phase::Active,
        1,
        c.add(&[
            u64::try_from(original_transfer_size)?,
            c.mul(2, LEASE_ID_BYTES)?,
            c.mul(3, c.vec(2, PATH_UNITS as u64)?)?,
            c.vec(1, 64)?,
        ])?,
    )?;
    a.push(
        "active.export_original_f_begin_path_backings",
        Phase::Active,
        1,
        c.mul(5, c.vec(2, PATH_UNITS as u64)?)?,
    )?;
    let original_root_nodes = c.add(&[super::wire::CONFIG_BYTES as u64, 1])? / 2;
    a.push(
        "retained.export_original_f_admitted_root_backings",
        Phase::Retained,
        1,
        c.add(&[
            c.vec_growth(Layout::of::<NativePath>().size, original_root_nodes)?,
            c.mul(c.mul(3, original_root_nodes)?, Layout::of::<u16>().size)?,
            c.mul(c.mul(8, original_root_nodes)?, Layout::of::<u16>().size)?,
        ])?,
    )?;
    // Publication is additive with the original lease, including uncertain
    // joint cleanup. A seal owns destination plus UUID/expected digest/authority
    // digest/payload digest. Root paths belong to each cloned RootCapability.
    let publication_path = c.vec(2, PATH_UNITS as u64)?;
    let publication_derived_path = c.vec(2, c.add(&[PATH_UNITS as u64, 64])?)?;
    let publication_authority_backing =
        c.add(&[publication_path, c.vec(1, 36)?, c.mul(3, c.vec(1, 64)?)?])?;
    let publication_receipt_backing =
        c.add(&[c.mul(3, publication_derived_path)?, c.vec(1, 8192)?])?;
    let (publication_custody_size, publication_custody_align) =
        crate::catalog_session::export_publication_custody_layout();
    a.push(
        "retained.export_publication_c_custody_arc_root",
        Phase::Retained,
        1,
        c.arc(Layout {
            size: u64::try_from(publication_custody_size)?,
            align: u64::try_from(publication_custody_align)?,
        })?,
    )?;
    // source + pending source + lease seal; custody/pending root paths; 3+1
    // custody IDs, exact original transfer ID, 3+1 pending IDs, lease transfer.
    // The original Arc is shared, not another allocation of its pointee.
    a.push(
        "active.export_publication_c_custody_pending_and_lease_backings",
        Phase::Active,
        1,
        c.add(&[
            c.mul(3, publication_authority_backing)?,
            c.mul(2, publication_path)?,
            c.mul(10, LEASE_ID_BYTES)?,
            c.vec(1, 8192)?,
        ])?,
    )?;
    // The independent caller request coexists with Proxy's boxed deep Call
    // clone, already covered by wire_typed_graph and the relay parser entries.
    a.push(
        "active.export_publication_c_caller_request_backing",
        Phase::Active,
        1,
        c.add(&[
            publication_authority_backing,
            publication_path,
            c.mul(4, LEASE_ID_BYTES)?,
            c.vec(1, 8192)?,
        ])?,
    )?;
    let (publication_transfer_size, _) =
        crate::filesystem_worker::export_publication_transfer_layout();
    // The old terminal owns source/seal/cached-reply (3), and the new active
    // candidate source/seal/PhotoPublication.seal/cached-reply (4). The local
    // returned reply and decoded request are in the F typed graph envelope.
    // Each record owns its outer transfer plus cached reply transfer and
    // root's epoch/token/session: (1+1+3)*2 = 10 ID backings. Four digest
    // strings are cached request digests and cached reply provenance digests.
    // Unix verify_restored temporarily has five VerifiedFiles (3 held + 2
    // replacements), plus its cloned expected FileRevision digest. Those six
    // digest backings are charged alongside the four cached digests. The
    // publication directory is a sixth path owner; expected adds no path.
    a.push(
        "active.export_publication_f_terminal_candidate_and_last_result_overlap",
        Phase::Active,
        1,
        c.add(&[
            u64::try_from(publication_transfer_size)?,
            c.mul(7, publication_authority_backing)?,
            c.mul(2, publication_receipt_backing)?,
            c.mul(2, publication_path)?,
            c.mul(6, publication_derived_path)?,
            c.mul(10, c.vec(1, 64)?)?,
            c.mul(10, LEASE_ID_BYTES)?,
            c.mul(
                2,
                c.vec(1, crate::filesystem_worker::wire::ERROR_BYTES as u64)?,
            )?,
        ])?,
    )?;
    // F decode happens after the relay has retained C/G graphs. One parser is
    // active; charge the larger response grammar including Failed receipts.
    // Six Content layers cover Operation/Source/StoredPath/NativePath nesting
    // and the response Value/Receipt alternatives before semantic validation.
    a.push(
        "active.export_publication_f_wire_parser_scratch",
        Phase::Active,
        1,
        c.parse(RELAY_BYTES, 6, publication_request.max(publication_reply))?,
    )?;
    // read_journal grows through 64 KiB+1, retains raw Vec while deserializing,
    // and may retain the first parsed seal during plan.json parsing. The partial
    // graph bounds prevalidation strings/paths; the extra typed seal remains
    // live across that second parse. Recovery's metadata-only reader moves its
    // returned seal into the same candidate authority slot as the strict reader;
    // prepare_restore's stored seal/plan parses use these existing scratch slots.
    // Serialization conversions clone seals or
    // receipts, then StoredPath native vectors, alongside output Vec growth.
    let journal_bytes = 64 * 1024;
    let journal_partial = c.add(&[
        c.vec_growth(1, journal_bytes)?,
        c.vec_growth(2, journal_bytes / 2)?,
        c.mul(2, publication_authority_backing)?,
    ])?;
    a.push(
        "active.export_publication_journal_and_serialization_scratch",
        Phase::Active,
        1,
        c.add(&[
            c.parse(journal_bytes, 4, journal_partial)?,
            c.vec_growth(1, journal_bytes + 1)?,
            c.mul(2, publication_authority_backing)?,
            c.mul(2, publication_receipt_backing)?,
            c.mul(6, publication_derived_path)?,
            c.vec_growth(1, RELAY_BYTES)?,
            c.mul(2, c.vec(1, 64)?)?,
        ])?,
    )?;
    // One export-only F stage is serial with the existing export actor. ICC and
    // XMP contents are streamed directly to files, so retained backing includes
    // only their handles/digests; each 16 KiB chunk remains in the same relay
    // pool already reserved for managed metadata. The raw worker request and
    // parsed immutable plan coexist for the stage lifetime. These are requested
    // Rust backing bytes; native codec allocations and measured RSS are separate.
    let export_stage_path = c.vec(2, PATH_UNITS as u64)?;
    let export_stage_binding = c.add(&[
        c.vec(1, 128)?,
        c.vec(1, 128)?,
        c.vec(1, 64)?,
        c.vec(1, 256)?,
    ])?;
    // The immutable plan retains exact raw JSON plus a parsed Recipe/paths
    // graph. The serde Content layout bound covers variable vectors/strings;
    // counting only a handful of paths misses legal large recipes.
    let export_stage_work = c.add(&[
        Layout::of::<crate::catalog_exports::ExportWork>().size,
        crate::catalog_session::export_stage::PLAN_BYTES as u64,
        c.content(crate::catalog_session::export_stage::PLAN_BYTES as u64)?
            .1,
        export_stage_binding,
    ])?;
    let receipt_bytes = crate::catalog_session::export_stage::RECEIPT_BYTES as u64;
    // Successful cached terminal owns all decoded native facts (including the
    // notes String vector), added seal/snapshot, and native path/provenance.
    let export_stage_completion = c.add(&[
        Layout::of::<crate::catalog_session::export_stage::Completion>().size,
        c.content(receipt_bytes)?.1,
        publication_authority_backing,
        c.mul(3, export_stage_path)?,
        export_stage_binding,
        c.mul(4, LEASE_ID_BYTES)?,
    ])?;
    let export_stage_cached_small = c.add(&[
        export_stage_binding,
        c.mul(4, LEASE_ID_BYTES)?,
        export_stage_path,
        c.vec(1, crate::filesystem_worker::wire::ERROR_BYTES as u64)?,
    ])?;
    let (export_stage_size, export_stage_align) =
        crate::filesystem_worker::export_stage_owner_layout();
    a.push(
        "retained.export_stage_f_owner_and_stage_backings",
        Phase::Retained,
        1,
        c.add(&[
            c.layout_upper(&[Layout {
                size: u64::try_from(export_stage_size)?,
                align: u64::try_from(export_stage_align)?,
            }])?
            .size,
            c.mul(6, LEASE_ID_BYTES)?,
            c.mul(3, export_stage_path)?, // stage, RootCapability, partial cleanup
            crate::catalog_session::export_stage::REQUEST_BYTES as u64,
            export_stage_work,
            c.mul(2, c.vec(1, 64)?)?,
            c.vec_growth(Layout::of::<std::fs::File>().size, 2)?,
            // Independent terminal-seal, user and supervisor replay slots.
            export_stage_completion,
            c.mul(3, export_stage_cached_small)?,
        ])?,
    )?;
    a.push(
        "active.export_stage_full_envelopes_and_transient_backings",
        Phase::Active,
        1,
        c.add(&[
            c.mul(3, RELAY_BYTES)?,
            // Stage construction, exact-plan serde and native request v2 clone.
            c.mul(3, export_stage_work)?,
            c.parse(
                crate::catalog_session::export_stage::PLAN_BYTES as u64,
                2,
                export_stage_work,
            )?,
            c.mul(3, export_stage_binding)?,
            c.mul(4, export_stage_path)?,
            crate::catalog_session::export_stage::REQUEST_BYTES as u64,
            c.parse(receipt_bytes, 2, export_stage_completion)?,
            // Facts + pre-effect completion + full F and C wrapper clones,
            // then result/cache/reply clone overlap. These lifetimes are serial.
            c.mul(4, export_stage_completion)?,
            c.mul(2, CHUNK_BYTES)?, // upload trailer and streamed hash scratch
        ])?,
    )?;
    // The serial export worker owns at most one profile transfer. Its cache and
    // the in-progress assembly share the existing 32 MiB quota: before token
    // publication, the assembly occupies the unused cache allowance. Eight
    // Vec backings cover either eight cached entries or seven plus assembly.
    let profile_total = c.mul(2, EXPORT_PROFILE_BYTES as u64)?;
    let profile_vectors = c.add(&[c.mul(2, profile_total)?, c.mul(8, 8)?])?;
    let (profile_entry_size, profile_entry_align) =
        crate::application::exports::profile_cache_entry_layout();
    a.push(
        "retained.export_profile_cache_and_assembly_backings",
        Phase::Retained,
        1,
        c.add(&[
            c.table(
                Layout {
                    size: u64::try_from(profile_entry_size)?,
                    align: u64::try_from(profile_entry_align)?,
                },
                8,
            )?,
            c.mul(
                8,
                c.add(&[
                    c.mul(2, LEASE_ID_BYTES)?,
                    c.mul(3, PATH_UNITS as u64)?,
                    64,
                    c.arc(Layout::of::<Vec<u8>>())?,
                ])?,
            )?,
            profile_vectors,
        ])?,
    )?;
    let profile_result_backing = c.add(&[LEASE_ID_BYTES, c.mul(3, PATH_UNITS as u64)?, 64])?;
    a.push(
        "retained.export_profile_status_result_backing",
        Phase::Retained,
        1,
        profile_result_backing,
    )?;
    a.push(
        "active.export_profile_status_query_clone_backing",
        Phase::Active,
        1,
        profile_result_backing,
    )?;
    // `output_profile` temporarily owns the request clone and returned ICC
    // clone while the quota-backed assembly remains live.
    a.push(
        "active.export_profile_lcms_clone_backings",
        Phase::Active,
        2,
        c.vec(1, EXPORT_PROFILE_BYTES as u64)?,
    )?;
    let profile_request = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Operation>().size,
        Layout::of::<crate::catalog_session::ExportProfileRequest>().size,
        c.mul(4, LEASE_ID_BYTES)?,
        c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
    ])?;
    let profile_reply = c.add(&[
        Layout::of::<crate::filesystem_worker::wire::Response>().size,
        c.mul(4, LEASE_ID_BYTES)?,
        c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
        64,
        c.vec(1, CHUNK_BYTES)?,
    ])?;
    a.push(
        "active.export_profile_f_typed_graphs",
        Phase::Active,
        1,
        c.add(&[profile_request, profile_request.max(profile_reply)])?,
    )?;
    let (transfer_size, _) = crate::filesystem_worker::export_profile_transfer_layout();
    a.push(
        "active.export_profile_f_retained_transfer",
        Phase::Active,
        1,
        c.add(&[
            u64::try_from(transfer_size)?,
            c.mul(2, LEASE_ID_BYTES)?,
            c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
            c.vec(1, SOURCE_REVISION_OBJECT_BYTES)?,
            c.vec(1, SOURCE_REVISION_CHANGED_BYTES)?,
        ])?,
    )?;
    a.push(
        "active.export_profile_f_begin_path_backings",
        Phase::Active,
        1,
        c.mul(3, c.vec(2, PATH_UNITS as u64)?)?,
    )?;
    a.push(
        "active.export_profile_c_caller_request_backing",
        Phase::Active,
        1,
        c.add(&[
            c.mul(4, LEASE_ID_BYTES)?,
            c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
        ])?,
    )?;
    a.push(
        "active.export_profile_worker_requested_path_backing",
        Phase::Active,
        1,
        c.vec(2, PATH_UNITS as u64)?,
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
    let [native_owner, native_slot] = super::native::owner_layouts();
    let native_owner = Layout {
        size: u64::try_from(native_owner.0)?,
        align: u64::try_from(native_owner.1)?,
    };
    let native_slot = Layout {
        size: u64::try_from(native_slot.0)?,
        align: u64::try_from(native_slot.1)?,
    };
    a.push(
        "fixed.native_g_owner_arc_backing",
        Phase::Retained,
        1,
        c.arc(native_owner)?,
    )?;
    a.push(
        "fixed.native_g_slot_arc_backings",
        Phase::Active,
        w,
        c.arc(native_slot)?,
    )?;
    a.push(
        "fixed.native_g_slot_registry_backing",
        Phase::Active,
        1,
        c.vec_growth(Layout::of::<Arc<()>>().size, w)?,
    )?;
    let [
        export_owner,
        export_slot,
        export_stage_state,
        export_pending,
        export_completed,
        ..,
    ] = super::export_native::owner_layouts();
    let export_owner = Layout {
        size: u64::try_from(export_owner.0)?,
        align: u64::try_from(export_owner.1)?,
    };
    let export_slot = Layout {
        size: u64::try_from(export_slot.0)?,
        align: u64::try_from(export_slot.1)?,
    };
    a.push(
        "fixed.export_native_g_owner_arc_backing",
        Phase::Retained,
        1,
        c.arc(export_owner)?,
    )?;
    a.push(
        "retained.export_native_g_previous_retire_receipt",
        Phase::Retained,
        1,
        // Inline tuple storage is in Owner. Account separately for the full
        // root/path/binding/status heap graph retained after slot destruction.
        c.add(&[
            c.mul(2, RELAY_BYTES)?,
            export_stage_binding,
            crate::catalog_session::export_native::ERROR_BYTES as u64,
        ])?,
    )?;
    a.push(
        "fixed.export_native_g_slot_arc_backings",
        Phase::Active,
        w,
        c.arc(export_slot)?,
    )?;
    a.push(
        "fixed.export_native_g_slot_registry_backing",
        Phase::Active,
        1,
        c.vec_growth(Layout::of::<Arc<()>>().size, w)?,
    )?;
    a.push(
        "fixed.export_native_g_slot_state_and_relay_backings",
        Phase::Active,
        w,
        c.add(&[
            c.layout_upper(&[
                Layout {
                    size: u64::try_from(export_stage_state.0)?,
                    align: u64::try_from(export_stage_state.1)?,
                },
                Layout {
                    size: u64::try_from(export_pending.0)?,
                    align: u64::try_from(export_pending.1)?,
                },
                Layout {
                    size: u64::try_from(export_completed.0)?,
                    align: u64::try_from(export_completed.1)?,
                },
            ])?
            .size,
            // Registration, pending dispatch, cached outcome, exact work/plan,
            // enriched receipt, and the C/G plus G/F wrapper pair.
            c.mul(3, export_stage_work)?,
            export_stage_completion,
            c.mul(3, export_stage_cached_small)?, // terminal supervisor result plus dispatch/return clones
            c.mul(2, export_stage_binding)?,
            c.mul(2, RELAY_BYTES)?,
            crate::catalog_session::export_stage::REQUEST_BYTES as u64,
            crate::catalog_session::export_stage::RECEIPT_BYTES as u64,
            c.mul(2, crate::catalog_session::export_stage::BLOB_BYTES)?,
            crate::catalog_session::export_stage::CHUNK_BYTES as u64,
        ])?,
    )?;
    let [
        export_c_registry,
        export_c_state,
        export_c_executor_state,
        export_c_executor,
        export_c_attempt,
    ] = crate::catalog_session::managed_export_registry_layouts();
    let [
        export_c_facade,
        export_c_service,
        export_c_active,
        export_c_phase,
        export_c_disposition,
    ] = crate::export_service::managed_export_state_layouts();
    a.push(
        "retained.export_managed_c_registry_and_exact_replay_graphs",
        Phase::Retained,
        1,
        c.add(&[
            u64::try_from(export_c_registry.0)?,
            u64::try_from(export_c_state.0)?,
            u64::try_from(export_c_executor_state.0)?,
            u64::try_from(export_c_executor.0)?,
            u64::try_from(export_c_attempt.0)?,
            // Active/pending executor requests and pending Close each retain a
            // complete RootCapability/executor graph. This also covers the
            // constructor's relay clone while the session registry is pinned.
            c.mul(4, RELAY_BYTES)?,
            c.mul(12, LEASE_ID_BYTES)?,
            c.mul(4, c.vec(2, PATH_UNITS as u64)?)?,
        ])?,
    )?;
    a.push(
        "retained.export_managed_c_service_attempt_work_and_phase_graphs",
        Phase::Retained,
        1,
        c.add(&[
            // The stable facade retains the Backend discriminator and Box
            // pointer inline in the application export owner.
            u64::try_from(export_c_facade.0)?,
            // Backend::Managed box-owns this complete allocation. The Box
            // pointee is a separate retained heap owner charged exactly once.
            u64::try_from(export_c_service.0)?,
            u64::try_from(export_c_active.0)?,
            u64::try_from(export_c_phase.0)?,
            u64::try_from(export_c_disposition.0)?,
            // Active owns exact work in Begin plus the pending stage clone;
            // the registry/handle roots and bindings remain live across ticks.
            c.mul(2, export_stage_work)?,
            c.mul(3, export_stage_binding)?,
            c.mul(8, LEASE_ID_BYTES)?,
            c.mul(2, c.vec(2, PATH_UNITS as u64)?)?,
            c.vec(1, 8192)?,
        ])?,
    )?;
    a.push(
        "retained.export_managed_c_full_icc_and_xmp_blob_backings",
        Phase::Retained,
        1,
        // photo_export_inputs materializes both independently bounded SQL
        // blobs and C retains them between 16 KiB upload ticks. They therefore
        // consume two full 16 MiB Vec capacities, rather than a streaming term.
        c.mul(2, c.vec_growth(1, EXPORT_PROFILE_BYTES as u64)?)?,
    )?;
    a.push(
        "active.export_managed_c_chunk_pending_request_and_relay_clones",
        Phase::Active,
        1,
        c.add(&[
            c.mul(4, CHUNK_BYTES)?,
            c.mul(
                3,
                crate::catalog_session::export_stage::REQUEST_BYTES as u64,
            )?,
            c.mul(3, RELAY_BYTES)?,
            c.mul(2, export_stage_binding)?,
        ])?,
    )?;
    a.push(
        "retained.export_managed_c_completion_receipt_and_metrics",
        Phase::Retained,
        1,
        c.add(&[
            export_stage_completion,
            Layout::of::<crate::export_service::ExportCompletionMetrics>().size,
            c.content(receipt_bytes)?.1,
            c.mul(5, c.vec(1, 8192)?)?,
            c.mul(8, c.vec(1, 256)?)?,
        ])?,
    )?;
    let export_executor_owner = crate::filesystem_worker::export_executor_owner_layout();
    let export_executor_candidate = crate::export_worker::compact_recovery_layout();
    a.push(
        "retained.export_executor_f_owner_and_compact_inventory",
        Phase::Retained,
        1,
        c.add(&[
            u64::try_from(export_executor_owner.0)?,
            c.vec_growth(
                u64::try_from(export_executor_candidate.0)?,
                crate::catalog_session::export_executor::MAX_DIRECTORIES,
            )?,
            c.mul(
                crate::catalog_session::export_executor::MAX_DIRECTORIES,
                c.add(&[c.vec(2, PATH_UNITS as u64)?, 128, 128, 64, LEASE_ID_BYTES])?,
            )?,
        ])?,
    )?;
    a.push(
        "retained.export_executor_exact_g_f_replay_graphs",
        Phase::Retained,
        1,
        // F: last reply, predecessor Close, and prevalidated Discard reply.
        // G: pending request, last request+reply, predecessor request+reply.
        // Eight complete envelopes cover these simultaneously retained graphs.
        // F pending digest and three active root/lock/staging path allocations
        // are separate from replay graphs. Cleanup File/progress arrays and
        // both lifecycle high-water counters are inline in the owner layouts.
        c.add(&[
            c.mul(8, RELAY_BYTES)?,
            64,
            c.mul(3, c.vec(2, PATH_UNITS as u64)?)?,
        ])?,
    )?;
    a.push(
        "active.export_executor_frames_inventory_scan_and_request_validation",
        Phase::Active,
        1,
        c.add(&[
            c.mul(4, RELAY_BYTES)?,
            // One current bounded request buffer/RawValue/work decode graph,
            // plus checked_plan's simultaneous parsed validation graph. The
            // decoder validates once and validate_persisted validates again;
            // these two validation scratch graphs do not overlap each other.
            c.parse(
                crate::catalog_session::export_stage::REQUEST_BYTES as u64,
                2,
                crate::catalog_session::export_stage::PLAN_BYTES as u64,
            )?,
            c.parse(
                crate::catalog_session::export_stage::PLAN_BYTES as u64,
                1,
                crate::catalog_session::export_stage::PLAN_BYTES as u64,
            )?,
            // Recovery has a full entries Vec beside the reserved candidates.
            // Candidate path allowance above bounds the union of remaining
            // entry paths and accumulated candidate paths. The transient term
            // covers canonicalization/read_dir/join copies for the current
            // entry and up to two 8-artifact inspection lists during Discard.
            c.vec_growth(
                std::mem::size_of::<std::path::PathBuf>() as u64,
                crate::catalog_session::export_executor::MAX_DIRECTORIES + 1,
            )?,
            c.mul(32, c.vec(2, PATH_UNITS as u64)?)?,
            // A held request digest read uses take(R+1)/read_to_end; growth is
            // charged even though a full work/plan is no longer decoded there.
            c.vec_growth(1, crate::export_worker::REQUEST_LIMIT + 1)?,
            crate::catalog_session::export_executor::ERROR_BYTES as u64,
        ])?,
    )?;
    let claim_record_bytes = crate::export_worker::COMPACT_CLAIM_RECORD_BYTES as u64;
    a.push(
        "retained.export_discard_claim_control_and_scaffolding",
        Phase::Retained,
        1,
        // One serialized Discard: bounded committed record/typed copies and
        // wrapper/claimed/source path backing. Handles, bitmap, and Record
        // headers are included in F Owner layout. Private metadata adds no
        // second inventory allowance and cannot accumulate across recovery.
        c.add(&[
            4 * claim_record_bytes,
            c.mul(4, c.vec(2, PATH_UNITS as u64)?)?,
        ])?,
    )?;
    a.push(
        "active.export_discard_claim_journal_and_recovery",
        Phase::Active,
        1,
        // JSON commit/scratch/readback and at most two claim-entry references;
        // the bounded request digest buffer is already charged above.
        c.add(&[
            c.parse(claim_record_bytes, 2, claim_record_bytes)?,
            4 * claim_record_bytes,
            c.vec(8, 2)?,
            c.mul(4, c.vec(2, PATH_UNITS as u64)?)?,
        ])?,
    )?;
    // Export control coexists with ordinary data: G completed replay, C
    // active query, C consumed replay, query/reply outputs, and the receiving
    // decode/return copy. Account whole envelopes and typed graphs, not only
    // the inner Status error or an F receipt. The output classes also retain
    // all 16 early Stop keys independently of the one query/result pair.
    let export_control = super::filesystem::CONTROL_BYTES as u64;
    a.push(
        "relay.export_native_control_lifetimes",
        Phase::Active,
        1,
        c.add(&[
            c.mul(6, export_control)?,
            c.mul(6, wire_typed_graph(c, export_control)?)?,
            c.mul(6, c.arc(Layout::of::<Vec<u8>>())?)?,
            c.mul(3, c.vec_growth(slot_item, 1)?)?,
            c.vec_growth(slot_item, 16)?,
            c.vec_growth(
                Layout::of::<crate::catalog_session::export_native::Key>().size,
                16,
            )?,
            c.mul(16 * 5, LEASE_ID_BYTES)?,
        ])?,
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
        for name in [
            "retained.import_f_source_custody_and_transfer",
            "active.import_f_inspection_parse_and_encoding",
            "active.import_c_transfer_decode_and_preparation",
        ] {
            assert!(
                default_report
                    .contributions
                    .iter()
                    .any(|entry| entry.name == name && entry.total > 0),
                "missing {name}"
            );
        }
        let mut names = std::collections::HashSet::new();
        assert!(
            default_report
                .contributions
                .iter()
                .all(|entry| names.insert(entry.name))
        );
        let export_caller = default_report
            .contributions
            .iter()
            .find(|entry| entry.name == "active.export_destination_c_caller_request_backing")
            .unwrap();
        assert_eq!(export_caller.phase, Phase::Active);
        assert_eq!(export_caller.count, 1);
        assert_eq!(
            export_caller.each,
            Checked.add(&[
                Checked.mul(3, LEASE_ID_BYTES)?,
                Checked.mul(2, Checked.vec(2, PATH_UNITS as u64)?)?,
            ])?
        );
        let export_snapshot_scratch = default_report
            .contributions
            .iter()
            .find(|entry| {
                entry.name == "active.export_destination_resolution_and_validation_scratch"
            })
            .unwrap();
        assert_eq!(export_snapshot_scratch.phase, Phase::Active);
        assert_eq!(export_snapshot_scratch.count, 1);
        let derived_path_units = Checked.add(&[PATH_UNITS as u64, 64])?;
        assert_eq!(
            export_snapshot_scratch.each,
            Checked.add(&[
                Checked.mul(14, Checked.vec(2, derived_path_units)?)?,
                Checked.mul(2, Checked.vec(1, 8192)?)?,
                Checked.mul(4, Checked.vec(1, 64)?)?,
                Checked.vec_growth(1, Checked.mul(4, RELAY_BYTES)?)?,
            ])?
        );
        let export_alias = default_report
            .contributions
            .iter()
            .find(|entry| {
                entry.name == "active.export_alias_projection_candidate_and_source_backing_envelope"
            })
            .unwrap();
        assert_eq!(export_alias.phase, Phase::Active);
        assert_eq!(export_alias.count, 1);
        assert_eq!(
            export_alias.each,
            Checked.add(&[
                Checked.mul(2, 256 * 1024)?,
                Checked.mul(5, Checked.vec(2, PATH_UNITS as u64)?)?,
                Checked.mul(3, Checked.vec(1, Checked.add(&[PATH_UNITS as u64, 4])?)?,)?,
                Checked.vec(1, 39)?,
            ])?
        );
        let original_custody = default_report
            .contributions
            .iter()
            .find(|entry| entry.name == "active.export_original_c_custody_and_lease_backings")
            .unwrap();
        assert_eq!(original_custody.phase, Phase::Active);
        assert_eq!(original_custody.count, 1);
        assert_eq!(
            original_custody.each,
            Checked.add(&[
                Checked.mul(5, LEASE_ID_BYTES)?,
                Checked.mul(2, Checked.vec(2, PATH_UNITS as u64)?)?,
                Checked.vec(1, 64)?,
            ])?
        );
        let original_f = default_report
            .contributions
            .iter()
            .find(|entry| entry.name == "active.export_original_f_active_terminal_overlap")
            .unwrap();
        assert_eq!(original_f.phase, Phase::Active);
        assert_eq!(original_f.count, 1);
        assert_eq!(
            original_f.each,
            Checked.add(&[
                u64::try_from(crate::filesystem_worker::export_original_transfer_layout().0)?,
                Checked.mul(2, LEASE_ID_BYTES)?,
                Checked.mul(3, Checked.vec(2, PATH_UNITS as u64)?)?,
                Checked.vec(1, 64)?,
            ])?
        );
        for name in [
            "active.export_publication_c_custody_pending_and_lease_backings",
            "active.export_publication_c_caller_request_backing",
            "active.export_publication_f_terminal_candidate_and_last_result_overlap",
            "active.export_publication_f_wire_parser_scratch",
            "active.export_publication_journal_and_serialization_scratch",
        ] {
            let entry = default_report
                .contributions
                .iter()
                .find(|entry| entry.name == name)
                .unwrap();
            assert_eq!(entry.phase, Phase::Active);
            assert_eq!(entry.count, 1);
            assert!(entry.each > 0);
        }
        let export_stage_retained = default_report
            .contributions
            .iter()
            .find(|entry| entry.name == "retained.export_stage_f_owner_and_stage_backings")
            .unwrap();
        assert_eq!(export_stage_retained.phase, Phase::Retained);
        assert_eq!(export_stage_retained.count, 1);
        assert!(
            export_stage_retained.each > crate::catalog_session::export_stage::REQUEST_BYTES as u64
        );
        let export_stage_active = default_report
            .contributions
            .iter()
            .find(|entry| entry.name == "active.export_stage_full_envelopes_and_transient_backings")
            .unwrap();
        assert_eq!(export_stage_active.phase, Phase::Active);
        assert_eq!(export_stage_active.count, 1);
        assert!(export_stage_active.each >= 3 * RELAY_BYTES);
        let profile_cache = default_report
            .contributions
            .iter()
            .find(|entry| entry.name == "retained.export_profile_cache_and_assembly_backings")
            .unwrap();
        assert_eq!(profile_cache.phase, Phase::Retained);
        assert_eq!(profile_cache.count, 1);
        let profile_clones = default_report
            .contributions
            .iter()
            .find(|entry| entry.name == "active.export_profile_lcms_clone_backings")
            .unwrap();
        assert_eq!(profile_clones.phase, Phase::Active);
        assert_eq!(profile_clones.count, 2);
        assert_eq!(
            profile_clones.each,
            Checked.vec(1, EXPORT_PROFILE_BYTES as u64)?
        );
        let profile_transfer = default_report
            .contributions
            .iter()
            .find(|entry| entry.name == "active.export_profile_f_retained_transfer")
            .unwrap();
        assert_eq!(profile_transfer.phase, Phase::Active);
        assert_eq!(profile_transfer.count, 1);
        assert_eq!(
            profile_transfer.each,
            Checked.add(&[
                u64::try_from(crate::filesystem_worker::export_profile_transfer_layout().0)?,
                Checked.mul(2, LEASE_ID_BYTES)?,
                Checked.mul(2, Checked.vec(2, PATH_UNITS as u64)?)?,
                Checked.vec(1, SOURCE_REVISION_OBJECT_BYTES)?,
                Checked.vec(1, SOURCE_REVISION_CHANGED_BYTES)?,
            ])?
        );
        let profile_status = default_report
            .contributions
            .iter()
            .find(|entry| entry.name == "retained.export_profile_status_result_backing")
            .unwrap();
        let profile_status_clone = default_report
            .contributions
            .iter()
            .find(|entry| entry.name == "active.export_profile_status_query_clone_backing")
            .unwrap();
        assert_eq!(profile_status.phase, Phase::Retained);
        assert_eq!(profile_status_clone.phase, Phase::Active);
        assert_eq!(profile_status.each, profile_status_clone.each);
        assert_eq!(
            profile_status.each,
            Checked.add(&[LEASE_ID_BYTES, Checked.mul(3, PATH_UNITS as u64)?, 64,])?
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
    fn managed_export_c_capacity_terms_cover_retained_and_transient_graphs() -> Result<()> {
        let report = report(&config())?;
        for name in [
            "retained.export_managed_c_registry_and_exact_replay_graphs",
            "retained.export_managed_c_service_attempt_work_and_phase_graphs",
            "retained.export_managed_c_full_icc_and_xmp_blob_backings",
            "active.export_managed_c_chunk_pending_request_and_relay_clones",
            "retained.export_managed_c_completion_receipt_and_metrics",
        ] {
            assert!(
                report.contributions.iter().any(|entry| entry.name == name),
                "missing managed export C capacity term {name}"
            );
        }
        let blobs = report
            .contributions
            .iter()
            .find(|entry| entry.name == "retained.export_managed_c_full_icc_and_xmp_blob_backings")
            .unwrap();
        assert_eq!(blobs.phase, Phase::Retained);
        assert_eq!(
            blobs.each,
            Checked.mul(2, Checked.vec_growth(1, EXPORT_PROFILE_BYTES as u64)?)?
        );
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
    #[test]
    fn native_g_owner_and_slot_roots_follow_exact_source_layouts() -> Result<()> {
        let config = config();
        let report = report(&config)?;
        let [owner, slot] = super::super::native::owner_layouts();
        let owner = Layout {
            size: u64::try_from(owner.0)?,
            align: u64::try_from(owner.1)?,
        };
        let slot = Layout {
            size: u64::try_from(slot.0)?,
            align: u64::try_from(slot.1)?,
        };
        let owner_entry = report
            .contributions
            .iter()
            .find(|entry| entry.name == "fixed.native_g_owner_arc_backing")
            .context("native G owner layout contribution")?;
        assert_eq!(
            (owner_entry.count, owner_entry.each),
            (1, Checked.arc(owner)?)
        );
        let slot_entry = report
            .contributions
            .iter()
            .find(|entry| entry.name == "fixed.native_g_slot_arc_backings")
            .context("native G slot layout contribution")?;
        assert_eq!(
            (slot_entry.count, slot_entry.each),
            (config.preview_limits.workers as u64, Checked.arc(slot)?)
        );
        let registry = report
            .contributions
            .iter()
            .find(|entry| entry.name == "fixed.native_g_slot_registry_backing")
            .context("native G registry layout contribution")?;
        assert_eq!(
            (registry.count, registry.each),
            (
                1,
                Checked.vec_growth(
                    Layout::of::<Arc<()>>().size,
                    config.preview_limits.workers as u64,
                )?
            )
        );
        Ok(())
    }
    #[test]
    fn export_native_g_slots_count_every_legal_worker_and_registry_backing() -> Result<()> {
        let config = config();
        let report = report(&config)?;
        let [owner, slot, ..] = super::super::export_native::owner_layouts();
        let owner = Layout {
            size: u64::try_from(owner.0)?,
            align: u64::try_from(owner.1)?,
        };
        let slot = Layout {
            size: u64::try_from(slot.0)?,
            align: u64::try_from(slot.1)?,
        };
        let owner_entry = report
            .contributions
            .iter()
            .find(|entry| entry.name == "fixed.export_native_g_owner_arc_backing")
            .context("export native G owner layout contribution")?;
        assert_eq!(
            (owner_entry.count, owner_entry.each),
            (1, Checked.arc(owner)?)
        );
        let slots = report
            .contributions
            .iter()
            .find(|entry| entry.name == "fixed.export_native_g_slot_arc_backings")
            .context("export native G slot layout contribution")?;
        assert_eq!(
            (slots.count, slots.each),
            (config.preview_limits.workers as u64, Checked.arc(slot)?)
        );
        let registry = report
            .contributions
            .iter()
            .find(|entry| entry.name == "fixed.export_native_g_slot_registry_backing")
            .context("export native G registry layout contribution")?;
        assert_eq!(
            (registry.count, registry.each),
            (
                1,
                Checked.vec_growth(
                    Layout::of::<Arc<()>>().size,
                    config.preview_limits.workers as u64,
                )?
            )
        );
        let retained = report
            .contributions
            .iter()
            .find(|entry| entry.name == "fixed.export_native_g_slot_state_and_relay_backings")
            .context("export native G retained relay contribution")?;
        assert_eq!(retained.count, config.preview_limits.workers as u64);
        assert!(retained.each >= 2 * RELAY_BYTES);
        Ok(())
    }
    #[test]
    fn export_stage_backings_are_admitted_by_the_existing_shared_reservation() -> Result<()> {
        let config = config();
        let report = report(&config)?;
        let retained = report
            .contributions
            .iter()
            .find(|c| c.name == "retained.export_stage_f_owner_and_stage_backings")
            .unwrap()
            .each;
        let active = report
            .contributions
            .iter()
            .find(|c| c.name == "active.export_stage_full_envelopes_and_transient_backings")
            .unwrap()
            .each;
        let receipt_graph = Checked
            .content(crate::catalog_session::export_stage::RECEIPT_BYTES as u64)?
            .1;
        assert!(
            retained > receipt_graph + crate::catalog_session::export_stage::REQUEST_BYTES as u64
        );
        assert!(active >= 4 * receipt_graph + 3 * RELAY_BYTES + 2 * CHUNK_BYTES);
        assert!(report.retained >= retained && report.active >= active);
        // This is the existing process admission object, not a stage pool.
        let pool = crate::preview::ByteBudget::new(report.requested)?;
        let occupied = pool.try_reserve(1).unwrap();
        assert!(
            super::super::preview_metadata_admission::ProcessReservation::reserve(&config, &pool)
                .is_err()
        );
        assert_eq!(pool.used(), 1);
        drop(occupied);
        let owner =
            super::super::preview_metadata_admission::ProcessReservation::reserve(&config, &pool)?;
        assert_eq!(pool.used(), report.requested);
        let retained_owner = owner.clone();
        owner.arm();
        owner.retire();
        drop(owner);
        assert_eq!(pool.used(), report.requested);
        drop(retained_owner);
        assert_eq!(pool.used(), 0);
        let retry =
            super::super::preview_metadata_admission::ProcessReservation::reserve(&config, &pool)?;
        drop(retry);
        assert_eq!(pool.used(), 0);
        println!(
            "export_stage retained={retained} active={active}; shared total={}",
            report.requested
        );
        Ok(())
    }
}
