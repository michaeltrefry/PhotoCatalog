# S8 editing and derivative qualification — prospective protocol 1

This is the first source checkpoint, **not an admitted experiment or acceptance
result**. No harness build, correctness execution, source-image read, or measurement
has run for this checkpoint. The executable probe uses the production editor,
PreviewService and ExportService. The Python entry point currently writes a plan;
it cannot launch children. Remaining required implementation is listed below.
The final integrated source, including writer admission and export phase metrics,
must replace the development base before a reviewed execution request is frozen.

## Fixed acceptance contract

These targets were approved prospectively for S8; they are not inferred from S6:

| Operation | p95 ceiling | Samples per configuration |
| --- | ---: | ---: |
| Warm 1600 linear-input recipe kernel | 100 ms | 2 warmups + 100 |
| Current 1600 preview delivered through service | 250 ms | 2 warmups + 100 |
| Combined recipe on already developed original, source ≤32 MP | 5 s | 2 warmups + 20 |
| First RAW decode and edited 1600 delivery | 20 s | 2 warmups + 20 |
| Durable JPEG export | 25 s | 2 warmups + 20 |
| Durable PNG export | 40 s | 2 warmups + 20 |
| Durable TIFF export | 30 s | 2 warmups + 20 |
| Durable edit acknowledgment during actual import overlap | 100 ms | 100 overlapping saves |

Report every camera/format/dimension and configuration separately, with all raw
samples and nearest-rank p50/p95/p99/max; never average cameras to hide a failure.
Warmups remain in receipts but are excluded from these distributions. All failures
are retained and stop further admission; no automatic retry or tuning loop. Desktop
frame delivery remains S12. Export-overlap foreground latency is an additional
required diagnostic using the same 100 ms acknowledgment target, not a replacement
for actual import overlap.

The previous 30-source S6 worker evidence is provenance and feasibility context,
not S8 performance qualification. Its largest observed process was 1,760,837,632
bytes, and its derived reservation was 2164 MiB. Those workers produced retained
previews with an older source identity. They did not run this recipe/export matrix.

## Cohort and fixed operation registry

The exact 30 IDs in `scripts/edit_qualification.py::IDS` bind the already validated
S6 manifest: 24 camera/public sources and six procedural preparation fixtures.
The prospective request embeds each manifest row and the manifest SHA256. Original
photos and Lightroom sources are read-only. Each input gets one ordinary exclusive
copy in its own directory; verify the original/copy byte hashes before and original
hashes after the campaign. Never hard-link originals, import a user's parent
directory, or describe private SSD-copy measurements as RAID throughput.

The eight timing inputs are Canon 5D IV CR2, Fuji X-T3 RAF and float DNG, DJI
L1D-20c DNG, public Panasonic GX7MK2 RW2, Canon 6D PSD and TIFF, and G15 JPEG.
The first five form the RAW subset. Export timing uses 5D IV CR2, X-T3 float DNG,
and G15 JPEG. These choices include the largest current source, the prior memory
peak, several native decoders, integer/float DNG and composites. The exact source
dimensions remain independently supplied by the manifest, never decoder-derived.

The source registry fixes 14 recipes: neutral, crop, straighten, exposure, WB,
contrast, highlights, shadows, saturation, vibrance, sharpening, luminance noise,
chroma noise, and combined. The registry contains complete values and an A/B pair
for each operation. Canonical recipe identity comes from Rust validation, alongside
the independent request-map digest. Service trials alternate the fixed pair to
advance durable revisions; neutral has kernel coverage only. Two warmups cover
both states. Every non-neutral operation receives its own 100 service samples;
every operation, including neutral and combined, receives 100 kernel samples.

White balance occurs during original preparation before the relevant native
development/profile operations. Therefore the WB kernel row measures processing
of its prepared WB basis, **not WB preparation latency**. WB service trials reuse
the two correctly identified prepared bases after warmup. First-RAW trials disable
the prepared cache and measure actual decoding each time; they make no cold OS
page-cache claim. Prepared proxies are scene-linear floats, never browse JPEGs.

The fixed timing export matrix is JPEG quality 90 / 8-bit sRGB on explicit linear
white; PNG 8/16-bit sRGB with straight alpha; TIFF 8/16-bit sRGB with straight alpha;
and TIFF float32 linear sRGB with straight alpha. Original output dimensions are
the post-recipe crop dimensions. All six configurations run for each export input.
Correctness applies those six outputs to the combined recipe for all 30 inputs.

