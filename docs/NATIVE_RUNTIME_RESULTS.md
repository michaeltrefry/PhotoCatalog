# Native Rust runtime and query-work results

The native runtime campaign completed on 2026-09-09 with all recorded checks passing at 1M, 5M and 10M synthetic catalog records. Independent review reconciled all 15 distributions, 2,400 samples and six child processes. The separate bundled-SQLite diagnostic also verifies the corrected query results and constant measured VM work at all tested scales and cursors. Confidence is high in this recorded evidence for the measured binaries and workload; the limits below remain part of its interpretation.

This report adds native evidence to [PRODUCTION_PROFILE_RESULTS.md](PRODUCTION_PROFILE_RESULTS.md). [BACKEND_DECISION.md](BACKEND_DECISION.md) owns the backend selection. No database queries, tests, builds, repeat measurements or source-file hashes were executed while preparing this document; the values and digests below were read from existing receipts.

## Execution and settings

The runtime campaign ran from **2026-09-09 13:57:48.127950 UTC to 13:59:48.168438 UTC** at source revision `1ac4737fae578029521e979fc7b9fe9bed579502`. The build reference was recorded afterward at 14:01:06.702876 UTC and records the same source head and binary/source digests. It identifies Rust 1.98.0 (`88d9e12ae`, 2026-08-18), Cargo 1.98.0 (`797e8a9bc`, 2026-08-05), target `aarch64-apple-darwin`, and LLVM 22.1.8.

The actual runtime reports **bundled SQLite 3.51.1**. The Python supervisor reports SQLite 3.53.3 for its own binding; that is not the engine measured by these Rust children. The host is macOS 26.6.2/arm64, with 18 logical CPUs and 128 GiB RAM. Host observations are retained, but snapshots alone do not establish uninterrupted absence of competing CPU, disk or GPU work; the coordinator's host-load record is separate.

All runtime connections use the same declared 256 MiB cache configuration, with settings read back from the production connection helper and both native writer connections:

| Setting | Actual value |
|---|---|
| `journal_mode` | `wal` |
| `synchronous` | `2` (FULL) |
| `fullfsync` | `1` |
| `foreign_keys` | `1` |
| `cache_size` | `-262144` (256 MiB per connection) |
| `mmap_size` | `0` |
| `temp_store` | `1` (file) |
| `busy_timeout` | `5000` ms |
| `wal_autocheckpoint` | `1000` pages |

Each scale uses two separate, verified pristine copies and two fresh child processes: one browsing-only child, and one child that browses before running competing foreground/background writes. Copy preparation and integrity verification are outside measured intervals. The copy validator's 4096 MiB preparation allowance is not the runtime configuration.

## All native timing distributions

All values are milliseconds, rounded to three decimals; raw precision and every sample remain in the private receipt. Percentiles use the recorded linear interpolation and were independently recomputed. Browse operations return 200 assets from the actual `Catalog::browse` path, including JSON metadata deserialization, at ten repeating cursor positions from 50% through 90.5%. UUID/order and metadata checks occur after each timed page. Get/reopen identity checks also occur outside those page intervals.

**“Browse before mixed writes” is the receipt's `mixed_browse` distribution. It was collected before the importer started, not during import.** The native concurrency measurements are the rating/edit commits and background import transactions. Python's separate page-correction experiment contains the pages-during-import measurements.

| Records | Workload | n | p50 | p95 | p99 | Maximum |
|---|---|---:|---:|---:|---:|---:|
| 1M | Browse-only child | 200 | 0.114 | 0.136 | 0.160 | 0.217 |
| 1M | Browse before mixed writes | 200 | 0.108 | 0.124 | 0.153 | 0.211 |
| 1M | Durable rating during import | 100 | 75.422 | 83.704 | 87.357 | 87.546 |
| 1M | Durable edit during import | 100 | 75.804 | 83.712 | 86.699 | 87.941 |
| 1M | Background import batch | 200 | 50.174 | 60.809 | 62.041 | 69.808 |
| 5M | Browse-only child | 200 | 0.102 | 0.135 | 0.150 | 0.202 |
| 5M | Browse before mixed writes | 200 | 0.097 | 0.126 | 0.138 | 0.187 |
| 5M | Durable rating during import | 100 | 77.375 | 84.466 | 86.163 | 87.360 |
| 5M | Durable edit during import | 100 | 76.637 | 83.580 | 85.589 | 87.659 |
| 5M | Background import batch | 200 | 56.047 | 65.871 | 68.108 | 73.355 |
| 10M | Browse-only child | 200 | 0.102 | 0.129 | 0.142 | 0.300 |
| 10M | Browse before mixed writes | 200 | 0.102 | 0.127 | 0.139 | 0.192 |
| 10M | Durable rating during import | 100 | 77.554 | 86.948 | 92.347 | 99.120 |
| 10M | Durable edit during import | 100 | 78.439 | 87.238 | 105.234 | 116.937 |
| 10M | Background import batch | 200 | 62.613 | 74.372 | 77.707 | 82.389 |

