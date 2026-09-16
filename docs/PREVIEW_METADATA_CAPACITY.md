# Managed preview metadata requested backing

`Config::requested_preview_metadata_bytes` assembles the requested Rust backing
for preview metadata retained in the managed catalog process. Configuration
validation evaluates the same checked formula, so a target whose allocation
arithmetic cannot represent the configured owner graph is rejected before the
catalog actor starts. The result is informational capacity: it is not subtracted
from `ServiceLimits::working_bytes`, and it does not change any accepted byte,
request, worker or cache-entry limit.

For each long-lived `HashMap<K,V>`, let `T=(K,V)`, `S=size_of::<T>()`,
`A=max(align_of::<T>(),16)` and `Q` be the greatest historical allocation demand.
The requested backing for one possible old/new resize overlap is:

```text
B = next_power_of_two(max(16, floor(16Q/7)))
H(T,Q) = round_up(B*S,A) + B + 16
table(T,Q) = 2*H(T,Q)
```

The service and scheduler job/key tables use `Q=N+W+1`; request, consumer,
completion and read tables use `Q=N+1`. Every one of the `N+W` scheduler jobs
gets its own consumer-table backing with historical `Q=N+1`, even though the
current consumer count across jobs is at most `N`. Decoded and prepared caches
use their configured entry maxima and retain historical table backing after
clear/eviction. The store's 256-entry deferred-touch table uses historical
`Q=257`. Fresh `Vec<T>` construction uses `(2n+8)*S(T)`; a possible
old/new growth overlap uses `(3n+8)*S(T)`.

The retained phase names the service/scheduler/read/cache tables, pointed
descriptor and identity graphs, bounded error strings, key strings and shared
cancellation `Arc` allocations. Decoded ownership includes `D+N` distinct
`Arc<RetainedPixels>` allocation roots: `D` cache entries plus `N` completed
managed read views that may outlive eviction. Their RGB vectors remain under
the existing decoded-live byte admission.

The active phase adds four live-length `RenderWork` graphs beyond each retained
service original, one managed-read work graph, exact
`Arc<Mutex<Option<Job>>>` and `Box<ManagedRead>` roots with their nested
metadata, the exact preview-G `Owner` and configured `Slot` allocation roots,
slot-registry growth, ready-batch and publication metadata, the sole actor 64 KiB
saved-job/record parser, up to `W+1` simultaneous 192 KiB receipt parsers, and
the filesystem relay owners. The saved-job query peak includes all 1000 raw
ID/descriptor rows and the separately accumulated 1000-element `JobView`
result. Relay accounting includes three 1 MiB incoming
assemblies, independent input/output 16 KiB chunks, `4 MiB + 26*16 KiB`
outgoing packet backing, queue growth, packet `Arc` allocations, four accepted
call graphs, result/query/status graphs, `W+1` waiting actions before the shared
stage mutex, the shared pending and proposed requests, and counted native,
record and job serialization destinations.

Managed store recovery retains at most its admitted 1 MiB row graph and parses
one key descriptor at a time; the parser charge excludes the already owned raw
row bytes. Service/store fixed path backing covers the executable, staging,
prepared, manifest and two tier roots.

The active error terms name the `W+1` job admission strings, native status
strings and transport-result error strings separately. A prevalidation stage
admission reply can retain its error up to the actual 1 MiB outer relay cap;
native status strings use their 4096-byte validation. `Task`, managed render,
managed read and encoded delivery replace retained `anyhow::Error` values with
`RetainedError`, which preserves filesystem failure receipts/kinds, worker and
decode statuses, quota, byte-budget, store-resource and I/O classification plus a
`Failure::new`-bounded 4096-byte message. Independent encoded-budget and stage
busy markers remain attached when they accompany a typed filesystem failure.
The report charges old/new bounded strings during adoption for `W+1`
render/read transports. Encoded delivery is phase-exclusive with those
transports and fits the same envelope. Relay fault backing is split by its
source owner: parent latch, child latch, two child call outcomes, admission
query, store query, native pending query and retained native result.

