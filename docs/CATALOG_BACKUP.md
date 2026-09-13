# Catalog backup and restore

PhotoCatalog backs up a consistent committed database snapshot, including photo
and variant identities, folder references, organization, edit and undo history,
original XMP packets, metadata choices, source provenance, and retained Lightroom
evidence. Pending jobs and partially completed migration repairs are preserved.
The original photo files stay at their existing filesystem locations and need
their own backup. Preview caches are regenerable and are excluded.

The Rust API is independent of the desktop shell. The command-line interface
exposes the same operations while the Tauri interface is being integrated.

## Create and verify a backup

Set `CATALOG` to an existing catalog directory and `BUNDLE` to a new directory.
Generate a limits file, review its capacity and time allowances for the operation,
then run the backup:

```sh
photocatalog --catalog "$CATALOG" backup-limits > backup-limits.json
photocatalog --catalog "$CATALOG" backup-create "$BUNDLE" --limits backup-limits.json
photocatalog --catalog "$CATALOG" backup-inspect "$BUNDLE" --limits backup-limits.json
```

Receipts are JSON on standard output. Progress is JSON on standard error, emitted
when the phase changes or at most four times per second during a phase. Inspection
does not open, initialize, or upgrade the path supplied as `--catalog`.

The copy uses SQLite's online backup API with a pinned read snapshot. Concurrent
writers can continue committing; their later commits are outside this snapshot.
The operation uses bounded page batches and checks its time, size, free-space,
retained WAL, and busy-retry allowances. Holding a snapshot can retain WAL history,
so a backup can stop when its admitted WAL limit is reached.

A completed bundle contains the self-contained database and a versioned receipt
with its application identity, schema version, byte length, and BLAKE3 digest.
Verification checks the digest, SQLite integrity, and foreign keys. Original XMP
and imported opaque data are copied as stored bytes; backup does not reinterpret
Adobe settings or rescan original image paths.

The destination must be unused. A pending marker protects incomplete attempts,
and completion is recorded only after the copy has been verified and synchronized.
Ordinary catalog opening rejects backup bundles and incomplete restores.

## Restore into a new catalog

Set `RESTORED` to an unused destination directory:

```sh
photocatalog --catalog "$RESTORED" backup-restore "$BUNDLE" --limits backup-limits.json
photocatalog --catalog "$RESTORED" restore-status
```

Restore verifies the bundle, copies its database, and applies any supported schema
upgrade to the new copy. The backup and the previous catalog remain unchanged.
An unsupported schema, corrupt copy, interrupted operation, or failed upgrade
does not produce an openable restored catalog.

The restored catalog keeps original paths and identities. Use the normal relink
review and apply commands if the originals have moved. Browsing and metadata
organization work while originals are offline. Requested previews are regenerated
through the preview service when the corresponding originals are available.

Pending external jobs remain held after restore. `restore-status` returns the
restore receipt and `jobs_held` state. To explicitly release those jobs for later
execution, use the receipt's `restore_id`:

```sh
photocatalog --catalog "$RESTORED" restore-resume "$RESTORE_ID" --acknowledge-pending-jobs
```

This command starts no jobs. It acknowledges that jobs captured in the backup may
be resumed. Export, migration, and recovery commands still enforce their normal
source, destination, revision, and operation checks. Existing external export
staging is not included in the catalog backup; a missing or changed destination
must be handled by the corresponding job's normal recovery or replanning flow.

## Cancellation and operation limits

Each create, inspect, or restore command accepts `--cancel-file PATH`. Create that
file to request cancellation at the next checkpoint. A file that already exists
cancels before the operation starts. Process interruption also leaves unfinished
output protected by the pending marker. Retry a failed attempt with a new
destination; it never overwrites an existing catalog or successful backup.

The limits JSON rejects unknown fields and contains:

| Field | Meaning |
| --- | --- |
| `pages_per_step` | Positive SQLite page batch size. |
| `max_seconds` | Operation deadline. |
| `max_database_bytes` | Maximum admitted database size. |
| `min_free_bytes` | Required free-space reserve. |
| `max_source_wal_bytes` | Maximum retained source WAL size during backup. |
| `max_busy_steps` | Maximum busy or locked step retries. |
| `verification_vm_steps` | SQLite verification instruction allowance. |

These are operation allowances, not measured completion-time or application
memory guarantees. Full verification reads the database. Keep the backup work
off the foreground UI executor.

Rust callers use `backup_catalog_with_control`, `inspect_backup_with_control`,
and `restore_catalog_with_control` with a shared `CancellationToken` to interrupt
SQLite verification or upgrade work already in progress. Progress callbacks
report operation phases and copy/hash progress. The convenience functions without
a token also support callback errors as cancellation at those checkpoints.

Keep the restore receipt and completion controls with the restored catalog.
Missing or corrupt receipt data cannot silently release captured jobs. If restore
controls are damaged, restore the verified backup into another new destination.

## Local acceptance verification — 2026-09-13

The reference Mac passed 662 release tests with five existing ignored tests on
the final production source. One subsequently added test also passed, exercising
a real organization job and queued photo export during backup; that addition
changed test code only. Strict all-target Clippy and formatting passed after the
addition. Independent reviews cover the source, preservation fixtures, and failure
boundaries. Platform CI and delivery state are recorded in
[sc-22846](https://app.shortcut.com/trefry/story/22846).

- A second catalog connection advances a real organization job from ready through
  running to complete at backup checkpoints. A foreground connection sees the
  completed work while the backup retains the earlier committed boundary. The
  queued photo-export plan, selected XMP, and pending item remain exact in the
  restored snapshot; export execution stays held and produces no output.
- Logical table, schema, sequence, text, and BLOB comparisons preserve real XMP
  with unknown properties, metadata choices, independent variants, undo/redo,
  selected incomplete evidence chunks, all ten keyword-repair phases, and all
  four current-settings repair phases, including partial archives.
- An offline restore relinks a moved synthetic original and regenerates its
  missing edited preview through the normal worker path. Restored-job release
  is not needed for browsing, relinking, or this requested preview.
- Fault tests cover low disk, WAL pressure, corruption, unsupported/failed
  upgrades, existing destinations, callback cancellation, interruption during
  executing SQLite verification, database mutation/replacement during inspection,
  and missing/corrupt restore controls. Actual CLI children are killed and reaped
  during both backup and restore; incomplete output is rejected and prior state
  remains usable.

These are synthetic correctness and recovery tests. They do not measure backup
throughput on the user's large catalog or prove the terminal application
performance targets. S10's private safety copies are separate evidence.
