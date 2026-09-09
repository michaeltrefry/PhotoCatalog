# S6 acceptance ledger

Checkpoint: S6 source integration of merged S7 e68d37500ca87f057d56137376fe5aeab0fc419b.
Prior f9b2905 source review passed; this integration awaits review and runtime gates. S6 remains **In Progress**. This ledger reconciles the three sc-22841
criteria in [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md#sc-22841--deliver-compressed-previews-with-bounded-storage-and-responsive-scheduling).
It does not replace the frozen experiment protocols or historical receipts.
Confidence: high for the evidence boundaries below; runtime confidence for the
new integrated probe remains unestablished.

## Completed evidence

| Evidence | Exact scope and result |
| --- | --- |
| Local correctness at a2f33b7e1ecdc6c293913376adc002cf57ab9d12 | Package rebuilt to avoid stale artifacts. 242 Rust tests passed, 0 failed, 3 ignored, across 25 suites; 18 preview Python, 39 benchmark Python, and 5 corpus Python contracts passed. Package fmt and all-target Clippy passed. Focused selections: preview library 30, service 17, layout 3, navigation 2. These are not full measured campaigns. |
| JPEG/WebP/AVIF comparison | Frozen Stage A completed with preserved failures/artifact chains and blinded review. Timing is exploratory under recorded background load, not verified quiet-host budget evidence; no favorable subset or corrected ranking is claimed. |
| Selected codec/tier quality | JPEG80, edges 512/1600 selected. Current renderer qualification completed: 16 changed groups visually inspected; 44 unchanged groups inherited through exact reference/selected-RGB identities. Per tier: 26 acceptable, 0 unacceptable, 4 uninformative; the four remain uninformative. No Adobe-equivalence claim. |
| Full-worker memory | All 30 sources, 30 native workers and 30 verifiers completed; source hashes preserved. Largest observed native HWM 1,760,837,632 bytes. Frozen Stage B reservation is 2,269,118,464 bytes (2164 MiB), total renderer allowance 3 GiB, one normally admitted worker. This is a cohort accounting profile, not OS-enforced RSS or a guarantee for every supported camera. |
| Offline/import/priority correctness | Actual child renders both tiers; import publishes and later reads without originals. Incremental import yields to a retained foreground read while the actual native import lease exists. Two configured worker slots admit only one under the frozen allowance, queue the second and progress after release. |
| Resource/cancellation correctness | Shared native/read request admission, foreground ordering, held-pixel accounting beyond LRU eviction, typed encoded/decoded pressure, release/retry progress, maximum-u64 admission arithmetic, last-consumer cancel and same-key resubmission before reap passed. Native cancel/owner EOF are proven at an admitted post-decode checkpoint, not as deterministic interruption inside a codec. |
| Quota/fault correctness | Separate large LRU budget never evicts retained thumbnails; retained quota refusal preserves fallback. Injected StorageFull follows a real partial staging write; actual SQLite FULL is induced by max_page_count, checks integrity/no provisional state, then capacity restoration and retry. A checksummed truncated worker image is rejected; an actually killed Unix helper's partial output is not published. This does not claim the host filesystem was filled. |
| Revision/crash/relocation correctness | Catalog-authoritative generation/fingerprint rejects stale completion, known nonpixel edits retain valid previews, cross-store journal interruption/restart tests cover manifest attachment, catalog commit boundaries and journal removal. Relocation resumes copying/switch/cleanup, handles admission-marker interruption and rejects foreign roots. Source-overlap guards include omitted configured roots and an actual relink into relocated cache. |

Private receipt identities (portable names; absolute user paths are not committed):

- `sc-22841-service-navigation-gate-v1/receipt.json`: SHA256 `d74c43fa956616cf960ff87dbb8ead523e6d4c81f0a7e703248458a0ff8f8390`.
- `sc-22841-memory-v1-imt0b3ko/campaign/campaign.json`: SHA256 `b02edc6043ecb78d254be097fa765811a81a3234cd4ac110b4c4326b47e2321f`; independent audit `a21f2d6a4719e574f53c179a2552c826b20e52d8b5aa646097203251ca705f1b`.
- `sc-22841-current-quality-v1-q3reqlv0/current-renderer-quality-decision.json`: SHA256 `6dd8a185fc3f8e5fe363fd7eb46886288fd4ac025d5cbc22e3c290b5dcd6906d`.

Older source-checkpoint documents describe then-pending tests/memory/quality.
The identities above establish their completed scope without rewriting old evidence.

## Remaining gates, in execution order

| Gate | Exact remaining work | Acceptance boundary |
| --- | --- | --- |
| S7 integration | Source integrated: shared admission acquired once in the preview transaction helper; import reserve/failure/publication/recovery and background submission use Background, visible submission and ordinary final generation authority use Foreground. Preserve previous preview references. Review and run the added priority regression. | Current correctness predates this change. No claim that native-worker priority alone prevents catalog-writer starvation. |
| Integrated donor/schema reconciliation | Source reconciled to schema 5 plus exact lens/capture index. Protocol2 preserves original 4-to-5 ancestry and binds pristine schema5 donor bytes; optional later 5-to-5 proof stays separate. Review and run new ancestry rejection tests. | **Execution remains blocked until integration review/gates/build freeze.** Reject schema4 before Catalog::open; no migration during measurement. No original SQLite connections. |
| Current-head correctness and build freeze | Run the nine source-ready integrated tests and added catalog-writer priority/parser regressions plus affected shared fixture/layout/navigation tests, then complete relevant Rust/Python/fmt/Clippy gates. Freeze clean whole-source archive, actual release app/probes, native dependency versions, storage and input receipts. | The merged source has formatting/syntax inspection only; no new runtime test or campaign results. Rebuild the package after checkout switches. |
| Layout comparison | `preview_layout_campaign.py`: 10k flat, 10k prefix, 100k flat, 100k prefix; four preparation and four measured lookup children. Six fixed passes each (three sequential, three seeded random), zero retries. | All four groups and 24 pass distributions; exactly N distinct actual files/read-content hashes per pass, seed+COM oracle, identical payload totals across layouts, allocated file/directory/manifest/index overhead. Select layout from the full evidence; no automatic winner or synthetic-distribution forecast. |
| Retained service/navigation | `preview_navigation_campaign.py`: **44 measured children + 44 untimed verifiers**. Each profile: one warm child (3 warmups +100 measured +1 hot), 20 fresh-process children, one child with ten fixed 100-viewport navigation traces. Standard and constrained profiles. | Actual production queue/read path, all ownership/pixel oracles, raw timings/late-work/resource classifications, offline originals and complete host telemetry. Warm headless page p95 ≤1000 ms. Fresh process is not cold OS cache. Ten-thousand-row RSS is component evidence only. |
| Integrated 10M retained browsing | After schema reconciliation, `preview_integrated_campaign.py`: exclusive source/companion copy, bounded three-column 10k identity overlay, then **2 measured warm children +2 untimed verifiers** (standard/constrained), each 3 warmups +100 measured +1 hot. | Exact 10M count; preserved stable IDs and actual offline paths; held 200 views; zero native jobs; source invariance. Report all 208 trial observations, warm p95 ≤1000 ms and entire browse process HWM ≤4 GiB per profile. No UI frame-time award. |
| Integrated fault/quota gate | Rerun existing fault, concurrent-import, stale completion, journal recovery and relocation tests at the final integrated head, including native CI. Preserve the distinction between injected filesystem failure and actual SQLite capacity failure. | Tests already passed at a2f33b7; new writer/schema/default integration must not regress them. No new performance threshold or unrequested host-disk fill campaign. |
| Final defaults/configuration decision | Combine reviewed layout, quality, footprint and page/RSS results. Record selected layout, explicit retained/larger quotas, cache locations, request/cache/live-memory budgets, worker reservation and supported-source resource-retry behavior. Make shipped defaults/configuration examples agree with that decision. | Codec/edges are selected; storage/resource defaults remain provisional. `ServiceLimits::default()` currently reserves 3 GiB per worker, while the frozen experiment uses 2164 MiB. Thumbnail quota exhaustion reports retained-capacity pressure rather than evicting offline coverage. Configurable larger allowance must retain the full decoder's supported-source capability. |
| Final review and delivery | Independent review of exact integrated head/evidence; parent-authorized PR, terminal applicable macOS/Linux/Windows CI, merge, terminal main CI and Shortcut readback. | Source workflow contains native WebP provisioning, all-target tests/Clippy and Linux preview Python-contract discovery. Workflow source is not execution proof. No S6 Done from local tests alone. |

## Coverage limits requiring explicit final-review attention

No additional unimplemented story-local mechanism was identified in this bounded
source pass beyond the integration/default/measurement work listed above. Three
limits must remain explicit rather than being mistaken for completed coverage:

- The current pixel recipe is revision zero. Keys include edit revision, but S8
  must supply actual recipe data and matching identity when editing is added;
  arbitrary nonzero recipe revisions are not silently rendered as unedited images.
- Native Windows/Linux execution of the current S6 worker/recovery/relocation
  paths is still pending. The killed-partial helper test is Unix-only. Relocation
  publishes through hard links on the destination filesystem; support on filesystems
  without hard links (for example exFAT) has not been qualified. Source paths remain
  preserved on failure; do not claim arbitrary cache-filesystem compatibility.
- Cancellation of retained in-process decode is cooperative between decoder calls;
  original rendering uses a killable child. Navigation must report the observed
  latency under that contract. S12 owns desktop/UI time; the headless components
  cannot satisfy the UI portion of the one-second budget by omission.
