# Epic sc-22835 execution ledger

Source of truth: https://app.shortcut.com/trefry/epic/22835

## Current wave

S9 sc-22844 remains In Progress. Corrected FULL and metadata-only PATHS are complete and independently audited. PATHS session16784 exited0/reaped after3,481 commands, next27152:1,006,834 references,301,117 available and705,717 missing at their stored paths. Result `9c53960818b6d52f7e2e6a8ea610c6186eb6cc2c3e52c9193a9814fd6b044232`; output `105b0e7ac5f36497888c55e7d687e026e3337147cd97295eb5a6cc2b137c91dc`; independent terminal review `2b681bb0a4cc4b4448b267641eb503e5db815386e6f8497a90d374f86180e434`. No failures/cleanup. Terminal audit source v2 corrects sparse native report-state counts,13 regressions and root source review passed; actual audit completed5.84s/72.6MB sampled/reaped0,49.96MB metadata. No packet phase, family choices or migration yet.

The sidecar metadata scan completed and was independently verified: session 36036 exited 0/reaped, 24.853 seconds and 48,431,104-byte peak, all 47 revisions and 4,027,336 literal stat calls. Measurement `12d07f0e51963781ae1e55fc38ae6d2af6a61c44589ec3fae7324bc1b9d70858`; actual independent review `f6875da60cec50a79c6714b57deaed28e645a7c7e0bb8df8d5ceb598d0645d40`; owner result `c97c447b2eaa30b577cdf6140511937bbc1d2d7596324e30deeba86ddbd62d22`. The 301,117 available original references total 15,160,542,029,800 bytes; 256 sidecar occurrences total 846,812 raw bytes plus the same separately decoded bytes. No photo/sidecar body or database reads. All inspection processes are reaped. Do not rerun completed FULL, PATHS or sidecar measurement.

sc-23137 is verified In Progress and blocks sc-22844. Source analysis exposes 30.32 TB of repeated whole-file hashing before parser reads; this is a derived read volume, not measured runtime. Hubble implements the focused single-pass source-proof fix off main9dbcc76 in `sc-23137-xmp-single-pass`; Hooke assesses explicit successor-native qualification without changing historical bindings. Packet funding remains preparation-only with unresolved embedded payload/projection allowances. No packet grant, extraction, family choices or migration. Exact continuation is in PAUSED_HANDOFF.md; S9 comment23138 and bug relation read back verified. PR11 remains draft at80d56a0/all4CIgreen; frozen execution is unchanged.

Current-catalog scope revalidated: user wants one current member per family, while all-47 external packet extraction is frozen runner sequencing. The private `sc-22844-current-catalog-proposal-9oqhobxn` presents all 48 candidates and 16 proposed choices from saved PATHS metadata; independent review passed (`b4ef4d4fae9267c7cb4a632662eeea510bc47b9c38358e54f50d320e106c5373`), user choice pending asynchronously. No selections or source mutations applied. S9 comment23139 records the explicit successor roster/attribution requirement.

sc-23137 candidate `f68f78e947d59af91d9ac51c55986f9cab62391e` is in draft PR13 and tracker In Review; final CI/source review/merge pending. 398 local tests passed at39012d5, final change only removes a needless return; strict Clippy/fmt pass. Receipt `2724dd0b82cc5dec5e35ff569a031a7e64ea103cba708a3b5ef30a99d74ac308`. Broader Unix ctime was rejected after concrete kernel precision evidence; current one-pass eligibility is held local APFS or Windows deny-write/full-ID proof, conservative elsewhere. Reference RAID APFS confirmed read-only. Native lane released, all local sessions reaped. S9 selected execution adapter implementation continues in isolated `sc-22844-selected-packets`; no actual choice or phase grant.

PR13 current head `aa21c2453f3a16cc4423124207ad6ad7ad30b1a7` is ready, source reviewed, final CI34653155165 running. Previous f68 CI34652404977 passed Mac/Windows/benchmark; Linux test-only unused import was corrected by aa21 and independently reviewed (`40971aa9...`). Preserved exact failed log and previous terminal JSON under sc-23137-single-pass-gate-v1. Conditional selected16 packet budget is now complete as a prospective policy: 1,140,624,680,187-byte initial minimum, independently reviewed `24a9a3a0...`; actual user selection/admission still pending. Three-catalog native fixture package df9833e3... is prepared but not executed. No actual packet phase or source-file writes.

