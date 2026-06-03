//! Hand-rolled `reqwest`-backed client for the `minotari_payment_processor`
//! (PP) HTTP API.
//!
//! Per `analysis/specs/MODE_3_REWORK_SPEC.md §6`, Mode 3 talks to PP through
//! a small set of REST endpoints rather than a code-generated client; the
//! upstream OpenAPI surface is stable and the harness only needs four call
//! sites, so the hand-rolled types are cheaper than introducing an
//! openapi-generator build step. Request/response shapes mirror
//! `vendor/minotari_payment_processor/minotari_payment_processor/src/api/{payments,events,version}.rs`
//! and `src/db/payment.rs` at submodule commit `f0572c9`.
//!
//! **No retry / backoff inside the client.** Every method surfaces transport
//! and HTTP errors as `Err(anyhow::Error)` so the calling scenario records the
//! failure raw — same posture as the rest of the harness per AC-30/31/32.
//! `wait_ready` does poll on a fixed cadence but only for the startup
//! readiness probe (§4); ordinary submission/poll calls return immediately.

use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const LOG_TARGET: &str = "c::pp_http_client";

/// Constant readiness-probe deadline. Per `MODE_3_REWORK_SPEC.md §4`, PP
/// startup is ~2-3s in practice; 30s gives ~10x headroom for slow disks /
/// cold caches.
pub const READINESS_DEADLINE: Duration = Duration::from_secs(30);

/// Constant readiness-probe inter-attempt backoff (per `SPEC §4`).
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Per-attempt timeout for the readiness probe's HTTP call.
const READINESS_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// Single-item bulk payment item — one recipient + amount + idempotency key.
///
/// `client_id` is the idempotency key: PP rejects duplicate
/// `(account_name, client_id)` pairs at the DB layer so the harness must
/// assign a unique value per submitted item. `payment_id` is an optional
/// caller-supplied memo (PP echoes it back unchanged).
///
/// `amount` is signed `i64` in upstream — see
/// `vendor/.../api/payments.rs:38` — and the harness mirrors that. The
/// caller must convert from the harness's `u64` microMinotari with a
/// `try_from` so a >`i64::MAX` value surfaces as a build-side error rather
/// than a wrap-around.
#[derive(Debug, Clone, Serialize)]
pub struct BulkPaymentItem {
    /// Idempotency key. Must be unique per `(account_name, client_id)`.
    pub client_id: String,
    /// Recipient address as a base58 Tari address string.
    pub recipient_address: String,
    /// Amount in microMinotari, signed i64 per upstream.
    pub amount: i64,
    /// Optional caller-supplied payment memo. None for harness use.
    pub payment_id: Option<String>,
}

/// Request body for `POST /v1/payment-batches` — a batch of up to
/// `MAX_BATCH_SIZE` items (PP enforces 100 per `vendor/.../src/lib.rs`).
#[derive(Debug, Clone, Serialize)]
pub struct BulkPaymentRequest {
    /// PP account name as configured under `[mode_3.accounts]`. v1 always
    /// uses the literal `"bench"`.
    pub account_name: String,
    /// Items in this batch.
    pub items: Vec<BulkPaymentItem>,
}

/// Response body returned by `POST /v1/payment-batches`. Mirrors
/// `BulkPaymentResponse` at `vendor/.../api/payments.rs:48`.
#[derive(Debug, Clone, Deserialize)]
pub struct BulkPaymentResponse {
    /// Batch identifier — the harness reuses this as the stand-in for an
    /// on-chain txid in `TxRecord.txid` until the signer succeeds.
    pub batch_id: String,
    /// Account name as supplied in the request, echoed back.
    pub account_name: String,
    /// Free-form batch status string (PP's enum at submission time).
    pub status: String,
    /// One `PaymentResponse` per item in the request.
    pub payments: Vec<PaymentResponse>,
}

