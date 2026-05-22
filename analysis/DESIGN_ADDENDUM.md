# DESIGN_ADDENDUM — wallet-benchmarks#1

> **Status:** Post-Phase-1b supplement to `analysis/DESIGN.md`. Read both before any implementation work — DESIGN.md is the architect output (historical record, do not modify), this addendum captures decisions made between Phase 1b approval and the swe-impl spawn.
> **Precedence:** Where this addendum conflicts with DESIGN.md or RESULT_PROFILE_SCHEMA.md, **the addendum wins**. Conflicts are called out explicitly under each item below.
> **Re-flag check:** None of M1–M4 / S1–S4 cross any of directive 6's escalation triggers (no custom key derivation, no path-deps, no retry/backoff/throttling, no non-Esmeralda networks, no reaching past `sign_locked_transaction` into signing internals). swe-impl proceeds without arch-security.

## M1. Dep-graph spike (swe-impl's FIRST commit)

Before any module work, swe-impl's first commit is a ~50-line spike at `src/main.rs` that **proves all five hot dependencies are callable as DESIGN.md specifies**:

1. `tari_common::configuration::Network::Esmeralda` — constructs without error.
2. `tari_transaction_components::consensus::ConsensusConstantsBuilder::new(Network::Esmeralda).build()` — returns `ConsensusConstants`.
3. `tari_transaction_components::key_manager::KeyManager::new(...)` — constructs from a `WalletType` reconstituted from a deterministic test mnemonic (the BIP39 all-zeros phrase; no real seed material). No reaching into private signing internals; only `WalletType::from_mnemonic` and `KeyManager::new` are called.
4. `tari_transaction_components::offline_signing::sign_locked_transaction` — is in scope; the spike does NOT need to call it end-to-end (it would need a real unsigned tx), but its symbol must resolve and its signature must match what DESIGN.md §Mode 2 specifies.
5. `minotari_node_wallet_client::Client::new(...)` — constructs against `https://rpc.esmeralda.tari.com` (no network call made; just constructor proof). The published crate version `5.3.1` from crates.io.

The spike's `fn main()` body prints a one-line confirmation per dep and exits 0. No async runtime needed (or use `#[tokio::main]` if any constructor demands it).

**Pass condition:** `cargo build --release` exits 0. `cargo run --release` exits 0 with the five confirmation lines.

**Fail action:** if any dep fails to resolve, type-check, or compile, **STOP and report the resolution error** to the main thread. Do not proceed to any module work. Likely causes: tari git rev `766f80ccc20596413ee208311750c11e02a2841d` (per DESIGN.md §Dependency strategy) has rotted relative to the published `minotari_node_wallet_client = "5.3.1"`; `tari_crypto = "0.22.1"` and `tari_utilities = "0.8"` may need bumping to match the tari rev; or `minotari_app_grpc` may not expose the expected paths with `default-features = false`. Resolution typically means either re-pinning the tari rev forward or accepting newer crate versions — both are dep-strategy decisions that need explicit sign-off, not silent in-flight changes.

This spike serves a second purpose: it is the smallest possible reproduction of the full dep graph, so any future dep-resolution rot fails fast in commit 1 instead of buried in scenario code.

## M2. Funding pre-flight (`enforce_funding`)

Add to `src/guards.rs`, called from `main()` immediately after `enforce_esmeralda(&config)?` and before any mode runs:

```rust
pub fn enforce_funding(
    config: &Config,
    seeds: &[SeedHandle; 3],   // [old, new, payment_processor], in that order
) -> anyhow::Result<()>
```

Behavior:

- For each seed in `seeds`, derive the wallet address (the same `WalletType::tari_address()` path Mode 2 uses; reused from the `print-address` subcommand per S1).
- Query each address's spendable balance via `minotari_node_wallet_client::Client::get_balance(address)` against `config.base_node_url`.
- Compute `required = config.a_fund * 11 / 10` (10% headroom, integer math in microTari — note that `1.1` as a float is forbidden by `clippy::float_arithmetic` in many tari-style configs; do the rational multiplication).
- If **any** seed reports balance `< required`, `anyhow::bail!` with all three balances and `required` shown, plus a one-line pointer to RUNBOOK §Funding. Format:
  ```
  Funding pre-flight failed. Required ≥ 11_000_000_000 µT per seed (a_fund × 1.1).
    old_wallet:        <bal> µT (short by <delta> µT)   ❌
    new_wallet:        <bal> µT (short by <delta> µT)   ❌
    payment_processor: <bal> µT                          ✓
  See RUNBOOK §Funding for how to mine to each address using minotari_miner.
  ```
