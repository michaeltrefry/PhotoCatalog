//! Nondefault, test-only requested-allocation observations. Not an RSS limiter.
//! Run one exact probe in its own process with --test-threads=1. The allocator
//! exists only under cfg(all(test, feature="internal-capacity-probes")).
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
};

struct ObservedSystem;
#[global_allocator]
static ALLOCATOR: ObservedSystem = ObservedSystem;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static INVALID: AtomicBool = AtomicBool::new(false);
static FAILURES: AtomicUsize = AtomicUsize::new(0);

// No locks, allocation, formatting, panic or recursion in this path. Counters
// linearize at the allocator API boundary, not inside System::realloc. System's
// own temporary copying/storage is outside this requested-Layout observation.
fn change(old: usize, new: usize) {
    let result = LIVE.fetch_update(SeqCst, SeqCst, |live| {
        live.checked_sub(old)?.checked_add(new)
    });
    if let Ok(before) = result {
        let live = before - old + new;
        PEAK.fetch_max(live, SeqCst);
        for event in &EVENTS {
            if event.active.load(SeqCst) != 0 {
                event.phase_peak.fetch_max(live, SeqCst);
            }
        }
    } else {
        INVALID.store(true, SeqCst);
    }
}
fn allocated(pointer: *mut u8, layout: Layout) {
    if pointer.is_null() {
        FAILURES.fetch_add(1, SeqCst);
    } else {
        change(0, layout.size());
        node_layout(layout.size(), layout.align());
    }
}
fn reallocated(pointer: *mut u8, old: usize, new: usize) {
    if pointer.is_null() {
        // Failed realloc leaves the original allocation and its charge intact.
        FAILURES.fetch_add(1, SeqCst);
    } else {
        change(old, new);
    }
}
unsafe impl GlobalAlloc for ObservedSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        allocated(pointer, layout);
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        allocated(pointer, layout);
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        change(layout.size(), 0);
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(pointer, layout, size) };
        reallocated(next, layout.size(), size);
        next
    }
}

// A fixed table records phase snapshots without allocating during an observed
// operation. Peak is process-cumulative: it is never reset under other threads.
// Each event also records the maximum explicitly supplied owned capacity term.
const COUNT: usize = 32;
static EVENTS: [Event; COUNT] = [const { Event::new() }; COUNT];
struct Event {
    active: AtomicUsize,
    phase_peak: AtomicUsize,
    visits: AtomicUsize,
    live: AtomicUsize,
    peak: AtomicUsize,
    capacity: AtomicUsize,
}
impl Event {
    const fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            phase_peak: AtomicUsize::new(0),
            visits: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            capacity: AtomicUsize::new(0),
        }
    }
}
pub(crate) const SEAL_SETS: usize = 0;
pub(crate) const DESCRIPTOR_RECORD: usize = 1;
pub(crate) const DESCRIPTOR_BYTES: usize = 2;
pub(crate) const DESCRIPTOR_MANIFEST: usize = 3;
pub(crate) const DESCRIPTOR_SEAL: usize = 4;
pub(crate) const DESCRIPTOR_CLONES: usize = 5;
pub(crate) const PENDING_MANIFEST: usize = 6;
pub(crate) const REQUEST_MANIFEST: usize = 7;
pub(crate) const STAGED_NEXT: usize = 8;
pub(crate) const STAGED_BREAK: usize = 9;
pub(crate) const STAGED_READY: usize = 10;
pub(crate) const CHUNK_VERIFY: usize = 11;
pub(crate) const SOURCE_ENCODED: usize = 12;
pub(crate) const SOURCE_DECODED: usize = 13;
pub(crate) const OPEN_AUTHORITY: usize = 14;
pub(crate) const OPEN_DECODED: usize = 15;
pub(crate) const FRAME_INPUT: usize = 16;
pub(crate) const FRAME_OUTPUT: usize = 17;
pub(crate) const TWO_MANIFESTS: usize = 18;

