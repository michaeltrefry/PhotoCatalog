# S9 page replay memory correction

A private main-only inspection attempt stopped prospectively at its approved
512 MiB Python sampled RSS threshold while replaying a saved page's canonical
content hash. The failed attempt remains failed; this change does not raise that
threshold, reclassify its ownership, change source evidence or authorize a retry.

The old loop retained its previous result, decoded page and final row while loading
the next page. Canonical hashing also constructed a complete JSON string, encoded
bytes and newline concatenation. An 8 MiB wire limit is not a decoded-object or
process-RSS bound. This identifies avoidable overlapping allocations, not proof of
an unbounded leak or an attribution of every observed RSS byte.

The corrected helper owns a single page and returns only scalar progress before
another page is loaded. Hashing uses the same sorted keys, compact separators,
ASCII escapes and final newline as the existing canonical serializer, feeding
encoder chunks directly into the digest. A large string chunk is encoded in at
most 64 KiB pieces. Python's encoder may still materialize an escaped string;
this is not a streaming JSON parser or a hard process-memory guarantee.

`read_json` retains its existing byte cap and bytes-input JSON decoding behavior,
including UTF-8/16/32 handling. Replacing it with a UTF-8-only text reader would
silently narrow accepted evidence. No changes to binary, schema, source IDs,
limits, phases, journal keys, output digest format, full replay or cursor checks
are included. The native binary remains pinned to its existing tested source.

Tests require the preceding page, last row and nested path metadata to be released
before the next load; exact old/new canonical bytes and hashes for nested/escaped,
floating-point and large typed-cell values; bounded hash byte chunks; failure on
invalid identity/order; and interruption followed by full replay with exact sparse
cursor and digest recovery. Existing runner/generation contracts remain required.

A fresh-process synthetic diagnostic may compare frozen and corrected code using
one identical predeclared bounded fixture, child deadline, sampled stop and output
cap. Report input bytes, process peak RSS and sampled observations separately.
Its diagnostic stop is not permission to exceed the real inspection limit, and
synthetic improvement alone cannot qualify actual replay. Any real continuation
must bind the corrective source and an explicitly reviewed evidence/control
transition, retaining the prior failed run, rather than editing frozen bindings or
fabricating a successful pause.