- If all three pass, proceed.

This is called once per harness invocation. Subsequent mode runs do not re-check.

**Why 10% headroom:** S1's doubling rounds + S4's concurrent dispatches + S5's fees eat into the `a_fund` reserve faster than naive accounting predicts (some txs land twice if the harness retries a connect — wait, no retries; let me restate: some txs leave the wallet in a `pending` state that masks balance for the next scenario's pre-check). 10% is a deliberate safety margin documented here so no future contributor "optimizes" it to 0.

**This is a guardrail, not a measurement.** Funding-tx fees and timings remain out-of-scope per AC-35; pre-flight balance reads are a startup check, not a per-scenario metric.

## M3. Mode 3 batching CLI shape — verify before writing module

The `minotari` CLI's `create-unsigned-transaction` command may accept the batch 1-to-many shape in one of three ways: (a) repeated `--recipient <addr>::<amount>` flags, (b) a single `--recipients-file <path>` flag pointing at a JSON/TOML file, (c) neither. DESIGN.md §Mode 3 left the choice to implementation time. Before swe-impl writes any Mode 3 module code (i.e. before `src/modes/payment_processor.rs` exists beyond a `// TODO` stub):

1. Read `minotari-cli:minotari/src/cli.rs` at the pinned tari rev `766f80ccc20596413ee208311750c11e02a2841d` (per DESIGN.md §Dependency strategy). Specifically locate the `CreateUnsignedTransaction` variant of the top-level enum and inspect its derived `clap` annotations.
   ```
   gh api repos/tari-project/minotari-cli/contents/minotari/src/cli.rs?ref=<minotari-cli pinned commit>
   ```
   (Note: `minotari-cli`'s commit is pinned separately from tari's rev; per DESIGN.md §3 versions block we pin minotari-cli at `52a7287a3fe1e7831855649c530534af9f2d4830`. Use that ref.)
2. Determine whether `--recipient` is `Vec<String>` (repeated flag), whether `--recipients-file` exists, or whether neither does.
3. Confirm with a local invocation against a fresh wallet: `minotari create-unsigned-transaction --help` and look at the printed usage.
4. **Document the proven invocation** as a new section appended to this addendum file (`§Mode 3 CLI shape — proven`) before writing any Mode 3 code. The section records: which flag works, the exact argv shape used by Mode 3, and the commit hash + date of the verification.
5. **If neither shape works** — i.e. the CLI does not support batch 1-to-many in a single invocation — **escalate to the main thread immediately**. Do not write Mode 3 as a loop of single-tx invocations (that would violate AC-7 and silently turn Mode 3 into Mode 2). Possible escalation outcomes: pin a newer minotari-cli commit, add the flag upstream (out of scope), or surface the gap in the PR body and let the maintainer decide.

This verification is M3's gate; do not skip it.

## M4. Baseline path canonicalized

There is **one** canonical baseline file: `baselines/esmeralda_canonical.json`. There is no root-level `baseline_profile.json`.

**Conflicts to resolve in implementation:**

- `analysis/RESULT_PROFILE_SCHEMA.md` line 3 states `**File:** baseline_profile.json at repo root.` — **swe-impl ignores this line in favor of M4 here**. The harness's default `--output` argument is `baselines/esmeralda_canonical.json` (relative to the workspace root), not `./baseline_profile.json`. Schema field shape is unchanged; only the file location moves.
- DESIGN.md §Workspace layout shows both `baseline_profile.json` (root, "the AC-3 artifact, committed after live run") and `baselines/` directory. **Drop the root file**; keep the `baselines/` directory.
- DESIGN.md §Test Strategy already references `baselines/esmeralda_canonical.json` — that's correct, leave as-is.

**`baselines/.gitignore` content** (commit 1):
```
*.json
!esmeralda_canonical.json
```
Ad-hoc per-operator runs land in `baselines/esmeralda_<timestamp>.json` and are gitignored; only the canonical file is tracked.

**Operator workflow** (replaces step 6 of DESIGN.md §Baseline result profile production):
```
# Operator runs harness with default output path (no --output flag needed)
harness run --config config.toml
# Output lands at baselines/esmeralda_<run-id>.json

# Verify
cargo test --test result_profile_schema -- --baseline baselines/esmeralda_<run-id>.json

# Commit the canonical
cp baselines/esmeralda_<run-id>.json baselines/esmeralda_canonical.json
git add baselines/esmeralda_canonical.json
git commit -m "chore: commit baseline result profile from Esmeralda run <run-id>"
```

