# Catalog backend comparison (sc-22837)

This harness compares **synthetic metadata**, never RAW decoding, preview generation, UI frame times, or a private library. The runtime is Rust. Python drives the native SQLite and DuckDB bindings for a reproducible comparison; `catalog_probe` separately measures the actual Rust catalog API and bundled SQLite engine.

## Reproduce

Use Python 3.14 with `pip install -r benchmarks/requirements.txt`, then:

```sh
python -m unittest discover -s benchmarks -v
cargo build --locked --release --bin catalog_probe --jobs 4
python benchmarks/catalog_benchmark.py campaign --root /absolute/empty/output --counts 1000000,5000000,10000000 --repetitions 100 --fresh-repetitions 20 --native-probe target/release/catalog_probe
```

The default campaign refuses a nonempty destination. Output can be large: provision at least 100 GiB for CSVs, both engines, indexes, WAL, and temporary files. Catalogs belong on the internal SSD; no source-image access is involved.

Preparation can precede a quiet measurement window. Add `--prepare-only` to generate, load, index, validate, and checkpoint all datasets. Later invoke the same command with `--resume-prepared` instead. The script digest, scales, and repetition contract must match; a completed campaign cannot be reused. The preparation and measurement host snapshots remain distinct. Standalone `generate`, `load`, `read`, `plans`, `mixed`, and `recovery` commands are also available for diagnosis.

Do not run the final timing campaign alongside builds, RAW processing, or other substantial work. Record competing load separately. The output manifest captures OS/CPU/RAM, available memory, load average, Python/native engine versions, script digest, generator seed, repetition count, and cache interpretation. It does not claim to detect all competing processes.

## Fixed version-1 workload

Seed **22837**, algorithm `mix`, schema and SQL corpus are versioned in `catalog_benchmark.py`. A streaming CSV generator supplies the same logical records to both engines. Each input has a SHA-256 manifest, and loader receipts reconcile counts/sums plus exact first/middle/last records against the independent generator. Public tests additionally compare all page query results against a Python filter/sort oracle.

There are 1M, 5M, and 10M assets, two keyword relationships per asset, and one collection membership per five assets. Asset UUIDs, locations, content/preview digests, metadata, indexed fields, and editing relationships are distinct. Indexed file/folder identity remains separate from generated asset identity.

Skew is deliberately broader than one photographer's existing organization: 80% of assets fall in 100 hot folders, 70% are unrated, and 60% use one camera. Remaining assets span 10,000 folders and 19 additional cameras. One keyword comes from a 32-keyword hot set and another from a 4,096-keyword tail. Capture times are clustered in bursts, spanning and wrapping a 6,200-day horizon. Metadata varies by dimensions, camera, date, lens, ISO, aperture, orientation, description, and source identity. Format proportions are 85% CR2, 5% DNG, 7% JPEG, and 3% TIFF. These are explicit synthetic assumptions, not a library inventory or capacity forecast.

The logical schema is identical. SQLite's `INTEGER PRIMARY KEY` provides its native rowid seek; DuckDB uses a `BIGINT PRIMARY KEY`. Both receive uniqueness constraints, relationship foreign keys, and the same folder/date/rating/combined indexes. Neither engine's native strengths are disabled. Indexed schema fields anticipate the approved organization workload; they are benchmark fields, not an early production organization API.

The seven page cases are deep sequence, folder, capture date, rating, keyword join, folder/rating/date combination, and collection join. Deep cursors span 50–90.5% of the catalog; collections use 50–77% so the relationship page remains full at 1M. The combined case uses the common unrated state and a bounded date window. Every page must return 200 records at the acceptance scales, with valid filters, strictly ordered distinct identities, and independently checked keyword/collection membership. Sparse or empty results fail rather than masquerading as 200-row measurements. The native core probe additionally includes metadata JSON deserialization and exact contiguous sequence/UUID assertions.

