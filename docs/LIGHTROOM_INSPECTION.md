# Lightroom inspection and preservation protocol (S9)

Protocol 1, reviewed before implementation and wider source capture. This is
inspection and migration planning only. It never changes a PhotoCatalog catalog,
imports assets, executes Adobe/Lua text, rewrites source XMP, or claims Adobe
rendering equivalence. Native paths use the existing lossless `NativePath` type.

## Capture boundary

The CLI uses a dedicated capture process, with no SQLite connections to original
files and no other threads opening/closing the same original inodes. POSIX record
locks are process scoped and another descriptor close can release them. All live
source handles are read-only, non-following, regular-file verified handles;
source and output trees must be disjoint after resolving existing ancestors.
Output is an exclusively created, application-owned directory outside originals.

1. Inventory each explicit catalog and all adjacent `catalog.lrcat-{wal,shm,journal}`,
   Lightroom lock markers, stem `.lrcat-data` and `.acr` companions, including
   bounded recursive directory contents. Record absent companions as well as
   present files. Record excluded rebuildable preview/helper caches explicitly.
   Directory entry, depth, file-size and total-byte limits are explicit errors,
   never evidence of absence or a complete scan. Reject links, special nodes,
   duplicate inode aliases, and changing directory sets.
2. Acquire nonblocking read locks using the SQLite native byte protocol: the main
   file range `[0x40000000, 0x40000200)` and, for an existing WAL shared-memory
   file, the deadman-switch byte `128` first, then bytes `[120,128)`. The
   shared deadman lock remains held throughout capture so a fresh SQLite opener
   cannot take its exclusive initialization lock and truncate SHM. Unix uses `fcntl(F_SETLK,F_RDLCK)`; Windows uses
   `LockFileEx` shared locks with immediate failure. This blocks default SQLite
   main-file writers and WAL writers/checkpointers/recovery, rather than relying
   on unrelated `flock` locks. If a nonempty WAL has no lockable SHM, report an
   unsafe capture and retain no claim of a complete logical snapshot. Explicitly
   reject a hot rollback journal from complete inspection; retain its evidence.
3. If an auxiliary directory has a recognizable RocksDB/LevelDB layout and an
   existing `LOCK` file, acquire a read lock on that existing file's entire
   lock range before enumerating/copying auxiliary contents. Do not instantiate
   RocksDB or create a missing lock. Lock contention is an active-writer failure.
   This is a cooperative protocol, not protection against arbitrary writers or
   filesystems with broken advisory locks. Auxiliary format identification and
   recognized locking behavior are recorded as evidence, not guessed from the
   `.lrcat-data` extension alone.
4. Hold the same descriptors and locks across bounded copying and a second full
   source digest pass. Compare descriptor/path identities, sizes, modification
   and change times, companion existence, directory membership and Lightroom
   lock markers before and after. Never follow growth beyond the original
   declared length. New/disappearing/replaced files, changed bytes or active lock
   markers invalidate the capture. Preserve failed-stage diagnostics explicitly;
   an interrupted job cannot publish a ready snapshot.
5. Raw evidence files, including original main/WAL/SHM and complete auxiliary
   trees, remain untouched after publication, with individual digests and native
   source paths. Recovery/SQLite backup happens only on a separate private
   working copy. Validate WAL framing/checksums and retain any uncommitted/stale
   tail classification; SQLite recovery must not silently hide malformed input.
   The resulting standalone logical snapshot receives its own digest, integrity
   result and relationship/count report. It is never called original bytes.
6. Record SQLite-consistent, auxiliary-preserved and application-consistent as
   separate states. Stable auxiliary bytes without a verified writer protocol or
   closed-application evidence remain application-consistency-unverified. They
   may be inspected and retained but cannot produce an unqualified complete
   preservation status. Inspection-stage completion and complete opaque byte retention
   are separate from application consistency and semantic/import/render support.
   Missing required auxiliary evidence, including `hasBigData`
   or AI metadata references with no corresponding companion, is incomplete.
   A closed-application assertion must have explicit provenance; process absence
   alone is useful observation, not proof of cross-store transactional atomicity.

No live SQLite open, checkpoint, recovery, new journal/SHM creation, write handle,
or lock-file creation is permitted. Capture failures do not silently fall back
to an unlocked immutable read of a live source. The read locks are short-lived
per catalog; a contending application receives no automatic retry loop from us.

## Family selection

