# Current handoff — 2026-09-11

Latest verified checkpoint: FULL attempt 19 (`bfb69265-096e-40d5-8925-d4b01262d592`) paused cleanly at command 10340 after crossing the saved replay boundary. Actual session 38645 exited and was reaped. Commands 10239–10339 completed: one discovery, 74 resumes, one report and 25 row pages; no new command failures. Result SHA256 `6d03bc509f5e71797bae926b55be0db6ffacb86b1896d219ef906e46d01977de`; audit PASS `858c0f3ac457e360667e8ea9b899fb4ba220556c7959fa5ca40cdaec305367df` under `sc-22844-raid-full-nineteenth-slice-review-v2`. Sampled Python peak was 262,537,216 bytes and native peak 377,978,880 bytes; these are sampled observations, not high-water marks. The unchanged base binary runs with reviewed controller `0ac64b39…` and JSON profile `5ccb6aea…`. Attempt 20 (`a5320f72-5630-46f5-8b38-b2413a9fdb40`, session 10415) started with the same profile after local lock-fix validation released the native slot; consult current control for execution state. This checkpoint is not FULL completion or S9 acceptance.

Public head `58e7a1a` passed benchmark contracts, macOS and Windows CI in run `34623419551`; Linux failed the export owner-drop/reopen test with EWOULDBLOCK. Bug sc-23102 blocks sc-22844 and tracks explicit export-service lock release plus deterministic retained-descriptor regressions. Local native validation passed during the clean inspection pause: three lifecycle regressions, five export-service integrations, strict all-target Clippy and formatting. Independent source review PASS `f7d2b35e…`; gate receipt `42781976…`. PR #11 remains draft and unmerged; exact new-head CI and merged-state acceptance remain required.

Historical attempt 18 (`2bacecf2-06d6-4341-bb56-4f6a83c42a91`) exceeded the 512 MiB sampled Python limit at 539,197,440 bytes during saved-row replay. Session 91297 was reaped, with only discovery 10238 added. Result `2854b115c33d7765c0460ed89c33c0e8aae8e436368f8270eba8b1aa25afdddd` remains failed. The earlier diagnostic substituted file-sized reads for production cap-sized buffered reads, so its memory result did not establish production allocation behavior. The fixed-chunk reader passed 129 Python tests and independent review. Corrected diagnostic v3 replayed 10,221 records without native dispatch in 197.2 seconds at 253,100,032 bytes peak RSS; session 4846 and child 33755 were reaped. Independent recovery review `a11f1ab8…` preserved both failed intervals before attempt 19 admission. Older snapshots below are historical.

Historical stop after seventeenth attempt
`e1ddf030-f29f-440c-84d1-dd0421b079a3` exceeded the 512 MiB Python sampled RSS
limit during saved-page JSON replay (537,378,816 bytes observed). Actual session
`88833` exited and was reaped; cleanup recorded no remaining observed processes.
Its result remains `failed_or_unknown`, SHA256
`2c71d45e486bfbd3472897565d93e9c9b87a4b702bc606cb4275b2697e2990d9`.
Journal next command is 10238; this attempt completed only fresh discovery 10237.
Do not resume from an older clean checkpoint or relabel this failure. The reader
now releases encoded bytes before constructing JSON objects while preserving
encoding and error behavior. One read-only diagnostic reached the first unrecorded
command after 10,221 saved records in 203 seconds, at 343,457,792 bytes peak RSS
under the unchanged 512 MiB limit. It executed no native commands and verified
the failed evidence unchanged. Session 9436 and its owned child were reaped.
Evidence is under `sc-22844-readonly-replay-diagnostic-v2/execution-20260911-root-01`.
Actual recovery still requires its independent incident review and separate grant;
the diagnostic does not establish production or S9 acceptance. The clean
checkpoint and earlier active snapshot below are historical; check current control.

