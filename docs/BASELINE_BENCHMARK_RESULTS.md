# Original baseline benchmark results — sc-22837

The completed original campaign does **not establish a passing default configuration for either engine across all scales**. At 256 MiB, SQLite exceeds the 100 ms warm page p95 target for rating at 5M and for deep sequence/rating at 10M. DuckDB completes its 256 MiB read matrix but runs out of memory during mixed import/write work at 5M and 10M. This report preserves those failures and the separate 64 MiB stress evidence. It makes no backend selection and no claim that deep navigation performs bounded runtime work.

The manifest records `complete=true`, `prepared_only=false`, seed 22837, schema/workload version 2, 100 warm repetitions and 20 fresh-process repetitions. Exit 0 means orchestration finished and recorded child errors, not that all budgets passed. The frozen summary ANDs 64 MiB stress into `all_pass`; that extra condition is not a production-configuration acceptance rule. The reviewed supplemental protocol evaluates one configuration per engine across all queries and scales separately.

## Scope and notation

All latency cells show **p50/p95/p99 in milliseconds, with measured sample count in parentheses**. Numbers are rounded to 0.001 ms; full precision and raw samples remain external. Quantiles linearly interpolate ordered samples. Partial distributions describe successful samples before a failure and cannot establish workload eligibility.

Approved thresholds: warm 200-row page p95 ≤100 ms; fresh-process open-plus-page p95 ≤500 ms; durable rating/edit p95 ≤100 ms during import; browse-process RSS ≤4 GiB. Aggregates do not inherit the page target. Fresh measurements use 256 MiB and one new process per sample. **Cold OS/filesystem cache was not measured.** Warm measurements use three warmups then 100 queries per workload. Deep cursors span 50–90.5%; the [versioned workload](../benchmarks/README.md) specifies filter and collection cursor rules.

The synthetic corpus has 1M/5M/10M assets, two keyword relationships per asset and one collection membership per five assets. Mutable ratings occupy a dependent annotations table; foreign keys remain enabled. Metadata, UUIDs, locations and preview hashes are synthetic. No image bytes, compressed preview payloads, private photos, RAW processing, rendering or UI performance were measured. Disk totals therefore are not complete catalog sizes including previews.

## Host, versions and settings

The host reports macOS 26.6.2, arm64, 18 logical CPUs and 128 GiB RAM. A separate read-only hardware check identifies Apple M5 Max, model Mac17,6, and confirms the benchmark root is on the internal solid-state APFS Data volume (Apple Fabric; container capacity 3,996,276,899,840 bytes). This is internal SSD catalog performance; originals remain on the external RAID. The hardware receipt is `/Users/michael/PhotoCatalog-private-results/sc-22837-reference-hardware-20260909.json`. Python is 3.14.6; native Python bindings report SQLite 3.53.3 and DuckDB 1.5.5. The separate Rust probe reports bundled SQLite 3.51.1; its receipts do not embed compiler or executable digest, so binary provenance is not inferred from the Python version. The execution ledger separately binds the frozen probe binary to its reviewed source and verified SHA-256.

| Host receipt phase | Available RAM (GiB) | Load average 1/5/15 min |
| --- | --- | --- |
| Preparation | 29.276 | 3.957/4.884/5.932 |
| Measurement start | 106.101 | 3.885/3.739/4.385 |
| Measurement end | 106.425 | 4.102/3.387/3.472 |

Preparation and later foreground measurement were separate phases. Loading occurred with substantially less available RAM; the preparation times below are not quiet-host ingestion throughput. These snapshots do not prove absence of every competing workload or GPU idleness; host observation/reservation evidence is separate.

SQLite uses WAL, synchronous FULL, fullfsync ON, foreign keys ON, mmap disabled, file-backed temporary storage, 5000 ms busy timeout and 1000-page WAL autocheckpoint. Cache sizes are −262144 KiB (256 MiB), −65536 KiB (64 MiB), and −4194304 KiB for preparation. DuckDB uses four threads, insertion-order preservation, 16 MiB checkpoint threshold, progress output disabled and matching 256/64 MiB memory limits; preparation uses 4096 MiB. Engine cache/buffer limits are not process-RSS caps. Native Rust mixed receipts record WAL/FULL/fullfsync, 256 MiB cache and 5000 ms busy timeout.

