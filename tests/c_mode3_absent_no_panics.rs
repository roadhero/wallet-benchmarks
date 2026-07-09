//! Defect C acceptance (design Addition 2, criterion 2): the Config surface
//! is safe when the `[mode_3]` block is absent. Absence means "Mode 3
//! disabled": config load succeeds, Config::validate passes, and the run
//! loop records the nine Mode 3 cells as skipped (covered by the main.rs
//! unit test `absent_mode3_records_nine_skipped_cells`; the construction
//! backstop is covered by payment_processor's
//! `new_without_mode3_errs_cleanly_instead_of_panicking`).
//!
//! Scope note: a full-binary end-to-end run with Mode 3 absent needs live
//! wallets and a base node, so it is not CI-runnable here; this test pins
//! the lib-side contract the run loop relies on.

use wallet_benchmarks::config::{load, Config};

#[test]
fn config_mode_3_none_is_safe_across_all_call_sites() {
    // Through the production load path (file on disk, not toml::from_str),
    // with no [mode_3] block present.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("harness.toml");
    std::fs::write(
        &path,
        "network = \"esmeralda\"\nbase_node_url = \"https://rpc.esmeralda.tari.com\"\n",
    )
    .expect("write config");
    let cfg: Config = load::load(&path).expect("mode_3-less config must load");
    assert!(cfg.mode_3.is_none(), "absent block parses to None");
    cfg.validate()
        .expect("Config::validate must accept an absent mode_3 block");
}
