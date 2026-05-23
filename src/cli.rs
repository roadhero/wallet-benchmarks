//! Top-level CLI shape for the `wallet-benchmarks` harness.
//!
//! The harness exposes three subcommands per `analysis/DESIGN_ADDENDUM.md §S1`:
//!
//! * `run` — the main benchmark harness (wired in a later step).
//! * `gen-seed` — generates a fresh 24-word Tari mnemonic to stdout.
//! * `print-address` — derives the wallet address from a seed mnemonic held in a
//!   named environment variable and prints it as a base58 string.
//!
//! Invoked with no subcommand, the binary defaults to `run` with a stock
//! `harness.toml` path — matching the maintainer's pattern in `minotari-cli`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Default `harness.toml` path used by the `run` subcommand when `--config` is
/// not supplied.
pub const DEFAULT_CONFIG_PATH: &str = "harness.toml";

/// Top-level CLI parser.
#[derive(Debug, Parser)]
#[command(
    name = "wallet-benchmarks",
    version,
    about = "Tari wallet benchmarking harness for Esmeralda",
    long_about = None,
)]
pub struct Cli {
    /// Subcommand to dispatch.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Harness subcommands.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Commands {
    /// Run the benchmark harness against the configured Esmeralda base node.
    Run {
        /// Path to the harness TOML config. Defaults to `harness.toml` in the
        /// current working directory.
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
        /// Path to write the canonical result-profile JSON. Defaults to
        /// `baselines/esmeralda_canonical.json` per
        /// `analysis/DESIGN_ADDENDUM.md §M4`.
        #[arg(long, default_value = "baselines/esmeralda_canonical.json")]
        output: PathBuf,
        /// Skip the funding pre-flight (`enforce_funding`). For testing
        /// only — production baseline runs MUST use the live pre-flight to
        /// catch under-funded seeds before consuming ~hours of scenario
        /// time. See `analysis/PR_BODY_PLAN.md §Operator Setup`.
        #[arg(long = "skip-funding-preflight")]
        skip_funding_preflight: bool,
    },
    /// Generate a fresh 24-word Tari mnemonic and print it to stdout.
    GenSeed,
    /// Derive the wallet address from the seed mnemonic held in the named env
    /// var and print it to stdout as a base58 string.
    PrintAddress {
        /// Name of the environment variable holding the 24-word mnemonic.
        #[arg(long = "seed-env")]
        seed_env: String,
    },
}

impl Cli {
    /// Returns the resolved command, defaulting to `Commands::Run` when no
    /// subcommand was supplied.
    pub fn resolved_command(self) -> Commands {
        self.command.unwrap_or(Commands::Run {
            config: PathBuf::from(DEFAULT_CONFIG_PATH),
            output: PathBuf::from("baselines/esmeralda_canonical.json"),
            skip_funding_preflight: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_run_with_explicit_config() {
        let cli = Cli::parse_from(["wallet-benchmarks", "run", "--config", "foo.toml"]);
        assert_eq!(
            cli.resolved_command(),
            Commands::Run {
                config: PathBuf::from("foo.toml"),
                output: PathBuf::from("baselines/esmeralda_canonical.json"),
                skip_funding_preflight: false,
            }
        );
    }

    #[test]
    fn parses_gen_seed_subcommand() {
        let cli = Cli::parse_from(["wallet-benchmarks", "gen-seed"]);
        assert_eq!(cli.resolved_command(), Commands::GenSeed);
    }

    #[test]
    fn parses_print_address_with_seed_env() {
        let cli = Cli::parse_from([
            "wallet-benchmarks",
            "print-address",
            "--seed-env",
            "HARNESS_SEED_OLD",
        ]);
        assert_eq!(
            cli.resolved_command(),
            Commands::PrintAddress {
                seed_env: "HARNESS_SEED_OLD".to_string(),
            }
        );
    }

    #[test]
    fn defaults_to_run_when_no_subcommand() {
        let cli = Cli::parse_from(["wallet-benchmarks"]);
        assert_eq!(
            cli.resolved_command(),
            Commands::Run {
                config: PathBuf::from(DEFAULT_CONFIG_PATH),
                output: PathBuf::from("baselines/esmeralda_canonical.json"),
                skip_funding_preflight: false,
            }
        );
    }

    #[test]
    fn run_with_no_config_uses_default_path() {
        let cli = Cli::parse_from(["wallet-benchmarks", "run"]);
        assert_eq!(
            cli.resolved_command(),
            Commands::Run {
                config: PathBuf::from(DEFAULT_CONFIG_PATH),
                output: PathBuf::from("baselines/esmeralda_canonical.json"),
                skip_funding_preflight: false,
            }
        );
    }
}
