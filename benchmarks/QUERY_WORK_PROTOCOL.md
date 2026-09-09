# Runtime query-work diagnostic (protocol 3)

This supplemental diagnostic addresses sc-22837's requirement that ordinary deep navigation avoid unbounded work. The default baseline retains the frozen SQL. An explicit SQLite candidate variant supports a separately reviewed before/after experiment; neither variant selects a backend, replaces latency receipts, or adds a latency eligibility threshold. Compare work growth across the three catalog sizes and all three cursor cases; a fast sampled page alone does not establish bounded work.

`query_work.py --variant baseline` (also the default) imports the exact frozen `QUERY_SQL["page_deep"]` and `QUERY_SQL["rating"]`, verifies the frozen harness SHA-256, and executes each at 50% and 90% sequence cursors. The original 50% and 90% cases remain unchanged, including the frozen iteration-0 rating predicate. Protocol 2 additionally replays `bench.query_parameters(name, count, 9)` for each query. This preserves the exact frozen cursor and its actual iteration-9 rating predicate; the implementation does not approximate a nominal 90.5% cursor or reuse the iteration-0 rating. Each case records a distinct `case_label`, `iteration` (null for the original explicit cursors), actual parameters and descriptive cursor percentage. Original profile filenames remain unchanged; new profiles use `page_deep-iteration-9.profile.json` and `rating-iteration-9.profile.json`. Each receipt records SQL, parameters, engine version, effective settings, explain plan, all 200 returned records, and work metrics.

The extension follows receipt-only analysis of the completed original campaign: SQLite deep-page latency spikes occur at `iteration % 10 == 9`, whose exact frozen cursor is 90.5% at the acceptance scales. Profiling only 50% and 90% could omit that measured case. This is a diagnostic coverage correction, not a conclusion about its execution plan or runtime work. The generator independently verifies the first 200 qualifying records and all six selected values, so a correctly filtered but skipped page fails.

## Explicit variants and query identity

Protocol 3 preserves all six protocol-2 cases and their parameters/oracles. The
CLI and `run(..., variant="baseline")` default to `baseline` for both engines.
`--variant sqlite_page_candidate` is accepted only with `--engine sqlite`; the
API also rejects candidate DuckDB runs before reading snapshots or opening a
database. It copies `query_candidates.CANDIDATE_SQL` for `page_deep` and `rating`.
The candidate module, frozen `bench.QUERY_SQL`, workload generator, and campaign
harness are not modified or patched by variant selection.

Every receipt records `variant`, the exact two-query `sql_map`, and
`sql_map_sha256`: SHA-256 of UTF-8 JSON with sorted keys, compact separators and
`ensure_ascii=False`. SQL string whitespace remains part of that identity. Each
of the six query records still contains its exact executed SQL and unchanged
parameters. `candidate_source_sha256` identifies the candidate module's source
file when selected, and is explicitly null for baseline. Existing diagnostic
script, frozen-harness, engine/native-source, snapshot-manifest and before/after
database preservation evidence remain required. Each variant needs a separate,
new exclusive output directory; original baseline receipts are never overwritten.

Collect baseline evidence first. Candidate execution requires passing semantic
checks and review of the conditional experiment after the timing lane is
released. Candidate results use the same settings, cases and full six-value
first-200-record oracle, and remain separate from both baseline and latency
receipts. No SQL shape or successful diagnostic completion proves a speedup or
bounded catalog-size behavior.

SQLite uses fully typed ctypes calls exported through Python's `_sqlite3` extension, on a separately owned native connection. It records the native library version/source ID and requires that version to match Python's SQLite. It does not inspect CPython object memory. `NVISIT`/`NLOOP` and scan explanations are included when the linked SQLite build supports `SQLITE_ENABLE_STMT_SCANSTATUS`. Otherwise the receipt explicitly records the capability gap and includes `VM_STEP`, `SORT`, and `FULLSCAN_STEP`. VM steps are virtual-machine operations, not directly comparable to DuckDB rows scanned. A zero `FULLSCAN_STEP` alone does not show bounded range-scan work. Missing, negative, zero VM work, and incomplete available scan metrics fail collection.

DuckDB emits JSON profiling for only the target query. The complete operator tree includes `operator_rows_scanned`, cardinality, names/types and extra information, plus cumulative rows scanned and returned count. The validator rejects missing operator metrics, a mismatched SQL identity, inconsistent tree totals, or an incomplete tree. Operator scan counts describe engine work; they are not a statement about physical disk reads. Profiling overhead and prior hash reads make incidental profile timing unsuitable for latency acceptance.