Discovery returns every candidate plus explicit exclusions and limits. Filename
version/suffix and filesystem times are hints only. Internal provider IDs,
image/file global IDs and content overlap, schema version, image change times,
history/snapshot times, and source-stored prior names contribute separate
provenance-bearing evidence. Provider IDs are not assumed globally authoritative.
Renamed families can be related by internal evidence, while independent catalogs
with the same year/name remain distinguishable. An explicit family/member choice
is recorded with its evidence revision; changing evidence invalidates that choice.

A report shows a suggested current member, selected member if resolved, all older
excluded members, and every contradictory/missing/tied recency signal. A newer
schema alone does not prove newer photo edits; copied filesystem timestamps alone
do not prove recency. Conflicting branches are never merged or silently selected.
The observed 2018 suffixed-newer case and repeated upgrade suffixes are fixtures.

## Durable plan and bounded inspection

A separate versioned `inspection.sqlite3` stores inventory, revisions, family
choices, preserved artifact records, schema objects, table/column inventories,
lossless source rows, normalized inspection projections, references, issues,
counts and resumable stage progress. The application catalog schema is unchanged.
A source catalog lineage key, capture revision ID and source row identity are
separate: original global/local identifiers remain available alongside a stable
namespaced source ID. No ID is derived from a photo's mutable filesystem path.

Retain every ordinary source table/column and SQLite value type, including unknown
rows, blobs, invalid-UTF8 text bytes and real-number bits. Preserve original schema
text without executing it. Whitelist generated read queries against ordinary
physical tables; reject virtual/generated execution surfaces explicitly. Disable
trusted schema/extension loading, impose SQLite limits and cancellation, and
never evaluate stored Lua, plug-in code, smart-collection text or develop text.
Unknown constructs remain retained-only with reconciled source row counts; any
unreadable table has an explicit incomplete count, not an invented zero.

Source rows are streamed into durable bounded transactions with a restart cursor;
finished batches are idempotent. Consumers page the plan by stable key/sequence,
not large OFFSET scans. Failed or cancelled work leaves pending stages and cannot
publish a complete plan. New captures never overwrite prior evidence or plans.

The dry run enumerates files and path origins, distinct virtual copies and master
relations, folders, keywords/hierarchies/synonyms, ratings/flags/labels, collections
and membership/order, published/smart constructs, develop/before/history/snapshot
records, and every unsupported table/category. Known references are reconciled
against source IDs, with dangling relations, cycles, duplicates and cross-selected
catalog conflicts listed individually. Physical source table totals and semantic
category totals remain separately checkable. Basic photographic metadata uses
explicit schema profiles; unknown versions keep all raw evidence and identify
unverified projections rather than guessing compatibility.

Missing/foreign/native paths are distinct. Inspect only explicit referenced paths,
never recursively scan the originals tree. Any root mapping used for inspection is
recorded and does not modify source paths or authorize migration. Existing sidecar
and embedded XMP use the existing packet-preservation layer; unavailable or failed
source packet inspections stay explicit. Catalog XMP cells preserve the original
compressed BLOB/type/digest and separately labeled decoded XML bytes. The observed
4-byte big-endian length plus zlib wrapper requires bounded exact-length decoding;
unknown wrappers remain retained-only. Conflicting catalog/sidecar/embedded facts
carry source IDs and retained versions, without choosing one automatically.

Semantic support is stated per construct. Opaque Adobe develop/history/snapshot,
AI/mask/profile and plug-in instructions remain preserved and enumerable. Neither
opaque retention nor XML parsing is evidence of matching Adobe rendering.

## Required verification before release

Synthetic fixtures cover supported/unknown versions and columns; all SQLite value
types; malformed and valid compressed XMP; row/relation reconciliation; virtual
copies; hierarchy cycles; sparse/duplicate identifiers; known and unsupported
constructs; copied mtimes, renamed families, misleading/repeated suffixes, divergent
branches, ambiguity and 2018-style recency. Capture tests use actual subprocess
SQLite writers in rollback and WAL modes, committed/uncommitted/torn WAL evidence,
a fresh SQLite opener against quiescent existing SHM (deadman lock),
missing/changed companions, contended auxiliary locks, symlink and special-node
races, cancellation/restart, output overlap and exact source preservation.

Private selected-current dry runs use newly captured copies after this protocol
and implementation are reviewed. Earlier 2013/2019 compatibility samples do not
become selections automatically. Each receipt binds current source digests,
selection evidence, counts/relations, raw/decoded XMP, limitations and exact code.
Cross-platform hosted tests and independent review are required for completion.

## Primary sources