pub(crate) fn observe(index: usize, capacity: usize) {
    let Some(event) = EVENTS.get(index) else {
        INVALID.store(true, SeqCst);
        return;
    };
    let live = LIVE.load(SeqCst);
    event.visits.fetch_add(1, SeqCst);
    event.live.fetch_max(live, SeqCst);
    event.peak.fetch_max(PEAK.load(SeqCst).max(live), SeqCst);
    event.capacity.fetch_max(capacity, SeqCst);
}
pub(crate) struct Phase(usize);
pub(crate) fn phase(index: usize) -> Phase {
    let event = &EVENTS[index];
    event.active.fetch_add(1, SeqCst);
    event.phase_peak.fetch_max(LIVE.load(SeqCst), SeqCst);
    observe(index, 0);
    Phase(index)
}
impl Drop for Phase {
    fn drop(&mut self) {
        observe(self.0, 0);
        EVENTS[self.0].active.fetch_sub(1, SeqCst);
    }
}
pub(crate) fn request_manifest(bytes: usize) {
    observe(REQUEST_MANIFEST, bytes);
    if EVENTS[PENDING_MANIFEST].active.load(SeqCst) != 0 {
        // This event proves the actual nested caller retained a prior result.
        // The capacity value is the current second result only; the first
        // result's separately recorded maximum is not claimed simultaneous exact.
        observe(TWO_MANIFESTS, bytes);
    }
}
pub(crate) fn visits(index: usize) -> usize {
    EVENTS[index].visits.load(SeqCst)
}
pub(crate) fn capacity(index: usize) -> usize {
    EVENTS[index].capacity.load(SeqCst)
}
pub(crate) struct Baseline {
    live: usize,
    peak: usize,
}
pub(crate) fn begin() -> Baseline {
    assert!(!INVALID.load(SeqCst), "allocation counter lost exactness");
    Baseline {
        live: LIVE.load(SeqCst),
        peak: PEAK.load(SeqCst),
    }
}
pub(crate) fn report(name: &str, baseline: Baseline) {
    let live = LIVE.load(SeqCst);
    let peak = PEAK.load(SeqCst).max(live);
    assert!(!INVALID.load(SeqCst), "allocation counter lost exactness");
    println!(
        "CAPACITY {name} baseline_live={} baseline_peak={} current_live={live} process_peak={peak} failures={} units=requested_Rust_Layout_bytes excludes=System_realloc_internals,C_SQLite,stacks,OS,RSS",
        baseline.live,
        baseline.peak,
        FAILURES.load(SeqCst)
    );
    for (index, event) in EVENTS.iter().enumerate() {
        let visits = event.visits.load(SeqCst);
        if visits != 0 {
            println!(
                "CAPACITY_PHASE {name} index={index} visits={visits} live={} process_peak={} phase_peak={} owned_capacity={}",
                event.live.load(SeqCst),
                event.peak.load(SeqCst),
                event.phase_peak.load(SeqCst),
                event.capacity.load(SeqCst)
            );
        }
    }
}

pub(crate) fn path(path: &crate::storage_volume::NativePath) -> usize {
    match path {
        crate::storage_volume::NativePath::UnixBytes(v) => v.capacity(),
        crate::storage_volume::NativePath::WindowsWide(v) => v.capacity() * 2,
    }
}
pub(crate) fn revision(revision: &crate::lightroom::source::Revision) -> usize {
    revision.object.capacity() + revision.changed.capacity()
}
pub(crate) fn artifact(a: &crate::lightroom::capture::Artifact) -> usize {
    path(&a.source)
        + path(&a.relative)
        + a.role.capacity()
        + a.stored.capacity()
        + a.blake3.capacity()
        + revision(&a.revision)
}
pub(crate) fn manifest(m: &crate::lightroom::capture::Manifest) -> usize {
    use crate::lightroom::{
        Issue,
        capture::{Artifact, Entry},
    };
    use std::mem::size_of;
    let optional = |s: &Option<String>| s.as_ref().map_or(0, String::capacity);
    path(&m.request.source)
        + path(&m.request.output)
        + optional(&m.request.closed_application_evidence)
        + m.state.capacity()
        + m.raw_byte_retention.capacity()
        + m.sqlite_consistency.capacity()
        + m.application_consistency.capacity()
        + m.cooperative_lock_protocol.capacity()
        + m.artifacts.capacity() * size_of::<Artifact>()
        + m.artifacts.iter().map(artifact).sum::<usize>()
        + m.companion_inventory.capacity() * size_of::<Entry>()
        + m.companion_inventory
            .iter()
            .map(|e| path(&e.path) + path(&e.relative) + e.role.capacity() + e.changed.capacity())
            .sum::<usize>()
        + m.absent_companions.capacity() * size_of::<crate::storage_volume::NativePath>()
        + m.absent_companions.iter().map(path).sum::<usize>()
        + m.issues.capacity() * size_of::<Issue>()
        + m.issues
            .iter()
            .map(|i| i.code.capacity() + optional(&i.source_id) + i.detail.capacity())
            .sum::<usize>()
        + optional(&m.logical_blake3)
        + optional(&m.revision_id)
        + m.logical_revision.as_ref().map_or(0, revision)
}
pub(crate) fn seal(s: &crate::lightroom::migration_source::InputSeal) -> usize {
    use crate::lightroom::migration_source::{SelectedCapture, SupplementPin};
    use std::mem::size_of;
    path(&s.database)
        + revision(&s.identity)
        + s.blake3.capacity()
        + s.approval.document_blake3.capacity()
        + s.approval.scope.capacity()
        + s.approval.roster_blake3.capacity()
        + s.selected.capacity() * size_of::<SelectedCapture>()
        + s.selected
            .iter()
            .map(|v| {
                v.revision.capacity()
                    + v.family.capacity()
                    + v.family_evidence_digest.capacity()
                    + v.manifest_blake3.capacity()
            })
            .sum::<usize>()
        + s.excluded_revisions.capacity() * size_of::<String>()
        + s.excluded_revisions
            .iter()
            .map(String::capacity)
            .sum::<usize>()
        + s.supplements.capacity() * size_of::<SupplementPin>()
        + s.supplements
            .iter()
            .map(|v| {
                v.revision.capacity()
                    + v.source_id.capacity()
                    + v.origin.capacity()
                    + v.source_revision.blake3.capacity()
                    + v.proof_blake3.capacity()
            })
            .sum::<usize>()
}

