//! S5 throughput — individual vs. batch arms (AC-19, AC-20).
//!
//! Per `analysis/DESIGN.md §Scenario state machine §S5` (lines 433-434),
//! `analysis/RESULT_PROFILE_SCHEMA.md §S5 scenario (AC-19, AC-20, AC-21)`
//! lines 200-216, and the 3i.1.g brief: S5 amortises a 100-recipient pool
//! across two arms so the per-mode `throughput_multiplier` is a meaningful
//! `t_individual / t_batch` (wall-clock ratio; >1.0 means batch finishes
//! faster overall than the equivalent volume of individual sends) per
//! schema line 215.
//!
//! * **Individual arm** — `s5_m` (default 100) sequential single-recipient
//!   sends via [`Mode::send_single`]. Runs for every mode (`SeedRole::Old`,
//!   `SeedRole::New`, `SeedRole::Pp`).
//! * **Batch arm** — `s5_m / s5_k` (default 10) sequential `s5_k`-recipient
//!   (default 10) sends via [`Mode::send_batch_one_to_many`]. Runs for all
//!   three modes — Mode 1's batch path dispatches gRPC `Transfer` with
//!   `single_tx = true` per @SWvheerden's 2026-06-05 PR-6 comment
//!   ("you can run this on the console wallet"); Mode 2/3 dispatch via the
//!   `minotari create-unsigned-transaction` subprocess pipeline. See
//!   `analysis/DESIGN_AMENDMENT.md §11`.
//!
//! `throughput_multiplier = t_individual / t_batch` (wall-clock ratio per
//! schema line 215) when both arms apply AND both `t_total_ms` are `> 0`;
//! `None` otherwise.
//!
//! Both arms target the same total send-volume (`s5_m` recipients) so the
//! multiplier compares like-for-like work. The recipient pool itself is
//! derived once via [`crate::seed::derive_recipient_pool`] and passed to
//! the per-arm runners as `RecipientStrategy::Pool(&pool)` — round-robin
//! resolve covers both arms without per-arm pool re-derivation.
//!
//! **Confirmation polling is deferred to step 3k** — same shape as S4 per
//! `analysis/API_DRIFT.md §3i.1.g`. The per-arm `t_total_ms` measures the
//! send-loop wall-clock only; cell-level `stall_count` is structurally 0
//! and `timeout_count` is 0 (S5 has no budget arm).
//!
//! **No retry, no backoff, no throttle.** AC-30 / AC-31 / AC-32 patterns
//! enforced statically by `tests/c_no_retry_backoff_throttle.rs`.

use std::time::Instant;

use tari_common_types::tari_address::TariAddress;

use crate::broadcast::RejectionReason;
use crate::modes::{Mode, TxRecord};
use crate::scenarios::{DetailPhase, DetailRecord, RecipientStrategy, ScenarioCtx};
use crate::seed::{derive_recipient_pool, SeedHandle, SeedRole};

