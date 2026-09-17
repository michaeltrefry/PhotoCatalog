# Prospective S12 import durability timing

The inherited requirement is durable acknowledgment p95 **≤100 ms while background import runs**. This protocol measures exactly **100 predeclared rating or edit inputs**, all successful, with no latency-based selection. It does not change that budget. Historical export protocol-2 campaigns retain their original verdicts; in particular an export PARTIAL cannot become an import pass.

## Receipt and causal proof

The application receipt remains protocol 2. The additive `clock_alignment.profile: "import_v1"` selects 200 ms after-response anchor spacing, a 600,000 ms campaign limit, at most 3,002 anchor attempts and one in-flight native request. Legacy/export profile remains 100 ms/300,000 ms. These are instrumentation bounds, not performance budgets. The serialized application receipt remains bounded at 2 MiB.

Each sample has its original browser `performance.now()` durable latency and a captured `import_id`. Its `sample_events` row adds `durable_event`, recorded once at durable acknowledgment. Causal event numbers share one sequence with sample start/end, anchor send/receive, and status request/response observations. The maximum event number is 9,944; failed requests may leave gaps, never fabricated responses. The evaluator requires a durable event for every import-bound durable latency. Idle setup saves may retain their latency without an import causal event; no retrospective event is fabricated. A later presentation event is retained but does not qualify import overlap.

Native anchors contain lossless decimal nanosecond strings on macOS `mach_absolute_time()`, the GUI PID, one native session, run ID and sequential anchor ID. Before any campaign, run the installed executable's clock conformance diagnostic described in [S12_CLOCK_ALIGNMENT.md](S12_CLOCK_ALIGNMENT.md). No wall/epoch timestamp conversion is used. For each input the evaluator takes the nearest anchor received before input start and the nearest anchor sent after durable acknowledgment. Both native endpoints must lie inside one inward native import-lock segment. Delayed anchors only widen uncertainty; they cannot improve admission.

`clock_alignment.import_evidence` contains:

- `bindings`: at most four `{key, id, source_blake3}` records; key is consecutive and one-based. The source digest is the existing synchronous BLAKE3 of UTF-8 `JSON.stringify([source.encoding, source.units])`, after bounded NativePath validation, once per UUID. A source mismatch or lost observation increments `overflowed`.
- `timeline`: at most 1,202 `{request_event, event, binding, phase, imported, unchanged, failed, skipped, metadata_updated, metadata_warnings, awaiting_resources, pending_previews}` observations of successful authoritative `import_status` polling responses only. Start/resume/cancel command replies update UI context but do not enter this timeline. Seven counters are canonical unsigned decimal u64 strings; `pending_previews` is a u32 integer. The request event is recorded immediately before the native status command; the response event is recorded on success before updating UI context. Polling is single-flight, and each next request follows the previous response. A response event alone is not a lower bound on its native snapshot.
- `overflowed`: must equal zero.

This prospective evaluator requires exactly one declared UUID/source binding, causal status observations before, within, and after the cohort, and a cumulative progress advance between two active status snapshots whose entire first-request-through-second-response causal envelope fits the native segment and cohort. Request/response ordering prevents delayed replies with pre-cohort snapshots from masquerading as in-cohort progress. A retained lock with no progress, or progress without a proved native lock, remains PARTIAL. `awaiting_resources` and preview counts are not assumed monotonic or treated as proof of completed work.

## Native ownership

The GUI owns C (`--catalog-desktop-worker`) and F (`--catalog-filesystem-worker`) as **direct siblings**. They are not a GUI→C→F process chain: `src/application/desktop/startup.rs` creates F before `process::Owner::spawn` creates C, and F outlives its dependent C. A fresh one-catalog session is mandatory. Every positive probe rejects missing or duplicate GUI-owned C/F roles and binds their exact PIDs/births, role arguments, ancestry, executable path and file identity.

The sole active C import UUID and source are supplied by authoritative status. The source invariant permits only one active import task, and managed F holds `catalog/import.lock` exclusively from Begin through Finish/Abort. Thus this campaign binds that UUID to the unique F lock owner without adding product IPC solely for measurement. The lock must have a predeclared regular-file device/inode in an unchanged canonical catalog directory; aliases and symlinks are rejected. Exact executable SHA-256 and stat identity are checked before/after observation, with stat identity rechecked at every positive probe.

