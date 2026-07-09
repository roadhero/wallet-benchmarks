//! S0 warmup — single funding-style transaction; produces `h_birth`.
//!
//! Per `analysis/DESIGN.md §Scenario state machine §S0` and
//! `analysis/RESULT_PROFILE_SCHEMA.md §S0 scenario (AC-11)`:
//!
//! Preconditions: B0 complete, wallet still imported.
//!
//! Steps:
//!
//! 1. Read pre-state (`mode.get_balance()`, `mode.get_utxo_count()`).
//! 2. Construct + broadcast ONE tx of `config.a_fund / 127` microTari to the
//!    caller-supplied recipient (the harness-controlled "second mode-2 seed"
//!    in production; an arbitrary test address in unit tests).
//! 3. Poll `mode.get_utxo_count()` until the count changes (the wallet has
//!    matured the tx past its own configured confirmation depth), bounded by
//!    `config.per_tx_confirmation_timeout_ms`. The poll is a
//!    `tokio::select!` with `tokio::time::sleep` as the deadline arm only —
//!    NOT a serialization mechanism (AC-32 exemption documented at the loop
//!    site).
//! 4. Read post-state.
//! 5. Record per-schema deltas + the underlying `TxRecord`.
//!
//! Verification: AC-11. Result fields: `pre_balance`, `post_balance`,
//! `balance_delta`, `pre_utxo_count`, `post_utxo_count`, `utxo_delta`,
//! `t_construct_ms`, `t_broadcast_ms`, `t_confirm_ms`, `tx_record`,
//! `peak_rss_bytes`, `peak_cpu_pct`.
//!
//! Failure-halt: yes — S0 must succeed for S1 to have funds. This fn returns
//! `Err` on send-side failure or post-state-read failure; the caller (the
//! run loop in step 3i.2) maps that to a halt of the remaining scenarios
//! per `DESIGN.md §Scenario state machine §S0`.
//!
//! **No retry, no backoff, no throttle.** Single submit; single confirmation
//! wait. If the wallet doesn't see the tx within `per_tx_confirmation_timeout_ms`
//! the outcome's `t_confirm_ms` carries `None` and the cell is recorded raw
//! per AC-30/31/32/33. Note the CELL envelope status stays `"success"`
//! (scenario ran to completion and produced its measurement, same as
//! B0/S2/S3/S6/S7 per `result_profile::outcome_to_envelope_json`); the
//! confirmation run-out is carried by `t_confirm_ms: null`, NOT by a
//! `"timeout"` status. An earlier revision of this doc claimed a
//! `"timeout"` cell status; that string is `s4_status`'s, and this comment
//! previously misattributed it. S1/S4 differ by design: they aggregate
//! MANY sends, so their envelopes derive status from per-send
//! terminal-state counters (`stall_count` etc.) that a single-send warmup
//! does not have.

use std::time::Duration;

use anyhow::Context;

use crate::modes::{Mode, TxRecord};
use crate::sampler::Pid;
use crate::scenarios::ScenarioCtx;

/// S0's per-cell payload, matching `RESULT_PROFILE_SCHEMA.md §S0` plus the
/// derived deltas the run loop reads to determine S0 success (AC-11
/// halt-or-continue contract).
///
/// `peak_rss_bytes` and `peak_cpu_pct` are `None` until the 1 Hz metrics
/// sampler lands (step 3j); the schema permits `null` for both.
#[derive(Debug, Clone, PartialEq)]
pub struct S0Outcome {
    /// Wallet balance reported before broadcast, in microTari.
    pub pre_balance: u64,
    /// Wallet balance reported after the post-broadcast state change was
    /// observed (or after `per_tx_confirmation_timeout_ms` elapsed without
    /// the change being seen — `t_confirm_ms` then carries `None`).
    pub post_balance: u64,
    /// `post_balance` as `i64` minus `pre_balance` as `i64`. Signed because
    /// a wallet that sends to a foreign recipient observes a negative delta.
    pub balance_delta: i64,
    /// UTXO count reported before broadcast.
    pub pre_utxo_count: u64,
    /// UTXO count reported after the post-broadcast state change.
    pub post_utxo_count: u64,
    /// `post_utxo_count` as `i64` minus `pre_utxo_count` as `i64`.
    pub utxo_delta: i64,
    /// Time the underlying `mode.send_single` spent on construction
    /// (`tx_record.t_total_ms - tx_record.t_broadcast_ms`). `None` when the
    /// underlying record's `t_broadcast_ms` exceeds `t_total_ms` (which
    /// would indicate a measurement bug — surfaced rather than panicking).
    pub t_construct_ms: Option<u64>,
    /// Mode-reported `t_broadcast_ms` — time from `mode.send_single` entry
    /// to broadcast-call completion.
    pub t_broadcast_ms: u64,
    /// Time from broadcast-call completion to observed post-state change.
    /// `None` when the timeout elapsed without observing the change.
    pub t_confirm_ms: Option<u64>,
    /// The underlying `TxRecord` returned by `mode.send_single`. Scenario
    /// layer folds this verbatim into the per-cell `tx_records[]` per
    /// `RESULT_PROFILE_SCHEMA.md §4`.
    pub tx_record: TxRecord,
    /// `peak_rss_bytes` — `None` until the 1 Hz sampler lands in step 3j.
    pub peak_rss_bytes: Option<u64>,
    /// `peak_cpu_pct` — `None` until the 1 Hz sampler lands in step 3j.
    pub peak_cpu_pct: Option<f64>,
}

