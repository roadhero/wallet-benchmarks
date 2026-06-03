//! Mode 3 — scan-shaped methods skip with `UnsupportedOperation`.
//!
//! Per `analysis/specs/MODE_3_REWORK_SPEC.md §7` and `§13` integration list:
//! Mode 3 does not own a scanning wallet, so the four scan-shaped methods
//! (`scan_from_birthday`, `get_balance`, `get_utxo_count`,
//! `wipe_and_reimport`) must return `UnsupportedOperation` rather than
//! attempt any subprocess or HTTP call. The runner in `main.rs` downcasts
//! the error and records the cell as `CellResult::NotRun` rather than
//! `CellResult::Error` (see `src/main.rs:247`).
//!
//! This integration test constructs a real `PaymentProcessor` instance via
//! the public `Mode` trait surface and asserts each of the four methods
//! returns `UnsupportedOperation`. No daemon is spawned and no
//! `start_external_services` call happens — the methods short-circuit
//! before reaching the HTTP client or any subprocess.

use std::path::PathBuf;

use wallet_benchmarks::{
    config::{
        Config, Mode3Account, Mode3Accounts, Mode3Config, Seeds, WorkerSleepOverrides,
    },
    gen_seed,
    modes::{payment_processor::PaymentProcessor, Mode, UnsupportedOperation},
    seed::SeedHandle,
    wallet_lifecycle::HarnessDataDir,
};

fn fake_pp_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_pp.sh")
}

fn fake_minotari_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_minotari.sh")
}

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

/// Builds a fully-populated Mode 3 instance with all env vars set. Returns
/// the instance + a list of env-var names the caller must `unset_env` to
/// clean up.
fn build_mode3(suffix: &str) -> (PaymentProcessor, Vec<String>) {
    let seeds_cfg = Seeds {
        old: format!("WB_INT_MODE3_OLD_{suffix}"),
        new: format!("WB_INT_MODE3_NEW_{suffix}"),
        payment_processor: format!("WB_INT_MODE3_PP_{suffix}"),
        wallet_password: format!("WB_INT_MODE3_PW_{suffix}"),
    };
    let view_env = format!("WB_INT_MODE3_VIEW_{suffix}");
    let spend_env = format!("WB_INT_MODE3_SPEND_{suffix}");
    let m_old = gen_seed().expect("m_old");
    let m_new = gen_seed().expect("m_new");
    let m_pp = gen_seed().expect("m_pp");
    set_env(&seeds_cfg.old, &m_old);
    set_env(&seeds_cfg.new, &m_new);
    set_env(&seeds_cfg.payment_processor, &m_pp);
    set_env(&seeds_cfg.wallet_password, "test-password");
    set_env(
        &view_env,
        "572a5fb63972da84aeec33071d13074e244d80c52be842ab5b0859ef4b4db00a",
    );
    set_env(
        &spend_env,
        "40e65c9bbf4592bc995c421108c01a5d7c9f9b2239569757895134549cef371f",
    );
    let cfg = Config {
        seeds: seeds_cfg.clone(),
        mode_3: Some(Mode3Config {
            pp_binary_path: fake_pp_path(),
            minotari_binary_path: fake_minotari_path(),
            api_port: 9145,
            pr_port: 9146,
            terminal_state_poll_timeout_secs: 1,
            worker_sleep_overrides: WorkerSleepOverrides::default(),
            accounts: Mode3Accounts {
                bench: Mode3Account {
                    view_key_env: view_env.clone(),
                    public_spend_key_env: spend_env.clone(),
                },
            },
        }),
        ..Config::default()
    };
    let seeds = SeedHandle::new(&seeds_cfg);
    let pp_dir = HarnessDataDir::new(&format!("int-mode3-pp-{suffix}"), "payment_processor")
        .expect("pp data dir");
    let pr_dir = HarnessDataDir::new(&format!("int-mode3-pr-{suffix}"), "payment_processor")
        .expect("pr data dir");
    let mode = PaymentProcessor::new(cfg, seeds, pp_dir, pr_dir).expect("Mode 3 ctor");
    (
        mode,
        vec![
            seeds_cfg.old,
            seeds_cfg.new,
            seeds_cfg.payment_processor,
            seeds_cfg.wallet_password,
            view_env,
            spend_env,
        ],
    )
}

fn teardown(envs: &[String]) {
    for e in envs {
        unset_env(e);
    }
}

/// Helper: assert the error downcasts to `UnsupportedOperation` with the
/// expected `op` field.
fn assert_unsupported(err: anyhow::Error, expected_op: &str) {
    let uo = err
        .downcast_ref::<UnsupportedOperation>()
        .unwrap_or_else(|| panic!("error must downcast to UnsupportedOperation: {err:#}"));
    assert_eq!(
        uo.mode, "payment_processor",
        "UnsupportedOperation.mode must be 'payment_processor'",
    );
    assert_eq!(
        uo.op, expected_op,
        "UnsupportedOperation.op must be {expected_op}",
    );
}

