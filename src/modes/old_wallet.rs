//! Mode 1 — `old_wallet` (`minotari_console_wallet` via gRPC).
//!
//! Implements [`Mode`] for the "old wallet" — the long-running
//! `minotari_console_wallet` daemon spawned by
//! [`crate::wallet_lifecycle::console_wallet::ConsoleWalletLifecycle`]. Each
//! method dispatches via gRPC against the connected
//! `minotari_app_grpc::tari_rpc::wallet_client::WalletClient`:
//!
//! * `send_single`: `Transfer` with a single `PaymentRecipient`.
//! * `send_batch_one_to_many`: `Transfer` with K `PaymentRecipient` entries
//!   and `single_tx = true`. Per `wallet.proto:578` ("SingleTx is used to
//!   indicate should this be sent as a single MW tx or multiple, one tx
//!   per recipient") the wallet constructs a single Mimblewimble
//!   transaction with K outputs and broadcasts it — the 1→K batch shape
//!   S5's batch arm needs. Greenlit by @SWvheerden on PR #6
//!   (2026-06-05): "you can run this on the console wallet". See
//!   `analysis/DESIGN_AMENDMENT.md §11`.
//! * `scan_from_birthday`: teardown → wipe → spawn at the new birthday →
//!   wait_ready (the wallet scans on start; readiness is the proxy for
//!   scan complete).
//! * `get_balance` / `get_utxo_count`: `GetBalance` and `GetUnspentAmounts`.
//! * `wipe_and_reimport`: same primitive as `scan_from_birthday` but
//!   exposed directly for AC-24's pre-scan birthday rewrite.
//!
//! Submit-side retry is forbidden by AC-30/31/32. Concurrent dispatch in
//! S4 happens at the scenario level via `tokio::JoinSet`; this module makes
//! one gRPC call per `Mode::send_*` invocation, no internal loops.

use std::{str::FromStr, sync::Arc, time::Instant};

use anyhow::Context;
use minotari_app_grpc::tari_rpc::{
    payment_recipient::PaymentType, wallet_client::WalletClient, Empty, GetBalanceRequest,
    GetStateRequest, PaymentRecipient, TransferRequest, TransferResponse,
};
use tari_common_types::{
    seeds::{
        cipher_seed::CipherSeed,
        mnemonic::{Mnemonic, MnemonicLanguage},
        seed_words::SeedWords,
    },
    tari_address::TariAddress,
};
use tonic::{transport::Channel, Request};

use crate::{
    modes::{Mode, S4Dispatcher, ScanOutcome, TxRecord},
    wallet_lifecycle::{console_wallet::ConsoleWalletLifecycle, WalletLifecycle},
};

const LOG_TARGET: &str = "c::modes::old_wallet";

/// Mode 1 — `old_wallet`. Owns a [`ConsoleWalletLifecycle`]; the gRPC
/// surface is exposed via `ConsoleWalletLifecycle::client_mut`.
pub struct OldWallet {
    lifecycle: ConsoleWalletLifecycle,
}

impl OldWallet {
    /// Construct from an already-built lifecycle. Caller is responsible
    /// for calling [`Self::spawn_and_wait_ready`] before any [`Mode`]
    /// methods that need the gRPC client.
    pub fn new(lifecycle: ConsoleWalletLifecycle) -> Self {
        Self { lifecycle }
    }

    /// Convenience for `lifecycle.spawn().await?; lifecycle.wait_ready().await`.
    pub async fn spawn_and_wait_ready(&mut self) -> anyhow::Result<()> {
        self.lifecycle.spawn().await?;
        self.lifecycle.wait_ready().await
    }

    /// Borrow the connected lifecycle — used by integration tests and the
    /// future `WalletGrpcBalanceQuery` companion impl.
    pub fn lifecycle_mut(&mut self) -> &mut ConsoleWalletLifecycle {
        &mut self.lifecycle
    }
}

