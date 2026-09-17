# Installed desktop measurements

Use disposable catalogs and fixture originals. Record the source revision,
executable hash, build features, display/viewport, workload, and competing load
before collecting a distribution. Retain incomplete attempts and warmups.

Launch the installed executable with `--s12-measure=<run-id>` to enable the
measurement banner. Finalize once through the banner; the resulting JSON path
is shown in the app. An existing receipt cannot be replaced with different
contents. Ordinary launches do not record these measurements.

The interaction receipt separates durable acknowledgment from a DOM check and
two animation-frame opportunities. The latter is not physical display delivery.
Import/export flags reflect the last observed status at input start; corroborate
actual overlapping work independently when evaluating concurrent workloads.

The bounded five-second scroll capture records raw animation-frame timestamps
and scroll positions. This measures callback cadence, not the CPU time spent
rendering each frame. Browser timer precision can be coarser than the acceptance
budget; report that limitation rather than treating rounded intervals as CPU
work or changing the budget.

## Retained-preview diagnostics

Measurement launches also emit up to 128 `S12_PREVIEW_DIAGNOSTIC` JSON lines to
stderr. Capture stderr when launching the executable. Each line is bounded to
16 KiB and identifies a ticket and preview-key digests without image paths.
Diagnostic readback happens after normal object-URL delivery and does not hold
the displayed image waiting for the log.

The record distinguishes retained-cache selection from original rendering,
checks expected versus selected cache keys, and separates retained-read queue,
owner, checksum, decode, encoded delivery, and frontend invocation timing.
These spans can overlap and must not simply be added together.
`ready_for_transfer_ms` includes the second retained read; its nested
`retained_read.queue_ms` is the read's own queue wait. WebView image decoding
and the two-frame presentation check remain in the interaction receipt.

A refresh of unchanged mounted rows can reuse existing object URLs. It does
not establish retained-thumbnail reload performance. Use an actual remount
with current retained keys and retain its complete visible-photo roster.

## Web Inspector measurement package

For a separately identified rendering-frame recording, build with the optional
`measurement-devtools` Cargo feature:

```sh
python scripts/desktop_tool.py --desktop desktop build -- --features measurement-devtools
```

Only this feature enables release Web Inspector. In such a build, launching
with `--s12-measure=<run-id>` opens it automatically. Ordinary release packages
omit the feature. Record the feature explicitly alongside the package hash;
do not silently substitute this package for a normal release artifact.

Use Web Inspector's Rendering Frames timeline for main-thread rendering work.
Preserve the unfiltered recording, declared scroll interval, timing semantics,
and profiler overhead. Callback cadence remains separate corroboration; neither
recording alone proves physical compositor scanout.
