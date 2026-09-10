# sc-22842 organization query experiment — protocol and retained design

Current source compatibility revision: driver **5**, native receipt protocol **2**,
catalog schema **6**. This revision is source preparation, not a new performance
qualification. The qualified driver-4/native-1/schema-5 binaries, source archives,
private fixtures and receipts remain unchanged.

The synthetic `organization_fixture.protocol=1`, row formulas and provenance,
17 workloads, sample counts and performance budgets stay unchanged. Current
native query/transition entrypoints refuse older catalogs before `Catalog::open`;
no schema migration belongs to a timed query. `prepare` creates schema 6 directly.
For reused owned copies, run `migrate-fixture` explicitly during preparation.
Migration receipts identify `identity_scope=pre_existing_tables`: the before/after
typed hash and table counts cover exactly that old table set, including FTS and
row identities. Six newly introduced `edit_*` tables are separately named in
`added_tables` with zero counts; this is not a claim that the entire schema-6
hash equals the old schema-5 hash. A 6-to-6 verification reports no additions.

The driver preserves the original native-protocol-1 4-to-5 proof bytes, then records
a distinct 5-to-6 migration and changed physical main hash. A copied schema-6
fixture instead requires current producer evidence or a bound predecessor
migration receipt, retains predecessor proof bytes/manifest identity, and records
a separate 6-to-6 verification with unchanged physical bytes. Old-schema test
fixtures remove edit tables rather than merely relabeling a current database.

This experiment is separate from S2 backend selection. SQLite remains the selected
backend; no alternative backend/profile or threshold search is performed here.
The protocol, production code, probe, fixture formula, and coordinator must be
frozen and independently reviewed before scale preparation/timing. The original protocol-1 design is retained below, with subsequent admission corrections noted. Driver 4 completed local qualification; see the [results report](ORGANIZATION_PERFORMANCE_RESULTS.md) for exact source identities, failed predecessors and pending delivery gates.

## Workload and independent oracle

Create new, private 1, 5, and 10 million asset catalogs. The native
`organization_probe prepare` command creates schema 4 through `Catalog::open`,
then fills the actual normalized query schema directly in 10,000-row transactions.
Each asset has a unique padded filename/ID; 28 capture-date tie groups; 3 cameras;
4 lenses; alternating JPEG/DNG and labels; 6 ratings; 3 flags; 7 hierarchical term
groups; 10,000 additional high-cardinality terms; 5 folders plus recursive root;
a collection containing IDs congruent to 3 modulo 11; and a conflict marker every
97th asset. FTS contains blue-sunset terms every fifth asset and green-mountain
terms otherwise. Explicit formulas and every expected field live in the probe.

This fixture provides realistic relational cardinalities and predictable sparse
combinations, but it is synthetic normalized data. It does not measure image
import, source packet extraction, or projection construction throughput. Real
source/projection/edit synchronization is independently tested by the integration
suite and the separate transition workload on disposable copies. It must not be
presented as a catalog imported from millions of real image files.

All measured reads invoke `Catalog::search`, including its production SQL builder,
continuations, cursor validation, row decoding, and actual VM-step/sort counters.
An independent arithmetic oracle constructs expected full rows without querying
SQLite or calling the production predicate builder. It iterates ordered modulo
streams instead of allocating/sorting a million-row oracle. Complete row identity,
metadata revision, folder, filename, date, camera/lens, format, rating, flag, label,
conflicts, and provenance are checked after timing. Plans are retained as supporting
estimated evidence, not runtime proof.

Fixed cases (17): browse, rating-filtered sequence, rating sort, capture+rating, reverse filename+text, direct keyword,
high-cardinality keyword, collection, full mixed filters, FTS driving sequence,
FTS residual capture+ lens, camera+lens+format, date+camera, label+flag, direct
folder, recursive folder, and conflict-only. Sequence/filename anchors cycle
50%, 54.5%, 59%, …, 90.5% of sequence. Capture anchors alternate day 15/day 26
with deep within-day sequence ties; the bounded-date case uses days 14/18.
Rating-sort anchors alternate values 3/5 with deep sequence ties.
The high-cardinality case deliberately includes true tails with fewer than 200
remaining matches, especially at 1M; the oracle's exact count is required, and
these samples are labeled tails rather than evidence of full 200-row delivery.

Every other case returns 200 rows where available. A visible page may require
multiple production requests, each capped at 4,096 candidates. The receipt records
all chunks, candidate counts, returned counts, continuation identity, VM steps,
sorts, and request timing. Total page latency includes every continuation and row
decoding; a quick empty partial chunk is not accepted as a completed 200-row page.
A 10,000-chunk safety limit is a failed sample, never a successful exhaustion.
FTS's internal posting work is not fully reflected in VM steps: compare actual
latency/work across all scales and retain this counter limitation explicitly.

## Fixed measurement configuration and admission

Use the compiled bundled SQLite production path, 256 MiB cache, mmap disabled,
WAL, FULL durability, foreign keys enabled, and file temporary storage. Settings
from the separate diagnostic connection are identified as such; library tests
independently verify actual Catalog settings. One measurement child runs at a
time, with no competing compilation, rendering, or model campaign. Preserve the
host-observer trace and reference hardware/storage identity.

Each case/scale has 3 declared warmups and 100 measured warm pages, followed by
20 fresh-process pages with deterministic anchor indices 0–19. A fresh process
is not a cleared OS page cache; describe it precisely. Opening/setup time is
reported separately and included when interpreting first-page startup. The probe
reads only fixture control rows/high-water before timing, not full-table counts
or database hashes. Full row validation runs after timing. Preserve every failure,
raw sample, child exit, settings receipt, and source/binary hash. No favorable
retries or deletion of failed runs. All three scales are attempted.

