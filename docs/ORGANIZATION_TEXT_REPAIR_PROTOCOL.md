# sc-22842 text repair — source checkpoint and corrective preflight

Status: source and corrective driver frozen for review; the new tests have not run. No scale qualification
or story completion is claimed. The immutable protocol-1 binary/source and failed
partial campaign remain under
`sc-22842-scale-v1-0vk7e9qu` in private results. At 1M assets, the reverse-filename
case's 100 warm pages had p95 5870.494938 ms (min 4794.837, max 6023.286); all 20
fresh-process pages also took seconds. The coordinator was intentionally stopped
as failed partial at 2026-09-09 20:39:40 UTC. Unrun cases/scales remain unproven.

The production correction removes repeated persistent-index MATCH from candidate
predicates. Sequence queries driven by FTS use their MATCH once. Other ordered
streams evaluate only admitted candidate text in a TEMP FTS5 index with identical
`unicode61`/quoted phrase-prefix semantics. No persistent schema, global indexes,
fixture formulas, sort/filter semantics, or acceptance thresholds change.

## Bounded preflight, after source review and lane authorization

1. Execute focused production tests for Unicode/diacritics, quoted literal terms,
   punctuation forming phrases, prefixes, every sort/direction, other scalar and
   membership predicates, sparse partial pages, true tails, byte-limit
   continuation and oversized-document retry. Require exact independent IDs.
2. Exercise read-only-main sessions while another connection holds IMMEDIATE;
   require main/WAL byte invariance during searches, stable old snapshot results
   across a committed text change, new-session visibility and stale serialized
   cursor rejection. These tests use disposable small catalogs only.
3. Compare small 64/640-row catalogs with identical queried candidates and growing
   unrelated global postings. Record actual candidate counts, local indexed
   bytes/batches, VM steps and sorts. This checks staging bounds, not million-row
   latency. Query plans must remove the correlated persistent MATCH path.
4. Run the existing full 17-case 1k probe and three-phase coordinator smoke, all
   tests and lint against the exact new source. Child pages additionally retain
   `text_work` counters; raw errors and failed test/smoke attempts remain retained.
   The frozen v1 binary and receipts must not be overwritten or reused as results
   for this candidate.

## Final corrective campaign — separate freeze still required

After those functional gates, freeze the exact source, release build recipe,
probe, coordinator and admission settings for independent review before timing.
The existing 17 cases, all 1/5/10M scales, 3 warmups/100 measured warm/20 fresh
pages, exact full-row/tail oracles, 200-operation transitions, immutable-source
proofs, host observation and overlap accounting remain required. Use the same
256 MiB production engine settings and default 1 MiB/document, 8 MiB/request text
limits. Record every continuation and all local staging work inside each complete
visible-page latency. Preserve the fixed p95 <100 ms warm, <1 second first-page,
<=4 GiB browse RSS budgets. An admission-limited partial page is never a completed
200-row result or successful exhaustion.

The corrective coordinator validates `text_work` coverage and internally
consistent counts in every child, in addition to all original result gates.
Source-compatible prepared datasets may be used only with separately recorded
pristine-copy provenance. No favorable case retries, omitted failing scales,
fixture changes or threshold adjustments are authorized by this document.
Direct global FTS still has internal posting work not represented fully by VM
steps; the full scale campaign must measure it honestly rather than infer its
cost from counters alone.

## Corrective driver protocol 2 (frozen before execution)

`benchmarks/organization_campaign.py` now identifies its manifests/results with
`driver_protocol: 2`. Native receipts and synthetic rows retain fixture protocol
1; their data and formulas are unchanged. Query children declare the default
`text_limits`. Every chunk must report nonnegative integral text counters, exact
candidate reads, zero sorts, bounded indexed rows/bytes/batches, and TEMP work
included in total VM steps. These tiny fixture documents cannot hit admission;
an unexpected admission-limited chunk fails validation. Nonlocal queries must
report zero local indexing work; local query plans must have no persistent FTS
scan, and direct FTS must have exactly one. The sum of chunk times must fit inside
the full visible-page latency. Negative contract tests exercise omitted fields,
impossible byte/row/batch counts, hidden timing, wrong limits and unexpected FTS.

Before the full measurement phase, run `--phase diagnostic`: at 1M and 10M only,
run `filename-reverse` then `text`, each with 3 warmups and 5 measured samples at
iterations 0–4. There are four children, each with a fixed 120-second deadline.
All four are attempted even if one fails. Each receipt retains actual counters,
full row oracles, raw times, process RSS, UTC/monotonic observer intervals and
errors; source hashes bracket each scale. This diagnostic is explicitly
`acceptance_evidence: false`. It is designed to expose remaining fundamental cost
before another long campaign, not estimate qualifying p95 tails. The parent must
inspect the diagnostic before launching the full measure phase. Missing or failed
diagnostics prevent nonsmoke measurement. The smoke diagnostic uses 1k only;
smoke measurement itself does not require the diagnostic.

### Preparation and source reuse

`--phase prepare --reuse-prepared /absolute/old/campaign/manifest.json` is optional.
It requires a complete protocol-1 manifest covering the exact unchanged scales,
plus the original `prepare-N.json` receipts. The new root must not exist and must
be disjoint from both the old evidence directory and every old catalog. Before
copying, validate the SQLite header's schema 4 and PhotoCatalog application ID,
main size/hash, original preparation receipt hash/counts/engine/settings and
companion state. WAL/journal must be empty; SHM may remain, with its bounded bytes,
hash and file identity retained. No SQLite connection is opened on old sources.

Each main is copied to an exclusive new file and fsynced. Source and copy hashes
must match the frozen manifest, with unchanged main/companion identity, size and
timestamps across copying. Original preparation receipts are copied byte-for-byte;
a separate `reuse-N.json` proof and new manifest bind the source, copy and old
manifest digest. Failures preserve partial outputs and an incomplete manifest.
No old main, sidecar, receipt or permission is modified or deleted. The old v1
1M empty WAL/32 KiB SHM therefore remain intact. Native startup independently
checks the copied fixture protocol/count/high-water/schema before every query.
Subsequent diagnostics, measurements and transition copies operate only on the
new root; every phase verifies the frozen driver and build receipt identities.

### Build and commands

After the exact clean commit is reviewed, use the same pinned SDK and production
Rust dependencies, `CARGO_BUILD_JOBS=4 OMP_NUM_THREADS=1`, and
`cargo build --locked --release --bin organization_probe`. Record the complete
command/environment, clean source/tree identities, binary/lockfile/driver/source
hashes, SDK source verification, build log and UTC times in the build reference.
Copy the tested binary to the new private campaign's outer directory. A debug
build is permitted only for `--smoke` and never qualifies the scale campaign.

Use the existing pinned benchmark Python environment. With identical `--binary`,
`--root` and `--build-reference` arguments, run phases serially: `prepare`
(optionally `--reuse-prepared`), `diagnostic`, then after diagnostic review,
`measure`, and finally `transitions`. The full measurement and transition loops,
thresholds, cohort formulas and sample counts remain those of protocol 1.
Record the host observer throughout preparation and measurement. The original
failed v1 run and all new failed attempts remain separate permanent evidence.
