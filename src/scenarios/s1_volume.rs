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
//!   `config.per_tx_confirmation_timeout_ms`. On timeout, the tx is counted
//!   under cell-level `stall_count` per AC-33.
//! * If all `2^(k-1)` in a round fail, the round still completes; subsequent
//!   rounds run.
//!
//! Cell-level metrics (per `RESULT_PROFILE_SCHEMA.md` lines 112-116, the
//! universal error sub-object that applies to EVERY scenario):
//!
//! * `success_count: u64` — successful tx submissions in this scenario.
//! * `rejection_count: u64` — mempool/validation rejection per base-node
//!   response. Collapses DoubleSpend, FeeTooLow, ValidationFailed, etc.
//! * `stall_count: u64` — tx accepted but unconfirmed past timeout (AC-33).
//! * `timeout_count: u64` — S4 budget timeout / harness-side timeout.
//!   S1 has no budget timeout — field stays 0 for schema uniformity.
//! * `details: Vec<DetailRecord>` — one entry per non-success event.
//!
//! Per-round metrics (per `RESULT_PROFILE_SCHEMA.md` line 156):
//!
//! * `round_idx: u32` — 1..=7.
//! * `tx_count: u32` — `2^(round_idx - 1)`.
//! * `failure_count: u32` — `count(tx_records[].status != "success")`.
//! * `t_round_ms: u64` — start-of-first-construct → last terminal-state.
//! * `tx_records: Vec<TxRecord>` per AC-38.
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
use crate::scenarios::{DetailPhase, DetailRecord};

/// Poll cadence inside the per-tx confirmation loop. Same value as S0 —
/// not a throttle (AC-32 exempted as it sits inside `tokio::select!`).
const CONFIRMATION_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Canonical number of rounds. `config.doubling_rounds` defaults to 6, but
/// `RESULT_PROFILE_SCHEMA.md §S1` includes one fan-out round (round 7) for
/// a total of 7 — kept as a constant here so the meaning is grep-able.
const S1_ROUNDS: u8 = 7;

/// One round's worth of per-round aggregates plus the raw `TxRecord`s.
/// Mirrors `RESULT_PROFILE_SCHEMA.md` line 156: `failure_count` is the only
/// per-round counter — finer-grained partitions (rejection vs. construct
/// failure vs. stall) roll up to the cell-level [`S1Outcome`] counters.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundOutcome {
    /// 1..=7. Round `k` dispatches `2^(k-1)` txs serially.
    pub round_idx: u32,
    /// Target tx count for this round (`2^(round_idx - 1)`).
    pub tx_count: u32,
    /// `count(tx_records[].status != "success")` per schema line 156.
    pub failure_count: u32,
    /// Wall-clock from start-of-first-construct → last terminal-state
    /// event, in milliseconds.
    pub t_round_ms: u64,
    /// Raw `TxRecord` for every tx in this round, in dispatch order.
    /// Renamed from `txs` to match schema line 150's `tx_records[]`.
    pub tx_records: Vec<TxRecord>,
}

/// S1's per-cell payload. `rounds.len()` is normally `S1_ROUNDS` (7) but
/// can be shorter when the caller (run loop in step 3i.2) decides to halt
/// per AC-13 — that decision lives at the cell envelope, not in S1 itself.
///
/// Cell-level counter fields are the universal `errors` sub-object from
/// `RESULT_PROFILE_SCHEMA.md` lines 112-116 — they apply to EVERY scenario,
/// so the field set here is shared by S0/S2/S3/S4/S5/S6/S7 in shape.
#[derive(Debug, Clone, PartialEq)]
pub struct S1Outcome {
    /// Per-round records, in dispatch order (1..=7).
    pub rounds: Vec<RoundOutcome>,
    /// Successful tx submissions across all rounds in this scenario.
    /// Schema line 112.
    pub success_count: u64,
    /// Mempool/validation rejection per base-node response. Schema line 113.
    /// Collapses DoubleSpend / FeeTooLow / ValidationFailed / AlreadyMined /
    /// Orphan / TimeLocked into a single counter; the specific rejection
    /// reason rides on the corresponding `tx_record.error_string` and the
    /// `DetailRecord` entry pushed onto `details`.
    pub rejection_count: u64,
    /// Tx accepted by base node but unconfirmed past
    /// `config.per_tx_confirmation_timeout_ms`. Schema line 114, AC-33.
    pub stall_count: u64,
    /// S4 budget timeout / harness-side timeout. Schema line 115. S1 has
    /// no budget timeout — this counter stays 0; field kept for schema
    /// uniformity across all scenarios.
    pub timeout_count: u64,
    /// One entry per non-success event. Schema line 116. Construct- and
    /// sign-phase failures (which never reach the base node) live here
    /// only — they do NOT increment `rejection_count`.
    pub details: Vec<DetailRecord>,
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
    let mut success_count: u64 = 0;
    let mut rejection_count: u64 = 0;
    let mut stall_count: u64 = 0;
    let mut details: Vec<DetailRecord> = Vec::new();

