# XMP retention, reconciliation and controlled export

S4 stores original XMP carriers independently of image decoding. A carrier is the exact byte sequence recovered from its container, including applicable compression or fragment headers. Separately stored parse inputs identify the original packet indices and transformation; they never replace the original evidence. Compressed catalog blobs are addressed by BLAKE3 and verified when read. [Container coverage and limits](XMP_PACKET_EXTRACTION.md) distinguish complete, absent, malformed, unsupported, resource-limited and changed-source inspections.

Schema version 2 adds immutable source observations, packet occurrences, parsed-model descriptors, indexed common fields, conflict choices and a decision history. Migration is transactional and preserves version-1 asset identities, paths, state, metadata and preview references. A monotonically increasing `render_generation` advances on import attempts and catalog metadata edits; preview work must use the catalog generation as authority. Source files are never changed by import or catalog edits.

Each source has a stable kind/location key and a current observation. Repeating an identical observation is idempotent. A missing sidecar retains its previous metadata and displays its unavailability. A file changing during inspection is retained as diagnostic evidence without becoming the current source revision. Sidecar discovery covers `name.xmp` and `name.ext.xmp`, case variants, and Unicode names. A same-stem RAW/JPEG pair or multiple matching sidecars is explicitly ambiguous. Directory entries are indexed once per scanned directory in temporary SQLite tables rather than enumerated again for every image.

Strict XML validation precedes Adobe XMP interpretation. UTF-8, UTF-16 and UTF-32 transport decoding is strict; DTD/entity declarations are refused. Parsing has byte, node and nesting limits. Malformed packets and projection limitations remain visible in source/model history. JPEG main and extended packets are joined using `xmpNote:HasExtendedXMP` and the verified extended GUID, with all fragments and reconstruction links retained. Missing, overlapping or ambiguous associations require review rather than a silently incomplete editable model.

Common projections include ratings, labels, titles, descriptions, rights, creators, flat/hierarchical keywords, camera/lens fields, orientation, dates and GPS coordinates. Unknown properties, Adobe develop data, arrays, nested structures and qualifiers remain in original packets and full models. Conflicting current values have no automatic winner. An explicit field choice binds a particular model revision; if that source changes, the choice becomes stale and visible. Catalog edits produce a new immutable model. Explicit removals are tombstones so an older sidecar value cannot reappear on the next edit. Scalar edits and appends check full-model preservation, including unknown qualifiers; localized edits change the requested language only.

## CLI workflow

The UI-independent APIs are exposed through `photocatalog --catalog CATALOG`:

- `metadata ASSET` shows the current metadata revision, source availability, effective fields and candidates.
- `metadata-history ASSET --after 0 --limit 50` lists immutable observations and model descriptors; `metadata-decisions` lists decisions and edits.
- `metadata-resolve ASSET FIELD MODEL --expected-revision REVISION` selects a current candidate atomically.
- `metadata-edit ASSET edits.json --base-model MODEL --expected-revision REVISION` creates a catalog-only model. JSON operations are `set`, `remove`, `append`, and `localized`; each supplies a namespace URI and property path. Append also supplies `ordered`; localized supplies `language`.
- `metadata-packets ASSET OBSERVATION OUTPUT.json` writes retained byte evidence to a new file. This output can contain private metadata and belongs outside Git.
- `metadata-export-plan ASSET BASE_MODEL DESTINATION.xmp --expected-revision REVISION` prepares a catalog-stored plan and reads the destination without writing it. Unresolved conflicts prevent planning. Resolved fields are copied as complete properties into the selected full base model, preserving unrelated properties and qualifiers.
- `metadata-export-apply OPERATION` explicitly publishes the planned bytes. It checks both catalog revision and destination revision.
- `metadata-export-discover DIRECTORY` lists recovery evidence; `metadata-export-recover RECOVERY_DIRECTORY` explicitly resumes an operation.

Example catalog-only rating edit:

```json
[{"operation":"set","namespace":"http://ns.adobe.com/xap/1.0/","path":"Rating","value":"4"}]
```

## Publication and recovery

Publication stages and flushes the new bytes and an operation journal before touching an existing destination. It captures the prior destination in an exclusive recovery directory, verifies the captured revision, and publishes via a no-clobber primitive. A changed or concurrently recreated destination is not overwritten. Rollback restores only into an absent destination and otherwise retains both versions for review. Unsupported filesystem primitives, disk exhaustion and uncertain durability produce explicit recoverable receipts.

The capture creates a temporary gap in pathname availability. This is not a filesystem compare-and-swap claim. Captures remain discoverable even after success because an external process can still hold a writable handle to the captured inode. Recovery never silently deletes this evidence. macOS/Linux use file/directory flushes; macOS additionally requests `F_FULLFSYNC`. Windows uses flushed files and write-through namespace moves and requires hosted validation. Cross-platform completion and real-camera evidence are separate from local focused tests.

The original reference RAID and Lightroom catalogs remain read-only. Metadata export validation uses disposable destinations; ordinary source migration is a later tracked operation with its own dry-run evidence.