/// Poll-interval for `mode.get_utxo_count` in the confirmation loop. Not a
/// throttle / backoff (those are forbidden by AC-30/32) — it is a courtesy
/// interval between calls to the wallet's read endpoint, scoped only by
/// `config.per_tx_confirmation_timeout_ms`. Picked from the maintainer-idiom
/// 2-second readiness-poll cadence in `wallet_lifecycle::console_wallet`.
const CONFIRMATION_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Run S0 against the given mode.
///
/// Reads the destination address from `ctx.recipients` via
/// `resolve_for(ctx.seeds, 0)` — S0 dispatches exactly one tx so the index
/// is always 0. Production callers wire the harness-controlled
/// "second mode-2 seed" address via `RecipientStrategy::Fixed` per
/// `DESIGN.md §Scenario state machine §S0`; the parametrized
/// `RecipientStrategy::SelfAddress(SeedRole)` variant is reserved for the
/// send-to-self scenarios (S4/S6/S7).
pub(super) async fn run(ctx: &ScenarioCtx<'_>, mode: &mut dyn Mode) -> anyhow::Result<S0Outcome> {
    let config = ctx.config;
    let sampler = ctx.sampler_factory.map(|f| {
        f.start(
            Pid(mode.target_pid_for_sampling()),
            ctx.config.sampler_interval_ms,
        )
    });

    let recipient = ctx.recipients.resolve_for(ctx.seeds, 0)?;

    let pre_balance = mode.get_balance().await?;
    let pre_utxo_count = mode.get_utxo_count().await?;

    // `a_fund / 127`: S1 multiplies this funding UTXO into 128 outputs across
    // 7 doubling rounds (1+2+4+8+16+32+64 = 127 send-side txs producing 128
    // final UTXOs). The single S0 tx must carry enough value that the wallet
    // can split it 128 ways net of fees; integer division here biases the
    // amount slightly low, leaving the fee-headroom from `enforce_funding`'s
    // 10% margin intact.
    let amount = config.a_fund / 127;
    let tx_record = mode
        .send_single(&recipient, amount, config.fee_rate)
        .await?;

    // `tokio::select!` confirmation loop. Every sleep inside the select arms
    // is a **deadline** or a **poll interval bound** — not a throttle or
    // backoff. AC-32 (per `analysis/DESIGN_ADDENDUM.md §S3` and the
    // `tests/c_no_retry_backoff_throttle.rs` carve-out) excises
    // `tokio::select! { ... }` bodies before grepping; both the
    // `sleep_until(deadline)` deadline arm and the
    // `sleep(CONFIRMATION_POLL_INTERVAL)` cadence arm live entirely inside
    // this select block. `confirm_start` reads through `ctx.clock` so the
    // test suite can drive the confirm-timing measurement deterministically.
    let confirm_start = ctx.clock.now();
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(config.per_tx_confirmation_timeout_ms);

    let mut current_utxo = pre_utxo_count;
    let (post_utxo_count, t_confirm_ms) = loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                // Timeout: record the last observed UTXO count and `None`
                // for the confirm time per AC-33's "raw, do not retry" rule.
                break (current_utxo, None);
            }
            _ = tokio::time::sleep(CONFIRMATION_POLL_INTERVAL) => {
                let observed = mode.get_utxo_count().await?;
                if observed != pre_utxo_count {
                    let elapsed = ctx.clock.now().duration_since(confirm_start);
                    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                    break (observed, Some(elapsed_ms));
                }
                current_utxo = observed;
            }
        }
    };

    let post_balance = mode.get_balance().await?;

    let balance_delta = (post_balance as i64) - (pre_balance as i64);
    let utxo_delta = (post_utxo_count as i64) - (pre_utxo_count as i64);
    let t_construct_ms = tx_record.t_total_ms.checked_sub(tx_record.t_broadcast_ms);
    let t_broadcast_ms = tx_record.t_broadcast_ms;

    let (peak_rss_bytes, peak_cpu_pct) = match sampler {
        Some(s) => s.stop().await,
        None => (None, None),
    };

    // Next-scenario readiness gate, AFTER every S0 measurement is taken so
    // the outcome above is byte-identical with or without the gate. S0's
    // contract is handing S1 a wallet that can fund its sends (module doc,
    // failure-halt rule): on a wallet whose spendable inputs were all
    // locked by this send (e.g. a single-UTXO wallet), S1's first send
    // would otherwise fail at construct with "Funds are pending" and hide
    // the true cause. Per the Mode::settle_after_send contract, an
    // unsettled gate here IS an S0 failure; Modes 1/3 report settled
    // unconditionally (no-op default).
    let settled = mode
        .settle_after_send()
        .await
        .context("S0 settle_after_send")?;
    if !settled {
        anyhow::bail!(
            "S0 send succeeded but its change failed to confirm within the settle \
             deadline; the wallet cannot fund S1 (see Mode::settle_after_send and \
             RUNBOOK section 7.10)",
        );
    }

    Ok(S0Outcome {
        pre_balance,
        post_balance,
        balance_delta,
        pre_utxo_count,
        post_utxo_count,
        utxo_delta,
        t_construct_ms,
        t_broadcast_ms,
        t_confirm_ms,
        tx_record,
        peak_rss_bytes,
        peak_cpu_pct,
    })
}

