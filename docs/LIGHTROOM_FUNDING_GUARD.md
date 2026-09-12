# Supplementary main inspection funding guard

This is a separate, explicitly identified control layer for S9 inspection. It does
not change the frozen protocol 3 driver/helper, native inspector, plan schema,
adopted command namespaces, or existing evidence. It does not require another plan
copy. The wrapper, adapter, policy and source proofs must all be reviewed before a
new attempt; the first prepared request is **not admitted to run**.

`scripts/lightroom_funding_guard.py` verifies the policy digest supplied by the
reviewed wrapper, then verifies adapter, frozen driver, helper, configuration and
binding digests before executing the checked driver bytes. Native source and
binary digests must agree with the original binding; the frozen driver still
performs its own binary validation. The only inherited driver method replaced is
`Runner.space`. The original driver's `main()` performs main-only dispatch and
retains its phase, replay, command reservation, failure and pause protocols.

`scripts/lightroom_main_funding.patch` records the small change to the private
reviewed main wrapper. Its base SHA is
`3da83dbc7ff0a7cad71912240c0e7a3d8e477eaa2b9d467f8f284e16abc84cb6`.
The resulting wrapper SHA is
`3aa8d1222d8323f491fb9a176fa61416eee790f7b1eca2d05514ea7019e55f7b`.
The microsecond-birth ownership supervisor remains unchanged. The wrapper binds
the adapter policy, invokes the adapter instead of directly invoking the frozen
driver, samples disk alongside existing process observations, and includes the
supplementary observation receipt in its result. This additional execution source
is never represented as part of the old immutable driver identity.

## Prospective funding policy

| Amount | Bytes |
|---|---:|
| S8 protected qualification funding | 385,473,314,816 |
| Existing S9 operating reserve | 34,359,738,368 |
| Existing S9 initial growth allowance | 181,595,701,248 |
| Initial admission and command-boundary pause threshold | 601,428,754,432 |
| Sampled emergency threshold | 419,833,053,184 |

The growth allowance is the unchanged prior main admission estimate (all 48
catalog main bytes times the existing multiplier), not a measured per-command
allocation bound. Conservatively retaining it as headroom for each next command
does not authorize a larger command or change its existing limits. It may cause
an early pause. The initial sample during request preparation was below the
combined admission threshold; execution remains held.

Before each **new** native command reservation, the adapter requires the larger
of the inherited requested minimum and 601,428,754,432 bytes. A shortage publishes
an owned pause without replacing any existing pause, then raises the frozen
`PauseRequested`. Already completed replay entries retain their original meaning;
they neither reserve nor spawn a new command. Earlier failed/unresolved commands
still fail and cannot become successful replay entries.

The outer wrapper samples free space before starting its child and at its existing
approximately one-second observation points. Below the pause threshold it requests
a cooperative command-boundary pause. At or below the emergency threshold it uses
the existing owned-process failure cleanup. The 32 GiB operating reserve separates
that sampled emergency threshold from S8's protected amount. Such an emergency is
a preserved failed/unknown attempt, never a clean pause or permission to retry.

Initial and every newly observed minimum are fsynced in distinct exclusive files
per adapter/outer role. Final observations include the minimum and stop reason;
the existing wrapper also retains stop decisions and cleanup evidence. An existing
attempt's observation files cannot be silently replaced by a retry.

These checks are **not a filesystem quota**. Another process, a single native
command, or cleanup can allocate between observations. Neither the growth
allowance nor the 32 GiB reserve establishes a hard upper bound on that allocation.
If a guaranteed protected allocation is required, separate reserved storage or an
enforced quota is necessary. No such guarantee is claimed by this diagnostic
control policy. Existing memory stops, 600-second scheduling pause, 4800-second
sampled emergency deadline and ownership cleanup remain unchanged.

## Focused validation

Run the portable synthetic adapter contracts with:

```sh
python3 -m unittest discover -s tests -p test_lightroom_funding_guard.py -v
```

The tests exercise the actual frozen `Runner.call` boundary without initializing a
catalog: shortage leaves its journal and step map unchanged, completed replay
still works, larger original admission remains effective, and source/config/native
binding errors fail before source execution. Other cases cover preserved foreign
pause bytes, minima/stop receipts, exact main-only dispatch and attempt reuse.

Three additional controlled Python fixtures in the private package exercise the
actual patched wrapper's cooperative and emergency branches, supplemental argv,
existing cleanup routing, and pre-spawn failure. They mock process observation and
spawn; actual native process-birth/cleanup validation remains the previously passed
eight supervisor fixtures. No native or database execution is claimed by this gate.
