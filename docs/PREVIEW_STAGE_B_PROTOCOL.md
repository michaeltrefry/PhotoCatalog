# Preview Stage B execution protocol 1

Status: Mac correctness gate passed; the campaign is unrun. Independent review and the parent's
explicit hardware-lane grant are required before execution. This supplements
PREVIEW_EXPERIMENT_PROTOCOL.md; it does not replace its workloads or acceptance
budgets. The sealed 512/1600 JPEG80 selection and historical Stage A artifacts
remain unchanged. Stage A timing was exploratory under recorded background load.

## Worker memory gate

The first gate uses the same frozen 30-input manifest (22 private, two CC0 public,
six procedural) and its independent source dimensions. Source paths stay outside
Git. It launches one fresh `preview_runtime_probe worker` for each input, in frozen
manifest order. Each probe launches the actual application's `--preview-worker`,
using the production owner lease, full-original decoder, preparation, two selected
encodes, and complete result validation. No embedded-thumbnail substitute, render
retry, per-camera setting or parallel worker is permitted. There are 30 owner
probes, 30 native worker grandchildren and 30 untimed artifact-verification probes.

Freeze an explicit DecodeLimits JSON and its hash with the probe/application
binaries and manifest before running. The initial isolated memory qualification
uses the full decoder's compatibility limits: encoded input 536870912 bytes,
intermediate pixels 18446744073709551615, individual allocation allowance
18446744073709551615. Historical native/raster allocation and 100 MP final-surface
limits still apply. These are individual admission checks, not an aggregate RSS
limit or a production preview configuration. The isolated cohort establishes
whether the provisional worker allowance can actually accommodate the complete
pipeline before that allowance is frozen for a service experiment.

Both JPEG80 outputs are requested together with an 8 MiB encoded-output limit.
Each worker records process `getrusage(RUSAGE_SELF)` high-water RSS after full
source decode, both prepared/encoded tiers, and final source hash verification.
macOS returns bytes; Linux KiB are converted to bytes. Only the bounded 64 KiB
receipt serialization/write follows that sampling point. Windows explicitly
reports this measurement unavailable and cannot pass the Mac memory gate by
substituting zero. The owner independently materializes both resulting RGB8
surfaces through the production decoder; its process memory is separately observed
by host telemetry and is not mislabeled as the native child's high-water value.

The proposed worker reservation is fixed as
`ceil_MiB(maximum_of_30_child_peaks * 1.25 + 64 MiB)`. The 25 percent is an explicit
accounting margin for input/platform variance, and 64 MiB covers bounded owner
receipt, validation and encoded buffers. This formula is not a newly invented
performance acceptance threshold, proof for untested cameras, or aggregate OS
enforcement. The coordinator must review all 30 peaks and freeze the resulting
reservation and total worker allowance before any concurrency experiment. The
existing 4 GiB browse RSS gate remains separate. If the cohort fails or memory
telemetry is unavailable, no reservation is admitted from a passing subset.

The probe records failure phase, completed artifacts and anchors even when a
later operation fails. Each saved JPEG is reread, hashed, checked for dimensions
and completion, fully decoded and reconciled against its recorded decoded RGB8
digest in an untimed verifier. Python binds independent SHA-256 identities to the
actual files and receipts. Source SHA-256 values are checked before and after the
whole campaign; each worker also verifies its own source BLAKE3 before/after work.
The process timeout is 300 seconds, coordinator timeout 360 seconds; timeout kills
and joins the owner, whose EOF lease terminates its native child. A failure stops
the campaign and is retained. No best-of or replacement samples are collected.

The coordinator starts the reviewed passive host observer before the first child.
Every child and verifier has UTC plus monotonic boundaries, so host observations
can be aligned without reconstructing intervals. Host/OS/RAM/storage provenance,
manifest/limits/binary/source/Cargo-lock identities, stdout/stderr and partial
receipts are preserved in a new exclusive private output directory. Missing GPU
data remain unavailable; neither successful execution nor telemetry collection
awards a quiet-host claim. APFS Data/firmlink storage attribution must be recorded
from actual paths, retaining uncertainty instead of treating `/` as proof.