Managed export-directory preparation adds two bounded active ownership terms.
The F typed-graph term charges the G-owned filesystem `Operation` plus the
larger of the simultaneous F request and returned `Response`, including the
boxed request, root-capability strings and every native-path backing. The C
caller term charges the original request's three lease-ID strings and two
native-path backings while its deep-cloned relay `Call` is in flight; that Call
graph remains separately charged. Directory contents and exported image bytes
are outside this read-only preparation phase.

Destination planning and alias admission reuse that same single-operation
export envelope. The export coordinator owns one serial worker and dispatches
one operation at a time, so directory preparation, destination snapshotting and
stateless alias fact queries are phase-exclusive. F canonicalizes the requested
photo destination and reads an existing ordinary file through one held handle,
with the admitted size, object/change identity, byte count and digest rechecked
before returning. F also resolves the no-follow destination fact, followed-file
fact, directory fact and optional canonical-file path. Every reply is bound to
the complete catalog root capability and exact requested path/fact kind; C keeps
the authoritative catalog transaction, indexed candidate selection and all
identity/spelling comparisons. The existing alias contract still performs fresh
synchronous facts at each recheck and does not freeze later external changes.

The active report uses the maximum exact F operation/request/response graph,
the simultaneous original C caller request backing and the retained snapshot
result. Snapshot validation separately charges its path clones/conversions,
bounded detail and the transient serializer output for the three repeated
destination paths before a 64 KiB durable-plan rejection. A separate C phase
envelope charges the two simultaneously reachable 256 KiB SQL rows, their
parsed native paths, the destination projection's path/string allocations, and
the later source/canonical/destination exclusion backings. No destination bytes
survive beyond the bounded snapshot revision. Publication capture/link
filesystem operations remain outside this component.

Managed original verification adds a stateless F planning inspection and one
stateful held-file lease for acceptance/publication. The serial export actor is
the sole caller, so F enforces one active lease without adding configurable
parallelism. C records the exact root capability, requested native path,
transfer ID, step and allowance before Begin dispatch. It retains that custody
through every existing SQL intent/capture/link recheck and clears it only after
a validated Finish or Abort reply. An uncertain Begin is retried with the same
identity before cleanup; repeated Begin, Finish and Abort are idempotent for
that transfer, while foreign provenance cannot close an active handle. Session
Close reconciles custody before SQL/root release, and destructors do not perform
blocking cleanup or discard an uncertain owner.

The report names the C custody `Arc<Mutex<Option<_>>>` allocation, its active
root/path/transfer/revision backing, the simultaneous C caller request, and F's
active-plus-terminal/new-Begin overlap. F's retained `VerifiedFile` includes the
held handle, native path and 64-byte revision digest. Original bytes are hashed
through a fixed 64 KiB stack buffer and are never transferred or heap-retained;
thread stacks remain outside this requested-backing calculation. The already
installed publication recovery branch still acquires no original lease. F's
long-lived admitted-original-root vector and all independently allocated native
path vectors are a separate retained term derived from the 4 MiB startup frame;
the formula includes outer-vector growth and per-path minimum allocation.

Managed photo publication is a separate stateful F lease held by the same
serial export actor. C retains SQL transactions, catalog and alias decisions,
and every intent/finalization commit; F alone retains the operation lock and
the verified payload, destination and captured-file handles while executing
Capture, Link, RestoreLink and their existing verification/recheck steps. Each
request carries the complete catalog root, transfer, step, mode and original
seal or recovery authority. F caches the exact last request digest and bounded
typed outcome, so replay after a lost mutating reply returns the saved result
without repeating the namespace action. Finish and Abort retain a terminal
record for exact replay; a new Begin constructs its complete candidate while
that terminal remains live. Fresh mutation is cancelable before dispatch, but
an admitted namespace step runs to a retained result before reconciliation.

