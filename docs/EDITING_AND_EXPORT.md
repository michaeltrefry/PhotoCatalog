# Editing and photo export

The UI-independent Rust APIs and CLI are implemented on the S8 branch. This is an
implementation guide, not a completed qualification report. Full corpus, large-image,
performance and three-platform gates remain open in [sc-22843](https://app.shortcut.com/trefry/story/22843).
The desktop interface is a later dependent slice of the same epic.

## Recipes and variants

`Catalog::edit_variant` exposes an implicit neutral `master` without writing a row.
Creating a variant gives it an independent identity and history. Every save, undo
and redo requires the expected revision; revisions increase even when undo returns
to an earlier recipe. A preview or export from the earlier revision cannot become
current through an undo/redo cycle.

The CLI provides `edit-view`, `edit-variants`, `edit-variant-create`, `edit-save`,
`edit-undo`, `edit-redo` and `edit-history`. Recipe JSON is versioned and rejects
unknown fields or unsupported versions. For example, this changes exposure while
leaving the other controls neutral:

```json
{
  "version": "1",
  "settings": {
    "crop": null,
    "straighten_degrees": 0.0,
    "exposure_ev": 1.0,
    "white_balance": {"mode": "as_shot"},
    "contrast": 0.0,
    "highlights": 0.0,
    "shadows": 0.0,
    "saturation": 0.0,
    "vibrance": 0.0,
    "sharpening": {"amount": 0.0, "radius_px": 1.0},
    "noise_reduction": {"luminance": 0.0, "chroma": 0.0}
  }
}
```

White balance is applied during original development, followed by noise reduction,
exposure, tone and color, fixed-canvas straighten/crop and sharpening. Crops use
normalized edges on the physically oriented original. Straightening introduces
transparent corners; it does not silently zoom. Export resizing and color encoding
follow exact recipe rendering. Interactive proxy spatial effects are approximate.

Copy-adjustment jobs freeze a source recipe and selected groups. Append up to 200
expected target identities/revisions per request, seal the total, and process bounded
steps. Results distinguish successful copies, changed targets and incompatible
geometry. Use `edit-copy-begin`, `edit-copy-targets`, `edit-copy-seal`,
`edit-copy-step`, `edit-copy-items`, `edit-copy-status` and `edit-copy-cancel`.

## Preview integration

`PreviewService` has separate original, refined-variant and interactive requests.
Cache keys include variant, monotonic edit revision, recipe and renderer identity.
A disk-bounded prepared linear proxy can serve compatible warm interactions; white
balance, source-instance or renderer changes require new preparation. A proxy never
qualifies as an exact export input. Retained offline previews keep their own recorded
revision, and pending reads reject stale revisions. Each new edited preview records
whether its worker decoded the original or consumed a fully validated prepared
proxy, including the proxy receipt and source-instance digest. Older cached records
have no such evidence. This describes the worker that produced the pixels; a cached
delivery is not a new worker execution.

An export actor suspends new native preview launches and waits for existing work to
drain before starting its one export process. Foreground development can preempt it:
the actor stops and reaps the export, fences its attempt, releases its reservation,
and then resumes preview launches. Cached preview reads can continue. Completion
verification and durability are blocking filesystem work and belong on an executor
thread. Full file verification and durability barriers occur outside catalog writer
authority. Short guarded capture/link steps recheck held file identities and content
change stamps. Performance qualification must still establish responsiveness.

## Batch export

A batch is built incrementally; it does not materialize a whole catalog selection.
Each item binds one variant revision, original fingerprint/location, output settings,
selected metadata and destination snapshot. JPEG 8-bit, PNG 8/16-bit and TIFF 8/16-bit and float32 are
supported. Choose original dimensions or aspect-preserving fit, explicit upscaling,
sRGB, linear-sRGB or a supplied ICC profile, and alpha preservation or an explicit
linear RGB background. JPEG requires a background; float TIFF requires a supported
linear matrix profile.

The CLI sequence is:

1. `photo-export-begin` creates a batch.
2. `photo-export-paths --limit 512` incrementally builds the derived original-path
   protection index; repeat while `pending` is true. Unmapped originals must be
   explicitly encoded or relinked. This step reads paths, not image bytes.
3. `photo-export-add JOB target.json output.json --expected-total N
   --max-original-bytes BYTES --max-payload-bytes BYTES` freezes one item. An existing
   destination requires `overwrite: true` in that target. Destination protection
   also checks current filesystem aliases to other catalog originals. Its explicit
   directory/candidate allowances can be configured with the two `--alias-*` flags.
4. `photo-export-seal JOB --expected-total N` admits the completed selection.
5. `photo-export-run JOB limits.json --max-items N --max-seconds SECONDS`, together
   with the global `--preview-config` argument, executes a bounded slice. Resource
   settings are explicit; allocation ceilings alone are not an RSS measurement.
6. Inspect `photo-export-status`, `photo-export-items` and `photo-export-plan`.

A target uses `key: {asset_id, variant_id}`, `expected_revision`, `destination`,
`overwrite` and `metadata`. Choose metadata `{"mode":"omit"}` or
`{"mode":"resolved","expected_revision":N,"base_model":ID}`. Resolved export
requires an explicit retained full model where one exists and refuses unresolved
metadata conflicts. Derivative orientation/dimensions/profile are updated, active
Adobe develop instructions are removed from rendered derivatives, and unknown
unrelated XMP semantics remain preserved. Original packets and originals are never
rewritten by this workflow.

Output JSON mirrors `OutputSize`, `OutputFormat` and `AlphaPolicy`; its profile is
`{"kind":"srgb"}`, `{"kind":"linear_srgb"}` or
`{"kind":"icc","path":"/absolute/profile.icc"}`. ICC bytes are read with an
explicit 16 MiB bound and retained by content identity rather than expanded into IPC.

`photo-export-cancel` prevents new capture/link operations. An output already
installed under a committed publication intent can still be finalized after
cancellation; this records the operation that occurred before cancellation. `photo-export-recover` retires
abandoned worker transports and fences rendering attempts in bounded pages. A
worker-discovered sealed file does not by itself authorize publication: restarting
an unaccepted item independently rerenders and compares the complete encoded bytes.
Canceled or stale work cannot use this path to publish. Explicit accepted-seal retry
and safe restoration of captured destination bytes are separately exposed by
`photo-export-retry-seal` and `photo-export-restore`. Conflicting external revisions
remain preserved and visible in the receipt. Publication commits intent before a
possible install, so crash recovery can distinguish an installed result from stale
unfinished work. Safe restoration retains captured bytes and supports retry after
interruption, including when the staged new payload is missing. A replacement file
with equal bytes but a different object identity does not authorize clobbering it.

## Evidence boundary

Focused tests currently cover persistent variants/copy history, source and edit
races, six format/depth exports, actual process cancellation/restart/preemption,
queued preview recovery, and orphan-seal refusal/reuse. Original hashes are checked
on disposable fixtures. The qualification harness must additionally prove all 30
reference inputs, 100 MP support, independent pixel/color/metadata comparisons, and
the epic's fixed operation and memory targets on frozen source. Tauri frame latency
requires the later desktop readiness gate.