/// S5's per-cell payload. Mirrors `RESULT_PROFILE_SCHEMA.md §S5` lines
/// 200-216 for the per-arm shape, plus the universal cell counters from the
/// `§errors sub-object` (lines 112-116) shared by every scenario.
///
/// `arms.batch.applies` is true for all three modes — Mode 1 wires the
/// batch arm via gRPC `Transfer` with `single_tx = true` per
/// `analysis/DESIGN_AMENDMENT.md §11`.
#[derive(Debug, Clone, PartialEq)]
pub struct S5Outcome {
    /// Successful tx submissions across both arms. Schema line 112.
    pub success_count: u64,
    /// Mempool/validation rejection per base-node response across both arms.
    /// Schema line 113.
    pub rejection_count: u64,
    /// Schema line 114 — confirmation-phase stalls. S5 does NOT yet wire
    /// confirmation polling (deferred to step 3k per
    /// `analysis/API_DRIFT.md §3i.1.g`); this counter is structurally 0
    /// until 3k populates per-tx `t_confirm_ms`. Same posture as S4.
    pub stall_count: u64,
    /// Schema line 115 — budget / harness-side timeouts. S5 has no budget
    /// arm; this counter stays 0 (kept for schema uniformity with the rest
    /// of the universal error sub-object).
    pub timeout_count: u64,
    /// Schema line 116 — one entry per non-success non-rejection event
    /// (broadcast / construct / sign failures from `Mode::send_*`).
    pub details: Vec<DetailRecord>,
    /// Per-arm outcomes. Both `applies = true` on every mode post
    /// `analysis/DESIGN_AMENDMENT.md §11`.
    pub arms: S5Arms,
    /// `throughput_multiplier` per schema line 215 + AC-19. Computed as
    /// `t_individual / t_batch` (wall-clock ratio; >1.0 means batch
    /// finishes faster overall than the equivalent volume of individual
    /// sends) when both `t_total_ms` are `> 0`; `None` on divide-by-zero
    /// (degenerate fake-mode case).
    pub throughput_multiplier: Option<f64>,
    /// `peak_rss_bytes` — `None` until the 1 Hz sampler lands in step 3j.
    pub peak_rss_bytes: Option<u64>,
    /// `peak_cpu_pct` — `None` until the 1 Hz sampler lands in step 3j.
    pub peak_cpu_pct: Option<f64>,
}

/// Container for the two S5 arms. Kept as a named struct so the writer
/// (step 3k) can match on `s5_outcome.arms.individual` /
/// `s5_outcome.arms.batch` without tuple-position bugs.
#[derive(Debug, Clone, PartialEq)]
pub struct S5Arms {
    /// Individual arm — `s5_m` sequential single-recipient sends.
    pub individual: ArmOutcome,
    /// Batch arm — `s5_m / s5_k` sequential `s5_k`-recipient batch sends.
    /// Runs on all three modes per `analysis/DESIGN_AMENDMENT.md §11`.
    pub batch: ArmOutcome,
}

/// Per-arm outcome — shape mirrors the schema-level shape under
/// `RESULT_PROFILE_SCHEMA.md §S5 arms.<batch|individual>` (lines 207-214),
/// flattened: there is no separate `applies` per-field gate in the schema;
/// instead the schema sentinel is `applies = false` → all per-arm metric
/// fields are `null`. The Rust shape uses `Option` on the rate-per-unit-time
/// field (`throughput_tx_per_sec`) and a 0 sentinel on integral fields when
/// `applies = false` — same convention as S1's `peak_*` sampler fields.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmOutcome {
    /// AC-20 — `true` whenever the arm ran. Schema line 207's
    /// `arms.batch.applies` bool gate. Post-DESIGN_AMENDMENT §11 all
    /// three modes run both arms, so this is `true` on every produced
    /// outcome; the field is kept for schema uniformity and for the
    /// `compute_throughput_multiplier` divide-by-zero guard.
    pub applies: bool,
    /// Number of `send_*` calls in this arm: `s5_m` for individual,
    /// `s5_m / s5_k` for batch.
    pub tx_count: u32,
    /// Recipients per `send_*` call: `1` for individual, `s5_k` for batch.
    pub recipients_per_tx: u32,
    /// `tx_count * recipients_per_tx` — total recipients served by this
    /// arm. Both arms target the same total under the default config
    /// (`s5_m = 100`, `s5_k = 10`): individual arm serves `100 * 1 = 100`,
    /// batch arm serves `10 * 10 = 100`.
    pub total_sends: u32,
    /// Arm wall-clock from first `send_*` to last terminal-state event,
    /// in milliseconds.
    pub t_total_ms: u64,
    /// `tx_count / (t_total_ms / 1000)` — the comparable rate for the
    /// AC-19 multiplier. `None` when `t_total_ms == 0` (divide-by-zero
    /// guard). Per repo idiom for rate-per-unit-time fields.
    pub throughput_tx_per_sec: Option<f64>,
    /// Raw `TxRecord` from each `send_*` call in dispatch order. Schema-
    /// equivalent of the per-arm `tx_records[]` slot.
    pub tx_records: Vec<TxRecord>,
}

