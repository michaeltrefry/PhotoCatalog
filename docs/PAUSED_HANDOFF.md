# PhotoCatalog continuation — 2026-09-11

## Current state

Epic sc-22835 remains In Progress. S1–S8 are verified Done; S9 sc-22844 is In Progress. Bugs sc-23102 and sc-23122 are verified Done through PR #12. S10–S13 retain their recorded dependencies. Do not mark S9 Done from the completed inspection phases alone.

**FULL and metadata-only PATHS are complete and independently audited. Do not rerun either phase.** No family choices or migration have executed. The sidecar metadata scan is also complete and independently reviewed; all inspection processes are reaped. Packet extraction has not started. Bug sc-23137 is In Progress and blocks the next phase while repeated whole-file hashing is corrected.

Current run: `/Volumes/MichaelJon/PhotoCatalog-Scratch/sc-22844-schema3-20260911/inspection/schema3-run`. Current phase-control pointer is terminal paths attempt `f56d09e7-e3bb-4d19-b43f-ac2252cd0ef1`, journal next27152, no reservation or pause at its terminal review. Check live receipts before claiming that this remains current.

## Completed phase evidence

Private evidence root: `/Users/michael/PhotoCatalog-private-results`.

| Evidence | Location under private root | SHA256 |
| --- | --- | --- |
| FULL29 result | `sc-22844-raid-full-control-v1/attempts/17a9c510-8893-4988-a22f-4ca025dbab95/result.json` | `a022b93193438067b3e48692750faf69caf3f69dd53d36ce948c924cf839bb9f` |
| FULL terminal review | `sc-22844-raid-full-terminal-review-v1/terminal-review.json` | `c2977071b1b4bd9e7c79cbd1ff2082d9cb89c683b066743b26796ebfcf1dc679` |
| PATHS result | `sc-22844-raid-full-control-v1/attempts/f56d09e7-e3bb-4d19-b43f-ac2252cd0ef1/result.json` | `9c53960818b6d52f7e2e6a8ea610c6186eb6cc2c3e52c9193a9814fd6b044232` |
| PATHS terminal review | `sc-22844-first-paths-terminal-review-v1/terminal-review.json` | `2b681bb0a4cc4b4448b267641eb503e5db815386e6f8497a90d374f86180e434` |
| Actual paths tool completion | PATHS attempt `/root-terminal-wait.json` | `3e871fc1d6eafbf6ec28c3b945f179825ad62693f4c9939043550bba9af4d4d8` |

FULL output is run `reports/full-review-bf20b3284ac182c2a65365b66b965169ee2ed3c896bdd5c2ed19a3551d298bff.json`, SHA `7367db0a8cabaab85f1aeff9874357e93cc18e36ddec1c89b119b3251037d3ac`:48 candidates,47 requested full captures,16 families,19,872,102 retained source rows, no errors. Captures and prior failed replay intervals remain preserved.

PATHS output is run `reports/paths-review-7367db0a8cabaab85f1aeff9874357e93cc18e36ddec1c89b119b3251037d3ac.json`, SHA `105b0e7ac5f36497888c55e7d687e026e3337147cd97295eb5a6cc2b137c91dc`:47 requested revisions,1,006,834 paths checked,301,117 available and705,717 missing at stored locations. Counts are catalog references, not unique photos. All3,481 new commands succeeded; session16784 exited0/reaped, cleanup null. Sampled combined RSS peak414,875,648B. The terminal audit read49,963,531B metadata in5.84s, sampled72,581,120B, session84089 exited0/reaped.

The independently authored PATHS auditor is `sc-22844-paths-terminal-source-v2/audit_paths.py`, SHA `af5a71d6cea309fc6b9b13d86c4c3bc36eae3aa83410e9aac94c120f2abb5f05`;13 focused tests pass. Root source review `sc-22844-paths-auditor-root-review-ik27peab/review.json`, SHA `94a69805aecbaf8f1ead617f8927df6766b2726fc7b415846641373f39caffe6`. V1's nonexistent aggregate report field was corrected to actual sparse paths_<state> counts; V1 remains preserved. The older cadence terminal auditor is FULL-only.

Actual audit owner/request/launch/terminal evidence: `sc-22844-paths-terminal-audit-admission-9228de30-d550-4a92-8447-826eee06493b`. Owner recipe `3fdb13ea2e77170a5b78c13c90392b50c17f886bf266c2d7e5f5a0ed94748844`; owner result `82347ace9d2d6834abda1231dc45d75ef534fbd08430cf38e9f41b1d3024d1e6`.

## Completed sidecar metadata measurement

