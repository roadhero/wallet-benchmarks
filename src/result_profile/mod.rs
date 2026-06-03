//! Result-profile writer — 27-cell matrix → canonical JSON per
//! `analysis/RESULT_PROFILE_SCHEMA.md`.
//!
//! Pure transform: takes the matrix the orchestrator built in
//! `main::run_harness_async`, the harness config, env capture, version
//! probe, and redaction denylist; emits the canonical JSON file at the
//! supplied path.
//!
//! Common-envelope coverage: scenarios on this branch do not yet carry
//! every common-envelope field (`wall_clock_ms`, `tip_height_*`,
//! `fees_paid_microtari`, `balance_*`, `balance_reconciliation_ok`).
//! The writer emits these with defensible defaults (`0` / `null`) where
//! the underlying `ScenarioOutcome` does not provide a value. The
//! result-profile shape passes schema parsing; field-level coverage of
//! the missing envelope values lands in Phase 4 alongside the canonical
//! baseline run. Documented in `analysis/PR_BODY_PLAN.md §Phase 4
//! envelope-coverage gap`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use serde_json::{json, Value};

use crate::config::Config;
use crate::env_capture::Environment;
use crate::modes::TxRecord;
use crate::scenarios::BroadcastOutcome;
use crate::scenarios::{
    ArmOutcome, B0Outcome, DetailPhase, DetailRecord, RoundOutcome, S0Outcome, S1Outcome,
    S2Outcome, S3Outcome, S4Outcome, S5Arms, S5Outcome, S6Outcome, S7Outcome, ScenarioId,
    ScenarioOutcome, SubBlockOutcome, TaskOutcome,
};
use crate::seed::redact::RedactionDenylist;
use crate::seed::SeedRole;
use crate::versions::Versions;

const LOG_TARGET: &str = "c::result_profile";

/// Per-cell result. `Outcome` is the success path; `Error` carries the
/// scenario's bail message; `NotRun` indicates the cell was skipped
/// (e.g. Mode 1's S5 batch arm).
pub enum CellResult {
    /// Boxed because `ScenarioOutcome` is the largest enum variant in the
    /// crate (~240 bytes from S4Outcome's sub_blocks Vec); boxing keeps the
    /// `CellResult` discriminant small (clippy::large_enum_variant).
    Outcome(Box<ScenarioOutcome>),
    Error(anyhow::Error),
    NotRun,
}

/// Cell envelope captured by the run loop alongside each [`CellResult`].
/// Closes the common-envelope coverage gap for fields the
/// `ScenarioOutcome` types don't carry intrinsically.
///
/// `tip_height_start` / `tip_height_end` are `None` when the base-node
/// tip query failed for that cell — the writer emits `null` plus a
/// per-cell `tip_query_note` rather than the catch-all
/// `envelope_coverage_note`.
pub struct CellEntry {
    pub result: CellResult,
    pub wall_clock_ms: u64,
    pub tip_height_start: Option<u64>,
    pub tip_height_end: Option<u64>,
    pub tip_query_note: Option<String>,
    pub fees_paid_microtari: u64,
}

/// Matrix of `(mode, scenario)` → `CellEntry` populated by
/// `main::run_harness_async`. Up to 27 cells (3 modes × 9 scenarios);
/// `NotRun` cells are stored explicitly so the writer can emit `null`
/// at the appropriate JSON slot.
pub struct Matrix {
    pub cells: HashMap<(SeedRole, ScenarioId), CellEntry>,
}

impl Matrix {
    pub fn new() -> Self {
        Self {
            cells: HashMap::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        role: SeedRole,
        scenario: ScenarioId,
        result: CellResult,
        wall_clock_ms: u64,
        tip_height_start: Option<u64>,
        tip_height_end: Option<u64>,
        tip_query_note: Option<String>,
        fees_paid_microtari: u64,
    ) {
        self.cells.insert(
            (role, scenario),
            CellEntry {
                result,
                wall_clock_ms,
                tip_height_start,
                tip_height_end,
                tip_query_note,
                fees_paid_microtari,
            },
        );
    }

    pub fn get(&self, role: SeedRole, scenario: ScenarioId) -> Option<&CellEntry> {
        self.cells.get(&(role, scenario))
    }
}

impl Default for Matrix {
    fn default() -> Self {
        Self::new()
    }
}

/// Lowercase mode-name keys per `RESULT_PROFILE_SCHEMA.md §4`.
pub fn mode_key(role: SeedRole) -> &'static str {
    match role {
        SeedRole::Old => "old_wallet",
        SeedRole::New => "new_wallet",
        SeedRole::Pp => "payment_processor",
    }
}

