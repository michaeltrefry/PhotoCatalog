# PhotoCatalog bundled SQLite identity patch

This directory contains the exact published `libsqlite3-sys` 0.36.0 crate,
including SQLite 3.51.1, plus one change to `sqlite3/sqlite3.c`.
`UPSTREAM.json` records the crates.io archive checksum and every original file
SHA-256. `photocatalog-identity.patch` is the complete upstream source delta.
The crate's MIT LICENSE, upstream metadata, SQLite public-domain notice in the
amalgamation/header, and the unmodified SQLCipher source/notices are retained.

The root and desktop Cargo manifests patch the same crates.io package to this
directory. The upstream build script compiles the single bundled amalgamation;
PhotoCatalog does not compile or link a second SQLite copy. Both lockfiles keep
version 0.36.0 and its dependency set; the registry checksum is retained here
because Cargo path packages do not carry a registry checksum in their lock entry.
Installed notice collection includes this crate's notices and patch provenance.

## Private ABI v1

Custom `sqlite3_file_control` opcode `0x50434301` is implemented only by the
bundled Unix VFS. SQLite reserves custom opcode values above 100 for applications.
The argument is a pointer to two contiguous, aligned `sqlite3_uint64` values,
in order: device and inode. On SQLITE_OK both values are initialized from one
`osFstat(pFile->h, ...)` call using SQLite's compiled internal `unixFile` type.
The operation never opens, duplicates, closes, unlocks, or returns a descriptor.
Null output returns SQLITE_MISUSE; fstat failure returns SQLITE_IOERR_FSTAT.
Other VFS implementations retain upstream behavior and may return SQLITE_NOTFOUND.
Rust requires SQLITE_OK before comparing the result with the retained File pin.
No Rust code casts SQLite's private structure or obtains a native fd.

The existing HAS_MOVED check remains a second pathname-drift check. Windows
keeps its supported WIN32_GET_HANDLE observation and existing identity comparison.
Alternate/system SQLite builds or unsupported VFS choices fail identity admission
on Unix; there is no path-only fallback. SQLCipher and loadable-extension builds
are not newly qualified by this patch.

## Deliberate boundary

This fixes exact opened-object observation (including a database B opened during
an A/B/A pathname substitution). It does not prevent pathname substitution or
make connection creation/cleanup safe around arbitrary same-process Source locks.
In particular, existing raw pin opens/closes and SQLite VFS failed-open paths
remain separate custody work. No stronger pre-query or GUI lock safety is claimed.

Focused Rust tests cover mismatched retained/opened objects even when HAS_MOVED
passes, A/B/A restoration, retained HAS_MOVED rejection, unsupported in-memory
storage, and repeated identity observations while an independent process is
denied a POSIX byte lock. Existing relink worker identity tests remain applicable.
