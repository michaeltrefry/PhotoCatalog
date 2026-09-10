# S6 acceptance ledger

S6 remains **In Progress**. S7 is Done at merged
`e68d37500ca87f057d56137376fe5aeab0fc419b`, with all four main-CI jobs in
34418548263 passed and Shortcut closeout/readback recorded in comment 22955.
This ledger reconciles the three sc-22841 criteria in
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md#sc-22841--deliver-compressed-previews-with-bounded-storage-and-responsive-scheduling).
Frozen protocols and historical receipts retain their original source identities.

## Completed evidence

| Evidence | Exact scope and result |
| --- | --- |
| Integrated local correctness | The final v4 receipt for frozen `e535d00a28ebbbb62c240dc00bbefc937272c555` reconciles 261 Rust passes, 0 failures, 3 ignored, across 27 suites; 28 scripts and 52 benchmark Python contracts passed. The final CLI-only repair passed 18 affected tests, package fmt and all-target Clippy. Earlier compile/fixture/Clippy failures are preserved. Package rebuild isolation prevented sibling-worktree stale artifacts. |
| S7 integration and donor contracts | Shared catalog writer admission is acquired once: import reserve/failure/publication/recovery use Background; visible work uses Foreground. Stable preview references and the actual priority/parser/CLI regressions passed. Schema5 donor checks preserve original 4-to-5 ancestry and separately bind the pristine 5-to-5 verification. Small adversarial tests and the separate fixed 10M retained-preview campaign passed; the latter is a disclosed synthetic three-column overlay, not an organization-fidelity audit. |
| Release freeze | Three e535d00 release binaries, whole clean source archive, protocols, inputs, native versions, APFS Data attribution and 159 SDK source checks were bound; SDK before/after checks matched. Parent freeze audit passed. |
| JPEG/WebP/AVIF comparison | Stage A completed with artifact chains, preserved failures and sealed blinded review. Timing is exploratory under recorded background load, not a verified quiet-host budget result; no favorable subset or corrected ranking is claimed. |
| Selected codec/tier quality | JPEG80 at 512/1600. Current-renderer qualification inspected 16 changed groups and inherited 44 unchanged groups through exact reference/selected-RGB hashes. Per tier: 26 acceptable, 0 unacceptable, 4 uninformative, with those labels preserved. No Adobe-equivalence claim. |
| Worker memory calibration | Thirty sources, native workers and verifiers completed with unchanged source hashes. Largest observed native HWM was 1,760,837,632 bytes. The reviewed reservation plus margin is 2,269,118,464 bytes (2164 MiB), adopted by ServiceLimits within a 3 GiB total and one normal worker. This is inherited cohort calibration, not exact repaired-binary HWM, OS enforcement or a future-camera guarantee. |
| Layout | Four preparation and four lookup children completed; 220,000 distinct stored objects and 1,320,000 timed lookups across 24 fixed passes. COM/seed-byte and unique-content/file-count proofs passed, raw samples independently reconciled, all inputs preserved, zero retries. Flat was selected by the authoritative corrected parent review. Full distributions, allocation counts and desktop-load limitations are in [PREVIEW_LAYOUT_RESULTS.md](PREVIEW_LAYOUT_RESULTS.md). |
| Flat retained navigation | All 44 measured children and 44 verifiers passed 268 trials and 152,468 ready reads. Warm page p95 was 120.915/117.534 ms standard/constrained; maximum process HWM was 293,224,448/178,225,152 bytes. Offline/ownership/pixel oracles passed with zero native jobs; parent independently audited raw receipts. Host sampling limitations remain explicit. |
| Integrated 10M browsing | Two measured children and two verifiers passed all 208 trials and 41,600 reads. Warm p95 was 111.596/110.151 ms and whole-process HWM 137,461,760/141,262,848 bytes, passing both existing budgets. Exact count/schema/offline paths and three-column/20k-DML overlay proof passed; original main/companion and migration ancestry remained unchanged. Parent independent audit passed. |
| Explicit configuration | The complete example, with only private temporary storage/original-root paths substituted, opened through the actual frozen CLI and service. Flat, JPEG80/512/1600, explicit 2164 MiB reservation, memory limits and illustrative 64 GiB retained/16 GiB large quotas were preserved. It validates configuration loading/admission, not the changed compiled default constant or universal storage capacity. |
| Offline/import/priority | Actual children render both tiers; imports publish and later read without originals. Incremental import yields to retained foreground reads while a native lease exists. Two worker slots under the frozen allowance admit one, queue the other and progress after release. |
| Resource/cancellation | Shared request/completion admission, foreground ordering, held-pixel accounting beyond LRU eviction, typed encoded/decoded pressure, release/retry progress, maximum-u64 arithmetic and same-key resubmission before reap passed. Actual cancel/owner EOF evidence uses an admitted post-decode checkpoint; it is not deterministic interruption inside a codec. |
| Quota/fault | The large LRU never evicts retained thumbnails. Capacity refusal preserves fallback. Injected StorageFull follows a real partial write; actual SQLite FULL uses max_page_count and verifies integrity, no provisional state, capacity restoration and retry. Truncated checksummed output and an actually killed Unix helper's partial output are rejected. This does not mean the host filesystem was filled. |
| Revision/crash/relocation | Catalog generation/fingerprint rejects stale completion; known nonpixel edits retain previews. Journal tests cover manifest attachment and catalog commit boundaries. Relocation resumes copying/switch/cleanup, preserves foreign files and handles marker-admission interruption. Source-overlap tests include omitted roots and actual relinking into a relocated cache. |

