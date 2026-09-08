# Epic sc-22835 execution ledger

Source of truth: https://app.shortcut.com/trefry/epic/22835

## Current wave

sc-22837 and sc-22838 execute in isolated worktrees after verified completion of sc-22836. Reserve the reference Mac for timed scale measurements after other heavy builds finish.

| Story | State | Work surface / artifact | Evidence | Next action |
| --- | --- | --- | --- | --- |
| sc-22836 | Done | [PR #1](https://github.com/michaeltrefry/PhotoCatalog/pull/1), merged 56f0b37 | Independent review PASS; 18 local tests; real CR2/JPEG source-invariance receipt; PR and merged-main three-platform CI 34227667817 SUCCESS; Shortcut read-back | Complete |
| sc-22837 | In Progress | Benchmark worktree at 3baba21; frozen harness 625d677 | Reviewed harness and supplemental profile driver; six verified 1M/5M/10M preparations and pristine snapshots | Await quiet host, run comparisons, integrate decision |
| sc-22838 | In Progress | [Draft PR #2](https://github.com/michaeltrefry/PhotoCatalog/pull/2) | 29 release Rust tests; independent validator rejection review; public Canon/Panasonic RAW pass on Mac; 22 private header references | Finish spatial DNG calibration/crop/metadata repairs, corpus validation, exact-head review and three-platform CI |
| sc-22839 | To Do | XMP fidelity | Prerequisites sc-22837/sc-22838 | Dependency-bound |
| sc-22840 | To Do | Offline storage and relinking | Prerequisite sc-22837 | Dependency-bound |
| sc-22841 | To Do | Preview storage and scheduling | Prerequisites sc-22837/sc-22838 | Dependency-bound |
| sc-22842 | To Do | Organization and search | Prerequisite sc-22839 | Dependency-bound |
| sc-22843 | To Do | Editing and export | Prerequisites sc-22838/sc-22839/sc-22841 | Dependency-bound |
| sc-22844 | To Do | Lightroom dry run | Prerequisite sc-22839 | Dependency-bound |
| sc-22845 | To Do | Lightroom migration | Prerequisites sc-22840/sc-22842/sc-22843/sc-22844 | Dependency-bound |
| sc-22846 | To Do | Backup and restore | Prerequisites sc-22843/sc-22845 | Dependency-bound |
| sc-22847 | To Do | Desktop UI | Prerequisites sc-22840/sc-22841/sc-22842/sc-22843/sc-22845/sc-22846 | Dependency-bound |
| sc-22848 | To Do | Integrated readiness | Prerequisite sc-22847 | Terminal proof only after integration |

## Resource and authorization ledger

- User authorized epic delivery, including ordinary PR/CI/merge, and selected michaeltrefry for the private PhotoCatalog repository.
- Originals and Lightroom sources on the RAID remain read-only. Private source fixtures are outside Git. No live migration or original mutation has occurred.
- Two isolated implementation lanes, Cargo jobs capped at four each. Final scale timing requires a quiet lane; no GPU work started.
- Initial repository contained only planning documents, now recorded as baseline commit 09f8e9a. No CodeGraph tools/index or root CODEGRAPH.md were available.
- Native Windows/Linux behavior requires actual remote CI evidence; local Mac tests are not equivalent.
- sc-22836 independent review passed exact head 8c56058; merged tree is identical. Future material stories require fresh independent review before merge and Done.

## Current evidence boundaries

- Database preparation is complete, but foreground timing has not started. The separate SceneWorks workload is not owned by this epic and has not been stopped. Preserve the pristine snapshots before any mixed writes.
- The original benchmark's all-cache-profile summary is diagnostic. Production eligibility uses one declared configuration across all scales, with the unchanged latency, durability and actual RSS budgets; see the sc-22837 supplemental protocol.
- S3 preflight reproduced and repaired AVIF orientation precedence, PSD transparency and missing-composite semantics, and private-validator format mislabeling. Subsequent crop and source-metadata differences remain under repair.
- Private source headers independently identify precision, intended dimensions and camera metadata. Two DNG fixtures derive from JPEG/TIFF and do not establish native camera RAW coverage. Private images and evidence remain outside Git.
- PR #2 CI at 4192217 passed macOS tests and public RAW validation; Linux/Windows and the final corrected head remain unverified at this checkpoint. No S3 completion claim.

## Foundation risk register

Incomplete previews after interruption; duplicate asset creation on retry; source changes during extraction; generated fixtures falsely standing in for real CR2 compatibility; private images or metadata being committed; unbounded directory/file reads; platform differences in path and file publication semantics. Validate these within sc-22836's scope before closeout.
