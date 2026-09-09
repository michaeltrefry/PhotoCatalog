# Production profiles and query-work results

Status: completed supplemental evidence, 2026-09-09. The subsequent [backend decision](BACKEND_DECISION.md) integrates these results with native validation.

DuckDB's 1024 MiB profile passes the supplemental numerical gates at 1M, 5M, and 10M records, but its tested deep-page queries perform increasing scan work as catalog size grows. SQLite's frozen queries have no passing profile through 2048 MiB. The separate SQLite query correction at 256 MiB passes its page, memory, write, correctness, and recovery checks at all three scales; separate work receipts show 2,410 VM steps for deep pages and 2,413 for rating pages at every tested scale and cursor. Native Rust integration and independent acceptance review are recorded separately in the backend decision and execution ledger.

Confidence is high in these receipt-level observations. They do not establish performance on other computers, storage, distributions, or production implementations. This report summarizes existing JSON evidence only; preparing it did not execute databases, tests, builds, or new measurements. The original failed results remain valid evidence alongside the correction.

## Contract and evidence boundaries

The [production-profile protocol](../benchmarks/PRODUCTION_PROFILE_PROTOCOL.md), [query-work protocol](../benchmarks/QUERY_WORK_PROTOCOL.md), and [page-correction protocol](../benchmarks/CANDIDATE_VALIDATION_PROTOCOL.md) define separate experiments. The fixed acceptance conditions are:

- One declared production configuration across all workloads and all 1M/5M/10M scales; try 256, then 1024, then 2048 MiB and stop at the first profile passing every scale.
- Every 200-row warm metadata page: p95 ≤100 ms. Every fresh-process database-open-plus-page: p95 ≤500 ms.
- Acknowledged durable rating and edit transactions during active background import: each p95 ≤100 ms, with complete samples, successful commits and readbacks, and no operation errors.
- Measured warm and fresh browsing process RSS ≤4 GiB. Engine allowances are not RSS ceilings: SQLite's cache is per connection; DuckDB's buffer allowance excludes some allocations.
- Exact workload, parameter and sample coverage; correct full record projections and stable identities/metadata; complete plans; valid pristine copies, foreign-key integrity and unchanged sources; forced interruption before/after commit must recover the expected values 1/2.
- Ordinary deep navigation must avoid work growing with catalog size for a fixed page. Plan presence and low sampled latency alone do not prove this.
- Reviewed evidence and competing-load context, followed by actual Rust production integration using the chosen settings and queries, with catalog preservation, browse/reopen and durable writes during import validated.

Each supplemental warm distribution has 100 samples after three warmups; each fresh distribution has 20 new-process samples. Complete mixed runs have 100 ratings, 100 edits and 200 pages during import. Mixed RSS and page-during-import latency are reported without adding thresholds absent from the contract. Aggregates, loading cost and disk growth remain decision inputs, not 100 ms page gates. The original 64 MiB constrained-memory matrix remains stress evidence, including failures; it is not an automatic disqualification of a passing production configuration. See [original results](BASELINE_BENCHMARK_RESULTS.md).

The measurement host reports macOS 26.6.2/arm64, 18 logical CPUs and 128 GiB RAM; Python 3.14.6, SQLite 3.53.3 and DuckDB 1.5.5. These are native engines through Python bindings, not the bundled Rust SQLite build. Preparation, copy validation and hashes are outside measured intervals and can warm OS caches. Fresh-process results therefore do not mean cold storage. Host snapshots accompany the receipts; they do not by themselves establish uninterrupted absence of foreign CPU, I/O or GPU work. The coordinator's separate host-load record and independent reconciliation remain part of final acceptance. No RAW, preview, image-editing, UI or RAID performance is inferred.

## Supplemental profiles using the frozen SQL

All times below are milliseconds. Warm/fresh values are the largest workload p95 at that scale, excluding aggregates; the workload is in parentheses. RSS is GiB. **Warm RSS here covers the whole subprocess, including its aggregate workload**, not an isolated page measurement. The retained evaluator conservatively applies its RSS gate to that process peak. Fresh RSS is the maximum across the seven page subprocess groups. Write values are rating/edit p95 during import.

