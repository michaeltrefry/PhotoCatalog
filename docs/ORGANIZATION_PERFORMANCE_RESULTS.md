# Organization and search performance — sc-22842

The final v4 campaign passes the fixed query and concurrent-save budgets on the
reference Mac at 1, 5, and 10 million synthetic assets. The independent parent
reconciliation reports no errors. This is local performance qualification;
final three-platform CI, PR review/merge and tracker closeout remain pending.
The earlier failed campaigns, including the rejected v3 mixed result, remain
preserved and are summarized below.

## Configuration and evidence boundary

The reference system is an Apple M5 Max (Mac17,6), 18 logical CPUs, 128 GiB RAM,
macOS 26.6.2 (25G83), with catalogs on its internal APFS SSD. The APFS container
capacity is 3,996,276,899,840 bytes. The production bundled SQLite 3.51.1 path
uses a 256 MiB cache per connection, mmap disabled, WAL, FULL synchronization,
foreign keys enabled and file-backed temporary storage. Controlled native work
ran serially, with phase-bound host observation; this was an ordinary desktop
session, not a cleared-cache or bare-machine test.

The [frozen query protocol](ORGANIZATION_QUERY_PROTOCOL.md) and
[corrective protocol](ORGANIZATION_TEXT_REPAIR_PROTOCOL.md) define the same
17 query cases and fixture formulas throughout. These are synthetic normalized
catalogs, not millions of imported photographs. Every query uses the production
Rust API. An independent arithmetic oracle checks complete result rows, cursor
anchors, exact tails, revisions and provenance outside the timed region. Page
time includes all continuation chunks, decoding, candidate text staging and TEMP
FTS work. No successful empty partial page substitutes for 200 available results.

Each case/scale has 3 warmups, 100 measured warm pages and 20 fresh-process pages:
51 groups, 1,071 query children, 6,120 measured pages and 153 warmups. Eight
separately declared diagnostics (40 measured pages and 24 warmups) passed before
qualification; none contributes samples to the final distributions. No failed
run was overwritten, and no measured result was reused or selectively retried.

The gates are full-page p95 <=100 ms warm, <=500 ms fresh-process and browse
process RSS <=4 GiB. Each fresh child's whole-process wall time must also be
<1 second, an additional startup guard. It is not a replacement for the 500 ms
query budget or evidence for the thumbnail/UI first-visible-page target.
Quantiles use linear interpolation over every sample. Fresh processes retain
OS filesystem caches; these measurements do not establish cold-storage latency.

## All 17 query cases

Each cell is **warm / fresh-process native page p95**, in milliseconds. Exact
p50/p95/p99/max, sample counts, fresh-child wall distributions, resource maxima,
work counters and returned-count histograms are in the portable
[distribution table](ORGANIZATION_PERFORMANCE_DISTRIBUTIONS.csv). Work/RSS maxima
and row histograms in that table cover the combined 120 measured pages per
case/scale and are repeated for each latency population.

| Case | 1M | 5M | 10M |
| --- | ---: | ---: | ---: |
| browse | 0.143 / 0.202 | 0.142 / 0.194 | 0.141 / 0.202 |
| rating | 0.217 / 0.328 | 0.207 / 0.274 | 0.210 / 0.308 |
| rating-sort | 0.217 / 0.299 | 0.232 / 0.296 | 0.236 / 0.548 |
| capture-rating | 0.238 / 0.492 | 0.261 / 0.507 | 0.449 / 0.551 |
| filename-reverse | 4.278 / 5.208 | 4.575 / 4.922 | 5.245 / 4.961 |
| keyword | 0.309 / 0.411 | 0.321 / 0.407 | 0.323 / 0.689 |
| wide-keyword | 0.189 / 0.342 | 0.273 / 1.196 | 0.659 / 1.165 |
| collection | 0.315 / 0.367 | 0.302 / 0.387 | 0.296 / 0.384 |
| mixed | 10.274 / 12.883 | 12.451 / 13.537 | 12.615 / 13.188 |
| text | 6.007 / 6.268 | 28.950 / 31.073 | 60.295 / 61.871 |
| text-capture | 1.846 / 3.565 | 1.898 / 3.401 | 2.164 / 3.326 |
| camera-lens | 0.460 / 0.548 | 0.470 / 0.541 | 0.459 / 0.548 |
| date-camera | 1.055 / 1.115 | 1.006 / 1.144 | 0.962 / 1.062 |
| label-flag | 0.278 / 0.344 | 0.277 / 0.347 | 0.281 / 0.346 |
| folder | 0.202 / 0.271 | 0.199 / 0.268 | 0.196 / 0.259 |
| folder-recursive | 0.200 / 0.256 | 0.192 / 0.258 | 0.191 / 0.272 |
| conflicted | 3.875 / 4.139 | 3.913 / 4.356 | 3.897 / 4.197 |