- Actual exec session **36036** exited 0 and was reaped under frozen `edit_campaign.invoke`; no remaining processes or errors. Owner elapsed 24.944 seconds, measured scanner 24.853 seconds, peak 48,431,104 bytes.
- Admission directory: `sc-22844-sidecar-stat-admission-2a7bfb06-f5e7-4f35-88a4-684815d5a3ef`.
- Request SHA `ca1f41ef7697e1f367331848ce32d073993afcc2079e7cd264374b4075e42550`; owner recipe SHA `56d4592361ed678bfd515f73ae1f7ea80da3c568c3e55b0e08ca92d370b2603f`; root admission SHA `3dceff5ae1fe63e0f8c510d5137e68e977756f31343b3a00dd3eb7c2c4e0ca6e`.
- Completed output: `sc-22844-sidecar-stat-result-2a7bfb06-f5e7-4f35-88a4-684815d5a3ef/measurement.json`.
- Source `sc-22844-sidecar-stat-source-v1-f6u2n08v/measure.py`, SHA `2ac83fc83bdb0c1d79811a5b3cf1364c4594c808c10d7924573441e27dcbda89`.
- Independent source review `sc-22844-sidecar-stat-independent-h8eynvlj/review.json`, SHA `3cd52dcbf6bc8c6a004b370d2453f40849383fb5fb52784e1e4341a3ec1808cb`;19 synthetic tests pass, earlier case-insensitive fixture failures retained.
- Exactly1,083 existing PATH pages/626,471,719B admitted, max696,131B;1,006,834 rows and4,027,336 prospective literal sidecar lstat calls. One traversal, no DB/photo/sidecar bodies. Original sizes come from recorded path evidence; four literal sidecar probes preserve alias multiplicity and can coincide with the original pathname.
- Inner limits600s/512MiB,8MiB/page,8GiB cumulative pages,8192 pages,1.1M rows,4.4M stats,64MiB prerequisite metadata,2MiB output. Owner610s/512MiB,32GiB reserve, one process, bounded logs/telemetry. Unknown candidate sizes remain explicit. No automatic retries or packet grant.

- Measurement SHA `12d07f0e51963781ae1e55fc38ae6d2af6a61c44589ec3fae7324bc1b9d70858`, status MEASURED_SIDECAR_METADATA_ONLY. Independent actual review `sc-22844-sidecar-stat-actual-independent-v1/review.json`, SHA `f6875da60cec50a79c6714b57deaed28e645a7c7e0bb8df8d5ceb598d0645d40`, PASS. Owner result SHA `c97c447b2eaa30b577cdf6140511937bbc1d2d7596324e30deeba86ddbd62d22`. Do not rerun this scan.
- All 47 revisions and 4,027,336 literal lstat calls reconcile; no unavailable candidate occurrences. Available original references total 15,160,542,029,800 bytes. The 256 regular sidecar occurrences total 846,812 raw bytes and separately 846,812 identity-decoded bytes, preserving case-insensitive alias multiplicity. Embedded packet quantities remain unmeasured; theoretical caps are not expected storage allocations.

## Current-catalog proposal

Private proposal `/Users/michael/PhotoCatalog-private-results/sc-22844-current-catalog-proposal-9oqhobxn/review.md` and `mapping.json` covers all 16 families and 48 candidates using the hash-verified completed PATHS report. Proposed choices are not applied; external image XMP is unassessed. Independent review passed: `sc-22844-catalog-proposal-independent-2i9wwwlz/review.json`, SHA `b4ef4d4fae9267c7cb4a632662eeea510bc47b9c38358e54f50d320e106c5373`. The user has received an asynchronous concrete choice request; approval is pending. Highlighted lower counts in seven families, especially 2015-2 and Lightroom Catalog-v13-5; internal recency does not prove intentional removals. No source originals/catalogs were read by this presentation step.

Native family selection has no packet-completion prerequisite. The existing runner's all-47 packet roster is implementation sequencing rather than the user's scope: the user requested one current catalog per family. The qualified successor must bind the explicitly chosen roster with new command/output identities, keep the full 47-member prerequisite evidence, and label excluded backup XMP unassessed. Final family digests must be refreshed after enrichment; prior user choices can remain authorized if membership, chosen revision and decision basis are unchanged, but stale digest values cannot be reused. S9 comment23139 records this requirement reconciliation.

## Next actions

