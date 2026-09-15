# Disposable Windows and Linux validation fixture itinerary

This pack supports the installed LensWorks workflows required by [sc-23641](https://app.shortcut.com/trefry/story/23641) and [sc-23642](https://app.shortcut.com/trefry/story/23642). It supplements the complete acceptance criteria in those stories and the Windows checklist in `docs/WINDOWS_DESKTOP_ACCEPTANCE.md`; it does not replace either source of truth.

The pack contains generated raster/DNG/XMP/Lightroom inputs and one checksum-pinned public Canon CR2. It contains no private photos or catalogs. Generated images establish routing, persistence, reconciliation, offline, relink, recovery, and format handling. They do not establish real-camera compatibility, numerical color accuracy, or subjective image quality. Those remain final native/installer qualification work.

## Build and verify

Run from a reviewed source checkout. Python 3.9 or newer, FFmpeg with MJPEG/BMP/TIFF encoders, and the WebP `cwebp` tool are required. Use `--ffmpeg PATH` and `--cwebp PATH` when they are not on `PATH`.

```text
python3 scripts/build_validation_fixture_pack.py
python3 scripts/build_validation_fixture_pack.py --verify-only
```

The default output is `.deps/validation-fixtures/lensworks-cross-platform-v1`, which is ignored by Git. Use `--raw-cache PATH` to reuse a local `validate_public_raw.py` download. Use `--offline` to prohibit downloads. A rebuild of a nonempty pack requires `--replace`, and replacement is refused unless the builder ownership marker is present.

Before testing, inspect `fixture-manifest.json`, verify `fixture-manifest.sha256`, and retain both with the result. The manifest records every source file size and SHA-256, public CR2 license/source/checksum, generator versions, expected counts, and readiness flags. Catalog absolute roots are generated from the chosen output on the current machine; no repository script contains a user or machine path.

Do not mark the handoff ready until the coordinator supplies the final installer/package, its SHA-256, and its exact source commit, and the installed native workflows complete. The generated manifest deliberately leaves all final installer, platform completion, and release-handoff flags false.

## Expected inventory and reconciliation

- Direct import has 8 unique originals: one each of CR2, DNG, JPEG, PNG, AVIF, WebP, BMP, and TIFF. They live below `originals/2014/January/2014-01-02` and `originals/2015/February/2015-02-03`.
- Two XMP sidecars contain unknown namespace properties, a language-qualified title, an array, and a nested resource. Their title/rating values conflict with the selected catalogs. Source hashes must remain unchanged through every workflow.
- Lightroom discovery has four candidates in two families. Select `2014-v13-2.lrcat` and `2015-v13.lrcat`; exclude `2014-v13.lrcat` and `2015-v13-3.lrcat`. The mtimes intentionally show that a numeric suffix is not uniformly the current catalog.
- The two selected catalogs contain 9 file rows, 11 image/variant rows, 2 virtual copies, 5 keywords, 5 collections, 10 collection memberships, 2 history rows, 2 snapshots, and 4 catalog XMP packets. One Canon original appears in both selected catalogs, reconciling to 8 unique assets while all 11 source variants remain accounted for. The two excluded catalogs contribute zero imported records.
- `expected-reconciliation.json` and the identical manifest section are the machine-readable authority. Record any difference as a failure; do not revise expected values during the run.

## Installed workflow order

1. Record OS distribution/version, architecture, RAM, display scaling/session, package filename/SHA-256, exact source commit, pack-manifest SHA-256, and whether the originals directory is local, removable, or network-backed. Install and launch as an ordinary user without a source checkout or terminal dependency.
2. Create a new catalog outside the fixture pack. Import `originals`, cancel once after visible progress, then repeat. Require 8 unique assets, no duplicate assets, two retained sidecar packets, and one master variant per asset. Reopen and recheck counts.
3. Browse every format and both nested date trees. Search/filter, rate, flag, add/remove a keyword, create a collection, and reopen. Exercise an edit plus undo/redo and a virtual copy. Export JPEG, PNG, and TIFF into `destinations/export`; first use fresh names, then exercise explicit existing-destination review with `recovery/existing-export.jpg`. Hash originals before and after.
4. Inspect the Canon and DNG XMP sources. Confirm sidecar/catalog conflicts and unknown properties remain visible with provenance. Change an approved field, review the existing sidecar destination, publish, and use the product recovery action if publication is interrupted. Confirm unrelated unknown properties remain and ordinary import/edit never rewrites originals.
5. Close the app, rename `originals` to `originals-offline`, and reopen. Require retained thumbnails plus unavailable-original status. Move that directory to `destinations/relink/originals`, relink its parent, and verify both year/month/date branches. Exercise relink undo, then reapply the mapping. Use the relocated root for the remaining steps.
6. Review preview storage, change the large-preview budget, and relocate previews to a fresh disposable directory outside the relocated originals. Pause/resume, reopen, and verify thumbnails. A preview destination inside originals must be refused.
7. Inspect all four Lightroom candidates, explicitly select the two listed current members, and compare the dry-run report with `expected-reconciliation.json`. Import into the same destination catalog. Repeat/resume once and require unchanged counts, no excluded-catalog records, stable overlap resolution, both virtual copies, all five collections, four catalog XMP packets, and explicit compatibility status for retained Adobe data.
8. Create and inspect a backup in `destinations/backup`; restore into the fresh `destinations/restore`. Reopen the restored catalog, verify the 8 assets, 11 accounted migration variants, edits, collections, XMP provenance/conflicts, and held background jobs. Explicitly release held jobs, then relink the restored catalog to `destinations/relink/originals` if needed.
9. Exercise recovery with a missing original, a manually write-protected copy of `recovery/unwritable-destination`, an existing export name, cancel/retry, and app restart. Never alter the fixture sources to manufacture the condition. Require actionable errors, no silent write replay, no incomplete backup publication, and no overwrite without explicit review.
10. Complete the platform story's keyboard, focus, readability, screenshot, performance, worker-close, and package/installer checks. Run `--verify-only` again against the pack and attach the result plus the completed story checklist. A Not tested row remains open.

For a repeat run, create a new destination catalog and fresh export/backup/restore directories. Rebuild with `--replace` only when a clean byte-for-byte source reset is required. Preserve the first run's manifest and result separately so retry evidence cannot overwrite it.
