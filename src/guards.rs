//! Pre-flight guards for the wallet-benchmarks harness.
//!
//! These run from `main()` before any subprocess spawn or RPC call. The mainnet-
//! protection guard is the single hardest safety gate in the harness: a non-
//! `esmeralda` network identifier or a mainnet base-node host MUST abort the run
//! with a non-zero exit before any wallet binary touches the network.
//!
//! See `analysis/DESIGN.md §Mainnet-protection guard`.

use anyhow::bail;
use tari_common_types::tari_address::TariAddress;

use crate::{config::Config, seed::SeedHandle};

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
/// Abstracts the "what is this address's spendable balance?" call so the
/// runtime can pair with either a live base-node client or a deterministic
/// fake in tests. The published `minotari_node_wallet_client = "5.3.1"`
/// `BaseNodeWalletClient` trait does NOT expose a balance method (verified
/// by reading the trait at `src/client/mod.rs`); the live implementation
/// will be filled in alongside `wallet_lifecycle` (step 3e) once the
/// harness has a concrete address-scanning surface to read from. See
/// `analysis/API_DRIFT.md §Step 3d`.
pub trait BalanceQuery {
    fn get_balance(&self, address: &TariAddress) -> anyhow::Result<u64>;
}

/// Funding pre-flight: every harness seed must have a wallet balance ≥
/// `config.a_fund * 11 / 10` (10% headroom). Per
/// `DESIGN_ADDENDUM.md §M2`. Called from `main()` immediately after
/// [`enforce_esmeralda`] and before any mode runs. Funding-tx fees and
/// timings are explicitly NOT in the result profile (AC-35) — this is a
/// guardrail, not a measurement.
pub fn enforce_funding(
    config: &Config,
    seeds: &SeedHandle,
    balance_query: &dyn BalanceQuery,
) -> anyhow::Result<()> {
    log::debug!(target: LOG_TARGET, "funding pre-flight against a_fund={}", config.a_fund);
    let required =
        config.a_fund.saturating_mul(FUNDING_HEADROOM_NUMERATOR) / FUNDING_HEADROOM_DENOMINATOR;

    let addr_old = seeds.address_old()?;
    let addr_new = seeds.address_new()?;
    let addr_pp = seeds.address_payment_processor()?;

    let bal_old = balance_query
        .get_balance(&addr_old)
        .map_err(|e| e.context("querying balance for the old-wallet seed"))?;
    let bal_new = balance_query
        .get_balance(&addr_new)
        .map_err(|e| e.context("querying balance for the new-wallet seed"))?;
    let bal_pp = balance_query
        .get_balance(&addr_pp)
        .map_err(|e| e.context("querying balance for the payment-processor seed"))?;

    let any_short = bal_old < required || bal_new < required || bal_pp < required;
    if !any_short {
        log::info!(
            target: LOG_TARGET,
            "funding pre-flight passed: required={required} uT, old={bal_old} uT, \
             new={bal_new} uT, pp={bal_pp} uT",
        );
        return Ok(());
    }

    let mut report = format!(
        "Funding pre-flight failed. Required >= {required} uT per seed (a_fund * 11/10).\n",
    );
    for (label, balance) in [
        ("old_wallet", bal_old),
        ("new_wallet", bal_new),
        ("payment_processor", bal_pp),
    ] {
        if balance < required {
            let deficit = required - balance;
            report.push_str(&format!(
                "  {label}: {balance} uT (short by {deficit} uT) FAIL\n",
            ));
        } else {
            report.push_str(&format!("  {label}: {balance} uT OK\n"));
        }
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

    use std::{cell::RefCell, collections::HashMap};

    use crate::{config::Seeds, gen_seed, seed::SeedHandle};

    /// Deterministic [`BalanceQuery`] for tests. Returns the value mapped to
    /// the address's base58 form; missing entries return the configured
    /// fallback. A `force_err` flag flips every call into an `Err` so the
    /// propagation test can exercise the context wrap.
    struct FakeBalanceQuery {
        balances: HashMap<String, u64>,
        force_err: bool,
        calls: RefCell<Vec<String>>,
    }

    impl FakeBalanceQuery {
        fn new(force_err: bool) -> Self {
            Self {
                balances: HashMap::new(),
                force_err,
                calls: RefCell::new(Vec::new()),
            }
        }
        fn set(&mut self, addr: &TariAddress, balance: u64) {
            self.balances.insert(addr.to_base58(), balance);
        }
    }

    impl BalanceQuery for FakeBalanceQuery {
        fn get_balance(&self, address: &TariAddress) -> anyhow::Result<u64> {
            self.calls.borrow_mut().push(address.to_base58());
            if self.force_err {
                anyhow::bail!("simulated balance query failure");
            }
            Ok(*self.balances.get(&address.to_base58()).unwrap_or(&0))
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

    /// Helper: install three fresh seeds into the per-test env vars, return
    /// the `SeedHandle` plus the three derived addresses for use as
    /// `FakeBalanceQuery` keys.
    fn install_three_seeds(seeds_cfg: &Seeds) -> (SeedHandle, [TariAddress; 3]) {
        let m_old = gen_seed().expect("gen_seed old");
        let m_new = gen_seed().expect("gen_seed new");
        let m_pp = gen_seed().expect("gen_seed pp");
        set_env(&seeds_cfg.old, &m_old);
        set_env(&seeds_cfg.new, &m_new);
        set_env(&seeds_cfg.payment_processor, &m_pp);
        let handle = SeedHandle::new(seeds_cfg);
        let addresses = [
            handle.address_old().expect("addr old"),
            handle.address_new().expect("addr new"),
            handle.address_payment_processor().expect("addr pp"),
        ];
        (handle, addresses)
    }

    #[test]
    fn enforce_funding_passes_when_all_balances_meet_required() {
        let seeds_cfg = unique_seeds("ALL_OK");
        let (handle, [a_old, a_new, a_pp]) = install_three_seeds(&seeds_cfg);
        let cfg = Config {
            a_fund: 10_000_000_000,
            ..Config::default()
        };
        let required = cfg.a_fund * 11 / 10; // 11_000_000_000
        let mut bq = FakeBalanceQuery::new(false);
        bq.set(&a_old, required);
        bq.set(&a_new, required + 1);
        bq.set(&a_pp, required.saturating_mul(2));
        let r = enforce_funding(&cfg, &handle, &bq);
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
        r.expect("three sufficient balances must pass");
    }

    #[test]
    fn enforce_funding_bails_when_one_balance_short() {
        let seeds_cfg = unique_seeds("ONE_SHORT");
        let (handle, [a_old, a_new, a_pp]) = install_three_seeds(&seeds_cfg);
        let cfg = Config {
            a_fund: 10_000_000_000,
            ..Config::default()
        };
        let required = cfg.a_fund * 11 / 10;
        let mut bq = FakeBalanceQuery::new(false);
        bq.set(&a_old, required); // OK
        bq.set(&a_new, 0); // short by required
        bq.set(&a_pp, required + 1_000_000); // OK
        let err = enforce_funding(&cfg, &handle, &bq).expect_err("a short balance must fail");
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

    #[test]
    fn enforce_funding_propagates_balance_query_errors() {
        let seeds_cfg = unique_seeds("ERR_PROPAGATE");
        let (handle, _addresses) = install_three_seeds(&seeds_cfg);
        let cfg = Config {
            a_fund: 10_000_000_000,
            ..Config::default()
        };
        let bq = FakeBalanceQuery::new(true);
        let err = enforce_funding(&cfg, &handle, &bq).expect_err("query err propagates");
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
}
