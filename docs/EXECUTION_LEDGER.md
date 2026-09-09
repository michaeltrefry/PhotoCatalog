# Epic sc-22835 execution ledger

Source of truth: https://app.shortcut.com/trefry/epic/22835

## Current wave

sc-22836 and sc-22838 are verified Done. sc-22837 has selected and integrated SQLite at the measured 256 MiB per-connection settings. Original/supplemental comparisons, corrective timing, bundled query-work validation, current native timing, and independent evidence reviews are complete. Local checks pass. The remaining closeout work is final documentation review, exact-head hosted CI, normal PR merge, merged-main validation, and Shortcut read-back. Dependent stories remain To Do until that closeout.

| Story | State | Work surface / artifact | Evidence | Next action |
| --- | --- | --- | --- | --- |
| sc-22836 | Done | [PR #1](https://github.com/michaeltrefry/PhotoCatalog/pull/1), merged 56f0b37 | Independent review PASS; 18 local tests; real CR2/JPEG source invariance; merged-main three-platform CI 34227667817 SUCCESS; Shortcut read-back | Complete |
| sc-22837 | Done | [PR #3](https://github.com/michaeltrefry/PhotoCatalog/pull/3), merged f353f3d; [decision](BACKEND_DECISION.md) | Reviewed head 7bb3930 and merge have identical trees; PR CI 34362927300 and main CI 34366395501 all four jobs SUCCESS; Shortcut Done read-back, comment 22896 | Complete |
| sc-22838 | Done | [PR #2](https://github.com/michaeltrefry/PhotoCatalog/pull/2), merged a0bf374 | Independent review PASS; 31 release tests; 22 private samples/44 renders; public RAW checks; PR CI 34237848026 and merged-main CI 34242815786 three-platform SUCCESS; Shortcut read-back | Complete |
| sc-22839 | In Progress | XMP fidelity; codex/sc-22839-xmp | S2/S3 Done; exact-byte extraction, semantic model and guarded export under implementation; comment 22898 | Integrate schema/projections/conflicts/export; independent review and cross-platform CI |
| sc-22840 | To Do | Offline storage and relinking | S2 Done; volume identity/path preflight complete | Sequence shared catalog schema work after S4 |
| sc-22841 | In Progress | Preview storage and scheduling; codex/sc-22841-previews | S2/S3 Done; protocol/adapters/harness under implementation; comment 22899 | Review protocol before timed runs; implement full cache/scheduling/recovery scope |
| sc-22842 | To Do | Organization and search | Prerequisite sc-22839 | Dependency-bound |
| sc-22843 | To Do | Editing and export | Prerequisites sc-22838/sc-22839/sc-22841 | Dependency-bound |
| sc-22844 | To Do | Lightroom dry run | Prerequisite sc-22839 | Dependency-bound |
| sc-22845 | To Do | Lightroom migration | Prerequisites sc-22840/sc-22842/sc-22843/sc-22844 | Dependency-bound |
| sc-22846 | To Do | Backup and restore | Prerequisites sc-22843/sc-22845 | Dependency-bound |
| sc-22847 | To Do | Desktop UI | Prerequisites sc-22840/sc-22841/sc-22842/sc-22843/sc-22845/sc-22846 | Dependency-bound |
| sc-22848 | To Do | Integrated readiness | Prerequisite sc-22847 | Terminal proof only after integration |

## Resource and authorization ledger

- User authorized epic delivery, ordinary PR/CI/merge, and the private michaeltrefry/PhotoCatalog repository. The user released the reference Mac on 2026-09-09.
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
