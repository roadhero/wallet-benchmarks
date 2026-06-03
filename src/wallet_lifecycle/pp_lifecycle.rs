//! `minotari_payment_processor` (PP) child-process lifecycle.
//!
//! Per `analysis/specs/MODE_3_REWORK_SPEC.md §1`, `§2`, and `§9`, this
//! module spawns the PP daemon, holds its `Child`, and tears it down on
//! demand (SIGTERM-5s-grace-then-SIGKILL).
//!
//! Configuration is conveyed entirely through environment variables: PP's
//! own [`PaymentProcessorEnv::load`] reads them via the `config` crate's
//! [`Environment::default().separator("__")`] source. The spawn site
//! `env_clear()`s first then sets the matrix listed in spec §2 — `DATABASE_URL`,
//! `TARI_NETWORK`, `PAYMENT_RECEIVER`, `BASE_NODE`, `CONSOLE_WALLET_PATH`,
//! `CONSOLE_WALLET_BASE_PATH`, `CONSOLE_WALLET_PASSWORD`, `LISTEN_IP`,
//! `LISTEN_PORT`, the four worker-sleep keys, `REVEAL_PII`, and the
//! `ACCOUNTS__BENCH__*` triple (name / view_key / public_spend_key). The
//! signer-wallet password and the view/spend keys are *plaintext secrets*
//! and the spawn invocation is the only call site that handles them in
//! Mode 3 — the harness reads them from the env-var names recorded under
//! [`crate::config::Mode3Config`].
//!
//! CWD is confined to the per-mode `HarnessDataDir` so PP's
//! `logs/audit.log` (relative to CWD) stays inside the harness's tree and
//! Drop cleanup removes it (spec §2 stdio rationale).

use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context};
use tokio::{
    process::{Child, Command},
    time::sleep,
};

use crate::{
    config::{Config, Mode3Account, WorkerSleepOverrides},
    pp_http_client::PpHttpClient,
    seed::SeedHandle,
    wallet_lifecycle::HarnessDataDir,
};

const LOG_TARGET: &str = "c::wallet_lifecycle::pp_lifecycle";

/// Network identifier passed via `TARI_NETWORK`. Defense-in-depth re-assertion
/// — `crate::guards::enforce_esmeralda` is the primary gate.
const NETWORK_ALLOWLIST: &str = "esmeralda";

/// Hard-coded bench account name. Mirrors `[mode_3.accounts.bench]` in
/// `harness.toml`; emitted as `ACCOUNTS__BENCH__NAME=bench`. v1 supports
/// a single PP account (per spec §15 out-of-scope note).
const ACCOUNT_NAME: &str = "bench";

/// LISTEN_IP value baked at the lifecycle layer. `127.0.0.1` per spec §2's
/// bench-safety note (PP's own default would bind `0.0.0.0`).
const LISTEN_IP: &str = "127.0.0.1";

/// `REVEAL_PII` value baked at the lifecycle layer. `true` per spec §2 —
/// bench logs are comparable.
const REVEAL_PII: &str = "true";

/// Grace window after SIGTERM before SIGKILL (per spec §9).
const SIGTERM_GRACE: Duration = Duration::from_secs(5);

/// Tick interval inside the SIGTERM grace loop.
const TEARDOWN_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Construction inputs for [`PpLifecycle`]. Built up from `Config` +
/// `SeedHandle` + the Mode 3 data dir; isolated into a struct so future
/// fields don't churn the call site.
pub struct PpLifecycleConfig {
    /// Absolute path to the `minotari_payment_processor` binary.
    pub pp_binary: PathBuf,
    /// Absolute path to the `minotari` (minotari-cli) binary the PP signer
    /// shells out to. Emitted as `CONSOLE_WALLET_PATH`.
    pub console_wallet_binary: PathBuf,
    /// Network identifier (`"esmeralda"`).
    pub network: String,
    /// PR daemon URL (e.g. `"http://127.0.0.1:9146"`). Emitted as
    /// `PAYMENT_RECEIVER`.
    pub pr_url: String,
    /// Base-node URL. Emitted as `BASE_NODE`.
    pub base_node_url: String,
    /// Signer wallet password (revealed plaintext).
    pub wallet_password: String,
    /// PP HTTP listen port (`Config::mode_3.api_port`).
    pub api_port: u16,
    /// Hex view private key (read from `view_key_env` at spawn time).
    pub view_key_hex: String,
    /// Hex public spend key (read from `spend_key_env` at spawn time).
    pub spend_key_hex: String,
    /// PP worker sleep overrides (spec §8).
    pub worker_sleeps: WorkerSleepOverrides,
}

