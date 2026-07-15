# Diagnostic map — "new wallet keeps failing" (2026-07-14 report)

Produced by a four-track diagnostic pass over tip 79f6338 (failure-mode
enumeration, S0 lifecycle trace, S1..S7 trace, adversarial audit of the
July-13 fixes), each claim verified against the pinned upstream
`minotari-cli@52a7287a` source cloned locally, then validated by live
funded runs on Esmeralda (runs 1-2, 2026-07-15). Fix commits follow the
map.

## Confirmed defects and their fixes

| # | Defect | Mechanism | Fix |
|---|---|---|---|
| 1 | Spendable gate ignored maturity | `count_confirmed_spendable_utxos` mirrored the CLI selector's status + confirmation predicates but not `maturity <= tip` (upstream `outputs.rs::fetch_unspent_outputs`). A coinbase-funded wallet (the documented `minotari_miner` funding path) is confirmed several blocks before it is selectable: observed live, mined 755167 / confirmed 755170 / maturity 755173. Gate passed early, send failed "Funds are pending", S0 then blamed the settle. | a73b3e1 |
| 2 | S0 error cascaded into S3/S7 | The rescan birthday was populated only by the Ok(S0) arm of `update_scenario_input`, though it derives from wall clock alone. Any S0 error starved S3/S7 of `h_birth_s3`/`s7_h_birth` and both bailed with the internal "requires ScenarioInput::…" message. This is the maintainer's exact s0+s3+s7 err trio. | dda4ac3 |
| 3 | PP seed still demanded with Mode 3 off | `assert_distinct` read `$HARNESS_SEED_PP` unconditionally, so a two-wallet setup aborted at startup despite the funding-preflight exemption (44febd0). | dda4ac3 |
| 4 | Settle knob never reached S1's per-send settle | `settle_after_send(None)` hardcoded the 600 s default; RUNBOOK 7.11 advertised raising `s0_change_confirm_timeout_secs` as the remedy, but the value never reached the settle that actually times out. | 8d615aa |
| 5 | One scan blip erred a whole S1 cell | `refresh_wallet_view().await?` in the confirmation poll propagated a single transient subprocess failure out of a ~127-send loop. Now warn + skip the poll; the per-tx deadline classifies persistent failure as a stall. | 8d615aa |
| 6 | S0 hid wire-failed sends | S0 never inspected `tx_record.status`; a construct/broadcast failure proceeded into the confirm loop, burned the settle deadline, and erred with "send succeeded but its change failed to confirm". Now bails immediately with the recorded error string. | b1117f2 |
| 7 | S0 measured lock latency as confirm time | S0's confirmation poll never scanned, so Mode 2's only observable change was its own input flipping UNSPENT→LOCKED. Now scans each poll, matching S1. | b1117f2 |
| 8 | Hardcoded 1_000 uT amounts in S4/S5 | Below the fee floor (`fee_rate * 700`) at `fee_rate >= 2`; every task fails at sign under the default fee_rate=5. Now `s1_amount_per_tx_microtari`, floor-validated at startup. | b1117f2 |
| 9 | Mode 1 had no spendable gate or settle | The trait no-ops assumed the self-scanning daemon manages availability. Run 1: S0's send left the whole balance as pending change; S1 fail-fasted in 0.2 s on ten identical gRPC "Funds are still pending" errors. Mode 1 now polls `GetUnspentAmounts` for both hooks. | 9686052 |

## The environmental finding (run 1, live)

**`fee_rate = 1` transactions take ~an hour to mine on Esmeralda.** Run
1's Mode 2 S0 send: broadcast 04:58:20, mined at block 755659 (~06:00).
No settle window absorbs a ~60-minute inclusion latency; every chained
send then sees pending funds, which reads as "new wallet keeps failing".
The maintainer's pasted config sets `fee_rate = 1` with
`s1_amount_per_tx_microtari = 1000` (the amount forced the low fee: at
fee_rate=5 the floor validation rejects amount 1000). The defaults
(`fee_rate = 5`, amount 4000) are validated by run 2.

The harness's role is bounded honesty, not masking: the gates wait a
configured window and then name the true state; fail-fast stops the
identical-failure grind and records why. The remedy for the latency
itself is config: a viable fee rate, or a raised
`s0_change_confirm_timeout_secs` if measuring at fee 1 is the intent.

## Hypotheses considered and ruled out

- Locked inputs counted as spendable: disproven; `lock_output` sets
  `status='LOCKED'` (upstream outputs.rs:361) and the predicate excludes
  them.
- Scan under-coverage: `--max-blocks-to-scan u64::MAX`; confirmation pass
  runs per processed block during catch-up.
- Read/write DB split: both paths resolve `<data_dir>/wallet.sqlite3`.
- Cross-cell fail-fast leakage: tracker is per-scenario-instance.

## Deferred (recorded, not fixed this round)

- Subprocess awaits carry no hard timeout (a wedged `minotari scan`
  outlives its caller's deadline poll arm). Deadline arms bound every
  loop; a truly hung child is survivable only via operator kill.
- Error-cell details hardcode `phase:"scan"`; S1 buckets construct
  failures as Broadcast rejections (S5 maps correctly). Diagnosability
  polish, not correctness.
- Mode 2 scan cells report `h_tip_start/end = 0`, degenerate
  blocks-per-sec metrics.
- Double `wipe_and_reimport` per scan cell (scenario + scan_from_birthday
  both wipe): wasted work.
- S5 sends with no settle by design (throughput measurement); on a
  low-UTXO wallet its arms fail-fast and record the reason. Deliberate.
