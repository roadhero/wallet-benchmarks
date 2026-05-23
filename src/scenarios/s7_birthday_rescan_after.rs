//! S7 birthday rescan after S5 — wipe data dir, set birthday=`h_birth`,
//! scan from the funding-tx height, and verify `outputs_found` against the
//! caller-supplied expected count.
//!
//! Per `analysis/DESIGN.md §Scenario state machine §S7` and
//! `analysis/RESULT_PROFILE_SCHEMA.md §S7 scenario`: S7 has the **identical
//! Outcome shape as S3** (birthday-scoped rescan after the prior funding
//! scenario). The semantic difference vs S3 is the preceding state — S3
//! runs after S1's UTXO multiplication; S7 runs after S5's 100-recipient
//! send-volume — but the per-cell payload structure is the same.
//!
//! Preconditions: S6 has completed against the same mode. The caller passes
//! `h_birth` (S0's funding height, `u16` days-since-2022-01-01) and
//! `expected_outputs` (S1 net + S5 net) via `ScenarioInput`; S7 does not
//! introspect any prior outcome directly.
//!
//! Steps mirror S3 verbatim:
//!
//! 1. `mode.wipe_and_reimport(h_birth)` — set birthday before rescan.
//! 2. Time the scan via `mode.scan_from_birthday(h_birth)`.
//! 3. `blocks_scanned = h_tip_end - h_tip_start` from the `ScanOutcome`.
//! 4. `outputs_found_matches_expected = (scan.outputs_found ==
//!    expected_outputs)` recorded as a `bool`. Mismatch is **not** an `Err`
//!    — the cell-envelope writer (step 3k) decides. `h_birth` itself rides
//!    on the outcome so the writer can assert `birthday_set == h_birth`.
//!
//! Failure-halt: no — a failing scan records `status = "failure"` at the
//! cell envelope and the run continues.

use std::time::Instant;

use crate::modes::Mode;

/// S7's per-cell payload — identical structure to
/// [`crate::scenarios::S3Outcome`]. `peak_rss_bytes` and `peak_cpu_pct`
/// are `None` until the 1 Hz metrics sampler lands (step 3j).
#[derive(Debug, Clone, PartialEq)]
pub struct S7Outcome {
    /// `t_scan_ms` per `RESULT_PROFILE_SCHEMA.md §S7` — duration of the
    /// post-wipe birthday-scoped scan only.
    pub t_scan_ms: u64,
    /// `blocks_per_sec` per `RESULT_PROFILE_SCHEMA.md §S7` —
    /// `blocks_scanned / (t_scan_ms / 1000)`. `None` when `t_scan_ms ==
    /// 0` (degenerate fake-mode case); matches the `Option<f64>` rate
    /// pattern established by B0/S2/S3/S6.
    pub blocks_per_sec: Option<f64>,
    /// `h_birth` — the funding-tx height the rescan targeted (`u16`
    /// days-since-2022-01-01 per `Mode::scan_from_birthday`). Rides on
    /// the outcome so the cell-envelope writer can populate the schema's
    /// `birthday_set` field.
    pub h_birth: u16,
    /// `h_tip_start` per `RESULT_PROFILE_SCHEMA.md §S7` — tip when scan
    /// begins (sourced from `ScanOutcome::h_tip_start`).
    pub h_tip_start: u64,
    /// `h_tip_end` per `RESULT_PROFILE_SCHEMA.md §S7` — tip when scan
    /// completes (sourced from `ScanOutcome::h_tip_end`).
    pub h_tip_end: u64,
    /// `blocks_scanned` per `RESULT_PROFILE_SCHEMA.md §S7` —
    /// `h_tip_end - h_tip_start`. The comparison with S6's full-history
    /// `blocks_scanned` happens at the cell-envelope writer level (3k);
    /// S7 records the raw value only.
    pub blocks_scanned: u64,
    /// `outputs_found` per `RESULT_PROFILE_SCHEMA.md §S7` — wallet
    /// outputs rediscovered by the birthday-scoped rescan; expected to
    /// equal `expected_outputs` (AC-16 mirror for post-S5 state).
    pub outputs_found: u64,
    /// Expected outputs: S1's net plus S5's net. Plumbed in via
    /// [`crate::scenarios::ScenarioInput::s7_expected_outputs`].
    pub expected_outputs: u64,
    /// `outputs_found == expected_outputs`. Recorded as a `bool` — a
    /// mismatch is **not** treated as scenario failure here; the
    /// cell-envelope writer decides what to do with `false`.
    pub outputs_found_matches_expected: bool,
    /// `peak_rss_bytes` per `RESULT_PROFILE_SCHEMA.md §S7` — `None` until
    /// the 1 Hz sampler lands in step 3j.
    pub peak_rss_bytes: Option<u64>,
    /// `peak_cpu_pct` per `RESULT_PROFILE_SCHEMA.md §S7` — `None` until
    /// the 1 Hz sampler lands in step 3j.
    pub peak_cpu_pct: Option<f64>,
}

/// Run S7 against the given mode.
///
/// `expected_outputs` is the AC-16-mirror verification target (S1 net +
/// S5 net). `h_birth` is the S0 funding height. Both are supplied by the
/// caller via [`crate::scenarios::ScenarioInput`].
///
/// Returns `Err` on `wipe_and_reimport` or `scan_from_birthday` failure;
/// the caller maps that to a `status = "failure"` cell.
/// `outputs_found_matches_expected = false` is **not** an `Err` — it
/// rides on the outcome.
pub(super) async fn run(
    mode: &mut dyn Mode,
    expected_outputs: u64,
    h_birth: u16,
) -> anyhow::Result<S7Outcome> {
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

    Ok(S7Outcome {
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
            h_tip_start: 950,
            h_tip_end: 1100,
            outputs_found,
            utxo_count: outputs_found,
            balance_microtari: 0,
        }
    }

    #[tokio::test]
    async fn s7_outputs_match_expected_when_counts_agree() {
        let mut fake = FakeMode::new();
        fake.canned_scan = Some(canned_scan(256));
        let h_birth: u16 = 365;

        let outcome = run(&mut fake, 256, h_birth)
            .await
            .expect("S7 runs with canned scan");

        assert_eq!(outcome.outputs_found, 256);
        assert_eq!(outcome.expected_outputs, 256);
        assert!(
            outcome.outputs_found_matches_expected,
            "outputs_found == expected_outputs => true",
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
            "S7 calls wipe_and_reimport then scan_from_birthday",
        );
    }

    #[tokio::test]
    async fn s7_outputs_mismatch_records_false_without_bailing() {
        let mut fake = FakeMode::new();
        fake.canned_scan = Some(canned_scan(256));

        let outcome = run(&mut fake, 255, 365)
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
    async fn s7_propagates_scan_failure() {
        let mut fake = FakeMode::new();
        fake.fail_with = Some("simulated S7 scan failure".to_string());

        let err = run(&mut fake, 0, 365)
            .await
            .expect_err("scan failure must bubble up");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("simulated S7 scan failure"),
            "error message must carry the underlying error: {msg}",
        );
    }
}
