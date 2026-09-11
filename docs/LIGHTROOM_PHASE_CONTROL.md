# Full capture, path and packet phase control

This supplementary control passed a disposable actual-CLI integration check;
the corresponding real-catalog phases remain pending. It
never initializes/adopts a run, copies an old plan, chooses a family, imports into
PhotoCatalog or modifies frozen runner/config/binary bindings. Native identities
are explicit recipe inputs; a future corrected inspector requires its own reviewed
binding and evidence transition. It cannot silently replace the current v6 binary.

`lightroom_phase_control.py` executes one declared `full`, `paths` or `packets`
invocation through the frozen runner. The existing main-only wrapper and funding
adapter remain unchanged. Its companion `lightroom_phase_contract.py` checks
requests, predecessor/output evidence and funding arithmetic without SQLite access.
The controller verifies helper bytes before executing them. It uses the exact
reviewed macOS libproc supervisor (`cd4ebbda…`) for native process birth identities,
owned-root cleanup and fresh identity checks before descendant signals. Its old
adoption entry point is never invoked. The existing funding helper supplies only
`FundingMonitor` and `guarded_type`, not its main-only executable entry point.

## Recipe and separate grant

No concrete phase budget is supplied until the actual main review and inventories
exist. The recipe has these exact top-level fields:

- `protocol: 1`, `phase`, absolute `run` and separate external `control`, unique
  `attempt_id` UUID, and positive `expected_next_command`.
- SHA256 `{path, sha256}` references: `input`, `baseline`, `config`, `binding`,
  `journal`, `funding`, `grant`. `code` includes references for `controller`,
  `contract`, `runner`, adjacent `helper`, `funding_guard`, `supervisor`, `python`.
  All source/runtime paths and bytes are explicit. Native bytes are revalidated
  by the frozen runner before it launches a command.
- `previous: {result, review}` binds the actual successful prior phase or clean
  paused invocation. Review requires `status: PASS`, exact `result` and `binding`
  references, plus `output` for completed transitions. Failed/orphan control state
  blocks continuation even if a different initial recipe is presented.
- `pause: {kind: absent}` or `{kind: owned, reference, owner, identity}`. Identity
  is `[device, inode, bytes, mtime_ns, ctime_ns]`. An admitted owned pause is captured
  by rename, verified, and retained. A mismatch restores without clobber or keeps
  both entries. The controller never unlinks a raced-in replacement. When run and
  control devices differ, the capture lives in an exclusively created
  `RUN/.pause-capture-ATTEMPT_UUID/pause-captured` directory, so the atomic rename
  remains on the run's filesystem. Preexisting directories/links are rejected.
  The local attempt receipt references that retained capture; the capture parent,
  run directory and local attempt directory are synced before successful admission.
- `memory` explicitly supplies positive `python_process_rss_bytes`,
  `native_process_rss_bytes`, `combined_owned_rss_bytes`; no new machine capacity
  or measured bound is inferred. For full, `paths_review` is `{kind:not_applicable}`;
  for paths it is the same sentinel; packets requires an actual paths-review ref.
- Optional `temp_storage` is `{directory, device, inode, environment}`. `directory`
  is an absolute non-symlink directory; `device` and `inode` bind its stable identity
  (not modification time). `environment` must contain exactly `SQLITE_TMPDIR` and
  `TMPDIR`, both equal to `directory`. The directory must be writable/searchable
  and on the same device as `run`, so existing free-space observations cover both
  plan growth and SQLite temporary files. Omission preserves existing recipes.

The separate grant is exactly `{status: EXECUTION_GRANTED, scope: PHASE,
attempt_id: UUID, recipe_body_sha256: SHA}`. The hash covers canonical sorted ASCII
JSON plus LF of every recipe field except `grant`. Creating source/templates is
not execution permission. A coordinator fills this only after exact evidence,
resource and source review. Launch uses a frozen private copy, never worktree imports:

```text
PINNED_PYTHON -I -B FROZEN_CONTROL.py RECIPE.json RECIPE_SHA256
```

