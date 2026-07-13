# wallet-benchmarks Runbook

This runbook walks an operator through producing a canonical Esmeralda baseline against `tari-project/wallet-benchmarks#1`. The harness binary is `wallet-benchmarks`; the output is `baselines/esmeralda_canonical.json` per `analysis/RESULT_PROFILE_SCHEMA.md`. Read end-to-end before starting; the prerequisites involve four moving parts (Tari binaries, the minotari CLI, the vendored PP submodule, and three funded testnet wallets) and a partial setup wastes hours.

For background on what each mode measures, see `analysis/PR_BODY_v2.md`. For the wallet-stack quirks the harness has to work around, see §7 "Troubleshooting" below; that section mirrors `PR_BODY_v2 §4` "Wallet pain findings" with operator-facing reframing.

------

## §1. Prerequisites

### Binaries

| Component | Required version | Where it comes from |
|---|---|---|
| `minotari_node`, `minotari_console_wallet`, `tari_base_node` (and the rest of the tari_suite bundle) | `v5.4.0-pre.4` or later Esmeralda build | GitHub release `tari-project/tari/v5.4.0-pre.4`. Operator pre-extracts and places on `$PATH` or sets `Config::minotari_console_wallet_path` in `harness.toml`. |
| `minotari` CLI | commit `52a7287a` of `tari-project/minotari-cli`, **with the tari crates bumped to `5.4.0-rc.1`** (see the wallet-crypto warning below) | Operator clones, bumps the `tari_*` workspace versions from `5.3.1-pre.0` to `5.4.0-rc.1`, `cargo build --release`, drops the resulting binary somewhere reachable (default `/usr/local/bin/minotari`). |
| `minotari_payment_processor` (PP) | commit `f0572c9` of `tari-project/minotari_payment_processor` | Vendored as a submodule at `vendor/minotari_payment_processor`. Operator builds from the submodule per §2. **Known limitation:** PP pins tari `5.2.1-pre.2` (tari_crypto 0.22.1) and cannot read outputs created on the current network era; Mode 3 scanning does not work until upstream PP moves its pin (see the warning below). |
| Rust toolchain | stable, edition 2021 | The harness builds against the standard `rust-toolchain.toml` (no nightly features). |
| sqlite3 system lib | present | The harness's `rusqlite` dependency is built with `bundled` so the system sqlite isn't strictly required, but PP's `sqlx` build needs the system header on Linux. |

**Wallet-crypto version alignment (load-bearing).** The v5.4.0-pre/rc binaries build against `tari_crypto 0.23`; tari `5.3.x` and earlier crates build against `tari_crypto 0.22.1`. The encrypted-data recovery that wallet scanning depends on is not compatible across that boundary: a wallet built on 0.22.1 components recovers nothing from outputs created by v5.4-era binaries (verified live with a recovery probe: identical output bytes and identical seed recover under 5.4.0-rc components and return nothing under 5.3.0-pre.3). Every component that scans or signs, including this harness, the `minotari` CLI, and PP, must be built against the same tari lineage as the network binaries. The harness's own `Cargo.toml` pins `5.4.0-rc.1` for this reason. Upstream `minotari-cli` main still pins `5.3.0-pre.3` and needs the bump at build time; upstream PP pins `5.2.1-pre.2` and does not compile against 5.4 without migration.

### Repository checkout

The PP source is a git submodule. A standard clone will leave it empty and `cargo build` will fail at the migration `include_str!` step. Clone with submodules:

```sh
git clone --recurse-submodules \
  https://github.com/<your-fork>/wallet-benchmarks.git
cd wallet-benchmarks
```

If you already have a clone without the submodule, run:

```sh
git submodule update --init --recursive
```

Verify the submodule rev:

```sh
git -C vendor/minotari_payment_processor rev-parse HEAD
# expected: f0572c98cbfac7377412dc6d4094c7d7dfc5de2c
```

------

## §2. One-time setup

These steps run once per operator machine. Re-run only when a binary version changes or the seeds are regenerated.

### §2.1. Build the harness

```sh
cargo build --release
ls target/release/wallet-benchmarks
```

The harness CLI is at `target/release/wallet-benchmarks`. Use the absolute path in invocations below, or add `target/release` to `$PATH`.

### §2.2. Build the PP daemon

PP uses `sqlx` compile-time checked queries with no committed offline query cache, so a bare `cargo build` fails with ~30 `set DATABASE_URL to use query macros online` errors. Point `DATABASE_URL` at a sqlite database with the PP schema applied; the migrations are plain SQL and apply with the stock `sqlite3` CLI (no `sqlx-cli` install needed):

```sh
cd vendor/minotari_payment_processor
rm -f /tmp/pp-build.db
for f in migrations/*.sql; do sqlite3 /tmp/pp-build.db < "$f"; done
DATABASE_URL="sqlite:///tmp/pp-build.db" cargo build --release
ls target/release/minotari_payment_processor
cd ../..
```

The build database is only consulted at compile time for query type-checking; PP creates and migrates its real runtime database itself.

The PP path goes into `harness.toml` as `mode_3.pp_binary_path`. Default the spec uses is `/tmp/tari-pp-build/pp/target/release/minotari_payment_processor`; you can place it anywhere readable and reference the actual path.