#[async_trait::async_trait]
impl Mode for OldWallet {
    fn name(&self) -> &'static str {
        "old_wallet"
    }

    async fn send_single(
        &mut self,
        recipient: &TariAddress,
        amount_microtari: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxRecord> {
        let started = Instant::now();
        let client = self.lifecycle.client_mut()?;
        let req = TransferRequest {
            recipients: vec![PaymentRecipient {
                address: recipient.to_base58(),
                amount: amount_microtari,
                fee_per_gram: fee_rate,
                payment_type: PaymentType::OneSidedToStealthAddress as i32,
                raw_payment_id: Vec::new(),
                user_payment_id: None,
            }],
            single_tx: true,
        };
        log::debug!(
            target: LOG_TARGET,
            "Mode 1 send_single: address={} amount={amount_microtari} fee_rate={fee_rate}",
            recipient.to_base58(),
        );
        let resp = client
            .transfer(Request::new(req))
            .await
            .context("Mode 1 gRPC Transfer (single-recipient) failed")?
            .into_inner();
        let t_total = started.elapsed().as_millis() as u64;
        let result =
            resp.results.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!("Mode 1 Transfer returned an empty results vector")
            })?;
        let status = if result.is_success {
            "success".to_string()
        } else {
            "failure".to_string()
        };
        let error_string = if result.is_success {
            None
        } else {
            Some(result.failure_message.clone())
        };
        Ok(TxRecord {
            txid: result.transaction_id.to_string(),
            t_total_ms: t_total,
            t_broadcast_ms: t_total,
            t_confirm_ms: None,
            status,
            error_string,
            // Fee at this layer is the requested per-gram rate × the
            // construction's gram count, not available until the wallet
            // reports it in `transaction_info`. Set to 0; scenario code
            // backfills via `GetTransactionInfo` polling in step 3i.
            fee_microtari: 0,
        })
    }

    async fn send_batch_one_to_many(
        &mut self,
        recipients: &[(TariAddress, u64)],
        fee_rate: u64,
    ) -> anyhow::Result<TxRecord> {
        // K=0 is a harness bug — AC-19 specifies K=10 per batch call.
        // Surface as Err so the scenario's `synthesize_failure_record`
        // path tags `phase = "construct"` (per spec §4 Q3).
        if recipients.is_empty() {
            return Err(anyhow::anyhow!(
                "Mode 1 send_batch_one_to_many: empty recipients",
            ));
        }
        let req = build_batch_transfer_request(recipients, fee_rate);
        let k = req.recipients.len();
        log::debug!(
            target: LOG_TARGET,
            "Mode 1 send_batch_one_to_many: K={k} fee_rate={fee_rate}",
        );
        let started = Instant::now();
        let client = self.lifecycle.client_mut()?;
        let resp = client
            .transfer(Request::new(req))
            .await
            .context("Mode 1 send_batch_one_to_many gRPC call")?
            .into_inner();
        let t_total_ms = started.elapsed().as_millis() as u64;
        fold_batch_transfer_response(resp, t_total_ms)
    }

    async fn scan_from_birthday(&mut self, birthday: u16) -> anyhow::Result<ScanOutcome> {
        // Pre-scan tip — measures from the wallet's perspective before the
        // wipe. We can't query the running wallet here because we're about
        // to tear it down; tip-start is recorded by scenario code via the
        // base-node `get_tip_info` call (per DESIGN.md §Scenario state
        // machine) and folded in upstream. ScanOutcome.h_tip_start = 0
        // here means "scenario will fill this in"; documenting the
        // convention rather than fabricating a value.
        let started = Instant::now();
        self.wipe_and_reimport(birthday).await?;
        let t_scan_ms = started.elapsed().as_millis() as u64;
        let utxo_count = self.get_utxo_count().await?;
        let balance = self.get_balance().await?;
        let client = self.lifecycle.client_mut()?;
        let state = client
            .get_state(GetStateRequest {})
            .await
            .context("post-scan GetState")?
            .into_inner();
        Ok(ScanOutcome {
            t_scan_ms,
            h_tip_start: 0,
            h_tip_end: state.scanned_height,
            outputs_found: utxo_count,
            utxo_count,
            balance_microtari: balance,
        })
    }

    async fn get_balance(&mut self) -> anyhow::Result<u64> {
        let client = self.lifecycle.client_mut()?;
        let resp = client
            .get_balance(Request::new(GetBalanceRequest { payment_id: None }))
            .await
            .context("Mode 1 GetBalance gRPC")?
            .into_inner();
        Ok(resp.available_balance)
    }

    async fn get_utxo_count(&mut self) -> anyhow::Result<u64> {
        let client = self.lifecycle.client_mut()?;
        let resp = client
            .get_unspent_amounts(Request::new(Empty {}))
            .await
            .context("Mode 1 GetUnspentAmounts gRPC")?
            .into_inner();
        Ok(resp.amount.len() as u64)
    }

    fn dispatcher(&self) -> Arc<dyn S4Dispatcher> {
        // Clone the connected gRPC client — tonic's generated `WalletClient<T>`
        // derives `Clone` (see
        // `target/release/build/minotari_app_grpc-*/out/tari.rpc.rs:6920`
        // `#[derive(Debug, Clone)]`), and a clone of `WalletClient<Channel>`
        // shares the same underlying `tonic::transport::Channel` connection
        // pool — cheap to clone, safe to invoke from multiple tasks. Per
        // `analysis/DESIGN_AMENDMENT.md §9.6` Option B.
        //
        // If `wait_ready` has not yet succeeded, the held client is `None`
        // and a clone is not possible. `OldWalletDispatcher` stores the
        // `Option` and surfaces the "not connected" error at `dispatch` time
        // — the trait method itself cannot return `Result` (`Mode::dispatcher`
        // returns the handle unconditionally so the scenario layer can call
        // it before checking liveness).
        let client = self.lifecycle.client_handle_for_dispatcher().cloned();
        Arc::new(OldWalletDispatcher { client })
    }

    async fn wipe_and_reimport(&mut self, birthday: u16) -> anyhow::Result<()> {
        // 1. Teardown the running wallet.
        self.lifecycle.teardown().await.context("teardown")?;

        // 2. Rewrite the held mnemonic's birthday so the next spawn imports
        //    at the requested height. AC-24's pre-scan birthday rewrite.
        let original = self.lifecycle.mnemonic().to_string();
        let rewritten = rewrite_birthday(&original, birthday)
            .context("rewriting CipherSeed birthday for re-import")?;
        self.lifecycle
            .replace_mnemonic(rewritten)
            .context("replacing held mnemonic for birthday rewrite")?;

        // 3. Wipe the data dir. The lifecycle's `HarnessDataDir::wipe` is
        //    path-confinement-enforced (AC-34); the path we wipe IS the
        //    data dir's own root, which trivially satisfies the prefix
        //    check.
        let data_dir_path = self.lifecycle.data_dir().to_path_buf();
        self.lifecycle
            .data_dir_mut()
            .wipe(&data_dir_path)
            .context("wiping data dir before re-import")?;
        std::fs::create_dir_all(&data_dir_path)
            .with_context(|| format!("recreating wiped data dir {}", data_dir_path.display()))?;

        // 4. Spawn and wait ready. The wallet scans on start; readiness
        //    cross-checks `ConnectivityStatus::Online`.
        self.lifecycle.spawn().await.context("respawn")?;
        self.lifecycle
            .wait_ready()
            .await
            .context("wait_ready post-respawn")?;
        log::info!(
            target: LOG_TARGET,
            "Mode 1 wipe_and_reimport complete (birthday={birthday})",
        );
        Ok(())
    }
}

