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

    /// Names of the environment variables holding seed and passphrase material. The
    /// values themselves are read at runtime; only the names are persisted.
    #[serde(default)]
    pub seeds: Seeds,
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
            seeds: Seeds::default(),
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
        assert_eq!(cfg.seeds.old, "HARNESS_SEED_OLD");
        assert_eq!(cfg.seeds.new, "HARNESS_SEED_NEW");
        assert_eq!(cfg.seeds.payment_processor, "HARNESS_SEED_PP");
        assert_eq!(cfg.seeds.wallet_password, "HARNESS_WALLET_PW");
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
            seeds: Seeds {
                old: "OLD".to_string(),
                new: "NEW".to_string(),
                payment_processor: "PP".to_string(),
                wallet_password: "PW".to_string(),
            },
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
}
