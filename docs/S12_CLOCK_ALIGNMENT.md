# S12 export clock alignment, protocol 2

This is a prospective measurement method. It does not reinterpret earlier receipts or establish that an installed campaign passed. The durable edit budget remains **p95 <= 100 ms**, using NumPy's linear percentile over the first 100 qualifying inputs from exactly 200 predeclared inputs. Final export reconciliation and source integrity remain separate acceptance requirements.

## Causal proof

Browser latency still uses differences of `performance.now()`. Protocol 2 does not convert `performance.timeOrigin` into the observer's current wall clock. In an explicitly enabled S12 measurement session, the native clock command returns decimal-string nanoseconds from macOS `mach_absolute_time`, converted with `mach_timebase_info`. The command binds each consecutive anchor ID to the measurement run, a native session UUID, and the GUI PID. Native receipt validation checks returned anchors against the issued values.

The frontend records a shared event sequence for sample start/end and anchor send/receive. If anchor A was received before an input began, and anchor B was sent after its presentation callback, the input's complete interval lies inside `[native(A), native(B)]`. The evaluator chooses the nearest such anchors deterministically and admits an input only when this **entire envelope** lies inside one native observer segment's inward-facing monotonic endpoints. It uses no midpoint estimate, rate fit, epoch offset, or extrapolation. Slow or missing anchors reduce provable coverage. Rounded frontend timestamps are not used to infer event order.

The observer retains exact executable, PID/birth/ancestry, stage/request/job/attempt/authority, lock identity/contention, and native lsof ownership checks. A lifecycle gap closes the segment at its last successful inward endpoint; that worker's segment cannot reopen. AccessDenied is a lifecycle gap only when a fresh check affirmatively establishes disappearance, zombie state, or PID reuse. A live or unverifiable process remains an observation error. Wall-clock drift is retained as a diagnostic, not used in protocol 2's causal proof. Every segment closure has a raw record; evaluation requires a bijection between raw positive/closure records and summary segments, reconciles aggregate counts, and checks closure causes and boundaries.

## Bounds and activation

- Normal sessions without `--s12-measure=<run-id>` do not start clock sampling. Measurement-enabled sessions start one anchor campaign on the first active export status; a during-export sample provides a fallback trigger. The fallback's first sample may lack a prior anchor and therefore remain uncovered.
- There is at most one anchor request in flight, with a **100 ms delay after each reply** before the next request. The campaign stops after 300 seconds, at 3,002 anchor attempts, on a request error, or on receipt finalization. Status toggles cannot restart it. An unresolved request is retained as incomplete. No edit awaits an anchor.
- Receipts retain the existing 512-sample and 2,048-scroll-frame bounds. Protocol 2 adds bounded anchor and sample-event arrays; the persisted receipt limit is **2 MiB**. Native tests cover maximum-sized receipts and exact finalization retries with an outstanding anchor.
- A measurement session with no export records no native anchors and does not call the macOS-only clock. Other platforms fail explicitly if this export clock is requested; they do not substitute wall time.

## Qualification and commands

Qualify the exact installed executable and Python runtime before a new campaign. The diagnostic starts only a bounded clock process, with 16 stdin/stdout exchanges; it opens no GUI or catalog. Each native stamp must fall inside a Python monotonic request/response bracket. The receipt binds executable identity and SHA-256 before and after, Python runtime/clock implementation, native PID/session, and every bracket. Malformed, partial, or delayed output produces a failed diagnostic receipt retaining up to 4,096 stdout bytes as hex evidence. Passing this diagnostic establishes clock conformance only.

```sh
python scripts/measurement_clock_conformance.py \
  --executable /absolute/path/to/installed-executable \
  --output /absolute/path/to/new-clock-conformance.json

python scripts/observe_export_native_v2.py \
  --pid GUI_PID --exe /absolute/path/to/installed-executable \
  --exe-sha256 EXECUTABLE_SHA256 \
  --export-workers /absolute/path/to/manifest/export-workers \
  --job SEALED_JOB_UUID --seconds 300 --interval 0.05 \
  --output /absolute/path/to/new-observer.jsonl

python scripts/evaluate_export_overlap_v2.py \
  --receipt /absolute/path/to/new-frontend-receipt.json \
  --observer /absolute/path/to/new-observer.jsonl \
  --declaration /absolute/path/to/frozen-input-declaration.json \
  --output /absolute/path/to/new-timing-evaluation.json
```

Use a qualified macOS Python with psutil for observation and NumPy for evaluation. All output paths must be new. Freeze source/package/runtime bindings, declaration, observer and evaluator hashes before the inputs; do not update a declaration after seeing outcomes.

The declaration contains `run_id`, `job`, `executable_sha256`, `setup_cutoff_ordinal`, and `input_ordinals`. The latter must equal the contiguous 200 ordinals immediately after the setup cutoff (for cutoff 3, ordinals 4 through 203). All receipt ordinals must run from 1 through that final input, with every declared input an edit. Missing or extra post-setup inputs are rejected. Preserve the separately predeclared input values and delivery evidence as campaign artifacts.

Selection uses causal envelopes and ordinal order before reading durable latencies. A non-complete input whose start is proven active fails the campaign even outside the selected cohort. Unknown coverage for a non-complete input prevents a timing pass; insufficient qualifying intervals yields partial evidence. Every raw input and decision remains available. An evaluator result of `TIMING_PASS_REQUIRES_EXPORT_RECONCILIATION` is not product acceptance: independently reconcile the exact 100-item job and observed attempts/authorities to final published items, verify outputs and source invariance, and retain package and workload provenance.

Focused checks:

```sh
cd desktop
npx vitest run src/performanceMeasurement.test.ts src/measurementClock.test.ts
npx tsc --noEmit
cd ..
python -m unittest discover -s scripts -p test_export_overlap_v2.py -v
cargo test --manifest-path desktop/src-tauri/Cargo.toml measurement
```