For a bound temporary directory, launch that same command with both environment
variables explicitly set, for example `/usr/bin/env SQLITE_TMPDIR=ABSOLUTE_TEMP
TMPDIR=ABSOLUTE_TEMP PINNED_PYTHON ...`. Set them before the controller starts;
the controller and native subprocesses inherit them. The recipe (including these
values and directory identity) is covered by the existing separate grant hash.
Keep `control` and its receipts on the local evidence disk when `run` and temporary
files use external scratch, so external-volume failure can still be recorded.

Parent and child admission, the immediate parent launch, every runner call/space
boundary and the outer observation loop validate the temporary destination and
environment. A mismatch fails the attempt and uses the existing owned-process
cleanup; it is not a resumable successful pause. This is sampled detection, not
hard confinement: SQLite can fall back to local directories if the destination
becomes unusable between checks. Environment routing does not move explicitly
named capture/plan files or their adjacent journals. No allocation quota or strict
latency guarantee is implied.

## Phase input, output and replay

`full` input is the runner's reviewed request with `main_review_sha256`,
`automatic_selection:false`, and unique requests containing `candidate_key`,
`revision`, `family_evidence_digest`, `role` and nonempty `reason`. Role is
`prospective_suggestion` or `additional_ambiguity_evidence`. The baseline is exact
`RUN/reports/main-review.json`. The output is `full-review-REQUEST_CANONICAL_SHA.json`
and its distinct plan is `RUN/plans/final-REQUEST_CANONICAL_SHA`. All prior members
are inspected into that plan; requested successful full captures replace their
main-only evidence. Failed copies cannot qualify through fallback.

`paths` input and baseline are the same successful full-review file. Only explicitly
full-requested members are checked. `packets` takes that same full review plus the
successful `paths-review-FULL_REVIEW_FILE_SHA.json` prerequisite. Both outputs are
`PHASE-review-FULL_REVIEW_FILE_SHA.json`. Path output includes available bytes and
status counts; packet work retains raw/transformed XMP and conflicts. There is no
implicit mapping of old mount names or foreign paths. Missing paths, unsupported
carriers and retained-only Adobe semantics remain visible, not pixel equivalence.

The unchanged runner replays verified completed commands, never failed commands.
The supplementary full override issues an attempt-unique guarded discovery before
any full capture/plan work, compares the complete inventory with the main baseline,
and persists its started/complete/paused/failed admission. Completion/pause validation
binds that actual command and stdout. The frozen full method's cached discovery
remains unchanged, but is no longer the sole admission on resumed slices.
Paths/packets perform a new UUID-keyed inventory admission on each
invocation, even when returning an existing immutable review. Input/source hashes
are checked again before the controller accepts a returned report. A completed
output still means `review_returned_not_acceptance`, not S9 completion.

Each invocation requests a cooperative pause at 600 seconds and stops at a sampled
4,800-second emergency deadline, with separate 30-second interrupt and 10-second
kill/wait graces. Pause is accepted only with a new actual `paused` phase receipt,
nonzero root exit, saved pause proof, complete serial command results and no failed
command. An abnormal observer/classifier exit stays `unknown_requires_review`,
even if the final command happened to succeed. At most 20,000 new commands and
16 MiB of command-result metadata are reconciled per invocation. Owned process
observations are bounded to 20,000 lifetimes and 8 active processes. Samples do
not establish all-descendant coverage or OS high-water memory. Root Popen cleanup
is independent of process observation and evidence writes. Stdout/stderr retain
at most 4 MiB each; overflow remains failure with truncation evidence.

## Evidence-derived funding

A funding proof binds `protocol:1`, `phase`, `input_sha256`, exact `categories`,
`single_command_headroom`, `protected_bytes`, and `reserve_bytes:34359738368`.
Each category/headroom is `{basis:[{reference,pointer}], numerator,denominator,reason}`.
A pointer is a bounded list of literal object keys/list indexes ending at an
observed nonnegative integer. Bytes are `ceil(sum(observations)*numerator/denominator)`.
Multipliers are explicit reviewed growth allowances, never asserted as measured
maxima. Zero categories require actual zero evidence. Missing categories fail.
The derivation bounds cumulative metadata reads to 128 MiB; consolidate larger
measurements into separately verified summaries rather than loading page bodies.