Sources: [DuckDB metrics](https://duckdb.org/docs/current/dev/metrics), [DuckDB profiling settings](https://duckdb.org/docs/current/configuration/pragmas#profiling), [SQLite statement counters](https://sqlite.org/c3ref/c_stmtstatus_counter.html), and [SQLite scan counters](https://sqlite.org/c3ref/c_scanstat_est.html).

## Preservation and output

The reference run uses standalone copies at `/Users/michael/PhotoCatalog-private-results/sc-22837-query-work-pristine-v3`. Read-only connections used by the earlier copy helpers left empty SQLite WAL and 32 KiB SHM files at the original pristine location. The diagnostic correctly rejected those sources; its failed receipt is retained. No companions were removed. With no open database handles and no WAL/journal content, APFS clones of each manifest-verified main file were created, their full hashes checked, and original main/companion metadata checked unchanged. `derivation.json` records this operation and the snapshot manifest is byte-identical to the original. These copies isolate immutable query-work access from the original copy helpers.

Only the previously verified production-pristine snapshot is accepted. The snapshot manifest must be complete and match the frozen harness. Each selected database is checked against its recorded size and SHA-256 before opening. Connections are read-only. SQLite additionally uses `immutable=1` and rejects all WAL, SHM, journal, and temporary sidecars before opening; this requires an isolated, checkpointed source with no concurrent writer. It sets only connection-local pragmas and never sets journal mode. DuckDB's profile and spill output stay in the newly created exclusive output directory. Neither engine checkpoints, changes schema, nor writes source records.

The full database hash, size, modification time and inode are compared again after closing, including error paths. These are full sequential hash reads and must also wait for the timing-lane release. Source sidecar appearance fails preservation. Output paths cannot overlap the snapshot tree. Existing output directories are refused. Exceptions leave `receipt.json` with `complete: false`, the error, available query evidence, and preservation evidence where obtainable. A failed run must use a new output path when rerun; failed evidence is retained.

## Verification and execution protocol

Protocol 1's six diagnostic tests and all 21 benchmark contract tests passed in the reference Python environment. Protocol 2's 23 combined tests then passed hosted CI at `8bb8889dd3694b486259974afdd19a589f688c04`. Protocol 3 and the conditional candidate runner passed all 38 hosted evidence tests in [CI 34356352572](https://github.com/michaeltrefry/PhotoCatalog/actions/runs/34356352572) at `5a1f13dcb7d0eee17dd7156108a8229b26d20b24`, including actual child SQL execution and recovery dispatch. The latest reference-Mac run passes all 39 combined tests, including the full DuckDB runner setup regression. The earlier local runs verified native SQLite access, real DuckDB JSON metrics, settings readback, source preservation, and rejected evidence. A real DuckDB setup failure was repaired by enabling JSON profiling before assigning a `.json` output path. `operator_type` is emitted automatically and was checked on every operator by the strict validator. The reference scale diagnostics have now completed on verified standalone clones. SQLite candidate work is constant across the tested scales/cursors; DuckDB baseline scan work grows. See the backend results and private receipts for outcomes. The following protocol records the controlled execution procedure:

1. Run the small diagnostic tests first in the existing Python environment:

   ```sh
   /Users/michael/PhotoCatalog-private-results/sc-22837-env/bin/python -m unittest discover -s benchmarks -p test_query_work.py -v
   ```

2. Review their actual results, including native API loading, emitted DuckDB JSON schema, strict settings readback, source preservation and rejected metrics. Repair any verified failure and independently review the final diagnostic before scale execution.
3. Run one process at a time for each engine at 1M, 5M and 10M, using that engine's single candidate memory setting from the production-profile campaign. If no profile qualifies, use the original 256 MiB default consistently across all three scales to diagnose its failures; record that this is a failed configuration, not a production candidate. The command requires memory explicitly and does not infer or choose a candidate. For example, **for a selected 256 MiB profile or the explicitly labeled failed-default diagnostic**:

   ```sh
   /Users/michael/PhotoCatalog-private-results/sc-22837-env/bin/python benchmarks/query_work.py \
     --snapshot /Users/michael/PhotoCatalog-private-results/sc-22837-query-work-pristine-v3 \
     --engine sqlite --variant baseline --count 1000000 --memory-mb 256 \
     --output /Users/michael/PhotoCatalog-private-results/sc-22837-query-work/baseline-sqlite-1000000-256mib
   ```

   Repeat sequentially with `--engine duckdb`, the actual candidate memory, and the other two counts. Each command performs six profiled 200-row queries plus plan reads and connection settings; it also reads the selected database twice for preservation hashes. The six commands together read roughly twice the total snapshot database size for hashes. There are no renderer, image, GPU, native build, or source mutation operations.
4. Read each completed receipt and assess operator/VM work growth against count and cursor depth, explicitly including `frozen_iteration_9`. Preserve the observations separately from the latency campaign and record any unbounded scan finding in the backend decision. Do not treat `complete: true` as AC passage: it means diagnostic evidence was collected and validated, not that query work was bounded.

5. Only after baseline collection, passing candidate semantic tests, and review of
   the conditional experiment, repeat the SQLite commands with
   `--variant sqlite_page_candidate` and new candidate-specific output directories.
   Keep all six cases at each scale and compare runtime-work counters/plan evidence
   with that engine's baseline using the same settings and preserved source.
   Do not substitute candidate receipts for the completed frozen campaign or infer
   latency eligibility from diagnostic timings.
