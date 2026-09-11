# Lightroom failed-generation repair adoption (protocol 2)

This is the explicit S9 transition from the failed schema-2 v4 inspection to a
new v5 generation with the bounded page-memory runner. It does not retry v4,
raise its 512 MiB Python admission, upgrade the native inspector, or execute a
migration. Legacy protocol-1 paused schema-1 adoption remains unchanged.

## Admission and failure evidence

The new run is initialized exclusively with a pinned runner, generation helper,
configuration and the existing native binary. Initialization records identity;
it does not mean adoption is ready. `adopt RUN` writes a pending attempt marker
before verification. Any failure retains that marker and all owned partial
artifacts, and a second adoption attempt is rejected. Only the final
`generation-adoption.json` permits main replay.

The protocol-2 request pins the exact failed main attempt/result, its failed
phase, current control document and a separate parent ownership review. The
review must bind the failed result and phase hashes, terminal command boundary,
known-process absence and no unresolved command. The original supervisor's
`failed_or_unknown` and ownership qualification are preserved in the new
receipt. A missing pause is required; no successful pause or failed-command
success is fabricated. This narrowly admits the demonstrated Python memory
interruption with a reaped root and no observed cleanup uncertainty, rather
than treating an arbitrary failure as restartable.

Both predecessor runner locks are held: current v4 and its explicitly retained
protocol-1 v2 ancestor. All local command receipts and steps must be terminal
successful, source-bound, unique and match the sealed command-index hash.
The ancestor's bound adoption receipt supplies its distinct prefix, binding
and physical state. Original v4 integer command IDs and `adopted/NNNNNNNNN`
ancestor IDs resolve in separate namespaces. Replayed results are labelled
`adopted/local/NNNNNNNNN` or `adopted/inherited/NNNNNNNNN`; they are never
recorded as commands newly executed in v5. No integer coercion merges these
namespaces. A deeper or different lineage requires a separately reviewed
protocol; this implementation supports the actual two-origin transition.

## Copy and reconciliation

Only quiescent private derived plans are eligible. The exact source file
identity and all present SQLite companions are checked before and after the
copy and verification. Nonempty WAL or rollback journals are rejected; a
separately reviewed recovery is required. The runner locks coordinate these
inspection processes, and the independent ownership review supplies admission
for this specific stopped run; this is not a global SQLite writer lock claim.

One exclusive owned working copy is made, preserving every present companion.
Each copy records streaming source SHA-256, destination readback SHA-256,
length and source identity; files and containing directories are synced.
The source is never opened by SQLite. One read-only immutable typed scan of the
owned copy verifies application identity, schema 2, all 13 physical tables,
retained counts and revision coverage, and no family choices. It records
ordered type-preserving digests including rowids, opaque BLOB/TEXT bytes and
floating-point bits. No schema or data mutation, native upgrade command, or
second logical scan is performed. Source/copy physical hash equality establishes
copy equivalence; the typed scan reconciles the copied state to the checkpoint.

Completed outcomes are checked against retained capture manifests, source
inventory, report counts/stages and saved row-page digests. The active prefix
is reconstructed from its immutable saved outputs and checked against its
exact page count/cursor and retained table counts. Page JSON is consumed in
separate scopes using the same streaming canonical hashing as the repaired
main runner. No page payloads or original photo values appear in the request.
Capture manifests retain their original private paths; the transition does not
recapture originals or claim fresh verification of every large raw artifact.
Existing capture readback remains the source-evidence boundary.

## Bounds and continuation

The request declares per-artifact copy size/time, typed scan time/rows/row bytes,
command count and capture count. Typed scanning bounds whole rows, not chunked
cell I/O; the cap includes retained hex expansion. A separately reviewed exact
argv recipe supplies the 512 MiB Python sampled stop, disk reserve and external
deadline. Sampled observations are not an RSS high-water mark or hard guarantee.
A failed attempt is preserved, never reclassified after a threshold change.

After independent adoption readback, a separate main continuation recipe is
required. The existing persistent fresh-inventory admission barrier runs before
new inspection commands; failed or orphan admission cannot silently retry.
Completed prefixes replay against their original hashes and exact old-to-new
plan-path mapping. The active member resumes at the existing cursor. Full
companions, original-photo path/packet phases, family choices and migration
remain outside this transition and require their existing concrete gates.

Tiny fixture contracts exercise exact two-origin replay, source preservation,
missing ownership, changed binding/hash/control/lineage, failed local commands,
wrong schema/counts, cursor mismatch, nonempty WAL, partial copy and immutable
receipt admission. Actual v5 adoption is separate private evidence, not implied
by these fixtures.
