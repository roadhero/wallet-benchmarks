//! Pre-flight guards for the wallet-benchmarks harness.
//!
//! These run from `main()` before any subprocess spawn or RPC call. The mainnet-
//! protection guard is the single hardest safety gate in the harness: a non-
//! `esmeralda` network identifier or a mainnet base-node host MUST abort the run
//! with a non-zero exit before any wallet binary touches the network.
//!
//! See `analysis/DESIGN.md §Mainnet-protection guard`.

use anyhow::bail;

use crate::config::Config;

const LOG_TARGET: &str = "c::guards";

const ALLOWLIST_NETWORK: &str = "esmeralda";

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

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;

    fn config(network: &str, url: &str) -> Config {
        Config {
            network: network.to_string(),
            base_node_url: Url::parse(url).expect("test url parses"),
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
}