/// Run S5 against the given mode.
///
/// `seed_role_for_mode` names the seed slot from which the recipient pool
/// is derived. Routed through `ScenarioInput::s5_seed_role_for_mode` by
/// the run loop (step 3i.2) so the scenario stays mode-agnostic — see
/// [`super::ScenarioInput`]. All three modes run both arms per
/// `analysis/DESIGN_AMENDMENT.md §11`.
pub(super) async fn run(
    ctx: &ScenarioCtx<'_>,
    mode: &mut dyn Mode,
    seed_role_for_mode: SeedRole,
) -> anyhow::Result<S5Outcome> {
    let config = ctx.config;
    let sampler = ctx.sampler_factory.map(|f| {
        f.start(
            crate::sampler::Pid(mode.target_pid_for_sampling()),
            ctx.config.sampler_interval_ms,
        )
    });
    let fee_rate = config.fee_rate;
    let amount_per_recipient: u64 = 1_000;
    let m = config.s5_m;
    let k = config.s5_k;
    anyhow::ensure!(
        k > 0 && m.is_multiple_of(k),
        "S5 requires s5_m ({m}) to be a positive multiple of s5_k ({k}); \
         non-multiple values would leave a partial batch and bias the AC-19 \
         throughput_multiplier",
    );

    // Derive the 100-recipient pool once and share it across both arms so
    // the batch and individual arms serve the same recipient set (AC-19's
    // "same 100-recipient list").
    let pool = derive_recipient_pool(ctx.seeds, seed_role_for_mode, m as usize)?;
    let pool_strategy = RecipientStrategy::Pool(&pool);

    // Individual arm runs for every mode. Recipient resolution goes via
    // `RecipientStrategy::Pool(&pool)::resolve_for` to keep the pattern
    // uniform with S1 / S4 (per the 3i.1.g brief directive).
    let individual = run_individual_arm(
        mode,
        ctx.seeds,
        &pool_strategy,
        m,
        amount_per_recipient,
        fee_rate,
    )
    .await?;

    // Batch arm runs for all three modes. Mode 1 dispatches the batch via
    // gRPC `Transfer` with `single_tx = true` (per
    // `analysis/DESIGN_AMENDMENT.md §11`); Mode 2/3 dispatch via the
    // `minotari create-unsigned-transaction` subprocess pipeline.
    let batch = run_batch_arm(
        mode,
        ctx.seeds,
        &pool_strategy,
        m,
        k,
        amount_per_recipient,
        fee_rate,
    )
    .await?;

    // Cell-level counter fold: sum across both arms. stall_count stays
    // structurally 0 until 3k wires confirmation polling (see module docs);
    // timeout_count stays 0 (no budget arm).
    let mut success_count: u64 = 0;
    let mut rejection_count: u64 = 0;
    let mut details: Vec<DetailRecord> = Vec::new();
    for arm in [&individual, &batch] {
        for tx in &arm.tx_records {
            classify_record(tx, &mut success_count, &mut rejection_count, &mut details);
        }
    }

    let throughput_multiplier = compute_throughput_multiplier(&individual, &batch);

    let (peak_rss_bytes, peak_cpu_pct) = match sampler {
        Some(s) => s.stop().await,
        None => (None, None),
    };

    Ok(S5Outcome {
        success_count,
        rejection_count,
        stall_count: 0,
        timeout_count: 0,
        details,
        arms: S5Arms { individual, batch },
        throughput_multiplier,
        peak_rss_bytes,
        peak_cpu_pct,
    })
}

