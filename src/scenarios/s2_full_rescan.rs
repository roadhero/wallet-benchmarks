//! S2 full rescan — wipe data dir, set birthday=0, scan from genesis, and
//! verify `outputs_found` against the caller-supplied expected count.
//!
//! Per `analysis/DESIGN.md §Scenario state machine §S2`,
//! `analysis/RESULT_PROFILE_SCHEMA.md §S2 scenario (AC-15, AC-24, AC-34)`,
//! and AC-15/AC-22/AC-34:
//!
//! Preconditions: S1 has completed against the same mode — the chain has
//! the 512 outputs the rescan is expected to rediscover. The caller passes
//! the expected count explicitly via `ScenarioInput`; S2 does not introspect
//! the prior `S1Outcome`.
//!
//! Steps:
//!
//! 1. `h_tip_start` is sourced from `ScanOutcome::h_tip_start` per the same
//!    B0 pattern (the harness does not expose a pre-scan tip-only query;
//!    see `analysis/API_DRIFT.md`).
//! 2. `mode.wipe_and_reimport(0)` — birthday=0, full-history rescan (AC-24).
//! 3. Time the scan via `mode.scan_from_birthday(0)`. Wall-clock timer
//!    spans the call only, matching B0.
//! 4. `outputs_found = scan.outputs_found`, `blocks_scanned = h_tip_end -
//!    h_tip_start` (the difference is the materially-covered range; B0
//!    does the same implicit calculation but does not record it).
//! 5. **Verification per AC-15:**
//!    `outputs_found_matches_expected = (scan.outputs_found ==
//!    expected_outputs)` recorded as a `bool`. Mismatch is **not** an `Err`
//!    — the cell-envelope writer (step 3i.2 / 3k) decides what to do with
//!    a `false`. S2 records the raw observed `outputs_found` and
//!    `expected_outputs` alongside the boolean.
//!
//! Failure-halt: no — a failing scan records `status = "failure"` at the
//! cell envelope and S3 can still run.
//!
//! Cell-level universal counters (`RESULT_PROFILE_SCHEMA.md` lines 112-116)
//! live on the cell envelope at the result-profile writer level — not on
//! [`S2Outcome`]. S2 is a scan-only scenario whose counters are all 0
//! (no per-tx submissions), so the schema's §S2 table (lines 162-176)
//! omits them from the per-scenario payload. Mirrors B0's shape.

use std::time::Instant;

use crate::modes::Mode;

/// S2's per-cell payload, matching `RESULT_PROFILE_SCHEMA.md §S2`.
///
/// `peak_rss_bytes` and `peak_cpu_pct` are `None` until the 1 Hz metrics
/// sampler lands (step 3j); the schema permits `null` for both
/// (`u64 | null` / `f64 | null`).
#[derive(Debug, Clone, PartialEq)]
pub struct S2Outcome {
    /// `t_scan_ms` per `RESULT_PROFILE_SCHEMA.md §S2` — duration of the
    /// post-wipe full-history scan only.
    pub t_scan_ms: u64,
    /// `blocks_per_sec` per `RESULT_PROFILE_SCHEMA.md §S2` —
    /// `(h_tip_end - h_tip_start) / (t_scan_ms / 1000)`. `None` when
    /// `t_scan_ms == 0` (degenerate fake-mode case in tests); matches B0's
    /// `Option<f64>` pattern across rate fields.
    pub blocks_per_sec: Option<f64>,
    /// `h_tip_start` per `RESULT_PROFILE_SCHEMA.md §S2` — tip when scan
    /// begins (sourced from `ScanOutcome::h_tip_start`).
    pub h_tip_start: u64,
    /// `h_tip_end` per `RESULT_PROFILE_SCHEMA.md §S2` — tip when scan
    /// completes (sourced from `ScanOutcome::h_tip_end`).
    pub h_tip_end: u64,
    /// `blocks_scanned` — `h_tip_end - h_tip_start`. The schema's §S2 table
    /// does not list this field explicitly (S3 does, line 185), but the
    /// cell-envelope writer (step 3i.2 / 3k) computes the S6-vs-B0 /
    /// S2-vs-B0 deltas from this value; recording it here keeps the
    /// computation single-sourced.
    pub blocks_scanned: u64,
    /// `outputs_found` per `RESULT_PROFILE_SCHEMA.md §S2` — wallet
    /// outputs rediscovered by the rescan; expected to equal
    /// `expected_outputs` (AC-15).
    pub outputs_found: u64,
    /// Expected outputs from the prior S1 funding/multiplication. Plumbed
    /// in via [`crate::scenarios::ScenarioInput::expected_outputs_s2`];
    /// AC-15 asserts `outputs_found == expected_outputs`.
    pub expected_outputs: u64,
    /// `outputs_found == expected_outputs`. Recorded as a `bool` per
    /// AC-15 — a mismatch is **not** treated as scenario failure here;
    /// the cell-envelope writer decides what to do with `false`.
    pub outputs_found_matches_expected: bool,
    /// `peak_rss_bytes` per `RESULT_PROFILE_SCHEMA.md §S2` — `None` until
    /// the 1 Hz sampler lands in step 3j.
    pub peak_rss_bytes: Option<u64>,
    /// `peak_cpu_pct` per `RESULT_PROFILE_SCHEMA.md §S2` — `None` until
    /// the 1 Hz sampler lands in step 3j.
    pub peak_cpu_pct: Option<f64>,
}

