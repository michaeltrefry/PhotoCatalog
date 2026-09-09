# sc-22842 text repair — source checkpoint and corrective preflight

Status: source-only candidate; the new tests have not run. No scale qualification
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

The corrective coordinator must validate `text_work` coverage and internally
consistent counts in every child, in addition to all original result gates.
Source-compatible prepared datasets may be used only with separately recorded
pristine-copy provenance. No favorable case retries, omitted failing scales,
fixture changes or threshold adjustments are authorized by this document.
Direct global FTS still has internal posting work not represented fully by VM
steps; the full scale campaign must measure it honestly rather than infer its
cost from counters alone.
