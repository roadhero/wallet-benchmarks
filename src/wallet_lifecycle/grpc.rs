//! gRPC client connector for Mode 1's wallet lifecycle.
//!
//! Wraps the published [`minotari_app_grpc::tari_rpc::wallet_client::WalletClient`]
//! with a connect-time retry loop per `analysis/DESIGN.md §Mode 1 step 1` ("the
//! tonic Channel below uses an exponential backoff on connect only, which is
//! allowed; this is not retry on submit"). AC-30/31/32 forbid submit-side
//! retry, NOT connection-establishment retry — race-tolerant connect is
//! explicitly called out as in-scope.
//!
//! `WalletClient<Channel>` from the published `minotari_app_grpc = "5.3.1"`
//! crate is the surface scenarios call into. Construction here returns the
//! client; the trait's reactive surface (`GetBalance`, `GetState`, `Transfer`,
//! etc.) is invoked from the [`crate::modes`] module.

use std::time::{Duration, Instant};

use minotari_app_grpc::tari_rpc::wallet_client::WalletClient;
use tonic::transport::Channel;
use url::Url;

const LOG_TARGET: &str = "c::wallet_lifecycle::grpc";

/// Initial backoff delay between connect attempts.
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
/// Backoff multiplier per failed attempt.
const BACKOFF_MULTIPLIER: u32 = 2;
/// Cap on backoff between attempts — keeps the retry loop responsive on a
/// console wallet that is *almost* ready.
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// Connect to a `minotari_console_wallet` gRPC endpoint with bounded
/// connect-time retry.
///
/// `addr` is a URL whose `host`+`port` denote the wallet's gRPC listener —
/// typically `http://127.0.0.1:<dynamic-port>` constructed by
/// [`super::console_wallet`]. `deadline` is the wall-clock budget; the
/// connect loop returns the first `Ok` from [`WalletClient::connect`] or
/// the most recent error after the deadline passes.
///
/// **Why this is AC-compliant:** `DESIGN.md §Mode 1 step 1` explicitly
/// permits "race-tolerant" connect-time backoff. AC-30/31/32 forbid
/// retry/backoff/throttle in `src/scenarios/s4.rs` (submit-side) — this
/// file is enforcement-excluded.
pub async fn connect_with_retry(
    addr: &Url,
    deadline: Duration,
) -> anyhow::Result<WalletClient<Channel>> {
    let endpoint_str = addr.as_str().to_string();
    let started = Instant::now();
    let mut backoff = INITIAL_BACKOFF;
    let mut last_err: Option<anyhow::Error> = None;
    log::debug!(
        target: LOG_TARGET,
        "connecting to wallet gRPC at {endpoint_str} with deadline {deadline:?}",
    );

    while started.elapsed() < deadline {
        match WalletClient::connect(endpoint_str.clone()).await {
            Ok(client) => {
                log::debug!(target: LOG_TARGET, "wallet gRPC connect to {endpoint_str} succeeded");
                return Ok(client);
            }
            Err(e) => {
                log::debug!(
                    target: LOG_TARGET,
                    "wallet gRPC connect to {endpoint_str} failed (backoff={backoff:?}): {e}",
                );
                last_err = Some(anyhow::Error::new(e));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * BACKOFF_MULTIPLIER).min(MAX_BACKOFF);
            }
        }
    }

    let err = last_err.unwrap_or_else(|| {
        anyhow::anyhow!("connect deadline elapsed before any attempt completed")
    });
    Err(err.context(format!(
        "failed to connect to wallet gRPC at {endpoint_str} within {deadline:?}"
    )))
}