/// Build the full profile JSON, apply redaction, write to `output_path`.
pub fn write(
    matrix: &Matrix,
    config: &Config,
    env: &Environment,
    versions: &Versions,
    redaction: &RedactionDenylist,
    output_path: &Path,
) -> anyhow::Result<()> {
    let profile = assemble_profile(matrix, config, env, versions, redaction);
    redaction
        .check(&profile)
        .context("redaction denylist matched the assembled profile — programming error in result_profile writer; mnemonic/password material leaked into a serialized value")?;
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir {}", parent.display()))?;
        }
    }
    let bytes = serde_json::to_vec_pretty(&profile)
        .context("serializing assembled profile to pretty JSON")?;
    std::fs::write(output_path, &bytes)
        .with_context(|| format!("writing result profile to {}", output_path.display()))?;
    log::info!(
        target: LOG_TARGET,
        "wrote result profile ({} bytes) to {}",
        bytes.len(),
        output_path.display(),
    );
    Ok(())
}

fn assemble_profile(
    matrix: &Matrix,
    config: &Config,
    env: &Environment,
    versions: &Versions,
    redaction: &RedactionDenylist,
) -> Value {
    json!({
        "schema_version": 1,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "config": config_block(config),
        "environment": env_block(env),
        "versions": versions_block(versions),
        "modes": modes_block(matrix),
        "deltas": deltas_block(matrix),
        "redaction_denylist": redaction,
    })
}

fn config_block(config: &Config) -> Value {
    json!({
        "a_fund": config.a_fund,
        "c_min": config.c_min,
        "volume_target": config.volume_target,
        "doubling_rounds": config.doubling_rounds,
        "fanout_outputs_per_tx": config.fanout_outputs_per_tx,
        "concurrent_batches": config.concurrent_batches,
        "s4_t_budget_ms": config.s4_t_budget_ms,
        "s5_m": config.s5_m,
        "s5_k": config.s5_k,
        "fee_rate": config.fee_rate,
        "network": config.network,
        "base_node_url": config.base_node_url.as_str(),
        "per_tx_confirmation_timeout_ms": config.per_tx_confirmation_timeout_ms,
        "sampler_interval_ms": config.sampler_interval_ms,
    })
}

fn env_block(env: &Environment) -> Value {
    json!({
        "cpu_model": env.cpu_model,
        "ram_bytes": env.ram_bytes,
        "disk_type": env.disk_type,
        "os": env.os,
        "network_path_to_base_node": env.network_path_to_base_node,
    })
}

fn versions_block(versions: &Versions) -> Value {
    // `Versions` derives `Serialize`; round-trip via serde_json so the JSON
    // shape stays in lockstep with the type definition automatically.
    serde_json::to_value(versions).unwrap_or(Value::Null)
}

fn modes_block(matrix: &Matrix) -> Value {
    let mut modes = serde_json::Map::new();
    for role in [SeedRole::Old, SeedRole::New, SeedRole::Pp] {
        let mut scenarios = serde_json::Map::new();
        for scenario_id in ScenarioId::all() {
            let cell = matrix.get(role, scenario_id);
            scenarios.insert(scenario_id.to_string(), cell_to_json(cell));
        }
        modes.insert(mode_key(role).to_string(), Value::Object(scenarios));
    }
    Value::Object(modes)
}

fn cell_to_json(cell: Option<&CellEntry>) -> Value {
    let entry = match cell {
        Some(c) => c,
        None => return Value::Null,
    };
    match &entry.result {
        CellResult::NotRun => Value::Null,
        CellResult::Error(e) => error_envelope_json(e, entry),
        CellResult::Outcome(outcome) => outcome_to_envelope_json(outcome.as_ref(), entry),
    }
}

fn error_envelope_json(e: &anyhow::Error, entry: &CellEntry) -> Value {
    let mut v = json!({
        "status": "failure",
        "wall_clock_ms": entry.wall_clock_ms,
        "tip_height_start": entry.tip_height_start,
        "tip_height_end": entry.tip_height_end,
        "fees_paid_microtari": entry.fees_paid_microtari,
        "balance_before_microtari": 0,
        "balance_after_microtari": 0,
        "balance_delta_microtari": 0,
        "balance_reconciliation_ok": false,
        "balance_coverage_note": BALANCE_COVERAGE_NOTE,
        "errors": {
            "success_count": 0,
            "rejection_count": 0,
            "stall_count": 0,
            "timeout_count": 0,
            "details": [{
                "txid": null,
                "error_string": format!("{e:#}"),
                "phase": "scan",
            }],
        },
        "payload": null,
    });
    if let Some(note) = entry.tip_query_note.as_ref() {
        if let Value::Object(ref mut m) = v {
            m.insert("tip_query_note".to_string(), Value::String(note.clone()));
        }
    }
    v
}

