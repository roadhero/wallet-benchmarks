# Competition Analysis — wallet-benchmarks#1

Captured: 2026-05-22. Recheck before opening our PR.

## At a glance

| | PR #3 (enok1111 / "LJ") | PR #4 (sanrishi / Sanchit Rishi) |
|---|---|---|
| Title | feat: complete wallet benchmark harness — 3 modes, 9 scenarios (B0-S7), tests & CI | Feat/harness implementation |
| Created | 2026-05-18 15:40 UTC | 2026-05-18 16:02 UTC (22 min later) |
| Last update | 2026-05-20 12:02 UTC | 2026-05-19 19:50 UTC |
| Branch | `bounty/issue-1-wallet-benchmark-harness` | `feat/harness-implementation` |
| Diff size | +5452 / -2, 19 files | +4498 / -2, 21 files |
| Architecture | Single-crate Rust (`src/modes/`, `src/scenarios/`) | Cargo workspace, `harness/` sub-crate with vendored protos |
| CI | Yes — `.github/workflows/ci.yml` (129 lines) | None |
| Baseline profile committed? | No — explicitly deferred ("requires live Esmeralda + funded wallets") | `baseline_profile.json` exists but is **1 line / stub** |
| Reviewers so far | gemini-code-assist (auto) only | gemini-code-assist (auto) only — invoked via `/gemini review` |
| Maintainer review | None from @SWvheerden | None from @SWvheerden |
| Build state | Author claims `cargo check` and `cargo build --release` pass; never run live | Gemini flagged invalid dep versions in `harness/Cargo.toml` (build failures) — unresolved at last update |
| End-to-end run evidence | None | None |

## PR #3 in detail

**Author:** @enok1111 (display "LJ"). New GitHub account, no prior tari-project activity.

**What it attempts:**
- Three `WalletMode` impls (`old_wallet.rs`, `new_wallet.rs`, `payment_processor.rs`), each ~800–920 lines.
- Real `tonic` gRPC client wrapping `minotari_app_grpc::WalletClient`.
- Claims real `minotari::Scanner` integration for B0/S2/S3/S6/S7 scan scenarios.
- Nine scenario dispatcher in `src/scenarios/mod.rs`; one full scenario file (`b0_baseline.rs`, 336 lines); the others appear inlined as methods on each mode.
- TOML-driven config (`config.example.toml`), CLI via `clap`, JSON result output, `compute_deltas()` for S2−B0 and S5 throughput multiplier.
- 26 unit/integration tests (config + metrics calculation — *not* live wallet tests).
- CI workflow.

**What's missing vs. the AC:**
- **Baseline result profile not committed** — author explicitly defers this ("requires running against a live Esmeralda testnet with funded wallets"). The AC requires a committed baseline profile as proof-of-run. **This is a hard AC gap.**
- **Never run end-to-end.** No evidence any scenario has executed against a live base node + wallet. Author's "26 passing tests" cover config parsing and delta math only.
- **Transactional path has explicit TODOs.** In the author's own response to the gemini review (commit `65e1527`): *"The actual gRPC call (GetState → scanned_height) is marked as TODO since tonic integration with minotari_app_grpc protos is Phase 3 work."* So `wait_for_scan_complete` is structurally present but non-functional.
- **View-only wallet limitation acknowledged but not resolved.** Author admits the `minotari` crate is view-only and falls back to `minotari_console_wallet` gRPC for *all* signing — meaning the "New Wallet" mode is mostly a thin wrapper around the old wallet, contradicting the issue's intent (mode 2 = "uses the `minotari` crate directly: local UTXO selection, `sign_locked_transaction`, broadcast via HTTP RPC to a base node. No external wallet process.").
- **B0 logic disconnected from `WalletMode` trait** — gemini flagged this; author replied that `run_b0` impls "currently have `todo!()` placeholders … will call into the b0_baseline module once gRPC/library integration is complete (Phase 3)." So at HEAD, B0 panics for at least two of three modes.

**Strengths to acknowledge:**
- Most thorough README, CI, and config plumbing of the two.
- Author is responsive — turned around 5 gemini comments in 2 days.
- Architecture is recognizable and idiomatic.

## PR #4 in detail

**Author:** @sanrishi (Sanchit Rishi). New account, no prior tari-project activity.

**What it attempts:**
- Cargo workspace with a `harness/` sub-crate.
- Vendors **5 proto files in `harness/proto/`** (`network.proto`, `sidechain_types.proto`, `transaction.proto`, `types.proto`, `wallet.proto`) totaling ~3,400 lines.
- `WalletDriver` trait in `harness/src/driver.rs` + three driver impls in `harness/src/drivers/`.
- Single big `scenarios.rs` (414 lines).
- A `baseline_profile.json` is committed but is **1 line** (stub or empty object).
- PR body is two lines: *"implementation ....\nCloses #1"*.