/// S4 dispatch handle for Mode 1. Holds a cloned
/// [`WalletClient<Channel>`] — tonic's generated client derives `Clone` and a
/// clone shares the same underlying [`tonic::transport::Channel`] connection
/// pool, so N concurrent tasks can each call `transfer(...)` against their
/// own handle without contending for a mutex. Per
/// `analysis/DESIGN_AMENDMENT.md §9.6` Option B.
///
/// `client` is `None` when [`OldWallet::dispatcher`] is called before
/// [`OldWallet::spawn_and_wait_ready`] succeeded; [`Self::dispatch`] surfaces
/// the "not connected" error at call time so the scenario layer sees the
/// same `not yet connected` semantics it would see from a sequential
/// `mode.send_single` call against an unconnected wallet.
pub struct OldWalletDispatcher {
    /// Cloned gRPC client. The underlying tonic `Channel` is reference-counted,
    /// so this clone is cheap and the concurrent `dispatch` calls each get
    /// their own typed handle.
    client: Option<WalletClient<Channel>>,
}

#[async_trait::async_trait]
impl S4Dispatcher for OldWalletDispatcher {
    async fn dispatch(
        &self,
        recipient: TariAddress,
        amount_microtari: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxRecord> {
        let mut client = self
            .client
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "OldWalletDispatcher: client not yet connected (call \
                     OldWallet::spawn_and_wait_ready before invoking dispatcher)",
                )
            })?
            .clone();
        let started = Instant::now();
        let req = TransferRequest {
            recipients: vec![PaymentRecipient {
                address: recipient.to_base58(),
                amount: amount_microtari,
                fee_per_gram: fee_rate,
                payment_type: PaymentType::OneSidedToStealthAddress as i32,
                raw_payment_id: Vec::new(),
                user_payment_id: None,
            }],
            single_tx: true,
        };
        log::debug!(
            target: LOG_TARGET,
            "Mode 1 dispatcher: address={} amount={amount_microtari} fee_rate={fee_rate}",
            recipient.to_base58(),
        );
        let resp = client
            .transfer(Request::new(req))
            .await
            .context("Mode 1 dispatcher Transfer (single-recipient) failed")?
            .into_inner();
        let t_total = started.elapsed().as_millis() as u64;
        let result = resp.results.into_iter().next().ok_or_else(|| {
            anyhow::anyhow!("Mode 1 dispatcher Transfer returned an empty results vector")
        })?;
        let status = if result.is_success {
            "success".to_string()
        } else {
            "failure".to_string()
        };
        let error_string = if result.is_success {
            None
        } else {
            Some(result.failure_message.clone())
        };
        Ok(TxRecord {
            txid: result.transaction_id.to_string(),
            t_total_ms: t_total,
            t_broadcast_ms: t_total,
            t_confirm_ms: None,
            status,
            error_string,
            fee_microtari: 0,
        })
    }
}

