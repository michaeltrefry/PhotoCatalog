# PhotoCatalog

A Rust photo catalog and non-destructive editor in development for macOS, Windows, and Linux. The foundation is tracked in [Shortcut epic sc-22835](https://app.shortcut.com/trefry/epic/22835).

## Foundation CLI

The foundation provides a UI-independent library and CLI with SQLite metadata storage and provisional JPEG thumbnails. The [database decision](docs/BACKEND_DECISION.md) records the measured configuration and its limits; preview formats retain their own benchmark story. The image pipeline adds full-resolution RAW/DNG development and common raster/PSD composite decoding, with explicit color/precision provenance and compatibility limits. See [image pipeline](docs/IMAGE_PIPELINE.md) for the supported variants and native build requirements. XMP source retention, conflict review, catalog-only metadata edits, and controlled export are described in [metadata workflow](docs/XMP_METADATA.md). Editing recipes and the desktop interface remain tracked work.

Install the native dependencies documented in the image pipeline, fetch the verified SDK, then build and check:

```sh
python3 scripts/fetch_dng_sdk.py --destination .deps
export PHOTOCATALOG_DNG_SDK="$PWD/.deps/dng_sdk_1_7_1_2724/dng_sdk_1_7_1"
cargo build --locked
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Import an explicitly selected folder into a new catalog directory, then browse after a process restart:

```sh
photocatalog --catalog /path/to/catalog import /path/to/photo-folder
photocatalog --catalog /path/to/catalog browse --limit 100
photocatalog --catalog /path/to/catalog browse --after 100 --limit 100
photocatalog --catalog /path/to/catalog get PHOTO_UUID
photocatalog --catalog /path/to/catalog preview PHOTO_UUID /path/to/new-preview.jpg
```

Use the last returned `sequence` as the `--after` cursor, rather than an offset. JSON responses expose each entry's processing state; failed or pending entries must not be treated as ready photos. Repeating the same import resumes unfinished work. `import --max-files N` bounds a processing run. Original files are read only; the catalog and preview files are created under the selected catalog directory. Use a separate output directory for preview extraction.

## Validation and private fixtures

Portable tests use generated raster images, a mathematical DNG profile/mask oracle and structural CR2 fixtures. Generated CR2 data exercises the browsing container/recovery path; it is not evidence of general camera support. Full-quality `media::decode_full` never develops these synthetic CR2 containers as actual sensor RAW. Real-camera validation is performed separately using private fixture copies outside this repository.

```sh
python3 scripts/validate_private_fixtures.py \
  --binary ./target/debug/photocatalog \
  --fixtures /path/to/private-cr2-and-jpeg-fixtures
```

This check imports, restarts, paginates, retrieves previews, retries without duplication, and verifies unchanged source digests. It writes temporary catalog/output files outside the source folder and emits a receipt without original filenames or photographic metadata values. Do not commit private originals, catalogs, or generated previews.

## Delivery

See [requirements](docs/REQUIREMENTS.md), [implementation plan](docs/IMPLEMENTATION_PLAN.md), and [execution ledger](docs/EXECUTION_LEDGER.md). The live Shortcut epic and stories are the operational source of truth. The current foundation does not yet provide the desktop interface, relinking, photo-edit recipes, Lightroom migration, or backup/restore; those remain open tracked requirements in the epic.
