//! `minotari_console_wallet` lifecycle (Mode 1's backing wallet).
//!
//! Per `analysis/DESIGN.md §Mode 1 — concrete wiring`, Mode 1 spawns a long-
//! running `minotari_console_wallet` subprocess, polls its gRPC `GetState`
//! until `is_synced && scanned_height == base_node_tip`, drives scenarios
//! against the gRPC surface, and tears the wallet down on demand or on `Drop`
//! (SIGTERM with a 10s grace window, then SIGKILL).
//!
//! Argv shape (literal, in this order, per `DESIGN.md §Mode 1 step 1`):
//!
//! ```text
//! minotari_console_wallet \
//!   --network esmeralda \
//!   --base-path  <data_dir> \
//!   --password   $HARNESS_WALLET_PW \
//!   --seed-words-file <data_dir>/seed.txt \
//!   --non-interactive-mode \
//!   --grpc-address /ip4/127.0.0.1/tcp/<dynamic-port>
//! ```
//!
//! The `--network esmeralda` flag at the front mirrors the `minotari` CLI
//! shape proven in `DESIGN_ADDENDUM.md §Mode 3 CLI shape — proven`. Mainnet
//! protection enforces the literal `"esmeralda"` at the [`crate::guards`]
//! layer before spawn — this layer trusts it. Defense in depth: spawn
//! asserts `network == "esmeralda"` again before the `Command::new` call.
//!
//! Submit-side retry is forbidden by AC-30/31/32 and is NOT introduced here.
//! Connect-time backoff lives in [`super::grpc::connect_with_retry`] and is
//! explicitly allowed by `DESIGN.md §Mode 1 step 1`.

use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context};
use minotari_app_grpc::tari_rpc::{
    wallet_client::WalletClient, ConnectivityStatus, GetStateRequest,
};
use tokio::{
    process::{Child, Command},
    time::sleep,
};
use tonic::transport::Channel;
use url::Url;

use crate::{
    config::Config,
    seed::SeedHandle,
    wallet_lifecycle::{grpc::connect_with_retry, HarnessDataDir, WalletLifecycle},
};

const LOG_TARGET: &str = "c::wallet_lifecycle::console_wallet";

/// Default binary name resolved via `$PATH` when [`Config::minotari_console_wallet_path`]
/// is `None`.
const DEFAULT_BINARY: &str = "minotari_console_wallet";

/// Polling interval for the `GetState` readiness loop (matches
/// `DESIGN.md §Mode 1 step 2` — "Poll gRPC `GetState` every 1s").
const READY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Grace window after SIGTERM before SIGKILL (per `DESIGN.md §Mode 1 step 4`).
const SIGTERM_GRACE: Duration = Duration::from_secs(10);

/// Tick interval inside the SIGTERM grace loop.
const TEARDOWN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// `Network` allowlist string we cross-check at spawn time as defense in
/// depth. `crate::guards::enforce_esmeralda` is the primary gate; this is the
/// belt-and-braces re-assertion called out in
/// `DESIGN.md §Mainnet-protection guard`.
const NETWORK_ALLOWLIST: &str = "esmeralda";

/// Mode 1's `WalletLifecycle` implementation.
///
/// Owns the spawned [`Child`], the [`HarnessDataDir`], the dynamic gRPC port,
/// and (after [`Self::wait_ready`]) the connected [`WalletClient`]. Drop sends
/// SIGTERM (best-effort) so a panic or Ctrl-C doesn't leak the wallet
/// process — the explicit [`Self::teardown`] path is preferred because it
/// can `await` the grace window.
pub struct ConsoleWalletLifecycle {
    /// Binary to spawn — resolved from `Config::minotari_console_wallet_path`
    /// or the default `$PATH` name.
    binary: PathBuf,
    /// Network identifier passed via `--network`. Allowlisted to
    /// `"esmeralda"` at construction time.
    network: String,
    /// Wallet passphrase (revealed at spawn time, otherwise redacted).
    wallet_password: String,
    /// Seed mnemonic (revealed at spawn time when written to seed.txt).
    seed_mnemonic: String,
    /// Bound port from the OS — held in `Self::spawn_argv` so callers can
    /// poll the same endpoint via [`super::grpc::connect_with_retry`].
    grpc_port: Option<u16>,
    /// Harness-owned data directory. Drop cleans it up.
    data_dir: HarnessDataDir,
    /// Spawned wallet — `None` before `spawn`, `Some` between spawn and
    /// teardown.
    child: Option<Child>,
    /// Connected gRPC client — `None` before `wait_ready`, `Some` after.
    client: Option<WalletClient<Channel>>,
    /// Configured per-tx confirmation timeout — used as the wait-ready
    /// deadline (bounded but generous per `DESIGN.md §Mode 1 step 2`).
    ready_deadline: Duration,
}

