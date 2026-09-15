# Disposable Windows and Linux validation fixture itinerary

This pack supports the installed LensWorks workflows required by [sc-23641](https://app.shortcut.com/trefry/story/23641) and [sc-23642](https://app.shortcut.com/trefry/story/23642). It supplements the complete acceptance criteria in those stories and the Windows checklist in `docs/WINDOWS_DESKTOP_ACCEPTANCE.md`; it does not replace either source of truth.

The immutable baseline contains generated raster/DNG/XMP/Lightroom inputs and one checksum-pinned public Canon CR2. It contains no private photos or catalogs. Generated images establish routing, persistence, source reconciliation, offline, relink, and recovery behavior. They do not establish real-camera compatibility, numerical color accuracy, or subjective image quality.

## Build the immutable portable baseline

Run from a reviewed source checkout. Python 3.9 or newer, FFmpeg with MJPEG/BMP/TIFF encoders, and the WebP `cwebp` tool are required. Use `--ffmpeg PATH` and `--cwebp PATH` when needed.
On Windows, use `py -3` in place of `python3` if that is the installed launcher.

```text
python3 scripts/build_validation_fixture_pack.py
python3 scripts/build_validation_fixture_pack.py --verify-only
```

The default baseline is `.deps/validation-fixtures/lensworks-cross-platform-v1`, ignored by Git. Use `--raw-cache PATH` to reuse a local `validate_public_raw.py` download and `--offline` to prohibit downloads. Replacement requires `--replace` and the builder ownership marker.

The baseline Lightroom catalogs contain the portable sentinel `__LENSWORKS_WORKING_ORIGINALS_ROOT__/`, never a builder-machine path. `fixture-manifest.json` records every baseline source size/SHA-256, public CR2 license/source/checksum, generator versions, exact source-row counts, and readiness flags. `fixture-manifest.sha256` seals it.

## Prepare and freeze a destination-host working set

Never test in the immutable baseline. On the destination Windows or Linux host, materialize a separate working set; this copies inputs, replaces the catalog sentinel with that host's absolute working-originals path, and hashes the rebound catalogs and copied inputs before any installed workflow runs.

```text
python3 scripts/build_validation_fixture_pack.py --prepare-working-copy .deps/validation-fixtures/lensworks-cross-platform-v1
python3 scripts/build_validation_fixture_pack.py --verify-working-copy
```

The default working set is `.deps/validation-fixtures/lensworks-working-v1`. Its `working-manifest.json` records the baseline-manifest digest, destination-host root binding, every input hash at freeze, and the two XMP sidecars as the only authorized mutable source paths. Mutable exports, backups, restores, previews, recovery probes, and relink targets live under `outputs/` and are excluded from immutable input hashes.

After approved XMP publication, verify all immutable inputs while explicitly reporting sidecar changes:

```text
python3 scripts/build_validation_fixture_pack.py --verify-working-copy --allow-authorized-sidecar-changes
```

After offline/relink moves the originals, also pass their current root:

```text
python3 scripts/build_validation_fixture_pack.py --verify-working-copy --current-originals-root .deps/validation-fixtures/lensworks-working-v1/outputs/relink/originals --allow-authorized-sidecar-changes
```

The baseline can always be verified independently at its original paths. A changed or missing photo/catalog input fails working verification; only a declared sidecar digest may differ with the explicit flag.

## Source inventory and qualification boundary

- The baseline has 8 unique original paths: one each of CR2, DNG, JPEG, PNG, AVIF, WebP, BMP, and TIFF, beneath `originals/2014/January/2014-01-02` and `originals/2015/February/2015-02-03`.
- Two XMP sidecars contain an unknown namespace property, language-qualified title, array, and nested resource. Their title/rating values differ from selected-catalog packets.
- Lightroom discovery has four candidates in two families. Select `2014-v13-2.lrcat` and `2015-v13.lrcat`; exclude `2014-v13.lrcat` and `2015-v13-3.lrcat`. The mtimes show that a numeric suffix is not uniformly current.
- The selected source catalogs contain exactly 9 file rows, 11 image/variant rows, 2 virtual-copy rows, 5 keywords, 5 collections, 10 collection memberships, 2 history rows, 2 snapshots, and 4 catalog XMP packets. One Canon path appears in both selected catalogs, so those rows reference 8 unique original paths.

These are source facts, not destination acceptance counts. Direct import followed by Lightroom migration into the same catalog has not yet established how many destination asset, variant, or XMP-source records the installed product must expose. `expected-reconciliation.json` therefore leaves target counts pending. Before platform handoff, the coordinator must run the root native qualification, fill `target-qualification.template.json` as separate reviewed evidence, and supply it with the final installer/package. A missing or pending target qualification blocks sc-23641/sc-23642 acceptance.

Do not mark handoff ready until the coordinator also supplies the final installer/package, its SHA-256, exact source commit, and accepted target qualification. Baseline and working manifests deliberately leave installer, target-count, platform, native-workflow, and release-handoff flags false.

## Installed workflow order

1. Record OS distribution/version, architecture, RAM, display scaling/session, package filename/SHA-256, exact source commit, baseline and working manifest SHA-256 values, and storage type. Install and launch as an ordinary user without a source checkout or terminal dependency.
2. From a new catalog outside the working set, import `inputs/originals`, cancel once after visible progress, then repeat. Record actual destination asset/variant/XMP counts and compare them with the coordinator's qualified target evidence. Reopen and recheck. Do not infer destination counts from source-row totals.
3. Inspect all four `inputs/lightroom-catalogs` candidates while the bound `inputs/originals` root is still present. Explicitly select the two listed current members, compare dry-run source counts with `expected-reconciliation.json`, and migrate into the same catalog used in step 2. Repeat/resume once. Require the coordinator-qualified destination counts, zero excluded-catalog records, stable shared-path handling, both virtual copies, five collections, four retained catalog packets, and explicit compatibility status.
4. Now inspect the resulting sidecar/catalog and catalog/catalog metadata conflicts. Confirm unknown properties remain visible with provenance. Change an approved field, review the existing sidecar destination, and publish. Record the authorized sidecar's before/after digest; unrelated photo inputs and the other sidecar remain unchanged. Run working verification with `--allow-authorized-sidecar-changes`.
5. Browse every format and both nested date trees. Search/filter, rate, flag, add/remove a keyword, create a collection, and reopen. Exercise an edit plus undo/redo and a virtual copy. Export JPEG, PNG, and TIFF into `outputs/export`; first use fresh names, then exercise explicit existing-destination review with `outputs/export/existing-export.jpg`.
6. Close the app, move `inputs/originals` to `outputs/originals-offline`, and reopen. Require retained thumbnails plus unavailable-original status. Move it again to `outputs/relink/originals`, relink its parent, and verify both year/month/date branches. Exercise relink undo, then reapply the mapping. Lightroom migration has already completed, so its frozen pre-relocation root binding is not reused after this move.
7. Review preview storage, change the large-preview budget, and relocate previews to `outputs/previews`, outside current originals. Pause/resume, reopen, and verify thumbnails. A preview destination inside originals must be refused.
8. Create and inspect a backup in `outputs/backup`; restore into fresh `outputs/restore`. Reopen it and compare assets, variants, edits, collections, and XMP provenance with the supplied qualified target evidence. Restored jobs remain held until explicitly released; relink the restored catalog to `outputs/relink/originals` if needed.
9. Exercise recovery with a missing original, a manually write-protected copy of `outputs/recovery/unwritable-destination`, the existing export sentinel, cancel/retry, and restart. Require actionable errors, no silent write replay, no incomplete backup publication, and no overwrite without explicit review.
10. Complete the platform story's keyboard, focus, readability, screenshot, performance, worker-close, and package checks. Verify the immutable baseline again. Verify the working set using `--current-originals-root outputs/relink/originals` and `--allow-authorized-sidecar-changes`; attach the reported sidecar digest changes, both manifests, qualification evidence, and completed story checklist. A Not tested row remains open.

For a repeat run, prepare a new working set and destination catalog. Preserve the first run's baseline/working manifests and results separately so retry evidence cannot overwrite them.
