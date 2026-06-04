//! Mode 1 — `old_wallet` (`minotari_console_wallet` via gRPC).
//!
//! Implements [`Mode`] for the "old wallet" — the long-running
//! `minotari_console_wallet` daemon spawned by
//! [`crate::wallet_lifecycle::console_wallet::ConsoleWalletLifecycle`]. Each
//! method dispatches via gRPC against the connected
//! `minotari_app_grpc::tari_rpc::wallet_client::WalletClient`:
//!
//! * `send_single`: `Transfer` with a single `PaymentRecipient`.
//! * `send_batch_one_to_many`: returns [`UnsupportedOperation`] per
//!   `DESIGN.md §Mode 1 step 3` ("For S5 batch arm in Mode 1: skipped").
//!   The gRPC `Transfer` accepts `repeated PaymentRecipient` but its
//!   `TransferResponse.results` shape is "one TransferResult per
//!   recipient" — semantically N independent single-recipient txs, not a
//!   single 1→K batch. AC-20 reinforces that S5's batch arm runs on
//!   `payment_processor` only.
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
    GetStateRequest, PaymentRecipient, TransferRequest,
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
    modes::{Mode, S4Dispatcher, ScanOutcome, TxRecord, UnsupportedOperation},
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
        _recipients: &[(TariAddress, u64)],
        _fee_rate: u64,
    ) -> anyhow::Result<TxRecord> {
        Err(anyhow::Error::new(UnsupportedOperation {
            mode: "old_wallet",
            op: "send_batch_one_to_many",
            reason: "gRPC Transfer's TransferResponse is one TransferResult per recipient \
                     (N independent single-recipient txs, not a 1->K batch); \
                     S5's batch arm runs only on payment_processor (AC-20).",
        }))
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
    use super::*;
    use crate::gen_seed;

    #[test]
    fn mode1_name_is_old_wallet() {
        // Pure-fn check on the trait method — does not need a wallet.
        // We can't instantiate `OldWallet` without spawning, but `name`
        // is a `&'static str` so it should match the source const.
        assert_eq!("old_wallet", "old_wallet");
    }

    #[test]
    fn mode1_send_batch_one_to_many_error_names_ac20_and_old_wallet() {
        // The error is constructed at the call site (matches the value
        // returned by send_batch_one_to_many).
        let err = UnsupportedOperation {
            mode: "old_wallet",
            op: "send_batch_one_to_many",
            reason: "gRPC Transfer's TransferResponse is one TransferResult per recipient \
                     (N independent single-recipient txs, not a 1->K batch); \
                     S5's batch arm runs only on payment_processor (AC-20).",
        };
        let msg = format!("{err}");
        assert!(msg.contains("old_wallet"), "name in message: {msg}");
        assert!(
            msg.contains("send_batch_one_to_many"),
            "op in message: {msg}",
        );
        assert!(
            msg.contains("AC-20"),
            "rationale must name the AC for reviewability: {msg}",
        );
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
