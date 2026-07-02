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

// Re-exported for the funding pre-flight: the New/Pp roles must be
// balance-checked through the same CLI wallet stack they spend from (see
// `cli_balance_for_seed` for the derivation-mismatch rationale).
pub(crate) use minotari_wallet_ops::cli_balance_for_seed;

use std::sync::Arc;

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
    /// dispatches gRPC `Transfer` with `single_tx = true` and K recipients
    /// (one MW tx with K outputs, per `wallet.proto:578` and PR #6 inline
    /// from @SWvheerden 2026-06-05). Mode 2 and Mode 3 implement this via
    /// repeated `--recipient` flags on `minotari create-unsigned-transaction`.
    /// See `analysis/DESIGN_AMENDMENT.md §11`.
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

    /// Return a clone-able dispatch handle for S4's concurrent construction.
    ///
    /// Realizes `DESIGN.md §S4 state machine` line 415's `mode.clone_handle()`
    /// intent via Option B from `analysis/DESIGN_AMENDMENT.md §9.6` (greenlit
    /// by main thread). The trait's `send_*` methods take `&mut self` — fine
    /// for sequential scenarios but incompatible with `tokio::JoinSet::spawn`'s
    /// `'static + Send` future requirement when N tasks each need to invoke
    /// the construction pipeline concurrently.
    ///
    /// Each impl returns an `Arc<dyn S4Dispatcher>` capturing its own
    /// concurrency-safe immutable state:
    ///
    /// * Mode 1 (`OldWallet`): a cloned `WalletClient<Channel>` (tonic
    ///   `#[derive(Clone)]` on the generated client; the gRPC channel is
    ///   cheap to clone — see `target/release/build/minotari_app_grpc-*/out/tari.rpc.rs:6920`).
    /// * Mode 2 (`NewWallet`) / Mode 3 (`PaymentProcessor`): `Arc<Config>`,
    ///   `Arc<SeedHandle>`, `Arc<Broadcaster>`, `Arc<PathBuf>` (data dir),
    ///   and the per-mode [`crate::modes::minotari_subprocess::SeedRole`].
    ///   `create_sign_and_submit` is already a free function taking shared
    ///   references; the Arc bundle is what makes the cross-task clone work.
    ///
    /// **No Mutex on the production dispatcher surface** — concurrency safety
    /// comes from Arc-clone of immutable state, not from runtime locking. The
    /// `tests/c_no_dispatch_serialization_in_s4.rs` static grep enforces this
    /// against `src/scenarios/s4_concurrent.rs`.
    fn dispatcher(&self) -> Arc<dyn S4Dispatcher>;

    /// PID to sample for the per-scenario [`crate::sampler::ResourceSampler`].
    ///
    /// Default impl returns the harness's own PID — measures harness-side
    /// overhead per the 3j brief's option (a) per-mode sampling choice.
    /// Mode-specific overrides could point at e.g. the spawned
    /// `console_wallet` PID for Mode 1; v1 keeps all three modes on the
    /// default so cross-mode `peak_rss_bytes` / `peak_cpu_pct` comparisons
    /// are apples-to-apples (each measures the same harness process).
    fn target_pid_for_sampling(&self) -> i32 {
        std::process::id() as i32
    }
}

/// Clone-able dispatch handle for S4's concurrent construction (AC-17).
///
/// One handle per scenario; N concurrent tasks each hold an `Arc<dyn S4Dispatcher>`
/// clone and call [`Self::dispatch`] without coordinating through a shared
/// `&mut Mode`. The receiver is `&self` so the borrow checker permits N
/// concurrent invocations.
///
/// Returns the same [`TxRecord`] shape as [`Mode::send_single`] so the S4
/// scenario can fold per-task outcomes into its `tx_records[]` aggregate
/// without re-deriving timing fields.
#[async_trait::async_trait]
pub trait S4Dispatcher: Send + Sync {
    /// Construct, sign, and broadcast a single-recipient transaction. Same
    /// contract as [`Mode::send_single`] — failure surfaces as a `TxRecord`
    /// with `status != "success"` (no retry, no backoff, per AC-30/31/32).
    async fn dispatch(
        &self,
        recipient: TariAddress,
        amount_microtari: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxRecord>;
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
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

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

        fn dispatcher(&self) -> Arc<dyn S4Dispatcher> {
            // Snapshot the FakeMode's current `send_single_sequence` into a
            // shared `Arc<Mutex<Vec<SendOutcome>>>` so N concurrent S4 tasks
            // can pop from the same canned sequence. The Mutex lives ONLY
            // inside the `#[cfg(test)]` test-fixture surface — production
            // dispatchers (Mode 1/2/3) carry no Mutex per
            // `analysis/DESIGN_AMENDMENT.md §9.6` Option B and the brief's
            // sharper STOP triggers.
            Arc::new(FakeModeDispatcher {
                sequence: Arc::new(Mutex::new(self.send_single_sequence.clone())),
                cursor: Arc::new(AtomicUsize::new(0)),
                canned_default: self.canned_send_single.clone(),
            })
        }
    }