Execution, only after review and grant:

```
python3 scripts/preview_worker_campaign.py --manifest PRIVATE_MANIFEST \
  --limits FROZEN_LIMITS --binary ABSOLUTE_RELEASE_PROBE \
  --worker ABSOLUTE_RELEASE_APP --output NEW_PRIVATE_DIRECTORY \
  --lane-token coordinator-authorized
```

## Remaining Stage B gates

Current-renderer qualification prepares new lossless references for all 30 inputs
and the selected pair. Exact unchanged prepared-pixel digests can reuse sealed
visual judgments; changed references (including the repaired G15 RAW highlights)
require inspection of the selected JPEG80 outputs against the new reference.
No historical source, reference or blind judgment is rewritten. These additional
quality preparations do not run inside the native memory measurement child.

Flat versus two-level prefix layout uses 10,000 and 100,000 distinct logical
asset/revision keys and genuinely byte-distinct valid JPEG objects. For each
synthetic entry, insert one JPEG COM segment immediately after SOI, carrying the
fixed ASCII prefix `photocatalog-layout-v1:` and its zero-padded 10-digit entry
number. The 30 selected JPEG80 payloads are used round-robin; this lossless metadata
construction changes encoded bytes/content hashes while preserving decoded RGB8.
Verify decoded equality against the corresponding selected source payload, and
assert distinct content hashes, keys, actual files and directory entries. The
same entry number and source payload produce identical bytes in both layouts.
Report the constant 37-byte marker overhead per object separately and disclose
the 30-image pixel diversity. No content-hash deduplication is enabled. Record payload bytes, file allocation,
directory allocation/count, manifest/index/WAL/lock/marker overhead. The fixed
three sequential and three seeded random passes include read/checksum and preserve
first-pass versus later OS-cache state. This fixture construction cannot forecast
the user's real library format frequency or filesystem footprint by itself.

The subsequent production-service harness must preserve the original frozen
200-thumbnail first-page, 100 warm/20 fresh process trials, 100-viewport navigation
trace repeated ten times, standard/constrained cache and queue limits, offline
originals, and exact quota/disk-failure gates. It must call `Catalog::browse` and
`PreviewService::cached` with retained returned-pixel ownership included in RSS.
DB lookup, read/hash, full RGB8 decode, queue delay and wall intervals are recorded
separately. The service source checkpoint is not measurement evidence. Layout and
resource defaults stay configurable and unfrozen until these gates are reviewed.
The existing first-visible-page target applies only to the measured headless
component here; desktop/UI/frame-time proof remains S12 work.

The worker-memory probe/coordinator passed its tiny Mac correctness gate. The
layout probe is now source-ready and unrun: `preview_layout_probe prepare` creates
one selected layout/count using the successful worker campaign, and `lookup`
performs three sequential passes followed by random passes with seeds
22841/22842/22843. It uses actual `PreviewStore::publish_record` (including complete
render records) and `read_limited` with 8 MiB encoded admission, production
deferred access touches, one manifest connection and no decoded image cache.
Every returned object is independently checked outside its per-lookup timer:
the COM segment must carry the expected entry number, and hashing the remaining
original JPEG bytes must match the selected seed digest. Each pass requires
exactly N distinct actual returned-content hashes. This verification is included
in pass wall time and reported separately; manifest checksums alone are not the
experiment's payload oracle. Duplicate fixture identities are rejected before
preparation, while legitimately identical base pixels remain disclosed.
Raw per-lookup samples are written after each pass; each pass also records wall
time, boundaries, verified bytes and failures. Footprints are captured after
preparation and before/after lookup, including actual lock/marker/index/WAL files.