### §2.3. Generate three seeds

The harness uses three named seeds (Mode 1 / Mode 2 / Mode 3 wallets). The default env-var names are `HARNESS_SEED_OLD`, `HARNESS_SEED_NEW`, `HARNESS_SEED_PP`. Generate each with the harness's `gen-seed` subcommand and capture the output:

```sh
./target/release/wallet-benchmarks gen-seed > /tmp/seed-old.txt
./target/release/wallet-benchmarks gen-seed > /tmp/seed-new.txt
./target/release/wallet-benchmarks gen-seed > /tmp/seed-pp.txt
chmod 600 /tmp/seed-*.txt
```

The `gen-seed` output is a 24-word Tari mnemonic on a single line. Treat these files as production secrets even on testnet: the harness derives spendable wallets from them, and operational habits propagate to mainnet.

### §2.4. Print and record the addresses

The harness's `print-address` subcommand derives the Tari address from the seed mnemonic held in the named env var. Export and derive each:

```sh
export HARNESS_SEED_OLD="$(cat /tmp/seed-old.txt)"
export HARNESS_SEED_NEW="$(cat /tmp/seed-new.txt)"
export HARNESS_SEED_PP="$(cat /tmp/seed-pp.txt)"

./target/release/wallet-benchmarks print-address --seed-env HARNESS_SEED_OLD
./target/release/wallet-benchmarks print-address --seed-env HARNESS_SEED_NEW
./target/release/wallet-benchmarks print-address --seed-env HARNESS_SEED_PP
```

Each prints one base58 Tari address. Record all three; you fund them in §4.

### §2.5. Mode 3 view-key and spend-public-key (auto-derived; env override optional)

Mode 3 watches a single account named `"default"` via a view-only wallet held by the PR daemon, plus a signing wallet held by PP's spawned `console_wallet`. Both daemons consume the same view-key and spend-public-key pair.

