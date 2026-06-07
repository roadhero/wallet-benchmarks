# DESIGN_AMENDMENT — wallet-benchmarks#1

> **Status:** Written by swe-impl mid-Step 1 (M1 dep-graph spike). Handing back to the main thread per `analysis/DESIGN_ADDENDUM.md §M1` "Fail action".
> **Triggered by:** unresolvable transitive dep at the pinned tari rev — see §1 below.
> **Workdir state:** files written but not committed. `git status` shows untracked `Cargo.toml`, `.gitignore`, `baselines/`, `src/` at the time of writing. No commits made on `bounty/wallet-benchmarks-1-create-benchmarks` beyond `16c36f1` (Phase 1b artifacts).

## §1 Resolution failure at pinned rev

`cargo build --release` with the literal `Cargo.toml` from `DESIGN.md §Dependency strategy` (edition 2021, the exact dep block, no modifications) fails at resolution:

```
$ cargo build --release
    Updating crates.io index
    Updating git repository `https://github.com/tari-project/tari/`
error: failed to select a version for the requirement `core2 = "^0.4.0"`
  version 0.4.0 is yanked
location searched: crates.io index
required by package `multihash v0.16.0`
    ... which satisfies dependency `multihash = "^0.16"` of package `multiaddr v0.14.0`
    ... which satisfies dependency `multiaddr = "^0.14.0"` of package `tari_common v5.2.1-pre.2 (https://github.com/tari-project/tari/?rev=766f80ccc20596413ee208311750c11e02a2841d#766f80cc)`
    ... which satisfies git dependency `tari_common` of package `wallet-benchmarks v0.1.0 (...)`
