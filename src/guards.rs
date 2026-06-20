//! Pre-flight guards for the wallet-benchmarks harness.
//!
//! These run from `main()` before any subprocess spawn or RPC call. The mainnet-
//! protection guard is the single hardest safety gate in the harness: a non-
//! `esmeralda` network identifier or a mainnet base-node host MUST abort the run
//! with a non-zero exit before any wallet binary touches the network.
//!
//! See `analysis/DESIGN.md §Mainnet-protection guard`.

use anyhow::bail;

use crate::{
    config::Config,
    seed::{SeedHandle, SeedRole},
    wallet_lifecycle::pr_balance_query::PrBalanceQuery,
};

const LOG_TARGET: &str = "c::guards";

const ALLOWLIST_NETWORK: &str = "esmeralda";

/// 10% headroom above `a_fund` (integer math; clippy::float_arithmetic-safe).
const FUNDING_HEADROOM_NUMERATOR: u64 = 11;
const FUNDING_HEADROOM_DENOMINATOR: u64 = 10;

const MAINNET_HOST_DENYLIST: &[&str] = &[
    "rpc.tari.com",
    "seeds.tari.com",
    "mainnet.tari.com",
    "mainnet-rpc.tari.com",
];

/// Hard allowlist enforcing that the harness only ever targets Esmeralda.
///
/// Bails (non-zero exit at the caller) when:
///   * `config.network` is anything other than `"esmeralda"`, or
///   * `config.base_node_url`'s host string contains any mainnet denylist entry.
///
/// Defense in depth: `wallet_lifecycle::console_wallet::spawn()` will re-assert
/// the same allowlist on each wallet spawn so a stale `Config` cannot leak past
/// this entry point.
pub fn enforce_esmeralda(config: &Config) -> anyhow::Result<()> {
    if config.network != ALLOWLIST_NETWORK {
        log::error!(
            target: LOG_TARGET,
            "rejecting non-esmeralda network: {}",
            config.network
        );
        bail!("network={} but only 'esmeralda' is allowed", config.network);
    }
    let host = config.base_node_url.host_str().unwrap_or("");
    if MAINNET_HOST_DENYLIST.iter().any(|d| host.contains(d)) {
        log::error!(
            target: LOG_TARGET,
            "rejecting mainnet-denylisted base-node host: {}",
            host
        );
        bail!("base_node_url host '{}' on mainnet denylist", host);
    }
    Ok(())
}

/// Funding pre-flight balance query.
///
/// Abstracts the "what is this seed's spendable balance?" call so the
/// runtime can pair with either a live `console_wallet`-backed implementation
/// (see [`crate::wallet_lifecycle::balance_query::WalletGrpcBalanceQuery`])
/// or a deterministic fake in tests. Keyed by [`SeedRole`] rather than by
/// address because the live implementation spawns a wallet per role anyway;
/// the address parameter was design-smell (caller derives address from
/// seed, impl reverse-looks-up). Per `analysis/DESIGN_AMENDMENT.md §7`.
///
/// Async because the live implementation spawns a transient `console_wallet`
/// subprocess for each call; the trait method awaits the wallet's
/// `wait_ready` poll and the `GetBalance` gRPC roundtrip.
#[async_trait::async_trait]
pub trait BalanceQuery: Send + Sync {
    /// Return the spendable balance, in microTari, for the given role's
    /// seed. Failure modes (spawn error, gRPC error, etc.) bubble up as
    /// `Err` so [`enforce_funding`] can wrap them with role context.
    async fn get_balance(&self, role: SeedRole) -> anyhow::Result<u64>;
}