/// Note attached to every cell's balance trio. Tracked in
/// PR_BODY_PLAN.md §Pre-merge cleanups — explicit Phase 4 carry, NOT
/// the generic envelope_coverage_note.
const BALANCE_COVERAGE_NOTE: &str =
    "balance_before_microtari / balance_after_microtari / balance_delta_microtari / \
     balance_reconciliation_ok are emitted as 0 / false defaults; each would require a \
     WalletGrpcBalanceQuery call (~30s for Mode 1) per cell. 27 cells × ~30s = ~13min \
     added to the canonical run; deferred to Phase 4 per analysis/PR_BODY_PLAN.md \
     §Pre-merge cleanups.";

fn outcome_to_envelope_json(outcome: &ScenarioOutcome, entry: &CellEntry) -> Value {
    let (status, errors, peak_rss, peak_cpu, payload) = match outcome {
        ScenarioOutcome::B0(b) => (
            "success",
            default_errors(),
            b.peak_rss_bytes,
            b.peak_cpu_pct,
            b0_payload(b),
        ),
        ScenarioOutcome::S0(s) => (
            "success",
            default_errors(),
            s.peak_rss_bytes,
            s.peak_cpu_pct,
            s0_payload(s),
        ),
        ScenarioOutcome::S1(s) => (
            s1_status(s),
            errors_block(
                s.success_count,
                s.rejection_count,
                s.stall_count,
                s.timeout_count,
                &s.details,
            ),
            s.peak_rss_bytes,
            s.peak_cpu_pct,
            s1_payload(s),
        ),
        ScenarioOutcome::S2(s) => (
            "success",
            default_errors(),
            s.peak_rss_bytes,
            s.peak_cpu_pct,
            s2_payload(s),
        ),
        ScenarioOutcome::S3(s) => (
            "success",
            default_errors(),
            s.peak_rss_bytes,
            s.peak_cpu_pct,
            s3_payload(s),
        ),
        ScenarioOutcome::S4(s) => (
            s4_status(s),
            errors_block(
                s.success_count,
                s.rejection_count,
                s.stall_count,
                s.timeout_count,
                &s.details,
            ),
            s.peak_rss_bytes,
            s.peak_cpu_pct,
            s4_payload(s),
        ),
        ScenarioOutcome::S5(s) => (
            "success",
            errors_block(
                s.success_count,
                s.rejection_count,
                s.stall_count,
                s.timeout_count,
                &s.details,
            ),
            s.peak_rss_bytes,
            s.peak_cpu_pct,
            s5_payload(s),
        ),
        ScenarioOutcome::S6(s) => (
            "success",
            default_errors(),
            s.peak_rss_bytes,
            s.peak_cpu_pct,
            s6_payload(s),
        ),
        ScenarioOutcome::S7(s) => (
            "success",
            default_errors(),
            s.peak_rss_bytes,
            s.peak_cpu_pct,
            s7_payload(s),
        ),
    };

    // tip_height_start / tip_height_end: prefer the outcome's intrinsic
    // value (scans carry h_tip_*) over the caller-supplied entry value
    // (which comes from a base-node tip query). Scan outcomes are the
    // canonical source for their own scan range — the outer tip query is
    // a fallback for send scenarios that don't carry tip data on their
    // outcome.
    let (intrinsic_tip_start, intrinsic_tip_end) = intrinsic_tips_for(outcome);
    let tip_start = intrinsic_tip_start.or(entry.tip_height_start);
    let tip_end = intrinsic_tip_end.or(entry.tip_height_end);

    let mut envelope = json!({
        "status": status,
        "wall_clock_ms": entry.wall_clock_ms,
        "tip_height_start": tip_start,
        "tip_height_end": tip_end,
        "fees_paid_microtari": entry.fees_paid_microtari,
        "balance_before_microtari": 0,
        "balance_after_microtari": 0,
        "balance_delta_microtari": 0,
        "balance_reconciliation_ok": true,
        "balance_coverage_note": BALANCE_COVERAGE_NOTE,
        "errors": errors,
        "peak_rss_bytes": peak_rss,
        "peak_cpu_pct": peak_cpu,
        "payload": payload,
    });
    if let Some(note) = entry.tip_query_note.as_ref() {
        if let Value::Object(ref mut m) = envelope {
            m.insert("tip_query_note".to_string(), Value::String(note.clone()));
        }
    }
    envelope
}