Both engines retain durable commit settings and relationship integrity. SQLite fullfsync aligns with DuckDB 1.5.5's macOS full filesystem-sync path; the [durability rationale](../benchmarks/README.md) documents the source comparison. Process-kill recovery is measured below; physical power-loss recovery is not.

## Warm distributions

Every numeric cell has 100 samples. `OOM/no receipt` means the DuckDB 64 MiB child exited during warmup and published neither distributions nor RSS. Earlier in-process work, if any, was not retained, so neither a zero timing nor a partial retained count is inferred. Aggregate rows are not 200-row pages.

### 1M assets

| Workload | SQLite 256 MiB | SQLite 64 MiB | DuckDB 256 MiB | DuckDB 64 MiB |
| --- | --- | --- | --- | --- |
| page_deep | 0.120/6.980/12.554 (100) | 0.134/12.560/12.741 (100) | 6.454/7.364/7.594 (100) | OOM/no receipt |
| folder | 7.180/16.755/18.696 (100) | 0.350/0.448/0.464 (100) | 2.262/2.667/2.714 (100) | OOM/no receipt |
| date | 0.169/10.165/12.497 (100) | 0.166/0.313/0.331 (100) | 7.661/8.613/8.859 (100) | OOM/no receipt |
| rating | 5.526/19.515/30.515 (100) | 12.483/22.179/22.606 (100) | 4.666/5.003/5.038 (100) | OOM/no receipt |
| keyword | 0.480/0.663/0.687 (100) | 0.279/0.348/0.360 (100) | 5.682/6.929/7.030 (100) | OOM/no receipt |
| combined | 0.314/1.192/1.603 (100) | 0.362/0.446/0.517 (100) | 2.032/2.221/2.260 (100) | OOM/no receipt |
| collection | 0.566/1.067/1.802 (100) | 0.421/0.517/0.543 (100) | 4.841/5.420/5.518 (100) | OOM/no receipt |
| aggregate | 321.156/346.380/365.167 (100) | 324.672/362.449/386.501 (100) | 7.283/7.394/7.431 (100) | OOM/no receipt |

### 5M assets

| Workload | SQLite 256 MiB | SQLite 64 MiB | DuckDB 256 MiB | DuckDB 64 MiB |
| --- | --- | --- | --- | --- |
| page_deep | 0.146/65.384/70.761 (100) | 0.142/65.813/68.291 (100) | 13.954/17.419/19.550 (100) | OOM/no receipt |
| folder | 6.298/17.200/18.604 (100) | 0.373/0.472/0.643 (100) | 6.979/10.725/10.768 (100) | OOM/no receipt |
| date | 0.175/12.495/13.497 (100) | 0.170/0.338/0.357 (100) | 12.170/14.094/15.944 (100) | OOM/no receipt |
| rating | 72.449/126.031/252.013 (100) | 67.508/114.047/116.694 (100) | 4.766/5.281/5.333 (100) | OOM/no receipt |
| keyword | 0.574/0.771/0.833 (100) | 0.300/0.380/0.395 (100) | 12.410/19.231/19.399 (100) | OOM/no receipt |
| combined | 0.386/1.759/2.065 (100) | 0.378/0.433/0.492 (100) | 2.048/3.843/3.892 (100) | OOM/no receipt |
| collection | 0.816/7.449/10.248 (100) | 0.517/0.632/0.679 (100) | 9.093/12.382/12.497 (100) | OOM/no receipt |
| aggregate | 2188.835/2237.395/2294.056 (100) | 2147.714/2175.398/2240.251 (100) | 54.246/55.597/56.170 (100) | OOM/no receipt |

### 10M assets