## Historical checkpoints

Corrected FULL inspection is terminal and independently audited PASS. FULL29 (`17a9c510-8893-4988-a22f-4ca025dbab95`, session28679) exited0/reaped at command23671, no command failures/cleanup. Result SHA `a022b93193438067b3e48692750faf69caf3f69dd53d36ce948c924cf839bb9f`; `sc-22844-raid-full-terminal-review-v1/terminal-review.json` SHA `c2977071b1b4bd9e7c79cbd1ff2082d9cb89c683b066743b26796ebfcf1dc679` passed (31,652,232B metadata). FULL output SHA `7367db0a8cabaab85f1aeff9874357e93cc18e36ddec1c89b119b3251037d3ac` contains48 outcomes/47 requested full captures,16 families,19,872,102 retained source rows, no inspection errors. Do not continue FULL. The subsequent metadata-only paths phase is now running; no packet phase, family choice or migration has executed. Both old failed intervals and clean cadence transition remain preserved. This is terminal FULL provenance proof, not complete S9 acceptance.

Actual first-path sizing completed under the existing supervisor: `sc-22844-path-sizing-result-44ed1d98-f565-4ce5-add2-2dd93a4ae379/measurement.json`, SHA `a21718b9258f99e68119a67996f038b16a3d0ff4c456926646fd96e6c9392065`, MEASURED_SIZING_ONLY.47 revisions/1,006,834 pending paths,428,026,080 stored descriptor bytes,418,695,916 native pending-admission bytes (max741).6.267s,229,562,000 VM-step floor,63,356,928B process HWM and sampled group peak; owner/session29407 exited0/reaped, no errors. Plan93,308,780,544B,4096-byte pages, one freelist page, journal_mode delete, no WAL/SHM/rollback journal, before/after identities unchanged. Admission/request/owner/results/actual tool responses are under `sc-22844-path-sizing-admission-44ed1d98-f565-4ce5-add2-2dd93a4ae379`; request SHA `b2a0e7c55e6d01814d0a5ba5c785793fad57428d59a7dd68784f17dcabe6d28b`, owner-recipe SHA `173e38876317bbebe2078b4941b7a0505319af35cd05e40b8e8e83e937bb3fa8`, owner-result SHA `093187442763e846ee6bc88c6f3e05dd1b4fe8718e4998400cd8c97d7b51fee3`. Actual sizing independently passed: `sc-22844-path-sizing-actual-independent-bs5fwhg1/review.json`, SHA `c527810074c1b5e5c5812a30030af9aaad8806a836d1f7b074eb009ff8636292`. First paths funding/procedure is prepared in `sc-22844-first-paths-funding-9vjvdcnw`: funding SHA `d8240a8615a8bfaf9888f4a49a5cbf74a74c32528a8efa3a53f62759adbc4255`, prospective initial free minimum320,784,132,099B with unchanged headroom/reserve. Independent funding review passed: `sc-22844-first-paths-funding-independent-ey9zggtk/review.json`, SHA `3003c8f2a87c027b031887b7889d9fc140c97b271b544ca8505a8016db8115c5`; corrected PROCEDURE-v2 distinguishes requested WAL/FULL/fullfsync from the quiescent DELETE observation and states the actual locking/free-space monitor boundaries. Exact unsigned phase-boundary recipe is under `sc-22844-first-paths-admission-f56d09e7-e3bb-4d19-b43f-ac2252cd0ef1`, SHA `47901e6f1082c8edf7c2f19634df971d125ce80074da18e811cf5090f01e8aab`; both locks acquired, FULL29 current/journal23671 and measured plan identity unchanged, no pause, exact command000023671 absent. RAID3.725TB/local302GB available at preparation. First paths phase is now running: attempt `f56d09e7-e3bb-4d19-b43f-ac2252cd0ef1`, session16784, recipe SHA `5aeca619495e20c47cdfffb3407740146ad5358ae9d60125e3448db2d343eccc`, root admission `abe0638a32a628234b14804b2fb3308be43f2d5d57478f74aea0cd7ef9feaefe`. Actual launch response is saved unchanged in the admission directory. First revision checked12,316 paths in12.178s across13 nonempty native commands; this is a prefix observation, not phase completion or whole-library throughput. Frozen source is unchanged. The old FULL terminal auditor is FULL-only; a bounded phase-specific terminal review is being prepared, anchored to completed FULL provenance.