```

Reproducible across `cargo build` and `cargo update`. Toolchain: `cargo 1.95.0 / rustc 1.95.0`.

**Root cause:** at the pinned tari rev (`766f80ccc20596413ee208311750c11e02a2841d`), `tari_common` is `v5.2.1-pre.2` and pulls in `multiaddr 0.14.0` → `multihash 0.16.0` → `core2 ^0.4.0`. The only published version of `core2` matching `^0.4.0` is `0.4.0`, which has been yanked from crates.io. No version in the `^0.4.0` range is available, so cargo cannot resolve.

**This matches the `§M1` fail-action signal verbatim:**
> "tari git rev `766f80ccc20596413ee208311750c11e02a2841d` (per DESIGN.md §Dependency strategy) has rotted relative to the published `minotari_node_wallet_client = "5.3.1"`"

Per the addendum: "Resolution typically means either re-pinning the tari rev forward or accepting newer crate versions — both are dep-strategy decisions that need explicit sign-off, not silent in-flight changes."

## §2 What was attempted (and not attempted)

**Attempted:**
- `cargo build --release` (resolution fail above).
- `cargo update` (resolution fail above; same error).

**Deliberately NOT attempted** (would be the "silent swap" the addendum forbids):
- Adding a `[patch.crates-io]` entry to override `multihash` / `multiaddr` / `core2`.
- Bumping `tari_common` / `tari_common_types` / `tari_transaction_components` to a newer rev or to a published version.
- Adding any dep not listed in `DESIGN.md §Dependency strategy`.
- Editing the `Cargo.toml` beyond what `DESIGN.md` specifies.

The spike code at `src/main.rs` was written against the API surface inspected from the pinned rev (see `analysis/DESIGN.md` lines 235–296 for the literal dep block, and `src/main.rs` for the five proofs). It would compile under any resolution where the dep graph is satisfied — the blocker is exclusively resolution, not API mismatch.

## §3 Decision matrix for the main thread

The main thread chooses one path forward; all three are dep-strategy decisions outside swe-impl's mandate.

### Option A — re-pin the tari rev forward

Find the most recent commit on `tari-project/tari` whose `multiaddr` transitive has been bumped past 0.14.0 (or whose `multihash` no longer requires `core2 ^0.4.0`). Update `DESIGN.md §Dependency strategy` and `DESIGN_ADDENDUM.md §M3` (the minotari-cli commit pin `52a7287a...` references the tari rev consistently — both pins should move together or the dep-graph compatibility argument breaks).

Tradeoffs:
- Cleanest: stays on git-rev pin, preserves the "canonical third-party pinning" argument from DESIGN.md §Dependency strategy.
- Risk: newer rev may have other yanked-dep issues or API-shape changes (the spike's API references — `WalletType::SeedWords`, `SeedWordsWallet::construct_new`, `KeyManager::new`, `sign_locked_transaction` signature, `Client::new`, `CipherSeed::from_mnemonic` via the `Mnemonic` trait — would need re-verification against the new rev).
- This bumps the `minotari-cli` pin coordinately if M3 hasn't run yet.

### Option B — patch the yanked dep

Add a `[patch.crates-io]` entry pinning `core2` to a non-yanked version (e.g. `core2 = "0.3"`) and accept that this is a deliberate downgrade. Alternatively, override `multihash` to a newer version that uses a non-yanked `core2`.

Tradeoffs:
- Minimal change to dep strategy; preserves the pinned tari rev.
- Adds a `[patch.crates-io]` block to `Cargo.toml` which DESIGN.md does NOT authorize. Reviewer surface: every reviewer asks "why this patch?"
- May not actually compile (the API of the patched `core2` / `multihash` may have drifted incompatibly with what `tari_common`'s `multiaddr` expects).

### Option C — pin to published versions instead of git rev

Replace the three git-rev pins (`tari_common`, `tari_common_types`, `tari_transaction_components`) with whatever the most recent crates.io-published versions are. Verify that `minotari_node_wallet_client = "5.3.1"`'s transitive `tari_transaction_components` matches.

Tradeoffs:
- Eliminates the git-rev dep-rot risk entirely.
- DESIGN.md §Dependency strategy explicitly chose git-rev "to align with `minotari-cli`'s expected host (matches PR #99)". Moving away from that breaks the "match canonical third-party pinning" argument.
- Need to verify that the published versions still expose `sign_locked_transaction`, `KeyManager::new(WalletType)`, etc. — the spike's API surface comes from the pinned rev's source, not from any crates.io tarball.

## §4 swe-impl's recommendation (advisory only)

**Option A**, with the substep:
1. Run `gh api repos/tari-project/tari/commits?sha=development&per_page=20` and inspect each commit's `Cargo.lock` (via `git show <sha>:Cargo.lock | grep multihash`) until a rev is found whose `multihash` is past 0.16.0 or whose `multiaddr` is past 0.14.0.
2. Verify that rev still exposes the four exported API surfaces the spike uses:
   - `tari_common::configuration::Network::Esmeralda`
   - `tari_transaction_components::consensus::ConsensusConstantsBuilder::new(Network).build()`
   - `tari_transaction_components::key_manager::{WalletType, SeedWordsWallet, KeyManager}` with `KeyManager::new(WalletType)`
   - `tari_transaction_components::offline_signing::sign_locked_transaction` with the 4-arg signature DESIGN.md §Mode 2 specifies
   - `tari_common_types::seeds::{cipher_seed::CipherSeed, mnemonic::Mnemonic, seed_words::SeedWords}` with `SeedWords: FromStr` and `CipherSeed: Mnemonic`
3. Verify the new rev's `minotari-cli` companion commit (the §M3 verification target) exists at a coordinated rev.
4. Update `DESIGN.md §Dependency strategy` to the new rev, update `DESIGN_ADDENDUM.md §M3` to the coordinated minotari-cli commit, leave the rest of DESIGN untouched.

Option A keeps the design's pinning argument intact and eliminates the yanked-dep failure mode at the root.

## §5 Re-flag check

This amendment doesn't touch any of directive 6's escalation triggers — it's a pure dep-resolution decision, no consensus / wallet / bridge / signing code is being added or modified. No re-flag needed. The eventual chosen path (A / B / C) preserves the same API surface as DESIGN.md already approved.

## §6 What swe-impl will do on resumption

Once the main thread updates DESIGN.md §Dependency strategy (and DESIGN_ADDENDUM.md §M3 if needed):

1. Replace `Cargo.toml` with the updated dep block verbatim.
2. Re-run `cargo build --release`. If clean: re-run `cargo run --release` and verify the five confirmation lines. If still failing: write another amendment.
3. Commit Step 1 with `chore(deps): dep-graph spike for tari ecosystem` per the spawn prompt.
4. Proceed to Step 2 (M3 CLI verification) per `§S4`.

No code in `src/main.rs` should need to change unless the new tari rev changed any of the five API surfaces — if so, swe-impl re-verifies via `gh api` reads of the new rev's source before adjusting the spike.

---

Handing back to the main thread.

# §7 — Step 3e.4 LiveBalanceQuery gap (post-resolution amendment)

> **Status:** Written by swe-impl during Step 3e.4 bundle. Resolved in-flow (not a stop-and-escalate); recorded here so the design trail is auditable.
> **Triggered by:** Structural impossibility of implementing `BalanceQuery::get_balance(&TariAddress) -> u64` against the published `minotari_node_wallet_client = "5.3.1"`.

## §7.1 The gap

`DESIGN_ADDENDUM.md §M2` specifies `Client::get_balance(address)` as the funding pre-flight surface. The published `BaseNodeWalletClient` trait at `src/client/mod.rs` lines 26–75 of `minotari_node_wallet_client-5.3.1` exposes:

```
get_address, is_online, get_tip_info, get_header_by_height,
get_height_at_time, get_utxos_by_block, sync_utxos_by_block,
get_last_request_latency, get_utxos_mined_info, fetch_utxo,
query_deleted_utxos, submit_transaction, transaction_query,
get_mempool_fee_per_gram_stats, get_kernel_merkle_proof
```

None of these is "give me address X's spendable balance":
* `get_utxos_mined_info(hashes, version)` takes UTXO hashes the caller does not have.
* `fetch_utxo(hash)` takes a UTXO hash, not an address.
* `sync_utxos_by_block` walks the full chain from a start header — would reimplement wallet scanning.
* No address-indexed view exists on the base-node HTTP surface.

The Step 3e.4 prompt's suggested option (a) — `client.get_utxos_mined_info(address, ...)` — does not type-check (the method takes `Vec<Vec<u8>>`, not a `TariAddress`).

## §7.2 The only working source-of-truth

Address balances live on the **wallet** gRPC, not the base-node HTTP. `minotari_app_grpc/proto/wallet.proto` lines 963 + 2240–2245 expose `GetBalance` which returns `available_balance + pending_incoming_balance + pending_outgoing_balance + timelocked_balance`. This requires a **running console_wallet for that seed** — three separate console-wallet spawns to pre-flight three seeds.

## §7.3 Resolution adopted (no escalation)

The `BalanceQuery` trait was specced before the API gap surfaced. To unblock `main()` calling `enforce_funding` without spawning three console wallets in a pre-flight, swe-impl ships `LiveBalanceQuery` as a placeholder implementation that returns a structured error pointing at this amendment:

```rust
impl BalanceQuery for LiveBalanceQuery {
    fn get_balance(&self, _address: &TariAddress) -> anyhow::Result<u64> {
        anyhow::bail!(
            "LiveBalanceQuery: balance pre-flight against the base node is not \
             implementable on minotari_node_wallet_client = \"5.3.1\" (no address-indexed \
             balance endpoint). See analysis/DESIGN_AMENDMENT.md §7. Set \
             Config::skip_funding_preflight = true OR fund each wallet via minotari_miner \
             and trust the operator (per RUNBOOK §Funding)."
        )
    }
}
```

The `enforce_funding` call from `main()` becomes opt-out via a Config flag — when the operator funds via `minotari_miner` per RUNBOOK and explicitly accepts the risk, the pre-flight is skipped.

This is the minimum-surface resolution that:

1. Lets `main()` wire up without spawning three console wallets for a pre-flight.
2. Preserves AC-35 (three distinct seeds, three separately-funded wallets) — the operator still funds each seed via `minotari_miner` as RUNBOOK documents.
3. Documents the gap publicly for upstream attention (the PR body §Pain Points already mentions this; this section is the formal record).
4. Defers the "spawn three console wallets to check balances" alternative to a future amendment once Mode 1's wallet-spawn infrastructure has proved out — at that point a `WalletGrpcBalanceQuery` impl that piggybacks on Mode 1's spawn would be straightforward.

## §7.4 What is NOT shipped in 3e

* No `wiremock`-based unit tests for `LiveBalanceQuery::get_balance` — there is no HTTP endpoint to mock against; the impl bails before any network call.
* No live-network smoke against `https://rpc.esmeralda.tari.com` for balance — same reason.
* The `Fake`-backed `enforce_funding` unit tests in `src/guards.rs` continue to cover the funding-pre-flight logic with a deterministic mock.

