# Current handoff — 2026-09-11

The user resumed and authorized dedicated RAID scratch. S8 is verified Done.
Earlier published head `e7ca0e4` passed all four jobs in CI `34590817567`; check
PR #11 for current-head CI. It remains draft, not merged. Legacy MAIN completed 48 outcomes / 19,872,102 source
rows / 5,740 table descriptors. Its known schema-2 relationship defect means
those derived links are not corrected S9 acceptance.

Corrected schema-3 FULL has 47 independently verified full captures. Seven full
rebuild/readback sequences were complete at last observation; all-48 FULL
outcomes remain incomplete. Second slice
`c6bf42d7-cea0-4ed1-baf5-415cfe0e7c12` paused cleanly before command 655 after
599 successful new commands; result SHA256
`df98edc1966c18aab0c071ead6e30ac7dbf0208fd8d7c41108d3823dbab98529`,
independent review SHA256
`d268a04b3567081bb0c0a8f4835f94c779e9a3e94bc0e8e3d467e12d855d2fb0`.

Twelfth FULL attempt `8df05a37-4c2d-4f44-aaaf-262426ea0082`, session `86615`,
paused cleanly and was reaped at next command 7240 after 725 successful new
commands and no failures. Result SHA256
`0e22ff05e06005ff3059b8d0c2a53a70c022a9c2bb26d02307ce0751678dc451`;
PASS review `sc-22844-raid-full-twelfth-slice-review-v1/paused-review.json`, SHA256
`7eec2df2e80118afd7b5ae6ebec4f2198868135a91fabc036cb99317bd736e66`.
This is a clean checkpoint, not FULL completion.

Thirteenth slice `a588ba5b-8084-4123-be1e-569ebf90d219`, session `84418`,
is active from command 7240 with the explicitly reviewed canonical hash profile.
Its recipe SHA256 is
`07e6e4db9aee08885349750099dd2e8db81f7a72ebc0a8b68680ee5ee4104f23`.
The current control pointer and actual terminal receipt must be checked before
resuming or starting competing native work.

- Scratch run: `/Volumes/MichaelJon/PhotoCatalog-Scratch/sc-22844-schema3-20260911/inspection/schema3-run`.
- SQLite temp: `/Volumes/MichaelJon/PhotoCatalog-Scratch/sc-22844-schema3-20260911/sqlite-temp`;
  both `SQLITE_TMPDIR` and `TMPDIR` are explicitly bound to this directory.
- Local control: `/Users/michael/PhotoCatalog-private-results/sc-22844-raid-full-control-v1`.
- Current recipe: `sc-22844-canonical-continuations/a588ba5b-8084-4123-be1e-569ebf90d219-final/recipe.json`
  under private results. Native source `ccae8ad` and base driver `af2c5e1d…`
  remain unchanged; controller `28c84319…` installs reviewed profile `243ccfff…`.
  This changes effective Python execution; complete hashes and gates are in the ledger.
- After a verified clean pause, use its actual result/PASS review and unchanged
  funding/limits to prepare a fresh UUID continuation. The private
  `sc-22844-canonical-continuation-preparer-source-v2` requires external reviews
  and a grant; it never grants, launches or removes a pause. Keep the same profile
  on continuation and use the reviewed profile-aware checkpoint auditor.
  Root owns review/grant/launch; failed or unknown attempts cannot silently retry.

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
