# Epic sc-22835 execution ledger

Source of truth: https://app.shortcut.com/trefry/epic/22835

## Current wave

sc-22836–sc-22843 are verified Done. S8 PR #10 merged as `6ff4487`, with identical reviewed/merged trees and all four PR/main CI jobs passing; Shortcut closeout was read back. S9 main inspection has resumed after the user reconnected the RAID. The latest completed-candidate checkpoint contains 22 outcomes and 14,167,725 retained source rows, not distinct photos. The REAL/INTEGER relationship correction is integrated and passes its native gate; old false-dangling reports remain historical evidence. A disposable actual-CLI check passed the transition from legacy main evidence to a fresh schema-3 plan, then companion capture, paths, packets and replay. Draft PR #11 is open. Its first CI run passed Linux/macOS/evidence contracts and exposed Windows URI/sharing defects, corrected in `9da1b98` with independent source review. Actual catalog main inspection remains incomplete; real companion/path/packet acceptance and current-family review are outstanding. Live Shortcut remains authoritative. Sections below retain historical evidence.

| Story | State | Work surface / artifact | Evidence | Next action |
| --- | --- | --- | --- | --- |
| sc-22836 | Done | [PR #1](https://github.com/michaeltrefry/PhotoCatalog/pull/1), merged 56f0b37 | Independent review PASS; 18 local tests; real CR2/JPEG source invariance; merged-main three-platform CI 34227667817 SUCCESS; Shortcut read-back | Complete |
| sc-22837 | Done | [PR #3](https://github.com/michaeltrefry/PhotoCatalog/pull/3), merged f353f3d; [decision](BACKEND_DECISION.md) | Reviewed head 7bb3930 and merge have identical trees; PR CI 34362927300 and main CI 34366395501 all four jobs SUCCESS; Shortcut Done read-back, comment 22896 | Complete |
| sc-22838 | Done | [PR #2](https://github.com/michaeltrefry/PhotoCatalog/pull/2) and corrective [PR #5](https://github.com/michaeltrefry/PhotoCatalog/pull/5), merged bef8b6c | Final renderer review; 115 tests; private camera/render comparisons; PR CI 34386168135 and main CI 34390636314 all four jobs SUCCESS; Shortcut read-back | Complete |
| sc-22839 | Done | [PR #4](https://github.com/michaeltrefry/PhotoCatalog/pull/4), merged be85c16 | SDK normalization repaired; independent RDF/opaque XMP preservation checks; PR CI 34380874912 and main CI 34382746592 all four jobs SUCCESS; Shortcut Done read-back | Complete |
| sc-22840 | Done | [PR #6](https://github.com/michaeltrefry/PhotoCatalog/pull/6), merged 60fc33c | Controlled APFS detach/remount/reorganization/replacement/undo; Linux bind mount and Windows volume GUID/junction proof; PR CI 34394610454 and main CI 34395883109 all four jobs SUCCESS; Shortcut Done read-back | Complete |
| sc-22841 | Done | [PR #8](https://github.com/michaeltrefry/PhotoCatalog/pull/8), merged789a39d | Reviewed preview defaults and full30-file memory/quality, layout, navigation and integrated10M evidence; PR CI34430709720 and main CI34432335579 SUCCESS; Shortcut Done read-back | Complete |
| sc-22842 | Done | [PR #7](https://github.com/michaeltrefry/PhotoCatalog/pull/7), mergede68d375 | [Mac qualification report](ORGANIZATION_PERFORMANCE_RESULTS.md); reviewed scale/actual-overlap evidence; main CI34418548263 SUCCESS; Shortcut Done read-back | Complete |
| sc-22843 | Done | [PR #9](https://github.com/michaeltrefry/PhotoCatalog/pull/9), merged f6d19dc; corrective [PR #10](https://github.com/michaeltrefry/PhotoCatalog/pull/10), merged 6ff4487; [qualification report](EDIT_QUALIFICATION_RESULTS.md) | Reviewed 533 cases/249 numerical configurations PASS; PR CI34538492535 and main CI34550127979 all four jobs SUCCESS; exact tree and closeout evidence verified; Shortcut Done read-back | Complete |
| sc-22844 | In Progress | [Draft PR #11](https://github.com/michaeltrefry/PhotoCatalog/pull/11); frozen v6 main; corrected schema-3 final-plan profile | Numeric gate: 4 focused, 18 Lightroom library and 22 integration tests pass. Controller: 22 contracts and disposable actual CLI integration/replay pass. Complete Windows correction reviewed and focused Mac checks pass; hosted Windows remains pending. Main resumed from 7076; latest completed-outcome checkpoint is 23 candidates / 15,063,505 source rows | Verify Windows correction in CI; finish main, separately funded full/path/packet stages, current-family decisions and merged-state acceptance |
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
corruption is an explicit cache miss. The retained identity/change-stamp guard now spans native export decode; its
regression rewrites the same inode before decode and restores bytes/mtime afterward.
The inconsistent decode is rejected while the stable positive control passes.
The prospective S8 targets remain unchanged. The serial qualification harness
must freeze exact source/cohort/oracles/settings/resources before execution.

## S9 inspection resumption checkpoint — 2026-09-10

The seventh v5 inspection attempt stopped after macOS reused the PID of a
completed inspector for a later command. All 198 observed PIDs are absent and
the wrapper was reaped. The failed result remains preserved; user Lightroom
catalogs are unchanged. Commands 5020–5848 succeeded, and read-only row command
5849 was interrupted. Fourteen completed members retain 9,256,288 rows; the
fifteenth retains 483,383 rows with 24 successful readback pages. Its failed
25th page does not establish progress.

The supervisor now identifies owned processes using native birth seconds and
microseconds. Independent source reviews passed this correction and protocol3
checkpoint `cda4a65`, which admits the exact failed attempt into a separate v6
inspection namespace while retaining v5/v4/v2 provenance. Eight supervisor
fixtures and 60 Lightroom contracts passed.
Receipt: `sc-22844-pid-recovery-gate-cda4a65-v1/result.json` under private results.
The actual v6 init/adopt phases subsequently exited successfully. Independent
review reconciled 13 typed tables, 1,774 descriptors, 9,739,671 retained rows,
15 captures and 14 completed member outcomes. The active member retains 24
successful pages through cursor 9,280,288; failed command 5849 adds no progress.
Known processes are absent and v6 main has not started. Evidence is
`sc-22844-generation-request-v6-cda4a65/independent-adoption-review.json`
under private results. Adoption finished before the tiny editing smoke began.
The next main request is held: its existing growth allowance plus protected S8
qualification funding exceeds currently available space. Continuous protection
of that funding is required before further inspection growth. Auxiliary,
referenced-path, packet and current family selection work remains outstanding.

## S8 qualification preparation checkpoint

At `45df945`, the full locked all-target Rust gate passed 370 tests across 33
suites, with three intentional ignored fixtures. Strict Clippy and package
formatting passed. The initial broad gate exposed 16 dirty-binding trigger
failures; conditional inserts fixed repeated UPSERT/REPLACE conflict behavior.
The failures remain retained. The independent Adobe DNG sensor-neutral test
passed; it does not establish real-camera Adobe appearance parity.

The probe compiled at `a5ac9d6`. At `6bdb95f`, all 42 Python checker contracts
passed, including actual subprocess cleanup, evidence failures and the fixed
per-configuration statistics. Independent statistics review passed. Receipts:
`sc-22843-core-and-probe-gate-v1/receipt.json` and
`sc-22843-supervisor-statistics-v1/receipt.json` under private results.
The expanded checker gate at `d38a526` passed 80 tests. Nine actual preview/export
process tests and strict all-target Clippy passed, including worker high-water
receipts. Fifteen memory/cleanup checker tests passed at `3539509`. These gates
exercise checker contracts; they are not actual corpus or timing qualification. No large editing campaign has started. Final aggregate, durable
metadata/service and 100MP independent checks, binding, campaign, review and
three-platform delivery remain required.

The first tiny synthetic end-to-end smoke failed because image 0.25.9 silently
missed the recognized TIFF ICC tag. Commit `9483835` reads that tag through the
existing TIFF dependency and rejects malformed profiles. The native regression
reproduced the wrong gamma before the fix; the media suite then passed 19 tests
with one intentional external-fixture ignore. Independent source review passed.
Tiny smoke v2 passed all four analytic fixtures with 14 recipes each using the
unchanged independent oracle and tolerances. Both smoke attempts are retained
under `sc-22843-tiny-smoke-v1` / `sc-22843-tiny-smoke-v2` in private results.
This is correctness evidence only; performance and large-image qualification
remain pending.

Tiny production-path smoke v3 passed two references, warm previews (2+100) and
first-original lifecycle on TIFF (2+20), and retained nine failures. Six metadata
cases exposed invalid RDF in the controlled fixture; `3473220` preserves its
intended nested qualifier in valid explicit RDF syntax. Twenty-two JPEG exports
completed, but the checker incorrectly expected no XMP despite the service's
regenerated technical metadata. `26d38cc` corrects that expectation while keeping
direct-codec omission checks strict. All 98 Python contracts and the focused
native RDF regression passed; independent source review passed. Two tiny overlap
cases did not retain a live worker long enough. No overlap or performance result
is awarded, and a fresh corrective smoke remains required. Failed v3 artifacts
are preserved under `sc-22843-service-smoke-v3` in private results.

Corrected read-only verification of v3's 22 retained JPEG outputs passed, with
the original inputs and failed receipts unchanged. Fresh smoke v4 then passed
seven cases: the two references, warm preview, first-original delivery, the
22-output export including cleanup, and PNG8/16 selected-metadata preservation.
Six failures remain retained. `48a2d17` fixes TIFF readback through the held file
descriptor by providing the Python decoder a display name; the regression failed
at all three precisions before the fix, then all 18 readback contracts passed.
Independent source review passed. These results do not yet qualify actual TIFF
derivative preservation.

JPEG with a large selected XMP packet exposed a product preservation defect:
the pinned SDK's extended packet loses the named RDF subject. The strict checker
and importer reject the resulting subject mismatch. Encoder transport correction
`afaa4ca` preserves the named subject and recomputes the extension digest;
seven native export tests pass, including full export/reimport preservation and
rejection of a changed extension subject. Stale-renderer publication guards
`1ded7b6` pass 17 native tests while retaining finalization of already-installed
outputs. Both changes passed independent source review. Two tiny overlap cases
still lack a sufficiently long-lived natural worker. All v4 known processes are
absent; no performance award follows from this correctness smoke. Full campaign
preparation still awaits final source binding and an explicit phase admission.

Bounded outer supervision and host-log funding are implemented in `ac373d1` and
`a649807`, with a separate preparation limit. Final review found late zero-exit
acceptance, repeated zombie accounting, and unresolved ancestry reporting;
`809e434` fixes all three. The integrated Python gate passes 122 tests with no
skips, and independent exact-source review passes. The full native gate passes
377 tests with three intentional skips; strict Clippy and both debug and release
builds pass. These results qualify the source checks, not the unstarted full
campaign. The next service smoke must use the corrected native binaries and
final supervisor helpers; v5 remains an unexecuted source proposal.

S9 supplementary funding control `90a0f1a` passed 12 synthetic tests and parent
independent source review. It preserves the frozen inspector and adopted evidence
while checking free space before new commands and at outer observation points.
It is not a filesystem quota. Its prepared main request remains held and must
bind the final S8 funding amount before any execution; no further large plan copy
is needed solely for this control change.
