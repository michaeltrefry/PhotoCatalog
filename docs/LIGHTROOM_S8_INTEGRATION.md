# Lightroom inspection integration with editing and export

This is a source integration checkpoint for sc-22844, not inspection acceptance,
a replacement for the retained inspector, or permission to execute migration.
The live story still requires complete current-family inspection, companion and
XMP preservation, reconciled counts and explicit uncertainty/selection review.

## Current boundary

The source integration checkpoints below are historical. Current source uses
inspection schema 3 and rejects older derived plans before writable open; see
the [numeric relationship correction](LIGHTROOM_NUMERIC_RELATIONSHIPS.md).
The application catalog remains schema 6. The actual v6 main run retains its
separately frozen schema-2 executable. A fresh corrected final plan will derive
from the preserved captures, without upgrading or copying the old giant plan.
See the [execution ledger](EXECUTION_LEDGER.md) for current validation and the
[phase controller](LIGHTROOM_PHASE_CONTROL.md) for the tested handoff protocol.

## Historical validation checkpoint

The integrated candidate `427e9f61b8b83a2e409ed6ff7e544ce15b68cc11` passed
412 native tests across 36 suites (five intentional subprocess/external-fixture
ignores), 71 Lightroom Python tests, package formatting, strict all-target Clippy,
and binary/example builds. The private gate receipt is
`c1523d8641eaa50a50910319633a3d17a745b8b16a7f738b907eca8b53eb4141`.
Its copied inspector hash `c3f33b25471d76473ce008c9acda80cae00fbe9c1b03513da57f8e21e5c40a56`
is distinct from the archived v6 executable and is not substituted into that run.

After the editing platform corrections, candidate `78c1658` passed all 14 focused
Lightroom library tests and all 21 integration tests; the two intentional ignored
entries are subprocess entry points. The follow-up receipt is
`16143db7e3b4e7289e18eca369e6fb8cff280105a00699216043cfd74d17b188`.
Lightroom source and helpers are unchanged. Merge `bbc6504` includes S8's merged
main `f6d19dc` and has exactly the tested `78c1658` tree; the sole CI conflict was
resolved by retaining the existing Lightroom fixture step. No extra test run is
required for that tree-identical merge.

These builds reused the bounded scratch Cargo target after its prior owner
released it. All frozen measured/inspection executables were already separate
private copies and remained untouched. The source-only instructions below record
the earlier integration review; their then-pending local checks are now satisfied.
Hosted S9 CI and the remaining actual inspection criteria still apply.

## Historical source inputs and conflict resolution

- Editing/export parent: `2ed0e5cbceb857e542562ad4d01b8e75eeff8eff`.
- Inspector and recovery driver: `cda4a65b3089d11c126fff1c377ec8e10365038d`.
- Supplementary funding guard, including integer-zero prospective reserve:
  `f1cccdcbdaae9c373b2282bf918c1808c2c3b879` (descends from the inspector).

The only textual merge conflict was adjacent Python CI steps. The merged Linux
contract job retains the preview and editing suites and adds all
`tests/test_lightroom_*.py` fixtures. Native tests remain in the existing three
platform job. `src/lib.rs` adds `pub mod lightroom`; no other application source,
dependency, native decoder/encoder or application schema change is required.

Static comparison confirms that the inspector's consumed `NativePath` enum and
implementation, `xmp::Projection`, `xmp::project`, and
`configure_catalog_connection` implementation are identical across the inspector
and editing/export parents. The entire `xmp_packets.rs` is also identical. All
imported Lightroom implementation, CLI, runner, tests and existing documents
retain the funding branch's exact bytes. These checks support API compatibility;
they do not substitute for compiling and running the integrated candidate.

The application catalog remains schema 6. The separate `inspection.sqlite3`
remains inspection schema 2, application ID `0x50434c49`. The dedicated
`lightroom_inspect` binary does not route through the application CLI or call
`Catalog::open`. It creates/adopts only the separately authorized derived plan.
Schema identity admission and transactional inspection index upgrades remain
unchanged. No S10 import, application schema migration, editing conversion or UI
work is included.

## Existing evidence remains pinned

