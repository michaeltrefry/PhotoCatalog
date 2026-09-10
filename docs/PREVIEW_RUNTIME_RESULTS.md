# Retained preview navigation and integrated catalog results

The fixed Flat/JPEG80 retained-preview campaigns completed without retries. Both profiles passed the existing warm 200-thumbnail page p95 ≤1000 ms target and process high-water ≤4 GiB target. The integrated campaign uses a complete 10M-asset catalog and a 10k-object retained cache. These are headless core measurements, not desktop frame-time evidence or a claim that all 10M originals have cached thumbnails. Final exact-head CI and delivery remain separate gates.

## Frozen execution and conditions

All measurements use clean source `e535d00a28ebbbb62c240dc00bbefc937272c555`, source archive SHA256 `4323717776aa4926ccb411f362a05328a8358cecf0ec928cc969c6ba0c48bbff`, navigation probe `356536a798ac0a94efdf6eaee4521a45b5c8b459b356306470274074cb39c34e`, and app `c6517282d207d4bc9a1db4391f2cc08c8e9b4ae5d1102f85b813c75f8c9801b9`. The release freeze binds Rust/Cargo 1.98, SQLite 3.51.1, the complete source/native dependency identities, SDK before/after verification and APFS Data storage attribution. Build-reference SHA256 is `2dbed204e0ec4b01cc555889e73021f42eb0016fa4ff9252160ba6a7f63b6155`.

Each measured page uses production Catalog::browse, the retained service request queue, checksum/header validation and actual JPEG decode through fully materialized sRGB RGB8. The timer ends with 200 caller-owned surfaces live. Full pixel/key/dimension verification follows outside the page timer. Cumulative process HWM includes preflight/count scans and in-process oracle work; it is not incremental page memory. Source originals remain offline, all native-job counts are zero, and worker staging was absent at admission and after the campaigns.

Both profiles reserve 2164 MiB per worker within 3 GiB, with one normal worker. Standard limits are 400 requests, 32 MiB encoded staging, 256 MiB decoded LRU/live allowance. Constrained limits are 200 requests, 8 MiB encoded staging, 32 MiB LRU and the same 256 MiB live allowance. Caller-held pixels remain charged beyond LRU eviction. Source decode admission retains the frozen 256 MiB encoded/32M-pixel/768 MiB-allocation ceilings; no renderer runs in these retained-read measurements.

No OS cache purge occurred. Fresh means a new process. Count preflight itself warms catalog pages. Nearest-rank percentiles include every retained sample. Profiles execute in fixed order; small differences between them do not prove a causal speed advantage.

## Ten-thousand-row navigation component

UTC 2026-09-10 02:15:32.954–02:17:57.733. All 44 measured children and 44 verifier children passed: 268 trials, 152,468 ready reads, 132 canceled consumer tickets, zero failed/resource-limited read outcomes. Every completed page verified all 200 identities and pixels. The standard profile canceled 132 navigation tickets; constrained canceled 0 because its actual execution kept up. This is observed behavior, not a guaranteed cancellation count.

| Profile | Workload | n | p50 ms | p95 ms | p99 ms | max ms |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| standard | warmup | 3 | 152.051250 | 206.703500 | 206.703500 | 206.703500 |
| standard | warm | 100 | 117.328834 | 120.914834 | 129.886750 | 131.698542 |
| standard | hot_lru | 1 | 15.018208 | 15.018208 | 15.018208 | 15.018208 |
| standard | fresh | 20 | 122.653500 | 124.026000 | 124.476375 | 124.476375 |
| standard | navigation | 10 | 4991.059625 | 5147.354667 | 5147.354667 | 5147.354667 |
| constrained | warmup | 3 | 117.067750 | 122.705041 | 122.705041 | 122.705041 |
| constrained | warm | 100 | 116.407334 | 117.533833 | 118.083083 | 118.905334 |
| constrained | hot_lru | 1 | 116.160542 | 116.160542 | 116.160542 | 116.160542 |
| constrained | fresh | 20 | 120.610875 | 122.126875 | 122.439125 | 122.439125 |
| constrained | navigation | 10 | 4989.323917 | 4994.701916 | 4994.701916 | 4994.701916 |

Navigation trials contain 100 prescribed viewports at 50 ms intervals, so their wall duration is approximately 5 seconds and includes verification. Viewport scheduling overrun p95/p99/max was 1.626/2.923/11.213 ms standard and 1.652/1.754/15.414 ms constrained. This is a headless scheduling observation, not a frame-time budget. The single constrained hot pass cannot hold the whole page in its 32 MiB LRU and is not an all-cache-hit claim.

| Profile | Page/warm child terminal HWM bytes | Navigation child HWM bytes | Maximum held page pixel bytes |
| --- | ---: | ---: | ---: |
| Standard |139362304|293224448|102817038|
| Constrained |133906432|178225152|102817038|

Host telemetry contains 137 samples. All warm/navigation children have samples, but 32 short fresh-process children have no point sample strictly inside their interval. Surrounding one-second samples remain available and cannot exclude subsecond activity. Observed measured-window CPU reached 28.1%, GPU 2%; Codex renderer reached 131.87% of one CPU core and SystemUIServer 93.87%. Process visibility is partial because some processes deny access. These results do not certify an isolated or uniformly quiet host; no samples were discarded or corrected.

## Integrated ten-million-asset catalog

