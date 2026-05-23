//! S3 birthday rescan — wipe data dir, set birthday=`h_birth`, scan from
//! the funding-tx height, and verify `outputs_found` against the
//! caller-supplied expected count.
//!
//! Per `analysis/DESIGN.md §Scenario state machine §S3`,
//! `analysis/RESULT_PROFILE_SCHEMA.md §S3 scenario (AC-16)`, and AC-16 /
//! AC-23: identical shape to S2 with the birthday parameter switched to
//! the S0 funding height. Crucially this scenario scans a materially
//! smaller range than S2, but the harness records the raw `blocks_scanned`
//! and lets the cell-envelope writer (step 3i.2 / 3k) compute the
//! S3-vs-S2 comparison — the S2 result isn't available at the scenario
//! level.
//!
//! Preconditions: S2 has completed against the same mode. The caller
//! passes `h_birth` (S0's funding height, in `u16` days-since-2022-01-01
//! per `Mode::scan_from_birthday`'s signature) and `expected_outputs`
//! (the AC-16 verification target) via `ScenarioInput`; S3 does not
//! introspect any prior outcome directly.
//!
//! Steps mirror S2:
//!
//! 1. `mode.wipe_and_reimport(h_birth)` — set birthday before rescan.
//! 2. Time the scan via `mode.scan_from_birthday(h_birth)`.
//! 3. `blocks_scanned = h_tip_end - h_tip_start` from the `ScanOutcome`.
//! 4. **Verification per AC-16:** `outputs_found_matches_expected =
//!    (scan.outputs_found == expected_outputs)` recorded as a `bool`.
//!    Mismatch is **not** an `Err` — the cell-envelope writer decides
//!    what to do with `false`. `h_birth` itself rides on the outcome so
//!    the writer can assert `birthday_set == h_birth` per schema line 184.
//!
//! Failure-halt: no — a failing scan records `status = "failure"` at the
//! cell envelope and the run continues.

use std::time::Instant;

use crate::modes::Mode;

/// S3's per-cell payload, matching `RESULT_PROFILE_SCHEMA.md §S3`.
///
/// Identical shape to [`crate::scenarios::S2Outcome`] plus the `h_birth`
/// the rescan targeted. `peak_rss_bytes` and `peak_cpu_pct` are `None`
/// until the 1 Hz metrics sampler lands (step 3j).
#[derive(Debug, Clone, PartialEq)]
pub struct S3Outcome {
    /// `t_scan_ms` per `RESULT_PROFILE_SCHEMA.md §S3` — duration of the
    /// post-wipe birthday-scoped scan only.
    pub t_scan_ms: u64,
    /// `blocks_per_sec` per `RESULT_PROFILE_SCHEMA.md §S3` —
    /// `blocks_scanned / (t_scan_ms / 1000)`. `None` when `t_scan_ms ==
    /// 0` (degenerate fake-mode case); matches the `Option<f64>` rate
    /// pattern established by B0/S2.
    pub blocks_per_sec: Option<f64>,
    /// `h_birth` — the funding-tx height the rescan targeted. In `u16`
    /// per the `Mode::scan_from_birthday` signature (days since
    /// 2022-01-01 per `DESIGN.md §6 Birthday rewrite`). Rides on the
    /// outcome so the cell-envelope writer can populate the schema's
    /// `birthday_set` field (line 184) and assert it equals S0's
    /// `h_birth`.
    pub h_birth: u16,
    /// `h_tip_start` per `RESULT_PROFILE_SCHEMA.md §S3` — tip when scan
    /// begins (sourced from `ScanOutcome::h_tip_start`).
    pub h_tip_start: u64,
    /// `h_tip_end` per `RESULT_PROFILE_SCHEMA.md §S3` — tip when scan
    /// completes (sourced from `ScanOutcome::h_tip_end`).
    pub h_tip_end: u64,
    /// `blocks_scanned` per `RESULT_PROFILE_SCHEMA.md §S3` (line 185) —
    /// `h_tip_end - h_tip_start`. The comparison with S2's full-history
    /// `blocks_scanned` (intent: "covers materially fewer blocks") happens
    /// at the cell-envelope writer level (step 3k); S3 records the raw
    /// value only.
    pub blocks_scanned: u64,
    /// `outputs_found` per `RESULT_PROFILE_SCHEMA.md §S3` — wallet
    /// outputs rediscovered by the birthday-scoped rescan; expected to
    /// equal `expected_outputs` (AC-16).
    pub outputs_found: u64,
    /// Expected outputs from the prior S1 funding/multiplication.
    /// Plumbed via [`crate::scenarios::ScenarioInput::expected_outputs_s2`]
    /// since S2 and S3 share the same verification target (the chain
    /// still has the same 512 outputs after the birthday change).
    pub expected_outputs: u64,
    /// `outputs_found == expected_outputs`. Recorded as a `bool` per
    /// AC-16 — a mismatch is **not** treated as scenario failure; the
    /// cell-envelope writer decides what to do with `false`.
    pub outputs_found_matches_expected: bool,
    /// `peak_rss_bytes` per `RESULT_PROFILE_SCHEMA.md §S3` — `None` until
    /// the 1 Hz sampler lands in step 3j.
    pub peak_rss_bytes: Option<u64>,
    /// `peak_cpu_pct` per `RESULT_PROFILE_SCHEMA.md §S3` — `None` until
    /// the 1 Hz sampler lands in step 3j.
    pub peak_cpu_pct: Option<f64>,
}