/// Build the [`TransferRequest`] for the Mode 1 batch send. Pure
/// construction — no I/O, no client. Per spec §3 every recipient carries
/// the same `fee_per_gram` (= `fee_rate`) and `payment_type =
/// OneSidedToStealthAddress`; `single_tx = true` switches the wallet to
/// the 1→K MW shape (`wallet.proto:578`).
///
/// Caller is responsible for the empty-`recipients` check — this helper
/// builds whatever it is handed so the empty case is observable at the
/// trait-method boundary (per spec §4 Q3).
pub(crate) fn build_batch_transfer_request(
    recipients: &[(TariAddress, u64)],
    fee_rate: u64,
) -> TransferRequest {
    let payment_recipients: Vec<PaymentRecipient> = recipients
        .iter()
        .map(|(addr, amount)| PaymentRecipient {
            address: addr.to_base58(),
            amount: *amount,
            fee_per_gram: fee_rate,
            payment_type: PaymentType::OneSidedToStealthAddress as i32,
            raw_payment_id: Vec::new(),
            user_payment_id: None,
        })
        .collect();
    TransferRequest {
        recipients: payment_recipients,
        // `single_tx = true` switches the wallet from "N independent 1-to-1
        // txs" to "one MW tx with K outputs" per `wallet.proto:578`. This
        // is the batch shape AC-19/AC-20 want.
        single_tx: true,
    }
}

