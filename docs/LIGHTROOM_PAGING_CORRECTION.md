# Inspection paging correction and generation adoption

This document records the historical schema-2 correction. Current schema 3
retains these indexes but rejects older derived plans without upgrading them; see
[the numeric relationship correction](LIGHTROOM_NUMERIC_RELATIONSHIPS.md). The
version-1-to-2 upgrade evidence below remains historical.

The actual paused private inspection plan showed a revision-prefix `source_ids`
scan and a temporary ordering sort for both whole-revision and nullable-table
row pages. The exact bundled SQLite 3.51.1 diagnostic reproduced increasing VM
work at 100, 1,000 and 10,000 synthetic rows per revision, interleaved with another
revision. Four revision/sequence indexes removed the ordering sorts and bounded
ordinary page work without changing the JOIN. The explicit-table branch now uses
`source_rows(revision,table_name,sequence)` directly; a rare or absent table must
not scan every other table's rows to fill a page.

Inspection schema 2 adds those four indexes, `facts_revision` (whose implicit
rowid supplies the conflict cursor order), and two partial ordered path queues.
The queues separately cover pending metadata checks and pending-or-metadata-only
packet checks. With both indexes available, SQLite chose the broader packet queue
for a pending-only query in the marker-heavy fixture; explicit `INDEXED BY` binds
each queue to its required index. Pending completion uses `EXISTS`, not a full
count. Changing path state/evidence removes or adds queue entries in the same
existing per-path transaction. Metadata-only inspection followed by packet
inspection retains the previous ordering and eligibility semantics.

No retained row, source ID, sequence, source key, typed value, capture reference,
family choice, evidence revision, or workflow cursor is rewritten by the schema
upgrade. The application catalog schema is unchanged. Current schema admission
checks the required index definitions; missing/replaced queue indexes fail rather
than silently returning to scans. Version-1 upgrades acquire an immediate write
transaction, re-read the version, create all indexes and advance the version
atomically. A failed index creation rolls back prior index additions and leaves
version 1/data intact. Current version-2 opens do not perform schema writes.

Conflict results can be sparse: finding two conflicting facts near the end still
requires inspecting nonconflicting candidate facts and their related values.
The revision/rowid index removes repeated prefix scanning and the ORDER BY sort;
it does not make rare-result discovery constant work. Path-collision joins also
retain candidate-probing work; their existing locator index and new ordered outer
index are sufficient for the observed ordering defect. No collision join rewrite
or materialized conflict model is introduced.

Native regressions execute the production SQL, independently verify cursors and
all typed row fields, and measure VM steps/sorts for deep, empty, interleaved,
rare and missing-table cases. Queue tests process repeated batches after large
completed prefixes with many metadata-only markers, then drain the packet queue.
The existing end-to-end path/packet tests remain required. The diagnostics and
regressions are statement-work evidence, not new latency qualification or proof
that the remaining real-catalog inspection is complete.

## Frozen evidence and proposed adoption protocol

This section is a reviewable execution plan, not evidence of execution. The
coordinator must approve exact new source/binary/config hashes and resource
admission before any copy or upgrade. The existing v2 run remains paused and
immutable; its journal, failed earlier attempt, frozen executable and logs are
not rewritten. The protocol-2 runner and one-revision seed handler are not silently
repurposed to adopt this multi-revision plan.

1. Freeze/test the corrective core. Copy its tested `lightroom_inspect` executable
   to a new exclusive private binding directory and record SHA256, length,
   toolchain, source commit, build mode and native SQLite version. A new runner
   binding must explicitly distinguish old adopted commands from new executions.
2. Under the existing run's exclusive lock, verify its owned pause, no children,
   final journal sequence, active revision/cursor, completed outcomes, capture
   references and saved page/progress receipt hashes. Inventory the private plan
   directory with no-follow handles. Reject active writers or nonregular files.
   Copy the quiescent main and **every present companion**, preserving their
   identities/digests before and after. Do not open the old plan with a mutating
   connection. Immutable source reading is allowed only with demonstrably absent
   or empty WAL/journal; otherwise use the reviewed capture/recovery procedure on
   the private plan and retain its raw companions separately.
3. Proposed exclusive generation is
   `PRIVATE_RESULTS_ROOT/sc-22844-current-families-v3`.
   Preserve a raw-copy directory and use a separate `plans/main/inspection.sqlite3`
   working copy. Never reuse an existing destination. Record copied artifact
   hashes, source identity/companion state, disk reserve and the exact predecessor
   `PRIVATE_RESULTS_ROOT/sc-22844-current-families-v2`.
   No original catalog/photo access is required for this adoption.
4. Establish a complete logical-state digest on the copied, recovered plan before
   upgrade. Stream the fixed, known inspection tables in rowid order, retaining
   table/column names, SQLite type, rowid and exact value bytes in the digest. The helper uses bounded whole SQLite rows, not incremental cell I/O: set an
   explicit 128 MiB row cap (including hex expansion and packet raw+decoded data),
   an 8 MiB accumulated summary cap and a 512 MiB process-memory admission. A row
   exceeding the cap fails explicitly. Never evaluate retained source SQL/Lua.
   Record per-table counts and digests, all capture/source IDs, family choices,
   progress and maximum sequence. This is a single ordered state scan, not a
   replay of the old expensive page query. The copied state must agree with the
   predecessor's independently read ordered state; preserve those receipts.
5. Open **only the working copy** with the new executable, e.g. the explicit
   command `lightroom_inspect rows NEW_PLAN EXISTING_REVISION --after 0 --limit 1`.
   Record its stdout/stderr, exit status and upgrade elapsed time. Independently
   verify application ID, schema 2 and all required index definitions. Repeat the
   ordered logical-state scan and require exact before/after equality. A failed
   upgrade is retained as a failed adoption, never automatically retried. Keep
   the raw copy available to inspect the pre-upgrade bytes.
6. Publish a separate adoption manifest only after all checks pass. It binds the
   two completed members and active member, predecessor journal/progress and raw
   capture paths; each adopted command references the exact original command
   record/stdout hash. Adoption is not a new child execution. Previously saved
   page outputs remain evidence; reconstruct the running page digest/counts from
   those bounded saved outputs once if needed, rather than rerunning their SQL.
   The current reviewed checkpoint is command-next 1458, active row-page-next
   526, last cursor 977626; these are expectations to verify, not assumptions to
   overwrite live facts. The active member has 115 tables/883795 retained rows.
   Do not call its verification complete while the readback is partial.
7. A separately reviewed new-generation runner must resume exactly after that
   adopted prefix, assign new command IDs under its own journal, and preserve
   the boundary between adopted and newly executed records. Keep page limit
   1000, actual 8 MiB+LF output cap, resume budget 10000, command deadlines and
   cooperative pause behavior. A fresh whole-inventory admission is required
   before subsequent original-catalog capture/inspection. No full companions,
   original-photo paths, packet-file reads, family choices or migration are
   authorized merely by this plan upgrade. The remaining 45 uncaptured members
   plus active verification remain required under the existing scope.

The concrete adoption helper/config, exact hashes and copy/scan commands still
require coordinator review after the final native gate. Neither this document
nor passing synthetic tests authorizes editing the frozen v2 run or claims that
its remaining work has been executed.
