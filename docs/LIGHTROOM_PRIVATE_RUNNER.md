# Private Lightroom inspection runner

`scripts/run_lightroom_inspection.py` is a private macOS evidence coordinator for
the reviewed `lightroom_inspect` binary. It never runs `choose` or migration and
never opens Lightroom SQLite directly. The frozen Rust implementation performs
source capture and plan operations. Source/packet I/O requires the coordinator's
resource-lane release; writing this script is not execution evidence.

Archived protocol 2 targets corrected core commit `7c9bdeb`, separately from its runner
commit. Its macOS native gate passed and the tested binary was copied exclusively
with SHA256 `1e7f5550dbc73325f5b34ff9fd9eeda08e1b53ba62c6f33170f2378b211a965e`
and length 19,380,752 bytes. Initialization fails before output creation if its
binding is absent. The runner records the exact
Python source and complete configuration before execution. The binary is copied
through an opened regular no-follow handle into an exclusive private output and
verified before publication. Later phases reject changed binary/source/config.
Paths retain Unix bytes through Python's filesystem encoding; foreign encodings
are rejected. Outputs must be outside both supplied original roots.

A configuration includes the keys in `DEFAULTS`, plus `tested_binary`,
`catalog_root`, `original_root`, and `exclusive_output`, all absolute paths. The
complete configuration is reviewed and frozen before `init`; defaults are not
silently filled after admission. Fixed planned command deadlines are 120 seconds
normally, 3,600 for capture/direct path inspection, 1,200 for a resume batch, 600
for reports and 900 for family comparison. Output caps are 8 MiB including LF for
pages, 16 MiB for capture/discovery documents, 64 MiB for report/family aggregates,
8 MiB for stderr and 16 MiB for journal documents. Failure to fit a limit is an
explicit failed operation with its bounded log retained, not absent data.

Two draining threads write only the admitted stdout/stderr bytes and signal
excess. The owner terminates/reaps its process group on deadline, output overflow
or interruption, including nested capture workers. Pipes are drained concurrently;
post-exit drain is bounded. Every command has an exclusive numbered directory,
started/process/final records, log digests and a source binding. Atomic journal
updates use unique pending files, so an interrupted update does not overwrite a
previous record. Unpublished pending files remain inspectable.

A completed operation is replayed from its verified receipt on restart. Failed
operations remain failed without a blind retry. A started operation without a
final record requires explicit owned-process/publication review; the runner will
not guess whether a child remains alive or a capture was published. SIGKILL/power
loss cannot execute Python cleanup, so this unresolved state is deliberately fatal
to automatic continuation. A new reviewed attempt must retain the failed capture
and operation. Resuming ordinary completed prefixes does not repeat captures or
reset Rust transaction cursors. Phase records distinguish returned review
artifacts from acceptance and preserve failures/interruption.

After resource admission:

```text
python3 scripts/run_lightroom_inspection.py init PRIVATE_CONFIG.json
python3 scripts/run_lightroom_inspection.py main PRIVATE_RUN
```

`main` preserves fresh discovery, registers an independent plan, captures every
candidate main/WAL/SHM/journal with `--main-only`, and resumes typed reconciliation.
All retained rows are paged and reconciled to reported table totals; issues,
packets and metadata conflicts are enumerated separately. Cursor/source-revision
checks reject incomplete or misattributed pages. Full stdout is retained as bounded
page files, so disk admission must include these additional derived evidence bytes.
An initial conservative space gate uses 12 times discovered main-file bytes plus
32 GiB reserve; subsequent calls preserve that reserve. This is an admission
estimate, not a claim that auxiliary data will fit or a performance target.

The main review retains every candidate outcome, count, capture limitation and
family suggestion/ambiguity. End discovery differences are explicit. A generated
full-capture request template binds that review's digest and each suggested
member's revision/family digest. These are prospective copies, not selections.
Ambiguous families remain separately listed and unselected. The coordinator
reviews this concrete request; ordinary prospective preservation is already
within read-only inspection scope. Actual ambiguous family choices require their
own concrete review and are outside this automatic runner.