/// PP payment status enum. Verbatim mirror of
/// `vendor/minotari_payment_processor/minotari_payment_processor/src/db/payment.rs:14-22`:
///
/// ```text
/// #[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
/// #[serde(rename_all = "SCREAMING_SNAKE_CASE")]
/// pub enum PaymentStatus { Received, Batched, Confirmed, Failed, Cancelled }
/// ```
///
/// Per `MODE_3_REWORK_SPEC.md §6` and the §13 implementation-time
/// verification note: the serde repr is read directly from the upstream
/// source — not inferred. Spec-BLOCKER #3 is therefore *closed at write
/// time* by source inspection.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PaymentStatus {
    /// Payment accepted by the API, not yet batched.
    Received,
    /// Payment grouped into a batch by `batch_creator`.
    Batched,
    /// Mined and confirmed on-chain.
    Confirmed,
    /// Failed somewhere in the construct/sign/broadcast/confirm pipeline.
    Failed,
    /// Cancelled via `POST /v1/payments/{id}/cancel`.
    Cancelled,
}

impl PaymentStatus {
    /// `true` when no further state transition is expected. The shutdown
    /// poll loop (per `MODE_3_REWORK_SPEC.md §9`) treats these as the
    /// stop conditions for individual payments.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Confirmed | Self::Failed | Self::Cancelled)
    }
}

/// Per-payment response carried inside [`BulkPaymentResponse`] and returned
/// individually by `GET /v1/payments/{id}`. Mirrors `PaymentResponse` at
/// `vendor/.../api/payments.rs:54-78`. All `Option` fields match upstream's
/// `#[serde(skip_serializing_if = "Option::is_none")]` semantics — missing
/// keys deserialize as `None`.
#[derive(Debug, Clone, Deserialize)]
pub struct PaymentResponse {
    /// PP-assigned payment id (UUID-shaped string).
    pub payment_id: String,
    /// Current status. See [`PaymentStatus::is_terminal`].
    pub status: PaymentStatus,
    /// Caller-supplied idempotency key, echoed back.
    pub client_id: String,
    /// Account this payment belongs to.
    pub account_name: String,
    /// Recipient address as the harness supplied it.
    pub recipient_address: String,
    /// Amount in microMinotari, signed i64 per upstream.
    pub amount: i64,
    /// Payment-reference (assigned post-broadcast).
    #[serde(default)]
    pub payref: Option<String>,
    /// Reason set when status is `Failed`.
    #[serde(default)]
    pub failure_reason: Option<String>,
    /// Mined block height; set once `Confirmed`.
    #[serde(default)]
    pub mined_height: Option<i64>,
    /// Mined header hash hex; set once `Confirmed`.
    #[serde(default)]
    pub mined_header_hash: Option<String>,
    /// Mined block timestamp; set once `Confirmed`.
    #[serde(default)]
    pub mined_timestamp: Option<i64>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last-update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// `GET /health/version` response body. Mirrors `ServiceVersion` at
/// `vendor/.../api/version.rs:7`.
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceVersion {
    /// `CARGO_PKG_VERSION` of the running PP binary.
    pub version: String,
}

/// Query-string filters for `GET /v1/events`. Mirrors `GetEventsQuery` at
/// `vendor/.../api/events.rs:14-32`.
#[derive(Debug, Clone, Default)]
pub struct EventFilters {
    /// Optional account-name filter.
    pub account_name: Option<String>,
    /// Optional payment-id filter.
    pub payment_id: Option<String>,
    /// Optional batch-id filter.
    pub batch_id: Option<String>,
    /// Optional event-type filter.
    pub event_type: Option<String>,
    /// Inclusive lower bound on `created_at`.
    pub from: Option<DateTime<Utc>>,
    /// Inclusive upper bound on `created_at`.
    pub to: Option<DateTime<Utc>>,
    /// Page size cap. PP's default is 50.
    pub limit: Option<i64>,
    /// Page offset.
    pub offset: Option<i64>,
}

/// One row from `GET /v1/events`. Mirrors `Event` at
/// `vendor/.../db/event.rs:29-40`.
#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    /// Primary key.
    pub id: i64,
    /// Event-type variant name as stored in sqlite (one of
    /// `BatchCreated`, `BatchSigned`, `TransactionBroadcast`,
    /// `TransactionMempoolDetected`, `TransactionConfirmed`,
    /// `TransactionReorged`, `TransactionBroadcastFailed`,
    /// `PaymentReceived`, `PaymentCancelled`, `BatchFailed`).
    pub event_type: String,
    /// Free-form description.
    pub description: String,
    /// Optional metadata as a JSON-encoded string.
    #[serde(default)]
    pub metadata_json: Option<String>,
    /// Account this event belongs to.
    pub account_name: String,
    /// Optional payment id (event-type-dependent).
    #[serde(default)]
    pub payment_id: Option<String>,
    /// Optional batch id (event-type-dependent).
    #[serde(default)]
    pub batch_id: Option<String>,
    /// Insert timestamp.
    pub created_at: DateTime<Utc>,
}

