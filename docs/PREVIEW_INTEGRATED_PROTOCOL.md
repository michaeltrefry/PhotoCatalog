# Integrated retained previews at 10 million assets — version 3

Status: source preparation, **UNRUN**. This fixed extension closes the integrated
catalog-plus-preview resident-memory measurement left open by the 10k navigation
component protocol. No performance result or backend/default award is implied.
Execution requires independent source review, a clean release build bound to the
whole source archive, and the parent's explicit serialized Mac lane. Existing
codec, worker-reservation, quality, and headless page targets do not change.

This source revision adapts the existing workload to S8 schema 6. Version-2
schema-5 measurements and frozen binaries remain historical evidence; this is
not a rerun, replacement result, or a new performance claim.

The donor is a separately, explicitly migrated schema-6 copy of the pristine
schema-5 S7 synthetic 10M catalog. Original schema-4-to-5 migration ancestry must
remain byte-identical. The historical schema-5 main SHA256 is
0ac57eefbfbfeee46e83c17b8116c48ae3cb0dedae01605c1016981a536e91db;
it is an ancestor, **not** the new schema-6 donor hash. Never use a transition or
mixed-workload-mutated copy. A later 5-to-5 verification can accompany the old
proof but cannot substitute for either migration.

Private paths remain outside Git. The source binding contains `source_catalog`,
`source_revision`, `catalog_count` (10000000), `schema_version` (6), and `files`
(keys `""`, `-wal`, `-shm`, `-journal`, each with `present` and, if present,
`bytes`/`sha256`). Its `receipts` retain `build_reference`, `preparation`, and
`source_verification` path/SHA256 pairs. `migration_ancestry` still contains the
original `proof`/`native` pairs (native protocol 1, 4-to-5) and optional separately
labeled 5-to-5 `verification`. New `schema6_migration` contains distinct
`proof`/`native` pairs (native protocol 2, 5-to-6, catalog_schema=6).

The new proof binds the old schema-5 main to the actual schema-6 donor hash,
requires successful native/observer results, and compares every pre-existing
typed table/row identity with the original ancestry. `identity_scope` must be
`pre_existing_tables`. `added_tables` separately lists the nine initially empty
edit/export tables and four alias tables. Alias directories/paths start empty;
alias state has one row and dirty membership contains every existing binding.
`alias_initial_state` separately reports unbound assets and dirty count, checked
against pre-existing asset/binding counts. No whole-schema hash equivalence is
claimed, and no alias reconciliation runs during timed reads. The lens/capture
index remains exact. Copy both generations' original receipt bytes into the new
bundle, named `proof`/`native` and `schema6_proof`/`schema6_native`; never rewrite
historical paths or migrate the preserved source. The current overlay and page
probe refuse schema 5 before opening through `Catalog`.

## Source-preserving setup

Use `scripts/preview_integrated_campaign.py prepare`. It exclusively creates a
new private bundle outside the donor directory, checks reviewed file identities
and free space, then copies main and every present companion with read-only file
handles and exclusive output creation. The minimum reviewed free-byte allowance
must cover all source bytes, the retained encoded-cache quota, and at least 2 GiB
of additional database/filesystem headroom. This is admission, not a guarantee
against another process consuming disk space. Every copied file is synced and
hashed; source hashes and companion presence are rechecked. **No original is
opened with SQLite**, checkpointed, recovered, renamed, or deleted. Partial copies
and failure receipts are retained; retries use a new directory.

Only after a complete raw-copy receipt does the Rust `preview_navigation_probe
overlay` command open the new copied catalog. It requires application ID
1346913089, schema exactly 5 with the exact `organization_lens_capture` index, and
exactly 10M assets. Both overlay and measured preflight reject schema 4 before
Catalog::open; there is no implicit measurement-time migration. It republishes the selected
512/JPEG80 cache under the existing first-10k `fixture-{sequence:012}` IDs. It
reuses the reviewed layout's 30 seed payload identities and identical 37-byte
unique COM construction, verifies each seed's full RGB identity, and requires
10,000 distinct generated content hashes and manifest objects. Seed verification
and cache publication are untimed setup; no original image rendering occurs.