#[cfg(test)]
mod tests {
    use tari_common_types::tari_address::TariAddress;

    use super::*;
    use crate::clock::RealClock;
    use crate::config::Config;
    use crate::modes::test_support::FakeMode;
    use crate::scenarios::RecipientStrategy;
    use crate::seed::redact::RedactionDenylist;
    use crate::seed::SeedHandle;
    use crate::{gen_seed, seed::derive_address};

    fn fake_recipient() -> TariAddress {
        let mnemonic = gen_seed().expect("gen_seed");
        derive_address(&mnemonic).expect("derive_address for fake recipient")
    }

    fn sample_tx_record() -> TxRecord {
        TxRecord {
            txid: "deadbeef".to_string(),
            t_total_ms: 250,
            t_broadcast_ms: 200,
            t_confirm_ms: None,
            status: "success".to_string(),
            error_string: None,
            fee_microtari: 100,
        }
    }

    #[tokio::test]
    async fn s0_records_pre_and_post_state_deltas() {
        let mut fake = FakeMode::new();
        // Pre-state: 10_000_000_000 microTari, 1 UTXO.
        // Post-state: 9_999_999_900 (one tx out, fee 100), 2 UTXOs (split).
        // get_balance is called twice (pre, post); get_utxo_count is called
        // once for pre-state and then repeatedly in the confirmation loop
        // until the value changes. Saturating-last-value behavior makes the
        // sequence `[1, 2]` model "pre = 1, post = 2 forever after".
        fake.canned_balance = vec![10_000_000_000, 9_999_999_900];
        fake.canned_utxo_count = vec![1, 2];
        fake.canned_send_single = Some(sample_tx_record());

        // Keep the confirmation timeout generous enough that the first poll
        // sees the state change; the poll-interval sleep is real-time but
        // small (2s).
        let cfg = Config {
            per_tx_confirmation_timeout_ms: 10_000,
            ..Config::default()
        };
        let recipient = fake_recipient();
        let seeds = SeedHandle::for_test();
        let redaction = RedactionDenylist::for_test();
        let clock = RealClock;
        let ctx = ScenarioCtx {
            config: &cfg,
            seeds: &seeds,
            redaction: &redaction,
            clock: &clock,
            recipients: RecipientStrategy::Fixed(&recipient),
            sampler_factory: None,
        };

        let outcome = run(&ctx, &mut fake)
            .await
            .expect("S0 runs against the canned FakeMode");

        assert_eq!(outcome.pre_balance, 10_000_000_000);
        assert_eq!(outcome.post_balance, 9_999_999_900);
        assert_eq!(outcome.balance_delta, -100);
        assert_eq!(outcome.pre_utxo_count, 1);
        assert_eq!(outcome.post_utxo_count, 2);
        assert_eq!(outcome.utxo_delta, 1);
        assert_eq!(outcome.t_broadcast_ms, 200);
        assert_eq!(outcome.t_construct_ms, Some(50));
        assert!(
            outcome.t_confirm_ms.is_some(),
            "post-state change observed within timeout, t_confirm_ms must be Some",
        );
        assert_eq!(outcome.tx_record.txid, "deadbeef");
        assert!(outcome.peak_rss_bytes.is_none(), "sampler lands in 3j");
        assert!(outcome.peak_cpu_pct.is_none(), "sampler lands in 3j");

        let calls = fake.calls.lock().unwrap().clone();
        // Expected call order: pre-state balance + utxo_count, send_single,
        // confirmation-poll get_utxo_count(s), final get_balance. The poll
        // count depends on real clock advancement of the
        // `CONFIRMATION_POLL_INTERVAL` sleep, so we assert the start and end
        // explicitly and the substring shape in the middle.
        assert_eq!(calls[0], "get_balance", "pre-state balance first");
        assert_eq!(calls[1], "get_utxo_count", "pre-state utxo count next");
        assert_eq!(calls[2], "send_single", "broadcast the funding tx");
        // The settle gate (next-scenario readiness) is the last call, AFTER
        // every measurement; the post-state balance read comes immediately
        // before it.
        assert_eq!(
            calls.last().copied(),
            Some("settle_after_send"),
            "settle gate runs last",
        );
        assert_eq!(
            calls.get(calls.len() - 2).copied(),
            Some("get_balance"),
            "post-state balance read is the final measurement",
        );
        // Every call after `send_single` and before the post-state
        // `get_balance` + settle gate must be a `get_utxo_count` (the
        // confirmation poll).
        for c in &calls[3..calls.len() - 2] {
            assert_eq!(*c, "get_utxo_count", "confirmation poll body");
        }
    }