| Workload | SQLite 256 MiB | SQLite 64 MiB | DuckDB 256 MiB | DuckDB 64 MiB |
| --- | --- | --- | --- | --- |
| page_deep | 0.144/132.594/143.804 (100) | 0.145/134.080/135.025 (100) | 21.985/36.818/38.634 (100) | OOM/no receipt |
| folder | 6.534/17.466/19.129 (100) | 0.378/0.458/0.520 (100) | 12.788/23.715/24.159 (100) | OOM/no receipt |
| date | 0.184/12.677/13.385 (100) | 0.171/0.338/0.353 (100) | 31.152/33.477/35.247 (100) | OOM/no receipt |
| rating | 148.965/266.941/490.646 (100) | 135.879/225.362/229.638 (100) | 9.043/11.289/11.478 (100) | OOM/no receipt |
| keyword | 0.596/0.775/0.830 (100) | 0.303/0.374/0.454 (100) | 23.872/39.366/39.551 (100) | OOM/no receipt |
| combined | 0.387/1.889/1.969 (100) | 0.383/0.446/0.644 (100) | 4.560/6.437/6.681 (100) | OOM/no receipt |
| collection | 0.856/8.565/11.102 (100) | 0.523/0.679/0.691 (100) | 17.902/26.319/26.984 (100) | OOM/no receipt |
| aggregate | 5167.797/5231.000/5271.635 (100) | 4317.614/4378.408/4411.300 (100) | 171.037/182.061/185.253 (100) | OOM/no receipt |

SQLite 256 MiB p95 misses: 5M rating 126.031 ms; 10M deep sequence 132.594 ms and rating 266.941 ms. At 64 MiB, 5M rating is 114.047 ms and 10M deep/rating are 134.080/225.362 ms. Other successful page distributions meet the sampled 100 ms target. All DuckDB 256 MiB warm pages meet it; its 64 MiB processes fail at every scale. Aggregate speed does not compensate for another workload's failure.

## Fresh-process distributions (256 MiB)

Each numeric cell has 20 samples with no child errors. `Open+page` is the approved metric; query-only values expose connection overhead. Every open-plus-page p95 is below 500 ms. OS caches are uncontrolled; fresh aggregate queries were not measured.

### 1M assets

| Workload | SQLite open+page | SQLite query only | DuckDB open+page | DuckDB query only |
| --- | --- | --- | --- | --- |
| page_deep | 0.908/16.253/16.683 (20) | 0.180/15.552/15.987 (20) | 13.160/14.326/14.371 (20) | 8.768/9.923/10.009 (20) |
| folder | 1.284/1.358/1.365 (20) | 0.565/0.595/0.604 (20) | 8.560/9.609/9.821 (20) | 4.249/5.417/5.555 (20) |
| date | 1.105/1.135/1.143 (20) | 0.427/0.451/0.456 (20) | 14.873/16.038/16.187 (20) | 10.425/11.571/11.628 (20) |
| rating | 17.098/27.791/29.282 (20) | 16.410/27.076/28.599 (20) | 10.835/11.337/11.386 (20) | 6.394/6.874/6.896 (20) |
| keyword | 1.184/1.259/1.275 (20) | 0.472/0.532/0.540 (20) | 12.813/14.619/14.666 (20) | 8.310/10.136/10.202 (20) |
| combined | 1.248/1.293/1.370 (20) | 0.544/0.612/0.623 (20) | 8.171/8.503/8.512 (20) | 3.772/3.997/4.054 (20) |
| collection | 1.497/1.538/1.577 (20) | 0.791/0.832/0.876 (20) | 11.724/12.530/12.823 (20) | 7.417/8.288/8.395 (20) |

### 5M assets

| Workload | SQLite open+page | SQLite query only | DuckDB open+page | DuckDB query only |
| --- | --- | --- | --- | --- |
| page_deep | 0.903/76.851/77.156 (20) | 0.183/76.138/76.457 (20) | 21.657/26.859/27.209 (20) | 16.564/21.744/21.758 (20) |
| folder | 1.295/1.386/1.417 (20) | 0.585/0.632/0.644 (20) | 15.684/21.297/21.383 (20) | 10.616/16.010/16.274 (20) |
| date | 1.143/1.220/1.250 (20) | 0.445/0.461/0.467 (20) | 20.125/21.265/21.409 (20) | 14.963/16.156/16.356 (20) |
| rating | 79.137/125.118/126.147 (20) | 78.426/124.400/125.467 (20) | 11.952/12.451/12.473 (20) | 6.862/7.225/7.290 (20) |
| keyword | 1.190/1.253/1.257 (20) | 0.485/0.538/0.547 (20) | 22.411/29.613/29.799 (20) | 17.255/24.550/24.749 (20) |
| combined | 1.283/1.313/1.350 (20) | 0.573/0.601/0.603 (20) | 9.147/10.739/10.811 (20) | 4.044/5.676/5.683 (20) |
| collection | 1.536/1.611/1.628 (20) | 0.819/0.855/0.871 (20) | 18.737/22.585/22.647 (20) | 13.725/17.486/17.528 (20) |