Publication accounting is additive with original custody. C owns the publication
custody Arc (which shares the original Arc without allocating another pointee),
three source/pending/lease seal graphs, two retained root paths, ten retained
lease IDs including the exact original transfer, and its independent caller
request/root/detail graph. A publication seal includes the optional overwritten
file digest as well as its operation, authority and payload strings.

F may simultaneously own seven authority graphs: the old terminal's source,
seal and cached reply, and the new active transfer's source, seal, publication
seal and cached reply. The returned reply is independently covered by the F typed
request/response envelope. Each F record has five lease ID backings: its outer
transfer, cached reply transfer, and root epoch/token/session. Both records
therefore charge ten IDs, two cached root paths and four request/reply digests.
Verification can temporarily hold five VerifiedFiles while replacing two of
three existing proofs, plus the cloned expected FileRevision digest in
verify_restored. All six file-related digest backings and six paths including
the publication directory are charged explicitly.
Old/new receipts and bounded failure strings also remain independently owned.

The F wire parser has a separate 1 MiB prevalidation Content/typed-graph bound.
Journal accounting covers the growing 64 KiB+1 read Vec, parse overlap, the
previous parsed seal while reading plan.json, and cloned seal/receipt/path
conversions alongside serialization output. The metadata-only recovery reader
moves its returned seal into the existing candidate authority graph; it adds no
concurrent parsed owner. The locked restoration seal/plan rechecks use the same
journal scratch slots as strict publication. C/G relay Call clones retain their
existing independent graph and buffer contributions. The fixed 64 KiB hashing
buffer is stack memory; native handles, allocator overhead, runtime state and
RSS remain outside this requested-allocation report. Successor numeric values
must come from the frozen-source runtime capacity report.

Managed ICC acquisition uses one serial export-profile transfer. F retains the
opened stable `Source`, the exact catalog root capability, requested path,
transfer identity, next step and offset until an acknowledged finish or abort.
The 16 KiB profile chunks use the existing raw binary trailer on both relay
hops; neither hop admits the 16 MiB profile as one encoded message. The F typed
graphs, retained transfer root/path backing and C caller request backing are
named active terms. Cancellation alone does not release the F handle, and an
uncertain abort keeps root release from succeeding.
After verified finish closes the read-only `Source`, F retains bounded terminal
path, transfer, step, allowance and byte metadata. Matching finish recovery or
abort cleanup is idempotent; a foreign transfer cannot close an active source
or adopt terminal evidence. The retained-transfer term also includes the
bounded decimal `Source.before` object and change-revision string backings.

The existing eight-token cache and its 32 MiB total byte quota are a retained
term. An in-progress C assembly occupies only the unused portion of that same
quota, so eight vector backings cover eight cached profiles or seven cached
profiles plus one assembly. The term also includes the cache table, both token
strings, requested filename, digest, and each cached `Arc<Vec<u8>>` allocation
root. During lcms validation, `output_profile` temporarily owns two additional
copies of the new profile, each independently bounded by 16 MiB; those are a
separate active term. Native lcms allocations remain opaque native storage.
F performs no ICC interpretation, and no partial assembly becomes a token.
The published `ProfileAdmission` remains in export control status beside the
cache, and a status query can hold one deep clone. Their token, original-name
and digest string backings are separate named retained and active terms.

Tagged serde parsing uses the pinned `serde::__private229::de::Content` and
`(Content,Content)` layouts. For raw length `R`, `N=floor((R+1)/2)`,
`U=N*(max(2S,P)+8*max(S,P))`, and `G=S+U+R`. A parser charge is
`R + layers*G + U + 3R + 8 + partial_typed`. The outer relay packet uses six
tagged layers at 1 MiB; the reserved-control packet uses the independent 16 KiB
lane; native receipts and actor descriptors use two layers. The typed term
covers the complete prevalidation alternatives, including numeric native-path
vectors, `RenderWork` keys, configuration bytes and a partially built element.
Every wire typed graph also includes the maximum exact boxed Call pointee root;
the native alternative includes both its boxed Request and nested boxed
`RenderWork` roots.