/// Run S2 against the given mode.
///
/// `expected_outputs` is the AC-15 verification target — supplied by the
/// caller (step 3i.2 dispatch threads it via
/// [`crate::scenarios::ScenarioInput::expected_outputs_s2`]). S2 does not
/// introspect any prior `S1Outcome` directly; the run loop owns the
/// per-scenario data plumbing.
///
/// Returns `Err` on `wipe_and_reimport` or `scan_from_birthday` failure;
/// the caller maps that to a `status = "failure"` cell. AC-15
/// `outputs_found_matches_expected = false` is **not** an `Err` — it
/// rides on the outcome and the cell-envelope writer decides.
pub(super) async fn run(mode: &mut dyn Mode, expected_outputs: u64) -> anyhow::Result<S2Outcome> {
    // birthday=0 is full-history per AC-24. wipe_and_reimport teardown is
    // a Mode-trait concern (see `analysis/DESIGN.md §6 Birthday rewrite`).
    mode.wipe_and_reimport(0).await?;

    let scan_start = Instant::now();
    let scan = mode.scan_from_birthday(0).await?;
    let t_scan_ms = u64::try_from(scan_start.elapsed().as_millis()).unwrap_or(u64::MAX);

    // `blocks_scanned = h_tip_end - h_tip_start` per the schema. Saturating
    // sub guards against the degenerate fake-mode case where canned values
    // could be inverted; production scans always have `h_tip_end >=
    // h_tip_start`.
    let blocks_scanned = scan.h_tip_end.saturating_sub(scan.h_tip_start);

    // `blocks_per_sec` per schema, computed against the materially-covered
    // range (`blocks_scanned`). `None` when `t_scan_ms == 0` so downstream
    // tests and the result-profile writer don't have to special-case `NAN`.
    let blocks_per_sec = if t_scan_ms == 0 {
        None
    } else {
        Some((blocks_scanned as f64) / ((t_scan_ms as f64) / 1000.0))
    };

    let outputs_found_matches_expected = scan.outputs_found == expected_outputs;

    Ok(S2Outcome {
        t_scan_ms,
        blocks_per_sec,
        h_tip_start: scan.h_tip_start,
        h_tip_end: scan.h_tip_end,
        blocks_scanned,
        outputs_found: scan.outputs_found,
        expected_outputs,
        outputs_found_matches_expected,
        peak_rss_bytes: None,
        peak_cpu_pct: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::{test_support::FakeMode, ScanOutcome};

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
    async fn s2_outputs_match_expected_when_counts_agree() {
        let mut fake = FakeMode::new();
        fake.canned_scan = Some(canned_scan(128));

        let outcome = run(&mut fake, 128).await.expect("S2 runs with canned scan");

        assert_eq!(outcome.outputs_found, 128);
        assert_eq!(outcome.expected_outputs, 128);
        assert!(
            outcome.outputs_found_matches_expected,
            "AC-15: outputs_found == expected_outputs => true",
        );
        assert_eq!(outcome.h_tip_start, 100);
        assert_eq!(outcome.h_tip_end, 1100);
        assert_eq!(outcome.blocks_scanned, 1000, "h_tip_end - h_tip_start");
        assert!(outcome.peak_rss_bytes.is_none(), "sampler lands in 3j");
        assert!(outcome.peak_cpu_pct.is_none(), "sampler lands in 3j");

        // Mode call order: wipe_and_reimport must precede scan_from_birthday
        // (AC-24 / AC-34); the wipe is the precondition for the rescan.
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec!["wipe_and_reimport", "scan_from_birthday"],
            "S2 calls wipe_and_reimport then scan_from_birthday",
        );
    }

    #[tokio::test]
    async fn s2_outputs_mismatch_records_false_without_bailing() {
        let mut fake = FakeMode::new();
        // Observed=128, expected=127 — AC-15 mismatch. Must record
        // matches_expected=false and Ok(outcome); MUST NOT bail.
        fake.canned_scan = Some(canned_scan(128));

        let outcome = run(&mut fake, 127)
            .await
            .expect("AC-15 mismatch must NOT be an Err");

        assert_eq!(outcome.outputs_found, 128);
        assert_eq!(outcome.expected_outputs, 127);
        assert!(
            !outcome.outputs_found_matches_expected,
            "AC-15: 128 != 127 => false",
        );
    }

    #[tokio::test]
    async fn s2_propagates_scan_failure() {
        let mut fake = FakeMode::new();
        fake.fail_with = Some("simulated S2 scan failure".to_string());

        let err = run(&mut fake, 0)
            .await
            .expect_err("scan failure must bubble up");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("simulated S2 scan failure"),
            "error message must carry the underlying error: {msg}",
        );
    }
}