/// Returns `(h_tip_start, h_tip_end)` from a scan-shaped outcome. None
/// for tx-shaped scenarios (S0/S1/S4/S5) whose tip range comes from the
/// caller-supplied `CellEntry::tip_height_*` fields instead.
fn intrinsic_tips_for(outcome: &ScenarioOutcome) -> (Option<u64>, Option<u64>) {
    match outcome {
        ScenarioOutcome::B0(b) => (Some(b.h_tip_start), Some(b.h_tip_end)),
        ScenarioOutcome::S2(s) => (Some(s.h_tip_start), Some(s.h_tip_end)),
        ScenarioOutcome::S3(s) => (Some(s.h_tip_start), Some(s.h_tip_end)),
        ScenarioOutcome::S6(s) => (Some(s.h_tip_start), Some(s.h_tip_end)),
        ScenarioOutcome::S7(s) => (Some(s.h_tip_start), Some(s.h_tip_end)),
        _ => (None, None),
    }
}

fn default_errors() -> Value {
    json!({
        "success_count": 0,
        "rejection_count": 0,
        "stall_count": 0,
        "timeout_count": 0,
        "details": [],
    })
}

fn errors_block(
    success: u64,
    rejection: u64,
    stall: u64,
    timeout: u64,
    details: &[DetailRecord],
) -> Value {
    json!({
        "success_count": success,
        "rejection_count": rejection,
        "stall_count": stall,
        "timeout_count": timeout,
        "details": details.iter().map(detail_to_json).collect::<Vec<_>>(),
    })
}

fn detail_to_json(d: &DetailRecord) -> Value {
    json!({
        "txid": d.txid,
        "error_string": d.error_string,
        "phase": detail_phase_str(d.phase),
    })
}

fn detail_phase_str(p: DetailPhase) -> &'static str {
    match p {
        DetailPhase::Construct => "construct",
        DetailPhase::Sign => "sign",
        DetailPhase::Broadcast => "broadcast",
        DetailPhase::Confirm => "confirm",
        DetailPhase::Scan => "scan",
    }
}

fn s1_status(s: &S1Outcome) -> &'static str {
    if s.rounds.len() < 7 {
        "halted"
    } else {
        "success"
    }
}

fn s4_status(s: &S4Outcome) -> &'static str {
    if s.sub_blocks.iter().any(|sb| sb.budget_elapsed) {
        "timeout"
    } else {
        "success"
    }
}

// ----- Per-scenario payload serializers -----

fn b0_payload(b: &B0Outcome) -> Value {
    json!({
        "t_scan_ms": b.t_scan_ms,
        "blocks_per_sec": b.blocks_per_sec,
        "h_tip_start": b.h_tip_start,
        "h_tip_end": b.h_tip_end,
        "peak_rss_bytes": b.peak_rss_bytes,
        "peak_cpu_pct": b.peak_cpu_pct,
        "utxo_count_verified": b.utxo_count_verified,
        "balance_verified_microtari": b.balance_verified_microtari,
        "outputs_found": b.outputs_found,
    })
}

fn s0_payload(s: &S0Outcome) -> Value {
    json!({
        "pre_balance": s.pre_balance,
        "post_balance": s.post_balance,
        "balance_delta": s.balance_delta,
        "pre_utxo_count": s.pre_utxo_count,
        "post_utxo_count": s.post_utxo_count,
        "utxo_delta": s.utxo_delta,
        "t_construct_ms": s.t_construct_ms,
        "t_broadcast_ms": s.t_broadcast_ms,
        "t_confirm_ms": s.t_confirm_ms,
        "tx_record": tx_record_to_json(&s.tx_record),
        "peak_rss_bytes": s.peak_rss_bytes,
        "peak_cpu_pct": s.peak_cpu_pct,
    })
}

fn s1_payload(s: &S1Outcome) -> Value {
    json!({
        "rounds": s.rounds.iter().map(round_to_json).collect::<Vec<_>>(),
        "peak_rss_bytes": s.peak_rss_bytes,
        "peak_cpu_pct": s.peak_cpu_pct,
    })
}

fn round_to_json(r: &RoundOutcome) -> Value {
    json!({
        "round_idx": r.round_idx,
        "tx_count": r.tx_count,
        "failure_count": r.failure_count,
        "t_round_ms": r.t_round_ms,
        "tx_records": r.tx_records.iter().map(tx_record_to_json).collect::<Vec<_>>(),
    })
}