**The harness auto-derives the pair from `HARNESS_SEED_PP` at startup.** As of commit `b7d05a3` (per @SWvheerden's 2026-06-19 review feedback), `wallet_lifecycle::pp_lifecycle::resolve_account_keys` walks the mnemonic through the same `WalletType::SeedWords` path that `print-address` uses and extracts the `(view_private_key_hex, spend_public_key_hex)` pair. No manual extraction step is required for the common case.

The env-var override path remains available for two cases:

- **Operator-injected override.** When you want PP and the PR daemon to scan a wallet that differs from `HARNESS_SEED_PP` (rare; useful when funding flows through a different wallet). Set BOTH `TARI_BENCH_VIEW_KEY` and `TARI_BENCH_SPEND_KEY`; the harness uses them verbatim.
- **Cross-machine handoff.** When the wallet was funded on a different machine and you only have the key pair, not the mnemonic. Same shape.

```sh
# Optional override only. Skip this block for normal canonical-baseline runs.
export TARI_BENCH_VIEW_KEY=<hex view private key, 64 chars>
export TARI_BENCH_SPEND_KEY=<hex public spend key, 64 chars>
```

`resolve_account_keys` requires **both or neither** of the env vars to be set. Half an override (only `TARI_BENCH_VIEW_KEY` set, or only `TARI_BENCH_SPEND_KEY` set) bails at startup with a clear error message: half-overrides are almost always a typo, and silently filling the missing half from the seed would risk pairing two unrelated wallets' keys.

The harness reads the env-var values only; it never writes them to disk.

### §2.6. Set the wallet password

The wallet password is shared across all spawned wallets in this run (Mode 1's console_wallet, Mode 2's minotari subprocess, and PP's signer console_wallet). Default env var name is `HARNESS_WALLET_PW`. Pick anything non-empty; rotate per-run if you want:

```sh
export HARNESS_WALLET_PW=harness_pp_password
```

The fixture `harness_pp_password` is what the PP lifecycle code expects when no override is configured. The password is read from env at lifecycle spawn, so keep it consistent across all subprocesses for the run (the same value drives Mode 1's console_wallet, Mode 2's `minotari` subprocess, and PP's spawned signer wallet).

### §2.7. Write the environment file

Source this on every shell that runs the harness so the env stays consistent:

```sh
cat > .env.harness <<'EOF'
export HARNESS_SEED_OLD="$(cat /tmp/seed-old.txt)"
export HARNESS_SEED_NEW="$(cat /tmp/seed-new.txt)"
export HARNESS_SEED_PP="$(cat /tmp/seed-pp.txt)"
export HARNESS_WALLET_PW=harness_pp_password
# Optional Mode 3 overrides (leave commented for the common case).
# Setting these makes PP and the PR daemon scan a wallet that differs
# from HARNESS_SEED_PP (§2.5). Set both or neither.
# export TARI_BENCH_VIEW_KEY=<hex>
# export TARI_BENCH_SPEND_KEY=<hex>
EOF
chmod 600 .env.harness
```

Source it in every harness-running shell:

```sh
. ./.env.harness
```

Do not commit `.env.harness` or the `/tmp/seed-*.txt` files. They contain spendable wallet material.

------

## §3. `harness.toml` reference

Copy `harness.toml.example` to `harness.toml` and edit per host. Every field is optional except `mode_3.pp_binary_path` and `mode_3.minotari_binary_path` (when Mode 3 is enabled). Defaults match the bounty issue's parameter table and `analysis/DESIGN.md §Decisions`.

### §3.1. Top-level keys

| Key | Default | Override when |
|---|---|---|
| `network` | `"esmeralda"` | Never; `guards::enforce_esmeralda` rejects anything else. |
| `base_node_url` | `https://rpc.esmeralda.tari.com` | Running against a private base-node deployment. |
| `minotari_console_wallet_path` | `$PATH` lookup | The bundled `minotari_console_wallet` is not on `$PATH`. |
| `minotari_path` | `$PATH` lookup | The `minotari` CLI (Mode 2 binary) is not on `$PATH`. |
| `a_fund` | `10_000_000_000` µT (10k XTM) | Bounty issue parameter table. Match `analysis/ANALYSIS.md AC-1`. |
| `c_min` | `3` | DESIGN.md §Decisions; the minimum confirmation depth. |
| `volume_target` | `512` | S1 volume target (output count). |
| `doubling_rounds` | `6` | S1 doubling factor. |
| `fanout_outputs_per_tx` | `8` | S1 per-tx fan-out. |
| `s4_t_budget_ms` | `900_000` (15 min) | S4 wall-clock budget. |
| `s5_m`, `s5_k` | `100`, `10` | S5 batch dimensions. |
| `fee_rate` | `5` µT per gram | Bounty parameter table. |
| `per_tx_confirmation_timeout_ms` | `1_800_000` (30 min) | Upper bound on the per-send confirmation poll (S0's single wait; S1's per-send wait). No longer bounds console-wallet boot; see `wallet_ready_deadline_ms`. |
| `s0_change_confirm_timeout_secs` | `600` (10 min) | Bounds S0's post-send settle gate AND the pre-send entry gates at S0 and S1 (the wallet must hold at least 1 confirmed spendable input before sending; see section 7.11). `0` skips all three gates. S1's per-send settle between chained sends keeps its own shared default. |
| `fail_fast_identical_failure_threshold` | `10` | Send loops (S1, S4, S5) abort the scenario after this many contiguous byte-identical failures, recording the reason in the cell's details. A success or a different error string resets the streak; `0` disables the policy. |
| `wallet_ready_deadline_ms` | `1_800_000` (30 min) | How long Mode 1's `wait_ready` waits for a spawned `minotari_console_wallet` to bind gRPC. A `--recovery` wallet binds only after its recovery scan; birthday-0 recovery walks the whole chain (measured ~490-2,300 blocks/s at height ~731k, i.e. 5-25 min, and growing with the chain). Raise this before raising anything else when B0/S2/S6 report "failed to connect to wallet gRPC". |
| `sampler_interval_ms` | `1_000` | Resource sampler cadence. |
| `s1_amount_per_tx_microtari` | `4000` µT | The per-tx amount S1 sends. Must exceed the full single-send fee (`fee_rate × ~700 grams`; config validation rejects amounts at or below it) or Mode 2/3 signing refuses the tx. |

### §3.2. `[seeds]` table

Names of the env vars holding each seed mnemonic. The TOML never contains the mnemonics themselves.

```toml
[seeds]
old = "HARNESS_SEED_OLD"
new = "HARNESS_SEED_NEW"
payment_processor = "HARNESS_SEED_PP"
wallet_password = "HARNESS_WALLET_PW"
```

Override any of these only if you have a multi-run setup where you swap seeds between runs and need distinct env-var namespaces. The defaults are what every other piece of harness documentation references.

### §3.3. `[mode_3]` table

OPTIONAL. Absent block = Mode 3 disabled: the funding pre-flight exempts the payment-processor seed (its balance is never queried; the pre-flight pass line shows `pp=DISABLED`), the run covers Modes 1 and 2, and the nine Mode 3 cells are recorded as skipped (null in the profile, `status=skipped` progress lines) instead of failing. This is distinct from a present-but-unreachable Mode 3 setup: with the block configured, the payment-processor seed must be funded like the others, and if the PR daemon balance probe cannot be reached the pre-flight warns and marks the run's Mode 3 arm skipped (`pp=SKIPPED`). When the block IS present, `Config::validate` checks it at startup (binary paths, account key env vars) before anything spawns, so a bad Mode 3 config fails in the first second rather than hours in.

```toml
[mode_3]
pp_binary_path = "/abs/path/to/minotari_payment_processor"
minotari_binary_path = "/usr/local/bin/minotari"
api_port = 9145
pr_port = 9146
pr_base_url = "https://rpc.esmeralda.tari.com"
terminal_state_poll_timeout_secs = 60
```

| Key | Default | Notes |
|---|---|---|
| `pp_binary_path` | required | Absolute path to the binary built in §2.2. |
| `minotari_binary_path` | required | Absolute path to the `minotari` CLI from §1. PR daemon's `minotari daemon` subcommand runs this binary. |
| `api_port` | `9145` | PP's HTTP listen port. Override only when 9145 is taken. |
| `pr_port` | `9146` | PR daemon's HTTP listen port. Override only when 9146 is taken. |
| `pr_base_url` | `https://rpc.esmeralda.tari.com` | The blockchain RPC the PR daemon scans against. Default matches `base_node_url`; override when co-located with a private base node. |
| `terminal_state_poll_timeout_secs` | `60` | After all S0/S4/S5 sends, the harness polls each submitted payment to a PP terminal state. This caps the wait per submitted payment. |

### §3.4. `[mode_3.worker_sleep_overrides]` table

PP ships with conservative defaults (10-minute batch creator interval) tuned for production retail use. The harness drives all five intervals down to 1s (5s for the confirmation checker) so a bench run completes in under an hour. Override only when investigating PP internals; production canonical baselines should leave the bench values in place.

```toml
[mode_3.worker_sleep_overrides]
batch_creator = 1            # PP default 600
unsigned_tx_creator = 1      # PP default 15
transaction_signer = 1       # PP default 10
broadcaster = 1              # PP default 15
confirmation_checker = 5     # PP default 60
```

A `null` value lets PP keep its own default.

### §3.5. `[mode_3.accounts.bench]` table

```toml
[mode_3.accounts.bench]
view_key_env = "TARI_BENCH_VIEW_KEY"
public_spend_key_env = "TARI_BENCH_SPEND_KEY"
```

The two `*_env` fields name env vars holding the 64-char hex view key and the 64-char hex public spend key. **Both env vars are optional.** When unset (the common case), the harness auto-derives the pair from `HARNESS_SEED_PP` via `wallet_lifecycle::pp_lifecycle::resolve_account_keys` (see §2.5). When set, the env-var values override the derived pair for operators who need to scan a wallet that differs from `HARNESS_SEED_PP`. The values themselves never live in TOML; same convention as the `[seeds]` block.

PP and the PR daemon watch the same `"default"` account using whichever keypair `resolve_account_keys` returns. The operator-facing config key segment `BENCH` is the env-var-name convention and does not need to match what minotari calls the account internally (per `init_wallet.rs:121` that name is hardcoded `"default"`).

------

## §4. Funding

Each funded wallet must hold at least **11_000_000_000 µT (11k tXTM)** before the run starts. The bounty's `a_fund` parameter is 10k XTM per wallet; the extra 10% is `enforce_funding`'s headroom margin to account for fees consumed during the run. Three wallets need funding when `[mode_3]` is configured; without a `[mode_3]` block only the old and new wallets do, because the pre-flight exempts the payment-processor seed entirely (section 3.3).

### §4.1. Fund the wallets (faucet or solo mining)

For each of the three addresses recorded in §2.4, request funding via the Tari Esmeralda faucet (or whichever testnet funding channel your operator setup uses). Allow a few minutes for mining and confirmation.

**Solo mining path (no faucet needed).** Esmeralda SHA3 difficulty is low enough for CPU solo mining (a laptop finds blocks in seconds to minutes), and coinbase maturity is only 6 blocks. Against a fully synced local node:

```sh
minotari_miner --network esmeralda -b <miner_base_dir> \
  --non-interactive-mode --miner-max-blocks 1 \
  -p miner.base_node_grpc_address=http://127.0.0.1:18142 \
  -p miner.wallet_payment_address=<address from §2.4> \
  -p miner.num_mining_threads=8 \
  -p miner.mine_on_tip_only=true \
  -p miner.range_proof_type=bullet_proof_plus
```

Two traps, both verified live on v5.4.0-rc.1:

* **Use `bullet_proof_plus` coinbases.** The miner's default `revealed_value` coinbases are not recoverable by scanning wallets: the wallet decrypts the value but reconstructs a different commitment, the base node reports the reconstructed output as unmined, and the funds stay invisible forever. BulletProofPlus coinbases recover normally.
* **The node's gRPC allowlist must include the mining methods** (`get_new_block_template`, `get_new_block`, `submit_block`), and the node must be fully synced (`mine_on_tip_only` refuses otherwise, which is what you want; mining on an unsynced node forks you off canonical).

One block pays roughly the full block reward (thousands of tXTM at current esmeralda emission), so a single block per wallet more than covers `a_fund` with headroom.

### §4.2. Verify each wallet sees the funds

There is no one-shot `get-balance` subcommand on `minotari_console_wallet`; the wallet is a long-running daemon that exposes balance via gRPC. The fastest reliable verification path is to let the harness's own `enforce_funding` pre-flight do the check.

**Primary verify path: run the harness's pre-flight.** Set up the env file from §2.7 and run the harness with default flags. `enforce_funding` (per `src/guards.rs`) spawns a transient `minotari_console_wallet` per seed, waits until each wallet sees a positive balance via `wait_ready_funded` (per `src/wallet_lifecycle/console_wallet.rs`, gates on `Online + GetStateResponse.balance.available_balance > 0`), then prints:

```
[HH:MM:SS] preflight  old=11000000000uT new=11000000000uT pp=11000000000uT  PASS
```

Without a `[mode_3]` block the pp column reports the exemption instead of a balance:

```
[HH:MM:SS] preflight  old=11000000000uT new=11000000000uT pp=DISABLED  PASS
```

If any wallet is short, `enforce_funding` bails with a per-seed shortage report naming exactly which seed and by how much. This is the same scan-and-balance code path the canonical run uses, so a green pre-flight here means a green pre-flight on the canonical run that follows.

Per-wallet timing on a healthy network: ~30s of subprocess startup plus the wallet's own scan time (5-15 min on a cold import to a recent block height). The harness runs the per-seed checks in parallel (since commit `1beaf43`), each under its own `wallet_ready_deadline_ms` deadline (30 min by default), so a fully cold pre-flight wraps in roughly the slowest single wallet's scan time.

**Manual verify path (optional).** If you want to verify outside the harness (e.g. before configuring `harness.toml`), the working pattern mirrors what the harness does for Mode 1's console wallet:

```sh
# 1. Spawn the wallet non-interactively in RECOVERY mode with the seed
#    words in the wallet's env var. It recovers from the wallet birthday
#    during boot; the gRPC server then reports the balance immediately.
#    Pick any free port.
MINOTARI_WALLET_SEED_WORDS="$HARNESS_SEED_OLD" \
./tools/minotari_console_wallet \
  --network esmeralda \
  --base-path /tmp/verify-old \
  --password "$HARNESS_WALLET_PW" \
  --recovery \
  --non-interactive-mode \
  --grpc-address /ip4/127.0.0.1/tcp/18142 &

# 2. Tail the wallet log for recovery progress, then either query gRPC
#    directly (grpcurl + GetState / GetBalance) or kill the wallet
#    and let the harness's pre-flight do the read.
```

**Do NOT use `--seed-words-file` to import a seed.** On the v5.4 wallet that flag is an EXPORT: `init_wallet` writes the wallet's own seed words to the given path after startup and never reads it. A fresh non-interactive wallet given only `--seed-words-file` silently creates a brand-new random wallet and overwrites your file with its new mnemonic; the wallet then scans forever for keys nobody funded. Seed import is `--recovery` plus `--seed-words "<24 words>"` (or the `MINOTARI_WALLET_SEED_WORDS` env var, which keeps the mnemonic off argv). The harness spawns wallets exactly this way in `src/wallet_lifecycle/console_wallet.rs::spawn`. Note also that `get-balance` is NOT a console-wallet subcommand: the wallet is a daemon and balance reads go over gRPC.

In practice, sticking to the primary path (let `enforce_funding` do it) is simpler and is what `RUNBOOK §5` already assumes.

### §4.3. Mode 3: view-key wallet check

This section applies only when `[mode_3]` is configured; without the block the pre-flight never queries the payment-processor seed at all (section 3.3). For Mode 3, the funded wallet is the same one whose view-key was extracted in §2.5. The PR daemon (spawned by the harness at run time) exposes `GET /accounts/default/balance` as the canonical balance probe. You don't need to pre-warm this; the harness's `enforce_funding` logs a warning and skips the Mode 3 arm if the PR daemon isn't yet up (the warn-and-skip behaviour from `src/guards.rs` plus `src/wallet_lifecycle/pr_balance_query.rs`, reported as `pp=SKIPPED` on the pass line).

------

## §5. Run invocation

With the environment sourced (§2.7), the binary built (§2.1), `harness.toml` written (§3), and the wallets funded (§4), run the baseline. Mode 3 runs only when `harness.toml` carries a `[mode_3]` block (§3.3); without it the run covers Modes 1 and 2 and marks Mode 3 skipped. The profile is checkpointed after the pre-flight and after each mode (`run_complete: false`), so an interrupted run keeps its completed cells; the final write flips `run_complete` to `true`.

```sh
. ./.env.harness
./target/release/wallet-benchmarks run \
  --config harness.toml \
  --output baselines/esmeralda_canonical.json
```

Both flags accept defaults; the bare form below is equivalent for a baseline run:

```sh
./target/release/wallet-benchmarks run
```

(`run` is the default subcommand; `./target/release/wallet-benchmarks` with no subcommand resolves to `Commands::Run` with default `--config harness.toml` and default `--output baselines/esmeralda_canonical.json`.)

### §5.1. Skip the funding pre-flight (testing only)

```sh
./target/release/wallet-benchmarks run --skip-funding-preflight
```

`--skip-funding-preflight` exists for the test suite and for debugging the run loop without burning faucet funds. A production canonical baseline MUST use the live pre-flight to catch under-funded seeds before consuming hours of scenario time. The flag does not appear in any operator-facing artifact.

### §5.2. Expected wall-clock

A canonical Esmeralda baseline takes 3 to 5 hours. The dominant cost is S1 (volume) scan time across all three modes; S2 and S3 (chain-tip-driven scenarios) gate on testnet block production. Plan accordingly; the harness logs the per-scenario start time so you can spot-check progress.

### §5.3. Logging

`env_logger` reads `RUST_LOG`. The defaults are quiet; for a baseline run, set:

```sh
RUST_LOG=info,c=debug ./target/release/wallet-benchmarks run
```

The harness's log targets use the `c::` prefix (mirroring the tari codebase convention; see the `LOG_TARGET` constants in `src/`), so the DEBUG directive must be `c=debug`, NOT `wallet_benchmarks=debug`. A crate-name directive matches the module path only when the log macro omits `target:`, which the harness never does; with the wrong directive every per-poll wait_ready diagnostic is silently filtered out. `c=debug` surfaces the per-mode lifecycle events (wait_ready poll-by-poll state, balance polling, subprocess spawn/teardown, PP HTTP probes). `info` keeps the per-tx noise readable.

### §5.4. Terminal feedback (independent of `RUST_LOG`)

As of commit `eab102a`, the harness prints progress to stdout at every scenario boundary regardless of `RUST_LOG`. A baseline run looks like:

```
[14:33:09] preflight  old=11000000000uT new=11000000000uT pp=11000000000uT  PASS
[14:33:11] mode=old_wallet scenario=B0  start
[14:33:42] mode=old_wallet scenario=B0  done   tx_count=0 elapsed=31.4s status=ok
[14:33:42] mode=old_wallet scenario=S0  start
[14:33:47] mode=old_wallet scenario=S0  done   tx_count=1 elapsed=4.2s status=ok
...
[18:11:43] mode=payment_processor scenario=S7  done   tx_count=0 elapsed=0.1s status=skipped

=== run summary ===
mode\scenario        B0            S0            S1            S2            ...
old_wallet           ok (0)        ok (1)        ok (512)      ok (0)        ...
new_wallet           ok (0)        ok (1)        ok (512)      ok (0)        ...
payment_processor    skipped       ok (1)        skipped       skipped       ...
```

The pre-flight line is emitted by `enforce_funding` after balances pass; if it fails, `enforce_funding` bails before the line prints and the harness exits non-zero. Each per-scenario `start` / `done` pair brackets one `(mode, scenario)` cell. The `tx_count` column is a best-effort count per scenario (S5 reports `success_count`, S4 reconstructs from `n_concurrent × success_rate`, scan-only scenarios report 0); the canonical numbers live in the result-profile JSON. `status` is one of `ok`, `skipped` (the runner mapped an `UnsupportedOperation` to `CellResult::NotRun`), or `err`.

The summary table at the end is a fixed-width text table that pastes cleanly into a PR comment.

------

## §6. Output

The canonical output is `baselines/esmeralda_canonical.json` (override with `--output`). Schema is documented in `analysis/RESULT_PROFILE_SCHEMA.md`:

```jsonc
{
  "schema_version": 1,
  "run_id": "2026-MM-DDTHH-MM-SSZ-XXXX",
  "run_start": "...",
  "run_end":   "...",
  "config":      { ... },
  "environment": { ... },
  "versions":    { ... },
  "modes": {
    "old_wallet":        { /* 9 scenario cells */ },
    "new_wallet":        { /* 9 scenario cells */ },
    "payment_processor": { /* 9 scenario cells */ }
  }
}
```

Validate it loads cleanly:

```sh
python3 -c 'import json; json.load(open("baselines/esmeralda_canonical.json")); print("OK")'
jq '.modes | keys' baselines/esmeralda_canonical.json
```

The harness writes the file at run completion. A crash or `^C` mid-run leaves no partial output by design (the in-memory `Matrix` is written once at the very end). If you need partial state for debugging, watch `RUST_LOG=info` output.

------

## §7. Troubleshooting

This section maps real upstream-wallet behaviour to operator-facing symptoms. Mirror of `PR_BODY_v2 §4` "Wallet pain findings".

### §7.1. Mode 3 batches stall at `SigningInProgress`

**Symptom**: The harness completes S0 for Modes 1 and 2 quickly. For Mode 3, the result-profile shows Mode 3's S5 cells with PP payment status terminating at `AwaitingSignature` or `SigningInProgress`, with a `failure_reason` that references the signer subprocess.

**Cause**: `minotari create-unsigned-transaction` (run inside the PR daemon's `unsigned_tx_creator` worker) emits unsigned-tx JSON at protocol `version: 4.0.0`. `minotari_console_wallet sign-one-sided-transaction` (run inside PP's `transaction_signer` worker, spawned per signing step) rejects this expecting `version: 5.0.0`. PP retries every `TRANSACTION_SIGNER_SLEEP_SECS` (1s in bench config) with no max-attempts cap. Payments stall indefinitely.

**Resolution**: This is an upstream toolchain composition bug, not a harness defect. It is the expected outcome of a Mode 3 canonical baseline against current `v5.4.0-pre.4` plus `minotari-cli@52a7287a` plus PP@`f0572c9`. The harness records the real PP terminal state and the actual error string per the "harness does not hide wallet pain" AC. Ingest throughput (`POST /v1/payment-batches`) remains measurable; end-to-end settlement is not. See `analysis/PP_PEPESILVIA_RECON.md` for the upstream reproduction.

### §7.2. Mode 1 / Mode 2 `enforce_funding` times out on a wallet you just funded

**Symptom**: The harness aborts with `wallet did not reach ready (policy=OnlineAndFunded, role=..., grpc=...) within 1800s; polls=N; last state: connectivity=Online (1), scanned_height=H, available_balance=B uT` even though you confirmed the wallet was funded in §4.2.

**Read the embedded last state first.** The pre-flight gate is `Online` AND `available_balance > 0`; the timeout error reports the last `GetState` snapshot so you can classify the failure without a re-run:

- `connectivity=Online, scanned_height=0` for the whole window: the wallet's UTXO scanner never completed a pass. See the scanner note below; this is almost always a poisoned or missing scan source, not a funding problem.
- `connectivity=Online, scanned_height` advancing but `available_balance=0`: the scan is running but has not reached the funding transaction's height yet (or the wallet genuinely holds nothing). Give it time or check the funding tx landed.
- `connectivity=Initializing/Offline` throughout: the wallet never reached the network; check connectivity/tor.

Per-poll detail is available with `RUST_LOG=info,c=debug` (§5.3): one DEBUG line per second per wallet with connectivity, scanned_height, and available_balance.

**How the wallet scans (v5.4.x)**: the console wallet's UTXO scanner reads the chain over the base node's HTTP wallet-query service, NOT over gRPC or p2p. The scan source is `wallet.http_server_url`, which defaults to `http://127.0.0.1:9005` on Esmeralda, with `https://rpc.esmeralda.tari.com` as fallback, on a 60 second scan interval. The fallback engages only when the primary does not respond at all.

**The trap**: if anything answers on `127.0.0.1:9005` with a stale or unsynced chain view (a local base node mid-sync is the classic case; every `minotari_node` serves this port by default), every console wallet on that host scans against it, sees a chain shorter than the funding height, and reports `available_balance=0` forever. The primary responded, so the fallback never engages, and the wallet still shows `Online` because p2p connectivity is healthy. From the outside this is indistinguishable from an unfunded wallet, except that `scanned_height` stays pinned at 0 (or at the stale node's tip).

**Resolution**:
- If you run a local base node, make sure it is fully synced (`curl -s http://127.0.0.1:9005/get_tip_info | jq .is_synced` must be `true`) before running the pre-flight; a synced local node is also the fastest scan source.
- If you do not run one, make sure nothing else is bound to 9005, so the wallet falls back to the public endpoint.
- To pin the scan source explicitly, pass `-p wallet.http_server_url=<url>` to any manually spawned console wallet (the harness's spawned wallets use the wallet defaults).

### §7.3. Mode 2 hits `insufficient_funds` mid-run

**Symptom**: Mode 2's S0 or S1 send_single calls fail with `create-unsigned-transaction` reporting `insufficient_funds`, despite the wallet showing a positive balance immediately afterwards.

**Cause**: The `minotari scan` subprocess exits cleanly when blockchain catch-up completes, but the wallet finalizes per-output commitment and state-write work asynchronously. The next subprocess call (`create-unsigned-transaction`) reads the sqlite3 DB before that finalization completes; UTXOs appear in the count but aren't yet flagged spendable.

**Resolution**: The harness's `wait_for_balance_positive` in `src/modes/minotari_wallet_ops.rs` polls `minotari Balance` every 5s (default 5-minute deadline) after `run_scan_subprocess` returns, so `scan_from_birthday` blocks until the wallet self-reports a positive balance. If you still see `insufficient_funds` after this, the funding tx hasn't been seen by the scan window the harness configured (`max_blocks_to_scan = u64::MAX`); check that the funding tx is mined and confirmed past `c_min` blocks.

### §7.4. PR daemon argv parse error at Mode 3 startup

**Symptom**: The harness logs `PrLifecycle::spawn: import-view-key exited with ExitStatus(2)` or `daemon exit code: 2` followed by clap parse error text from the `minotari` binary.

**Cause**: The `minotari import-view-key` and `minotari daemon` subcommands accept `--database-path` (a sqlite3 file path, not a directory) and require `--network` at the top-level `Cli` position (BEFORE the subcommand). Older drafts of the lifecycle passed the data dir to `--database-path` and put `--network` after the subcommand; both fail clap parsing.

**Resolution**: Current code in `src/wallet_lifecycle/pr_lifecycle.rs` constructs `data_dir.join("wallet.sqlite3")` for `--database-path` and positions `--network` correctly. If you see the error, you're likely running an older build; `cargo build --release` against the current commit fixes it.

### §7.5. Secrets visible in `ps -ef`

**Symptom**: On a multi-user host, `ps -ef` shows the spawned `minotari import-view-key` and `minotari daemon` commands with `--password`, `--view-private-key`, and `--spend-public-key` arguments inline.

**Cause**: Both subcommands accept these as CLI flags. Upstream `minotari` does not appear to accept env-var fallbacks. The view key alone reveals every incoming amount and address for the account; the password unlocks the wallet DB.

**Resolution**: Run the harness on a single-user box or a CI runner that does not share OS process tables. Documented limitation; if upstream `minotari` later adds env-var support, the harness's lifecycle code should switch to it. Track as defense-in-depth follow-up.

### §7.6. PP `logs/audit.log` accumulates files in CWD

**Symptom**: After a run, the operator sees a `logs/` directory next to the harness binary with rolling `audit.log.1`, `audit.log.2`, etc.

**Cause**: PP writes `logs/audit.log` (10MB × 5 rolling files) relative to its CWD. The path is hardcoded; no env override.

**Resolution**: The harness sets PP's CWD to the per-run Mode 3 tempdir under `HarnessDataDir`, so the `logs/` directory lands inside the tempdir and is cleaned on `Drop`. If you still see top-level `logs/` files, you may be running an older build that doesn't CWD-confine PP; rebuild against the current commit.

### §7.7. PP ports 9145 / 9146 already in use

**Symptom**: PR daemon or PP fails to bind on its configured port with `address already in use`.

**Cause**: Both ports are configurable per-instance (per the post-review fix B2). Default `9145` / `9146` was a convention from the pre-review draft.

**Resolution**: Override `mode_3.api_port` and `mode_3.pr_port` in `harness.toml`. The harness reads both into the lifecycle constructors at startup; you do not need to rebuild.

### §7.8. PP's `harness_pp_password` fixture

The `CONSOLE_WALLET_PASSWORD` env passed to PP is the fixture string `harness_pp_password`. This is the password PP uses when it shells out to `minotari_console_wallet sign-one-sided-transaction`. Keep `HARNESS_WALLET_PW=harness_pp_password` (the default §2.6 value) unless you also patch `src/wallet_lifecycle/pp_lifecycle.rs` to read it from a different env var. The two values must match for PP's signer worker to decrypt the wallet DB.

### §7.9. The `--account-name` flag does not exist on `minotari daemon`

**Symptom**: An older lifecycle draft passed `--account-name BENCH` to `minotari daemon` or `minotari import-view-key`; both subcommands reject the flag with clap parse error.

**Cause**: Upstream `minotari-cli` does not expose `--account-name` on these subcommands. The created or served wallet's account name is hardcoded to `"default"` per `minotari-cli@52a7287a/minotari/src/utils/init_wallet.rs:121`.

**Resolution**: The harness's `ACCOUNT_NAME` constants in `pr_lifecycle.rs:57` and `pp_lifecycle.rs:60` are both `"default"`. The operator-facing env-var key `BENCH` (in `ACCOUNTS__BENCH__NAME=default`) is just the config-map identifier; PP's HTTP calls to the PR daemon hit `/accounts/default/...` and resolve correctly.

### §7.10. Cell status `success` with `t_confirm_ms: null` or `stall_count > 0`

**Symptom**: A result-profile cell reports `"status": "success"` while its payload carries `t_confirm_ms: null` (S0) or its errors block carries `stall_count > 0` (S1), typically on Mode 3.

**Cause (by design, not a bug)**: The cell envelope status means "the scenario ran to completion and produced its measurement", the same convention as B0/S2/S3/S6/S7 (`result_profile::outcome_to_envelope_json`). Confirmation truth lives one level down and its shape differs by scenario: S0 is a single-send warmup, so its confirmation run-out is `t_confirm_ms: null` in the payload; S1/S4 aggregate many sends, so theirs is the per-send terminal-state counters (`stall_count`, `timeout_count`) and, for S1, the round count (`halted` = fewer than the canonical 7 rounds ran). Mode 3 reports a constant UTXO count of 0 (no scanning wallet), so every per-send confirmation wait on that mode runs to its bound and records honestly as `t_confirm_ms: null` / a stall.

**Resolution**: Read the payload and errors block, not just the status, when judging Mode 3 cells. End-to-end confirmation coverage for Mode 3 requires a per-payment terminal-state signal from the PP pipeline (`/v1/payments/{id}`), which is follow-up work; the per-cell envelope is behaving as specified.

### §7.11. S0 fails with "wallet has no confirmed spendable input"

**Symptom**: S0 errs at entry with "wallet has no confirmed spendable input after Ns; the funding output is likely still inside the confirmation window".

**Cause**: Mode 2's CLI wallet can only spend outputs that are mined AND buried by the confirmation window. A freshly funded wallet shows its balance as pending (the funding transaction is mined but not yet buried), so a send would fail at lock-funds with "Funds are pending". The entry gate waits for at least one confirmed spendable input, bounded by `s0_change_confirm_timeout_secs`, and fails S0 with the true cause instead of letting the send produce a misleading error.

**Resolution**: wait a few blocks for the funding output to confirm and re-run, or raise `s0_change_confirm_timeout_secs` so the gate waits longer (each block is roughly 45 seconds on esmeralda; burial takes the confirmation window plus one block). Setting the key to `0` skips the gate entirely and restores the raw failure behavior. S1 runs the same gate at entry but does not fail on it: its sends record the pending-funds state honestly.

------

## §8. After a successful run

1. The result-profile JSON is at `baselines/esmeralda_canonical.json`. Commit it (or attach it to the PR) as the canonical baseline artifact.
2. Tear down: no explicit step required. The harness owns the per-mode tempdirs, the PR daemon child, and the PP child. Drop guards (`tokio::process::Command::kill_on_drop(true)`) escalate SIGTERM to SIGKILL on drop.
3. The funded testnet wallets retain their unused balance. Recycle them across runs unless you regenerated seeds.

------

## §9. References

- `analysis/PR_BODY_v2.md`: the PR body for `tari-project/wallet-benchmarks#1`. §4 of that doc is the upstream-source view of every gotcha §7 above describes operationally.
- `analysis/RESULT_PROFILE_SCHEMA.md`: output JSON shape.
- `analysis/specs/MODE_3_REWORK_SPEC.md`: Mode 3 design and §16 amendment record.
- `analysis/MODE3_REWORK_BRIEF.md`: strategic decisions (PAYMENT_RECEIVER strategy, measurement scope, etc.).
- `harness.toml.example`: every config key with inline notes.
- `src/cli.rs`: the harness CLI surface.