Full categories: `capture_raw`, `capture_working`, `capture_logical`, `final_plan`,
`all_row_pages`, `reports_and_commands`, `sqlite_temporary`, `control_evidence`.
Use immutable main capture manifests for selected companion inventories and
main/WAL/logical sizes, all-member plan-size/stat evidence and command/page byte
summaries. Capture keeps raw files, working main/WAL, and a separate logical
snapshot. The new final plan and pages cover **all members**, not just requested
full replacements; do not fund only companion deltas.

Paths categories: `path_plan_growth`, `path_pages`, `reports_and_commands`,
`sqlite_temporary`, `control_evidence`. Derive from full retained file-reference
counts and measured descriptor/page sizes with an explicit growth allowance.
Packets categories: `retained_packets`, `decoded_packets`, `metadata_projection`,
`packet_and_path_pages`, `reports_and_commands`, `sqlite_temporary`, `control_evidence`.
Use the completed path assessment, explicit sidecar/source inventory and packet
parser caps; raw source size is not a substitute for decoded/retained packet costs.
Include every requested member, not a selected subset inferred from names.

Initial required free bytes are protected bytes + **32 GiB operating reserve** +
all remaining phase categories + separate single-command headroom. Resumed requests
must derive remaining needs from immutable completed receipts, not blindly subtract
elapsed time or reuse a prior free-space value. The runtime pauses below protected
+ reserve + command headroom and stops at protected + reserve. These are sampled
and command-boundary guards, not a hard allocation quota. A phase may consume more
than an estimate: preserve its failed/paused evidence and review remaining funding.
Headroom must cover the largest admitted individual capture/SQLite expansion or
packet operation plus logs; the 8 MiB page output cap does not bound SQLite growth.

All phases preserve the frozen native limits (including 8 GiB/file, 64 GiB capture
inventory, 32 MiB cells and 8 MiB row pages unless the bound frozen configuration
says otherwise). The coordinator separately audits successful output counts,
companion retention, packet gaps and family evidence, then records only explicitly
resolved choices. Evidence enrichment invalidates earlier choices. No user decision
is needed merely to preserve additional ambiguous members for inspection.

## Explicit imported-main bootstrap

The initial FULL invocation may use `previous: {kind: imported_main, bootstrap,
review}` instead of a same-binding predecessor. This is a separately reviewed
transition, not a claim that the new run executed main. It admits only `full`,
`expected_next_command:1`, absent pause, empty commands/steps/plans/captures,
no current or orphan control attempt, and no current `phase-*` history. The new
run must have been initialized with `generation_request:null` and
`seed_request:null`; no legacy command replay is enabled.

`bootstrap` is a pinned JSON reference with exactly:

- `protocol:1`, `status:IMPORTED_MAIN_NOT_EXECUTED`, absolute `old_run`;
- references `old_binding`, `old_main`, `old_result`, `old_review`, `new_binding`,
  `new_main`;
- `initialization:{result,pointer,review,config,argv}`;
- `captures:[{candidate_key,capture_path,manifest}]`.

The separate bootstrap `review` requires `status:PASS`, exact `bootstrap` and
`new_binding` references. Old and new runs and source identities remain distinct.
`old_main` is the old run's terminal `reports/main-review.json`; `new_main` is
byte-identical at the new baseline path. The ordinary old-owner result route requires
`main_review_returned_not_acceptance`, exit zero, explicit `root_reaped:true`,
`observed_owned_processes_reaped`, empty command failures and its actual main
phase reference. That phase must bind the old source and old output. The old
independent review pins the result, old binding and output. None of those records
is rewritten to use a corrected native binary's identity. Any known defects in
old derived relationships remain disclosed; corrected FULL re-inspection produces
new derived evidence.

Initialization `result` is the real owner receipt; `pointer` selects its actual
init child record (for example `['phases',0]`). The selected record must contain
its exact `argv`, zero exit, no failure, `root_reaped:true` and
`ownership_status:observed_owned_processes_reaped`. Its independent review pins
`result` and the actual new `binding`. Initialization `config` must decode to the
same configuration as the new run's copied config. The exact admitted argv is
`[PYTHON,'-I','-B',RUNNER,'init',CONFIG_PATH]`. If a concrete owner uses a different
receipt shape, a reviewed explicit shape adaptation is required; a fabricated
execution receipt is not a substitute. The controller's existing source/config
checks and the frozen Runner's native-byte checks also apply.

