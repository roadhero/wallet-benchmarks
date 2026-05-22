//! Entry point for the `wallet-benchmarks` harness.
//!
//! Parses the CLI per `analysis/DESIGN_ADDENDUM.md §S1`, dispatches to the
//! corresponding library entry point. The `run` subcommand is wired up in a
//! later step (scenarios); for now it prints a one-line "not yet wired" notice
//! and exits with status 2 (CLI-OK-but-action-not-ready) so operators see the
//! intended shape from `--help` without the harness pretending to do work.

use std::process::ExitCode;

use clap::Parser;
use wallet_benchmarks::{
    cli::{Cli, Commands},
    gen_seed, print_address,
};

const LOG_TARGET: &str = "c::main";

/// Exit code returned by the `run` subcommand until scenarios are wired in.
const EXIT_RUN_NOT_READY: u8 = 2;

fn main() -> ExitCode {
    env_logger::init();

    match Cli::parse().resolved_command() {
        Commands::Run { config } => {
            log::info!(target: LOG_TARGET, "run requested with config {}", config.display());
            // The scenario wiring lands in a later step; the CLI parses
            // successfully but executing the harness is not yet supported.
            eprintln!(
                "wallet-benchmarks run --config {}: scenarios will be wired up in a later step; not yet implemented",
                config.display()
            );
            ExitCode::from(EXIT_RUN_NOT_READY)
        }
        Commands::GenSeed => match gen_seed() {
            Ok(mnemonic) => {
                println!("{mnemonic}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("gen-seed failed: {err:#}");
                ExitCode::FAILURE
            }
        },
        Commands::PrintAddress { seed_env } => match print_address(&seed_env) {
            Ok(address) => {
                println!("{address}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("print-address failed: {err:#}");
                ExitCode::FAILURE
            }
        },
    }
}
