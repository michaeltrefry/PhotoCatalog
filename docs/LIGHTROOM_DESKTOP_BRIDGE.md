# Lightroom inspection desktop bridge

`application::lightroom_bridge` exposes the complete existing inspection Workbench
through `Request::Lightroom`. It has an explicit lifetime independent of the
foreground PhotoCatalog catalog. Closing or opening that catalog does not stop
inspection. Explicit workbench Close and application shutdown cancel the owned
inspection thread and capture process; reopening waits until they are reaped.
The bridge permits one desktop inspection owner per process. Standalone Workbench
callers retain their documented responsibility to exclude other same-process
inspection SQLite access.

This surface covers discovery, owned capture, inventory registration, capture
addition, bounded inspection resume, every retained row/report/path/issue/packet/
conflict query, explicit family assignment and choice, selection review and
immutable sealing. Original/sidecar inspection is a separate explicit action.
CaptureManifest explicitly reconciles completed retained capture evidence after
lost replies. OpenExisting never resumes work automatically. Full migration
admission, execution, reconciliation, repairs and workbench UI acceptance remain
separate required S12 work; inspection completion does not claim them complete.

## Wire and control ownership

The outer command is `{command:"lightroom",args:{request}}`. Rust variants are in
`src/application/lightroom_bridge/wire.rs`; browser types and the transport
function are in `desktop/src/lightroom.ts`. `Options` advertises default Workbench,
source inspection and selection budgets. Every numeric identifier, cursor,
counter, PID and budget uses a canonical decimal string. NativePath code units
retain the existing bounded integer-array representation. The executable cannot
be supplied by a request: capture uses the trusted `application::Config` worker.

Open carries a new explicit attempt token and Create or OpenExisting mode.
Subsequent admissions bind workbench, generation and the currently observed
operation. Generation alone is insufficient: a completed read may have replaced
the result without changing inspection evidence. Status can recover the current
open attempt after a lost reply. Stale attempts, operations, generations, review
and result tokens do not authorize replacement or replay.

Status, Result, Cancel and Close bypass the catalog command queue and never join
workers. Close acknowledges Closing after signaling cancellation; Closed is
observed only after worker exit. Actor maintenance joins only a finished worker.
Application shutdown signals inspection cancellation before catalog/native joins.
Blocking filesystem reads can delay worker drain, but do not block cached control
access. One ordinary Plan or one SelectionReview owns the inspection database.
ReleaseReview is explicit and closes all review/source handles before reopening
the pinned inspection writer.

## Bounded exact inputs and results

Every serialized outer request and response is at most 128 KiB, or a smaller
configured bridge allowance, including JSON escaping and envelope fields. Core
request/result admission remains separately configurable. Small typed actions and
all queries are direct admissions. Inventory JSON, selection-request JSON and
exact approval JSON use one explicitly managed in-memory input slot:

1. InputBegin binds purpose, total UTF-8 byte length and optional expected BLAKE3.
2. InputAppend supplies the exact next byte offset and a UTF-8 fragment. Follow
   Options.chunk_bytes and minimum_nonfinal_chunk_bytes. Every response reports
   received bytes so a lost response can be reconciled without replay.
3. InputFinish requires the declared complete length, checks any expected digest,
   and publishes the immutable input token and computed BLAKE3.
4. An explicit action consumes the complete token of the correct purpose.
   InputDiscard releases unwanted input; a rejected admission retains it.

Omitting the expected digest means there is no independent client transport-digest
verification. It does not authorize sealing: Seal still requires the caller's
explicit pinned approval digest and current selection review token. Approval bytes
and whitespace are preserved exactly. Neither staging nor polling asserts that
Lightroom is closed, chooses a family, approves a destination/policy or converts
TEST approval into canonical migration approval.

The slot retains chunk strings and bounded metadata. It charges owned string
capacities and chunk descriptors against three times declared raw bytes plus
256 bytes of fixed allowance; encoded request/reply buffers have separate bounded
envelope allowances. Joining and typed decoding run only on the existing
Workbench thread, with cancellation checked around bounded decode and between
chunks. No staging files or second SQLite worker are introduced.

ResultPage returns exact UTF-8 fragments of opaque core JSON with operation and
result-token identity, byte offset, next offset and total bytes. Fragments must
be assembled without parsing intermediate pieces. `ResultAssembly` enforces exact
identity and contiguous byte offsets. `parseLosslessJson` is an optional bounded
review aid: every AST node is tagged, object entries preserve duplicate/unsafe
keys and ordering, and numeric lexemes remain strings. It never routes source
numbers through JavaScript Number. Exact raw text remains the authority; the AST
is not approval serialization or a source identity replacement.

Picker choices preserve NativePath units and do not create files. Future capture,
workbench and seal directories are admitted using core create-new semantics on
explicit action. Approval destination is an exact catalog root; choosing it does
not create, open or import a destination catalog.