Every successfully captured old member has exactly one manifest reference at
`capture_path/manifest.json`, matching its old candidate source, capture revision
and any prior inspection revision. At most 256 manifests and 128 MiB total
manifest metadata are admitted. This verifies referenced manifest provenance,
not a new hash of retained raw catalogs. Native `add` validates actual captured
bytes when FULL re-inspects them. The controller does not copy the giant main
plan, perform an adoption, or recreate old commands. A separately admitted
bootstrap preparation establishes the byte-identical baseline and actual init
proof before this controller can run.

SIGTERM of the owner becomes a recorded failure and owned-child cleanup; repeated
SIGTERM is ignored during that cleanup. SIGKILL/power loss remains an unresolved
attempt requiring explicit review. Actual tests cover TERM delivery with a real
separate child, bounded stdout overflow, observer failure, fresh full discovery
before work, imported-main provenance and same-count candidate substitution.
These synthetic checks do not qualify a real full/path/packet invocation.

## Disposable actual CLI check

Controller source `931938f` and corrected native source `ccae8ad` passed a
separately admitted tiny fixture run. An actual legacy CLI initialized and ran
main; an actual corrected CLI initialized a distinct run. The imported-main
bootstrap then drove full capture, paths and packets through the controller,
including a replay of each phase. The new run executed no main phase; two old
captures and one new capture remained, without duplicate captures on replay.
All 81 new native commands succeeded, fixture source hashes remained unchanged,
and no family selection or migration occurred.

The retained private receipt is
`sc-22844-phase-cli-smoke-execution-931938f-v1/smoke-result.json`, SHA256
`2816b526952174bfdf6151525424a9aa9b0425160ae6df294c92e347ef1c7cfa`.
This check used the explicit-root-reaping old-owner route. The legacy-v3 adapter
below has separate source and synthetic contract evidence; this fixture did not
execute the actual private legacy wrapper. The tiny run did not reach the
600-second cooperative pause. These results establish fixture integration, not
real catalog completeness, scale, or Adobe rendering equivalence.


### Actual legacy v3 owner shape

The real previously reviewed main v3 wrapper (SHA256
`3aa8d1222d8323f491fb9a176fa61416eee790f7b1eca2d05514ea7019e55f7b`)
emits its funding child's terminal `exit_code` from `Popen.poll()`, but **does not
emit `root_reaped`**. Its raw result is never rewritten to add that field.
Only `imported_main` may instead add bootstrap field
`old_owner:{kind:legacy_main_v3,wrapper,started,process,recipe,outer_wait,review}`.
All entries other than `kind` are pinned references. This adapter requires:

- the exact wrapper bytes above; its parsed literal CORE/binary/driver/helper/config
  constants must match `old_binding`; the wrapper is not executed by admission;
- actual `started.json`, `process.json` and `result.json` under the old recipe's
  control/attempt directory; started wrapper/supervisor/recipe identities, funding
  policy, funding-child argv/PID and start/finish timestamps must agree;
- the original result must have no `root_reaped` field, no failure field,
  `cleanup:null`, exit zero, clean ownership and no new command failures; its
  separately reviewed actual completed-main phase remains bound to the old source;
- `outer_wait` is the unchanged terminal tool response, with integer `exit_code:0`,
  no ongoing `session_id`; its output may be empty because the actual wrapper
  launch redirects stdout/stderr to a retained log;
- its separate `review` has `status:PASS`, `kind:legacy_main_v3_outer_wait`, exact
  `wrapper`, `started`, `process`, `recipe`, `outer_wait`, `result` and `binding`
  references, and the positive `session_id` whose terminal response the coordinator
  actually observed. This review associates the tool session with the reviewed
  launch and receipt; the controller does not invent that association from a PID.

The normal new-controller predecessor path still requires explicit
`root_reaped:true`. A paused legacy result never qualifies. Synthetic tests use
fixture constants and native-shaped records to test the adapter and negative
provenance cases; they do not claim to have executed the hardcoded private wrapper.
A tiny actual CLI transition smoke may instead use its own honestly supervised
old-main receipt with explicit reaping. Real private bootstrap uses the actual
legacy wrapper/result/tool-wait artifacts after main finishes.