Bugs sc-23102 and sc-23122 are verified Done: PR #12 merged as `9dbcc76d4e9dfa3dbd626007b4e73698d2995a9e`, exact tree match to reviewed `af8c6a4c743a8849287e2f35ce5c1b9b51e5c532`. PR CI34638787643 and merged-main CI34639830635 both passed all4 jobs; live main and tracker read-back verified. Delivery receipt `sc-23122-orphan-handshake-gate-v1/delivery.json` SHA `702ca2f4808f68301bb3a40d186124b79bd48f74b78a54e97c413e632ca22824`. S9 integrated main at local `2dc7b105c6fbefd0d82a48e2e3d40bee452fd9f5`; no frozen runtime changed. Public PR11 head `80d56a0eec2beb1ac02a3bb60f09889854ce5e5e` passed all4 jobs in CI34641343286; receipt `sc-22844-delivery-80d56a0/ci-terminal.json` SHA `702ff9e17a0425af985535a58c99b993b37298d6df8b086458a011ced005ba99`. It remains draft/unmerged and S9 is In Progress.

Historical split preparation: Export-lock bug sc-23102 is now isolated unchanged on live-main6ff4487 in PR #12, head `ae59afcf25d7a8f8849ac7242351887b1a34a4da`, worktree `sc-23102-export-owner-release`. Exact transplant review `d9a2124269e0cd7e1d9ee054ca253064c0fb744d3ac08faf7f43853eba756e26` passed. Fresh CI34637538748 exposed an unchanged timing-dependent supervisor test: job103388995719 expected known orphan but received unproven ancestry. Exact failed log retained under `sc-23102-export-lock-gate-v1/ci-ae59afc-benchmark-failed.log`. New Bug sc-23122 is In Progress, blocks23102, and is being repaired in the same PR test file with explicit child-observation synchronization; no production runtime change or local native build is admitted. The original PR11 remains draft and the unchanged358ac3d CI success is historical evidence, not standalone delivery acceptance.

Historical public snapshot: head `358ac3d3b1aa2ae2865fbbced13601f6daa27bf4` passed all four CI `34625720876` jobs. Bug sc-23102 is In Review with deterministic before/after reproduction, three lifecycle tests, five export integrations, strict Clippy/formatting and independent review `7c2e2296…`. PR #11 remains draft/unmerged and S9 remains In Progress. Repository visibility was reverified PUBLIC. Source is stationary; this handoff update is local pending the next material delivery batch.

Cadence transition is reviewed and admitted: 3600s cooperative soft duration, unchanged4800s emergency/RSS/disk/source/user-pause guards, early pause at12MiB completed new-result metadata or18000 new commands, unchanged16MiB/20000 terminal caps. Package `sc-22844-cadence-source-v1-EvaR9L` has56 author tests plus3 independent boundary checks. Source review `d8b6eb9a024adcb2e55da28179b831439ba13cd91541adf12e7294cae9b49008`; actual clean transition `sc-22844-cadence-transition-independent-oekvvrrt/transition-pass.json`, SHA `d3ff4e36a91041b170461631065e7b49c0d0ec0906f2888d603285022396c921`. Root admission `sc-22844-canonical-continuations/81bb3b08-58ed-4b34-a623-ad5d7fa33c0c/root-admission.json`, SHA `16ead5ea88b441c422e631c22b3522be88098637c712ca21e99f6c4dba46631a`. Both old failed intervals remain failed; new pause/terminal auditors are `9b7689e89e461e8c232d7dc8b401b42842f59b58d0caef244ca2c720d1952993` / `da77b8f8604d37c76ed97b21875500a0c010343922f0b288298056f51881c6d6`. Actual quiescent accounting:92737536B local control allocation, RAID3.771TB/local316GB free independently checked. The96020296B category is an estimate, not quota; no funding number or resource limit increased.

