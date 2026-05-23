//! S6 full rescan after S5 — wipe data dir, set birthday=0, scan from
//! genesis, and verify `outputs_found` against the caller-supplied expected
//! count.
//!
//! Per `analysis/DESIGN.md §Scenario state machine §S6` and
//! `analysis/RESULT_PROFILE_SCHEMA.md §S6 scenario`: S6 has the **identical
//! Outcome shape as S2** (full-history rescan after the prior funding
//! scenario). The semantic difference vs S2 is the preceding state — S2
//! runs after S1's UTXO multiplication; S6 runs after S5's 100-recipient
//! send-volume — but the per-cell payload structure is the same.
//!
//! Preconditions: S5 has completed against the same mode. The caller passes
//! the expected count explicitly via `ScenarioInput::s6_expected_outputs`
//! (S1's net outputs plus S5's net outputs, computed by step 3i.2 / 3k).
//! S6 does not introspect any prior outcome directly.
//!
//! Steps mirror S2 verbatim:
//!
//! 1. `mode.wipe_and_reimport(0)` — birthday=0, full-history rescan.
//! 2. Time the scan via `mode.scan_from_birthday(0)`. Wall-clock timer
//!    spans the call only.
//! 3. `outputs_found = scan.outputs_found`, `blocks_scanned = h_tip_end -
//!    h_tip_start`.
//! 4. `outputs_found_matches_expected = (scan.outputs_found ==
//!    expected_outputs)` recorded as a `bool`. Mismatch is **not** an `Err`
//!    — the cell-envelope writer (step 3k) decides what to do with `false`.
//!
//! Delta fields (`t_scan_s6_minus_s2_ms`, `t_scan_s6_over_b0_ratio`, etc.
//! per schema §S6) are computed by 3k's result-profile writer, NOT by S6
//! itself. S6 emits the raw values that feed those deltas.
//!
//! Failure-halt: no — a failing scan records `status = "failure"` at the
//! cell envelope and the run continues.

use std::time::Instant;

use crate::modes::Mode;
use crate::sampler::Pid;
use crate::scenarios::ScenarioCtx;

/// S6's per-cell payload — identical structure to
/// [`crate::scenarios::S2Outcome`]. `peak_rss_bytes` and `peak_cpu_pct`
/// are `None` until the 1 Hz metrics sampler lands (step 3j).
#[derive(Debug, Clone, PartialEq)]
pub struct S6Outcome {
    /// `t_scan_ms` per `RESULT_PROFILE_SCHEMA.md §S6` — duration of the
    /// post-wipe full-history scan only.
    pub t_scan_ms: u64,
    /// `blocks_per_sec` per `RESULT_PROFILE_SCHEMA.md §S6` —
    /// `blocks_scanned / (t_scan_ms / 1000)`. `None` when `t_scan_ms ==
    /// 0` (degenerate fake-mode case); matches the `Option<f64>` rate
    /// pattern established by B0/S2.
    pub blocks_per_sec: Option<f64>,
    /// `h_tip_start` per `RESULT_PROFILE_SCHEMA.md §S6` — tip when scan
    /// begins (sourced from `ScanOutcome::h_tip_start`).
    pub h_tip_start: u64,
    /// `h_tip_end` per `RESULT_PROFILE_SCHEMA.md §S6` — tip when scan
    /// completes (sourced from `ScanOutcome::h_tip_end`).
    pub h_tip_end: u64,
    /// `blocks_scanned` — `h_tip_end - h_tip_start`. Feeds 3k's
    /// `t_scan_s6_minus_s2_ms` and `t_scan_s6_over_b0_ratio` delta
    /// computations.
    pub blocks_scanned: u64,
    /// `outputs_found` per `RESULT_PROFILE_SCHEMA.md §S6` — wallet
    /// outputs rediscovered by the rescan; expected to equal
    /// `expected_outputs` (AC-15 mirror for post-S5 state).
    pub outputs_found: u64,
    /// Expected outputs: S1's net plus S5's net. Plumbed in via
    /// [`crate::scenarios::ScenarioInput::s6_expected_outputs`].
    pub expected_outputs: u64,
    /// `outputs_found == expected_outputs`. Recorded as a `bool` — a
    /// mismatch is **not** treated as scenario failure here; the
    /// cell-envelope writer decides what to do with `false`.
    pub outputs_found_matches_expected: bool,
    /// `peak_rss_bytes` per `RESULT_PROFILE_SCHEMA.md §S6` — `None` until
    /// the 1 Hz sampler lands in step 3j.
    pub peak_rss_bytes: Option<u64>,
    /// `peak_cpu_pct` per `RESULT_PROFILE_SCHEMA.md §S6` — `None` until
    /// the 1 Hz sampler lands in step 3j.
    pub peak_cpu_pct: Option<f64>,
}

