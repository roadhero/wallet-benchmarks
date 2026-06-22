//! Harness configuration.
//!
//! The [`Config`] struct mirrors `analysis/RESULT_PROFILE_SCHEMA.md §1` field-for-field
//! and supplies the documented defaults from the bounty issue's parameter table. Seed
//! material itself never lives in [`Config`]; instead [`Seeds`] records the *names* of
//! the environment variables the harness reads at runtime — keeping mnemonics and
//! passphrases out of any TOML on disk per `analysis/DESIGN.md §Secret handling`.
//!
//! The TOML loader lives in [`load`]; every field is `#[serde(default)]` so a minimal
//! `harness.toml` only needs to override the keys the operator cares about.

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use url::Url;

pub mod load;

/// Default Esmeralda base-node HTTP endpoint.
const DEFAULT_BASE_NODE_URL: &str = "https://rpc.esmeralda.tari.com";

/// Defaults from the bounty issue's parameter table and DESIGN.md §Decisions.
mod defaults {
    pub(super) const A_FUND: u64 = 10_000_000_000;
    pub(super) const C_MIN: u32 = 3;
    pub(super) const VOLUME_TARGET: u32 = 512;
    pub(super) const DOUBLING_ROUNDS: u32 = 6;
    pub(super) const FANOUT_OUTPUTS_PER_TX: u32 = 8;
    pub(super) const S4_T_BUDGET_MS: u64 = 900_000;
    pub(super) const S5_M: u32 = 100;
    pub(super) const S5_K: u32 = 10;
    pub(super) const FEE_RATE: u64 = 5;
    pub(super) const NETWORK: &str = "esmeralda";
    pub(super) const PER_TX_CONFIRMATION_TIMEOUT_MS: u64 = 1_800_000;
    pub(super) const SAMPLER_INTERVAL_MS: u64 = 1_000;
    /// Default per-tx amount for S1 volume sends, in microTari. Must
    /// exceed `fee_rate × kernel_weight` (≈ 175 µT at default fee_rate=5,
    /// kernel weight 35) so the resulting change UTXOs are net-positive
    /// and spendable. 1000 µT = 0.001 XTM provides ~825 µT net per
    /// resulting UTXO at default fee_rate.
    pub(super) const S1_AMOUNT_PER_TX_MICROTARI: u64 = 1_000;

    pub(super) const SEED_ENV_OLD: &str = "HARNESS_SEED_OLD";
    pub(super) const SEED_ENV_NEW: &str = "HARNESS_SEED_NEW";
    pub(super) const SEED_ENV_PP: &str = "HARNESS_SEED_PP";
    pub(super) const WALLET_PW_ENV: &str = "HARNESS_WALLET_PW";
}

