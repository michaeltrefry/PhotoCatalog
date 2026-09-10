# Preview configuration selection

The Stage A blind judgments were sealed before inspecting codec mappings on 2026-09-09. All 120 groups and 1,080 candidates have retained judgments: 597 acceptable, 339 unacceptable, and 144 needing review. The combined receipt SHA-256 is `19e6343fd245b57aefef396a893a6c0e4ada2433336c2f20bc6dd21ef69d744f`; the unblinded matrix is `34bc66482caa561755c88c187a44ab6221e58870c7193f3db88caaec132b4200`. These private artifacts preserve every group, label, verdict and reason.

## Applicability decision

The alpha (3×1), orientation (3×2), and transfer (6×1) preparation fixtures cannot demonstrate perceptual texture quality. Their independent exact preparation oracles remain mandatory; their visual judgments remain `needs_review`. An independently inspected, nearly black source JPEG is likewise visually uninformative. These four inputs remain in all integrity, encoded-byte and cost evidence. This explicit applicability decision does not convert their judgments into passes or remove recorded failures. Each selected configuration passed the other 26 informative inputs with zero visual rejections. No all-30 visual-pass claim is made.

The frozen G15 RAW reference contains the subsequently repaired S3 highlight-color defect. Its judgments describe additional compression loss relative to those frozen pixels, not production color accuracy. The repaired production renderer subsequently passed separate qualification: 16 changed groups were inspected and 44 unchanged groups inherited through exact reference/selected-RGB hashes. Each tier retained 26 acceptable and 4 uninformative judgments, with no visual rejections. Frozen Stage A artifacts remain unchanged.

## Pair frozen for Stage B

| Role | Configuration | Corpus encoded bytes | Median file bytes | Median of per-file decode medians |
| --- | --- | ---: | ---: | ---: |
| Retained thumbnail | JPEG, quality 80, longest edge 512 | 1,332,031 | 30,409 | 0.406 ms |
| Evictable larger preview | JPEG, quality 80, longest edge 1600 | 6,559,455 | 184,050 | 2.956 ms |

Preparation, no-upscaling behavior, JPEG YUV444 settings and production decode remain exactly as frozen in PREVIEW_EXPERIMENT_PROTOCOL.md. There are no camera-specific settings. JPEG 512/80 was the sole thumbnail configuration without an observed visual rejection. JPEG 2560/65 also passed the 26 informative inputs, but its corpus bytes were 10,313,953 and its median of per-file decode medians was 5.849 ms. The smaller 1600/80 configuration therefore provides the lower measured footprint and decode cost among eligible larger previews. A full-resolution editing view remains a separate renderer requirement.

Equal weighting of the compatibility corpus gives about 44.4 GB of encoded thumbnails per million assets, excluding allocation and catalog overhead. This is a corpus projection, not a prediction of the user's image distribution. The larger preview cache is evictable rather than multiplied by library size.

Confidence is medium for comparative visual quality and exploratory codec cost. Stage A recorded concurrent background activity, so those timings do not establish a quiet-host performance budget. Stage B subsequently measured retained-storage overhead, bounded page decode/cache behavior and the fixed headless interaction costs; cancellation/recovery also passed correctness checks. Tauri frame-path validation remains required in S12. The private `selected-pair-v1.json` receipt binds this decision before Stage B execution.


Flat is now selected from the completed 10k/100k four-group layout experiment.
[PREVIEW_LAYOUT_RESULTS.md](PREVIEW_LAYOUT_RESULTS.md) retains all 24 distributions,
footprints, actual host conditions and the authoritative corrected decision.
[PREVIEW_SERVICE.md](PREVIEW_SERVICE.md#selected-settings-and-explicit-disk-capacity)
documents the adopted 2164 MiB worker reservation and complete illustrative
64 GiB retained/16 GiB larger-cache configuration. These decisions preserve the
existing JPEG80/512/1600 quality choice. Navigation and integrated 10M RSS
passed both fixed profiles; [PREVIEW_RUNTIME_RESULTS.md](PREVIEW_RUNTIME_RESULTS.md)
records all distributions and host limitations. The complete configuration
example passed actual CLI admission. Final exact-head cross-platform CI and
delivery remain required; the subsequently changed default constant awaits that
final CI gate.
