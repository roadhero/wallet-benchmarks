//! TOML loader for [`crate::config::Config`].
//!
//! Every field on `Config` carries `#[serde(default)]`, so a minimal `harness.toml`
//! that only sets a handful of keys still loads — the remaining keys come from the
//! documented defaults in `RESULT_PROFILE_SCHEMA.md §1`. Errors at this layer are
//! I/O failures, TOML syntax/type mismatches, or unknown top-level keys; the
//! last get an operator-friendly message with a suggested correction (an
//! operator once ran with the seed env-var names as bare top-level keys, which
//! the old loader silently ignored because the `[seeds]` defaults happen to
//! carry the same names).

use std::path::Path;

use anyhow::Context;

use crate::config::Config;

const LOG_TARGET: &str = "c::config::load";

/// Read a `harness.toml` from disk and deserialize it into a [`Config`].
///
/// Unknown top-level keys are rejected BEFORE serde parsing with a message
/// that names the key, suggests the closest valid alternative (curated
/// aliases for the seed-configuration mistakes, then a token heuristic, then
/// edit distance against the real schema keys), and points at the RUNBOOK.
/// `#[serde(deny_unknown_fields)]` on `Config` remains the backstop for any
/// entry path that bypasses this loader.
pub fn load(path: &Path) -> anyhow::Result<Config> {
    log::debug!(target: LOG_TARGET, "loading config from {}", path.display());
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file at {}", path.display()))?;
    reject_unknown_top_level_keys(&raw)
        .with_context(|| format!("checking config keys in {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw)
        .with_context(|| format!("parsing config TOML at {}", path.display()))?;
    Ok(cfg)
}

/// A `Config` with EVERY `Option` field populated, used solely to derive the
/// known top-level key set.
///
/// Why not `Config::default()`: TOML has no null, so the toml serializer
/// silently DROPS `None`-valued keys. Deriving the key set from the default
/// therefore excluded every Option field (`minotari_console_wallet_path`,
/// `minotari_path`, `mode_3`) and the loader rejected valid operator
/// configs at startup; observed live on a maintainer run 2026-07-09.
///
/// Maintenance contract: any NEW `Option` field added to `Config` MUST be
/// populated here or it will be rejected as unknown. Enforced by the
/// exhaustive destructuring in `config_accepts_every_valid_top_level_field`,
/// which fails to compile when `Config` gains a field, forcing the reader
/// to this site.
fn fully_populated_probe() -> Config {
    use crate::config::{Mode3Account, Mode3Accounts, Mode3Config, WorkerSleepOverrides};
    Config {
        minotari_console_wallet_path: Some(std::path::PathBuf::from("/probe")),
        minotari_path: Some(std::path::PathBuf::from("/probe")),
        mode_3: Some(Mode3Config {
            pp_binary_path: std::path::PathBuf::from("/probe"),
            minotari_binary_path: std::path::PathBuf::from("/probe"),
            api_port: 1,
            pr_port: 2,
            pr_base_url: "http://probe".to_string(),
            terminal_state_poll_timeout_secs: 1,
            worker_sleep_overrides: WorkerSleepOverrides::default(),
            accounts: Mode3Accounts {
                bench: Mode3Account {
                    view_key_env: "PROBE_VIEW".to_string(),
                    public_spend_key_env: "PROBE_SPEND".to_string(),
                },
            },
        }),
        ..Config::default()
    }
}

/// The known top-level key set, derived from a fully-populated `Config` so
/// Option fields are present (pinned by `known_keys_match_config_schema`
/// and `config_accepts_every_valid_top_level_field`).
fn known_top_level_keys() -> Vec<String> {
    let value = toml::Value::try_from(fully_populated_probe())
        .expect("probe Config serializes to TOML (programmer error otherwise)");
    match value {
        toml::Value::Table(t) => t.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// Curated suggestions for the seed-configuration mistake class: these are
/// `[seeds]` table keys operators have put at the top level.
fn seed_alias_suggestion(key: &str) -> Option<String> {
    let (table_key, env_var) = match key {
        "old" => ("old", "HARNESS_SEED_OLD"),
        "new" => ("new", "HARNESS_SEED_NEW"),
        "payment_processor" => ("payment_processor", "HARNESS_SEED_PP"),
        "wallet_password" => ("wallet_password", "HARNESS_WALLET_PW"),
        _ => return None,
    };
    Some(format!(
        "did you mean the [seeds] table key `{table_key}` (value = the name of the \
         env var holding the secret, default `{env_var}`)?"
    ))
}

/// Token heuristic: a key that mentions seed material almost certainly wanted
/// the `[seeds]` table.
fn seed_token_suggestion(key: &str) -> Option<String> {
    let k = key.to_ascii_lowercase();
    for token in ["seed", "old", "new", "password", "mnemonic"] {
        if k.contains(token) {
            return Some(
                "seed configuration lives in the [seeds] table (keys `old`, `new`, \
                 `payment_processor`, `wallet_password`, each naming an env var)"
                    .to_string(),
            );
        }
    }
    None
}

/// Hand-rolled Levenshtein distance (the config key space is tiny; a
/// dependency would be heavier than these few lines).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn nearest_known_key(key: &str, known: &[String]) -> Option<String> {
    known
        .iter()
        .map(|k| (levenshtein(key, k), k))
        .filter(|(d, _)| *d <= 3)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| format!("did you mean `{k}`?"))
}

/// Reject unknown top-level keys with an operator-friendly message. The
/// error is the fallback: it must name the key, suggest the likely intent,
/// and point at the RUNBOOK.
fn reject_unknown_top_level_keys(raw: &str) -> anyhow::Result<()> {
    let doc: toml::Value = toml::from_str(raw).context("parsing TOML document structure")?;
    let toml::Value::Table(table) = doc else {
        return Ok(());
    };
    let known = known_top_level_keys();
    let mut problems: Vec<String> = Vec::new();
    for key in table.keys() {
        if known.iter().any(|k| k == key) {
            continue;
        }
        let suggestion = seed_alias_suggestion(key)
            .or_else(|| seed_token_suggestion(key))
            .or_else(|| nearest_known_key(key, &known));
        let line = match suggestion {
            Some(s) => format!("unknown top-level key `{key}`: {s}"),
            None => format!("unknown top-level key `{key}` (no similar known key)"),
        };
        problems.push(line);
    }
    if problems.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "{}\nSee RUNBOOK.md sections 2.3-2.7 (seed configuration) and 3.1 (top-level \
         keys) for the valid key set.",
        problems.join("\n"),
    )
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

    #[test]
    fn known_keys_match_config_schema() {
        // The suggestion machinery derives the key set from a fully
        // populated probe at runtime; this pin guarantees the derivation
        // includes BOTH plain fields and every Option field. The Option
        // entries here are the regression pin for the 2026-07-09 defect
        // (deriving from Config::default() dropped None-valued keys and
        // the loader rejected valid operator configs).
        let known = known_top_level_keys();
        for expected in [
            "network",
            "a_fund",
            "seeds",
            "fee_rate",
            "minotari_console_wallet_path",
            "minotari_path",
            "mode_3",
        ] {
            assert!(
                known.iter().any(|k| k == expected),
                "derived key set must contain `{expected}`: {known:?}",
            );
        }
    }

    /// Every valid top-level field, as a (key, minimal-valid-TOML) pair.
    ///
    /// COMPILE-TIME COMPLETENESS GUARD: the exhaustive destructuring below
    /// (no `..`) fails to compile the moment `Config` gains a field, forcing
    /// whoever adds one to extend this list, the probe in
    /// `fully_populated_probe` (for Option fields), and the acceptance test.
    fn every_top_level_field() -> Vec<(&'static str, &'static str)> {
        let Config {
            a_fund: _,
            c_min: _,
            volume_target: _,
            doubling_rounds: _,
            fanout_outputs_per_tx: _,
            concurrent_batches: _,
            s4_t_budget_ms: _,
            s5_m: _,
            s5_k: _,
            fee_rate: _,
            network: _,
            base_node_url: _,
            per_tx_confirmation_timeout_ms: _,
            wallet_ready_deadline_ms: _,
            s0_change_confirm_timeout_secs: _,
            fail_fast_identical_failure_threshold: _,
            sampler_interval_ms: _,
            s1_amount_per_tx_microtari: _,
            seeds: _,
            minotari_console_wallet_path: _,
            minotari_path: _,
            mode_3: _,
        } = Config::default();
        vec![
            ("a_fund", "a_fund = 1"),
            ("c_min", "c_min = 1"),
            ("volume_target", "volume_target = 1"),
            ("doubling_rounds", "doubling_rounds = 1"),
            ("fanout_outputs_per_tx", "fanout_outputs_per_tx = 1"),
            ("concurrent_batches", "concurrent_batches = [1]"),
            ("s4_t_budget_ms", "s4_t_budget_ms = 1"),
            ("s5_m", "s5_m = 1"),
            ("s5_k", "s5_k = 1"),
            ("fee_rate", "fee_rate = 1"),
            ("network", "network = \"esmeralda\""),
            (
                "base_node_url",
                "base_node_url = \"https://rpc.esmeralda.tari.com\"",
            ),
            (
                "per_tx_confirmation_timeout_ms",
                "per_tx_confirmation_timeout_ms = 1",
            ),
            ("wallet_ready_deadline_ms", "wallet_ready_deadline_ms = 1"),
            (
                "s0_change_confirm_timeout_secs",
                "s0_change_confirm_timeout_secs = 1",
            ),
            (
                "fail_fast_identical_failure_threshold",
                "fail_fast_identical_failure_threshold = 1",
            ),
            ("sampler_interval_ms", "sampler_interval_ms = 1"),
            (
                "s1_amount_per_tx_microtari",
                "s1_amount_per_tx_microtari = 5000",
            ),
            ("seeds", "[seeds]\nold = \"SOME_ENV\""),
            (
                "minotari_console_wallet_path",
                "minotari_console_wallet_path = \"/usr/local/bin/minotari_console_wallet\"",
            ),
            (
                "minotari_path",
                "minotari_path = \"/usr/local/bin/minotari\"",
            ),
            (
                "mode_3",
                "[mode_3]\npp_binary_path = \"/x/pp\"\nminotari_binary_path = \"/x/minotari\"",
            ),
        ]
    }

    #[test]
    fn config_accepts_every_valid_top_level_field() {
        // Acceptance-path coverage for every field kind, one key at a time:
        // the 2026-07-09 defect rejected valid Option-typed keys while all
        // rejection-path tests passed. A minimal config per field must load.
        for (key, snippet) in every_top_level_field() {
            let file = write_toml(&format!("{snippet}\n"));
            load(file.path())
                .unwrap_or_else(|e| panic!("valid key `{key}` must be accepted, got: {e:#}"));
        }
        // And all of them together in one document.
        let all: String = every_top_level_field()
            .iter()
            // Table-valued keys ([seeds], [mode_3]) must come after the
            // plain keys in a TOML document.
            .filter(|(_, s)| !s.starts_with('['))
            .map(|(_, s)| format!("{s}\n"))
            .chain(
                every_top_level_field()
                    .iter()
                    .filter(|(_, s)| s.starts_with('['))
                    .map(|(_, s)| format!("{s}\n")),
            )
            .collect();
        let file = write_toml(&all);
        load(file.path()).expect("a config naming every valid key must load");
    }

    #[test]
    fn config_accepts_swvheerden_paste_verbatim() {
        // Regression fixture from the maintainer's 2026-07-09 report: his
        // config was rejected on its two binary-path keys. Reconstructed
        // from his 2026-07-08 paste with one documented adaptation: the four
        // seed env-var names appear under [seeds] rather than as bare
        // top-level keys, because the loader rejects the bare form BY DESIGN
        // (and his 2026-07-09 error listed only the two path keys, so his
        // current file no longer carries them bare).
        let file = write_toml(
            "network = \"esmeralda\"\n\
             base_node_url = \"https://rpc.esmeralda.tari.com\"\n\
             minotari_console_wallet_path = \"tools/minotari_console_wallet\"\n\
             minotari_path = \"tools/minotari\"\n\
             [seeds]\n\
             old = \"HARNESS_SEED_OLD\"\n\
             new = \"HARNESS_SEED_NEW\"\n\
             payment_processor = \"HARNESS_SEED_PP\"\n\
             wallet_password = \"HARNESS_WALLET_PW\"\n",
        );
        let cfg = load(file.path()).expect("the maintainer's config shape must load without error");
        assert_eq!(
            cfg.minotari_path.as_deref(),
            Some(std::path::Path::new("tools/minotari")),
        );
        assert!(cfg.mode_3.is_none());
    }

    #[test]
    fn config_error_names_unknown_key_and_suggests_alternative() {
        // Case 1: bare `old` at top level (the observed operator mistake) ->
        // curated alias pointing at [seeds] old + the env var.
        let file = write_toml("old = \"HARNESS_SEED_OLD\"\n");
        let err = load(file.path()).expect_err("bare seed key must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown top-level key `old`"), "{msg}");
        assert!(
            msg.contains("[seeds]") && msg.contains("HARNESS_SEED_OLD"),
            "must suggest the seeds table and env var: {msg}",
        );
        assert!(msg.contains("RUNBOOK"), "must point at the RUNBOOK: {msg}");

        // Case 2: typo `sed_old` -> token heuristic points at the seeds table.
        let file = write_toml("sed_old = \"HARNESS_SEED_OLD\"\n");
        let err = load(file.path()).expect_err("typo seed key must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown top-level key `sed_old`"), "{msg}");
        assert!(
            msg.contains("[seeds]"),
            "token heuristic must suggest the seeds table: {msg}",
        );

        // Case 3: genuinely unknown key -> clean error, no suggestion.
        let file = write_toml("foo_bar = 1\n");
        let err = load(file.path()).expect_err("unknown key must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown top-level key `foo_bar`"), "{msg}");
        assert!(
            msg.contains("no similar known key"),
            "no spurious suggestion for foo_bar: {msg}",
        );
    }

    #[test]
    fn near_miss_key_gets_edit_distance_suggestion() {
        // `fee_rat` is one edit from `fee_rate` and carries no seed token.
        let file = write_toml("fee_rat = 5\n");
        let err = load(file.path()).expect_err("near-miss key must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("did you mean `fee_rate`?"),
            "edit-distance suggestion expected: {msg}",
        );
    }

    #[test]
    fn shipped_example_config_parses() {
        // Pins harness.toml.example against drift: if the example ever names
        // a key the schema does not have, this fails at CI time instead of
        // on an operator's machine.
        let example = include_str!("../../harness.toml.example");
        let file = write_toml(example);
        load(file.path()).expect("the shipped example must always parse");
    }
}