/// Harness configuration — exact field mirror of `RESULT_PROFILE_SCHEMA.md §1`.
///
/// Operators populate a `harness.toml`; the loader fills missing keys from
/// [`Config::default`]. Secret values (seed mnemonics, wallet passphrase) live in
/// environment variables whose *names* are recorded in [`Self::seeds`]; the harness
/// resolves them at runtime via [`std::env::var`] — they never touch the TOML.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// Initial funding amount per mode, in microTari. Schema: `a_fund`.
    #[serde(default = "Config::default_a_fund")]
    pub a_fund: u64,

    /// Confirmation depth read by every `wait_for_confirmation` call. Schema: `c_min`.
    #[serde(default = "Config::default_c_min")]
    pub c_min: u32,

    /// Target final UTXO count for S1. Schema: `volume_target`.
    #[serde(default = "Config::default_volume_target")]
    pub volume_target: u32,

    /// Number of serial doubling rounds in S1 (1,2,4,8,16,32). Schema: `doubling_rounds`.
    #[serde(default = "Config::default_doubling_rounds")]
    pub doubling_rounds: u32,

    /// Outputs per transaction in the S1 fan-out round. Schema: `fanout_outputs_per_tx`.
    #[serde(default = "Config::default_fanout_outputs_per_tx")]
    pub fanout_outputs_per_tx: u32,

    /// S4 concurrent-batch N values. Schema: `concurrent_batches`.
    #[serde(default = "Config::default_concurrent_batches")]
    pub concurrent_batches: Vec<u32>,

    /// S4 hard wall-clock budget per N, in milliseconds. Schema: `s4_t_budget_ms`.
    #[serde(default = "Config::default_s4_t_budget_ms")]
    pub s4_t_budget_ms: u64,

    /// S5 individual-arm transaction count and recipient list length. Schema: `s5_m`.
    #[serde(default = "Config::default_s5_m")]
    pub s5_m: u32,

    /// Recipients per batch tx in the S5 batch arm. Schema: `s5_k`.
    #[serde(default = "Config::default_s5_k")]
    pub s5_k: u32,

    /// Fee rate in microTari per gram. Schema: `fee_rate`.
    #[serde(default = "Config::default_fee_rate")]
    pub fee_rate: u64,

    /// Tari network identifier. Hard-allowlisted to `"esmeralda"` by
    /// [`crate::guards::enforce_esmeralda`]. Schema: `network`.
    #[serde(default = "Config::default_network")]
    pub network: String,

    /// Esmeralda base-node HTTP endpoint. Cross-checked against a mainnet host denylist
    /// by [`crate::guards::enforce_esmeralda`]. Schema: `base_node_url`.
    #[serde(default = "Config::default_base_node_url")]
    pub base_node_url: Url,

    /// Per-tx confirmation wait, in milliseconds. Schema: `per_tx_confirmation_timeout_ms`.
    #[serde(default = "Config::default_per_tx_confirmation_timeout_ms")]
    pub per_tx_confirmation_timeout_ms: u64,

    /// Resource sampler tick interval, in milliseconds. Drives
    /// [`crate::sampler::ResourceSampler`]'s background loop for the
    /// per-scenario `peak_rss_bytes` / `peak_cpu_pct` fields. Default
    /// 1000 (1 Hz per `analysis/DESIGN.md §Sampling`). Schema:
    /// `sampler_interval_ms`.
    #[serde(default = "Config::default_sampler_interval_ms")]
    pub sampler_interval_ms: u64,

    /// Per-tx amount for S1 volume sends (microTari). Must exceed
    /// `fee_rate × kernel_weight` (≈ 175 µT at default fee_rate=5)
    /// so resulting change UTXOs are spendable. Default 1000 µT
    /// (= 0.001 XTM). Operator-controllable so funding-budget vs.
    /// spendable-UTXO-threshold tradeoffs can be tuned without code
    /// changes.
    #[serde(default = "Config::default_s1_amount_per_tx_microtari")]
    pub s1_amount_per_tx_microtari: u64,

    /// Names of the environment variables holding seed and passphrase material. The
    /// values themselves are read at runtime; only the names are persisted.
    #[serde(default)]
    pub seeds: Seeds,

    /// Optional override for the `minotari_console_wallet` binary path used by
    /// Mode 1's [`crate::wallet_lifecycle::console_wallet::ConsoleWalletLifecycle`].
    /// When `None`, the harness resolves `"minotari_console_wallet"` via `$PATH`.
    /// Operators on systems where the binary is in a non-standard location
    /// (developer builds under `~/code/tari/target/release/`, for example) set
    /// this to the absolute path. Not in the schema's `config` block — this is
    /// a host-specific runtime setting, not a measurement parameter.
    #[serde(default)]
    pub minotari_console_wallet_path: Option<PathBuf>,

    /// Optional override for the `minotari` (new-wallet CLI) binary path used by
    /// Modes 2 and 3's `create-unsigned-transaction` subprocess pipeline. When
    /// `None`, the harness resolves `"minotari"` via `$PATH`. `minotari_console_wallet`
    /// (Mode 1's wallet) and `minotari` (the new wallet from `tari-project/minotari-cli`)
    /// are different binaries; the harness records both paths separately so the
    /// operator can point each at the local build location.
    #[serde(default)]
    pub minotari_path: Option<PathBuf>,

    /// Mode 3 (`payment_processor`) configuration. Required when running
    /// Mode 3 scenarios — the run loop bails at startup if this is absent and
    /// Mode 3 is in the scenario list. See
    /// `analysis/specs/MODE_3_REWORK_SPEC.md §12` for the field-by-field
    /// rationale and default-value provenance.
    #[serde(default)]
    pub mode_3: Option<Mode3Config>,
}

