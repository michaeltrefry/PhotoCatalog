# Explicit recovery after the private inspector's PID-reuse interruption

This is inspection preservation work for sc-22844. It does not select catalogs,
execute migration, assert that Lightroom is closed, or assert Adobe rendering
parity. Original catalogs, photos, failed runs, and predecessor command receipts
remain unchanged.

Protocol3 is a new, exclusive derived-plan generation. Protocol1 and protocol2
retain their previous admission rules. The prospective supervisor separately
uses native macOS process birth seconds and microseconds; its repair must pass
independent review before any new real run. Whole-second `ps lstart` is not an
ownership identity. The old failure's receipt can only prove the older lifetime
that it actually observed; it cannot supply a missing high-resolution snapshot
retroactively.

The only newly admissible failure is the reviewed `owned PID identity changed`
stop, with the exact observed cleanup error, root SIGINT/reaping, terminal
interrupted main phase, no pause, and a separately pinned coordinator ownership
review. That review binds the failure, phase, interrupted-tail description,
exact next-command value, successful-prefix count, and the sole failed command.
Unresolved process publication, another error, a second failed command, an
unknown current owned process, or a changed evidence hash fails admission.

The excluded command must be the final terminal `rows` request for the active
revision, exact page index, cursor, and frozen page limit. It must have the
preserved exit -9 and KeyboardInterrupt receipt. A successful earlier command
and both process-publication records must show the same numeric PID in separate
lifetimes, with the earlier command finished before the later launch. The old
observed parent chain and wall-clock birth must match that earlier lifetime.
Failed stdout/stderr remain exact hash-bound byte artifacts. Failed output never
establishes rows, identities, counts, or cursor, even if it contains valid JSON.
Only all successful preceding pages establish the resumed cursor.

The bounded source chain is exactly v5 → v4 → v2, admitted from the pinned v5
configuration and protocol2 receipt, then v4's pinned protocol1 receipt. New
qualified sequence names are `adopted/v5/NNNNNNNNN`, `adopted/v4/NNNNNNNNN`, and
`adopted/v2/NNNNNNNNN`; old protocol2 names resolve to their actual predecessor,
never by integer coercion. Each ancestor binding, plan state, receipt, typed
reconciliation, and command index remains pinned. Root locks are acquired in
this same bounded order. No ancestor plan is opened or upgraded.

The new generation exclusively copies the quiescent schema2 plan and all present
admitted companions. Source and copied bytes must agree; nonempty WAL/journal
requires a separately reviewed recovery and cannot be silently ignored by an
immutable SQLite read. One read-only typed scan of the owned copy reconciles all
13 physical tables, every capture and retained table count, all completed
outcomes and row/source-ID hashes, plus the active successful prefix. Original
capture manifests remain referenced and checked. The pending marker is durable
before verification; interrupted/failed copies remain visible and cannot retry
or publish a ready adoption receipt.

The first new main attempt requires a separately bound successful adoption
review and an empty new command namespace. It must run fresh complete inventory
admission. The failed v5 tail stays excluded from adopted successes; its same
logical page key can execute once as a new command only in the newly admitted
generation. No failed command receipt is rewritten as successful, and no paused
predecessor is fabricated. Existing sampling thresholds, source guards, page and
copy limits, 600-second cooperative scheduling, and 4800-second main emergency
stop are unchanged.

The source tests use small disposable schema2 fixtures, three origin namespaces,
real typed-copy reconciliation, invalid failure/ownership/lineage/cursor cases,
changed failed streams, nonempty companions, and failed partial generations.
They provide correctness evidence only; actual generation adoption and main
continuation remain separate reviewed execution gates.