This first registry contains 337 probe cases: 90 correctness, 112 kernel, 104 warm
service, eight full, five first-RAW, and 18 durable export. Correctness includes
two independent-process pixel passes per source and one combined export pass.
This is **not the final campaign child count**: independent verification, generated
analytic/100-MP cases and foreground-overlap cases must be added and frozen first.

## Timer, ownership and memory boundaries

Kernel clocks bracket `render_recipe` on a retained prepared 1600 input. Full
clocks bracket that call on an already decoded original. Preparation, source
hashing and pixel verification are outside these timers and separately disclosed.
Service clocks start before the actual interactive request and end after current
key validation and materialized cached RGB retrieval. Durable recipe persistence
precedes that clock and receives its separate overlap gate. No old revision or
approximate result may satisfy the current key.

Export clocks start before durable job creation and include plan append/seal,
worker admission, original decode, exact recipe, metadata/profile conversion,
encoding, authority verification, publication and commit. Output verification
after return is outside that timer. Actual worker and publication subphase metrics
will be consumed from the bounded production completion API; unavailable phases
are never estimated by subtraction. Sums are not expected to equal end-to-end time
because queueing, polling and overlapping/inclusive intervals are reported as such.

Proposed normal admission is one worker, 4 GiB working reservation, 2 GiB individual
allocation, 32 MP rendered surface, 512 MiB encoded input/output extent and a 256
MiB/16-entry prepared cache. Source pixel admission and native intermediate pixels
are distinct; the final request must account for sensor margins before freezing
that decoder limit. RSS is empirical, not enforced by allocation accounting.
An external owner samples each process and the descendant group every 0.1 seconds;
proposed stops are 4 GiB per process and 4.5 GiB aggregate. Final production worker
reservation requires successful full-cohort peak-plus-margin evidence. The 4 GiB
browse gate does not certify editing-process memory.

Proposed child deadlines are 900 seconds for correctness/kernel/full/warm service,
1200 for first RAW, and 1800 for each export configuration. The coordinator must
own one process group, terminate and reap its known processes on failure, retain
partial output/logs/observations, and refuse further admission if ownership is
uncertain. Minimum free space is proposed at 96 GiB; observed output stop is 128
GiB. These are resource safeguards, not relaxed operation-time acceptance targets.
All paths/configs and total child/disk bounds need a reviewed request before run.

Host telemetry records UTC/monotonic anchors, CPU, RSS, swap, disk activity and
available GPU observations across actual sample intervals. Missing telemetry is
unavailable, not idle. Record APFS Data attribution, hardware/software versions,
whole source archive, app/probe/verifier identities and native dependency sources.
No uniform quiet-host claim follows from exclusive team ownership alone.

## Correctness and remaining execution gates

The probe records source identity, recipe digest, pixel digest, finite/alpha
counts, dimensions, provenance, output descriptors and partial sample receipts.
It always reports `qualification_complete: false`: these observations are not an
independent mathematical/color/codec oracle.

Before execution admission, the next harness checkpoint must implement and freeze:

1. Independent analytic float fixtures and reference equations for every operation
   and their fixed combination, signed/HDR RGB, straight alpha, orientation, crop
   rounding, rotation and boundary pixels. Freeze operation-specific absolute and
   relative tolerances from equations/precision, not observed errors. Repeat-source
   identity tests alone cannot establish correctness. Include raw WB/profile
   references with an independently defined camera matrix/neutral transform.
2. Independent output readback for JPEG, PNG and TIFF, every declared depth/profile,
   alpha/compositing and size/upscale option, resolved XMP/extended JPEG XMP,
   physically oriented safe EXIF and exact ICC bytes. Custom linear and nonlinear
   RGB ICC fixtures and unsupported combinations must be explicit. No silent
   omission or comparison solely through the production decoder.
3. A genuinely materialized deterministic 100-MP source and full operation/export
   support with a separately admitted larger allowance, plus the same source's
   lower-limit refusal before allocation. This is not a ≤32-MP timing award.
   Bounds and generated fixture bytes must be frozen before generation.
