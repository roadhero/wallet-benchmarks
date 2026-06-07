//! Live-network smoke for Mode 1's `send_batch_one_to_many` against Esmeralda.
//!
//! Scaffold-only — the operator fills the test body during the
//! baseline-run cycle per `analysis/specs/MODE_1_BATCH_SEND_SPEC.md §5
//! "Layer 2 — live-network smoke"` and `§8 Q5`. Gated behind
//! `#[ignore]` so `cargo nextest run` does NOT execute it by default;
//! invoke explicitly with `cargo test --test live_esmeralda_smoke_batch
//! -- --ignored`.
//!
//! Required environment for the live run:
//! - `HARNESS_WALLET_PW` — wallet password.
//! - Seed env vars listed in `Config::seeds` for the Mode 1 (`SeedRole::Old`)
//!   slot.
//! - A funded Mode 1 wallet on Esmeralda (per RUNBOOK §Funding).
//!
//! Expected shape:
//! 1. Spawn `OldWallet` via `ConsoleWalletLifecycle::new` + `spawn_and_wait_ready`.
//! 2. Build K=10 self-addressed `(TariAddress, u64)` recipients.
//! 3. Call `mode.send_batch_one_to_many(&recipients, fee_rate).await`.
//! 4. Assert `Ok(TxRecord { status: "success", .. })`.
//! 5. Tear down via the lifecycle's `Drop`.

#[ignore = "live-network smoke; requires funded Esmeralda wallet — see file doc"]
#[tokio::test]
async fn mode1_send_batch_one_to_many_lands_a_real_mw_tx_on_esmeralda() {
    // Scaffold per `analysis/specs/MODE_1_BATCH_SEND_SPEC.md §8 Q5`.
    // Operator fills the body during the baseline-run cycle.
    //
    // Reference shape (from spec):
    //
    //   let cfg = Config::default();
    //   let seeds = SeedHandle::new(&cfg.seeds);
    //   let data_dir = HarnessDataDir::new("live-smoke-batch", "old_wallet")
    //       .expect("data dir");
    //   let lifecycle = ConsoleWalletLifecycle::new(&cfg, &seeds, data_dir)
    //       .expect("lifecycle");
    //   let mut mode = OldWallet::new(lifecycle);
    //   mode.spawn_and_wait_ready().await.expect("wait ready");
    //   let self_address = /* derive from seeds */;
    //   let recipients: Vec<(TariAddress, u64)> = (0..10)
    //       .map(|_| (self_address.clone(), 1_000))
    //       .collect();
    //   let rec = mode
    //       .send_batch_one_to_many(&recipients, cfg.fee_rate)
    //       .await
    //       .expect("batch send ok");
    //   assert_eq!(rec.status, "success");
    //   assert!(!rec.txid.is_empty(), "txid populated from TransferResult");
}