**What's missing vs. the AC:**
- **Build broken.** Gemini's first review explicitly says: *"invalid dependency versions in `harness/Cargo.toml` that will cause build failures."* No follow-up commit visible.
- **Vendoring protos is the wrong shape.** The Tari repo updates these protos; vendored copies will rot. Should depend on `minotari_app_grpc` as a crate, not copy-paste protos.
- **`env!("CARGO_MANIFEST_DIR")` for config path** — flagged as making the binary non-portable.
- **Stubs everywhere.** `new_wallet.rs` is 53 lines (vs. PR #3's 810); `payment_processor.rs` is 61 lines. Functionally the only real driver is `old_wallet.rs` (255 lines).
- **Multiple correctness bugs flagged by gemini and not resolved:** hardcoded output metrics in legacy driver, balance-delta calculation bug, race conditions in process management, invalid address formats in scenarios, resource leak in wallet process management, missing protobuf file.
- **No CI.**
- **No PR description.** Two lines. No AC mapping. No usage instructions.
- **Baseline profile is a 1-line stub** — fails the AC's "proof-of-run" intent.

**Strengths to acknowledge:**
- Workspace layout (separate `harness/` crate) is cleaner than PR #3's flat `src/` if the harness ever ships alongside other tools in this repo.
- Has at least the *idea* of a committed baseline profile, even if the contents are stub.

## Maintainer state

- **@SWvheerden authored the issue** (2026-05-18 14:17 UTC) and last commented on the thread (`gh issue view 1 -R tari-project/wallet-benchmarks` shows him as the last of 9 comment authors). He has not reviewed either PR.
- His recent tari merges (#7844 today, #7841 2026-05-19, #7836/7835/7834 2026-05-15) show conventional-commits subjects, terse PR bodies, narrow scope per commit. Mirror this on our PR.
- He triggered no CI runs on either competing PR — every check shown is GitGuardian only. Either CI requires maintainer approval for fork-PR Cargo workflows (likely) or he hasn't engaged yet. Either way, **neither PR has demonstrated CI-green status**.

## Our positioning

What both competitors fail to deliver, and where we can win:

1. **Actual proof-of-run.** The AC says: *"A baseline result profile is committed alongside the harness code, serving as proof-of-run and reference baseline."* Neither PR has executed against Esmeralda. **A working, committed baseline JSON with real numbers beats any framework polish.** This is the single most differentiating thing we can produce.

2. **Faithful Mode 2 (`minotari-cli` library).** The issue explicitly says Mode 2 uses "the `minotari` crate directly: local UTXO selection, `sign_locked_transaction`, broadcast via HTTP RPC to a base node. **No external wallet process.**" PR #3 silently drops back to the old wallet for signing; PR #4 doesn't really implement it. Get this right.

3. **Don't vendor protos (PR #4's mistake).** Depend on `minotari_app_grpc` from the tari workspace at a pinned commit/tag, the way the rest of the Tari ecosystem does. Recorded in result profile per AC.

4. **Don't stub end-to-end behavior and ship it (PR #3's mistake).** If a scenario isn't runnable against Esmeralda, it shouldn't be in the harness yet. Better to ship 5 working scenarios with real numbers than 9 with `todo!()`.

5. **Implementation-language judgment is open.** The issue says: *"Implementation approach is up to you — Rust, Python, shell scripts, or any combination — as long as the results are reproducible and verifiable by a third party."* Both competitors went Rust monolith. We should evaluate during Architect phase whether a Python orchestrator wrapping `minotari_console_wallet` CLI + `minotari-cli` as a library subprocess is cleaner. Rust likely wins on type-safety for tonic and direct use of `minotari` as a crate — but the judgment is not yet locked.

6. **Honor the "Harness Measures, Does Not Engineer Around Wallet Pain" principle.** S4 (concurrent construction) is where the bounty bites. PR #3 *claims* to measure serialization gaps, but with no end-to-end run we don't know if it actually does. Neither PR shows S4 output. **Producing real S4 data — even if it shows the wallet failing or stalling — is high-value evidence.**

7. **PR shape.** Single descriptive PR title in Conventional Commits style, AC checklist with one-line evidence each, `## QA Results` block with the actual baseline JSON path, `## Risk Surface: none` per CLAUDE.md.

## Recheck before pushing our PR

- `gh pr list -R tari-project/wallet-benchmarks --state open` — confirm no new entrants and no fresh commits on #3/#4.
- `gh issue view 1 -R tari-project/wallet-benchmarks --comments` — confirm SWvheerden has not pre-committed to either PR.
- If SWvheerden has reviewed either PR in the meantime, re-read his review — it tells us exactly what he wants and may force scope adjustments.