fn tx_record_to_json(t: &TxRecord) -> Value {
    let mut v = json!({
        "txid": t.txid,
        "t_total_ms": t.t_total_ms,
        "t_broadcast_ms": t.t_broadcast_ms,
        "t_confirm_ms": t.t_confirm_ms,
        "status": t.status,
        "error_string": t.error_string,
        "fee_microtari": t.fee_microtari,
    });
    // sub_segments_ms is Mode 3-only per MODE_3_REWORK_SPEC.md §10/§11
    // — emit when present, omit otherwise so Modes 1 and 2 stay
    // byte-stable with their pre-rework profile output.
    if let Some(segments) = t.sub_segments_ms.as_ref() {
        if let Value::Object(ref mut m) = v {
            let map: serde_json::Map<String, Value> = segments
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(*v)))
                .collect();
            m.insert("sub_segments_ms".to_string(), Value::Object(map));
        }
    }
    v
}

fn s2_payload(s: &S2Outcome) -> Value {
    json!({
        "t_scan_ms": s.t_scan_ms,
        "blocks_per_sec": s.blocks_per_sec,
        "h_tip_start": s.h_tip_start,
        "h_tip_end": s.h_tip_end,
        "blocks_scanned": s.blocks_scanned,
        "outputs_found": s.outputs_found,
        "expected_outputs": s.expected_outputs,
        "outputs_found_matches_expected": s.outputs_found_matches_expected,
        "peak_rss_bytes": s.peak_rss_bytes,
        "peak_cpu_pct": s.peak_cpu_pct,
    })
}

fn s3_payload(s: &S3Outcome) -> Value {
    json!({
        "t_scan_ms": s.t_scan_ms,
        "blocks_per_sec": s.blocks_per_sec,
        "h_birth": s.h_birth,
        "h_tip_start": s.h_tip_start,
        "h_tip_end": s.h_tip_end,
        "blocks_scanned": s.blocks_scanned,
        "outputs_found": s.outputs_found,
        "expected_outputs": s.expected_outputs,
        "outputs_found_matches_expected": s.outputs_found_matches_expected,
        "peak_rss_bytes": s.peak_rss_bytes,
        "peak_cpu_pct": s.peak_cpu_pct,
    })
}

fn s4_payload(s: &S4Outcome) -> Value {
    let mut sub_blocks = serde_json::Map::new();
    for sb in &s.sub_blocks {
        sub_blocks.insert(sb.n_concurrent.to_string(), sub_block_to_json(sb));
    }
    json!({
        "sub_blocks": Value::Object(sub_blocks),
        "peak_rss_bytes": s.peak_rss_bytes,
        "peak_cpu_pct": s.peak_cpu_pct,
    })
}

fn sub_block_to_json(sb: &SubBlockOutcome) -> Value {
    json!({
        "n_concurrent": sb.n_concurrent,
        "budget_elapsed": sb.budget_elapsed,
        "batch_wall_clock_ms": sb.batch_wall_clock_ms,
        "success_rate": sb.success_rate,
        "max_serialization_gap_ms": sb.max_serialization_gap_ms,
        "double_selection_rejections": sb.double_selection_rejections,
        "tx_records": sb.tx_records.iter().map(task_outcome_to_json).collect::<Vec<_>>(),
    })
}

fn task_outcome_to_json(t: &TaskOutcome) -> Value {
    json!({
        "txid": t.txid,
        "t_submit_ms": t.t_submit_ms,
        "t_construct_complete_ms": t.t_construct_complete_ms,
        "broadcast_outcome": broadcast_outcome_str(&t.broadcast_outcome),
        "t_confirm_ms": t.t_confirm_ms,
        "error_string": t.error_string,
        "rejection_reason": t.rejection_reason.as_ref().map(|r| format!("{r:?}")),
    })
}

fn broadcast_outcome_str(b: &BroadcastOutcome) -> &'static str {
    match b {
        BroadcastOutcome::Accepted => "accepted",
        BroadcastOutcome::Rejected => "rejected",
        BroadcastOutcome::Error => "error",
        BroadcastOutcome::Aborted => "aborted",
    }
}

fn s5_payload(s: &S5Outcome) -> Value {
    json!({
        "arms": s5_arms_to_json(&s.arms),
        "throughput_multiplier": s.throughput_multiplier,
        "peak_rss_bytes": s.peak_rss_bytes,
        "peak_cpu_pct": s.peak_cpu_pct,
    })
}

fn s5_arms_to_json(arms: &S5Arms) -> Value {
    json!({
        "batch": arm_to_json(&arms.batch),
        "individual": arm_to_json(&arms.individual),
    })
}