```text
python3 scripts/run_lightroom_inspection.py full PRIVATE_RUN REVIEWED_REQUEST.json
```

`full` rejects stale/incomplete discovery or request evidence. Requested full
captures preserve all companion evidence. A new final review plan replaces each
same-locator main-only predecessor with its successful full capture and reuses
other captured evidence; it never counts both captures as separate catalog
members. Failed full copies retain the predecessor with a visible full-copy
failure. Original reports/captures remain unchanged. Final family evidence is
recomputed and no choices are copied. Every full request produces a distinct plan
and provenance chain. Raw SQLite artifact digests/revisions in both outcomes allow
source changes to be distinguished from added auxiliary evidence.

```text
python3 scripts/run_lightroom_inspection.py paths PRIVATE_RUN FULL_REVIEW.json
# After reviewing the direct-file counts/bytes and admitting that I/O slot:
python3 scripts/run_lightroom_inspection.py packets PRIVATE_RUN FULL_REVIEW.json
```

`paths` first requires a complete unchanged full-review ending inventory and a
successful full-capture/inspection provenance chain for every requested member.
A failed full copy or main-only fallback is not admitted for direct file I/O.
Every invocation, including replay, records a new fresh discovery admission and
rejects drift before direct lookup. Start/end inventory deltas remain explicit;
a changed metadata-phase ending inventory blocks packet admission. Completed
phase reports are immutable, with separate admission receipts on replay.

`paths` performs direct metadata-only lookups for prospective full members. Its
pages include available-reference bytes and per-status counts; missing and foreign
paths remain visible. `packets` requires this completed assessment and preserves
raw packets, transformed inputs and conflicts using the existing bounded Rust
inspection API. It reads no pixels and guesses no original-root mapping. Calling
this phase is the coordinator's explicit resource admission; it must not overlap
another controlled campaign. Failures remain visible and the phase does not call
itself complete. New families reports reflect the changed evidence revision.

The automatic result always leaves `automatic_selection=false`,
`application_consistency=unverified`, and `migration_executed=false`. Per-command
success, SQLite consistency, auxiliary byte retention, projection completeness,
packet gaps and unresolved families are distinct. Neither preserved Adobe text
nor successful packet parsing establishes Adobe rendering equivalence.

Source-only tests are in `tests/test_lightroom_private_run.py`: exclusive binary
binding, resumed command identity, bounded stdout/no implicit retry, deadlines,
interrupted/tampered journal rejection, prohibited actions/overlap and independent
revision page scopes, full/fresh/ending-inventory changes, and failed-full-copy
admission. They use disposable synthetic subprocesses and require no
Lightroom originals or native build. Run them only after lane release:

```text
python3 -m unittest discover -s tests -p test_lightroom_private_run.py -v
```

The real frozen-CLI smoke (`scripts/smoke_lightroom_private_run.py`) constructs two
synthetic catalogs, opaque auxiliary files and a sidecar for a missing original.
It runs main → full → paths → packets, reopens/replays every phase, checks retained
row/conflict counts and unchanged fixture hashes, and leaves no selected family.
Its exclusive private output retains every command and a source/binary-bound
receipt. It uses no RAID input, existing photo, image decode or codec operation:

```text
python3 scripts/smoke_lightroom_private_run.py FROZEN_BINARY NEW_PRIVATE_OUTPUT
```

Historical protocol 1 local runner evidence: six original and ten repaired synthetic tests pass;
the frozen CLI smoke passes every phase and replay, with three captures and no
additional capture on restart. That historical CLI/core was exact f516a6e. This is local
synthetic evidence, not proof of real-family inspection or hosted runner success.

The coordinator may create `RUN/pause-request` to yield the shared resource lane.
The runner finishes its current child, checks this request before reserving the
next command, and raises a distinct paused status without a failed command or
orphan record. Completed-prefix receipt replay remains available. The runner never
removes the request or resumes itself: the coordinator removes its owned request
and invokes the same phase later. A pause is not truncation, completion or
acceptance. The synthetic pause test creates the request during an active child,
proves that child completes, blocks the next child, replays the completed prefix,
and resumes only after explicit request removal.


