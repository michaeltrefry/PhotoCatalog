# Prospective full-campaign outer owner

Source basis: 1d38f6c; bounded implementation checkpoint, no campaign execution or qualification.

The first full campaign has an explicit 86,400-second sampled owner deadline.
This is a failure stop, not a performance target or permission to retry. It is
shorter than the sum of the unchanged action deadlines: 367,800 seconds for the
533 probes and the same 367,800 seconds for their verifiers (735,600 total).
Preparation and the independent final aggregate are separate admitted phases.
The outer deadline includes campaign startup, all inner actions and inline cleanup.

## Fixed process accounting

- 533 probe roots and 533 verifier roots: 1,066.
- 104 warm-service cases times 102 requests: 10,608 worker launches.
- Five first-original cases times 22 requests: 110 worker launches.
- Eighteen export cases times 22 requests: 396 worker launches.
- Six selected-metadata export cases: six worker launches.
- 135 service setup imports and two overlap background jobs: 137 workers.
- Total normal successful workload workers: 11,257; action roots: 1,066.
- One campaign root, one host-identity sysctl, and at most 86,401 ioreg
  invocations (one initial sample plus a one-second wait before each later sample).
- Sum: at most 98,726 scheduled lifetimes within the nominal deadline. This is a
  source-derived successful roster, not a claim that sampling sees every process.
  Shutdown grace can add a few host samples. An explicit 131,072 observed-identity
  cap accommodates them without unbounded tracking; excess fails closed.
- Successful normal live population is campaign + probe + one worker + ioreg = 4.
  An outer active cap of 8 covers the inner guard's four identities plus campaign
  and host sampler, with bounded margin. Inner caps remain 4 active / 256 seen.

## Minimal API and ownership change

Optional keyword-only supervision settings are available on invoke; existing callers retain
all current values. Explicit outer settings: max_active=8, max_seen=131072,
max_telemetry_bytes=8 GiB, max_identity_bytes=128 MiB, max_sample_bytes=8192,
max_identity_event_bytes=512. Keep the 0.1-second sampling interval, 4 MiB per
stdout/stderr, current RSS/disk checks, exception handling and Popen root authority.
The outer launcher must bind its exact argv, source, settings and recipe before
starting the campaign; pending build/preparation gates still reject execution.

Track only currently unresolved/live identities in memory. Append a bounded
identity event on discovery and terminal observation; remove an identity from the
active map only after an identity mismatch, NoSuchProcess or zombie observation
has been recorded. AccessDenied/other errors never mean retirement: stop and retain
that identity for cleanup. A retired identity cannot become live again; a reused
PID with a different creation identity is a new lifetime, admitted only through
current root-anchored ancestry. Never adopt a newly reused numeric PID just because
an earlier children() enumeration mentioned that number. Validate the enumerated
process identity and live root/parent chain before adding it. Continue sampling
already-owned orphan identities after their parents exit.

Each tick and cleanup operate only on active identities; history stays in bounded
JSONL evidence. Cleanup still signals the root through Popen even if identity
collection or evidence writing fails, then signals only revalidated active
identities. Retired-history terminal observations plus active cleanup establish
known-identity absence. Do not claim undiscovered-descendant absence. Preserve
aggregate counts and the hashed, fsynced event ledger in the result. Evidence
write failure does not suppress root cleanup or produce a successful receipt.

At most 864,002 regular 0.1-second frames at 8 KiB each require 7,077,904,384 bytes,
within 8 GiB. Two events for 131,072 identities at 512 bytes require 128 MiB.
Event/frame limits must be checked before writing; finite fields and bounded names
must prevent unexpectedly large serialization. Counters, peak values and active
frames stay compatible with existing verifier expectations; the final result adds
settings, discovery/retirement counts and the identity-ledger descriptor.

## Funding and host evidence

Funding version 2 adds outer telemetry (8 GiB), identity ledger (128 MiB),
stdout/stderr (8 MiB), a 1 MiB outer receipt allowance, and allocation overhead
separately from existing per-action streams. Each inner action additionally funds
256 KiB of identity evidence. Old funded bindings are rejected, not reinterpreted.
The prior campaign host.jsonl was not byte-capped or separately funded.
Its one-second samples contain all host processes, so child-count limits cannot
bound its bytes. The implementation adds an explicit bounded HostObservation output allowance
(8 GiB total / 1 MiB per serialized sample, reject rather than silently truncate). This is an auxiliary evidence capacity cap, not a promise
that every host can run for 24 hours; exhaustion retains failure. Its full allowance is added to funding. Preserve truthful sampled timing, separate cleanup graces,
and unavailable-host-field semantics. No sampling interval or workload changes.

## Focused validation before freeze

1. More than 256 synthetic sequential identities with reused numeric PIDs; active
   set remains small, every observed lifetime gets discovery/retirement evidence,
   and per-tick inspection count depends on active population, not history.
2. Active orphan survives parent exit and is still killed; retired PID reused by a
   foreign process is never signaled or admitted through stale enumeration.
3. AccessDenied, root identity setup failure, event-write failure, identity count,
   telemetry cap and host-log cap failures all preserve evidence and reap root.
4. Tiny actual-child sequential churn and orphan/interrupt cleanup. No renderer,
   database, source image or full campaign needed.
5. Exact 533-case count/deadline/worker accounting and new funded byte components;
   reject omitted or weaker outer settings while preserving inner defaults.
6. Fake monotonic clock proves 24-hour failure including inline cleanup; incomplete
   campaign remains failed, with no automatic retries or qualification promotion.

## Source API / remaining admission

The private outer launcher calls `invoke(command, folder, limits, disk_root,
supervision=binding["outer_owner"]["supervision"])`; its `deadline_seconds` must
come from `binding["outer_owner"]["deadline_seconds"]`. Its exact command and RSS
limits still require the parent-owned launch review. Invoke rejects deadlines
above 24 hours and starts the deadline before launch/identity collection. The
campaign uses the bound host-log settings; inner invocations keep their defaults.
Preparation and aggregate remain separate commands. Their observed process counts
are not charged to the campaign lifetime roster (their evidence bytes remain
funded by the existing seven-generator/one-aggregate allowances).

Successful results bind identities.jsonl, the explicit supervision settings,
discovered/retired counts and bounded remaining cleanup candidate keys. Retired
identities were observed terminal, not necessarily reaped by this supervisor;
the Popen root and active-identity cleanup have their separate explicit receipts.
The source, package, resulting funding, actual outer wrapper and preparation must
be frozen and independently reviewed before any full campaign run.

## Separate preparation owner

Preparation has its own bound preset, distinct from the full campaign:
3,900-second outer failure stop, four active / 8,192 observed identities,
128 MiB process frames, 8 MiB lifetime events, 4 MiB per stdout/stderr,
and host logs capped at 512 MiB total / 1 MiB per record. Its private outer
launcher consumes `funding["preparation_owner"]`; `edit_prepare.prepare` consumes
those exact host settings. The existing 3,600-second preparation deadline,
600-second generator deadline, generator memory admission, and held-original
copy policy are unchanged. Observer errors are checked before/after each source
copy, the separate background copy and every generator. Completed tiny-copy
proofs remain in a failed receipt, and no further copy/generator launches after
a known observer error.

Funding adds a separate 657 MiB preparation owner/host/receipt allowance and
16 additional 4 KiB filesystem allocation units, over the full-campaign owner
allowances. The new plan and admission also bind `preparation_owner`; no earlier
funded amount silently covers these additional bytes. Preparation failure
retains its new namespace and does not authorize resume, retry or campaign work.