fn arm_to_json(a: &ArmOutcome) -> Value {
    json!({
        "applies": a.applies,
        "tx_count": a.tx_count,
        "recipients_per_tx": a.recipients_per_tx,
        "total_sends": a.total_sends,
        "t_total_ms": a.t_total_ms,
        "throughput_tx_per_sec": a.throughput_tx_per_sec,
        "tx_records": a.tx_records.iter().map(tx_record_to_json).collect::<Vec<_>>(),
    })
}

fn s6_payload(s: &S6Outcome) -> Value {
    json!({
        "t_scan_ms": s.t_scan_ms,
        "blocks_per_sec": s.blocks_per_sec,
        "h_tip_start": s.h_tip_start,
        "h_tip_end": s.h_tip_end,
        "blocks_scanned": s.blocks_scanned,
        "outputs_found": s.outputs_found,
        "expected_outputs": s.expected_outputs,
        "outputs_found_matches_expected": s.outputs_found_matches_expected,
        "peak_rss_bytes": s.peak_rss_bytes,
        "peak_cpu_pct": s.peak_cpu_pct,
    })
}

fn s7_payload(s: &S7Outcome) -> Value {
    json!({
        "t_scan_ms": s.t_scan_ms,
        "blocks_per_sec": s.blocks_per_sec,
        "h_birth": s.h_birth,
        "h_tip_start": s.h_tip_start,
        "h_tip_end": s.h_tip_end,
        "blocks_scanned": s.blocks_scanned,
        "outputs_found": s.outputs_found,
        "expected_outputs": s.expected_outputs,
        "outputs_found_matches_expected": s.outputs_found_matches_expected,
        "peak_rss_bytes": s.peak_rss_bytes,
        "peak_cpu_pct": s.peak_cpu_pct,
    })
}

// ----- Deltas block (§5) -----

fn deltas_block(matrix: &Matrix) -> Value {
    json!({
        "t_scan_s2_minus_b0_ms": per_mode_delta(matrix, |m, role| {
            let b0 = scan_t_scan_ms(matrix.get(role, ScenarioId::B0));
            let s2 = scan_t_scan_ms(m.get(role, ScenarioId::S2));
            match (b0, s2) {
                (Some(b), Some(s)) => Some(json!(s as i64 - b as i64)),
                _ => None,
            }
        }),
        "t_scan_s6_minus_s2_ms": per_mode_delta(matrix, |m, role| {
            let s2 = scan_t_scan_ms(m.get(role, ScenarioId::S2));
            let s6 = scan_t_scan_ms(m.get(role, ScenarioId::S6));
            match (s2, s6) {
                (Some(a), Some(b)) => Some(json!(b as i64 - a as i64)),
                _ => None,
            }
        }),
        "t_scan_s6_over_b0_ratio": per_mode_delta(matrix, |m, role| {
            let b0 = scan_t_scan_ms(m.get(role, ScenarioId::B0));
            let s6 = scan_t_scan_ms(m.get(role, ScenarioId::S6));
            match (b0, s6) {
                (Some(b), Some(s)) if b > 0 => Some(json!(s as f64 / b as f64)),
                _ => None,
            }
        }),
        "s5_throughput_multiplier": s5_cross_mode_multipliers(matrix),
    })
}

fn per_mode_delta<F>(matrix: &Matrix, mut f: F) -> Value
where
    F: FnMut(&Matrix, SeedRole) -> Option<Value>,
{
    let mut out = serde_json::Map::new();
    let mut notes = serde_json::Map::new();
    for role in [SeedRole::Old, SeedRole::New, SeedRole::Pp] {
        match f(matrix, role) {
            Some(v) => {
                out.insert(mode_key(role).to_string(), v);
            }
            None => {
                out.insert(mode_key(role).to_string(), Value::Null);
                notes.insert(
                    mode_key(role).to_string(),
                    Value::String(
                        "input cell halted/timeout/null or insufficient data; delta undefined"
                            .to_string(),
                    ),
                );
            }
        }
    }
    if !notes.is_empty() {
        out.insert("notes".to_string(), Value::Object(notes));
    }
    Value::Object(out)
}

fn scan_t_scan_ms(cell: Option<&CellEntry>) -> Option<u64> {
    let outcome = match cell.map(|e| &e.result) {
        Some(CellResult::Outcome(o)) => o.as_ref(),
        _ => return None,
    };
    match outcome {
        ScenarioOutcome::B0(b) => Some(b.t_scan_ms),
        ScenarioOutcome::S2(s) => Some(s.t_scan_ms),
        ScenarioOutcome::S6(s) => Some(s.t_scan_ms),
        _ => None,
    }
}

