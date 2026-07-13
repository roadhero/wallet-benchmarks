//! S4 concurrent construction — N ∈ {8, 16, 32, 64, 128} concurrent
//! `mode.dispatcher().dispatch(...)` tasks per sub-block, raced against
//! the per-sub-block wall-clock budget.
//!
//! Per `analysis/DESIGN.md §S4 state machine`, `analysis/RESULT_PROFILE_SCHEMA.md
//! §S4 scenario (AC-17, AC-18, AC-30, AC-31, AC-32, AC-33)`, and the 3i.1.f
//! brief: S4 measures how a wallet's UTXO-selection logic behaves under
//! concurrent submission pressure. The harness deliberately does **not**
//! pre-partition the input UTXO set, does **not** semaphore-throttle the
//! tasks, does **not** retry on rejection, and does **not** backoff between
//! tasks (AC-30/31/32). Selection contention surfaces raw as
//! `double_selection_rejections` and `max_serialization_gap_ms`.
//!
//! Sweep: for each `N` in `ctx.config.concurrent_batches` (default
//! `vec![8, 16, 32, 64, 128]` per `Config::default_concurrent_batches`):
//!
//! 1. Acquire `dispatcher = mode.dispatcher()` once before the JoinSet so
//!    every spawned task inherits the same `Arc<dyn S4Dispatcher>` handle.
//!    The dispatcher is built once per sub-block; cross-sub-block runs
//!    re-acquire (each sub-block independently measures contention rates).
//! 2. Spawn N concurrent tasks via [`tokio::task::JoinSet`]; each task
//!    clones the Arc and calls `dispatcher.dispatch(...)`. Per-task timing
//!    captures `t_submit_ms` (sub-block start → just-before-dispatch) and
//!    `t_construct_complete_ms` (sub-block start → after dispatch returns).
//! 3. Race `joinset.join_next()` against `ctx.clock.sleep(budget)` inside
//!    a two-arm `tokio::select!` with `biased` ordering. On budget arm:
//!    `joinset.abort_all()` + drain. The `tests/c_no_dispatch_serialization_in_s4.rs`
//!    static grep enforces this exact two-arm shape.
//!
//! Per-task `TaskOutcome` fields are folded into the `SubBlockOutcome`
//! aggregates (success_rate, max_serialization_gap_ms,
//! double_selection_rejections). Cell-level counters (success_count /
//! rejection_count / stall_count / timeout_count) sum across sub-blocks.
//!
//! Recipient strategy is `SelfAddress(SeedRole)` per
//! `analysis/DESIGN.md §S4 state machine` and the brief directive — S4 sends
//! to the same wallet's own address so the UTXO-set contention measurement
//! is local to that wallet's selection logic. The role is whichever the
//! `ScenarioCtx::recipients` field carries.
//!
//! **Operator setup**: N=128 means up to 128 concurrent subprocess spawns
//! for Modes 2/3 (each `create-unsigned-transaction` invocation forks
//! `minotari`). On macOS the default soft fd limit is 256; on Linux 1024.
//! The S4 runbook (added in step 3k) instructs operators to set
//! `ulimit -n 4096` before running the harness. Tracked in
//! `analysis/PR_BODY_PLAN.md §Operator Setup`.
//!
//! **Scenario-side confirmation polling is out-of-scope for this commit.**
//! `t_confirm_ms` is recorded as `None` for every task — confirmation
//! requires a base-node polling loop that S0 already implements; S4's
//! confirmation backfill will land alongside the result-profile writer in
//! step 3k (the writer needs the per-task txids; this commit emits them
//! ready for that pass). Tracked in `analysis/API_DRIFT.md`.

use std::time::{Duration, Instant};

use crate::broadcast::RejectionReason;
use crate::modes::{Mode, TxRecord};
use crate::scenarios::{DetailPhase, DetailRecord, ScenarioCtx};
use crate::seed::redact::RedactionDenylist;

/// S4's per-cell payload, matching `RESULT_PROFILE_SCHEMA.md §S4` (lines
/// 187-198) plus the universal cell counters from `§errors sub-object`
/// (lines 112-116) `{success_count, rejection_count, stall_count,
/// timeout_count}` summed across all sub-blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct S4Outcome {
    /// One entry per N in the sweep, ordered to match
    /// `ctx.config.concurrent_batches`. Schema's `sub_blocks` is a JSON
    /// object keyed by stringified N; the writer (step 3k) emits the keys
    /// from `n_concurrent` on each entry.
    pub sub_blocks: Vec<SubBlockOutcome>,
    /// Sum of `success_count` over all sub-blocks (universal cell counter,
    /// `RESULT_PROFILE_SCHEMA.md §errors sub-object` line 112).
    pub success_count: u64,
    /// Sum of `rejection_count` over all sub-blocks (line 113).
    pub rejection_count: u64,
    /// Sum of `stall_count` over all sub-blocks (line 114). Per
    /// `RESULT_PROFILE_SCHEMA.md` line 114, "stall" means "tx accepted but
    /// unconfirmed past `per_tx_confirmation_timeout_ms` (AC-33)" — strictly
    /// a confirmation-phase event. S4 does NOT yet run confirmation polling
    /// (see module docs); the per-task `t_confirm_ms` is always `None`.
    /// Until step 3k wires confirmation polling and populates
    /// `t_confirm_ms`, `stall_count` is structurally 0. Tracked in
    /// `analysis/API_DRIFT.md §3i.1.g`.
    pub stall_count: u64,
    /// Sum of `timeout_count` over all sub-blocks (line 115). For S4 this
    /// is the count of tasks aborted by the per-sub-block budget arm.
    pub timeout_count: u64,
    /// Universal `details[]` per `RESULT_PROFILE_SCHEMA.md` line 116 — one
    /// entry per non-success non-rejection event. S4 records broadcast-
    /// phase failures (dispatcher `Err` and non-cancelled `JoinError`) here
    /// with `phase = Broadcast`. Aborted-by-budget tasks feed `timeout_count`,
    /// NOT `details[]`. Rejections feed `rejection_count`, NOT `details[]`.
    pub details: Vec<DetailRecord>,
    /// `peak_rss_bytes` per `RESULT_PROFILE_SCHEMA.md` lines 126-127 —
    /// `None` when `ctx.sampler_factory` is `None` (matches the schema's
    /// `u64 | null` permission).
    pub peak_rss_bytes: Option<u64>,
    /// `peak_cpu_pct` per `RESULT_PROFILE_SCHEMA.md` lines 126-127 —
    /// `None` when `ctx.sampler_factory` is `None`, or when fewer than
    /// 2 samples were collected (CPU% needs a delta).
    pub peak_cpu_pct: Option<f64>,
}

