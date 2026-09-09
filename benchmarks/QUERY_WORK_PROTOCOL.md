# Runtime query-work diagnostic (protocol 2)

This supplemental diagnostic addresses sc-22837's requirement that ordinary deep navigation avoid unbounded work. It does not change SQL, select a backend, replace latency receipts, or add a latency eligibility threshold. Compare work growth across the three catalog sizes and all three cursor cases; a fast sampled page alone does not establish bounded work.

`query_work.py` imports the exact frozen `QUERY_SQL["page_deep"]` and `QUERY_SQL["rating"]`, verifies the frozen harness SHA-256, and executes each at 50% and 90% sequence cursors. The original 50% and 90% cases remain unchanged, including the frozen iteration-0 rating predicate. Protocol 2 additionally replays `bench.query_parameters(name, count, 9)` for each query. This preserves the exact frozen cursor and its actual iteration-9 rating predicate; the implementation does not approximate a nominal 90.5% cursor or reuse the iteration-0 rating. Each case records a distinct `case_label`, `iteration` (null for the original explicit cursors), actual parameters and descriptive cursor percentage. Original profile filenames remain unchanged; new profiles use `page_deep-iteration-9.profile.json` and `rating-iteration-9.profile.json`. Each receipt records SQL, parameters, engine version, effective settings, explain plan, all 200 returned records, and work metrics.

The extension follows receipt-only analysis of the completed original campaign: SQLite deep-page latency spikes occur at `iteration % 10 == 9`, whose exact frozen cursor is 90.5% at the acceptance scales. Profiling only 50% and 90% could omit that measured case. This is a diagnostic coverage correction, not a conclusion about its execution plan or runtime work. The generator independently verifies the first 200 qualifying records and all six selected values, so a correctly filtered but skipped page fails.

SQLite uses fully typed ctypes calls exported through Python's `_sqlite3` extension, on a separately owned native connection. It records the native library version/source ID and requires that version to match Python's SQLite. It does not inspect CPython object memory. `NVISIT`/`NLOOP` and scan explanations are included when the linked SQLite build supports `SQLITE_ENABLE_STMT_SCANSTATUS`. Otherwise the receipt explicitly records the capability gap and includes `VM_STEP`, `SORT`, and `FULLSCAN_STEP`. VM steps are virtual-machine operations, not directly comparable to DuckDB rows scanned. A zero `FULLSCAN_STEP` alone does not show bounded range-scan work. Missing, negative, zero VM work, and incomplete available scan metrics fail collection.

DuckDB emits JSON profiling for only the target query. The complete operator tree includes `operator_rows_scanned`, cardinality, names/types and extra information, plus cumulative rows scanned and returned count. The validator rejects missing operator metrics, a mismatched SQL identity, inconsistent tree totals, or an incomplete tree. Operator scan counts describe engine work; they are not a statement about physical disk reads. Profiling overhead and prior hash reads make incidental profile timing unsuitable for latency acceptance.

Sources: [DuckDB metrics](https://duckdb.org/docs/current/dev/metrics), [DuckDB profiling settings](https://duckdb.org/docs/current/configuration/pragmas#profiling), [SQLite statement counters](https://sqlite.org/c3ref/c_stmtstatus_counter.html), and [SQLite scan counters](https://sqlite.org/c3ref/c_scanstat_est.html).

## Preservation and output

Only the previously verified production-pristine snapshot is accepted. The snapshot manifest must be complete and match the frozen harness. Each selected database is checked against its recorded size and SHA-256 before opening. Connections are read-only. SQLite additionally uses `immutable=1` and rejects all WAL, SHM, journal, and temporary sidecars before opening; this requires an isolated, checkpointed source with no concurrent writer. It sets only connection-local pragmas and never sets journal mode. DuckDB's profile and spill output stay in the newly created exclusive output directory. Neither engine checkpoints, changes schema, nor writes source records.

The full database hash, size, modification time and inode are compared again after closing, including error paths. These are full sequential hash reads and must also wait for the timing-lane release. Source sidecar appearance fails preservation. Output paths cannot overlap the snapshot tree. Existing output directories are refused. Exceptions leave `receipt.json` with `complete: false`, the error, available query evidence, and preservation evidence where obtainable. A failed run must use a new output path when rerun; failed evidence is retained.

## Deferred verification and run plan

Protocol 1's six diagnostic tests and all 21 benchmark contract tests passed in the existing Python environment after the coordinator released the small-test lane. Protocol 2 adds two case-generation tests. All 23 tests passed in hosted Linux CI [34353915562](https://github.com/michaeltrefry/PhotoCatalog/actions/runs/34353915562) at `8bb8889dd3694b486259974afdd19a589f688c04`; the extension's reference-Mac run remains pending while supplemental timing owns the lane. The protocol-1 local runs verified native SQLite access, real DuckDB JSON metrics, settings readback, source preservation, and rejected evidence. A real DuckDB setup failure was repaired by enabling JSON profiling before assigning a `.json` output path. `operator_type` is emitted automatically and was checked on every operator by the strict validator. No scale query-work result is claimed. After the coordinator authorizes scale execution:

1. Run the eight small diagnostic tests first in the existing Python environment:

   ```sh
   /Users/michael/PhotoCatalog-private-results/sc-22837-env/bin/python -m unittest discover -s benchmarks -p test_query_work.py -v
   ```

2. Review their actual results, including native API loading, emitted DuckDB JSON schema, strict settings readback, source preservation and rejected metrics. Repair any verified failure and independently review the final diagnostic before scale execution.
3. Run one process at a time for each engine at 1M, 5M and 10M, using that engine's single candidate memory setting from the production-profile campaign. The command requires memory explicitly and does not infer or choose a candidate. For example, **only if the candidate is 256 MiB**:

   ```sh
   /Users/michael/PhotoCatalog-private-results/sc-22837-env/bin/python benchmarks/query_work.py \
     --snapshot /Users/michael/PhotoCatalog-private-results/sc-22837-production-pristine \
     --engine sqlite --count 1000000 --memory-mb 256 \
     --output /Users/michael/PhotoCatalog-private-results/sc-22837-query-work/sqlite-1000000-256mib
   ```

   Repeat sequentially with `--engine duckdb`, the actual candidate memory, and the other two counts. Each command performs six profiled 200-row queries plus plan reads and connection settings; it also reads the selected database twice for preservation hashes. The six commands together read roughly twice the total snapshot database size for hashes. There are no renderer, image, GPU, native build, or source mutation operations.
4. Read each completed receipt and assess operator/VM work growth against count and cursor depth, explicitly including `frozen_iteration_9`. Preserve the observations separately from the latency campaign and record any unbounded scan finding in the backend decision. Do not treat `complete: true` as AC passage: it means diagnostic evidence was collected and validated, not that query work was bounded.