| Engine | Allowance MiB | Records | Worst warm p95 | Worst fresh open+page p95 | Peak warm / fresh RSS | Rating / edit p95 | Failed checks |
|---|---:|---:|---:|---:|---:|---:|---|
| sqlite | 256 | 1M | 12.780 (rating) | 25.692 (rating) | 1.973 / 0.178 | 20.003 / 18.605 | None |
| sqlite | 256 | 5M | 119.544 (rating) | 116.317 (rating) | 2.134 / 0.361 | 21.338 / 17.980 | warm_pages |
| sqlite | 256 | 10M | 224.582 (rating) | 221.197 (rating) | 2.364 / 0.361 | 22.119 / 34.329 | warm_pages |
| sqlite | 1024 | 1M | 9.306 (rating) | 25.090 (rating) | 2.549 / 0.178 | 18.843 / 18.582 | None |
| sqlite | 1024 | 5M | 91.416 (rating) | 123.871 (rating) | 2.435 / 0.693 | 26.706 / 19.252 | None |
| sqlite | 1024 | 10M | 241.734 (rating) | 256.414 (rating) | 4.585 / 1.297 | 35.678 / 69.560 | warm_pages, warm_rss |
| sqlite | 2048 | 1M | 10.021 (rating) | 27.809 (rating) | 2.549 / 0.178 | 19.820 / 19.651 | None |
| sqlite | 2048 | 5M | 59.462 (rating) | 135.979 (rating) | 4.060 / 0.693 | 25.074 / 17.865 | warm_rss |
| sqlite | 2048 | 10M | 190.873 (rating) | 258.860 (rating) | 4.819 / 1.342 | 35.405 / 32.591 | warm_pages, warm_rss |
| duckdb | 256 | 1M | 8.652 (date) | 15.982 (date) | 0.230 / 0.130 | 3.711 / 3.495 | None |
| duckdb | 256 | 5M | 19.854 (keyword) | 28.759 (keyword) | 0.543 / 0.232 | Incomplete: OOM | durable_writes |
| duckdb | 256 | 10M | 39.100 (keyword) | 51.038 (keyword) | 0.813 / 0.306 | Incomplete: OOM | durable_writes |
| duckdb | 1024 | 1M | 8.612 (date) | 16.259 (date) | 0.236 / 0.130 | 4.738 / 4.154 | None |
| duckdb | 1024 | 5M | 19.570 (keyword) | 29.799 (keyword) | 0.750 / 0.232 | 3.711 / 3.840 | None |
| duckdb | 1024 | 10M | 38.332 (keyword) | 52.207 (keyword) | 1.368 / 0.397 | 4.329 / 5.032 | None |

DuckDB 256 MiB failed at 5M after only 21 rating and 20 edit samples, with foreground allocation failure and a background commit/pin failure near its configured limit. At 10M it failed after one rating and one edit, again on allocation. Partial write p95 values are not passing evidence. DuckDB 1024 MiB completed all writes without these errors; its worst warm/fresh p95 was 38.332/52.207 ms, maximum warm/fresh RSS 1.368/0.397 GiB, and worst rating/edit p95 4.738/5.032 ms. The protocol consequently did not run DuckDB 2048 MiB.

For SQLite, larger cache allowance alone did not resolve the frozen query shape. The 1024 and 2048 MiB attempts also exceeded the retained warm-process RSS gate. All profile/scale receipts pass the remaining reported pristine-copy, plan, fresh-page, fresh-RSS and recovery checks. The manifest's `selected_profiles_mib` is `duckdb: 1024, sqlite: null`; this is numerical profile eligibility, not a product backend decision.

## Runtime work, separate from timing

Nine successful protocol-3 receipts cover baseline SQLite 256 MiB, corrected SQLite 256 MiB and baseline DuckDB 1024 MiB at all three scales. Each has six cases: deep/rating pages at 50%, 90%, and frozen iteration 9's exact 90.5% cursor. Rating is 4 in the first two cases and 5 in iteration 9. Each returns the first qualifying 200 rows with all six projected fields checked against the independent frozen-generator oracle. All report complete collection and preserved source files. The read-only standalone SQLite copies had no required WAL companions.