Untimed preparation exclusively copied the pristine 13,659,504,640-byte schema 5 donor with SHA256 `0ac57eefbfbfeee46e83c17b8116c48ae3cb0dedae01605c1016981a536e91db`. Original 4→5 migration and separate 5→5 verification receipt bytes were retained. All main/companion source hashes and presence states were preserved; no original SQLite connection was opened. Preparation took 238.786 seconds with sampled native peak 393592832 bytes, which is setup evidence, not measured browse HWM.

The overlay republished 10000 byte-distinct retained objects under existing asset IDs and changed only `fingerprint`, `render_generation`, and `preview_hash` in the first 10000 assets. It preserved remaining columns/schema/sequence; connection changes were exactly 20000 and storage_epoch advanced 10000000→10010000. Count remained 10000000. Actual preserved locations matched `/synthetic/folder{sequence%5}/file{sequence:012}.jpg` and `/synthetic` was absent. This is a synthetic preview-identity overlay, not an import or proof of organization fidelity.

UTC 2026-09-10 02:37:29.757–02:38:10.904. Two serial measured children and two verifiers passed all 208 trials and 41600 ready reads. Each profile includes 3 warmups, 100 measured pages and 1 hot page; all source, bound-file and ancestry invariance checks passed.

| Profile | Workload | n | p50 ms | p95 ms | p99 ms | max ms |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| standard | warmup | 3 | 107.836959 | 113.601625 | 113.601625 | 113.601625 |
| standard | warm | 100 | 108.737292 | 111.596250 | 112.983000 | 113.091250 |
| standard | hot_lru | 1 | 12.820166 | 12.820166 | 12.820166 | 12.820166 |
| constrained | warmup | 3 | 109.340292 | 114.985750 | 114.985750 | 114.985750 |
| constrained | warm | 100 | 108.332625 | 110.150750 | 110.768417 | 111.327167 |
| constrained | hot_lru | 1 | 107.924083 | 107.924083 | 107.924083 | 107.924083 |

| Profile | Whole-process terminal HWM bytes | Maximum held page pixel bytes | Warm p95 target | RSS target |
| --- | ---: | ---: | --- | --- |
| standard | 137461760 | 102817038 | PASS ≤1000 ms | PASS ≤4 GiB |
| constrained | 141262848 | 102817038 | PASS ≤1000 ms | PASS ≤4 GiB |

Host telemetry contains 31 samples, 15 in each measured-child interval. CPU maxima were 14.9%/12.7%; GPU 0–1%. Standard included a Codex renderer burst 125.17% of one core; mediaanalysisd reached 24.85%/32.66%. All samples have partial process visibility. These qualified observations retain ordinary competing desktop activity and are not a quiet-host guarantee. The count scan and warmed copied storage remain part of the stated scope.

## Configuration and changed-source applicability

The complete JSON example opened successfully through the frozen real CLI `cache-jobs`, with only its three storage paths and original-root path replaced by private temporary paths. Policy, explicit 2164 MiB worker reservation, all memory settings, Flat layout, 64 GiB retained and 16 GiB large quotas were unchanged. The empty queue returned successfully; no original rendering occurred. This validates actual deserialization/service admission, not capacity for any promised image count or the subsequently changed compiled default constant. Final remote CI covers that constant change.

The source-reviewed Windows retirement repair at `0a9daf610fa17e4f0a655e43549207fbd4d2ad64` passed all four hosted CI jobs 34425527698. It does not change measured retained decode/read paths; both campaigns meet the required empty-staging/zero-native-jobs applicability conditions. Native lifecycle timing is not inherited because retirement adds fsync. Worker memory remains the independently reviewed 30-source calibration, not an exact new-binary HWM guarantee. Final exact-head CI/merge/main-CI/tracker readback remain required.

## Portable private receipt identities

Raw receipts/samples, catalog copies and photos remain outside Git. SHA256 identities:

| Receipt | SHA256 |
| --- | --- |
| navigation campaign | `1750429a060295320c6367d2df4984d6ed437c1d1b01d637b5390148ec668d44` |
| navigation rawsample audit | `e605c08e0a2e55110844ea4a08ea0e3939a8418f158ab9c7ecd57f1e1cda1fc9` |
| parent navigation review | `06f2a1828962dbbc743a8aca87219ba2ac5320dac2b209ca7a8e768d3b86880d` |
| integrated copy | `14f9357c48c0b0810928785c40b7ac3dee9f15844c80e2584c94ee71e8998dbd` |
| integrated overlay | `c91fa8e7e00d8a5270e75ade20f4670d97a1b327ed51596b123ec77760347473` |
| integrated preparation | `6fa469b9557bc0b19e35502848a169334f01819a4ea11838f57ef1412092df8a` |
| integrated campaign | `ba9715a71a4d7d120b6e6022b04991a47082ee99b994dc5ff36b14559367b70f` |
| parent integrated review | `346f95da9616472df87371a65865f524a8c707fb08e4d39dd3d9907758d5ab2f` |
| integrated rawsample audit | `903350abd0f6c5e9e3e0dc27a3d1ee1fea1fd5103db8ff005af0a2f9ff9fb839` |
| configuration smoke | `3176068503a986c7cf17b6356c81057ce1b45c5963343b660d79ed3b8336dfc4` |

Receipt homes: `sc-22841-layout-v1-sy3xj63s/navigation-flat`, `sc-22841-integrated-v1-2vfntelt`, and `sc-22841-config-smoke-_7_1c5ap`. Confidence is high in completed result integrity and the observed budget passes, bounded to this synthetic catalog/retained cohort and recorded desktop load.
