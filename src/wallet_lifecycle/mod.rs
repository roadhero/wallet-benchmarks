//! Wallet lifecycle abstractions for the harness.
//!
//! Per `analysis/DESIGN.md §Module boundaries and data flow`, every Mode owns a
//! `WalletLifecycle` that knows how to spawn its underlying wallet binary, poll
//! it to a known-ready state, and tear it down on demand (or on `Drop`). Mode 1
//! (`old_wallet`) backs this with a `minotari_console_wallet` subprocess plus
//! the published `minotari_app_grpc` `WalletClient`; Modes 2 and 3 use the
//! `minotari` CLI as a subprocess with no long-lived state, so they implement
//! the same trait with no-op spawn / wait_ready.
//!
//! The trait is `async` via [`async_trait::async_trait`] — matches the
//! `BaseNodeWalletClient` shape in `minotari_node_wallet_client = "5.3.1"` and
//! keeps trait objects usable from scenario dispatch.

pub mod console_wallet;
pub mod data_dir;
pub mod grpc;

use std::path::Path;

#[doc(inline)]
pub use data_dir::HarnessDataDir;

/// Lifecycle contract for the wallet binary that backs a [`crate::modes::Mode`].
///
/// Implementations own:
/// * the subprocess (if any) — spawn/teardown is mediated through these methods,
/// * the per-mode tempdir under `target/harness-data/<run-id>/<mode>/`,
/// * any client connection (gRPC `Channel`) layered on top.
///
/// Trait methods are deliberately small — scenario code calls higher-level
/// [`crate::modes::Mode`] entry points; `WalletLifecycle` is the runtime
/// substrate beneath that. The shape mirrors `DESIGN.md §Module boundaries and
/// data flow` step 2 ("WalletLifecycle: spawn, wait_ready (poll GetState),
/// run, terminate (SIGTERM)").
#[async_trait::async_trait]
pub trait WalletLifecycle: Send + Sync {
    /// Start the underlying wallet process (no-op for modes that have none).
    /// Idempotent: calling twice after a successful spawn returns `Ok(())`.
    async fn spawn(&mut self) -> anyhow::Result<()>;

    /// Block until the wallet reports itself ready for scenario calls — for
    /// Mode 1 this is `is_synced == true` and `scanned_height == base_node_tip`
    /// per `DESIGN.md §Mode 1 step 2`. Bounded by the per-tx confirmation
    /// timeout in [`crate::config::Config::per_tx_confirmation_timeout_ms`].
    async fn wait_ready(&mut self) -> anyhow::Result<()>;

    /// Tear down the wallet process: `SIGTERM`, wait up to 10s, then `SIGKILL`
    /// per `DESIGN.md §Mode 1 step 4`. Idempotent.
    async fn teardown(&mut self) -> anyhow::Result<()>;

    /// Path to the harness-owned data dir for this wallet — used by the
    /// birthday-rewrite scenarios (B0/S2/S3/S6/S7) that need to wipe and
    /// re-import via the same lifecycle's data root.
    fn data_dir(&self) -> &Path;
}