/// Per-N (sub-block) measurement, matching
/// `RESULT_PROFILE_SCHEMA.md §S4 scenario` lines 191-198.
#[derive(Debug, Clone, PartialEq)]
pub struct SubBlockOutcome {
    /// `n_concurrent` (schema line 192) — echoed N.
    pub n_concurrent: u32,
    /// `budget_elapsed` (line 193) — true if the per-sub-block budget arm
    /// fired before all tasks completed.
    pub budget_elapsed: bool,
    /// `batch_wall_clock_ms` (line 194) — from first dispatch to last
    /// terminal event (whichever arm of the `tokio::select!` resolved last).
    pub batch_wall_clock_ms: u64,
    /// `success_rate` (line 195) — `success_count / n_concurrent` in
    /// `[0.0, 1.0]`.
    pub success_rate: f64,
    /// `max_serialization_gap_ms` (line 196) — max delta between
    /// consecutive sorted `t_construct_complete_ms` values across
    /// completed tasks. A non-zero gap means the wallet's own internal
    /// serialisation (e.g. lock contention on a shared selection cache)
    /// imposed sequential order on what the harness submitted concurrently.
    pub max_serialization_gap_ms: u64,
    /// `double_selection_rejections` (line 197) — count of tasks whose
    /// rejection reason indicates the same UTXO was selected by another
    /// concurrent task (DoubleSpend variant; or "duplicate input"
    /// substring on a broadcast-error string).
    pub double_selection_rejections: u32,
    /// `tx_records[]` (line 198) — per-task record. The schema-level
    /// `tx_records[]` is `{ txid, t_submit_ms, t_construct_complete_ms,
    /// broadcast_outcome, t_confirm_ms, error_string?, rejection_reason? }`.
    pub tx_records: Vec<TaskOutcome>,
}

/// Per-task outcome under `SubBlockOutcome::tx_records[]`. Mirrors
/// `RESULT_PROFILE_SCHEMA.md §S4` line 198's per-task object shape.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskOutcome {
    /// `txid` per schema line 198. Empty string when the task aborted or
    /// the dispatch failed before a txid was assigned.
    pub txid: String,
    /// `t_submit_ms` per schema — sub-block start → just-before-dispatch.
    /// Always set (non-`None`) for tasks that began before the budget arm
    /// fired.
    pub t_submit_ms: u64,
    /// `t_construct_complete_ms` per schema — sub-block start → after
    /// `dispatch` returns. `None` when the task was aborted by the budget
    /// arm or panicked inside the spawn.
    pub t_construct_complete_ms: Option<u64>,
    /// `broadcast_outcome` per schema — `"accepted" | "rejected" | "error"`
    /// PLUS the S4-specific `"aborted"` variant for tasks that the budget
    /// arm cancelled before the underlying `dispatch` future resolved.
    pub broadcast_outcome: BroadcastOutcome,
    /// `t_confirm_ms` per schema — confirmation polling backfilled by the
    /// result-profile writer (step 3k). `None` for now; see module docs.
    pub t_confirm_ms: Option<u64>,
    /// `error_string?` per schema — redaction applied at capture per the
    /// 3i.1.f brief directive.
    pub error_string: Option<String>,
    /// `rejection_reason?` per schema — raw from the broadcast wrapper.
    /// `None` on non-rejected outcomes.
    pub rejection_reason: Option<RejectionReason>,
}

/// `broadcast_outcome` enum mirroring the schema's per-task object
/// (line 198) plus the S4-specific `Aborted` arm for budget-arm cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroadcastOutcome {
    /// Base node accepted the tx (TxRecord status == "success").
    Accepted,
    /// Base node rejected the tx (TxRecord status == "failure" AND a
    /// `rejection_reason` was carried).
    Rejected,
    /// Dispatch returned an `Err` before the base node ruled — broadcast-
    /// or construction-layer error, no `rejection_reason`.
    Error,
    /// Task was aborted by the per-sub-block budget arm.
    Aborted,
}

impl std::fmt::Display for BroadcastOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Error => "error",
            Self::Aborted => "aborted",
        })
    }
}

