//! `minotari daemon` lifecycle — Mode 3's PR-role HTTP daemon.
//!
//! Per `analysis/specs/MODE_3_REWORK_SPEC.md §5`, Mode 3 spawns a
//! `minotari daemon` child to back PP's view-key wallet operations
//! (`POST /accounts/{name}/lock_funds`,
//! `POST /accounts/{name}/create_unsigned_transaction`,
//! `GET /accounts/{name}/balance`). The daemon is a *view-only* wallet —
//! `minotari import-view-key` consumes a view-key + spend-public-key hex
//! pair (no mnemonic), and PP holds the corresponding spend secret.
//!
//! Spawn sequence (spec §5):
//! 1. `minotari import-view-key --view-private-key <hex> --spend-public-key <hex>
//!    --base-path <dd> --network esmeralda --password <pw>` (one-shot).
//! 2. `minotari daemon --base-path <dd> --network esmeralda --port <port>
//!    --account-name default --password <pw>` (long-running Child).
//! 3. Poll `GET http://127.0.0.1:<port>/accounts/default/balance` until 200
//!    (same 200ms backoff / 30s deadline as PP — see [`crate::pp_http_client`]).
//!
//! Teardown is the same SIGTERM-grace-then-SIGKILL escalation Mode 1 uses
//! (per `crate::wallet_lifecycle::console_wallet::ConsoleWalletLifecycle::teardown`),
//! with a shorter 5s grace per spec §9.

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

use crate::wallet_lifecycle::HarnessDataDir;

const LOG_TARGET: &str = "c::wallet_lifecycle::pr_lifecycle";

/// Network identifier passed via `--network`. Hard-allowlisted to
/// `"esmeralda"` (Mode 3 inherits the harness-wide mainnet-protection
/// guard via [`crate::guards::enforce_esmeralda`]).
const NETWORK_FLAG_VALUE: &str = "esmeralda";

/// Account name passed via `--account-name`. v1 hard-codes `default`;
/// matches the literal PP calls `/accounts/default/...` against.
const ACCOUNT_NAME: &str = "default";

/// PR readiness-probe inter-attempt backoff (matches PP per spec §5).
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// PR readiness-probe wall-clock deadline (matches PP per spec §5).
const READINESS_DEADLINE: Duration = Duration::from_secs(30);

/// Per-attempt HTTP timeout inside the readiness probe.
const READINESS_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// Grace window after SIGTERM before SIGKILL (per spec §9).
const SIGTERM_GRACE: Duration = Duration::from_secs(5);

/// Tick interval inside the SIGTERM grace loop.
const TEARDOWN_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Construction inputs for [`PrLifecycle`]. Pulled into a struct so adding
/// fields (e.g. operator-supplied seed-name overrides) doesn't churn the
/// call site.
pub struct PrLifecycleConfig {
    /// Absolute path to the `minotari` binary (from
    /// `Config::mode_3.minotari_binary_path`).
    pub minotari_binary: PathBuf,
    /// Network identifier; must be `"esmeralda"`.
    pub network: String,
    /// Hex-encoded view private key.
    pub view_private_key_hex: String,
    /// Hex-encoded public spend key.
    pub spend_public_key_hex: String,
    /// Wallet passphrase (revealed plaintext).
    pub wallet_password: String,
    /// HTTP listen port (default 9146 — see `Mode3Config::pr_port`).
    pub port: u16,
}

/// Mode 3's PR daemon lifecycle handle.
///
/// Owns the spawned `Child` plus the per-mode `HarnessDataDir`. Spawn runs
/// `import-view-key` then `daemon`; teardown sends SIGTERM, waits 5s, then
/// SIGKILL. Drop logs a warn if the child is still alive — `kill_on_drop(true)`
/// on the inner `Child` is the catch-all.
pub struct PrLifecycle {
    cfg: PrLifecycleConfig,
    /// The PR daemon's data dir. Lifecycle owns it so its drop cleans the
    /// tree (same pattern as Mode 1's `ConsoleWalletLifecycle`).
    data_dir: HarnessDataDir,
    /// `None` before [`Self::spawn`], `Some` between spawn and teardown.
    child: Option<Child>,
}

