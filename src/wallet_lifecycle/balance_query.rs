//! Live [`BalanceQuery`] implementation backed by the base-node HTTP client.
//!
//! Per `analysis/DESIGN_AMENDMENT.md §7`, the published
//! `minotari_node_wallet_client = "5.3.1"` `BaseNodeWalletClient` trait does
//! NOT expose an address-indexed balance endpoint. The only source-of-truth
//! for "address X's balance" on this codebase is the wallet gRPC's
//! `GetBalance`, which requires a running console_wallet for the relevant
//! seed.
//!
//! Until Mode 1 lands a shared wallet-spawn pathway that a
//! `WalletGrpcBalanceQuery` impl can piggyback on, this module ships
//! [`LiveBalanceQuery`] as an explicit placeholder: construction succeeds,
//! [`BalanceQuery::get_balance`] bails with a structured error pointing at
//! the amendment and the operator-side workaround (`minotari_miner` funding
//! per RUNBOOK).
//!
//! Tests for the funding pre-flight continue to exercise `enforce_funding`
//! against the deterministic `FakeBalanceQuery` in `src/guards.rs`; that
//! coverage is unaffected by this gap.

use minotari_node_wallet_client::http::Client as BaseNodeHttpClient;
use tari_common_types::tari_address::TariAddress;
use url::Url;

use crate::guards::BalanceQuery;

const LOG_TARGET: &str = "c::wallet_lifecycle::balance_query";

/// Live `BalanceQuery` placeholder. Constructed against a base-node URL so
/// the dependency wiring matches the eventual real implementation; the
/// underlying [`BaseNodeHttpClient`] is held but not yet exercised.
///
/// See `analysis/DESIGN_AMENDMENT.md §7` for the rationale.
pub struct LiveBalanceQuery {
    /// Underlying base-node HTTP client. Held for the eventual real impl —
    /// currently unused, but the construction-side wiring is in place so a
    /// future patch can flip the `get_balance` body to a real query without
    /// touching call sites.
    _client: BaseNodeHttpClient,
    /// The URL for diagnostic messages.
    endpoint: Url,
}

impl LiveBalanceQuery {
    /// Construct a `LiveBalanceQuery` pointing at `base_node_url`. Cheap and
    /// side-effect-free — no network call made here.
    pub fn new(base_node_url: &Url) -> Self {
        Self {
            _client: BaseNodeHttpClient::new(base_node_url.clone(), base_node_url.clone()),
            endpoint: base_node_url.clone(),
        }
    }
}

impl BalanceQuery for LiveBalanceQuery {
    fn get_balance(&self, _address: &TariAddress) -> anyhow::Result<u64> {
        log::warn!(
            target: LOG_TARGET,
            "LiveBalanceQuery::get_balance called against {}; bailing per DESIGN_AMENDMENT.md §7",
            self.endpoint,
        );
        anyhow::bail!(
            "LiveBalanceQuery: balance pre-flight against the base node is not implementable \
             on minotari_node_wallet_client = \"5.3.1\" (no address-indexed balance endpoint; \
             see analysis/DESIGN_AMENDMENT.md §7). Operator workaround: fund each wallet via \
             minotari_miner per RUNBOOK §Funding and skip the pre-flight, or wait for the \
             follow-up wallet-gRPC-backed BalanceQuery once Mode 1 lands."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esmeralda_url() -> Url {
        Url::parse("https://rpc.esmeralda.tari.com").expect("esmeralda url parses")
    }

    #[test]
    fn live_balance_query_construction_does_not_make_a_network_call() {
        let _bq = LiveBalanceQuery::new(&esmeralda_url());
        // Construction is the test — if `BaseNodeHttpClient::new` ever gets
        // an eager connect added upstream, the harness assumptions break
        // and this test starts hanging.
    }

    #[test]
    fn live_balance_query_get_balance_bails_with_design_amendment_pointer() {
        let bq = LiveBalanceQuery::new(&esmeralda_url());
        // Build any TariAddress — the impl bails before reading it.
        let mnemonic = crate::gen_seed().expect("gen_seed");
        let addr = crate::seed::derive_address(&mnemonic).expect("derive_address");
        let err = bq.get_balance(&addr).expect_err("placeholder must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("DESIGN_AMENDMENT.md"),
            "error must point at the amendment for the gap rationale: {msg}",
        );
        assert!(
            msg.contains("minotari_miner"),
            "error must surface the operator workaround: {msg}",
        );
    }
}