4. Actual foreground recipe saves while import/export workers are demonstrably
   active, with all 100 accepted samples inside the overlap interval; worker
   cancellation, stale completion/undo ABA, resource refusal, source preservation,
   disk/extent failures and crash recovery. Existing core tests may provide bounded
   correctness evidence, but cannot substitute for measured overlap.
5. Final completion-metrics integration, independent verifier, serial owned-process
   watchdog, immutable execution bindings, negative receipt/admission tests, final
   source review and native correctness gate. The current planner has no run mode.

The first source checkpoint and its tests are **UNRUN**. These gates remain within
S8; this document neither defers them to another story nor awards completion.

## Second-checkpoint oracle design (source only, not admitted)

`edit_reference.py` independently evaluates the specified equations with float64
linear algebra and direct 2D neighborhood sums. It does not call product code or
LCMS. The initial generated sources use a known matrix ICC profile and float32
TIFF, with signed/HDR channels and straight alpha. `edit_fixtures.py` emits bounded
scanline buffers, including a separately admitted 10000×10000 source. The 100-MP
fixture's pixel payload is exactly 1,600,000,000 bytes, plus TIFF/ICC/strip metadata;
its generation does not allocate that payload as one Python array.

The proposed small-fixture comparison bound is `2e-5 + 2e-5*abs(expected)` per
component, or `4e-5 + 2e-5*abs(expected)` when geometry changes. These are declared
before results, to accommodate float32 operation accumulation, fixed-point ICC
matrix quantization and nonlinear propagation; they are not measured tolerances.
Masks/dimensions/channel declarations additionally have exact structural checks.
The analytic implementation uses the published Robertson brackets for the two
fixed WB temperatures and a float64 Bradford solve. It is not a general CCT oracle.
Raw-camera WB still needs its separately defined camera-space fixture/reference.

Independent readback uses imagecodecs (libjpeg/libpng) and tifffile, not the product
codec path. Header parsers verify ICC fragmentation, PNG metadata CRCs, extended
XMP byte coverage/GUID, and offset-safe EXIF reconstruction. Lossless integer
comparisons propose maximum 2 LSB at 8-bit and 4 LSB at 16-bit, including transfer
rounding; float output uses the geometry tolerance above. JPEG's constant analytic
patch has a prospective 3-LSB DC/YCbCr bound at quality 90. General photographs'
lossy differences are reported without inventing a whole-image JPEG pixel limit.
These checks require actual execution and independent source review.

Scientific dependencies currently available in the prior private reference
validation environment are NumPy 2.5.3, tifffile 2026.8.23 and imagecodecs 2026.8.16.
The final execution binding must pin and report those actual versions; ordinary
stdlib CI can exercise scalar contracts, but skipped scientific tests confer no
acceptance. Neither fixture generation nor any oracle has been executed here.

The preliminary disk figures above are superseded as an unresolved admission
proposal: 96 GiB cannot fund a 128 GiB cap. The final coordinator must calculate
copies + retained artifacts + largest active child + emergency reserve, and require
that entire amount free. Approved retention direction is to independently verify
all 22 exports of each successful timing child, retain its first measured output
plus every sample/hash/readback receipt, and remove only other verified successful
outputs. Retain every failure and partial artifact. All camera correctness outputs
remain retained. Exact arithmetic and the live-free-space stop remain mandatory
before an execution request can be admitted; no disk cleanup has run.

## Integrated probe and supplemental request checkpoint

The source probe now consumes typed `EditInputProvenance`, checks current
variant/revision/key, observes a newly active worker PID for every delivery, and
requires each timed warm sample's consumed proxy fingerprint, WB, renderer, edge,
original dimensions and digest to agree. First-RAW requires `OriginalDecoded`.
New, previously unseen WB values remain cold work; the warm result covers exactly
the two prewarmed values. Owner PID observations are not OS liveness proof: the
external process telemetry binds PID plus creation time and excludes zombies.

Supplemental source requests currently add 160 format/profile/size/alpha cases,
eight independent-process analytic passes, eight full recipe-pair proxy references,
two 100-MP admission cases, six typed failure cases, six durable metadata exports,
and two foreground overlap cases. Together with the preliminary337 registry this
is 529 proposed probe children; verifier/generator children and exact disk bounds
are still to be finalized. No automatic execution plan is produced from this count.
All four profile paths are represented: built-in sRGB, built-in linear sRGB,
custom linear matrix ICC and custom nonlinear matrix ICC where supported.