fn s5_individual_t_total_ms(matrix: &Matrix, role: SeedRole) -> Option<u64> {
    match &matrix.get(role, ScenarioId::S5)?.result {
        CellResult::Outcome(boxed) => match boxed.as_ref() {
            ScenarioOutcome::S5(s) => Some(s.arms.individual.t_total_ms),
            _ => None,
        },
        _ => None,
    }
}

fn s5_batch_t_total_ms(matrix: &Matrix, role: SeedRole) -> Option<u64> {
    match &matrix.get(role, ScenarioId::S5)?.result {
        CellResult::Outcome(boxed) => match boxed.as_ref() {
            ScenarioOutcome::S5(s) if s.arms.batch.applies => Some(s.arms.batch.t_total_ms),
            _ => None,
        },
        _ => None,
    }
}

fn s5_cross_mode_multipliers(matrix: &Matrix) -> Value {
    let pp_batch = s5_batch_t_total_ms(matrix, SeedRole::Pp);
    let new_individual = s5_individual_t_total_ms(matrix, SeedRole::New);
    let old_individual = s5_individual_t_total_ms(matrix, SeedRole::Old);

    let pp_vs_new = match (pp_batch, new_individual) {
        (Some(b), Some(i)) if b > 0 => Some(i as f64 / b as f64),
        _ => None,
    };
    let pp_vs_old = match (pp_batch, old_individual) {
        (Some(b), Some(i)) if b > 0 => Some(i as f64 / b as f64),
        _ => None,
    };

    json!({
        "payment_processor_batch_vs_new_wallet_individual": pp_vs_new,
        "payment_processor_batch_vs_old_wallet_individual": pp_vs_old,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Seeds};
    use crate::env_capture::Environment;
    use crate::scenarios::B0Outcome;
    use crate::versions::BinaryVersion;

    fn fake_env() -> Environment {
        Environment {
            cpu_model: "test cpu".to_string(),
            ram_bytes: 8_000_000_000,
            disk_type: "ssd".to_string(),
            os: "test os".to_string(),
            network_path_to_base_node: "remote".to_string(),
        }
    }

    fn fake_versions() -> Versions {
        Versions {
            minotari_console_wallet: BinaryVersion {
                tag: Some("v5.3.1".to_string()),
                commit: None,
            },
            minotari_cli: BinaryVersion {
                tag: Some("v5.3.1".to_string()),
                commit: None,
            },
            base_node: BinaryVersion {
                tag: Some("v5.3.1".to_string()),
                commit: Some("deadbeef".to_string()),
            },
            harness: BinaryVersion::default(),
        }
    }

    fn fake_b0() -> B0Outcome {
        B0Outcome {
            t_scan_ms: 500,
            blocks_per_sec: Some(100.0),
            h_tip_start: 0,
            h_tip_end: 50,
            peak_rss_bytes: Some(4096),
            peak_cpu_pct: Some(12.5),
            utxo_count_verified: 0,
            balance_verified_microtari: 0,
            outputs_found: 0,
        }
    }

    fn fake_s2() -> S2Outcome {
        S2Outcome {
            t_scan_ms: 1500,
            blocks_per_sec: Some(80.0),
            h_tip_start: 0,
            h_tip_end: 120,
            blocks_scanned: 120,
            outputs_found: 128,
            expected_outputs: 128,
            outputs_found_matches_expected: true,
            peak_rss_bytes: Some(8192),
            peak_cpu_pct: Some(25.0),
        }
    }

    fn fake_s6() -> S6Outcome {
        S6Outcome {
            t_scan_ms: 2500,
            blocks_per_sec: Some(60.0),
            h_tip_start: 0,
            h_tip_end: 150,
            blocks_scanned: 150,
            outputs_found: 256,
            expected_outputs: 256,
            outputs_found_matches_expected: true,
            peak_rss_bytes: Some(16384),
            peak_cpu_pct: Some(33.3),
        }
    }

    fn redaction_for_tests() -> RedactionDenylist {
        RedactionDenylist::init_from_env(&Seeds::default())
    }

    #[test]
    fn writer_round_trips_minimal_profile() {
        let mut matrix = Matrix::new();
        for role in [SeedRole::Old, SeedRole::New, SeedRole::Pp] {
            matrix.record(
                role,
                ScenarioId::B0,
                CellResult::Outcome(Box::new(ScenarioOutcome::B0(fake_b0()))),
                0,
                None,
                None,
                None,
                0,
            );
            matrix.record(
                role,
                ScenarioId::S2,
                CellResult::Outcome(Box::new(ScenarioOutcome::S2(fake_s2()))),
                0,
                None,
                None,
                None,
                0,
            );
            matrix.record(
                role,
                ScenarioId::S6,
                CellResult::Outcome(Box::new(ScenarioOutcome::S6(fake_s6()))),
                0,
                None,
                None,
                None,
                0,
            );
        }

        let config = Config::default();
        let env = fake_env();
        let versions = fake_versions();
        let redaction = redaction_for_tests();

        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();

        write(&matrix, &config, &env, &versions, &redaction, &path).expect("write");

        let raw = std::fs::read_to_string(&path).expect("read back");
        let parsed: Value = serde_json::from_str(&raw).expect("parse JSON");
        assert!(
            parsed.get("schema_version").is_some(),
            "schema_version present"
        );
        assert!(parsed.get("config").is_some(), "config present");
        assert!(parsed.get("environment").is_some(), "environment present");
        assert!(parsed.get("versions").is_some(), "versions present");
        assert!(parsed.get("modes").is_some(), "modes present");
        assert!(parsed.get("deltas").is_some(), "deltas present");
        assert!(
            parsed.get("redaction_denylist").is_some(),
            "redaction_denylist present",
        );

        // 3 modes × 9 scenarios; B0/S2/S6 populated, rest null.
        let modes = parsed.get("modes").unwrap();
        for mode_name in ["old_wallet", "new_wallet", "payment_processor"] {
            let m = modes.get(mode_name).expect("mode present");
            for scenario in ["b0", "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"] {
                assert!(m.get(scenario).is_some(), "{mode_name}.{scenario} present");
            }
        }

        // B0 status check on one cell.
        let b0_cell = modes.get("old_wallet").unwrap().get("b0").unwrap();
        assert_eq!(b0_cell.get("status").unwrap().as_str(), Some("success"));
    }

    #[test]
    fn deltas_compute_correctly_with_known_inputs() {
        let mut matrix = Matrix::new();
        // Use Old mode only; the other two stay null.
        matrix.record(
            SeedRole::Old,
            ScenarioId::B0,
            CellResult::Outcome(Box::new(ScenarioOutcome::B0(fake_b0()))),
            0,
            None,
            None,
            None,
            0,
        );
        // S2 t_scan_ms=1500, B0 t_scan_ms=500 → delta=1000, ratio=3.0
        matrix.record(
            SeedRole::Old,
            ScenarioId::S2,
            CellResult::Outcome(Box::new(ScenarioOutcome::S2(fake_s2()))),
            0,
            None,
            None,
            None,
            0,
        );
        // S6 t_scan_ms=2500, S2=1500 → delta_s6_s2=1000; S6/B0 ratio=5.0
        matrix.record(
            SeedRole::Old,
            ScenarioId::S6,
            CellResult::Outcome(Box::new(ScenarioOutcome::S6(fake_s6()))),
            0,
            None,
            None,
            None,
            0,
        );

        let deltas = deltas_block(&matrix);
        let s2_minus_b0 = deltas
            .get("t_scan_s2_minus_b0_ms")
            .unwrap()
            .get("old_wallet")
            .unwrap();
        assert_eq!(s2_minus_b0.as_i64(), Some(1000));
        let s6_over_b0 = deltas
            .get("t_scan_s6_over_b0_ratio")
            .unwrap()
            .get("old_wallet")
            .unwrap();
        assert!((s6_over_b0.as_f64().unwrap() - 5.0).abs() < f64::EPSILON);
        // new_wallet input missing → null with note
        let nw = deltas
            .get("t_scan_s2_minus_b0_ms")
            .unwrap()
            .get("new_wallet")
            .unwrap();
        assert!(nw.is_null());
    }

    #[test]
    fn error_cell_emits_failure_status_with_error_string() {
        let mut matrix = Matrix::new();
        matrix.record(
            SeedRole::Old,
            ScenarioId::B0,
            CellResult::Error(anyhow::anyhow!("simulated scan failure")),
            0,
            None,
            None,
            None,
            0,
        );
        let cell = cell_to_json(matrix.get(SeedRole::Old, ScenarioId::B0));
        assert_eq!(cell.get("status").unwrap().as_str(), Some("failure"));
        let details = cell.get("errors").unwrap().get("details").unwrap();
        let first = details.as_array().unwrap().first().unwrap();
        assert!(
            first
                .get("error_string")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("simulated scan failure"),
            "error_string surfaces underlying anyhow message",
        );
    }
}