### 10M assets

| Workload | SQLite open+page | SQLite query only | DuckDB open+page | DuckDB query only |
| --- | --- | --- | --- | --- |
| page_deep | 0.908/139.606/139.975 (20) | 0.182/138.898/139.238 (20) | 31.839/48.266/48.693 (20) | 25.803/42.219/42.762 (20) |
| folder | 1.290/1.359/1.370 (20) | 0.580/0.634/0.640 (20) | 24.755/35.661/35.698 (20) | 18.808/29.595/29.614 (20) |
| date | 1.126/1.203/1.209 (20) | 0.424/0.460/0.461 (20) | 40.828/42.443/42.790 (20) | 34.892/36.497/36.855 (20) |
| rating | 143.903/232.949/233.239 (20) | 143.157/232.237/232.521 (20) | 17.257/19.728/19.789 (20) | 11.178/13.610/13.668 (20) |
| keyword | 1.183/1.231/1.294 (20) | 0.471/0.508/0.559 (20) | 35.020/51.404/52.625 (20) | 28.956/45.472/46.409 (20) |
| combined | 1.278/1.338/1.370 (20) | 0.562/0.630/0.640 (20) | 12.319/14.650/14.934 (20) | 6.223/8.497/8.670 (20) |
| collection | 1.519/1.570/1.571 (20) | 0.798/0.845/0.852 (20) | 30.396/38.201/38.952 (20) | 24.447/32.186/32.824 (20) |

## Mixed import, rating and edit distributions (256 MiB)

The Python mixed workload targets 200 alternating foreground transactions (100 rating/100 edit), each starting after an active background import transaction is observed. A page read follows each acknowledged write. Import uses equivalent bulk parameterized 32-row batches with relationships and the same 2 ms producer yield. Foreground timing spans BEGIN through durable COMMIT and includes binding overhead; readback verifies acknowledgements. Background row generation is outside transaction timing. Different run durations produce different committed import counts, so post-mixed contents are not expected to match between engines.

| Scale | Engine | Rating | Edit | Page during import | Background 32-row commit | Active starts | Imported rows | Result |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1M | sqlite | 6.511/29.117/36.752 (100) | 6.304/35.360/38.561 (100) | 0.342/18.057/19.366 (200) | 36.282/72.939/109.692 (62) | 200 | 1984 | complete |
| 1M | duckdb | 2.706/5.144/6.002 (100) | 2.794/4.270/4.809 (100) | 7.742/10.196/11.519 (200) | 26.874/29.238/38.258 (80) | 200 | 2560 | complete |
| 5M | sqlite | 6.534/28.123/40.166 (100) | 5.826/31.895/35.589 (100) | 0.770/78.643/82.506 (200) | 15.159/90.663/96.094 (127) | 200 | 4064 | complete |
| 5M | duckdb | 3.587/5.153/8.951 (21) | 2.995/4.933/5.259 (20) | 14.479/20.330/22.623 (40) | 25.628/29.609/42.304 (28) | 41 | 896 | OOM/partial |
| 10M | sqlite | 6.960/23.056/30.562 (100) | 6.237/57.891/78.664 (100) | 0.627/149.996/156.407 (200) | 10.624/91.602/102.742 (265) | 200 | 8480 | complete |
| 10M | duckdb | 10.344/10.344/10.344 (1) | 2.910/2.910/2.910 (1) | 47.516/47.516/47.516 (1) | 36.585/50.833/52.100 (3) | 2 | 96 | OOM/partial |

Background quantiles are recomputed solely from recorded samples using the frozen interpolation rule. All background error arrays are empty and the harness verifies final row count equals initial count plus committed imports. SQLite completes all 200 starts at every scale. DuckDB completes 1M; its 5M and 10M distributions are incomplete. Successful partial samples cannot establish eligibility. SQLite's 10M page-during-import p95 is 149.996 ms; the frozen mixed-writes summary checks rating/edit transactions and does not convert that page observation into a write-budget pass.

