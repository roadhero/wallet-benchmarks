//! Mode 3 — `payment_processor`.
//!
//! Per `analysis/specs/MODE_3_REWORK_SPEC.md`, Mode 3 is a true three-process
//! orchestrator: the harness spawns the `minotari_payment_processor` daemon
//! (PP), a `minotari daemon` (PR) child for the view-key wallet PP queries,
//! and drives PP over HTTP. The old shim that reused Mode 2's offline-sign
//! pipeline is gone — Mode 3 no longer shares the `minotari` subprocess
//! pipeline used by Modes 1 and 2.
//!
//! Scenario coverage (spec §7):
//!
//! | Scenario | Mode 3 |
//! |---|---|
//! | B0 (scan)                       | NotRun — Skipped (no scanning wallet) |
//! | S0 (single send)                | runs — POST batch of 1 to PP |
//! | S1 (volume, confirm-loop)       | NotRun — Skipped (v4/v5 mismatch) |
//! | S2 (full rescan)                | NotRun — Skipped (no scanning wallet) |
//! | S3 (birthday rescan)            | NotRun — Skipped (no scanning wallet) |
//! | S4 (concurrent)                 | runs — Arc<PpHttpClient> shared |
//! | S5 (throughput, both arms)      | runs — POST batches |
//! | S6 (rescan after)               | NotRun — Skipped (no scanning wallet) |
//! | S7 (rescan after)               | NotRun — Skipped (no scanning wallet) |
//!
//! Scan-shaped methods (`scan_from_birthday`, `get_balance`,
//! `get_utxo_count`, `wipe_and_reimport`) return
//! [`crate::modes::UnsupportedOperation`] so the runner records the cell as
//! [`crate::result_profile::CellResult::NotRun`] per spec §7. The
//! [`Mode::send_single`] and [`Mode::send_batch_one_to_many`] paths POST to
//! PP via [`crate::pp_http_client::PpHttpClient`].

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use anyhow::Context;
use tari_common_types::tari_address::TariAddress;

use crate::{
    config::Config,
    modes::{Mode, S4Dispatcher, ScanOutcome, TxRecord, UnsupportedOperation},
    pp_http_client::{BulkPaymentItem, PpHttpClient},
    seed::SeedHandle,
    wallet_lifecycle::{pp_lifecycle::PpLifecycle, pr_lifecycle::PrLifecycle, HarnessDataDir},
};

const LOG_TARGET: &str = "c::modes::payment_processor";

/// Mode 3's name as it appears in `results.<name>` of the result profile.
pub const MODE_NAME: &str = "payment_processor";

/// PP's MAX_BATCH_SIZE — verified at `vendor/.../src/lib.rs` per
/// `MODE_3_REWORK_SPEC.md §10`.
pub const PP_MAX_BATCH_SIZE: usize = 100;

/// Account name PP and the PR daemon both watch. Mirrors
/// `ACCOUNTS__BENCH__NAME=bench` on PP's spawn env.
const ACCOUNT_NAME: &str = "bench";

/// Common reason string for every Skipped-by-Mode-3 scenario, per spec §7.
const UNSUPPORTED_REASON: &str =
    "Mode 3 (payment_processor) does not own a scanning wallet; the daemon holds view-keys \
     only and signing is delegated to a separate console_wallet instance. Scenarios that \
     exercise the scanning code path do not apply.";

/// Mode 3 — `payment_processor`.
///
/// Owns the PP lifecycle (a child process), the PR lifecycle (a second child
/// process), and the `Arc<PpHttpClient>` used both for direct submission and
/// for the dispatcher returned to S4. The struct does NOT spawn either
/// lifecycle on construction; the run-loop is expected to call
/// [`PaymentProcessor::start_external_services`] before scenarios begin and
/// [`PaymentProcessor::shutdown`] after they end.
pub struct PaymentProcessor {
    /// Harness configuration (`Config::mode_3` reachable via `cfg.mode_3`).
    cfg: Config,
    /// PR daemon lifecycle (held until shutdown).
    pr_lifecycle: PrLifecycle,
    /// PP daemon lifecycle (held until shutdown).
    pp_lifecycle: PpLifecycle,
    /// Shared HTTP client to PP. Cloned via `Arc::clone` for S4 fan-out.
    http: Arc<PpHttpClient>,
    /// Per-mode tx-index counter — shared with the S4 dispatcher so per-task
    /// `client_id` strings stay unique across concurrent submissions.
    tx_idx: Arc<AtomicU64>,
    /// Submitted payment ids — collected for the shutdown poll loop's
    /// terminal-state recording.
    submitted_payment_ids: Vec<String>,
}

