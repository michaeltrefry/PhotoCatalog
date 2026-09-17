# Prospective scrolling frame measurement

The epic requires a 60 Hz target, p95 main-thread frame time at most 16.7 ms, p99,
and dropped-frame reporting. It does not specify per-frame on-CPU attribution or
physical scanout. Those are distinct quantities; neither is inferred here.

Use WebKit rendering-record elapsed wall durations only after a separate bounded
conformance check binds the installed application, WebKit/Inspector version,
WebContent target, exported marker semantics and frame boundaries. Confirm that
a known bounded main-thread task in a continuous animation phase appears in the
expected rendering record, both markers survive export, and recording boundaries
do not truncate the interval. An unsupported export remains unqualified. This
protocol is prospective; existing trace verdicts are unchanged.

The opt-in S12 recorder emits these Inspector timestamps around its existing
five-second mounted-grid capture:

- `LensWorks:S12:<run_id>:scroll:start`
- `LensWorks:S12:<run_id>:scroll:end:<reason>`

Before collection, freeze package/tool versions, display and viewport, cached
corpus, gesture/range, competing load, marker names and percentile convention.
Use a short continuous-animation pre/post roll to expose complete boundary
records. Require the matching end reason `duration_elapsed`, complete raw rAF
receipt, expected scroll movement, uninterrupted target/focus/visibility and no
overflow. Keep every full rendering record intersecting the marked interval,
including both boundary records; do not clip or discard long/idle-looking frames.
Missing, discontinuous or truncated evidence is invalid or partial, never a
smaller passing cohort.

Report NumPy linear p95/p99, maximum, count and every frame-budget exceedance
from those full elapsed wall spans. The p95 gate remains 16.7 ms. Label rAF callback
intervals and the count exceeding 16.7 ms separately as a dropped-opportunity proxy,
with raw timestamps and clock precision retained. They are not a physical
compositor-drop count or a replacement for the direct frame-duration gate.
Inspector overhead remains included and disclosed. Do not subtract idle time or
call wall duration CPU occupancy.

These markers prepare collection; they establish no measured acceptance result.
See [WebKit Timelines](https://webkit.org/web-inspector/timelines-tab/) and
[Rendering Frames](https://webkit.org/blog/3996/introducing-the-rendering-frames-timeline/)
for the instrument's frame/event-loop meaning. Bind installed-tool behavior
explicitly rather than treating current upstream documentation as a version pin.
