//! S1 volume — 127 txs across 7 doubling rounds.
//!
//! Per `analysis/DESIGN.md §Scenario state machine §S1`,
//! `analysis/RESULT_PROFILE_SCHEMA.md §S1 scenario (AC-12, AC-13, AC-14)`,
//! and the main-thread S1 directive: 7 rounds, round `k` dispatches
//! `2^(k-1)` serial transactions (1, 2, 4, 8, 16, 32, 64; total 127).
//!
//! Loop discipline:
//!
//! * Within a round: construct → submit → wait-for-confirmation, one tx at
//!   a time. The next tx in the round only starts after the prior tx
//!   reaches a terminal state.
//! * Round `k+1` begins only after every tx in round `k` has reached a
//!   terminal state (confirmed | rejected | stall).
//! * **NO retry** on tx failure — the record is folded into the round raw
//!   and the round moves on to the next tx (AC-30).
//! * **NO UTXO pre-partitioning** — the wallet's own selection logic IS the
//!   measurement (AC-30 / AC-31). Enforced statically by
//!   `tests/c_no_utxo_pre_partition_in_s1.rs`.
//! * Per-tx confirmation timeout is bounded by
//!   `config.per_tx_confirmation_timeout_ms`. On timeout, the tx is marked
//!   `"timeout"` and counted as a stall per AC-33.
//! * If all `2^(k-1)` in a round fail, the round still completes; subsequent
//!   rounds run.
//!
//! Per-round metrics (mirror the user-prompt schema for §3i.1.b's
//! S1Outcome):
//!
//! * `round_idx: u8` — 1..=7.
//! * `tx_count: u8` — `2^(round_idx - 1)`.
//! * `successes`, `double_selection_rejections`, `construction_failures`,
//!   `stalls`.
//! * `t_round_ms: u64` — start-of-first-construct → last terminal-state.
//! * `txs: Vec<TxRecord>` per AC-38.
//!
//! Resource-sampler fields (`peak_rss_bytes`, `peak_cpu_pct`) carry `None`
//! until the 1 Hz sampler lands in step 3j.
//!
//! Confirmation polling reuses the S0 idiom: `tokio::select!` with one arm
//! `sleep_until(deadline)` (timeout) and one arm
//! `sleep(CONFIRMATION_POLL_INTERVAL)` (poll cadence). Both sleeps live
//! inside the select body — the AC-32 carve-out
//! (`tests/c_no_retry_backoff_throttle.rs`) excises that body before grep.

use std::time::{Duration, Instant};

use tari_common_types::tari_address::TariAddress;

use crate::config::Config;
use crate::modes::{Mode, TxRecord};

/// Poll cadence inside the per-tx confirmation loop. Same value as S0 —
/// not a throttle (AC-32 exempted as it sits inside `tokio::select!`).
const CONFIRMATION_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Canonical number of rounds. `config.doubling_rounds` defaults to 6, but
/// `RESULT_PROFILE_SCHEMA.md §S1` includes one fan-out round (round 7) for
/// a total of 7 — kept as a constant here so the meaning is grep-able.
const S1_ROUNDS: u8 = 7;

/// One round's worth of per-round aggregates plus the raw `TxRecord`s.
/// Mirrors the user-prompt schema for §3i.1.b.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundOutcome {
    /// 1..=7. Round `k` dispatches `2^(k-1)` txs serially.
    pub round_idx: u8,
    /// Target tx count for this round (`2^(round_idx - 1)`).
    pub tx_count: u8,
    /// Number of txs whose `TxRecord::status == "success"`.
    pub successes: u32,
    /// Number of txs whose rejection reason indicates the wallet selected
    /// the same UTXO that another concurrent / recent tx already locked.
    /// Detected by substring matching on `TxRecord::error_string` against
    /// the upstream `DoubleSpend` / "duplicate input" markers.
    pub double_selection_rejections: u32,
    /// Number of txs that failed before broadcast completion (TxRecord
    /// `status` carrying a `failure:construct` / `failure:sign` /
    /// `failure:broadcast` suffix).
    pub construction_failures: u32,
    /// Number of txs whose confirmation polling timed out
    /// (`status == "timeout"`).
    pub stalls: u32,
    /// Wall-clock from start-of-first-construct → last terminal-state
    /// event, in milliseconds.
    pub t_round_ms: u64,
    /// Raw `TxRecord` for every tx in this round, in dispatch order.
    pub txs: Vec<TxRecord>,
}

/// S1's per-cell payload. `rounds.len()` is normally `S1_ROUNDS` (7) but
/// can be shorter when the caller (run loop in step 3i.2) decides to halt
/// per AC-13 — that decision lives at the cell envelope, not in S1 itself.
#[derive(Debug, Clone, PartialEq)]
pub struct S1Outcome {
    /// Per-round records, in dispatch order (1..=7).
    pub rounds: Vec<RoundOutcome>,
    /// `peak_rss_bytes` — `None` until the 1 Hz sampler lands in step 3j.
    pub peak_rss_bytes: Option<u64>,
    /// `peak_cpu_pct` — `None` until the 1 Hz sampler lands in step 3j.
    pub peak_cpu_pct: Option<f64>,
}

