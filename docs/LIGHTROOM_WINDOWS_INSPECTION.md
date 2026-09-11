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