The first standalone DuckDB diagnostic failed at connection setup because `enable_progress_bar` was incorrectly passed as global configuration. The repaired diagnostic applies it as a session setting after opening read-only, retains its dedicated spill directory, and completed all three scales under fresh `standalone-fixed` receipt identities. The failed receipt remains archived and contributes no query-work result.

| SQLite baseline case | VM steps at 1M | At 5M | At 10M | Sorts per execution |
|---|---:|---:|---:|---:|
| Deep, 50% | 2,410 | 2,410 | 2,410 | 0 |
| Deep, 90% | 1,102,615 | 5,502,615 | 11,002,615 | 1 |
| Deep, 90.5% | 1,047,615 | 5,227,615 | 10,452,615 | 1 |
| Rating, 50% | 333,882 | 1,652,958 | 3,306,489 | 1 |
| Rating, 90% | 69,167 | 330,703 | 667,996 | 1 |
| Rating, 90.5% | 65,778 | 319,251 | 630,342 | 1 |

The corrected SQLite SQL uses assets as the outer loop for deep pages and the rating/asset-id index as the outer loop for rating pages, preserving join membership, full projection, ordering and limit. Every corrected deep case records **2,410 VM steps**, every corrected rating case **2,413**, with **zero sorts** across all three scales and cursor cases. There is no limit before the join. All baseline and corrected cases report zero `FULLSCAN_STEP`; the baseline's large VM counts demonstrate why that counter alone is inadequate. The native build lacks statement scan-status support, so per-loop `NVISIT`/`NLOOP` are explicitly unavailable. VM steps are not physical I/O counts or a universal complexity proof for arbitrary sparse catalogs.

| DuckDB baseline cursor, both workloads | Summed operator rows scanned at 1M | At 5M | At 10M |
|---|---:|---:|---:|
| 50% | 996,092 | 3,076,662 | 5,526,207 |
| 90% | 340,153 | 1,020,014 | 1,531,214 |
| 90.5% | 340,153 | 1,020,014 | 1,531,215 |

DuckDB's reported scan work grows substantially for a fixed 200-row page. Its numerical profile pass therefore does not establish the required bounded navigation behavior. These are summed engine operator counters, potentially spanning multiple inputs, not distinct photos read or directly comparable to SQLite VM instructions. Diagnostic profiling times are not latency-eligibility measurements.

## SQLite correction timing at 256 MiB

The version-1 correction experiment is now complete with `all_pass: true` at every scale. Only `page_deep` and `rating` SQL changed; the other five page queries, schema, dataset, transaction shape and durability settings remained fixed. All warm/fresh result hashes match the original baseline's complete iteration/row-count/hash arrays. The experiment records successful source/copy integrity, settings/identity, seven plans, complete distributions, mixed writes/readbacks and interrupted-transaction recovery. Its own verdict explicitly leaves bounded work to the separate diagnostics and native production validation pending.

Each table cell below is **p50 / p95 / p99 in milliseconds**. Each warm page has n=100; each fresh open+page and fresh query-only distribution has n=20. RSS is measured separately below. Values are rounded to three decimals; complete precision and samples remain in the private receipt.