Large aggregates group all assets by camera and rating; their latency is reported separately from page budgets. Warm query output has three untimed warmups and 100 timed samples per case. Fresh-process output uses 20 separately started interpreters per page case, with open-plus-query and query-only distributions. Raw samples and result digests accompany p50/p95/p99. Small smoke runs intentionally have fewer samples and establish correctness only.

## Durability, concurrency, and memory

SQLite uses WAL, `synchronous=FULL`, **`fullfsync=ON`**, foreign keys, disabled mmap, file-backed temporary storage, 5-second busy timeout, and a 1,000-page automatic checkpoint. DuckDB retains durable WAL commits and a 16 MiB checkpoint threshold. DuckDB 1.5.5's macOS `FileSync` explicitly invokes `F_FULLFSYNC`; SQLite's matching flag is therefore required for a fair comparison and is also enabled in the Rust runtime. Sources: [DuckDB pinned implementation](https://raw.githubusercontent.com/duckdb/duckdb/v1.5.5/src/common/local_file_system.cpp), [SQLite fullfsync](https://www.sqlite.org/pragma.html#pragma_fullfsync), [DuckDB transaction durability](https://duckdb.org/2024/10/30/analytics-optimized-concurrent-transactions).

Background import and foreground writes use separate connections in one process. A background transaction inserts 32 complete assets and their relationships using parameterized bulk statements in both engines. Data generation is outside the timed transaction. An identical 2 ms producer yield gives foreground work a scheduling opportunity. Each foreground rating or edit write starts while import is active, includes transaction commit in its measured acknowledgment, and has a readback. No write conflicts, missing rows, or failed acknowledgments are discarded. DuckDB's single-process concurrency boundary fits the product's single-computer requirement; its optimistic conflict behavior remains relevant. [DuckDB concurrency](https://duckdb.org/docs/current/connect/concurrency).

Initial dataset loading is preparation, not a claim about runtime import throughput. SQLite uses 10,000-row Python `executemany` batches; DuckDB uses native CSV `COPY`. Loading gets 4 GiB engine cache/buffer allowance. Interactive runs use 256 MiB and a constrained 64 MiB configuration, with four DuckDB workers. SQLite cache allowances are per connection; DuckDB's buffer allowance is process-wide and excludes some allocations. Actual sampled RSS is reported in addition to configured limits. [DuckDB memory limits](https://duckdb.org/docs/current/configuration/pragmas#memory-limit).

The native Rust probe uses bundled SQLite (reported independently of Python's version), actual catalog browse/get/reopen, and a second native test with 32 template-derived synthetic rows and their relationships per background transaction. Foreground work is released after the background obtains its write lock; acknowledgment prevents the next background transaction from artificially starving the foreground. It measures durable rating/edit writes with readback, committed counts, and exactly the same durability flags. These native timings are independent evidence, not pooled with Python measurements.

Recovery is tested before and after a real commit: a worker acknowledges an earlier transaction, enters another transaction, signals the observed boundary, and is forcibly killed. A new process must recover precisely the committed value. This verifies process-crash transaction recovery; it is not a physical power-loss experiment.

## Interpretation

`budget_summary.json` applies the approved database budgets: page p95 ≤100 ms warm, open-plus-page p95 ≤500 ms fresh process, durable rating/edit p95 ≤100 ms during import, and browse RSS ≤4 GiB. The raw receipts contain plans, disk growth, measured settings, errors, correctness data, and all distributions. Aggregate scans are reported without inheriting the page target. Engine failures remain explicit and fail eligibility.

**Fresh processes are not cold OS-cache measurements.** The harness never purges the user's machine cache or silently labels a reopened database cold. Cold filesystem cache remains separately unmeasured; the approved numeric target is explicitly fresh-process. No image, preview, or UI requirement is concluded by this database story.

The database decision must be written only after the full fixed campaign and independent review. Preserve raw receipts externally and commit the compact reviewed decision/evidence summary. No completed decision is asserted by a smoke test.
