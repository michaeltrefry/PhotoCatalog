# Selected Lightroom import

The Rust migration worker imports an explicitly selected set of inspected Lightroom captures into one destination catalog. Photo paths remain external and retain their filesystem folder hierarchy. A physical path may be shared only through an explicit overlap policy; each selected master and virtual copy keeps its own source identity, metadata and current edit recipe.

The input is a sealed schema3 inspection database with an exact selected/excluded partition, final family decisions, capture manifests, file identity and whole-file BLAKE3. The approval document must match the seal. Opening the worker verifies this input and holds read-only cooperative source locks. Run it in an isolated process and exclude non-cooperative writers for its lifetime. It never opens original photographs to reconstruct missing metadata or follows original capture locators to find raw companions.

## Worker command

`lightroom_migrate run` accepts explicit input files and a destination directory:

```sh
lightroom_migrate run \
  --destination /absolute/test-catalog \
  --seal /absolute/selected-seal.json \
  --approval /absolute/authorization.json \
  --policy /absolute/import-policy.json \
  --max-steps 10000 --max-seconds 60 \
  --stop-file /absolute/control/STOP
```

The seal and policy are serialized `InputSeal` and `importer::Policy` values. The policy supplies an import owner, path and keyword overlap decisions, and explicit sealed-copy mappings for capture artifacts. `RequireDecision` pauses at a conflict. `ReuseExactPath` shares only identical native path bytes; `ReuseExactHierarchy` reuses exact keyword hierarchies and synonyms. Neither merges independent logical-image edits.

Repeat the command with identical inputs to resume. The output reports `paused`, `stopped`, `needs_decision` or `complete`, with the durable run identity and cursor. Progress goes to stderr. Source admission has its own deadline; the work budget starts after admission and is checked between steps. A stop file is also checked between steps, so the admitted read/transaction may finish first. Real integrity and I/O errors remain failures. A process interruption leaves completed chunks and source receipts available for replay.

```sh
lightroom_migrate status --destination /absolute/test-catalog --run RUN_ID
```

Status opens the destination read-only, requires the current schema and does not migrate it. It works without the inspection database or originals.

## Retention and compatibility

The worker retains selected schema objects, typed rows, references, paths, metadata packets, issues and raw catalog/companion files in bounded compressed chunks. All selected inspection data enters custody before native projection; raw catalog and companion copies must complete before reconciliation accepts the run. Source-table paging uses indexed cursors. Missing and oversized interpretations keep retained evidence and explicit compatibility results.

Native projection includes original references, independent logical images, embedded and catalog XMP, supported current Adobe parameters, flags, keyword hierarchy/synonyms/memberships and collection memberships. Sidecar ownership remains unresolved unless separately established. Historical develop settings, snapshots, smart-collection instructions, stacks, group semantics and unsupported fields remain source-bound evidence. Native collection ordering is not claimed to reproduce Lightroom's mixed ordering encodings.

An unresolved metadata conflict must not stop the selected import or silently establish source precedence. For a conflicted rating or label, migration retains the selected Lightroom value as a separate image-local metadata candidate and reports that result explicitly. Equal values still require review when a source association is ambiguous. Imported keyword memberships accumulate in their own source-owned set, keeping competing external keyword sets and their original packets intact. A retained candidate does not imply that its value or keyword membership is effective in browsing and search; the metadata conflict APIs expose the candidates for resolution. Existing successful component receipts remain unchanged on replay, including an image whose flag and label committed before its rating was reached.

Keyword interpretation is bounded. If prefix recovery or the accumulated packet reaches its limit, the source remains explicitly partial with `ResourceLimit` status; subsequent memberships cannot silently promote that partial set to a complete candidate. Every original relationship remains in custody. Updating an imported source follows the existing metadata-choice rules: a choice of its previous observation can become stale and require review, while an independent local edit remains selected.

Variant evidence APIs expose retained current and historical Adobe settings offline. Ambiguous links cannot establish a variant association. Only qualified current parameter mappings enter native recipes; the compatibility result explicitly does not claim Adobe rendering equivalence.

For catalog current settings, the coordinator validates the complete bounded data grammar before choosing the settings container. Bare and returned tables use the root; Lightroom's recognized `s = { ... }` assignment uses `s`. Unknown assignments, malformed data and nested historical parameters do not establish a current container. The evidence API still honors its caller's explicit settings path.

Reconciliation checks selected custody and walked-row counts, native mappings, artifact completeness and exclusions. A mapping epoch is rechecked under writer admission before accepting each report and marking the run complete. Completion of this worker describes the import and reconciliation; it is not approval to replace a user's canonical library.