/// Run S4 against the given mode.
///
/// `mode` is consumed by reference (pre/post-state capture lives at the
/// scenario-runner layer, step 3k); this scenario builds the dispatcher
/// once per sub-block via `mode.dispatcher()` and orchestrates the
/// concurrent dispatch entirely inside the JoinSet.
///
/// Returns `Err` only on dispatcher acquisition failure (Mode 1 before
/// `wait_ready`, etc.) — per-task failures fold into the `SubBlockOutcome`
/// records without halting the sweep, matching AC-33's "raw, do not retry"
/// rule.
pub(super) async fn run(ctx: &ScenarioCtx<'_>, mode: &mut dyn Mode) -> anyhow::Result<S4Outcome> {
    let config = ctx.config;
    let sampler = ctx.sampler_factory.map(|f| {
        f.start(
            crate::sampler::Pid(mode.target_pid_for_sampling()),
            ctx.config.sampler_interval_ms,
        )
    });
    let fee_rate = config.fee_rate;
    let amount_per_task: u64 = 1_000;
    let budget = Duration::from_millis(config.s4_t_budget_ms);
    let sub_block_sizes = config.concurrent_batches.clone();

    let mut sub_blocks: Vec<SubBlockOutcome> = Vec::with_capacity(sub_block_sizes.len());
    let mut success_count: u64 = 0;
    let mut rejection_count: u64 = 0;
    let mut timeout_count: u64 = 0;
    let mut details: Vec<DetailRecord> = Vec::new();

    // Fail-fast policy (uniform across S1/S4/S5 send loops). S4 tasks
    // complete concurrently, so the streak is observed in recorded task
    // order per sub-block; contiguity is therefore approximate for S4 and
    // documented as such, while a fully failing wallet (every task, same
    // string) still trips it deterministically.
    let mut streak =
        crate::scenarios::FailureStreakTracker::new(config.fail_fast_identical_failure_threshold);
    let mut aborted = false;
    for &n in &sub_block_sizes {
        if aborted {
            break;
        }
        let outcome = run_one_sub_block(
            ctx,
            mode,
            n,
            amount_per_task,
            fee_rate,
            budget,
            ctx.redaction,
        )
        .await?;
        success_count = success_count.saturating_add(count_outcome(&outcome, |t| {
            matches!(t.broadcast_outcome, BroadcastOutcome::Accepted)
        }));
        rejection_count = rejection_count.saturating_add(count_outcome(&outcome, |t| {
            matches!(t.broadcast_outcome, BroadcastOutcome::Rejected)
        }));
        timeout_count = timeout_count.saturating_add(count_outcome(&outcome, |t| {
            matches!(t.broadcast_outcome, BroadcastOutcome::Aborted)
        }));
        // BroadcastOutcome::Error → schema line 116 `details[]` with
        // phase = Broadcast (the dispatcher is the broadcast layer for S4).
        // Per `RESULT_PROFILE_SCHEMA.md` line 114, stall_count is reserved
        // for confirmation-phase timeouts; broadcast errors are NOT stalls.
        for task in &outcome.tx_records {
            if matches!(
                task.broadcast_outcome,
                BroadcastOutcome::Error | BroadcastOutcome::Rejected
            ) {
                let failed = task.error_string.clone().unwrap_or_default();
                if streak.observe_failure(&failed) && !aborted {
                    let reason = streak.abort_reason();
                    log::warn!("S4 fail-fast: {reason}");
                    details.push(DetailRecord {
                        txid: None,
                        error_string: reason,
                        phase: DetailPhase::Broadcast,
                    });
                    aborted = true;
                }
            } else {
                streak.observe_success();
            }
            if matches!(task.broadcast_outcome, BroadcastOutcome::Error) {
                details.push(DetailRecord {
                    txid: if task.txid.is_empty() {
                        None
                    } else {
                        Some(task.txid.clone())
                    },
                    error_string: task.error_string.clone().unwrap_or_default(),
                    phase: DetailPhase::Broadcast,
                });
            }
        }
        sub_blocks.push(outcome);
    }

    // stall_count is structurally 0 until 3k wires confirmation polling
    // and populates `t_confirm_ms`. See S4Outcome::stall_count doc-comment
    // and `analysis/API_DRIFT.md §3i.1.g`.
    let stall_count: u64 = 0;

    let (peak_rss_bytes, peak_cpu_pct) = match sampler {
        Some(s) => s.stop().await,
        None => (None, None),
    };

    Ok(S4Outcome {
        sub_blocks,
        success_count,
        rejection_count,
        stall_count,
        timeout_count,
        details,
        peak_rss_bytes,
        peak_cpu_pct,
    })
}

/// Run one sub-block of `n` concurrent dispatches with the given budget.
async fn run_one_sub_block(
    ctx: &ScenarioCtx<'_>,
    mode: &mut dyn Mode,
    n: u32,
    amount: u64,
    fee_rate: u64,
    budget: Duration,
    redaction: &RedactionDenylist,
) -> anyhow::Result<SubBlockOutcome> {
    let dispatcher = mode.dispatcher();
    let sub_block_start = Instant::now();

    let mut joinset = tokio::task::JoinSet::new();
    for tx_idx in 0..n {
        let recipient = ctx.recipients.resolve_for(ctx.seeds, tx_idx)?;
        joinset.spawn(dispatch_one_task(
            dispatcher.clone(),
            recipient,
            amount,
            fee_rate,
            sub_block_start,
        ));
    }

    let mut tx_records: Vec<TaskOutcome> = Vec::with_capacity(n as usize);
    let mut budget_elapsed = false;
    let clock = ctx.clock;

    loop {
        if joinset.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = clock.sleep(budget) => {
                budget_elapsed = true;
                joinset.abort_all();
                while let Some(joined) = joinset.join_next().await {
                    record_joined(&mut tx_records, joined, true, redaction);
                }
                break;
            }
            joined = joinset.join_next() => {
                match joined {
                    Some(j) => record_joined(&mut tx_records, j, false, redaction),
                    None => break,
                }
            }
        }
    }

    let batch_wall_clock_ms =
        u64::try_from(sub_block_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let success_rate = compute_success_rate(&tx_records, n);
    let max_serialization_gap_ms = compute_max_serialization_gap(&tx_records);
    let double_selection_rejections = count_double_selection_rejections(&tx_records);

    Ok(SubBlockOutcome {
        n_concurrent: n,
        budget_elapsed,
        batch_wall_clock_ms,
        success_rate,
        max_serialization_gap_ms,
        double_selection_rejections,
        tx_records,
    })
}