#[test]
fn capacity_allocator_accounting_preserves_failed_realloc_charge() {
    let baseline = begin();
    let before = LIVE.load(SeqCst);
    // Exercise the actual failure-accounting branch without a platform-specific
    // enormous allocation or overcommit. No System pointer is fabricated/freed.
    reallocated(std::ptr::null_mut(), 41, 83);
    assert!(LIVE.load(SeqCst) >= before);
    let mut allocation = Vec::<u8>::with_capacity(4096);
    allocation.resize(4096, 1);
    allocation.reserve_exact(4096);
    assert!(allocation.capacity() >= 8192);
    drop(allocation);
    report("allocator", baseline);
}

// Dedicated BTree probes tag only the insertion interval after all borrowed
// Strings and iteration storage are allocated. Unexpected Layouts remain visible.
static NODE_TAG: AtomicUsize = AtomicUsize::new(0);
static NODES: [[NodeLayout; 16]; 2] = [const { [const { NodeLayout::new() }; 16] }; 2];
struct NodeLayout {
    size: AtomicUsize,
    align: AtomicUsize,
    count: AtomicUsize,
}
impl NodeLayout {
    const fn new() -> Self {
        Self {
            size: AtomicUsize::new(0),
            align: AtomicUsize::new(0),
            count: AtomicUsize::new(0),
        }
    }
}
fn node_layout(size: usize, align: usize) {
    let tag = NODE_TAG.load(SeqCst);
    if tag == 0 {
        return;
    }
    let Some(entries) = NODES.get(tag - 1) else {
        INVALID.store(true, SeqCst);
        return;
    };
    for entry in entries {
        let old = entry.size.load(SeqCst);
        if old == size || (old == 0 && entry.size.compare_exchange(0, size, SeqCst, SeqCst).is_ok())
        {
            let previous = entry.align.swap(align, SeqCst);
            if previous != 0 && previous != align {
                INVALID.store(true, SeqCst);
            }
            entry.count.fetch_add(1, SeqCst);
            return;
        }
    }
    INVALID.store(true, SeqCst);
}
struct NodeTag;
impl Drop for NodeTag {
    fn drop(&mut self) {
        NODE_TAG.store(0, SeqCst);
    }
}
fn tag_nodes(tag: usize) -> NodeTag {
    assert_eq!(NODE_TAG.swap(tag, SeqCst), 0);
    NodeTag
}

#[test]
fn capacity_btree_actual_reference_layouts_include_split_cascade_bound() {
    use std::collections::BTreeSet;
    let strings: Vec<String> = (0..16_384).map(|i| format!("{i:064x}")).collect();
    let baseline = begin();
    let mut references = BTreeSet::new();
    {
        let _tag = tag_nodes(1);
        for s in &strings {
            assert!(references.insert(s));
        }
    }
    let mut tuples = BTreeSet::new();
    {
        let _tag = tag_nodes(2);
        for s in strings.iter().rev() {
            assert!(tuples.insert((s, s, s)));
        }
    }
    assert_eq!(references.len(), 16_384);
    assert_eq!(tuples.len(), 16_384);
    let mut maximum = [0usize; 2];
    for (tag, entries) in NODES.iter().enumerate() {
        let mut types = 0;
        for entry in entries {
            let size = entry.size.load(SeqCst);
            if size != 0 {
                types += 1;
                maximum[tag] = maximum[tag].max(size);
                println!(
                    "CAPACITY_BTREE tag={} layout_size={size} layout_align={} allocations={}",
                    tag + 1,
                    entry.align.load(SeqCst),
                    entry.count.load(SeqCst)
                );
            }
        }
        // Root leaf plus internal nodes are both reached (including internal
        // splits). An unexpected allocator type invalidates the measured premise.
        assert_eq!(types, 2);
    }
    let nodes = 1 + (16_384 - 1) / 5;
    let bound = nodes * (2 * maximum[0] + maximum[1]) + 7 * maximum[0].max(maximum[1]);
    println!(
        "CAPACITY_BTREE_BOUND rust=1.98.0 min_nonroot_keys=5 nodes_per_tree={nodes} split_extra_nodes=7 three_set_requested_bound={bound}"
    );
    drop((references, tuples));
    report("btree-layouts", baseline);
}
