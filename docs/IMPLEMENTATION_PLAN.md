# PhotoCatalog foundation — implementation plan

Status: **Recorded in Shortcut on 2026-09-08: sc-22836 through sc-22848; all 13 stories and 25 dependency relations verified.**
Requirements source of truth: [Shortcut epic sc-22835](https://app.shortcut.com/trefry/epic/22835).
Local requirements file is a review snapshot, not a replacement for Shortcut.

## Plan decisions

The user approved macOS/Windows/Linux, a Rust backend, single-computer catalogs, external originals, editable paths, basic editing, Lightroom import if feasible, and complete retention of existing XMP. SceneWorks integration comes after the foundation.

Catalog selection correction: most discovered Lightroom files are backups or upgraded copies. Import only the most recent/current catalog per logical family, usually a capture year, otherwise a named collection. Group variants for discovery, exclude older copies from default import, and let the user resolve ambiguous suggestions. The pattern {catalogName}-vXX[-X] is a naming hint; the trailing suffix is not a proven revision counter. Filesystem dates and internal schema/change evidence are additional hints, not independent proof. The earlier 2013/2019 inspections establish feasibility, not authority to select those files for real migration.

Verified filename/date exception (2026-09-08): the unsuffixed -v13 member is newer in the paired 2015, 2016, and 2017 families, while 2018-v13-3.lrcat (2026-05-18) is newer than 2018-v13.lrcat (2025-09-25). Do not filter all trailing -X files as backups. Suggest the most recently modified member per family when other evidence agrees and expose contradictions. These dates establish modification order, not proof of the authoritative photo/edit set.

The recorded implementation plan includes these decisions and bounded feasibility work:
- Include TIFF and PSD composite decoding because both exist in the sampled library; do not implement Photoshop layer editing.
- Preserve and report video catalog records; video playback/editing requires a separate scope decision.
- Use JPEG, PNG, and TIFF for the initial batch-export surface.
- Use the performance budgets below for the backend decision and integrated acceptance.
- Choose the final RAW/camera matrix from an inventory and representative files early in sc-22838. Decoding support cannot be inferred from an extension.
- Preserve all Adobe/source metadata and auxiliary evidence. Native Adobe rendering parity is unproven; any unsupported translation remains visible and needs a user decision if it prevents replacement of the user's workflow.
- Use minimal CLI access first; implement the desktop app with Tauri and refine the supplied Fieldbook design with the user at sc-22847. The usable desktop foundation is in this plan, not an untracked follow-up.

## Performance acceptance

These are the recorded acceptance targets, not measured results. Changing a target requires an explicit plan update rather than redefining success after a benchmark. Reference host verified on 2026-09-08: MacBook Pro, M5 Max, 18 CPU cores, 40 GPU cores, 128 GB RAM. Catalog/previews are measured on local SSD; real originals come from the RAID. Synthetic metadata is never presented as proof of real RAW import throughput.

| Operation / resource | Budget and measurement |
| --- | --- |
| Typical indexed metadata page, 200 results | p95 <= 100 ms warm and <= 500 ms fresh-process; fixed folder/date/rating/keyword query corpus at 1M/5M/10M |
| Save one rating or edit recipe | durable acknowledgment p95 <= 100 ms while background import runs |
| First visible page with retained thumbnails | p95 <= 1 second warm; database, preview decode, and UI time reported separately |
| Cached grid scrolling | 60 Hz target; p95 main-thread frame time <= 16.7 ms, with dropped-frame and p99 reporting |
| Browse-only process memory | <= 4 GiB RSS at 10M metadata records; caches and database configured to fit, editing/RAW worker peak measured separately |
| Thumbnail/larger-preview disk footprint | report bytes per asset and projected total; select codec/tier sizes from reviewed quality evidence; larger previews have an enforced configurable budget |
| RAW import / full-resolution editing / export | report throughput and p50/p95/p99 by camera, dimensions, operation and worker count; fix additional operation-specific budgets before final implementation decisions |

Cold OS-cache testing must be separately labeled and controlled; a fresh process is not proof of cold filesystem cache. Record actual available hardware resources, competing load, workload seeds, database/codec versions, durability settings, disk layout, warmup, repetitions, and query correctness. Test a constrained-memory configuration too; 128 GB on the reference host must not hide unbounded memory growth. Large aggregate scans need separate reported latency rather than inheriting point-query targets.

## Ordered slices

Thirteen stories separate independent mechanisms (storage, image processing, XMP, identity/relinking, previews, organization, edits, migration inspection/execution, recovery, UI) and converge in one terminal readiness story. This avoids making every slice responsible for proving the entire epic. Dependencies below are prerequisites; there is no requirement to run independent work as parallel agents.

### sc-22836 — Catalog a real external folder through a usable Rust skeleton

Type: `feature`  
Depends on: none  
Inherits epic requirements: E1, E2, E3, E4, E8, E11

Create a UI-independent Rust core and minimal CLI that imports an explicitly selected fixture folder, stores stable records and previews, and supports paged browsing after restart. A provisional SQLite implementation is an experiment, not the backend decision.

Acceptance (story-local):
- Import representative CR2 and JPEG fixtures, retrieve their metadata and preview by stable identity, and paginate results after restarting the process.
- Repeated or interrupted import resumes without duplicate records or incomplete previews being presented as valid.
- The same fixture workflow builds and runs on macOS, Windows, and Linux; private library fixtures remain outside Git.

Validation: CLI end-to-end fixtures, injected interruption/restart, source hashes, three-platform CI.

### sc-22837 — Select and integrate the catalog backend using representative scale benchmarks

Type: `chore`  
Depends on: sc-22836  
Inherits epic requirements: E1, E2, E11

Compare SQLite and DuckDB with the same logical catalog, realistic skew and relationships, durable write policy, and foreground workload. Integrate the selected backend into the skeleton; retain the comparison harness without imposing a speculative permanent multi-database abstraction.

Acceptance (story-local):
- Reproducible results at 1M, 5M, and 10M asset records cover cold/warm paging, combined filters, aggregates, rating/edit writes during import, memory, disk growth, and interrupted transactions.
- An evidence-backed database decision meets the fixed interaction budgets, or an explicit requirement/architecture decision is escalated before dependent delivery.
- The selected backend persists the skeleton's identities and data without loss, and its query plans avoid unbounded work on ordinary deep-page navigation.

Validation: Versioned generator seed/schema/workload, named hardware and storage, query plans, p50/p95/p99 distributions, exact durability settings and memory limits.

### sc-22838 — Provide broad photo decoding and a validated RAW/color pipeline

Type: `chore`  
Depends on: sc-22836  
Inherits epic requirements: E1, E3, E9

Select and integrate mature decoding, metadata access, camera profiling, and color-management components behind Rust interfaces. Freeze a capability matrix using cameras found in the catalog plus standard format fixtures; include TIFF and PSD composite rendering as the library-driven addition.

Acceptance (story-local):
- CR2, DNG, JPEG, PNG, AVIF, WebP, BMP, TIFF, and PSD composites decode to correctly oriented color-managed images on all three platforms, with explicit statuses for unsupported variants and corrupt inputs.
- The RAW matrix covers representative Canon, Fujifilm, Panasonic, and DJI files identified during discovery, including supported HDR/linear DNG, camera profiles, and higher bit depths; any unavailable capability is brought back for decision.
- Full-quality rendering preserves declared color/alpha/bit-depth behavior and supplies a deterministic input to the editor without depending on a lossy browse thumbnail.

Validation: Private real-camera corpus plus redistributable format fixtures, numerical/reference-image comparisons and visual inspection; dependency/build/license compatibility audit.

### sc-22839 — Retain complete XMP and reconcile metadata sources

Type: `feature`  
Depends on: sc-22837, sc-22838  
Inherits epic requirements: E4, E6, E7, E11

Store original embedded and sidecar XMP packets with source identity, digest, and provenance while indexing useful fields. Represent conflicting source values without losing their originals. Controlled export merges only selected changes.

Acceptance (story-local):
- Import retains original packet bytes and all unknown namespaces, arrays, nested structures, and qualifiers; indexed common metadata remains queryable.
- Conflicting catalog/sidecar/embedded values are visible with source provenance and can be resolved without erasing the retained source versions.
- Explicit metadata export round-trips unrelated XMP semantics unchanged, detects externally changed destinations, and leaves originals/sidecars untouched during ordinary import and editing.

Validation: Adversarial XMP fixtures, original-byte digests, parse/serialize semantic comparisons, cross-source conflict and concurrent-file-change cases.

### sc-22840 — Reconnect external storage and relink folders or individual originals

Type: `feature`  
Depends on: sc-22837  
Inherits epic requirements: E4, E5, E11

Represent unavailable storage and support previewable, atomic, undoable path remapping for root folders, subfolders, and files, including old macOS and Windows path references.

Acceptance (story-local):
- Disconnect/reconnect preserves browsing and organization; return of a known volume reconnects matching originals without creating new identities.
- Proactive and missing-folder relinks show matched, missing, and ambiguous candidates, update descendants by relative path, and support individual exceptions without silently matching same-name different files.
- Interrupted relinks and undo preserve a consistent set of paths, asset identities, variants, and edit associations.

Validation: Fixture volume removal/remount, changed mount names, relocated/reorganized trees, Unicode/case differences, filename collisions, transaction fault injection, real legacy path mappings.

### sc-22841 — Deliver compressed previews with bounded storage and responsive scheduling

Type: `feature`  
Depends on: sc-22837, sc-22838  
Inherits epic requirements: E2, E5, E8, E9, E11

Select preview codec/layout by measured decode latency, quality, and total footprint. Retain an offline thumbnail tier and budget larger previews separately; schedule visible photos ahead of bulk work.

Acceptance (story-local):
- Codec/layout evidence covers JPEG, WebP, and AVIF, with selected thumbnail/preview sizes, quality samples, and measured storage and scrolling costs.
- Configurable cache budgets, cancellable requests, and foreground priority hold resource limits during imports; unavailable-original thumbnails remain usable and disk exhaustion is reported without corrupting catalog state.
- Preview keys include asset revision, edit revision, and rendering version, preventing stale results from overwriting newer edits; interrupted writes are recoverable.

Validation: Real RAW/raster preview set, codec comparison, rapid-navigation race tests, memory/queue bounds, low-disk and source-disconnection cases.

### sc-22842 — Organize and search the library without loading the whole catalog

Type: `feature`  
Depends on: sc-22839  
Inherits epic requirements: E2, E4, E7

Provide durable folder views, hierarchical keywords, ratings, pick/reject flags, color labels, collections, and combined photographic-metadata filters through paged APIs.

Acceptance (story-local):
- All agreed organization operations persist after restart and maintain keyword hierarchy and collection membership through asset relinks.
- Combined text/keyword, date, camera/lens, format, rating, flag, and label queries support stable sorting/pagination under concurrent imports and edits.
- Batch organization changes are atomic or expose a resumable job with an exact completion state, and remain within the interaction budgets.

Validation: Mixed-filter correctness fixtures, tie/cursor-boundary changes, high-cardinality keywords/collections, durable batch-operation tests.

### sc-22843 — Edit non-destructively and export full-quality batches

Type: `feature`  
Depends on: sc-22838, sc-22839, sc-22841  
Inherits epic requirements: E4, E6, E8, E9, E11

Implement the complete agreed basic editing surface as versioned recipes, with variants, persistent undo, adjustment transfer, responsive previews, and full-resolution export. Initial export formats are JPEG, PNG, and TIFF.

Acceptance (story-local):
- Crop/straighten, exposure, white balance, contrast, highlights/shadows, color adjustments, sharpening, and noise reduction work on supported RAW/raster sources; undo and independent variants survive restart.
- Copy/paste adjustments and batch export apply the intended recipe deterministically, preserve selected metadata, and report missing originals or incompatible operations rather than substituting browse previews.
- Exports honor selected output size, format, bit depth, and color profile with cancellation, explicit overwrite handling, and no changes to originals.

Validation: Per-operation and combined recipe fixtures, color/precision reference checks, original hashes, cross-platform export comparisons, cancellation/restart and missing-source cases.

### sc-22844 — Inspect Lightroom catalogs and produce a preservation and migration dry run

Type: `feature`  
Depends on: sc-22839  
Inherits epic requirements: E3, E4, E6, E10, E11

Group candidate files by logical year/named catalog family and suggest one most recent/current member per family for user review, leaving backups and older variants unselected. Read consistent snapshots and relevant companion data from the selected members, detect schema versions, and generate a complete migration plan with source identifiers and preserved opaque data.

Acceptance (story-local):
- Snapshot handling accounts for active writers, WAL/SHM, and .lrcat-data or .acr companions; unsafe/incomplete sources are reported and never silently read as complete.
- Dry runs show the selected current member and excluded older copies for each family, expose ambiguous recency evidence, and enumerate selected files, virtual copies, organization, edits/history/snapshots, missing paths, conflicts, and unsupported constructs with source IDs and reconciled counts.
- Original XMP and source catalog/auxiliary evidence are preserved with digests; the report distinguishes supported semantic imports from retained-only data and unverified Adobe rendering equivalence.

Validation: Synthetic schema/version/WAL and catalog-family fixtures, including misleading suffixes, copied-file timestamps, renamed families, ambiguous candidates, and the observed 2018 case where the suffixed member is newer; read-only snapshots of selected current catalogs. Earlier 2013/2019 samples may be compatibility fixtures without being real migration selections. Verify unchanged sources and count/relation reconciliation. Never execute stored catalog text as code.

### sc-22845 — Import Lightroom organization and independent edit variants with reconciliation

Type: `feature`  
Depends on: sc-22840, sc-22842, sc-22843, sc-22844  
Inherits epic requirements: E4, E5, E6, E7, E9, E10, E11

Execute the approved dry-run selection of one current catalog per family into PhotoCatalog, retaining distinct source records and virtual copies while resolving shared originals and path mappings. Older catalog backups are not merged into the imported history. Translate only settings whose semantics are validated; preserve every remaining selected-source payload and expose the compatibility result.

Acceptance (story-local):
- Selected source counts and relationships reconcile after import: files, metadata, keywords, collections and virtual copies remain accounted for; preserved smart-collection/stack/history constructs are explicitly classified where native behavior differs.
- Reimport or resume is idempotent; excluded backups produce no imported records, and overlap or conflicting variants among the selected current catalogs are handled by stable source identities and explicit decisions.
- Each imported variant exposes original Adobe settings/history and a compatibility status; validated settings are rendered through native recipes, with unsupported settings and appearance differences clearly reported.

Validation: Selected-current-catalog snapshot imports with fresh count/relation reconciliation, excluded-backup assertions, overlap/conflict fixtures, source and XMP hashes, interrupted migration and translated-render checks. The previously observed 40/2,154 virtual-copy counts apply only if those exact 2013/2019 snapshots are used as compatibility fixtures; do not use them as acceptance totals for a different current catalog.

### sc-22846 — Back up and restore catalog state and migration evidence

Type: `feature`  
Depends on: sc-22843, sc-22845  
Inherits epic requirements: E4, E6, E10, E11

Provide consistent backups of irreplaceable state with verified restoration and safe schema upgrades, separately from regenerable previews. A catalog backup does not pretend to back up external originals.

Acceptance (story-local):
- Backup captures identities, metadata, edit/undo/variant state, original XMP packets, provenance, and retained migration evidence consistently while jobs run.
- Restore into a new destination reproduces the preserved state and supports relinking external originals; preview absence triggers safe regeneration.
- Interrupted backup/restore or failed schema upgrades preserve a usable prior state and report actionable failure without promoting incomplete backups.

Validation: Clean-destination restore with logical and blob digest comparisons, concurrent jobs, low-disk faults, partial archives, and upgrade failure fixtures.

### sc-22847 — Expose the foundation through a functional desktop interface on all three platforms

Type: `feature`  
Depends on: sc-22840, sc-22841, sc-22842, sc-22843, sc-22845, sc-22846  
Inherits epic requirements: E1, E2, E3, E5, E6, E7, E8, E9, E10, E11, E12

Package the desktop app with Tauri, as explicitly selected by the user. Validate a small measured grid/editor integration, then expose the full agreed foundation through the supplied Fieldbook design. Fieldbook is only the design-reference label and will not be the product name. Keep PhotoCatalog as the working name until the user selects the product name; do not adopt Fieldbook branding or its .fbcat extension from the prototype. Keep catalog and image-processing logic in the UI-independent Rust core, with a bounded command/data boundary to the frontend. Refine UI choices with the user; frontend component framework and final styling remain open, and SceneWorks integration remains outside this foundation.

Acceptance (story-local):
- Installable Tauri desktop packages on macOS, Windows, and Linux expose catalog/import, virtualized browsing, search/organization, offline status, relink review/undo, editing/export, Lightroom dry-run/import, metadata conflicts, and backup/restore.
- Visible-photo scheduling, keyboard navigation, cancellation/progress, and a color-managed image viewport keep foreground operations within the fixed interaction budgets.
- Errors, retained-only Adobe settings, missing originals, and destructive/export choices are understandable in the relevant user flow; selected UI design and platform packaging are validated with the user.

Validation: Build/package, install, launch, and end-to-end desktop workflows on all three platforms, reference-Mac frame/interaction traces, accessibility/keyboard checks and reviewed screenshots. Final visual-design refinements require explicit agreed scope.

### sc-22848 — Verify integrated foundation readiness and reconcile delivery

Type: `chore`  
Depends on: sc-22847  
Inherits epic requirements: E1, E2, E3, E4, E5, E6, E7, E8, E9, E10, E11, E12

Own the one terminal integrated acceptance run after the backend, rendering contract, and desktop interface are stationary. Reconcile the epic and every delivered slice with merged code and evidence.

Acceptance (story-local):
- The epic's complete external-storage import/browse/edit/export/offline/relink/restart/restore scenario passes on the reference Mac, with platform-specific integrated checks on Windows and Linux.
- Terminal scale and real-image measurements satisfy the fixed budgets, and Lightroom/XMP reconciliation proves retained source information and reports every approved compatibility limitation.
- Merged revisions, terminal CI, reproducible artifacts, and live Shortcut read-back agree; unresolved agreed requirements keep the affected story and epic open.

Validation: One integrated readiness report referencing local story evidence, exact merged revisions, hardware/configuration, performance distributions, migration reconciliation, restore digests, and tracker read-back.

## Coverage

| Epic requirement | Owning slices |
| --- | --- |
| E1 | sc-22836, sc-22837, sc-22838, sc-22847; sc-22848 terminal proof |
| E2 | sc-22836, sc-22837, sc-22841, sc-22842, sc-22847; sc-22848 terminal proof |
| E3 | sc-22836, sc-22838, sc-22844, sc-22847; sc-22848 terminal proof |
| E4 | sc-22836, sc-22839, sc-22840, sc-22842, sc-22843, sc-22844, sc-22845, sc-22846; sc-22848 terminal proof |
| E5 | sc-22840, sc-22841, sc-22845, sc-22847; sc-22848 terminal proof |
| E6 | sc-22839, sc-22843, sc-22844, sc-22845, sc-22846, sc-22847; sc-22848 terminal proof |
| E7 | sc-22839, sc-22842, sc-22845, sc-22847; sc-22848 terminal proof |
| E8 | sc-22836, sc-22841, sc-22843, sc-22847; sc-22848 terminal proof |
| E9 | sc-22838, sc-22841, sc-22843, sc-22845, sc-22847; sc-22848 terminal proof |
| E10 | sc-22844, sc-22845, sc-22846, sc-22847; sc-22848 terminal proof |
| E11 | sc-22836, sc-22837, sc-22839, sc-22840, sc-22841, sc-22843, sc-22844, sc-22845, sc-22846, sc-22847; sc-22848 terminal proof |
| E12 | sc-22847; sc-22848 terminal proof |

## Shortcut write and execution contract

All 13 stories are recorded under the PhotoCatalog team in the Standard workflow, initially To Do, with three local acceptance criteria each. The 25 prerequisite relations have been read back and verified in the correct direction. Shortcut is the operational source of truth; this document is a local snapshot. Revalidate each live story and repository state before execution, and keep findings reflected in Shortcut. The first executable slice is sc-22836.

The private repository is https://github.com/michaeltrefry/PhotoCatalog, with main as its default branch. Three-platform Rust CI is part of sc-22836. Actual terminal checks and remote merge must be verified before claiming delivery; local validation is not merged delivery.
