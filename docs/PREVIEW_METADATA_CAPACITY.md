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
metadata, ready-batch and publication metadata, the sole actor 64 KiB
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
`Config::validate` evaluates representability only; it does not reserve these
bytes or enforce the report against a process budget. A full C admission still
needs a reservation owner and release lifetime at the managed catalog-open
boundary.

Fixed control backing includes each Calls root's three root-capability IDs and
canonical path plus its two optional retained stage IDs. Every active render or
read Job separately owns its spawn root/stage, possible status IDs and Stage ID.
The proxy binding IDs and completed digest are also named contributions.

Executed on macOS arm64 with Rust 1.98.0, the checked formula produces these
requested backing bounds in bytes. They are conservative capacity calculations,
not observed allocations or RSS measurements.

| Configuration | Retained | Active | Startup | Requested |
|---|---:|---:|---:|---:|
| Minimum | 3,261,278 | 5,868,572,026 | 5,364,516,472 | 5,871,833,304 |
| Default | 717,356,644 | 5,868,581,602 | 5,364,516,472 | 6,585,938,246 |
| Maximum | 1,065,973,174,780 | 10,385,736,502 | 5,364,516,472 | 1,076,358,911,282 |

The integrated gate passed three formula tests and 21 affected transport, codec,
cache and real-process tests. The bounded error representation preserves typed
`anyhow` context through `Error::downcast_ref`; walking only standard error
sources loses context markers. A regression test covers a filesystem failure
receipt together with encoded-budget and stage-busy markers.
