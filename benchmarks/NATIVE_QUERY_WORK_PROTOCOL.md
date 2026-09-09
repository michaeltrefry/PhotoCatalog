# Bundled Rust SQLite query-work diagnostic, version 1

`query_work_probe` is a separate measurement-support binary. It does not use or change `Catalog`, the existing `catalog_probe` binary, schema, frozen harness, or Python SQL registry. It is not a backend selection, latency admission test, or production cache/write validation.

The binary consumes an explicit small case manifest from query-work protocol 3. This avoids porting Python's floating-point cursor calculation or seeded rating selection. It accepts exactly six ordered cases: `page_deep` and `rating` at 50%, then both at 90%, then both at frozen iteration 9. Case labels, iteration identities, paired cursors, parameter counts/ranges, scale, and percentage metadata are checked. Legacy rating values must agree. The exact iteration-9 cursor and rating remain manifest inputs and must be independently reconciled with `query_work.query_cases`; range checks alone are not proof of those exact values.

SQL is compiled into the binary as four whitelisted strings: the original and reviewed candidate `page_deep`/`rating` queries. No manifest field can supply SQL. The receipt reports the actual string passed to `prepare`, exact parameters, all six selected values for all 200 rows, query plan, bundled SQLite version and source ID, settings readback, manifest/probe-source BLAKE3 digests, and raw `VM_STEP`, `SORT`, and `FULLSCAN_STEP` counters from rusqlite 0.38's `Statement::get_status`. Each measured statement is newly prepared, executed once, and fully consumed before counters are read. Plan execution and validation queries use different statements and are excluded from those counters. `FULLSCAN_STEP` alone does not expose all range work; interpret it with `VM_STEP` and the plan. Missing/nonpositive VM counts, negative other counters, missing plans, incomplete pages, wrong identity/order, and cross-variant row differences fail the diagnostic.

Each case runs baseline then candidate on the same read-only connection. This is work-counter evidence, not a latency comparison: no timings or fresh/cold claims are produced. There are 12 measured queries, covering both variants for all six cases. The coordinator must independently compare every SQL string with the Python baseline/candidate registry, each case with Python's protocol function, and all six fields of every returned record with its generator oracle. Equality between two Rust variants alone cannot prove that both match the intended Python workload.

## Read-only boundary

Use only the preserved private synthetic snapshots, never user originals or a Lightroom catalog. The binary requires the benchmark application/schema IDs, exact asset count and maximum sequence, empty edits, and initial recovery state. These are compatibility checks, not an authentication mechanism for the source. The coordinator must select the expected snapshot from its existing manifest and establish artifact identity.

The connection uses an absolute percent-escaped SQLite URI with `mode=ro&immutable=1`, explicit `SQLITE_OPEN_READ_ONLY`, `is_readonly` verification, and `query_only=1`. It rejects WAL, SHM, and rollback-journal sidecars, because immutable access must not ignore uncheckpointed changes. Unix filenames preserve raw OS bytes and escape URI-reserved characters; Windows uses a Unicode local path, normalizes the canonical drive prefix, and explicitly rejects UNC paths. Copy a network snapshot to local private storage first. Statement read-only status is also checked. The source must remain quiescent throughout execution; immutable mode intentionally assumes no other writer.

Connection-local cache size is explicit (default 256 MiB), with mapping disabled and the same relevant synchronous/fullfsync/foreign-key/temp/busy settings as the diagnostic. Journal mode is read back without changing it; immutable SQLite may report a mode different from the writable WAL benchmark. This probe does not exercise or establish durable-write behavior. Source byte length and modification time must remain unchanged. That metadata check is not a cryptographic integrity proof; later validation should reconcile snapshot identity externally before/after the run. Tiny tests compare source bytes and verify that UPDATE/CREATE fail without creating sidecars.

Output is a single exclusively created receipt file; its parent directory must already exist. Before creating it, the probe resolves the existing database and output parent, rejects aliases of the database and its WAL/SHM/journal companions (including ASCII case variants), and follows existing directory symlinks. Existing outputs are never overwritten and prevent opening a database connection. Normal failures leave `complete=false`, the error, and previously completed query records; the process exits nonzero. A forced kill can leave an empty/incomplete file, which must be retained and treated as a failed run. A successful receipt means evidence collection and local checks passed; it still requires independent Python validation and interpretation.

## Commands after the coordinator releases the lane

No builds, tests, database queries, or full database hashes were run while authoring this support code. First run the ordinary Rust formatting, clippy, and targeted tests under the pinned toolchain, in one reserved Cargo lane:

```sh
cargo fmt --all --check
CARGO_BUILD_JOBS=4 cargo clippy --locked --bin query_work_probe -- -D warnings
CARGO_BUILD_JOBS=4 cargo test --locked --bin query_work_probe
CARGO_BUILD_JOBS=4 cargo build --release --locked --bin query_work_probe
```

The five small tests cover both SQL variants and all six cases with real bundled SQLite counters, six-field output, URI escaping, source-byte preservation and write rejection, malformed/incomplete cases, sidecar rejection, preserved existing output, database/companion output aliases leaving the source directory unchanged, and explicit failed receipts for underfilled pages. They use a 20,000-row synthetic fixture whose manifest tests the manifest boundary; it is not benchmark-generator equivalence or scale evidence.

After small tests and independent review pass, generate manifests using the existing protocol function, into a new private directory. For example, from the repository root (adjust the destination and repeat for 1M/5M/10M):

```sh
/Users/michael/PhotoCatalog-private-results/sc-22837-env/bin/python - <<'PY'
import json, sys
from pathlib import Path
sys.path.insert(0, 'benchmarks')
import query_work
count = 1_000_000
output = Path('/Users/michael/PhotoCatalog-private-results/sc-22837-native-query-work-v1/cases-1000000.json')
with output.open('x') as stream:
    json.dump({'protocol_version': query_work.VERSION, 'count': count,
               'cases': query_work.query_cases(count)}, stream, indent=2)
PY

target/release/query_work_probe \
  --db /Users/michael/PhotoCatalog-private-results/sc-22837-query-work-pristine-v3/1000000/sqlite/catalog.sqlite3 \
  --cases /Users/michael/PhotoCatalog-private-results/sc-22837-native-query-work-v1/cases-1000000.json \
  --memory-mib 256 \
  --output /Users/michael/PhotoCatalog-private-results/sc-22837-native-query-work-v1/native-1000000.json
```

Record the executable digest and exact build/toolchain/dependency identity externally alongside the source digest in each receipt. Repeat with the matching preserved 5M and 10M snapshot/case manifest, retaining every failure. The coordinator performs the independent registry/parameter/generator reconciliation before treating the results as S2 native work evidence. Actual production cache integration and native durable-write validation remain separate, evidence-dependent work.