The high-cardinality keyword case deliberately reaches short tails: 9–50 rows at
1M, 47–200 at 5M and 95–200 at 10M. The text/capture/lens case alternates a full
200-row result with a genuinely empty exhausted tail (60 of each per scale).
These counts are independently derived from the frozen data formulas; they do
not prove 200-row delivery where no such rows exist. All other cases return 200.

| Scale | Largest warm sample (ms) | Largest fresh page (ms) | Largest fresh child (ms) | Peak browse RSS (MiB) |
| --- | ---: | ---: | ---: | ---: |
| 1M | 10.628 | 13.521 | 89.178 | 284.016 |
| 5M | 29.277 | 31.613 | 92.269 | 382.969 |
| 10M | 60.997 | 62.653 | 133.644 | 480.781 |

Every recorded query sort counter is zero. Reverse filename/text consumes at most
1,000 candidates and 13,400 staged text bytes at each scale; text/capture consumes
at most 999 candidates and 13,386 bytes. Date/camera consumes at most 600 candidates.
The sparse mixed case needs up to three chunks and approximately 12,000 candidates;
the full-page timer includes all three. Candidate limits are per request, not
an assertion that every completed page costs only 200 inspected rows.

Global sequence-driven text still grows from 6.007 to 28.950 to 60.295 ms warm
p95 as catalog size increases, despite a maximum 5,418 VM steps at every scale.
FTS posting traversal is not fully represented by SQLite VM counters. The measured
workloads pass; this is not a constant-work claim for all text searches or proof
that arbitrary sparse combinations will meet the same latency at larger sizes.

## Saves and snapshots during actual source updates

Each scale uses a separate verified disposable catalog copy. It adds a 200-asset
cohort, then performs 100 rating and 100 label saves on one half while a second
Catalog connection retains 200 parsed source observations on the other half.
The foreground also reads 200 snapshot pages (40,000 exact rows). All source/model
revisions and projections are checked, and 100 saved assets per scale are checked
after reopening. Across scales this is 600 saves, 600 source updates, 600 pages,
120,000 snapshot rows and 300 reopened assets. Original synthetic query rows and
source input files remain unchanged.

Operation intervals establish overlap with the background activity interval;
they do not imply simultaneous execution inside SQLite's single writer. All
200 saves at every scale overlap background activity, as do 199 of 200 snapshot
pages. Both full-population and overlapping-population p95 must be <=100 ms;
zero overlap fails. No sleeps or benchmark retiming were added to obtain overlap.

| Scale | Overlapping saves | Save p50 | Save p95 | Save p99 | Save max | Overlapping pages | Page p95 | Page max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1M | 200/200 | 9.690 | 12.788 | 13.535 | 14.094 | 199/200 | 0.525 | 0.632 |
| 5M | 200/200 | 9.697 | 12.161 | 13.009 | 13.119 | 199/200 | 0.453 | 0.586 |
| 10M | 200/200 | 9.041 | 11.543 | 12.507 | 13.462 | 199/200 | 0.465 | 0.582 |

Latency values are milliseconds. Full rating, label, source-update and snapshot
p50/p95/p99/max distributions are retained in the CSV. The one nonoverlapping
snapshot page per scale took 0.281/0.204/0.379 ms respectively; there were no
nonoverlapping saves.

Source-update latency and mixed-process RSS are diagnostic. Mixed RSS was
402.359/350.812/353.250 MiB at 1M/5M/10M; the 4 GiB gate applies to the separate
browse processes. This workload exercises actual metadata retention, indexed
projection refresh and organization writes, with parsed packets prepared before
retention timing. It does not measure RAW decode or complete image-import throughput.

The production [writer admission queue](CATALOG_WRITER_ADMISSION.md) gives pending
foreground transactions priority over background work, with FIFO ordering within
each class. It preserves SQLite authority, transaction rollback and revision CAS.
It cannot preempt a running transaction, guarantee background progress under an
infinite foreground stream, or provide fairness across separate CLI processes.

## Failed evidence and corrective changes

| Campaign | Retained outcome | Correction before a separate frozen run |
| --- | --- | --- |
| v1, source `c24d256` | Failed partial campaign: 1M filename/text warm p95 5,870.495 ms; all 100 warm and 20 fresh outcomes retained. No all-scale claim. | Replace repeated global MATCH setup on sorted candidates with bounded candidate-local FTS, preserving native tokenizer/phrase-prefix semantics and charging staging work to page latency. |
| v2, source `7b8a87f` | Full query execution failed: text/capture warm p95 81.860/1,021.116/2,859.928 ms at 1M/5M/10M; date/camera 10M 176.417 ms. A separate blanket row-count check rejected valid empty tails. | Add schema 5 lens/capture/sequence index and disjoint tie/later keyset streams; validate exact formula-derived tails. Restore the omitted native fresh-page <=500 ms gate, which whole-child <1 second had not established. |
| v3, source `9713f3a` | Query gates passed. The original full-population mixed summary was rejected: only 19/13/17 saves overlapped background work, with p95 292.180/413.699/280.726 ms. The original false-green audit and superseding rejection both remain retained. | Shared in-process foreground/background writer admission; apply the unchanged 100 ms target to actual overlapping populations as well as complete populations, with zero overlap rejected. |
| v4, source `0cfc1fd` | Complete query and transition audit PASS with no errors. | Current measured candidate; final delivery CI/review remains separate. |