/// Run S6 against the given mode.
///
/// `expected_outputs` is the AC-15-mirror verification target — supplied
/// by the caller (step 3i.2 dispatch threads it via
/// [`crate::scenarios::ScenarioInput::s6_expected_outputs`]). S6 does not
/// introspect any prior outcome directly.
///
/// Returns `Err` on `wipe_and_reimport` or `scan_from_birthday` failure;
/// the caller maps that to a `status = "failure"` cell.
/// `outputs_found_matches_expected = false` is **not** an `Err` — it
/// rides on the outcome.
pub(super) async fn run(
    ctx: &ScenarioCtx<'_>,
    mode: &mut dyn Mode,
    expected_outputs: u64,
) -> anyhow::Result<S6Outcome> {
    let sampler = ctx.sampler_factory.map(|f| {
        f.start(
            Pid(mode.target_pid_for_sampling()),
            ctx.config.sampler_interval_ms,
        )
    });

    mode.wipe_and_reimport(0).await?;

    let scan_start = Instant::now();
    let scan = mode.scan_from_birthday(0).await?;
    let t_scan_ms = u64::try_from(scan_start.elapsed().as_millis()).unwrap_or(u64::MAX);

    let blocks_scanned = scan.h_tip_end.saturating_sub(scan.h_tip_start);

    let blocks_per_sec = if t_scan_ms == 0 {
        None
    } else {
        Some((blocks_scanned as f64) / ((t_scan_ms as f64) / 1000.0))
    };

    let outputs_found_matches_expected = scan.outputs_found == expected_outputs;

    let (peak_rss_bytes, peak_cpu_pct) = match sampler {
        Some(s) => s.stop().await,
        None => (None, None),
    };

    Ok(S6Outcome {
        t_scan_ms,
        blocks_per_sec,
        h_tip_start: scan.h_tip_start,
        h_tip_end: scan.h_tip_end,
        blocks_scanned,
        outputs_found: scan.outputs_found,
        expected_outputs,
        outputs_found_matches_expected,
        peak_rss_bytes,
        peak_cpu_pct,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::{test_support::FakeMode, ScanOutcome};
    use crate::sampler::FakeSamplerFactory;
    use crate::scenarios::test_support::TestCtxOwner;

    fn canned_scan(outputs_found: u64) -> ScanOutcome {
        ScanOutcome {
            t_scan_ms: 0,
            h_tip_start: 100,
            h_tip_end: 1100,
            outputs_found,
            utxo_count: outputs_found,
            balance_microtari: 0,
        }
    }

    #[tokio::test]
    async fn s6_outputs_match_expected_when_counts_agree() {
        let mut fake = FakeMode::new();
        fake.canned_scan = Some(canned_scan(256));

        let owner = TestCtxOwner::new();
        let outcome = run(&owner.ctx(), &mut fake, 256)
            .await
            .expect("S6 runs with canned scan");

        assert_eq!(outcome.outputs_found, 256);
        assert_eq!(outcome.expected_outputs, 256);
        assert!(
            outcome.outputs_found_matches_expected,
            "outputs_found == expected_outputs => true",
        );
        assert_eq!(outcome.h_tip_start, 100);
        assert_eq!(outcome.h_tip_end, 1100);
        assert_eq!(outcome.blocks_scanned, 1000, "h_tip_end - h_tip_start");
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
            vec!["wipe_and_reimport", "scan_from_birthday"],
            "S6 calls wipe_and_reimport then scan_from_birthday",
        );
    }

    #[tokio::test]
    async fn s6_outputs_mismatch_records_false_without_bailing() {
        let mut fake = FakeMode::new();
        fake.canned_scan = Some(canned_scan(256));

        let owner = TestCtxOwner::new();
        let outcome = run(&owner.ctx(), &mut fake, 255)
            .await
            .expect("mismatch must NOT be an Err");

        assert_eq!(outcome.outputs_found, 256);
        assert_eq!(outcome.expected_outputs, 255);
        assert!(
            !outcome.outputs_found_matches_expected,
            "256 != 255 => false",
        );
    }

    #[tokio::test]
    async fn s6_propagates_scan_failure() {
        let mut fake = FakeMode::new();
        fake.fail_with = Some("simulated S6 scan failure".to_string());

        let owner = TestCtxOwner::new();
        let err = run(&owner.ctx(), &mut fake, 0)
            .await
            .expect_err("scan failure must bubble up");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("simulated S6 scan failure"),
            "error message must carry the underlying error: {msg}",
        );
    }

    /// 3j wiring: peak_rss_bytes populated from FakeSamplerFactory.
    #[tokio::test]
    async fn s6_populates_peaks_from_sampler() {
        let mut fake = FakeMode::new();
        fake.canned_scan = Some(canned_scan(256));
        let factory = FakeSamplerFactory::new(vec![32768], vec![]);
        let mut owner = TestCtxOwner::new();
        owner.config.sampler_interval_ms = 5;
        let outcome = run(&owner.ctx_with_sampler(&factory), &mut fake, 256)
            .await
            .expect("S6 runs");
        assert_eq!(outcome.peak_rss_bytes, Some(32768));
        assert!(outcome.peak_cpu_pct.is_none());
    }
}
