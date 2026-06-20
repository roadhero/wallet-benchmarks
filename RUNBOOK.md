# wallet-benchmarks Runbook

This runbook walks an operator through producing a canonical Esmeralda baseline against `tari-project/wallet-benchmarks#1`. The harness binary is `wallet-benchmarks`; the output is `baselines/esmeralda_canonical.json` per `analysis/RESULT_PROFILE_SCHEMA.md`. Read end-to-end before starting; the prerequisites involve four moving parts (Tari binaries, the minotari CLI, the vendored PP submodule, and three funded testnet wallets) and a partial setup wastes hours.

For background on what each mode measures, see `analysis/PR_BODY_v2.md`. For the wallet-stack quirks the harness has to work around, see §7 "Troubleshooting" below; that section mirrors `PR_BODY_v2 §4` "Wallet pain findings" with operator-facing reframing.

------

## §1. Prerequisites

### Binaries

| Component | Required version | Where it comes from |
|---|---|---|
| `minotari_node`, `minotari_console_wallet`, `tari_base_node` (and the rest of the tari_suite bundle) | `v5.4.0-pre.4` Esmeralda build | GitHub release `tari-project/tari/v5.4.0-pre.4`. Operator pre-extracts and places on `$PATH` or sets `Config::minotari_console_wallet_path` in `harness.toml`. |
| `minotari` CLI | commit `52a7287a` of `tari-project/minotari-cli` | Operator clones, `cargo build --release`, drops the resulting binary somewhere reachable (default `/usr/local/bin/minotari`). |
| `minotari_payment_processor` (PP) | commit `f0572c9` of `tari-project/minotari_payment_processor` | Vendored as a submodule at `vendor/minotari_payment_processor`. Operator builds from the submodule per §2. |
| Rust toolchain | stable, edition 2021 | The harness builds against the standard `rust-toolchain.toml` (no nightly features). |
| sqlite3 system lib | present | The harness's `rusqlite` dependency is built with `bundled` so the system sqlite isn't strictly required, but PP's `sqlx` build needs the system header on Linux. |

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

```sh
cd vendor/minotari_payment_processor
cargo build --release
ls target/release/minotari_payment_processor
cd ../..
```

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

`resolve_account_keys` requires **both or neither** of the env vars to be set. Half an override (only `TARI_BENCH_VIEW_KEY` set, or only `TARI_BENCH_SPEND_KEY` set) bails at startup with a clear error message — half-overrides are almost always a typo and silently filling the missing half from the seed would risk pairing two unrelated wallets' keys.

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
# Optional Mode 3 overrides — leave commented for the common case.
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
| `per_tx_confirmation_timeout_ms` | `1_800_000` (30 min) | Sets the upper bound on S0 / S3 confirmation polling. |
| `sampler_interval_ms` | `1_000` | Resource sampler cadence. |
| `s1_amount_per_tx_microtari` | `1000` µT | The per-tx amount S1 sends. Must exceed `fee_rate × kernel_weight` (~175 µT) for change UTXOs to be net-positive. |

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

Required when running Mode 3. The loop validates `Config::mode_3 == Some` at startup before spawning the PR or PP children.

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

The three wallets must each hold at least **11_000_000_000 µT (11k tXTM)** before the run starts. The bounty's `a_fund` parameter is 10k XTM per wallet; the extra 10% is `enforce_funding`'s headroom margin to account for fees consumed during the run.

### §4.1. Request testnet faucet funds

For each of the three addresses recorded in §2.4, request funding via the Tari Esmeralda faucet (or whichever testnet funding channel your operator setup uses). Allow a few minutes for mining and confirmation.

### §4.2. Verify each wallet sees the funds

For Mode 1 and Mode 2, verify via the console wallet's `get-balance`:

```sh
# Mode 1 (old) seed
HARNESS_SEED=$HARNESS_SEED_OLD \
  minotari_console_wallet \
    --network esmeralda \
    --base-path /tmp/verify-old \
    --password "$HARNESS_WALLET_PW" \
    --seed-words "$HARNESS_SEED_OLD" \
    get-balance
```

Repeat with `HARNESS_SEED_NEW` and `HARNESS_SEED_PP`. The output reports an `available_balance` field. Wait until each shows at least 11k tXTM equivalent (`11_000_000_000` µT) before proceeding.

Allow time for initial scan completion on each wallet. The console_wallet reports the balance via gRPC `GetState`, but the scan-and-validation step takes minutes from a cold import. The harness's own `console_wallet::wait_ready` (see §7 "Troubleshooting") gates on `has_done_initial_validation`, so a partial scan returns 0 and surfaces as `enforce_funding` failure during a run if the operator hasn't pre-confirmed funded state.

### §4.3. Mode 3: view-key wallet check

For Mode 3, the funded wallet is the same one whose view-key was extracted in §2.5. The PR daemon (spawned by the harness at run time) exposes `GET /accounts/default/balance` as the canonical balance probe. You don't need to pre-warm this; the harness's `enforce_funding` logs a warning and skips the Mode 3 arm if the PR daemon isn't yet up (the warn-and-skip behaviour from `src/guards.rs` plus `src/wallet_lifecycle/pr_balance_query.rs`).

------

## §5. Run invocation

With the environment sourced (§2.7), the binary built (§2.1), `harness.toml` written (§3), and the wallets funded (§4), run the baseline:

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
RUST_LOG=info,wallet_benchmarks=debug ./target/release/wallet-benchmarks run
```

`wallet_benchmarks=debug` surfaces the per-mode lifecycle events (wait_ready transitions, balance polling, subprocess spawn/teardown, PP HTTP probes). `info` keeps the per-tx noise readable.

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

### §7.2. Mode 1 / Mode 2 `enforce_funding` fails on a wallet you just funded

**Symptom**: The harness aborts at startup with `enforce_funding: wallet X reports 0 balance` even though you confirmed the wallet was funded in §4.2.

**Cause**: The funding pre-flight spawns a transient console_wallet per seed (Mode 1 / Mode 2 / Mode 3), polls its gRPC `GetState`, and reads `available_balance`. The wallet's gRPC reports `Online` connectivity status well before scan-and-output-validation completes. Before the bug fix in `src/wallet_lifecycle/console_wallet.rs::wait_ready`, the readiness gate returned on `Online` alone and the balance read raced an in-flight scan, returning 0.

**Resolution**: Current code gates on both `Online` AND `has_done_initial_validation == true` (per `GetStateResponse` field 4). If you still see this, the pre-flight is honouring the gate but the scan hasn't reached the funding tx height yet. Wait 5 to 10 minutes after funding before re-running, or run `minotari_console_wallet ... get-balance` (§4.2) to pre-warm.

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