/// Mode 3 — `payment_processor` — configuration.
///
/// Per `MODE_3_REWORK_SPEC.md §12`, Mode 3 spawns a `minotari_payment_processor`
/// (PP) daemon and a `minotari daemon` (PR) child process. Both binaries are
/// pre-built by the operator (Phase 0); the harness only references their
/// paths and reads the per-account view-key + spend-public-key hex pair from
/// the env var names recorded under [`Mode3Config::accounts`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mode3Config {
    /// Absolute path to the `minotari_payment_processor` binary. Required;
    /// `Config::load` (or per-Mode validation at startup) refuses to proceed
    /// when this points at a non-existent file.
    pub pp_binary_path: PathBuf,

    /// Absolute path to the `minotari` (minotari-cli) binary used to spawn
    /// the PR daemon (`minotari daemon ...`) and run the one-shot
    /// `minotari import-view-key` step. Distinct from
    /// [`Mode3Config::pp_binary_path`]: PR ships in `tari-project/minotari-cli`,
    /// PP ships in `tari-project/minotari_payment_processor`. Most installs
    /// will set this to the same path the existing Mode 2 harness already
    /// uses via [`Config::minotari_path`], but Mode 3 keeps the override
    /// separate so the operator can co-locate independent builds.
    pub minotari_binary_path: PathBuf,

    /// PP HTTP listen port. Default 9145.
    #[serde(default = "Mode3Config::default_api_port")]
    pub api_port: u16,

    /// PR daemon HTTP listen port. Default 9146. NOTE: `minotari daemon`
    /// binds `0.0.0.0:<pr_port>` (no `--listen-ip` flag); on a shared host
    /// the operator must local-firewall this port. The harness cannot
    /// enforce binding to loopback.
    #[serde(default = "Mode3Config::default_pr_port")]
    pub pr_port: u16,

    /// Base URL the PR daemon (`minotari daemon`) talks to for its own
    /// blockchain RPC client. Passed via `--base-url` on the `daemon`
    /// subcommand; the real `minotari` CLI surfaces this flag through
    /// `NodeArgs` and treats it as mandatory in practice (the daemon needs
    /// a base node to scan). Default mirrors the harness-wide Esmeralda
    /// base node so a fresh operator can opt in without extra config.
    #[serde(default = "Mode3Config::default_pr_base_url")]
    pub pr_base_url: String,

    /// Per-payment terminal-state poll timeout at shutdown (seconds).
    /// After all S4/S5 sends, the run loop polls each submitted payment
    /// for [`crate::pp_http_client::PaymentStatus::is_terminal`]; the
    /// shutdown logs a warn and proceeds once this deadline elapses.
    #[serde(default = "Mode3Config::default_terminal_poll_timeout")]
    pub terminal_state_poll_timeout_secs: u64,

    /// PP worker sleep overrides. Defaults to bench values
    /// (1/1/1/1/5 seconds for batch_creator / unsigned_tx_creator /
    /// transaction_signer / broadcaster / confirmation_checker) per
    /// `MODE_3_REWORK_SPEC.md §8`.
    #[serde(default)]
    pub worker_sleep_overrides: WorkerSleepOverrides,

    /// PP account map. v1 hard-codes a single `bench` account; multi-account
    /// support is an out-of-scope extension.
    #[serde(default)]
    pub accounts: Mode3Accounts,
}

/// PP account map carried under [`Mode3Config::accounts`]. v1 ships a single
/// `bench` account; the struct exists so future multi-account expansion is a
/// non-breaking field addition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Mode3Accounts {
    /// The bench account PP and the PR daemon both watch.
    #[serde(default)]
    pub bench: Mode3Account,
}

/// Names of the env vars holding the hex view key and public spend key for a
/// single PP account.
///
/// Per `MODE_3_REWORK_SPEC.md §12`, view keys are secret-adjacent (a view key
/// reveals all incoming amounts/addresses for the account) so the values
/// themselves never live in TOML — only the env var names do, mirroring the
/// existing [`Seeds`] convention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mode3Account {
    /// Env var name holding the hex view key.
    #[serde(default = "Mode3Account::default_view_key_env")]
    pub view_key_env: String,
    /// Env var name holding the hex public spend key.
    #[serde(default = "Mode3Account::default_spend_key_env")]
    pub public_spend_key_env: String,
}

/// Per-worker sleep overrides for the PP daemon. Each `Option<u64>` is the
/// number of seconds the named worker sleeps between iterations; `None` lets
/// PP fall back to its hardcoded default. Defaults here match the bench's
/// "drive PP as fast as it'll go" posture per `MODE_3_REWORK_SPEC.md §8`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerSleepOverrides {
    /// `BATCH_CREATOR_SLEEP_SECS`. Default 1 (PP's default is 600).
    #[serde(default = "WorkerSleepOverrides::default_batch_creator")]
    pub batch_creator: Option<u64>,
    /// `UNSIGNED_TX_CREATOR_SLEEP_SECS`. Default 1 (PP's default is 15).
    #[serde(default = "WorkerSleepOverrides::default_unsigned_tx_creator")]
    pub unsigned_tx_creator: Option<u64>,
    /// `TRANSACTION_SIGNER_SLEEP_SECS`. Default 1 (PP's default is 10).
    #[serde(default = "WorkerSleepOverrides::default_transaction_signer")]
    pub transaction_signer: Option<u64>,
    /// `BROADCASTER_SLEEP_SECS`. Default 1 (PP's default is 15).
    #[serde(default = "WorkerSleepOverrides::default_broadcaster")]
    pub broadcaster: Option<u64>,
    /// `CONFIRMATION_CHECKER_SLEEP_SECS`. Default 5 (PP's default is 60).
    #[serde(default = "WorkerSleepOverrides::default_confirmation_checker")]
    pub confirmation_checker: Option<u64>,
}