| Scale | Stage | Recorded failure | Evidence coverage |
| --- | --- | --- | --- |
| 1M | DuckDB 64 MiB warm | Allocate 32 KiB at 64.0/64.0 MiB; exit 1 | Warmup failure; no distributions/RSS |
| 5M | DuckDB 64 MiB warm | Allocate 256 KiB at 63.7/64.0 MiB; exit 1 | Warmup failure; no distributions/RSS |
| 10M | DuckDB 64 MiB warm | Allocate 128 KiB at 64.0/64.0 MiB; exit 1 | Warmup failure; no distributions/RSS |
| 5M | DuckDB 256 MiB mixed | Pin 256 KiB at 255.8/256.0 MiB; foreground OOM | 41 starts; 21/20/40 rating/edit/page samples |
| 10M | DuckDB 256 MiB mixed | Pin 256 KiB at 256.0/256.0 MiB; foreground OOM | 2 starts; 1/1/1 rating/edit/page samples |

## Memory, disk and preparation

RSS is in GiB. Warm peaks span the entire page-plus-aggregate process, not individual query peaks. Fresh RSS is the maximum across fresh samples; mixed peaks include incomplete runs. Every recorded successful browse peak is below 4 GiB; missing DuckDB 64 MiB peaks provide no pass evidence. The approved 4 GiB browse target does not apply to preparation RSS, which exceeds 4 GiB in some cases.

| Scale | Engine | Load RSS | Warm256 RSS | Warm64 RSS | Fresh max RSS | Mixed RSS | Loaded disk (bytes) | Mixed disk before→after (bytes) | Preparation load (s) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1M | sqlite | 1.253 | 2.008 | 1.774 | 0.178 | 0.134 | 930746368 | 930779136 → 933916672 | 35.669 |
| 1M | duckdb | 1.048 | 0.230 | unavailable | 0.130 | 0.265 | 543174656 | 543174656 → 627847168 | 7.365 |
| 5M | sqlite | 5.482 | 2.260 | 1.068 | 0.361 | 0.367 | 4670160896 | 4670193664 → 4677021696 | 249.718 |
| 5M | duckdb | 2.648 | 0.521 | unavailable | 0.233 | 0.477 | 2710056960 | 2710056960 → 2830643200 | 35.847 |
| 10M | sqlite | 5.928 | 1.864 | 0.693 | 0.361 | 0.373 | 9380655104 | 9380687872 → 9393852416 | 739.736 |
| 10M | duckdb | 4.529 | 0.748 | unavailable | 0.307 | 0.469 | 5381828608 | 5381828608 → 5454704640 | 54.450 |

Disk totals sum regular database-prefix files, including matching WAL/SHM when present; mixed-after sizes follow checkpointing. They exclude CSV data, directories of spill files, originals and preview payloads. SQLite loading uses Python batches of 10000; DuckDB uses native COPY per CSV table, followed by indexing/integrity verification and checkpointing. Preparation timings include these binding differences and are not a same-adapter insertion microbenchmark.

## Integrity, hash reconciliation and recovery

Read-only receipt reconciliation compared exact warm 256 MiB correctness arrays between engines: iteration, row count and selected-result SHA-256. Fresh-process hashes were also compared pairwise for each workload/iteration. All scales match, and SQLite 64/256 MiB arrays match. DuckDB 64 MiB has no comparable array. Post-mixed states are excluded because import counts and surviving work differ.

| Scale | Warm256 paired records | Fresh paired records | Assets | Keyword relations | Collection relations | Rating sum | Sequence sum |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1M | 800 matched (700 page + 100 aggregate) | 140 matched | 1000000 | 2000000 | 200000 | 899331 | 500000500000 |
| 5M | 800 matched (700 page + 100 aggregate) | 140 matched | 5000000 | 10000000 | 1000000 | 4501732 | 12500002500000 |
| 10M | 800 matched (700 page + 100 aggregate) | 140 matched | 10000000 | 20000000 | 2000000 | 9005579 | 50000005000000 |

Totals: 2400 paired warm records and 420 paired fresh records match, with another 2400 SQLite 64/256 records matching. Each page has 200 selected rows; aggregates have 115 groups. Loader proofs also match captured-date and file-byte sums against CSV manifests, and the frozen loader checks first/middle/last full generator records. This is not an exhaustive hash of every database row; selected page hashes exclude unselected metadata columns.