1. Complete sc-23137 in `/Users/michael/Repos/PhotoCatalog-worktrees/sc-23137-xmp-single-pass`, branch `codex/sc-23137-xmp-single-pass`, based on verified main `9dbcc76d4e9dfa3dbd626007b4e73698d2995a9e`. Preserve the whole-source digest and exact packet semantics while replacing redundant scans with a held file identity/change-stamp proof. Synthetic mutation and byte-count tests, independent review, all-platform CI and verified merge are required. No photo extraction while this is unresolved.
2. Qualify the successor native explicitly at the PATHS-to-PACKETS boundary. Existing ordinary transitions prohibit changing the frozen native binding; do not overwrite old run config/binary/evidence or silently use new code. Hooke is assessing the smallest explicit transition and compatibility proof. Completed FULL/PATHS and sidecar measurements remain preserved.
3. Finish the seven-category packet funding basis from `sc-22844-packets-funding-fhnxafdz/PROCEDURE.md` and `funding-skeleton.json`. The skeleton is preparation only, with unresolved embedded payload/projection allowances. No new sampling or extraction campaign is admitted. Preserve headroom 181,595,701,248 bytes, protected 8 MiB and 32 GiB reserve unless explicitly revised with evidence.
4. Current inspector source implies 30,321,085,753,224 bytes of full hashing reads including sidecars, before parser reads. This is source-derived I/O volume, not measured runtime or disk allocation. The fix aims to remove one repeated full pass without weakening integrity; runtime remains unmeasured. Actual native setup requests WAL/FULL/fullfsync.
5. After packet qualification, render concrete family review with `sc-22844-family-renderer-v1/render.py` (source `4f2e4a4d2d2bcbd780ccbb8ea1803d5ed1d4231d6bae075fdbfdba0d0929b382`, independent review `c6e77d3a13d3b911809e2bff362555ef46be515dc0bcba9f1e3ca719812e550e`). All16 families/48 candidates, real UTC filesystem dates, unverified internal timestamp units, masters/virtual copies, selected-only conflicts and opaque paths must remain distinguishable. No choices made yet; decisions bind latest evidence digest.
6. Finish S9 delivery/CI/merge and tracker read-back before starting dependent S10 implementation.

## Frozen execution and publication

- Delivery worktree `/Users/michael/Repos/PhotoCatalog-worktrees/sc-22844-delivery`, branch `codex/sc-22844-inspection-delivery`, HEAD `80d56a0eec2beb1ac02a3bb60f09889854ce5e5e`. Current local changes are only these handoff/ledger documents. Do not edit on main or disturb root-worktree ZIPs/.DS_Store.
- Public PR #11 remains draft/open/unmerged; all4 CI34641343286 jobs passed at that exact head. PR #12 merged as `9dbcc76d4e9dfa3dbd626007b4e73698d2995a9e`; exact PR and merged-main checks passed, both bug trackers read back Done. Keep source stationary during actual phases and batch publication rather than pushing every progress checkpoint.
- Native source `ccae8ad46ecd024764e4837cc84c5fe391e43179`; binary SHA `eb541139f7f7888ee5eec58975ad9f36008c65500ee553fa4030d4507c47602f`. Frozen driver `sc-22844-schema3-driver-ccae8ad-v1`, SHA `af2c5e1dcb939713f8296998cc908e87b8e6658a4208b6e6cb12fe3b60132288`.
- Controller `sc-22844-cadence-source-v1-EvaR9L/lightroom_phase_control.py`, SHA `3c689d0cd71b6c19971dbb8c2a52881da2654cc0b26a9f2e125ebcef9b383ac1`; profile SHA `5ccb6aeaf8e0bdfe861cc35a6dfb2ddaddc1b17554ef3815a26ed956c9fcfd19`. Soft3600s, sampled emergency4800s,512MiB Python/1GiB native/1.5GiB combined, existing disk/log/metadata guards. Local control free is separately observed, not continuously guarded by the RAID monitor.
- Native/controller Python remains pinned Homebrew3.14 launcher. Metadata measurement owner uses `/Users/michael/PhotoCatalog-private-results/sc-22843-private-python-v1/env/bin/python` with psutil/blake3; bare Homebrew Python lacks psutil. Frozen owner is `sc-22843-service-smoke-driver-v6/helpers/scripts/edit_campaign.py`, SHA `6765732e148bf711a7311e5e231713accf51bff2ec20721af4caac3c762ced3f`.
- Both SQLITE_TMPDIR and TMPDIR point to `/Volumes/MichaelJon/PhotoCatalog-Scratch/sc-22844-schema3-20260911/sqlite-temp` for native/controller work. Preserve bound device/inode.

## User and evidence boundaries

Originals `/Volumes/MichaelJon/Raw` and Lightroom catalogs `/Volumes/MichaelJon/Catalogs` remain read-only. User resumed RAID use and authorized dedicated scratch; if disconnect is requested, wind down/reap owned work before declaring it available. Repository is public by the user's choice. Rust core stays UI-independent, Tauri targets all three desktop platforms, and Fieldbook is a design reference rather than the product name. No GPU work is needed for the current inspection.

[Execution ledger](/Users/michael/Repos/PhotoCatalog-worktrees/sc-22844-delivery/docs/EXECUTION_LEDGER.md) retains historical checkpoints, failures, review chains and earlier delivery evidence. Current source of truth remains the live Shortcut epic/stories and exact local/remote receipts.
