# Metadata association join-order correction

The first real-catalog inspection retained every physical source row but spent
several minutes in final metadata ownership reconciliation. A native process
sample identified `associate_catalog_facts → unique_target → sqlite3_step` rather
than packet parsing or capture I/O. The private run was deliberately stopped as
failed partial evidence; its raw capture, committed rows, diagnostics and command
logs remain preserved. It does not count as completed catalog inspection.

The original inner join allowed SQLite to choose every matching-table target as
its outer loop, probing the specific owner reference afterward. Proving that only
one target exists under `LIMIT 2` then scans the target-table range for each owner.
The corrected statement makes the specific owner reference the outer loop with
`CROSS JOIN`, so targets are probed by the complete `(revision,table_name,local_key)`
index. No schema, source IDs, records, predicate, uniqueness rule or returned-row
limit changes. Missing targets and duplicate links/targets still yield no unique
association. SQLite documents this explicit join-order control in its
[optimizer overview](https://www.sqlite.org/optoverview.html#manual_control_of_query_plans_using_cross_join).

The bundled-rusqlite regression executes both statements on the actual inspection
schema at 100, 1,000 and 10,000 synthetic target rows. It independently checks
returned identities, query plans and VM-step growth, then checks missing owners,
duplicate targets and conflicting outgoing references. It uses statement work
counts rather than elapsed-time assertions. The private real-plan diagnostic
also records its Python-linked SQLite version; that diagnostic is not substituted
for bundled-Rust execution.

The original runner/binary/config/receipts must not be edited or silently retried.
A corrective run gets a new tested core/binary binding and a reviewed provenance
chain. The stopped private inspection plan can itself be captured by the existing
read-only SQLite capture protocol while quiescent. Its raw main/WAL/SHM evidence is
retained; a recovered logical copy can seed a new private plan with the same IDs,
complete retained rows and pending reconciliation stage. Source capture paths
inside that copied plan continue to reference preserved immutable artifacts. Only
the copied plan is opened by the corrected executable. Admission must verify the
copied row totals, stage, source namespaces and referenced capture digests before
resuming reconciliation. No claim of repaired performance or completed real-family
inspection precedes that validation.
