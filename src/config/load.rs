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

/// The known top-level key set, derived from `Config::default()` itself so it
/// can never drift from the struct (pinned by
/// `known_keys_match_config_schema`).
fn known_top_level_keys() -> Vec<String> {
    let value = toml::Value::try_from(Config::default())
        .expect("Config::default serializes to TOML (programmer error otherwise)");
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
        // The suggestion machinery derives the key set from Config::default()
        // at runtime; this pin guarantees the derivation works and includes
        // the keys the tests below rely on.
        let known = known_top_level_keys();
        for expected in ["network", "a_fund", "seeds", "fee_rate"] {
            assert!(
                known.iter().any(|k| k == expected),
                "derived key set must contain `{expected}`: {known:?}",
            );
        }
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