/// Per-task dispatch wrapper. Lifted out of the spawn-loop body so the
/// for-loop site itself contains no `.await` — the
/// `tests/c_no_dispatch_serialization_in_s4.rs` static grep enforces this
/// at file-narrow scope.
async fn dispatch_one_task(
    dispatcher: std::sync::Arc<dyn crate::modes::S4Dispatcher>,
    recipient: tari_common_types::tari_address::TariAddress,
    amount: u64,
    fee_rate: u64,
    started: Instant,
) -> (u64, u64, anyhow::Result<TxRecord>) {
    let t_submit_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let result = dispatcher.dispatch(recipient, amount, fee_rate).await;
    let t_construct_complete_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    (t_submit_ms, t_construct_complete_ms, result)
}

/// Fold one `JoinHandle` outcome into the per-sub-block records vector.
fn record_joined(
    out: &mut Vec<TaskOutcome>,
    joined: Result<(u64, u64, anyhow::Result<TxRecord>), tokio::task::JoinError>,
    aborted_by_budget: bool,
    redaction: &RedactionDenylist,
) {
    match joined {
        Ok((t_submit_ms, t_construct_complete_ms, Ok(record))) => {
            // Dispatch returned a TxRecord — its `status` and `error_string`
            // distinguish accepted / rejected / error.
            let (broadcast_outcome, rejection_reason) = if record.status == "success" {
                (BroadcastOutcome::Accepted, None)
            } else if let Some(reason) = parse_rejection_reason(record.error_string.as_deref()) {
                (BroadcastOutcome::Rejected, Some(reason))
            } else {
                (BroadcastOutcome::Error, None)
            };
            let error_string = record
                .error_string
                .as_deref()
                .map(|s| redact_at_capture(s, redaction));
            out.push(TaskOutcome {
                txid: record.txid,
                t_submit_ms,
                t_construct_complete_ms: Some(t_construct_complete_ms),
                broadcast_outcome,
                t_confirm_ms: None,
                error_string,
                rejection_reason,
            });
        }
        Ok((t_submit_ms, t_construct_complete_ms, Err(e))) => {
            // Dispatch bailed before the base node ruled — broadcast / construct
            // / sign error. No txid, no rejection_reason.
            let msg = format!("{e:#}");
            out.push(TaskOutcome {
                txid: String::new(),
                t_submit_ms,
                t_construct_complete_ms: Some(t_construct_complete_ms),
                broadcast_outcome: BroadcastOutcome::Error,
                t_confirm_ms: None,
                error_string: Some(redact_at_capture(&msg, redaction)),
                rejection_reason: None,
            });
        }
        Err(join_err) => {
            // Task panic or budget-triggered abort. Cancelled errors come
            // from `joinset.abort_all()` (budget arm); panics surface here
            // too.
            let outcome = if aborted_by_budget && join_err.is_cancelled() {
                BroadcastOutcome::Aborted
            } else {
                BroadcastOutcome::Error
            };
            let msg = if join_err.is_cancelled() {
                "task aborted by S4 budget arm".to_string()
            } else {
                format!("JoinError: {join_err}")
            };
            out.push(TaskOutcome {
                txid: String::new(),
                // Submit time is unknown for an aborted task — record 0 so
                // the schema's `u64` shape is preserved; the
                // `broadcast_outcome = aborted` is the load-bearing signal
                // the writer reads.
                t_submit_ms: 0,
                t_construct_complete_ms: None,
                broadcast_outcome: outcome,
                t_confirm_ms: None,
                error_string: Some(redact_at_capture(&msg, redaction)),
                rejection_reason: None,
            });
        }
    }
}

/// Parse a `RejectionReason` from a `TxRecord::error_string` produced by the
/// Mode 2/3 helper's `format!("{:?}", outcome.rejection_reason)`. The Debug
/// form is stable enough for grep-style matching here — production code
/// only ever sees variants from the upstream enum.
fn parse_rejection_reason(s: Option<&str>) -> Option<RejectionReason> {
    let s = s?;
    if s.contains("DoubleSpend") {
        Some(RejectionReason::DoubleSpend)
    } else if s.contains("AlreadyMined") {
        Some(RejectionReason::AlreadyMined)
    } else if s.contains("Orphan") {
        Some(RejectionReason::Orphan)
    } else if s.contains("TimeLocked") {
        Some(RejectionReason::TimeLocked)
    } else if s.contains("ValidationFailed") {
        Some(RejectionReason::ValidationFailed)
    } else if s.contains("FeeTooLow") {
        Some(RejectionReason::FeeTooLow)
    } else if s.contains("duplicate input") {
        // Substring fallback per the 3i.1.f brief — older base nodes can
        // surface DoubleSpend as a free-form "duplicate input ..." error.
        Some(RejectionReason::DoubleSpend)
    } else {
        None
    }
}