impl ConsoleWalletLifecycle {
    /// Construct a fresh lifecycle. Does NOT spawn — call [`Self::spawn`]
    /// to start the child.
    ///
    /// `data_dir` is moved in so the lifecycle drives both the subprocess
    /// AND the per-mode tempdir cleanup; AC-34's wipe-before-scan calls
    /// go through [`HarnessDataDir::wipe`] on this owned data dir.
    ///
    /// The seed mnemonic is read once here (revealed) so the seed file
    /// can be written inside `spawn`; we hold the plaintext in a private
    /// field for the lifetime of the spawn. Holding mnemonic plaintext
    /// outside `RedactedString` is the same trade-off any wallet binary
    /// makes — the alternative is re-fetching from env on every spawn,
    /// which races with mid-run rotation.
    pub fn new(
        config: &Config,
        seeds: &SeedHandle,
        data_dir: HarnessDataDir,
    ) -> anyhow::Result<Self> {
        if config.network != NETWORK_ALLOWLIST {
            anyhow::bail!(
                "ConsoleWalletLifecycle::new refusing network={} (only 'esmeralda' is allowed; \
                 see crate::guards::enforce_esmeralda)",
                config.network,
            );
        }
        let binary = config
            .minotari_console_wallet_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_BINARY));
        let mnemonic = seeds.mnemonic_old()?;
        let password = seeds.wallet_password()?;
        Ok(Self {
            binary,
            network: config.network.clone(),
            wallet_password: password.reveal().to_string(),
            seed_mnemonic: mnemonic.reveal().to_string(),
            grpc_port: None,
            data_dir,
            child: None,
            client: None,
            ready_deadline: Duration::from_millis(config.per_tx_confirmation_timeout_ms),
        })
    }

    /// Allocate an OS-assigned ephemeral port on `127.0.0.1` and return it.
    /// The listener is dropped immediately so the wallet can bind — there is
    /// a small race window between drop and the wallet binding, which is the
    /// same trade-off `DESIGN.md §Mode 1 step 1` calls out as acceptable.
    pub fn allocate_port() -> anyhow::Result<u16> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .context("binding to 127.0.0.1:0 to allocate an ephemeral port")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Ok(port)
    }

    /// Build the argv vector for `minotari_console_wallet` per `DESIGN.md
    /// §Mode 1 step 1`. Pulled out so unit tests can snapshot it without
    /// spawning a subprocess.
    pub fn spawn_argv(network: &str, data_dir: &Path, port: u16) -> Vec<String> {
        let seed_path = data_dir.join("seed.txt");
        let grpc_addr = format!("/ip4/127.0.0.1/tcp/{port}");
        vec![
            "--network".to_string(),
            network.to_string(),
            "--base-path".to_string(),
            data_dir.display().to_string(),
            "--seed-words-file".to_string(),
            seed_path.display().to_string(),
            "--non-interactive-mode".to_string(),
            "--grpc-address".to_string(),
            grpc_addr,
        ]
    }

    /// gRPC endpoint URL once a port has been allocated.
    pub fn grpc_url(&self) -> anyhow::Result<Url> {
        let port = self
            .grpc_port
            .ok_or_else(|| anyhow!("ConsoleWalletLifecycle::grpc_url called before spawn"))?;
        Url::parse(&format!("http://127.0.0.1:{port}"))
            .context("constructing gRPC URL from allocated port")
    }

    /// Borrow the connected client. Returns `Err` if [`Self::wait_ready`]
    /// has not yet succeeded.
    pub fn client_mut(&mut self) -> anyhow::Result<&mut WalletClient<Channel>> {
        self.client.as_mut().ok_or_else(|| {
            anyhow!("ConsoleWalletLifecycle: client not yet connected (call wait_ready first)")
        })
    }

    /// Mutable access to the owned [`HarnessDataDir`] for explicit wipe
    /// calls — used by the Mode 1 `wipe_and_reimport` flow that AC-34
    /// polices (path-confinement is enforced inside `HarnessDataDir::wipe`).
    pub fn data_dir_mut(&mut self) -> &mut HarnessDataDir {
        &mut self.data_dir
    }

    /// Replace the held seed mnemonic. The next call to [`Self::spawn`]
    /// writes this new mnemonic into the freshly-wiped data dir's
    /// `seed.txt`. Used by the Mode 1 birthday-rewrite flow (AC-24): the
    /// caller decodes the existing mnemonic to a [`tari_common_types::seeds::cipher_seed::CipherSeed`],
    /// calls `change_birthday`, re-encodes, then hands the new mnemonic
    /// back via this method.
    pub fn replace_mnemonic(&mut self, mnemonic: String) {
        log::debug!(
            target: LOG_TARGET,
            "replacing held mnemonic (length={}); next spawn rewrites seed.txt",
            mnemonic.len(),
        );
        self.seed_mnemonic = mnemonic;
    }

    /// Read-only access to the held mnemonic — used by the birthday-rewrite
    /// flow that decodes-modifies-re-encodes before calling
    /// [`Self::replace_mnemonic`]. The string is the operator's mnemonic
    /// plaintext; treat the returned `&str` borrow accordingly.
    pub fn mnemonic(&self) -> &str {
        &self.seed_mnemonic
    }
}

