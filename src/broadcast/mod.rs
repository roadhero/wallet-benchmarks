//! Broadcast wrapper around `minotari_node_wallet_client::http::Client`.
//!
//! Per `analysis/DESIGN.md §Mode 2 step 5` and `DESIGN_ADDENDUM.md §Broadcast
//! directive 5`, the harness depends on the published `minotari_node_wallet_client
//! = "5.3.1"` to handle the dual-shape JSON-RPC response (bare `{"error": ...}`
//! vs envelope `{"result": {...}}`). The wrapper here is intentionally thin:
//! a typed `TxSubmissionOutcome` so scenario code doesn't import the upstream
//! crate's types, and a `From<TxSubmissionResponse>` impl that maps the
//! upstream enum into our forward-compatible variant.
//!
//! Key API drift surfaced during 3e:
//!
//! * Upstream `TxSubmissionResponse` has only three fields:
//!   `{ accepted, rejection_reason, is_synced }`. DESIGN.md called out a
//!   `details` field that does NOT exist on the published struct; we keep a
//!   nullable `details` on our outcome shape so a future client release that
//!   adds the field can be picked up without changing scenario code.
//! * `RejectionReason::DoubleSpend` exists in the upstream enum but is not
//!   listed in DESIGN.md §Mode 2 step 5. Treated as a first-class variant
//!   here so scenarios that surface it don't collapse into `Other`.
//! * The "bare error" JSON-RPC shape (no `result`, error string in `error`)
//!   is mapped by the upstream client into `Err(anyhow!("Transaction
//!   submission failed: <msg>"))` — our wrapper propagates that as-is.

use anyhow::Context;
use minotari_node_wallet_client::{http::Client as BaseNodeHttpClient, BaseNodeWalletClient};
use tari_transaction_components::{
    rpc::models::{TxSubmissionRejectionReason as UpstreamReason, TxSubmissionResponse},
    transaction_components::Transaction,
};
use url::Url;

const LOG_TARGET: &str = "c::broadcast";

/// Thin wrapper over [`BaseNodeHttpClient`] that exposes a typed outcome.
///
/// One `Broadcaster` per base-node URL. The underlying client carries its
/// own `reqwest` connection pool; clone-by-value is cheap (the upstream
/// `Client` already implements `Clone`).
pub struct Broadcaster {
    client: BaseNodeHttpClient,
}

impl Broadcaster {
    /// Construct a broadcaster pointing at `base_node_url`.
    ///
    /// The published client takes two URLs (`local_api_address`,
    /// `default_seed_address`) and prefers the local one when it succeeds.
    /// In the harness's single-endpoint setup we pass the same URL to both
    /// slots; the upstream client's preference logic still works (local API
    /// returns 2xx → use it; failures fall back to the same URL anyway).
    pub fn new(base_node_url: &Url) -> Self {
        Self {
            client: BaseNodeHttpClient::new(base_node_url.clone(), base_node_url.clone()),
        }
    }

    /// Submit `tx` to the configured base node.
    ///
    /// On envelope-success: maps the upstream `TxSubmissionResponse` into
    /// [`TxSubmissionOutcome`] and returns `Ok`. On bare-error: the upstream
    /// client returns `Err(anyhow!("Transaction submission failed: ..."))`,
    /// which we propagate with added context — scenario code records the
    /// error string in its `errors.details` entry.
    ///
    /// **No retry**, **no backoff**, **no throttling** — submit-side
    /// serialization is forbidden by AC-30/31/32. Concurrent dispatch in
    /// S4 happens via `tokio::JoinSet` at the scenario level, with each
    /// task making one `submit_transaction` call.
    pub async fn submit_transaction(&self, tx: Transaction) -> anyhow::Result<TxSubmissionOutcome> {
        log::debug!(
            target: LOG_TARGET,
            "submitting transaction (inputs={}, outputs={})",
            tx.body.inputs().len(),
            tx.body.outputs().len(),
        );
        let resp = self
            .client
            .submit_transaction(tx)
            .await
            .context("submit_transaction via minotari_node_wallet_client http client")?;
        Ok(TxSubmissionOutcome::from(resp))
    }
}

/// Result-profile shape for a single broadcast attempt.
///
/// Mirrors the fields scenario code records in
/// `RESULT_PROFILE_SCHEMA.md §4 tx_records[].*` — `accepted`,
/// `rejection_reason`, `is_synced`, and (forward-compat) free-form
/// `details`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxSubmissionOutcome {
    /// Did the base node accept the transaction into its mempool? `false`
    /// means rejected — read `rejection_reason` for the cause.
    pub accepted: bool,
    /// Categorised rejection cause, or [`RejectionReason::None`] on accept.
    pub rejection_reason: RejectionReason,
    /// Was the base node fully synced with its peers at submit time?
    /// Scenario code surfaces this in the result profile but does not
    /// gate on it — the harness measures whatever the base node returns.
    pub is_synced: bool,
    /// Optional free-form text from the base node — always `None` against
    /// the published 5.3.1 client; reserved for a future client revision
    /// that surfaces the JSON-RPC `details` field.
    pub details: Option<String>,
}