    #[tokio::test]
    async fn s0_returns_none_for_t_confirm_ms_on_timeout() {
        let mut fake = FakeMode::new();
        // Pre-state: utxo_count = 1. The fake never advances (single-value
        // sequence sticks), so the confirmation loop never sees a change.
        fake.canned_balance = vec![10_000_000_000];
        fake.canned_utxo_count = vec![1];
        fake.canned_send_single = Some(sample_tx_record());

        // Sub-poll-interval timeout: the deadline fires before the first
        // post-broadcast poll. AC-33 requires we record the timeout raw.
        let cfg = Config {
            per_tx_confirmation_timeout_ms: 50,
            ..Config::default()
        };
        let recipient = fake_recipient();
        let seeds = SeedHandle::for_test();
        let redaction = RedactionDenylist::for_test();
        let clock = RealClock;
        let ctx = ScenarioCtx {
            config: &cfg,
            seeds: &seeds,
            redaction: &redaction,
            clock: &clock,
            recipients: RecipientStrategy::Fixed(&recipient),
            sampler_factory: None,
        };

        let outcome = run(&ctx, &mut fake)
            .await
            .expect("S0 returns Ok on confirmation timeout (raw recording)");

        assert_eq!(outcome.pre_utxo_count, 1);
        assert_eq!(
            outcome.post_utxo_count, 1,
            "no state change observed within timeout",
        );
        assert_eq!(outcome.utxo_delta, 0);
        assert!(
            outcome.t_confirm_ms.is_none(),
            "AC-33 raw-record-timeout: t_confirm_ms is None",
        );
    }

