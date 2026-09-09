# Preview service and retained storage

This source checkpoint joins the selected 512/1600 JPEG80 pair to actual isolated
workers, catalog revision guards, durable jobs and cache relocation. Its service
integration tests are not yet run. Stage B layout/timing, full-worker peak-memory
qualification, current-renderer visual qualification and cross-platform native
CI remain required before S6 is complete. The `ServiceLimits::default()` values
are provisional admission settings for fixture execution, not a measured product
memory profile; Stage B freezes worker reservation and its accounting margin.

`Catalog::import_with_previews` drives the application service. `begin_import`
and `ImportSession::advance` expose the same discovery/metadata/reservation path
one directory entry at a time, queuing full-image rendering instead of waiting
for it. An application actor interleaves advances with foreground requests and
service ticks. The synchronous wrapper drains required jobs before reporting
completion. `Catalog::import` retains the explicitly documented compatibility
thumbnail path for existing library callers. CLI import and preview commands use
the configured service, including complete-original rendering for RAW.

CLI import, preview and cache commands take `--preview-config <JSON>`, containing
`PreviewConfiguration { store, policy, limits, original_roots }`. Settings are
explicit while the layout and memory profile are under measurement. Native worker
launch uses the actual application executable; library callers inject an absolute
worker executable. Both retained/evictable locations and byte quotas are
configurable. `cache-jobs` exposes queued, resource-limited, unavailable and failed
jobs; `cache-resume` performs bounded restart, with `--retry-blocked` for deliberate
retry after admission/storage changes. `cache-budgets` persists quota changes in
the preview manifest. It never evicts the last retained thumbnail to shrink a quota.

A job key binds asset/variant, authoritative source generation and fingerprint,
pixel-recipe revision, actual renderer/preparation identity, tier, dimensions and
codec settings. The current recipe revision is zero; S8 must supply the actual
recipe and its pixel identity together when it adds rendering edits. Ordinary
organization-only metadata revisions do not invalidate these keys. General
metadata transitions retain the existing conservative generation invalidation.

A worker result is fully decoded and validated before publication. File staging
and hashing occur outside the catalog writer guard. The catalog IMMEDIATE
transaction then rechecks pixel identity and executes the manifest attachment;
for imports it updates ready metadata/fingerprint/reference and organization
projections before committing. These are two database commits, not one atomic
cross-database transaction. The journal remains until the catalog commit succeeds.
An attached immutable render record includes its own metadata/provenance. Startup
can finish manifest-attached/catalog-pending imports without rereading originals
or rendering again. A catalog-ready/journal-present interruption only needs
journal removal. Current reads still validate source generation and key.

Each pending/failed replacement preserves its prior catalog preview reference.
Existing compatibility thumbnails remain explicitly stale fallbacks without
invented full-render provenance. A low configured decode ceiling or exhausted
quota leaves an actionable retryable job and retained thumbnail. Decode-cache
memory pressure does not invalidate a correct encoded object. Caller-owned
pixel references and encoded exports retain their memory reservations until they
are dropped. Native working reservations are accounting admissions; they do not
claim an aggregate native allocator or OS RSS enforcement.

`cache-relocate-begin` requires drained active/queued rendering and a separate,
empty destination. Original roots, tier roots and the manifest cannot overlap.
Both tier directories have process locks with a manifest UUID/tier/layout marker;
the destination is additionally bound to the durable relocation job. Bounded
`cache-relocate-step` calls copy using fixed-size streaming buffers and verify
length/checksum. Copy-phase reads keep using the old root. Only after every ready
object is copied does one manifest transaction switch the authoritative location
and enter cleanup. Source ownership stays locked until verified cleanup finishes.
Restart loads authoritative locations from the manifest even if the application
settings file still names the old root. Errors preserve progress, source files and
readable copies for explicit retry; cleanup does not recursively remove foreign
files or directories. Duplicate copies and filesystem/marker overhead are
separate from the encoded-object quota and must be included in disk reporting.

Source-ready validation covers actual worker imports/offline reads, durable
resource refusal and retry with a legacy fallback, pixel-generation rejection,
nonpixel rating edits, foreground preemption during incremental import, relocation
restart before/after the location switch, and actual owner-process exits at four
cross-database boundaries. Named short observer callbacks expose manifest attach,
before catalog commit, after catalog commit and before journal removal; default
execution installs no callback. This is an evidence plan until those tests run.