Historical sizing source preparation: script v3 was ready as PREPARATION_ONLY under `sc-22844-paths-sizing-script-v3-i9xvYY`: source `7c62af68f0bae9197631b4f4c03a1d3c4835119ecb294f6c86079bbd4dd11674`, independent review `sc-22844-paths-sizing-independent-cj0csq7s/review-v3.json` SHA256 `0917e7216eac679186e5d8ec900b8ac210bda9838f1a5659d79ebee573702852`, 25 synthetic tests passing. V1 metadata-growth/gate-status defects were corrected; V2’s 100M VM proposal was insufficient for even the 503921 pending paths already observed in 15 reports, so V3 uses prospective 1B VM steps with the same 120s/256MiB/other bounds. No actual-fit guarantee or real plan read. Terminal FULL/review, exact 47-revision inputs and separate root admission are required. Use its README and existing frozen owner; never execute directly outside supervision. The sizing gate used `/Users/michael/PhotoCatalog-private-results/sc-22843-private-python-v1/env/bin/python` (verified psutil7.2.2, SQLite3.53.3); use this existing private environment for the owner/measurement, not the bare FULL-controller Python which lacks psutil. Source-only cadence compatibility review `sc-22844-paths-sizing-cadence-interface-gkrnjxk5/review.json` SHA `b0ad3a87d4f429ebb61a15fd44623795bb8e122da2988af3973c0c344dfceef7` passed4 tests: no package change required; old auditor pin provides extracted metadata/lock utilities, while actual new result/review/profile refs must be bound in the real request.

Final family-choice renderer is source-ready under `sc-22844-family-renderer-v1`: `render.py` SHA `4f2e4a4d2d2bcbd780ccbb8ea1803d5ed1d4231d6bae075fdbfdba0d0929b382`, five synthetic tests PASS, independent review `sc-22844-family-renderer-independent-7idhapzk/review.json` SHA `c6e77d3a13d3b911809e2bff362555ef46be515dc0bcba9f1e3ca719812e550e`. It renders exact final FULL and packets reports into private REVIEW.md plus lossless mapping.json, covering all16 families/48 candidates, separate image/master/virtual/file counts, UTC filesystem dates and explicitly unknown internal timestamp units. No actual reports rendered or choices made. Use its README after final packet qualification.

A bounded local observation after FULL21 recorded 16 successful native adds, 15 reports and 9739671 retained source rows, not photo counts or terminal acceptance. Receipt `sc-22844-native-progress-after21-zn4ro2z7/observation.json`, SHA256 `306757f8c5be7e5a654f28af87f83a023b7946a291c29ecea119b5a1058af2a2`. Final family choices must bind the latest packet-phase evidence digest; pre-selection zero conflicts do not establish absence of overlap, and internal change timestamps have unverified units.

Historical attempt 18 (`2bacecf2-06d6-4341-bb56-4f6a83c42a91`) exceeded the 512 MiB sampled Python limit at 539,197,440 bytes during saved-row replay. Session 91297 was reaped, with only discovery 10238 added. Result `2854b115c33d7765c0460ed89c33c0e8aae8e436368f8270eba8b1aa25afdddd` remains failed. The earlier diagnostic substituted file-sized reads for production cap-sized buffered reads, so its memory result did not establish production allocation behavior. The fixed-chunk reader passed 129 Python tests and independent review. Corrected diagnostic v3 replayed 10,221 records without native dispatch in 197.2 seconds at 253,100,032 bytes peak RSS; session 4846 and child 33755 were reaped. Independent recovery review `a11f1ab8…` preserved both failed intervals before attempt 19 admission. Older snapshots below are historical.

Historical S9 stop after seventeenth attempt
`e1ddf030-f29f-440c-84d1-dd0421b079a3` exceeded the 512 MiB Python sampled RSS
limit during saved-page JSON replay (537,378,816 bytes observed). Actual session
`88833` exited and was reaped; cleanup recorded no remaining observed processes.
Its result remains `failed_or_unknown`, SHA256
`2c71d45e486bfbd3472897565d93e9c9b87a4b702bc606cb4275b2697e2990d9`.
Journal next command is 10238; this attempt completed only fresh discovery 10237.
Do not resume from an older clean checkpoint or relabel this failure. A reader
correction now releases encoded bytes before constructing JSON objects, with
encoding/error behavior preserved. Read-only diagnostic session 9436 reached the
first unrecorded command after 10,221 saved records in 203 seconds, with
343,457,792 bytes peak RSS under the unchanged 512 MiB limit, no native commands,
unchanged failed evidence and reaped owner/child. Receipts are under
`sc-22844-readonly-replay-diagnostic-v2/execution-20260911-root-01`.
Independent actual recovery review and separate admission remain pending;
diagnostic qualification alone does not establish production or S9 acceptance. The clean
checkpoint and earlier active snapshots below are historical; check current control.