All six engine/scale recovery receipts pass both forced-kill boundaries. Before the tested commit, reopened value 1 equals expected 1; after its acknowledged commit, value 2 equals expected 2. All workers exit −9. This confirms process-crash transaction recovery, not physical power-loss recovery. Subsequent recovery success does not erase prior read/write OOM failures.

EXPLAIN receipts include all eight queries but only frozen iteration 0. Plan presence does not establish bounded work: index range scans, joins and sorts can process growing ranges. Runtime scan/VM counters and 50%/90% query-work diagnostics were not measured in this original campaign.

## Native Rust probe — separate evidence

The Rust Catalog probe uses bundled SQLite 3.51.1, versus Python's 3.53.3. It measures browse/get with JSON deserialization and synthetic metadata, a different API/query shape from joined organization queries. Its single open time is not a 20-sample fresh distribution. Native mixed work clones 32 synthetic template rows plus relationships per transaction and releases the foreground after the background obtains its write lock. This differs from the Python active-transaction event, so its quantiles must not be pooled with Python values or used to erase failed filtered navigation.

| Scale | Browse | Rating | Edit | Background 32-row commit | Single open (ms) | Sampled RSS (GiB) | Imported rows | Identity/metadata/restart verified |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1M | 0.172/0.276/0.290 (100) | 67.954/74.419/116.649 (50) | 67.910/71.416/72.533 (50) | 50.268/61.980/63.619 (100) | 13.738 | 0.076 | 3200 | True |
| 5M | 0.101/0.134/0.144 (100) | 67.575/83.521/124.536 (50) | 67.545/78.626/96.062 (50) | 57.107/72.229/77.967 (100) | 17.034 | 0.316 | 3200 | True |
| 10M | 0.100/0.142/0.150 (100) | 91.919/99.544/109.427 (50) | 92.724/99.189/101.412 (50) | 64.792/79.637/83.627 (100) | 17.769 | 0.316 | 3200 | True |

Native browse has 100 samples, rating/edit 50 each, and background commits 100 per scale. Its sampled page/write p95 values meet numeric thresholds; 10M rating/edit p95 are near the 100 ms boundary at 99.544/99.189 ms. Some write p99 values exceed 100 ms; no p99 target was imposed. Plans report an integer-primary-key range search, with no runtime-work counters. This is evidence for the measured skeleton path, not final backend selection, filtered navigation or image throughput.

## Evidence locations and identities

Private root: `/Users/michael/PhotoCatalog-private-results/sc-22837-final-v2`. Engine receipts are `<count>/sqlite.json` and `<count>/duckdb.json`; native receipts are `<count>/rust_native.json`. They retain full distributions, hashes, settings, EXPLAIN output, errors and recovery observations. This document was assembled using small JSON parses only: no database queries, tests, builds, scale reruns, rendering or large artifact hashing.

Frozen harness SHA-256: `167b13d524c0f21a18426bb8b21acc6792f275de7884edca1cf995c6ff874c89`. The following hashes cover only small manifests for this completed baseline, not the running supplemental campaign.

| Manifest | Bytes | SHA-256 |
| --- | --- | --- |
| campaign.json | 2251 | `f3b8e81a28fba9c76aba49138c40972d10cb13acfed95ded9ae64e5548e85536` |
| budget_summary.json | 6587 | `2810cfdd3037f1c5e1569477040d75233722cacfa7b4c0b689169e6becd99c6f` |
| 1000000/csv/manifest.json | 702 | `7f058bdd38b811a20599fd3f4ca5e22c2a0c816cb614a603787fb8e562c3988b` |
| 5000000/csv/manifest.json | 706 | `1ed8b12bb3796e046f8b1f125db7939311913264bd6cf794478b5dee372ebabf` |
| 10000000/csv/manifest.json | 711 | `4bc910f34658e13a1305d5b09616e4eaf8ec05495ba40127db351625fbaded1e` |

Confidence: high in receipt transcription, counts, reconciliation and recorded failures. No confidence claim is made about unmeasured cold-cache behavior, bounded query work, power loss, preview storage or final backend choice.