## Correcting an earlier current-settings container decision

Schema 8 adds a resumable repair for completed imports made by the earlier coordinator, which always selected the root table. Back up the quiescent destination first. Supply the original seal and approval plus a `current_repair::Request` JSON document containing `run`, `expected_complete_progress_blake3` (the exact stored progress bytes), `expected_mapping_epoch`, and `reason`.

```sh
lightroom_migrate repair-current \
  --destination /absolute/test-catalog \
  --seal /absolute/selected-seal.json \
  --approval /absolute/authorization.json \
  --request /absolute/current-repair-request.json \
  --max-steps 10000 --max-seconds 60 \
  --stop-file /absolute/control/STOP

lightroom_migrate repair-status --destination /absolute/test-catalog --repair REPAIR_ID
```

Admission pins the completed run, selection, policy and mapping epoch. A schema-7 destination is checked read-only before upgrading. Repeat the identical repair request to resume; status is read-only and requires the current schema. The work budget and stop file are checked between steps, after source admission.

The repair archives the old completion and reconciliation reports, then visits current-stage outcomes by indexed cursor. It replaces only an original retained-only root decision whose source proves the recognized container. The installed recipe must still be the exact predecessor import revision; a later edit stops adoption. Each changed recipe, metadata receipt, run outcome, compressed predecessor archive and cursor commit together. Already correct or uninterpretable outcomes remain unchanged. Source custody, original files, image identities and folder mappings remain intact.

Ordinary import cannot finish while projection repair is pending. Repair reports `complete` only after fresh reconciliation. Unsupported Adobe parameters remain explicit appearance gaps; rebinding a container does not expand the qualified translation surface or establish Adobe rendering equivalence. Per-item predecessor readback verifies archived byte lengths and digests.

## Correcting a retained keyword-boundary prefix

Schema 10 adds `repair-keywords` for a narrowly identified completed-import defect:
a selected Lightroom `AgLibraryKeyword` row whose name and parent are both typed
Null was retained instead of being recognized as a hierarchy boundary. Its named
children and their image memberships could therefore remain unprojected. A proven
boundary supplies an empty parent path; it is not a named keyword and cannot be a
membership or synonym target.

This repair accepts an explicitly pinned predecessor: dictionary outcomes must
have their sole retained dictionary receipt and no behavior receipt; memberships
must be retained ledger outcomes without native membership receipts; the admitted
synonym count must be zero. It also requires the specified current-settings repair
to be Complete. This is not a general overwrite or reimport command. Ordinary
selected imports retain their broader keyword and synonym capabilities.

Back up the quiescent destination and preserve the predecessor evidence before
admission. Use the original sealed inspection and approval, the existing destination,
and a reviewed `keyword_repair::Request` document:

```sh
lightroom_migrate repair-keywords \
  --destination /absolute/test-catalog \
  --seal /absolute/selected-seal.json \
  --approval /absolute/authorization.json \
  --request /absolute/keyword-repair-request.json \
  --source-open-seconds 600 \
  --max-steps 1000 --max-seconds 60 \
  --stop-file /absolute/control/STOP

lightroom_migrate keyword-repair-status \
  --destination /absolute/test-catalog --repair REPAIR_ID
```

The request has these fields; derive the digests and locators from the exact
completed destination and its selected retained evidence, not from filenames or
re-serialized terminal summaries:

| Fields | Binding |
|---|---|
| `run`, `expected_complete_progress_blake3`, `expected_mapping_epoch` | Existing completed import, BLAKE3 of its exact stored progress bytes, and current mapping epoch. |
| `current_repair`, `expected_current_repair_progress_blake3` | Existing completed current-settings repair and BLAKE3 of its exact stored progress bytes. |
| `expected_dictionaries`, `expected_memberships`, `expected_synonyms`, `expected_captures` | Exact run-ledger stage counts and selected capture count. The repair requires zero synonyms, 1–1,024 dictionaries, at most 10,000 memberships and 1–16 captures. Actual source counts are request data, not application constants. |
| `expected_roster_blake3` | Digest chain of every admitted old dictionary and membership outcome and applicable dictionary receipt, in the ordering below. |
| `predecessor_evidence_blake3` | BLAKE3 of the independently prepared predecessor evidence document. The native repair binds this digest; the execution admission must separately verify the document bytes and their meaning. |
| `roots` | One `RootProof` per selected capture, each with `origin`, `raw_digest`, `retained_table`, and `table_digest`. `origin` is a `SourceRecord`: `retained_record` plus the exact typed `source` key. Row and table digests refer to their selected retained records. |
| `reason` | Nonempty explanation, at most 4,096 UTF-8 bytes. |