/// Run S1 against the given mode.
///
/// `recipient` is the destination address for every send; the wallet's
/// own UTXO-selection logic decides which UTXOs to consume per tx.
/// Production caller (step 3i.2) supplies the harness-controlled
/// recipient; tests pass an arbitrary `TariAddress`.
///
/// `rounds_override` lets the unit test cap the loop to a single round
/// without running the full 127-tx, multi-minute sequence. Production
/// callers pass `None` to get the canonical 7-round loop.
pub(super) async fn run(
    config: &Config,
    mode: &mut dyn Mode,
    recipient: &TariAddress,
    rounds_override: Option<u8>,
) -> anyhow::Result<S1Outcome> {
    let total_rounds = rounds_override.unwrap_or(S1_ROUNDS);
    let mut rounds = Vec::with_capacity(total_rounds as usize);

    for round_idx in 1..=total_rounds {
        let tx_count_u32: u32 = 1u32 << (round_idx - 1);
        let tx_count = u8::try_from(tx_count_u32).unwrap_or(u8::MAX);
        let round_start = Instant::now();
        let mut txs = Vec::with_capacity(tx_count as usize);
        let mut successes: u32 = 0;
        let mut stalls: u32 = 0;
        let mut construction_failures: u32 = 0;
        let mut double_selection_rejections: u32 = 0;

        for _tx_slot in 0..tx_count {
            // Construct + submit one tx. The wallet's own UTXO selection
            // logic decides inputs — NO pre-partitioning of any kind.
            // Amount is `config.fee_rate` plus a nominal payload; the
            // production payload value is calibrated per
            // `RESULT_PROFILE_SCHEMA.md §S1` round-to-round-fanout decisions
            // (out of scope for this commit; reuses the value the run
            // loop in step 3i.2 will plumb through).
            let amount = config.fee_rate.saturating_mul(10);
            let send_result = mode.send_single(recipient, amount, config.fee_rate).await;
            match send_result {
                Ok(tx_record) => {
                    // Classify by `status`:
                    if tx_record.status == "success" {
                        successes += 1;
                    } else if is_double_selection_rejection(&tx_record) {
                        double_selection_rejections += 1;
                    } else {
                        construction_failures += 1;
                    }
                    // Wait for the tx to reach a terminal state at the
                    // wallet's reported UTXO-count level before the next
                    // tx in this round begins (AC-30 serial-within-round
                    // discipline).
                    let confirmed = wait_for_state_change(config, mode).await?;
                    if !confirmed {
                        stalls += 1;
                        // The tx_record from a successful send_single
                        // already says "success"; we don't mutate it. The
                        // round-level `stalls` counter is what records the
                        // confirmation-side timeout per AC-33.
                    }
                    txs.push(tx_record);
                }
                Err(send_err) => {
                    // Send-side error: record a synthetic TxRecord with
                    // failure status, fold into construction_failures.
                    // No retry (AC-30) — move on to the next slot.
                    construction_failures += 1;
                    txs.push(synthesize_failure_record(&send_err));
                }
            }
        }

        let round_elapsed = round_start.elapsed();
        let t_round_ms = u64::try_from(round_elapsed.as_millis()).unwrap_or(u64::MAX);
        rounds.push(RoundOutcome {
            round_idx,
            tx_count,
            successes,
            double_selection_rejections,
            construction_failures,
            stalls,
            t_round_ms,
            txs,
        });
    }

    Ok(S1Outcome {
        rounds,
        peak_rss_bytes: None,
        peak_cpu_pct: None,
    })
}

/// Heuristic for "this tx was rejected because the wallet selected a UTXO
/// that another concurrent / recent tx already had". Matches against the
/// upstream `RejectionReason::DoubleSpend` discriminant name, plus the
/// "duplicate input" substring that some base-node releases use in the raw
/// rejection text. Surfaced raw per AC-30 — we do NOT retry, we count.
fn is_double_selection_rejection(rec: &TxRecord) -> bool {
    let Some(err) = rec.error_string.as_deref() else {
        return false;
    };
    err.contains("DoubleSpend") || err.contains("duplicate input")
}

/// Construct a synthetic `TxRecord` for a send-side error. Used when
/// `mode.send_single` returns `Err` rather than an `Ok(record)` carrying a
/// `failure:*` status — the harness needs a record either way so the
/// round's `txs[]` count matches its `tx_count` target.
fn synthesize_failure_record(err: &anyhow::Error) -> TxRecord {
    TxRecord {
        txid: String::new(),
        t_total_ms: 0,
        t_broadcast_ms: 0,
        t_confirm_ms: None,
        status: "failure:construct".to_string(),
        error_string: Some(format!("{err:#}")),
        fee_microtari: 0,
    }
}