Schema's `redaction_denylist` field (R10 in RESULT_PROFILE_SCHEMA.md §6) catches `/Users/<user>` and `/home/<user>` paths regardless of where the baseline lives, so M4 introduces no new redaction surface.

## S1. `gen-seed` and `print-address` as harness CLI subcommands

The harness binary `wallet-benchmarks` exposes (via `clap` derive):

```
wallet-benchmarks run --config <toml>                              # default; the main harness
wallet-benchmarks gen-seed                                          # prints a fresh 24-word BIP39 mnemonic to stdout
wallet-benchmarks print-address --seed-env <ENV_VAR_NAME>           # prints the base58 wallet address derived from the seed in the named env var
```

**Why subcommands of the same binary, not separate binaries:**

- Shares the dep graph (no duplicated `Cargo.toml` `[[bin]]` entries; no double-build cost).
- Operator only learns one binary name.
- All three subcommands share the seed-handling and address-derivation code paths — `enforce_funding` (M2), Mode 2, and `print-address` all call the same `WalletType::tari_address()` site.
- Matches `minotari` CLI's own subcommand style (mirroring maintainer convention).

**Implementation constraint:** `gen-seed` calls `WalletType::generate()` (or the equivalent `CipherSeed::new()` + `to_mnemonic()` if `generate()` doesn't exist with that name — verify against the pinned tari rev). `print-address` calls `WalletType::from_mnemonic(&mnemonic, None)?.tari_address()` and then `.to_base58()` (per S2). **No custom key code** — both subcommands use the same exported APIs Mode 2 uses; same risk-surface scope (TIER-2, API contact only).

`gen-seed` output goes only to stdout; the binary makes no attempt to store, log, or echo the mnemonic elsewhere. The redaction denylist (R1, R2) applies to the result profile, not to interactive `gen-seed` output where the operator wants the mnemonic.

## S2. Address encoding: base58

All `--recipient` arguments passed to `minotari create-unsigned-transaction` (Mode 2 and Mode 3) use `TariAddress::to_base58()` as the encoding. Not emoji-ID. Not hex.

**Rationale:**

- PR #99's step-defs use base58 — direct mirror per DESIGN.md §Mode 2.
- Emoji-ID is human-only; copy-paste hazards on terminals; CLI parsers may reject non-ASCII argv.
- Hex doubles the byte count and is not the canonical wire form Tari ecosystem uses for addresses (it's used for keys and txids, not addresses).

The `print-address` subcommand (S1) also emits base58 — the operator can pipe its output directly into `minotari_miner --wallet-payment-address $(wallet-benchmarks print-address --seed-env HARNESS_SEED_OLD_WALLET)`.

Verification: the harness includes a unit test in `src/seed/tests.rs` (or wherever the address-derivation code lives) asserting that `TariAddress::from_base58(&addr.to_base58()).unwrap() == addr` for a deterministic test mnemonic — round-trip canary.

## S3. AC-36 grep test — tightened pattern

`tests/c_min_not_hardcoded.rs` (the AC-36 enforcement) uses a tighter pattern than DESIGN.md §Test Strategy implied. Two regexes, ORed; either match fails the test:

1. `wait_for_confirmation\w*\s*\(\s*\d+\s*\)` — catches `wait_for_confirmation(3)`, `wait_for_confirmations(6)`, `wait_for_confirmation_depth(1)`.
2. `confirmations?\s*[>=]=\s*\d+` — catches `confirmations >= 3`, `confirmation == 1`, `confirmations < 6`.

**What is NOT flagged** (intentionally):

- Generic numeric literals in source — `let chunk_size = 64;`, `for i in 0..6 {}`, `Duration::from_secs(3)`.
- Tests using literal confirmation depths inside `#[cfg(test)] mod tests` (test code is allowed to set `c_min = 3` for fixture determinism).
- Comments containing the numbers.

The grep test must strip `#[cfg(test)] mod tests { ... }` blocks before running the regex (same balanced-brace approach as the AC-30/31/32 grep tests per DESIGN.md §Test Strategy). It scans `src/scenarios/*.rs`, `src/modes/*.rs`, `src/wallet_lifecycle/*.rs`, and `src/broadcast/*.rs`.

This pattern was tightened because the original "no literal 1, 3, 6" framing would have false-positived on legitimate uses (e.g. `RAM_HEADROOM_GB = 50`, `S4_N_VALUES = [8, 16, 32, 64, 128]`). The tighter regex catches only the construct that AC-36 actually polices — hardcoded confirmation depths in flow control.

## S4. Pre-flight execution order for swe-impl

The swe-impl phase executes in this order. Each step must complete before the next begins:

1. **M1 dep-graph spike commit.** `cargo build --release` + `cargo run --release` exit 0. If fail, STOP and escalate.
2. **M3 CLI verification.** Read `minotari-cli/minotari/src/cli.rs` at pinned rev, verify `create-unsigned-transaction` batching shape, append `§Mode 3 CLI shape — proven` section to this addendum file. If neither shape works, STOP and escalate.
3. **Module implementation order** (per DESIGN.md line 869): `guards` (mainnet + funding) → `config` → `env_capture` + `versions` → `seed` (with redaction + `gen-seed`/`print-address` subcommands per S1) → `wallet_lifecycle` + `broadcast` → `modes/mode1` → `modes/mode2` → `modes/payment_processor` → `scenarios/` (B0 → S0 → S1 → S2 → S3 → S4 → S5 → S6 → S7) → `result_profile` (writer + deltas) → `main` (the CLI wiring that supersedes the M1 spike).

The M1 spike code at `src/main.rs` is replaced by the real `main.rs` in step 3's final sub-step; the spike's purpose is dep-graph validation, not production code.

**swe-test runs in parallel with swe-impl's step 3**, pairing unit tests with each module as it lands (per CLAUDE.md §Workflow Orchestration).

## Runtime expectation for the canonical baseline run

DESIGN.md §Baseline result profile production estimated 8–14 hours wall-clock. **That estimate is too low.** Revised, after accounting for Esmeralda's ~2-minute block time and the explicit per-scenario confirmation waits:

- **Per mode:**
  - B0: ~30 min (archival scan from genesis; depends on tip height — Esmeralda is at ~655k blocks as of 2026-05-22 per the live probe).
  - S0: ~6 min (one tx + `C_min=3` confirmations × ~2 min/block = ~6 min minimum).
  - S1: 127 txs across 7 rounds, each round waiting for confirmation depth `C_min=3`. Rounds run serially, txs within a round run serially. Per-round wall-clock ≈ `(tx_count × ~10s broadcast) + (C_min × 2 min)` = `(tx_count × 10s) + 6 min`. Sum across 7 rounds ≈ `(127 × 10s) + (7 × 6 min)` ≈ 21 min + 42 min = **~63 min/mode**.
  - S2: ~30 min (full rescan, slightly slower than B0 because there's history to discover).
  - S3: ~3 min (scans only from `H_birth` — much shorter than full scan).
  - S4: hard budget `S4_T_budget = 15 min` × 5 N-values = **75 min/mode** (the dominant scenario).
  - S5: ~30 min (110 txs across two arms; batch arm 10 txs × 6 min/conf ≈ 60 min if serial confirmations, but txs are dispatched back-to-back so confirmation overlap brings this to ~20 min; individual arm 100 txs × 6 min/conf ≈ 600 min worst-case — but again, confirmations overlap heavily, dominated by block-time, ~15 min realistic). Bound at ~30 min/mode.
  - S6: ~35 min (rescan with more history than S2).
  - S7: ~5 min (birthday rescan, more history than S3).
  - **Subtotal per mode: ~4–5 hours.**

- **Three modes sequential: 12–15 hours minimum.**

- **Realistic operator wall-clock for the canonical run: 36–54 hours**, accounting for:
  - Esmeralda block-time variance (mined by community, not uniform 2 min).
  - Mempool congestion (S4's 128 concurrent dispatch may experience delays beyond the harness's per-tx timeout, generating `timeout_count` / `stall_count` per AC-33 — the scenario still runs to budget).
  - One-off retries by the operator if a wallet process crashes (NOT a harness retry — operator restarts the harness and skips already-completed cells if/when that capability lands; v1 may require re-running from S0 on operator restart).
  - Funding-wait time (mining ~33 000 tXTM total across three seeds, at Esmeralda's per-block reward — likely overnight on a single GPU, off-clock vs harness wall-clock but operator-time real).

**This figure goes in RUNBOOK §Producing the Baseline.** Operators reading the RUNBOOK before kicking off a baseline run see "Expected wall-clock: 36–54 hours" up front, with the per-scenario breakdown beneath. If the maintainer asks for a faster path (e.g. "use a shorter `C_min`"), that's a configuration-override decision recorded in the result profile's `config` block, not a harness change.

## Re-flag check

Walking each new decision through directive 6:

- **M1** spike calls only `WalletType::from_mnemonic`, `KeyManager::new`, `sign_locked_transaction` (resolution only, not invocation), and `Client::new` — all exported APIs already approved in DESIGN.md. No new key derivation, no signing internals. **Pass.**
- **M2** `enforce_funding` reads `get_balance` only; no signing, no key handling beyond `WalletType::tari_address()` (shared with S1 / Mode 2). **Pass.**
- **M3** reads `cli.rs` from a pinned commit via `gh api`; no path-dep, no submodule. **Pass.**
- **M4** changes file path only; schema and code shape unchanged. **Pass.**
- **S1** subcommands call `WalletType::generate()` / `WalletType::from_mnemonic()` / `TariAddress::to_base58()`. No new key code. **Pass.**
- **S2** address encoding is a wire-format choice; no crypto surface. **Pass.**
- **S3** tightens a grep test; static-check tooling only. **Pass.**
- **S4** is an ordering directive; no code surface change. **Pass.**

**No re-flag required. swe-impl proceeds without arch-security.**

## §Dependency strategy — resolved (post-M1 spike escalation)

> **Status:** Resolution decision following swe-impl's escalation in `analysis/DESIGN_AMENDMENT.md`. Adopted 2026-05-22.
> **Precedence:** This section supersedes `DESIGN.md §Dependency strategy` in the same way `DESIGN_ADDENDUM.md` line 4 specifies for the file overall. swe-impl uses the Cargo.toml block below, not DESIGN.md's.

### 1. Resolution

Replace the three git-rev tari deps with crates.io v5.3.1 pins. Exact Cargo.toml block (replaces DESIGN.md `§Dependency strategy` "Tari ecosystem" block):

```toml
# Tari ecosystem — Mode 1 (gRPC) and Mode 2/3 (offline sign + HTTP submit)
minotari_app_grpc           = { version = "5.3.1", default-features = false }
minotari_node_wallet_client = "5.3.1"
tonic                       = { version = "0.13", features = ["transport"] }
prost                       = "0.13"

tari_common                 = "5.3.1"
tari_common_types           = "5.3.1"
tari_transaction_components = "5.3.1"
tari_crypto                 = { version = "0.22.1", features = ["borsh"] }
tari_utilities              = "0.8"
```

### 2. Rationale

- **Mirrors `minotari-cli`'s own Cargo.toml pinning style** (which uses crates.io 5.3.x for these deps), per CLAUDE.md §Maintainer Mirror Rule. `minotari-cli` is authored by the bounty maintainer (SWvheerden); `minotari_payment_processor` — the source of the original git-rev choice — is not.
- **Single-resolver consistency with `minotari_node_wallet_client = "5.3.1"` and `minotari_app_grpc = "5.3.1"`.** Eliminates the version-skew issue swe-impl surfaced in `DESIGN_AMENDMENT.md` (two copies of tari types transitively pulled at different versions).
- **`core2 0.4.0` yank is moot** — crates.io 5.3.1 of the tari crates resolves past it via its own transitive selection.

### 3. AC-27 recording

The `versions` block in the result profile records, for each of the three tari crates:

- `tag: "v5.3.1"`
- `commit: "5d6ef11bb89caa34fe9ee676d608f273db90038d"`

A single tag covers all three because they ship from a workspace. `RESULT_PROFILE_SCHEMA.md §3` line 85 already permits the tag+commit pair ("If running off a tag, `commit` may still be filled (recommended)"). We take the recommended path.

### 4. Precedence

This section supersedes DESIGN.md `§Dependency strategy` for purposes of swe-impl's Cargo.toml construction. DESIGN.md remains the historical architect output; swe-impl reads both files and uses the override above where they conflict.

### 5. DESIGN_AMENDMENT.md disposition

Left intact as the historical record of why we changed pinning. Do not edit. A reader can follow the chain `DESIGN.md` → `DESIGN_AMENDMENT.md` (diagnosis) → `DESIGN_ADDENDUM.md §Dependency strategy — resolved` (decision).