impl PaymentProcessor {
    /// Construct Mode 3 from the harness config + seeds + the two per-mode
    /// data dirs (one for PP, one for PR — distinct so sqlite locks cannot
    /// collide).
    ///
    /// Validates `Config::mode_3` is present and reads the account env vars
    /// at construction so a missing var bails before any IO.
    pub fn new(
        cfg: Config,
        seeds: SeedHandle,
        pp_data_dir: HarnessDataDir,
        pr_data_dir: HarnessDataDir,
    ) -> anyhow::Result<Self> {
        let mode_3 = cfg.mode_3.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Config::mode_3 is required for Mode 3 (payment_processor); see \
                 analysis/specs/MODE_3_REWORK_SPEC.md §12"
            )
        })?;
        let pr_port = mode_3.pr_port;
        let (view_key_hex, spend_key_hex) =
            crate::wallet_lifecycle::pp_lifecycle::read_account_env(&mode_3.accounts.bench)?;
        let wallet_password = seeds
            .wallet_password()
            .context("reading wallet password for Mode 3")?
            .reveal()
            .to_string();
        let pr_lifecycle = PrLifecycle::new(
            crate::wallet_lifecycle::pr_lifecycle::PrLifecycleConfig {
                minotari_binary: mode_3.minotari_binary_path.clone(),
                network: cfg.network.clone(),
                view_private_key_hex: view_key_hex,
                spend_public_key_hex: spend_key_hex,
                wallet_password,
                port: pr_port,
            },
            pr_data_dir,
        )?;
        let pp_lifecycle = PpLifecycle::new(&cfg, &seeds, pp_data_dir)?;
        let http = Arc::new(pp_lifecycle.http_client_owned());
        Ok(Self {
            cfg,
            pr_lifecycle,
            pp_lifecycle,
            http,
            tx_idx: Arc::new(AtomicU64::new(0)),
            submitted_payment_ids: Vec::new(),
        })
    }

    /// Spawn the PR and PP children, apply the vendored sqlite migrations,
    /// and wait for both readiness probes. Order matches spec §2 step 4:
    /// PR before PP so PP's startup can connect immediately. Migrations
    /// land between the two so PP's `DATABASE_URL` open succeeds.
    pub async fn start_external_services(&mut self) -> anyhow::Result<()> {
        log::info!(
            target: LOG_TARGET,
            "starting Mode 3 external services (PR + migrations + PP)",
        );
        self.pr_lifecycle
            .spawn()
            .await
            .context("spawning PR daemon")?;
        crate::pp_migrations::apply_migrations(self.pp_lifecycle.data_dir_path())
            .context("applying PP migrations")?;
        self.pp_lifecycle
            .spawn()
            .await
            .context("spawning PP daemon")?;
        log::info!(target: LOG_TARGET, "Mode 3 external services ready");
        Ok(())
    }

    /// Poll every submitted payment id to a terminal state (or the
    /// configured deadline), then teardown both child processes.
    ///
    /// Implements spec §9's shutdown sequence:
    /// 1. Caller has stopped invoking `send_*`.
    /// 2. Poll all submitted payment ids to terminal.
    /// 3. PP teardown (SIGTERM, 5s grace, SIGKILL).
    /// 4. PR teardown (SIGTERM, 5s grace, SIGKILL).
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        let timeout_secs = self
            .cfg
            .mode_3
            .as_ref()
            .map(|m| m.terminal_state_poll_timeout_secs)
            .unwrap_or(60);
        log::info!(
            target: LOG_TARGET,
            "shutdown: polling {} submitted payment(s) for terminal state (timeout {timeout_secs}s)",
            self.submitted_payment_ids.len(),
        );
        // `tokio::select!` poll loop. The two sleep arms (`sleep_until`
        // deadline and `sleep(POLL_INTERVAL)` cadence) live inside the
        // select body and are exempt from the AC-32 ban per the carve-out
        // documented at `tests/c_no_retry_backoff_throttle.rs:22-25`. The
        // cadence sleep is a poll-interval bound (the loop must
        // periodically re-check terminal state), not a throttle or
        // backoff — once the cadence elapses, the next iteration polls
        // every still-non-terminal payment id without retry of
        // already-completed ones.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let poll_interval = std::time::Duration::from_millis(500);
        let mut terminal: std::collections::HashSet<String> = std::collections::HashSet::new();
        while terminal.len() < self.submitted_payment_ids.len() {
            let mut timed_out = false;
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    timed_out = true;
                }
                _ = tokio::time::sleep(poll_interval) => {
                    for id in &self.submitted_payment_ids {
                        if terminal.contains(id) {
                            continue;
                        }
                        match self.http.poll_payment(id).await {
                            Ok(p) if p.status.is_terminal() => {
                                terminal.insert(id.clone());
                            }
                            Ok(_) => { /* still in progress; loop again */ }
                            Err(e) => {
                                log::debug!(
                                    target: LOG_TARGET,
                                    "shutdown poll {id} failed: {e:#}",
                                );
                            }
                        }
                    }
                }
            }
            if timed_out {
                log::warn!(
                    target: LOG_TARGET,
                    "shutdown: {} payment(s) not terminal after {timeout_secs}s; proceeding to teardown",
                    self.submitted_payment_ids.len() - terminal.len(),
                );
                break;
            }
        }
        self.pp_lifecycle.teardown().await.context("PP teardown")?;
        self.pr_lifecycle.teardown().await.context("PR teardown")?;
        Ok(())
    }

    /// Advance the per-mode tx index and return the previous value.
    fn next_tx_idx(&self) -> u64 {
        self.tx_idx.fetch_add(1, Ordering::SeqCst)
    }

    /// Convert a list of (TariAddress, u64) tuples into PP's
    /// [`BulkPaymentItem`] shape. `client_id` is the per-batch idempotency
    /// key — assigned `{tag}-{batch_idx}-{item_idx}` so a concurrent
    /// dispatcher and the sequential scenario API don't collide.
    fn items_for_batch(
        tag: &str,
        batch_idx: u64,
        recipients: &[(TariAddress, u64)],
    ) -> anyhow::Result<Vec<BulkPaymentItem>> {
        recipients
            .iter()
            .enumerate()
            .map(|(item_idx, (addr, amt))| {
                let amount = i64::try_from(*amt)
                    .with_context(|| format!("amount {amt} exceeds i64::MAX for PP submission"))?;
                Ok(BulkPaymentItem {
                    client_id: format!("{tag}-{batch_idx}-{item_idx}"),
                    recipient_address: addr.to_base58(),
                    amount,
                    payment_id: None,
                })
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl Mode for PaymentProcessor {
    fn name(&self) -> &'static str {
        MODE_NAME
    }

    async fn send_single(
        &mut self,
        recipient: &TariAddress,
        amount_microtari: u64,
        _fee_rate: u64,
    ) -> anyhow::Result<TxRecord> {
        let idx = self.next_tx_idx();
        let items =
            Self::items_for_batch("bench-tx", idx, &[(recipient.clone(), amount_microtari)])?;
        let started = Instant::now();
        let resp = self
            .http
            .submit_batch(ACCOUNT_NAME, items)
            .await
            .context("Mode 3 send_single submit_batch")?;
        let t = started.elapsed().as_millis() as u64;
        for p in &resp.payments {
            self.submitted_payment_ids.push(p.payment_id.clone());
        }
        log::info!(
            target: LOG_TARGET,
            "Mode 3 send_single idx={idx} batch_id={} -> {} payment(s) in {t}ms",
            resp.batch_id,
            resp.payments.len(),
        );
        // batch_id stands in for the on-chain txid until the signer succeeds
        // (per spec §10). t_confirm_ms stays None — the per-payment terminal
        // state is established by the shutdown poll loop, not per-send.
        Ok(TxRecord {
            txid: resp.batch_id.clone(),
            t_total_ms: t,
            t_broadcast_ms: t,
            t_confirm_ms: None,
            status: "success".to_string(),
            error_string: None,
            fee_microtari: 0,
            sub_segments_ms: None,
        })
    }

    async fn send_batch_one_to_many(
        &mut self,
        recipients: &[(TariAddress, u64)],
        _fee_rate: u64,
    ) -> anyhow::Result<TxRecord> {
        anyhow::ensure!(
            recipients.len() <= PP_MAX_BATCH_SIZE,
            "Mode 3 batch size {} exceeds PP MAX_BATCH_SIZE={PP_MAX_BATCH_SIZE}",
            recipients.len(),
        );
        let idx = self.next_tx_idx();
        let items = Self::items_for_batch("bench-tx", idx, recipients)?;
        let started = Instant::now();
        let resp = self
            .http
            .submit_batch(ACCOUNT_NAME, items)
            .await
            .context("Mode 3 send_batch_one_to_many submit_batch")?;
        let t = started.elapsed().as_millis() as u64;
        for p in &resp.payments {
            self.submitted_payment_ids.push(p.payment_id.clone());
        }
        log::info!(
            target: LOG_TARGET,
            "Mode 3 send_batch_one_to_many idx={idx} K={} batch_id={} in {t}ms",
            recipients.len(),
            resp.batch_id,
        );
        Ok(TxRecord {
            txid: resp.batch_id.clone(),
            t_total_ms: t,
            t_broadcast_ms: t,
            t_confirm_ms: None,
            status: "success".to_string(),
            error_string: None,
            fee_microtari: 0,
            sub_segments_ms: None,
        })
    }

    async fn scan_from_birthday(&mut self, _birthday: u16) -> anyhow::Result<ScanOutcome> {
        Err(UnsupportedOperation {
            mode: MODE_NAME,
            op: "scan_from_birthday",
            reason: UNSUPPORTED_REASON,
        }
        .into())
    }

    async fn get_balance(&mut self) -> anyhow::Result<u64> {
        Err(UnsupportedOperation {
            mode: MODE_NAME,
            op: "get_balance",
            reason: UNSUPPORTED_REASON,
        }
        .into())
    }

    async fn get_utxo_count(&mut self) -> anyhow::Result<u64> {
        Err(UnsupportedOperation {
            mode: MODE_NAME,
            op: "get_utxo_count",
            reason: UNSUPPORTED_REASON,
        }
        .into())
    }

    async fn wipe_and_reimport(&mut self, _birthday: u16) -> anyhow::Result<()> {
        Err(UnsupportedOperation {
            mode: MODE_NAME,
            op: "wipe_and_reimport",
            reason: UNSUPPORTED_REASON,
        }
        .into())
    }

    fn dispatcher(&self) -> Arc<dyn S4Dispatcher> {
        Arc::new(PpDispatcher {
            http: Arc::clone(&self.http),
            counter: Arc::clone(&self.tx_idx),
        })
    }
}

/// Concurrent dispatch handle for Mode 3's S4 implementation.
///
/// Holds `Arc<PpHttpClient>` (cloned across tasks; the inner
/// `reqwest::Client` is internally Arc-shared and concurrency-safe per spec
/// §11) and the parent `PaymentProcessor`'s tx-index counter so per-task
/// `client_id` strings don't collide with sequential `send_*` calls or with
/// other concurrent tasks.
pub struct PpDispatcher {
    http: Arc<PpHttpClient>,
    counter: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl S4Dispatcher for PpDispatcher {
    async fn dispatch(
        &self,
        recipient: TariAddress,
        amount_microtari: u64,
        _fee_rate: u64,
    ) -> anyhow::Result<TxRecord> {
        let idx = self.counter.fetch_add(1, Ordering::SeqCst);
        let amount = i64::try_from(amount_microtari).with_context(|| {
            format!("amount {amount_microtari} exceeds i64::MAX for PP submission")
        })?;
        let item = BulkPaymentItem {
            client_id: format!("s4-{idx}"),
            recipient_address: recipient.to_base58(),
            amount,
            payment_id: None,
        };
        let started = Instant::now();
        let resp = self
            .http
            .submit_batch(ACCOUNT_NAME, vec![item])
            .await
            .context("Mode 3 PpDispatcher submit_batch")?;
        let t = started.elapsed().as_millis() as u64;
        // S4 doesn't carry a submitted_payment_ids vec back to the parent —
        // the shutdown poll loop only sees ids from sequential send_*
        // calls. This is intentional per spec §11; S4 is a throughput
        // measurement, terminal-state tracking is best-effort there.
        Ok(TxRecord {
            txid: resp.batch_id.clone(),
            t_total_ms: t,
            t_broadcast_ms: t,
            t_confirm_ms: None,
            status: "success".to_string(),
            error_string: None,
            fee_microtari: 0,
            sub_segments_ms: None,
        })
    }
}

#[cfg(test)]
mod tests {
    // TODO(swe-test): populate per MODE_3_REWORK_SPEC.md §13. Test list:
    //   - mode3_name_is_payment_processor
    //   - mode3_scan_from_birthday_returns_unsupported_operation
    //   - mode3_get_balance_returns_unsupported_operation
    //   - mode3_get_utxo_count_returns_unsupported_operation
    //   - mode3_wipe_and_reimport_returns_unsupported_operation
    //   - mode3_send_single_posts_to_pp_via_wiremock
    //   - mode3_send_batch_posts_to_pp_via_wiremock
    //   - mode3_send_batch_rejects_over_max_batch_size
    //   - mode3_dispatcher_returns_arc_pp_dispatcher
    //   - mode3_dispatcher_assigns_unique_client_ids_per_task
    //
    // The cached-read tests from PR #6 (mode3_get_utxo_count_reads_from_wallet_db,
    // mode3_get_utxo_count_surfaces_db_error, mode3_wipe_and_reimport_attempts_subprocess,
    // mode3_scan_from_birthday_attempts_subprocess, mode3_get_balance_attempts_subprocess)
    // are retired with the shim — Mode 3 no longer reads the wallet DB or
    // spawns the minotari subprocess for those operations.
}