The adopted private v6 inspection checkpoint remains bound to core
`0f301c14cd6be7784b513db01ed651f527a10213`, driver `cda4a65`, and its archived
binary, source, command journals and receipts. The integrated source may produce
a different executable even though the inspector Rust files did not change:
the linked application library, native build inputs and renderer code changed.
Do not substitute this build into that frozen run or rewrite its binding.

The existing v6 continuation uses the original binary and reviewed supplementary
funding guard. It requires its separate checkpoint, funding, ownership and lane
admission. Integration alone does not require another large checkpoint copy or
adoption. Any eventual new executable deployment must have its own tested source
and binary identity and explicit evidence transition; that transition is not
performed here.

There is no native `CORE_IDENTITY` constant or source-file hashing macro in the
inspection Rust module or CLI. The exact identity paths are:

- `src/lightroom/mod.rs`: capture `PROTOCOL` is the explicit literal 1.
- `src/lightroom/plan.rs`: `PLAN_SCHEMA_VERSION` is the explicit literal 2 and
  the inspection application ID is `0x50434c49`.
- The internal `capture::capture` function assigns `revision_id` from
  `json_digest(&report.artifacts)`; `Plan::add_capture` checks the same digest.
  `json_digest` uses bounded serialized JSON and BLAKE3. The artifacts bind raw
  source revision/digest/location descriptors, not application source files.
- `scripts/run_lightroom_inspection.py` binds literal `SOURCE=0f301c14...`,
  binary SHA256 `3f2b697423f98e9142828c8b4889652c8c20f068ea98097bc4fd70f1fe0971f3`
  and binary length 19,432,736 bytes. `validate_config`, `init` and
  `Runner.__init__` enforce those declarations together with actual copied
  binary, driver, generation-helper and configuration hashes. Runner protocol
  3 is separate from capture protocol 1 and inspection database schema 2.

Consequently, unrelated editor source additions do not automatically invalidate
retained capture or row identities, and source integration does not force a new
large adoption. They also do not prove a rebuilt executable has the old binary
identity. This candidate retains the private runner's old literal bindings
intentionally: its archived native executable remains the authority for v6.

## Validation required at the original checkpoint

No build, test, database access, image operation or original-source access was
performed for this integration while the editing qualification owned the local
native/I/O lane. After the coordinator releases that lane, use a separate target
directory for the integration and the existing documented native dependencies:

```sh
CARGO_BUILD_JOBS=4 OMP_NUM_THREADS=1 cargo test --locked --lib lightroom::
CARGO_BUILD_JOBS=4 OMP_NUM_THREADS=1 cargo test --locked --test lightroom
CARGO_BUILD_JOBS=4 OMP_NUM_THREADS=1 cargo build --locked --bin lightroom_inspect
python3 -B -m unittest discover -s tests -p 'test_lightroom_*.py' -v
cargo fmt --package photocatalog --check
CARGO_BUILD_JOBS=4 OMP_NUM_THREADS=1 cargo clippy --locked --all-targets -- -D warnings
```

Set `CARGO_TARGET_DIR` to a fresh integration-specific path before these commands;
do not overwrite shared frozen targets or archived executables. The library and
integration suites cover source/WAL capture and publication, family ambiguities,
XMP retention/conflicts, source identity, bounded paging, typed preservation and
schema-upgrade rollback. The Python suite covers replay, failure ancestry,
memory, generation adoption and funding; its POSIX modules belong in Linux CI,
not Windows Python. Then run the repository's complete all-target native test
gate and current preview/editing contract suites before integration delivery.
Applicable hosted Linux/macOS/Windows checks and independent review remain
required; macOS-only results cannot establish Windows locking behavior.

`scripts/lightroom_main_funding.patch` is an existing reviewed unified diff.
Its blank context lines contain the required leading context-space character.
Preserve that artifact; exclude only that file when applying an ordinary
trailing-whitespace check to this merge. Do not run vendor-wide formatting.

## Inspection scope at the original checkpoint

The retained actual checkpoint has fourteen completed member outcomes and one
active member with twenty-four successfully verified pages. Remaining main-only
candidate capture/readback, full relevant companions, path and embedded/sidecar
packet evidence, family recency/ambiguity decisions, reconciled final dry-run
reports and unchanged-source proof are still required by sc-22844. No part of
this integration declares those gates complete or executes any migration.

Confidence: high for the source identity and merge findings; integrated runtime
and cross-platform compatibility remain unverified until the stated gates run.
