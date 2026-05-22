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

pub(super) mod minotari_subprocess;
pub(super) mod minotari_wallet_ops;
pub mod new_wallet;
pub mod old_wallet;
pub mod payment_processor;

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

/// Per-tx status helper — folds into the schema's per-cell envelope under
/// `RESULT_PROFILE_SCHEMA.md §4 tx_records[].status` ("success" / "failure" /
/// "halted" / "timeout") and the `errors.details[].phase`
/// ("construct" / "sign" / "broadcast" / "confirm" / "scan") field.
///
/// `TxRecord.status` itself is a `String` so the schema's required literal
/// values land verbatim. These helpers provide typed entry points the modes
/// call into to avoid stringly-typed bugs at the construction site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxRecordStatus {
    /// Broadcast accepted by the base node.
    Success,
    /// Broadcast returned a non-accept; `RejectionReason` lives in the record's
    /// `error_string`.
    Rejected,
    /// Failed before broadcast completion. The accompanying [`TxRecordPhase`]
    /// names where in the pipeline (construct / sign / broadcast / confirm).
    Failed(TxRecordPhase),
}

impl TxRecordStatus {
    /// Construct a `Failed(phase)` status — shorthand for the most common
    /// constructor used by the Mode 2/3 subprocess pipeline.
    pub fn failed(phase: TxRecordPhase) -> Self {
        Self::Failed(phase)
    }
}

impl std::fmt::Display for TxRecordStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Per `RESULT_PROFILE_SCHEMA.md §4` the status enum is
            // {"success","failure","halted","timeout"}. "Rejected" surfaces as
            // "failure" with the rejection_reason in error_string.
            Self::Success => f.write_str("success"),
            Self::Rejected => f.write_str("failure"),
            Self::Failed(phase) => write!(f, "failure:{phase}"),
        }
    }
}

/// Where in the Mode 2/3 subprocess pipeline a failure occurred. Lifted from
/// `RESULT_PROFILE_SCHEMA.md §4 errors.details[].phase`. Scenario code reads
/// the suffix on a `failure:<phase>` status string to populate the per-cell
/// `errors.details[].phase` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxRecordPhase {
    /// `minotari create-unsigned-transaction` subprocess.
    Construct,
    /// In-process `sign_locked_transaction`.
    Sign,
    /// `Broadcaster::submit_transaction`.
    Broadcast,
    /// Confirmation polling (post-broadcast).
    Confirm,
    /// Wallet scan (B0/S2/S3/S6/S7).
    Scan,
}

impl std::fmt::Display for TxRecordPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Construct => "construct",
            Self::Sign => "sign",
            Self::Broadcast => "broadcast",
            Self::Confirm => "confirm",
            Self::Scan => "scan",
        })
    }
}