The CLI import dispatch was also separated from the large command match after a
Windows debug import stack overflow on the earlier PR head. Both old and new
bounded 1 MiB child tests passed locally on macOS, so they did not reproduce the
Windows failure. The Windows cause/fix remains provisional until the final
Windows CI executes the actual import regressions; local success is not that proof.

## Identity, preservation and reproducibility

The release build used Rust/Cargo 1.98.0, `cargo build --locked --release --bin
organization_probe`, four build jobs and `OMP_NUM_THREADS=1`. The source was clean.
The pinned SDK archive and 159 source files matched before and after the build.

| Artifact | Identity |
| --- | --- |
| Source | `0cfc1fd8d94144c60a831026c8ab42929100f008` |
| Source tree | `092eef4f66cfc77ab054fc6ab6f6a3e5ebba1339` |
| Release binary SHA-256 | `52083464ebdc01e36aadf936714772c64d5069c6ffb8166b28bf5935c7d6d933` |
| Coordinator SHA-256 | `35bb8667f9df0a095bb2801b17cd86a6262b513e1382589c50004ed8d8b01575` |
| Lockfile SHA-256 | `d67bc1f7b57956e9f2ce433dc7563acb9bb827fbd116692d34d1c964abc6c6d4` |
| Complete parent audit SHA-256 | `59e955a1c4bf92f561ea365bb2e41acb14764306fb4d83336c43eed73faf43fe` |
| Independent arithmetic auditor SHA-256 | `c1a3747eba6da8ed796f4447083a480afc2dfa0f169383dfaf35c9234bda5bc2` |

Private evidence is retained under campaign basename
`sc-22842-scale-v4-xafzqdq4`: source archive, build reference, SDK verification,
phase receipts/host traces, raw native/child outputs, and `parent-complete-audit.json`.
The public report and CSV contain synthetic aggregates only. The auditor recomputes
rows, distributions, coverage, source-proof identities and gates from retained
JSON; it does not reread huge databases. The parent separately verified build
identity and source proofs. The auditor author also implemented S7; independent
production review and final reconciliation were performed by the parent.

Preparation retained byte-exact v3 schema 4→5 migration receipts and their native
proofs, including typed logical identities for all 44 tables (FTS shadows included).
The v4 copies then passed a separately labeled schema 5→5 verification with
unchanged physical hashes, table counts, logical identity and exact index SQL.
No earlier fixture was mutated, no migration was silently relabeled, and no
query samples were reused. Queries and transitions retain separate copy lineage.

| Scale | Unchanged v4 pristine main SHA-256 |
| --- | --- |
| 1M | `8447e49cba504c56b45e202e181ca96f1dbaa345b3dc4d8e10f5a244384022a0` |
| 5M | `dd51f244906fa8414b5caad94cf88f3742d5d5feac401dbec703483242457bb4` |
| 10M | `0ac57eefbfbfeee46e83c17b8116c48ae3cb0dedae01605c1016981a536e91db` |

UTC phase windows on 2026-09-09: preparation 22:44:10–22:47:13, diagnostics
22:48:27–22:48:39, full queries 22:49:42–22:54:11, transitions 22:54:39–22:55:35.
All four phases and their owned host observers exited successfully.

## Story-local readiness

| Acceptance surface | Evidence | Remaining delivery gate |
| --- | --- | --- |
| Durable folders, keywords/hierarchy, ratings, flags, labels and collections through restart/relink | Production API/CLI and focused persistence, source-conflict, relink and batch regressions; exact reopened transition checks | Final three-platform integration CI and review |
| Combined filters with stable sorting, cursor expiry/staleness and concurrent snapshots | All 17 cases at all three scales, full-row arithmetic oracle, native counters, mixed snapshots and source/projection revisions | Final CI; no desktop/UI claim |
| Atomic per-asset changes and explicitly resumable batches within interaction budgets | Atomic rollback/revision/failure/cancellation/resume tests; actual-library admission FIFO/no-barging/unwind tests; all 600 overlapped saves pass | Final CI and merged-head verification |

The exact runtime source passed 183 Rust tests (one separately ignored case),
52 Python contracts, package formatting and all-target Clippy. Correctness and
performance runs are distinct evidence. This report does not mark sc-22842 Done;
PR #7, final CI/merge and Shortcut read-back are still required. Preview delivery,
full image editing, Lightroom migration and the desktop interface retain their
own acceptance stories. Confidence is high for the recorded Mac workload and
its bounded API contracts; portability remains subject to actual final CI.