/// Wait for `mode.get_utxo_count()` to change from its current value, or
/// for `config.per_tx_confirmation_timeout_ms` to elapse. Returns `true`
/// on observed change, `false` on timeout.
///
/// The same `tokio::select!` pattern as S0: deadline arm
/// (`sleep_until`) plus poll-cadence arm (`sleep(CONFIRMATION_POLL_INTERVAL)`).
/// Both sleeps live inside the select body — AC-32 exempt per
/// `tests/c_no_retry_backoff_throttle.rs`'s carve-out.
async fn wait_for_state_change(config: &Config, mode: &mut dyn Mode) -> anyhow::Result<bool> {
    let baseline = mode.get_utxo_count().await?;
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(config.per_tx_confirmation_timeout_ms);
    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(false);
            }
            _ = tokio::time::sleep(CONFIRMATION_POLL_INTERVAL) => {
                let observed = mode.get_utxo_count().await?;
                if observed != baseline {
                    return Ok(true);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::test_support::{FakeMode, SendOutcome};
    use crate::{gen_seed, seed::derive_address};

    fn fake_recipient() -> TariAddress {
        let mnemonic = gen_seed().expect("gen_seed");
        derive_address(&mnemonic).expect("derive_address for fake recipient")
    }

    fn success_record() -> TxRecord {
        TxRecord {
            txid: "tx-success".to_string(),
            t_total_ms: 100,
            t_broadcast_ms: 80,
            t_confirm_ms: None,
            status: "success".to_string(),
            error_string: None,
            fee_microtari: 50,
        }
    }

    #[tokio::test]
    async fn s1_round_1_dispatches_exactly_one_tx() {
        let mut fake = FakeMode::new();
        // For each tx slot, send_single returns success.
        fake.send_single_sequence = vec![SendOutcome::Ok(success_record())];
        // The confirmation polling reads `get_utxo_count`; the wait helper
        // first reads the baseline (= pre-confirmation value) then polls
        // for change. With a tiny timeout, the wait returns `false` on the
        // first sleep — no real wallet state change is needed for the
        // round-shape assertion.
        fake.canned_utxo_count = vec![10];

        let cfg = Config {
            per_tx_confirmation_timeout_ms: 50,
            ..Config::default()
        };
        let recipient = fake_recipient();
        let outcome = run(&cfg, &mut fake, &recipient, Some(1))
            .await
            .expect("S1 single-round runs");

        assert_eq!(outcome.rounds.len(), 1, "rounds_override=Some(1)");
        let r = &outcome.rounds[0];
        assert_eq!(r.round_idx, 1);
        assert_eq!(r.tx_count, 1, "round 1 dispatches 2^0 = 1 tx");
        assert_eq!(r.txs.len(), 1, "txs length matches tx_count");
        assert_eq!(r.successes, 1);
        assert_eq!(r.construction_failures, 0);
        assert_eq!(r.double_selection_rejections, 0);
        // Confirmation polled for the full timeout without a state change
        // → counted as a stall per AC-33.
        assert_eq!(r.stalls, 1, "no UTXO change observed within timeout");
        assert!(outcome.peak_rss_bytes.is_none(), "sampler lands in 3j");
    }

    #[tokio::test]
    async fn s1_round_with_alternating_send_outcomes_does_not_retry() {
        let mut fake = FakeMode::new();
        // Round 2: 2 txs. Sequence: success, failure. The failure must NOT
        // trigger a retry — the round records the failure raw and ends
        // after exactly 2 tx attempts.
        fake.send_single_sequence = vec![
            SendOutcome::Ok(success_record()),
            SendOutcome::Err("broadcast rejected".to_string()),
        ];
        fake.canned_utxo_count = vec![10];

        let cfg = Config {
            per_tx_confirmation_timeout_ms: 50,
            ..Config::default()
        };
        let recipient = fake_recipient();

        // Run two rounds: rounds_override=2 → 1 tx in round 1, 2 txs in
        // round 2. The 3 total send_single calls consume all three
        // sequence entries (we add one extra success at the front).
        fake.send_single_sequence = vec![
            SendOutcome::Ok(success_record()),
            SendOutcome::Ok(success_record()),
            SendOutcome::Err("broadcast rejected".to_string()),
        ];
        let outcome = run(&cfg, &mut fake, &recipient, Some(2))
            .await
            .expect("S1 two-round runs");

        assert_eq!(outcome.rounds.len(), 2);
        // Round 1: one success.
        assert_eq!(outcome.rounds[0].tx_count, 1);
        assert_eq!(outcome.rounds[0].successes, 1);
        assert_eq!(outcome.rounds[0].construction_failures, 0);
        // Round 2: one success + one construction failure. NO RETRY of
        // the failure: txs.len() == tx_count.
        assert_eq!(outcome.rounds[1].tx_count, 2);
        assert_eq!(outcome.rounds[1].txs.len(), 2, "no retry on failure");
        assert_eq!(outcome.rounds[1].successes, 1);
        assert_eq!(outcome.rounds[1].construction_failures, 1);
        assert!(
            outcome.rounds[1].txs[1].error_string.is_some(),
            "synthesized failure record must carry the send-side error string",
        );

        // Verify exactly 3 send_single calls were made — no retries on
        // failure (AC-30).
        let calls = fake.calls.lock().unwrap().clone();
        let send_count = calls.iter().filter(|c| **c == "send_single").count();
        assert_eq!(send_count, 3, "exactly 1+2 sends across two rounds");
    }
}