The user resumed and authorized dedicated RAID scratch. S8 is verified Done.
Historical delivery head `70f5284` passed all four jobs in CI `34604737271`
(receipt `sc-22844-canonical-runtime-profile-v1/ci-70f5284-terminal.json`).
PR #11 remains draft, not merged. Legacy MAIN completed 48 outcomes / 19,872,102 source
rows / 5,740 table descriptors. Its known schema-2 relationship defect means
those derived links are not corrected S9 acceptance.

Corrected schema-3 FULL has 47 independently verified full captures. Native
progress at command 8857 showed 12 successful adds, 11 reports and 7,297,390
retained rows, with no observed native failures or stage warnings. This does not
prove completed Python inspections; the driver can catch errors until final FULL.
All-48 FULL outcomes remain unqualified. Second slice
`c6bf42d7-cea0-4ed1-baf5-415cfe0e7c12` paused cleanly before command 655 after
599 successful new commands; result SHA256
`df98edc1966c18aab0c071ead6e30ac7dbf0208fd8d7c41108d3823dbab98529`,
independent review SHA256
`d268a04b3567081bb0c0a8f4835f94c779e9a3e94bc0e8e3d467e12d855d2fb0`.

Fifteenth FULL attempt `bd23129e-1876-442c-b4a8-a0b6576b935e`, session `91609`,
paused cleanly and was reaped at next command 9528 after 711 successful new
commands from 8817 and no failures. Result SHA256
`26d7c7313f0abe76653ba85a765bbdf0771ed973d075263f68906df9a74e607f`;
PASS review `sc-22844-raid-full-fifteenth-slice-review-v1/paused-review.json`, SHA256
`4784b3b3e11f78fb5ee39d762e1686f0b66aa905116f8f92ed96aaba0a810b85`.
This is a clean checkpoint, not FULL completion.

Sixteenth slice `d74d191c-661d-481b-9f35-11a2897df6fa`, session `51299`,
paused cleanly and was reaped at next command 10237 after 709 successful commands.
Its result SHA256 is
`349efeeb58b1e528c8059fea8d2675260f9f88da0c369256c5798604d1d02cb6`;
independent PASS review SHA256 is
`5cae56a9af819b0e7f4842121b8eb87b8750836e1eacda5c466cd9faeb71e3ae`.
Its recipe SHA256 is
`e32d27c2fe995490044795e23ad235256ffbe3e44424f7855d79af30deb65844`.
This snapshot can become stale while execution continues. Before resuming or
starting competing native work, read the actual `current.json` in
`sc-22844-raid-full-control-v1` and that attempt's result and terminal tool receipt;
do not treat this document as current process ownership proof.

- Scratch run: `/Volumes/MichaelJon/PhotoCatalog-Scratch/sc-22844-schema3-20260911/inspection/schema3-run`.
- SQLite temp: `/Volumes/MichaelJon/PhotoCatalog-Scratch/sc-22844-schema3-20260911/sqlite-temp`;
  both `SQLITE_TMPDIR` and `TMPDIR` are explicitly bound to this directory.
- Local control: `/Users/michael/PhotoCatalog-private-results/sc-22844-raid-full-control-v1`.
- Failed current recipe: `sc-22844-canonical-continuations/e1ddf030-f29f-440c-84d1-dd0421b079a3-final/recipe.json`
  under private results. Native source `ccae8ad` and base driver `af2c5e1d…`
  remain unchanged; controller `28c84319…` installs reviewed profile `243ccfff…`.
  This changes effective Python execution; complete hashes and gates are in the ledger.
- After a verified clean pause, use its actual result/PASS review and unchanged
  funding/limits to prepare a fresh UUID continuation. The private
  `sc-22844-canonical-continuation-preparer-source-v2` requires external reviews
  and a grant; it never grants, launches or removes a pause. Keep the same profile
  on continuation and use the reviewed profile-aware checkpoint auditor.
  Root owns review/grant/launch; failed or unknown attempts cannot silently retry.