## Corrective seed adoption (protocol 2; real adoption pending)

The default row page is now 1,000 with the unchanged 8 MiB actual JSON-plus-LF
cap; resume remains 10,000 rows per call. Short pages are not EOF. Page receipts
include SHA256 streams of canonical complete rows and separately their sequence,
source ID, revision and table. These digests do not replace raw capture BLAKE3.

`seed_request` is either null (ordinary fresh plan) or an explicitly reviewed JSON
object embedded in the immutable run configuration. No old binary hash is assigned
to the corrected core. The old run's archived script/config/binary and command
receipts remain unchanged. Seed requests contain:

- `protocol: 1`, absolute `predecessor` (a separate sibling private run),
  `revision`, `candidate_key`, relative `capture_path`;
- `expected_table_counts` mapping every retained table name to its exact count,
  including zero-row tables, and their summed `expected_rows`;
- `binding`, `paused_phase`, `failed_result`, `stop_decision`,
  `table_state_evidence`, `inventory_result`, and `capture_result`, each an exact
  `{path, sha256}` reference relative to the predecessor. Command references must
  address their actual numbered `commands/NNNNNNNNN/result.json` receipts. Expected
  counts are independently reconciled to the pinned private table-state evidence
  during coordinator request review, not inferred from the new result.

The predecessor must retain its pause request and terminal failed resume command
with negative exit code; its journal must end immediately after that command. An
exclusive nonblocking lock on its existing runner lock is held through adoption,
using a read-only descriptor. No old plan SQLite connection is opened. The actual
Rust capture command captures the private plan's main/WAL/SHM into the new run;
unsafe or incomplete preservation is rejected. Actual `create`/`add` commands in a
separate verification plan validate every captured raw and logical digest without
retaining that plan's tables again. A bounded no-follow copy of the verified
logical snapshot creates the new main plan exclusively, with source revision,
copy SHA256 and fsync proof. An interrupted copy remains explicit; it is never
overwritten or treated as published. A completed copy receipt does not claim the
mutable adopted plan still has the snapshot's file hash after reconciliation.

The original catalog capture remains at its immutable old location. An actual
`add` against the copied plan verifies its raw/logical evidence idempotently. The
pre-resume report must show the expected revision, only complete retained tables,
the exact expected counts, pending reconciliation and no other revisions or
carried selections. Every retained row and source identity is paged before and
after one actual corrective resume. The resume must retain zero additional rows
and reach reconciled state; pending direct paths remain explicit and are not read
by seed. Any row, ID, lineage or original-capture change rejects adoption.
Failures retain the new command logs and all previous evidence; no command in the
failed run is rewritten as success.

```text
# Only after native binding, request review and explicit resource admission:
python3 scripts/run_lightroom_inspection.py init NEW_PRIVATE_CONFIG.json
python3 scripts/run_lightroom_inspection.py seed NEW_PRIVATE_RUN
python3 scripts/run_lightroom_inspection.py main NEW_PRIVATE_RUN
```

Seed-aware main performs fresh discovery and records a delta against the complete
pinned predecessor inventory before plan mutation or further original capture.
Any changed/missing/added candidate or incomplete discovery rejects admission.
The adopted candidate references its seed receipt and old capture command; no
new-run create/capture command is invented for it. Other candidates use the normal
capture/inspection journal. No choices are carried or made. Full auxiliary capture
and later original-photo path/packet phases still require their existing separate
review/admission. Pause requests remain effective at every new subprocess boundary
and before starting the private snapshot copy.

New tests model the native boundary with disposable files and record all calls;
they cover exact row/ID preservation, changed identity/count/inventory rejection,
no old-file changes, no source recapture/fabricated command, failed-command
provenance, paused ownership, interrupted copy, exclusive copy/revision checks and
unbound-binary rejection. All 17 runner tests passed locally after lane release. The corrected native gate
also passed 8 module and 20 integration tests, package formatting and all-target
Clippy. Bundled SQLite 3.51.1 used 31 corrected VM steps at each of 100, 1,000 and
10,000 targets, versus 1,515, 15,015 and 150,015 for the original statement. These
are statement-work counts, not a timing qualification. Tests do not replace the pending real native
corrective-plan capture and reconciliation proof.

