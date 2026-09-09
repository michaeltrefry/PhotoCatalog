# Preview Stage B execution protocol 1

Status: source candidate, unbuilt and unrun. Independent review and the parent's
explicit hardware-lane grant are required before execution. This supplements
PREVIEW_EXPERIMENT_PROTOCOL.md; it does not replace its workloads or acceptance
budgets. The sealed 512/1600 JPEG80 selection and historical Stage A artifacts
remain unchanged. Stage A timing was exploratory under recorded background load.

## Worker memory gate

The first gate uses the same frozen 30-input manifest (22 private, two CC0 public,
six procedural) and its independent source dimensions. Source paths stay outside
Git. It launches one fresh `preview_runtime_probe worker` for each input, in frozen
manifest order. Each probe launches the actual application's `--preview-worker`,
using the production owner lease, full-original decoder, preparation, two selected
encodes, and complete result validation. No embedded-thumbnail substitute, render
retry, per-camera setting or parallel worker is permitted. There are 30 owner
probes, 30 native worker grandchildren and 30 untimed artifact-verification probes.

Freeze an explicit DecodeLimits JSON and its hash with the probe/application
binaries and manifest before running. The initial isolated memory qualification
uses the full decoder's compatibility limits: encoded input 536870912 bytes,
intermediate pixels 18446744073709551615, individual allocation allowance
18446744073709551615. Historical native/raster allocation and 100 MP final-surface
limits still apply. These are individual admission checks, not an aggregate RSS
limit or a production preview configuration. The isolated cohort establishes
whether the provisional worker allowance can actually accommodate the complete
pipeline before that allowance is frozen for a service experiment.

Both JPEG80 outputs are requested together with an 8 MiB encoded-output limit.
Each worker records process `getrusage(RUSAGE_SELF)` high-water RSS after full
source decode, both prepared/encoded tiers, and final source hash verification.
macOS returns bytes; Linux KiB are converted to bytes. Only the bounded 64 KiB
receipt serialization/write follows that sampling point. Windows explicitly
reports this measurement unavailable and cannot pass the Mac memory gate by
substituting zero. The owner independently materializes both resulting RGB8
surfaces through the production decoder; its process memory is separately observed
by host telemetry and is not mislabeled as the native child's high-water value.

The proposed worker reservation is fixed as
`ceil_MiB(maximum_of_30_child_peaks * 1.25 + 64 MiB)`. The 25 percent is an explicit
accounting margin for input/platform variance, and 64 MiB covers bounded owner
receipt, validation and encoded buffers. This formula is not a newly invented
performance acceptance threshold, proof for untested cameras, or aggregate OS
enforcement. The coordinator must review all 30 peaks and freeze the resulting
reservation and total worker allowance before any concurrency experiment. The
existing 4 GiB browse RSS gate remains separate. If the cohort fails or memory
telemetry is unavailable, no reservation is admitted from a passing subset.

The probe records failure phase, completed artifacts and anchors even when a
later operation fails. Each saved JPEG is reread, hashed, checked for dimensions
and completion, fully decoded and reconciled against its recorded decoded RGB8
digest in an untimed verifier. Python binds independent SHA-256 identities to the
actual files and receipts. Source SHA-256 values are checked before and after the
whole campaign; each worker also verifies its own source BLAKE3 before/after work.
The process timeout is 300 seconds, coordinator timeout 360 seconds; timeout kills
and joins the owner, whose EOF lease terminates its native child. A failure stops
the campaign and is retained. No best-of or replacement samples are collected.

The coordinator starts the reviewed passive host observer before the first child.
Every child and verifier has UTC plus monotonic boundaries, so host observations
can be aligned without reconstructing intervals. Host/OS/RAM/storage provenance,
manifest/limits/binary/source/Cargo-lock identities, stdout/stderr and partial
receipts are preserved in a new exclusive private output directory. Missing GPU
data remain unavailable; neither successful execution nor telemetry collection
awards a quiet-host claim. APFS Data/firmlink storage attribution must be recorded
from actual paths, retaining uncertainty instead of treating `/` as proof.

Execution, only after review and grant:

```
python3 scripts/preview_worker_campaign.py --manifest PRIVATE_MANIFEST \
  --limits FROZEN_LIMITS --binary ABSOLUTE_RELEASE_PROBE \
  --worker ABSOLUTE_RELEASE_APP --output NEW_PRIVATE_DIRECTORY \
  --lane-token coordinator-authorized
```

## Remaining Stage B gates

Current-renderer qualification prepares new lossless references for all 30 inputs
and the selected pair. Exact unchanged prepared-pixel digests can reuse sealed
visual judgments; changed references (including the repaired G15 RAW highlights)
require inspection of the selected JPEG80 outputs against the new reference.
No historical source, reference or blind judgment is rewritten. These additional
quality preparations do not run inside the native memory measurement child.

Flat versus two-level prefix layout uses 10,000 and 100,000 distinct logical
asset/revision keys and genuinely byte-distinct valid JPEG objects. For each
synthetic entry, insert one JPEG COM segment immediately after SOI, carrying the
fixed ASCII prefix `photocatalog-layout-v1:` and its zero-padded 10-digit entry
number. The 30 selected JPEG80 payloads are used round-robin; this lossless metadata
construction changes encoded bytes/content hashes while preserving decoded RGB8.
Verify decoded equality against the corresponding selected source payload, and
assert distinct content hashes, keys, actual files and directory entries. The
same entry number and source payload produce identical bytes in both layouts.
Report the constant 37-byte marker overhead per object separately and disclose
the 30-image pixel diversity. No content-hash deduplication is enabled. Record payload bytes, file allocation,
directory allocation/count, manifest/index/WAL/lock/marker overhead. The fixed
three sequential and three seeded random passes include read/checksum and preserve
first-pass versus later OS-cache state. This fixture construction cannot forecast
the user's real library format frequency or filesystem footprint by itself.

The subsequent production-service harness must preserve the original frozen
200-thumbnail first-page, 100 warm/20 fresh process trials, 100-viewport navigation
trace repeated ten times, standard/constrained cache and queue limits, offline
originals, and exact quota/disk-failure gates. It must call `Catalog::browse` and
`PreviewService::cached` with retained returned-pixel ownership included in RSS.
DB lookup, read/hash, full RGB8 decode, queue delay and wall intervals are recorded
separately. The service source checkpoint is not measurement evidence. Layout and
resource defaults stay configurable and unfrozen until these gates are reviewed.
The existing first-visible-page target applies only to the measured headless
component here; desktop/UI/frame-time proof remains S12 work.

Only the first worker-memory probe/coordinator is implemented by this protocol
checkpoint. Layout/service/navigation coordinators and remaining fault coverage
are still required S6 implementation, not deferred scope or completion claims.
