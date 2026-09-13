# Selected-image metadata inspection

S12 adapter wire contract, based on epic E6/E10 and the existing logical-image
metadata and retained Lightroom history APIs. This document does not establish
desktop or installed-platform acceptance.

The actor owns catalog selection. The adapter owns no filesystem writes, export
authority, durable stream state, or schema. Requests use `command`/`args` and
responses use `kind`/`value`, nested under the actor's metadata envelope. Every
exposed integer identity, revision, counter and byte offset uses a canonical
decimal string. Retained JSON stays opaque text; clients must not parse and
re-emit it to preserve evidence. Native filesystem paths keep `NativePath`
encoding; importer-defined locators remain opaque bytes.

## Requests

| Command | Arguments | Result |
| --- | --- | --- |
| identity | selected variant key | logical image metadata identity |
| fields | identity, opaque cursor, limit | effective values, conflict flags, selected model |
| candidates | identity, field, opaque cursor, limit | current candidate values and source/model identities |
| sources | identity, opaque cursor, limit | source association, availability, locator and current observation |
| observations | identity, opaque cursor, limit | immutable source revisions, issues and provenance |
| models | identity, observation, opaque cursor, limit | model descriptors, projections and parse errors |
| packets | identity, observation, opaque cursor, limit | original packet descriptors, length and digest |
| decisions | identity, opaque cursor, limit | explicit resolution/edit history |
| file_instances | identity, opaque cursor, limit | owned file-instance provenance and observation time |
| resolve | key, expected metadata revision, field, model | new metadata revision |
| blob_chunk | key, packet observation/ordinal or model ID, byte offset, length | exact retained bytes, offset, total, digest, continuation |
| text_chunk | identity, typed text reference, byte offset, length | exact UTF-8/locator bytes and continuation |
| import_history | key, optional opaque anchor, direction, optional opaque cursor, limit | selected-image retained relations and coverage |
| import_fields | key, opaque anchor, row/table/entity role, field cursor, limit | retained typed field descriptors |
| import_chunk | key, opaque anchor, role, field, byte offset, length | retained typed field bytes and continuation |
| adobe_properties | key, opaque anchor, column, opaque settings path, property cursor, limit | lexical properties and compatibility accounting |

Page collections have `rows`, `next`, and work counters where scanning can
produce an empty page. The exact returned cursor must be replayed; an empty page
with continuation is not exhaustion. Metadata cursors bind the reviewed logical
image identity. Imported anchors are re-proven by the existing core for every
navigation request and additionally checked against the selected variant.

Potentially large retained values use `{bytes, inline, reference}`. Inline text
is optional; the typed reference permits reading every retained byte in bounded
chunks. These references are not arbitrary SQL column names or database IDs:
the adapter validates their selected-image ownership and uses fixed queries.

## Bounds and existing core gaps

The existing `metadata_for_image` expands all candidates and sources;
`metadata_history_for_image` expands every model for each observation; packet
inspection expands every retained packet. The desktop adapter must not forward
these whole views. It uses indexed, read-only pages over the same authoritative
tables, admitting field byte lengths before loading values. Candidate traversal
must bound both sources and models and preserve progress through absent fields.

Existing packet storage is zlib-compressed, with a 16 MiB raw per-packet cap.
Chunk reads stream compressed SQLite substrings with fixed buffers, inflate
at most that cap and verify the complete packet digest before returning a slice
of at most 16 KiB. Repeated chunks repeat this bounded CPU work; no packet fleet
or whole raw packet is allocated. Byte output preserves UTF-16 and other packet
encodings without reconstructing XML.

Imported history APIs already limit traversal to eight hops and bound retained
input/output bytes. Their bounded selected-record interpretation remains distinct
from page response bounds. Oversized or uninterpretable Adobe settings must be
reported as retained-only, with complete retained field bytes still accessible.
Property lexical spelling, coordinate-space labels, compatibility failures and
`adobe_rendering_equivalent: false` remain visible. Retention is not renderer
equivalence.

Resolution calls `resolve_metadata_for_image` with the exact selected variant,
reviewed metadata revision and current candidate model. The existing core checks
revision and source currency inside its writer transaction. This chooses an
effective catalog value while preserving observations, models, packets and files.
Metadata writing/export to originals or sidecars requires its separate explicit
workflow and is not fulfilled by this adapter.

## Required verification

Use synthetic logical master/copy fixtures with conflicting sidecar, embedded
and catalog values. Verify selected-image ownership, stale resolution rejection,
complete source/provenance and decision navigation, byte-cut continuation,
UTF-16 packet reconstruction, checksum rejection, oversized retained fields,
bounded source/model scans, imported sibling isolation and history compatibility.
Record source hashes and actual gate results separately from this contract.

## Exact transport conventions

The Rust `Request`/`Response` definitions in `src/application/metadata.rs` are the
wire source of truth. `key` is `{asset_id,variant_id}`. `identity` reuses the
organization `ImageIdentity`. `after` is nullable opaque text for all current
metadata pages, including fields, models and packets. A response cursor binds the
entire identity and query; replay with a refreshed identity is rejected.

Imported `direction` is `Incoming` or `Outgoing`; `role` is `row`, `table`, or
`entity`. The initial `import_history` request may omit `anchor_json`; every
continuation must supply its returned anchor and `next` as `after_json`.
`import_fields.after` is a field-name string (initially empty).
`adobe_properties.after` is a decimal string (initially `"0"`); its property
page `next` uses the same spelling. `settings_path_json` is opaque JSON text,
initially `"[]"`; returned property `path_json` may be passed back for inspection.
These are read-only selectors; they never apply imported settings to an image.

`BlobSource` is `{kind:"packet",observation,ordinal}` or `{kind:"model",id}`.
The `chunk` response is `{bytes:number[],offset,total,next,blake3,verified,
inspected_bytes}`. Byte array elements are 0–255 numbers; all other numbers in
this shape are decimal strings. `next` and `blake3` may be null. Text references
use the tagged Rust `TextReference` shape and must be sent back unchanged.
Text chunks may split UTF-8 characters or UTF-16 code units; concatenate bytes
before decoding, and retain the original encoding for packet inspection.

`limit` (1–100) and `length` (1–16384) are bounded JSON numbers, not identities.
Current pages never scan more than `Limits.scan_rows` candidates per request.
A single retained packet chunk hashes at most 16 MiB using fixed 16 KiB buffers,
with cancellation checks on input/output progress. Imported interpretation
re-proves at most eight ancestry hops and uses existing 8 MiB record / 4 MiB
history output ceilings; retained field chunk reads verify at most one existing
1 MiB storage chunk. Those costs repeat on navigation and are separate from the
returned page-byte limit. Resource-limit errors preserve continuation state;
clients must show the error and must not infer exhaustion.

## Native copies of imported images

When the selected native copy lacks a direct importer mapping, history follows
at most 64 immutable `copied_from_sequence` links on that same physical asset.
Every link must move to an earlier sequence. A direct mapping always wins;
missing origins, corrupt cycles, mismatched assets and excess depth are explicit
errors. The retained mapping is validated against the proven ancestor, while
all returned anchors and page keys remain bound to the selected copy. Parent
and sibling anchors cannot be replayed for that selection. Packet inspection
still uses the copy's retained observation associations: later parent packets
do not become visible merely because imported history has a shared ancestor.
