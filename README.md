# PhotoCatalog

A Rust photo catalog and non-destructive editor in development for macOS, Windows, and Linux. The foundation is tracked in [Shortcut epic sc-22835](https://app.shortcut.com/trefry/epic/22835).

## Foundation CLI

The first slice provides a UI-independent library and CLI with provisional SQLite metadata storage and JPEG thumbnails. The database and preview formats will be selected through the epic's benchmarks. This slice recognizes CR2 embedded JPEG previews, JPEG, and PNG; it is not the complete RAW development or editing engine.

Build and check:

```sh
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

Portable tests generate their own images and structural CR2 fixtures. Generated CR2 data exercises the container/recovery path; it is not evidence of general camera support. Real-camera validation is performed separately using private fixture copies outside this repository.

```sh
python3 scripts/validate_private_fixtures.py \
  --binary ./target/debug/photocatalog \
  --fixtures /path/to/private-cr2-and-jpeg-fixtures
```

This check imports, restarts, paginates, retrieves previews, retries without duplication, and verifies unchanged source digests. It writes temporary catalog/output files outside the source folder and emits a receipt without original filenames or photographic metadata values. Do not commit private originals, catalogs, or generated previews.

## Delivery

See [requirements](docs/REQUIREMENTS.md), [implementation plan](docs/IMPLEMENTATION_PLAN.md), and [execution ledger](docs/EXECUTION_LEDGER.md). The live Shortcut epic and stories are the operational source of truth. The first slice does not yet provide the desktop interface, full XMP handling, relinking, editing, Lightroom migration, or backup/restore; those remain open tracked requirements in the epic.
