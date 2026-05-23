//! Live [`BalanceQuery`] implementation backed by a transient
//! `console_wallet` subprocess per role.
//!
//! Per `analysis/DESIGN_AMENDMENT.md §7` (closed in step 3k), the published
//! `minotari_node_wallet_client = "5.3.1"` `BaseNodeWalletClient` trait does
//! NOT expose an address-indexed balance endpoint. The closure path is to
//! reuse the existing wallet gRPC's `GetBalance`: spawn a transient
//! `console_wallet` for the role's seed, wait for it to come ready via
//! [`crate::wallet_lifecycle::grpc::connect_with_retry`] + the lifecycle's
//! `wait_ready` poll, call `GetBalance`, tear down via SIGTERM grace +
//! SIGKILL escalation.
//!
//! Per-call cost: ~30s of `console_wallet` startup time per seed. Funding
//! pre-flight queries three seeds sequentially → ~90s harness startup
//! before the scenario matrix runs. Operators can pair this with manual
//! `minotari_miner` funding per RUNBOOK §Funding; the `--skip-funding-preflight`
//! flag on `main` is for testing only.

use std::sync::Arc;

use async_trait::async_trait;
use minotari_app_grpc::tari_rpc::GetBalanceRequest;
use tonic::Request;

use crate::config::Config;
use crate::guards::BalanceQuery;
use crate::seed::{SeedHandle, SeedRole};
use crate::wallet_lifecycle::console_wallet::ConsoleWalletLifecycle;
use crate::wallet_lifecycle::data_dir::HarnessDataDir;
use crate::wallet_lifecycle::WalletLifecycle;

const LOG_TARGET: &str = "c::wallet_lifecycle::balance_query";

/// Live `BalanceQuery` impl. Spawns a transient `console_wallet` per call,
/// queries `GetBalance`, returns `available_balance`. Holds only `Arc` to
/// the long-lived harness state (`Config`, `SeedHandle`) — the per-call
/// state (tempdir, subprocess, gRPC client) lives entirely inside the
/// `get_balance` invocation.
///
/// **Important**: each call spawns + tears down a wallet (~30s). Designed
/// for the funding pre-flight (3 calls at startup), not for per-scenario
/// liveness checks.
pub struct WalletGrpcBalanceQuery {
    config: Arc<Config>,
    seeds: Arc<SeedHandle>,
}

impl WalletGrpcBalanceQuery {
    /// Construct a `WalletGrpcBalanceQuery`. Cheap and side-effect-free —
    /// no network call made here.
    pub fn new(config: Arc<Config>, seeds: Arc<SeedHandle>) -> Self {
        Self { config, seeds }
    }

