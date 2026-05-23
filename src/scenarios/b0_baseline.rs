//! B0 baseline — from-genesis archival scan against an unfunded wallet's
//! birthday.
//!
//! Per `analysis/DESIGN.md §Scenario state machine §B0` and
//! `analysis/RESULT_PROFILE_SCHEMA.md §B0 scenario (AC-10)`:
//!
//! Preconditions: wallet imported, birthday set to 0, data dir wiped.
//! (Birthday rewrite + wipe happen inside `Mode::wipe_and_reimport(0)`,
//! owned by the modes layer per `src/modes/mod.rs::Mode` and `DESIGN.md §6
//! Birthday rewrite`. B0's scenario-level wipe+import call is wired in
//! step 3i.2 when the full scenario runner lands; this step lands the
//! measurement core only.)
//!
//! Steps (this step lands the in-process measurement core only; the 1 Hz
//! metrics sampler from `DESIGN.md §B0 step (2)` lands in step 3j when the
//! sampler module is introduced — `peak_rss_bytes` and `peak_cpu_pct` carry
//! `None` until then, matching the schema's `u64 | null` / `f64 | null`):
//!
//! 1. Invoke `mode.scan_from_birthday(0)` and measure wall time.
//! 2. Read `outputs_found` / `utxo_count` / `balance` from the returned
//!    `ScanOutcome` (expected `0`/`0`/`0` against an unfunded wallet).
//!
//! Verification: AC-10. Result fields: `t_scan_ms`, `blocks_per_sec`,
//! `h_tip_start`, `h_tip_end`, `peak_rss_bytes`, `peak_cpu_pct`,
//! `utxo_count_verified`, `balance_verified_microtari`, `outputs_found`.
//!
//! Failure-halt: no — a failing scan records `status = "failure"` and we
//! move to S0. This fn returns `Err` on scan failure; the caller
//! (`scenarios::run_scenario` / the run loop in step 3i.2) maps that to
//! the cell envelope's `status` field.

use std::time::Instant;

use crate::modes::Mode;
use crate::sampler::Pid;
use crate::scenarios::ScenarioCtx;

/// B0's per-cell payload, matching `RESULT_PROFILE_SCHEMA.md §B0 scenario`.
///
/// `peak_rss_bytes` and `peak_cpu_pct` are `None` until the 1 Hz metrics
/// sampler lands (step 3j); the schema permits `null` for both
/// (`u64 | null` / `f64 | null`).
#[derive(Debug, Clone, PartialEq)]
pub struct B0Outcome {
    /// `t_scan_ms` per `RESULT_PROFILE_SCHEMA.md §B0` — duration of the
    /// from-genesis scan only.
    pub t_scan_ms: u64,
    /// `blocks_per_sec` per `RESULT_PROFILE_SCHEMA.md §B0` —
    /// `(tip_height_end - 0) / (t_scan_ms / 1000)`. `None` when
    /// `t_scan_ms == 0` (degenerate fake-mode case in tests).
    pub blocks_per_sec: Option<f64>,
    /// `h_tip_start` per `RESULT_PROFILE_SCHEMA.md §B0` — tip when scan
    /// begins (sourced from `ScanOutcome::h_tip_start`).
    pub h_tip_start: u64,
    /// `h_tip_end` per `RESULT_PROFILE_SCHEMA.md §B0` — tip when scan
    /// completes (sourced from `ScanOutcome::h_tip_end`).
    pub h_tip_end: u64,
    /// `peak_rss_bytes` per `RESULT_PROFILE_SCHEMA.md §B0` — `None` until
    /// the 1 Hz sampler lands in step 3j.
    pub peak_rss_bytes: Option<u64>,
    /// `peak_cpu_pct` per `RESULT_PROFILE_SCHEMA.md §B0` — `None` until
    /// the 1 Hz sampler lands in step 3j.
    pub peak_cpu_pct: Option<f64>,
    /// `utxo_count_verified` per `RESULT_PROFILE_SCHEMA.md §B0` — expected
    /// `0` against an unfunded wallet's birthday-0 scan.
    pub utxo_count_verified: u64,
    /// `balance_verified_microtari` per `RESULT_PROFILE_SCHEMA.md §B0` —
    /// expected `0`.
    pub balance_verified_microtari: u64,
    /// `outputs_found` per `RESULT_PROFILE_SCHEMA.md §B0` — expected `0`.
    pub outputs_found: u64,
}

