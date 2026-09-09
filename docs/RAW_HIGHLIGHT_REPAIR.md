# RAW highlight repair (S3)

The previous RAW adapter developed unit-white-balance integer camera RGB, then
applied unequal as-shot multipliers in float. This preserved numeric headroom but
bypassed LibRaw's highlight handling: nominally white, clipped sensor channels
became unequal, and the camera matrix could turn green negative. Display channel
clipping then produced a magenta sky. This was present in the original S3 and S6
reference render, before preview encoding.

The corrected `photocatalog-render-4` path uses camera white balance during
LibRaw development and its documented `highlight=2` blend. It restores the common
headroom scale in float from the actual developed `pre_mul` values, then applies
the camera matrix. Source crop, orientation, precision, alpha and the independent
Adobe DNG SDK path remain covered by the existing full-image tests.

## Policy and primary evidence

LibRaw documents mode 2 as blend, distinct from modes 0 (clip), 1 (unclip), and
3+ (rebuild). Its `scale_colors` divides WB multipliers by their maximum for a
nonzero highlight mode, rather than clipping a minimum-normalized WB image.
The adapter takes the minimum of the actual normalized first three `pre_mul`
values, not a guessed nominal camera WB ratio, and divides output camera samples
by that value in float. This retains negative matrix results and intensities
above 1; it introduces no display tone curve.

The blend uses the weakest balanced-channel ceiling and preserves camera-space
intensity while reducing chroma above that ceiling. It is deliberately
conservative: it can reduce chroma in valid bright colors whose physical sensor
samples are not all clipped. Below that ceiling it does nothing. It cannot
reconstruct lost detail or guarantee the manufacturer's JPEG rendering.

Primary sources, examined at linked LibRaw 0.22.2:

- [API output parameters](https://www.libraw.org/docs/API-datastruct.html):
  `highlight`, linear `gamm`, camera WB and no automatic brightness.
- [scale_colors](https://github.com/LibRaw/LibRaw/blob/0.22.2/src/postprocessing/postprocessing_utils_dcrdefs.cpp#L111):
  camera WB/white-patch/already-balanced handling and maximum-WB normalization.
- [dcraw_process](https://github.com/LibRaw/LibRaw/blob/0.22.2/src/postprocessing/dcraw_process.cpp):
  scale, demosaic, highlight blend, then RGB conversion.
- [blend_highlights](https://github.com/LibRaw/LibRaw/blob/0.22.2/src/postprocessing/postprocessing_aux.cpp#L278):
  camera-space chroma reduction and the weakest-channel ceiling.

## Independent regression contract

`src/media/raw_highlight_tests.rs` generates uncompressed RGGB CFA DNG bytes and
calls the same native `pc_raw` entry point used by CR2/RAF/RW2. These are synthetic
LibRaw adapter tests, not claims of real CR2 coverage and not tests of Adobe DNG.
The fixed IEC sRGB/D65 matrix and source AsShotNeutral define an independent
color oracle. Three illuminants vary the strongest WB channel. Tests cover:

- Neutral 0.125/0.25/0.5 exposure ramp, matching absolute linear intensity.
- Red, green and blue chromatic patches, matching all three expected components
  within 0.003 (allowing 12-bit sample quantization and native matrix rounding).
- Fully clipped sensor white remaining neutral above 1, without false chroma.
- Bright colored patches remaining chromatic and above 1, finite opaque pixels,
  unchanged input bytes, exact output dimensions and linear sRGB declaration.

The test's low-intensity comparisons would reject gamma, a changed exposure
normalization, or a global saturation adjustment. The bright-color check rejects
blanket highlight neutralization; it does not claim exact chroma preservation
above the blend ceiling. Existing DNG signed/HDR matrix, calibration, precision,
orientation and transparency oracles are run separately.

## Validation evidence

Local macOS Rust 1.98.0 / LibRaw 0.22.2 checks: all 115 Rust tests passed,
including the new CFA test and 13 full-image integration tests; package formatting
and all-target Clippy with warnings denied passed. The all-workspace formatting
command also visits the vendored SDK crate's omitted upstream test module; use
the repository CI command `cargo fmt --package photocatalog --check`.

A separately compiled copy of the exact previous native source produces
`(2.000000, 1.000000, 2.500000)` for the synthetic clipped neutral patch. The
corrected native source produces `(1.833333, 1.833333, 1.833333)` at the same pixel;
the 0.125 neutral patch remains approximately 0.125 in both. This directly
reproduces the new regression's failure on the previous production code.

The candidate implementation `4c81da00fd44a0b3935c57da5cfca684d4089d18`
completed all 22 private corpus cases (44 fresh-process renders), including six
LibRaw RAWs. Independently referenced dimensions/crop, source precision, camera
identity, finiteness and alpha coverage passed. Source hashes and modification
times remained unchanged, and each pair of pixel digests matched. All 16 images
using other decoders have exactly the previous S3 pixel digest.

The two pinned public CC0 RAWs (Canon EOS 6D CR2 and Panasonic GX7MK2 RW2) also
passed their source/header checks, with matching pixel digests in separate repeat
renders. Signed/HDR float ranges were retained: approximately -0.308 to 1.747
and -0.060 to 1.373 respectively. Their full previews were visually checked
against the extracted camera JPEGs for expected object colors; camera tone and
exposure differences remain. All six private LibRaw previews were inspected for
gross false highlight colors, including the repaired G15 sky. The exact cropped
G15 sample changes from `(3.089364, -0.049095, 2.960948)` to approximately
`(1.920204, 1.920204, 1.920204)`. Fine edge chromatic aberration is not claimed
corrected. These checks are not a colorimetric calibration certification.

Private receipts, never original photographs in Git:

- `/Users/michael/PhotoCatalog-private-results/sc-22838-highlight-corpus-v1/receipt.json`,
  SHA256 `c159b4fb4c9d06e9233fd505f021d4686658e115911d0b424aad27ae50a90818`.
- `/Users/michael/PhotoCatalog-private-results/sc-22841-source-anomalies-volume/highlight-public.jsonl`,
  SHA256 `e83e0f6f54dc738dce1aee179abe62c606a18e0f44964be030bc35ac6f43bd17`.
- `/Users/michael/PhotoCatalog-private-results/sc-22841-source-anomalies-volume/repair-validation.json`,
  SHA256 `c5f31103ff7eb9fcdea64baf8b6966efbebb1cafa701e031c86466b125e59109`;
  binds exact source and executable hashes, before/after numeric probes, repeat
  comparisons and inspected image identities.

Validation uses the existing `validate_image_corpus.py` and
`validate_public_raw.py` scripts with the built `render_probe`, a new output
directory, four Cargo build jobs and `OMP_NUM_THREADS=1`. These are macOS
correctness results; hosted platform CI remains a separate integration gate. No frozen S6 source,
reference, candidate, timing, quality judgment or metric artifact is overwritten.
Performance qualification and a new preview campaign remain separate gates.
