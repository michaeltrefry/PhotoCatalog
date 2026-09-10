# Editing and export qualification — reference Mac

The frozen S8 headless campaign passed all **533 cases** and all **249 numerical
performance configurations** on 2026-09-10. The independent aggregate reported no
missed p95 targets. This qualifies the measured Rust core/service configuration;
cross-platform CI, merged delivery and the dependent Tauri interface have separate
acceptance gates. It does not establish Lightroom rendering parity.

## Environment and scope

Reference host: Apple M5 Max, 18 CPU cores, 128 GiB RAM, macOS 26.6.2
(Darwin 25.6.0, arm64). Originals were read-only; measurements used independent
copies on the local SSD/APFS volume. These results do not measure external RAID
throughput or cold OS-cache latency. Native builds and other team measurement
work were serialized. Retained host telemetry records ordinary desktop activity;
no uniformly idle-host claim is made.

The fixed cohort contains 30 prior RAW/raster sources and seven generated sources,
including 64 MP and 100 MP float TIFFs. Eight sources form the timing cohort, five
the first-RAW cohort and three the export cohort. The
[protocol](EDIT_QUALIFICATION_PROTOCOL.md) specifies exact recipes, limits, clocks,
sample counts, independent oracles and important JPEG/WB/large-image limitations.

## Performance

The table shows the largest independently calculated p95 in each category, rather
than a pooled percentile. The [complete metrics CSV](evidence/EDIT_QUALIFICATION_METRICS.csv)
retains all 249 source/operation configurations, original dimensions, measured
counts, minimum, p50, p95, p99, maximum and target. Raw observations, including
warmups and phase timings, remain in the private evidence package.

| Operation | Configurations | Worst p95 | Fixed p95 ceiling |
| --- | ---: | ---: | ---: |
| Prepared 1600 recipe kernel | 112 | 62.458 ms | 100 ms |
| Current edited-preview service delivery | 104 | 204.619 ms | 250 ms |
| Combined full-resolution recipe, already developed input | 8 | 869.373 ms | 5,000 ms |
| First RAW decode and edited-preview delivery | 5 | 9,709.832 ms | 20,000 ms |
| Durable JPEG8 export | 3 | 9,345.433 ms | 25,000 ms |
| Durable PNG8 export | 3 | 3,647.804 ms | 40,000 ms |
| Durable PNG16 export | 3 | 4,612.828 ms | 40,000 ms |
| Durable TIFF8 export | 3 | 8,006.747 ms | 30,000 ms |
| Durable TIFF16 export | 3 | 19,705.525 ms | 30,000 ms |
| Durable float32 TIFF export | 3 | 20,974.400 ms | 30,000 ms |
| Durable edit save during actual import | 1 | 3.809 ms | 100 ms |
| Durable edit save during actual export | 1 | 3.828 ms | 100 ms |

The tails remain relevant even though the p95 gates pass. Across warm-preview
configurations, the largest p99 was 253.165 ms and the largest individual value was
472.001 ms. X-T3 DNG TIFF16 reached 24,761.905 ms; float32 TIFF reached
29,237.429 ms. No samples were discarded to improve these results. Export rows
include original decoding, encoding and durable publication; already-developed
recipe rows exclude decoding. The 64/100 MP support controls have no ≤32 MP timing
award. Desktop frame delivery has not been measured by this harness.

## Correctness, memory and durability

All 1,066 ordered probe/verifier actions reconciled. Coverage includes two
independent-process all-operation passes and six-format combined exports for each
of the 30 original sources; analytic operation checks; 160 format/profile/size/
alpha combinations; full-resolution 64/100 MP admission and explicit resource
refusals; cancellation followed by rendering from the same unchanged large input;
six durable selected-metadata exports; and 100 saves during each actual import and
export worker lifetime. Native tests separately exercise worker crash/restart,
undo/redo, copy adjustments, missing/changed originals, source/destination races,
stale publication and interrupted restoration.

The aggregate verified 37 unchanged source copies, all 30 original SHA256 values,
30 deterministic repeat pairs and 18 completed export-cleanup records. Each timed
export batch independently checked all 22 outputs before retaining its first
measured output and removing only its authorized disposable outputs and recovery
state. Failed earlier tests and smoke runs remain retained with their original
verdicts; the full campaign itself ran once without retries.

Measured Mac process high-water values:

| Scope | Peak resident bytes | Approximate GiB |
| --- | ---: | ---: |
| Largest normal-case probe, including its untimed work | 2,956,623,872 | 2.754 |
| Largest actual export/preview worker | 2,603,253,760 | 2.424 |
| Largest large-image probe, 100 MP | 6,713,360,384 | 6.252 |

