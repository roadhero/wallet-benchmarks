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
    use super::*;
    use std::path::{Path, PathBuf};

    /// Absolute path to the fake `minotari` binary under `tests/fixtures/`.
    fn fake_minotari_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_minotari.sh")
    }

    /// Allocate a localhost port via OS ephemeral allocation. Mirrors the
    /// helper in `pp_lifecycle::tests` (kept inline so the two test modules
    /// stay independent).
    fn allocate_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("local_addr").port()
    }

    /// Build a `PrLifecycleConfig` populated with deterministic test values.
    fn make_config(port: u16) -> PrLifecycleConfig {
        PrLifecycleConfig {
            minotari_binary: fake_minotari_path(),
            network: NETWORK_FLAG_VALUE.to_string(),
            view_private_key_hex:
                "572a5fb63972da84aeec33071d13074e244d80c52be842ab5b0859ef4b4db00a".to_string(),
            spend_public_key_hex:
                "40e65c9bbf4592bc995c421108c01a5d7c9f9b2239569757895134549cef371f".to_string(),
            wallet_password: "test-password".to_string(),
            port,
        }
    }

    #[test]
    fn import_view_key_argv_matches_spec() {
        // Pure-fn snapshot. Per spec §5 step 1 the argv must include
        // import-view-key + --base-path + --network + --account-name in
        // that order. The view-key / spend-key / password flags are
        // appended in spawn() (kept out of the argv builder so secrets
        // don't sit in stable argv strings).
        let argv = PrLifecycle::import_view_key_argv("esmeralda", Path::new("/tmp/pr"), 9146);
        assert_eq!(argv[0], "import-view-key");
        assert_eq!(argv[1], "--base-path");
        assert_eq!(argv[2], "/tmp/pr");
        assert_eq!(argv[3], "--network");
        assert_eq!(argv[4], "esmeralda");
        assert_eq!(argv[5], "--account-name");
        assert_eq!(argv[6], ACCOUNT_NAME);
        // Port is intentionally omitted — assert no element equals the
        // port literal so a future regression that re-adds it surfaces.
        assert!(
            !argv.iter().any(|a| a == "9146"),
            "import-view-key argv must not carry --port: {argv:?}",
        );
    }

    #[test]
    fn daemon_argv_matches_spec() {
        // Per spec §5 step 2 the daemon argv carries --port + --account-name
        // in addition to the import-view-key shape.
        let argv = PrLifecycle::daemon_argv("esmeralda", Path::new("/tmp/pr"), 9146);
        assert_eq!(argv[0], "daemon");
        assert_eq!(argv[1], "--base-path");
        assert_eq!(argv[2], "/tmp/pr");
        assert_eq!(argv[3], "--network");
        assert_eq!(argv[4], "esmeralda");
        assert_eq!(argv[5], "--port");
        assert_eq!(argv[6], "9146");
        assert_eq!(argv[7], "--account-name");
        assert_eq!(argv[8], ACCOUNT_NAME);
    }

    #[test]
    fn daemon_argv_includes_the_dynamic_port() {
        let argv = PrLifecycle::daemon_argv("esmeralda", Path::new("/data"), 12345);
        assert!(
            argv.iter().any(|a| a == "12345"),
            "daemon argv must reference the dynamic port literally: {argv:?}",
        );
    }

    #[test]
    fn new_refuses_non_esmeralda_network() {
        // The lifecycle is defense-in-depth: even though the harness-wide
        // guard enforces esmeralda first, the constructor re-asserts it
        // so a misconfigured caller cannot bypass the gate.
        let mut cfg = make_config(9146);
        cfg.network = "mainnet".to_string();
        let dir =
            HarnessDataDir::new("test-pr-network-refuse", "payment_processor").expect("data dir");
        let err = match PrLifecycle::new(cfg, dir) {
            Ok(_) => panic!("non-esmeralda must be refused"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("esmeralda"),
            "refusal must name the allowed network: {msg}",
        );
    }

    #[tokio::test]
    async fn spawn_returns_handle_and_pid() {
        // Use the same direct-spawn pattern as pp_lifecycle's matching
        // test — bypass wait_ready (which would consume the 30s
        // readiness deadline against the never-binding fake) and just
        // confirm the daemon subprocess launches with a valid PID.
        let dir =
            HarnessDataDir::new("test-pr-spawn-child", "payment_processor").expect("data dir");
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).expect("logs dir");
        let log_path = logs.join("pr-daemon.log");
        let stdout = std::fs::File::create(&log_path).expect("create log");
        let stderr = stdout.try_clone().expect("clone log fd");
        let argv = PrLifecycle::daemon_argv("esmeralda", dir.path(), allocate_port());
        let child = Command::new(fake_minotari_path())
            .args(&argv)
            .args(["--password", "test-password"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .expect("spawn fake_minotari daemon");
        let pid = child.id().expect("child PID");
        assert!(pid > 0, "spawned daemon child must report a PID");
        drop(child);
    }

    #[tokio::test]
    async fn wait_ready_times_out_when_fake_never_binds() {
        // Exercises the full PrLifecycle::spawn flow against a fake that
        // performs import-view-key (exit 0) and then daemon (sleep loop
        // with no TCP bind). The readiness probe loops until the
        // READINESS_DEADLINE (30s) and bails with the documented
        // "did not return 200" message.
        let dir = HarnessDataDir::new("test-pr-wait-ready", "payment_processor").expect("data dir");
        let port = allocate_port();
        let mut life = PrLifecycle::new(make_config(port), dir).expect("construct");
        let err = life.spawn().await.expect_err("must time out");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("did not return 200"),
            "error must surface the readiness-probe deadline: {msg}",
        );
        life.teardown().await.expect("teardown ok after timeout");
    }

    #[tokio::test]
    async fn teardown_sends_sigterm_then_sigkill() {
        // SIGTERM the fake daemon (which has a TERM trap and exits 0).
        // Verify teardown returns Ok well under the 5s grace window.
        // We inject the child manually to skip the readiness deadline.
        let dir = HarnessDataDir::new("test-pr-teardown", "payment_processor").expect("data dir");
        let port = allocate_port();
        let mut life = PrLifecycle::new(make_config(port), dir).expect("construct");
        let logs = life.data_dir_path().join("logs");
        std::fs::create_dir_all(&logs).expect("logs dir");
        let log_path = logs.join("pr-daemon.log");
        let stdout = std::fs::File::create(&log_path).expect("create log");
        let stderr = stdout.try_clone().expect("clone log fd");
        let argv = PrLifecycle::daemon_argv("esmeralda", life.data_dir_path(), port);
        let child = Command::new(fake_minotari_path())
            .args(&argv)
            .args(["--password", "test-password"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .expect("spawn fake_minotari daemon");
        life.child = Some(child);
        let start = Instant::now();
        life.teardown().await.expect("teardown ok");
        let elapsed = start.elapsed();
        assert!(
            elapsed < SIGTERM_GRACE,
            "teardown must complete before the SIGTERM grace window expires; took {elapsed:?}",
        );
        assert!(
            !life.is_alive(),
            "lifecycle must report not-alive after teardown"
        );
        life.teardown().await.expect("second teardown is a no-op");
    }

    #[tokio::test]
    async fn drop_invokes_kill_on_drop() {
        // Mount a lifecycle, inject a real fake daemon, then drop without
        // teardown. The held Child has kill_on_drop(true) so Tokio's
        // Drop sends SIGKILL. Poll liveness until the kernel reaps it.
        let dir = HarnessDataDir::new("test-pr-drop-kill", "payment_processor").expect("data dir");
        let port = allocate_port();
        let mut life = PrLifecycle::new(make_config(port), dir).expect("construct");
        let logs = life.data_dir_path().join("logs");
        std::fs::create_dir_all(&logs).expect("logs dir");
        let log_path = logs.join("pr-daemon.log");
        let stdout = std::fs::File::create(&log_path).expect("create log");
        let stderr = stdout.try_clone().expect("clone log fd");
        let argv = PrLifecycle::daemon_argv("esmeralda", life.data_dir_path(), port);
        let child = Command::new(fake_minotari_path())
            .args(&argv)
            .args(["--password", "test-password"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .expect("spawn fake_minotari daemon");
        let pid = child.id().expect("PID");
        life.child = Some(child);
        drop(life);
        let mut alive = true;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            #[cfg(unix)]
            {
                use nix::sys::signal::kill as nix_kill;
                use nix::unistd::Pid;
                let pid_i32 = pid as i32;
                if nix_kill(Pid::from_raw(pid_i32), None).is_err() {
                    alive = false;
                    break;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = pid;
                alive = false;
                break;
            }
        }
        assert!(!alive, "child PID must be reaped after Drop (kill_on_drop)");
    }
}