The production service has an opt-in `cached_with_metrics` wrapper around the
same internal read/decode implementation as `cached`. It records catalog identity,
manifest/read/checksum, header/decode, total duration and decoded-cache hits/misses;
ordinary calls do not collect clocks. Instrumentation overhead is included in
measured wall time and is never subtracted. Service/navigation coordinators and
remaining fault coverage are still required S6 work; no timing or layout choice
is implied by these source additions.

## Retained service probe specification (source checkpoint; unrun)

`preview_navigation_probe` uses the same bounded dataset schema as the layout
probe. Preparation requires the 10,000-object variant and creates a new, separate
catalog containing exactly those sequence/key/source-fingerprint generations.
This direct SQL seed is a disclosed synthetic fixture, not import evidence.
The complete retained RenderRecord and selected JPEG objects are the layout
fixture's actual records. Every synthetic original points into a directory that
is never created; preparation and run admission reject an existing originals
root. No native renderer is launched in these retained-only workloads. Layout
is taken from the bound dataset and recorded, never implicitly selected here.

The first page consists of catalog sequences 1..200, corresponding to fixed
layout indexes 0..199. Each trial invokes the production `Catalog::browse`,
`PreviewService::queue_read`, `tick_read`, and `take_read` path. Request admission,
FIFO foreground scheduling, catalog identity revalidation, manifest/read/hash,
complete RGB8 decoding and caller-held pixel reservations are included. A page
owns all 200 returned views until its wall timer stops. Expected key, dimensions
and decoded RGB8 BLAKE3 are checked against the corresponding independent seed
outside the page timer; verification time is reported separately. Timer overhead,
request bookkeeping and in-timer identity assertions are included, not subtracted.

Profiles are fixed as follows. Standard uses SQLite's measured 256 MiB per
connection setting, 256 MiB decoded LRU and live-pixel allowance, 32 MiB encoded
staging, 400 shared request descriptors, and one retained decode per owner call.
Constrained uses the same DB profile, 32 MiB LRU, 256 MiB live-pixel allowance,
8 MiB staging, 200 descriptors and one decoder. Its per-native-worker encoded
reservation is 4 MiB (half staging) to satisfy the service's foreground-headroom
invariant; no original worker is admitted here. Standard's corresponding
reservation is 8 MiB. All remaining limits are emitted in each receipt; native
working-memory defaults are irrelevant to these retained-only runs and remain
subject to the separate worker gate. The visible page can outlive LRU entries,
but its bytes remain charged to the live allowance and process RSS.

The `warm` command runs three warmups, 100 measured pages, then one separately
labeled hot-LRU observation. Each warmup/measured page drops the prior returned
views and clears the LRU before DB lookup. Connection and OS cache remain warm;
encoded bytes are not retained between reads. The hot observation preserves
whatever LRU entries the configured profile can retain; it is not assumed to be
100 percent hits. `fresh` runs exactly one page in a new process. The external
coordinator invokes exactly 20 fresh children per profile; it does not evict OS
cache or call this cold-disk evidence. Output files are written after each trial.

`navigation` performs ten traces, clearing LRU and dropping views between traces.
Each trace schedules 100 viewport changes at absolute offsets 0, 50, ... 4950 ms.
Viewport v uses indexes `(50*v + 0..199) mod 10000`. At each change the owner
cancels out-of-view pending tickets, releases out-of-view returned pixels, retains
the intersection, and submits missing visible identities in page order. It then
runs at most one real retained decode before receiving the next input. If an
owner call overruns, every scheduled viewport is delivered in order with its
actual delay recorded; the trace clock is never moved to conceal missed input
deadlines. Completion after a canceled/unowned ticket is an error. The final
viewport must have all 200 current views. Each returned image is independently
verified after its owner-call timer; that verification remains in trace wall time
and is separately reported. This instrumentation cost is not silently removed
from navigation responsiveness. A 60-second per-trace safety deadline preserves
a failure, not a replacement trace.

