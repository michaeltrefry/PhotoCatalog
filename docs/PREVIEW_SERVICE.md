# Preview service and retained storage

This source checkpoint joins the selected 512/1600 JPEG80 pair to actual isolated
workers, catalog revision guards, durable jobs and cache relocation. The Mac gate
passes 228 Rust tests and 11 Python preview-contract tests, with package formatting
and all-target Clippy clean. Stage B layout/timing, full-worker peak-memory
qualification, current-renderer visual qualification and cross-platform native
CI remain required before S6 is complete. The `ServiceLimits::default()` values
are provisional admission settings for fixture execution, not a measured product
memory profile; Stage B freezes worker reservation and its accounting margin.

The subsequent retained-read queue is source-ready and unrun. `queue_read`,
`tick_read`, `take_read` and `cancel_read` are service APIs, sharing request and
unconsumed-completion admission with native render consumers. Read tickets have a
separate Rust type/namespace and monotonic service-lifetime IDs. Foreground reads
precede background reads; one owner iteration performs at most one retained
decode, with catalog generation checked before entering it. Missing cache data,
stale requests and errors produce consumable outcomes. Canceling queued or
unconsumed results releases service ownership, while caller-held pixel references
remain charged to their live memory budget. Cancellation is cooperative between
in-process retained decodes; the existing original-render process kill/wait
mechanism remains separate. These APIs will drive the fixed headless navigation
trace; a harness-only queue cannot substitute for them.
Both encoded staging refusal and decoded-live refusal have typed errors. Read
completion classifies either as transient resource pressure; no message matching
or cache invalidation is used for admission failures. The constrained navigation
receipt must keep resource-refused reads separate from corrupt/I/O failures and
missing/stale results. A held encoded export, refused read, released export and
successful subsequent read are covered by a source-ready regression.

`Catalog::import_with_previews` drives the application service. `begin_import`
and `ImportSession::advance` expose the same discovery/metadata/reservation path
one directory entry at a time, queuing full-image rendering instead of waiting
for it. An application actor interleaves advances with foreground requests and
service ticks. The synchronous wrapper requires a drained service before it
starts, then drains its own required jobs before reporting completion. It rejects
caller-owned outstanding work with an actionable error; the incremental actor
API retains foreground/import interleaving. `Catalog::import` retains the explicitly documented compatibility
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
If a resume batch fails after admitting earlier jobs, it cancels only those new
in-memory admissions and retains their durable journals for retry. No unreturned
consumer continues running. A canceled worker's lease cannot delete a newer
same-key request or its journal when the old process is reaped.

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
empty destination. An interrupted marker-before-journal admission can be retried
only when the target carries this manifest/tier/layout identity and contains
solely its admission markers. Foreign files or another manifest's marker are
preserved and rejected. Original roots, tier roots and the manifest cannot overlap.
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

Local validation covers actual worker imports/offline reads, durable
resource refusal and retry with a legacy fallback, pixel-generation rejection,
nonpixel rating edits, foreground preemption during incremental import, relocation
restart before/after the location switch, and actual owner-process exits at four
cross-database boundaries. Named short observer callbacks expose manifest attach,
before catalog commit, after catalog commit and before journal removal; default
execution installs no callback. These correctness results do not award measured
performance or cross-platform native acceptance.
The six-finding review repair also adds canceled/same-key resubmission, occupied
synchronous import, mixed valid/foreign-path resume, interrupted relocation
admission, all root-overlap pairs, and deferred-constraint COMMIT failure/reuse
regressions. All passed in the same local gate; logs and initial compile/fixture
failures are retained privately in `sc-22841-service-repair-v1`. The runtime probe
was also exercised through a tiny generated DNG and its saved JPEG verifier,
including same-size byte corruption rejection. No Stage B campaign has run.
