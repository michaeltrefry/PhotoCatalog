# Organization and photographic search (sc-22842)

The Rust core exposes durable folders, flat and hierarchical keywords, ratings,
pick/reject flags, color labels, collections, combined filters, and recorded batch
operations. The CLI is the current interface; the desktop interface remains a
separate story. This document describes behavior, not a scale-performance result.

## Metadata authority and identity

S4 observations, source packets, selected models, and semantic conflicts remain
authoritative. The organization tables are indexed projections. Import, metadata
resolution/edit, and verified storage remapping refresh affected projections in
the same transaction. Searches never open originals. Existing offline catalogs
retain their known metadata and organization; lens data is available only when
source EXIF or retained XMP actually supplies it. Older decoded JSON without a
lens field remains readable.

A conflicted source field is not used as an authoritative filter value. Search
rows expose conflict names and effective model IDs; explicit catalog flag
provenance is included separately. Resolve a conflicting keyword/rating/label
field through S4 before changing that field. An explicit catalog choice, including
removal, keeps S4 precedence after an external source refresh. Opaque and qualified
properties remain in the full model. Organization edits reconcile the complete
selected property, derive addresses from that exact input, and apply one bounded
native edit pass before canonicalization; unordered arrays cannot reorder between
chunks. No operation writes source images or sidecars.

`dc:subject` is a flat term space; `lr:hierarchicalSubject` is a distinct hierarchy
whose components use `|` in XMP. The hierarchy API uses arrays of components.
Assigning `Animals|Birds|Owls` adds ancestor membership for recursive searches;
`keyword_direct` distinguishes explicitly assigned nodes. Removing a term removes
that exact assignment. Moving a hierarchy changes that prefix and its descendants
only in the explicitly selected assets, preserving per-item qualifiers and
unrelated leaves. Existing empty nodes remain until explicitly deleted; deletion
refuses nodes with children or assignments. Future imported source terms retain
their literal names rather than silently following an old rename.

Ratings are 0–5. Unknown/conflicted rating is returned as null; source XMP -1 is
retained and implies reject unless a catalog flag explicitly overrides it. Flags
are unflagged/pick/reject. Labels are arbitrary bounded strings, including empty
to clear. Collections carry creation provenance, revision-checked names, and
per-membership provenance. Membership and flags are linked to stable asset IDs,
so relinking originals preserves them. Removing a collection requires it to be
empty, making member removal an explicit, reviewable batch operation.

Folder identity uses tagged native path components, never display names. Unix
bytes, Windows UTF-16, drive roots, and UNC roots keep their originating semantics.
Unicode/case variants do not silently merge. Display strings may be lossy; they
are not locators. Unbound legacy rows have no invented folder association until
storage mapping supplies a tagged current locator.

## Query and cursor contract

`Query` combines text, one keyword (direct or recursive), a folder (direct or
recursive), collection, date interval, camera make/model, lens, format, rating,
flag, label, and conflict presence. Text is 1–16 whitespace-separated literal
prefix terms combined with AND using SQLite FTS5 Unicode tokenization; raw query
operators are not interpreted. Whitespace-only text is rejected. Exact scalar
filters preserve case except format, which is normalized uppercase. Dates use
photographic local calendar order at second precision; EXIF date spelling is
normalized, fractional seconds and timezone suffixes do not create an inferred
UTC timestamp. Bounds are inclusive `date_from` and exclusive `date_until`.
Missing dates sort before known dates and do not match a date interval.

Sorts are sequence, capture date, filename, and rating, in either direction. Stable
sequence breaks ties. Queries use indexed keyset boundaries with no OFFSET.
Membership or FTS drives sequence queries; capture/rating/filename use their sort
indexes. Remaining predicates are checked against at most the requested number
of candidate assets. Each request accepts 1–1,000 output rows and a candidate
scan budget between that limit and 4,096.

