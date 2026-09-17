# Prospective scrolling frame measurement

The fixed requirement is a 60 Hz target, p95 main-thread frame time at most
16.7 ms, p99, and dropped-frame reporting. It does not require on-CPU accounting
or physical scanout. This document defines two different clocks explicitly.

## Historical v1 envelope

Protocol v1 used each WebKit rendering record's full `endTime - startTime` wall
span. The v30 result remains a valid **FAIL under v1**: p95 18.160958 ms. That
strict envelope includes waits and gaps without exported Script/Layout activity,
so its failure alone does not prove that instrumented main-thread work exceeded
16.7 ms. No v2 result may relabel, trim, or replace the frozen v1 result.

## Prospective v2 metric

Protocol v2 defines main-thread frame work as the exact interval union of the
installed Inspector's exported main-page Script and Layout/rendering records.
An interval includes synchronous waits or preemption inside the exported task;
it is not CPU occupancy. The installed tool binding is WebKit/WebInspector
`21624.5.1.11.3` and `Main.js` SHA-256
`8f52d98cb9bde071ba2e63719372da37d00c07122e6096a3a874ac969ca76b2f`.
Its Main Thread statistics use flat Script/Layout spans without adding nested
records. Exported rendering frames contain only type/start/end, so v2 performs
the same overlap-safe union from the flat records.

The work allowlist is frozen:

- Script: `script-evaluated`, `api-script-evaluated`,
  `microtask-dispatched`, `event-dispatched`, `observer-callback`,
  `timer-fired`, and `animation-frame-fired`.
- Layout/rendering: `recalculate-styles`, `forced-layout`, `layout`, `paint`,
  and `composite`.

All allowlisted work is retained regardless of whether it comes from product
code, a framework, dynamic styles, polling, or measurement rAF callbacks.
Nested and overlapping intervals are merged once. No manual layout subtraction
or source-name filter is allowed.

`garbage-collected` is reported separately because Inspector's Main Thread
breakdown does not classify it as a main-thread task. For the acceptance gate,
page-target GC spans are conservatively unioned with task spans. The gated
quantity is named **instrumented main-thread work plus page-GC envelope**. It can
overcount concurrent GC, but it cannot gain a pass by omitting collection pauses.
Worker-target GC is invalid evidence for this metric.

Timer install/remove, animation-frame request/cancel, probe markers,
layout/style invalidation, first/largest-contentful-paint markers, and
`console-profile-recorded` do not create task-work intervals. Expected instants
must have zero duration. A positive console-profile envelope is retained and
reported but never counted as task work. Any unknown timeline or event kind,
including a zero-duration unknown, stops evaluation for review.

## Frame and gap accounting

All raw rendering frames intersecting the start/end markers remain in the
cohort, including both full boundary frames. The evaluator requires the complete
guard frame immediately before and after the cohort. Frames must be finite,
ordered, nonoverlapping, and inside the recording bounds.

For included raw frame `F_i=[s_i,e_i]`, define its conservative assignment
window as:

```text
A_i = [e_(i-1), s_(i+1)]
```

`A_i` contains the full frame and both adjacent inter-frame gaps. Task and GC
intervals in an internal gap are therefore assigned to both adjacent frame
samples. This deliberate duplication is reported; intervals are still unioned
only once inside each sample. Boundary-gap work is assigned to the adjacent
included frame. Missing guards, overlapping frames, unassigned positive work,
or malformed/nonfinite/negative intervals invalidate the evidence.

For every included frame, report:

- full elapsed envelope `e_i - s_i`;
- task union within the raw frame;
- page-GC increment and task-plus-GC union within the raw frame;
- task union and task-plus-GC gate union within `A_i`;
- gap contribution and deliberately duplicated gap duration; and
- `(e_i - s_i) - raw_task_plus_GC` as **unknown/uninstrumented remainder**.

The remainder is not proven idle, `Other` CPU time, or permission to discard the
frame. Report NumPy linear p95/p99/max/count/exceedances for the envelope, raw
task, raw task-plus-GC, assigned task, and assigned task-plus-GC distributions.
Only assigned task-plus-GC p95 is compared with 16.7 ms for v2. The v1 envelope
and its verdict remain alongside it.

## Required installed-tool conformance

V2 is unusable until one package-bound Inspector recording qualifies all three
declared phases in a continuous rAF sequence:

1. An idle rAF phase with at least five full frames. Its maximum assigned
   task-plus-GC work must be no more than the prospectively frozen 5 ms limit;
   it must not inherit a refresh-length gap from the frame envelope.
2. One approximately 24 ms synchronous task. Same-domain task start/end markers
   must differ from 24 ms by no more than the frozen 1 ms tolerance, and one
   allowlisted Script interval must contain that marker interval within the same
   tolerance.