impl PrLifecycle {
    /// Construct a lifecycle handle. Does NOT spawn — call [`Self::spawn`]
    /// to run the import-view-key one-shot and start the daemon.
    pub fn new(cfg: PrLifecycleConfig, data_dir: HarnessDataDir) -> anyhow::Result<Self> {
        if cfg.network != NETWORK_FLAG_VALUE {
            anyhow::bail!(
                "PrLifecycle::new refusing network={} (only '{NETWORK_FLAG_VALUE}' is allowed; \
                 see crate::guards::enforce_esmeralda)",
                cfg.network,
            );
        }
        Ok(Self {
            cfg,
            data_dir,
            child: None,
        })
    }

    /// Returns `true` between a successful spawn and the next teardown.
    pub fn is_alive(&self) -> bool {
        self.child.is_some()
    }

    /// Build the argv vector for the `minotari import-view-key` one-shot.
    /// Pure function so unit tests can snapshot it without spawning.
    pub fn import_view_key_argv(
        network: &str,
        data_dir: &std::path::Path,
        port: u16,
    ) -> Vec<String> {
        // `--port` and `--account-name` are not part of import-view-key,
        // but `--base-path` is; we keep the rest constant. Wallet password
        // and view/spend keys are appended in `spawn` since they're secrets.
        let _ = port; // explicit silencing — argv shape does not include port
        vec![
            "import-view-key".to_string(),
            "--base-path".to_string(),
            data_dir.display().to_string(),
            "--network".to_string(),
            network.to_string(),
            "--account-name".to_string(),
            ACCOUNT_NAME.to_string(),
        ]
    }

    /// Build the argv vector for `minotari daemon`. Pure function so unit
    /// tests can snapshot it without spawning.
    pub fn daemon_argv(network: &str, data_dir: &std::path::Path, port: u16) -> Vec<String> {
        vec![
            "daemon".to_string(),
            "--base-path".to_string(),
            data_dir.display().to_string(),
            "--network".to_string(),
            network.to_string(),
            "--port".to_string(),
            port.to_string(),
            "--account-name".to_string(),
            ACCOUNT_NAME.to_string(),
        ]
    }