The read-only observer checks shared-flock contention immediately before and after an all-holder lsof query for the exact lock path. Every reported descriptor must have the declared device/inode and belong to F, the sole visible open owner. Darwin lsof returns a blank lock field even for a known exclusive flock: this blank is preserved as unknown, never relabeled `W`. Contention plus the sole visible owner and the reviewed retained-F-lease lifetime supplies the practical ownership evidence. This is bounded by lsof/process visibility under the observing account; it is not a direct kernel lock-owner oracle. Nonzero exit, stderr, missing/mismatching owners, malformed records, replacement, and timeout all fail closed. The focused Darwin test qualifies this behavior on a temporary self-owned lock and also proves contention disappears after release; it touches no product process. Positive bounds point inward: after the first wholly successful probe through before the last wholly successful probe. No gap is bridged. The first contention loss closes and permanently retires the single segment. AccessDenied, SystemError, unknown process errors, replacement, lsof errors or overruns are fatal, with bounded original exception chains, a closure, and a final summary. The sole exception is an unbound direct GUI child that disappears between child enumeration and command-line inspection: it is ignored only after a fresh PID-and-birth lifecycle read proves that exact child exited or the PID was reused. A declared C/F owner, a live child, or an uncertain lifecycle still fails closed. Unlike short export rendering children, retained F is never reclassified as an expected exit.

## Predeclaration and use

Prepare a fresh, opt-in measurement run, with recovery/setup finished before declaring inputs. Retain full UI readiness diagnostics before the first input. Freeze `declaration.json` before any declared input. It contains:

- `run_id`, canonical `import_id`, and `source_blake3` matching authoritative status and the receipt binding.
- `input_kind: "edit"` with `input_action: "edit"`, or `input_kind: "cull"` with `input_action: "rating"`; `setup_cutoff_ordinal`; and `input_ordinals` equal to exactly `cutoff+1 … cutoff+100`. Preserve the planned control/values and actual input ledger separately for final reconciliation.
- `root_pid`, `root_birth_unix_s`, `desktop_pid`, `desktop_birth_unix_s`, `filesystem_pid`, `filesystem_birth_unix_s`, captured from the same fresh GUI session.
- Canonical absolute `executable`, lowercase `executable_sha256`, and `executable_identity` containing `st_dev`, `st_ino`, `st_size`, `st_mtime_ns`, `st_ctime_ns`, `st_mode` (as returned by the observer helper).
- Canonical absolute `catalog`, `catalog_identity: [device,inode]`, and `lock_identity: [device,inode]` for the existing `import.lock`. Obtain identities with `file_identity`; do not guess them.

Start anchors and the observer before inputs, wait for native positive proof and a successful received anchor, and keep the entire 100-input schedule within the admitted window. Incomplete/missing tail inputs remain PARTIAL; any extra post-setup sample or noncontiguous declaration is invalid. Do not skip declared slots when controls temporarily disable; wait for readiness before delivering that input. An ambiguous Return stops the run instead of silently retrying or replacing an input.

```sh
python scripts/observe_import_native_v3.py --declaration declaration.json --output observer.jsonl
python scripts/evaluate_import_overlap_v3.py --receipt receipt.json --observer observer.jsonl \
  --declaration declaration.json --output result.json
python -m unittest discover -s scripts -p test_import_overlap_v3.py -v
```

Both evidence outputs use create-new semantics. Observer identity binds SHA-256 of the entire canonical declaration (`json.dumps(sort_keys=True, separators=(",", ":"), ensure_ascii=True, allow_nan=False)`), including the original run/cohort/action/cutoff. Observer defaults are at most 600 seconds, 200 ms between completed probes, with a final 1.1-second query reserve; it may close earlier at import completion. The evaluator reconciles every raw positive/closure/gap against summary counts and endpoints, rejects unknown rows and any fatal observer error, and hashes all input artifacts into its new result.

## Separate terminal acceptance

`TIMING_PASS_REQUIRES_IMPORT_RECONCILIATION` is explicitly **not final product acceptance**. It means all 100 input-to-durable envelopes were admitted, all 100 inputs succeeded, useful background progress was established, and NumPy linear p95 over all 100 durable latencies is ≤100 ms. All timings and outcomes remain in the receipt; no first-100-of-200 selection exists here.

The same UUID/source must subsequently reach the expected terminal result. Independently retain authoritative terminal counts/errors, reconcile the exact source corpus and catalog outcomes, prove original/source invariance, verify each intended rating/recipe and final state, and prove checked worker drain/closed-database reconciliation. Neither elapsed observation time, UI `during_import`, a retained lock, nor a timing pass substitutes for those checks. A failed terminal import cannot satisfy the campaign even if foreground timing passed.

These scripts and tests establish the method's source contract. A real installed campaign and terminal reconciliation remain required; the document makes no claim of measured acceptance.
