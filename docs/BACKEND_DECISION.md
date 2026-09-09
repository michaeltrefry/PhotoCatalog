# Catalog backend decision — sc-22837

Status: **Original comparison complete; supplemental profiles and production integration pending.** SQLite remains the skeleton's provisional implementation. Neither engine's original 256 MiB profile meets all required scales. The declared 256/1024/2048 MiB production-profile campaign is running; no production backend is selected yet.

## Decision contract

Compare SQLite and DuckDB at 1M, 5M, and 10M synthetic assets with the same logical metadata, ratings, keyword and collection relationships, and durable editing workload. The version-2 harness and its methodology are in [benchmarks/README.md](../benchmarks/README.md).

The accepted database budgets are unchanged:

- 200-row metadata pages: p95 ≤100 ms warm and ≤500 ms including database open in a fresh process.
- Durable rating or edit acknowledgment: p95 ≤100 ms while background import is active.
- Browse-only RSS: ≤4 GiB at 10M records, including both warm and fresh-process observations; also run constrained cache/buffer settings.
- Deep navigation: keyset query plans must seek rather than perform work proportional to the skipped prefix for the selected production path.
- Restart, transaction interruption, stable identity, and retained metadata must pass independently of speed.

Aggregates, database size, disk growth, and initial loading cost inform the decision but do not inherit the page-latency target. No preview, RAW-processing, export, or UI performance is claimed by this database comparison. Fresh-process measurements do not establish cold filesystem-cache performance.

## Physical design findings before measurement

A first smoke exposed a DuckDB 1.5.5 constraint limitation: updating an indexed rating on an asset referenced by foreign keys failed because the engine rewrote the update as a delete and insert. This is documented [DuckDB index behavior](https://duckdb.org/docs/current/sql/indexes#over-eager-constraint-checking-in-foreign-keys).

The reviewed comparison addresses that limitation in both engines with a dependent `annotations(asset_id PRIMARY KEY REFERENCES assets, rating)` table. Its rating index has no incoming references. Assets, annotations, keywords, collections, and edits retain foreign-key integrity. The logical photo data and requested rating/edit operations are unchanged. Both engines passed the revised small-workload operation checks; those checks are correctness evidence only.

SQLite uses WAL with `synchronous=FULL` and `fullfsync=ON`. The latter matches the macOS storage flush used by the [pinned DuckDB implementation](https://raw.githubusercontent.com/duckdb/duckdb/v1.5.5/src/common/local_file_system.cpp). The Rust catalog also enables fullfsync. Neither benchmark engine disables durable commits or referential integrity.

Python drives both native engine bindings, with actual versions recorded in receipts. A separate Rust probe measures the bundled SQLite version, actual catalog browse/get/reopen with metadata decoding, and durable native rating/edit transactions during import. Its measurements remain separate from Python's; this prevents binding overhead from being mistaken for native-runtime performance.

## Required final evidence

The original campaign completed on 2026-09-09 with exact frozen harness hash `167b13d524c0f21a18426bb8b21acc6792f275de7884edca1cf995c6ff874c89`. Independent review reconciled 1,026 distributions, all six load proofs, cross-engine result hashes, 48 query plans and interrupted-transaction recovery. SQLite's 256 MiB rating-filter warm p95 exceeds 100 ms at 5M (126.031 ms) and 10M (266.941 ms); its 10M deep-page p95 is 132.594 ms. DuckDB's 256 MiB mixed workload runs out of memory at 5M and 10M, so its partial low write timings are not acceptance evidence. These adverse results remain in the original receipts.

The original Rust 10M probe preserves identity/metadata across restart and records rating/edit p95 of 99.544/99.189 ms with only 50 samples each and less than 1 ms budget headroom. That evidence belongs to the frozen probe, not a later integrated implementation. Plan presence is also separate from bounded work: a read-only diagnostic will collect actual operator/VM work at two cursor depths after the timing lane releases. Its runtime initialization and source-preservation tests have passed; scale results are not available yet.

The final decision must identify the reviewed source revision, script and input digests, exact native engine versions, reference hardware/storage, competing-load record, preparation and measurement boundaries, cache settings, and raw receipt locations. It must include p50/p95/p99 and sample counts for every required workload at every scale, all measured memory peaks, disk sizes/growth, query plans, operation failures, cross-engine result reconciliation, and interrupted-transaction outcomes.

No production selection, compatibility migration, or acceptance conclusion is recorded yet. Once the fixed campaign is complete, this document will state the selected backend, rationale, measured tradeoffs, remaining limitations, and the evidence that existing skeleton identities and data survive the integration.