A result exposes `rows`, `scanned`, `page_complete`, `has_more`, `exhausted`, and
`next`. **An empty partial result is not an empty library.** Continue from `next`
when `page_complete` is false. The last scanned candidate forms the continuation,
including rejected candidates. `has_more` is conservative at an exact limit;
only SQLite end-of-stream establishes `exhausted`. VM instruction and sort counts
are returned as diagnostics; VM steps do not fully count work inside FTS posting
lists. Bounded candidate delivery alone is not proof that every engine operation
is independent of catalog size. Scale measurements are a separate acceptance
gate.

A serializable cursor binds the exact query, schema protocol, initial asset
high-water, and projection epoch. Changed projections—including concurrent
metadata completion during import—can invalidate it. The API returns an explicit
stale error; it never silently resumes across an incompatible result ordering.
Clients restart the query when they choose to adopt changes.

For uninterrupted traversal during edits/import, `search_session` owns a genuine
SQLite read snapshot with an internal cursor. Up to four sessions can exist in a
process, each with a 1–300 second lifetime, a capacity-one request queue, 256 MiB
engine cache, mmap disabled, and file-backed temporary storage. Expiry actively
closes the connection even if the client stops polling, limiting WAL retention.
Closing or dropping the session releases the snapshot. Expired sessions must
restart; there is no indefinite snapshot promise. Browse-process RSS, including
simultaneously active sessions, still requires the epic's measured 4 GiB gate.

Schema 4 creates a persistent high-water backfill checkpoint. Existing catalogs
run `organization-index` in batches of at most 1,000 assets. Search refuses an
incomplete or dirty index instead of presenting partial data as complete. Each
batch commits its projection rows and checkpoint together, and reopening resumes
from that checkpoint. Index backfill does not read original files.

## Durable operations and CLI examples

Use `photocatalog --help` and each command's help for exact positional arguments.
All JSON request files are capped at 1 MiB. Typical query and operation payloads:

```json
{"text":"blue sunset","rating":4,"lens":"RF24-70mm F2.8 L IS USM","sort":"capture","direction":"ascending"}
```

```json
{"operation":"move_keyword","from":["Travel","Old"],"to":["Travel","New"]}
```

`search` accepts a query file and optional cursor file. `search-session` emits
bounded pages as newline-delimited JSON. `search-plan` exposes estimated SQLite
plans without treating them as runtime proof. `folders`, `keywords`, and
`collections` are paged by durable IDs. Keyword create/delete and collection
create/rename/delete are explicit commands. `organize` applies one operation with
the expected asset metadata revision in one durable transaction.

For a large selection:

1. `organization-begin` records the immutable operation.
2. `organization-append` stages pages of asset IDs and expected revisions, at most
   1,000 per request. Repeated identical staging is idempotent; conflicting revision
   values are rejected atomically.
3. `organization-seal` closes selection. No asset changes occur while preparing.
4. `organization-step` processes bounded individual transactions. The change,
   projection, audit event, and completion marker commit together.
5. `organization-show`, `organization-items`, and `organization-events` expose
   exact applied, pending, failed, skipped, and cancelled state after restart.

A failure rolls back the affected asset, records the error, and pauses the batch.
Previously acknowledged items are not replayed. `organization-review` explicitly
accepts a new revision for retry or skips the failed item. Cancellation stops
future work while retaining exact prior results and remaining selection. It is
not represented as all-or-nothing undo. Each asset transition is atomic, while
multi-asset progress is explicitly resumable and visible. Point edits avoid job
staging commits. Lock contention and disk failures remain surfaced errors.

## Validation status

The core tests use real disposable JPEG/XMP imports for source preservation,
conflicts, qualifiers, lens tags, relinking, restart, and batch faults. Synthetic
metadata fixtures exercise production projection/query APIs against independent
mathematical result oracles, ties, both directions, partial empty pages, and
snapshot changes. They make no RAW decode claim and no million-row timing claim.
The production-query measurement protocol must be frozen and independently
reviewed before the 1/5/10 million campaigns. Full integrated tests, cross-platform
CI, final performance evidence, and parent review remain acceptance gates.
