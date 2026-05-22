//! TOML loader for [`crate::config::Config`].
//!
//! Every field on `Config` carries `#[serde(default)]`, so a minimal `harness.toml`
//! that only sets a handful of keys still loads — the remaining keys come from the
//! documented defaults in `RESULT_PROFILE_SCHEMA.md §1`. Errors at this layer are
//! either I/O failures or TOML syntax/type mismatches; both surface as `anyhow`
//! errors annotated with the offending path.

use std::path::Path;

use anyhow::Context;

use crate::config::Config;

const LOG_TARGET: &str = "c::config::load";

/// Read a `harness.toml` from disk and deserialize it into a [`Config`].
///
/// The contract is intentionally narrow: read the bytes, parse as TOML, let `serde`
/// fill missing keys from the per-field defaults. Operators see one error type
/// (`anyhow::Error`) carrying the path of the file that failed.
pub fn load(path: &Path) -> anyhow::Result<Config> {
    log::debug!(target: LOG_TARGET, "loading config from {}", path.display());
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file at {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw)
        .with_context(|| format!("parsing config TOML at {}", path.display()))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn write_toml(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("create tempfile");
        file.write_all(contents.as_bytes())
            .expect("write TOML body");
        file
    }

    #[test]
    fn loads_minimal_toml_with_defaults_filled() {
        let file = write_toml("network = \"esmeralda\"\n");
        let cfg = load(file.path()).expect("minimal toml loads");
        assert_eq!(cfg.network, "esmeralda");
        // Defaults still applied for everything else.
        assert_eq!(cfg.c_min, 3);
        assert_eq!(cfg.s4_t_budget_ms, 900_000);
        assert_eq!(cfg.seeds.old, "HARNESS_SEED_OLD");
    }

    #[test]
    fn loads_empty_file_as_full_defaults() {
        let file = write_toml("");
        let cfg = load(file.path()).expect("empty TOML loads");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn errors_when_path_is_missing() {
        let missing = Path::new("/nonexistent/wallet-benchmarks-harness.toml");
        let err = load(missing).expect_err("missing path should error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reading config file"),
            "error context should name the read phase: {msg}"
        );
    }

    #[test]
    fn errors_when_toml_value_has_wrong_type() {
        let file = write_toml("c_min = \"three\"\n");
        let err = load(file.path()).expect_err("type mismatch should error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parsing config TOML"),
            "error context should name the parse phase: {msg}"
        );
    }
}