| Records | Page | Warm query | Fresh open+query | Fresh query only |
|---|---|---:|---:|---:|
| 1M | page_deep | 0.098 / 0.135 / 0.147 | 0.850 / 0.905 / 1.047 | 0.171 / 0.212 / 0.222 |
| 1M | folder | 0.391 / 0.494 / 0.524 | 1.260 / 1.334 / 1.460 | 0.570 / 0.620 / 0.638 |
| 1M | date | 0.156 / 0.335 / 0.370 | 1.084 / 1.144 / 1.146 | 0.418 / 0.462 / 0.471 |
| 1M | rating | 0.133 / 0.229 / 0.257 | 0.996 / 1.051 / 1.084 | 0.309 / 0.336 / 0.358 |
| 1M | keyword | 0.243 / 0.292 / 0.308 | 1.118 / 1.156 / 1.174 | 0.446 / 0.479 / 0.495 |
| 1M | combined | 0.257 / 0.323 / 0.340 | 1.250 / 1.315 / 1.324 | 0.572 / 0.603 / 0.613 |
| 1M | collection | 0.400 / 0.496 / 0.551 | 1.473 / 1.522 / 1.628 | 0.779 / 0.822 / 0.850 |
| 5M | page_deep | 0.103 / 0.143 / 0.161 | 0.851 / 0.908 / 0.933 | 0.174 / 0.186 / 0.186 |
| 5M | folder | 0.389 / 0.479 / 0.602 | 1.280 / 1.360 / 1.691 | 0.575 / 0.621 / 0.812 |
| 5M | date | 0.165 / 0.360 / 0.398 | 1.107 / 1.167 / 1.218 | 0.426 / 0.449 / 0.504 |
| 5M | rating | 0.144 / 0.228 / 0.252 | 0.979 / 1.032 / 1.037 | 0.294 / 0.333 / 0.346 |
| 5M | keyword | 0.261 / 0.293 / 0.394 | 1.093 / 1.151 / 1.165 | 0.426 / 0.457 / 0.500 |
| 5M | combined | 0.271 / 0.327 / 0.350 | 1.227 / 1.280 / 1.295 | 0.549 / 0.592 / 0.596 |
| 5M | collection | 0.584 / 0.680 / 0.797 | 1.443 / 1.517 / 1.534 | 0.758 / 0.838 / 0.875 |
| 10M | page_deep | 0.105 / 0.142 / 0.151 | 0.869 / 0.915 / 1.020 | 0.174 / 0.190 / 0.193 |
| 10M | folder | 0.426 / 0.577 / 0.628 | 1.272 / 1.327 / 1.360 | 0.579 / 0.630 / 0.645 |
| 10M | date | 0.172 / 0.375 / 0.539 | 1.127 / 1.167 / 1.184 | 0.447 / 0.480 / 0.495 |
| 10M | rating | 0.149 / 0.250 / 0.276 | 0.996 / 1.108 / 1.340 | 0.311 / 0.354 / 0.430 |
| 10M | keyword | 0.275 / 0.329 / 0.421 | 1.150 / 1.233 / 1.522 | 0.453 / 0.520 / 0.525 |
| 10M | combined | 0.283 / 0.355 / 0.414 | 1.264 / 1.304 / 1.310 | 0.560 / 0.610 / 0.629 |
| 10M | collection | 0.589 / 0.695 / 0.772 | 1.480 / 1.587 / 1.632 | 0.782 / 0.867 / 0.932 |

| Records | Rating (n=100) | Edit (n=100) | Page during import (n=200) | Warm / fresh / mixed peak RSS, GiB |
|---|---:|---:|---:|---:|
| 1M | 12.022 / 19.530 / 23.856 | 14.448 / 19.738 / 21.006 | 0.234 / 0.307 / 0.428 | 0.250 / 0.051 / 0.120 |
| 5M | 12.760 / 20.499 / 20.818 | 12.547 / 19.844 / 21.261 | 0.228 / 0.286 / 0.305 | 0.311 / 0.051 / 0.360 |
| 10M | 14.268 / 20.813 / 21.562 | 16.559 / 20.525 / 22.710 | 0.233 / 0.312 / 0.412 | 0.317 / 0.051 / 0.360 |

No foreground or background errors were recorded in these correction runs. The worst warm page p95 is 0.695 ms (10M collection), worst fresh open+page p95 1.587 ms (10M collection), worst rating p95 20.813 ms and worst edit p95 20.525 ms (both 10M). Correction warm RSS is **browse-only**, because the unchanged aggregate was not rerun; it must not be presented as directly comparable to the supplemental warm subprocess peaks that include aggregates.

The correction retains the original aggregate evidence by receipt identity and digest, without making a new aggregate measurement or awarding a new aggregate pass:

| Records | Original aggregate p50 / p95 / p99, ms | Samples |
|---|---:|---:|
| 1M | 321.156 / 346.380 / 365.167 | 100 |
| 5M | 2188.835 / 2237.395 / 2294.056 | 100 |
| 10M | 5167.797 / 5231.000 / 5271.635 | 100 |

## Provenance and remaining acceptance

Raw receipts and source paths remain private, outside Git. The following are logical archive-relative identifiers, not public links:

- `sc-22837-production-profiles/production_profiles.json` and `{sqlite,duckdb}/{memory}/{count}/receipt.json`: all 15 completed profile/scale attempts above.
- `sc-22837-query-work-v3/sqlite-{count}-256mib-{baseline,candidate}-standalone/receipt.json`: six completed SQLite work receipts.
- `sc-22837-query-work-v3/duckdb-{count}-1024mib-baseline-standalone-fixed/receipt.json`: three completed DuckDB work receipts; the failed 1M receipt without `-fixed` is retained separately.
- `sc-22837-sqlite-page-correction-v1/experiment.json`: all three correction scales, child request/settings identities, complete distributions, copy proofs and original aggregate references.
- `sc-22837-final-v2/{count}/sqlite.json`: original aggregate evidence, under its original measurement conditions; original results remain described in [BASELINE_BENCHMARK_RESULTS.md](BASELINE_BENCHMARK_RESULTS.md).

Recorded SHA-256 identities below were read from receipts, not recomputed during report preparation:

| Artifact | Recorded SHA-256 |
|---|---|
| Frozen comparison harness | `167b13d524c0f21a18426bb8b21acc6792f275de7884edca1cf995c6ff874c89` |
| Pristine snapshot manifest | `6b6022acf3da78434708414398c06eb8054fe9dc52a854bf5c89189f577778a4` |
| Supplemental profile driver / correction copy helper | `1c509cf8a06ce2dba734ef71ca4aecd6b4a50d1b9eac513ab9cc9ee2a8e6f5eb` |
| SQLite work diagnostic script | `4a4b61885e849dabb5f6865bd69d614b0f3a7efa3992de3546230d0dd944049f` |
| Repaired DuckDB work diagnostic script | `f2b748bbff97a456a133965c4d1dc98215d8066647df76f236abe32266a32bc9` |
| Baseline two-query diagnostic SQL map | `d38ba57499d4c1c726209f6f7dba1b2d218bfb52521eb819c6831bdd243fc5cb` |
| Corrected two-query diagnostic SQL map | `ea27cf0bb610b85b49909a3bbe5bb8a7ceab5029b5797ccd7bf5b87cbd238bf0` |
| Candidate module | `94d9b423434fcf782f2b3115b44a5ed6d73febf324434acdf05cb712ad178768` |
| Correction timing driver | `85146fee8b36334e12cfc3b636e386538e86307b7cd738e3374abcbeebec82fe` |
| Correction seven-query SQL map | `a35bab1734a083dfcaaeebe88c90c741495fbc26255d1036b38f4d6231f5f8e3` |
| Original aggregate campaign manifest | `f3b8e81a28fba9c76aba49138c40972d10cb13acfed95ded9ae64e5548e85536` |

The corrected SQLite query path has reviewed timing, full-record, and measured-work evidence on the tested generator. Subsequent [native validation](NATIVE_RUNTIME_RESULTS.md) passes the actual Rust Catalog browse/get/reopen, declared settings, stable identity/metadata preservation, and durable writes during import. Bundled SQLite also reproduces the equivalent corrected joined queries in the separate work diagnostic. The organization API and UI remain scoped to sc-22842 and sc-22847; S2 integrates and validates the existing skeleton's selected backend.

Independent reconciliation covers these receipts, native evidence, and the retained original aggregate/load/size/growth results. [BACKEND_DECISION.md](BACKEND_DECISION.md) records the SQLite selection and host-load context. This report retains the complete experimental comparison; final CI, merge and tracker closeout remain recorded in the execution ledger.