Retain the tested **4 GiB per-worker editing/export reservation**, with one native
worker. A 25% margin above the largest actual worker, rounded up to 16 MiB,
requires 3,104 MiB; applying the same margin to the larger normal probe requires
3,536 MiB. Both fit the unchanged 4,096 MiB reservation. This is a calibration
decision for the measured cohort, not an RSS guarantee for arbitrary future files.
The separately configured 12 GiB large-image allowance remains applicable to the
64/100 MP controls. Sampled RSS limits and allocation accounting are distinct from
OS process high-water evidence.

The measured editing configuration uses a 2 GiB individual allocation ceiling,
32 MP rendered surface, 40 MP native intermediate ceiling, 512 MiB encoded extents,
and a 256 MiB/16-entry prepared cache. First-RAW cases disable that cache. Set both
`ServiceLimits.working_bytes` and `per_worker_bytes` to 4,294,967,296 for this
editing profile; export uses the same `worker_bytes` and `working_bytes` values.
The existing S6 browse defaults are not the measured S8 configuration. Applications
must choose the editing profile explicitly rather than infer it from browse
reservations. Render/decode/extent limits remain configurable for larger inputs.

The campaign outer owner sampled a peak group RSS of 6,824,984,576 bytes, including
coordinator/observer processes. It reported all discovered identities retired,
root reaped with exit zero, no remaining identities and no ownership errors. This
scope does not assert absence of undiscovered processes. The separately supervised
aggregate also exited zero and retired its tracked processes.

## Reproducible evidence identities

Private images, raw XMP and filesystem paths are not published with this report.
The retained private package contains the original artifacts identified here.

| Artifact | SHA256 or source commit |
| --- | --- |
| Frozen helper/source commit | `2ed0e5cbceb857e542562ad4d01b8e75eeff8eff` |
| Native build source commit | `a6498079a01e98c703d56a6559e8fc80b1dcc5da` |
| Release edit probe | `b4fea4c4278bc053dcdbe807ced1d826bfcebd0b0b789559a63a43b738a1ea24` |
| Release application | `804dadd68e88090a65e00c36b7d1eee63b2531e347e76d89c0e3b8792ec41e4a` |
| Full campaign binding | `b666153be867f210b414c24a4ba2482c4fbf75e4499c259406fe5e0669075ce5` |
| Campaign result | `5052b62bb6b0fba699472d724c50e9cb6154f19799912d62bfeefcd9d6d4a071` |
| Campaign outer completion | `34c6954f08c4f812eca0b82e65d43fe2a4952a8da3b9dc60ead0932ff14601c9` |
| Independent campaign closure review | `6741d870477fbcbb3abb86883e63d2a05ad5877b3bb348e9e5dd4002f30c3dba` |
| Aggregate result | `67414d110df37f527710c3897f6f24ffc698e98e734ea4f79341e55ee0b2b949` |
| Aggregate outer completion | `a1c02c24fd762122acfe0bc5326ae529470c5c122aa057817d1272fee7b5fd84` |
| Independent aggregate review | `c8095e01d4e0b070849b5f8191e25e6ce4a1f0ca6be3419419d45167adf40d3c` |

Native source is unchanged between the two source commits; the complete intervening
delta consists of Python supervision/tests and the ledger. Later report-only
changes do not rebuild or relabel the measured binaries. The byte-bound private
Python 3.14.6 runtime includes NumPy 2.5.3, tifffile 2026.8.23, imagecodecs
2026.8.16, blake3 1.0.9 and psutil 7.2.2; startup validates the runtime and helper
files. Scientific/OpenMP thread settings were fixed to one.

Local delivery gates also passed: 377 native tests across 34 suites, three
intentional ignored fixtures; 122 Python contracts; strict all-target Clippy;
package formatting; debug and release builds. Applicable PR and merged-main CI
on Linux, macOS and Windows are still required before sc-22843 closes.

## Subsequent platform corrections

The first PR #9 CI run, `34532778141`, passed macOS and benchmark contracts but
exposed Linux recovery ownership and Windows source-change detection defects.
Linux could reuse a deleted payload's historical inode for a foreign destination;
recovery now requires a verified live payload and matching full destination
identity. Missing or damaged payloads still permit restoration of the captured
original. Windows now holds write exclusion through source verification/use and
checks a bounded source digest in the admitted preview worker before reusing
prepared pixels. Foreground cache lookup selects candidates using metadata only.
Legacy Windows cache records without a digest miss safely. Windows durability
flushes run before reacquiring final proofs, outside catalog writer authority.

These corrections were integrated at `6147307`. The frozen campaign and its
binaries above remain unchanged. Pixel operations, encoders and the Mac
`SourceInstance` serialized fields are unchanged; the new bounded recovery reads
occur on restoration or failure paths. Windows worker-side hashing adds source-read cost, which this
Mac campaign does not measure. Focused corrective checks and hosted platform CI
provide separate evidence; the old measurements are not measurements of a rebuilt
binary.

Renderer keys include source-file identities, so this code update invalidates old
edited-preview cache keys on every platform. The first request can rebuild its
preview; the table above describes the frozen campaign's declared warm and first
decode cases, not cache survival across an application upgrade.
