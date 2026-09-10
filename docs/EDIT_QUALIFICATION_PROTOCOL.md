# S8 editing and derivative qualification protocol

This document describes the implemented, frozen qualification method. It is an
editorial consolidation of the reviewed protocol, not a new experiment or a
change to its targets, fixtures, samples or tolerances. Measurements and acceptance
decisions are recorded in [the results report](EDIT_QUALIFICATION_RESULTS.md). A completed probe or successful individual
verifier does not by itself award performance, memory or whole-story acceptance.

The bound helper source is `2ed0e5cbceb857e542562ad4d01b8e75eeff8eff`; the native
release was built from `a6498079a01e98c703d56a6559e8fc80b1dcc5da`. Their complete Git
delta contains only the supervisor Python, its tests and the execution ledger.
The private build/source-association receipts retain that distinction, together
with both source archives and exact app, probe, SDK and dependency identities.
Historical failed tests and corrective smoke runs remain in private receipts;
this consolidation neither deletes them nor replaces their verdicts.

## Fixed acceptance targets and reporting

| Operation | p95 ceiling | Warmups + measured samples per configuration |
| --- | ---: | ---: |
| Warm 1600 linear-input recipe kernel | 100 ms | 2 + 100 |
| Current edited 1600 preview delivered through PreviewService | 250 ms | 2 + 100 |
| Combined recipe on an already developed original, source ≤32 MP | 5 s | 2 + 20 |
| First RAW decode and edited 1600 delivery | 20 s | 2 + 20 |
| Durable JPEG export | 25 s | 2 + 20 |
| Durable PNG export | 40 s | 2 + 20 |
| Durable TIFF export | 30 s | 2 + 20 |
| Durable edit acknowledgment during actual import overlap | 100 ms | 0 + 100 |
| Durable edit acknowledgment during actual export overlap | 100 ms | 0 + 100 |

Export overlap is an additional required diagnostic; it does not replace import
overlap. Report each source, format, original dimensions, recipe and output
configuration separately. Retain every raw sample and warmup. Use nearest-rank
p50/p95/p99, minimum and maximum; exclude warmups from measured distributions.
Do not pool cameras or configurations to conceal a miss. Correctness/refusal and
64/100 MP support cases receive no ≤32 MP timing award.

These are headless component and durable-operation measurements. Desktop frame
delivery remains S12. Sources are ordinary copies on the private local SSD/APFS
volume; this is not RAID throughput evidence. Fresh children and disabling the
prepared cache do not flush filesystem caches or establish cold-storage latency.

## Sources, operations and complete registry

The exact 30 source IDs and supplied dimensions are fixed by the S6 manifest and
`edit_qualification.py::IDS`: 24 camera/public sources and six preparation
fixtures. Each original is opened read-only as an admitted ordinary file and
copied to an exclusive one-file directory. Copies are independent files, never
hard links to originals. Preparation binds original/copy SHA256, copy BLAKE3,
size and held-file/path identity. A separate copy of the X-T3 RAW occupies the
one-file background-import directory. Original photos and Lightroom files are
never written. Final aggregate checks original and copied-source hashes again.

Seven additional sources are deterministic float32 TIFF constructions:
signed/alpha, impulse, noise, flat, metadata, 8000×8000 and 10000×10000. Generation
streams scanlines and records exact byte hashes, dimensions, ICC and construction
identity. Thus the campaign has **37 source entries**, plus the separately bound
background copy. The 100 MP RGBA float payload is 1,600,000,000 bytes before TIFF
metadata; generation does not allocate that full payload as a Python array.

The timing cohorts are fixed:

- Eight inputs: Canon 5D IV RAW, X-T3 RAW, X-T3 DNG, L1D-20c DNG, public Panasonic
  GX7MK2 RAW, Canon 6D PSD, Canon 6D TIFF and G15 JPEG.
- First-RAW timing: the first five inputs above.
- Export timing: Canon 5D IV RAW, X-T3 DNG and G15 JPEG.