/// Run the individual arm — `m` sequential single-recipient sends.
/// Recipient resolution goes through
/// [`RecipientStrategy::Pool::resolve_for`] so the per-slot lookup pattern
/// matches S1 / S4. AC-32-exempt: no sleeps, no throttle.
async fn run_individual_arm(
    mode: &mut dyn Mode,
    seeds: &SeedHandle,
    strategy: &RecipientStrategy<'_>,
    m: u32,
    amount_per_recipient: u64,
    fee_rate: u64,
) -> anyhow::Result<ArmOutcome> {
    let mut tx_records: Vec<TxRecord> = Vec::with_capacity(m as usize);
    let arm_start = Instant::now();
    for tx_idx in 0..m {
        let recipient = strategy.resolve_for(seeds, tx_idx)?;
        let send_result = mode
            .send_single(&recipient, amount_per_recipient, fee_rate)
            .await;
        match send_result {
            Ok(rec) => tx_records.push(rec),
            Err(send_err) => {
                let error_string = format!("{send_err:#}");
                tx_records.push(synthesize_failure_record(&error_string, "construct"));
            }
        }
    }
    let t_total_ms = u64::try_from(arm_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(ArmOutcome {
        applies: true,
        tx_count: m,
        recipients_per_tx: 1,
        total_sends: m,
        t_total_ms,
        throughput_tx_per_sec: throughput(m, t_total_ms),
        tx_records,
    })
}

/// Run the batch arm — `m / k` sequential `k`-recipient batch sends.
/// Recipient resolution goes through the same
/// [`RecipientStrategy::Pool::resolve_for`] surface, cycling through the
/// pool with a monotonically increasing index so the total served-set
/// covers each pool slot once.
async fn run_batch_arm(
    mode: &mut dyn Mode,
    seeds: &SeedHandle,
    strategy: &RecipientStrategy<'_>,
    m: u32,
    k: u32,
    amount_per_recipient: u64,
    fee_rate: u64,
) -> anyhow::Result<ArmOutcome> {
    let tx_count = m / k;
    let mut tx_records: Vec<TxRecord> = Vec::with_capacity(tx_count as usize);
    let arm_start = Instant::now();
    let mut recipient_idx: u32 = 0;
    for _ in 0..tx_count {
        let mut recipients: Vec<(TariAddress, u64)> = Vec::with_capacity(k as usize);
        for _ in 0..k {
            let addr = strategy.resolve_for(seeds, recipient_idx)?;
            recipient_idx = recipient_idx.saturating_add(1);
            recipients.push((addr, amount_per_recipient));
        }
        let send_result = mode.send_batch_one_to_many(&recipients, fee_rate).await;
        match send_result {
            Ok(rec) => tx_records.push(rec),
            Err(send_err) => {
                let error_string = format!("{send_err:#}");
                tx_records.push(synthesize_failure_record(&error_string, "construct"));
            }
        }
    }
    let t_total_ms = u64::try_from(arm_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(ArmOutcome {
        applies: true,
        tx_count,
        recipients_per_tx: k,
        total_sends: tx_count.saturating_mul(k),
        t_total_ms,
        throughput_tx_per_sec: throughput(tx_count, t_total_ms),
        tx_records,
    })
}

/// Classify one `TxRecord` (from either arm) into the cell-level counters.
///
/// Matches the existing S1 idiom (`src/scenarios/s1_volume.rs::run`):
///   * wire-`success` → `success_count`; no details entry.
///   * wire-`failure` with a parseable rejection reason → `rejection_count`;
///     no details entry (rejection_count is the schema-level slot per
///     line 113; the reason rides on the record's `error_string`).
///   * wire-`failure` with no rejection reason (construct / sign / broadcast
///     error returned via `Ok(record)` from the Mode helper, OR synthesised
///     by `synthesize_failure_record` from a send-side `Err`) → details
///     entry with `phase = Construct | Sign | Broadcast` per the record's
///     `status` suffix.
fn classify_record(
    tx: &TxRecord,
    success_count: &mut u64,
    rejection_count: &mut u64,
    details: &mut Vec<DetailRecord>,
) {
    if tx.status == "success" {
        *success_count += 1;
        return;
    }
    let txid_opt = if tx.txid.is_empty() {
        None
    } else {
        Some(tx.txid.clone())
    };
    if parse_rejection_reason(tx.error_string.as_deref()).is_some() {
        *rejection_count += 1;
        return;
    }
    let phase = phase_from_status(&tx.status);
    details.push(DetailRecord {
        txid: txid_opt,
        error_string: tx.error_string.clone().unwrap_or_default(),
        phase,
    });
}

/// `tx_count / (t_total_ms / 1000)`. `None` when `t_total_ms == 0`.
fn throughput(tx_count: u32, t_total_ms: u64) -> Option<f64> {
    if t_total_ms == 0 {
        return None;
    }
    Some((tx_count as f64) / (t_total_ms as f64 / 1000.0))
}

/// `throughput_multiplier = t_individual / t_batch` per
/// `RESULT_PROFILE_SCHEMA.md` line 215 — wall-clock ratio across the two
/// arms. >1.0 means batch finishes the equivalent send-volume faster
/// than individual.
///
/// `None` when either `t_total_ms` is zero (degenerate fake-mode case;
/// divide-by-zero protection).
fn compute_throughput_multiplier(individual: &ArmOutcome, batch: &ArmOutcome) -> Option<f64> {
    if !batch.applies {
        return None;
    }
    if individual.t_total_ms == 0 || batch.t_total_ms == 0 {
        return None;
    }
    Some(individual.t_total_ms as f64 / batch.t_total_ms as f64)
}

/// Map a `status` string suffix to a `DetailPhase`. Mirrors the encoding
/// `TxRecordStatus::Failed(phase)` produces (`"failure:construct"` etc.)
/// plus a bare `"failure"` default → `Broadcast`.
fn phase_from_status(status: &str) -> DetailPhase {
    if status.starts_with("failure:construct") || status.starts_with("construct") {
        DetailPhase::Construct
    } else if status.starts_with("failure:sign") || status.starts_with("sign") {
        DetailPhase::Sign
    } else {
        DetailPhase::Broadcast
    }
}

/// Parse a `RejectionReason` from a `TxRecord::error_string` — same Debug-
/// substring shape S4 already uses (`src/scenarios/s4_concurrent.rs::parse_rejection_reason`).
/// Kept inline rather than shared so each scenario's `run` is self-contained
/// for review.
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
        Some(RejectionReason::DoubleSpend)
    } else {
        None
    }
}

