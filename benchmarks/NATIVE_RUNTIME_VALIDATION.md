# Native runtime settings and write validation, protocol 1

This validation applies the measured SQLite settings to the actual `Catalog` connections and the existing `catalog_probe` native workload. It does not change schema, asset identity, query selection, or import scheduling. The shared `configure_catalog_connection(&Connection)` helper configures app-owned, already validated writable connections; it exposes no raw catalog connection or arbitrary-write API. `Catalog::open` checks application/schema compatibility before invoking it. The read-only `query_work_probe` remains separate and does not call this writable configuration helper.

The exact configuration is WAL; synchronous FULL (2); fullfsync 1; foreign keys 1; cache_size -262144 (256 MiB target per connection); mmap_size 0; temp_store FILE (1); busy_timeout 5000 ms; wal_autocheckpoint 1000 pages. FILE is the frozen benchmark setting. MEMORY would be a different experiment. Cache size is a per-connection target rather than a hard process-memory ceiling. The native probe reads back the foreground and background connections' actual settings into its receipt instead of reporting a hardcoded configuration claim.

Acceptance for integration includes the existing rejection tests for unrelated/future schemas, a complete actual `Catalog` settings readback, and an independently constructed nonempty v1 catalog reopened twice without changing IDs, sequences, schema/application versions, raw location bytes, exact metadata JSON including unknown fields, preview references/bytes, pending state/error, or the AUTOINCREMENT sequence. The probe connection has its own complete settings readback test. These source changes were prepared while another controlled timing campaign held the host; local tests/builds must wait for the coordinator's release.

## Controlled native run

After code review and local checks pass, preserve the original frozen native executable and receipts. Build the new `catalog_probe` with the pinned Rust toolchain and lockfile, recording executable/source/toolchain/dependency identities. Use separate verified pristine writable copies at 1M, 5M, and 10M; never point this binary at the preserved snapshot, a prior mixed-run database, user originals, or the source RAID. The coordinator should reuse the reviewed checkpointed-copy helper and reconcile each independent target proof before execution. Hosted cross-platform CI must pass on the final integration before story closeout; it may run independently of local controlled measurement.

Run one dedicated browse-only process and one combined native process at each scale, both with **`--repetitions 200`**, using separate pristine writable copies:

```sh
CARGO_BUILD_JOBS=4 cargo test --locked --lib
CARGO_BUILD_JOBS=4 cargo test --locked --bin catalog_probe
CARGO_BUILD_JOBS=4 cargo test --locked --test native_probe
CARGO_BUILD_JOBS=4 cargo build --release --locked --bin catalog_probe

target/release/catalog_probe \
  --catalog /Users/michael/PhotoCatalog-private-results/sc-22837-native-runtime-v1/1000000/browse/sqlite \
  --count 1000000 --repetitions 200 --browse-only

target/release/catalog_probe \
  --catalog /Users/michael/PhotoCatalog-private-results/sc-22837-native-runtime-v1/1000000/mixed/sqlite \
  --count 1000000 --repetitions 200
```

The example catalog directory must contain the newly prepared writable copy. Repeat with matching 5M/10M copies. Capture stdout, stderr, process status, source/copy provenance, actual settings, process RSS observations, and quiet-host/timing conditions in exclusive private receipts. Retain every failed or partial run. Do not retry to obtain favorable percentiles.

Require `native_runtime_protocol=1`, exactly 200 browse samples in each process, and in the combined process 100 rating samples, 100 edit samples, and 200 background transaction samples with their complete raw arrays and independently recomputed p50/p95/p99/max. All settings readbacks must equal the frozen configuration. The dedicated process must report `browse_only=true`, explicit browsing scope, and `native_mixed=null`; the combined process must report `browse_only=false`. A CLI regression uses only the real v1 assets schema without the synthetic write tables to prove browse-only never enters `native_mixed`. The existing probe checks identities, metadata parsing, restart access, seek-based query plans, durable rating/edit readbacks, and exactly 6,400 appended rows. Native browse p95 must be at most 100 ms and each durable rating/edit p95 at most 100 ms. Any process error, incomplete sample set, failed readback, configuration mismatch, or latency breach prevents qualification. This command does not measure 20 fresh-process page samples; those remain separately reported Python evidence.

An external monitor must sample each dedicated process. Apply the approved 4 GiB browse budget to the `--browse-only` process peak; this process retains the actual Catalog browse/get/reopen/metadata work and omits mixed writes. Label the combined process peak as **whole-process diagnostic RSS** without a mixed size admission limit. Preserve both measurements and their explicit modes; do not substitute the combined peak for the browse-only evidence.

## Limits of cross-language comparison

The frozen native command used 100 repetitions, producing only **50 rating and 50 edit samples**. Its old 10M p95 values were approximately 99 ms, close to the 100 ms limit; they do not establish margin or predict the 100-per-operation result after configuration changes. New evidence at every scale is required.

Keep the native workload unchanged and label these differences from Python:

- Native foreground targets consecutive low sequence IDs; Python chooses seeded scattered IDs. Native rating values and edit recipes also differ.
- Native import clones 32 template rows plus relationships inside an IMMEDIATE transaction. Python generates 32 new logical rows before a deferred transaction and uses bulk inserts.
- Native handshakes one import batch per foreground operation, releasing foreground after the background acquires the write lock and waiting for its acknowledgment. Python uses continuous background import with a 2 ms yield and an active-transaction event.
- Native performs its browse phase before writes; Python mixed also fetches a deep page after each foreground write. The native browse API returns/deserializes production metadata and uses its existing unfiltered seek query, whereas the Python page matrix includes joined organization filters.
- The original native campaign ran after Python mixed writes and recovery on that database. This validation starts from fresh copies; results must identify the different preparation.

The native run establishes performance of this specific Rust integration workload and durable operations under its stated contention. It is not workload equivalence to the Python comparison. A real failure must be diagnosed in place; do not preemptively change scheduling, disable durability, alter thresholds, or introduce speculative optimizations.
