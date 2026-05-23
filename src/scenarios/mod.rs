//! Scenario layer — B0/S0/S1/S2/S3/S4/S5/S6/S7.
//!
//! Per `analysis/DESIGN.md §Scenario state machine` and
//! `analysis/RESULT_PROFILE_SCHEMA.md §4`, scenario code is mode-agnostic:
//! every scenario receives a `&mut dyn Mode` and orchestrates the
//! `mode.send_*` / `mode.scan_from_birthday` / `mode.get_*` primitives
//! into a per-cell outcome. Cells for the 27 (mode × scenario) matrix are
//! distinct enough in shape — B0/S2/S3/S6/S7 are scan-shaped, S0/S1 are
//! transaction-shaped with `rounds[]`, S4 has a `sub_blocks` map, S5 has
//! `arms.batch` / `arms.individual` — that the per-scenario outcome lives
//! as a separate variant on a sum-type [`ScenarioOutcome`] rather than a
//! flat struct of `Option<…>` fields. This mirrors `RESULT_PROFILE_SCHEMA.md
//! §4`'s split into per-scenario subsections.
//!
//! **No retry, backoff, throttling, or partitioning** anywhere in scenario
//! code per AC-30/31/32 — same constraint as `src/modes/*`.
//!
//! This module is added one scenario at a time per `DESIGN.md §swe-impl
//! execution order`. Step 3i.1.a.2 lands the trait + `ScenarioId` enum +
//! dispatch skeleton. Step 3i.1.a.3 lands B0. Subsequent steps land S0..S7.

mod b0_baseline;
mod s0_warmup;
mod s1_volume;

pub use b0_baseline::B0Outcome;
pub use s0_warmup::S0Outcome;
pub use s1_volume::{RoundOutcome, S1Outcome};

use tari_common_types::tari_address::TariAddress;

use crate::config::Config;
use crate::modes::Mode;

/// One record per non-success event surfaced by a scenario, mirroring
/// `RESULT_PROFILE_SCHEMA.md §errors sub-object` line 116:
/// `details: array<object>` with `{ txid, error_string, phase }`. Lives
/// on `ScenarioOutcome` variants that record cell-level failure detail —
/// shared across S1..S7 because every scenario folds construct/sign/
/// broadcast/confirm/scan-phase failures through the same schema slot.
#[derive(Debug, Clone, PartialEq)]
pub struct DetailRecord {
    /// Transaction ID when known, `None` for failures that occurred
    /// before the txid was assigned (construct-phase failures).
    pub txid: Option<String>,
    /// Free-form error description; schema §errors.details[] line 116.
    pub error_string: String,
    /// Pipeline phase at which the failure was observed.
    pub phase: DetailPhase,
}

/// Per `RESULT_PROFILE_SCHEMA.md §errors sub-object` line 116:
/// `phase ∈ {"construct", "sign", "broadcast", "confirm", "scan"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailPhase {
    /// `minotari create-unsigned-transaction` subprocess or in-process
    /// construction failure.
    Construct,
    /// `sign_locked_transaction` (Mode 2/3) failure.
    Sign,
    /// `Broadcaster::submit_transaction` failure.
    Broadcast,
    /// Confirmation-poll failure (NOT timeout — timeouts feed `stall_count`
    /// per schema line 114).
    Confirm,
    /// Wallet scan failure (B0/S2/S3/S6/S7).
    Scan,
}

impl std::fmt::Display for DetailPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Construct => "construct",
            Self::Sign => "sign",
            Self::Broadcast => "broadcast",
            Self::Confirm => "confirm",
            Self::Scan => "scan",
        })
    }
}

/// Per-run context passed to scenarios. Carries the harness configuration
/// (which scenarios read for `c_min`, `a_fund`, `fee_rate`,
/// `per_tx_confirmation_timeout_ms`, etc.) plus the harness-controlled
/// recipient address scenarios send to.
///
/// Lifted out of `run_scenario`'s arguments so the dispatch signature stays
/// stable as additional scenarios that need configuration (S1..S7) come
/// online. B0 ignores all fields; S0 reads them.
pub struct ScenarioCtx<'a> {
    /// Harness configuration (mirrors `RESULT_PROFILE_SCHEMA.md §1`).
    pub config: &'a Config,
    /// Destination address for scenario-level sends (S0's funding-style tx,
    /// S1's UTXO-multiplication rounds, S4's concurrent dispatch, S5's
    /// arm-specific recipient lists). The harness's run loop derives this
    /// from the configured seed environment per `DESIGN.md §Scenario state
    /// machine §S0`.
    pub recipient: &'a TariAddress,
}