/// Response body returned by `GET /v1/events`. Mirrors `EventListResponse`
/// at `vendor/.../api/events.rs:36-39`.
#[derive(Debug, Clone, Deserialize)]
pub struct EventListResponse {
    /// Page contents.
    pub events: Vec<Event>,
    /// Total matching count across all pages (for paginated UIs).
    pub total_count: i64,
}

/// Hand-rolled HTTP client for the PP REST API.
///
/// Holds a single `reqwest::Client` reused across calls (multiplexes HTTP/2
/// connections, shares the connection pool) — same pattern as
/// [`crate::broadcast::Broadcaster`]. Cloned across S4's concurrent tasks
/// via `Arc<PpHttpClient>` per `MODE_3_REWORK_SPEC.md §11`.
pub struct PpHttpClient {
    client: reqwest::Client,
    base_url: String,
}

impl PpHttpClient {
    /// Construct a new client targeting `base_url` (e.g.
    /// `"http://127.0.0.1:9145"`). Builds one `reqwest::Client`; calls
    /// reuse it.
    pub fn new(base_url: String) -> Self {
        // Default reqwest::Client is fine — rustls-tls is gated by the
        // crate features and the harness only ever talks to 127.0.0.1 in
        // Mode 3, so the TLS feature isn't exercised here.
        Self {
            client: reqwest::Client::new(),
            base_url,
        }
    }

    /// Base URL the client was constructed with (read-only).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /health/version` — single-shot.
    pub async fn health_version(&self) -> anyhow::Result<ServiceVersion> {
        let url = format!("{}/health/version", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("GET {url} returned HTTP {status}");
        }
        resp.json::<ServiceVersion>()
            .await
            .with_context(|| format!("parsing ServiceVersion JSON from {url}"))
    }

