# Resumed after the user pause

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
