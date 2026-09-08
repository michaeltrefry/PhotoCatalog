# Epic sc-22835 execution ledger

Source of truth: https://app.shortcut.com/trefry/epic/22835

## Current wave

sc-22836 only. Complete its local acceptance, independent review, exact-head cross-platform CI, merge, and tracker read-back before admitting dependent stories. sc-22837 and sc-22838 become the next frontier after that merge.

| Story | State | Work surface / artifact | Evidence | Next action |
| --- | --- | --- | --- | --- |
| sc-22836 | In Progress | Rust core, CLI, portable fixtures, CI | Requirements and live workflow reconciled; private CR2/JPEG fixture copies hash-verified | Implement, independently review, validate and merge |
| sc-22837 | To Do | Backend benchmark and selection | Prerequisite sc-22836 | Wait for foundation |
| sc-22838 | To Do | Decoding and color pipeline | Prerequisite sc-22836 | Wait for foundation |
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
- One implementation Cargo lane, jobs capped at four, in an isolated sc-22836 worktree. No GPU or scale-benchmark lane started.
- Initial repository contained only planning documents, now recorded as baseline commit 09f8e9a. No CodeGraph tools/index or root CODEGRAPH.md were available.
- Native Windows/Linux behavior requires actual remote CI evidence; local Mac tests are not equivalent.
- Independent review is pending implementation; no story has been marked Done.

## Foundation risk register

Incomplete previews after interruption; duplicate asset creation on retry; source changes during extraction; generated fixtures falsely standing in for real CR2 compatibility; private images or metadata being committed; unbounded directory/file reads; platform differences in path and file publication semantics. Validate these within sc-22836's scope before closeout.