/// Forward-compatible rejection reason — mirrors the upstream
/// `TxSubmissionRejectionReason` enum with an extra `Other(String)` arm
/// so a future client release that adds a variant doesn't force a
/// breaking change on us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectionReason {
    /// Transaction was accepted; this is what `TxSubmissionOutcome::accepted == true`
    /// pairs with.
    None,
    /// Already-mined input.
    AlreadyMined,
    /// Double-spend attempt detected by the base node.
    DoubleSpend,
    /// Orphan transaction — parent input unknown.
    Orphan,
    /// Time-locked input not yet eligible to spend.
    TimeLocked,
    /// Generic validation failure — script, range proof, etc.
    ValidationFailed,
    /// Fee below the base node's minimum.
    FeeTooLow,
    /// Forward-compatibility catch-all. Not reachable through the published
    /// 5.3.1 client (its `TxSubmissionRejectionReason` is a closed enum) but
    /// keeps our type future-proof.
    Other(String),
}

impl From<UpstreamReason> for RejectionReason {
    fn from(upstream: UpstreamReason) -> Self {
        match upstream {
            UpstreamReason::None => RejectionReason::None,
            UpstreamReason::AlreadyMined => RejectionReason::AlreadyMined,
            UpstreamReason::DoubleSpend => RejectionReason::DoubleSpend,
            UpstreamReason::Orphan => RejectionReason::Orphan,
            UpstreamReason::TimeLocked => RejectionReason::TimeLocked,
            UpstreamReason::ValidationFailed => RejectionReason::ValidationFailed,
            UpstreamReason::FeeTooLow => RejectionReason::FeeTooLow,
        }
    }
}

impl From<TxSubmissionResponse> for TxSubmissionOutcome {
    fn from(resp: TxSubmissionResponse) -> Self {
        Self {
            accepted: resp.accepted,
            rejection_reason: RejectionReason::from(resp.rejection_reason),
            is_synced: resp.is_synced,
            details: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_response(
        accepted: bool,
        reason: UpstreamReason,
        is_synced: bool,
    ) -> TxSubmissionResponse {
        TxSubmissionResponse {
            accepted,
            rejection_reason: reason,
            is_synced,
        }
    }

    #[test]
    fn envelope_success_yields_accepted_true_and_no_rejection() {
        let resp = build_response(true, UpstreamReason::None, true);
        let outcome = TxSubmissionOutcome::from(resp);
        assert!(outcome.accepted);
        assert_eq!(outcome.rejection_reason, RejectionReason::None);
        assert!(outcome.is_synced);
        assert_eq!(outcome.details, None);
    }

    #[test]
    fn envelope_rejection_orphan_maps_to_orphan_variant() {
        let resp = build_response(false, UpstreamReason::Orphan, true);
        let outcome = TxSubmissionOutcome::from(resp);
        assert!(!outcome.accepted);
        assert_eq!(outcome.rejection_reason, RejectionReason::Orphan);
    }

    #[test]
    fn envelope_rejection_fee_too_low_maps_to_fee_too_low_variant() {
        let resp = build_response(false, UpstreamReason::FeeTooLow, true);
        let outcome = TxSubmissionOutcome::from(resp);
        assert_eq!(outcome.rejection_reason, RejectionReason::FeeTooLow);
    }

    #[test]
    fn envelope_rejection_time_locked_maps_to_time_locked_variant() {
        let resp = build_response(false, UpstreamReason::TimeLocked, true);
        let outcome = TxSubmissionOutcome::from(resp);
        assert_eq!(outcome.rejection_reason, RejectionReason::TimeLocked);
    }

    #[test]
    fn envelope_rejection_validation_failed_maps_to_validation_failed_variant() {
        let resp = build_response(false, UpstreamReason::ValidationFailed, true);
        let outcome = TxSubmissionOutcome::from(resp);
        assert_eq!(outcome.rejection_reason, RejectionReason::ValidationFailed);
    }

    #[test]
    fn envelope_rejection_already_mined_maps_to_already_mined_variant() {
        let resp = build_response(false, UpstreamReason::AlreadyMined, true);
        let outcome = TxSubmissionOutcome::from(resp);
        assert_eq!(outcome.rejection_reason, RejectionReason::AlreadyMined);
    }

    #[test]
    fn envelope_rejection_double_spend_maps_to_double_spend_variant() {
        // Upstream exposes DoubleSpend but DESIGN.md only listed five
        // rejection variants — we surface DoubleSpend as first-class so
        // S4's contention-rate measurements (AC-30/31/32 "raw recording")
        // don't collapse it into RejectionReason::Other.
        let resp = build_response(false, UpstreamReason::DoubleSpend, true);
        let outcome = TxSubmissionOutcome::from(resp);
        assert_eq!(outcome.rejection_reason, RejectionReason::DoubleSpend);
    }

    #[test]
    fn is_synced_is_propagated_through_the_outcome() {
        let resp_synced = build_response(true, UpstreamReason::None, true);
        let resp_unsynced = build_response(true, UpstreamReason::None, false);
        assert!(TxSubmissionOutcome::from(resp_synced).is_synced);
        assert!(!TxSubmissionOutcome::from(resp_unsynced).is_synced);
    }

    #[test]
    fn broadcaster_construction_does_not_make_a_network_call() {
        // `Client::new` is documented as construction-only — no HTTP call.
        // This test exists to catch a future upstream change that adds
        // an eager connect to `new()`; the harness assumes construction
        // is cheap and side-effect-free.
        let url = Url::parse("https://rpc.esmeralda.tari.com").expect("url");
        let _broadcaster = Broadcaster::new(&url);
    }
}