## §7.5 Re-flag check

This resolution does NOT cross any directive-6 trigger: no consensus / wallet / bridge / signing code is added. The placeholder impl is pure-Rust no-network. **No re-flag.**

## §7.6 Closure — step 3k commit 1

The placeholder is **closed** as of step 3k commit 1. Resolution shape (greenlit by main thread):

* `BalanceQuery` trait becomes `#[async_trait]` and takes `SeedRole` rather than `&TariAddress`. The address parameter was design-smell — caller derives address from seed, impl reverse-looks-up. `SeedRole` is what `enforce_funding`'s iteration loop has on hand.
* `WalletGrpcBalanceQuery` composes the existing `ConsoleWalletLifecycle`: per `get_balance(role)` call, resolve mnemonic via `SeedHandle::mnemonic_for(role)`, build a per-call `HarnessDataDir` (process-id + nanosecond-stamped run id to avoid collisions), `ConsoleWalletLifecycle::new` + `replace_mnemonic` + `spawn` + `wait_ready`, query `GetBalance` (proto: `GetBalanceRequest { payment_id: None }`, response field `available_balance`), `teardown` via SIGTERM grace + SIGKILL escalation.
* Cost: ~30s console_wallet startup × 3 seeds = ~90s funding pre-flight. Tracked in `analysis/PR_BODY_PLAN.md` §Operator Setup.
* Test coverage: trait shape exercised via `guards::tests` with `FakeBalanceQuery`; live-spawn surface smoke-tested via the bogus-binary-path test that asserts the spawn-failure path bails with role context.

