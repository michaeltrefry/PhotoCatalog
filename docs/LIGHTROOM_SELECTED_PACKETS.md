# Selected catalog packet inspection

Selected PACKETS inspects external metadata only for explicitly authorized current
catalog members. FULL and PATHS still retain and validate the complete requested
catalog roster. Excluded members remain in the plan and family evidence; their
external packets are labeled `external_packets_not_selected`. This operation does
not choose a catalog in the plan, import a backup, migrate data, or award S9
acceptance.

The controller and contract extend the frozen runner through its existing Runner
methods. The first transition starts from independently reviewed, clean terminal
PATHS. Its explicit transition receipt allows a new controller/contract and a
qualified executable while preserving the original config, base binding, driver,
generation helper, Python profile, temporary destination and memory limits.
Funding is separately phase-bound. Later selected continuations require the exact
same selection, native profile and code, plus a reviewed clean predecessor.
Ordinary failed predecessors remain rejected.

Historical private failure bridges and cadence packages are not copied into the
repository or rewritten. Their terminal PATHS review is the immutable boundary
anchor. The archival predecessor validator reads that recipe, result, phase and
execution proof; it does not reopen historical journals, current pointers or
process identities. Live admission still checks the current checkpoint and pause
under the existing owner and runner locks.

## Selection document

Every descriptor below is an absolute `path` plus `sha256`. Recipe
`packet_selection` references this exact document:

```text
protocol: 1
kind: selected_current_catalog_packets
full: descriptor of recipe.input
paths: descriptor of recipe.paths_review
paths_review: descriptor of independent terminal PATHS review
proposal: descriptor of the retained proposal (provenance only)
authorization: descriptor of the reviewed user-authorization receipt
selected: [{family_id, family_evidence_digest, candidate_key, revision, reason}]
excluded: [{candidate_key, revision, disposition: external_packets_not_selected}]
```

Selected and excluded entries must form an exact partition of the FULL requested
roster. Each eligible family must have exactly one selected member matching the
PATHS family digest and membership. Missing or unresolved whole families are
rejected; the code never supplies a fallback choice. Reasons must contain
non-whitespace text and occupy at most 4,096 UTF-8 bytes. Dispatch follows original
FULL order, irrespective of selection-document order.

Authorization has exactly `status: USER_AUTHORIZED`,
`selection_body_sha256` (canonical document with `authorization` omitted),
`source_message`, `quote`, and `reviewer`. The preserved message must have
`role: user`, a nonempty `message_id`, and text containing the nonempty quote.
The independent reviewer must establish that the actual message authorizes this
exact selection. The program verifies receipt association; it cannot authenticate
or interpret a fabricated conversation. Tests use explicitly synthetic messages,
not actual user authorization.

Packet enrichment changes family evidence. Any later application of a catalog
choice must review the new family digest and preserve the user's selected member;
this phase does not call `choose` or silently substitute a different member.

## Qualified successor executable

Recipe `native_execution_profile` references exactly:

```text
protocol: 1
kind: qualified_selected_packets_native
base_binding: original run binding descriptor
base_driver: frozen runner descriptor
native: {path, sha256, bytes, source}
build: descriptor of build receipt
qualification: descriptor of independent native qualification
```

The native source is a full commit ID; the executable is a distinct ordinary file,
1 byte through 256 MiB, streamed and hashed before dispatch. The historical
`run/lightroom_inspect` remains unchanged. Build evidence must say `BUILT` and
repeat `native`. Qualification must say `PASS`, repeat `native`, `build`,
`base_binding` and `base_driver`, specify `schema_version: 3`, name a reviewer and
reference 1–16 evidence documents. Its exact assertions are
`cli_schema_compatible`, `unchanged_packet_values`,
`stability_and_fallback_verified`, and `source_preservation_verified`, all true.
An executable built from a branch lacking the S9 CLI is not qualified by these
Python tests. Actual CLI equivalence and platform evidence are separate gates.

The owner writes `native-execution.json` before dispatch; the child writes
`native-execution-consumed.json` with its PID. Both references are included in the
result and reviewed against the actual process receipt. Native command records
use an effective binding containing the successor source/digest/length and both
the original binding digest and native profile. The Runner's base binding is
restored in `finally`, including failures; original source identity is not
relabelled. Existing Python profile authorization/consumption remains required.

## Transition, commands and pauses

Only the first selected recipe contains `packet_transition`. Its exact PASS
schema is produced by `packet_transition_expected`: old result/review and recipe,
original binding, selected document, native profile, new controller and contract,
unchanged Python profile, current journal, first command and fixed-invariant
digest. This is an independently supplied review, not an execution grant. The
ordinary exact `EXECUTION_GRANTED` recipe-body grant is still separately required.
The preparer must not create either approval from a proposed choice.

Command keys and output names include both the FULL digest and selection digest.
Allowed commands are fresh `discover`, selected-revision `check-paths --packets`,
`paths`, `packets`, `metadata-conflicts`, `issues`, `report`, and `families`.
All use the original plan and fixed config `page_limit`; there is no packet batch
override. Capture, add, migration and choose commands are rejected before the
Runner can reserve a command. Every invocation, including replay, first compares
fresh discovery with the retained FULL ending inventory. A pause propagates to
the existing owner rather than becoming a swallowed member error.

Selected attempts use a 3,600-second soft pause and unchanged 4,800-second sampled
emergency stop. Other repository modes retain their previous 600-second soft
slice. Selected early-pause guards request a boundary at 12 MiB of new result
metadata or 18,000 new commands, retaining the 16 MiB/20,000 terminal limits.
The guard is sampled/command-boundary headroom, not a hard bound on the next
record or a guarantee that no reservation precedes pause installation. Funding
amounts and memory ceilings remain explicit recipe inputs and monitored
estimates, not physical quotas. No automatic next phase or failed retry occurs.

## Independent selected audit

`lightroom_selected_packets_audit.py REQUEST SHA256` runs under the existing
bounded outer owner. The request pins `controller`, `contract`, `auditor`,
`recipe`, `result`, raw `terminal_wait`, and independent `terminal_association`,
and supplies `author` and an exclusive `output`. The terminal association is
PASS evidence binding the raw terminal response to this exact recipe/result and
`tool_session_id`; redirected empty terminal stdout is valid. The outer tool
must have terminal exit 0 and no ongoing session. Inner clean pause is exit 1;
inner terminal review is exit 0. Neither is an acceptance award.

The auditor holds existing owner/runner locks and checks current checkpoint,
absence of reservation, native command/step/started/process/stream identities,
complete owner logs, owner start/funding and observed root lifetime, finite ordered
owner/phase/native-command intervals, phase/output identity, selected-only output roster and both
execution profiles. At terminal it also checks per-member native zero endings,
report/page state reconciliation, unchanged PATHS row count/last sequence/identity
digest (allowing updated content and states), terminal empty pages, family evidence and
fresh/ending inventory against FULL. It reads bounded metadata, not PAGE or
packet/source bodies; content/identity digests remain attributed native claims.
It allows 128 MiB cumulative metadata, 16 MiB individual metadata, 16 MiB new
command-result metadata, 20,000 new commands, a 120-second loop deadline and
4 MiB output. The external owner supplies whole-process time/memory bounds and
retains failed audit stderr. This audit is not source-body preservation proof,
final catalog selection, migration acceptance, or permission to run anything.

Local fixtures exercise partition/authorization, successor identity, pre-reservation
rejection, replay/pause, binding restoration on failure and the real metadata
auditor against tiny synthetic receipts. No real catalog choice, native CLI
qualification, original-file read or external packet execution is implied.