- SQLite [native lock ranges](https://raw.githubusercontent.com/sqlite/sqlite/version-3.51.3/src/os.h),
  [Unix SHM locks](https://raw.githubusercontent.com/sqlite/sqlite/version-3.51.3/src/os_unix.c),
  [Windows locks](https://raw.githubusercontent.com/sqlite/sqlite/version-3.51.3/src/os_win.c),
  [WAL framing](https://raw.githubusercontent.com/sqlite/sqlite/version-3.51.3/src/wal.c),
  and [copy/locking hazards](https://www.sqlite.org/howtocorrupt.html).
- RocksDB [POSIX file-lock implementation](https://raw.githubusercontent.com/facebook/rocksdb/v9.11.2/env/fs_posix.cc).
  Layout resemblance alone does not identify Adobe's exact embedded engine version.
- Adobe [catalog and auxiliary-data requirements](https://helpx.adobe.com/lightroom-classic/desktop/technical-support/workflow-issues/catalog-issues/catalog-faq-lightroom.html).


## Current CLI and checkpoint

The dedicated `lightroom_inspect` binary exposes these independent stages. All
paths below are explicit user-owned destinations outside the originals tree.
Commands emit JSON; redirection is performed by the invoking shell, not by an
implicit export or migration operation.

```text
lightroom_inspect discover CATALOG_DIRECTORY
lightroom_inspect create NEW_PLAN_DIRECTORY
lightroom_inspect register-inventory PLAN INVENTORY_JSON
lightroom_inspect capture SOURCE_LRCAT NEW_CAPTURE_DIRECTORY --main-only
lightroom_inspect capture SOURCE_LRCAT NEW_FULL_CAPTURE_DIRECTORY
lightroom_inspect add PLAN CAPTURE_DIRECTORY
lightroom_inspect resume PLAN REVISION --max-rows 1000
lightroom_inspect rows PLAN REVISION --table Adobe_images --after 0 --limit 100
lightroom_inspect report PLAN REVISION
lightroom_inspect issues PLAN REVISION --after 0 --limit 100
lightroom_inspect check-paths PLAN REVISION --limit 100 --packets
lightroom_inspect paths PLAN REVISION --after 0 --limit 100
lightroom_inspect packets PLAN REVISION --after 0 --limit 100
lightroom_inspect packet-bytes PLAN REVISION PACKET_SEQUENCE --offset 0 --limit 65536
lightroom_inspect metadata-conflicts PLAN REVISION --after 0 --limit 100
lightroom_inspect families PLAN
lightroom_inspect assign-family PLAN REVISION FAMILY --reason EXPLANATION
lightroom_inspect choose PLAN FAMILY REVISION --expected-evidence DIGEST --reason EXPLANATION
```

`capture` launches a dedicated `capture-worker` subprocess. Its output directory
must be new and have an existing parent; source/output ancestry overlap is rejected
before any output creation. A failed capture retains a failed manifest and any raw
files already copied. `--main-only` omits auxiliary bytes deliberately and reports
that omission. Complete opaque companion byte retention does not identify Adobe's
engine or establish cross-store consistency. The optional
`--closed-application-evidence` is an explicit assertion with provenance, not an
inference from the absence of a process.

The plan has its own application ID `0x50434c49` and schema version 3. Opening
an unrelated database or an older inspection plan is rejected by an immutable
read before write-open or journal configuration. Original artifact digests are
verified on addition; logical snapshot object/size/change-time identity is checked
before and after resumed reads. Generated source namespaces remain separate from
capture revisions and original global/local IDs; family assignment does not merge
source records.

Version 3 corrects derived relationship keys so exactly integral, representable
REAL identifiers can match equal INTEGER identifiers without changing retained
cells, original source keys/IDs, global IDs, or opaque packets. Duplicate numeric
identifiers remain separate entities and ambiguous relationships remain unresolved.
The ordered paging/queue indexes introduced in version 2 remain required.

The CLI `create` receipt reports `schema: 3`. Versions 1 and 2 require a **new
plan from preserved captures**; no automatic rewrite, partial upgrade, or deletion
of their retained rows, packets, paths or journals occurs. Existing frozen runs
remain bound to their original executables and evidence. See the
[numeric relationship correction](LIGHTROOM_NUMERIC_RELATIONSHIPS.md) for the
precise equality and compatibility policy. This does not remove unknown source
schema warnings or qualify Adobe rendering semantics.

Default capture limits are 16,384 entries, depth 16, 8 GiB per source file and
64 GiB total. Reader and writer share a 16 MiB manifest budget. Derivative row
batches and row pages have an 8 MiB cumulative serialized-data budget in addition
to row limits. Text/blob cells use reversible hex byte strings; real cells retain
the observed IEEE-754 bits. A cell has a 32 MiB SQLite limit. A row that cannot fit
the derivative budget remains in the original snapshot with a failed table
projection and reconciled expected/retained counts; it is not silently skipped.
Packet bytes can be retrieved in chunks of at most 1 MiB. Original catalog XMP
cells and transformed parse input are separately addressable.

`report` includes a bounded issue sample and the total issue count; `issues`
provides keyset access to individual source-ID diagnostics. Family comparisons
admit at most 256 captured revisions per plan. Uninspected discovery candidates
remain visible, and a family choice is explicit and bound to the full evidence
digest. Adding contradictory evidence invalidates the choice. The preliminary
numeric Adobe schema profiles are 1100000 and 1300000 from the privately copied
2022 pair; other schemas retain raw data and identify semantic projections as
unverified. Unsupported views, triggers, indexes and other schema objects remain
stored verbatim without executing their text. Physical tables with generated,
virtual or unpageable rows remain explicitly snapshot-only.

Direct path checks do not scan the originals tree. Missing and foreign paths are
separate from available files. Without `--packets`, the report explicitly records
uninspected embedded/sidecar XMP; running with `--packets` can upgrade that bounded
metadata-only observation. Sidecars are checked even when the original is missing.
Conflicting projected values preserve every candidate and provenance; neither this
command nor family review applies any metadata or asset migration.

Initial local checkpoint: 15 synthetic integration tests and two source-handle
regressions pass on macOS, including independent rollback/WAL writer processes,
committed WAL recovery, a fresh-opener SHM deadman positive control, FIFO timeout,
changed/replaced evidence, malformed WAL, missing SHM, opaque typed rows, virtual
copies, hierarchy cycles, missing-original sidecar preservation/conflicts, and
bounded oversized-row retention. All-target Clippy passes. This is local evidence,
not a claim of hosted cross-platform success, a completed private-current-family
dry run, or completed S9 acceptance. Wider live captures remain review-gated.


Publication uses a flushed `manifest.pending.json`, then atomic no-clobber
hard-link creation of `manifest.json` in the same private directory. All referenced
raw and logical filenames are synced first on Unix. An existing final name is
never replaced. Unsupported hard-link filesystems fail explicitly with pending
evidence retained. Pending JSON is an unpublished description and is not accepted
by `read_manifest`; only the final name is consumable. File contents are flushed
on all platforms; Windows lacks the Unix directory-fsync step here, so this code
does not claim equivalent directory-entry power-loss durability on Windows.
The publication hook regressions inspect references at every boundary and inject
both interruption and an unexpected destination before final-name creation.

Review repairs bind every family choice to the inspection evidence revision as
well as the capture, summaries, stage and inventories. Row retention, failed row
projection, and each file's packet/fact/path enrichment advance that revision in
the same transaction as their evidence. A metadata-only choice therefore becomes
stale after packet inspection even when the completion-stage label stays equal.
Repeated `resume` on a reconciled capture is a no-op after source verification.
A missing snapshot timestamp is incomparable with a present one; schema version
cannot supply that missing recency evidence. Catalog XMP facts receive a file
association only through a unique image-to-file reference chain. Duplicate or
missing links retain the facts with NULL association and an explicit issue.

Selected catalogs also report exact native-locator collisions independently of
global identifiers. `possible_path_collision_count` is reconciled over all pairs;
`possible_path_collisions` is a bounded sample. Equal locator text does not prove
the same file, content or catalog identity. Obtain every pair through
`path-collisions PLAN LEFT_REVISION RIGHT_REVISION --after-left N --after-right N
--limit 100`, continuing with both sequence values from the last returned pair.
No case-folding, mount equivalence or path-identity inference is applied here.

Derived schema admission is capped at 4,096 native/schema objects and 8 MiB of
schema text/name data; exceeding a cap rejects plan admission explicitly while
the raw capture remains intact. WITHOUT ROWID detection uses SQLite's native
`PRAGMA table_list` metadata, never parsing stored SQL. Text primary-key cursors
bind their exact original bytes as SQLite TEXT, including invalid UTF-8. Row,
packet, path and metadata-conflict pages include their full serialized descriptor
in the 8 MiB output budget, including compact JSON array punctuation and the
CLI’s single final newline. The CLI emits compact JSON. A short page is not EOF: resume from its last cursor
until an empty page. A single oversized descriptor fails explicitly; the original
snapshot/packet bytes remain retained. These are derived-inspection limits, not a
claim that unsupported source data was fully projected.
