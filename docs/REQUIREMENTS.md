# PhotoCatalog — requirements snapshot

Authoritative record: [Shortcut epic sc-22835](https://app.shortcut.com/trefry/epic/22835).

## Objective
Build PhotoCatalog as a responsive desktop photo catalog and basic non-destructive editor for macOS, Windows, and Linux. A Rust backend manages millions of photo records and highly compressed previews while originals remain on the filesystem, including external storage. One computer uses a catalog at a time. macOS is the primary development and performance reference platform.

## Status and authorization
Requirements baseline agreed in the planning conversation on 2026-09-08, including Lightroom catalog import if feasible and retention of all existing XMP data. This epic records that agreed baseline under the user's requirements workflow. The revised implementation plan was authorized for story creation in the subsequent planning conversation. Stories sc-22836 through sc-22848 and 25 dependency relations are now recorded and read-back verified. Numeric budgets below are acceptance targets, not measured results. No migration of the user's live library has been performed.

## Requirements
E1. Cross-platform Rust core: support macOS, Windows, and Linux with a UI-independent backend and cross-platform validation from the beginning. Established native image libraries may be evaluated behind bounded Rust interfaces. The desktop app will use Tauri, as selected by the user on 2026-09-08. Keep the Rust core independent of the desktop shell; use the supplied Fieldbook handoff as the UI design reference. Fieldbook is only the design-reference label and will not be the product name. The product name is undecided; PhotoCatalog remains a working project/repository name. Frontend component framework and final visual refinements remain open.
E2. Scale and responsiveness: catalog millions of photos with bounded memory, incremental queries, cancellable and resumable work, and priority for foreground interaction while importing or generating previews. Compare SQLite and DuckDB on representative interactive, write, search, and aggregate workloads before selecting the production backend.
E3. Format support: CR2, DNG, JPG/JPEG, PNG, AVIF, WebP, and BMP are explicitly required, with broad RAW/photo support validated by camera and format rather than extension alone. The recorded plan includes TIFF and PSD composite decoding and evaluation of additional camera formats present in the real library; preserve and report every migration record even where decoding is unsupported.
E4. Originals and identity: originals remain unchanged on the filesystem by default. Stable asset identities and distinct edit-variant identities must survive path changes. Storage volume identity and original file location are replaceable references, not photo identity.
E5. Relinking and offline use: browse retained previews and organize metadata while originals are unavailable. Detect unavailable volumes/folders, reconnect when possible, and permit proactive or missing-folder root repointing, recursive relative-path updates, subfolder and individual-photo relinking. Preview matches, mismatches, and ambiguities before applying changes. Apply relinks atomically with undo; never silently attach edits to the wrong photo. Repointing does not move source files.
E6. Metadata fidelity: retain all existing XMP data from sidecars and embedded packets, including unknown namespaces, structures, qualifiers, arrays, and Adobe develop properties. Preserve original packets and provenance alongside indexed fields. Import must not rewrite source XMP or originals. Conflicts between catalog, sidecar, and embedded metadata must remain observable without silent overwrites. Any later metadata write/export must preserve unrelated properties and be explicitly controlled.
E7. Organization: folders, hierarchical keywords, ratings, flags, color labels, collections, and filtering by photographic metadata. Preserve meaningful imported distinctions and source provenance rather than flattening them.
E8. Preview storage: highly compressed persistent thumbnails for offline browsing, separately budgeted larger previews, configurable storage locations, edit-aware invalidation, orientation and color correctness, and recovery from interrupted writes. Choose codec and layout by measured size, quality, encode/decode cost, and browsing performance. Proposed default is local SSD catalog/previews, external originals.
E9. Basic editing: non-destructive crop/straighten, exposure, white balance, tonal and color adjustments, sharpening/noise reduction, undo, copying adjustments between photos, and batch export. Use versioned edit recipes and render final exports from originals. Evaluate RAW development and color management on representative cameras, profiles, orientations, and bit depths. Masks/healing were not added to the agreed native editing baseline.
E10. Lightroom migration: import only the most recent/current catalog for each logical catalog family (usually the year the photos were taken, otherwise its named collection), not every backup or upgraded copy. Group older variants for discovery and exclude them from default import; the user can override the suggested current member where evidence is ambiguous. The user's filenames commonly follow {catalogName}-vXX[-X], but the optional trailing suffix has no confirmed universal meaning. Use filename version hints, filesystem dates, and available internal catalog/version/change evidence to suggest a current catalog; do not treat any one of these as proof or silently merge variants. Preserve metadata, organization, virtual copies, and source edit/history information from selected catalogs with traceable provenance. Discover schema versions, companion data, missing references, and conflicts among selected current catalogs. Require a dry-run report and consistent snapshot handling, including WAL and auxiliary data. Preserve unsupported data and explicitly report semantic/rendering gaps; retaining Adobe instructions is not proof of matching Adobe rendering.
E11. Durability: idempotent/resumable ingestion and migration, atomic durable metadata/edit updates, interrupted-job recovery, schema migration safeguards, and verified backup/restore for irreplaceable catalog metadata, edit recipes, original XMP packets, and imported source evidence. Rebuildable previews are managed separately.
E12. Foundation and integration boundaries: establish the foundation before final UI design and feature expansion. SceneWorks integration is deferred at the user's request. No cloud/multi-computer coordination is required. Source modification, moving originals, and rewriting existing Lightroom catalogs are not part of migration.

## Epic acceptance
- On the reference Mac, exercise import from external storage, browse/filter/organize, edit/export, restart, disconnect/reconnect, and folder/file relink without losing identity, metadata, variants, or edits.
- Compare SQLite and DuckDB using equivalent durability and realistic metadata distributions at 1M/5M/10M scales, distinguishing synthetic metadata from real image processing. Apply the recorded performance budgets before interpreting benchmark outcomes; include cold/warm reads, deep paging, mixed import/edit traffic, memory, storage, preview cost, and crash recovery.
- Validate the format/camera matrix with actual decoding, previews, metadata fidelity, and export evidence rather than extension detection.
- Prove Lightroom migration against a consistent source snapshot: counts and relations reconcile, virtual copies remain distinct, unknown XMP is retained, repeated import is safe, unsupported constructs and conflicts are reported, and source files remain unchanged.
- Restore a backup into a clean destination and verify identities, metadata, edits, imported evidence, and relinking. Cross-platform checks pass; release claims require merged implementation and terminal validation.
- Any genuine feasibility gap in agreed scope must be presented for a user decision; it cannot be silently declared complete.

## Execution order
Stories sc-22836–sc-22848 are recorded under the PhotoCatalog team, initially To Do. Start with sc-22836 (usable Rust catalog skeleton). Backend selection and image processing follow it; metadata, relinking, previews, organization, editing, migration and recovery converge in the functional desktop story sc-22847, then the sole terminal readiness story sc-22848. Full slice criteria, dependency direction, performance targets and coverage are below; the live stories are the execution source of truth.

## Verified reference environment and discovery
Read-only inspection on 2026-09-08:
- Reference Mac: MacBook Pro, Apple M5 Max, 18 CPU cores, 40 GPU cores, 128 GB memory.
- Existing Lightroom catalogs: /Volumes/MichaelJon/Catalogs.
- Originals: /Volumes/MichaelJon/Raw on the user's nominal 16 TB external RAID.
- Bounded catalog inventory found 48 .lrcat files, excluding Backups, Old Lightroom Catalogs, package contents, and directories deeper than three levels. This is not 48 independent authoritative catalogs. The user clarified that most are backups: select only the current catalog per year/named family. Earlier inspections establish format feasibility, not which catalog is authoritative.
- Every inventoried .lrcat had the SQLite header. 2016-v13.lrcat had a WAL companion and was not queried.
- Two catalogs without WAL were inspected using SQLite mode=ro&immutable=1. Source size/mtime were checked around schema inspection. This is discovery evidence, not a production snapshot strategy.
- 2013-v10.lrcat: 12,356 image records, 12,316 file records, 40 virtual copies, 53,090 develop history steps. Root references include an older macOS volume and a Windows drive path.
- 2019-v13.lrcat: 14,599 image records, 12,445 file records, 2,154 virtual copies, 24,474 develop history steps.
- Bounded original-file sample: 4,915 files in 45 directories, concentrated in 2013; includes CR2, DNG, JPEG, TIFF, PSD, XMP and a small number of video files. It is not a total-library census.
- Two XMP packets parsed successfully; each had 156 distinct element/attribute names and Adobe Camera Raw properties.
- The recorded plan includes TIFF/PSD composite decoding, JPEG/PNG/TIFF export and numeric performance targets. Video behavior beyond record preservation, final RAW matrix and Adobe rendering compatibility remain explicit decisions in the scoped feasibility/UI stories.

- Filename/date comparison on 2026-09-08: the unsuffixed -v13 catalog is newer by filesystem modification time for the paired 2015, 2016, and 2017 families, but 2018 is the reverse (2018-v13-3.lrcat: 2026-05-18; 2018-v13.lrcat: 2025-09-25). Generic Lightroom Catalog variants share the 2024-11-20 date with differing times, which does not independently establish authority. Treat trailing -X as neither a universal backup marker nor a recency ordering. Suggest the most recently modified member per family when other evidence agrees; flag disagreement for review. These are candidate-selection observations, not authorization to import.

## Sources
- Adobe metadata/XMP storage: https://helpx.adobe.com/lightroom-classic/desktop/organize-photos-in-lightroom-classic/metadata-basics-actions.html
- Adobe companion catalog data: https://helpx.adobe.com/sg/lightroom-classic/kb/catalog-faq-lightroom.html
- XMP namespaces: https://developer.adobe.com/xmp/docs/xmp-namespaces/
- DuckDB workload guidance: https://duckdb.org/docs/current/guides/performance/how_to_tune_workloads
- SQLite WAL: https://www.sqlite.org/wal.html

## Out of scope
SceneWorks integration and additional feature ideas are postponed by the user until the foundation is settled. Multi-computer concurrent catalog use is unnecessary. Photoshop layer editing, video editing, and Adobe renderer parity are not assumed capabilities; existing source data must still be preserved and compatibility gaps surfaced.


## Recorded story index

| Story | Type | Outcome | Depends on | AC count |
| --- | --- | --- | --- | --- |
| [sc-22836](https://app.shortcut.com/trefry/story/22836) | feature | Catalog a real external folder through a usable Rust skeleton | none | 3 |
| [sc-22837](https://app.shortcut.com/trefry/story/22837) | chore | Select and integrate the catalog backend using representative scale benchmarks | sc-22836 | 3 |
| [sc-22838](https://app.shortcut.com/trefry/story/22838) | chore | Provide broad photo decoding and a validated RAW/color pipeline | sc-22836 | 3 |
| [sc-22839](https://app.shortcut.com/trefry/story/22839) | feature | Retain complete XMP and reconcile metadata sources | sc-22837, sc-22838 | 3 |
| [sc-22840](https://app.shortcut.com/trefry/story/22840) | feature | Reconnect external storage and relink folders or individual originals | sc-22837 | 3 |
| [sc-22841](https://app.shortcut.com/trefry/story/22841) | feature | Deliver compressed previews with bounded storage and responsive scheduling | sc-22837, sc-22838 | 3 |
| [sc-22842](https://app.shortcut.com/trefry/story/22842) | feature | Organize and search the library without loading the whole catalog | sc-22839 | 3 |
| [sc-22843](https://app.shortcut.com/trefry/story/22843) | feature | Edit non-destructively and export full-quality batches | sc-22838, sc-22839, sc-22841 | 3 |
| [sc-22844](https://app.shortcut.com/trefry/story/22844) | feature | Inspect Lightroom catalogs and produce a preservation and migration dry run | sc-22839 | 3 |
| [sc-22845](https://app.shortcut.com/trefry/story/22845) | feature | Import Lightroom organization and independent edit variants with reconciliation | sc-22840, sc-22842, sc-22843, sc-22844 | 3 |
| [sc-22846](https://app.shortcut.com/trefry/story/22846) | feature | Back up and restore catalog state and migration evidence | sc-22843, sc-22845 | 3 |
| [sc-22847](https://app.shortcut.com/trefry/story/22847) | feature | Expose the foundation through a functional desktop interface on all three platforms | sc-22840, sc-22841, sc-22842, sc-22843, sc-22845, sc-22846 | 3 |
| [sc-22848](https://app.shortcut.com/trefry/story/22848) | chore | Verify integrated foundation readiness and reconcile delivery | sc-22847 | 3 |
