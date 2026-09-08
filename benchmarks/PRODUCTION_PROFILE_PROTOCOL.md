# Supplemental production cache profile protocol

This protocol supplements the frozen version-2 comparison. It does **not** rewrite its receipts or change its workloads, dataset seed, transaction shape, integrity checks, or durability. The existing `budget_summary.json` ANDs the 64 MiB and 256 MiB observations; treat that output as the original diagnostic matrix, not the final production eligibility decision.

The approved contract requires a constrained-memory test and a 4 GiB browse RSS budget. It does not require every query to satisfy the production latency target with a 64 MiB engine allowance. Report all 64 MiB results and failures as stress evidence. They do not automatically disqualify an engine whose declared production configuration meets the unchanged contract.

## Predeclared selection procedure

The default production allowance is **256 MiB**. If it fails, **1024 MiB**, then **2048 MiB**, are offered to each engine in that fixed order. A candidate must use **one configuration across all queries and all three scales**, with no selection of a different cache allowance for individual results. Stop at the first profile that passes every required scale; keep every earlier failure and raw receipt. If none passes, record no eligible production profile and escalate the architecture/requirement decision.

SQLite's allowance is per connection; DuckDB's is shared by its buffer manager and excludes some other allocations. Actual warm and fresh-process RSS remain gated at **4 GiB**. Higher allowances do not relax that measured RSS limit.

Unchanged acceptance thresholds:

- 200-row warm metadata pages: p95 ≤100 ms.
- Fresh-process database open plus page: p95 ≤500 ms.
- Acknowledged durable rating/edit write while importing: p95 ≤100 ms.
- Warm and fresh browse RSS: ≤4 GiB.
- Complete query-plan evidence, exact workload/sample coverage, unchanged identities and metadata, full durable commits, foreign-key integrity, and successful interrupted-transaction recovery.

The native Rust production integration must subsequently use the selected settings and pass its own validation. This supplemental driver measures native engines through the existing Python bindings; it does not substitute for the Rust integration evidence. Neither protocol claims cold OS-cache, image-processing, preview, or UI performance.

## Preserve pristine inputs first

Before the original campaign executes its read/mixed/recovery phase, snapshot the completed `--prepare-only` output:

```sh
python benchmarks/production_profiles.py snapshot \
  --source /absolute/sc-22837-final-v2 \
  --output /absolute/sc-22837-production-pristine
```

The source must have a successful preparation-only receipt for both engines at every scale, the exact frozen harness digest, and the matching generator manifest. The driver verifies logical counts/sums and selected identity/metadata records, all expected annotation rows, zero edit rows, and an untouched recovery marker. It rejects post-mixed state, a missing or failed load receipt, wrong counts, incomplete annotation state, or a changed source contract. A failed snapshot is not published as ready.

SQLite copies use its snapshot backup API through a read-only source connection. DuckDB copies hold a read-only engine connection that excludes external writers and require an absent/empty WAL. Each destination is independently verified and checkpointed before publishing the snapshot. Untimed copy verification uses a 4096 MiB buffer allowance; it is not a production candidate or a latency result. Sources are benchmark databases, and no original photos or Lightroom catalogs are accessed.

The snapshot manifest records exact driver/harness hashes, source manifest hash, dataset contracts, database digests, verification proof, preparation hardware, and all predeclared profiles. Preserve the snapshot while the original campaign runs; mixed writes must never contaminate the supplemental inputs.

## Run only in the reserved measurement window

```sh
python benchmarks/production_profiles.py run \
  --source /absolute/sc-22837-production-pristine \
  --output /absolute/sc-22837-production-profiles
```

The output must be new. The driver verifies its own and the frozen harness's hashes against the snapshot. For each attempted engine/profile/scale, it creates another independently verified, checkpointed working copy. The pristine baseline remains intact. Copying, digest checks, and verification are untimed preparation; all selected query/write measurements start afterward.

The driver invokes the **existing frozen CLI** for plans, warm queries, fresh-process queries, mixed writes, and forced process-crash recovery using the profile's explicit `--memory-mb` argument. It retains p50/p95/p99, raw samples, measured settings, correctness digests, RSS, background errors, and disk growth from those original commands. `production_profiles.json` records every attempted profile and chooses only one profile per engine that passes at every scale. No numerical threshold changes, per-query best-result selection, blanket retries, or suppressed operation failures are permitted.

A production decision still needs the full 1M/5M/10M scale set, quiet-host evidence, reviewed raw results, and native Rust validation. Small fixtures can test this driver's rejection logic but do not establish performance suitability.
