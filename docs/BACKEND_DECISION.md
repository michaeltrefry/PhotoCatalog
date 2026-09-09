# Catalog backend decision — sc-22837

Selected: **SQLite with a 256 MiB cache target per connection**, WAL, synchronous FULL, fullfsync enabled, foreign keys enabled, memory mapping disabled, FILE temporary storage, a 5-second busy timeout, and 1,000-page WAL autocheckpoints.

SQLite meets the fixed database budgets at 1M, 5M, and 10M synthetic assets after correcting two equivalent joined page queries. Actual bundled Rust validation also passes, preserves existing catalog data, and confirms bounded query work. The original SQLite failures and DuckDB comparison remain part of the evidence. PR merge and final CI/tracker closeout are tracked in the execution ledger.

## Why this backend

The original SQLite queries allowed join ordering and a temporary sort to do work proportional to the remaining catalog. More cache did not solve that reliably: none of the original 256/1024/2048 MiB profiles passed all scales and memory gates. The corrected deep-page query keeps assets as the outer indexed range; the rating query drives the indexed annotation range and retains the same join, fields, filtering, ordering, and limit. Adversarial tests cover gaps, missing annotations, sparse ratings, updates, and short/empty tails. Full record comparisons verify semantic equivalence.

Those corrections use 2,410 VM steps for a page and 2,413 for a rating page, with no sort, at every measured scale and cursor (50%, 90%, and exact frozen iteration 9). Both Python's SQLite 3.53.3 and Rust's bundled SQLite 3.51.1 reproduce those counters and identical full records. The current Catalog browse path already uses a direct rowid seek and decodes its production metadata. The representative filtered SQL contracts are retained for the organization implementation in sc-22842; this story does not claim that future organization UI/API is implemented.

DuckDB 1.5.5 at 1024 MiB passes the numerical page/write/RSS gates. However, its tested 200-row deep page scans 996,092 / 3,076,662 / 5,526,207 rows at the 50% cursor across 1M/5M/10M. It therefore fails the bounded-navigation criterion for this physical design. Its 256 MiB mixed workload fails with out-of-memory errors at 5M/10M. These findings concern the tested schemas, queries, and engine versions; they do not claim DuckDB can never serve another catalog architecture. No second production database or permanent multi-backend abstraction is introduced.

## Fixed acceptance and measured outcome

The accepted thresholds remain warm 200-row page p95 ≤100 ms, fresh-process open plus page p95 ≤500 ms, durable rating/edit p95 ≤100 ms during background import, and browse RSS ≤4 GiB at 10M. All three scales are exercised. Ordinary deep navigation must avoid unbounded work; correctness, durability, restart, interrupted transactions, and identity preservation are separate gates. Aggregates, loading cost, storage, and mixed RSS are reported without inventing page or memory targets for them.

| Assets | Corrected SQLite worst warm-page p95 | Worst fresh-process page p95 | Worst durable-write p95 | Warm browsing RSS |
| --- | ---: | ---: | ---: | ---: |
| 1M | 0.496 ms | 1.522 ms | 19.738 ms | 256 MiB |
| 5M | 0.680 ms | 1.517 ms | 20.499 ms | 318 MiB |
| 10M | 0.695 ms | 1.587 ms | 20.813 ms | 325 MiB |

Each page has 100 warm samples and 20 fresh-process samples; each scale has 100 durable ratings, 100 durable edits, and 200 pages during import. Independent review reconciles 432 child receipts and 495 distributions, exact SQL/settings identities, complete baseline record hashes, source/copy proofs, and recovery to values 1/2 when interrupted before/after commit. Original and supplemental campaigns retain every failed profile; no favorable retry or per-query cache selection is used.

The original unchanged aggregate remains slower in SQLite: p95 346 / 2,237 / 5,231 ms. It was referenced, not rerun or relabeled as a passing interactive page in the corrective experiment. Broad aggregates must remain off the foreground page path. These results do not establish preview, RAW development, UI scrolling, export, or RAID speed; those retain their own epic acceptance work.

## Actual Rust integration

The shared `configure_catalog_connection` helper applies the exact measured settings after Catalog ownership/schema checks. No schema conversion or identity migration is required. The preservation test independently builds a populated previous v1 schema, then checks two actual Catalog reopens: stable IDs/sequences, raw location bytes, exact metadata JSON including unknown fields, preview references/bytes, pending state/error, schema/application versions, and AUTOINCREMENT state all remain unchanged. Actual Catalog settings are read back in the unit test. Native measurement receipts separately label their diagnostic, foreground, and background connection readbacks.