#[tokio::test]
async fn mode3_scan_from_birthday_b0_returns_unsupported() {
    // B0 birthday=0 — see analysis/specs/MODE_3_REWORK_SPEC.md §7 table.
    let (mut mode, envs) = build_mode3("B0");
    let err = mode
        .scan_from_birthday(0)
        .await
        .expect_err("B0 scan_from_birthday must be skipped");
    assert_unsupported(err, "scan_from_birthday");
    teardown(&envs);
}

#[tokio::test]
async fn mode3_scan_from_birthday_s2_returns_unsupported() {
    // S2 full rescan (birthday=0 with funded wallet).
    let (mut mode, envs) = build_mode3("S2");
    let err = mode
        .scan_from_birthday(0)
        .await
        .expect_err("S2 full rescan must be skipped");
    assert_unsupported(err, "scan_from_birthday");
    teardown(&envs);
}

#[tokio::test]
async fn mode3_scan_from_birthday_s3_returns_unsupported() {
    // S3 birthday rescan — calls scan_from_birthday with non-zero birthday.
    let (mut mode, envs) = build_mode3("S3");
    let err = mode
        .scan_from_birthday(12345)
        .await
        .expect_err("S3 birthday rescan must be skipped");
    assert_unsupported(err, "scan_from_birthday");
    teardown(&envs);
}

#[tokio::test]
async fn mode3_scan_from_birthday_s6_returns_unsupported() {
    // S6 = S2-shape after S5 — same method, same expected outcome.
    let (mut mode, envs) = build_mode3("S6");
    let err = mode
        .scan_from_birthday(0)
        .await
        .expect_err("S6 must be skipped");
    assert_unsupported(err, "scan_from_birthday");
    teardown(&envs);
}

#[tokio::test]
async fn mode3_scan_from_birthday_s7_returns_unsupported() {
    // S7 = S3-shape after S5 — same method, same expected outcome.
    let (mut mode, envs) = build_mode3("S7");
    let err = mode
        .scan_from_birthday(99)
        .await
        .expect_err("S7 must be skipped");
    assert_unsupported(err, "scan_from_birthday");
    teardown(&envs);
}

#[tokio::test]
async fn mode3_get_balance_returns_unsupported() {
    // The scan scenarios also call get_balance / get_utxo_count to record
    // pre/post counts. Mode 3 routes both into UnsupportedOperation.
    let (mut mode, envs) = build_mode3("GET_BAL");
    let err = mode
        .get_balance()
        .await
        .expect_err("get_balance must be skipped");
    assert_unsupported(err, "get_balance");
    teardown(&envs);
}

#[tokio::test]
async fn mode3_get_utxo_count_returns_unsupported() {
    let (mut mode, envs) = build_mode3("GET_UTXO");
    let err = mode
        .get_utxo_count()
        .await
        .expect_err("get_utxo_count must be skipped");
    assert_unsupported(err, "get_utxo_count");
    teardown(&envs);
}

#[tokio::test]
async fn mode3_wipe_and_reimport_returns_unsupported() {
    let (mut mode, envs) = build_mode3("WIPE");
    let err = mode
        .wipe_and_reimport(0)
        .await
        .expect_err("wipe_and_reimport must be skipped");
    assert_unsupported(err, "wipe_and_reimport");
    teardown(&envs);
}

#[tokio::test]
async fn mode3_unsupported_reason_is_consistent_across_methods() {
    // Per spec §7 every Mode 3 skip surfaces the same UNSUPPORTED_REASON
    // string (no scanning wallet ... view-keys only). Verify the four
    // scan-shaped methods all carry that exact reason.
    let (mut mode, envs) = build_mode3("REASON");
    let mut reasons: Vec<&'static str> = Vec::new();
    let e1 = mode.scan_from_birthday(0).await.expect_err("scan");
    reasons.push(
        e1.downcast_ref::<UnsupportedOperation>()
            .expect("downcast")
            .reason,
    );
    let e2 = mode.get_balance().await.expect_err("balance");
    reasons.push(
        e2.downcast_ref::<UnsupportedOperation>()
            .expect("downcast")
            .reason,
    );
    let e3 = mode.get_utxo_count().await.expect_err("utxo");
    reasons.push(
        e3.downcast_ref::<UnsupportedOperation>()
            .expect("downcast")
            .reason,
    );
    let e4 = mode.wipe_and_reimport(0).await.expect_err("wipe");
    reasons.push(
        e4.downcast_ref::<UnsupportedOperation>()
            .expect("downcast")
            .reason,
    );
    let first = reasons[0];
    for r in &reasons[1..] {
        assert_eq!(*r, first, "all skip reasons must be identical");
    }
    assert!(
        first.contains("scanning wallet"),
        "reason must explain the no-scanning-wallet rationale: {first}",
    );
    teardown(&envs);
}
