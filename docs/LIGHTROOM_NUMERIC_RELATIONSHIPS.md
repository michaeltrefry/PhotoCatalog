# Numeric relationship projection correction

A completed compatibility-candidate report listed a dangling develop-settings
reference for every retained image. Bounded primary-key queries of its immutable
private capture confirmed actual INTEGER target IDs equal to REAL source cache
IDs. The source column had no declared affinity. Comparing serialized `Cell`
variants treated those equal numbers as different keys. The original report and
its unknown source-schema warning remain evidence; no failed result is rewritten.

Inspection schema 3 uses one derived relationship-key function for local IDs,
foreign references, path lookup and hierarchy traversal. INTEGER IDs keep their
exact i64 values. Finite REAL values normalize to those IDs only when integral
and in `[-2^63, 2^63)`. The upper bound is exclusive because converting i64::MAX
to f64 rounds to 2^63. Integers are never converted to f64: 2^53+1 therefore does
not collide with the rounded REAL 2^53. Fractions, out-of-range values, nonfinite
bit patterns, TEXT and BLOB remain typed and are not coerced. Null and numeric
zero (including negative zero) are absent-reference/local-ID sentinels; textual
or binary zero is not.

Raw cells, physical row keys, source identity generation, global IDs, packets and
opaque metadata remain unchanged. Duplicate canonical local IDs do not merge
entities: all are reported, exactly-one metadata/path association still rejects
ambiguity, and hierarchy traversal stops with an explicit ambiguity issue rather
than choosing the first duplicate. Missing references and actual cycles remain
reported independently. Source schema `1000000` remains unknown under the existing
source-profile policy; this repair alone does not establish full source-version
support or Adobe develop/render equivalence.

Versions 1 and 2 of the **derived inspection plan** are rejected before write-open
with instructions to create a new plan from preserved captures. There is no
in-place migration or unbounded automatic rebuild. Existing plan main/WAL/SHM,
retained packet/path evidence and historical outputs remain untouched. A new
schema-3 plan ingests the already preserved captures and performs the corrected
projections; its own fresh output and execution require the usual admission.
The application catalog schema and source catalog files do not change.

Focused tests cover the observed no-affinity cache column, native SQLite numeric
equality at precision boundaries, typed raw preservation, genuine missing IDs,
zero sentinels, ambiguous mixed-type duplicates, hierarchy cycles, path and
metadata ownership, and byte-preserving rejection of old plans including live
WAL/SHM. Runtime results are recorded separately; this document defines behavior,
not an execution or final-inspection acceptance claim.
