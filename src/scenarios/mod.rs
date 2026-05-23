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

pub use b0_baseline::B0Outcome;
pub use s0_warmup::S0Outcome;
pub use s1_volume::{RoundOutcome, S1Outcome};
pub use s2_full_rescan::S2Outcome;

use tari_common_types::tari_address::TariAddress;

use crate::clock::Clock;
use crate::config::Config;
use crate::modes::Mode;
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
    /// expected to rediscover. `None` for non-S2 dispatches; the S2 arm
    /// bails with a clear error if `None`.
    pub expected_outputs_s2: Option<u64>,
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
    // Subsequent variants land per `DESIGN.md §swe-impl execution order`:
    //   …      → S3..S7
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
        ScenarioId::B0 => b0_baseline::run(mode).await.map(ScenarioOutcome::B0),
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
            s2_full_rescan::run(mode, expected)
                .await
                .map(ScenarioOutcome::S2)
        }
        ScenarioId::S3 | ScenarioId::S4 | ScenarioId::S5 | ScenarioId::S6 | ScenarioId::S7 => {
            anyhow::bail!(
                "scenario {id} not yet implemented (step 3i.1.d/e lands S2; \
             subsequent scenarios follow in DESIGN.md swe-impl execution order)"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Seeds;
    use crate::gen_seed;

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
}
