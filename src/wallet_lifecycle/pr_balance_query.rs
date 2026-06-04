//! Mode 3 PR-daemon-balance query — funding pre-flight backing.
//!
//! Per `analysis/specs/MODE_3_REWORK_SPEC.md §5` the PR daemon is a
//! view-only `minotari daemon` instance: it has no mnemonic seed of its
//! own and PP holds the spend secret, so the existing
//! `WalletGrpcBalanceQuery` (which spawns a transient `console_wallet` per
//! seed) is the wrong tool for Mode 3's `SeedRole::Pp` funding check —
//! the mnemonic-derived address has no relationship to where the operator
//! actually funds the view-key wallet.
//!
//! The correct surface is the PR daemon's own HTTP endpoint:
//!
//! ```text
//! GET http://127.0.0.1:<pr_port>/accounts/default/balance
//! ```
//!
//! which returns `{"available": <microMinotari>, ...}` per the real
//! handler at `minotari-cli@52a7287a/minotari/src/api/accounts/balance.rs`.
//!
//! Per swe-review C4: this module exists so the funding pre-flight can
//! point at the right wallet. The query is a plain `reqwest::Client::get`
//! with no retry — the caller (`enforce_funding`) decides what to do
//! with transport / 404 errors. When the PR daemon isn't yet up (which
//! is the common case at pre-flight time — the harness spawns PR later,
//! inside the per-mode loop), the call returns a `reqwest` error and the
//! caller logs a warn + skips the Mode 3 funding check. The wiremock
//! test below proves the URL + JSON shape; live integration is exercised
//! by the canonical baseline run.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

const LOG_TARGET: &str = "c::wallet_lifecycle::pr_balance_query";

/// Per-attempt HTTP timeout. Pre-flight is a single-shot probe — no retry,
/// no cadence loop, so a short cap keeps the harness responsive when the
/// operator hasn't pre-warmed the PR daemon.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Subset of the `AccountBalance` JSON shape the funding pre-flight needs.
/// The PR daemon returns more fields (`total`, `locked`, `unconfirmed`,
/// `immature`, ...); we ignore them because the funding check is keyed on
/// the operator-spendable balance only — same convention
/// `WalletGrpcBalanceQuery` uses via `available_balance`.
#[derive(Debug, Deserialize)]
struct BalanceResponse {
    available: u64,
}

/// Pre-flight balance query against the Mode 3 PR daemon's
/// `GET /accounts/{name}/balance` endpoint.
///
/// Cheap and side-effect-free at construction — no network call. The query
/// happens inside [`Self::get_balance`].
pub struct PrBalanceQuery {
    base_url: String,
    account_name: String,
    client: reqwest::Client,
}

impl PrBalanceQuery {
    /// Construct from the PR daemon's HTTP base (e.g.
    /// `"http://127.0.0.1:9146"`) and the account name to query (Mode 3
    /// uses `"default"` — see `crate::wallet_lifecycle::pr_lifecycle::ACCOUNT_NAME`).
    pub fn new(base_url: impl Into<String>, account_name: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            account_name: account_name.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Build the absolute URL the query will hit. Public so the funding
    /// pre-flight can log it on warn-paths.
    pub fn balance_url(&self) -> String {
        format!("{}/accounts/{}/balance", self.base_url, self.account_name)
    }

    /// Issue a single `GET /accounts/<name>/balance` and parse the
    /// `available` field. Returns the spendable microMinotari balance on
    /// 200; bails on transport error, non-2xx, or a JSON shape mismatch.
    pub async fn get_balance(&self) -> anyhow::Result<u64> {
        let url = self.balance_url();
        log::debug!(target: LOG_TARGET, "querying PR daemon balance at {url}");
        let resp = self
            .client
            .get(&url)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET {url} returned HTTP {status}: {body}");
        }
        let parsed: BalanceResponse = resp
            .json()
            .await
            .with_context(|| format!("parsing AccountBalance JSON from {url}"))?;
        Ok(parsed.available)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn balance_url_matches_endpoint_shape() {
        // Per swe-review C4: the URL shape MUST be
        // /accounts/<name>/balance with the correct account name (the
        // unified "default" from N2). Lock this so a future regression
        // can't silently rewrite it.
        let q = PrBalanceQuery::new("http://127.0.0.1:9146", "default");
        assert_eq!(
            q.balance_url(),
            "http://127.0.0.1:9146/accounts/default/balance"
        );
    }

    #[tokio::test]
    async fn get_balance_hits_correct_endpoint_and_parses_available_field() {
        // Per swe-review C4: prove the pre-flight does
        // GET /accounts/default/balance and reads the `available` field
        // from the real handler's JSON shape (see
        // `minotari-cli@52a7287a/minotari/src/api/accounts/balance.rs:118`).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/accounts/default/balance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1_500_000_u64,
                "available": 1_234_567_u64,
                "locked": 0_u64,
                "unconfirmed": 50_000_u64,
                "immature": 0_u64,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let q = PrBalanceQuery::new(server.uri(), "default");
        let bal = q.get_balance().await.expect("balance ok");
        assert_eq!(bal, 1_234_567);
    }

    #[tokio::test]
    async fn get_balance_bails_on_404_account_not_found() {
        // If the account name on PP and the PR daemon ever drift apart
        // again (the N2 regression), this is the exact failure operators
        // will see — a 404 from the PR daemon. Surface the status so the
        // pre-flight log explains the cause.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/accounts/missing/balance"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let q = PrBalanceQuery::new(server.uri(), "missing");
        let err = q.get_balance().await.expect_err("404 must surface");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("404"),
            "error must name the HTTP status: {msg}"
        );
    }

    #[tokio::test]
    async fn get_balance_bails_on_connection_refused() {
        // When the PR daemon isn't yet spawned (the common pre-flight
        // case — start_external_services runs later, inside the per-mode
        // loop), the pre-flight gets a transport error. The caller in
        // `enforce_funding` logs a warn + skips the Pp check.
        let q = PrBalanceQuery::new("http://127.0.0.1:1", "default");
        let err = q.get_balance().await.expect_err("connection refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("GET http://127.0.0.1:1/accounts/default/balance"),
            "error must surface the URL so operators can locate the misconfiguration: {msg}",
        );
    }
}
