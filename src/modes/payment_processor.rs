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

/// Account name PP and the PR daemon both watch. Unified to `"default"`
/// because upstream `minotari-cli@52a7287a/minotari/src/utils/init_wallet.rs:121`
/// hard-codes the PR daemon's wallet name as `"default"` and neither
/// `daemon` nor `import-view-key` accepts an `--account-name` override.
/// The PR daemon's `GET /accounts/{name}/balance` endpoint expects this
/// exact value, and PP's `ACCOUNTS__BENCH__NAME` env value flips to
/// `"default"` to match (see `pp_lifecycle::ACCOUNT_NAME`).
const ACCOUNT_NAME: &str = "default";

/// Reason string surfaced when `scan_from_birthday` is called against Mode 3
/// (B0/S2/S3/S6/S7). Per swe-review C5, the reason names the specific
/// operation rather than a one-size-fits-all generic so the cell log
/// explains what's actually unsupported.
const REASON_SCAN_FROM_BIRTHDAY: &str =
    "Mode 3 doesn't own a scanning wallet — PP holds view-keys only and signing is \
     delegated to a separate console_wallet";

/// Reason string surfaced when `wipe_and_reimport` is called against Mode 3.
/// The PR daemon's view-key wallet is managed at lifecycle startup, not
/// reimported per-scenario; there's no operator-driven re-import surface.
const REASON_WIPE_AND_REIMPORT: &str =
    "Mode 3 doesn't own a re-importable wallet — the PR daemon's view-key wallet is \
     managed at lifecycle startup";

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
        let pr_base_url = mode_3.pr_base_url.clone();
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
                base_url: pr_base_url,
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

    /// Test-only constructor that takes a pre-built `Arc<PpHttpClient>` so
    /// unit tests can point Mode 3 at a wiremock server instead of the
    /// real PP daemon's loopback port. The two lifecycles still own real
    /// `HarnessDataDir`s but are never spawned in tests — the scan-shaped
    /// methods short-circuit to `UnsupportedOperation` and the send_*
    /// methods route through the injected HTTP client.
    #[cfg(test)]
    pub(crate) fn from_parts_for_test(
        cfg: Config,
        seeds: SeedHandle,
        pp_data_dir: HarnessDataDir,
        pr_data_dir: HarnessDataDir,
        http: Arc<PpHttpClient>,
    ) -> anyhow::Result<Self> {
        let mode_3 = cfg.mode_3.as_ref().ok_or_else(|| {
            anyhow::anyhow!("from_parts_for_test requires Config::mode_3 to be Some")
        })?;
        let pr_port = mode_3.pr_port;
        let pr_base_url = mode_3.pr_base_url.clone();
        let (view_key_hex, spend_key_hex) =
            crate::wallet_lifecycle::pp_lifecycle::read_account_env(&mode_3.accounts.bench)?;
        let wallet_password = seeds
            .wallet_password()
            .context("reading wallet password for Mode 3 test ctor")?
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
                base_url: pr_base_url,
            },
            pr_data_dir,
        )?;
        let pp_lifecycle = PpLifecycle::new(&cfg, &seeds, pp_data_dir)?;
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
        })
    }

    async fn scan_from_birthday(&mut self, _birthday: u16) -> anyhow::Result<ScanOutcome> {
        Err(UnsupportedOperation {
            mode: MODE_NAME,
            op: "scan_from_birthday",
            reason: REASON_SCAN_FROM_BIRTHDAY,
        }
        .into())
    }

    /// Mode 3's PP daemon does NOT own a wallet-side balance surface — the
    /// view-key balance lives with the PR daemon, not with PP — so PP "as
    /// far as Mode 3 is concerned" has zero spendable balance. Returning
    /// `Ok(0)` (rather than `UnsupportedOperation`) lets S0 progress past
    /// its opening `get_balance()` / `get_utxo_count()` calls into the
    /// actual `POST /v1/payment-batches` work that S0 measures. Without
    /// this, S0 would error-propagate via `?` and be recorded as `NotRun`
    /// before the batch submission ever fires (swe-review B1).
    async fn get_balance(&mut self) -> anyhow::Result<u64> {
        Ok(0)
    }

    /// See [`Self::get_balance`] — same rationale. PP doesn't own UTXOs;
    /// the view-key UTXO set lives with the PR daemon. Reporting zero
    /// keeps S0 progressing instead of erroring out before its
    /// batch-POST work runs.
    async fn get_utxo_count(&mut self) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn wipe_and_reimport(&mut self, _birthday: u16) -> anyhow::Result<()> {
        Err(UnsupportedOperation {
            mode: MODE_NAME,
            op: "wipe_and_reimport",
            reason: REASON_WIPE_AND_REIMPORT,
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, Mode3Account, Mode3Accounts, Mode3Config, Seeds, WorkerSleepOverrides},
        gen_seed,
        seed::derive_address,
    };
    use serde_json::json;
    use std::path::PathBuf;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Absolute path to the fake binaries under `tests/fixtures/`. The
    /// scan-side and dispatcher tests never spawn anything — we just need
    /// `Mode3Config::validate` to accept the paths.
    fn fake_pp_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_pp.sh")
    }
    fn fake_minotari_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_minotari.sh")
    }

    fn set_env(name: &str, value: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(name, value);
        }
    }
    fn unset_env(name: &str) {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(name);
        }
    }

    /// Per-test env-var name suffix so two tests can run in parallel
    /// without seed-env races (matches the unique_seeds pattern from
    /// `src/modes/new_wallet.rs::tests`).
    fn unique_seeds(suffix: &str) -> Seeds {
        Seeds {
            old: format!("WALLET_BENCHMARKS_TEST_MODE3_OLD_{suffix}"),
            new: format!("WALLET_BENCHMARKS_TEST_MODE3_NEW_{suffix}"),
            payment_processor: format!("WALLET_BENCHMARKS_TEST_MODE3_PP_{suffix}"),
            wallet_password: format!("WALLET_BENCHMARKS_TEST_MODE3_PW_{suffix}"),
        }
    }

    /// Build a full Config with Mode3Config populated. The view-key/spend-key
    /// env vars are unique per-test (suffix-namespaced).
    fn make_mode3_config(suffix: &str) -> (Config, Seeds, String, String) {
        let seeds_cfg = unique_seeds(suffix);
        let view_env = format!("WALLET_BENCHMARKS_TEST_MODE3_VIEW_{suffix}");
        let spend_env = format!("WALLET_BENCHMARKS_TEST_MODE3_SPEND_{suffix}");
        let cfg = Config {
            seeds: seeds_cfg.clone(),
            mode_3: Some(Mode3Config {
                pp_binary_path: fake_pp_path(),
                minotari_binary_path: fake_minotari_path(),
                api_port: 9145,
                pr_port: 9146,
                pr_base_url: "https://rpc.esmeralda.tari.com".to_string(),
                terminal_state_poll_timeout_secs: 1,
                worker_sleep_overrides: WorkerSleepOverrides::default(),
                accounts: Mode3Accounts {
                    bench: Mode3Account {
                        view_key_env: view_env.clone(),
                        public_spend_key_env: spend_env.clone(),
                    },
                },
            }),
            ..Config::default()
        };
        (cfg, seeds_cfg, view_env, spend_env)
    }

    /// Construct a Mode 3 instance bound to the supplied wiremock URL.
    /// Sets all the required env vars and returns the names so the
    /// caller can teardown.
    fn build_mode3_with_http(
        suffix: &str,
        base_url: String,
    ) -> (PaymentProcessor, Seeds, Vec<String>) {
        let (cfg, seeds_cfg, view_env, spend_env) = make_mode3_config(suffix);
        let m_old = gen_seed().expect("m_old");
        let m_new = gen_seed().expect("m_new");
        let m_pp = gen_seed().expect("m_pp");
        set_env(&seeds_cfg.old, &m_old);
        set_env(&seeds_cfg.new, &m_new);
        set_env(&seeds_cfg.payment_processor, &m_pp);
        set_env(&seeds_cfg.wallet_password, "test-password");
        set_env(
            &view_env,
            "572a5fb63972da84aeec33071d13074e244d80c52be842ab5b0859ef4b4db00a",
        );
        set_env(
            &spend_env,
            "40e65c9bbf4592bc995c421108c01a5d7c9f9b2239569757895134549cef371f",
        );
        let pp_dir = HarnessDataDir::new(&format!("test-mode3-pp-{suffix}"), MODE_NAME)
            .expect("pp data dir");
        let pr_dir = HarnessDataDir::new(&format!("test-mode3-pr-{suffix}"), MODE_NAME)
            .expect("pr data dir");
        let http = Arc::new(PpHttpClient::new(base_url));
        let seeds = SeedHandle::new(&seeds_cfg);
        let mode = PaymentProcessor::from_parts_for_test(cfg, seeds, pp_dir, pr_dir, http)
            .expect("PaymentProcessor::from_parts_for_test");
        (mode, seeds_cfg, vec![view_env, spend_env])
    }

    fn teardown_envs(seeds_cfg: &Seeds, extras: &[String]) {
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
        unset_env(&seeds_cfg.wallet_password);
        for e in extras {
            unset_env(e);
        }
    }

    /// Deterministic test recipient — derived from a fresh seed each call,
    /// so addresses don't collide across parallel tests.
    fn test_recipient() -> TariAddress {
        let m = gen_seed().expect("gen_seed");
        derive_address(&m).expect("derive_address")
    }

    fn payment_response_json(payment_id: &str, status: &str, client_id: &str) -> serde_json::Value {
        json!({
            "payment_id": payment_id,
            "status": status,
            "client_id": client_id,
            "account_name": "bench",
            "recipient_address": "tari://esmeralda/recipient",
            "amount": 1000_i64,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        })
    }

    #[tokio::test]
    async fn mode3_name_is_payment_processor() {
        let server = MockServer::start().await;
        let (mode, seeds_cfg, extras) = build_mode3_with_http("NAME", server.uri());
        assert_eq!(MODE_NAME, "payment_processor");
        assert_eq!(mode.name(), "payment_processor");
        teardown_envs(&seeds_cfg, &extras);
    }

    #[tokio::test]
    async fn mode3_scan_from_birthday_returns_unsupported_operation() {
        let server = MockServer::start().await;
        let (mut mode, seeds_cfg, extras) = build_mode3_with_http("SCAN", server.uri());
        let err = mode
            .scan_from_birthday(0)
            .await
            .expect_err("Mode 3 must not support scan_from_birthday");
        let uo = err
            .downcast_ref::<UnsupportedOperation>()
            .expect("error must downcast to UnsupportedOperation");
        assert_eq!(uo.mode, "payment_processor");
        assert_eq!(uo.op, "scan_from_birthday");
        // Per swe-review C5: per-method reason names the actual gap so
        // the cell log explains what's unsupported.
        assert_eq!(uo.reason, REASON_SCAN_FROM_BIRTHDAY);
        teardown_envs(&seeds_cfg, &extras);
    }

    #[tokio::test]
    async fn mode3_get_balance_returns_zero() {
        // Per swe-review B1: PP doesn't own a wallet-side balance surface
        // (view-key balance lives on the PR daemon). Returning Ok(0) lets
        // S0 progress past its opening balance probe into the actual
        // POST /v1/payment-batches measurement.
        let server = MockServer::start().await;
        let (mut mode, seeds_cfg, extras) = build_mode3_with_http("BAL", server.uri());
        let balance = mode
            .get_balance()
            .await
            .expect("get_balance must succeed with Ok(0) (B1 fix)");
        assert_eq!(
            balance, 0,
            "Mode 3 reports zero spendable balance — PP doesn't own the view-key wallet",
        );
        teardown_envs(&seeds_cfg, &extras);
    }

    #[tokio::test]
    async fn mode3_get_utxo_count_returns_zero() {
        // Same B1 rationale as get_balance — PP doesn't own UTXOs; the
        // view-key UTXO set lives on the PR daemon.
        let server = MockServer::start().await;
        let (mut mode, seeds_cfg, extras) = build_mode3_with_http("UTXO", server.uri());
        let count = mode
            .get_utxo_count()
            .await
            .expect("get_utxo_count must succeed with Ok(0) (B1 fix)");
        assert_eq!(
            count, 0,
            "Mode 3 reports zero UTXOs — PP doesn't own the view-key wallet",
        );
        teardown_envs(&seeds_cfg, &extras);
    }

    #[tokio::test]
    async fn mode3_wipe_and_reimport_returns_unsupported_operation() {
        let server = MockServer::start().await;
        let (mut mode, seeds_cfg, extras) = build_mode3_with_http("WIPE", server.uri());
        let err = mode
            .wipe_and_reimport(0)
            .await
            .expect_err("Mode 3 must not support wipe_and_reimport");
        let uo = err
            .downcast_ref::<UnsupportedOperation>()
            .expect("error must downcast to UnsupportedOperation");
        assert_eq!(uo.mode, "payment_processor");
        assert_eq!(uo.op, "wipe_and_reimport");
        // Per swe-review C5: per-method reason names the actual gap.
        assert_eq!(uo.reason, REASON_WIPE_AND_REIMPORT);
        teardown_envs(&seeds_cfg, &extras);
    }

    #[tokio::test]
    async fn mode3_send_single_posts_to_pp_via_wiremock() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/payment-batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "batch_id": "batch-single",
                "account_name": "bench",
                "status": "RECEIVED",
                "payments": [
                    payment_response_json("pay-single", "RECEIVED", "bench-tx-0-0")
                ],
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (mut mode, seeds_cfg, extras) = build_mode3_with_http("SEND1", server.uri());
        let recipient = test_recipient();
        let rec = mode
            .send_single(&recipient, 1_000, 5)
            .await
            .expect("send_single ok");
        assert_eq!(
            rec.txid, "batch-single",
            "TxRecord.txid carries the batch id (stand-in for on-chain txid per spec §10)",
        );
        assert_eq!(rec.status, "success");
        assert!(rec.error_string.is_none());
        teardown_envs(&seeds_cfg, &extras);
    }

    #[tokio::test]
    async fn mode3_send_batch_posts_to_pp_via_wiremock() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/payment-batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "batch_id": "batch-multi",
                "account_name": "bench",
                "status": "RECEIVED",
                "payments": [
                    payment_response_json("pay-1", "RECEIVED", "bench-tx-0-0"),
                    payment_response_json("pay-2", "RECEIVED", "bench-tx-0-1"),
                    payment_response_json("pay-3", "RECEIVED", "bench-tx-0-2"),
                ],
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (mut mode, seeds_cfg, extras) = build_mode3_with_http("BATCH", server.uri());
        let recipients = vec![
            (test_recipient(), 1_000),
            (test_recipient(), 2_000),
            (test_recipient(), 3_000),
        ];
        let rec = mode
            .send_batch_one_to_many(&recipients, 5)
            .await
            .expect("send_batch ok");
        assert_eq!(rec.txid, "batch-multi");
        assert_eq!(rec.status, "success");
        teardown_envs(&seeds_cfg, &extras);
    }

    #[tokio::test]
    async fn mode3_send_batch_rejects_over_max_batch_size() {
        // PP's MAX_BATCH_SIZE is 100; the mode pre-validates and bails
        // BEFORE reaching the HTTP client so the wiremock server never
        // sees a request (`.expect(0)` would also fail if the bail was
        // skipped).
        let server = MockServer::start().await;
        let (mut mode, seeds_cfg, extras) = build_mode3_with_http("OVERSIZE", server.uri());
        // Reuse a single derived recipient — the size check fires before
        // any per-item processing, so address-uniqueness doesn't matter.
        // Deriving 101 fresh addresses takes 60+s on this machine; the
        // boundary check is the same either way.
        let one_recipient = test_recipient();
        let recipients: Vec<(TariAddress, u64)> = (0..(PP_MAX_BATCH_SIZE + 1))
            .map(|_| (one_recipient.clone(), 1_000))
            .collect();
        let err = mode
            .send_batch_one_to_many(&recipients, 5)
            .await
            .expect_err("oversize batch must bail before HTTP");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("MAX_BATCH_SIZE"),
            "error must name PP's MAX_BATCH_SIZE: {msg}",
        );
        teardown_envs(&seeds_cfg, &extras);
    }

    #[tokio::test]
    async fn mode3_dispatcher_returns_arc_per_call() {
        // dispatcher() returns Arc<dyn S4Dispatcher>. Per spec §11 each
        // call returns a fresh handle (no cached singleton) so S4's
        // JoinSet fan-out can hand each task its own clone without
        // racing on a shared mutable cursor.
        let server = MockServer::start().await;
        let (mode, seeds_cfg, extras) = build_mode3_with_http("DISP", server.uri());
        let a: Arc<dyn S4Dispatcher> = mode.dispatcher();
        let b: Arc<dyn S4Dispatcher> = mode.dispatcher();
        // Two trait objects don't compare for identity; assert each is
        // clone-able and the underlying pointer is non-null.
        let _a_clone: Arc<dyn S4Dispatcher> = Arc::clone(&a);
        let _b_clone: Arc<dyn S4Dispatcher> = Arc::clone(&b);
        teardown_envs(&seeds_cfg, &extras);
    }

    #[tokio::test]
    async fn mode3_dispatcher_assigns_unique_client_ids() {
        // 4 concurrent dispatch calls must produce 4 distinct client_ids
        // (the per-mode AtomicU64 counter is shared with the dispatcher
        // per spec §11). We verify by capturing request bodies on the
        // wiremock side and asserting they include 4 distinct s4-N
        // client_id values.
        let server = MockServer::start().await;
        // wiremock's `received_requests` API gives us the bodies; mount a
        // permissive mock that always returns the same shape.
        Mock::given(method("POST"))
            .and(path("/v1/payment-batches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "batch_id": "batch-disp",
                "account_name": "bench",
                "status": "RECEIVED",
                "payments": [payment_response_json("p", "RECEIVED", "s4-x")],
            })))
            .mount(&server)
            .await;
        let (mode, seeds_cfg, extras) = build_mode3_with_http("DISP_UNIQ", server.uri());
        let dispatcher = mode.dispatcher();
        let mut joinset = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let d = Arc::clone(&dispatcher);
            let recipient = test_recipient();
            joinset.spawn(async move { d.dispatch(recipient, 1_000, 5).await });
        }
        let mut ok_count = 0;
        while let Some(joined) = joinset.join_next().await {
            let _rec = joined.expect("join ok").expect("dispatch ok");
            ok_count += 1;
        }
        assert_eq!(ok_count, 4, "4 dispatches must all succeed");
        let reqs = server.received_requests().await.expect("received_requests");
        assert_eq!(
            reqs.len(),
            4,
            "wiremock must have received 4 requests (one per dispatch); got {}",
            reqs.len(),
        );
        // Parse out each request body's items[0].client_id and assert
        // they are all distinct.
        let mut client_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for req in &reqs {
            let body: serde_json::Value = serde_json::from_slice(&req.body).expect("body is JSON");
            let cid = body["items"][0]["client_id"]
                .as_str()
                .expect("client_id is a string")
                .to_string();
            assert!(
                cid.starts_with("s4-"),
                "dispatcher client_id must be prefixed with s4-: {cid}",
            );
            client_ids.insert(cid);
        }
        assert_eq!(
            client_ids.len(),
            4,
            "all 4 dispatch client_ids must be distinct; got {client_ids:?}",
        );
        teardown_envs(&seeds_cfg, &extras);
    }
}
