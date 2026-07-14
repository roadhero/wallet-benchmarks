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
mod s2_full_rescan;
mod s3_birthday_rescan;
mod s4_concurrent;
mod s5_throughput;
mod s6_full_rescan_after;
mod s7_birthday_rescan_after;

pub use b0_baseline::B0Outcome;
pub use s0_warmup::S0Outcome;
pub use s1_volume::{RoundOutcome, S1Outcome};
pub use s2_full_rescan::S2Outcome;
pub use s3_birthday_rescan::S3Outcome;
pub use s4_concurrent::{BroadcastOutcome, S4Outcome, SubBlockOutcome, TaskOutcome};
pub use s5_throughput::{ArmOutcome, S5Arms, S5Outcome};
pub use s6_full_rescan_after::S6Outcome;
pub use s7_birthday_rescan_after::S7Outcome;

use tari_common_types::tari_address::TariAddress;

use crate::clock::Clock;
use crate::config::Config;
use crate::modes::Mode;
use crate::sampler::SamplerFactory;
use crate::seed::redact::RedactionDenylist;
use crate::seed::{SeedHandle, SeedRole};

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

/// Fail-fast policy for send loops (S1, S4, S5): counts CONTIGUOUS
/// failures whose error strings are byte-identical and signals abort when
/// the configured threshold is reached. A success or a failure with a
/// DIFFERENT string resets the streak; stalls (no error string) also
/// reset, since they are a distinct terminal class. Threshold 0 disables
/// the policy. One mechanism for every send loop per the uniform-policy
/// requirement; scan scenarios have no send loop, so it does not apply to
/// them by structure.
///
/// Identity is exact string match by design: transient node-side errors
/// vary their messages (heights, tx ids), while a systematically stuck
/// wallet repeats the same message verbatim (observed live: 127 identical
/// "Funds are pending" rejections).
pub(crate) struct FailureStreakTracker {
    threshold: usize,
    streak: Option<(String, usize)>,
}

impl FailureStreakTracker {
    pub(crate) fn new(threshold: usize) -> Self {
        Self {
            threshold,
            streak: None,
        }
    }

    /// Record a failure; returns `true` when the scenario should abort.
    pub(crate) fn observe_failure(&mut self, error_string: &str) -> bool {
        if self.threshold == 0 {
            return false;
        }
        match &mut self.streak {
            Some((s, n)) if s == error_string => {
                *n += 1;
            }
            _ => {
                self.streak = Some((error_string.to_string(), 1));
            }
        }
        self.streak
            .as_ref()
            .is_some_and(|(_, n)| *n >= self.threshold)
    }

    /// A success (or any non-failure terminal state) resets the streak.
    pub(crate) fn observe_success(&mut self) {
        self.streak = None;
    }

    /// The abort reason recorded into the cell's details when
    /// [`Self::observe_failure`] returned `true`.
    pub(crate) fn abort_reason(&self) -> String {
        match &self.streak {
            Some((s, n)) => format!(
                "aborted after {n} contiguous identical failures \
                 (fail_fast_identical_failure_threshold = {}): {s}",
                self.threshold,
            ),
            None => "aborted by fail-fast policy".to_string(),
        }
    }
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

/// Per-run context passed to scenarios. Carries the harness configuration,
/// the runtime seed handle, the result-profile redaction denylist, the
/// scenario clock, and the recipient strategy that picks an address per
/// per-tx slot.
///
/// Lifted out of `run_scenario`'s arguments so the dispatch signature stays
/// stable as additional scenarios that need configuration (S2..S7) come
/// online. B0 ignores all fields; S0/S1 read the relevant subset; S2-S7
/// will land on this shape.
pub struct ScenarioCtx<'a> {
    /// Harness configuration (mirrors `RESULT_PROFILE_SCHEMA.md §1`).
    pub config: &'a Config,
    /// Runtime seed accessor. S5's recipient pool derives addresses from
    /// the same seed material the harness already owns; future scenarios
    /// that need to address-derive on demand pull from here rather than
    /// re-reading env vars at the scenario layer.
    pub seeds: &'a SeedHandle,
    /// Result-profile redaction denylist. Plumbed through ctx so any
    /// scenario-level `error_string` capture can be checked against the
    /// denylist before being folded into a `DetailRecord`. Static for
    /// the duration of the run.
    pub redaction: &'a RedactionDenylist,
    /// Scenario clock — `RealClock` in production, `FakeClock` (under
    /// `#[cfg(test)]`) for deterministic test sleeps via
    /// `tokio::time::pause()`.
    pub clock: &'a dyn Clock,
    /// How to choose the destination address per per-tx slot. See
    /// [`RecipientStrategy`].
    pub recipients: RecipientStrategy<'a>,
    /// Per-scenario resource sampler factory. `None` disables sampling
    /// — the scenario's `peak_rss_bytes` / `peak_cpu_pct` fields stay
    /// `None`. Production code passes `Some(&LiveSamplerFactory)` via
    /// `main.rs` (3k). Tests default to `None` for fixtures that don't
    /// care about peaks, and `Some(&FakeSamplerFactory { ... })` for
    /// per-scenario peak-population assertions.
    pub sampler_factory: Option<&'a dyn SamplerFactory>,
}