3. At callback 60, a connected-element style mutation and geometry read inside
   rAF; at callback 61, removal of that element; and at callback 62, the phase
   end marker after both rendering opportunities. The export must contain positive
   `forced-layout`, `paint`, and `composite` records plus the containing
   `animation-frame-fired` interval. Their simple sum must exceed their union,
   proving nested work is not double-counted.

The same recording must preserve zero-duration timestamp markers and complete
guard frames around the idle, busy, and layout cohorts. Literal zero-work live
frames are not required: the continuous rAF control and compositor activity are
themselves retained work. The evaluator still reports their count, and its
synthetic tests cover the empty interval-union case.
Between the callback-10 idle-start marker and callback-120 done marker, it must
export at least the expected 111 remaining `animation-frame-fired` records.
This callback check is bounded to the declared phase instead of the entire
recording. Unknown event kinds, an ambiguous target, discontinuity, absent
guards, missing render kinds, failed busy containment, or idle work above 5 ms
leaves v2 unqualified. Synthetic evaluator tests separately fix nested union,
cross-frame clipping, duplicate gap assignment, GC inclusion, missing-boundary,
and fail-closed unknown-kind behavior.

Freeze the conformance plan before capture with protocol
`s12_frame_work_v2_conformance_plan`, source commit and package-binding hash,
the evaluator SHA-256, installed version/hash above, exact marker names and callback ordinals,
`task_requested_ms=24`, `marker_tolerance_ms=1`, `idle_min_frames=5`,
`idle_max_gate_work_ms=5`, required layout/script kinds, and these policies:

```json
{
  "gap_policy": "both_adjacent_frames",
  "gc_policy": "gate_union_page_gc",
  "unknown_event_policy": "fail"
}
```

One passing conformance recording qualifies this installed package/tool/evaluator
combination. A failed conformance is instrumentation evidence, not a product
frame result.

Qualify and preserve the conformance result before collecting a measured run:

```sh
python3 scripts/evaluate_frame_work_v2.py \
  --qualify-conformance \
  --package-binding PACKAGE_BINDING.json \
  --conformance-trace RAW_PRIVATE_CONFORMANCE.json \
  --conformance-plan FROZEN_CONFORMANCE_PLAN.json \
  --output NEW_CONFORMANCE_RESULT.json
```

## Measured-run qualification

The opt-in recorder markers remain:

- `LensWorks:S12:<run_id>:scroll:start`
- `LensWorks:S12:<run_id>:scroll:end:duration_elapsed`

Before capture, freeze an `s12_frame_work_v2_plan` containing metric version 2,
the source/package, evaluator, and conformance-plan/trace hashes, run ID, marker names, 16.7
ms budget, `numpy_linear`, the three policy values above, five-second duration
and tolerances, display identity, page/grid viewports, declared competing load,
minimum scroll movement, and rAF edge tolerance. The plan must exist before the
measured trace; do not create it after inspecting results.

The postcapture context record uses protocol `s12_frame_work_v2_context` and
binds the run/source/executable, the trace's sole `Page` target, actual
page/grid viewport and display/competing-load declaration. It must affirm that
focus, visibility, target identity, display, and viewport remained unchanged.
These are observed capture facts, not editable metric choices.

The evaluator also requires a protocol-2 receipt with the same run ID, zero
overflow, outcome `complete`, end reason `duration_elapsed`, bounded monotonic
rAF series, matching duration, unchanged mounted scroll target/dimensions, and
the predeclared minimum movement. The trace must have no discontinuity and its
two markers must be internal to the recording. Missing or inconsistent evidence
is `INVALID_EVIDENCE`; it never produces a smaller favorable cohort.

Run the versioned evaluator only after the conformance and measured plans are
frozen:

```sh
python3 scripts/evaluate_frame_work_v2.py \
  --trace RAW_PRIVATE_TRACE.json \
  --receipt RAW_RAF_RECEIPT.json \
  --plan FROZEN_FRAME_WORK_V2_PLAN.json \
  --context CAPTURE_CONTEXT.json \
  --package-binding PACKAGE_BINDING.json \
  --conformance-trace RAW_PRIVATE_CONFORMANCE.json \
  --conformance-plan FROZEN_CONFORMANCE_PLAN.json \
  --output NEW_RESULT.json
```

The evaluator reads but never reproduces network records or headers. Output
paths are create-only. A passing timing result is
`FRAME_WORK_V2_PASS_REQUIRES_TERMINAL_ACCEPTANCE`; the prescribed corpus/scale,
real scroll workload, and other story acceptance remain separate.

## rAF and interpretation boundary

Keep every raw rAF callback interval. Report NumPy linear p95/p99/max/count and
intervals above 16.7 ms as a **dropped-opportunity proxy**. It is neither a
physical compositor-drop count nor a substitute for frame work. Inspector
overhead remains included and disclosed. V2 supports an operational
instrumented-work result at the installed Inspector's coverage; it does not
claim exhaustive native-task accounting, CPU occupancy, GPU completion, or
physical presentation.