`edit_qualification.py::recipes()` fixes all numeric values and pair ordering for
14 operations: neutral, crop, straighten, exposure, WB, contrast, highlights,
shadows, saturation, vibrance, sharpening, luminance noise reduction, chroma noise
reduction and combined. Kernel measurements use recipe A. Service measurements
alternate A/B with a new durable revision for every trial: 100 service samples
per operation comprise 50 A and 50 B samples. First-RAW uses 20 alternating
samples (10 per base). Neutral has kernel
coverage only. Full and export timings use the fixed combined recipe. Canonical
Rust recipe digests and the independent normalized request bind the same settings.

`edit_disk_budget.py::complete_cases()` is the authoritative ordered registry:

| Phase | Probe cases |
| --- | ---: |
| Linear proxy references, before all consumers | 8 |
| Correctness: 90 original-source, 8 analytic, 160 output-matrix | 258 |
| Warm kernels, 14 operations × 8 sources | 112 |
| Current service delivery, 13 operations × 8 sources | 104 |
| Already developed full combined recipe | 8 |
| First RAW delivery | 5 |
| Durable export timing, 6 formats × 3 sources | 18 |
| Large-image admission | 2 |
| Typed refusals, including both large sources | 8 |
| Large-image cancellation and recovery | 2 |
| Durable selected-metadata exports | 6 |
| Foreground saves during import/export | 2 |
| **Total** | **533** |

Each probe is followed immediately by its independent verifier: **1,066 serial
campaign actions**. Seven preparation generators and one separately admitted
aggregate make **1,074 bounded child invocations** across the three phases; native
workers are additionally tracked descendants. All 30 original sources have two
independent-process all-operation pixel passes and a combined pass exporting all
six formats. Repeated all-operation surfaces must match exactly across processes.

## Pixel, WB and timer boundaries

The editor uses physically oriented, scene-linear float RGBA and straight alpha.
RAW/DNG requested WB is applied before the relevant native development/profile
operations. Raster WB has explicit D65-reference semantics; AsShot preserves the
baseline. Prepared proxies bind the original fingerprint, source instance, WB,
renderer, original dimensions, edge and bytes. They are never decoded browse JPEGs.

The kernel timer brackets `render_recipe` on a retained linear input at edge 1600;
the full timer brackets that call on the already developed original. Original
decoding, hashing and WB-basis preparation are outside those timers. In particular,
the WB kernel row measures processing of its prepared WB basis, not WB preparation
latency. Spatial operations on a proxy are approximate relative to an exact
original-sized render. Straighten uses a fixed canvas with transparent exterior,
followed by normalized crop; it does not silently choose an inscribed canvas.
Resampling interpolates alpha-weighted colors while exposing straight alpha.

Current-preview timing starts before `request_interactive` and ends after materialized
current RGB retrieval and validation of the new key and producer evidence. The
preceding recipe save is outside this timer and is covered separately by overlap
acknowledgment measurements. Each trial must observe a new worker completion;
repeated delivery of an old cache record cannot satisfy the test. Every measured
warm trial requires typed `PreparedProxy` evidence matching the actual source,
WB, renderer and edge. The two warmups prepare the fixed A/B states. Unseen WB
values remain cold work outside this warm claim.

First-RAW delivery uses zero prepared-cache entries/bytes and requires typed
`OriginalDecoded` provenance for every trial. It includes original decoding in
the service path but makes no cold OS-cache claim. Service and export loops also
have a 180-second per-operation failure stop, distinct from acceptance targets
and the enclosing per-case watchdog.

Durable export timing starts before job creation and includes append/seal,
admission, original decode, exact recipe, metadata/profile conversion, encoding,
source/authority verification, publication and durable completion. Independent
output readback follows the timer. Worker metrics record actual source checks,
setup, decode, recipe, metadata, encode, sync and total durations. Owner metrics
record seal, acceptance, publication and four fresh-export authority intervals.
Inclusive or overlapping phases need not sum to end-to-end time; missing phases
are never estimated by subtraction.

## Independent correctness and output policy