The native runtime uses Rust 1.98.0, rusqlite 0.38.0, and bundled SQLite 3.51.1. Each scale uses two independently verified fresh writable copies: 200 browse-only samples with a dedicated RSS monitor, then a separate process with 200 browse samples and 100 ratings/100 edits while importing 6,400 rows in 200 transactions. The native workload intentionally differs from Python: consecutive foreground targets, template-row cloning inside IMMEDIATE transactions, and one import/write handshake per operation. Its results are separate evidence, not a cross-language speed comparison.

| Assets | Native browse p95 | Rating p95 | Edit p95 | Dedicated browse peak | Combined-process peak (diagnostic) |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1M | 0.136 ms | 83.704 ms | 83.712 ms | 11.06 MiB | 83.16 MiB |
| 5M | 0.135 ms | 84.466 ms | 83.580 ms | 11.11 MiB | 329.42 MiB |
| 10M | 0.129 ms | 86.948 ms | 87.238 ms | 11.08 MiB | 329.45 MiB |

All native p95 gates pass. The 10M edit p99 is 105.234 ms and maximum is 116.937 ms; passing p95 does not imply every operation finishes within 100 ms. Dedicated browsing holds only the pages this workload touches, so its roughly 11 MiB peak is not a prediction for a complete UI or filtered browsing session. The earlier frozen native run had only 50 samples per write type and approximately 99 ms p95; it remains separately reported.

Local validation passes all 40 Rust tests, formatting, and clippy with warnings denied, plus all 39 Python benchmark contract tests. The full RAW implementation was unchanged by backend integration; its prior private-corpus and cross-platform evidence remains in sc-22838. Final hosted CI and merge evidence belongs to the execution ledger.

## Evidence and reproducibility

The reference computer is a MacBook Pro with Apple M5 Max, 18 logical CPUs and 128 GiB RAM, running macOS 26.6.2. Synthetic catalogs are on the internal APFS SSD; originals and Lightroom catalogs on the external RAID remain untouched. Python 3.14.6 drives SQLite 3.53.3 and DuckDB 1.5.5. The private build receipt records the current native executable, source hashes, Cargo lockfile, toolchain, and Rust compiler identity.

Preparation, copy validation, and full database hashing occur outside timed query intervals and warm OS caches. “Fresh” means a newly opened process/database connection; it does not establish cold filesystem-cache performance. All timing runs were serialized, with local build/render lanes paused. The passive observer retained ordinary desktop CPU/RAM/GPU/I/O observations; it was stopped after the last native run, with a final SIGTERM receipt (6,472 samples). This was a controlled development-host run, not an otherwise empty operating system.

- [Original comparison](BASELINE_BENCHMARK_RESULTS.md): all p50/p95/p99 distributions, constrained 64 MiB failures, loading/storage/growth, plans, recovery, and original native evidence.
- [Native runtime results](NATIVE_RUNTIME_RESULTS.md): all 15 native distributions, separate RSS, artifact identities, and tail samples.
- [Production profiles and correction](PRODUCTION_PROFILE_RESULTS.md): all 15 profile outcomes, complete corrected page/write tables, memory distinctions, query-work counters, and receipt hashes.
- [Frozen protocol](../benchmarks/README.md), [profile protocol](../benchmarks/PRODUCTION_PROFILE_PROTOCOL.md), [correction protocol](../benchmarks/CANDIDATE_VALIDATION_PROTOCOL.md), [work diagnostics](../benchmarks/QUERY_WORK_PROTOCOL.md), and [native runtime protocol](../benchmarks/NATIVE_RUNTIME_VALIDATION.md).

Private evidence under `/Users/michael/PhotoCatalog-private-results/` is retained outside Git:

| Artifact | Purpose |
| --- | --- |
| `sc-22837-final-v2` | Completed frozen original comparison; failures retained |
| `sc-22837-production-profiles` | Completed predeclared memory profiles |
| `sc-22837-production-pristine` | Original preserved synthetic snapshots |
| `sc-22837-query-work-pristine-v3/derivation.json` | Verified standalone clone provenance; no companion removal |
| `sc-22837-query-work-v3` | Original/corrected Python engine work, full records, preservation proofs, and failed diagnostic receipts |
| `sc-22837-sqlite-page-correction-v1` | One-shot corrected timing, full records/sample evidence, unchanged aggregate references |
| `sc-22837-native-query-work-v1` | Bundled Rust work and independent full-record reconciliation |
| `sc-22837-native-runtime-v1` | Current native timing, separate copies/RSS, raw results, and build-reference.json |
| `sc-22837-measurement-20260909-host.jsonl` | Complete competing-load observations and final observer shutdown |

Confidence is high that SQLite satisfies this database contract on the measured reference workload. Performance on other distributions, machines, storage, and integrated image/UI workloads still requires their respective acceptance checks. Query-shape changes or new indexes must preserve bounded navigation and repeat affected representative measurements.
