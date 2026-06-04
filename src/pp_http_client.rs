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
    /// PP account name as configured under `[mode_3.accounts]`. Currently
    /// the literal `"default"` — matches the wallet name hardcoded by
    /// minotari-cli's `init_wallet.rs:121` (`friendly_name.unwrap_or("default")`),
    /// which import-view-key does not override.
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
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// JSON skeleton for a `PaymentResponse`. Matches the upstream shape
    /// citing `vendor/.../api/payments.rs:54-78`. Variable bits supplied by
    /// the caller; the timestamp fields use a fixed RFC3339 string so the
    /// fixture is deterministic.
    fn payment_response_json(payment_id: &str, status: &str, client_id: &str) -> serde_json::Value {
        json!({
            "payment_id": payment_id,
            "status": status,
            "client_id": client_id,
            "account_name": "bench",
            "recipient_address": "tari://esmeralda/recipient_addr",
            "amount": 1000_i64,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        })
    }

    #[tokio::test]
    async fn health_version_200_returns_parsed_version() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health/version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "1.2.3",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = PpHttpClient::new(server.uri());
        let v = client.health_version().await.expect("health_version ok");
        assert_eq!(v.version, "1.2.3");
    }

    #[tokio::test]
    async fn wait_ready_retries_on_connection_refused_then_succeeds_on_200() {
        // Two-stage scenario: first scope a Mock that returns 503 a
        // bounded number of times, then a second Mock that returns 200.
        // wiremock keeps the most-recent matching mount in priority order,
        // so the 200 mount supersedes the 503 once both are installed.
        let server = MockServer::start().await;
        // First: 503 a few times (simulating worker init), then 200.
        Mock::given(method("GET"))
            .and(path("/health/version"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(3)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/health/version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": "1.2.3" })))
            .mount(&server)
            .await;
        let client = PpHttpClient::new(server.uri());
        let v = client
            .wait_ready(Duration::from_secs(5))
            .await
            .expect("wait_ready ok after retries");
        assert_eq!(v.version, "1.2.3");
    }

    #[tokio::test]
    async fn wait_ready_fails_on_total_timeout() {
        // The server is bound but every probe returns 500, which the loop
        // treats as "PP up but workers initialising" — it keeps polling
        // until the deadline. A short deadline (300ms) guarantees the
        // loop exits with the deadline-exceeded bail, not a transport
        // error.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health/version"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = PpHttpClient::new(server.uri());
        let err = client
            .wait_ready(Duration::from_millis(300))
            .await
            .expect_err("must time out");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("did not return 200"),
            "error must name the deadline-exceeded bail: {msg}",
        );
    }

    #[tokio::test]
    async fn wait_ready_fails_on_dns_error_immediately() {
        // RFC 2606 reserves `.invalid` for guaranteed-fail DNS — `reqwest`
        // surfaces this as a transport error with `is_connect() == false`
        // and `is_timeout() == false`, hitting the wait_ready "Other reqwest
        // error" branch that fails fast.
        let client = PpHttpClient::new("http://nonexistent.invalid".to_string());
        let err = client
            .wait_ready(Duration::from_secs(10))
            .await
            .expect_err("DNS-fail must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unexpected error polling") || msg.contains("nonexistent.invalid"),
            "error must surface the DNS/transport failure: {msg}",
        );
    }

    #[tokio::test]
    async fn submit_batch_serializes_request_correctly() {
        let server = MockServer::start().await;
        let expected_body = json!({
            "account_name": "bench",
            "items": [
                {
                    "client_id": "bench-tx-0-0",
                    "recipient_address": "tari://esmeralda/recipient_addr",
                    "amount": 1000_i64,
                    "payment_id": null,
                }
            ],
        });
        Mock::given(method("POST"))
            .and(path("/v1/payment-batches"))
            .and(header("content-type", "application/json"))
            .and(body_json(&expected_body))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "batch_id": "batch-1",
                "account_name": "bench",
                "status": "RECEIVED",
                "payments": [payment_response_json("pay-1", "RECEIVED", "bench-tx-0-0")],
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = PpHttpClient::new(server.uri());
        let items = vec![BulkPaymentItem {
            client_id: "bench-tx-0-0".to_string(),
            recipient_address: "tari://esmeralda/recipient_addr".to_string(),
            amount: 1000,
            payment_id: None,
        }];
        let resp = client
            .submit_batch("bench", items)
            .await
            .expect("submit ok");
        assert_eq!(resp.batch_id, "batch-1");
        assert_eq!(resp.payments.len(), 1);
    }

    #[tokio::test]
    async fn submit_batch_parses_202_response() {
        // Upstream may answer 202 Accepted while it stages the batch; the
        // client treats every 2xx as success and parses the body.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/payment-batches"))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                "batch_id": "batch-202",
                "account_name": "bench",
                "status": "RECEIVED",
                "payments": [payment_response_json("pay-202", "RECEIVED", "c1")],
            })))
            .mount(&server)
            .await;
        let client = PpHttpClient::new(server.uri());
        let resp = client
            .submit_batch(
                "bench",
                vec![BulkPaymentItem {
                    client_id: "c1".to_string(),
                    recipient_address: "addr".to_string(),
                    amount: 1,
                    payment_id: None,
                }],
            )
            .await
            .expect("202 must parse");
        assert_eq!(resp.batch_id, "batch-202");
        assert_eq!(resp.payments[0].status, PaymentStatus::Received);
    }

    #[tokio::test]
    async fn submit_batch_returns_error_on_400_batch_too_large() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/payment-batches"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string("batch size 101 exceeds MAX_BATCH_SIZE"),
            )
            .mount(&server)
            .await;
        let client = PpHttpClient::new(server.uri());
        let err = client
            .submit_batch(
                "bench",
                vec![BulkPaymentItem {
                    client_id: "c1".to_string(),
                    recipient_address: "addr".to_string(),
                    amount: 1,
                    payment_id: None,
                }],
            )
            .await
            .expect_err("400 must surface as Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("400") && msg.contains("MAX_BATCH_SIZE"),
            "error must carry the upstream body and status: {msg}",
        );
    }

    #[tokio::test]
    async fn submit_batch_returns_error_on_503() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/payment-batches"))
            .respond_with(ResponseTemplate::new(503).set_body_string("service unavailable"))
            .mount(&server)
            .await;
        let client = PpHttpClient::new(server.uri());
        let err = client
            .submit_batch(
                "bench",
                vec![BulkPaymentItem {
                    client_id: "c1".to_string(),
                    recipient_address: "addr".to_string(),
                    amount: 1,
                    payment_id: None,
                }],
            )
            .await
            .expect_err("503 must surface as Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("503"),
            "error must carry the HTTP 503 status: {msg}",
        );
    }

    #[tokio::test]
    async fn poll_payment_404_returns_typed_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/payments/missing-id"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let client = PpHttpClient::new(server.uri());
        let err = client
            .poll_payment("missing-id")
            .await
            .expect_err("404 must surface as Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("404"),
            "error must carry the HTTP 404 status: {msg}",
        );
    }

    #[tokio::test]
    async fn poll_payment_200_parses_status_enum() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/payments/pay-abc"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(payment_response_json(
                    "pay-abc",
                    "CONFIRMED",
                    "c-abc",
                )),
            )
            .mount(&server)
            .await;
        let client = PpHttpClient::new(server.uri());
        let resp = client.poll_payment("pay-abc").await.expect("200 parses");
        assert_eq!(resp.payment_id, "pay-abc");
        assert_eq!(resp.status, PaymentStatus::Confirmed);
        assert!(resp.status.is_terminal(), "Confirmed is a terminal state");
    }

    #[tokio::test]
    async fn stream_events_with_filters_builds_correct_query_string() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/events"))
            .and(query_param("account_name", "bench"))
            .and(query_param("payment_id", "pay-1"))
            .and(query_param("batch_id", "batch-1"))
            .and(query_param("event_type", "PaymentReceived"))
            .and(query_param("limit", "25"))
            .and(query_param("offset", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "events": [],
                "total_count": 0,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = PpHttpClient::new(server.uri());
        let filters = EventFilters {
            account_name: Some("bench".to_string()),
            payment_id: Some("pay-1".to_string()),
            batch_id: Some("batch-1".to_string()),
            event_type: Some("PaymentReceived".to_string()),
            from: None,
            to: None,
            limit: Some(25),
            offset: Some(5),
        };
        let resp = client
            .stream_events(filters)
            .await
            .expect("filtered events ok");
        assert_eq!(resp.total_count, 0);
        assert!(resp.events.is_empty());
    }
}
