# Preview layout results

Source: `e535d00a28ebbbb62c240dc00bbefc937272c555`. Campaign SHA256 `a218b31481ff061fcdbdcfb3b27ed5ddf970a7647a96a69b418b05b538912123`. Reconciliation SHA256 `1bc3415f47ca59ee681aece43e31c9629ec0ab0763f08a11093048a545102e90`.

All four preparations and four lookup children passed. Every one of 24 passes verified all N distinct actual payload hashes, COM entry identity, unchanged original seed bytes, and actual file cardinality. Total 220,000 distinct stored objects across four groups; 1,320,000 timed lookups. No retry, replacement sample, original render, or source RAID access. Seed and bound-input hashes remained unchanged.

Latency is milliseconds per production manifest lookup + encoded file read + checksum, excluding full-image pixel decode and the independent COM/seed oracle. Oracle time is excluded from lookup samples and included in pass wall. Passes 0–2 are sequential; 3–5 use seeds 22841–22843.

| Count | Layout | Pass | n | p50 ms | p95 ms | p99 ms | max ms |
|---:|---|---:|---:|---:|---:|---:|---:|
| 10000 | flat | 0 | 10000 | 0.143083 | 0.297375 | 0.358375 | 30.772000 |
| 10000 | flat | 1 | 10000 | 0.037125 | 0.079084 | 0.180834 | 16.752000 |
| 10000 | flat | 2 | 10000 | 0.037458 | 0.079000 | 0.181625 | 13.703208 |
| 10000 | flat | 3 | 10000 | 0.040000 | 0.082250 | 0.186708 | 29.854667 |
| 10000 | flat | 4 | 10000 | 0.040125 | 0.082208 | 0.187042 | 24.245375 |
| 10000 | flat | 5 | 10000 | 0.040000 | 0.082917 | 0.185584 | 24.160917 |
| 10000 | hash-prefix | 0 | 10000 | 0.145708 | 0.299667 | 0.362583 | 18.256125 |
| 10000 | hash-prefix | 1 | 10000 | 0.037958 | 0.079875 | 0.182792 | 19.994917 |
| 10000 | hash-prefix | 2 | 10000 | 0.037875 | 0.079792 | 0.183000 | 13.586125 |
| 10000 | hash-prefix | 3 | 10000 | 0.040709 | 0.083750 | 0.186667 | 24.116125 |
| 10000 | hash-prefix | 4 | 10000 | 0.040750 | 0.083459 | 0.186834 | 24.020542 |
| 10000 | hash-prefix | 5 | 10000 | 0.040292 | 0.082250 | 0.184875 | 23.891667 |
| 100000 | flat | 0 | 100000 | 0.150042 | 0.310792 | 0.360292 | 31.483708 |
| 100000 | flat | 1 | 100000 | 0.041084 | 0.082167 | 0.183708 | 18.690792 |
| 100000 | flat | 2 | 100000 | 0.041125 | 0.082125 | 0.184125 | 17.289750 |
| 100000 | flat | 3 | 100000 | 0.045500 | 0.087667 | 0.190584 | 38.444042 |
| 100000 | flat | 4 | 100000 | 0.045458 | 0.088042 | 0.191291 | 36.759208 |
| 100000 | flat | 5 | 100000 | 0.045708 | 0.087834 | 0.191750 | 90.758167 |
| 100000 | hash-prefix | 0 | 100000 | 0.150917 | 0.313083 | 0.362209 | 35.961834 |
| 100000 | hash-prefix | 1 | 100000 | 0.042625 | 0.083166 | 0.184958 | 14.556625 |
| 100000 | hash-prefix | 2 | 100000 | 0.042459 | 0.083000 | 0.185333 | 17.828875 |
| 100000 | hash-prefix | 3 | 100000 | 0.046959 | 0.088958 | 0.192292 | 74.580375 |
| 100000 | hash-prefix | 4 | 100000 | 0.046959 | 0.089000 | 0.193500 | 81.432334 |
| 100000 | hash-prefix | 5 | 100000 | 0.046750 | 0.088541 | 0.192208 | 34.301500 |

Footprints below are bytes. Thumbnail logical bytes include the owner marker, while encoded payload excludes it. Manifest is the actual post-lookup database/lock footprint. `st_blocks * 512` reports zero directory allocation on this APFS host; this is not proof that directory/B-tree metadata costs zero physical storage. Directory counts and allocation-method limitation are retained. Every before/after footprint is in reconciliation.json.

