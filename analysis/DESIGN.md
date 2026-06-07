# DESIGN — wallet-benchmarks#1

> **Status:** Phase 1b complete. Awaiting human approval before Phase 2 (swe-impl).
> **Produced by:** `arch-system` + `arch-test` (parallel fan-out, 2026-05-22).
> **Workdir:** `/Users/macpro/Documents/tari-bounties/wallet-benchmarks` (branch `bounty/wallet-benchmarks-1-create-benchmarks`).
> **Companion docs:** `analysis/ANALYSIS.md`, `analysis/COMPETITION.md`, `analysis/RESULT_PROFILE_SCHEMA.md` (the v1 schema, produced first by arch-system per directive 3).
> **Re-flag check:** Both architects passed all 8 directives. **arch-security NOT spawned.**

## Decisions cross-summary (spot-check pane)

### From arch-system §System

- **Plain Rust single-crate binary, not cucumber.** Mirror SWvheerden's idioms (`anyhow` at boundaries, `log` not `tracing`, `tonic` 0.13 + `prost` 0.13, `clap` derive, edition 2021, `LOG_TARGET` consts) but not the harness shape. PR #99's value is the *Mode 2 pipeline*, which we reuse verbatim — the cucumber wrapper is orthogonal.
- **No `make-it-rain` in S5** — would muddy the throughput multiplier.
- **Mode 2 wiring = PR #99 pipeline verbatim** with only `Network::LocalNet → Network::Esmeralda`. Same `Command::new(minotari) create-unsigned-transaction --output-file ...`, same `PrepareOneSidedTransactionForSigningResult::from_json`, same `sign_locked_transaction(&key_manager, consensus_constants, Network::Esmeralda, unsigned_tx)`, same submit-via-HTTP.
- **Depend on `minotari_node_wallet_client = "5.3.1"` from crates.io** — verified published via `cargo search`. Eliminates ~100 lines of bespoke JSON-RPC + dual-shape error parsing.
- **Tari git rev `766f80ccc20596413ee208311750c11e02a2841d`** — same as `minotari_payment_processor` uses (canonical third-party pinning).
- **Invoke `minotari` CLI as subprocess only, never as a crate dependency.** Avoids migration thrash (minotari-cli#124 added schema migration 00031 the day before bounty filed).
- **Mainnet-protection guard is a hard allowlist.** Cross-checks `config.network == "esmeralda"` AND `config.base_node_url.host()` against a denylist of mainnet hosts; exits non-zero before any subprocess spawn or RPC.
- **No CI on bounty PR** — neither competitor's CI ran; AC doesn't require it; git-rev'd tari crate would require fork-PR approval gates. RUNBOOK documents the local pre-PR check.
- **Modes run sequentially**, not parallel — parallel modes would contend on the same base-node mempool and muddy S4 metrics.

### From arch-test §Test Strategy

- **Plain Rust unit + integration tests, no cucumber.** Fresh fork has no inherited test convention; CLAUDE.md §Test Placement names `<crate>/tests/<name>.rs` as the workspace default. Cucumber would add ~600 lines of `World` scaffolding for zero AC coverage gain.
- **The baseline result profile production IS the integration test.** A real end-to-end run against `https://rpc.esmeralda.tari.com` producing `baselines/esmeralda_<run-id>.json` is canonical proof for AC-3, AC-4, AC-8, AC-9, AC-10..AC-23, AC-25..AC-28, AC-34, AC-37.
- **Dual-shape broadcast deserializer fixtures** — bare-error and full envelope, derived from the 2026-05-22 live probe.
- **Negative tests for AC-30/31/32 are static code checks (code-grep), not runtime tests** — runtime tests for "harness does NOT retry" are unfalsifiable. AC-33 is the one behavioral exception.
- **Redaction is unit-tested with realistic secrets present** (BIP39 test mnemonic + programmatically-constructed dangerous values seeded into adjacent state).
- **Live-network tests are `#[ignore]`'d by default**; `cargo test` never touches the network.
- **Mock the JSON-RPC endpoint with `wiremock = "0.6"`**; subprocess mocking via `mockall = "0.13"` behind a `Spawner` trait; `tokio::time::pause()` for S4 budget-timeout determinism.
- **No tonic mock for Mode 1 gRPC** — surface is too wide (~25 methods); gRPC contract validated by live-network smoke + the committed baseline run.
- **Mainnet-protection guard is the highest-priority safety test** — sits in `tests/mainnet_guard.rs`.

## Minor cross-architect divergence (resolve before swe-impl)

- arch-system's workspace layout listed `tests/no_forbidden_patterns.rs` (one consolidated grep test); arch-test split it into `tests/no_retry_in_s4.rs`, `tests/no_utxo_partitioning.rs`, `tests/no_throttling_in_s4.rs`. **Resolve in favor of arch-test's split** — diagnostic clarity per test failure matters more than file count. swe-impl uses arch-test's layout.
- arch-system listed only 4 integration test files; arch-test enumerated ~14 (one per AC group). **Resolve in favor of arch-test's full inventory.** swe-impl creates all files arch-test specified.

---

## §System Design

### Decisions

- **Plain Rust binary, not cucumber.** The bounty repo is a fresh standalone repo, not in-tree. The 27 (mode × scenario) cells are deterministic and fixed-shape — gherkin BDD's value (human-readable scenarios that map to multiple step interpretations) does not apply here, and a single `@benchmark` feature would degenerate to one giant scenario calling 27 step macros. We mirror SWvheerden's idioms (`anyhow` at boundaries, `log` not `tracing`, `tonic` 0.13 + `prost` 0.13, `clap` derive, edition 2021, `LOG_TARGET` consts) but not the harness shape. PR #99's value to us is the *Mode 2 pipeline*, which we reuse verbatim — the cucumber wrapper around it is orthogonal.
- **No `make-it-rain` in S5.** The bounty measures three explicit modes; introducing the old wallet's built-in batch generator as a fourth comparison muddies the throughput multiplier and is silent on what the issue asked for.
- **Mode 2 wiring = PR #99 pipeline verbatim.** Mirrored from `minotari-cli:integration-tests/steps/wallet_benchmark.rs` (the `send_transactions` step): (1) `Command::new(minotari) create-unsigned-transaction --database-path … --password … --account-name default --recipient <addr::amount> --output-file <tx_i.json>` as a blocking subprocess; (2) `PrepareOneSidedTransactionForSigningResult::from_json(&unsigned_json)` to deserialize; (3) `sign_locked_transaction(&key_manager, consensus_constants.clone(), Network::Esmeralda, unsigned_tx)` in-process — `key_manager` constructed via `KeyManager::new(wallet)` where `wallet` is `WalletType` reconstructed from the harness's stored seed; `consensus_constants` from `ConsensusConstantsBuilder::new(Network::Esmeralda).build()`; (4) broadcast — `minotari_node_wallet_client::Client::submit_transaction(transaction)` (confirmed published, see Dependency strategy).
- **Dep on `minotari_node_wallet_client` 5.3.1 — published, verified.** `cargo search minotari_node_wallet_client` returned `minotari_node_wallet_client = "5.3.1"`. We depend on it directly, eliminating ~100 lines of bespoke JSON-RPC + dual-shape error parsing. PR #99's hand-rolled `reqwest::Client::post(submit_url).json(...).send()` only reads HTTP status, not the envelope — our use of the published client picks up the rejection-reason enum properly.
- **Hardware disclosure via documented per-OS source commands** (see §Environment-disclosure). We do not depend on any "system info" crate; we run the documented commands per OS and parse, so the profile field's provenance is grep-able from the source.
- **Mainnet-protection guard is a hard allowlist.** Single function `enforce_esmeralda(&Config) -> Result<()>` called from `main()` before any subprocess or RPC. Allowlist of exactly `"esmeralda"` for `config.network`. Cross-checks `config.base_node_url.host()` against a mainnet-host denylist (`rpc.tari.com`, `seeds.tari.com`). Exits non-zero before any wallet spawn or RPC.
- **Result profile is the contract, scenarios reference fields by name.** No scenario writes fields not in `RESULT_PROFILE_SCHEMA.md`. New fields require a schema edit first.
- **Modes run sequentially.** Three modes back-to-back in a single binary run. No `--mode-only` flag in v1 (kept simple; out of scope).
- **Funding is documented but external to the measurement.** RUNBOOK directs the operator to run `minotari_miner` locally pointed at `https://rpc.esmeralda.tari.com` before invoking the harness; harness verifies the wallet has `≥ a_fund` before running scenarios and exits non-zero with a clear message if it doesn't.

### Non-Goals

- No `make-it-rain` comparison in S5.
- No CI workflow on the bounty PR. Justification: (a) neither competitor's CI run succeeded; (b) the AC explicitly does not require CI; (c) a CI workflow on a repo with one published Cargo crate dep (`minotari_node_wallet_client`) and a git-rev dep (`tari` rev `766f80c...`) needs a network-allowed runner config which adds review surface unrelated to the AC. Local `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` are documented in RUNBOOK as a pre-PR checklist.
- No retry, backoff, partitioning, throttling, semaphore, sleep-between-dispatches, or serialization anywhere in scenario code. Enforced by an arch-test code-grep test.
- No mainnet support of any kind.
- No multi-OS first-class support; harness ships with Linux + macOS env-capture paths and is documented for Ubuntu 22.04 / Linux as the primary OS.
- No nightly automation, no continuous benchmark dashboard, no historical-baseline-comparison tooling.
- No L2/ootle/DAN/sidechain interaction.
- No protobuf vendoring — depend on `minotari_app_grpc` published crate.
- No path-dep or workspace-merge of `tari` or `minotari-cli` source.
- No custom seed manager, key derivation, or any code touching `*/signing/`, `*/sign.rs`, `*/keys.rs`, `*/wallet_keys/`. We invoke exactly one exported function (`sign_locked_transaction`) and reconstruct `KeyManager` exactly as PR #99 does.
- No sweep-back, fund cleanup, or fund return to faucet.
- No parallelism across modes (modes run sequentially in v1).

### Workspace layout

Fresh single-crate Rust binary at repo root. No Cargo workspace, no sub-crates (KISS, mirrors PR #3's layout choice on this axis).

```
/Users/macpro/Documents/tari-bounties/wallet-benchmarks/
├── Cargo.toml                      # populated on commit 1
├── Cargo.lock                      # committed (binary repo, not library)
├── .gitignore                      # populated on commit 1, hard-deny secrets
├── README.md                       # short, points at RUNBOOK
├── RUNBOOK.md                      # full step-by-step (AC-2 deliverable)
├── baseline_profile.json           # the AC-3 artifact, committed after live run
├── analysis/                       # ANALYSIS.md, DESIGN.md, RESULT_PROFILE_SCHEMA.md, COMPETITION.md
├── src/
│   ├── main.rs                     # clap entry, mainnet-guard, top-level orchestrator (~120 lines)
│   ├── lib.rs                      # re-exports for unit tests; small (~30 lines)
│   ├── config/
│   │   ├── mod.rs                  # Config struct, defaults from issue table, serde
│   │   └── load.rs                 # TOML loader, env override, CLI override (~120 lines)
│   ├── env_capture/
│   │   ├── mod.rs                  # Environment struct + capture()
│   │   ├── linux.rs                # /proc/cpuinfo, /proc/meminfo, lsblk parsing
│   │   └── macos.rs                # sysctl + diskutil parsing
│   ├── versions.rs                 # binary-version probing via `--version`, git rev capture
│   ├── seed/
│   │   ├── mod.rs                  # SeedHandle, env→seed, birthday rewrite (AC-24)
│   │   └── redact.rs               # RedactionRule, denylist init from env at startup
│   ├── wallet_lifecycle/
│   │   ├── mod.rs                  # WalletLifecycle trait
│   │   ├── console_wallet.rs       # spawn → poll GetState (no sleep-assume) → run → SIGTERM(grace) → SIGKILL
│   │   ├── data_dir.rs             # harness-owned tempdirs under target/harness-data/<run-id>/
│   │   └── grpc.rs                 # tonic Channel + WalletClient (minotari_app_grpc)
│   ├── modes/
│   │   ├── mod.rs                  # Mode trait
│   │   ├── old_wallet.rs           # Mode 1 — console_wallet gRPC
│   │   ├── new_wallet.rs           # Mode 2 — PR #99 pipeline (subproc + sign + submit)
│   │   └── payment_processor.rs    # Mode 3 — reuses Mode 2's pipeline with batch 1→K
│   ├── broadcast/
│   │   └── mod.rs                  # thin wrapper over minotari_node_wallet_client::Client
│   ├── scenarios/
│   │   ├── mod.rs                  # ScenarioRunner trait + dispatcher
│   │   ├── b0.rs
│   │   ├── s0.rs
│   │   ├── s1.rs
│   │   ├── s2.rs
│   │   ├── s3.rs
│   │   ├── s4.rs
│   │   ├── s5.rs
│   │   ├── s6.rs
│   │   └── s7.rs
│   ├── result_profile/
│   │   ├── mod.rs                  # ResultProfile + per-cell builders matching the schema
│   │   ├── deltas.rs               # computed deltas, AC-28
│   │   └── write.rs                # atomic write of baseline_profile.json
│   ├── metrics/
│   │   ├── mod.rs                  # 1Hz peak_rss/peak_cpu sampler (background tokio task)
│   │   └── timing.rs               # Instant-based helpers
│   ├── guards.rs                   # enforce_esmeralda()
│   └── errors.rs                   # crate-local HarnessError (anyhow at boundaries)
├── tests/
│   ├── redaction.rs                # arch-test will populate
│   ├── mainnet_guard.rs            # arch-test will populate
│   ├── no_forbidden_patterns.rs    # arch-test code-grep (no retry/backoff/sleep/etc.)
│   └── schema_roundtrip.rs         # serde roundtrip for ResultProfile vs schema
├── baselines/                      # historical alongside current baseline (just baseline_profile.json for v1; dir reserved)
└── (no .github/)                   # see Non-Goals re CI
```

Files marked "create on first commit": `Cargo.toml`, `.gitignore`, `src/main.rs` skeleton, `src/lib.rs`, `src/guards.rs`, `src/config/`, `src/seed/redact.rs`, `RUNBOOK.md` skeleton, `README.md`, `analysis/`. The rest are populated incrementally as scenarios come online.

Estimated total LOC at v1: ~3,500 lines source + ~600 lines tests. (PR #3 was 5,452 lines with stubs; we ship less code with fewer stubs.)

### Module boundaries and data flow

```
                              ┌────────────────────────────────────────────────┐
                              │                 src/main.rs                    │
                              │  clap → Config → enforce_esmeralda() → run()   │
                              └──────────────────────┬─────────────────────────┘
                                                     │
              ┌──────────────────────────────────────┼──────────────────────────────────────┐
              │                                      │                                      │
              ▼                                      ▼                                      ▼
   ┌──────────────────┐                  ┌────────────────────────┐                ┌──────────────────┐
   │ env_capture::    │                  │  versions::probe()     │                │ seed::redact::   │
   │ capture()        │                  │  (binaries --version,  │                │ init_denylist()  │
   │ (Linux/macOS)    │                  │   git rev for harness) │                │ (from env vars)  │
   └────────┬─────────┘                  └────────────┬───────────┘                └────────┬─────────┘
            │                                         │                                     │
            └──────────────────────┬──────────────────┴─────────────────────────────────────┘
                                   │
                                   ▼
                       ┌────────────────────────┐
                       │  ResultProfile (in     │
                       │  memory, schema v1)    │
                       └───────────┬────────────┘
                                   │
       ┌───────────────────────────┴───────────────────────────┐
       │ For each Mode in [old_wallet, new_wallet, pp]:        │
       │   modes::<mode>::run(&Config, &SeedHandle, &mut       │
       │     ModeReport)                                       │
       └───────────────────────────┬───────────────────────────┘
                                   │
                                   ▼
                ┌───────────────────────────────────┐
                │   Mode trait impl                 │
                │   - spawn / connect               │
                │   - WalletLifecycle: spawn,       │
                │     wait_ready (poll GetState),   │
                │     run, terminate (SIGTERM)      │
                │   - hand off to ScenarioRunner    │
                └─────────────────┬─────────────────┘
                                  │
       ┌──────────────────────────┴─────────────────────────────┐
       │  For each scenario in [B0,S0,S1,S2,S3,S4,S5,S6,S7]:    │
       │    scenarios::<id>::run(&mut ModeContext) → CellResult │
       └──────────────────────────┬─────────────────────────────┘
                                  │
              ┌───────────────────┼─────────────────────┐
              ▼                   ▼                     ▼
   ┌──────────────────┐ ┌────────────────────┐ ┌─────────────────────┐
   │ broadcast::      │ │ wallet_lifecycle:: │ │ Mode 2/3 only:      │
   │ submit_tx()      │ │ data_dir wipe      │ │ Subprocess          │
   │ (HTTP, port 443) │ │ + birthday rewrite │ │ minotari            │
   │ via              │ │ + re-import        │ │ create-unsigned-    │
   │ minotari_node_   │ │ (AC-15/16/22/23/   │ │ transaction         │
   │ wallet_client    │ │  24/34)            │ │ + sign_locked_      │
   │ ::Client         │ │                    │ │ transaction (in-    │
   └────────┬─────────┘ └────────────────────┘ │ proc)               │
            │                                  └──────────┬──────────┘
            │   ┌─────────────────────────────────────────┘
            ▼   ▼
   ┌────────────────────────┐
   │  metrics::sampler      │  (background tokio task,
   │  1Hz peak_rss/peak_cpu │  cancelled at end of cell)
   └───────────┬────────────┘
               │
               ▼
   ┌────────────────────────────────┐
   │ ResultProfile::push_cell(...)  │
   └───────────┬────────────────────┘
               │
               ▼  (after all 27 cells)
   ┌────────────────────────────────┐
   │ result_profile::deltas::       │
   │ compute(&mut profile)          │
   └───────────┬────────────────────┘
               │
               ▼
   ┌────────────────────────────────┐
   │ result_profile::write::write   │
   │ (baseline_profile.json,        │
   │  atomic via tempfile + rename) │
   └────────────────────────────────┘
```

**Shared across modes:** `config`, `env_capture`, `versions`, `seed`, `wallet_lifecycle::data_dir`, `broadcast` (Mode 2/3 only — Mode 1 uses gRPC `Transfer`), `metrics`, `result_profile`, `scenarios/*` (each scenario is mode-agnostic — it calls `mode.send(...)`, `mode.scan(...)`, etc., through the `Mode` trait).

**Mode-specific:** the `Mode` trait implementations. Each implements `send_single(...)`, `send_batch_one_to_many(...)`, `scan_from_birthday(...)`, `get_balance()`, `get_utxo_count()`, `wipe_and_reimport(birthday)`. Mode 1 fulfils them by tonic calls; Mode 2 by `minotari` subprocess + in-process signing + HTTP submit; Mode 3 by Mode 2 with `--recipient` repeated K times.

### Dependency strategy

Decisions per crate:

- `minotari_app_grpc = "5.3.1"` from crates.io, `default-features = false` (skips the binary build, keeps tonic client). Used in Mode 1 only.
- `minotari_node_wallet_client = "5.3.1"` from crates.io. **Verified published today** (`cargo search`). Used in Mode 2 and Mode 3 for `submit_transaction`. Eliminates dual-shape JSON-RPC reimpl.
- `tari_transaction_components` — git rev to align with `minotari-cli`'s expected host (matches PR #99). Provides `sign_locked_transaction`, `KeyManager`, `ConsensusConstantsBuilder`, `PrepareOneSidedTransactionForSigningResult`, `TransactionResult`.
- `tari_common` — git rev — provides `Network::Esmeralda`.
- `tari_common_types` — git rev — provides `TariAddress`, `TariAddressFeatures`, `CipherSeed`, `seeds::cipher_seed::change_birthday`.
- `tari_script`, `tari_sidechain` — not needed by the harness directly; transitive.
- `tari_crypto = "0.22.1"` from crates.io (matches `minotari_payment_processor`'s pin). Borsh feature.
- `tari_utilities = "0.8"` from crates.io (matches `minotari_payment_processor`).
- `minotari` (the CLI from minotari-cli) — **invoked as subprocess only, NOT crate-depended.** No `minotari` in `[dependencies]`. Operator builds it via documented `cargo build --release -p minotari --git ...` in RUNBOOK, and `harness.toml` records the resulting binary path. This eliminates the migration-thrash risk PR #3 stepped into (PR #124 added migration 00031 the day before the bounty filed — schema would silently rev under us).
- Pinning rev: `766f80ccc20596413ee208311750c11e02a2841d` — same as `minotari_payment_processor` uses; we match canonical third-party pinning. (If by build time `minotari-cli`'s expected host has moved, we re-pin the tari rev consistently with `minotari-cli`'s commit shown in `versions.minotari_cli.commit`.)

Literal `Cargo.toml` block:

```toml
[package]
name = "wallet-benchmarks"
version = "0.1.0"
edition = "2021"
publish = false

[[bin]]
name = "wallet-benchmarks"
path = "src/main.rs"

[dependencies]
# Async runtime + IO
tokio       = { version = "1.47", features = ["full"] }
reqwest     = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
url         = { version = "2.5", features = ["serde"] }

# CLI / config / logging (mirrors maintainer style)
clap        = { version = "4", features = ["derive", "env"] }
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
toml        = "0.8"
log         = "0.4"
env_logger  = "0.11"
anyhow      = "1"
thiserror   = "1"
tempfile    = "3"
chrono      = { version = "0.4", features = ["serde"] }
rand        = "0.8"
regex       = "1"
sha2        = "0.10"
hex         = "0.4"

# Tari ecosystem — Mode 1 (gRPC) and Mode 2/3 (offline sign + HTTP submit)
minotari_app_grpc         = { version = "5.3.1", default-features = false }
minotari_node_wallet_client = "5.3.1"
tonic       = { version = "0.13", features = ["transport"] }
prost       = "0.13"

tari_common               = { git = "https://github.com/tari-project/tari/", rev = "766f80ccc20596413ee208311750c11e02a2841d" }
tari_common_types         = { git = "https://github.com/tari-project/tari/", rev = "766f80ccc20596413ee208311750c11e02a2841d" }
tari_transaction_components = { git = "https://github.com/tari-project/tari/", rev = "766f80ccc20596413ee208311750c11e02a2841d" }
tari_crypto    = { version = "0.22.1", features = ["borsh"] }
tari_utilities = "0.8"

[dev-dependencies]
proptest = "1"
walkdir  = "2"
```

### Mode 1 (Old Wallet) — concrete wiring

1. **Spawn lifecycle.** `wallet_lifecycle::console_wallet::spawn(&Config, &SeedHandle, &TempDataDir) -> WalletHandle`. Builds the command:
   ```
   minotari_console_wallet
     --network esmeralda
     --base-path <tempdir>
     --password $HARNESS_WALLET_PW  (passed via env, never on argv)
     --seed-words-file <tempdir>/seed.txt
     --non-interactive-mode
     --grpc-address /ip4/127.0.0.1/tcp/<dynamic-port>
   ```
   Dynamic port chosen by binding a `TcpListener` and dropping it, recording the port (race-tolerant — the tonic Channel below uses an exponential backoff *on connect* only, which is allowed; this is not retry on submit).
2. **Wait for ready.** Poll gRPC `GetState` every 1s until response `is_synced == true` AND `scanned_height == base_node_tip` (the latter cross-checked against `https://rpc.esmeralda.tari.com/get_tip_info`). No max retries — generous-but-bounded under `per_tx_confirmation_timeout_ms`. **No sleep-then-assume.**
3. **gRPC client.** `minotari_app_grpc::tari_rpc::wallet_client::WalletClient::connect(channel).await`. Methods used per scenario:
   - `GetVersion` (versions block)
   - `GetState` (ready + tip tracking)
   - `GetAddress` (S0 funding verification)
   - `GetBalance` (every scenario's `balance_before` / `balance_after`)
   - `Transfer` (S0, S1 doubling rounds, S1 fan-out, S4 (called concurrently via `tokio::JoinSet`), S5 individual arm, S5 batch arm with `single_tx = true` per maintainer comment 2026-06-05)
   - `GetCompletedTransactions` + `GetTransactionInfo` (confirmation polling, txid → status mapping)
   - For S5 batch arm in Mode 1: runs via `Transfer` with `single_tx = true` and K recipients per call. Per the wallet.proto:578 doc: "SingleTx is used to indicate should this be sent as a single MW tx or multiple, one tx per recipient." With `single_tx = true` the wallet builds a single 1→K Mimblewimble transaction. Per @SWvheerden 2026-06-05.
4. **Tear down.** `SIGTERM` → wait up to 10s for exit → `SIGKILL` → wait on PID. Wrapped in a `Drop` impl on `WalletHandle` so panics and Ctrl-C don't leak the process. Tempdir under `target/harness-data/<run-id>/old_wallet/` removed on graceful shutdown; left in place if harness panicked (operator can diagnose).
5. **Knobs.** `network = "esmeralda"` (literal), `data_dir = <tempdir>`, password from `$HARNESS_WALLET_PW`, base-node URL from `config.base_node_url`. No `make-it-rain`.

### Mode 2 (New Wallet) — concrete wiring

Verbatim mirror of `minotari-cli:integration-tests/steps/wallet_benchmark.rs::send_transactions`, with the substitutions: `LocalNet → Esmeralda`, hand-rolled `http_client.post(submit_url).json(req).send()` → `minotari_node_wallet_client::Client::submit_transaction()`.

1. **Subprocess: create unsigned transaction.**
   ```rust
   let output_path = tempdir.join(format!("tx_{}.json", tx_idx));
   let status = Command::new(&config.minotari_binary_path)
       .arg("create-unsigned-transaction")
       .args(["--database-path", db_path.to_str().unwrap()])
       .args(["--password", &wallet_password])      // from env via SeedHandle
       .args(["--account-name", "default"])
       .args(["--recipient", &format!("{}::{}", recipient_addr_b58, amount_microtari)])
       .args(["--output-file", output_path.to_str().unwrap()])
       .env_clear()
       .env("HOME", &harness_home)                  // controlled $HOME to prevent ~/.tari pollution
       .env("TARI_NETWORK", "esmeralda")
       .output()?;
   if !status.status.success() { /* push errors.details, status = "failure" */ }
   ```
2. **Parse unsigned tx.**
   ```rust
   use tari_transaction_components::offline_signing::models::PrepareOneSidedTransactionForSigningResult;
   use tari_transaction_components::offline_signing::models::TransactionResult; // brings from_json
   let unsigned_json = std::fs::read_to_string(&output_path)?;
   let unsigned: PrepareOneSidedTransactionForSigningResult =
       PrepareOneSidedTransactionForSigningResult::from_json(&unsigned_json)?;
   ```
3. **KeyManager + consensus.**
   ```rust
   use tari_transaction_components::consensus::ConsensusConstantsBuilder;
   use tari_transaction_components::key_manager::KeyManager;
   use tari_transaction_components::key_manager::wallet_types::WalletType;
   use tari_common::configuration::Network;

   // wallet reconstructed from stored seed loaded from $HARNESS_SEED_NEW (never written to disk except
   // the controlled --seed-words-file used for the `minotari` subprocess)
   let wallet: WalletType = WalletType::from_mnemonic(&seed_handle.mnemonic_new(), None)?;
   let key_manager = KeyManager::new(wallet)?;
   let consensus_constants = ConsensusConstantsBuilder::new(Network::Esmeralda).build();
   ```
   `KeyManager`, `WalletType`, and `from_mnemonic` are all existing exported APIs of `tari_transaction_components`. We do not write a key-derivation helper and we do not touch `*/signing/*.rs`.
4. **In-process offline signing.**
   ```rust
   use tari_transaction_components::offline_signing::sign_locked_transaction;
   let signed = sign_locked_transaction(
       &key_manager,
       consensus_constants.clone(),
       Network::Esmeralda,           // <-- the only delta vs PR #99
       unsigned,
   )?;
   let tx = signed.signed_transaction.transaction;
   ```
5. **Broadcast via `minotari_node_wallet_client::Client`.**
   ```rust
   use minotari_node_wallet_client::Client as BaseNodeClient;
   let client = BaseNodeClient::new(
       config.base_node_url.clone(),     // https://rpc.esmeralda.tari.com
       config.base_node_url.clone(),     // default_seed_address (same in our setup)
   );
   let resp = client.submit_transaction(tx).await?;  // returns TxSubmissionResponse
   // resp.accepted, resp.rejection_reason, resp.is_synced, resp.details
   ```
   Dual-shape parsing (the bare `{"error": ...}` form vs envelope) is the published client's responsibility. If the client surfaces only `anyhow::Error` for the bare-error shape, we capture its string into `errors.details[].error_string`.
6. **Birthday rewrite (AC-24).** For B0, S2, S6 (Mode 2): before scan, harness wipes the data dir, calls `CipherSeed::from_mnemonic(&mnemonic, None)` (from `tari_common_types::seeds::cipher_seed`), then `cipher_seed.change_birthday(0)`, then re-encodes to mnemonic with `to_mnemonic()`, writes to a fresh `seed.txt` in the wiped data dir, and re-imports via `minotari import-seed --database-path <new-dir> --seed-words-file seed.txt --password …`. The new-dir mnemonic file lives only inside the tempdir and is removed on tempdir cleanup. For S3, S7: same flow but `change_birthday(h_birth_days)` where `h_birth_days = h_birth_block_to_days(s0.h_birth)`.
7. **No external `minotari_console_wallet` in Mode 2's code path** — satisfies AC-6. The `minotari` binary is the only subprocess and is used solely for `create-unsigned-transaction`, `import-seed`, `scan`, `get-balance` (read-side); signing is in-process.

### Mode 3 (Payment Processor) — concrete wiring

Mode 3 IS Mode 2 with one knob: `create-unsigned-transaction` is invoked with multiple `--recipient addr1::amt1 --recipient addr2::amt2 …` flags (verified by reading `minotari-cli:minotari/src/cli.rs` during implementation; if the binary doesn't accept repeated `--recipient`, fall back to `--recipients-file <path>` with a JSON array — schema TBD at implementation time but documented as a single confirmed command before `swe-impl` writes Mode 3). Steps 2-5 above are unchanged: one unsigned-tx JSON → `sign_locked_transaction` → `Client::submit_transaction`.

For S5 batch arm in Mode 3: K=10 recipients per tx, 10 batch txs (per AC-19). For S5 individual arm in Mode 3 (per ambiguity #3 working interpretation): M=100 single-recipient txs, marked `context-only` in profile.

### Scenario state machine

Per-scenario short spec. All write into the cell-envelope from §4 of the schema; specific fields cross-referenced.

**B0** — Preconditions: wallet imported, birthday set to 0, data dir wiped. Steps: (1) record `h_tip_start` via base-node `/get_tip_info`; (2) start 1Hz metrics sampler; (3) invoke `mode.scan_from_birthday(0)` synchronously; (4) on return, record `t_scan_ms`, `h_tip_end`, peak metrics; (5) call `mode.get_utxo_count()`, `mode.get_balance()`, `mode.outputs_found()`; assert all = 0. Verification: AC-10. Result fields: `b0.t_scan_ms`, `blocks_per_sec`, `h_tip_start`, `h_tip_end`, `peak_rss_bytes`, `peak_cpu_pct`, `utxo_count_verified`, `balance_verified_microtari`, `outputs_found`. Failure-halt: a failing scan does not halt the run — it records `status = "failure"` and we move to S0 (next scenario gets a fresh wipe anyway).

**S0** — Preconditions: B0 complete, wallet still imported. Steps: (1) pre-check wallet `get_balance() ≥ a_fund` (else exit with funding-required message); (2) construct & broadcast a single 1-recipient self-tx using `mode.send_single(addr, a_fund_minus_fee)` — recipient is a freshly-derived address on the same seed; (3) time `t_broadcast_to_mempool_ms` = from `submit_transaction` call return to first `is_in_mempool == true` poll-hit; (4) time `t_broadcast_to_confirmed_ms` = from same start to depth ≥ `c_min`; (5) record `h_birth = block_height_of_first_confirmation`. Verification: utxo_count == 1, balance == a_fund (within fee tolerance). Failure-halt: yes — S0 must succeed for S1 to have funds; `status = "failure"` → subsequent S1-S7 cells emitted with `status = "halted"` and `note = "S0 failed"`.

**S1** — Preconditions: S0 completed. Steps: for round in [1,2,4,8,16,32] (doubling, outputs_per_tx = 2) then [64] (fan-out, outputs_per_tx = 8): (a) record `pre_balance`; (b) construct + broadcast `tx_count_target` txs sequentially (no concurrency in S1); each tx is 1-in/N-out using `mode.send_to_self_n_outputs(N)`; (c) wait for all txs in the round to reach confirmation depth `c_min`; (d) record `post_balance`, `round_fees`, `reconciliation_delta`; (e) if any tx in round failed, set `halted_at_round = round_name`, break out of loop. After loop: record `final_utxo_count = mode.get_utxo_count()`. Verification: AC-12, AC-13, AC-14. Failure-halt: yes — round-level (AC-13). No retry, no skip-and-continue.

**S2** — Preconditions: S1 completed (`status = "success"`). Steps: (1) `mode.terminate_then_wipe_data_dir()`; (2) `mode.set_birthday(0)`; (3) `mode.import()`; (4) run B0-shaped scan; (5) record `outputs_found = mode.get_utxo_count()` (expected 512); (6) call `mode.get_transaction_history()` and assert every txid from S1 rounds is present, set `s1_txids_history_verified`. Verification: AC-15, AC-24, AC-34. Failure-halt: no — record `status = "failure"` and continue (S3 can still run).

**S3** — Preconditions: S2 completed. Steps: same as S2 but `mode.set_birthday(h_birth_from_s0)` and record `blocks_scanned = h_tip_end - h_birth_block_height`. Verification: AC-16.

**S4** — Preconditions: S3 completed. Steps: for `n in [8, 16, 32, 64, 128]`:
  ```rust
  let mut joins = tokio::task::JoinSet::new();
  let dispatch_start = Instant::now();
  for _ in 0..n {
      let m = mode.clone_handle();
      joins.spawn(async move {
          // dispatch in parallel — NO semaphore, NO sleep, NO throttle
          let tx_record = m.send_single(recipient, amount).await;
          tx_record
      });
  }
  let deadline = dispatch_start + Duration::from_millis(s4_t_budget_ms);
  let mut records = Vec::with_capacity(n as usize);
  while let Some(joined) = tokio::select! {
      r = joins.join_next() => r,
      _ = tokio::time::sleep_until(deadline.into()) => None,
  } { records.push(joined?); }
  let budget_elapsed = Instant::now() >= deadline && !joins.is_empty();
  joins.abort_all();   // cancellation is harness teardown, NOT retry — confirmed AC-30/31/32 compliant
  ```
  AC compliance: zero retry/backoff (AC-30), no UTXO pre-partitioning — selection is delegated to `mode` (AC-31), no semaphore/sleep/serialization (AC-32). `max_serialization_gap_ms` computed across `tx_records[].t_construct_complete_ms` timestamps; `double_selection_rejections` counted from `tx_records[].rejection_reason == "ValidationFailed"` with the "duplicate input" substring (recorded raw, we don't fix the rejection). Failure-halt: no — every N runs even if a prior N had 0% success.

**S5** — Preconditions: S4 completed. Steps: (1) record `pre_state_utxos`, `pre_state_balance` (AC-21, do NOT normalize); (2) generate a deterministic 100-recipient list (seeded RNG from `run_id` so the same list runs across all three modes), hash with sha256 and record as `recipient_list_hash`; (3) per mode-arm matrix (per AC-20 + ambiguity #3): old_wallet runs Individual arm; new_wallet runs Individual arm; payment_processor runs Batch arm AND Individual arm (latter context-only); (4) Batch arm: 10 txs each 1→K=10, recipients[i*K..i*K+K]; (5) Individual arm: 100 single-recipient txs using the same list. Both arms: record `t_total_ms`, `fees_total_microtari`, `fee_per_recipient_microtari = fees_total / 100`, `blocks_consumed = h_tip_end - h_tip_start`, `tx_success_count`, `tx_failure_count`. Compute `throughput_multiplier` within-mode if both arms ran (PP only); cross-mode multipliers go into `deltas`.

**S6** — Same shape as S2; runs after S5; record deltas `delta_vs_s2_ms`, `delta_vs_b0_ratio`. AC-22.

**S7** — Same shape as S3; runs after S6; birthday = `h_birth`. AC-23.

### Mainnet-protection guard

File: `src/guards.rs`.

```rust
pub fn enforce_esmeralda(config: &Config) -> anyhow::Result<()> {
    const ALLOWLIST_NETWORK: &str = "esmeralda";
    const MAINNET_HOST_DENYLIST: &[&str] = &[
        "rpc.tari.com", "seeds.tari.com",
        "mainnet.tari.com", "mainnet-rpc.tari.com",
    ];
    if config.network != ALLOWLIST_NETWORK {
        anyhow::bail!("network={} but only 'esmeralda' is allowed", config.network);
    }
    let host = config.base_node_url.host_str().unwrap_or("");
    if MAINNET_HOST_DENYLIST.iter().any(|d| host.contains(d)) {
        anyhow::bail!("base_node_url host '{}' on mainnet denylist", host);
    }
    // Cross-check the network passed to wallet binaries on each spawn (defense in depth):
    //   wallet_lifecycle::console_wallet::spawn() asserts the same.
    Ok(())
}
```

Unit test contract (for arch-test to populate): given `Config { network = "mainnet", … }` → `Err`; given `Config { network = "esmeralda", base_node_url = "https://rpc.tari.com/..." }` → `Err`; given the all-esmeralda happy path → `Ok(())`. Test file: `tests/mainnet_guard.rs`.

### Secret handling

- **Env vars (required at runtime, never in `harness.toml`):**
  - `HARNESS_SEED_OLD` — old-wallet mode seed (24 words space-separated)
  - `HARNESS_SEED_NEW` — new-wallet mode seed
  - `HARNESS_SEED_PP` — payment-processor mode seed
  - `HARNESS_WALLET_PW` — wallet passphrase (same for all three modes for simplicity)
- **CLI flag for seeds-file path:** `--seeds-file <path>` (optional alt) — file is TOML with keys `seed_old`, `seed_new`, `seed_pp`, `wallet_pw`. File path itself must be outside the repo root (asserted at startup); harness refuses paths under the repo. File is never logged.
- **Pipeline to `minotari --import-seed`:** seed written to a tempfile inside `target/harness-data/<run-id>/<mode>/seed.txt`, mode 0600, passed via `--seed-words-file`; tempfile deleted on data-dir cleanup. `--password` is passed via env var to the subprocess (`env_clear()` then `env("HARNESS_WALLET_PW", ...)`), not on argv where it would show in `ps`. (If the minotari CLI requires `--password` on argv, we accept that and document it; argv leakage is bounded to the subprocess run, not the harness.)
- **`.gitignore` from commit 1:**
  ```
  /target/
  /baselines/*.json   # except baseline_profile.json which is committed
  !baseline_profile.json
  /target/harness-data/
  .env
  .env.*
  *.seed
  seeds/
  seeds.toml
  wallets/
  *.tari/
  .tari/
  /Users/
  /home/
  /tmp/wallet-benchmarks-*
  ```
- **Result-profile redaction:** §6 of schema. Initialised at startup with env-var values; arch-test will write a unit test that loads a fixture profile (with redaction triggers seeded via env), serialises it, and asserts no rule matches.

### Funding path

Documented in `RUNBOOK.md`, not in measurement scope:

1. Build `minotari_miner` from `tari-project/tari` at the pinned rev:
   ```
   cargo build --release -p minotari_miner --git https://github.com/tari-project/tari --rev <recorded rev>
   ```
2. Pick one of the three mode seeds (start with `HARNESS_SEED_OLD`), recover the wallet, copy the wallet address (`minotari_console_wallet --command 'whoami' --non-interactive-mode`).
3. Point the miner at Esmeralda and mine into that address until balance ≥ `a_fund + headroom` (recommend `a_fund × 1.5`):
   ```
   minotari_miner --address <wallet-address> --base-node-url https://rpc.esmeralda.tari.com --network esmeralda
   ```
4. Repeat for `HARNESS_SEED_NEW` and `HARNESS_SEED_PP`. Three separate funding rounds, one per seed (AC-35).
5. Run the harness. Funding tx fees and timings are NOT in the result profile (the funding wallet never appears as a mode).

If the maintainer recommends a different funding path before our PR submission (issue thread continues), RUNBOOK is updated and `versions` / `environment` recorded against the documented path. Provisional path is the default.

### Environment-disclosure

Per-OS source command for each AC-26 field, captured at the start of every run by `src/env_capture/{linux.rs, macos.rs}`:

| Field | Linux | macOS |
|---|---|---|
| `cpu_model` | `grep "model name" /proc/cpuinfo | head -1` (parse after `: `) | `sysctl -n machdep.cpu.brand_string` |
| `ram_bytes` | `grep MemTotal /proc/meminfo` (kB) × 1024 | `sysctl -n hw.memsize` |
| `disk_type` | `lsblk -d -o name,rota` (parse the root device's rota; 0 → ssd, 1 → hdd; refine with `cat /sys/block/<dev>/queue/rotational`); `nvme` substring in device name → `nvme-ssd` | `diskutil info /` (parse "Solid State: Yes" → ssd; "Protocol: PCI-Express" → nvme-ssd) |
| `os` | `uname -srm` | `uname -srm` |
| `network_path_to_base_node` | URL-derived: parse `config.base_node_url.host()`. `127.0.0.1`, `::1`, `localhost` → `"local"`; anything else → `"remote"`. | same |

Failures in capture (e.g. running in a sandboxed CI with `/proc` unavailable) fall back to `"unknown"` for that one field with a `note` at the environment level — but the harness still proceeds. No env field is allowed to be `null`.

### CI strategy

**Skip CI on the bounty PR.** Justifications:
1. The AC doesn't require it; SWvheerden didn't trigger CI on either competitor PR.
2. The repo depends on a git-rev'd tari crate, which means CI needs network egress to GitHub; fork-PR CI on GitHub Actions has a per-org approval gate that adds review surface unrelated to the AC.
3. The harness's headline value (real numbers in `baseline_profile.json`) is established by the committed JSON, not by CI green.

Instead, RUNBOOK documents a "before-PR" local check:
```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```
This block is grep-able by reviewers and matches `tari` / `minotari-cli` workspace expectations.

If the maintainer asks for CI in review, we add a minimal `.github/workflows/check.yml` running the four commands above on Ubuntu 22.04 — but not in v1.

### What's intentionally NOT in scope

- All items from ANALYSIS.md §Out-of-scope (mainnet, multi-OS guarantees, dashboards, sweep-back, custom protos, CI, performance assertions, retry/backoff/partitioning/throttling).
- **Modes run sequentially, not parallel across modes.** A v2 could parallelise but the AC says nothing about it and parallel modes contend for the same base-node mempool, which would muddy S4 metrics.
- **No comparison against a previous baseline.** `baselines/` directory is reserved but holds only `baseline_profile.json` (alias at repo root, same content). Historical compares are out of scope.
- **No Python or shell orchestrator.** Maintainer-mirror weight favors Rust (every SWvheerden merge across both repos is pure Rust).
- **No webhook, dashboard, or push-to-S3 emission of results.** Result is a file on disk.
- **No coverage of L2 / Ootle / sidechain operations.**
- **No view-key-only wallet path.** Modes 2/3 require full seed because `sign_locked_transaction` needs spend-key access via `KeyManager`.
- **No `minotari_console_wallet` spawn in Mode 2.** AC-6 verification.

### Re-flag check

Walking through directives 1–8:

1. **No cucumber.** Plain Rust binary, scenario fns in `src/scenarios/`. Justified in Decisions. **Pass.**
2. **No `make-it-rain`.** Listed in Non-Goals. **Pass.**
3. **`RESULT_PROFILE_SCHEMA.md` is the first artifact** — produced above before §System. **Pass.**
4. **Mode 2 wiring mirrors PR #99 exactly** — same `Command::new(minotari) create-unsigned-transaction --output-file ...`, same `PrepareOneSidedTransactionForSigningResult::from_json`, same `sign_locked_transaction(&key_manager, consensus_constants, Network::Esmeralda, unsigned_tx)`, same submit-via-HTTP. The only delta is `Network::LocalNet → Network::Esmeralda` (required by AC). No fourth signing path. No `minotari_console_wallet` in Mode 2. **Pass.**
5. **Broadcast dual-shape** — handled by depending on `minotari_node_wallet_client = "5.3.1"` (verified published via `cargo search`); reimplementation not needed. **Pass.**
6. **No new code under `*/signing/`**, `*/sign.rs`, `*/keys.rs`, `*/wallet_keys/`. No path-dep or submodule into `tari`/`minotari-cli`. No retry/backoff/pre-partitioning/throttling. No mainnet support. No private signing internals — only the exported `sign_locked_transaction`. **Pass — no REFLAG-ARCH-SECURITY heading needed.**
7. **Redaction denylist** has 10 concrete regex/substring rules in §6 of the schema; arch-test will write the `serde_json::to_string(&profile)` test against this list. **Pass.**
8. **Hardware/environment per-OS source commands** documented in §Environment-disclosure. **Pass.**

No directive triggers escalation. Design proceeds to arch-test (Phase 1b parallel) for the §Test section, then merged into `DESIGN.md`.

## §Test Strategy

### Decisions

- **Plain Rust unit + integration tests, no cucumber.** Deviation from `minotari-cli#99` is intentional: this repo is a fresh fork with no inherited test convention, and CLAUDE.md §Test Placement names `<crate>/tests/<name>.rs` as the workspace default. Cucumber would require checking in a parallel `integration-tests/` crate with `World` state plumbing that adds ~600 lines of scaffolding for zero AC coverage gain — the AC contract is "harness produces baseline profile", not "harness expresses scenarios in Gherkin".
- **The baseline result profile production IS the integration test.** A real end-to-end run against `https://rpc.esmeralda.tari.com` producing `baselines/esmeralda_<run-id>.json` is canonical proof for AC-3, AC-4, AC-8, AC-9, AC-10..AC-23, AC-25..AC-28, AC-34, AC-37 — the file is the test output, the committed file is the test artifact, the schema-validation test re-verifies it on every CI run.
- **Dual-shape broadcast deserializer fixtures.** Two unit-test fixtures derived from the 2026-05-22 live probe (bare-error string and full JSON-RPC envelope) drive the deserializer; both real shapes must round-trip without panic.
- **Negative tests for AC-30/31/32/33 are static code checks (code-grep), not runtime tests.** Runtime tests for "the harness does NOT retry" are unfalsifiable in finite time; a compile-time / `cargo test` grep across `src/scenarios/s4.rs` (and friends) for forbidden tokens is deterministic and reviewable. AC-33 is the one exception — that one is behavioral.
- **Redaction is unit-tested with realistic secrets present.** Per directive 7, the redaction test constructs a profile carrying every dangerous value in adjacent state, then asserts serialization output contains none of them.
- **Live-network tests are `#[ignore]`'d by default.** Single `cargo test` run never touches the network. Live tests run under `cargo test -- --ignored` and a `live-network` feature flag — explicit operator opt-in, deterministic CI without flake.
- **Mock the JSON-RPC endpoint with `wiremock 0.6`.** Already idiomatic in Rust ecosystem, no global state, async-tokio native. Mode 1 console-wallet lifecycle uses a `tempfile::TempDir` for data-dir and a `MockChild` shim trait, not a real `tonic` mock server — Mode 1's gRPC surface is too wide for cost-effective mocking, so Mode 1 unit tests cover the lifecycle state machine only; gRPC contract is validated by live-network smoke test.
- **Mainnet-protection guard is the highest-priority safety test.** Sits in `tests/mainnet_guard.rs` as a behavioral integration test asserting that any non-Esmeralda network identifier causes `Harness::new()` to error before any subprocess spawns.

### Test inventory — by AC

| AC ID | What we test | Where it lives | Type | Fixture / Mock |
|-------|--------------|----------------|------|----------------|
| AC-1 | `cargo build --release` exits 0 from a clean clone with documented commands only. | `tests/build_smoke.rs` (asserts `Cargo.lock` exists, workspace members enumerated) + CI job `build` | integration + CI | none |
| AC-2 | `RUNBOOK.md` exists and contains every section named in the runbook contract (`# Prerequisites`, `# Funding`, `# Configuration`, `# Run`, `# Output`). | `tests/runbook_completeness.rs` — parses RUNBOOK.md, asserts headings present | integration | RUNBOOK.md (the doc under test) |
| AC-3 | `baselines/esmeralda_canonical.json` exists, parses as the schema, contains non-stub numeric values (sanity floor: every `T_*` > 0, every `H_*` > 0). | `tests/baseline_committed.rs` + `tests/result_profile_schema.rs` | integration | `baselines/esmeralda_canonical.json` (the committed real-run output) |
| AC-4 | A single `harness run --config <toml>` invocation produces a complete profile end-to-end. | `RUNBOOK.md` smoke procedure + `tests/single_invocation.rs` (asserts CLI has no `--scenario` / `--mode` required flags — i.e. defaults run everything) | integration + manual smoke | none |
| AC-5 | `Mode1OldWallet` spawns `minotari_console_wallet`, polls gRPC `GetState`, tears down on drop. | `src/modes/mode1/lifecycle.rs` `#[cfg(test)] mod tests` | unit | `MockChild` trait + `tempfile::TempDir` |
| AC-6 | `Mode2NewWallet` does NOT contain any reference to `minotari_console_wallet` in its code path. | `tests/mode2_no_console_wallet.rs` (grep) | static-check | source files |
| AC-7 | `Mode3PaymentProcessor` invokes the batch 1-to-many code path (single `Transaction` with K outputs), not a loop of single-output sends. | `src/modes/mode3.rs` `#[cfg(test)] mod tests` | unit | mocked transaction builder |
| AC-8 | Profile has 27 (mode, scenario) cells, each non-null. | `tests/result_profile_schema.rs::test_27_cells_present` | integration | `fixtures/result_profile_schema_example.json` + the real `baselines/esmeralda_canonical.json` |
| AC-9 | Every scenario block has the 5 cross-cutting metric fields populated. | `tests/result_profile_schema.rs::test_cross_cutting_metrics` | integration | same |
| AC-10 | B0 block has T_scan, blocks_per_sec, H_tip_start/end, peak_rss, peak_cpu, utxo_count==0, balance==0, outputs_found==0. | `tests/result_profile_schema.rs::test_b0_shape` + `src/scenarios/b0.rs` `#[cfg(test)] mod tests` | unit + integration | fixture + real baseline |
| AC-11 | S0 block has the four timing metrics; utxo_count==1, balance==A_fund. | `tests/result_profile_schema.rs::test_s0_shape` | integration | fixture |
| AC-12 | S1 has 6 doubling rounds + 1 fan-out round; final utxo_count==512. | `tests/result_profile_schema.rs::test_s1_shape` + `src/scenarios/s1.rs` `#[cfg(test)] mod tests` for round arithmetic | unit + integration | fixture |
| AC-13 | S1 halts on round failure — subsequent rounds absent and marked `halted`. | `src/scenarios/s1.rs::tests::halts_on_round_failure` | unit | mocked wallet driver that fails round 3 |
| AC-14 | S1 reconciles `balance_after == balance_before − Σ round_fees` per round, records delta. | `src/scenarios/s1.rs::tests::reconciliation_per_round` | unit | mocked wallet driver with deterministic balances |
| AC-15 | S2 wipes data dir, sets birthday=0, finds 512 outputs. | `src/scenarios/s2.rs::tests::wipe_and_rescan_from_genesis` | unit | tempdir + mock CipherSeed |
| AC-16 | S3 sets birthday=H_birth from S0; records `blocks_scanned = H_tip_end − H_birth`. | `src/scenarios/s3.rs::tests::birthday_from_s0` | unit | mock CipherSeed |
| AC-17 | S4 iterates N∈{8,16,32,64,128}; stops on terminal-or-budget. | `src/scenarios/s4.rs::tests::iterates_all_n_values` + `::tests::stops_on_budget` | unit | tokio time fake (`tokio::time::pause()`) + mock driver |
| AC-18 | S4 records the 5 per-tx and 4 aggregate fields including serialization gap. | `src/scenarios/s4.rs::tests::records_all_metrics` | unit | mock driver emitting timestamps |
| AC-19 | S5 runs Batch (K=10) and Individual (M=100) arms with same 100-recipient list; computes throughput multiplier. | `src/scenarios/s5.rs::tests::batch_and_individual_arms` | unit | deterministic recipient fixture |
| AC-20 | S5 Batch arm runs for `payment_processor`; Individual arm runs for `new_wallet` and `old_wallet`. | `src/scenarios/s5.rs::tests::arm_per_mode_mapping` | unit | none |
| AC-21 | S5 records `pre_state_utxos`, `pre_state_balance` (post-S4); does NOT reset state. | `src/scenarios/s5.rs::tests::no_state_normalization` | unit | mock driver tracking state-reset calls |
| AC-22 | S6 records `T_scan(S6) − T_scan(S2)` and `T_scan(S6) / T_scan(B0)`. | `tests/result_profile_schema.rs::test_s6_deltas` + `src/result/deltas.rs::tests` | unit + integration | fixture |
| AC-23 | S7 uses birthday=H_birth (same shape as S3, post-S5 ordering). | `src/scenarios/s7.rs::tests::birthday_from_s0_post_s5` | unit | mock CipherSeed |
| AC-24 | Pre-scan birthday rewrite invokes `CipherSeed::change_birthday(0)` for B0/S2/S6 and `change_birthday(H_birth)` for S3/S7. | `src/wallet_setup/birthday.rs::tests` | unit | constructed `CipherSeed` from BIP39 test mnemonic |
| AC-25 | Profile's `config` block contains all 11 keys with documented defaults. | `tests/result_profile_schema.rs::test_config_block_complete` | integration | `fixtures/result_profile_schema_example.json` |
| AC-26 | Profile's `environment` block has CPU, RAM, disk, OS, network-path (all non-null, regex-validated per directive 8). | `src/environment/capture.rs::tests::all_fields_populated` + `tests/result_profile_schema.rs::test_environment_block` | unit + integration | live capture on test host |
| AC-27 | Profile's `versions` block has commit/tag for `minotari_console_wallet`, `minotari`, and base node — all match real upstream refs (regex: `[0-9a-f]{40}` or `v\d+\.\d+\.\d+(-pre\.\d+)?`). | `tests/result_profile_schema.rs::test_versions_block` | integration | fixture + real baseline |
| AC-28 | Profile's `deltas` block contains 4 named computed values, all non-null numerics. | `tests/result_profile_schema.rs::test_deltas_block` + `src/result/deltas.rs::tests` | unit + integration | fixture |
| AC-29 | `baselines/esmeralda_canonical.json` parses as valid JSON via `serde_json::from_str::<ResultProfile>`. | `tests/result_profile_schema.rs::test_canonical_baseline_parses` | integration | real baseline |
| AC-30 | `src/scenarios/s4.rs` contains zero matches for `\b(retry|backoff|reattempt|resubmit|reschedule)\b`. | `tests/no_retry_in_s4.rs` | static-check | source files |
| AC-31 | No scenario file contains UTXO partitioning tokens (`\b(partition|shard|chunk_utxos|split_inputs)\b`). | `tests/no_utxo_partitioning.rs` | static-check | source files |
| AC-32 | `src/scenarios/s4.rs` contains zero matches for `\b(Semaphore|RateLimit|Throttle|tokio::time::sleep|interval)\b`. | `tests/no_throttling_in_s4.rs` | static-check | source files |
| AC-33 | Harness fed a deliberately-stalling mock wallet records `timeout_count > 0` and `stall_count > 0` rather than panicking. | `tests/stalls_are_reported.rs` | integration | `StallingWalletDriver` mock |
| AC-34 | Each scan scenario (B0, S2, S3, S6, S7) calls `wipe_data_dir()` before scan; refuses paths outside the harness-owned tempdir. | `src/wallet_setup/wipe.rs::tests::refuses_external_paths` + `src/scenarios/{b0,s2,s3,s6,s7}.rs::tests::wipes_before_scan` | unit | tempdir |
| AC-35 | Three distinct seeds appear in run logs / per-mode config; funding-tx timing is not present in any scenario's metrics. | `tests/three_seeds_distinct.rs` + `src/wallet_setup/seeds.rs::tests::distinct_per_mode` | unit + integration | env-var fixture |
| AC-36 | All "wait for confirmation" call sites read `C_min` from config (no literal `1`, `3`, `6` confirmation depths). | `tests/c_min_not_hardcoded.rs` | static-check | source files |
| AC-37 | `fee_rate` appears in `config` block; harness logs show `fee_rate` value passed to each mode's send call. | `tests/result_profile_schema.rs::test_fee_rate_recorded` + `src/modes/*/send.rs::tests::fee_rate_passed_through` | unit + integration | fixture |
| AC-38 | Every (mode, scenario) cell has an `errors` sub-object with `success_count`, `rejection_count`, `stall_count`, `timeout_count` and `error_strings` array. | `tests/result_profile_schema.rs::test_errors_breakdown` | integration | fixture |
| (Safety, not numbered AC) | Mainnet-protection guard rejects `network: mainnet` before spawning any wallet. | `tests/mainnet_guard.rs` | integration | none |
| (Directive 5a) | Broadcast deserializer handles bare `{"error": "..."}` shape as parse-error variant. | `src/broadcast/deserialize.rs::tests::bare_error_shape` | unit | `fixtures/submit_transaction_bare_error.json` |
| (Directive 5b) | Broadcast deserializer handles full JSON-RPC envelope with `accepted=true`. | `src/broadcast/deserialize.rs::tests::envelope_success` | unit | `fixtures/submit_transaction_envelope_success.json` |
| (Directive 5c) | Each of 5 rejection variants (Orphan, FeeTooLow, TimeLocked, ValidationFailed, AlreadyMined) deserializes to the typed enum value. | `src/broadcast/deserialize.rs::tests::rejection_<variant>` × 5 | unit | 5 fixture files |
| (Directive 7) | Result profile never contains seed phrases, view/spend keys, passphrases, raw signed-tx blobs, gRPC tokens, faucet creds, `$HOME` username paths. | `src/result/redaction.rs::tests::result_profile_never_contains_secrets` | unit | full-fake-profile builder + `REDACTION_DENYLIST` |
| (Directive 8) | Environment field capture produces non-empty values matching per-field regex. | `src/environment/capture.rs::tests::field_regex_match` | unit | live capture |
| (Live-network smoke) | `https://rpc.esmeralda.tari.com/get_tip_info` returns 200 + JSON parseable as `TipInfo`; `/json_rpc submit_transaction` round-trips. | `tests/live_esmeralda_smoke.rs` `#[ignore]` | live-network | live endpoint |
| (Baseline run) | Full B0+S0–S7 × 3-modes execution against funded Esmeralda wallets writes a profile that passes all schema tests. | `RUNBOOK.md §Producing the Baseline` (operator-driven), output committed | baseline-run | live Esmeralda + funded wallets |

### Test layout

```
wallet-benchmarks/
├── Cargo.toml
├── README.md
├── RUNBOOK.md
├── src/
│   ├── lib.rs
│   ├── config.rs                            # #[cfg(test)] mod tests — default-values, override-flag tests
│   ├── broadcast/
│   │   ├── mod.rs
│   │   └── deserialize.rs                   # #[cfg(test)] mod tests — directives 5a, 5b, 5c (7 unit tests)
│   ├── environment/
│   │   └── capture.rs                       # #[cfg(test)] mod tests — directive 8 (per-field regex)
│   ├── modes/
│   │   ├── mod.rs
│   │   ├── mode1/lifecycle.rs               # #[cfg(test)] mod tests — spawn/poll/teardown state machine
│   │   ├── mode2.rs                         # #[cfg(test)] mod tests — pipeline ordering
│   │   └── mode3.rs                         # #[cfg(test)] mod tests — batch path invocation
│   ├── scenarios/
│   │   ├── mod.rs
│   │   ├── b0.rs                            # #[cfg(test)] mod tests
│   │   ├── s0.rs                            # #[cfg(test)] mod tests
│   │   ├── s1.rs                            # #[cfg(test)] mod tests — AC-13, AC-14
│   │   ├── s2.rs                            # #[cfg(test)] mod tests — AC-15
│   │   ├── s3.rs                            # #[cfg(test)] mod tests — AC-16
│   │   ├── s4.rs                            # #[cfg(test)] mod tests — AC-17, AC-18
│   │   ├── s5.rs                            # #[cfg(test)] mod tests — AC-19, AC-20, AC-21
│   │   ├── s6.rs                            # #[cfg(test)] mod tests — AC-22
│   │   └── s7.rs                            # #[cfg(test)] mod tests — AC-23
│   ├── result/
│   │   ├── mod.rs
│   │   ├── deltas.rs                        # #[cfg(test)] mod tests — AC-22, AC-28
│   │   ├── redaction.rs                     # #[cfg(test)] mod tests — directive 7
│   │   └── schema.rs                        # #[cfg(test)] mod tests — schema field type checks
│   └── wallet_setup/
│       ├── birthday.rs                      # #[cfg(test)] mod tests — AC-24
│       ├── seeds.rs                         # #[cfg(test)] mod tests — AC-35
│       └── wipe.rs                          # #[cfg(test)] mod tests — AC-34 (path-confinement)
├── tests/
│   ├── common/
│   │   ├── mod.rs                           # shared: load_fixture(), build_fake_profile(), MockChild
│   │   └── stalling_driver.rs               # StallingWalletDriver impl for AC-33
│   ├── build_smoke.rs                       # AC-1
│   ├── runbook_completeness.rs              # AC-2
│   ├── baseline_committed.rs                # AC-3 (file exists + parses + non-stub)
│   ├── single_invocation.rs                 # AC-4 (CLI shape)
│   ├── mode2_no_console_wallet.rs           # AC-6 (grep)
│   ├── result_profile_schema.rs             # AC-8, AC-9, AC-10..AC-12, AC-22, AC-25..AC-29, AC-37, AC-38
│   ├── no_retry_in_s4.rs                    # AC-30 (grep)
│   ├── no_utxo_partitioning.rs              # AC-31 (grep)
│   ├── no_throttling_in_s4.rs               # AC-32 (grep)
│   ├── stalls_are_reported.rs               # AC-33 (behavioral with StallingWalletDriver)
│   ├── three_seeds_distinct.rs              # AC-35 (env-var + log)
│   ├── c_min_not_hardcoded.rs               # AC-36 (grep)
│   ├── mainnet_guard.rs                     # Safety
│   └── live_esmeralda_smoke.rs              # #[ignore] live-network
├── fixtures/                                # checked-in test data
│   ├── get_tip_info_archival.json
│   ├── submit_transaction_bare_error.json
│   ├── submit_transaction_envelope_success.json
│   ├── submit_transaction_envelope_rejected_orphan.json
│   ├── submit_transaction_envelope_rejected_fee_too_low.json
│   ├── submit_transaction_envelope_rejected_time_locked.json
│   ├── submit_transaction_envelope_rejected_validation_failed.json
│   ├── submit_transaction_envelope_rejected_already_mined.json
│   ├── unsigned_transaction.json
│   ├── result_profile_schema_example.json
│   └── bip39_wordlist.txt                   # vendored for redaction test pattern compilation
├── baselines/
│   ├── .gitignore                           # *.json, !esmeralda_canonical.json
│   └── esmeralda_canonical.json             # the committed real-run baseline (AC-3)
└── analysis/
    ├── ANALYSIS.md
    ├── COMPETITION.md
    ├── DESIGN.md
    └── RESULT_PROFILE_SCHEMA.md
```

`tests/common/mod.rs` is shared across integration tests (idiomatic Rust pattern — `cargo test` discovers `tests/common/` as a non-test crate when imported via `mod common;`). It contains: `load_fixture(name: &str) -> serde_json::Value`, `build_fake_profile_with_realistic_secrets() -> ResultProfile`, `MockChild` (mock subprocess for Mode 1 lifecycle), `StallingWalletDriver` (in its own file).

`baselines/.gitignore` excludes all JSON except `esmeralda_canonical.json` — operators producing local runs don't accidentally commit their personal hardware profiles, but the one canonical baseline is committed (AC-3).

### Fixtures

| Fixture | Source | Checked in? | Used by |
|---------|--------|-------------|---------|
| `fixtures/get_tip_info_archival.json` | Captured from 2026-05-22 live probe of `rpc.esmeralda.tari.com/get_tip_info` (pruning_horizon=0 confirmed) | Yes | Mode 2 unit tests, scanner unit tests |
| `fixtures/submit_transaction_bare_error.json` | `{"error": "missing field \`offset\`"}` — directive note from arch-test prompt, confirmed by live probe | Yes | `src/broadcast/deserialize.rs::tests::bare_error_shape` |
| `fixtures/submit_transaction_envelope_success.json` | `{"result":{"accepted":true,"rejection_reason":"None","is_synced":true,"details":null},"error":null,"id":"1"}` | Yes | `src/broadcast/deserialize.rs::tests::envelope_success` |
| `fixtures/submit_transaction_envelope_rejected_{orphan,fee_too_low,time_locked,validation_failed,already_mined}.json` | Mechanically generated from the success envelope shape, varying `accepted=false` + `rejection_reason` | Yes (5 files) | `src/broadcast/deserialize.rs::tests::rejection_<variant>` |
| `fixtures/unsigned_transaction.json` | Captured output of `minotari create-unsigned-transaction` from a local run — paraphrased JSON shape mirrors PR #99's step-defs | Yes | Mode 2 offline-signer unit tests; sourced via `gh pr view 99 -R tari-project/minotari-cli` by `swe-test` |
| `fixtures/result_profile_schema_example.json` | Hand-built, fully-populated example matching the schema arch-system produces in `RESULT_PROFILE_SCHEMA.md` | Yes | `tests/result_profile_schema.rs` (all schema tests load this as the structural baseline) |
| ~~`fixtures/bip39_wordlist.txt`~~ | ~~BIP39 English wordlist~~ | **Dropped.** The R1 redaction regex is structural (11–23 lowercase 3-to-8-letter words separated by whitespace), so it catches both BIP-39 and Tari 24-word mnemonics by construction — no wordlist needed. Verified by `src/seed/redact.rs::tests::denylist_catches_realistic_seed_phrase` in step 3d.2. |
| `baselines/esmeralda_canonical.json` | Produced by the operator running the full harness end-to-end against Esmeralda once funding completes | Yes (1 file, the canonical) | `tests/baseline_committed.rs` (AC-3), `tests/result_profile_schema.rs` (re-runs all schema tests against the real artifact) |
| Per-operator local baselines | Generated by anyone running the harness locally | No — gitignored | Local diagnostics only |

The fake-secret test scaffold (directive 7) is constructed *programmatically* inside `src/seed/redact.rs::tests` and never serialized to disk — no committed file ever contains a "realistic" mnemonic or view key. The fake values use a fresh `gen_seed()` mnemonic per test invocation (24-word Tari mnemonic via `CipherSeed::random()`) plus syntactically-valid-but-disposable Tari testnet view-only addresses generated for this purpose. Documenting this convention in `tests/README.md` is part of `swe-test`'s job.

### Mocks

| Mock | Crate / approach | Scope | Used for |
|------|------------------|-------|----------|
| JSON-RPC `submit_transaction` server | `wiremock = "0.6"` | Per-test `MockServer::start().await` | Mode 2 broadcast pipeline unit tests; AC-33 stall-reporting (mock returns slow response or hangs past timeout) |
| Subprocess (`minotari_console_wallet`, `minotari create-unsigned-transaction`) | Trait abstraction `trait Spawner { fn spawn(&self, ...) -> Result<Box<dyn Child>>; }` with `MockSpawner` impl using `mockall = "0.13"` | Per-test; injected into mode-under-test via constructor | Mode 1 lifecycle tests (AC-5), Mode 2 unsigned-tx creation, AC-35 seed-isolation |
| Wallet driver (high-level interface used by scenarios) | Trait `WalletDriver` with `MockWalletDriver` via `mockall` | Per-scenario test | All scenario unit tests (AC-12..AC-23) — drives deterministic state transitions, balance changes, success/failure outcomes |
| `StallingWalletDriver` | Hand-written `impl WalletDriver` that returns `Future::pending` or fixed-delay responses | Single integration test (`tests/stalls_are_reported.rs`) | AC-33 — proves harness records `stall_count > 0` rather than restarting |
| Tokio time | `tokio::time::pause()` / `advance()` (native) | S4 budget-timeout test | AC-17 (`S4_T_budget = 900s` elapses deterministically without 15-min wall-clock wait) |
| Filesystem | `tempfile = "3"` `TempDir` per test | Per-test, dropped on test end | AC-34 wipe path-confinement; Mode 1 data-dir scoping |
| Environment capture | Trait `EnvCapture` with `FakeEnvCapture` returning known values | `src/environment/capture.rs::tests` | AC-26 (verifies regex-validation logic without depending on test-host shape) plus a separate live-capture test that runs against the real host |

No `tonic` mock server for Mode 1's gRPC — the gRPC surface is too wide (8 protos, ~25 used methods) and a hand-rolled mock server would be ~1500 lines for limited test value. Mode 1 unit tests cover the lifecycle state machine and command construction; the gRPC wire contract is exercised by the live-network smoke test (`tests/live_esmeralda_smoke.rs`) and validated by the baseline-run artifact (`baselines/esmeralda_canonical.json` would not exist if gRPC didn't work).

### CI integration

`arch-system` ships **no CI on the bounty PR** (see §System Decisions). RUNBOOK documents the equivalent local invocation as a pre-PR contract:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked            # default-features, no --ignored
cargo build --workspace --release
```

Live-network tests are excluded by the `#[ignore]` annotation on every `tests/live_esmeralda_smoke.rs` test — default `cargo test` skips them. Operators run `cargo test -- --ignored` locally when verifying live behavior. AC-1 / AC-3 verification additionally requires producing `baselines/esmeralda_canonical.json` (see §Baseline result profile production below).

The redaction test (`src/result/redaction.rs::tests::result_profile_never_contains_secrets`) is in the default `cargo test` set — it runs every PR, every commit. If a future contributor adds a secret-leaking field, the local pre-PR check fails before submission.

The static-check grep tests (`tests/no_retry_in_s4.rs`, `tests/no_utxo_partitioning.rs`, `tests/no_throttling_in_s4.rs`, `tests/c_min_not_hardcoded.rs`, `tests/mode2_no_console_wallet.rs`) are in the default set — they're cheap (single `std::fs::read_to_string` + regex per file) and they're the AC-30/31/32/36/AC-6 enforcement bar.

**If the maintainer asks for CI in review:** add a minimal `.github/workflows/check.yml` running the four commands above on Ubuntu 22.04. Not in v1.

### Baseline result profile production (AC-3)

This is the most important test in the strategy.

**Command sequence (operator runs once, output committed):**

```
# 1. Configure
cp config.example.toml config.toml
$EDITOR config.toml   # set network=esmeralda, base_node_url=https://rpc.esmeralda.tari.com,
                      # seed env-var names, fee_rate, A_fund=10000, C_min=3

# 2. Generate three fresh seeds (one per mode), stored in env vars
export HARNESS_SEED_OLD_WALLET="$(harness gen-seed)"
export HARNESS_SEED_NEW_WALLET="$(harness gen-seed)"
export HARNESS_SEED_PP_WALLET="$(harness gen-seed)"

# 3. Fund each wallet to A_fund + headroom by mining locally
minotari_miner --base-node-grpc https://rpc.esmeralda.tari.com:18142 \
               --wallet-payment-address "$(harness print-address --seed-env HARNESS_SEED_OLD_WALLET)" \
               --until-balance "12000 tXTM"
# repeat for NEW_WALLET and PP_WALLET

# 4. Run
harness run --config config.toml --output baselines/esmeralda_$(date +%Y%m%d_%H%M%S).json

# 5. Verify (schema test runs against the freshly-produced file)
cargo test --test result_profile_schema -- --baseline baselines/esmeralda_<run-id>.json

# 6. Commit
cp baselines/esmeralda_<run-id>.json baselines/esmeralda_canonical.json
git add baselines/esmeralda_canonical.json
git commit -m "chore: commit baseline result profile from Esmeralda run <run-id>"
```

**Pre-conditions:** three funded Esmeralda wallets (one per mode), `rpc.esmeralda.tari.com` reachable, local disk with ≥ 50 GB free for archival wallet data dirs, `minotari_miner` available for funding.

**Estimated wall-clock for full B0+S0–S7 × 3-modes:** B0 ~30 min/mode (archival scan from genesis depends on Esmeralda tip height — could be hours); S0–S5 dominated by `C_min = 3` × Esmeralda block time (~120s) × ~7 confirmation waits per scenario ≈ 40 min/mode; S4 hard-capped at 15 min × 5 N-values × 3 modes = up to 225 min; S6/S7 are scans (~30 min/mode each). **Rough total: 8–14 hours operator-wall-clock for a complete run.** This figure goes in RUNBOOK.md.

**How operator verifies "real, not stub":** the schema test enforces sanity floors per field — `T_scan_B0 > 60s` (a real archival scan cannot complete in 60s), `H_tip_start > 0`, `H_tip_end >= H_tip_start`, `balance_S0 ∈ [A_fund - max_fee, A_fund]` (delta within one tx fee), `utxo_count_S2 == 512`, `versions.minotari_console_wallet` matches the regex `^[0-9a-f]{40}$|^v\d`, every per-tx record has `submitted_at` in the run's wall-clock window. Any stubbed cell trips one of these floors and fails `tests/baseline_committed.rs`.

**PR-body integration:** `## QA Results` section in the PR body includes:

- Path to committed baseline: `baselines/esmeralda_canonical.json`
- Run ID, operator hardware, wall-clock duration
- One-line summary of headline numbers (T_scan_B0, S5 throughput multiplier, S4 max-N reached)
- `cargo test --workspace` output (default + schema validation)

### "Harness Measures, Does Not Engineer Around Wallet Pain" enforcement (AC-30/31/32/33)

| AC | Test location | Forbidden pattern (regex) | Pass condition |
|----|---------------|---------------------------|----------------|
| AC-30 | `tests/no_retry_in_s4.rs` | `\b(retry|backoff|exponential|reattempt|resubmit|reschedule)\b` (case-insensitive) | Zero matches in `src/scenarios/s4.rs`. Test fails with the matching line numbers if found. Exit code 1 on detection. |
| AC-31 | `tests/no_utxo_partitioning.rs` | `\b(partition_utxos|shard_utxos|chunk_utxos|split_inputs_per_worker|preassign_outputs)\b` | Zero matches across `src/scenarios/*.rs`. Allows generic `chunk` / `partition` in non-scenario code (e.g. `src/result/` may chunk for serialization). |
| AC-32 | `tests/no_throttling_in_s4.rs` | `\b(Semaphore|RateLimit|RateLimiter|Throttle|tokio::time::sleep|tokio::time::interval|std::thread::sleep)\b` | Zero matches in `src/scenarios/s4.rs`. The `tokio::time::pause()` fixture used in `s4.rs::tests` is in `#[cfg(test)]` and excluded by reading only `cfg(not(test))` source via `cargo expand` or stripping `#[cfg(test)]` blocks pre-grep. |
| AC-33 | `tests/stalls_are_reported.rs` | (behavioral) | Setup: `Harness::new()` with `StallingWalletDriver` (sleeps 10s on every `send_transaction`, returns `Err(Timeout)` after harness's per-tx timeout of 5s in test config). Run S4 with `N_concurrent=8`. Assert: profile's `results.new_wallet.s4.errors.timeout_count >= 5`, `results.new_wallet.s4.errors.stall_count >= 1`, harness exits 0 (does not panic), scenario completes (does not abort mid-batch). |

Each static-check test reads the source file via `std::fs::read_to_string(env!("CARGO_MANIFEST_DIR").join("src/scenarios/s4.rs"))`, strips `#[cfg(test)] mod tests { ... }` blocks via balanced-brace parsing (or by reading the file with `proc_macro2::TokenStream` and filtering), then runs the regex. Test fails with `panic!("Forbidden pattern {pattern} matched at line {n}: {line}")`.

### Test-data secret handling

**Test seeds are disposable + obviously-fake.** The original arch-test plan assumed the BIP-39 all-zeros test phrase (`"abandon abandon ... about"`) could double as the deterministic test mnemonic. That assumption was verified-and-rejected during swe-impl step 1 (M1 spike): Tari's `CipherSeed` (the seed type at `tari_common_types::seeds::cipher_seed`, commit `5d6ef11bb89caa34fe9ee676d608f273db90038d`, v5.3.1) is a 24-word Tari-specific encoding, NOT BIP-39 — the abandon×12 phrase fails `CipherSeed::from_mnemonic` validation outright. Recorded in `analysis/API_DRIFT.md §Step 1`.

The replacement, verified working in step 3b and used throughout 3c+3d unit tests:

- **Non-deterministic tests** use `crate::gen_seed()` which wraps `CipherSeed::random()` → emits a freshly-generated 24-word Tari mnemonic per call. Every seed-touching test in `src/seed::tests`, `src/seed::redact::tests`, and `src/guards::tests` builds disposable mnemonics this way.
- **Cross-test isolation** uses unique per-test env-var names (`WALLET_BENCHMARKS_TEST_*_{suffix}`) rather than overwriting a shared `HARNESS_SEED_OLD` — required because cargo's default parallel runner shares process env, and two tests mutating the same env var name race.
- **The R1 regex (BIP-39 / Tari mnemonic shape catch)** does not need a wordlist — it matches structural shape (11–23 lowercase 3-to-8-letter words separated by whitespace). It catches both BIP-39 and Tari mnemonics by construction. Confirmed by the `denylist_catches_realistic_seed_phrase` test in step 3d.2, which feeds a freshly-generated 24-word Tari mnemonic into a fake profile and observes the R1/R2 catch.

Spirit unchanged: tests use disposable, obviously-fake seeds, never anything that could exist on a real funded wallet.

**Realistic-looking secrets for the redaction test** are constructed programmatically:

- Mnemonics: fresh `gen_seed()` outputs (a fresh 24-word Tari mnemonic per test invocation) — demonstrates the R1 regex catches the structural shape regardless of which word set the input uses.
- View keys: a syntactically valid Tari testnet address with the view-key prefix, computed once at test-fixture-build time from a `gen_seed()` mnemonic and pinned as a `const` in the test module.
- Spend keys: same approach.
- Passphrases: literal string `"test-passphrase-redaction-canary"`.
- Raw signed-tx blobs: hex string of length 512 (long enough to trip the "any hex/base64 over 256 chars" pattern), computed from `[0u8; 256].iter().map(...)`.
- gRPC bearer tokens: literal `"Bearer test_token_redaction_canary"`.
- Faucet credentials: literal `"FAUCET_API_KEY=test_redaction_canary"`.
- Username paths: literal `"/Users/testredactioncanary/wallet"` and `"/home/testredactioncanary/wallet"`.

None of these strings appear in committed files — they're constructed in `#[cfg(test)]` code. The test scaffold is exactly directive 7's snippet, with `REDACTION_DENYLIST` defined as a `&[&str]` of compiled regex patterns.

**Per CLAUDE.md §Secrets:** `.gitignore` from commit 1 includes `.env`, `.env.*`, `*.seed`, `seeds/`, `wallets/`, `*.tari/`, `~/.tari/`, `~/.local/share/tari/`. The harness reads seeds from env vars `HARNESS_SEED_OLD_WALLET`, `HARNESS_SEED_NEW_WALLET`, `HARNESS_SEED_PP_WALLET` (named in config.example.toml, never in a committed `config.toml`).

### Open questions for swe-test

1. **If `arch-system` chooses to depend on `minotari_node_wallet_client` (the published `tari` rust-client crate) for `submit_transaction`** rather than reimplement the POST, then the dual-shape deserializer test (directive 5) becomes a contract test against that crate's error mapping — we verify our wrapper surfaces both shapes correctly, not that we deserialize raw JSON. **arch-system DID choose this**; swe-test wraps the published client and tests our error mapping, not raw JSON.
2. **Schema field names defer to `RESULT_PROFILE_SCHEMA.md`.** Every reference in this strategy uses semantic field names (`profile.results[mode][scenario].errors.timeout_count`). The actual JSON path may differ — `swe-test` resolves names against arch-system's finalized schema before writing test code. **Schema now finalized in `analysis/RESULT_PROFILE_SCHEMA.md`.**
3. **`MockChild` trait signature depends on whether `arch-system` chooses `std::process::Child` or `tokio::process::Child` for subprocess management.** `tokio::process::Child` is async-friendlier for Mode 1's gRPC-poll-loop, but `std::process::Child` keeps the lifecycle code simpler. `swe-test` chooses to match `arch-system`'s pick (likely tokio given the rest of the architecture).
4. **Mode 2 fixture JSON shapes (PR #99 mirror).** `swe-test` runs `gh pr view 99 -R tari-project/minotari-cli` and pulls the exact `PrepareOneSidedTransactionForSigningResult` JSON shape from `integration-tests/steps/wallet_benchmark.rs` to populate `fixtures/unsigned_transaction.json`. If PR #99's JSON has rotted vs current `tari_transaction_components` v5.3.1, regenerate by running `minotari create-unsigned-transaction` locally and committing the output.
5. **Whether `--include-ignored=false` is needed explicitly in CI.** Default `cargo test` excludes `#[ignore]`-marked tests; no flag needed. **No CI in v1 — moot.**
6. **`StallingWalletDriver` interface depends on the `WalletDriver` trait shape arch-system designs.** If `arch-system` makes `WalletDriver` object-safe with `async-trait`, the mock is trivial. If it's a static-dispatch enum, mock needs a variant — swe-test adapts.

### Re-flag check

I have reviewed the test strategy against directive 6's escalation triggers:

- **Custom key derivation or seed→key reduction in test code:** No. The redaction-test fixture uses the BIP39 all-zeros test phrase as an opaque string — no key derivation happens in test code; addresses for the fixture are precomputed once and pinned as `const`. No mnemonic-to-key reduction logic appears in `src/` or `tests/`.
- **Path-dep or git submodule into `tari` / `minotari-cli` for fixtures:** No. Fixtures are committed JSON files captured from live probes or PR #99's published code, not vendored sources.
- **Retry / backoff / partitioning / throttling in test or test scaffolding:** No. `wiremock` and `mockall` are used per-test without retry. The `#[ignore]` annotation on live-network tests is the explicit opt-out, not a retry mechanism. No flaky-test retry harness.
- **Live network calls to anything other than `https://rpc.esmeralda.tari.com`:** No. The single live-network test file (`tests/live_esmeralda_smoke.rs`) hits only `rpc.esmeralda.tari.com`. The baseline-run procedure documented in `RUNBOOK.md` also hits only that host plus `seeds.esmeralda.tari.com` for DNS peer-seed bootstrap (read-only DNS query, not an API call).

**No re-flag required.** Phase 1b proceeds with `arch-system` + `arch-test` only.

---

## Phase 2 hand-off (swe-impl, then swe-test)

1. **swe-impl** reads `analysis/DESIGN.md` + `analysis/RESULT_PROFILE_SCHEMA.md`. Implements the workspace layout (arch-system §System §Workspace layout), then `src/` modules in this order: `guards` (mainnet protection) → `config` → `env_capture` + `versions` → `seed` (with redaction) → `wallet_lifecycle` + `broadcast` → `modes/mode1` → `modes/mode2` → `modes/payment_processor` → `scenarios/` (B0 → S0 → S1 → S2 → S3 → S4 → S5 → S6 → S7) → `result_profile` (writer + deltas) → `main`. Commits per logical unit (one type of work per commit).
2. **swe-test** reads `analysis/DESIGN.md §Test Strategy` and `analysis/RESULT_PROFILE_SCHEMA.md`. Implements unit tests inline as it goes (paired with each `swe-impl` module), then integration tests under `tests/` and fixtures under `fixtures/`. Static-check tests last (after source is stable so the grep target paths exist).
3. **Operator** (human) produces `baselines/esmeralda_canonical.json` by running the harness end-to-end against Esmeralda once swe-impl + swe-test merge. This is the AC-3 artifact and is the trigger for opening the PR.
4. **swe-review** reads the diff and the produced baseline; checks against the AC matrix (ANALYSIS.md), the schema (RESULT_PROFILE_SCHEMA.md), and the 8 directives.
