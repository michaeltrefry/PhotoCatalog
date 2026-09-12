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

The worker retains selected schema objects, typed rows, references, paths, metadata packets, issues and raw catalog/companion files in bounded compressed chunks. All selected data enters custody before native projection. Source-table paging uses indexed cursors. Missing and oversized interpretations keep retained evidence and explicit compatibility results.

Native projection includes original references, independent logical images, embedded and catalog XMP, supported current Adobe parameters, flags, keyword hierarchy/synonyms/memberships and collection memberships. Sidecar ownership remains unresolved unless separately established. Historical develop settings, snapshots, smart-collection instructions, stacks, group semantics and unsupported fields remain source-bound evidence. Native collection ordering is not claimed to reproduce Lightroom's mixed ordering encodings.

Variant evidence APIs expose retained current and historical Adobe settings offline. Ambiguous links cannot establish a variant association. Only qualified current parameter mappings enter native recipes; the compatibility result explicitly does not claim Adobe rendering equivalence.

Reconciliation checks selected custody and walked-row counts, native mappings, artifact completeness and exclusions. A mapping epoch is rechecked under writer admission before accepting each report and marking the run complete. Completion of this worker describes the import and reconciliation; it is not approval to replace a user's canonical library.

## Separately qualified PSD evidence

The `prepare-supplements` command accepts a bounded JSON array of `supplements::Request` values and retains already qualified inspection documents and their exact payload copies:

```sh
lightroom_migrate prepare-supplements \
  --destination /absolute/test-catalog \
  --requests /absolute/supplement-requests.json
```

It returns normalized proof pins and evidence IDs. These must match the final selected seal and policy. The original inspection's historical status remains unchanged. Preparation reads only explicitly mapped proof copies, verifies their expected bytes and hashes, and does not probe the original PSD files again.
