//! Mode abstractions — the three benchmark dispatches.
//!
//! Per `analysis/DESIGN.md §Module boundaries and data flow`, scenario code is
//! mode-agnostic: B0–S7 call `mode.send_single(...)`, `mode.scan_from_birthday(...)`,
//! `mode.get_balance()`, etc., through the [`Mode`] trait. The trait's three
//! implementations correspond to the three modes the bounty measures:
//!
//! * `Mode 1 / old_wallet` (`modes::old_wallet`) — spawn `minotari_console_wallet`,
//!   drive via gRPC.
//! * `Mode 2 / new_wallet` (`modes::new_wallet`) — invoke `minotari
//!   create-unsigned-transaction` as a subprocess, sign in-process via
//!   `tari_transaction_components::offline_signing::sign_locked_transaction`,
//!   broadcast via `minotari_node_wallet_client`. Wiring lands in step 3g.
//! * `Mode 3 / payment_processor` (`modes::payment_processor`) — same as Mode 2
//!   but with repeated `--recipient` flags per `DESIGN_ADDENDUM.md §Mode 3 CLI
//!   shape — proven`. Wiring lands in step 3h.
//!
//! This module's job is just the trait + supporting types. Implementations
//! live in sibling files added one step at a time per `DESIGN_ADDENDUM.md §S4`.

use tari_common_types::tari_address::TariAddress;

/// Per-transaction record produced by `Mode::send_single` /
/// `Mode::send_batch_one_to_many`. Mirrors the cell envelope's
/// `tx_records[]` shape in `RESULT_PROFILE_SCHEMA.md §4` — scenario code
/// folds these into the per-round / per-N aggregates without re-deriving
/// the per-tx timings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxRecord {
    /// Transaction ID returned by the broadcast layer (Mode 1: gRPC
    /// `Transfer` returns the txid; Mode 2/3: derived from
    /// `signed.signed_transaction` body excess signature).
    pub txid: String,
    /// Total wall-clock for `mode.send_*` from call entry to broadcast
    /// completion, in milliseconds. Covers construction + broadcast;
    /// confirmation polling is separate.
    pub t_total_ms: u64,
    /// Time from `mode.send_*` entry to broadcast-call completion (i.e.
    /// `submit_transaction` returned, mempool entry observed for Mode 1
    /// via `is_in_mempool`).
    pub t_broadcast_ms: u64,
    /// Time from broadcast completion to confirmation depth >= `c_min`.
    /// `None` when the tx has not yet been confirmed at record time —
    /// scenarios that poll for confirmation backfill this.
    pub t_confirm_ms: Option<u64>,
    /// Status string per `RESULT_PROFILE_SCHEMA.md §4`:
    /// `"success" | "failure" | "halted" | "timeout"`.
    pub status: String,
    /// Free-form error description on `status != "success"`. None on success.
    pub error_string: Option<String>,
    /// Fee paid in microTari for this transaction. Set to 0 when the
    /// transaction failed before fee computation.
    pub fee_microtari: u64,
}

/// Per-scan outcome produced by `Mode::scan_from_birthday`. Carries the
/// scenario-side measurements (`t_scan_ms`, blocks scanned, outputs found,
/// final UTXO count, final balance) that B0/S2/S3/S6/S7 record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    /// Wall-clock duration of the scan in milliseconds.
    pub t_scan_ms: u64,
    /// Tip height observed when the scan started.
    pub h_tip_start: u64,
    /// Tip height observed when the scan completed.
    pub h_tip_end: u64,
    /// Number of outputs found during the scan that belong to this wallet.
    pub outputs_found: u64,
    /// Final UTXO count once the scan completed.
    pub utxo_count: u64,
    /// Final available balance in microTari once the scan completed.
    pub balance_microtari: u64,
}