Private receipt identities use portable names; no original images or raw samples
are committed:

- `sc-22841-final-correctness-v4/receipt.json`: SHA256 `f20c7d1628f931d1718476fe094c906dff6a611edcb8b1ba120feb9371f699a7`.
- `sc-22841-stage-b-release-v2-gvj3ffrx/build-reference.json`: SHA256 `2dbed204e0ec4b01cc555889e73021f42eb0016fa4ff9252160ba6a7f63b6155`.
- `sc-22841-memory-v1-imt0b3ko/campaign/campaign.json`: SHA256 `b02edc6043ecb78d254be097fa765811a81a3234cd4ac110b4c4326b47e2321f`; independent audit `a21f2d6a4719e574f53c179a2552c826b20e52d8b5aa646097203251ca705f1b`.
- `sc-22841-current-quality-v1-q3reqlv0/current-renderer-quality-decision.json`: SHA256 `6dd8a185fc3f8e5fe363fd7eb46886288fd4ac025d5cbc22e3c290b5dcd6906d`.
- `sc-22841-layout-v1-sy3xj63s/campaign/campaign.json`: SHA256 `a218b31481ff061fcdbdcfb3b27ed5ddf970a7647a96a69b418b05b538912123`.
- `sc-22841-layout-v1-sy3xj63s/reconciliation.json`: SHA256 `1bc3415f47ca59ee681aece43e31c9629ec0ab0763f08a11093048a545102e90`.
- `sc-22841-layout-v1-sy3xj63s/parent-layout-review-v2.json`: SHA256 `38aeb39d9d0fe9453182de1fe4fb0273d8091726dbf1f71c6064b74e332f0957`; v2 corrects prior selection prose, not numeric evidence.

Runtime distributions, exact identities, preparation invariants and host limitations are
recorded in [PREVIEW_RUNTIME_RESULTS.md](PREVIEW_RUNTIME_RESULTS.md).

## Remaining gates

| Gate | Required work and acceptance boundary |
| --- | --- |
| Current default correctness | Original e535d00 Windows failures are repaired at `0a9daf610fa17e4f0a655e43549207fbd4d2ad64`: independent source review and all four terminal hosted jobs in run `34425527698` passed. Parent audited eight affected worker/relocation tests and public RAW validation; Shortcut comment22964 records the evidence. The explicit 2164 MiB configuration passed actual CLI admission and both measured profiles. The subsequently changed compiled default constant still requires final remote CI on the exact delivery head. No new host-disk-fill experiment is required. |
| Final review/delivery | Independent exact-head review, final applicable macOS/Linux/Windows CI, parent-authorized merge, terminal main CI and Shortcut readback. Draft PR8 is not delivery proof. No S6 Done from local tests or component timings alone. |

The Windows repair leaves codec/preparation/renderer and measured layout reads
unchanged. Its native staging retirement adds durable I/O, so native rendering,
cancellation and recovery timings cannot be inherited. Retained navigation
applicability requires empty worker staging at startup and zero native jobs; keep
the exact frozen source/binary identities. Existing memory evidence remains
calibration rather than a measurement of the changed executable.

Current pixel recipe revision zero is explicit. S8 must provide actual recipe
values and matching identity; arbitrary nonzero edits are not silently rendered
as unedited images. Retained decode cancellation is cooperative between decoder
calls; native original rendering uses a killable child. S12 owns desktop frame
time. Relocation uses hard links on the destination filesystem; filesystems
without hard links, such as exFAT, are not qualified cache destinations. Failures
preserve readable/source copies. Confidence is high in completed correctness and
integrity evidence, moderate in the bounded layout decision, and high for the observed navigation/integrated budget passes within their
fixed synthetic cohorts and recorded host conditions. Final delivery is still pending.