    for round_idx in 1..=total_rounds {
        let tx_count: u32 = 1u32 << (round_idx - 1);
        let round_start = Instant::now();
        let mut tx_records = Vec::with_capacity(tx_count as usize);

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
                    // Classify by `status` per the universal cell-level
                    // counters in `RESULT_PROFILE_SCHEMA.md` lines 112-116.
                    let txid_opt = if tx_record.txid.is_empty() {
                        None
                    } else {
                        Some(tx_record.txid.clone())
                    };
                    if tx_record.status == "success" {
                        // Tentative success — confirmation polling below
                        // may downgrade this to a stall per AC-33.
                        let confirmed = wait_for_state_change(config, mode).await?;
                        if confirmed {
                            success_count += 1;
                        } else {
                            // AC-33: accepted but unconfirmed within
                            // `per_tx_confirmation_timeout_ms` → stall.
                            // Strict mutual exclusion with success.
                            stall_count += 1;
                        }
                    } else {
                        // Base-node rejection (DoubleSpend, FeeTooLow, etc.).
                        // Schema collapses these into rejection_count;
                        // the specific reason rides on tx_record.error_string.
                        rejection_count += 1;
                        details.push(DetailRecord {
                            txid: txid_opt,
                            error_string: tx_record.error_string.clone().unwrap_or_default(),
                            phase: DetailPhase::Broadcast,
                        });
                    }
                    tx_records.push(tx_record);
                }
                Err(send_err) => {
                    // Send-side error: record a synthetic TxRecord with
                    // failure status AND a DetailRecord with phase=Construct
                    // per schema line 116. NO retry (AC-30) — move on.
                    // Construct/Sign/Broadcast-phase failures live in
                    // details[] only — they do NOT increment rejection_count
                    // (which is reserved for base-node rejections per
                    // schema line 113).
                    let error_string = format!("{send_err:#}");
                    details.push(DetailRecord {
                        txid: None,
                        error_string: error_string.clone(),
                        phase: DetailPhase::Construct,
                    });
                    tx_records.push(synthesize_failure_record(&error_string));
                }
            }
        }

        let round_elapsed = round_start.elapsed();
        let t_round_ms = u64::try_from(round_elapsed.as_millis()).unwrap_or(u64::MAX);
        let failure_count =
            u32::try_from(tx_records.iter().filter(|r| r.status != "success").count())
                .unwrap_or(u32::MAX);
        rounds.push(RoundOutcome {
            round_idx: u32::from(round_idx),
            tx_count,
            failure_count,
            t_round_ms,
            tx_records,
        });
    }

    Ok(S1Outcome {
        rounds,
        success_count,
        rejection_count,
        stall_count,
        // S1 has no budget timeout — schema field kept for uniformity.
        timeout_count: 0,
        details,
        peak_rss_bytes: None,
        peak_cpu_pct: None,
    })
}