/// Names of the environment variables holding seed mnemonics and the wallet
/// passphrase. Per `DESIGN.md §Secret handling`, secrets never live in a TOML on
/// disk — only the *names* of the env vars do, so the runtime resolver
/// (`std::env::var(&config.seeds.old)` etc.) can fetch them just-in-time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Seeds {
    /// Env var holding the old-wallet mode seed mnemonic.
    #[serde(default = "Seeds::default_old")]
    pub old: String,
    /// Env var holding the new-wallet mode seed mnemonic.
    #[serde(default = "Seeds::default_new")]
    pub new: String,
    /// Env var holding the payment-processor mode seed mnemonic.
    #[serde(default = "Seeds::default_payment_processor")]
    pub payment_processor: String,
    /// Env var holding the wallet passphrase shared across all three modes.
    #[serde(default = "Seeds::default_wallet_password")]
    pub wallet_password: String,
}

impl Config {
    fn default_a_fund() -> u64 {
        defaults::A_FUND
    }
    fn default_c_min() -> u32 {
        defaults::C_MIN
    }
    fn default_volume_target() -> u32 {
        defaults::VOLUME_TARGET
    }
    fn default_doubling_rounds() -> u32 {
        defaults::DOUBLING_ROUNDS
    }
    fn default_fanout_outputs_per_tx() -> u32 {
        defaults::FANOUT_OUTPUTS_PER_TX
    }
    fn default_concurrent_batches() -> Vec<u32> {
        vec![8, 16, 32, 64, 128]
    }
    fn default_s4_t_budget_ms() -> u64 {
        defaults::S4_T_BUDGET_MS
    }
    fn default_s5_m() -> u32 {
        defaults::S5_M
    }
    fn default_s5_k() -> u32 {
        defaults::S5_K
    }
    fn default_fee_rate() -> u64 {
        defaults::FEE_RATE
    }
    fn default_network() -> String {
        defaults::NETWORK.to_string()
    }
    fn default_base_node_url() -> Url {
        // The default URL is a compile-time constant; parse failure here is a
        // programmer error in the crate itself, not operator input.
        Url::parse(DEFAULT_BASE_NODE_URL).expect("DEFAULT_BASE_NODE_URL parses")
    }
    fn default_per_tx_confirmation_timeout_ms() -> u64 {
        defaults::PER_TX_CONFIRMATION_TIMEOUT_MS
    }
    fn default_sampler_interval_ms() -> u64 {
        defaults::SAMPLER_INTERVAL_MS
    }
    fn default_s1_amount_per_tx_microtari() -> u64 {
        defaults::S1_AMOUNT_PER_TX_MICROTARI
    }

    /// Validate cross-field invariants after deserialization. Currently
    /// runs [`Mode3Config::validate`] when Mode 3 config is present,
    /// hard-failing on missing binary paths or unresolvable bench-account
    /// keys per `analysis/specs/MODE_3_REWORK_SPEC.md §12`. The seeds
    /// table is passed through so the account-key validation can fall
    /// back to seed derivation when the env-var override is not set.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(m) = self.mode_3.as_ref() {
            m.validate(&self.seeds)
                .context("validating mode_3 config")?;
        }
        Ok(())
    }
}

impl Mode3Config {
    fn default_api_port() -> u16 {
        9145
    }
    fn default_pr_port() -> u16 {
        9146
    }
    fn default_pr_base_url() -> String {
        "https://rpc.esmeralda.tari.com".to_string()
    }
    fn default_terminal_poll_timeout() -> u64 {
        60
    }