Use the inherited indexed-metadata budgets: warmed full-page p95 <=100 ms and
fresh-process full-page p95 <=500 ms, aggregated from all 20 native fresh-page
samples per case/scale. Native page time includes every continuation chunk and
TEMP text operation. Child startup/whole-process wall time remains a separate
extra guard (<1 second for each fresh child), not a substitute for the 500 ms
query budget or proof of the retained-thumbnail first-visible-page target.
Browse-only process RSS must be <=4 GiB. Compute quantiles using linear interpolation on
all raw samples. Report p50/p95/p99/max and exact returned counts. A numerical
pass alone does not prove bounded engine work. Compare deep work and sorts across
scales, including sparse filters and FTS. A requirement failure remains visible;
any corrective query experiment must be separately frozen and reviewed.

Fixture-generation time, storage size, index size, and peak preparation RSS are
diagnostic. Preserve pristine catalog main-file hashes before queries and verify
them after all read-only measurements. Sources may acquire empty SQLite WAL/SHM
companions; retain and report them, never erase companions to make evidence pass.
Do not query/hash large databases while another timed lane is active. Check free
space before preparation and use exclusive paths; never touch the user's photo
RAID. Supplemental write/source-transition work must use separate verified copies
so it cannot alter the frozen read workload.

The coordinator and transition workload are described below. Their final smoke, scale and review evidence is recorded in the results report; this protocol alone is not acceptance evidence.

## Coordinator and source transitions

`benchmarks/organization_campaign.py` has explicit prepare, measure, and
transitions phases. It requires a root-owned build reference containing exact
`binary_sha256` and 40-character `source_commit`; preparation records that receipt
and the coordinator's complete source/hash. Subsequent phases refuse a changed
binary/build reference/coordinator. Each phase reserves exclusive output paths;
failures are retained and a repeated invocation cannot overwrite them. Native
stderr and child exit/RSS/elapsed receipts remain beside every native result.
Children are serial, polled every 20 ms, and terminated/reaped on sampling failure
or timeout. The reader validator independently checks fixture formulas, cursor
anchors, full row values, sample coverage, settings, tails, and actual sorts. The
fixed read-query deadline is 30 minutes per child; preparation is four hours per
scale. Exceeding either is a recorded failure. `--smoke` uses 1,000 assets, two warm
samples after one warmup and two fresh processes; it cannot count as scale proof.

After terminal read measurements, the transition phase creates a separate full
copy of each pristine main database, verifies source/copy hashes and unchanged
source companions, and records copy provenance. Nonempty WAL/journal data blocks
copying. It adds 200 metadata-only cohort assets to each disposable copy; original
synthetic read rows remain unchanged. The foreground makes 200 point saves
(100 rating, 100 label) and reads 200 pages from one genuine snapshot while a
second Catalog connection retains 200 actual parsed XMP source observations.
The background repeatedly updates the same 100 source identities with new labels;
every resulting source/model revision and indexed label is checked. Generated XMP
files include an unrelated structured property and are hashed before/after; these
are private disposable inputs, never the user's files.

The foreground saves affect a separate 100-asset cohort. Exact revisions,
organization field values, pixel-generation stability, and all 40,000 snapshot
rows are verified; a reopened catalog proves the final saves persisted. Packet
inspection occurs before the background retain timer. Source preparation and
cohort insertion occur before either timed worker. This measures real S4 source
refresh/projection synchronization and organization contention, not full photo
import/decoding throughput. The existing real-import tests separately establish
that the import hook invokes these same transitions. Native receipts retain
individual errors and all attempted samples.

Report save and snapshot page distributions independently. Apply the inherited
100 ms p95 to point saves and foreground page delivery, both across the fixed
full populations and across the actually overlapping save/read populations.
A zero-overlap population fails admission. Background source-refresh
timing and whole mixed-process RSS are diagnostic; do not impose a new mixed RSS
limit. The separate browse-only runs retain the 4 GiB gate. Source inputs and
pristine database hashes must remain unchanged. Final bounded-work interpretation,
representative host load, integration CI, and independent review remain required;
neither coordinator summary is full-story Done evidence.

## Build identity and overlap attribution

Scale runs use the **release** probe built from the frozen reviewed head with:

```sh
CARGO_BUILD_JOBS=4 OMP_NUM_THREADS=1 \
PHOTOCATALOG_DNG_SDK=/absolute/path/to/pinned/dng_sdk_1_7_1 \
cargo build --locked --release --bin organization_probe
```

The root build receipt records the exact resolved command/SDK and target paths,
release profile, source commit, binary SHA-256, and build-log SHA-256. This receipt
is required by every phase. Debug builds are allowed only with `--smoke`, and are
labeled debug. Existing pinned native dependencies and compiler are unchanged;
record their exact checkout/build versions with the receipt rather than assuming
that a binary's filename proves its source.

Every child observer records begin/end UTC and monotonic timestamps for alignment
with the existing host-observer timeline. Native transition operations share one
monotonic origin anchored to Unix milliseconds; each retain, save, and snapshot
read records its begin/end interval. The coordinator reports overlapping and
nonoverlapping subsets separately, including sample counts and distributions.
The full-run budgets remain, and the existing 100 ms target also applies to
the actually overlapping save and snapshot-read populations. This prevents fast
operations after background completion from concealing stalls during contention.
Both populations and all samples remain visible; no favorable subset is selected.
A start barrier alone is not overlap proof. Zero overlapping saves or reads fails
the driver4 gate. See CATALOG_WRITER_ADMISSION.md for the preserved v3 failure
and production correction.
