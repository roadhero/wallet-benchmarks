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

/// Settle window for [`ReadyPolicy::OnlineAndScanStable`]: if
/// `scanned_height` is non-zero and does not change for this duration,
/// the wallet is treated as ready (scan has plateaued, presumably at
/// tip). 30s matches the operator-spec guidance from @SWvheerden's
/// 2026-06-22 review of `556fb94`.
const SCAN_STABLE_WINDOW: Duration = Duration::from_secs(30);

/// Strategy controlling [`ConsoleWalletLifecycle::wait_ready_with_policy`].
///
/// `wait_ready` (the trait-method default) calls with
/// [`Self::OnlineAndScanStable`]: scenarios just need the wallet
/// connected to a base node and not actively scanning.
/// `wait_ready_funded` (called by the funding pre-flight) uses
/// [`Self::OnlineAndFunded`]: the caller is about to read the balance,
/// so the operationally-meaningful signal is "wallet sees its money."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyPolicy {
    /// Wallet is `Online` AND `scanned_height` has not changed for
    /// [`SCAN_STABLE_WINDOW`] (with a baseline of `scanned_height > 0`
    /// so a wallet that hasn't started scanning is never declared
    /// stable). Coarse but reliable, and the right gate for sending
    /// scenarios.
    OnlineAndScanStable,
    /// Wallet is `Online` AND `GetStateResponse.balance.available_balance > 0`.
    /// Use from the funding pre-flight: if the wallet stays at 0 for the
    /// entire `ready_deadline`, the funding pre-flight will report a
    /// per-seed shortage error, which is the right outcome.
    OnlineAndFunded,
}

impl std::fmt::Display for ReadyPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OnlineAndScanStable => write!(f, "OnlineAndScanStable"),
            Self::OnlineAndFunded => write!(f, "OnlineAndFunded"),
        }
    }
}

/// Snapshot of the `GetState` fields that drive [`ReadyPolicy`] evaluation.
///
/// Carved out of the response so [`evaluate_ready_policy`] is a pure
/// function of (snapshot, policy, scan-stability state, monotonic now) —
/// the loop in [`ConsoleWalletLifecycle::wait_ready_with_policy`] is
/// then a thin wrapper that handles gRPC + sleeping + the deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PollSnapshot {
    pub connectivity: i32,
    pub scanned_height: u64,
    pub available_balance: u64,
}

/// Per-call tracker for the [`ReadyPolicy::OnlineAndScanStable`] window.
/// `evaluate_ready_policy` is the only mutator; the loop owns one
/// instance for its lifetime.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ScanStability {
    pub last_height: Option<u64>,
    pub stable_since: Option<Instant>,
}

/// Result of evaluating a [`ReadyPolicy`] against the latest poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadyDecision {
    Ready,
    NotYet,
}