The controlled metadata packet contains a named RDF subject, unknown qualified
and structured properties, Bag/Seq/Alt arrays, multilingual text, an80KB JPEG
extension payload, safe EXIF source values, active CRS settings and stale technical
fields. Direct encoding must retain the exact supplied packet semantics. The six
durable-service cases select the sole controlled full metadata base and require a
separate explicit derivative expectation: preserve unrelated metadata and original
packet bytes while replacing technical fields and removing active develop/source
pointer properties. A direct encoder check is not evidence of this durable policy.

The foreground probe uses a distinct catalog connection and virtual copy, so saves
do not invalidate the background export's master plan. Import advances and export
polling run continuously on the background thread. All100 saves must begin/end
inside the same owned worker lifecycle; no delay/holding checkpoint is inserted
into background work. The external audit must additionally establish the actual
process lifetime. Failed overlap aborts acceptance and retains its evidence.

The serial coordinator rejects any binding with unresolved execution gates,
unfunded disk bounds, nonfinite limits, changed build/protocol artifacts or escaped
action paths. It samples owned process identities/RSS/free space at0.1seconds,
caps stdout/stderr at4MiB each and process telemetry at32MiB per child, handles
SIGINT/SIGTERM with known-process termination/reaping, and never retries. SIGKILL
or host failure still requires explicit ownership reconciliation before another
run. The first prototype planner remains non-executable because the final
aggregate/oracle/funding gates are unresolved. No generated source or process has
been launched by this harness checkpoint.

Reference bindings add [blake3-py1.0.9](https://pypi.org/project/blake3/1.0.9/)
for independent artifact-byte reconciliation. This is a separate verifier process
using the standard hash implementation, not an independent editing implementation.
Native requested-WB evidence is recorded separately: the direct LibRaw CFA inverse-channel-ratio regression and Adobe DNG requested-white camera-neutral regression both passed the parent core gate at08e56fd/45df945. Neither is real-camera appearance parity. The qualification probe compiled at a5ac9d6; scientific fixture/contracts and complete campaign qualification are separate gates.

### Evidence admission repair (source only, before campaign approval)

The case verifier now requires the exact Cartesian set of recipe/iteration
identities, paired attempt/observation ordering, every requested encoding exactly
once, and bounded reads before allocating each JSONL line. Duplicate JSON fields,
NaN, infinity and overflowing decimal numbers are rejected. Requested output
specifications remain independently checked by the readback oracle.

For the fixed Mac overlap campaign, the probe queries `PROC_PIDTBSDINFO` before
and after each durable save. Both snapshots must describe the same non-zombie
direct child and kernel creation timestamp. Actor-held PIDs alone are not proof:
they can remain populated during post-reap hashing or publication. The verifier
also reconciles each kernel identity with the coordinator's independent psutil
observations. This establishes process-lifetime overlap; it does not claim every
CPU instruction occurred inside a native decoder. Other platforms refuse this
Mac-specific measurement phase explicitly; ordinary product APIs remain portable.

The final Python launch is a stdlib-only, externally hashed `edit_binding.py`
invoked with `-I`. Before importing campaign helpers it checks the copied private
`scripts` package and `benchmarks/observe_host.py`, rejects extra import-shadowing
files, validates exact helper paths and bytes, and checks Python executable/build
plus installed scientific distribution file manifests, including native extension
bytes and metadata. Versions alone do not bind dependency code. No worktree helper
imports are admitted. Collection/copy/hash work belongs to untimed preparation;
this source checkpoint has not constructed or admitted a real binding.

`edit_disk_budget.py` computes prospective retained raw/encoded artifacts,
per-service namespaces, bounded logs, source copies, active exports, allocation
overhead and 16 GiB free reserve from the complete matrix. The old preliminary
96 GiB minimum / 128 GiB output limit is not an admission rule. Final binding must
fund the calculated complete peak, retain all failed work, and declare cleanup of
verified successful repeated exports explicitly. Filesystem/SQLite overhead is an
explicit allowance guarded by actual free-space observations, not a forecast of
compression or immunity to external disk consumption. The final action registry,
source-copy binding, derivative/service/100-MP verification and aggregate remain
open; no source-only verification result awards whole-story acceptance.