Startup configuration parsing is a separate phase. Its typed root is the exact
desktop `ConfigWire`, including the outer `Vec<NativePath>`. For nested path
vectors, both the aggregate numeric-unit growth and every independently
possible eight-element minimum allocation are charged from the byte-derived
path-node count. The final result is:

```text
retained + max(active, startup)
```

The formula deliberately excludes encoded previews (`E`), RGB pixels (`R`),
the scheduler's `max(native phase, transfer phase)` reservations, the exact
delivery input plus binary grant, caller-held decoded-pixel `Arc` allocations
outside the `N` service-owned completed views,
prepared pixel files, SQLite and OS caches, native codec scratch, allocator
headers/fragmentation, TLS, stacks and whole-process RSS. Those owners retain
their existing admissions and evidence requirements. A shared `Arc` allocation
is charged once; handles embedded in typed roots do not duplicate its backing.

The managed route cannot execute the legacy synchronous `tick_read` failure
branch: a managed store has stage calls and dispatches to
`tick_managed_read_queue`, whose retained failures use the existing 4096-byte
`Failure` formatter. The uncapped legacy error formatting is therefore outside
this C aggregate rather than silently included.

The executable report retains every named contribution and its phase, count,
per-owner capacity and total. Focused tests cover default, minimum and maximum
supported owner counts, uniqueness and phase assembly, arithmetic/table/Vec
overflow rejection, independence from all image/native byte budgets, and
bounded retained-error classification round trips.
`Config::validate` evaluates representability. The managed desktop spawn route
also requires an explicit shared `ByteBudget` for this metadata and reserves the
complete report before copying `ConfigWire` or launching C. Refusal retains its
typed metadata error and the existing retryable F startup owner. Multiple C
starts using the same pool compete for that allowance; a failed reservation
never changes its used count. Native working, encoded staging and decoded pixel
budgets remain separate.

G owns this reservation. Catalog close, a drain acknowledgement and pipe EOF do
not retire it: C can still hold configuration or process-level objects. Only an
OS spawn error proving no child exists, or successful `Child::wait` followed by
transport joins, permits release. The charge remains held until the final
transport and filesystem relay parent owners are dropped, because the report
also names parent fault/queue backing that survives C. One Arc guard is shared
by those owners; its backing and added handles are included in the report. A
second C cannot replace an existing parent's admission guard. A lost/panicked
wait owner deliberately retains the charge. Configuration failure before guard
installation and an attempted spawn releases normally.

The private managed route has this metadata-admission contract and separately
requires an explicit native `ByteBudget` whose capacity exactly equals the
configured `working_bytes`. Preview G retains real reservations from that same
native pool through checked retirement; future native owners must receive the
same Arc-backed handle rather than manufacture equivalent pools. Selecting the
route in the desktop app remains open. Tests explicitly supply both synthetic
allowances. This accounting includes the Rust G owner roots but does not charge
their native working-byte reservations to metadata and does not establish the
4 GiB browse RSS requirement or include opaque native/runtime storage.

Fixed control backing includes each Calls root's three root-capability IDs and
canonical path plus its two optional retained stage IDs. Every active render or
read Job separately owns its spawn root/stage, possible status IDs and Stage ID.
The proxy binding IDs and completed digest are also named contributions.

The private migration relay adds independent ordinary/migration multipart
owners, one authority and sixteen recovery slots, complete destination pins,
bounded parsers and queue backing to the same process reservation. Frame size
bounds each fragment; configured message allowances bound complete headers.

The export-only filesystem stage adds two named terms to this same reservation.
`retained.export_stage_f_owner_and_stage_backings` contains the actual F owner
layout, retained directory/lock cleanup custody, root and stage identities, the
bounded 256 KiB native request, and the exact 128 KiB raw plan plus its parsed
Recipe/path/string graph. Its three replay slots separately retain the terminal
seal completion and ordinary user/supervisor replies. The successful completion
owns decoded native facts (including the metadata-note String vector), the seal
snapshot, paths and provenance; bounded failure graphs are also included.
ICC and selected-XMP contents each retain an independent 16 MiB file allowance.
They upload in 16 KiB chunks and verify through a 16 KiB scratch vector: F never
allocates an entire blob for verification.