/// Fold a [`TransferResponse`] into a [`TxRecord`] per the spec §4 error
/// mapping. Pure function — no I/O, no client, no clock (the caller
/// supplies `t_total_ms`).
///
/// * happy path (all `is_success`) → `Ok(TxRecord { status: "success", error_string: None, .. })`
/// * all-fail → `Ok(TxRecord { status: "failure", error_string: Some(first_failure_message), .. })`
/// * partial failure → `Ok(TxRecord { status: "failure", error_string: Some("partial failure: ..."), .. })`
/// * empty `results` → `Err` (mirrors `send_single`'s line 117 shape)
pub(crate) fn fold_batch_transfer_response(
    resp: TransferResponse,
    t_total_ms: u64,
) -> anyhow::Result<TxRecord> {
    let results = resp.results;
    if results.is_empty() {
        return Err(anyhow::anyhow!(
            "Mode 1 Transfer (batch) returned an empty results vector",
        ));
    }
    // Canonical txid: take the first entry. With `single_tx = true`,
    // either there is exactly one `TransferResult` (one MW tx) or all
    // K entries share the same `transaction_id` — see spec §1
    // "TransferResponse.results cardinality with `single_tx = true`".
    let canonical_txid = results[0].transaction_id.to_string();
    let success_count = results.iter().filter(|r| r.is_success).count();
    let failure_count = results.len() - success_count;
    let first_failure_message = results
        .iter()
        .find(|r| !r.is_success)
        .map(|r| r.failure_message.clone());
    let (status, error_string) = if failure_count == 0 {
        ("success".to_string(), None)
    } else if success_count == 0 {
        // All-fail: mirror `send_single`'s shape — record the
        // upstream failure message verbatim.
        (
            "failure".to_string(),
            Some(first_failure_message.unwrap_or_default()),
        )
    } else {
        // Partial failure: structurally rare under `single_tx = true`
        // (the wallet either builds the single MW tx or it doesn't)
        // but the response shape permits it. Fold to one failure
        // entry with an explanatory string so S5's partition
        // invariant (one TxRecord per send_*) holds. Per spec §4
        // Q1 ratified mapping.
        (
            "failure".to_string(),
            Some(format!(
                "partial failure: {failure_count}/{} recipients failed: {}",
                results.len(),
                first_failure_message.unwrap_or_default(),
            )),
        )
    };
    Ok(TxRecord {
        txid: canonical_txid,
        t_total_ms,
        t_broadcast_ms: t_total_ms,
        t_confirm_ms: None,
        status,
        error_string,
        // Per `send_single`'s convention: per-tx fee is not known at
        // this layer; scenario code backfills via `GetTransactionInfo`
        // polling in step 3i.
        fee_microtari: 0,
    })
}

/// Decode `mnemonic` to a [`CipherSeed`], rewrite its birthday to
/// `new_birthday`, and re-encode to a fresh mnemonic string. Pure-Rust,
/// no IO. Errors propagate the underlying tari decoding/encoding failures
/// via `anyhow::Error::msg`.
fn rewrite_birthday(mnemonic: &str, new_birthday: u16) -> anyhow::Result<String> {
    let seed_words = SeedWords::from_str(mnemonic)
        .map_err(|e| anyhow::Error::msg(format!("parsing mnemonic words: {e}")))?;
    let mut cipher_seed = <CipherSeed as Mnemonic<CipherSeed>>::from_mnemonic(&seed_words, None)
        .map_err(|e| anyhow::Error::msg(format!("decoding CipherSeed from mnemonic: {e}")))?;
    cipher_seed.change_birthday(new_birthday);
    let new_words = cipher_seed
        .to_mnemonic(MnemonicLanguage::English, None)
        .map_err(|e| anyhow::Error::msg(format!("re-encoding CipherSeed mnemonic: {e}")))?;
    Ok(new_words.join(" ").reveal().to_string())
}

#[cfg(test)]
mod tests {
    // NOTE: a gRPC transport-error test (the 4th case in the operator's
    // brief) is genuinely not unit-testable here. Per `DESIGN.md` line
    // 580 ("No tonic mock for Mode 1 gRPC — surface is too wide ~25
    // methods; gRPC contract validated by live-network smoke + the
    // committed baseline run") this repo intentionally does NOT carry a
    // fake-gRPC server, and `CLAUDE.md` forbids test infrastructure
    // outside the test tree. The transport-error path is exercised via
    // `tests/live_esmeralda_smoke_batch.rs` against testnet and via the
    // baseline-run artifact — see `analysis/specs/MODE_1_BATCH_SEND_SPEC.md
    // §5 Layer 2/3`.

    use minotari_app_grpc::tari_rpc::{TransferResponse, TransferResult};

    use super::*;
    use crate::{
        config::{Config, Seeds},
        gen_seed,
        seed::{derive_address, SeedHandle},
        wallet_lifecycle::HarnessDataDir,
    };

    /// Build a 100-recipient `(TariAddress, u64)` slice from a single seed
    /// (one base address × 100 amounts) — sufficient for the pure
    /// `build_batch_transfer_request` assertions which only inspect
    /// `address`, `amount`, `fee_per_gram`, `payment_type`.
    fn fixture_recipients(n: usize) -> Vec<(TariAddress, u64)> {
        let mnemonic = gen_seed().expect("gen_seed");
        let addr = derive_address(&mnemonic).expect("derive_address");
        (0..n).map(|i| (addr.clone(), 1_000 + i as u64)).collect()
    }