The SQL overlay changes only `assets.fingerprint`, `render_generation`, and
`preview_hash`. IDs, sequence, metadata text, native location bytes, display path,
state, error, and every organization/metadata/storage table remain in place.
Inside one IMMEDIATE transaction the probe compares all ten asset columns for
all first-10k rows before/after, permits exactly those three changes, checks the
same schema and AUTOINCREMENT state, rechecks 10M count, and requires exactly
20,000 connection total changes: 10,000 asset updates plus 10,000
`storage_asset_change` updates of `storage_epoch.revision`. The epoch must advance
exactly 10,000. Existing organization triggers watch other columns and do not
fire. These bounded checks and known unchanged trigger SQL replace a redundant
scan of all organization tables; they are not an organization-fidelity audit.
Unexpected extra DML, changed columns, keys, count, or schema roll back the overlay.
Receipts retain before/after row, key-map, and schema BLAKE3 identities.

Offline evidence uses the **actual preserved locations**:
`/synthetic/folder{sequence%5}/file{sequence:012}.jpg`. Both the location bytes and
display text must match this formula for every first-10k row, with matching stable
ID, sequence, and ready state. `/synthetic` must be absent; if present, fail rather
than touch its contents. These checks recur on the copied catalog before each
measured child. No unused offline sentinel or user photo path is probed.
This is a disclosed synthetic preview-identity overlay, not an actual import or
proof that the source's synthetic metadata describes the 30 image subjects.

## Fixed measured plan

Use `scripts/preview_integrated_campaign.py run`: exactly two serial measured
children, standard then constrained, each using the existing production
`Catalog::browse` → retained request queue → service decode/read → 200 live caller
views. Each child performs 3 warmups, 100 measured pages, and one hot-LRU page.
Between ordinary pages, drop views and clear the LRU; retain the preceding cache
for the hot page. Each page uses IDs 1–200. Actual source locations remain absent;
no native render jobs are admitted. The already frozen standard/constrained DB,
LRU, live-pixel, encoded-staging, request and worker-reservation limits apply
unchanged. The total catalog stays 10M; the retained cache contains exactly 10k
objects. This does not add fresh-process/navigation repeats to the separate
44-child component experiment or pool their distributions.

The page timer includes DB access, reads, checksum/header validation, allocation,
decode and color materialization until all 200 RGB8 surfaces are held. Full
pixel/key/dimension oracle verification is outside that page timer and separately
reported. Whole-process cumulative RSS high-water includes copied-catalog
count/schema/offline-row preflight, SQLite, caches, all held views, and oracle
work. It is not a per-page incremental allocation or a desktop frame time.
No OS cache purge or cold-storage claim; the count scan itself warms catalog
pages. Two untimed verifier children bind all trial BLAKE3 receipts afterward.
Host telemetry spans measured work and verifier work with explicit child anchors;
APFS Data/storage provenance and competing-load evidence are external frozen
inputs. Do not infer idle from unavailable telemetry.

Keep raw samples and nearest-rank p50/p95/p99/max/n separately for warmup, warm,
and hot for each profile. Report maximum RSS including the terminal child HWM.
Apply only the existing warm first-page p95 ≤1000 ms and browse RSS ≤4 GiB
requirements; failed gates are visible outcomes, not automatic retries. No UI/S12
frame-time award. Both profiles, all 208 page observations, zero native jobs,
actual offline path proof, exact schema/count and source invariance are required
for a complete evidence receipt; completion is distinct from passing budgets.

Each command's binding has version3, catalog_schema=6, clean=true, exact 40-character source revision,
kind `prepare` or `run`, and SHA256 fields for every declared input. Prepare binds
binary/archive/storage/source_binding/dataset/protocol/coordinator and
`minimum_free_bytes`. Run binds binary/worker/archive/storage/fixture/source_binding,
dataset/overlay/copy/preparation/protocol/coordinator/navigation_coordinator,
ancestry_proof/ancestry_native (plus ancestry_verification when present), and
planned_measured_children=2/planned_verifiers=2. The whole archive and exact
compiled binary identities bind shared Rust/native code; a script digest alone
is insufficient. Raw donor files and immutable bound receipts are rechecked at
campaign end. Actual cache/catalog files are owned mutable experimental copies.
Child timeout is 900 seconds (overlay setup 3600, verifier 60); kill/wait only owned
children. Preserve timeout/failure logs and all partial artifacts without retries.

## Focused correctness validation

Tests are source-ready and **UNRUN for this extension** until a native lane is
granted. Tiny real current-schema test catalogs check only-three-field success,
unchanged tail/organization dirty rows, wrong count/schema/key rejection,
extra-row and remaining-column mutation rollback, and actual path/absence checks.
Python byte fixtures check main/companion preservation, wrong source binding,
source mutation/new WAL during copy, exclusive output, binding admission, and
original migration-byte preservation with mutated donor/ancestry rejection.
No test opens an original catalog or allocates a 10M fixture. Existing layout and
navigation correctness suites must remain green before any campaign execution.