Raw receipts retain request, cancellation and completion identities/order,
per-read outcome (`ready`, `missing`, `stale`, typed `resource_limit`, or `failed`),
error detail, queue wait, owner read wall, instrumented read/decode components,
cache hit/miss counts, visible ownership, queue peaks and input overruns. A
required missing/stale/error result fails the trial and preserves its partial
observations. No failed admission is relabeled as an idle/missing thumbnail.
A successful retained trace has zero native jobs, so it makes no concurrent
full-original-import latency claim. Actual service/worker import-interleaving,
resource admission and crash/failure correctness remain separate required gates.

Process high-water RSS is emitted after each page/trace (macOS bytes; Linux
KiB normalized; Windows unavailable). Warm per-trial high-water values are
cumulative for that process, not resettable interval peaks. The coordinator must
also preserve host telemetry, complete clean source/archive and binary identities,
fixture/catalog/cache manifests, physical storage attribution and child UTC plus
monotonic boundaries. It must report the original 1-second warm first-page and
4 GiB browse RSS gates without inventing acceptance thresholds from observations.
Desktop frame-time/UI evidence remains unavailable until S12. The source-ready
probe and fixed commands do not authorize execution or select a layout:

```
preview_navigation_probe prepare --dataset ABS_10K_DATASET_JSON --output NEW_PRIVATE_FIXTURE
preview_navigation_probe run --fixture ABS_FIXTURE_JSON --worker ABS_APP \
  --output NEW_PRIVATE_RUN --profile standard --workload warm
# profile: standard|constrained; workload: warm|fresh|navigation
```

The new shared fixture module and navigation probe have been formatted only.
Compilation, correctness tests, coordinator validation and independent review
remain required before any timed service child is admitted.

The complete 30-worker memory gate has since established a frozen **Stage B
experiment allowance of 2,269,118,464 bytes (2164 MiB) per original worker**,
with the existing 3 GiB total working allowance. Both headless probe profiles
record that allowance even though they do not launch original workers. This
cohort-derived setting is not a production default or a guarantee for all future
camera inputs. Two such workers do not fit simultaneously: the actual-child
correctness test configures two worker slots and requires one active/one queued,
then successful progress when the first reservation is released. It does not
launch two simultaneous full renderers to manufacture a peak measurement.

The source-ready `preview_navigation_campaign.py` fixes **44 measured children**:
for standard, then constrained, one warm child, 20 fresh children and one ten-trace
navigation child. Each has one untimed `preview_navigation_probe verify` child,
for **44 receipt verifiers**. The verifier reconciles exact fixed trial filenames,
compiled source identity and trial-file BLAKE3 before the Python coordinator
performs independent count, consumer-conservation and finite-value checks.
The coordinator also records actual SHA-256 for all accepted trial files. Every
child has UTC/monotonic anchors, exclusive stdout/stderr/results and a 900-second
safety deadline; timeout terminates and joins only that owned retained-work child.
There are no native grandchildren in this campaign, retries or replacement trials.

Before admitting a measured child, the coordinator requires the six-pass 10k
layout prerequisite with 10k independently verified distinct payloads per pass,
bound to the exact dataset digest. It also requires an externally prepared,
reviewed JSON binding with `version: 1`, `clean: true`, a 40-character
`source_revision`, `planned_measured_children: 44`, `planned_verifiers: 44`, and
SHA-256 fields named `binary_sha256`, `worker_sha256`, `archive_sha256`,
`storage_sha256`, `fixture_sha256`, `dataset_sha256`, `layout_receipt_sha256`,
`protocol_sha256`, and `coordinator_sha256`. The archive is the complete clean
build source; the storage artifact preserves actual APFS Data/path attribution.
All bound files are rechecked at campaign end. Neither this mechanical binding
nor its lane token substitutes for independent review and the parent's explicit
quiet-lane grant. Passive host observation must start before the first child and
complete successfully; it does not judge quietness automatically.