Every browse p95 is below the 100 ms warm-page budget. Every rating and edit p95 is below the 100 ms durable-acknowledgment budget. At 10M, **two of the 100 edit samples exceed 100 ms: 116.937458 and 105.116167 ms** (zero-based edit-sample indices 2 and 84). The edit p99 is 105.234380 ms while p95 is 87.237969 ms. This passes the specified p95 gate, not a per-operation or p99 guarantee. The worst rating p95 is 86.947725 ms at 10M.

The importer performs 200 transactions of 32 cloned synthetic rows plus their relationships: 6,400 new assets per mixed copy. A rendezvous releases the foreground only after the background holds its immediate write transaction. Foreground timing includes acquiring its write transaction and committing durably; rating/revision readbacks follow the timed commit. The worker waits for foreground acknowledgment before its next batch. This establishes intentional writer contention, not unconstrained background throughput. Each run completes all 100 ratings, 100 edits and 200 import batches, with correct final imported counts and no child failure. Background batch latency is diagnostic and has no separate 100 ms admission threshold.

## Memory, open time and preservation

RSS values below are process peaks sampled by the supervisor, not exact allocator totals. The ≤4 GiB browse-memory gate applies to the isolated browse-only child. The mixed child's peak includes its preliminary browsing and its writer connections; it is separately recorded without inventing a mixed-memory admission threshold. No aggregate runs in either native child.

| Records | Browse-only peak bytes (MiB) | Mixed-child peak bytes (MiB) | Browse child open, ms | Mixed child open, ms |
|---|---:|---:|---:|---:|
| 1M | 11,599,872 (11.062) | 87,195,648 (83.156) | 10.548 | 7.969 |
| 5M | 11,649,024 (11.109) | 345,423,872 (329.422) | 10.970 | 11.167 |
| 10M | 11,616,256 (11.078) | 345,456,640 (329.453) | 10.738 | 9.505 |

These are six individual open observations, not fresh-process open-plus-query distributions or cold-storage evidence. The 20-sample fresh-process distributions remain in the separate page-correction campaign. Source/copy checks can warm filesystem caches.

All three pristine source hashes match before/after runtime execution. Browse and mixed copies at each scale independently record the expected physical size, logical counts/sums, relationships and source identity before measurements. Mutations occur only on disposable copies; source snapshots remain preserved. Runtime output records UUID/order and basic metadata checks, and successful get/reopen identity verification. This synthetic probe does not by itself prove arbitrary Lightroom/XMP preservation.

| Records | Source bytes | Preserved source SHA-256 |
|---|---:|---|
| 1M | 930,746,368 | `6521e35ade9cd51495dd9d6f18c83e4315c4d080c5624c39036223b5cc0b27e3` |
| 5M | 4,670,160,896 | `c0fca96144031528e9b3ff38b5b0c0fd9b2a7f5efe53cfdfca8af16140ff9bc2` |
| 10M | 9,380,655,104 | `902a968566183f551cf1bdcecd96106c6c18490fc88864d36150580fcb26cdbd` |

## Bundled-engine query-work proof

A separate read-only Rust diagnostic ran from **2026-09-09 13:47:54.697219 UTC to 13:48:06.466649 UTC**, at revision `7595b24a2c54eada5fa0a4093004482eca5b97b6`. It uses the same bundled SQLite **3.51.1**, with SQLite source ID `2025-11-28 17:28:25 281fc0e9afc38674b9b0991943b9e9d1e64c6cbdb133d35f6f5c87ff6af38a88`.

