# Override-window report — diagnose-and-fix, 2026-07-15..21

Phase 4 deliverable for the 2026-07-15 operator override (recorded in
PROCESS_LOG). Companion: analysis/DIAGNOSTIC_MAP_2026-07-15.md (full
defect matrix with mechanisms and file:line evidence).

## 1. Diagnostic map (summary)

Nine code defects found by a four-track diagnostic pass and verified
against the pinned minotari-cli@52a7287a source; all fixed. Ranked by
relevance to the maintainer's "new wallet keeps failing":

1. Spendable gate ignored coinbase maturity (gate passed, sends failed
   "Funds are pending") - a73b3e1.
2. S0 error starved S3/S7 of their birthday input; both bailed - the
   exact s0+s3+s7 err trio he reported - dda4ac3.
3. S0 hid wire-failed sends behind a misleading settle-timeout message -
   b1117f2.
4. The settle knob (his requested escape hatch) never reached S1's
   per-send settle - 8d615aa.
5. PP seed still demanded at startup with Mode 3 off - dda4ac3.
6. One transient scan failure erred a whole S1 cell - 8d615aa.
7. S4/S5 hardcoded amounts below the fee floor at default fee_rate -
   b1117f2.
8. Mode 1 had no spendable gate/settle at all; then its first gate
   metric (unspent count) included unconfirmed outputs - 9686052,
   8503c81.
9. No readiness discipline at the S4->S5 boundary - 2c21759.

Plus S0's confirmation poll measuring input-lock latency instead of
confirmation (b1117f2) and a scan-endpoint diagnosability message +
RUNBOOK 7.12 (45d708b).

## 2. Environment findings (live Esmeralda, decisive for his report)

- **fee_rate = 1 transactions take ~an hour to be mined** (observed:
  broadcast 04:58, mined block 755659 ~06:00). His pasted config uses
  fee_rate = 1 (forced by his 1000 uT amount: the fee-floor validation
  rejects amount 1000 at fee_rate 5). No settle window absorbs an hour;
  every chained send then reports pending funds. This is the leading
  explanation of "new wallet keeps failing".
- **Public-gateway submissions confirm in ~4..21 min at fee_rate = 5**
  (three observations); the 600 s settle default is borderline, hence
  the RUNBOOK guidance to set s0_change_confirm_timeout_secs = 1800 for
  public-gateway runs.
- **Esmeralda coinbase maturity is +6 blocks**; confirmed_height lands
  at +3. The gate must respect both (fix 1).
- A base node's port-9005 wallet-HTTP server answers tip queries but
  not the CLI scanner's block download; scans "succeed" finding
  nothing. Documented (RUNBOOK 7.12).
- Measured finding, not a defect: the Mode 1 console wallet reports a
  large available balance yet refuses sends ("Funds are still pending")
  immediately after a concurrent burst (S4) - the fail-fast policy
  records this with its reason instead of grinding.

## 3. Setup path (reproducible)

- Seeds: `wallet-benchmarks gen-seed` x2 (old, new; PP exempt with
  Mode 3 off), stored ~/.config/wallet-benchmarks/seeds/ (0600).
- Funding: SHA3 CPU mining against a synced local minotari_node
  (blocks found in seconds at ~60 M difficulty / 1.4 MH/s; one
  ~7,185 tXTM coinbase per wallet suffices; maturity ~4 min).
- minotari CLI: clone at 52a7287a, pin tari crates =5.4.0-rc.1
  (caret resolution pulls an API-incompatible 5.4.1), one out-of-tree
  arg fix in burn/mod.rs (subcommand unused by the harness).
- harness.toml: maintainer shape + fee_rate=5,
  s0_change_confirm_timeout_secs=1800, public RPC gateway; reduced
  scale doubling_rounds=3, s5_m=20.
- Unattended runs on a laptop need `caffeinate -is` + mains power:
  macOS deep-sleep suspends the run (timers pause coherently; the run
  survives, but wall clock stretches indefinitely).

## 4. Errors caught across runs, by root cause

- Run 1 (fixes A-H, fee 1): Mode 1 S1/S4/S5 fail-fast on pending funds
  (-> Mode 1 gate gap, fix 8); Mode 2 S0 settle timeout (-> fee-1
  latency finding); s3/s7 dispatch bails pre-fix confirmed live.