/// Run B0 against the given mode.
///
/// Wraps the scan in a [`crate::sampler::ResourceSampler`] (if `ctx.sampler_factory`
/// is `Some`) to populate `peak_rss_bytes` / `peak_cpu_pct`. When `None`,
/// peaks stay `None` per the schema's `u64 | null` / `f64 | null` permission.
///
/// Returns `Err` on scan failure; the caller (scenarios::run_scenario / the
/// run loop in step 3i.2) maps that to a `status = "failure"` cell.
pub(super) async fn run(ctx: &ScenarioCtx<'_>, mode: &mut dyn Mode) -> anyhow::Result<B0Outcome> {
    let sampler = ctx.sampler_factory.map(|f| {
        f.start(
            Pid(mode.target_pid_for_sampling()),
            ctx.config.sampler_interval_ms,
        )
    });

    let scan_start = Instant::now();
    let scan = mode.scan_from_birthday(0).await?;
    let t_scan_ms = u64::try_from(scan_start.elapsed().as_millis()).unwrap_or(u64::MAX);

    // `blocks_per_sec` per schema: `(h_tip_end - 0) / (t_scan_ms / 1000)`.
    // `None` when `t_scan_ms == 0` so downstream tests and the result-profile
    // writer don't have to special-case `NAN` (which compares `!= NAN`).
    // Establishes the pattern for all subsequent rate/throughput fields on
    // S0..S7 Outcomes.
    let blocks_per_sec = if t_scan_ms == 0 {
        None
    } else {
        Some((scan.h_tip_end as f64) / ((t_scan_ms as f64) / 1000.0))
    };

    let (peak_rss_bytes, peak_cpu_pct) = match sampler {
        Some(s) => s.stop().await,
        None => (None, None),
    };

    Ok(B0Outcome {
        t_scan_ms,
        blocks_per_sec,
        h_tip_start: scan.h_tip_start,
        h_tip_end: scan.h_tip_end,
        peak_rss_bytes,
        peak_cpu_pct,
        utxo_count_verified: scan.utxo_count,
        balance_verified_microtari: scan.balance_microtari,
        outputs_found: scan.outputs_found,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::{test_support::FakeMode, ScanOutcome};
    use crate::sampler::FakeSamplerFactory;
    use crate::scenarios::test_support::TestCtxOwner;

    #[tokio::test]
    async fn b0_reads_canned_scan_outcome() {
        let mut fake = FakeMode::new();
        fake.canned_scan = Some(ScanOutcome {
            t_scan_ms: 0,
            h_tip_start: 100,
            h_tip_end: 200,
            outputs_found: 0,
            utxo_count: 0,
            balance_microtari: 0,
        });

        let owner = TestCtxOwner::new();
        let outcome = run(&owner.ctx(), &mut fake)
            .await
            .expect("B0 runs with canned scan");

        assert_eq!(outcome.outputs_found, 0, "AC-10 expects outputs_found == 0");
        assert_eq!(outcome.utxo_count_verified, 0, "AC-10 expects 0 UTXOs");
        assert_eq!(
            outcome.balance_verified_microtari, 0,
            "AC-10 expects 0 balance",
        );
        assert_eq!(outcome.h_tip_start, 100);
        assert_eq!(outcome.h_tip_end, 200);
        // `blocks_per_sec` is `None` only when `t_scan_ms == 0`; the in-fn
        // timer reads `Instant::now()` twice across the `mode.scan_from_birthday`
        // await point, so the elapsed time is non-zero in practice. Assert on
        // the discriminant rather than the value so the test stays portable
        // across CI runners with different clock granularities.
        match outcome.blocks_per_sec {
            Some(v) => assert!(
                v.is_finite() && v >= 0.0,
                "blocks_per_sec must be a finite non-negative rate: {v}",
            ),
            None => {
                // Permitted in the degenerate `t_scan_ms == 0` case.
            }
        }
        assert!(
            outcome.peak_rss_bytes.is_none(),
            "sampler_factory: None → peak_rss_bytes is None",
        );
        assert!(
            outcome.peak_cpu_pct.is_none(),
            "sampler_factory: None → peak_cpu_pct is None",
        );

        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec!["scan_from_birthday"],
            "B0 calls scan_from_birthday exactly once",
        );
    }

    #[tokio::test]
    async fn b0_propagates_scan_failure() {
        let mut fake = FakeMode::new();
        fake.fail_with = Some("simulated scan failure".to_string());

        let owner = TestCtxOwner::new();
        let err = run(&owner.ctx(), &mut fake)
            .await
            .expect_err("scan failure must bubble up");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("simulated scan failure"),
            "error message must carry the underlying scan error: {msg}",
        );
    }

    /// 3j wiring: with a `Some(&FakeSamplerFactory)` carrying canned
    /// values, the B0Outcome's peak fields are populated from the
    /// sampler rather than left at None. Uses a 5ms interval against
    /// a 4KB canned RSS so the sampler ticks at least once before
    /// scan_from_birthday returns.
    #[tokio::test]
    async fn b0_populates_peaks_from_sampler() {
        let mut fake = FakeMode::new();
        fake.canned_scan = Some(ScanOutcome {
            t_scan_ms: 0,
            h_tip_start: 100,
            h_tip_end: 200,
            outputs_found: 0,
            utxo_count: 0,
            balance_microtari: 0,
        });
        let factory = FakeSamplerFactory::new(vec![4096], vec![]);
        let mut owner = TestCtxOwner::new();
        owner.config.sampler_interval_ms = 5;
        let ctx = owner.ctx_with_sampler(&factory);
        // Sampler's first sample happens immediately on task entry; the
        // await on scan_from_birthday gives the spawn enough time to land
        // it before stop().
        let outcome = run(&ctx, &mut fake).await.expect("B0 runs");
        assert_eq!(
            outcome.peak_rss_bytes,
            Some(4096),
            "FakeSamplerFactory canned RSS must flow into peak_rss_bytes",
        );
        // No CPU values pushed → still None (sampler needs ≥2 samples).
        assert!(outcome.peak_cpu_pct.is_none());
    }
}