# §8 — Mode 2/3 read-side flows: subprocess wiring deferred to step 3i

> **Status:** Written by swe-impl during Step 3g.2 / 3h. Resolved in-flow (not stop-and-escalate); recorded here so the design trail is auditable. Same precedent as §7 (LiveBalanceQuery placeholder).
> **Triggered by:** API drift between `DESIGN.md §Mode 2 step 7` and the actual `minotari` CLI surface at the pinned `minotari-cli` commit `52a7287a3fe1e7831855649c530534af9f2d4830`.

## §8.1 The gap

`DESIGN.md §Mode 2 step 7` and §Mode 3 enumerate read-side subprocess calls Mode 2/3 use for the scan / balance / utxo-count / re-import flows that B0/S2/S3/S6/S7 invoke:

- `minotari scan --from-birthday <birthday>` — for `Mode::scan_from_birthday`
- `minotari get-balance --account-name default` — for `Mode::get_balance`
- `minotari list-utxos --account-name default` — for `Mode::get_utxo_count`
- `minotari import-seed --database-path <dir> --seed-words-file ...` — for `Mode::wipe_and_reimport`

Reading `minotari/src/cli.rs` at the pinned commit (fetched via `gh api` during step 3h) shows these argv shapes do NOT exist verbatim:

- **`Scan` does exist** but accepts `--max-blocks-to-scan` (not `--from-birthday`); birthday is baked into the wallet via `CipherSeed::change_birthday(...)` then `Create --seed-words` (i.e. the same rewrite path Mode 1 uses).
- **`Balance` exists** but its output is human-formatted ASCII (microTari + Tari with thousand separators per the doc comment on line 245). No `--format json` flag. Parsing requires either a regex on stdout or a code-side number extractor.
- **`list-utxos` does NOT exist** — there is no subcommand that emits the wallet's UTXO count as a machine-parseable value. Closest is `Balance` which sums confirmed outputs but does not surface a count separately.
- **`import-seed` does NOT exist**; the equivalent is `Create --seed-words "<24 words>"` (which initialises an account from the supplied mnemonic per the doc comment on lines 291–306). Same `SecurityArgs` (--password) + `DatabaseArgs` (--database-path) + `AccountArgs` (--account-name) flatten as the other subcommands.

## §8.2 What is shipped in Modes 2/3 (3g/3h)

* `send_single` — fully wired via the shared `minotari_subprocess::create_sign_and_submit` helper (subprocess `create-unsigned-transaction` + parse + in-process sign + HTTP submit). AC-critical for S0/S1/S4/S5.
* `send_batch_one_to_many` — same helper, K repeated `--recipient` flags. AC-critical for S5 batch arm.
* `scan_from_birthday` / `get_balance` / `get_utxo_count` / `wipe_and_reimport` — bail with a structured `anyhow::Error` whose message names this amendment and the specific CLI shape mismatch. Loud-bailing matches the LiveBalanceQuery precedent from §7; no silent zero-valued profile cells.