Each scale records both baseline and corrected SQL for six cases, yielding 12 fully consumed statements per scale and 36 total. Cases cover deep/rating queries at 50%, 90%, and exact frozen iteration 9 (90.5%); rating values are 4, 4 and 5 respectively. Independent Python verification confirms SQL registry agreement, case parameters, all six projected fields and the first qualifying 200-row page against the frozen generator. It also verifies source hashes and metadata before/after. All three verification records pass.

| Corrected query | VM steps at every tested scale/cursor | Sorts | Full-scan steps |
|---|---:|---:|---:|
| `page_deep` | 2,410 | 0 | 0 |
| `rating` | 2,413 | 0 | 0 |

The bundled engine therefore reproduces the constant measured work of the Python SQLite correction. Baseline deep navigation at 90% still reaches 11,002,615 VM steps and one sort at 10M; corrected execution remains 2,410 steps without sorting. These counters cover one prepared statement, fully consumed exactly once. VM steps are not physical I/O or distinct rows; zero full-scan steps alone cannot rule out extensive range scans. These cases support bounded work on the tested generator, not a universal bound for arbitrary sparse joins.

This diagnostic opens standalone, checkpointed snapshots read-only with `immutable=1` and `query_only=1`; its retained `journal_mode=delete` describes those snapshots, not the writable production configuration. Profiling is explicitly diagnostic-only and does not contribute latency samples. The corrected joined SQL is exercised in this diagnostic; the native timed `Catalog::browse` operation above remains its actual single-table asset/metadata path. Neither result should be relabeled as timing a seven-filter public Rust API.

## Exact artifact identities and scope

Receipts stay private, outside Git. Logical archive identifiers are `sc-22837-native-runtime-v1/integration.json`, its six child records and `build-reference.json`; and `sc-22837-native-query-work-v1/{count}-receipt.json`, `{count}-cases.json`, and `verification.json`. Recorded SHA-256 identities are:

| Artifact | SHA-256 |
|---|---|
| Runtime release `catalog_probe` binary | `ab76f6d00358f4ad45e4f833bfd2e124b73cd74b9733d6fece6d02d101f3c8f8` |
| Runtime `src/lib.rs` | `e5e945cd03a2c639b1184cf6e27ae12b6745911a1b173e1c58bbaa703ca305dd` |
| Runtime `src/bin/catalog_probe.rs` | `fe3d0e4dacf550d7248e34e55ad79ad3a2ddd58ac9c51889d287adfae4250904` |
| Runtime supervisor | `cd0abe35710a82fc52197d9c6d07bf251cc63b08b0525608a30c2e75a729cca7` |
| Runtime `Cargo.lock` | `9cc49e426e029338a1ba2618de294e962bff575192ae3816898f5a450d8e3835` |
| Runtime `rust-toolchain.toml` | `495c69d1db93f25f53e3d0d2d40d812313366cc368de317871c2ace6c6ffafa4` |
| Pristine snapshot manifest | `6b6022acf3da78434708414398c06eb8054fe9dc52a854bf5c89189f577778a4` |
| Work diagnostic release `query_work_probe` binary | `6459dbd2ad4790b05acee5a1ac3bc48610cdb20ea9e630dafd134e804cb2ea09` |
| Work diagnostic source | `c8f93c44de3d9848eeb03e949a55c37110122205e0f1efe7d8d89a11adbe307a` |

The diagnostic also binds its exact case manifest using BLAKE3; these hashes use a different algorithm from the SHA-256 table above:

| Records | Case manifest BLAKE3 |
|---|---|
| 1M | `1fd152950570c55ce405e1c67a48010e4d3c1fac668983df539843ba24aabef3` |
| 5M | `135b62e154c8329a948f81e57020a87c363945764cd8ab5a1cf361e085329c2f` |
| 10M | `a4bee4e3debe545502ac78c42920cd5490f80eae13eb3482ee47336d951c6c35` |

This campaign validates the current Rust connection configuration, actual asset browsing/deserialization/reopen, and native synthetic durable transactions under prescribed contention. Its writes use the probe's rusqlite transactions and the production connection helper; they are not a claim that future editor or import application APIs already exist. It does not rerun seven-filter Python latency distributions, aggregate/load/size measurements, forced-crash recovery, or the constrained-memory matrix; those retain their separately identified evidence. It does not measure RAW decoding, previews, editing, external-original storage, user interface responsiveness, or other operating systems.

Independent native review passed for the exact recorded evidence. Final interpretation combines this report with the original and corrective campaigns, resource observations and the decision record; no earlier failed attempt or slow tail sample is discarded.