    /// Run the `import-view-key` one-shot, spawn `minotari daemon`, and
    /// poll for HTTP readiness against `GET /accounts/default/balance`.
    ///
    /// Idempotent: a second call after a successful spawn is a no-op.
    pub async fn spawn(&mut self) -> anyhow::Result<()> {
        if self.child.is_some() {
            log::debug!(
                target: LOG_TARGET,
                "PrLifecycle::spawn called twice; the child is already running",
            );
            return Ok(());
        }
        // Step 1: import-view-key one-shot. Wait for exit; bail on non-zero.
        let import_argv =
            Self::import_view_key_argv(&self.cfg.network, self.data_dir.path(), self.cfg.port);
        log::info!(
            target: LOG_TARGET,
            "spawning {} import-view-key (view key + spend key + password redacted)",
            self.cfg.minotari_binary.display(),
        );
        let import_status = Command::new(&self.cfg.minotari_binary)
            .args(&import_argv)
            .args(["--view-private-key", &self.cfg.view_private_key_hex])
            .args(["--spend-public-key", &self.cfg.spend_public_key_hex])
            .args(["--password", &self.cfg.wallet_password])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .with_context(|| {
                format!(
                    "spawning {} import-view-key (is the binary set via \
                     Config::mode_3.minotari_binary_path?)",
                    self.cfg.minotari_binary.display(),
                )
            })?;
        if !import_status.success() {
            anyhow::bail!(
                "{} import-view-key exited with {import_status:?}; check the operator's view-key + spend-public-key + password env vars",
                self.cfg.minotari_binary.display(),
            );
        }
        // Step 2: long-running daemon. Pipe stdio into the data dir so the
        // operator can inspect after the run (matches §2 stdio rationale).
        let logs_dir = self.data_dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir)
            .with_context(|| format!("creating logs dir at {}", logs_dir.display()))?;
        let log_path = logs_dir.join("pr-daemon.log");
        let stdout = std::fs::File::create(&log_path)
            .with_context(|| format!("creating PR daemon log at {}", log_path.display()))?;
        let stderr = stdout
            .try_clone()
            .context("cloning PR daemon log file handle for stderr")?;
        let daemon_argv = Self::daemon_argv(&self.cfg.network, self.data_dir.path(), self.cfg.port);
        log::info!(
            target: LOG_TARGET,
            "spawning {} daemon (port={}, account=default, password redacted)",
            self.cfg.minotari_binary.display(),
            self.cfg.port,
        );
        let child = Command::new(&self.cfg.minotari_binary)
            .args(&daemon_argv)
            .args(["--password", &self.cfg.wallet_password])
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {} daemon", self.cfg.minotari_binary.display(),))?;
        self.child = Some(child);
        // Step 3: HTTP readiness probe.
        self.wait_ready().await
    }

    /// Poll `GET /accounts/default/balance` until 200 OK or the deadline
    /// elapses. Same shape as [`crate::pp_http_client::PpHttpClient::wait_ready`].
    async fn wait_ready(&mut self) -> anyhow::Result<()> {
        let url = format!(
            "http://127.0.0.1:{}/accounts/{}/balance",
            self.cfg.port, ACCOUNT_NAME
        );
        let client = reqwest::Client::new();
        let started = Instant::now();
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            if started.elapsed() >= READINESS_DEADLINE {
                anyhow::bail!(
                    "PR daemon {url} did not return 200 within {:?} ({attempts} attempts)",
                    READINESS_DEADLINE,
                );
            }
            // Surface a child exit observably — the readiness loop would
            // otherwise spin until the deadline.
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    anyhow::bail!(
                        "PR daemon child exited during readiness probe (status={status:?}); \
                         see logs/pr-daemon.log under the Mode 3 data dir"
                    );
                }
            }
            match client
                .get(&url)
                .timeout(READINESS_ATTEMPT_TIMEOUT)
                .send()
                .await
            {
                Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
                    log::info!(
                        target: LOG_TARGET,
                        "PR daemon ready at {url} (attempts={attempts})",
                    );
                    return Ok(());
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
            sleep(READINESS_POLL_INTERVAL).await;
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
            .ok_or_else(|| anyhow!("PR teardown: child has no PID; nothing to signal"))?;
        send_signal(pid, Signal::Term)?;
        let deadline = Instant::now() + SIGTERM_GRACE;
        while Instant::now() < deadline {
            match child.try_wait()? {
                Some(status) => {
                    log::info!(
                        target: LOG_TARGET,
                        "PR daemon exited gracefully after SIGTERM (status={status:?})",
                    );
                    return Ok(());
                }
                None => sleep(TEARDOWN_POLL_INTERVAL).await,
            }
        }
        log::warn!(
            target: LOG_TARGET,
            "PR daemon did not exit within {SIGTERM_GRACE:?} of SIGTERM; sending SIGKILL",
        );
        send_signal(pid, Signal::Kill)?;
        let status = child.wait().await?;
        log::info!(
            target: LOG_TARGET,
            "PR daemon exited after SIGKILL (status={status:?})",
        );
        Ok(())
    }

    /// Path to the lifecycle's data dir.
    pub fn data_dir_path(&self) -> &std::path::Path {
        self.data_dir.path()
    }
}

impl Drop for PrLifecycle {
    fn drop(&mut self) {
        // `kill_on_drop(true)` on the inner Child handles SIGKILL via Tokio's
        // own drop impl. Drop logs a warn if the lifecycle wasn't torn down
        // explicitly so the operator notices missed graceful-shutdown paths.
        if self.child.is_some() {
            log::warn!(
                target: LOG_TARGET,
                "PrLifecycle dropped without explicit teardown; relying on kill_on_drop",
            );
        }
    }
}

/// POSIX signals used by [`PrLifecycle::teardown`]. Same shape as the
/// helper in `console_wallet.rs` — kept module-local so the harness's
/// non-unix builds (cargo doc / IDE) still compile.
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
        "PrLifecycle is only supported on unix platforms (Linux/macOS per DESIGN.md §Non-Goals)"
    )
}

#[cfg(test)]
mod tests {
    // TODO(swe-test): populate per MODE_3_REWORK_SPEC.md §13. Test list
    // (parallel to PpLifecycle's): exercise import_view_key_argv /
    // daemon_argv shape, spawn-with-fake-binary, teardown SIGTERM-then-SIGKILL,
    // Drop relying on kill_on_drop. Tests requiring a live HTTP server can
    // use a fake binary backed by python -m http.server / wiremock /
    // tests/fixtures/fake_pp.sh.
}