- Run 2 (fee 5, local 9005): Mode 1 fully sends (71/71 S1);
  Mode 2 scans found nothing (-> endpoint trap); S5 aborted 0.1 s
  after S4 (-> boundary gate, fix 9).
- Run 3 (all fixes, public RPC): Mode 1 9/9 ok; Mode 2 b0 ok (5.3 h
  full scan), s0 settle near-miss at 600 s (-> knob 1800 guidance),
  s1 ok 11 tx with no abort; S5 gate passed on unconfirmed outputs
  (-> fix 8b, available-balance condition).
- Run 4/4b (final): all 18 active cells ok (section 6); Mode 2 s0 ok
  (223 s / 1281 s in two runs) - the maintainer's failing cell passes.
  4a was killed by an OS update mid-Mode-2 (Mode 1 checkpoint survived,
  demonstrating the per-mode checkpointing); 4b ran to completion.

## 5. Fixes shipped (commit order)

| SHA     | One line                                                   |
| ------- | ---------------------------------------------------------- |
| a73b3e1 | wallet_db: maturity-aware confirmed-spendable predicate    |
| dda4ac3 | main: birthday decoupling + PP-seed distinctness exemption |
| 8d615aa | s1: settle knob threaded; transient scan blips tolerated   |
| b1117f2 | scenarios: S0 send honesty, S0 poll scans, config amounts  |
| 9686052 | old_wallet: Mode 1 gate + settle implemented               |
| 2c21759 | scenarios: entry gates at S4 and S5                        |
| 45d708b | modes/RUNBOOK: scan-endpoint trap named; fee-latency 7.12  |
| 8503c81 | old_wallet: gate requires available balance                |
| 7517b4e | analysis: override record + diagnostic map                 |
| d1e306f | WIP checkpoint (travel pause)                              |

Suite at tip: 335 passed / 1 skipped; clippy -D warnings clean;
sentinel src/modes/minotari_subprocess.rs byte-identical throughout.

## 6. Final result profile (run 4b, completed 2026-07-22)

run_complete = true; 18 recorded cells all ok, 9 Mode 3 cells skipped
with reason (no [mode_3] block) - the full 27-cell matrix per
RESULT_PROFILE_SCHEMA.md:

```
mode\scenario     b0      s0      s1       s2      s3      s4        s5       s6      s7
old_wallet        ok(0)   ok(1)   ok(65)   ok(0)   ok(0)   ok(186)   ok(0)    ok(0)   ok(0)
new_wallet        ok(0)   ok(1)   ok(10)   ok(0)   ok(0)   ok(35)    ok(22)   ok(0)   ok(0)
payment_processor skipped x9
```

Profile: wallet-benchmarks-runs/run4/run4b_profile.json. Notables:
Mode 2 s0 (the maintainer's failing cell) passed in 223 s; Mode 2 s5
measured its complete planned set (20 individual + 2 batch); Mode 1 s5
recorded the console wallet refusing post-concurrency sends via the
fail-fast reason (a measured wallet behavior, section 2). Wall clock
dominated by Mode 2's serial sends at public-gateway inclusion latency
and the two full-chain scans.

## 7. Hypotheses about the maintainer's failure, ranked

1. fee_rate=1 inclusion latency (~1 h) starving every settle window -
   consistent with his config, his symptom, and our live reproduction.
2. Coinbase-funded wallet inside the maturity window at S0 (if he
   mines-to-wallet right before running) - reproduced and fixed.
3. The s0+s3+s7 trio was the birthday cascade - reproduced and fixed;
   his matrix shape is fully explained by an S0 err.
4. Settle deadline vs public-gateway inclusion (~8-21 min at fee 5) -
   if he switches to fee 5 without raising the knob he may still see
   borderline S0 timeouts; guidance shipped.

Not validated against his environment: only his re-run can confirm
which of 1/2/4 applies to him (3 is structural and certain).

## 8. Process record

Override authorization, scope, and validation limitations:
analysis/PROCESS_LOG.md entry 2026-07-15. Fixes validated in our
environment only.