/// Hand-rolled fake `Mode` for scenario unit tests.
///
/// Records call order and returns canned values per method. Scoped to
/// `#[cfg(test)] pub(crate)` so scenario unit tests under `src/scenarios/*`
/// can construct it via `use crate::modes::test_support::FakeMode` without
/// exposing it to downstream crates.
///
/// Diverges from `analysis/DESIGN.md §Test Strategy`'s "MockWalletDriver via
/// mockall" reference — implementation tactic recorded in
/// `analysis/API_DRIFT.md §3i.1.a` and surfaced in PR_BODY_PLAN.md. Intent
/// (mock the Mode trait surface for scenario unit tests) is preserved.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use super::*;

    /// Per-call outcome injected into FakeMode's `send_single_sequence` —
    /// success returns the named `TxRecord`, failure bails with the named
    /// message. Used by scenarios (S1+) that need to model alternating
    /// success/failure across multiple sends in a single test.
    #[derive(Clone)]
    pub(crate) enum SendOutcome {
        /// Return `Ok(tx_record.clone())` from `send_single`.
        Ok(TxRecord),
        /// Bail with `anyhow!("FakeMode::send_single: {msg}")` from `send_single`.
        Err(String),
    }

    /// Hand-rolled fake for scenario unit tests. Records call order and
    /// returns canned values per method. See module docs for rationale.
    ///
    /// `canned_balance` and `canned_utxo_count` are sequences rather than
    /// single values so a single test can model pre-vs-post state across an
    /// intervening `send_*` call — S0 reads pre-state, submits a tx, then
    /// polls post-state. Each call to `get_balance` / `get_utxo_count`
    /// advances an internal index; once the index reaches the end of the
    /// sequence the **last** value is repeated indefinitely (saturating).
    /// An empty sequence still yields the "no canned value set" error.
    ///
    /// `send_single_sequence` (used by S1+) is the per-call outcome list:
    /// each `send_single` call pops the front; once exhausted, the fake
    /// falls through to `canned_send_single` (legacy single-value path).
    pub(crate) struct FakeMode {
        /// Returned by `scan_from_birthday`.
        pub canned_scan: Option<ScanOutcome>,
        /// Successive values returned by `get_balance`; last value sticks.
        pub canned_balance: Vec<u64>,
        /// Successive values returned by `get_utxo_count`; last value sticks.
        pub canned_utxo_count: Vec<u64>,
        /// Default `send_single` return value when `send_single_sequence` is
        /// empty.
        pub canned_send_single: Option<TxRecord>,
        /// Per-call outcomes for `send_single`. Each call consumes the
        /// front; once empty, falls through to `canned_send_single`.
        pub send_single_sequence: Vec<SendOutcome>,
        /// Returned by `send_batch_one_to_many`.
        pub canned_batch: Option<TxRecord>,
        /// If `Some`, every method bails with this message.
        pub fail_with: Option<String>,
        /// Method-name log in call order.
        pub calls: Mutex<Vec<&'static str>>,
        /// Index into `canned_balance` for the next `get_balance` call.
        balance_idx: Mutex<usize>,
        /// Index into `canned_utxo_count` for the next `get_utxo_count` call.
        utxo_idx: Mutex<usize>,
        /// Index into `send_single_sequence` for the next `send_single` call.
        send_single_idx: Mutex<usize>,
    }

    impl FakeMode {
        /// Construct a FakeMode with no canned values and no forced failure.
        pub(crate) fn new() -> Self {
            Self {
                canned_scan: None,
                canned_balance: Vec::new(),
                canned_utxo_count: Vec::new(),
                canned_send_single: None,
                send_single_sequence: Vec::new(),
                canned_batch: None,
                fail_with: None,
                calls: Mutex::new(Vec::new()),
                balance_idx: Mutex::new(0),
                utxo_idx: Mutex::new(0),
                send_single_idx: Mutex::new(0),
            }
        }

        fn record(&self, method: &'static str) {
            // `lock().unwrap()` mirrors the in-tree `Mutex` usage in
            // `src/modes/minotari_wallet_ops.rs` — poisoning surfaces as
            // a panic in tests, which is the desired behavior.
            self.calls.lock().unwrap().push(method);
        }

        fn check_fail(&self, method: &'static str) -> anyhow::Result<()> {
            if let Some(msg) = &self.fail_with {
                anyhow::bail!("FakeMode::{method}: {msg}");
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Mode for FakeMode {
        fn name(&self) -> &'static str {
            "fake"
        }

        async fn send_single(
            &mut self,
            _recipient: &TariAddress,
            _amount_microtari: u64,
            _fee_rate: u64,
        ) -> anyhow::Result<TxRecord> {
            self.record("send_single");
            self.check_fail("send_single")?;
            // First check the per-call sequence; once exhausted, fall through
            // to the legacy single-value canned_send_single.
            let outcome = {
                let mut idx = self.send_single_idx.lock().unwrap();
                if *idx < self.send_single_sequence.len() {
                    let out = self.send_single_sequence[*idx].clone();
                    *idx += 1;
                    Some(out)
                } else {
                    None
                }
            };
            match outcome {
                Some(SendOutcome::Ok(rec)) => Ok(rec),
                Some(SendOutcome::Err(msg)) => {
                    anyhow::bail!("FakeMode::send_single: {msg}")
                }
                None => self
                    .canned_send_single
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("FakeMode::send_single: no canned value set")),
            }
        }

        async fn send_batch_one_to_many(
            &mut self,
            _recipients: &[(TariAddress, u64)],
            _fee_rate: u64,
        ) -> anyhow::Result<TxRecord> {
            self.record("send_batch_one_to_many");
            self.check_fail("send_batch_one_to_many")?;
            self.canned_batch.clone().ok_or_else(|| {
                anyhow::anyhow!("FakeMode::send_batch_one_to_many: no canned value set")
            })
        }

        async fn scan_from_birthday(&mut self, _birthday: u16) -> anyhow::Result<ScanOutcome> {
            self.record("scan_from_birthday");
            self.check_fail("scan_from_birthday")?;
            self.canned_scan
                .clone()
                .ok_or_else(|| anyhow::anyhow!("FakeMode::scan_from_birthday: no canned value set"))
        }

        async fn get_balance(&mut self) -> anyhow::Result<u64> {
            self.record("get_balance");
            self.check_fail("get_balance")?;
            if self.canned_balance.is_empty() {
                anyhow::bail!("FakeMode::get_balance: no canned value set");
            }
            let mut idx = self.balance_idx.lock().unwrap();
            let here = (*idx).min(self.canned_balance.len() - 1);
            let value = self.canned_balance[here];
            if *idx < self.canned_balance.len() - 1 {
                *idx += 1;
            }
            Ok(value)
        }

        async fn get_utxo_count(&mut self) -> anyhow::Result<u64> {
            self.record("get_utxo_count");
            self.check_fail("get_utxo_count")?;
            if self.canned_utxo_count.is_empty() {
                anyhow::bail!("FakeMode::get_utxo_count: no canned value set");
            }
            let mut idx = self.utxo_idx.lock().unwrap();
            let here = (*idx).min(self.canned_utxo_count.len() - 1);
            let value = self.canned_utxo_count[here];
            if *idx < self.canned_utxo_count.len() - 1 {
                *idx += 1;
            }
            Ok(value)
        }

        async fn wipe_and_reimport(&mut self, _birthday: u16) -> anyhow::Result<()> {
            self.record("wipe_and_reimport");
            self.check_fail("wipe_and_reimport")?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn sample_scan() -> ScanOutcome {
            ScanOutcome {
                t_scan_ms: 1,
                h_tip_start: 0,
                h_tip_end: 0,
                outputs_found: 0,
                utxo_count: 0,
                balance_microtari: 0,
            }
        }

        #[tokio::test]
        async fn fake_mode_records_call_order() {
            let mut fake = FakeMode::new();
            fake.canned_scan = Some(sample_scan());
            fake.canned_balance = vec![7];
            fake.canned_utxo_count = vec![3];

            fake.scan_from_birthday(0).await.expect("scan ok");
            fake.get_balance().await.expect("balance ok");
            fake.get_utxo_count().await.expect("utxo_count ok");

            let calls = fake.calls.lock().unwrap().clone();
            assert_eq!(
                calls,
                vec!["scan_from_birthday", "get_balance", "get_utxo_count"],
                "FakeMode.calls must record method names in invocation order",
            );
        }

        #[tokio::test]
        async fn fake_mode_bails_when_fail_with_set() {
            let mut fake = FakeMode::new();
            fake.fail_with = Some("forced for test".to_string());

            let err = fake
                .get_balance()
                .await
                .expect_err("fail_with must force an error");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("FakeMode::get_balance"),
                "error must name the method: {msg}",
            );
            assert!(
                msg.contains("forced for test"),
                "error must carry fail_with payload: {msg}",
            );
        }
    }
}