/// Mode 3's PP daemon lifecycle handle.
///
/// Owns the spawned `Child`, the per-mode `HarnessDataDir` (CWD-confined),
/// and the `PpHttpClient` used for the readiness probe. Spawn populates
/// the env matrix from `PpLifecycleConfig`; teardown SIGTERMs, waits 5s,
/// SIGKILLs.
pub struct PpLifecycle {
    cfg: PpLifecycleConfig,
    /// Per-mode data dir — also CWD for the child and the parent dir of
    /// `payments.db` and `signer_wallet/`.
    data_dir: HarnessDataDir,
    /// Pre-constructed PP client, used by [`Self::wait_ready`].
    http: PpHttpClient,
    /// `None` before [`Self::spawn`], `Some` between spawn and teardown.
    child: Option<Child>,
}

impl PpLifecycle {
    /// Construct a lifecycle handle from the harness config + seeds +
    /// data dir. Reads the env vars under
    /// `Config::mode_3.accounts.bench.{view_key_env, public_spend_key_env}`
    /// at construction time so a missing var bails before any IO.
    ///
    /// Does NOT spawn — call [`Self::spawn`] to start the child.
    pub fn new(
        config: &Config,
        seeds: &SeedHandle,
        data_dir: HarnessDataDir,
    ) -> anyhow::Result<Self> {
        if config.network != NETWORK_ALLOWLIST {
            anyhow::bail!(
                "PpLifecycle::new refusing network={} (only '{NETWORK_ALLOWLIST}' is allowed; \
                 see crate::guards::enforce_esmeralda)",
                config.network,
            );
        }
        let mode_3 = config.mode_3.as_ref().ok_or_else(|| {
            anyhow!(
                "PpLifecycle::new requires Config::mode_3 to be set (see \
                 analysis/specs/MODE_3_REWORK_SPEC.md §12)"
            )
        })?;
        let pp_binary = mode_3.pp_binary_path.clone();
        let console_wallet_binary = mode_3.minotari_binary_path.clone();
        let view_key_hex =
            std::env::var(&mode_3.accounts.bench.view_key_env).with_context(|| {
                format!(
                    "reading hex view key from env var ${} (set it before running Mode 3; see \
                 analysis/specs/MODE_3_REWORK_SPEC.md §12)",
                    mode_3.accounts.bench.view_key_env,
                )
            })?;
        let spend_key_hex = std::env::var(&mode_3.accounts.bench.public_spend_key_env)
            .with_context(|| {
                format!(
                    "reading hex public spend key from env var ${} (set it before running Mode 3)",
                    mode_3.accounts.bench.public_spend_key_env,
                )
            })?;
        let wallet_password = seeds
            .wallet_password()
            .context("reading wallet password for PpLifecycle")?
            .reveal()
            .to_string();
        let pr_url = format!("http://127.0.0.1:{}", mode_3.pr_port);
        let api_port = mode_3.api_port;
        let cfg = PpLifecycleConfig {
            pp_binary,
            console_wallet_binary,
            network: config.network.clone(),
            pr_url,
            base_node_url: config.base_node_url.as_str().to_string(),
            wallet_password,
            api_port,
            view_key_hex,
            spend_key_hex,
            worker_sleeps: mode_3.worker_sleep_overrides.clone(),
        };
        let http = PpHttpClient::new(format!("http://127.0.0.1:{api_port}"));
        Ok(Self {
            cfg,
            data_dir,
            http,
            child: None,
        })
    }

    /// Returns `true` between a successful spawn and the next teardown.
    pub fn is_alive(&self) -> bool {
        self.child.is_some()
    }

    /// Borrow the held [`PpHttpClient`] for caller-side submission /
    /// polling. Cloning via `Arc<PpHttpClient>` is the caller's job (see
    /// [`Self::http_client_owned`] for a sharing helper).
    pub fn http(&self) -> &PpHttpClient {
        &self.http
    }

    /// Move-construct a fresh `PpHttpClient` against the same base URL.
    /// Used by `PaymentProcessor::dispatcher` to build the
    /// `Arc<PpHttpClient>` shared across S4's concurrent tasks.
    pub fn http_client_owned(&self) -> PpHttpClient {
        PpHttpClient::new(self.http.base_url().to_string())
    }

    /// Path to the PP data dir (also the CWD passed to the child).
    pub fn data_dir_path(&self) -> &std::path::Path {
        self.data_dir.path()
    }

    /// Path to the signer-wallet base path (subdir of the data dir, distinct
    /// from Mode 1's and Mode 2's data dirs so sqlite locks cannot collide).
    /// Emitted as `CONSOLE_WALLET_BASE_PATH`.
    pub fn signer_wallet_base_path(&self) -> PathBuf {
        self.data_dir.path().join("signer_wallet")
    }