The public reader/protocol 2 correction passed all 126 Lightroom Python tests
and independent review (`723051297dcde7e0ba64d25046f9b46560e2ae9c167814770d67873b3aba499f`).
Its sampled test-root RSS was 143,966,208 bytes; the test process was reaped.
Public support binds the expanded helper roster and equivalence evidence, while
ordinary failed-predecessor rejection remains. The incident-specific recovery
controller and history auditors are separate private evidence, not a generic retry
feature. This local gate does not replace CI for the eventual new commit.

sc-22836–sc-22843 are verified Done. S8 PR #10 merged as `6ff4487`, with identical reviewed/merged trees and all four PR/main CI jobs passing; Shortcut closeout was read back. S9 legacy MAIN is complete: 48 outcomes, 19,872,102 retained source rows and 5,740 source-table descriptors, not distinct photos. Its schema-2 numeric relationship defect prevents treating derived links as S9 acceptance. Corrected schema-3 FULL is rebuilding all 48 members; all 47 requested full captures are independently verified complete. Historical draft PR #11 head `70f5284`: CI `34604737271` has terminal SUCCESS in all four jobs, including Windows. Earlier CI failures remain historical evidence. Actual FULL completion, paths, packets, family decisions and merged-state acceptance remain outstanding. Live Shortcut remains authoritative.

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
| sc-22844 | In Progress | [Draft PR #11](https://github.com/michaeltrefry/PhotoCatalog/pull/11); corrected schema-3 FULL continuing | 47 captures verified. FULL26 clean at 13956; failures17/18 retained. All four CI jobs pass at `358ac3d` in `34625720876` | Terminal corrected FULL, separately sized paths/packets, family decisions, merge and merged-state acceptance |
| sc-23102 | In Review | Export-service lock lifetime correction in PR #11 | Deterministic before/after, 3 lifecycle +5 integration tests, Clippy/fmt, independent review and all four exact-head CI jobs pass | Merge with S9 delivery and verify merged-state closeout |
| sc-22845 | To Do | Lightroom migration | Prerequisites sc-22840/sc-22842/sc-22843/sc-22844 | Dependency-bound |
| sc-22846 | To Do | Backup and restore | Prerequisites sc-22843/sc-22845 | Dependency-bound |
| sc-22847 | To Do | Desktop UI | Prerequisites sc-22840/sc-22841/sc-22842/sc-22843/sc-22845/sc-22846 | Dependency-bound |
| sc-22848 | To Do | Integrated readiness | Prerequisite sc-22847 | Terminal proof only after integration |

## Bounded JSON read correction

Production requested the configured JSON cap even for small files. The earlier diagnostic substituted file-sized reads and therefore did not reproduce that allocation behavior. The corrected reader uses at most 64 KiB per read for admitted caps, joins fixed chunks, and releases the input buffers before decoding objects. It preserves byte encoding, JSON errors and cap overflow behavior. Public source and tests passed independent review `d1e2f62b62ea57ff0ed31c3ffa180f94b2d2b194e34c0e8f12ef6fd9dfdc6a95`; all 129 Lightroom Python tests passed. The initial synthetic standalone-helper fixture error and rejected bytearray prototype remain retained evidence.

Private helper `209b8a54…` and profile `5ccb6aea…` bind equivalence review `9f1134f6…`. The corrected diagnostic retains production buffered read sizes, the real JSON implementation, and frozen hashing/page processing. Its receipts are under `sc-22844-readonly-replay-diagnostic-v3/execution-root-01`. Synthetic small-file temporary allocation improved substantially; near-page chunk joining used more memory than the original reader, so the result is not a blanket memory or throughput claim. Controller `0ac64b39…` and auditors `0f8e97c6…` / `4a62e753…` passed independent source review and preserve both failed intervals. No failed attempt is relabeled as a clean checkpoint. Production attempt 19 crossed replay and reached a reviewed clean checkpoint; terminal FULL and S9 acceptance remain pending.

## Export-service ownership correction (sc-23102)

Linux CI at `58e7a1a` exposed an export owner-drop/reopen failure. A deterministic local test reproduced it by retaining a duplicate of the actual service lock descriptor after dropping the service. `ExportService` now creates an acquired-only explicit-unlock guard immediately after successful acquisition. Its release follows existing worker reaping and preview-pause cleanup; failed acquisition cannot unlock another owner. Constructor errors and unwind also release ownership. Three lock lifecycle regressions and all five existing export-service integration tests passed locally, and independent source review found no blocker. Exact-head three-platform CI remains required. The change does not alter render pixels, codecs or hashing; prior S8 numerical measurements retain their original source identity.

## Resource and authorization ledger

- User authorized epic delivery and ordinary PR/CI/merge. The user changed michaeltrefry/PhotoCatalog to public because private-repository Actions consumed the monthly allowance; preserve public visibility and batch validated changes before CI. The user released the reference Mac on 2026-09-09.
- Originals and Lightroom sources on the RAID remain read-only. Private fixtures and evidence remain outside Git. No live migration or original mutation has occurred.
- User authorized a dedicated RAID scratch folder for S9: `/Volumes/MichaelJon/PhotoCatalog-Scratch/sc-22844-schema3-20260911`. Inspection output is under `inspection/schema3-run`; SQLite temporary files use `sqlite-temp`. Local control/evidence remains under `/Users/michael/PhotoCatalog-private-results/sc-22844-raid-full-control-v1`. Scratch authorization does not permit writes to source catalogs or photos.
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

## S9 current FULL checkpoint — 2026-09-11

Delivery head `70f5284` passed all four jobs in CI `34604737271`; the saved
terminal receipt is `sc-22844-canonical-runtime-profile-v1/ci-70f5284-terminal.json`.
PR #11 remains draft and unmerged.

Legacy MAIN output is `sc-22844-current-families-v6/reports/main-review.json`
(SHA256 `5942e4dfea9ab05c1677bb7d1844ef68431e9ef080eb795878f781fd97026b44`).
The source remains preserved; corrected FULL creates a fresh schema-3 plan rather
than rewriting the old relationship evidence. No family is automatically chosen.

Execution binds native source `ccae8ad` (binary SHA256
`eb541139f7f7888ee5eec58975ad9f36008c65500ee553fa4030d4507c47602f`),
and base driver SHA256 `af2c5e1dcb939713f8296998cc908e87b8e6658a4208b6e6cb12fe3b60132288`,
using new controller SHA256
`28c84319a05f10bfc4d7eb82efbf3d5ab147ea6f9a452fb482ed160b6cd21590`
and canonical hash profile `sc-22844-canonical-runtime-profile-v1/profile.json`,
SHA256 `243ccfff25670add20fa9f4c1b5511e426da637f99637dc17456b312eeff0d2b`.
The profile installs a reviewed byte-equivalent hashing helper: effective Python
execution changed, while native and base driver files/bindings remain preserved.
Prior slices used controller `969eeaad…`; their artifacts are not relabelled.

The independently reviewed first slice completed all 47 requested captures.
Second attempt `c6bf42d7-cea0-4ed1-baf5-415cfe0e7c12` is cleanly paused/reaped,
with 599 successful new commands and next command 655. Its local result SHA256 is
`df98edc1966c18aab0c071ead6e30ac7dbf0208fd8d7c41108d3823dbab98529`;
independent review `sc-22844-raid-full-second-slice-review-v1/paused-review.json`
has SHA256 `d268a04b3567081bb0c0a8f4835f94c779e9a3e94bc0e8e3d467e12d855d2fb0`.
A bounded observation at next command 8857 recorded 12 successful native adds,
11 native reports and 7,297,390 retained rows, with no observed native failures
or stage warnings. These are native progress facts, not completed Python
inspections: the driver can catch inspection errors before returning final FULL.
Observation `sc-22844-native-progress-observation-2VBJ8X3X/observation.json` has
SHA256 `6524fd768e3c420269aa4b6ee88e1bde40f626353dfd5edd45f9a34f93387683`.
All-48 FULL outcome qualification remains pending.

Fifteenth FULL attempt `bd23129e-1876-442c-b4a8-a0b6576b935e`, session `91609`,
paused cleanly and was reaped at next command 9528 after 711 successful new
commands from 8817 and no failures. Result SHA256
`26d7c7313f0abe76653ba85a765bbdf0771ed973d075263f68906df9a74e607f`;
PASS review `sc-22844-raid-full-fifteenth-slice-review-v1/paused-review.json`, SHA256
`4784b3b3e11f78fb5ee39d762e1686f0b66aa905116f8f92ed96aaba0a810b85`.
This is a clean checkpoint, not FULL completion.

Sixteenth slice `d74d191c-661d-481b-9f35-11a2897df6fa`, session `51299`,
is active from command 9528 with the same reviewed canonical hash profile.
Its recipe SHA256 is
`e32d27c2fe995490044795e23ad235256ffbe3e44424f7855d79af30deb65844`.
This snapshot can become stale while execution continues. Before resuming or
starting competing native work, read the actual `current.json` in
`sc-22844-raid-full-control-v1` and that attempt's result and terminal tool receipt;
do not treat this document as current process ownership proof.

Funding and limits are unchanged: zero additional capture copies backed by all-47
completion, full remaining plan/page/temp/metadata allowances without partial
credit, separate headroom and 32 GiB operating reserve; 600-second cooperative
slices, 4,800-second sampled emergency stop, 512 MiB Python / 1 GiB native /
1.5 GiB combined sampled RSS limits. Exact scratch environment/identity checks
and fresh inventory/free-space admission remain required. Recent recorded free
space was 3,812,635,889,664 bytes against the unchanged 474,549,846,911-byte
minimum; this observation does not replace fresh admission.

The exact old-to-profile transition PASS is
`sc-22844-canonical-transition-review-gtGRGbi4/transition-pass.json`, SHA256
`92443376288e43ffc5e5367adb50e957a016fa183fd4c366d62fcbb824558c25`.
The first canonical-profile attempt (thirteenth slice) recorded authorization before dispatch (`execution-profile.json`,
SHA256 `fe890c4aef5cd2bb03b5e92490b2b074786b7dd971f8e1f3eae0bd69b4810ce1`)
and actual child installation (`execution-profile-consumed.json`, SHA256
`2490f16a3daf7766f0b2f6ca426265ccefb7d10ee8b2a09841bcf565fa771240`).
These receipts augment the base source identity; they do not assert a runtime gain.

The actual local native/controller transition gate passed in 12 seconds: 52 commands,
one total capture, eight hash comparisons, five negative cases and all 11 CLI
children reaped. Receipt `sc-22844-canonical-transition-smoke-execution-v2/smoke-result.json`
has SHA256 `2c97975bfaadcf77d3390509391fbd479bfc0cb6215088878e4cfe086dabcce8`.
All 116 Lightroom Python tests passed in 5.18 seconds; root tool receipts are under
`sc-22844-canonical-runtime-profile-v1`. These gates establish transition correctness,
not real-corpus speed or all-48 FULL completion.

The profile-aware metadata auditor `sc-22844-checkpoint-audit-profile-source-v1/audit_pause.py`
has SHA256 `92cc53471f3457be1448977f456328f786db6c99026b6c5773e5f1a8739bba7d`.
Its independent review and 20 synthetic checks passed; review SHA256
`d2d6f223465434598876f737ec6114cbdf7d138d5031556edd8658e95482b36a`.
The previous e7-only auditor remains preserved for historical checkpoints.
The canonical preparer source `bd91429a4cfe4e14c0aa26ed6100a7708b86c11cf6a4e6034e08a039c763c8fb`
passed independent review `2e38b2d6994f6c4a9ff383b7145923d3b182017f35046596c913ba427a1fe27d`;
actual read-only draft and externally granted final preparation both passed.

The terminal FULL auditor `sc-22844-terminal-audit-source-OKkmuQol/audit_terminal.py`
(SHA256 `1ef8cb5d860489973828acaa4181e80ee7ed7f5be4d20132854a6474fe56ebe5`)
is source-reviewed, with 13 author and eight independent synthetic checks passing.
Independent review `sc-22844-terminal-audit-independent-aohcguni/review.json` has
SHA256 `7028802a2ac35b3f9531bc518bf8954df0dcaff9e6399d32e75149da6293159a`.
This is readiness to audit an actual terminal result, not a FULL completion claim.

For subsequent clean pauses, preserve the actual result/review, current control,
journal, exact owned pause and source/funding bindings. The private
`sc-22844-canonical-continuation-preparer-source-v2` prepares packages only,
requiring an independent review for the initial profile transition and a root grant
for each launch; root reviews and launches.
Later continuations must retain the same profile and use its profile-aware audit.
No re-adoption, automatic retry, family choice or migration is authorized by a prepared recipe. Paths/packets require successful FULL output
and separately reviewed funding through the existing phase controller; no new
controller code is required. See `LIGHTROOM_PHASE_CONTROL.md`.

## Historical S9 inspection resumption checkpoint — 2026-09-10

The following records the earlier failure/adoption state, superseded by the current
MAIN/FULL checkpoint above; its failed and successful artifacts remain preserved.

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

PR13 final aa21c245 passed all four CI34653155165 jobs and merged as b411ab5b; reviewed merge tree04d6706e verified. Merged-main CI34653984699 running. Selected runner152f899 passed150 Python tests; independent review identified a terminal selected-path identity reconciliation gap, now being corrected. Integrated source9aa5d0a builds a separate release native under sc-22844-selected-native-build-oil8ij91 (session69457). Tiny native proof launcher is in preparation; no actual fixture native or real packet command has launched. User selection remains pending.

Selected source correction dccada8 independently reviewed PASS b89f06ab; integrated c1cebd54 passes153 tests with pinned Python3.14 (default-runtime failed attempt retained), gate409f3cc3. Native9aa release build completed/reaped0 (67.17s), build46516238/nativee85cd0bf. Local tiny prior native proof launched by Hubble under admission243d4341, stops before selected dispatch. Renderer selected extension9 tests/final reviewd6261039 passed; all48 candidates remain visible with packet scope separate from native choice. Real selection and packet admission still pending.

Merged-main CI34653984699 now all4 SUCCESS at b411ab5b. Tiny proof session1880 reaped1 after successful MAIN/FULL3/PATHS3 due solely to launcher expected `available` versus actual `available_packets_uninspected`; failed result retained. Direct native comparison continuation authorized on quiescent tiny-plan copies, without recapture or retrying failed owners.

sc-23137 | Done (read back; comment23145) | PR13 merged b411ab5b, PR/main all4 CIgreen; qualified9aa/e85 S9 native | direct108-command comparison and actual33-command selected pause/resume/replay PASS, independent aedce5f7; all sessions reaped | no remaining bug work.
sc-22844 | In Progress | draftPR11 published003770e, all4 CI34654732967 SUCCESS; local closeout docs ahead | selected2/excluded1 fixture final8ac44056, sources/prerequisites/replay preserved; native-generated renderer proof47348bfe | pending actual user catalog selection, then production-specific profile/funding/admission, selected XMP, final dry run and delivery. Real FULL/PATHS/sidecar scans remain completed; no real packet phase or migration started.

User clarification: proposed16 source catalogs are approved for TEST/dry-run use; intended destination is ONE consolidated PhotoCatalog library with nested folders such as year/month/date. Epic E7/E10 and S10 updated/read back, S9 comment23147. Current Rust physical-folder hierarchy and recursive queries verified directly. Approval does not authorize final migration, backup ingestion or filesystem reorganization. Source message bca24810 retained privately; next test manifest/profile must bind this limited scope.

User follow-up confirms the folder display should mirror the filesystem, with every year in the same catalog. E7 and S10 now state that directly.

sc-22844 | In Progress | selected16 actual XMP test launched session77218, attempt6ae59d32-9b77-4c9a-afe3-e2a30bd1fe91 | recipe3cfa42ac, independent packagee22173d7/nativeaa208bb8, root admission1899f3cf, fresh RAID3.724TB >1.141TB prospective minimum | await actual pause/terminal and independent audit; originals read-only, one future consolidated destination; no migration. First root preflight used prior-result instead of prior-recipe descriptor and failed before writes/launch, preserved then corrected. Post-packets finalization procedure is sc-22844-post-packets-finalization-ljhtrxp7/PROCEDURE.md; no finalization launched.
