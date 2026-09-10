# Epic sc-22835 execution ledger

Source of truth: https://app.shortcut.com/trefry/epic/22835

## Current wave

sc-22836–sc-22842 are verified Done. S8 editing/export implementation and qualification are in progress. S9 is inspecting Lightroom catalogs through bounded, resumable, read-only source passes. Native builds and large-I/O/measurement work use one owned lane. Live Shortcut remains authoritative; this checkpoint was reconciled on 2026-09-10.

| Story | State | Work surface / artifact | Evidence | Next action |
| --- | --- | --- | --- | --- |
| sc-22836 | Done | [PR #1](https://github.com/michaeltrefry/PhotoCatalog/pull/1), merged 56f0b37 | Independent review PASS; 18 local tests; real CR2/JPEG source invariance; merged-main three-platform CI 34227667817 SUCCESS; Shortcut read-back | Complete |
| sc-22837 | Done | [PR #3](https://github.com/michaeltrefry/PhotoCatalog/pull/3), merged f353f3d; [decision](BACKEND_DECISION.md) | Reviewed head 7bb3930 and merge have identical trees; PR CI 34362927300 and main CI 34366395501 all four jobs SUCCESS; Shortcut Done read-back, comment 22896 | Complete |
| sc-22838 | Done | [PR #2](https://github.com/michaeltrefry/PhotoCatalog/pull/2) and corrective [PR #5](https://github.com/michaeltrefry/PhotoCatalog/pull/5), merged bef8b6c | Final renderer review; 115 tests; private camera/render comparisons; PR CI 34386168135 and main CI 34390636314 all four jobs SUCCESS; Shortcut read-back | Complete |
| sc-22839 | Done | [PR #4](https://github.com/michaeltrefry/PhotoCatalog/pull/4), merged be85c16 | SDK normalization repaired; independent RDF/opaque XMP preservation checks; PR CI 34380874912 and main CI 34382746592 all four jobs SUCCESS; Shortcut Done read-back | Complete |
| sc-22840 | Done | [PR #6](https://github.com/michaeltrefry/PhotoCatalog/pull/6), merged 60fc33c | Controlled APFS detach/remount/reorganization/replacement/undo; Linux bind mount and Windows volume GUID/junction proof; PR CI 34394610454 and main CI 34395883109 all four jobs SUCCESS; Shortcut Done read-back | Complete |
| sc-22841 | Done | [PR #8](https://github.com/michaeltrefry/PhotoCatalog/pull/8), merged789a39d | Reviewed preview defaults and full30-file memory/quality, layout, navigation and integrated10M evidence; PR CI34430709720 and main CI34432335579 SUCCESS; Shortcut Done read-back | Complete |
| sc-22842 | Done | [PR #7](https://github.com/michaeltrefry/PhotoCatalog/pull/7), mergede68d375 | [Mac qualification report](ORGANIZATION_PERFORMANCE_RESULTS.md); reviewed scale/actual-overlap evidence; main CI34418548263 SUCCESS; Shortcut Done read-back | Complete |
| sc-22843 | In Progress | codex/sc-22843-edits; persistent recipes/variants, copy jobs, edited previews, exact batch export |134 focused tests passed at7a97b8b (120 library,14 integration); actual crash/restoration/cache-hit evidence; strict all-target Clippy at8f8bc58. These are correctness evidence only | Complete retained decode-source guard, full corpus/100MP/performance qualification, independent review and batched PR/three-platform CI/merge |
| sc-22844 | In Progress | codex/sc-22844-page-memory; frozen v5 main inspection | Recovery adoption and three main slices independently verified: ten completed members/6,619,809 rows; eleventh677,581 retained/readback partial; latest988 command results pass and observed processes reaped | Continue bounded main slices; independently admit auxiliary/path/packet phases and current-family ambiguity review before migration |
| sc-22845 | To Do | Lightroom migration | Prerequisites sc-22840/sc-22842/sc-22843/sc-22844 | Dependency-bound |
| sc-22846 | To Do | Backup and restore | Prerequisites sc-22843/sc-22845 | Dependency-bound |
| sc-22847 | To Do | Desktop UI | Prerequisites sc-22840/sc-22841/sc-22842/sc-22843/sc-22845/sc-22846 | Dependency-bound |
| sc-22848 | To Do | Integrated readiness | Prerequisite sc-22847 | Terminal proof only after integration |

## Resource and authorization ledger

- User authorized epic delivery and ordinary PR/CI/merge. The user changed michaeltrefry/PhotoCatalog to public because private-repository Actions consumed the monthly allowance; preserve public visibility and batch validated changes before CI. The user released the reference Mac on 2026-09-09.
- Originals and Lightroom sources on the RAID remain read-only. Private fixtures and evidence remain outside Git. No live migration or original mutation has occurred.
- All S2 measurement sessions are complete. Local builds/renders were paused during timing. The owned passive observer PID 30547/session 93348 was stopped afterward and exited successfully; its final receipt at 2026-09-09T14:00:27Z records SIGTERM and 6,472 samples. No outstanding S2 process needs resuming.
- S4 and S6 use isolated worktrees. Parent owns catalog schema/model integration; packet extraction and explicit export own separate modules. S6 owns preview adapters/store/scheduler; its rebuildable manifest has separate schema ownership. Heavy Cargo and timed measurements are serialized.
- Bound Cargo builds to four jobs and serialize heavy measurement/render lanes. Native Windows/Linux behavior requires hosted CI; local Mac tests alone do not establish it.
- Preserve user ZIPs and unrelated root-worktree files. No CodeGraph tools/index or root CODEGRAPH.md was available.

## S2 evidence and closeout

The frozen original harness SHA-256 is `167b13d524c0f21a18426bb8b21acc6792f275de7884edca1cf995c6ff874c89`; supplemental driver is `1c509cf8a06ce2dba734ef71ca4aecd6b4a50d1b9eac513ab9cc9ee2a8e6f5eb`. Both files remain unchanged. The frozen original native binary remains separately retained at `sc-22837-frozen-v2-native/catalog_probe`, SHA-256 `cfbe4dcce0f3f176af369e1c685f40bce45d5ae77c259603ab6d78948b6084a6`.

All private paths below are under `/Users/michael/PhotoCatalog-private-results/`:

| Evidence | Status / review |
| --- | --- |
| `sc-22837-final-v2` | Complete original campaign; 1,026 distributions, all load proofs, paired hashes, 48 plans and recovery independently reconciled. Default 256 MiB failures retained. |
| `sc-22837-production-profiles` | Complete predeclared profiles; 2,475 distributions reviewed. DuckDB 1024 MiB numerical PASS; no original SQLite profile qualifies. |
| `sc-22837-query-work-v3` | Independent full-record/counter/provenance review PASS. SQLite correction constant 2,410/2,413 VM steps; DuckDB scans grow. Failed diagnostics retained. |
| `sc-22837-query-work-pristine-v3/derivation.json` | Six verified APFS standalone clones; main-file hashes and original main/companion state retained unchanged. No WAL/SHM removal. |
| `sc-22837-sqlite-page-correction-v1` | Complete once, all scales PASS. Review reconciles 432 children and 495 distributions. 10M worst warm/fresh/write p95 0.695/1.587/20.813 ms; warm RSS 325 MiB. |
| `sc-22837-native-query-work-v1` | Review PASS: bundled SQLite 3.51.1, 36 queries/7,200 full records; same constant candidate VM work, hashes and source preservation. |
| `sc-22837-native-runtime-v1` | Complete once, all scales PASS; reviewed 6 children, 15 distributions/2,400 samples. 10M browse/rating/edit p95 0.129/86.948/87.238 ms. Two edit samples exceed 100 ms; p99 105.234/max 116.937 ms retained. |
| `sc-22837-measurement-20260909-host.jsonl` | Complete ordinary desktop CPU/RAM/GPU/I/O observation stream, with explicit final shutdown. |

Native runtime source is 1ac4737 (code introduced at ef53d25), binary SHA-256 `ab76f6d00358f4ad45e4f833bfd2e124b73cd74b9733d6fece6d02d101f3c8f8`. Its build reference binds Rust 1.98.0, lockfile, source and binary. Later documentation/comment changes do not change the measured execution path. Five native query-work tests plus new configuration/preservation/browse-only tests are included in the 40-test Rust suite. The 39 Python tests include real candidate child/recovery dispatch and full DuckDB diagnostic setup.

Full original/profile/corrected tables are in [baseline results](BASELINE_BENCHMARK_RESULTS.md) and [profile results](PRODUCTION_PROFILE_RESULTS.md). The [decision](BACKEND_DECISION.md) records the selected configuration, original adverse evidence, native contention differences, cold-cache limitations, and integration scope. No S2 result is evidence for RAW/preview/UI performance. Do not repeat a completed campaign to improve a result.

## S3 retained evidence

PR #2 reviewed head 2d30647 and merged a0bf374 have identical trees. Private corpus `sc-22838-e6a4545-corpus/receipt.json` and `verification.json` retain 22 fixtures/44 deterministic renders, numeric and visual color/HDR checks, and camera/format provenance. Two DNG fixtures derive from JPEG/TIFF and are explicitly not native-camera evidence. AVIF orientation, PSD transparency/missing-composite, and DNG crop/profile/spatial calibration repairs passed review and the corpus. This backend integration does not change the renderer; do not repeat the private corpus without a relevant change or unresolved concern.

## Foundation risk register

Incomplete previews after interruption; duplicate asset creation on retry; source changes during extraction; generated fixtures falsely standing in for real CR2 compatibility; private images or metadata being committed; unbounded directory/file reads; platform differences in path and file publication semantics. Validate these within sc-22836's scope before closeout.

## S7 qualification checkpoint — 2026-09-09

The [organization report](ORGANIZATION_PERFORMANCE_RESULTS.md) records the final
v4 query and actual-overlap transition PASS at runtime source `0cfc1fd`. The v1/v2
failures and rejected v3 mixed summary remain retained. Worst warm/fresh page p95
was 60.295/61.871 ms; browse RSS peaked at 480.781 MiB. All 200 saves overlapped
background activity at every scale, with p95 12.788/12.161/11.543 ms. Final CI and
PR/merged-head verification remain required; this checkpoint is not a Done claim.

## S8 implementation checkpoint — 2026-09-10

The Rust core implements versioned complete basic recipes, independent variants,
persistent undo/redo, bounded copy-adjustment jobs, and variant-aware retained and
interactive previews. Full-original JPEG8, PNG8/16 and TIFF8/16/float32 export uses
immutable plans, selected metadata, explicit overwrite approval, a process lease,
read-back seals and guarded durable publication. Actual process tests demonstrate
cancel/reap, preview preemption, undo ABA rejection and owner restart. An orphan
seal is never published just because it exists: a resumed worker rerenders the
original and requires full byte equality before reuse.

The focused gate at `7a97b8b` passed 120 library tests and 14 integration tests
(organization3, actual export service5, publication2, edited previews4).
Strict all-target Clippy passed at `8f8bc58`. Preserved logs include the earlier
error-context assertion, private fixture-helper compile error and lint failures.
These results establish focused correctness, not S8 performance or platform
qualification.

Publication now performs full verification outside the catalog writer, retains
held-file identities/change stamps for short guarded namespace steps, and commits
intent before capture/link. Tests cover interrupted and repeated restoration,
actual crash after publication, cancellation and later offline originals. Warm
preview records identify the actual checksum-validated prepared input consumed;
corruption is an explicit cache miss. An independent review identified a further
source-consistency gap across native export decode (temporary rewrite followed by
restored original bytes); a retained identity/change-stamp guard is being added.
The prospective S8 targets remain unchanged. The serial qualification harness
must freeze exact source/cohort/oracles/settings/resources before execution.

## S9 v5 recovery checkpoint — 2026-09-10

The failed v4 memory attempt remains preserved. V5 used one verified owned-copy
adoption retaining5,311,732 rows; sampled Python peak412,172,288 bytes was below
the unchanged512MiB admission. Three main slices now leave ten completed members
and6,619,809 rows. The eleventh retains677,581 rows;151 readback pages end at
cursor6,770,017. Third slice passed988 command receipts and paused at journal2754
after601.831 seconds. Sampled Python peak484,179,968 bytes and combined494,141,440
bytes remained inside the unchanged allowances. These samples are not OS high-water
marks. All265 observed owned processes were checked absent. Fourth main slice is
separately admitted using an immutable recipe and exact owned pause. No source
originals/catalogs are modified; no automatic family choice or live migration has
occurred. Auxiliary, referenced-path and packet work remains to be independently
admitted after main inspection.