`active.export_stage_full_envelopes_and_transient_backings` contains three full
current relay-maximum envelopes (`RELAY_BYTES`, presently 1 MiB), simultaneous
work/root/binding/path graphs, native request verification bytes, exact-plan and
receipt serde parse overlap, and four completion graphs for facts, pre-effect
admission and wrapper/cache/reply clone overlap. It separately charges the upload
trailer and streamed hash scratch. Variable typed graphs use the existing checked
serde Content layout bound rather than assuming serialized bytes equal heap
backing. No new user-visible plan, path, output, or metadata-note cap is imposed.
The 64 KiB native receipt limit is independent of the enriched F/C reply; both
actual full reply envelopes are admitted before any durable seal creation.

Export G additionally counts exact owner, configured slot and registry backing,
complete ordinary pending/terminal and privileged replay state, and retained
control request/reply graphs including encoded and typed copies. Its actual
native reservations remain in the separate preview-shared native pool until
explicit retirement; this metadata calculation does not replace that custody.

Managed C also charges its registry, exact replay graphs, preparation and
active-attempt state, retained full ICC/XMP vectors, and transient chunk/request
clones. The stable facade and its boxed managed-service pointee are each counted
once; G retains the predecessor retirement receipt needed for exact replay.

These terms are summed by `requested_preview_metadata_bytes` and reserved from
the existing shared metadata `ByteBudget`. The focused stage admission test uses
that same `ProcessReservation`, proves one-byte-short refusal and retained-owner
charging, then retries the same pool after release. This is requested Rust
backing only. Native worker/codec working storage, OS handles and mappings,
allocator overhead, and observed RSS require their own admission and evidence.
The managed-export integration gate on macOS arm64, Rust 1.98.0, reported the
following totals at source `b85d78d`. All seven capacity tests passed; evidence
is retained in private `sc-23612-desktop-integration-_r1ueez8/capacity.log`.
Executor terms include compact recovery candidates, replay graphs, bounded
private-claim records and overlapping request-validation scratch. Recovery
admits at most 1,024 logical transports across both namespaces and separately
bounds one empty claim intent. The maximum-plan fixture retained 755,200 bytes
of candidate backing against its 135,387,136-byte conservative allowance.
Requested is Retained + max(Active, Startup).

| Configuration | Retained | Active | Startup | Requested |
|---|---:|---:|---:|---:|
| Minimum | 744,320,515 | 13,541,609,808 | 5,364,516,472 | 14,285,930,323 |
| Default | 1,458,415,881 | 13,541,619,384 | 5,364,516,472 | 15,000,035,265 |
| Maximum | 1,066,714,234,017 | 20,607,807,204 | 5,364,516,472 | 1,087,322,041,221 |

These are conservative requested-allocation calculations. Reporting them does
not reserve the aggregate, measure observed allocation, or bound native/runtime
storage or RSS.

The earlier error-representation gate passed three formula tests and 21 affected transport, codec,
cache and real-process tests. The bounded error representation preserves typed
`anyhow` context through `Error::downcast_ref`; walking only standard error
sources loses context markers. A regression test covers a filesystem failure
receipt together with encoded-budget and stage-busy markers.

The admission gate passed nine focused tests: three formula cases, three
reservation/startup-error cases and three actual managed-process cases covering
refusal/retry, blocked wait with a surviving parent, and cold/warm delivery.
A fresh CLI build and package formatting passed. Source review caught the
initial premature release at C wait; final tests require the old parent to drop
before the pool can admit a second actual C/F pair. Evidence and the superseded
initial gate are retained in `sc-22847-c-admission-c5xhb6jj` under private results.
