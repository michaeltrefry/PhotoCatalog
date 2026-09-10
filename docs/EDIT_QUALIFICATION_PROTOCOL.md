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