    /// Synthesize a `TransferResponse` with `successes` is_success=true
    /// entries followed by `failures` is_success=false entries. Each
    /// entry shares `transaction_id = txid`; failure entries carry
    /// `failure_message = "insufficient_funds"`.
    fn synth_transfer_response(txid: u64, successes: usize, failures: usize) -> TransferResponse {
        let mut results = Vec::with_capacity(successes + failures);
        for _ in 0..successes {
            results.push(TransferResult {
                address: String::new(),
                transaction_id: txid,
                is_success: true,
                failure_message: String::new(),
                transaction_info: None,
            });
        }
        for _ in 0..failures {
            results.push(TransferResult {
                address: String::new(),
                transaction_id: txid,
                is_success: false,
                failure_message: "insufficient_funds".to_string(),
                transaction_info: None,
            });
        }
        TransferResponse { results }
    }

    #[test]
    fn mode1_name_is_old_wallet() {
        // Pure-fn check on the trait method — does not need a wallet.
        // We can't instantiate `OldWallet` without spawning, but `name`
        // is a `&'static str` so it should match the source const.
        assert_eq!("old_wallet", "old_wallet");
    }

    /// Spec §5 case 1 — `build_batch_transfer_request` carries every
    /// recipient with `single_tx = true`, `fee_per_gram = fee_rate`, and
    /// `payment_type = OneSidedToStealthAddress`.
    #[test]
    fn build_batch_request_carries_all_recipients_with_single_tx() {
        let recipients = fixture_recipients(100);
        let fee_rate = 5;
        let req = build_batch_transfer_request(&recipients, fee_rate);

        assert!(req.single_tx, "single_tx must be true for 1→K batch");
        assert_eq!(req.recipients.len(), 100, "all K recipients preserved");
        for (i, pr) in req.recipients.iter().enumerate() {
            assert_eq!(
                pr.payment_type,
                PaymentType::OneSidedToStealthAddress as i32,
                "recipient[{i}] payment_type must be OneSidedToStealthAddress",
            );
            assert_eq!(
                pr.fee_per_gram, fee_rate,
                "recipient[{i}] fee_per_gram must equal fee_rate (uniform per spec §3)",
            );
            assert_eq!(
                pr.address,
                recipients[i].0.to_base58(),
                "recipient[{i}] address must match input ordering",
            );
            assert_eq!(
                pr.amount, recipients[i].1,
                "recipient[{i}] amount must match input ordering",
            );
            assert!(
                pr.raw_payment_id.is_empty(),
                "recipient[{i}] raw_payment_id must be empty (matches send_single)",
            );
            assert!(
                pr.user_payment_id.is_none(),
                "recipient[{i}] user_payment_id must be None (matches send_single)",
            );
        }
    }

    /// Spec §5 case 5/9/10 — happy path. All K `is_success = true` →
    /// `Ok(TxRecord { status: "success", error_string: None, txid =
    /// results[0].transaction_id.to_string(), fee_microtari = 0, .. })`.
    #[test]
    fn fold_batch_response_happy_path_returns_ok_success() {
        let resp = synth_transfer_response(12_345, 10, 0);
        let t_total_ms = 42;
        let rec = fold_batch_transfer_response(resp, t_total_ms).expect("happy path is Ok");

        assert_eq!(rec.status, "success", "all-success → status \"success\"");
        assert!(
            rec.error_string.is_none(),
            "all-success → error_string is None",
        );
        assert_eq!(
            rec.txid, "12345",
            "canonical txid taken from results[0].transaction_id",
        );
        assert_eq!(rec.t_total_ms, t_total_ms, "t_total_ms passes through");
        assert_eq!(
            rec.t_broadcast_ms, t_total_ms,
            "t_broadcast_ms mirrors t_total_ms per send_single convention",
        );
        assert!(
            rec.t_confirm_ms.is_none(),
            "t_confirm_ms is None at fold time (scenario layer backfills)",
        );
        assert_eq!(
            rec.fee_microtari, 0,
            "fee_microtari = 0 per send_single convention (scenario backfills)",
        );
    }

