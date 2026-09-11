# Windows inspection correction

PR #11 CI run `34553741443` at `3d0b162` passed Linux, macOS and Python
contracts, then failed the Windows native suites. Commit `9da1b98` corrects
two Windows production behaviors and the active-writer fixture.

Canonical Windows paths retain an extended namespace such as `\\?\`. Replacing
their backslashes with slashes turned that prefix into an invalid SQLite URI
authority. The corrected Windows branch percent-encodes native backslashes
without changing the namespace. It retains explicit rejection of unpaired
surrogates. A Windows regression opens an actual extended-length Unicode path
containing `#` and `%`, queries it read-only, rejects a write, and compares bytes.

SQLite's Windows shared-memory purge may delete the SHM pathname after closing
its own handles. The deadman byte lock prevents truncation, but does not prevent
that deletion. Captured Windows source handles now allow read/write sharing and
deny delete sharing for their lifetime, matching SQLite's own shared-memory
handle policy. The existing test still requires unchanged SHM bytes both before
and after another SQLite connection closes. A separate test checks that deletion
and replacement are refused while the source is held and allowed after release.

Windows mandatory byte-range locks also prevented the fixture itself from
reading a live writer's SHM file. The fixture now compares the complete sibling
file metadata while the writer is held and verifies exact original tree bytes
after its rollback, with journal mode established before the baseline. Capture
must still fail with a lock diagnostic. Unix assertions remain unchanged.

The relevant source is the pinned `libsqlite3-sys 0.36.0` SQLite amalgamation:
`sqlite3ParseUri` handles authority before percent decoding; `winShmPurge`
closes and may delete; `winShmSystemLock` uses byte 128 for the deadman lock;
`winHandleOpen` shares read/write access without delete sharing. Independent
review checked those paths against the actual CI failures.

All production changes are conditional on Windows. The retained Mac main
executable and the corrected schema-3 private profile remain at their explicit
previous source/binary identities. No inspection receipt is relabeled as a new
build. Mac-focused checks cannot establish Windows qualification; successful
hosted Windows tests remain required before delivery.

At exact source `9da1b985`, the Mac regression gate passed 18 Lightroom library
tests, 22 integration tests, package formatting and strict all-target Clippy.
Each native suite has one intentional subprocess-worker ignore. All owned gate
processes were reaped before the existing main inspection resumed. The private
gate receipt SHA256 is
`7621e475f412dc41f87058a06b6cb596e2c138e5ef66ac510703513af7338716`;
the original failed hosted run remains retained.

## Complete capture protocol and path reconstruction

The second run `34555237833` passed the new actual Windows URI tests but retained
one library and three Lightroom integration failures. The abbreviated `gh run
view --log-failed` output ended at the large SHM assertion; the complete raw job
log was retrieved and reviewed before the next batch. Its SHA256 is
`488d37919719349fcd64cea21101b6ddd5b5b4e96f3146b9c583f82e1e11e674`.

The SHM bytes changed at offsets 96, 104 and 128, recording checkpoint progress.
That fixture held only the SHM locks. `sqlite3WalClose` can acquire the main-file
exclusive lock and enter a mode that bypasses SHM locking during checkpoint.
Production capture already holds the main shared range first. Test-only change
`9f9fb91` makes the fixture use that complete protocol and expands exact byte
checks to main, WAL and SHM before and after SQLite close. Its unprotected reset
control remains. The focused Mac test and formatting passed; Windows qualification
remains separate.

Change `d49e6c62` addresses the three integration failures. Adobe relative folder
components can use `/`, while Windows verbatim paths disable separator conversion.
Only the derived Windows inspection path normalizes separators; the original
locator, retained cells, namespace and foreign-path classification remain intact.
Tests cover drive, verbatim drive, UNC, verbatim UNC and foreign Unix paths, and
the actual sidecar test checks missing-original status and original path text.
The Unix-root fixture now explicitly expects foreign-path status on Windows.

The writer fixture queried cached directory-enumeration attributes while its
writer held the file open. It now reads metadata through a file handle, without
reading locked bytes. Its exact original-tree comparison after rollback remains.
Independent source review passed both changes. No frozen Mac executable or
private inspection receipt was replaced or relabeled.

The three affected integration cases, package formatting and strict all-target
Clippy passed at exact `d49e6c62`. The private gate receipt SHA256 is
`40bd7f2f39a5671bad2234ce144628a33639e2de42638863b62c660805e62f6d`.
All five child-process logs were checked and the gate owner reaped before the
existing main inspection resumed from command 7076. The prior focused lock-test
receipt SHA256 is
`11dbc7afeb11e47d93b69d82cbfd7afea937b740abf0ed109d4e4669b7b433a0`.