    /// Test-only `S4Dispatcher` for the scenario suite.
    ///
    /// Pops outcomes from a shared canned sequence under a `Mutex<Vec<SendOutcome>>`
    /// (sequence storage) + `AtomicUsize` (cursor). This Mutex is **fixture
    /// state**, not production dispatcher state — the brief's "no Mutex on the
    /// production dispatcher surface" prohibition is satisfied because this
    /// type lives under `#[cfg(test)] mod test_support`. Same precedent as
    /// FakeMode itself: hand-rolled fakes carry whatever interior mutability
    /// the scenario unit tests need.
    pub(crate) struct FakeModeDispatcher {
        /// Shared canned outcomes — one cursor advance per `dispatch` call.
        /// Once the cursor passes the sequence length, falls through to
        /// `canned_default` (matches FakeMode's `send_single` semantics).
        sequence: Arc<Mutex<Vec<SendOutcome>>>,
        /// Position cursor for the next `dispatch` call. `AtomicUsize` so the
        /// fetch-and-increment is lock-free across concurrent tasks; the
        /// Mutex on `sequence` is held only briefly to clone the indexed
        /// element out.
        cursor: Arc<AtomicUsize>,
        /// Fallback `Ok(TxRecord)` returned once the sequence is exhausted.
        /// `None` means "bail with a 'no canned value set' error" matching
        /// FakeMode::send_single's contract.
        canned_default: Option<TxRecord>,
    }

    #[async_trait::async_trait]
    impl S4Dispatcher for FakeModeDispatcher {
        async fn dispatch(
            &self,
            _recipient: TariAddress,
            _amount_microtari: u64,
            _fee_rate: u64,
        ) -> anyhow::Result<TxRecord> {
            let idx = self.cursor.fetch_add(1, Ordering::SeqCst);
            let outcome = {
                let seq = self.sequence.lock().unwrap();
                if idx < seq.len() {
                    Some(seq[idx].clone())
                } else {
                    None
                }
            };
            match outcome {
                Some(SendOutcome::Ok(rec)) => Ok(rec),
                Some(SendOutcome::Err(msg)) => {
                    anyhow::bail!("FakeModeDispatcher::dispatch: {msg}")
                }
                None => self.canned_default.clone().ok_or_else(|| {
                    anyhow::anyhow!("FakeModeDispatcher::dispatch: no canned value set")
                }),
            }
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

        fn sample_tx_record(tag: &str) -> TxRecord {
            TxRecord {
                txid: format!("txid-{tag}"),
                t_total_ms: 1,
                t_broadcast_ms: 1,
                t_confirm_ms: None,
                status: "success".to_string(),
                error_string: None,
                fee_microtari: 0,
            }
        }

        /// FakeMode::dispatcher returns an Arc-shareable handle that can be
        /// cloned and invoked from a tokio context. Proves the trait method
        /// is wired through and that the dispatcher Arc is usable as the
        /// `Arc<dyn S4Dispatcher>` shape S4's JoinSet pattern needs.
        #[tokio::test]
        async fn fake_mode_dispatcher_returns_arc_and_is_invokable() {
            let mut fake = FakeMode::new();
            fake.send_single_sequence = vec![SendOutcome::Ok(sample_tx_record("solo"))];
            let dispatcher = fake.dispatcher();
            let recipient = derive_test_address();
            let rec = dispatcher
                .dispatch(recipient, 1_000, 25)
                .await
                .expect("dispatch ok");
            assert_eq!(rec.txid, "txid-solo");
        }

        /// 4 concurrent dispatch tasks against a FakeModeDispatcher
        /// consume all 4 canned outcomes; cursor advances atomically across
        /// tasks. Proves the dispatcher is genuinely concurrent — Arc clone
        /// is sufficient, no `&mut self` required.
        #[tokio::test]
        async fn fake_dispatcher_pops_canned_sequence_concurrently() {
            let mut fake = FakeMode::new();
            fake.send_single_sequence = vec![
                SendOutcome::Ok(sample_tx_record("a")),
                SendOutcome::Ok(sample_tx_record("b")),
                SendOutcome::Ok(sample_tx_record("c")),
                SendOutcome::Ok(sample_tx_record("d")),
            ];
            let dispatcher = fake.dispatcher();

            let mut joinset = tokio::task::JoinSet::new();
            for _ in 0..4 {
                let d = dispatcher.clone();
                let recipient = derive_test_address();
                joinset.spawn(async move { d.dispatch(recipient, 1_000, 25).await });
            }
            let mut txids: Vec<String> = Vec::new();
            while let Some(joined) = joinset.join_next().await {
                let rec = joined.expect("join ok").expect("dispatch ok");
                txids.push(rec.txid);
            }
            txids.sort();
            assert_eq!(
                txids,
                vec![
                    "txid-a".to_string(),
                    "txid-b".to_string(),
                    "txid-c".to_string(),
                    "txid-d".to_string(),
                ],
                "all 4 canned outcomes must be consumed exactly once \
                 (order is not deterministic across tasks)",
            );
        }

        fn derive_test_address() -> TariAddress {
            let mnemonic = crate::gen_seed().expect("gen_seed");
            crate::seed::derive_address(&mnemonic).expect("derive_address")
        }
    }
}
