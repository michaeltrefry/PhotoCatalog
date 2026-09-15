# Windows desktop acceptance

Use this checklist with the final installer and fixture pack supplied for the
LensWorks foundation epic (sc-22835). A successful installer build alone does
not complete these checks. The coordinator supplies the release link, source
commit, installer SHA-256, and fixture expectations before the test begins.

## Record the environment

- Installer filename, SHA-256, and source commit:
- Windows version, CPU architecture, RAM, and display scaling:
- Display and selected color profile:
- Original-files location (local, removable, or network):

Use the supplied disposable fixture pack and a new catalog. It includes expected
photo counts, metadata, migration reconciliation counts, and color targets. Keep
the report's filenames and screenshots limited to those test fixtures.

## Test the installed app

Mark each row **Pass**, **Fail**, or **Not tested**. For a failure, record the step,
what happened, and whether closing/reopening changed the result.

| Check | Expected result | Result |
| --- | --- | --- |
| Install, launch, quit, reopen | Installer completes; ordinary-user launch opens LensWorks; no terminal or developer tools are needed. Quit closes the app and its workers. | |
| Create one catalog and import the fixture folders | All expected photos appear in their existing nested year/month/date folders, with progress and a clear completion state. Canceling and repeating an import does not duplicate photos. | |
| Browse and organize | Scroll, select, search, filter, rate, flag, and edit keywords. Results and counts match the fixture; changes persist after reopen. | |
| Inspect supported formats | CR2, DNG, JPEG, PNG, AVIF, WebP, BMP, and TIFF fixtures show the expected orientation, transparency, and metadata. Tagged color targets look consistent with the supplied reference; record any profile-dependent difference. | |
| Edit and export | Make an edit and virtual copy, then undo/redo. Export JPEG, PNG, and TIFF and inspect the outputs. Originals remain unchanged. Existing destinations require explicit review before replacement. Cancel/retry has an understandable result. | |
| Disconnect and relink originals | With the fixture original folder unavailable, retained previews remain visible and originals are reported unavailable. Repoint its parent folder, confirm the association, and verify every nested child. Relink undo restores the previous association. | |
| Metadata and XMP | Inspect retained/unknown metadata and Adobe compatibility notices. Edit the supplied fields, review an existing sidecar before approving replacement, publish, and exercise the supplied recovery case. Reopen preserves the expected metadata. | |
| Preview storage | Review the originals boundary, change the large-preview budget, relocate previews into a new folder, pause/resume, and reopen. Progress is visible; thumbnails still work. A destination inside originals is refused. | |
| Lightroom migration | Inspect the supplied catalogs, explicitly select the intended family revisions, review approval and overlaps, then import into the same destination catalog. Counts match the reconciliation sheet. Exercise the supplied supplement/repair case and cancel/retry without duplicate import. | |
| Backup and restore | Create and inspect a backup; restore into a new destination. Reopen it and verify photos, edits, and metadata. Restored background jobs remain held until explicitly released. | |
| Recovery and errors | Exercise the supplied missing-file and unwritable-destination cases. The app explains the problem and offers a usable retry or recovery action; closing/reopening does not silently repeat a write. | |
| Keyboard and readability | Navigate the grid and dialogs with the keyboard; focus remains visible. Escape closes appropriate dialogs. Labels, error text, and controls remain usable at the recorded display scaling and with Windows accessibility settings. | |

## Return the result

Send the completed table and any failure details. Include screenshots of the
library grid, editing view, migration reconciliation, and any unexpected error.
Record whether the final layout and wording are acceptable or what should change.
**Not tested** remains an open acceptance item; it is not counted as a pass.