/// Recipient picker for the per-tx send loop. Variants cover the three
/// patterns scenarios need:
///
/// * `SelfAddress(SeedRole)` — scenario sends to the wallet's own address,
///   derived from `ctx.seeds` for the named [`SeedRole`] slot. Used by the
///   send-to-self scenarios (S4/S6/S7 and any future scenario whose send
///   path returns funds to the sender wallet).
/// * `Fixed(&TariAddress)` — every tx in the scenario sends to the same
///   recipient. Used by S0/S1 against the harness's "second mode-2 seed".
/// * `Pool(&[TariAddress])` — round-robin recipient list. Used by S5 to
///   amortise the 100-recipient list across the per-tx loop.
pub enum RecipientStrategy<'a> {
    /// Send to the wallet's own address for the named [`SeedRole`] slot.
    /// Derived from `ctx.seeds` via [`SeedHandle::address_for`], so the
    /// derivation path matches the harness's funding pre-flight and the
    /// `print-address` subcommand.
    SelfAddress(SeedRole),
    /// Fixed single recipient for the whole scenario.
    Fixed(&'a TariAddress),
    /// Round-robin pool of recipients.
    Pool(&'a [TariAddress]),
}

impl<'a> RecipientStrategy<'a> {
    /// Resolve the recipient for the `tx_idx`-th tx in the scenario.
    ///
    /// * `SelfAddress(role)` → [`SeedHandle::address_for(role)`].
    /// * `Fixed(a)` → clones `a`.
    /// * `Pool(p)` → `p[tx_idx % p.len()]`; bails when `p` is empty.
    ///
    /// Takes `&SeedHandle` rather than `&dyn Mode` because the only variant
    /// that needs derivation reads from the harness's seed material, not
    /// from the mode's runtime state. Lets the per-scenario send loop pass
    /// `ctx.seeds` (already in scope) without threading `mode` through
    /// resolve sites that don't need it.
    pub fn resolve_for(&self, seeds: &SeedHandle, tx_idx: u32) -> anyhow::Result<TariAddress> {
        match self {
            RecipientStrategy::SelfAddress(role) => seeds.address_for(*role),
            RecipientStrategy::Fixed(a) => Ok((*a).clone()),
            RecipientStrategy::Pool([]) => {
                anyhow::bail!("RecipientStrategy::Pool is empty")
            }
            RecipientStrategy::Pool(p) => Ok(p[(tx_idx as usize) % p.len()].clone()),
        }
    }
}

/// Per-scenario inputs threaded into the [`run_scenario`] dispatch.
///
/// Most scenarios consume only `ScenarioCtx`; a few need additional inputs
/// derived from the prior scenarios in the run order (e.g. S2's
/// `expected_outputs` comes from S1's final UTXO count; S3's `h_birth`
/// comes from S0). Rather than expand [`ScenarioCtx`] with `Option<…>`
/// fields that are populated for two of nine scenarios and ignored for
/// the rest, the run loop (step 3i.2) carries this shape and supplies it
/// per dispatch.
///
/// Fields are added in lockstep with the scenarios that consume them.
/// S2 lands `expected_outputs_s2`; S3 will add `h_birth_s3`; S4/S5/S6/S7
/// follow as those scenarios come online.
#[derive(Debug, Clone, Default)]
pub struct ScenarioInput {
    /// AC-15 verification target: the count of outputs S2's rescan is
    /// expected to rediscover. Also serves AC-16's S3 target — the chain
    /// state is unchanged between S2 and S3, so both rescans verify
    /// against the same value. `None` for dispatches that don't need it;
    /// the S2/S3 arms bail with a clear error if `None`.
    pub expected_outputs_s2: Option<u64>,
    /// AC-16 birthday target: S0's funding height encoded as
    /// days-since-2022-01-01 per `Mode::scan_from_birthday`'s `u16`
    /// signature. `None` for dispatches that don't need it; the S3 arm
    /// bails with a clear error if `None`.
    pub h_birth_s3: Option<u16>,
    /// S5 (AC-19, AC-20) — which seed slot to derive the recipient pool
    /// from AND which mode the cell belongs to. The latter drives the
    /// batch-arm skip on Mode 1 (`SeedRole::Old` → `arms.batch.applies =
    /// false` because gRPC `Transfer` is single-recipient per
    /// `DESIGN.md §Mode 1` line 319). Routed through `ScenarioInput`
    /// rather than inspecting `mode.name()` so the scenario stays mode-
    /// agnostic at the type level (matches the existing dispatch shape
    /// for `expected_outputs_s2` / `h_birth_s3`). `None` for dispatches
    /// that don't need it; the S5 arm bails with a clear error if `None`.
    pub s5_seed_role_for_mode: Option<SeedRole>,
    /// AC-15-mirror verification target for S6: the count of outputs
    /// S6's post-S5 full-history rescan is expected to rediscover (S1's
    /// net plus S5's net). Conceptually distinct from
    /// `expected_outputs_s2` — S6 runs after a different upstream
    /// scenario — so it gets its own slot per the duplicate-with-purpose
    /// preference. `None` for dispatches that don't need it; the S6 arm
    /// bails with a clear error if `None`.
    pub s6_expected_outputs: Option<u64>,
    /// AC-16-mirror verification target for S7: same value as
    /// `s6_expected_outputs` (S1 net + S5 net — chain state is
    /// unchanged between S6 and S7). Kept as a separate slot per the
    /// duplicate-with-purpose preference (S7 runs after S6, but
    /// conceptually verifies the same target as S6 with a different
    /// scan window). `None` for dispatches that don't need it; the
    /// S7 arm bails with a clear error if `None`.
    pub s7_expected_outputs: Option<u64>,
    /// AC-16-mirror birthday target for S7: same shape as `h_birth_s3`
    /// (S0's funding height encoded as days-since-2022-01-01, `u16`).
    /// Kept as a separate slot per duplicate-with-purpose. `None` for
    /// dispatches that don't need it; the S7 arm bails with a clear
    /// error if `None`.
    pub s7_h_birth: Option<u16>,
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
    /// S2 — wipe + birthday=0 + full-history rescan (AC-15/AC-24/AC-34).
    S2(S2Outcome),
    /// S3 — wipe + birthday=`h_birth` + post-funding-height rescan (AC-16).
    S3(S3Outcome),
    /// S4 — concurrent construction, N ∈ {8,16,32,64,128} (AC-17/AC-18).
    S4(S4Outcome),
    /// S5 — batch vs. individual arms across the 100-recipient list
    /// (AC-19/AC-20).
    S5(S5Outcome),
    /// S6 — wipe + birthday=0 + full-history rescan after S5.
    S6(S6Outcome),
    /// S7 — wipe + birthday=`h_birth` + post-funding-height rescan
    /// after S5.
    S7(S7Outcome),
}

/// Run the named scenario against the given mode.
///
/// Dispatches to per-scenario impl based on `id`. No registry, no factory —
/// scenarios are added one at a time as the workspace fills out per
/// `DESIGN.md §swe-impl execution order`. B0 lands in step 3i.1.a.3; S0
/// lands in step 3i.1.b; S1 in step 3i.1.c; S2 in step 3i.1.d/e; S3..S7
/// follow.
///
/// `input` carries per-scenario inputs that the run loop derives from
/// prior scenarios — e.g. S2's `expected_outputs_s2`. Scenarios that
/// don't need extra inputs ignore the field; the relevant arm bails with
/// a clear error if a required input is missing.
pub async fn run_scenario(
    id: ScenarioId,
    ctx: &ScenarioCtx<'_>,
    mode: &mut dyn Mode,
    input: &ScenarioInput,
) -> anyhow::Result<ScenarioOutcome> {
    match id {
        ScenarioId::B0 => b0_baseline::run(ctx, mode).await.map(ScenarioOutcome::B0),
        ScenarioId::S0 => s0_warmup::run(ctx, mode).await.map(ScenarioOutcome::S0),
        ScenarioId::S1 => s1_volume::run(ctx, mode, None)
            .await
            .map(ScenarioOutcome::S1),
        ScenarioId::S2 => {
            let expected = input.expected_outputs_s2.ok_or_else(|| {
                anyhow::anyhow!(
                    "ScenarioId::S2 requires ScenarioInput::expected_outputs_s2 \
                     (AC-15 verification target)"
                )
            })?;
            s2_full_rescan::run(ctx, mode, expected)
                .await
                .map(ScenarioOutcome::S2)
        }
        ScenarioId::S3 => {
            let expected = input.expected_outputs_s2.ok_or_else(|| {
                anyhow::anyhow!(
                    "ScenarioId::S3 requires ScenarioInput::expected_outputs_s2 \
                     (AC-16 verification target — same as S2)"
                )
            })?;
            let h_birth = input.h_birth_s3.ok_or_else(|| {
                anyhow::anyhow!(
                    "ScenarioId::S3 requires ScenarioInput::h_birth_s3 \
                     (S0's funding height, u16 days-since-2022-01-01)"
                )
            })?;
            s3_birthday_rescan::run(ctx, mode, expected, h_birth)
                .await
                .map(ScenarioOutcome::S3)
        }
        ScenarioId::S4 => s4_concurrent::run(ctx, mode).await.map(ScenarioOutcome::S4),
        ScenarioId::S5 => {
            let role = input.s5_seed_role_for_mode.ok_or_else(|| {
                anyhow::anyhow!(
                    "ScenarioId::S5 requires ScenarioInput::s5_seed_role_for_mode \
                     (drives Mode 1 batch-arm skip per AC-20)"
                )
            })?;
            s5_throughput::run(ctx, mode, role)
                .await
                .map(ScenarioOutcome::S5)
        }
        ScenarioId::S6 => {
            let expected = input.s6_expected_outputs.ok_or_else(|| {
                anyhow::anyhow!(
                    "ScenarioId::S6 requires ScenarioInput::s6_expected_outputs \
                     (S1 net + S5 net; AC-15-mirror verification target)"
                )
            })?;
            s6_full_rescan_after::run(ctx, mode, expected)
                .await
                .map(ScenarioOutcome::S6)
        }
        ScenarioId::S7 => {
            let expected = input.s7_expected_outputs.ok_or_else(|| {
                anyhow::anyhow!(
                    "ScenarioId::S7 requires ScenarioInput::s7_expected_outputs \
                     (S1 net + S5 net; AC-16-mirror verification target)"
                )
            })?;
            let h_birth = input.s7_h_birth.ok_or_else(|| {
                anyhow::anyhow!(
                    "ScenarioId::S7 requires ScenarioInput::s7_h_birth \
                     (S0's funding height, u16 days-since-2022-01-01)"
                )
            })?;
            s7_birthday_rescan_after::run(ctx, mode, expected, h_birth)
                .await
                .map(ScenarioOutcome::S7)
        }
    }
}

/// CipherSeed birthday epoch: 2022-01-01 00:00:00 UTC in Unix seconds.
/// `change_birthday` encodes `u16` days since this instant (per
/// `analysis/ANALYSIS.md` Birthday encoding and the live wallet log
/// `birthday 1643 at epoch time 1782950400`, since
/// `1782950400 - 1643 * 86400 == 1640995200`).
const BIRTHDAY_EPOCH_UNIX_SECS: i64 = 1_640_995_200;
/// Seconds per day for the birthday day-count conversion.
const SECONDS_PER_DAY: i64 = 86_400;

/// The CipherSeed birthday (`u16` days-since-2022-01-01) for a wallet whose
/// funding transaction confirmed at `funding_unix_secs`.
///
/// S3/S7's `wipe_and_reimport(h_birth)` + `scan_from_birthday(h_birth)` need
/// S0's funding height expressed in the `u16` birthday format (`DESIGN.md §6`,
/// AC-16/AC-23). The birthday is fundamentally a date, not a block height:
/// S0's tx confirms within minutes of the run loop reaching
/// [`update_scenario_input`], so the wall-clock time at that point is S0's
/// funding time to day precision. One day is subtracted as a safety margin so
/// the encoded birthday is at or before the funding output even if the
/// funding tx and this computation straddle a UTC midnight; the resulting
/// birthday-scoped rescan still covers materially fewer blocks than S2's
/// from-genesis scan (genesis is day 0; a mid-2026 funding is day ~1640).
///
/// Clamped to `[0, u16::MAX]`: pre-epoch inputs floor at 0, and the
/// `u16::MAX` ceiling is ~2101, well beyond any realistic run date.
pub fn s0_funding_birthday(funding_unix_secs: i64) -> u16 {
    let days = (funding_unix_secs - BIRTHDAY_EPOCH_UNIX_SECS).max(0) / SECONDS_PER_DAY;
    let clamped = days.clamp(0, i64::from(u16::MAX)) as u16;
    clamped.saturating_sub(1)
}

/// Thread the just-completed scenario's results into `ScenarioInput` so
/// downstream scenarios in the same mode's loop pick up derived values
/// (S0's funding birthday to S3/S7; S1's `success_count` to S2/S3; S5's
/// `success_count` to S6/S7).
///
/// `now_unix_secs` is injected (rather than read from the wall clock inside)
/// so the S0 birthday derivation is deterministic under test, matching the
/// clock-injection pattern used by [`crate::wallet_lifecycle`]'s ready loop.
pub fn update_scenario_input(
    outcome: &ScenarioOutcome,
    input: &mut ScenarioInput,
    now_unix_secs: i64,
) {
    match outcome {
        ScenarioOutcome::S0(_s0) => {
            // The S3/S7 rescan birthday is wall-clock-derived and populated
            // at ScenarioInput construction in main.rs, NOT here: this arm
            // only runs on Ok outcomes, and coupling the birthday to S0's
            // success made an S0 error cascade into S3/S7 dispatch bails
            // (the maintainer's observed s0+s3+s7 err trio). Deliberate
            // no-op so the single source of truth is the run-start init.
            let _ = now_unix_secs;
        }
        ScenarioOutcome::S1(s1) => {
            // The chain's final UTXO count after S1's 7 rounds is the
            // expected scan target for S2 / S3.
            input.expected_outputs_s2 = Some(s1.success_count);
        }
        ScenarioOutcome::S5(s5) => {
            // After S5's send volume, the wallet's net output set is S1's
            // net plus S5's successful sends.
            let s5_successes = s5.success_count;
            input.s6_expected_outputs = input.expected_outputs_s2.map(|p| p + s5_successes);
            input.s7_expected_outputs = input.s6_expected_outputs;
        }
        _ => {}
    }
}

/// Cross-test fixtures for the scenario layer. Lets B0/S2/S3/S6/S7
/// tests (which don't currently maintain their own ctx fixture
/// machinery) build a minimal-no-sampler `ScenarioCtx` in two lines.
/// Same convention as `crate::modes::test_support::FakeMode`.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::clock::RealClock;
    use crate::config::{Config, Seeds};
    use crate::seed::redact::RedactionDenylist;
    use crate::seed::SeedHandle;

    /// Owns the lifetime-bound inputs for a [`ScenarioCtx`] in tests.
    /// The fields stay on `Self`; `ctx()` borrows them into a fresh
    /// `ScenarioCtx<'_>` per call.
    pub(crate) struct TestCtxOwner {
        pub config: Config,
        pub seeds: SeedHandle,
        pub redaction: RedactionDenylist,
        pub clock: RealClock,
    }

    impl TestCtxOwner {
        /// Build a default owner. `Config::default()`, `Seeds::default()`,
        /// `RedactionDenylist::for_test()`, `RealClock`. No env vars set —
        /// scan-only scenarios (B0/S2/S3/S6/S7) don't touch the seed
        /// derivation chain.
        pub fn new() -> Self {
            Self {
                config: Config::default(),
                seeds: SeedHandle::new(&Seeds::default()),
                redaction: RedactionDenylist::for_test(),
                clock: RealClock,
            }
        }

        /// Build a no-sampler ctx — `peak_rss_bytes` / `peak_cpu_pct`
        /// stay `None` post-run, matching the pre-3j test behaviour.
        pub fn ctx(&self) -> ScenarioCtx<'_> {
            ScenarioCtx {
                config: &self.config,
                seeds: &self.seeds,
                redaction: &self.redaction,
                clock: &self.clock,
                recipients: RecipientStrategy::SelfAddress(SeedRole::New),
                sampler_factory: None,
            }
        }

        /// Build a ctx wired to the supplied sampler factory — used by
        /// the per-scenario peak-population tests.
        pub fn ctx_with_sampler<'a>(&'a self, factory: &'a dyn SamplerFactory) -> ScenarioCtx<'a> {
            ScenarioCtx {
                config: &self.config,
                seeds: &self.seeds,
                redaction: &self.redaction,
                clock: &self.clock,
                recipients: RecipientStrategy::SelfAddress(SeedRole::New),
                sampler_factory: Some(factory),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Seeds;
    use crate::gen_seed;
    use crate::modes::TxRecord;

    /// Build an `S0Outcome` fixture. `update_scenario_input`'s S0 arm
    /// ignores the contents (it derives the birthday from the injected
    /// timestamp), so field values only need to be well-formed.
    fn sample_s0_outcome() -> S0Outcome {
        S0Outcome {
            pre_balance: 2_000_000_000,
            post_balance: 1_999_999_800,
            balance_delta: -200,
            pre_utxo_count: 1,
            post_utxo_count: 2,
            utxo_delta: 1,
            t_construct_ms: Some(50),
            t_broadcast_ms: 200,
            t_confirm_ms: Some(30_000),
            tx_record: TxRecord {
                txid: "deadbeef".to_string(),
                t_total_ms: 250,
                t_broadcast_ms: 200,
                t_confirm_ms: Some(30_000),
                status: "success".to_string(),
                error_string: None,
                fee_microtari: 200,
            },
            peak_rss_bytes: None,
            peak_cpu_pct: None,
        }
    }

    #[test]
    fn s0_funding_birthday_converts_days_since_2022_with_margin() {
        // 1782950400 == epoch + 1643 days (the live wallet-log value). With
        // the 1-day safety margin the encoded birthday is 1642.
        assert_eq!(s0_funding_birthday(1_782_950_400), 1642);
        // Exactly 100 days after the epoch -> 100 days, minus margin -> 99.
        assert_eq!(s0_funding_birthday(1_640_995_200 + 100 * 86_400), 99);
    }

    #[test]
    fn s0_funding_birthday_floors_at_zero_for_epoch_and_earlier() {
        // At the epoch: 0 days, saturating_sub(1) stays 0.
        assert_eq!(s0_funding_birthday(1_640_995_200), 0);
        // Before the epoch: clamped to 0, not a huge wraparound.
        assert_eq!(s0_funding_birthday(1_640_995_200 - 5), 0);
        assert_eq!(s0_funding_birthday(0), 0);
    }

    #[test]
    fn s0_funding_birthday_clamps_near_u16_max() {
        // Far-future timestamp must saturate near u16::MAX, never overflow
        // or wrap. The day-count clamps to u16::MAX, then the 1-day margin
        // leaves u16::MAX - 1.
        assert_eq!(s0_funding_birthday(i64::MAX), u16::MAX - 1);
    }

    #[test]
    fn update_scenario_input_s0_does_not_touch_birthday_slots() {
        // Regression (inverted from the original): the birthday slots are
        // populated at ScenarioInput construction in main.rs, from wall
        // clock alone. The S0 arm must NOT own them — when it did, an S0
        // error meant update_scenario_input never ran and S3/S7 bailed
        // with "requires ScenarioInput::h_birth_s3" (the maintainer's
        // observed s0+s3+s7 err trio). Slots set before S0 stay intact;
        // slots unset stay unset.
        let mut input = ScenarioInput {
            h_birth_s3: Some(1642),
            s7_h_birth: Some(1642),
            ..ScenarioInput::default()
        };
        let outcome = ScenarioOutcome::S0(sample_s0_outcome());
        update_scenario_input(&outcome, &mut input, 1_782_950_400);
        assert_eq!(input.h_birth_s3, Some(1642), "pre-set S3 slot untouched");
        assert_eq!(input.s7_h_birth, Some(1642), "pre-set S7 slot untouched");

        let mut empty = ScenarioInput::default();
        update_scenario_input(&outcome, &mut empty, 1_782_950_400);
        assert_eq!(empty.h_birth_s3, None, "S0 arm no longer populates");
        assert_eq!(empty.s7_h_birth, None);
    }

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

    /// Mutate env via `set_var` / `remove_var`. Both calls are `unsafe` on
    /// Rust 1.84+; allow `unused_unsafe` for older toolchains. Mirrors the
    /// helper in `crate::seed::tests`.
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

    /// Per-test `Seeds` with unique env-var names so parallel test runs
    /// don't race on the same vars.
    fn unique_seeds(suffix: &str) -> Seeds {
        Seeds {
            old: format!("WALLET_BENCHMARKS_TEST_RECIPIENT_OLD_{suffix}"),
            new: format!("WALLET_BENCHMARKS_TEST_RECIPIENT_NEW_{suffix}"),
            payment_processor: format!("WALLET_BENCHMARKS_TEST_RECIPIENT_PP_{suffix}"),
            wallet_password: format!("WALLET_BENCHMARKS_TEST_RECIPIENT_PW_{suffix}"),
        }
    }

    #[test]
    fn self_address_old_resolves_to_old_wallet_address() {
        let seeds_cfg = unique_seeds("SELF_OLD");
        let m = gen_seed().expect("mnemonic");
        set_env(&seeds_cfg.old, &m);
        let seeds = SeedHandle::new(&seeds_cfg);

        let strat = RecipientStrategy::SelfAddress(SeedRole::Old);
        let resolved = strat
            .resolve_for(&seeds, 0)
            .expect("resolve SelfAddress(Old)");
        let oracle = seeds.address_for(SeedRole::Old).expect("oracle address");

        unset_env(&seeds_cfg.old);
        assert_eq!(
            resolved.to_base58(),
            oracle.to_base58(),
            "SelfAddress(Old) must agree with SeedHandle::address_for(Old)",
        );
    }

    #[test]
    fn self_address_new_resolves_to_new_wallet_address() {
        let seeds_cfg = unique_seeds("SELF_NEW");
        let m = gen_seed().expect("mnemonic");
        set_env(&seeds_cfg.new, &m);
        let seeds = SeedHandle::new(&seeds_cfg);

        let strat = RecipientStrategy::SelfAddress(SeedRole::New);
        let resolved = strat
            .resolve_for(&seeds, 0)
            .expect("resolve SelfAddress(New)");
        let oracle = seeds.address_for(SeedRole::New).expect("oracle address");

        unset_env(&seeds_cfg.new);
        assert_eq!(
            resolved.to_base58(),
            oracle.to_base58(),
            "SelfAddress(New) must agree with SeedHandle::address_for(New)",
        );
    }

    #[test]
    fn self_address_pp_resolves_to_payment_processor_address() {
        let seeds_cfg = unique_seeds("SELF_PP");
        let m = gen_seed().expect("mnemonic");
        set_env(&seeds_cfg.payment_processor, &m);
        let seeds = SeedHandle::new(&seeds_cfg);

        let strat = RecipientStrategy::SelfAddress(SeedRole::Pp);
        let resolved = strat
            .resolve_for(&seeds, 0)
            .expect("resolve SelfAddress(Pp)");
        let oracle = seeds.address_for(SeedRole::Pp).expect("oracle address");

        unset_env(&seeds_cfg.payment_processor);
        assert_eq!(
            resolved.to_base58(),
            oracle.to_base58(),
            "SelfAddress(Pp) must agree with SeedHandle::address_for(Pp)",
        );
    }

    #[test]
    fn failure_streak_aborts_at_threshold() {
        let mut t = FailureStreakTracker::new(3);
        assert!(!t.observe_failure("Funds are pending"));
        assert!(!t.observe_failure("Funds are pending"));
        assert!(
            t.observe_failure("Funds are pending"),
            "third identical failure aborts"
        );
        let reason = t.abort_reason();
        assert!(
            reason.contains("3 contiguous identical failures")
                && reason.contains("Funds are pending")
                && reason.contains("fail_fast_identical_failure_threshold"),
            "reason must be self-describing: {reason}",
        );
    }

    #[test]
    fn failure_streak_resets_on_success() {
        let mut t = FailureStreakTracker::new(2);
        assert!(!t.observe_failure("x"));
        t.observe_success();
        assert!(!t.observe_failure("x"), "success resets the streak");
        assert!(t.observe_failure("x"));
    }

    #[test]
    fn failure_streak_resets_on_different_error() {
        let mut t = FailureStreakTracker::new(2);
        assert!(!t.observe_failure("error A at height 100"));
        assert!(
            !t.observe_failure("error A at height 101"),
            "identity is exact string match; a varying message is a new streak",
        );
        assert!(t.observe_failure("error A at height 101"));
    }

    #[test]
    fn failure_streak_disabled_at_zero() {
        let mut t = FailureStreakTracker::new(0);
        for _ in 0..1000 {
            assert!(!t.observe_failure("same"), "0 disables the policy");
        }
    }
}