The later schema-2 paging repair does not change this frozen protocol-2 runner,
its binary binding, or any completed command. This runner does not independently
assert a `create` schema number; the CLI receipt records the actual version.
Schema-2 generation adoption requires the separate reviewed protocol in
[LIGHTROOM_PAGING_CORRECTION.md](LIGHTROOM_PAGING_CORRECTION.md), including a new
binary/runner binding and explicit adopted-prefix records. A new executable must
not be substituted into this runner's existing configuration.

Protocol 3 binds corrective core `0f301c1`, whose tested macOS binary is
19,432,736 bytes, SHA256
`3f2b697423f98e9142828c8b4889652c8c20f068ea98097bc4fd70f1fe0971f3`.
It additionally binds the exact `lightroom_generation.py` helper digest. The
archived protocol-2 driver/binary/config remain unchanged. `generation_request`
and `seed_request` are mutually exclusive. The new `adopt RUN` phase acts only on
an owned sibling private plan and preserves all failure/started records.

The generation request pins predecessor binding, pause, journal, whole-inventory
command, completed outcomes, command-index digest, main/companion filesystem
revisions and expected retained table/revision counts. It declares copy/scan
seconds, bytes/rows, command and capture limits. Nonempty WAL or rollback journal
requires separately reviewed native capture/recovery; this helper refuses it.
Whole-row scans distinguish raw TEXT bytes from BLOBs and real-number bits, with
an explicit row cap and 8 MiB accumulated summary cap. They do not claim streamed
cell retrieval. The coordinator must admit memory for the row cap, Python object
overhead, the 16 MiB SQLite cache and saved-page parsing before execution.

Before publication, the helper verifies physical copy SHA equality, actual saved
completed row-page hashes/counts and active cursor/digest, then compares one
ordered logical scan before the real native upgrade with one after it. It checks
all copied revisions, counts, capture manifests and the absence of carried
choices. Newly executed upgrade commands stay in the new journal; adopted
results use `adopted/NNNNNNNNN` references with original record/digest provenance.
Replay accepts only exact predecessor arguments with the old plan argument
replaced by the new plan argument. Other path/cursor changes fail. Each main-phase
invocation executes a fresh whole-inventory check before newly executed plan or
source work. Completed outcomes are referenced, active page prefixes reconstruct
counts/digests from saved bytes once per phase invocation, and the next missing
page executes against the working copy. Prefix replay never executes old SQL.

Preparation and later execution commands (the exact private paths/config must be
reviewed and frozen first):

```text
python scripts/run_lightroom_inspection.py init REVIEWED_GENERATION_CONFIG.json
python scripts/run_lightroom_inspection.py adopt NEW_PRIVATE_RUN
# Stop here for receipt review; no automatic main-phase launch.
python scripts/run_lightroom_inspection.py main NEW_PRIVATE_RUN
```

An incomplete adoption does not automatically retry or overwrite its working copy.
A pause before adoption launches no copy or child; a pause before the upgrade is
honored by the normal runner command boundary. Any resulting partial adoption
requires review of its preserved started/copy/scan records. The previous run's
pause is never removed by this helper.

Generation adoption requires the Python 3.11+ SQLite `Connection.setlimit` API;
use the reviewed Python 3.14 runtime, which also matches CI. Unsupported runtimes
fail admission before creating the new run or private copy. The older system
`python3` is not substituted silently and the SQLite row cap is never disabled.
The initial system-Python contract attempt and its four runtime-API errors are
retained as failed evidence; they are not a successful adoption test.

Generation admission has a persistent attempt record published before reserving
any discovery command. A failed or unresolved discovery/registration blocks a
restart even if the process died between journal reservation and step publication.
Restart never derives a new attempt merely from the advanced journal counter.
A successful admission is backed by both saved successful commands. An explicit
pause before an unreserved child may progress to a new fresh-inventory attempt
only after verifying the step is absent and the journal has not advanced. Every
attempt retains its own state; the current pointer never erases prior failures.
