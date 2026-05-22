//! Library entry point for the `wallet-benchmarks` harness.
//!
//! See `analysis/DESIGN.md §Workspace layout` for the module roster. Modules are
//! added in the order specified in `analysis/DESIGN_ADDENDUM.md §S4 Pre-flight
//! execution order`.

pub mod cli;
pub mod config;
pub mod guards;

use anyhow::bail;

/// Stub for the `gen-seed` subcommand. The real implementation lands in a later
/// commit; this placeholder lets the CLI shape (`wallet-benchmarks gen-seed`)
/// parse and dispatch through the same code path.
pub fn gen_seed() -> anyhow::Result<String> {
    bail!("gen-seed not yet implemented");
}

/// Stub for the `print-address` subcommand. The real implementation lands in a
/// later commit; this placeholder lets the CLI shape parse and dispatch through
/// the same code path.
pub fn print_address(_seed_env_name: &str) -> anyhow::Result<String> {
    bail!("print-address not yet implemented");
}