| Count | Layout | Encoded | Thumbnail logical | Thumbnail allocated | Manifest logical | Manifest allocated | Thumbnail directories | Large logical / allocated | Setup seconds |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 10000 | flat | 439850762 | 439850821 | 462798848 | 26402816 | 27328512 | 1 | 55 / 4096 | 233.803 |
| 10000 | hash-prefix | 439850762 | 439850828 | 462798848 | 26427392 | 27328512 | 9562 | 62 / 4096 | 310.758 |
| 100000 | flat | 4398971762 | 4398971821 | 4628430848 | 263225344 | 265007104 | 1 | 55 / 4096 | 2349.694 |
| 100000 | hash-prefix | 4398971762 | 4398971828 | 4628430848 | 263233536 | 268500992 | 51581 | 62 / 4096 | 2426.511 |

Host observation covers all 24 pass windows, with 1–22 timestamped samples per pass. Per-window CPU/GPU summaries, other-process maxima, disk deltas, and unavailable fields are retained. Process visibility is partial because privileged processes deny access; unknown data never means idle. One-second samples cannot exclude subsecond interference. Whole-host disk deltas can straddle pass boundaries and have no exclusive workload attribution.

| Count | Layout | Host samples across passes | CPU range % | GPU range % | Read delta bytes | Write delta bytes |
|---:|---|---:|---:|---:|---:|---:|
| 10000 | flat | 8 | 4.4–6.5 | 0–1 | 496025600 | 774217728 |
| 10000 | hash-prefix | 8 | 4.8–6.1 | 0–1 | 496828416 | 774905856 |
| 100000 | flat | 89 | 3.7–12.0 | 0–2 | 4904591360 | 12845727744 |
| 100000 | hash-prefix | 90 | 3.7–12.6 | 0–2 | 5011152896 | 12896243712 |

Timing is qualified by observed desktop load, not uniformly quiet or isolated. mediaanalysisd reached 100.96% of one CPU core in the final prefix group, and Codex renderer bursts reached 107.47%; earlier flat passes also saw desktop and media-analysis activity. Preparation additionally included separately logged transient foreign compilers/cleanup and three coordinator-approved tiny Python test intervals. All results are retained; no affected subset or corrected ranking is claimed.

Flat is selected for the next fixed navigation phase by the authoritative parent review below. Both layouts have equal encoded payload and thumbnail-file allocation totals at each scale; Flat avoids 9,561 additional thumbnail directories at 10k and 51,580 additional directories at 100k, while all observed per-pass p50 values are lower. Small latency differences are not a causal speedup claim given fixed ordering and desktop load. The stronger reason is simpler filesystem structure with no demonstrated lookup disadvantage in this experiment. Do not extrapolate the 30-seed synthetic distribution or 100k object population to arbitrary filesystems/millions. Navigation, integrated 10M retained-page RSS, defaults, and final CI remain separate gates. Confidence: high in receipt/count/footprint reconciliation; moderate in the suggested layout choice; low in a precise causal latency difference.


The authoritative decision is private `sc-22841-layout-v1-sy3xj63s/parent-layout-review-v2.json`, SHA256 `38aeb39d9d0fe9453182de1fe4fb0273d8091726dbf1f71c6064b74e332f0957`. Its prior v1 prose incorrectly called the 100k manifest allocation equal; the numeric rows were always correct. The v2 correction preserves all measured evidence. Flat has 3,493,888 fewer allocated manifest bytes at 100k after lookup, while 10k manifest allocations are equal.

The campaign used frozen source `e535d00a28ebbbb62c240dc00bbefc937272c555`, source archive SHA256 `4323717776aa4926ccb411f362a05328a8358cecf0ec928cc969c6ba0c48bbff`, and layout binary SHA256 `c44bc0720fa9ad885a2c61cada561b4a81ba1450c841ab0b54d150c3fd22cc35`. Private build identity is `sc-22841-stage-b-release-v2-gvj3ffrx/build-reference.json`, SHA256 `2dbed204e0ec4b01cc555889e73021f42eb0016fa4ff9252160ba6a7f63b6155`. No private images or raw samples are committed.

The Windows worker lease repair at `0a9daf610fa17e4f0a655e43549207fbd4d2ad64` changes staging retirement/recovery/startup, while its relocation change is test-only. Independent source review confirms that the measured layout read path is unchanged. It does not authorize inheriting native lifecycle timing or claiming the old binary's HWM as a measurement of the repaired binary. Retained navigation must begin with empty worker staging and admit zero native jobs to use that source-applicability boundary.