A source key contains `capture_revision`, `table` and `key`. Cells use the tagged
inspection serialization, for example `{"type":"Integer","value":99}` for a
synthetic integer key, not an externally tagged enum or a converted text key.
Every admitted root must have the exact selected row/table proof, typed Null name
and parent, and its own dictionary ledger entry. Unknown request fields are rejected.

The old roster starts at BLAKE3 of the UTF-8 adapter string
`lightroom-keyword-repair-v1`. Visit `Keywords` followed by `KeywordMemberships`,
with ascending retained-record sequence in each stage. Each next digest hashes the
native compact JSON tuple `[previous_digest, encoded_stage, record, outcome_digest,
receipt_digest_or_null]`. Here `encoded_stage` is the JSON-encoded stage string,
including its quote characters; `outcome_digest` hashes exact old ledger bytes.
An applicable receipt digest hashes the eight-field native `Receipt` serialization
in declaration order: `source_identity`, `slot`, `owner`, `adapter`, `input_digest`,
`result`, `retained_record`, `proof`. Preserve the original `result` and `proof`
strings. The shared synthetic vectors in
[`tests/fixtures/keyword_repair_wire.json`](../tests/fixtures/keyword_repair_wire.json)
check this contract against Rust and Python. They are not actual source evidence.

The CLI validates the approval and read-only sealed inspection before taking the
destination import lock. It checks a schema-9/current destination read-only before
opening it for the transactional schema upgrade. The core repeats input, policy,
completed-predecessor, mapping and root checks; a stale or different request is an
error. Run under the same isolated ownership arrangement as the original importer;
exclude non-cooperative source and destination writers. Status requires the current
schema, opens read-only, and neither opens the inspection nor upgrades the catalog.

Repeat identical inputs and request to resume the same repair ID. The work deadline
starts after source admission and repair admission; work and stop limits are checked
between bounded steps. Output reports `paused`, `stopped` or `complete`, plus `repair`
progress and step timing. A paused/stopped exit is successful bounded execution, not
repair completion. Retain the error and saved checkpoint before investigating an
integrity, stale-state or I/O failure; do not regenerate the request to bypass it.

The repair first archives original completed progress and reports, then plans the
exact old dictionary/member roster and verifies its chain. Named dictionaries are
processed parent-first after all admitted boundaries are accounted for. Each
replacement commits its old-state CAS, native result, provenance receipt, ledger,
archive state and cursor together. Memberships use the existing image-local,
source-owned keyword accumulator: effective local choices, competing metadata,
current edit recipes and earlier current-settings archives are preserved. The
existing resource-limit and stale-choice semantics described above still apply.
A repaired membership candidate is not a claim that its term is the effective
browsing/search membership.

The run remains pending while repair owns it; ordinary import, current-settings
repair and unowned reconciliation cannot publish completion. Verification checks the
archived outcomes and replacement receipts before fresh per-capture reconciliation.
Only the final guarded transaction publishes both import and keyword repair Complete.
Progress counts describe repair items, not unique photos, named keywords or effective
memberships. Unsupported source instructions and original custody remain retained.

For bounded offline predecessor readback, the Rust API
`Catalog::keyword_repair_predecessor(repair_id, stage, retained_record)` accepts
only `importer::Stage::Keywords` or `KeywordMemberships`. It verifies the compressed
archive length and digest and returns the exact `old_outcome` bytes, original
source identity, optional old receipt (including unchanged `result`/`proof` strings),
optional planned decision, disposition and optional new-outcome digest. Dictionary
archives contain the planned decision; ledger-only membership predecessors retain
`None`. Read the current result with `migration_organization_projection` for the
source key and `keyword_membership` slot. Do not substitute the current decision
into the historical predecessor archive. Use actual retained
record IDs from the admitted roster or repair progress. Missing/unarchived records
and other stages are errors; this API does not manufacture an evidence anchor.
`Catalog::keyword_repair_progress` reads the durable cursor. These reads need no
originals or sealed-source lease once the destination is open. There is no CLI
predecessor-body command; `keyword-repair-status` returns only the checkpoint.

## Separately qualified PSD evidence

The `prepare-supplements` command accepts a bounded JSON array of `supplements::Request` values and retains already qualified inspection documents and their exact payload copies:

```sh
lightroom_migrate prepare-supplements \
  --destination /absolute/test-catalog \
  --requests /absolute/supplement-requests.json
```

It returns normalized proof pins and evidence IDs. These must match the final selected seal and policy. The original inspection's historical status remains unchanged. Preparation reads only explicitly mapped proof copies, verifies their expected bytes and hashes, and does not probe the original PSD files again.