`edit_reference.py` evaluates fixed equations in float64 with direct neighborhood
sums, independently of production editing and LCMS. All components of small
analytic fixtures are checked, including signed/HDR RGB, alpha, boundary pixels,
geometry and each operation/combination. The fixed component bound is
`2e-5 + 2e-5*abs(expected)`, or `4e-5 + 2e-5*abs(expected)` for changed geometry.
These bounds were declared from precision/math before measurement. Dimensions,
channel declarations and alpha structure also have exact checks. WB uses published
Robertson brackets for the two fixed temperatures and a float64 Bradford solve;
it is not a general arbitrary-temperature or real-camera appearance oracle.

Separate native synthetic tests cover the LibRaw requested-white camera-response
ratios and Adobe DNG known-camera-neutral patches. They provide bounded CFA/WB
proof, not Adobe/Lightroom rendering parity. Their source is
`src/media/raw_highlight_tests.rs`; recipe/cancellation/geometry core regressions
remain separate from the campaign's numerical and process evidence.

The fixed export formats are JPEG8 quality 90, PNG8/16 and TIFF8/16/float32. Timing
uses sRGB for integer formats, linear sRGB for TIFF float32, preserved alpha for
PNG/TIFF and an explicit linear-white JPEG background. Outputs use post-recipe
dimensions. The 160-case output matrix additionally covers built-in sRGB/linear
sRGB, custom linear/nonlinear matrix ICC profiles where supported, original size,
12×12 fit and 96×96 fit with/without enlargement, and alpha preservation versus
explicit compositing. Float TIFF is limited to the supported linear profiles.

`edit_readback.py` uses independent imagecodecs/tifffile decoding and bounded
container parsers. It admits encoded size, dimensions, decoded allocation and
cumulative metadata before decoding, retains the held descriptor, and verifies
actual format, depth, shape, alpha association, ICC and safe EXIF. Custom ICC bytes
are exact for the fixed linear/nonlinear matrix fixtures; this is not an
exhaustive arbitrary-ICC-profile claim. The reader admits the actual classic-TIFF
outputs and rejects BigTIFF or extra image/thumbnail IFDs. Built-in matrix/TRC
structure uses the fixed `8/65536` quantization
bound. Pixel reductions use eight-row blocks. Integer lossless error ceilings are
2 LSB for 8-bit and 4 LSB for 16-bit; float output uses the geometry component bound.

**JPEG has a deliberately limited quality gate.** The constant analytic fixture
at quality 90 has a 3-LSB DC/YCbCr bound. General-photo JPEG errors are reported
without a universal whole-image error threshold. Structure/profile/metadata and
artifact identity remain mandatory. Service previews must match the exact encoded
reference produced from the corresponding linear recipe basis, and retained
actual delivered RGB must match its framed digest. Independent JPEG decoder
rounding differences are reported, not required to be bit-identical. These checks
must not be described as universal perceptual JPEG-quality acceptance.

Controlled metadata includes a named RDF subject, unknown qualified/structured
properties, Bag/Seq/Alt arrays, multilingual values and an 80KB extension payload.
Direct encoding retains the supplied semantics. Six durable service exports select
the sole valid full base and compare an independently constructed derivative
expectation: preserve unrelated metadata, replace technical orientation/dimensions/
profile fields, remove active Adobe develop settings and unsafe source pointers,
and retain original packets unchanged. Omitting source metadata still permits the
specified minimal technical derivative XMP. JPEG standard/extended GUID, complete
chunk coverage and same-subject association are checked; PNG iTXt and TIFF tag700
carry XMP. ICC and safe EXIF must agree with physical output; source MakerNotes and
unadjusted IFD offsets are not copied. Unicode remains in XMP where safe EXIF is
ASCII-only. No silent conflict selection, metadata omission or format fallback is
accepted.