/// Construct a synthetic `TxRecord` for a send-side error. Used when
/// `mode.send_single` returns `Err` rather than an `Ok(record)` carrying a
/// `failure:*` status — the harness needs a record either way so the
/// round's `tx_records[]` count matches its `tx_count` target.
fn synthesize_failure_record(error_string: &str) -> TxRecord {
    TxRecord {
        txid: String::new(),
        t_total_ms: 0,
        t_broadcast_ms: 0,
        t_confirm_ms: None,
        status: "failure:construct".to_string(),
        error_string: Some(error_string.to_string()),
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
        // round-shape assertion. A `false` return downgrades the tentative
        // success to a stall per AC-33.
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
        assert_eq!(r.tx_records.len(), 1, "tx_records length matches tx_count");
        // Per-tx status was "success" on the wire (TxRecord.status), so the
        // per-round failure_count remains 0 — failure_count counts
        // tx_records[].status != "success", and the stall-on-timeout
        // downgrade lives at the cell level per schema lines 112-116.
        assert_eq!(r.failure_count, 0);
        // Cell-level: confirmation polled for the full timeout without a
        // state change → counted as a stall per AC-33. NOT a success.
        assert_eq!(outcome.success_count, 0);
        assert_eq!(
            outcome.stall_count, 1,
            "no UTXO change observed within timeout"
        );
        assert_eq!(outcome.rejection_count, 0);
        assert_eq!(outcome.timeout_count, 0, "S1 has no budget timeout");
        assert!(
            outcome.details.is_empty(),
            "wire-success contributes no details[]"
        );
        assert!(outcome.peak_rss_bytes.is_none(), "sampler lands in 3j");
    }

    #[tokio::test]
    async fn s1_round_with_alternating_send_outcomes_does_not_retry() {
        let mut fake = FakeMode::new();
        // Two rounds: rounds_override=2 → 1 tx in round 1, 2 txs in round 2.
        // Sequence: 2 successes, then a construct-side Err. The Err must
        // NOT trigger a retry — round 2 records the failure raw and ends
        // after exactly 2 tx attempts (3 total send_single calls).
        fake.send_single_sequence = vec![
            SendOutcome::Ok(success_record()),
            SendOutcome::Ok(success_record()),
            SendOutcome::Err("broadcast rejected".to_string()),
        ];
        fake.canned_utxo_count = vec![10];

        let cfg = Config {
            per_tx_confirmation_timeout_ms: 50,
            ..Config::default()
        };
        let recipient = fake_recipient();

        let outcome = run(&cfg, &mut fake, &recipient, Some(2))
            .await
            .expect("S1 two-round runs");

        assert_eq!(outcome.rounds.len(), 2);
        // Round 1: one wire-success.
        assert_eq!(outcome.rounds[0].tx_count, 1);
        assert_eq!(outcome.rounds[0].failure_count, 0);
        // Round 2: one wire-success + one construct failure. NO RETRY of
        // the failure: tx_records.len() == tx_count.
        assert_eq!(outcome.rounds[1].tx_count, 2);
        assert_eq!(outcome.rounds[1].tx_records.len(), 2, "no retry on failure");
        // failure_count counts tx_records[].status != "success" — the
        // synthesized construct failure has status "failure:construct".
        assert_eq!(outcome.rounds[1].failure_count, 1);
        assert!(
            outcome.rounds[1].tx_records[1].error_string.is_some(),
            "synthesized failure record must carry the send-side error string",
        );

        // Cell-level counters (universal per schema lines 112-116):
        // - All 3 wire-successes downgrade to stalls (canned_utxo_count
        //   never advances), so success_count=0, stall_count=2 (for the
        //   two Ok send outcomes — the third was an Err and never reaches
        //   the confirmation poll).
        assert_eq!(outcome.success_count, 0);
        assert_eq!(outcome.stall_count, 2);
        assert_eq!(outcome.rejection_count, 0);
        assert_eq!(outcome.timeout_count, 0);
        // Exactly one DetailRecord for the construct-side Err, phase=Construct.
        assert_eq!(outcome.details.len(), 1);
        assert_eq!(outcome.details[0].phase, DetailPhase::Construct);
        assert!(outcome.details[0].txid.is_none());
        assert!(outcome.details[0]
            .error_string
            .contains("broadcast rejected"));

        // Verify exactly 3 send_single calls were made — no retries on
        // failure (AC-30).
        let calls = fake.calls.lock().unwrap().clone();
        let send_count = calls.iter().filter(|c| **c == "send_single").count();
        assert_eq!(send_count, 3, "exactly 1+2 sends across two rounds");
    }
}