    #[tokio::test]
    async fn s0_propagates_send_failure() {
        let mut fake = FakeMode::new();
        fake.canned_balance = vec![10_000_000_000];
        fake.canned_utxo_count = vec![1];
        fake.fail_with = Some("broadcast rejected".to_string());

        let cfg = Config::default();
        let recipient = fake_recipient();
        let seeds = SeedHandle::for_test();
        let redaction = RedactionDenylist::for_test();
        let clock = RealClock;
        let ctx = ScenarioCtx {
            config: &cfg,
            seeds: &seeds,
            redaction: &redaction,
            clock: &clock,
            recipients: RecipientStrategy::Fixed(&recipient),
            sampler_factory: None,
        };

        let err = run(&ctx, &mut fake)
            .await
            .expect_err("send-side failure must bubble up");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("broadcast rejected"),
            "error must carry the send-side failure: {msg}",
        );
    }

    /// Shared ctx-builder boilerplate for the settle-gate tests.
    fn settle_test_fixture() -> (Config, TariAddress, SeedHandle, RedactionDenylist) {
        let cfg = Config {
            per_tx_confirmation_timeout_ms: 10_000,
            ..Config::default()
        };
        (
            cfg,
            fake_recipient(),
            SeedHandle::for_test(),
            RedactionDenylist::for_test(),
        )
    }

    #[tokio::test]
    async fn s0_calls_settle_after_post_state_reads() {
        // Defect D-1: the gate must run AFTER every measurement (post-state
        // balance read included) so S0's outcome is identical with or
        // without it.
        let mut fake = FakeMode::new();
        fake.canned_balance = vec![10_000_000_000, 9_999_999_900];
        fake.canned_utxo_count = vec![1, 2];
        fake.canned_send_single = Some(sample_tx_record());
        let (cfg, recipient, seeds, redaction) = settle_test_fixture();
        let clock = RealClock;
        let ctx = ScenarioCtx {
            config: &cfg,
            seeds: &seeds,
            redaction: &redaction,
            clock: &clock,
            recipients: RecipientStrategy::Fixed(&recipient),
            sampler_factory: None,
        };
        run(&ctx, &mut fake).await.expect("S0 runs");
        let calls = fake.calls.lock().unwrap();
        let settle_pos = calls
            .iter()
            .position(|c| *c == "settle_after_send")
            .expect("settle_after_send must be called");
        let last_balance_pos = calls
            .iter()
            .rposition(|c| *c == "get_balance")
            .expect("post-state get_balance recorded");
        assert!(
            settle_pos > last_balance_pos,
            "settle must run after the post-state balance read: {calls:?}",
        );
    }

    #[tokio::test]
    async fn s0_succeeds_when_gate_settles() {
        let mut fake = FakeMode::new();
        fake.settle_settled = true;
        fake.canned_balance = vec![10_000_000_000, 9_999_999_900];
        fake.canned_utxo_count = vec![1, 2];
        fake.canned_send_single = Some(sample_tx_record());
        let (cfg, recipient, seeds, redaction) = settle_test_fixture();
        let clock = RealClock;
        let ctx = ScenarioCtx {
            config: &cfg,
            seeds: &seeds,
            redaction: &redaction,
            clock: &clock,
            recipients: RecipientStrategy::Fixed(&recipient),
            sampler_factory: None,
        };
        let outcome = run(&ctx, &mut fake)
            .await
            .expect("settled gate keeps S0 ok");
        assert_eq!(outcome.post_utxo_count, 2);
    }

    #[tokio::test]
    async fn s0_errs_with_settle_reason_when_gate_unsettled() {
        // Per the Mode::settle_after_send contract, S0 maps Ok(false) to a
        // scenario error naming the true cause, because its remaining
        // contract (hand S1 a fundable wallet) is impossible. The
        // downstream alternative is S1 failing at construct with a
        // Funds-pending message that hides why.
        let mut fake = FakeMode::new();
        fake.settle_settled = false;
        fake.canned_balance = vec![10_000_000_000, 9_999_999_900];
        fake.canned_utxo_count = vec![1, 2];
        fake.canned_send_single = Some(sample_tx_record());
        let (cfg, recipient, seeds, redaction) = settle_test_fixture();
        let clock = RealClock;
        let ctx = ScenarioCtx {
            config: &cfg,
            seeds: &seeds,
            redaction: &redaction,
            clock: &clock,
            recipients: RecipientStrategy::Fixed(&recipient),
            sampler_factory: None,
        };
        let err = run(&ctx, &mut fake)
            .await
            .expect_err("unsettled gate must fail S0");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("change failed to confirm") && msg.contains("cannot fund S1"),
            "error must name the true cause: {msg}",
        );
    }
}