The recovery controller (`518a7f1d…`) and updated pause/terminal auditors
(`b296d6c3…` / `7793a33c…`) passed independent source review. They retain the
failed interval in history. Public source includes reusable protocol 2 helper
binding; the private incident bridge is not a generic public retry route.
Actual terminal qualification remains pending. Paths and packets use
the existing phase controller after successful FULL and separate funding review.

All-48 corrected FULL completion, paths, packets, family decisions, PR merge and
terminal merged-state acceptance remain outstanding. Original catalogs/photos
are read-only; only dedicated scratch is writable. Preserve frozen evidence and
the historical handoff below. See [execution ledger](EXECUTION_LEDGER.md) for the
exact resource bounds and proof paths.

# Historical resumption after the user pause

The user explicitly resumed on 2026-09-10 local time. The external RAID is connected.
The prior result, all 546 command receipts, journal, phase, admission and owned
pause were verified before admitting attempt
`bb6a2a22-bab7-4d0f-8bf2-be0554459512` from command 547. The unchanged v3 wrapper
captures the owned pause; the prior pause digest below is historical, not the
current run state. PR #10 passed all four CI jobs and merged as `6ff4487`.
See the current [execution ledger](EXECUTION_LEDGER.md) and live Shortcut for
subsequent progress. The original handoff is retained below as pause evidence.

# User-requested pause — 2026-09-10

The user needs to disconnect the external RAID. PhotoCatalog inspection is stopped;
all observed owned processes were reaped, and no local restart is scheduled.
Do not remove the pause or resume external-drive work until the user resumes
and the drive is available again. Original catalogs and photos are unchanged.

- S8: PR #9 merged as `f6d19dc` after passing PR CI. Main CI exposed a separate
  worker-lock lifetime defect; correction PR #10 is open at
  `922563f3d461b00b45e7a8c411ea1c490381bf93`. Its local tests and independent
  review pass; hosted CI `34538492535` was still running at pause. Refresh its
  actual result and exact head before merge. S8 remains In Review.
- S9: integration source `b228bd7` is locally validated and independently
  reviewed. It includes S8 PR #9, but not the unmerged PR #10 correction.
  Actual inspection continues to use its unchanged archived v6 executable.
- Actual v6 main attempt `ef1fb6b0-1eb1-4871-b9dc-46e63dd8c909` paused cleanly
  before command 547 after 546 successful commands. No new command failed.
  Outer session 3037 exited zero; inner exit one is the expected pause signal.
  Result status is `paused_at_command_boundary`, with
  `ownership_status=observed_owned_processes_reaped`.
- Private run: `sc-22844-current-families-v6` under the private-results root.
  Wrapper/control package: `sc-22844-after-s8-main-request-v3` under the same
  private-results root. Keep this wrapper and control location for continuation.

The attempt's `control/attempts/ef1fb6b0-1eb1-4871-b9dc-46e63dd8c909/result.json`
has SHA256 `50036982e923ae78a5d561b1d1cbf435e956756e746b40acf5c0ff10cfba6952`.
Journal SHA256 is `e1f1b0dbd8c1d9430f58059a4aee7ab2164fa2dacf152ed0b025293215e2bee4`;
owned pause SHA256 is `260913ff3ce58cbca3fc90672b9599f6f1401f977813bfe379bfab42bd26e4a9`.
The last slice's sampled group RSS peaked at 430,473,216 bytes.

On resumption, use a new attempt UUID and authorization. Bind `previous` to the
paused result; derive the journal, next-command and admission references from it.
Bind the existing pause's owner, digest and complete stat identity, with explicit
owned-pause removal. Change only the funding policy's attempt path and dependent
hash; keep all source/native/config/adoption and resource limits unchanged.
Fresh drive, checkpoint, process ownership and free-space checks remain required.
No new adoption is needed for an ordinary clean continuation.

S9 depends on completed S4 and may run alongside hosted S8 CI after local native
work releases the lane. This scheduling decision does not relax S8's merge/CI
completion gates. Shortcut comments 23044–23047 retain the decision and pause.
Remaining inspection, full companions, path/XMP evidence, family review, migration,
backup, Tauri UI and terminal readiness requirements are still open.