Aggregate reporting uses fixed nearest-rank p50/p95/p99/max with all samples and
no outlier deletion. Warmup, warm, fresh, hot-LRU and navigation remain separate.
It reports warm headless p95 against 1000 ms and component process high-water
against 4 GiB. **The original memory acceptance names 10 million metadata records;
this fixed 10k catalog component cannot award that integrated 10M gate.** A final
integrated catalog-plus-preview RSS observation is still required. UI timing is
also unavailable here. A successful campaign flag means the fixed work and
identity/oracle checks completed, not that every budget or full S6 requirement
passed. Failures retain original per-read resource/error classification and all
preceding observations.

```
python3 scripts/preview_navigation_campaign.py --binary ABS_NAVIGATION_PROBE \
  --worker ABS_APP --archive CLEAN_SOURCE_ARCHIVE --storage STORAGE_EVIDENCE \
  --fixture ABS_FIXTURE_JSON --layout-receipt ABS_SUCCESSFUL_10K_LOOKUP_JSON \
  --binding REVIEWED_BINDING_JSON --output NEW_PRIVATE_DIRECTORY \
  --lane-token coordinator-authorized
```

The coordinator, verifier, ownership-contract tests and two-child reservation
regression are source-ready and UNRUN pending the native correctness lane.

The source-ready `preview_layout_campaign.py` makes layout execution reproducible:
prepare 10k flat, 10k prefix, 100k flat, 100k prefix, each in a separate new folder;
then run one six-pass lookup child for each in that same fixed order. This is four
preparation children plus four measured lookup children, with zero retries or
replacement passes. Lookup OS state follows all preparations and prior lookups;
it is recorded and is never described as cold. Preparation has a 3600-second
safety deadline per group; lookup has 900 seconds. Only owned children are killed
and joined on timeout; partial files/receipts remain available for inspection.

Admission binds the verified 30-worker campaign and its actual selected 512 JPEG
files against their independent SHA-256 receipts, plus clean source archive,
layout binary, protocol/coordinator, and physical-storage evidence. The JSON
binding uses `version: 1`, `clean: true`, exact `source_revision`,
`planned_preparation_children: 4`, `planned_lookup_children: 4`,
`minimum_free_bytes`, and `binary_sha256`, `archive_sha256`, `storage_sha256`,
`worker_campaign_sha256`, `protocol_sha256`, `coordinator_sha256`. Free capacity
must meet that reviewed minimum before execution. The minimum must cover the
exact projected encoded bytes for all 220,000 objects plus an explicit 2 GiB
headroom floor for indexes, metadata, directory/file allocation and receipts.
That floor is resource admission, not a filesystem overhead prediction or a
guarantee against competing disk use. Actual allocated/logical footprint remains
the result. The constant COM construction contributes 8,140,000 encoded bytes
across these four filesets. No dataset is deleted automatically to fit capacity.

Every accepted pass must contain its fixed count of actual distinct returned
payloads and actual files, the original pass/seed identity and exactly N finite
raw samples. The coordinator retains raw samples and their SHA-256, and reports
nearest-rank distributions for all six passes individually, including first-pass
and later-pass differences. It records preparation/lookup footprints and keeps
file versus directory allocation and manifest/marker/index bytes visible. Final
comparison requires all four groups and equal encoded-payload totals for the two
layouts at each count; it does not pool passes into a more favorable result or
pick a layout automatically. All bound source files, seed JPEGs and generated
dataset descriptors are rechecked; failures retain partial observations. Host
telemetry starts before the first preparation and must complete, without itself
awarding quiet-host timing. No timing or layout selection has yet been executed
from this new source.

```
python3 scripts/preview_layout_campaign.py --binary ABS_LAYOUT_PROBE \
  --archive CLEAN_SOURCE_ARCHIVE --storage STORAGE_EVIDENCE \
  --worker-campaign ABS_VERIFIED_WORKER_CAMPAIGN_JSON \
  --binding REVIEWED_LAYOUT_BINDING_JSON --output NEW_PRIVATE_DIRECTORY \
  --lane-token coordinator-authorized
```
