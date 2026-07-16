# WIP checkpoint — 2026-07-16, override window paused mid-Phase-3

Session paused for operator travel. This file is the resume checkpoint;
delete it when the override window's Phase 4 report is delivered.

## Done

- Diagnostic panel + nine fixes, commits `a73b3e1..8503c81` (9 local
  commits ahead of origin `79f6338`; NOT pushed — push gates on Phase 3
  success + Phase 4 report per the 2026-07-15 override). Suite 335/335,
  clippy clean, sentinel `src/modes/minotari_subprocess.rs` untouched.
  Full map: `analysis/DIAGNOSTIC_MAP_2026-07-15.md`; override record:
  `analysis/PROCESS_LOG.md` (2026-07-15 entry).
- Wallets funded (coinbase, mature): seeds + addresses in
  `~/.config/wallet-benchmarks/seeds/` (0600). Old wallet ~230k tXTM,
  new wallet ~7.2k tXTM (plus S1-split outputs).
- Pinned `minotari` CLI rebuilt at
  `~/Documents/tari-bounties/minotari-cli/target/release/minotari`
  (52a7287a + tari crates pinned `=5.4.0-rc.1` + one local
  API-compat arg in burn/mod.rs; out-of-tree).
- Runs 1-3 evidence in `~/Documents/tari-bounties/wallet-benchmarks-runs/`
  (configs, stdout/stderr logs, checkpointed profiles).
  - Run 1: fee_rate=1 tx took ~60 min to mine (block 755659) - the
    environmental root of "keeps failing". Mode 1 gate gap found.
  - Run 2: Mode 1 S1 completed 71/71 at fee 5; S4->S5 boundary gap and
    the 9005-endpoint trap found (RUNBOOK 7.12).
  - Run 3 (full fix set): Mode 1 all 9 cells ok (S1 70 tx, S4 178 tx);
    Mode 2 b0 ok (5.3 h full scan), s0 err by ~seconds (public-gateway
    inclusion ~8 min vs 600 s settle default), s1 ok 11 tx/6.1 h with
    no fail-fast. Stopped mid-s2 for travel; Mode 2 cells therefore not
    in run3_profile.json (per-mode checkpointing), stdout log has them.

## Left

1. **Run 4** = the clean final profile. Same as run3/harness.toml plus
   `s0_change_confirm_timeout_secs = 1800`. Binary already built with
   all fixes. Expect ~18-24 h wall clock (two ~5 h Mode 2 full scans +
   S1's ~6 h). Non-slow alternative if wanted: also raise fee, or accept
   s2/s6 duration.
2. Assess run 4 per cell; iterate any err cell (diagnostic map has the
   playbook).
3. Phase 4 report per the override: diagnostic map, setup path, errors
   by root cause, fix SHAs, final profile, SWvheerden hypotheses
   (fee_rate=1 latency is the lead; maturity + cascade close behind).
4. Draft the PR #6 reply (RUN13) - only after a defensible profile.
5. Push (all commits), report SHAs. Operator posts the reply manually.

## Resume steps (fresh boot)

```sh
# 1. Node (resyncs the gap in minutes; wallet-http on 9005, grpc 18142):
nohup minotari_node --network esmeralda --base-path ~/tari-esme-node \
  --non-interactive-mode --grpc-enabled \
  --grpc-address /ip4/127.0.0.1/tcp/18142 \
  -p base_node.storage.pruning_horizon=0 \
  -p base_node.grpc_server_allow_methods=get_new_block_template,get_new_block,submit_block,get_tip_info,get_sync_info,get_sync_progress,get_mempool_stats,get_network_difficulty,get_constants,get_blocks,list_headers,get_header_by_hash,get_version,identify,get_tokens_in_circulation,get_block_timing,get_network_status,submit_transaction,transaction_state \
  > ~/tari-esme-node/node_stdout.log 2>&1 &
# wait for sync: curl -s http://127.0.0.1:9005/get_tip_info vs
# https://rpc.esmeralda.tari.com/get_tip_info

# 2. Run 4 (from wallet-benchmarks-runs/run4/, config = run3's plus the knob):
export HARNESS_SEED_OLD="$(cat ~/.config/wallet-benchmarks/seeds/seed_old.txt)"
export HARNESS_SEED_NEW="$(cat ~/.config/wallet-benchmarks/seeds/seed_new.txt)"
export HARNESS_WALLET_PW="local-run-pw"   # unset HARNESS_SEED_PP (Mode 3 off)
RUST_LOG=info nohup .../target/release/wallet-benchmarks run \
  --config harness.toml --output run4_profile.json \
  > run4_stdout.log 2> run4_stderr.log &
```

Note: the on-chain wallets may have late-mining stragglers from run 3's
S1 pending transactions; a fresh run's create+scan absorbs them (self
sends preserve value). Backup refs `backup-pre-amend`=c92489f,
`backup-retracted-items`=3b392e7 retained until merge.