/// The per-mode abstraction every scenario calls into.
///
/// Implementations: [`old_wallet::OldWallet`] (Mode 1), `NewWallet` (Mode 2,
/// step 3g), `PaymentProcessor` (Mode 3, step 3h).
///
/// The trait is intentionally minimal — only the methods DESIGN.md §Module
/// boundaries enumerates. Scenarios that need higher-level orchestration
/// (S4's concurrent dispatch, S5's batch/individual arm split) compose
/// these primitives at the scenario layer, not by adding methods here.
///
/// **No retry, backoff, throttling, or partitioning** in any implementation
/// per AC-30/31/32; static-check grep tests enforce this on `src/scenarios/*.rs`
/// and `src/modes/*.rs` (per `DESIGN_ADDENDUM.md §S3`).
#[async_trait::async_trait]
pub trait Mode: Send + Sync {
    /// Mode identifier — `"old_wallet"`, `"new_wallet"`, or
    /// `"payment_processor"`. Used as the top-level key under `results.<name>`
    /// in the result profile, and to drive AC-20's per-mode arm matrix.
    fn name(&self) -> &'static str;

    /// Construct and broadcast a single-recipient transaction.
    async fn send_single(
        &mut self,
        recipient: &TariAddress,
        amount_microtari: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxRecord>;

    /// Construct and broadcast a 1-to-many transaction. Mode 1 (`old_wallet`)
    /// returns `Err` here — its gRPC `Transfer` does not natively support
    /// 1→K batch (per `DESIGN.md §Mode 1 step 3` "For S5 batch arm in Mode 1:
    /// skipped"). Mode 2 and Mode 3 implement this via repeated `--recipient`
    /// flags on `minotari create-unsigned-transaction`.
    async fn send_batch_one_to_many(
        &mut self,
        recipients: &[(TariAddress, u64)],
        fee_rate: u64,
    ) -> anyhow::Result<TxRecord>;

    /// Scan the chain from the given birthday height. Called by B0
    /// (birthday=0, expects 0 outputs), S2 (birthday=0, expects 512),
    /// S3 (birthday=H_birth from S0), S6 (S2-shape after S5), S7
    /// (S3-shape after S5).
    async fn scan_from_birthday(&mut self, birthday: u16) -> anyhow::Result<ScanOutcome>;

    /// Current available balance in microTari, as the wallet sees it now.
    ///
    /// Takes `&mut self` because the underlying gRPC `WalletClient` is
    /// stateful and tonic's generated methods require `&mut self` —
    /// Mode 1 dispatches `GetBalance` against the held client. Mode 2/3
    /// implementations use a stateless subprocess invocation; they
    /// accept the `&mut` signature for trait uniformity.
    async fn get_balance(&mut self) -> anyhow::Result<u64>;

    /// Current UTXO count, as the wallet sees it now. Same `&mut self`
    /// rationale as [`Self::get_balance`].
    async fn get_utxo_count(&mut self) -> anyhow::Result<u64>;

    /// Teardown the wallet, wipe its data directory (via
    /// `HarnessDataDir::wipe`), set the seed's birthday to `birthday`,
    /// re-import, and spawn the wallet again. Called by B0/S2/S3/S6/S7
    /// per `RESULT_PROFILE_SCHEMA.md §4` and AC-24.
    async fn wipe_and_reimport(&mut self, birthday: u16) -> anyhow::Result<()>;
}

/// Error returned by Mode implementations that don't support a given
/// operation. Surfaces from `Mode 1::send_batch_one_to_many` (gRPC `Transfer`
/// is single-recipient); scenarios that hit this `Err` record
/// `arms.batch.applies = false` per AC-20 and continue.
#[derive(Debug, thiserror::Error)]
#[error("operation '{op}' is not supported on mode '{mode}': {reason}")]
pub struct UnsupportedOperation {
    /// Mode name (`"old_wallet"`, etc.).
    pub mode: &'static str,
    /// Operation name (`"send_batch_one_to_many"`).
    pub op: &'static str,
    /// Short reason — surfaced into result-profile `errors.details`.
    pub reason: &'static str,
}