    /// Startup validation per `analysis/specs/MODE_3_REWORK_SPEC.md §12`.
    ///
    /// Bails when:
    /// * `pp_binary_path` does not exist or is not a regular file.
    /// * `minotari_binary_path` does not exist or is not a regular file.
    /// * The bench-account keypair cannot be resolved at run time. The
    ///   keypair has two paths: an operator-injected env-var override
    ///   ([`Mode3Account::view_key_env`] + [`Mode3Account::public_spend_key_env`]),
    ///   and a seed-derive fallback that reads
    ///   [`Seeds::payment_processor`]'s env var. Validation policy
    ///   (mirrors `pp_lifecycle::resolve_account_keys` at the lifecycle
    ///   layer):
    ///
    ///   - **Both override env vars set** → validate each is a 64-char
    ///     ASCII-hex string. Lifecycle will use them verbatim.
    ///   - **Neither override env var set** → require that
    ///     `seeds.payment_processor`'s env var is set. Lifecycle will
    ///     derive the pair from that mnemonic via
    ///     [`crate::seed::derive_view_spend_keypair`].
    ///   - **Exactly one override env var set** → bail. Half an override
    ///     is almost always a typo, and silently filling the missing
    ///     half from the seed would risk pairing two unrelated wallets'
    ///     keys (same policy as `resolve_account_keys`).
    ///
    /// Failure mode #1 + #10 per spec §14. Called by [`Config::validate`]
    /// when [`Config::mode_3`] is `Some` so a missing path / unresolvable
    /// keypair surfaces before any subprocess is spawned.
    pub fn validate(&self, seeds: &Seeds) -> anyhow::Result<()> {
        ensure_executable(&self.pp_binary_path, "mode_3.pp_binary_path")?;
        ensure_executable(&self.minotari_binary_path, "mode_3.minotari_binary_path")?;

        let view_env_name = &self.accounts.bench.view_key_env;
        let spend_env_name = &self.accounts.bench.public_spend_key_env;
        let view_env = std::env::var(view_env_name).ok();
        let spend_env = std::env::var(spend_env_name).ok();

        match (view_env, spend_env) {
            (Some(view), Some(spend)) => {
                // Override path: validate hex shape so a misformatted
                // override fails here (single source of truth) rather
                // than deep in `minotari import-view-key` argv parsing.
                ensure_hex_64(&view, "mode_3.accounts.bench.view_key_env", view_env_name)?;
                ensure_hex_64(
                    &spend,
                    "mode_3.accounts.bench.public_spend_key_env",
                    spend_env_name,
                )?;
                Ok(())
            }
            (None, None) => {
                // Seed-derive path: lifecycle will hash HARNESS_SEED_PP
                // (or whatever seeds.payment_processor points to) into
                // the keypair. Require the seed env var to be set so
                // the derivation does not fail at lifecycle spawn time.
                let pp_seed_env = &seeds.payment_processor;
                std::env::var(pp_seed_env).map_err(|e| {
                    anyhow::anyhow!(
                        "Mode 3 keypair cannot be resolved: env vars ${} and ${} are unset \
                         (override path) AND seeds.payment_processor=${pp_seed_env} is unset \
                         ({e}) (seed-derive fallback). Set ${pp_seed_env} to the Mode 3 \
                         mnemonic for the default derivation, or set BOTH ${} and ${} to \
                         override the derivation. See RUNBOOK §2.5.",
                        view_env_name,
                        spend_env_name,
                        view_env_name,
                        spend_env_name,
                    )
                })?;
                Ok(())
            }
            (Some(_), None) => anyhow::bail!(
                "Mode 3 keypair override is half-set: env var ${} is set but ${} is not. \
                 Set BOTH to override the seed-derive default, or unset both to derive \
                 the pair from ${}.",
                view_env_name,
                spend_env_name,
                seeds.payment_processor,
            ),
            (None, Some(_)) => anyhow::bail!(
                "Mode 3 keypair override is half-set: env var ${} is set but ${} is not. \
                 Set BOTH to override the seed-derive default, or unset both to derive \
                 the pair from ${}.",
                spend_env_name,
                view_env_name,
                seeds.payment_processor,
            ),
        }
    }
}

/// Validate that `value` is a 64-character ASCII-hex string (32 bytes,
/// lowercase or uppercase). `config_key` is the dotted TOML path, used in
/// the error message; `env_name` is the env-var name whose value was read.
fn ensure_hex_64(value: &str, config_key: &str, env_name: &str) -> anyhow::Result<()> {
    if value.len() != 64 {
        anyhow::bail!(
            "{config_key} (read from ${env_name}) must be 64 hex chars (32 bytes); \
             got {} chars",
            value.len(),
        );
    }
    if !value.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!(
            "{config_key} (read from ${env_name}) must be ASCII hex digits only \
             (0-9, a-f, A-F)",
        );
    }
    Ok(())
}

fn ensure_executable(path: &std::path::Path, key: &str) -> anyhow::Result<()> {
    let meta = std::fs::metadata(path).map_err(|e| {
        anyhow::anyhow!(
            "{key}={} not found or not accessible: {e:#} (see analysis/specs/MODE_3_REWORK_SPEC.md \
             §14 failure mode #1 / Phase 0 setup)",
            path.display(),
        )
    })?;
    anyhow::ensure!(
        meta.is_file(),
        "{key}={} exists but is not a regular file (got file_type {:?}); set it to the binary \
         path",
        path.display(),
        meta.file_type(),
    );
    Ok(())
}

impl Mode3Account {
    fn default_view_key_env() -> String {
        "TARI_BENCH_VIEW_KEY".to_string()
    }
    fn default_spend_key_env() -> String {
        "TARI_BENCH_SPEND_KEY".to_string()
    }
}