    /// Poll `GET /health/version` until it returns 200 OK or `deadline`
    /// elapses. Returns the parsed [`ServiceVersion`] on success.
    ///
    /// Per `MODE_3_REWORK_SPEC.md §4`:
    /// * 200 OK → ready.
    /// * Connection-refused / timeout → keep polling (PP hasn't bound).
    /// * 4xx/5xx → log and keep polling (PP up but workers initialising).
    /// * Other reqwest error → fail-fast.
    /// * Deadline exceeded → bail with attempt count.
    pub async fn wait_ready(&self, deadline: Duration) -> anyhow::Result<ServiceVersion> {
        let start = Instant::now();
        let mut attempts = 0u32;
        let url = format!("{}/health/version", self.base_url);
        loop {
            attempts += 1;
            if start.elapsed() >= deadline {
                anyhow::bail!(
                    "PP {url} did not return 200 within {:?} ({attempts} attempts)",
                    deadline,
                );
            }
            match self
                .client
                .get(&url)
                .timeout(READINESS_ATTEMPT_TIMEOUT)
                .send()
                .await
            {
                Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
                    let version = resp
                        .json::<ServiceVersion>()
                        .await
                        .with_context(|| format!("parsing ServiceVersion JSON from {url}"))?;
                    log::info!(
                        target: LOG_TARGET,
                        "PP ready at {url} (version={}, attempts={attempts})",
                        version.version,
                    );
                    return Ok(version);
                }
                Ok(resp) => {
                    log::debug!(
                        target: LOG_TARGET,
                        "{url} returned HTTP {}; retrying in {:?}",
                        resp.status(),
                        READINESS_POLL_INTERVAL,
                    );
                }
                Err(e) if e.is_connect() || e.is_timeout() => {
                    log::debug!(
                        target: LOG_TARGET,
                        "{url} not yet bound ({e}); retrying in {:?}",
                        READINESS_POLL_INTERVAL,
                    );
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("unexpected error polling {url}"));
                }
            }
            tokio::time::sleep(READINESS_POLL_INTERVAL).await;
        }
    }

    /// `POST /v1/payment-batches` — submit a batch of up to 100 items.
    ///
    /// No retry: any error (transport, 4xx, 5xx) is surfaced to the caller
    /// so the scenario records the failure raw.
    pub async fn submit_batch(
        &self,
        account_name: &str,
        items: Vec<BulkPaymentItem>,
    ) -> anyhow::Result<BulkPaymentResponse> {
        let url = format!("{}/v1/payment-batches", self.base_url);
        let body = BulkPaymentRequest {
            account_name: account_name.to_string(),
            items,
        };
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("POST {url} returned HTTP {status}: {body}");
        }
        resp.json::<BulkPaymentResponse>()
            .await
            .with_context(|| format!("parsing BulkPaymentResponse JSON from {url}"))
    }

    /// `GET /v1/payments/{payment_id}` — fetch the current state of one
    /// payment. No retry: caller drives any polling cadence.
    pub async fn poll_payment(&self, payment_id: &str) -> anyhow::Result<PaymentResponse> {
        let url = format!("{}/v1/payments/{}", self.base_url, payment_id);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET {url} returned HTTP {status}: {body}");
        }
        resp.json::<PaymentResponse>()
            .await
            .with_context(|| format!("parsing PaymentResponse JSON from {url}"))
    }

    /// `GET /v1/events` with optional filters. No retry: caller drives any
    /// polling cadence.
    pub async fn stream_events(&self, filters: EventFilters) -> anyhow::Result<EventListResponse> {
        let url = format!("{}/v1/events", self.base_url);
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(v) = filters.account_name {
            query.push(("account_name", v));
        }
        if let Some(v) = filters.payment_id {
            query.push(("payment_id", v));
        }
        if let Some(v) = filters.batch_id {
            query.push(("batch_id", v));
        }
        if let Some(v) = filters.event_type {
            query.push(("event_type", v));
        }
        if let Some(v) = filters.from {
            query.push(("from", v.to_rfc3339()));
        }
        if let Some(v) = filters.to {
            query.push(("to", v.to_rfc3339()));
        }
        if let Some(v) = filters.limit {
            query.push(("limit", v.to_string()));
        }
        if let Some(v) = filters.offset {
            query.push(("offset", v.to_string()));
        }
        let resp = self
            .client
            .get(&url)
            .query(&query)
            .send()
            .await
            .with_context(|| format!("GET {url} (filters)"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET {url} returned HTTP {status}: {body}");
        }
        resp.json::<EventListResponse>()
            .await
            .with_context(|| format!("parsing EventListResponse JSON from {url}"))
    }
}

#[cfg(test)]
mod tests {
    // TODO(swe-test): populate per MODE_3_REWORK_SPEC.md §13 — wiremock-backed
    // request/response coverage for the seven endpoints. Test list (verbatim
    // from spec):
    //   - health_version_200_returns_parsed_version
    //   - wait_ready_retries_on_connection_refused_then_succeeds_on_200
    //   - wait_ready_fails_on_total_timeout
    //   - wait_ready_fails_on_dns_error_immediately
    //   - submit_batch_serializes_request_correctly
    //   - submit_batch_parses_202_response
    //   - submit_batch_returns_error_on_400_batch_too_large
    //   - submit_batch_returns_error_on_503
    //   - poll_payment_404_returns_typed_error
    //   - poll_payment_200_parses_status_enum
    //   - stream_events_with_filters_builds_correct_query_string
}