    /// Build the full env-var matrix for the PP spawn. Pulled out as a
    /// pure function on `&self` so unit tests can snapshot the value set
    /// without spawning the binary.
    pub fn build_env(&self) -> Vec<(String, String)> {
        let database_url = format!("sqlite://{}/payments.db", self.data_dir.path().display(),);
        let console_wallet_base_path = self.signer_wallet_base_path().display().to_string();
        let mut env: Vec<(String, String)> = vec![
            ("DATABASE_URL".to_string(), database_url),
            ("TARI_NETWORK".to_string(), self.cfg.network.clone()),
            ("PAYMENT_RECEIVER".to_string(), self.cfg.pr_url.clone()),
            ("BASE_NODE".to_string(), self.cfg.base_node_url.clone()),
            (
                "CONSOLE_WALLET_PATH".to_string(),
                self.cfg.console_wallet_binary.display().to_string(),
            ),
            (
                "CONSOLE_WALLET_BASE_PATH".to_string(),
                console_wallet_base_path,
            ),
            (
                "CONSOLE_WALLET_PASSWORD".to_string(),
                self.cfg.wallet_password.clone(),
            ),
            ("LISTEN_IP".to_string(), LISTEN_IP.to_string()),
            ("LISTEN_PORT".to_string(), self.cfg.api_port.to_string()),
            (
                "ACCOUNTS__BENCH__NAME".to_string(),
                ACCOUNT_NAME.to_string(),
            ),
            (
                "ACCOUNTS__BENCH__VIEW_KEY".to_string(),
                self.cfg.view_key_hex.clone(),
            ),
            (
                "ACCOUNTS__BENCH__PUBLIC_SPEND_KEY".to_string(),
                self.cfg.spend_key_hex.clone(),
            ),
            ("REVEAL_PII".to_string(), REVEAL_PII.to_string()),
        ];
        if let Some(v) = self.cfg.worker_sleeps.batch_creator {
            env.push(("BATCH_CREATOR_SLEEP_SECS".to_string(), v.to_string()));
        }
        if let Some(v) = self.cfg.worker_sleeps.unsigned_tx_creator {
            env.push(("UNSIGNED_TX_CREATOR_SLEEP_SECS".to_string(), v.to_string()));
        }
        if let Some(v) = self.cfg.worker_sleeps.transaction_signer {
            env.push(("TRANSACTION_SIGNER_SLEEP_SECS".to_string(), v.to_string()));
        }
        if let Some(v) = self.cfg.worker_sleeps.broadcaster {
            env.push(("BROADCASTER_SLEEP_SECS".to_string(), v.to_string()));
        }
        if let Some(v) = self.cfg.worker_sleeps.confirmation_checker {
            env.push(("CONFIRMATION_CHECKER_SLEEP_SECS".to_string(), v.to_string()));
        }
        env
    }