impl Default for Mode3Account {
    fn default() -> Self {
        Self {
            view_key_env: Self::default_view_key_env(),
            public_spend_key_env: Self::default_spend_key_env(),
        }
    }
}

impl WorkerSleepOverrides {
    fn default_batch_creator() -> Option<u64> {
        Some(1)
    }
    fn default_unsigned_tx_creator() -> Option<u64> {
        Some(1)
    }
    fn default_transaction_signer() -> Option<u64> {
        Some(1)
    }
    fn default_broadcaster() -> Option<u64> {
        Some(1)
    }
    fn default_confirmation_checker() -> Option<u64> {
        Some(5)
    }
}

impl Default for WorkerSleepOverrides {
    fn default() -> Self {
        Self {
            batch_creator: Self::default_batch_creator(),
            unsigned_tx_creator: Self::default_unsigned_tx_creator(),
            transaction_signer: Self::default_transaction_signer(),
            broadcaster: Self::default_broadcaster(),
            confirmation_checker: Self::default_confirmation_checker(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            a_fund: Self::default_a_fund(),
            c_min: Self::default_c_min(),
            volume_target: Self::default_volume_target(),
            doubling_rounds: Self::default_doubling_rounds(),
            fanout_outputs_per_tx: Self::default_fanout_outputs_per_tx(),
            concurrent_batches: Self::default_concurrent_batches(),
            s4_t_budget_ms: Self::default_s4_t_budget_ms(),
            s5_m: Self::default_s5_m(),
            s5_k: Self::default_s5_k(),
            fee_rate: Self::default_fee_rate(),
            network: Self::default_network(),
            base_node_url: Self::default_base_node_url(),
            per_tx_confirmation_timeout_ms: Self::default_per_tx_confirmation_timeout_ms(),
            sampler_interval_ms: Self::default_sampler_interval_ms(),
            s1_amount_per_tx_microtari: Self::default_s1_amount_per_tx_microtari(),
            seeds: Seeds::default(),
            minotari_console_wallet_path: None,
            minotari_path: None,
            mode_3: None,
        }
    }
}

impl Seeds {
    fn default_old() -> String {
        defaults::SEED_ENV_OLD.to_string()
    }
    fn default_new() -> String {
        defaults::SEED_ENV_NEW.to_string()
    }
    fn default_payment_processor() -> String {
        defaults::SEED_ENV_PP.to_string()
    }
    fn default_wallet_password() -> String {
        defaults::WALLET_PW_ENV.to_string()
    }
}

impl Default for Seeds {
    fn default() -> Self {
        Self {
            old: Self::default_old(),
            new: Self::default_new(),
            payment_processor: Self::default_payment_processor(),
            wallet_password: Self::default_wallet_password(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_schema() {
        let cfg = Config::default();
        assert_eq!(cfg.a_fund, 10_000_000_000);
        assert_eq!(cfg.c_min, 3);
        assert_eq!(cfg.volume_target, 512);
        assert_eq!(cfg.doubling_rounds, 6);
        assert_eq!(cfg.fanout_outputs_per_tx, 8);
        assert_eq!(cfg.concurrent_batches, vec![8, 16, 32, 64, 128]);
        assert_eq!(cfg.s4_t_budget_ms, 900_000);
        assert_eq!(cfg.s5_m, 100);
        assert_eq!(cfg.s5_k, 10);
        assert_eq!(cfg.fee_rate, 5);
        assert_eq!(cfg.network, "esmeralda");
        assert_eq!(
            cfg.base_node_url.as_str(),
            "https://rpc.esmeralda.tari.com/"
        );
        assert_eq!(cfg.per_tx_confirmation_timeout_ms, 1_800_000);
        assert_eq!(cfg.sampler_interval_ms, 1_000);
        assert_eq!(cfg.s1_amount_per_tx_microtari, 1_000);
        assert_eq!(cfg.seeds.old, "HARNESS_SEED_OLD");
        assert_eq!(cfg.seeds.new, "HARNESS_SEED_NEW");
        assert_eq!(cfg.seeds.payment_processor, "HARNESS_SEED_PP");
        assert_eq!(cfg.seeds.wallet_password, "HARNESS_WALLET_PW");
        assert_eq!(cfg.minotari_console_wallet_path, None);
        assert_eq!(cfg.minotari_path, None);
        assert!(cfg.mode_3.is_none());
    }

    #[test]
    fn deserialize_empty_table_yields_defaults() {
        let cfg: Config = toml::from_str("").expect("empty TOML loads with all defaults");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn deserialize_partial_override_keeps_other_defaults() {
        let toml_src = r#"
            network = "esmeralda"
            base_node_url = "https://rpc.esmeralda.tari.com"
        "#;
        let cfg: Config = toml::from_str(toml_src).expect("partial TOML loads");
        assert_eq!(cfg.network, "esmeralda");
        assert_eq!(cfg.c_min, 3);
        assert_eq!(cfg.concurrent_batches, vec![8, 16, 32, 64, 128]);
    }

    #[test]
    fn deserialize_full_override_round_trips() {
        let original = Config {
            a_fund: 1,
            c_min: 2,
            volume_target: 3,
            doubling_rounds: 4,
            fanout_outputs_per_tx: 5,
            concurrent_batches: vec![1, 2, 3],
            s4_t_budget_ms: 6,
            s5_m: 7,
            s5_k: 8,
            fee_rate: 9,
            network: "esmeralda".to_string(),
            base_node_url: Url::parse("https://rpc.esmeralda.tari.com").unwrap(),
            per_tx_confirmation_timeout_ms: 10,
            sampler_interval_ms: 250,
            s1_amount_per_tx_microtari: 1234,
            seeds: Seeds {
                old: "OLD".to_string(),
                new: "NEW".to_string(),
                payment_processor: "PP".to_string(),
                wallet_password: "PW".to_string(),
            },
            minotari_console_wallet_path: Some(PathBuf::from(
                "/opt/tari/bin/minotari_console_wallet",
            )),
            minotari_path: Some(PathBuf::from("/opt/tari/bin/minotari")),
            mode_3: None,
        };
        let serialized = toml::to_string(&original).expect("serialize");
        let reloaded: Config = toml::from_str(&serialized).expect("round-trip");
        assert_eq!(reloaded, original);
    }

    #[test]
    fn deserialize_rejects_wrong_type() {
        // `network` is declared as `String`; an integer is a hard type error.
        let toml_src = "network = 42\n";
        let err = toml::from_str::<Config>(toml_src).expect_err("type mismatch should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("network") || msg.contains("string"),
            "error should describe the network/string mismatch: {msg}"
        );
    }

    // ---------- Mode 3 validate ----------
    //
    // The three policies validate() must enforce per @SWvheerden's review
    // of b7d05a3:
    //   (a) both override env vars set + valid hex → passes
    //   (b) both override env vars unset + seed env set → passes
    //   (c) both override env vars unset + seed env unset → fails

    /// Builds a `Mode3Config` with two binaries that *do* exist on every
    /// dev box (`/bin/sh`, `/bin/cat`) so `ensure_executable` passes; tests
    /// can then focus on the keypair-resolution policy without setting up
    /// a real PP binary on disk.
    fn build_validating_mode3(view_env: &str, spend_env: &str) -> Mode3Config {
        Mode3Config {
            pp_binary_path: PathBuf::from("/bin/sh"),
            minotari_binary_path: PathBuf::from("/bin/cat"),
            api_port: 9145,
            pr_port: 9146,
            pr_base_url: "https://rpc.esmeralda.tari.com".to_string(),
            terminal_state_poll_timeout_secs: 60,
            worker_sleep_overrides: WorkerSleepOverrides::default(),
            accounts: Mode3Accounts {
                bench: Mode3Account {
                    view_key_env: view_env.to_string(),
                    public_spend_key_env: spend_env.to_string(),
                },
            },
        }
    }

    /// 64-char ASCII hex used as a valid override value.
    const VALID_HEX_A: &str = "572a5fb63972da84aeec33071d13074e244d80c52be842ab5b0859ef4b4db00a";
    const VALID_HEX_B: &str = "40e65c9bbf4592bc995c421108c01a5d7c9f9b2239569757895134549cef371f";

    /// Mutate env via `set_var` / `remove_var`. Both calls are `unsafe` on
    /// modern Rust; allow `unused_unsafe` for older toolchains.
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

    /// Unique env-var names per-test to avoid cross-test mutation races
    /// under cargo's default parallel runner.
    fn unique_envs(suffix: &str) -> (String, String, String) {
        (
            format!("WB_TEST_MODE3_VIEW_{suffix}"),
            format!("WB_TEST_MODE3_SPEND_{suffix}"),
            format!("WB_TEST_MODE3_SEED_{suffix}"),
        )
    }

    fn seeds_with_pp(seed_env: &str) -> Seeds {
        Seeds {
            old: "WB_TEST_SEED_OLD".to_string(),
            new: "WB_TEST_SEED_NEW".to_string(),
            payment_processor: seed_env.to_string(),
            wallet_password: "WB_TEST_PW".to_string(),
        }
    }

    #[test]
    fn mode3_validate_passes_with_both_override_env_vars_set_to_valid_hex() {
        // Case (a): override env vars set + hex valid → validate passes
        // without needing the seed env var to be set.
        let (view, spend, seed) = unique_envs("OVERRIDE_VALID");
        let cfg = build_validating_mode3(&view, &spend);
        let seeds = seeds_with_pp(&seed);
        set_env(&view, VALID_HEX_A);
        set_env(&spend, VALID_HEX_B);
        unset_env(&seed); // explicitly unset to prove the seed is not required
        let result = cfg.validate(&seeds);
        unset_env(&view);
        unset_env(&spend);
        assert!(
            result.is_ok(),
            "override path with valid hex should pass: {:?}",
            result.err(),
        );
    }

    #[test]
    fn mode3_validate_passes_with_both_override_env_vars_unset_and_seed_set() {
        // Case (b): override env vars unset + seed env set → lifecycle
        // will derive the pair; validate passes without inspecting the
        // seed value itself (the derive happens at lifecycle spawn).
        let (view, spend, seed) = unique_envs("SEED_PATH");
        let cfg = build_validating_mode3(&view, &spend);
        let seeds = seeds_with_pp(&seed);
        unset_env(&view);
        unset_env(&spend);
        set_env(&seed, "any non-empty value passes presence check");
        let result = cfg.validate(&seeds);
        unset_env(&seed);
        assert!(
            result.is_ok(),
            "seed-derive path with seed set should pass: {:?}",
            result.err(),
        );
    }

    #[test]
    fn mode3_validate_fails_when_overrides_and_seed_all_unset() {
        // Case (c): override env vars unset + seed env unset → no path to
        // resolve the keypair. Bail with a message naming both paths.
        let (view, spend, seed) = unique_envs("ALL_UNSET");
        let cfg = build_validating_mode3(&view, &spend);
        let seeds = seeds_with_pp(&seed);
        unset_env(&view);
        unset_env(&spend);
        unset_env(&seed);
        let err = cfg
            .validate(&seeds)
            .expect_err("no resolvable keypair must fail");
        let msg = format!("{err:#}");
        // Error must name both resolution paths so the operator knows
        // their two options.
        assert!(
            msg.contains(&view) && msg.contains(&spend),
            "error must name both override env vars: {msg}",
        );
        assert!(
            msg.contains(&seed),
            "error must name the seed env var: {msg}",
        );
    }

    #[test]
    fn mode3_validate_fails_when_only_view_override_is_set() {
        // Mixed override: ${view} set, ${spend} not → bail. Pairing
        // half an override with a seed-derived half would risk crossing
        // wallets.
        let (view, spend, seed) = unique_envs("HALF_VIEW");
        let cfg = build_validating_mode3(&view, &spend);
        let seeds = seeds_with_pp(&seed);
        set_env(&view, VALID_HEX_A);
        unset_env(&spend);
        set_env(
            &seed,
            "irrelevant — half-override should bail before seed check",
        );
        let err = cfg.validate(&seeds).expect_err("half override must fail");
        unset_env(&view);
        unset_env(&seed);
        let msg = format!("{err:#}");
        assert!(
            msg.contains("half-set") && msg.contains(&view) && msg.contains(&spend),
            "error must flag half-set + name both env vars: {msg}",
        );
    }

    #[test]
    fn mode3_validate_fails_when_only_spend_override_is_set() {
        // Mirror of the above: ${spend} set, ${view} not → bail.
        let (view, spend, seed) = unique_envs("HALF_SPEND");
        let cfg = build_validating_mode3(&view, &spend);
        let seeds = seeds_with_pp(&seed);
        unset_env(&view);
        set_env(&spend, VALID_HEX_B);
        unset_env(&seed);
        let err = cfg.validate(&seeds).expect_err("half override must fail");
        unset_env(&spend);
        let msg = format!("{err:#}");
        assert!(
            msg.contains("half-set") && msg.contains(&view) && msg.contains(&spend),
            "error must flag half-set + name both env vars: {msg}",
        );
    }

    #[test]
    fn mode3_validate_fails_when_override_view_key_is_not_64_hex_chars() {
        // Override path's hex shape gate. A short / non-hex value is
        // almost always a paste error; surface it at validate time
        // rather than as a clap parse failure inside `minotari import-view-key`.
        let (view, spend, seed) = unique_envs("BAD_VIEW");
        let cfg = build_validating_mode3(&view, &spend);
        let seeds = seeds_with_pp(&seed);
        set_env(&view, "too short");
        set_env(&spend, VALID_HEX_B);
        let err = cfg.validate(&seeds).expect_err("invalid hex must fail");
        unset_env(&view);
        unset_env(&spend);
        let msg = format!("{err:#}");
        assert!(
            msg.contains("64 hex chars"),
            "error must name the hex-length rule: {msg}",
        );
    }
}