## §8.3 Resolution path (step 3i)

In step 3i, the scenarios layer:

1. Replaces `scan_from_birthday` with: `mode.terminate_then_wipe_data_dir()` → rewrite mnemonic via `CipherSeed::change_birthday(birthday)` (the shared `rewrite_birthday` helper from Mode 1) → write new mnemonic to seed file → spawn `minotari Create --seed-words "<rewritten>" --database-path <dir> --password <pw> --account-name default` → spawn `minotari Scan --database-path ...` (with `--max-blocks-to-scan` either omitted, defaulting to 50, or set high enough to reach the tip in finite-but-bounded time).
2. Replaces `get_balance` with: subprocess `minotari Balance --database-path <dir> --account-name default` + a stdout regex extractor (`r"(\d+(?:_\d+)*) µT"` or similar). Compose with `BalanceQuery` from `src/wallet_lifecycle/balance_query.rs` if a uniform abstraction is needed.
3. Replaces `get_utxo_count` with: derive from `Balance` output if a count emerges from the human format; otherwise add a scenario-layer count derived from `outputs_found` during the scan phase.
4. Replaces `wipe_and_reimport` with: the Mode-1-style flow rebuilt around `Create --seed-words` instead of `import-seed`.

## §8.4 Re-flag check

This deferral does NOT cross any directive-6 trigger: no consensus / wallet / bridge / signing code is added in 3g/3h's send_* path beyond the already-approved `sign_locked_transaction` invocation. The placeholders are pure-Rust no-network bails. **No re-flag.** Same precedent as §7.

---

## §9 S4 concurrent-construction dispatch surface (step 3i.1.f)

> **Status:** swe-impl STOP at step 3i.1.f.commit2. Per the 3i.1.f brief's §Sharper STOP triggers item 1 ("Mode trait can't be invoked from N concurrent tasks without a Mutex around the dispatcher — sharper STOP rule item 4 — trait shape concern"). Handing back to the main thread.
> **Triggered by:** the `Mode` trait's `&mut self` receivers vs S4's `tokio::JoinSet::spawn` requirement for `'static + Send` futures.

### §9.1 What S4 needs

S4 dispatches N ∈ {8, 16, 32, 64, 128} concurrent construction tasks per sub-block. The brief's required dispatch pattern:

```rust
let mut joinset = tokio::task::JoinSet::new();
for _ in 0..n {
    joinset.spawn(construct_one(...));   // each task: one tx construct + submit
}
tokio::select! {
    biased;
    _ = ctx.clock.sleep(s4_t_budget_ms) => { joinset.abort_all(); /* budget */ }
    Some(joined) = joinset.join_next()   => { /* drain */ }
}
```

The construct/submit primitive each spawned task must call is the per-mode "construct + sign + broadcast" pipeline — i.e. the work `mode.send_single(...)` already does. The brief enumerates three resolution paths for the borrow-checker collision below (§9.3).

### §9.2 What the Mode trait exposes today

Per `src/modes/mod.rs` line 102 (and verified against the three impls):

```rust
#[async_trait::async_trait]
pub trait Mode: Send + Sync {
    fn name(&self) -> &'static str;
    async fn send_single(
        &mut self,                       // <-- &mut self
        recipient: &TariAddress,
        amount_microtari: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxRecord>;
    async fn send_batch_one_to_many(&mut self, ...) -> ...;   // &mut self
    async fn scan_from_birthday(&mut self, ...) -> ...;       // &mut self
    async fn get_balance(&mut self) -> ...;                   // &mut self
    async fn get_utxo_count(&mut self) -> ...;                // &mut self
    async fn wipe_and_reimport(&mut self, ...) -> ...;        // &mut self
}
```

`tokio::JoinSet::spawn` requires a `Future + Send + 'static`. The future must own its captures (`async move { ... }`). Giving N concurrent tasks a `&mut self` borrow of the same `Mode` impl is forbidden by the borrow checker. `Mode` has no `Clone` bound, no `clone_handle()` method, and no associated free function that takes only shared references.

### §9.3 Brief's three resolution paths — none works without a trait or signature change

The 3i.1.f brief §Concurrency surface enumerates:

| Path | Description | Status in current tree |
|---|---|---|
| (a) | Mode trait has a concurrent-safe construction primitive taking `&self` (or static fn) | **Does not exist.** No such method on the trait or any impl. |
| (b) | The Mode 2/3 subprocess pipeline (`create_sign_and_submit`) is a free function and each task calls it directly with cloned/shared immutable state | **Works for Mode 2/3 only.** `create_sign_and_submit(cfg: &Config, seeds: &SeedHandle, ..., broadcaster: &Broadcaster, data_dir: &Path, tx_idx: u64)` per `src/modes/minotari_subprocess.rs:187` is callable from N concurrent tasks via `Arc<Config>`, `Arc<SeedHandle>`, `Arc<Broadcaster>`, `Arc<PathBuf>`. But this bypasses the `Mode` trait — S4 would need a mode-specific dispatch path, and Mode 1's `OldWallet::send_single` is NOT a free function (it dispatches via `lifecycle.client_mut() -> &mut WalletClient<Channel>`). |
| (c) | gRPC client is `Clone`, each task gets its own clone | **Works in principle, blocked by trait layering.** `WalletClient<Channel>` from tonic IS `Clone` (verified — tonic generates `#[derive(Clone)]` on all clients), but `OldWallet::send_single` accesses it through `&mut self.lifecycle.client_mut()` per `src/modes/old_wallet.rs:92`. The clone is hidden behind the `Mode` trait surface. Exposing it requires either (i) extending `Mode` with a `clone_handle()`-style method, or (ii) bypassing the `Mode` trait for S4 dispatch with a mode-specific path. |

**No path satisfies all three: "works for every mode" + "no Mutex around the dispatcher" + "no Mode-trait change".**

### §9.4 The brief's STOP trigger — verbatim

> **Sharper STOP triggers (S4-specific)**
> STOP and escalate if:
> 1. Mode trait can't be invoked from N concurrent tasks without a Mutex around the dispatcher (sharper STOP rule item 4 — trait shape concern)

This is precisely the situation. `Mode::send_single`'s `&mut self` receiver prevents N concurrent invocations without one of:

1. Trait shape change (add a clone-able / `&self` construction primitive)
2. Per-mode dispatch path inside S4 (different code for Mode 1 vs Mode 2/3)
3. Mutex<Mode> around the dispatcher (explicitly forbidden by the brief)

Each of (1) and (2) is architectural scope. swe-impl is escalating rather than freelancing per the brief's instruction: "If you encounter one of the above, file a DESIGN_AMENDMENT.md entry, STOP, and report. Do NOT freelance around it."

### §9.5 DESIGN.md's existing directive

DESIGN.md line 415 (in the §S4 state machine spec) uses:

```rust
for _ in 0..n {
    let m = mode.clone_handle();           // <-- method does not exist
    joins.spawn(async move {
        let tx_record = m.send_single(recipient, amount).await;
        tx_record
    });
}
```

`mode.clone_handle()` is referenced but never defined on the `Mode` trait. Reading DESIGN.md line 415 forward into a concrete trait extension is the path the architect appears to have intended, but the addition of `fn clone_handle(&self) -> Box<dyn Mode>` to the trait is:

* A breaking change for all three existing `Mode` impls (`OldWallet`, `NewWallet`, `PaymentProcessor`)
* A breaking change for `FakeMode` and every existing scenario unit test that constructs it
* Of unclear semantics for `OldWallet` — what does "clone the lifecycle" mean when the lifecycle owns the subprocess `Child`, the `WalletGrpcClient`, and the per-mode `HarnessDataDir`? The `Child` and `HarnessDataDir` are owned; cloning them implies shared ownership of the spawned process, which is undefined.

This deserves an architect-level decision, not a swe-impl improvisation.

### §9.6 Proposed resolution options for the main thread

Three options, in increasing order of scope:

**Option A — Mode-2/3 only, Mode 1 sequential**

S4's per-task dispatch uses a clone-able dispatch handle threaded through `ScenarioCtx` (new field) or a new `ScenarioInput` field. Modes 2/3 wire it to `create_sign_and_submit` with `Arc<Config>` / `Arc<SeedHandle>` / `Arc<Broadcaster>` / `Arc<PathBuf>` captures. Mode 1's S4 is executed sequentially with a runtime warning that "concurrent dispatch is not measured for old_wallet" — surfaces in the result profile as a `note` field on the S4 cell. Pros: zero trait change. Cons: AC-17 reads "concurrent construction" with no mode exception; this is a behavioural delta vs the AC.

