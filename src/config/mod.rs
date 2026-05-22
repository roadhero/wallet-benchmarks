//! Harness configuration — minimal forward declaration for the `guards` module.
//!
//! Only the fields `guards::enforce_esmeralda` reads are defined here. The full
//! `Config` per `analysis/DESIGN.md §Workspace layout` (defaults, TOML loader,
//! env/CLI override) lands in a later step. Treat this as a placeholder that
//! later commits extend in-place.

use url::Url;

/// Harness configuration.
///
/// See `analysis/DESIGN.md §System Design` and `analysis/RESULT_PROFILE_SCHEMA.md §1`
/// for the full v1 shape. Only the fields required by `guards::enforce_esmeralda` are
/// present in this commit; the remaining keys land with the `config` module proper.
#[derive(Debug, Clone)]
pub struct Config {
    /// Tari network identifier. Hard-allowlisted to `"esmeralda"` by
    /// `guards::enforce_esmeralda`.
    pub network: String,
    /// Esmeralda base-node HTTP endpoint. Cross-checked against a mainnet host
    /// denylist by `guards::enforce_esmeralda`.
    pub base_node_url: Url,
}