/// Apply the redaction denylist at capture time. If any rule matches the
/// captured string, the entire string is replaced with `"<redacted>"` —
/// echoing the matched substring back would defeat the denylist. Per the
/// 3i.1.f brief's "error_string (apply ctx.redaction at capture)" directive.
fn redact_at_capture(s: &str, redaction: &RedactionDenylist) -> String {
    // `RedactionDenylist::check` operates on serialised JSON of any
    // `Serialize` value. Wrap the captured string in a `String` and check
    // that — the serialised form is `"<contents>"` which still triggers
    // every rule (mnemonics, hex keys, paths, etc.).
    match redaction.check(&s.to_string()) {
        Ok(()) => s.to_string(),
        Err(_) => "<redacted>".to_string(),
    }
}

/// `success_rate = success_count / n` in `[0.0, 1.0]`. Schema line 195.
fn compute_success_rate(records: &[TaskOutcome], n: u32) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let successes = records
        .iter()
        .filter(|t| matches!(t.broadcast_outcome, BroadcastOutcome::Accepted))
        .count();
    (successes as f64) / (n as f64)
}

/// Max delta between consecutive sorted `t_construct_complete_ms` across
/// completed tasks. Schema line 196. Aborted tasks (no
/// `t_construct_complete_ms`) are excluded from the gap computation —
/// they did not complete construction, so they cannot bound it.
fn compute_max_serialization_gap(records: &[TaskOutcome]) -> u64 {
    let mut times: Vec<u64> = records
        .iter()
        .filter_map(|t| t.t_construct_complete_ms)
        .collect();
    times.sort_unstable();
    times
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .max()
        .unwrap_or(0)
}

/// Count tasks whose rejection reason indicates a double-selection event.
/// Schema line 197 — DoubleSpend OR a "duplicate input" substring is the
/// signal. Only `BroadcastOutcome::Rejected` tasks are considered.
fn count_double_selection_rejections(records: &[TaskOutcome]) -> u32 {
    records
        .iter()
        .filter(|t| {
            matches!(t.broadcast_outcome, BroadcastOutcome::Rejected)
                && matches!(t.rejection_reason, Some(RejectionReason::DoubleSpend))
        })
        .count() as u32
}