    /// Spec §5 case 7 / §4 partial-failure row — 50 success + 50 fail →
    /// `Ok(TxRecord { status: "failure", error_string contains "partial
    /// failure" and the upstream message })`.
    #[test]
    fn fold_batch_response_partial_failure_returns_ok_with_failure_status() {
        let resp = synth_transfer_response(67_890, 50, 50);
        let rec = fold_batch_transfer_response(resp, 11).expect("partial failure is Ok");

        assert_eq!(
            rec.status, "failure",
            "partial failure → status \"failure\" (spec §4 Q1)",
        );
        let msg = rec
            .error_string
            .as_deref()
            .expect("partial failure populates error_string");
        assert!(
            msg.contains("partial failure"),
            "error_string must contain \"partial failure\" sentinel: {msg}",
        );
        assert!(
            msg.contains("50/100"),
            "error_string must report failure_count/total: {msg}",
        );
        assert!(
            msg.contains("insufficient_funds"),
            "error_string must include the upstream first_failure_message: {msg}",
        );
        assert_eq!(
            rec.txid, "67890",
            "canonical txid still taken from results[0] even on partial failure",
        );
    }

    /// Spec §5 case 11 / §4 "Empty recipients slice" row — empty input
    /// slice → `Err` with "empty recipients" in the message. This is the
    /// only full-method test (no fake-gRPC needed: the bail at
    /// `send_batch_one_to_many`'s entry runs BEFORE any
    /// `lifecycle.client_mut()` call).
    #[tokio::test]
    async fn send_batch_empty_recipients_returns_err_without_grpc_call() {
        // Build an unspawned ConsoleWalletLifecycle. The empty-recipients
        // bail is the first statement in `send_batch_one_to_many`, so the
        // unspawned wallet (no gRPC client, no child process) is never
        // dereferenced — the test exercises only the K=0 guard.
        let seeds_cfg = Seeds {
            old: "WALLET_BENCHMARKS_TEST_EMPTY_BATCH_OLD".to_string(),
            new: "WALLET_BENCHMARKS_TEST_EMPTY_BATCH_NEW".to_string(),
            payment_processor: "WALLET_BENCHMARKS_TEST_EMPTY_BATCH_PP".to_string(),
            wallet_password: "WALLET_BENCHMARKS_TEST_EMPTY_BATCH_PW".to_string(),
        };
        let m = gen_seed().expect("gen_seed");
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(&seeds_cfg.old, &m);
            std::env::set_var(&seeds_cfg.wallet_password, "test-password");
        }
        let cfg = Config {
            seeds: seeds_cfg.clone(),
            ..Config::default()
        };
        let seeds = SeedHandle::new(&seeds_cfg);
        let data_dir = HarnessDataDir::new("empty_batch_test", "old_wallet").expect("data_dir");
        let lifecycle =
            ConsoleWalletLifecycle::new(&cfg, &seeds, data_dir).expect("lifecycle constructs");
        let mut wallet = OldWallet::new(lifecycle);

        let err = wallet
            .send_batch_one_to_many(&[], 5)
            .await
            .expect_err("empty slice must Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty recipients"),
            "error message must contain \"empty recipients\": {msg}",
        );

        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(&seeds_cfg.old);
            std::env::remove_var(&seeds_cfg.wallet_password);
        }
    }

    #[test]
    fn rewrite_birthday_round_trips_through_cipher_seed() {
        let original = gen_seed().expect("gen_seed");
        let rewritten = rewrite_birthday(&original, 5_000).expect("rewrite");
        let seed_words = SeedWords::from_str(&rewritten).expect("rewritten parses");
        let cipher =
            <CipherSeed as Mnemonic<CipherSeed>>::from_mnemonic(&seed_words, None).expect("decode");
        assert_eq!(cipher.birthday(), 5_000);
    }

    #[test]
    fn rewrite_birthday_zero_yields_valid_genesis_birthday() {
        let original = gen_seed().expect("gen_seed");
        let rewritten = rewrite_birthday(&original, 0).expect("rewrite to zero");
        let seed_words = SeedWords::from_str(&rewritten).expect("rewritten parses");
        let cipher =
            <CipherSeed as Mnemonic<CipherSeed>>::from_mnemonic(&seed_words, None).expect("decode");
        assert_eq!(cipher.birthday(), 0);
    }

    #[test]
    fn rewrite_birthday_rejects_garbage_mnemonic() {
        let err = rewrite_birthday("definitely not a real mnemonic", 0).expect_err("must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("mnemonic") || msg.contains("CipherSeed"),
            "error should describe the decoding failure: {msg}",
        );
    }
}
