# Catalog writer admission

The failed v3 mixed workload exposed starvation between independent SQLite
writers in one process. Its overall save p95 passed because 181/187/183 of the
200 saves ran after the background source work ended. The actual overlapping
save populations had p95 292.180/413.699/280.726 ms at 1M/5M/10M, violating the
existing 100 ms target. The old receipts and parent rejection remain preserved;
the successful query-only evidence does not establish mixed acceptance.

A canonical catalog root now owns one shared, in-process transaction admission
queue. Each connection retains its own SQLite connection and read snapshots.
Foreground and background requests are FIFO within their class. At transaction
handoff, pending foreground work takes precedence over background work. A new
request cannot bypass an earlier request in the same class. Condition-variable
notification replaces repeated local SQLite lock competition; no workload sleep,
sample retiming, or busy-timeout adjustment is introduced.

The permit is acquired immediately before the writing transaction, after existing
packet parsing, model preparation, rendering and validation stages outside that
transaction. Existing authoritative revision checks remain inside the transaction.
The permit is declared before the transaction, so rollback completes before
admission is released on errors/unwind. Successful commits release admission
before post-commit reads/callbacks. Recursive writer admission on the same thread
returns an explicit error instead of deadlocking. Read-only snapshots, ordinary
current-schema catalog opens and TEMP-only sidecar enumeration do not acquire it.

| Boundary | Class | Permit scope |
| --- | --- | --- |
| Catalog initialization/schema migration | Foreground | Migration transaction; version rechecked after admission |
| Original import reserve, failed state, ready publication | Background | Existing atomic state/projection transaction; decode/hash/preview bytes outside |
| Source retain and unavailable-source observation | Background | Existing source/model/history/effective projection transaction |
| Import path declaration and verified volume binding | Background | Existing storage binding transaction |
| Embedded/sidecar current-locator recording | Background | Individual persistent helper call, after source retention |
| Organization projection backfill/dirty batch | Background | One existing bounded index batch |
| Metadata edit, conflict choice, export planning/publication | Foreground | Existing transaction and unchanged revision CAS |
| Stale-export recovery receipt | Foreground | Receipt UPDATE after safe restoration; delegation to current export stays unwrapped |
| Preview final generation authority | Foreground | Existing final catalog/manifest publication guard |
| Organization keyword/collection/batch control | Foreground | Transaction or individual atomic statement |
| Organization batch asset application | Foreground | One asset/progress transaction, including failure/review recording |
| Explicit relink preparation/apply/undo and legacy declarations | Foreground | Existing atomic storage transaction; orchestrators do not hold a second permit |

Automatic known-volume reconnect delegates to individually admitted storage plan
transactions. A relink still provides its existing atomic semantics and may hold
the writer for the duration of that operation; admission cannot preempt an active
transaction. Organization batches continue to yield between assets. Parsing and
admission wait are included in the unchanged operation latency measurement.

This is foreground prioritization within one application process, not a promise
of background progress under an infinite foreground stream. SQLite retains its
five-second busy timeout and all cross-process locking/durability authority;
independent CLI processes do not share this admission queue. No cross-process
fairness or universal 100 ms guarantee is claimed.

Driver protocol 4 additionally gates the actually overlapping save and snapshot
read populations at p95 <=100 ms. Zero overlap fails. Both complete-population
and overlapping/nonoverlapping distributions and counts remain reported. The
200-operation workload, source input bytes, intervals, thresholds and quantile
calculation are unchanged. The previous full v3 pass is superseded for mixed
acceptance, rather than rewritten.

Source-ready regressions queue actual source-retention and organization API calls
behind a held admission, prove FIFO/foreground ordering through committed history,
and verify rollback, unwind, recursive rejection, distinct catalogs, current-schema
open/read behavior, stable revisions and source-byte preservation. Native test and
measurement results must be recorded separately after the coordinated lane opens.
