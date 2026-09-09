# Epic sc-22835 execution ledger

Source of truth: https://app.shortcut.com/trefry/epic/22835

## Current wave

sc-22836 and sc-22838 are verified Done. The user released the reference Mac on 2026-09-09, and sc-22837's controlled comparison is running in its isolated worktree. The frozen harness, supplemental driver and native probe hashes match the saved checkpoint. No local build/render/GPU workload runs alongside timing; ordinary desktop load is recorded. The remaining stories depend on the database decision.

| Story | State | Work surface / artifact | Evidence | Next action |
| --- | --- | --- | --- | --- |
| sc-22836 | Done | [PR #1](https://github.com/michaeltrefry/PhotoCatalog/pull/1), merged 56f0b37 | Independent review PASS; 18 local tests; real CR2/JPEG source-invariance receipt; PR and merged-main three-platform CI 34227667817 SUCCESS; Shortcut read-back | Complete |
| sc-22837 | In Progress | [Draft PR #3](https://github.com/michaeltrefry/PhotoCatalog/pull/3); frozen harness 625d677 | Hosted CI 34350272575 SUCCESS at bc7c453; independent review of completed 1M/5M receipts; default-profile failures retained at 5M | Finish comparisons and runtime-work evidence, integrate decision |
| sc-22838 | Done | [Merged PR #2](https://github.com/michaeltrefry/PhotoCatalog/pull/2), a0bf374 | Independent review PASS; 31 release tests; 22 private samples/44 deterministic renders; strict public RAW checks; PR CI 34237848026 and merged-main CI 34242815786 SUCCESS on three platforms; Shortcut read-back | Complete |
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

- Database preparation and verified pristine snapshots are complete. The original comparison began on 2026-09-09 after the user released the Mac. The prior SceneWorks model workload is gone; no foreign workload was stopped. Live telemetry is `/Users/michael/PhotoCatalog-private-results/sc-22837-measurement-20260909-host.jsonl`.
- At 1M, both engines pass the default 256 MiB page/fresh/write/RSS and recovery checks with matching data. At 5M, SQLite rating-page warm p95 is 126.031 ms (>100 ms) and DuckDB's 256 MiB mixed workload fails with OOM and incomplete samples. These results are preliminary; no backend is selected. Original receipts are under `/Users/michael/PhotoCatalog-private-results/sc-22837-final-v2`.
- The original benchmark's all-cache-profile summary is diagnostic. Production eligibility uses one declared configuration across all scales, with the unchanged latency, durability and actual RSS budgets; see the sc-22837 supplemental protocol.
- S3 preflight reproduced and repaired AVIF orientation precedence, PSD transparency and missing-composite semantics, and private-validator format mislabeling. Subsequent crop, source-metadata and spatial DNG calibration repairs passed independent review and the full private corpus.
- Private source headers independently identify precision, intended dimensions and camera metadata. Two DNG fixtures derive from JPEG/TIFF and do not establish native camera RAW coverage. Private images and evidence remain outside Git.
- PR #2 final reviewed head 2d30647 passed three-platform CI 34237848026. Merged a0bf374 has the identical tree; its CI 34242815786 is terminal SUCCESS on all three platforms. Shortcut comment 22874 records acceptance evidence, and read-back confirms sc-22838 Done. The epic remains In Progress.
- The original native probe is frozen outside the checkout at `/Users/michael/PhotoCatalog-private-results/sc-22837-frozen-v2-native/catalog_probe`, SHA-256 `cfbe4dcce0f3f176af369e1c685f40bce45d5ae77c259603ab6d78948b6084a6`. Use that binary for the frozen campaign; separately validate production integration against the updated branch. The harness and supplemental driver hashes remain unchanged.
- The bounded host observer passed independent review after fixing process-ID reuse attribution. Its 10-second smoke observed GPU utilization at 100%; it is diagnostic competing-load evidence, not a timing result or permission to stop another workload. Benchmark-contract CI exercises small Python tests only.
- Read-only `EXPLAIN` collected eight plans per engine at each scale without executing the photo queries. SQLite plans indexed page access (and a temporary rating-page sort); DuckDB plans scans and hash joins. Plans alone do not establish latency or backend eligibility. Private evidence: `/Users/michael/PhotoCatalog-private-results/sc-22837-readonly-plan-preflight.json`.

## Measurement commands and checkpoint

Reconcile live Shortcut, Git state, process ownership, and receipts before resuming anything. Do not repeat a running or completed measurement. The original command below was started on 2026-09-09; its stderr progress is `/Users/michael/PhotoCatalog-private-results/sc-22837-final-v2-20260909.stderr.log`. The supplemental command has not yet run at this checkpoint. Retain passive host observations with the private receipts; see `benchmarks/observe_host.py` and the benchmark README.

From the `sc-22837` worktree, use the existing environment and original frozen probe:

```sh
/Users/michael/PhotoCatalog-private-results/sc-22837-env/bin/python benchmarks/catalog_benchmark.py campaign \
  --root /Users/michael/PhotoCatalog-private-results/sc-22837-final-v2 \
  --counts 1000000,5000000,10000000 --repetitions 100 --fresh-repetitions 20 \
  --native-probe /Users/michael/PhotoCatalog-private-results/sc-22837-frozen-v2-native/catalog_probe \
  --resume-prepared

/Users/michael/PhotoCatalog-private-results/sc-22837-env/bin/python benchmarks/production_profiles.py run \
  --source /Users/michael/PhotoCatalog-private-results/sc-22837-production-pristine \
  --output /Users/michael/PhotoCatalog-private-results/sc-22837-production-profiles
```

The pristine snapshot has already been independently verified. Keep it intact; the supplemental driver creates separate working copies. Preserve every failure, the original 64/256 MiB diagnostic matrix, and the predeclared 256/1024/2048 MiB production selection order. Then review raw results, select or escalate the backend decision, integrate the chosen settings into Rust, and validate native behavior before PR/CI/merge and Done. Fresh-process measurements do not establish cold OS-cache behavior.

## Foundation risk register

Incomplete previews after interruption; duplicate asset creation on retry; source changes during extraction; generated fixtures falsely standing in for real CR2 compatibility; private images or metadata being committed; unbounded directory/file reads; platform differences in path and file publication semantics. Validate these within sc-22836's scope before closeout.