    /// Spawn a transient lifecycle for `role`, query `GetBalance`, tear
    /// down. Inline rather than a closure-taking helper because the
    /// borrow-checker doesn't see the right lifetime relationship between
    /// the `&mut Lifecycle` capture and the returned future without
    /// higher-ranked trait bounds — and we only ever call `get_balance`,
    /// so the reuse value is zero.
    async fn inner_get_balance(&self, role: SeedRole) -> anyhow::Result<u64> {
        log::info!(
            target: LOG_TARGET,
            "WalletGrpcBalanceQuery: spawning transient console_wallet for role {role:?}",
        );

        // Resolve the role's mnemonic before constructing the lifecycle —
        // missing env var surfaces here with a clear error rather than as
        // a half-spawned wallet's failure.
        let mnemonic = self.seeds.mnemonic_for(role)?;

        // Per-call tempdir under the harness data root. The
        // `HarnessDataDir::Drop` cleans up on scope exit. Unique `run_id`
        // per call so concurrent balance-query invocations don't collide
        // on the same tempdir path.
        let role_tag = match role {
            SeedRole::Old => "balance_query_old",
            SeedRole::New => "balance_query_new",
            SeedRole::Pp => "balance_query_pp",
        };
        let run_id = format!(
            "{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let data_dir = HarnessDataDir::new(&run_id, role_tag)?;

        // The existing `ConsoleWalletLifecycle::new` hardcodes
        // `mnemonic_old` — override via `replace_mnemonic` before spawn so
        // the wallet boots with the role's actual seed.
        let mut lifecycle = ConsoleWalletLifecycle::new(&self.config, &self.seeds, data_dir)?;
        lifecycle.replace_mnemonic(mnemonic.reveal().to_string())?;

        // Spawn + wait_ready + query, capturing the result regardless of
        // the wait_ready outcome so teardown still runs below.
        let result = async {
            lifecycle.spawn().await?;
            lifecycle.wait_ready().await?;
            let client = lifecycle.client_mut()?;
            let resp = client
                .get_balance(Request::new(GetBalanceRequest { payment_id: None }))
                .await
                .map_err(|s| anyhow::anyhow!("GetBalance gRPC call returned status: {s}"))?
                .into_inner();
            // `available_balance` is the spendable portion; pending_incoming
            // / pending_outgoing / timelocked are not yet counted toward the
            // funding pre-flight (operator-funded balances should already be
            // spendable by the time the harness runs).
            Ok::<u64, anyhow::Error>(resp.available_balance)
        }
        .await;

        // Tear down regardless of outcome. Teardown errors are logged but
        // don't override the body's result (the balance value is the
        // operator-facing answer; teardown errors are diagnostic noise).
        if let Err(td_err) = lifecycle.teardown().await {
            log::warn!(
                target: LOG_TARGET,
                "teardown error for role {role:?}: {td_err:#}",
            );
        }

        result
    }
}

#[async_trait]
impl BalanceQuery for WalletGrpcBalanceQuery {
    async fn get_balance(&self, role: SeedRole) -> anyhow::Result<u64> {
        self.inner_get_balance(role)
            .await
            .map_err(|e| e.context(format!("WalletGrpcBalanceQuery for role {role:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: a `WalletGrpcBalanceQuery` constructed against a bogus
    /// `minotari_console_wallet_path` bails cleanly (subprocess spawn
    /// failure) rather than hanging. Exercises the full
    /// `get_balance(role)` → `with_transient_wallet` → spawn-fails path.
    ///
    /// Live end-to-end against a real `console_wallet` happens in Phase 4's
    /// canonical baseline run; an in-process integration test would
    /// duplicate that infrastructure (running `console_wallet` per test
    /// adds ~30s to the unit-test suite). The trait-level shape is
    /// exercised by `crate::guards::tests` via [`crate::guards::BalanceQuery`]
    /// → `FakeBalanceQuery`.
    #[tokio::test]
    async fn balance_query_bails_when_console_wallet_binary_missing() {
        use std::path::PathBuf;

        use crate::config::{Config, Seeds};

        let seeds_cfg = Seeds {
            old: "WALLET_BENCHMARKS_TEST_BQ_OLD".to_string(),
            new: "WALLET_BENCHMARKS_TEST_BQ_NEW".to_string(),
            payment_processor: "WALLET_BENCHMARKS_TEST_BQ_PP".to_string(),
            wallet_password: "WALLET_BENCHMARKS_TEST_BQ_PW".to_string(),
        };
        // Populate the env vars so `mnemonic_for(role)` resolves; the spawn
        // failure happens at the subprocess layer, downstream of seed
        // resolution.
        let m = crate::gen_seed().expect("gen_seed");
        // SAFETY: test-only env mutation; allowed on Rust 1.84+ unsafe.
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(&seeds_cfg.old, &m);
            std::env::set_var(&seeds_cfg.new, &m);
            std::env::set_var(&seeds_cfg.payment_processor, &m);
        }

        let cfg = Arc::new(Config {
            // Point at a path that almost certainly doesn't exist so spawn
            // fails immediately rather than after a long wait_ready.
            minotari_console_wallet_path: Some(PathBuf::from(
                "/nonexistent/wallet-benchmarks-test-bogus-binary",
            )),
            seeds: seeds_cfg.clone(),
            // Sub-second per_tx_confirmation_timeout_ms so wait_ready bails
            // fast on the off chance the spawn somehow succeeded.
            per_tx_confirmation_timeout_ms: 500,
            ..Config::default()
        });
        let seeds = Arc::new(SeedHandle::new(&seeds_cfg));
        let bq = WalletGrpcBalanceQuery::new(cfg, seeds);

        let err = bq
            .get_balance(SeedRole::Old)
            .await
            .expect_err("bogus binary path must surface as a spawn error");
        let msg = format!("{err:#}");

        // Cleanup env vars regardless of test outcome.
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(&seeds_cfg.old);
            std::env::remove_var(&seeds_cfg.new);
            std::env::remove_var(&seeds_cfg.payment_processor);
        }

        assert!(
            msg.contains("role Old") || msg.contains("WalletGrpcBalanceQuery"),
            "error must surface the role context: {msg}",
        );
    }
}