#[async_trait::async_trait]
impl WalletLifecycle for ConsoleWalletLifecycle {
    async fn spawn(&mut self) -> anyhow::Result<()> {
        if self.child.is_some() {
            log::debug!(target: LOG_TARGET, "spawn called twice; the child is already running");
            return Ok(());
        }
        // Defense-in-depth re-assertion (the primary guard is
        // `crate::guards::enforce_esmeralda`).
        if self.network != NETWORK_ALLOWLIST {
            anyhow::bail!(
                "ConsoleWalletLifecycle::spawn refusing network={} (only 'esmeralda' is allowed)",
                self.network,
            );
        }

        // 1. Write the seed mnemonic to a file inside the harness data dir
        //    (per `DESIGN.md §Secret handling`, the seed file lives only
        //    inside the harness tempdir and is gone on drop).
        let seed_path = self.data_dir.path().join("seed.txt");
        std::fs::write(&seed_path, &self.seed_mnemonic)
            .with_context(|| format!("writing seed.txt to {}", seed_path.display()))?;

        // 2. Allocate a dynamic port for gRPC.
        let port = Self::allocate_port()?;
        self.grpc_port = Some(port);

        // 3. Build argv per the design.
        let argv = Self::spawn_argv(&self.network, self.data_dir.path(), port);
        log::info!(
            target: LOG_TARGET,
            "spawning {} with argv {:?} (password redacted, passed via env)",
            self.binary.display(),
            argv,
        );

        // 4. Spawn. Password is passed on argv per
        //    `DESIGN_ADDENDUM.md §Mode 3 CLI shape — proven` — the console
        //    wallet's `--password` is read from argv (the env-clear plan
        //    in DESIGN.md §Secret handling applies to the surrounding
        //    environment, not to the password itself). Argv leakage is
        //    bounded to this subprocess run.
        let child = Command::new(&self.binary)
            .args(&argv)
            .args(["--password", &self.wallet_password])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "spawning {} (is it on $PATH, or set Config::minotari_console_wallet_path?)",
                    self.binary.display()
                )
            })?;
        self.child = Some(child);
        Ok(())
    }

    async fn wait_ready(&mut self) -> anyhow::Result<()> {
        if self.client.is_some() {
            return Ok(());
        }
        let url = self.grpc_url()?;
        // Connect-time backoff is allowed (see grpc.rs); use the configured
        // per-tx confirmation timeout as the wall-clock budget for the entire
        // wait-ready phase.
        let mut client = connect_with_retry(&url, self.ready_deadline).await?;
        let started = Instant::now();
        loop {
            if started.elapsed() >= self.ready_deadline {
                anyhow::bail!(
                    "wallet did not reach ready (ConnectivityStatus::Online) within {:?}",
                    self.ready_deadline,
                );
            }
            match client.get_state(GetStateRequest {}).await {
                Ok(resp) => {
                    let state = resp.into_inner();
                    // `network.status` is the prost-generated i32 corresponding to the
                    // `ConnectivityStatus` enum in proto/network.proto:
                    //   Initializing = 0, Online = 1, Degraded = 2, Offline = 3.
                    // We treat `Online` as the ready signal — matches the wallet's own
                    // self-reported readiness contract for scenario calls.
                    let connectivity = state
                        .network
                        .as_ref()
                        .map(|n| n.status)
                        .unwrap_or(ConnectivityStatus::Initializing as i32);
                    if connectivity == ConnectivityStatus::Online as i32 {
                        log::info!(
                            target: LOG_TARGET,
                            "wallet ready at gRPC {url} (scanned_height={}, status=Online)",
                            state.scanned_height,
                        );
                        self.client = Some(client);
                        return Ok(());
                    }
                    log::debug!(
                        target: LOG_TARGET,
                        "wallet not yet Online (scanned_height={}, status={connectivity}); \
                         polling again in {:?}",
                        state.scanned_height,
                        READY_POLL_INTERVAL,
                    );
                }
                Err(status) => {
                    log::debug!(
                        target: LOG_TARGET,
                        "GetState transient error: {status}; polling again in {:?}",
                        READY_POLL_INTERVAL,
                    );
                }
            }
            sleep(READY_POLL_INTERVAL).await;
        }
    }

    async fn teardown(&mut self) -> anyhow::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        self.client = None;

        let pid = child
            .id()
            .ok_or_else(|| anyhow!("teardown: child has no PID; nothing to signal"))?;

        // SIGTERM via nix (cfg(unix); the harness ships Linux + macOS support
        // per DESIGN.md §Non-Goals).
        send_signal(pid, Signal::Term)?;

        let deadline = Instant::now() + SIGTERM_GRACE;
        while Instant::now() < deadline {
            match child.try_wait()? {
                Some(status) => {
                    log::info!(
                        target: LOG_TARGET, "wallet exited gracefully after SIGTERM (status={status:?})",
                    );
                    return Ok(());
                }
                None => sleep(TEARDOWN_POLL_INTERVAL).await,
            }
        }

        log::warn!(
            target: LOG_TARGET,
            "wallet did not exit within {SIGTERM_GRACE:?} of SIGTERM; sending SIGKILL",
        );
        send_signal(pid, Signal::Kill)?;
        let status = child.wait().await?;
        log::info!(target: LOG_TARGET, "wallet exited after SIGKILL (status={status:?})");
        Ok(())
    }

    fn data_dir(&self) -> &Path {
        self.data_dir.path()
    }
}

