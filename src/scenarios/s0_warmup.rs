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
//! the outcome's `t_confirm_ms` carries `None` and `status = "timeout"` —
//! the cell is recorded raw per AC-30/31/32/33.

use std::time::Duration;

use crate::modes::{Mode, TxRecord};
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
/// `resolve_for(mode, 0)` — S0 dispatches exactly one tx so the index is
/// always 0. The recipient is not derived by S0 itself because the
/// `Mode` trait does not expose an "own address" method (recorded in
/// `analysis/API_DRIFT.md §3i.1.b`); production callers wire the
/// harness-controlled "second mode-2 seed" address via
/// `RecipientStrategy::Fixed` per `DESIGN.md §Scenario state machine §S0`.
pub(super) async fn run(ctx: &ScenarioCtx<'_>, mode: &mut dyn Mode) -> anyhow::Result<S0Outcome> {
    let config = ctx.config;
    let recipient = ctx.recipients.resolve_for(mode, 0)?;

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
        peak_rss_bytes: None,
        peak_cpu_pct: None,
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
        assert_eq!(
            calls.last().copied(),
            Some("get_balance"),
            "post-state balance read last",
        );
        // Every call after `send_single` and before the final `get_balance`
        // must be a `get_utxo_count` (the confirmation poll).
        for c in &calls[3..calls.len() - 1] {
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
}