/// Canonical ordering of the 9 scenario IDs that make up each mode's column
/// in the 27-cell matrix. `Display` matches the scenario names used as keys
/// under `results.<mode>.<scenario>` in `RESULT_PROFILE_SCHEMA.md §4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScenarioId {
    /// B0 — from-genesis archival scan against unfunded wallet.
    B0,
    /// S0 — single funding transaction; produces `h_birth`.
    S0,
    /// S1 — UTXO multiplication: 6 doubling rounds + 1 fan-out → 512 outputs.
    S1,
    /// S2 — wipe + birthday=0 + rescan; expects 512 outputs found.
    S2,
    /// S3 — wipe + birthday=h_birth + rescan.
    S3,
    /// S4 — concurrent construction, N ∈ {8,16,32,64,128}.
    S4,
    /// S5 — batch vs. individual arms across the 100-recipient list.
    S5,
    /// S6 — S2-shape after S5.
    S6,
    /// S7 — S3-shape after S5.
    S7,
}

impl ScenarioId {
    /// Canonical ordering matching the cell-walk order in
    /// `RESULT_PROFILE_SCHEMA.md §4` and `DESIGN.md §Scenario state machine`.
    pub fn all() -> [ScenarioId; 9] {
        [
            Self::B0,
            Self::S0,
            Self::S1,
            Self::S2,
            Self::S3,
            Self::S4,
            Self::S5,
            Self::S6,
            Self::S7,
        ]
    }
}

impl std::fmt::Display for ScenarioId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::B0 => "b0",
            Self::S0 => "s0",
            Self::S1 => "s1",
            Self::S2 => "s2",
            Self::S3 => "s3",
            Self::S4 => "s4",
            Self::S5 => "s5",
            Self::S6 => "s6",
            Self::S7 => "s7",
        })
    }
}

/// Per-(mode, scenario) outcome — one variant per scenario.
///
/// Sum-type rather than flat struct: cells differ substantially in shape
/// per `RESULT_PROFILE_SCHEMA.md §4` (B0/S2/S3/S6/S7 are scan-shaped, S0/S1
/// are tx-shaped with `rounds[]`, S4 has a `sub_blocks` map, S5 has
/// `arms.batch` / `arms.individual`). A flat struct would degenerate to
/// ~25 `Option<…>` fields where only ~3-9 are set per cell — the enum
/// keeps each variant's shape close to its `§4` table.
///
/// The result-profile writer (step 3i.2) matches on the variant to emit the
/// correct JSON key-set under `results.<mode>.<scenario>`.
///
/// Variants are introduced one at a time as scenarios are implemented; the
/// `#[non_exhaustive]` attribute is intentionally absent — the harness owns
/// the type and adds variants in lockstep with scenario impls.
#[derive(Debug, Clone, PartialEq)]
pub enum ScenarioOutcome {
    /// B0 — from-genesis archival scan against unfunded wallet (AC-10).
    B0(B0Outcome),
    /// S0 — single funding-style transaction; produces `h_birth` (AC-11).
    S0(S0Outcome),
    /// S1 — UTXO multiplication across 7 doubling rounds (AC-12/13/14).
    S1(S1Outcome),
    // Subsequent variants land per `DESIGN.md §swe-impl execution order`:
    //   …      → S2..S7
}

/// Run the named scenario against the given mode.
///
/// Dispatches to per-scenario impl based on `id`. No registry, no factory —
/// scenarios are added one at a time as the workspace fills out per
/// `DESIGN.md §swe-impl execution order`. B0 lands in step 3i.1.a.3; S0
/// lands in step 3i.1.b; S1..S7 follow.
pub async fn run_scenario(
    id: ScenarioId,
    ctx: &ScenarioCtx<'_>,
    mode: &mut dyn Mode,
) -> anyhow::Result<ScenarioOutcome> {
    match id {
        ScenarioId::B0 => b0_baseline::run(mode).await.map(ScenarioOutcome::B0),
        ScenarioId::S0 => s0_warmup::run(ctx.config, mode, ctx.recipient)
            .await
            .map(ScenarioOutcome::S0),
        ScenarioId::S1 => s1_volume::run(ctx.config, mode, ctx.recipient, None)
            .await
            .map(ScenarioOutcome::S1),
        ScenarioId::S2
        | ScenarioId::S3
        | ScenarioId::S4
        | ScenarioId::S5
        | ScenarioId::S6
        | ScenarioId::S7 => anyhow::bail!(
            "scenario {id} not yet implemented (step 3i.1.c lands S1; \
             subsequent scenarios follow in DESIGN.md swe-impl execution order)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_id_all_is_canonical_order() {
        let ids = ScenarioId::all();
        assert_eq!(ids.len(), 9, "9 scenarios per RESULT_PROFILE_SCHEMA.md §4");
        assert_eq!(ids[0], ScenarioId::B0);
        assert_eq!(ids[1], ScenarioId::S0);
        assert_eq!(ids[8], ScenarioId::S7);
    }

    #[test]
    fn scenario_id_display_matches_schema_keys() {
        // Schema names (RESULT_PROFILE_SCHEMA.md §4) lowercase the
        // scenario letters; matrix is `results.<mode>.b0`, `…s0`, etc.
        assert_eq!(ScenarioId::B0.to_string(), "b0");
        assert_eq!(ScenarioId::S0.to_string(), "s0");
        assert_eq!(ScenarioId::S7.to_string(), "s7");
    }
}