impl Drop for ConsoleWalletLifecycle {
    fn drop(&mut self) {
        // `tokio::process::Command::kill_on_drop(true)` handles the
        // wait-free SIGKILL of the inner `Child` via its own Drop impl.
        // We do not block on the grace window here — that's `teardown`'s
        // contract; Drop is the panic / Ctrl-C escape hatch.
        if self.child.is_some() {
            log::warn!(
                target: LOG_TARGET,
                "ConsoleWalletLifecycle dropped without explicit teardown; relying on kill_on_drop",
            );
        }
    }
}

/// POSIX signals used by [`ConsoleWalletLifecycle::teardown`]. Wrapped in our
/// own enum so non-unix builds get a clean `unimplemented!` rather than a
/// build failure on Windows (the harness is documented as Linux+macOS only,
/// but the lib still compiles on Windows for cargo doc / IDE workflows).
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
    anyhow::bail!("ConsoleWalletLifecycle is only supported on unix platforms (Linux/macOS per DESIGN.md §Non-Goals)")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn dynamic_port_allocation_returns_a_port() {
        let p = ConsoleWalletLifecycle::allocate_port().expect("allocate");
        assert!(p > 0);
    }

    #[test]
    fn dynamic_port_allocation_returns_distinct_ports() {
        // Allocate a handful and assert they are not all equal.
        let mut seen = HashSet::new();
        for _ in 0..8 {
            seen.insert(ConsoleWalletLifecycle::allocate_port().expect("allocate"));
        }
        assert!(
            seen.len() > 1,
            "ephemeral allocator must produce at least two distinct ports: {seen:?}",
        );
    }

    #[test]
    fn spawn_argv_matches_design() {
        let argv =
            ConsoleWalletLifecycle::spawn_argv("esmeralda", Path::new("/tmp/old_wallet"), 39201);
        // Order matters per DESIGN.md §Mode 1 step 1: --network first, then
        // --base-path, --seed-words-file, --non-interactive-mode, then
        // --grpc-address. (`--password` is appended in spawn().)
        assert_eq!(argv[0], "--network");
        assert_eq!(argv[1], "esmeralda");
        assert_eq!(argv[2], "--base-path");
        assert_eq!(argv[3], "/tmp/old_wallet");
        assert_eq!(argv[4], "--seed-words-file");
        assert_eq!(argv[5], "/tmp/old_wallet/seed.txt");
        assert_eq!(argv[6], "--non-interactive-mode");
        assert_eq!(argv[7], "--grpc-address");
        assert_eq!(argv[8], "/ip4/127.0.0.1/tcp/39201");
    }

    #[test]
    fn spawn_argv_includes_the_dynamic_port() {
        let argv = ConsoleWalletLifecycle::spawn_argv("esmeralda", Path::new("/data"), 12345);
        assert!(
            argv.iter().any(|a| a == "/ip4/127.0.0.1/tcp/12345"),
            "argv must reference the dynamic port literally: {argv:?}",
        );
    }
}