/// Run S3 against the given mode.
///
/// `h_birth` is the S0 funding height (`u16` days-since-2022-01-01 per
/// `Mode::scan_from_birthday`). `expected_outputs` is the AC-16
/// verification target (same as AC-15's S2 target). Both are supplied by
/// the caller via [`crate::scenarios::ScenarioInput`]; S3 does not
/// introspect any prior outcome directly.
///
/// Returns `Err` on `wipe_and_reimport` or `scan_from_birthday` failure;
/// the caller maps that to a `status = "failure"` cell. AC-16
/// `outputs_found_matches_expected = false` is **not** an `Err` — it
/// rides on the outcome.
pub(super) async fn run(
    mode: &mut dyn Mode,
    expected_outputs: u64,
    h_birth: u16,
) -> anyhow::Result<S3Outcome> {
    // Birthday-scoped: wipe + reimport at h_birth, then scan from same.
    mode.wipe_and_reimport(h_birth).await?;

    let scan_start = Instant::now();
    let scan = mode.scan_from_birthday(h_birth).await?;
    let t_scan_ms = u64::try_from(scan_start.elapsed().as_millis()).unwrap_or(u64::MAX);

    let blocks_scanned = scan.h_tip_end.saturating_sub(scan.h_tip_start);

    let blocks_per_sec = if t_scan_ms == 0 {
        None
    } else {
        Some((blocks_scanned as f64) / ((t_scan_ms as f64) / 1000.0))
    };

    let outputs_found_matches_expected = scan.outputs_found == expected_outputs;

    Ok(S3Outcome {
        t_scan_ms,
        blocks_per_sec,
        h_birth,
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
            // h_tip_start > 0 because S3 scans only the post-funding
            // range; in production the wallet imports at the birthday
            // height which becomes its scan window's lower bound.
            h_tip_start: 950,
            h_tip_end: 1100,
            outputs_found,
            utxo_count: outputs_found,
            balance_microtari: 0,
        }
    }

    #[tokio::test]
    async fn s3_outputs_match_expected_when_counts_agree() {
        let mut fake = FakeMode::new();
        fake.canned_scan = Some(canned_scan(128));
        let h_birth: u16 = 365;

        let outcome = run(&mut fake, 128, h_birth)
            .await
            .expect("S3 runs with canned scan");

        assert_eq!(outcome.outputs_found, 128);
        assert_eq!(outcome.expected_outputs, 128);
        assert!(
            outcome.outputs_found_matches_expected,
            "AC-16: outputs_found == expected_outputs => true",
        );
        assert_eq!(outcome.h_birth, h_birth, "h_birth must ride on the outcome");
        assert_eq!(outcome.h_tip_start, 950);
        assert_eq!(outcome.h_tip_end, 1100);
        assert_eq!(outcome.blocks_scanned, 150, "h_tip_end - h_tip_start");
        assert!(outcome.peak_rss_bytes.is_none(), "sampler lands in 3j");
        assert!(outcome.peak_cpu_pct.is_none(), "sampler lands in 3j");

        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec!["wipe_and_reimport", "scan_from_birthday"],
            "S3 calls wipe_and_reimport then scan_from_birthday",
        );
    }

    #[tokio::test]
    async fn s3_outputs_mismatch_records_false_without_bailing() {
        let mut fake = FakeMode::new();
        // Observed=128, expected=127 — AC-16 mismatch. Must record
        // matches_expected=false and Ok(outcome); MUST NOT bail.
        fake.canned_scan = Some(canned_scan(128));

        let outcome = run(&mut fake, 127, 365)
            .await
            .expect("AC-16 mismatch must NOT be an Err");

        assert_eq!(outcome.outputs_found, 128);
        assert_eq!(outcome.expected_outputs, 127);
        assert!(
            !outcome.outputs_found_matches_expected,
            "AC-16: 128 != 127 => false",
        );
    }

    #[tokio::test]
    async fn s3_propagates_scan_failure() {
        let mut fake = FakeMode::new();
        fake.fail_with = Some("simulated S3 scan failure".to_string());

        let err = run(&mut fake, 0, 365)
            .await
            .expect_err("scan failure must bubble up");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("simulated S3 scan failure"),
            "error message must carry the underlying error: {msg}",
        );
    }
}