/// Construct a synthetic `TxRecord` for a send-side `Err`. Mirrors
/// `src/scenarios/s1_volume.rs::synthesize_failure_record` — the harness
/// needs a record either way so the arm's `tx_records[]` count matches
/// `tx_count`.
fn synthesize_failure_record(error_string: &str, phase: &str) -> TxRecord {
    TxRecord {
        txid: String::new(),
        t_total_ms: 0,
        t_broadcast_ms: 0,
        t_confirm_ms: None,
        status: format!("failure:{phase}"),
        error_string: Some(error_string.to_string()),
        fee_microtari: 0,
    }
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
            old: format!("WALLET_BENCHMARKS_TEST_S5_OLD_{suffix}"),
            new: format!("WALLET_BENCHMARKS_TEST_S5_NEW_{suffix}"),
            payment_processor: format!("WALLET_BENCHMARKS_TEST_S5_PP_{suffix}"),
            wallet_password: format!("WALLET_BENCHMARKS_TEST_S5_PW_{suffix}"),
        }
    }

    fn ok_record(tag: &str) -> TxRecord {
        TxRecord {
            txid: format!("txid-{tag}"),
            t_total_ms: 1,
            t_broadcast_ms: 1,
            t_confirm_ms: None,
            status: "success".to_string(),
            error_string: None,
            fee_microtari: 0,
        }
    }

    /// Populate every seed env so the pool derivation always works
    /// regardless of which role the test passes in.
    fn populate_all_seeds(seeds_cfg: &Seeds) -> (String, String, String) {
        let m_old = gen_seed().expect("m_old");
        let m_new = gen_seed().expect("m_new");
        let m_pp = gen_seed().expect("m_pp");
        set_env(&seeds_cfg.old, &m_old);
        set_env(&seeds_cfg.new, &m_new);
        set_env(&seeds_cfg.payment_processor, &m_pp);
        (m_old, m_new, m_pp)
    }

    /// Build a usable ScenarioCtx + FakeMode with the supplied `s5_m` /
    /// `s5_k` knobs. Tests pass tiny values (e.g. m=4, k=2) so the fake
    /// sequence stays short while still proving the arm shape.
    #[allow(clippy::type_complexity)]
    fn build_ctx(
        suffix: &str,
        s5_m: u32,
        s5_k: u32,
        individual_sequence: Vec<SendOutcome>,
        canned_batch: Option<TxRecord>,
    ) -> (
        FakeMode,
        Seeds,
        Config,
        SeedHandle,
        RedactionDenylist,
        RealClock,
    ) {
        let seeds_cfg = unique_seeds(suffix);
        populate_all_seeds(&seeds_cfg);
        let cfg = Config {
            seeds: seeds_cfg.clone(),
            s5_m,
            s5_k,
            ..Config::default()
        };
        let seeds = SeedHandle::new(&seeds_cfg);
        let mut fake = FakeMode::new();
        fake.send_single_sequence = individual_sequence;
        fake.canned_batch = canned_batch;
        let redaction = RedactionDenylist::for_test();
        (fake, seeds_cfg, cfg, seeds, redaction, RealClock)
    }

    fn ctx_for<'a>(
        cfg: &'a Config,
        seeds: &'a SeedHandle,
        redaction: &'a RedactionDenylist,
        clock: &'a RealClock,
    ) -> ScenarioCtx<'a> {
        // S5 derives its own pool via `derive_recipient_pool`; the
        // ctx.recipients field is unused by the S5 arm (kept on the
        // ScenarioCtx for the other scenarios). SelfAddress(New) here is
        // an arbitrary placeholder — populate_all_seeds set $NEW so the
        // strategy could be resolved, but S5's `run` ignores it.
        ScenarioCtx {
            config: cfg,
            seeds,
            redaction,
            clock,
            recipients: RecipientStrategy::SelfAddress(SeedRole::New),
            sampler_factory: None,
        }
    }

    /// Individual arm with 4 successful sends → applies=true, tx_count=4,
    /// throughput_tx_per_sec is Some, tx_records.len()=4.
    #[tokio::test]
    async fn s5_individual_arm_smoke() {
        // Post DESIGN_AMENDMENT §11 the batch arm runs on every role —
        // provide a canned batch record so the test asserts only the
        // individual arm's shape without the batch arm tripping
        // FakeMode's "no canned value" bail.
        let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
            "INDIV_SMOKE",
            4,
            2,
            vec![
                SendOutcome::Ok(ok_record("a")),
                SendOutcome::Ok(ok_record("b")),
                SendOutcome::Ok(ok_record("c")),
                SendOutcome::Ok(ok_record("d")),
            ],
            Some(ok_record("batch")),
        );
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let outcome = run(&ctx, &mut fake, SeedRole::Old)
            .await
            .expect("S5 run ok");

        assert!(outcome.arms.individual.applies);
        assert_eq!(outcome.arms.individual.tx_count, 4);
        assert_eq!(outcome.arms.individual.recipients_per_tx, 1);
        assert_eq!(outcome.arms.individual.total_sends, 4);
        assert_eq!(outcome.arms.individual.tx_records.len(), 4);
        // throughput_tx_per_sec is Some when the arm wall-clock rounds to
        // ≥1ms — under FakeMode (near-instant returns) the round can drop
        // to 0 and the rate becomes None. Both shapes are valid per the
        // doc-comment on `ArmOutcome::throughput_tx_per_sec`; only the
        // contract is asserted: rate Some XOR t_total_ms == 0.
        if outcome.arms.individual.t_total_ms == 0 {
            assert!(outcome.arms.individual.throughput_tx_per_sec.is_none());
        } else {
            assert!(outcome.arms.individual.throughput_tx_per_sec.is_some());
        }
        // Cell-level: 4 individual successes + 2 batch successes = 6.
        assert_eq!(outcome.success_count, 6);
        assert_eq!(outcome.rejection_count, 0);
        assert_eq!(outcome.stall_count, 0);
        assert_eq!(outcome.timeout_count, 0);
        assert!(outcome.details.is_empty());
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
    }

    /// SeedRole::Old → batch arm RUNS post `analysis/DESIGN_AMENDMENT.md
    /// §11`. With s5_m=4 / s5_k=2 the batch arm dispatches 4/2 = 2 batch
    /// sends each carrying 2 recipients, identical to Modes 2/3.
    #[tokio::test]
    async fn s5_batch_arm_runs_on_mode1() {
        let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
            "BATCH_RUNS_M1",
            4,
            2,
            vec![
                SendOutcome::Ok(ok_record("a")),
                SendOutcome::Ok(ok_record("b")),
                SendOutcome::Ok(ok_record("c")),
                SendOutcome::Ok(ok_record("d")),
            ],
            Some(ok_record("batch")),
        );
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let outcome = run(&ctx, &mut fake, SeedRole::Old)
            .await
            .expect("S5 run ok");

        assert!(
            outcome.arms.batch.applies,
            "Mode 1 batch arm must run post DESIGN_AMENDMENT §11",
        );
        assert_eq!(outcome.arms.batch.tx_count, 2);
        assert_eq!(outcome.arms.batch.recipients_per_tx, 2);
        assert_eq!(outcome.arms.batch.total_sends, 4);
        assert_eq!(outcome.arms.batch.tx_records.len(), 2);
        let calls = fake.calls.lock().unwrap().clone();
        let batch_calls = calls
            .iter()
            .filter(|c| **c == "send_batch_one_to_many")
            .count();
        assert_eq!(
            batch_calls, 2,
            "exactly 2 batch dispatches for s5_m=4/s5_k=2 on SeedRole::Old",
        );
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
    }

    /// SeedRole::New → batch arm runs. With s5_m=4 / s5_k=2 the batch arm
    /// dispatches 4/2 = 2 batch sends each carrying 2 recipients.
    #[tokio::test]
    async fn s5_batch_arm_runs_on_mode2_3() {
        let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
            "BATCH_RUNS",
            4,
            2,
            vec![
                SendOutcome::Ok(ok_record("a")),
                SendOutcome::Ok(ok_record("b")),
                SendOutcome::Ok(ok_record("c")),
                SendOutcome::Ok(ok_record("d")),
            ],
            Some(ok_record("batch")),
        );
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let outcome = run(&ctx, &mut fake, SeedRole::New)
            .await
            .expect("S5 run ok");

        assert!(outcome.arms.batch.applies);
        assert_eq!(outcome.arms.batch.tx_count, 2);
        assert_eq!(outcome.arms.batch.recipients_per_tx, 2);
        assert_eq!(outcome.arms.batch.total_sends, 4);
        assert_eq!(outcome.arms.batch.tx_records.len(), 2);
        let calls = fake.calls.lock().unwrap().clone();
        let batch_calls = calls
            .iter()
            .filter(|c| **c == "send_batch_one_to_many")
            .count();
        assert_eq!(
            batch_calls, 2,
            "exactly 2 batch dispatches for s5_m=4/s5_k=2"
        );
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
    }

    /// Both arms apply → throughput_multiplier is the wall-clock ratio
    /// t_individual / t_batch per schema line 215.
    #[tokio::test]
    async fn s5_throughput_multiplier_computed_when_both_arms_apply() {
        let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
            "MULT_OK",
            4,
            2,
            vec![
                SendOutcome::Ok(ok_record("a")),
                SendOutcome::Ok(ok_record("b")),
                SendOutcome::Ok(ok_record("c")),
                SendOutcome::Ok(ok_record("d")),
            ],
            Some(ok_record("batch")),
        );
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let outcome = run(&ctx, &mut fake, SeedRole::Pp).await.expect("S5 run ok");

        // FakeMode returns near-instantly; both arms may complete with
        // sub-millisecond wall-clocks → t_total_ms can round to 0 and the
        // multiplier returns None (divide-by-zero protection). Assert the
        // contract: if both arm t_total_ms > 0, multiplier equals their
        // wall-clock ratio; otherwise multiplier is None.
        let ind_ms = outcome.arms.individual.t_total_ms;
        let bat_ms = outcome.arms.batch.t_total_ms;
        match outcome.throughput_multiplier {
            Some(mult) => {
                assert!(
                    ind_ms > 0 && bat_ms > 0,
                    "multiplier should only be Some when both t_total_ms > 0",
                );
                let expected = ind_ms as f64 / bat_ms as f64;
                assert!(
                    (mult - expected).abs() < f64::EPSILON * mult.abs().max(1.0),
                    "throughput_multiplier must equal t_individual / t_batch per schema line 215: \
                     got {mult}, expected {expected}",
                );
            }
            None => {
                assert!(
                    ind_ms == 0 || bat_ms == 0,
                    "multiplier should only be None on divide-by-zero (or batch.applies = false)",
                );
            }
        }
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
    }

    /// Cell-level partition invariant (same shape as S1 /
    /// `tests::s1_cell_counters_partition_total_attempts`).
    ///
    /// ```text
    /// total_sends_across_arms ==
    ///     success_count + rejection_count + stall_count + timeout_count
    ///     + count(details where phase ∈ {Construct, Sign, Broadcast})
    /// ```
    ///
    /// Note: `total_sends_across_arms` here counts per-`send_*` calls
    /// (tx_count summed across both arms), NOT per-recipient — each batch
    /// send is one call that produces one TxRecord regardless of K
    /// recipients.
    #[tokio::test]
    async fn s5_cell_counters_partition_total_sends() {
        let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
            "PARTITION",
            4,
            2,
            vec![
                SendOutcome::Ok(ok_record("a")),
                SendOutcome::Ok(ok_record("b")),
                SendOutcome::Ok(ok_record("c")),
                SendOutcome::Ok(ok_record("d")),
            ],
            Some(ok_record("batch")),
        );
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let outcome = run(&ctx, &mut fake, SeedRole::Pp).await.expect("S5 run ok");

        let total_sends =
            u64::from(outcome.arms.individual.tx_count) + u64::from(outcome.arms.batch.tx_count);
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
            total_sends,
            partition,
            "schema lines 112-116 partition invariant violated: \
             total_sends_across_arms={total_sends} must equal success({})+rejection({})\
             +stall({})+timeout({})+pre_broadcast_details({})={partition}",
            outcome.success_count,
            outcome.rejection_count,
            outcome.stall_count,
            outcome.timeout_count,
            pre_broadcast_details,
        );
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
    }

    /// `s5_m` must be a positive multiple of `s5_k` — bad config bails
    /// cleanly with a context-rich error.
    #[tokio::test]
    async fn s5_bails_when_s5_m_not_multiple_of_s5_k() {
        let (mut fake, seeds_cfg, cfg, seeds, redaction, clock) = build_ctx(
            "BAD_M_K",
            5,
            2,
            vec![SendOutcome::Ok(ok_record("x"))],
            Some(ok_record("batch")),
        );
        let ctx = ctx_for(&cfg, &seeds, &redaction, &clock);
        let err = run(&ctx, &mut fake, SeedRole::New)
            .await
            .expect_err("s5_m=5 / s5_k=2 must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("multiple") && msg.contains("s5_m") && msg.contains("s5_k"),
            "error must name the bad config: {msg}",
        );
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
    }
}