/// Count `tx_records` entries matching `pred`. Helper to keep the
/// universal-counter folding in `run` symmetric and readable.
fn count_outcome<F>(outcome: &SubBlockOutcome, pred: F) -> u64
where
    F: Fn(&TaskOutcome) -> bool,
{
    outcome.tx_records.iter().filter(|t| pred(t)).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::RealClock;
    use crate::config::{Config, Seeds};
    use crate::gen_seed;
    use crate::modes::test_support::{FakeMode, SendOutcome};
    use crate::scenarios::RecipientStrategy;
    use crate::seed::redact::RedactionDenylist;
    use crate::seed::{SeedHandle, SeedRole};

    fn set_env(name: &str, value: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(name, value);
        }
    }
    fn unset_env(name: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(name);
        }
    }

    fn unique_seeds(suffix: &str) -> Seeds {
        Seeds {
            old: format!("WALLET_BENCHMARKS_TEST_S4_OLD_{suffix}"),
            new: format!("WALLET_BENCHMARKS_TEST_S4_NEW_{suffix}"),
            payment_processor: format!("WALLET_BENCHMARKS_TEST_S4_PP_{suffix}"),
            wallet_password: format!("WALLET_BENCHMARKS_TEST_S4_PW_{suffix}"),
        }
    }

    fn ok_record(txid: &str) -> TxRecord {
        TxRecord {
            txid: txid.to_string(),
            t_total_ms: 1,
            t_broadcast_ms: 1,
            t_confirm_ms: None,
            status: "success".to_string(),
            error_string: None,
            fee_microtari: 0,
        }
    }

    fn rejected_record(txid: &str, reason: &str) -> TxRecord {
        TxRecord {
            txid: txid.to_string(),
            t_total_ms: 1,
            t_broadcast_ms: 1,
            t_confirm_ms: None,
            status: "failure".to_string(),
            error_string: Some(reason.to_string()),
            fee_microtari: 0,
        }
    }

    /// Build a usable ScenarioCtx + FakeMode with the supplied canned
    /// `SendOutcome` sequence. The sub-block sizes default to `[2]` so the
    /// fake's sequence does not need to be huge.
    fn build_ctx(
        suffix: &str,
        sub_block_sizes: Vec<u32>,
        budget_ms: u64,
        send_sequence: Vec<SendOutcome>,
    ) -> (
        FakeMode,
        Seeds,
        Config,
        SeedHandle,
        RedactionDenylist,
        RealClock,
    ) {
        let seeds_cfg = unique_seeds(suffix);
        let m_new = gen_seed().expect("m_new");
        set_env(&seeds_cfg.new, &m_new);
        let cfg = Config {
            seeds: seeds_cfg.clone(),
            concurrent_batches: sub_block_sizes,
            s4_t_budget_ms: budget_ms,
            ..Config::default()
        };
        let seeds = SeedHandle::new(&seeds_cfg);
        let mut fake = FakeMode::new();
        fake.send_single_sequence = send_sequence;
        let redaction = RedactionDenylist::for_test();
        (fake, seeds_cfg, cfg, seeds, redaction, RealClock)
    }

    /// Helper: build the `ScenarioCtx` from the parts. The struct itself
    /// can't outlive the references; the test calls this inline.
    fn ctx_for<'a>(
        cfg: &'a Config,
        seeds: &'a SeedHandle,
        redaction: &'a RedactionDenylist,
        clock: &'a RealClock,
    ) -> ScenarioCtx<'a> {
        ScenarioCtx {
            config: cfg,
            seeds,
            redaction,
            clock,
            recipients: RecipientStrategy::SelfAddress(SeedRole::New),
            sampler_factory: None,
        }
    }

    /// F4: S4 shares `fail_fast_identical_failure_threshold` with S1/S5.
    /// Three sub-blocks are planned; with the threshold at 2 and every
    /// task failing byte-identically, the streak trips inside sub-block 1
    /// and sub-blocks 2 and 3 never dispatch.
    #[tokio::test]
    async fn s4_aborts_after_contiguous_identical_task_errors() {
        let (mut fake, seeds_cfg, mut cfg, seeds, redaction, clock) = build_ctx(
            "FAILFAST_S4",
            vec![2, 2, 2],
            60_000,
            (0..6)
                .map(|_| SendOutcome::Err("identical broadcast failure".to_string()))
                .collect(),
        );
        cfg.fail_fast_identical_failure_threshold = 2;
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let outcome = run(&ctx, &mut fake).await.expect("run ok");
        assert_eq!(
            outcome.sub_blocks.len(),
            1,
            "abort inside sub-block 1 skips sub-blocks 2 and 3",
        );
        // S4 dispatches through the clone-able dispatcher snapshot (not
        // FakeMode::send_single), so dispatch volume is asserted via the
        // recorded task outcomes: sub-block 1's two tasks and nothing else.
        assert_eq!(
            outcome.sub_blocks[0].tx_records.len(),
            2,
            "only sub-block 1's tasks dispatched",
        );
        assert!(
            outcome
                .details
                .iter()
                .any(|d| d.error_string.contains("2 contiguous identical failures")),
            "abort reason recorded in details[]: {:?}",
            outcome
                .details
                .iter()
                .map(|d| d.error_string.as_str())
                .collect::<Vec<_>>(),
        );
        unset_env(&seeds_cfg.new);
    }

    /// N=2, both succeed → success_rate == 1.0, double_selection_rejections == 0,
    /// max_serialization_gap_ms is computed (delta between two completed times).
    #[tokio::test]
    async fn s4_n2_all_succeed() {
        let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
            "N2_OK",
            vec![2],
            60_000,
            vec![
                SendOutcome::Ok(ok_record("a")),
                SendOutcome::Ok(ok_record("b")),
            ],
        );
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let outcome = run(&ctx, &mut fake).await.expect("run ok");
        assert_eq!(outcome.sub_blocks.len(), 1);
        let sb = &outcome.sub_blocks[0];
        assert_eq!(sb.n_concurrent, 2);
        assert!(!sb.budget_elapsed, "budget must not fire on fast fake");
        assert!(
            (sb.success_rate - 1.0).abs() < f64::EPSILON,
            "success_rate must equal 1.0; got {}",
            sb.success_rate,
        );
        assert_eq!(sb.double_selection_rejections, 0);
        assert_eq!(sb.tx_records.len(), 2);
        for r in &sb.tx_records {
            assert_eq!(r.broadcast_outcome, BroadcastOutcome::Accepted);
            assert!(r.t_construct_complete_ms.is_some());
        }
        // Universal counters: 2 successes, 0 of everything else.
        assert_eq!(outcome.success_count, 2);
        assert_eq!(outcome.rejection_count, 0);
        assert_eq!(outcome.stall_count, 0);
        assert_eq!(outcome.timeout_count, 0);
        assert!(outcome.details.is_empty(), "no errors → no details[]");
        unset_env(&seeds_cfg.new);
    }

    /// N=2, one ok + one rejected with a DoubleSpend reason →
    /// double_selection_rejections == 1, rejection_count == 1.
    #[tokio::test]
    async fn s4_n2_one_double_spend() {
        let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
            "N2_DS",
            vec![2],
            60_000,
            vec![
                SendOutcome::Ok(ok_record("a")),
                SendOutcome::Ok(rejected_record("b", "DoubleSpend")),
            ],
        );
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let outcome = run(&ctx, &mut fake).await.expect("run ok");
        let sb = &outcome.sub_blocks[0];
        assert_eq!(sb.double_selection_rejections, 1);
        // success_rate == 0.5 (1 of 2 accepted).
        assert!(
            (sb.success_rate - 0.5).abs() < f64::EPSILON,
            "success_rate must equal 0.5; got {}",
            sb.success_rate,
        );
        assert_eq!(outcome.success_count, 1);
        assert_eq!(outcome.rejection_count, 1);
        assert_eq!(outcome.stall_count, 0);
        assert_eq!(outcome.timeout_count, 0);
        // Rejections feed `rejection_count`, NOT `details[]` — per
        // schema line 116 details[] is reserved for non-success
        // non-rejection events (broadcast / construct / sign / confirm
        // / scan errors).
        assert!(
            outcome.details.is_empty(),
            "rejection-only outcome must not push details[]",
        );
        unset_env(&seeds_cfg.new);
    }

    /// N=2 with a 10ms budget against a custom dispatcher that sleeps
    /// inside each dispatch — budget arm must fire and both tasks come
    /// back Aborted. Real-time budget (not paused-tokio) per the 3i.1.f
    /// brief's recommendation: "it's acceptable to use a real small
    /// budget + real small task duration".
    ///
    /// FakeModeDispatcher itself returns near-instantly (no sleep), so
    /// this test wraps the underlying mode's dispatcher with
    /// `SleepyDispatcher` whose `dispatch` awaits `tokio::time::sleep`
    /// for `100ms` before returning. With a 10ms budget the budget arm
    /// reliably fires first.
    #[tokio::test]
    async fn s4_n2_budget_arm_fires() {
        use crate::modes::S4Dispatcher;
        use std::sync::Arc;
        use std::time::Duration as StdDuration;
        use tari_common_types::tari_address::TariAddress;

        struct SleepyDispatcher;

        #[async_trait::async_trait]
        impl S4Dispatcher for SleepyDispatcher {
            async fn dispatch(
                &self,
                _recipient: TariAddress,
                _amount_microtari: u64,
                _fee_rate: u64,
            ) -> anyhow::Result<TxRecord> {
                tokio::time::sleep(StdDuration::from_millis(100)).await;
                Ok(ok_record("sleepy"))
            }
        }

        // ModeStubWithSleepyDispatcher: a minimal Mode impl whose only used
        // method is `dispatcher()` — every other method panics if invoked.
        // The S4 scenario only ever touches `mode.dispatcher()` per-sub-block
        // so the panicking arms are never reached.
        struct ModeStubWithSleepyDispatcher;

        #[async_trait::async_trait]
        impl crate::modes::Mode for ModeStubWithSleepyDispatcher {
            fn name(&self) -> &'static str {
                "sleepy_stub"
            }
            async fn send_single(
                &mut self,
                _recipient: &TariAddress,
                _amount_microtari: u64,
                _fee_rate: u64,
            ) -> anyhow::Result<TxRecord> {
                unreachable!("S4 only uses dispatcher()")
            }
            async fn send_batch_one_to_many(
                &mut self,
                _recipients: &[(TariAddress, u64)],
                _fee_rate: u64,
            ) -> anyhow::Result<TxRecord> {
                unreachable!("S4 only uses dispatcher()")
            }
            async fn scan_from_birthday(
                &mut self,
                _birthday: u16,
            ) -> anyhow::Result<crate::modes::ScanOutcome> {
                unreachable!("S4 only uses dispatcher()")
            }
            async fn get_balance(&mut self) -> anyhow::Result<u64> {
                unreachable!("S4 only uses dispatcher()")
            }
            async fn get_utxo_count(&mut self) -> anyhow::Result<u64> {
                unreachable!("S4 only uses dispatcher()")
            }
            async fn wipe_and_reimport(&mut self, _birthday: u16) -> anyhow::Result<()> {
                unreachable!("S4 only uses dispatcher()")
            }
            fn dispatcher(&self) -> Arc<dyn S4Dispatcher> {
                Arc::new(SleepyDispatcher)
            }
        }

        let (_unused_fake, seeds_cfg, cfg, seeds, redaction, clock) =
            build_ctx("N2_BUDGET", vec![2], 10, vec![]);
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let mut mode = ModeStubWithSleepyDispatcher;
        let outcome = run(&ctx, &mut mode).await.expect("run ok");
        let sb = &outcome.sub_blocks[0];
        assert!(
            sb.budget_elapsed,
            "10ms budget against a 100ms-sleeping dispatcher must fire the \
             budget arm; got budget_elapsed=false",
        );
        // All 2 tasks aborted by the budget arm.
        assert_eq!(
            outcome.timeout_count, 2,
            "both tasks must be Aborted by the budget arm; got timeout_count={}",
            outcome.timeout_count,
        );
        assert_eq!(outcome.success_count, 0);
        assert_eq!(outcome.rejection_count, 0);
        assert_eq!(outcome.stall_count, 0);
        // Aborted tasks feed `timeout_count`, NOT `details[]` — per the
        // S4Outcome::details doc-comment.
        assert!(
            outcome.details.is_empty(),
            "aborted-by-budget tasks must not push details[]",
        );
        for r in &sb.tx_records {
            assert_eq!(r.broadcast_outcome, BroadcastOutcome::Aborted);
            assert!(r.t_construct_complete_ms.is_none());
        }
        unset_env(&seeds_cfg.new);
    }

    /// N=2, one Ok + one dispatcher-side Err. The Err MUST land in
    /// `details[]` with phase=Broadcast, NOT in `stall_count`. Schema
    /// line 114 reserves stall_count for confirmation-phase events, which
    /// S4 does not yet run (deferred to 3k per API_DRIFT.md §3i.1.f).
    #[tokio::test]
    async fn s4_n2_one_broadcast_error_lands_in_details_not_stall() {
        let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
            "N2_BERR",
            vec![2],
            60_000,
            vec![
                SendOutcome::Ok(ok_record("a")),
                SendOutcome::Err("simulated broadcast bail".to_string()),
            ],
        );
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let outcome = run(&ctx, &mut fake).await.expect("run ok");

        assert_eq!(outcome.success_count, 1);
        assert_eq!(outcome.rejection_count, 0);
        assert_eq!(
            outcome.stall_count, 0,
            "broadcast errors must NOT feed stall_count (schema line 114)",
        );
        assert_eq!(outcome.timeout_count, 0);
        assert_eq!(
            outcome.details.len(),
            1,
            "exactly one BroadcastOutcome::Error → one details[] entry",
        );
        assert_eq!(outcome.details[0].phase, DetailPhase::Broadcast);
        assert!(
            outcome.details[0]
                .error_string
                .contains("simulated broadcast bail"),
            "details[0].error_string must carry the injected message; got {}",
            outcome.details[0].error_string,
        );
        unset_env(&seeds_cfg.new);
    }

    /// Cell-level partition invariant per `RESULT_PROFILE_SCHEMA.md`
    /// lines 112-116:
    ///
    /// ```text
    /// sum_over_sub_blocks(n_concurrent) ==
    ///     success_count + rejection_count + stall_count + timeout_count
    ///     + count(details where phase ∈ {Construct, Sign, Broadcast})
    /// ```
    ///
    /// Runs the 4 hand-built scenarios and asserts the invariant on each.
    #[tokio::test]
    async fn s4_cell_counters_partition_total_attempts() {
        async fn invariant(outcome: S4Outcome) {
            let total: u64 = outcome
                .sub_blocks
                .iter()
                .map(|sb| u64::from(sb.n_concurrent))
                .sum();
            let pre_broadcast_details: u64 = outcome
                .details
                .iter()
                .filter(|d| {
                    matches!(
                        d.phase,
                        DetailPhase::Construct | DetailPhase::Sign | DetailPhase::Broadcast,
                    )
                })
                .count() as u64;
            let partition = outcome.success_count
                + outcome.rejection_count
                + outcome.stall_count
                + outcome.timeout_count
                + pre_broadcast_details;
            assert_eq!(
                total,
                partition,
                "schema lines 112-116 partition invariant violated: \
                 sum(n_concurrent)={total} must equal success({})+rejection({})\
                 +stall({})+timeout({})+pre_broadcast_details({})={partition}",
                outcome.success_count,
                outcome.rejection_count,
                outcome.stall_count,
                outcome.timeout_count,
                pre_broadcast_details,
            );
        }

        // Case 1 — all succeed: 2 + 0 + 0 + 0 + 0 == 2.
        {
            let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
                "INV_OK",
                vec![2],
                60_000,
                vec![
                    SendOutcome::Ok(ok_record("a")),
                    SendOutcome::Ok(ok_record("b")),
                ],
            );
            let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
            let outcome = run(&ctx, &mut fake).await.expect("run ok");
            invariant(outcome).await;
            unset_env(&seeds_cfg.new);
        }
        // Case 2 — one double-spend rejection: 1 + 1 + 0 + 0 + 0 == 2.
        {
            let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
                "INV_DS",
                vec![2],
                60_000,
                vec![
                    SendOutcome::Ok(ok_record("a")),
                    SendOutcome::Ok(rejected_record("b", "DoubleSpend")),
                ],
            );
            let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
            let outcome = run(&ctx, &mut fake).await.expect("run ok");
            invariant(outcome).await;
            unset_env(&seeds_cfg.new);
        }
        // Case 3 — one broadcast error: 1 + 0 + 0 + 0 + 1 == 2.
        {
            let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
                "INV_BERR",
                vec![2],
                60_000,
                vec![
                    SendOutcome::Ok(ok_record("a")),
                    SendOutcome::Err("inv: broadcast bail".to_string()),
                ],
            );
            let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
            let outcome = run(&ctx, &mut fake).await.expect("run ok");
            invariant(outcome).await;
            unset_env(&seeds_cfg.new);
        }
        // Case 4 — budget arm fires for both tasks: 0 + 0 + 0 + 2 + 0 == 2.
        // Same SleepyDispatcher setup as s4_n2_budget_arm_fires; the
        // invariant arithmetic is the load-bearing assertion here.
        {
            use crate::modes::S4Dispatcher;
            use std::sync::Arc;
            use std::time::Duration as StdDuration;
            use tari_common_types::tari_address::TariAddress;

            struct SleepyDispatcher;

            #[async_trait::async_trait]
            impl S4Dispatcher for SleepyDispatcher {
                async fn dispatch(
                    &self,
                    _recipient: TariAddress,
                    _amount_microtari: u64,
                    _fee_rate: u64,
                ) -> anyhow::Result<TxRecord> {
                    tokio::time::sleep(StdDuration::from_millis(100)).await;
                    Ok(ok_record("sleepy"))
                }
            }
            struct ModeStubWithSleepyDispatcher;
            #[async_trait::async_trait]
            impl crate::modes::Mode for ModeStubWithSleepyDispatcher {
                fn name(&self) -> &'static str {
                    "sleepy_stub"
                }
                async fn send_single(
                    &mut self,
                    _r: &TariAddress,
                    _a: u64,
                    _f: u64,
                ) -> anyhow::Result<TxRecord> {
                    unreachable!()
                }
                async fn send_batch_one_to_many(
                    &mut self,
                    _r: &[(TariAddress, u64)],
                    _f: u64,
                ) -> anyhow::Result<TxRecord> {
                    unreachable!()
                }
                async fn scan_from_birthday(
                    &mut self,
                    _b: u16,
                ) -> anyhow::Result<crate::modes::ScanOutcome> {
                    unreachable!()
                }
                async fn get_balance(&mut self) -> anyhow::Result<u64> {
                    unreachable!()
                }
                async fn get_utxo_count(&mut self) -> anyhow::Result<u64> {
                    unreachable!()
                }
                async fn wipe_and_reimport(&mut self, _b: u16) -> anyhow::Result<()> {
                    unreachable!()
                }
                fn dispatcher(&self) -> Arc<dyn S4Dispatcher> {
                    Arc::new(SleepyDispatcher)
                }
            }

            let (_unused_fake, seeds_cfg, cfg, seeds, redaction, clock) =
                build_ctx("INV_BUDGET", vec![2], 10, vec![]);
            let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
            let mut mode = ModeStubWithSleepyDispatcher;
            let outcome = run(&ctx, &mut mode).await.expect("run ok");
            invariant(outcome).await;
            unset_env(&seeds_cfg.new);
        }
    }
}