The 64/100 MP controls admit each original under larger limits, check a deliberately
too-small decode allocation, and cancel the fixed combined recipe after actual
work. In the bound source, cancellation trips at the fourth callback: render
preflight, denoise row 0, row 1, then row 2. Two rows have completed after admission
and scratch allocation. The complete input digest must remain unchanged, and an
uncanceled render of that same immutable input must recover the independently
checked result. This is library cancellation/reuse evidence, not a new large-worker
crash experiment. Neutral large-image values are checked completely in eight-row
blocks; combined mathematical checking uses a fixed 17×17 grid with boundaries and
independently derived neighborhoods. It is not a full-image combined mathematical
oracle. Encoded outputs are nevertheless read back over their full dimensions
against the retained developed artifact, subject to the JPEG limitation above.

## Actual overlap and memory evidence

The overlap probe uses a distinct catalog connection and virtual copy so foreground
saves do not invalidate the background master export. Background import advancement
or export polling continues during all 100 saves. On the reference Mac,
`PROC_PIDTBSDINFO` snapshots before/after each save must show the same non-zombie
direct child and creation timestamp, reconciled with external psutil observations.
Actor-held PIDs alone are insufficient. No holds or synthetic delays extend worker
life. This proves process-lifetime overlap, not that every instruction coincides
with native decode. Other platforms explicitly refuse this Mac-specific phase.

Normal admission is one worker, 4 GiB working allowance, 2 GiB individual allocation,
32 MP rendered surface, 40 MP native intermediate ceiling and 512 MiB encoded
input/output extent. Native intermediate admission is separate from final output
dimensions. The prepared cache is 256 MiB/16 entries except first-RAW's zero cache.
Large controls use 100 MP render/110 MP intermediate ceilings, 4 GiB allocation,
12 GiB working allowance and 2 GiB encoded extents. These finite controls preserve
configurable supported-source admission; they do not claim every arbitrary 100 MP
encoded file fits the chosen limits.

Per-child sampled stops are 4 GiB/process and 4.5 GiB/group normally, or 12 GiB/process
and 12.5 GiB/group for large controls. Sampling occurs every 0.1 seconds. Positive Mac
probe and actual producer-bound worker `getrusage` high-water receipts are required
in addition to sampled group RSS. A missing peak cannot be interpreted as zero;
process peaks are not simultaneous group peaks. Probe HWM includes untimed work
and verification performed within that probe. Allocation reservations are not hard
RSS enforcement. A final worker reservation requires measured full-operation peaks
and the separately reviewed margin decision; neither these prospective allowances
nor the earlier S6 calibration establishes a universal future-camera guarantee.
The independent 4 GiB browsing requirement does not certify editing-process memory.

## Phase admission, supervision and retention

Preparation, campaign and aggregate have separate sealed argv, exclusive owner
and data namespaces, source/runtime associations and explicit parent grants. There
is no automatic next phase or retry. Preparation does not depend on already having
completed preparation or future campaign gates. Campaign admission requires its
successful 37-source preparation and complete resolved 533-case binding. Aggregate
is separately admitted after the same-binding campaign; it does not inherit a 24-hour
default. Failed or partial namespaces remain, never automatically resumed or
overwritten.

Preparation has a 3600-second internal deadline; each generator has 600 seconds and
1 GiB sampled process/group stops. Its outer owner has 3900 seconds, 2 GiB/process,
3 GiB/group, 4 active processes and 8192 lifetime identities. The campaign outer owner
has 86400 seconds, 12 GiB/process, 13 GiB/group, 8 active processes and 131072 lifetime
identities. These are failure stops, not operation-time acceptance targets.
Per-case deadlines remain 120 seconds for small output/analytic/refusal cases,
180 for durable metadata,300 for overlap,900 for original correctness/kernel/full/
warm delivery,1200 for proxy references and first RAW,1800 for timing exports and
3600 for admitted large/cancellation controls. The aggregate owner is separately
bounded to 3600 seconds, 4 GiB/process and 4.5 GiB/group.