/// Pure policy evaluator extracted from the polling loop so the four
/// cases the audit (`analysis/WAIT_READY_AUDIT.md`) calls out — happy
/// path, Online+0 forever, never-Online, balance-arrives-with-Online
/// same-poll race — are unit-testable without spinning a real wallet.
///
/// The `now` parameter is taken explicitly (rather than reading
/// `Instant::now()` inside) so tests can drive a synthetic monotonic
/// timeline by adding [`Duration`] offsets to a single baseline.
pub(crate) fn evaluate_ready_policy(
    snapshot: PollSnapshot,
    policy: ReadyPolicy,
    stability: &mut ScanStability,
    now: Instant,
) -> ReadyDecision {
    let online = snapshot.connectivity == ConnectivityStatus::Online as i32;
    match policy {
        ReadyPolicy::OnlineAndFunded => {
            if online && snapshot.available_balance > 0 {
                ReadyDecision::Ready
            } else {
                ReadyDecision::NotYet
            }
        }
        ReadyPolicy::OnlineAndScanStable => {
            if !online || snapshot.scanned_height == 0 {
                // Reset stability tracking on regressions / pre-Online state.
                stability.last_height = None;
                stability.stable_since = None;
                return ReadyDecision::NotYet;
            }
            match (stability.last_height, stability.stable_since) {
                (Some(prev), Some(since)) if prev == snapshot.scanned_height => {
                    if now.saturating_duration_since(since) >= SCAN_STABLE_WINDOW {
                        ReadyDecision::Ready
                    } else {
                        ReadyDecision::NotYet
                    }
                }
                _ => {
                    stability.last_height = Some(snapshot.scanned_height);
                    stability.stable_since = Some(now);
                    ReadyDecision::NotYet
                }
            }
        }
    }
}

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

    /// Shared (immutable) handle for the connected gRPC client, intended for
    /// cloning into an S4 dispatcher. Returns `None` if [`Self::wait_ready`]
    /// has not yet succeeded — the caller surfaces the "not connected" error
    /// at dispatch time (the [`crate::modes::Mode::dispatcher`] method must
    /// return the handle unconditionally, so liveness is checked one layer
    /// later).
    ///
    /// Cloning the returned reference yields an independent
    /// [`WalletClient<Channel>`] sharing the same underlying
    /// [`tonic::transport::Channel`] connection pool — cheap, safe across
    /// concurrent tasks.
    pub fn client_handle_for_dispatcher(&self) -> Option<&WalletClient<Channel>> {
        self.client.as_ref()
    }

    /// Mutable access to the owned [`HarnessDataDir`] for explicit wipe
    /// calls — used by the Mode 1 `wipe_and_reimport` flow that AC-34
    /// polices (path-confinement is enforced inside `HarnessDataDir::wipe`).
    pub fn data_dir_mut(&mut self) -> &mut HarnessDataDir {
        &mut self.data_dir
    }

    /// Returns `true` between a successful [`Self::spawn`] and the next
    /// [`Self::teardown`]. Used by [`Self::replace_mnemonic`] to refuse
    /// post-spawn mnemonic swaps that would silently desync the held
    /// value from the running wallet's loaded seed.
    pub fn is_spawned(&self) -> bool {
        self.child.is_some()
    }

    /// Replace the held seed mnemonic. The next call to [`Self::spawn`]
    /// writes this new mnemonic into the freshly-wiped data dir's
    /// `seed.txt`. Used by the Mode 1 birthday-rewrite flow (AC-24): the
    /// caller decodes the existing mnemonic to a [`tari_common_types::seeds::cipher_seed::CipherSeed`],
    /// calls `change_birthday`, re-encodes, then hands the new mnemonic
    /// back via this method.
    ///
    /// Bails when called post-spawn: a swap there would silently desync
    /// the held mnemonic from the running wallet's loaded seed (the
    /// wallet keeps using the previously-loaded value while `seed.txt`
    /// regenerates only on next spawn). Call before [`Self::spawn`] or
    /// after [`Self::teardown`].
    pub fn replace_mnemonic(&mut self, mnemonic: String) -> anyhow::Result<()> {
        if self.is_spawned() {
            anyhow::bail!(
                "replace_mnemonic called after spawn; this would silently desync the held \
                 mnemonic from the running wallet's loaded seed. Call before spawn() or \
                 after teardown().",
            );
        }
        log::debug!(
            target: LOG_TARGET,
            "replacing held mnemonic (length={}); next spawn rewrites seed.txt",
            mnemonic.len(),
        );
        self.seed_mnemonic = mnemonic;
        Ok(())
    }

    /// Read-only access to the held mnemonic — used by the birthday-rewrite
    /// flow that decodes-modifies-re-encodes before calling
    /// [`Self::replace_mnemonic`]. The string is the operator's mnemonic
    /// plaintext; treat the returned `&str` borrow accordingly.
    pub fn mnemonic(&self) -> &str {
        &self.seed_mnemonic
    }

    /// Force the lifecycle into the post-spawn state for unit tests
    /// without requiring a real `minotari_console_wallet` binary. Spawns
    /// `/bin/sleep` (kill-on-drop) and parks its `Child` in
    /// `self.child` so [`Self::is_spawned`] reports `true`.
    #[cfg(test)]
    fn force_spawned_for_test(&mut self) -> anyhow::Result<()> {
        let child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .context("spawning /bin/sleep for test")?;
        self.child = Some(child);
        Ok(())
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
        self.wait_ready_with_policy(ReadyPolicy::OnlineAndScanStable)
            .await
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

impl ConsoleWalletLifecycle {
    /// Wait until the wallet reports a positive `available_balance`.
    ///
    /// The gate used by [`crate::wallet_lifecycle::balance_query::WalletGrpcBalanceQuery`]
    /// in the funding pre-flight. The caller is about to query the
    /// balance anyway, so the readiness signal that actually matters is
    /// "scan reached a block containing this wallet's outputs". The
    /// `has_done_initial_validation` gate that previously lived in
    /// `wait_ready` (commit `556fb94`) is replaced because the upstream
    /// flag does not assert reliably in the field per @SWvheerden's
    /// 2026-06-22 review on PR #6: wallets hit the 30 min deadline
    /// before the flag flips even on healthy networks.
    ///
    /// If the wallet is genuinely unfunded the deadline still elapses
    /// and the funding pre-flight surfaces a per-seed shortage error,
    /// which is the right outcome.
    pub async fn wait_ready_funded(&mut self) -> anyhow::Result<()> {
        self.wait_ready_with_policy(ReadyPolicy::OnlineAndFunded)
            .await
    }

    /// Inner readiness loop. Returns when [`ReadyPolicy`] is satisfied or
    /// `ready_deadline` elapses. Defined here rather than on the trait
    /// so the trait surface stays small (only the trait-method
    /// `wait_ready` is in the contract).
    ///
    /// The policy decision per poll is delegated to
    /// [`evaluate_ready_policy`] so the four logical cases (happy path,
    /// Online+0 forever, never-Online, balance-arrives-with-Online
    /// same-poll race) are unit-testable without a real wallet — see
    /// `analysis/WAIT_READY_AUDIT.md` for the rationale.
    async fn wait_ready_with_policy(&mut self, policy: ReadyPolicy) -> anyhow::Result<()> {
        if self.client.is_some() {
            return Ok(());
        }
        let url = self.grpc_url()?;
        let mut client = connect_with_retry(&url, self.ready_deadline).await?;
        let started = Instant::now();
        let mut stability = ScanStability::default();
        loop {
            if started.elapsed() >= self.ready_deadline {
                anyhow::bail!(
                    "wallet did not reach ready ({policy}) within {:?}",
                    self.ready_deadline,
                );
            }
            match client.get_state(GetStateRequest {}).await {
                Ok(resp) => {
                    let state = resp.into_inner();
                    let snapshot = PollSnapshot {
                        connectivity: state
                            .network
                            .as_ref()
                            .map(|n| n.status)
                            .unwrap_or(ConnectivityStatus::Initializing as i32),
                        scanned_height: state.scanned_height,
                        available_balance: state
                            .balance
                            .as_ref()
                            .map(|b| b.available_balance)
                            .unwrap_or(0),
                    };
                    let decision =
                        evaluate_ready_policy(snapshot, policy, &mut stability, Instant::now());
                    if decision == ReadyDecision::Ready {
                        log::info!(
                            target: LOG_TARGET,
                            "wallet ready at gRPC {url} (policy={policy}, \
                             scanned_height={}, available_balance={} uT, \
                             status={})",
                            snapshot.scanned_height,
                            snapshot.available_balance,
                            snapshot.connectivity,
                        );
                        self.client = Some(client);
                        return Ok(());
                    }
                    log::debug!(
                        target: LOG_TARGET,
                        "wallet not yet ready (policy={policy}, scanned_height={}, \
                         available_balance={} uT, status={}); polling again in {:?}",
                        snapshot.scanned_height,
                        snapshot.available_balance,
                        snapshot.connectivity,
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

    /// Replacing the held mnemonic after `spawn` is a footgun: the
    /// running wallet keeps using the previously-loaded seed while
    /// `seed.txt` regenerates only on next spawn. The guard added
    /// alongside step 3k commit 3 refuses the swap. Verify it bails
    /// with the documented diagnostic.
    #[tokio::test]
    async fn replace_mnemonic_bails_after_spawn() {
        use crate::config::{Config, Seeds};
        use crate::seed::SeedHandle;
        use crate::wallet_lifecycle::HarnessDataDir;

        let seeds_cfg = Seeds {
            old: "WALLET_BENCHMARKS_TEST_RM_OLD".to_string(),
            new: "WALLET_BENCHMARKS_TEST_RM_NEW".to_string(),
            payment_processor: "WALLET_BENCHMARKS_TEST_RM_PP".to_string(),
            wallet_password: "WALLET_BENCHMARKS_TEST_RM_PW".to_string(),
        };
        let m = crate::gen_seed().expect("gen_seed");
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
        let data_dir = HarnessDataDir::new("rm_test", "old_wallet").expect("data_dir");
        let mut lifecycle =
            ConsoleWalletLifecycle::new(&cfg, &seeds, data_dir).expect("lifecycle constructs");

        // Pre-spawn: replace_mnemonic succeeds.
        lifecycle
            .replace_mnemonic("first replacement".to_string())
            .expect("pre-spawn swap allowed");

        // Force into spawned state, then assert the guard fires.
        lifecycle
            .force_spawned_for_test()
            .expect("force_spawned_for_test");
        assert!(
            lifecycle.is_spawned(),
            "after force_spawned, is_spawned == true"
        );

        let err = lifecycle
            .replace_mnemonic("second replacement".to_string())
            .expect_err("post-spawn swap must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("after spawn"),
            "guard message must call out the post-spawn condition: {msg}",
        );

        // Cleanup the /bin/sleep child via Drop.
        drop(lifecycle);
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(&seeds_cfg.old);
            std::env::remove_var(&seeds_cfg.wallet_password);
        }
    }

    // ---------- ReadyPolicy ----------

    #[test]
    fn ready_policy_display_matches_log_format() {
        // The wait_ready_with_policy log format relies on `{policy}`
        // resolving to a short, unambiguous string. If a future refactor
        // breaks the Display impl, this test catches it before the next
        // canonical-baseline run produces opaque error messages.
        assert_eq!(
            format!("{}", ReadyPolicy::OnlineAndScanStable),
            "OnlineAndScanStable",
        );
        assert_eq!(
            format!("{}", ReadyPolicy::OnlineAndFunded),
            "OnlineAndFunded",
        );
    }

    #[test]
    fn ready_policy_variants_are_exhaustive_and_distinct() {
        // Cheap compile-time guard against accidental enum drift: if
        // someone adds a third variant the match below stops being
        // exhaustive (Rust forces the new arm), and the assert_ne
        // pair guards against two variants being defined to compare
        // equal.
        let scan = ReadyPolicy::OnlineAndScanStable;
        let funded = ReadyPolicy::OnlineAndFunded;
        match scan {
            ReadyPolicy::OnlineAndScanStable | ReadyPolicy::OnlineAndFunded => {}
        }
        assert_ne!(scan, funded);
    }

    #[test]
    fn scan_stable_window_is_a_short_seconds_grade_duration() {
        // The wait_ready_with_policy stability gate uses
        // SCAN_STABLE_WINDOW as the "no progress for X seconds → ready"
        // bound. A canonical baseline run uses the default
        // per_tx_confirmation_timeout_ms = 30 min as the overall
        // wait_ready ceiling, so the stable window must be much
        // shorter than that. Asserting an explicit bound here catches
        // a future tweak that accidentally bumps the constant into the
        // minutes-or-more range and starves the baseline run.
        assert!(SCAN_STABLE_WINDOW >= Duration::from_secs(5));
        assert!(SCAN_STABLE_WINDOW <= Duration::from_secs(120));
    }

    // ---------- evaluate_ready_policy ----------
    //
    // The four cases the audit (analysis/WAIT_READY_AUDIT.md) calls out
    // are exercised here without spinning a real wallet so future
    // gate tweaks don't ship without coverage:
    //
    //   1. happy path: Online + funded → Ready immediately.
    //   2. Online + 0 balance forever → NotYet forever.
    //   3. never-Online → NotYet (and a hypothetical "balance leaked
    //      while still pre-Online" race is also NotYet).
    //   4. balance arrives in the same poll as Online → Ready (no
    //      one-poll delay smuggled in by the loop's state-tracking).
    //
    // Plus a pair for the scan-stable policy: it ticks Ready after
    // SCAN_STABLE_WINDOW of no-progress, and resets on height advance.

    fn online_snap(scanned_height: u64, available: u64) -> PollSnapshot {
        PollSnapshot {
            connectivity: ConnectivityStatus::Online as i32,
            scanned_height,
            available_balance: available,
        }
    }

    #[test]
    fn online_and_funded_ready_on_first_funded_poll() {
        let mut stability = ScanStability::default();
        let now = Instant::now();
        let snap = online_snap(12_345, 1_000_000);
        assert_eq!(
            evaluate_ready_policy(snap, ReadyPolicy::OnlineAndFunded, &mut stability, now),
            ReadyDecision::Ready,
        );
    }

    #[test]
    fn online_and_funded_stays_not_yet_when_balance_is_zero_forever() {
        let mut stability = ScanStability::default();
        let baseline = Instant::now();
        for i in 0..200 {
            let snap = online_snap(100 + i, 0);
            let now = baseline + Duration::from_secs(i);
            assert_eq!(
                evaluate_ready_policy(snap, ReadyPolicy::OnlineAndFunded, &mut stability, now),
                ReadyDecision::NotYet,
                "iteration {i}: a wallet that reports 0 balance must never be declared ready by OnlineAndFunded",
            );
        }
    }

    #[test]
    fn online_and_funded_not_yet_when_never_online() {
        let mut stability = ScanStability::default();
        let now = Instant::now();
        let init = PollSnapshot {
            connectivity: ConnectivityStatus::Initializing as i32,
            scanned_height: 0,
            available_balance: 0,
        };
        assert_eq!(
            evaluate_ready_policy(init, ReadyPolicy::OnlineAndFunded, &mut stability, now),
            ReadyDecision::NotYet,
        );
        // Even a hypothetical race where the wallet reports a non-zero
        // balance while still pre-Online must not flip the gate — Online
        // is a hard precondition.
        let pre_online_with_balance = PollSnapshot {
            connectivity: ConnectivityStatus::Offline as i32,
            scanned_height: 100,
            available_balance: 42_000,
        };
        assert_eq!(
            evaluate_ready_policy(
                pre_online_with_balance,
                ReadyPolicy::OnlineAndFunded,
                &mut stability,
                now,
            ),
            ReadyDecision::NotYet,
            "balance > 0 must not satisfy OnlineAndFunded while connectivity is pre-Online",
        );
    }

    #[test]
    fn online_and_funded_ready_when_balance_arrives_in_same_poll_as_online() {
        let mut stability = ScanStability::default();
        let baseline = Instant::now();
        // Five polls of pre-Online state: NotYet each time.
        for i in 0..5 {
            let snap = PollSnapshot {
                connectivity: ConnectivityStatus::Initializing as i32,
                scanned_height: 0,
                available_balance: 0,
            };
            let now = baseline + Duration::from_secs(i);
            assert_eq!(
                evaluate_ready_policy(snap, ReadyPolicy::OnlineAndFunded, &mut stability, now),
                ReadyDecision::NotYet,
            );
        }
        // Sixth poll: Online + funded both arrive in the same response.
        // The gate must declare Ready immediately — no extra poll cycle.
        let now = baseline + Duration::from_secs(6);
        let snap = online_snap(12_345, 42_000);
        assert_eq!(
            evaluate_ready_policy(snap, ReadyPolicy::OnlineAndFunded, &mut stability, now),
            ReadyDecision::Ready,
            "OnlineAndFunded must satisfy on the same poll Online+funded both become true",
        );
    }

    #[test]
    fn scan_stable_ready_after_window_with_no_height_progress() {
        let mut stability = ScanStability::default();
        let baseline = Instant::now();
        let snap = online_snap(100, 0);
        // First observation: records baseline, NotYet.
        assert_eq!(
            evaluate_ready_policy(
                snap,
                ReadyPolicy::OnlineAndScanStable,
                &mut stability,
                baseline,
            ),
            ReadyDecision::NotYet,
        );
        // Same height, just before the window closes: still NotYet.
        assert_eq!(
            evaluate_ready_policy(
                snap,
                ReadyPolicy::OnlineAndScanStable,
                &mut stability,
                baseline + SCAN_STABLE_WINDOW - Duration::from_millis(1),
            ),
            ReadyDecision::NotYet,
        );
        // Same height, at the window boundary: Ready.
        assert_eq!(
            evaluate_ready_policy(
                snap,
                ReadyPolicy::OnlineAndScanStable,
                &mut stability,
                baseline + SCAN_STABLE_WINDOW,
            ),
            ReadyDecision::Ready,
        );
    }

    #[test]
    fn scan_stable_resets_when_height_advances() {
        let mut stability = ScanStability::default();
        let baseline = Instant::now();
        // First observation at height 100.
        evaluate_ready_policy(
            online_snap(100, 0),
            ReadyPolicy::OnlineAndScanStable,
            &mut stability,
            baseline,
        );
        // Long after SCAN_STABLE_WINDOW, but at a NEW height: must be NotYet,
        // because the stability tracker resets to the new height/time.
        assert_eq!(
            evaluate_ready_policy(
                online_snap(200, 0),
                ReadyPolicy::OnlineAndScanStable,
                &mut stability,
                baseline + SCAN_STABLE_WINDOW * 3,
            ),
            ReadyDecision::NotYet,
            "scan-stable must reset on height advance instead of treating the elapsed wall-clock as 'no progress'",
        );
        // And the new height now also needs SCAN_STABLE_WINDOW from this moment
        // to flip to Ready — anything shorter is NotYet.
        let new_baseline = baseline + SCAN_STABLE_WINDOW * 3;
        assert_eq!(
            evaluate_ready_policy(
                online_snap(200, 0),
                ReadyPolicy::OnlineAndScanStable,
                &mut stability,
                new_baseline + SCAN_STABLE_WINDOW - Duration::from_millis(1),
            ),
            ReadyDecision::NotYet,
        );
        assert_eq!(
            evaluate_ready_policy(
                online_snap(200, 0),
                ReadyPolicy::OnlineAndScanStable,
                &mut stability,
                new_baseline + SCAN_STABLE_WINDOW,
            ),
            ReadyDecision::Ready,
        );
    }
}
