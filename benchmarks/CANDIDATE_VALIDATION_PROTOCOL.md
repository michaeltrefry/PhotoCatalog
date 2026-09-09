# SQLite page-correction experiment, version 1

This is a conditional corrective experiment for S2, not a replacement for the frozen SQLite/DuckDB comparison or its production-profile supplement. Run it only after separate runtime-work receipts establish that the original query path needs correction, and after the coordinator releases the controlled timing lane. The original campaigns, their SQL files, pristine snapshots, and all failed measurements remain immutable.

The hypothesis is that SQLite's join ordering causes unnecessary work for `page_deep` and `rating` at some deep cursors. The reviewed candidate in `query_candidates.py` expresses the same joins through `CROSS JOIN` and equivalent predicates, preserving the selected six fields, filtering, ordering, and limit. Only these two SQL strings change. This experiment measures that candidate with all five other page queries unchanged; it does not choose different SQL or cache sizes by workload or scale. Semantic equivalence must be demonstrated by returned records, not inferred from lower latency.

## Fixed scope and conditions

- SQLite only, with the default 256 MiB connection cache, at 1 million, 5 million, and 10 million assets. The runner requires the baseline's native SQLite version and exact connection settings. It records Python and engine versions, requested and observed settings, the complete seven-query candidate SQL map, its digest, original SQL digest, frozen harness digest, candidate module digest, copy-helper digest, and runner digest.
- Reuse the frozen generator, schema, parameter function, page validator, timing/RSS monitor, mixed-workload and recovery helpers. The unchanged settings include WAL, synchronous FULL, `fullfsync=1`, foreign keys enabled, memory mapping disabled, and the same transaction batch sizes. The candidate has no schema, indexed-data, aggregate, or durability change.
- Copy each scale from the reviewed pristine snapshot through `production_profiles.copy_checkpointed`, which checks generator relationships, unedited annotations, and initial recovery state. Verify the source digest against its manifest and again after the copy. Record the target's independent physical digest and logical proof. SQLite backup can change file-header bytes; source and target need not have identical physical hashes.
- Before any mixed writes on each copy, capture plans for all seven pages, then 100 warm samples and 20 fresh-process samples for every page. Warm sampling retains three warmups and iterations 0–99. Fresh sampling uses iterations 0–19, including the exact iteration-9 cursor/predicate from the frozen function. All exact parameters accompany the correctness hashes. Fresh processes do not evict OS caches and are not cold-storage evidence.
- Then run 200 alternating foreground operations during the same 32-row background import transactions: 100 durable ratings, 100 durable edits, and 200 pages during import. Require all foreground starts to overlap active import, all writes and readbacks to succeed, and complete samples. Finally kill the recovery worker before/after commit and require the original expected values 1/2 after reopening.

Warm page p95 remains at most 100 ms, fresh open-plus-query p95 at most 500 ms, and durable rating/edit p95 at most 100 ms. Measured warm, fresh, and mixed browsing RSS must be at most 4 GiB. The runner reports page-during-import latency without adding an independent threshold absent from the original contract. Every distribution includes n, p50, p95, p99, maximum, and raw samples; validation recomputes the distribution and rejects incomplete, invalid, or forged summaries. Missing plans, errors, incorrect settings/identity, non-200-row pages, and record-hash mismatches fail the experiment. No retries select a favorable result.

## Baseline reconciliation and aggregate evidence

Every warm and fresh page result must match the original baseline's complete `(iteration, row count, SHA-256)` correctness arrays for the same workload and parameters. The frozen validator also checks actual records against generated values and expected ordering. No post-import page hash is compared with a pristine baseline: the mixed helper performs its own write readbacks and page validation while records legitimately change.

The aggregate SQL and data are unchanged. Each scale references the original baseline receipt by absolute path and digest and embeds its original 100-sample aggregate distribution. The runner verifies that this evidence is complete but does not execute or award a new PASS to the aggregate. Use those original results, with their original preparation/measurement conditions, in the final combined decision. This avoids repeating 100 expensive aggregate scans per scale solely to validate a page-path correction.

The candidate runner's `all_pass` means only that this page-correction experiment passed its stated checks. It is not backend selection, bounded-work proof, fresh OS-cache proof, or native Rust production validation. Separate query-work diagnostics must establish the work performed at relevant deep cursors. Integration must still validate the selected query behavior through the actual Rust runtime, including durable writes during import and preservation of existing catalog identities/data.

## Process dispatch and preserved evidence

Each measurement is a new process executing `validate_query_candidate.py`. That process installs the explicit candidate map into the imported frozen helper's in-memory `QUERY_SQL` before running any workload. It does not modify either frozen Python file. The recovery helper's in-memory `__file__` is temporarily redirected to this runner so its crash workers also use this dispatch. A child publishes the exact identity, request, settings readback, and parameter schedule, which the coordinator validates independently against its own expectations. A small fixture test records the SQLite statement trace to verify that child processes really execute the candidate `CROSS JOIN` statements; tracing is disabled in the measurement campaign.

The run output must not exist and cannot overlap either evidence source. Individual child output files are opened exclusively before launching a process, so an existing receipt prevents the associated query or mutation. Each child receipt retains errors, process exit status, and stderr; JSON errors retain stdout. Per-scale records and `experiment.json` are checkpointed, and failures remain in place. `complete=true` means all scales were attempted; only `all_pass=true` means all checks passed. An interrupted run leaves its directory and receipts for inspection; use a new exclusive output directory rather than overwriting or resuming it. The command exits nonzero on failed checks.

Copy verification, full database hashing, and plan collection are preparation, outside measured query intervals. They warm OS caches, which is consistent with the explicitly labeled fresh-process protocol. The output includes host observations before preparation, after each copy before measurement, and after execution; the coordinator must separately record the quiet-host gate and measurement start, as in the original campaign. The runner never accesses user originals or the source RAID.

## Execution after lane release

First review this commit and run the small contract tests from the worktree containing this runner and the reviewed candidate module:

```sh
/Users/michael/PhotoCatalog-private-results/sc-22837-env/bin/python -m unittest discover -s benchmarks -p 'test_validate_query_candidate.py' -v
/Users/michael/PhotoCatalog-private-results/sc-22837-env/bin/python -m unittest discover -s benchmarks -p 'test_*.py' -v
```

These tests cover actual child SQL dispatch, recovery dispatch, matching baseline hashes and parameter schedules, rejection of errors/incomplete samples/incorrect identities and oversized RSS, and preservation of existing output. They are small synthetic correctness tests, not scale or performance evidence. No tests were run while writing this protocol because the original supplemental campaign held the timing lane.

Only if the runtime-work evidence requires correction, small tests and review pass, and the coordinator reserves the lane, run once into a fresh private directory:

```sh
/Users/michael/PhotoCatalog-private-results/sc-22837-env/bin/python benchmarks/validate_query_candidate.py run \
  --snapshot /Users/michael/PhotoCatalog-private-results/sc-22837-production-pristine \
  --baseline /Users/michael/PhotoCatalog-private-results/sc-22837-final-v2 \
  --output /Users/michael/PhotoCatalog-private-results/sc-22837-sqlite-page-correction-v1
```

Keep the entire output, including failed/partial child receipts. Inspect all scale checks, actual SQL/parameters, native version, settings, source/copy proofs, original aggregate references, and coordinator timing conditions before interpreting performance. Do not replace or reinterpret the existing frozen matrix with this corrective experiment.