/// Funding pre-flight: every harness seed must have a wallet balance ≥
/// `config.a_fund * 11 / 10` (10% headroom). Per
/// `DESIGN_ADDENDUM.md §M2`. Called from `main()` immediately after
/// [`enforce_esmeralda`] and before any mode runs. Funding-tx fees and
/// timings are explicitly NOT in the result profile (AC-35) — this is a
/// guardrail, not a measurement.
///
/// **Mode 3 note (swe-review C4)**: when `pr_balance_query` is `Some`,
/// the `SeedRole::Pp` check uses the PR daemon's
/// `GET /accounts/default/balance` endpoint rather than the generic
/// `balance_query` (which would spawn a transient `console_wallet`
/// against the PP mnemonic — wrong wallet, since Mode 3's signer is the
/// view+spend keypair held by the PR daemon, not a mnemonic-derived
/// address). When the PR daemon isn't yet up at pre-flight time
/// (the harness spawns PR later, inside the per-mode loop), the HTTP
/// call returns a transport error and the Mode 3 PP check is logged +
/// skipped rather than failing the whole pre-flight. Operators who want
/// strict pre-flight Mode 3 coverage must pre-warm the PR daemon
/// out-of-band before running the harness; the warn log explains this.
pub async fn enforce_funding(
    config: &Config,
    seeds: &SeedHandle,
    balance_query: &dyn BalanceQuery,
    pr_balance_query: Option<&PrBalanceQuery>,
) -> anyhow::Result<()> {
    log::debug!(target: LOG_TARGET, "funding pre-flight against a_fund={}", config.a_fund);
    let required =
        config.a_fund.saturating_mul(FUNDING_HEADROOM_NUMERATOR) / FUNDING_HEADROOM_DENOMINATOR;

    // Validate seed mnemonics are resolvable before the per-role query loop
    // — surfaces a missing-env-var error from `enforce_funding` rather than
    // from a half-spawned wallet.
    seeds.assert_distinct()?;

    let bal_old = balance_query
        .get_balance(SeedRole::Old)
        .await
        .map_err(|e| e.context("querying balance for the old-wallet seed"))?;
    let bal_new = balance_query
        .get_balance(SeedRole::New)
        .await
        .map_err(|e| e.context("querying balance for the new-wallet seed"))?;

    // Pp arm: Mode 3 path queries the PR daemon's HTTP API; the legacy
    // mnemonic-derived path stays as the fallback for Mode 1+2 runs that
    // don't configure Mode 3.
    let bal_pp_opt: Option<u64> = if let Some(pr_bq) = pr_balance_query {
        match pr_bq.get_balance().await {
            Ok(b) => {
                log::info!(
                    target: LOG_TARGET,
                    "Mode 3 PR-daemon balance pre-flight: {b} uT (querying {})",
                    pr_bq.balance_url(),
                );
                Some(b)
            }
            Err(e) => {
                log::warn!(
                    target: LOG_TARGET,
                    "Mode 3 PR-daemon balance pre-flight skipped: {e:#}. The PR daemon \
                     isn't reachable at {} yet — pre-warm it out-of-band if you want \
                     strict pre-flight coverage. Otherwise the per-mode loop will spawn \
                     it before scenarios run.",
                    pr_bq.balance_url(),
                );
                None
            }
        }
    } else {
        Some(
            balance_query
                .get_balance(SeedRole::Pp)
                .await
                .map_err(|e| e.context("querying balance for the payment-processor seed"))?,
        )
    };

    let any_short =
        bal_old < required || bal_new < required || bal_pp_opt.is_some_and(|b| b < required);
    if !any_short {
        match bal_pp_opt {
            Some(bal_pp) => {
                log::info!(
                    target: LOG_TARGET,
                    "funding pre-flight passed: required={required} uT, old={bal_old} uT, \
                     new={bal_new} uT, pp={bal_pp} uT",
                );
                // Terminal feedback (stdout, independent of RUST_LOG) so
                // the operator sees pre-flight outcome without log noise.
                println!(
                    "[{}] preflight  old={bal_old}uT new={bal_new}uT pp={bal_pp}uT  PASS",
                    chrono::Local::now().format("%H:%M:%S"),
                );
            }
            None => {
                log::info!(
                    target: LOG_TARGET,
                    "funding pre-flight passed (pp arm skipped): required={required} uT, \
                     old={bal_old} uT, new={bal_new} uT",
                );
                println!(
                    "[{}] preflight  old={bal_old}uT new={bal_new}uT pp=SKIPPED  PASS",
                    chrono::Local::now().format("%H:%M:%S"),
                );
            }
        }
        return Ok(());
    }

    let mut report = format!(
        "Funding pre-flight failed. Required >= {required} uT per seed (a_fund * 11/10).\n",
    );
    let pp_row: Option<(&str, u64)> = bal_pp_opt.map(|b| ("payment_processor", b));
    let core_rows: &[(&str, u64)] = &[("old_wallet", bal_old), ("new_wallet", bal_new)];
    for (label, balance) in core_rows.iter().copied().chain(pp_row) {
        if balance < required {
            let deficit = required - balance;
            report.push_str(&format!(
                "  {label}: {balance} uT (short by {deficit} uT) FAIL\n",
            ));
        } else {
            report.push_str(&format!("  {label}: {balance} uT OK\n"));
        }
    }
    if bal_pp_opt.is_none() {
        report.push_str(
            "  payment_processor: SKIPPED (PR daemon not reachable at pre-flight; see warn log)\n",
        );
    }
    report.push_str("See RUNBOOK §Funding for how to mine to each address using minotari_miner.");
    bail!("{report}");
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;

    fn config(network: &str, url: &str) -> Config {
        Config {
            network: network.to_string(),
            base_node_url: Url::parse(url).expect("test url parses"),
            ..Config::default()
        }
    }

    #[test]
    fn accepts_esmeralda_with_esmeralda_host() {
        let cfg = config("esmeralda", "https://rpc.esmeralda.tari.com");
        enforce_esmeralda(&cfg).expect("esmeralda + esmeralda host should pass");
    }

    #[test]
    fn rejects_mainnet_network() {
        let cfg = config("mainnet", "https://rpc.esmeralda.tari.com");
        let err = enforce_esmeralda(&cfg).expect_err("mainnet network must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("mainnet"),
            "error should name the network: {msg}"
        );
        assert!(
            msg.contains("esmeralda"),
            "error should name the allowlist: {msg}"
        );
    }

    #[test]
    fn rejects_esmeralda_pointed_at_mainnet_host() {
        let cfg = config("esmeralda", "https://rpc.tari.com/json_rpc");
        let err = enforce_esmeralda(&cfg).expect_err("mainnet host must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("denylist"),
            "error should mention denylist: {msg}"
        );
    }

    #[test]
    fn rejects_each_mainnet_denylist_entry() {
        for host in MAINNET_HOST_DENYLIST {
            let url = format!("https://{host}/");
            let cfg = config("esmeralda", &url);
            enforce_esmeralda(&cfg).expect_err(&format!("denylist entry must be rejected: {host}"));
        }
    }

    #[test]
    fn rejects_empty_network() {
        let cfg = config("", "https://rpc.esmeralda.tari.com");
        enforce_esmeralda(&cfg).expect_err("empty network must be rejected");
    }

    /// `url::Url::host_str` returns `"[::1]"` (bracketed) for an IPv6 URL —
    /// confirmed in `analysis/API_DRIFT.md §Step 3c`. The mainnet denylist
    /// is DNS-only, so an IPv6 loopback URL must pass; lock that in.
    #[test]
    fn accepts_ipv6_bracketed_loopback() {
        let cfg = config("esmeralda", "https://[::1]:9005");
        enforce_esmeralda(&cfg).expect("IPv6 loopback must pass the mainnet guard");
    }

    #[test]
    fn accepts_ipv4_loopback() {
        let cfg = config("esmeralda", "https://127.0.0.1:9005");
        enforce_esmeralda(&cfg).expect("IPv4 loopback must pass the mainnet guard");
    }

    #[test]
    fn accepts_localhost() {
        let cfg = config("esmeralda", "https://localhost:9005");
        enforce_esmeralda(&cfg).expect("localhost must pass the mainnet guard");
    }

    // ----- enforce_funding tests -----

    use std::collections::HashMap;

    use crate::{config::Seeds, gen_seed, seed::SeedHandle};

    /// Deterministic [`BalanceQuery`] for tests. Returns the value mapped to
    /// the role; missing entries return zero. A `force_err` flag flips every
    /// call into an `Err` so the propagation test can exercise the context
    /// wrap. Uses `std::sync::Mutex` because `BalanceQuery` is now async and
    /// requires `Send + Sync` (RefCell is `!Sync`).
    struct FakeBalanceQuery {
        balances: HashMap<SeedRole, u64>,
        force_err: bool,
        calls: std::sync::Mutex<Vec<SeedRole>>,
    }

    impl FakeBalanceQuery {
        fn new(force_err: bool) -> Self {
            Self {
                balances: HashMap::new(),
                force_err,
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn set(&mut self, role: SeedRole, balance: u64) {
            self.balances.insert(role, balance);
        }
    }

    #[async_trait::async_trait]
    impl BalanceQuery for FakeBalanceQuery {
        async fn get_balance(&self, role: SeedRole) -> anyhow::Result<u64> {
            self.calls.lock().unwrap().push(role);
            if self.force_err {
                anyhow::bail!("simulated balance query failure");
            }
            Ok(*self.balances.get(&role).unwrap_or(&0))
        }
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

    /// Seed config with unique env-var names for the calling test.
    fn unique_seeds(suffix: &str) -> Seeds {
        Seeds {
            old: format!("WALLET_BENCHMARKS_TEST_FUND_OLD_{suffix}"),
            new: format!("WALLET_BENCHMARKS_TEST_FUND_NEW_{suffix}"),
            payment_processor: format!("WALLET_BENCHMARKS_TEST_FUND_PP_{suffix}"),
            wallet_password: format!("WALLET_BENCHMARKS_TEST_FUND_PW_{suffix}"),
        }
    }

    /// Helper: install three fresh seeds into the per-test env vars and
    /// return the resulting [`SeedHandle`]. Address derivation is no longer
    /// needed (the trait is keyed by `SeedRole` directly).
    fn install_three_seeds(seeds_cfg: &Seeds) -> SeedHandle {
        let m_old = gen_seed().expect("gen_seed old");
        let m_new = gen_seed().expect("gen_seed new");
        let m_pp = gen_seed().expect("gen_seed pp");
        set_env(&seeds_cfg.old, &m_old);
        set_env(&seeds_cfg.new, &m_new);
        set_env(&seeds_cfg.payment_processor, &m_pp);
        SeedHandle::new(seeds_cfg)
    }

    #[tokio::test]
    async fn enforce_funding_passes_when_all_balances_meet_required() {
        let seeds_cfg = unique_seeds("ALL_OK");
        let handle = install_three_seeds(&seeds_cfg);
        let cfg = Config {
            a_fund: 10_000_000_000,
            ..Config::default()
        };
        let required = cfg.a_fund * 11 / 10; // 11_000_000_000
        let mut bq = FakeBalanceQuery::new(false);
        bq.set(SeedRole::Old, required);
        bq.set(SeedRole::New, required + 1);
        bq.set(SeedRole::Pp, required.saturating_mul(2));
        let r = enforce_funding(&cfg, &handle, &bq, None).await;
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
        r.expect("three sufficient balances must pass");
    }

    #[tokio::test]
    async fn enforce_funding_bails_when_one_balance_short() {
        let seeds_cfg = unique_seeds("ONE_SHORT");
        let handle = install_three_seeds(&seeds_cfg);
        let cfg = Config {
            a_fund: 10_000_000_000,
            ..Config::default()
        };
        let required = cfg.a_fund * 11 / 10;
        let mut bq = FakeBalanceQuery::new(false);
        bq.set(SeedRole::Old, required); // OK
        bq.set(SeedRole::New, 0); // short by required
        bq.set(SeedRole::Pp, required + 1_000_000); // OK
        let err = enforce_funding(&cfg, &handle, &bq, None)
            .await
            .expect_err("a short balance must fail");
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
        let msg = format!("{err:#}");
        // Error contains all three labels per DESIGN_ADDENDUM §M2 format.
        assert!(
            msg.contains("old_wallet"),
            "report must list old_wallet: {msg}"
        );
        assert!(
            msg.contains("new_wallet"),
            "report must list new_wallet: {msg}"
        );
        assert!(
            msg.contains("payment_processor"),
            "report must list payment_processor: {msg}",
        );
        // Required is named.
        assert!(
            msg.contains(&required.to_string()),
            "report must name required={required}: {msg}",
        );
        // Deficit is reported for the failing seed.
        assert!(
            msg.contains("short by"),
            "report must call out the short amount: {msg}"
        );
        assert!(
            msg.contains("RUNBOOK"),
            "report must point at RUNBOOK funding section: {msg}"
        );
    }

    #[tokio::test]
    async fn enforce_funding_propagates_balance_query_errors() {
        let seeds_cfg = unique_seeds("ERR_PROPAGATE");
        let handle = install_three_seeds(&seeds_cfg);
        let cfg = Config {
            a_fund: 10_000_000_000,
            ..Config::default()
        };
        let bq = FakeBalanceQuery::new(true);
        let err = enforce_funding(&cfg, &handle, &bq, None)
            .await
            .expect_err("query err propagates");
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
        let msg = format!("{err:#}");
        assert!(
            msg.contains("querying balance"),
            "context layer should name the phase: {msg}",
        );
        assert!(
            msg.contains("simulated balance query failure"),
            "underlying cause should be preserved: {msg}",
        );
    }

    #[tokio::test]
    async fn enforce_funding_routes_pp_arm_through_pr_balance_query_when_provided() {
        // Per swe-review C4: when `pr_balance_query` is Some, the Pp
        // arm uses the PR daemon's HTTP endpoint and the legacy
        // BalanceQuery for Pp is NOT consulted. Wiremock proves the call
        // shape (GET /accounts/default/balance), parses `available` from
        // the JSON, and treats the Pp arm as funded.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let seeds_cfg = unique_seeds("C4_PR_ROUTING");
        let handle = install_three_seeds(&seeds_cfg);
        let cfg = Config {
            a_fund: 10_000_000_000,
            ..Config::default()
        };
        let required = cfg.a_fund * 11 / 10;
        let mut bq = FakeBalanceQuery::new(false);
        bq.set(SeedRole::Old, required);
        bq.set(SeedRole::New, required);
        // Pp arm on the FakeBalanceQuery is intentionally zero —
        // if pr_balance_query routing is wrong and the fallback is taken,
        // the test fails with "short by required".
        bq.set(SeedRole::Pp, 0);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/accounts/default/balance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total": required + 1,
                "available": required + 1,
                "locked": 0_u64,
                "unconfirmed": 0_u64,
                "immature": 0_u64,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let pr_bq = PrBalanceQuery::new(server.uri(), "default");

        let result = enforce_funding(&cfg, &handle, &bq, Some(&pr_bq)).await;
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
        result.expect("pp arm via PR balance query must pass");

        // FakeBalanceQuery records every call it sees. Confirm the
        // Pp arm was NOT routed through it (the PrBalanceQuery handled
        // Pp instead).
        let calls = bq.calls.lock().unwrap().clone();
        assert!(
            !calls.contains(&SeedRole::Pp),
            "FakeBalanceQuery must not have been asked for Pp — routing went through \
             PrBalanceQuery instead. Saw calls: {calls:?}",
        );
    }

    #[tokio::test]
    async fn enforce_funding_logs_warning_and_skips_pp_when_pr_daemon_unreachable() {
        // The common pre-flight case: Mode 3 is configured but the PR
        // daemon isn't yet spawned (start_external_services runs later).
        // The pre-flight gets a connection error on the Pp arm; the
        // warn-path skips the Pp check and the rest of the pre-flight
        // proceeds.
        let seeds_cfg = unique_seeds("C4_PR_UNREACHABLE");
        let handle = install_three_seeds(&seeds_cfg);
        let cfg = Config {
            a_fund: 10_000_000_000,
            ..Config::default()
        };
        let required = cfg.a_fund * 11 / 10;
        let mut bq = FakeBalanceQuery::new(false);
        bq.set(SeedRole::Old, required);
        bq.set(SeedRole::New, required);
        // Point at port 1 — guaranteed connection-refused on every
        // sane host.
        let pr_bq = PrBalanceQuery::new("http://127.0.0.1:1", "default");

        let result = enforce_funding(&cfg, &handle, &bq, Some(&pr_bq)).await;
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
        result.expect(
            "pre-flight must pass with the Pp arm skipped when the PR daemon is unreachable",
        );
    }
}