Each action supervisor allows 4 active processes/256 identities, 32 MiB process
telemetry,256 KiB identity ledger and 4 MiB per stdout/stderr stream. Sample frames
are 8 KiB and identity events 512 bytes. Preparation outer telemetry/identity caps are
128 MiB/8 MiB; campaign outer caps are 8 GiB/128 MiB. Preparation host logging is 512 MiB
total, campaign host logging 8 GiB, both with 1 MiB records. Logs fail admission rather
than silently discarding evidence needed for acceptance. Successful zero exit does
not bypass deadlines; recurring zombie observations cannot consume new lifetime
slots. Cleanup uses retained process ownership and creation identities, terminates
and reaps known children, and stops further admission on uncertainty. Abrupt host
failure still requires explicit ownership reconciliation.

Disk funding comes from `edit_disk_budget.py::budget()` over the complete matrix,
not an expected compression ratio: retained content/namespaces/evidence plus the
largest active export, source copies, explicit allocation overhead and 16 GiB free
reserve. The bound minimum is 403,726,925,824 bytes; the separately funded aggregate
artifact adds 67,174,400 bytes, giving 403,794,100,224 bytes initial outer admission.
Live free-space checks remain mandatory. The accounting `output_stop_bytes` field
is not a separate filesystem quota; encoded extent/admission and live reserve are
the active safeguards. External disk consumption can still cause an honest failure.

Funding covers 23 simultaneous encoded extents during export batches of 22. Only after all 22
individual outputs, durable receipts and process retirement pass may cleanup
remove 21 unselected destination names, all 22 authority-verified recovery
directories and the disposable per-case catalog. Preserve the first measured
destination (iteration 2), source copies, preview namespaces, raw samples and
verification evidence. Retain digest-recorded inventories/deletion lists and the
cleanup receipt. Traversal is bounded and symlink-safe; no unknown prefix sweep.
Failed verification or cleanup stops progression and preserves remaining state.

## Frozen implementation and acceptance chain

Private Python is launched with `-I -B`. The binding covers executable/framework
library/app launcher, actual import-root files including existing `.pyc` and
unlisted files, distribution metadata/native extensions and the complete private
helper package. No worktree imports are admitted. Fixed distributions are NumPy
2.5.3, tifffile 2026.8.23, imagecodecs 2026.8.16, blake3-py 1.0.9 and psutil 7.2.2.
OMP, OpenBLAS, MKL and VECLIB thread environment values are all 1. Version strings
alone are insufficient; runtime bytes and actual module paths are checked.

The builder records all normalized Requests, full case specifications, limits,
source/metadata associations, commands and exact result/cleanup locations. Startup
validates the registry, helper/runtime closure, original-copy identities and
funding before any action. Case verifiers reject missing/duplicate/out-of-range
attempts or observations, missing outputs, nonfinite numbers, duplicate JSON keys,
oversized/growing sample streams and unsupported coverage. The final aggregate
reconciles 533 case proofs, supervisor identities/windows/captures, per-case memory
and statistics, exact repeat determinism, cleanup and final source invariance.
Every required numerical target must pass independently before a headless campaign
award; `whole_story_qualified` remains false in this harness.

Host evidence records UTC/monotonic anchors, CPU, RSS, swap, disk and available GPU
observations plus local storage attribution. Missing observations mean unavailable,
not idle. Exclusive team ownership does not prove a uniformly quiet host. Report
actual window limitations and all failures with the measured result; do not select
an apparently unaffected subset or silently rerun a favorable configuration.

Source review anchors: `src/bin/edit_probe.rs`; `src/edit/{recipe,render,geometry}.rs`;
`src/photo_render.rs`; `src/image_export/{encode,metadata,specification}.rs`;
`scripts/edit_{qualification,correctness_matrix,disk_budget,prepare,build_plan,request,admission,campaign}.py`;
`scripts/edit_{reference,large_reference,readback,derivative,artifacts,verify,memory,statistics,aggregate,cleanup,binding}.py`;
and `scripts/preview_host.py`. The separately sealed phase launcher and every
actual build, preparation, binding, supervision and aggregate receipt remain in
the private evidence package. This document specifies their required meaning;
the results record supplies their identities, outcomes and qualification decision.
