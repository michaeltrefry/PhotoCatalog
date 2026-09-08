# Original-image decoding and color contract (sc-22838)

`media::decode_full(path)` reads an original and returns oriented, full-resolution
`RenderedImage` pixels: linear sRGB primaries/D65, f32 RGBA, straight alpha.
Negative and above-one RGB values survive the working surface. The result records
decoder versions, source precision, selected color policy and compatibility notes.
No catalog preview, embedded JPEG or disk thumbnail enters this editor API.
`DecodeError.status` separates unsupported variants, corrupt data, resource limits
and filesystem errors. Import retains failures visibly in the existing catalog.

`media::decode` is the browsing entry point. CR2 retains the explicitly labelled
embedded-JPEG fast path from the skeleton; other formats derive a thumbnail from
the original renderer. `srgb_preview` composites against white in linear light,
encodes sRGB, clips to SDR and compresses JPEG. This display operation does not
alter the editor's float pixels. Preview codec/cache selection belongs to sc-22841.

## Decoder selection

| Input | Component and actual behavior |
| --- | --- |
| CR2/RAF/RW2 and recognized camera RAW | LibRaw; full-size demosaic into normalized camera RGB with unit WB, then as-shot WB and camera matrix applied in unclamped float. Sensor normalization still clips samples above sensor white. Missing WB is reported rather than guessed. |
| DNG, including floating/linear/JPEG-XL | Adobe DNG SDK 1.7.1 build 2724: original Stage1→Stage2→Stage3, required opcodes, default crop, as-shot neutral, camera profile's camera-to-PCS matrix and embedded HueSatMap calibration, then linear sRGB. Float samples and separate transparency mask are preserved. |
| JPEG/PNG/WebP/BMP/TIFF | image-rs; original precision converted to float. ICC RGB profiles use LittleCMS relative-colorimetric conversion with alpha copied and negative values allowed. |
| AVIF | libavif, single active decoder thread, 16-bit RGB conversion retaining 8/10/12-bit source precision and straight alpha; ICC preferred, otherwise declared CICP. Integer clean aperture and rotation/mirror are applied. |
| PSD | Bounded merged-composite reader for RGB/grayscale 8/16/32-bit, raw, PackBits and ZIP/prediction. ICC resource retained for transform. Negative layer counts and Mtrn/Mt16/Mt32 tags identify merged transparency; extra spot channels are not mistaken for alpha. RGB merged white matting is removed before ICC conversion. Files declaring no real merged composite are Unsupported. |

Untagged raster RGB assumes sRGB, explicitly recorded. PNG gAMA/cHRM generate a
source profile; sRGB overrides these. Non-RGB ICC profiles and unsupported PNG
CICP, PSD color modes/PSB, fractional AVIF apertures/non-square pixels and unknown
color encodings fail explicitly. DNG spatial ProfileGainTableMap calibration is
currently Unsupported, including maps supplied by an embedded profile; it is never
silently skipped. TIFF/PSD layer editing is not implemented.

The DNG SDK profile matrix incorporates ForwardMatrix, calibration signatures,
analog balance and illuminant interpolation. Embedded HueSatMap calibration is
interpolated for the as-shot white and applied by the SDK in linear ProPhoto before
conversion to working sRGB. Its transfer-encoding tables are honored. Display
LookTable and tone curves remain outside this scene-linear input policy; their
presence is reported. Original RAW is selected and a separate AI/enhanced rendition
is reported rather than silently substituted. These are explicit choices, not
claims of Adobe rendering parity. Source packets and imported develop records
are retained separately in sc-22839/45.

SDR calibration tables have a [0,1] RGB domain. Negative or over-one ProPhoto
samples bypass that table and retain their matrix-rendered values; no hidden
clamp destroys these samples. For profiles explicitly marked HDR with a
value-dimensional LUT, the SDK's overrange encoding is used; negative samples
still bypass the LUT. SDK's 2.5D table routine clips over-one values even with its
overrange flag, so that flag is not used to claim unsupported HDR behavior.
The renderer records applied and bypassed pixel counts and the domain policy.
In-domain LUT value/saturation behavior follows the profile and SDK, including
profile-defined saturation/value bounds. Subsequent working-gamut conversion
retains negative and above-one sRGB values.

## Native build and provenance

Set `PHOTOCATALOG_DNG_SDK` to the SDK directory containing `dng_sdk/source`:

```
python3 scripts/fetch_dng_sdk.py --destination .deps
```

The download is pinned to SHA256
`740fbe95c69e09e9cd17654a5e4fef2d7021254b06fd2b8c5557b79a1496b50c`.
Sources stay outside Git. Linux requires LibRaw≥0.21, libavif≥1.0,
libjxl/libjxl_threads≥0.11, libjpeg and zlib development packages. Ubuntu 24.04's
libjxl is too old; build the SDK-bundled 0.11.2 source or install a compatible
development package. macOS uses Homebrew libraw/libavif/jpeg-xl. Windows uses the
pinned vcpkg manifest with x64-windows-static-md. Rust remains the public backend;
the C++ ABI contains owned pixel buffers and plain scalars, no vendor struct offsets.

SDK builds use explicit platform flags, thread safety, JPEG support and XMP disabled
for this rendering dependency. Two missing upstream qDNGUseXMP guards in
`dng_jxl.cpp` are applied to a generated build copy with exact-match checks; the
download remains unchanged. XMP wrapper and metadata-write translation units are
excluded. Host NeedsMeta remains enabled because it also gates camera neutral and
profile parsing. The render dependency never writes source XMP.

Licenses: image-rs MIT/Apache-2.0; LibRaw LGPL-2.1 or CDDL-1.0; LittleCMS MIT;
libavif BSD-2-Clause and selected codec licenses; libjxl BSD-3-Clause with bundled
third-party notices; Adobe SDK license is copied under `third-party/`.
The SDK permits use/modification/distribution subject to its included terms.
Packaging must carry the applicable source/binary notices and selected LibRaw
license obligations; this development build does not certify a release bundle.

## Validation and limits

Generated CI tests cover format swatches, alpha, independent matrix math with
negative/above-one output, analytic nonidentity HueSatMap calibration and explicit
out-of-domain bypass, 16-bit distinction, all orientations, profile conversion,
corruption and repeatability. `examples/render_probe.rs` emits dimensions, finite
statistics, alpha counts, pixel digest and provenance; optional preview output uses
create-new semantics. Private corpus testing stays outside Git and includes real
Canon/Fujifilm/Panasonic/DJI records. Camera support is established by per-file
validation, not a filename extension or this component list.

Encoded source limit is 512 MiB; output is limited to 100 megapixels/40,000 pixels
per axis. LibRaw RAW allocations and individual SDK allocations are capped at
768 MiB. These are bounded editing limits, not the browse-memory budget. Working
buffers can coexist; the preview scheduler must account for the complete worker
peak before admitting concurrent jobs. Native build/codec versions and host load
must accompany timing claims. Set `OMP_NUM_THREADS=1` when validating a LibRaw
build with OpenMP; the later worker scheduler must control internal parallelism.

Cross-platform CI and independent private-corpus review remain required before
closing this story. Format support and these explicit rendering limitations need
to be reconciled with the epic's actual replacement workflow.