    /// Spawn the PP binary and wait for the HTTP readiness probe to return
    /// 200 against `GET /health/version`.
    ///
    /// Idempotent: a second call after a successful spawn is a no-op.
    pub async fn spawn(&mut self) -> anyhow::Result<()> {
        if self.child.is_some() {
            log::debug!(
                target: LOG_TARGET,
                "PpLifecycle::spawn called twice; the child is already running",
            );
            return Ok(());
        }
        // Create the signer-wallet base path subdir before spawning — PP's
        // signer would otherwise fail trying to mkdir-on-write.
        let signer_base = self.signer_wallet_base_path();
        std::fs::create_dir_all(&signer_base)
            .with_context(|| format!("creating signer wallet base at {}", signer_base.display()))?;
        // Per-run stdio capture file under the data dir's logs/.
        let logs_dir = self.data_dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir)
            .with_context(|| format!("creating logs dir at {}", logs_dir.display()))?;
        let log_path = logs_dir.join("pp.log");
        let stdout = std::fs::File::create(&log_path)
            .with_context(|| format!("creating PP log at {}", log_path.display()))?;
        let stderr = stdout
            .try_clone()
            .context("cloning PP log file handle for stderr")?;
        let envs = self.build_env();
        log::info!(
            target: LOG_TARGET,
            "spawning {} (listen={}:{}, cwd={}, logs={})",
            self.cfg.pp_binary.display(),
            LISTEN_IP,
            self.cfg.api_port,
            self.data_dir.path().display(),
            log_path.display(),
        );
        let mut command = Command::new(&self.cfg.pp_binary);
        command
            .current_dir(self.data_dir.path())
            .env_clear()
            // Preserve PATH so PP's `Command::new("minotari_console_wallet")`
            // fallback works on systems without an absolute override.
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env(
                "HOME",
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
            );
        for (k, v) in &envs {
            command.env(k, v);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "spawning {} (is Mode3Config::pp_binary_path correct?)",
                    self.cfg.pp_binary.display(),
                )
            })?;
        self.child = Some(child);
        self.wait_ready().await
    }

    /// Run the readiness probe against `GET /health/version`. Times out per
    /// [`crate::pp_http_client::READINESS_DEADLINE`].
    async fn wait_ready(&mut self) -> anyhow::Result<()> {
        // Same observable child-exit guard as PR's lifecycle: an early PP
        // crash surfaces here instead of spinning until the deadline.
        let deadline = Instant::now() + crate::pp_http_client::READINESS_DEADLINE;
        loop {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "PP did not become ready within {:?}; see logs/pp.log under the Mode 3 data dir",
                    crate::pp_http_client::READINESS_DEADLINE,
                );
            }
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    anyhow::bail!(
                        "PP child exited during readiness probe (status={status:?}); see \
                         logs/pp.log under the Mode 3 data dir"
                    );
                }
            }
            // Use a single inner attempt to keep the child-exit guard on
            // the outer loop's cadence.
            match self.http.wait_ready(Duration::from_millis(250)).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    log::debug!(target: LOG_TARGET, "PP not yet ready ({e:#}); will retry");
                    sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// SIGTERM the daemon, wait up to [`SIGTERM_GRACE`], then SIGKILL.
    /// Idempotent: a call without an outstanding child is a no-op.
    pub async fn teardown(&mut self) -> anyhow::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let pid = child
            .id()
            .ok_or_else(|| anyhow!("PP teardown: child has no PID; nothing to signal"))?;
        send_signal(pid, Signal::Term)?;
        let deadline = Instant::now() + SIGTERM_GRACE;
        while Instant::now() < deadline {
            match child.try_wait()? {
                Some(status) => {
                    log::info!(
                        target: LOG_TARGET,
                        "PP exited gracefully after SIGTERM (status={status:?})",
                    );
                    return Ok(());
                }
                None => sleep(TEARDOWN_POLL_INTERVAL).await,
            }
        }
        log::warn!(
            target: LOG_TARGET,
            "PP did not exit within {SIGTERM_GRACE:?} of SIGTERM; sending SIGKILL",
        );
        send_signal(pid, Signal::Kill)?;
        let status = child.wait().await?;
        log::info!(
            target: LOG_TARGET, "PP exited after SIGKILL (status={status:?})",
        );
        Ok(())
    }
}

impl Drop for PpLifecycle {
    fn drop(&mut self) {
        if self.child.is_some() {
            log::warn!(
                target: LOG_TARGET,
                "PpLifecycle dropped without explicit teardown; relying on kill_on_drop",
            );
        }
    }
}

/// Mode 3 account env-var names paired into a single argument for
/// downstream construction. Mirrors the shape under `[mode_3.accounts.bench]`
/// in `harness.toml`.
pub fn read_account_env(account: &Mode3Account) -> anyhow::Result<(String, String)> {
    let view_key = std::env::var(&account.view_key_env).with_context(|| {
        format!(
            "reading hex view key from env var ${} (see MODE_3_REWORK_SPEC.md §12)",
            account.view_key_env,
        )
    })?;
    let spend_key = std::env::var(&account.public_spend_key_env).with_context(|| {
        format!(
            "reading hex public spend key from env var ${} (see MODE_3_REWORK_SPEC.md §12)",
            account.public_spend_key_env,
        )
    })?;
    Ok((view_key, spend_key))
}

/// POSIX signals used by [`PpLifecycle::teardown`]. Same shape as the
/// helper in `console_wallet.rs` and `pr_lifecycle.rs` — kept module-local
/// so the harness's non-unix builds still compile.
enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) -> anyhow::Result<()> {
    use nix::{
        sys::signal::{kill, Signal as NixSig},
        unistd::Pid,
    };
    let sig = match signal {
        Signal::Term => NixSig::SIGTERM,
        Signal::Kill => NixSig::SIGKILL,
    };
    let pid_i32 = i32::try_from(pid)
        .with_context(|| format!("converting child PID {pid} to i32 for nix::kill"))?;
    kill(Pid::from_raw(pid_i32), sig)
        .with_context(|| format!("sending {sig:?} to PID {pid_i32}"))?;
    Ok(())
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: Signal) -> anyhow::Result<()> {
    anyhow::bail!(
        "PpLifecycle is only supported on unix platforms (Linux/macOS per DESIGN.md §Non-Goals)"
    )
}

#[cfg(test)]
mod tests {
    // TODO(swe-test): populate per MODE_3_REWORK_SPEC.md §13. Test list:
    //   - spawn_returns_handle_and_pid
    //   - wait_ready_times_out_when_fake_never_binds
    //   - teardown_sends_sigterm_then_sigkill
    //   - drop_invokes_kill_on_drop
    //   - build_env_carries_every_required_key_per_spec_§2
    //   - build_env_omits_worker_sleep_when_override_is_none
}