**Option B — Extend `Mode` with a clone-able dispatch primitive**

Add `fn dispatcher(&self) -> Arc<dyn S4Dispatcher>` (or similar) to the `Mode` trait. Each impl returns an `Arc`-wrapped handle that captures its own `Arc<...>` immutable state. For `OldWallet` the dispatcher holds a `WalletClient<Channel>` clone (tonic clones the channel cheaply). For `NewWallet` / `PaymentProcessor` it holds `Arc<Config>`, `Arc<SeedHandle>`, `Arc<Broadcaster>`, `Arc<PathBuf>`. `S4Dispatcher::dispatch(&self, ...) -> Future<TxRecord>` takes only `&self`, so the borrow checker permits N concurrent tasks each holding an `Arc<dyn S4Dispatcher>`. Pros: uniform across all 3 modes; matches DESIGN.md line 415's intent. Cons: trait expansion; new trait `S4Dispatcher`; per-impl wiring work (~150 LoC across `old_wallet.rs`, `new_wallet.rs`, `payment_processor.rs`, plus test wiring).

**Option C — Restructure `Mode` trait to use `&self` + interior mutability**

Move the mutable state inside each `Mode` impl behind `Arc<Mutex<...>>` or `Arc<tokio::sync::Mutex<...>>` and convert all `Mode` methods to `&self`. Then `Arc<dyn Mode>` is shareable across tasks. Pros: minimal new types. Cons: (a) introduces internal Mutex into modes, which the brief explicitly disallows for the dispatcher surface; (b) widespread change across all `Mode` impls and tests; (c) the borrow checker isn't the only thing constrained — `client_mut()` on the tonic-generated `WalletClient` actually requires `&mut self` because tonic's generated request methods take `&mut self`. So Mode 1 would still need a Mutex around the gRPC client, which the brief forbids.

### §9.7 Recommended path

**Option B.** It matches DESIGN.md line 415's `mode.clone_handle()` intent, is uniform across modes, and has no Mutex. The per-mode wiring (~150 LoC) is straightforward — `WalletClient<Channel>` is cheap-Clone for Mode 1; the immutable-state Arc bundle for Mode 2/3 is exactly what `create_sign_and_submit` already takes by reference.

### §9.8 Why not Option B in this commit batch

The brief for step 3i.1.f is scoped to "S4 production code — commit 2" with strict instructions: "If the static-check test in commit 1 can't be made to enforce all 6 assertions without false positives against legitimate code → STOP and escalate" and "Do NOT freelance around it." Adding a new trait, per-mode wiring, and the corresponding tests across `src/modes/{old_wallet,new_wallet,payment_processor}.rs` is decisively outside the "S4 scenario file" scope the brief contemplated. The main thread is the right place to greenlight Option B (or pick A or C) before swe-impl proceeds.

### §9.9 Workdir state at STOP

* Branch `bounty/wallet-benchmarks-1-create-benchmarks` at HEAD `3e927a1` (S3 birthday rescan), unchanged from start of step 3i.1.f.
* No commits made on this step. Test commit 1 not written. Production commit 2 not written.
* Untracked files: `analysis/API_DRIFT.md`, `analysis/PR_BODY_PLAN.md`, `analysis/DESIGN_AMENDMENT.md` (this file). Per the brief, untracked.
* `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build --release` — all clean at `3e927a1` (per main thread's earlier note: "153 tests passing, 4 gates green").

### §9.10 Re-flag check

This is an architectural decision about the `Mode` trait's dispatch surface. No code surface touched, no signing code, no key handling, no consensus surface. Pure trait-shape question. **No directive-6 re-flag.**

---

## §11. Mode 1 batch arm via `Transfer { single_tx: true }`

Per @SWvheerden's 2026-06-05 PR-6 inline comment "you can run this on the console wallet" against `src/modes/old_wallet.rs:144`, the Mode 1 `send_batch_one_to_many` impl is rewired against `WalletClient::transfer` with `single_tx = true` and K `PaymentRecipient` entries (one MW tx with K outputs). The proto-level doc-comment at `wallet.proto:578` confirms this shape: "SingleTx is used to indicate should this be sent as a single MW tx or multiple". The S5 batch arm's Mode-1 skip at `src/scenarios/s5_throughput.rs:208-209` is removed; all three modes now populate `arms.batch.*` and AC-19's `throughput_multiplier` is computable per mode.

---

