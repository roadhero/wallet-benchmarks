//! Result-profile redaction denylist.
//!
//! Per `analysis/RESULT_PROFILE_SCHEMA.md §6`, the harness ships a 10-rule
//! denylist that runs against any value serialised into the result profile.
//! Rules are a mix of static regexes (BIP-39 / Tari mnemonic shape, hex/base64
//! blob shapes, Bearer tokens) and runtime substring matches keyed off the
//! current env (seed phrases, passphrase, `$USER` path leakage).
//!
//! Two contracts apply:
//!
//!   1. [`RedactionDenylist::check`] runs after the profile is built and bails
//!      if any rule matches the JSON serialisation. Match positions are
//!      reported by index, never by content — the offending substring is
//!      never echoed into the error.
//!   2. The denylist itself is serialised into the profile (per schema §6
//!      line 265) so the contract is auditable from the artifact alone. The
//!      [`RedactionRule`] `Serialize` impl elides the env-derived `value`
//!      fields and the compiled `Regex` body — only `(id, kind, reason)`
//!      survive the JSON dump.

use std::sync::OnceLock;

use anyhow::Context;
use regex::Regex;
use serde::ser::{Serialize, SerializeStruct, Serializer};

use crate::config::Seeds;

const LOG_TARGET: &str = "c::seed::redact";

/// A single denylist rule. Variants mirror `RESULT_PROFILE_SCHEMA.md §6`.
pub enum RedactionRule {
    /// Compiled regex that triggers when a match appears in the serialised
    /// profile.
    Regex {
        id: &'static str,
        pattern: Regex,
        reason: &'static str,
    },
    /// Substring that triggers when found in the serialised profile. Used for
    /// env-derived values (seeds, passphrase, `$USER` username) — the
    /// substring itself is not echoed into the result profile's
    /// representation of the rule.
    Substring {
        id: &'static str,
        value: String,
        reason: &'static str,
    },
}

impl RedactionRule {
    /// Stable rule identifier (e.g. `"R1"`).
    pub fn id(&self) -> &'static str {
        match self {
            Self::Regex { id, .. } | Self::Substring { id, .. } => id,
        }
    }

    /// Human-readable reason recorded alongside the rule.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Regex { reason, .. } | Self::Substring { reason, .. } => reason,
        }
    }

    /// Returns the rule's variant kind as a static string, used by the
    /// auditable serialisation form.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Regex { .. } => "regex",
            Self::Substring { .. } => "substring",
        }
    }

    /// Returns `true` if `haystack` matches the rule. For [`Self::Substring`]
    /// an empty `value` never matches — that's the "env var unset at startup"
    /// degenerate case and we don't want it to redact every single character.
    fn matches(&self, haystack: &str) -> bool {
        match self {
            Self::Regex { pattern, .. } => pattern.is_match(haystack),
            Self::Substring { value, .. } => !value.is_empty() && haystack.contains(value.as_str()),
        }
    }
}

impl std::fmt::Debug for RedactionRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the env-derived value; the regex pattern body is also
        // elided to keep Debug consistent with the auditable serialisation.
        f.debug_struct("RedactionRule")
            .field("id", &self.id())
            .field("kind", &self.kind())
            .field("reason", &self.reason())
            .finish()
    }
}

/// Custom `Serialize` impl: emits only `(id, kind, reason)` per schema §6.
/// The compiled regex pattern body and the env-derived substring value are
/// deliberately elided so the JSON copy in the result profile never echoes
/// the secret material the denylist guards against.
impl Serialize for RedactionRule {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RedactionRule", 3)?;
        state.serialize_field("id", self.id())?;
        state.serialize_field("kind", self.kind())?;
        state.serialize_field("reason", self.reason())?;
        state.end()
    }
}

/// Full denylist — schema §6 rules R1..R10. Populated by
/// [`Self::init_from_env`].
pub struct RedactionDenylist {
    rules: Vec<RedactionRule>,
}

impl RedactionDenylist {
    /// Build the denylist by combining the static regex rules with runtime
    /// substring rules sourced from the env vars named in `seeds`.
    ///
    /// Missing env vars are tolerated — the corresponding substring rule
    /// degrades to an empty-string match (which `matches` treats as never
    /// firing). This keeps unit tests usable without forcing every test to
    /// export every env var.
    pub fn init_from_env(seeds: &Seeds) -> Self {
        log::debug!(target: LOG_TARGET, "initialising redaction denylist from env");
        let mut rules: Vec<RedactionRule> = Vec::new();

        // R1: BIP-39 12/24-word or Tari 24-word mnemonic shape.
        rules.push(RedactionRule::Regex {
            id: "R1",
            pattern: r1_pattern().clone(),
            reason: "Tari 24-word mnemonic OR BIP-39 12/24-word phrase — \
                     regex catches the structural shape of both.",
        });

        // R2: the three seed mnemonics, each as a whole and each word.
        push_seed_substrings(&mut rules, "R2", &seeds.old);
        push_seed_substrings(&mut rules, "R2", &seeds.new);
        push_seed_substrings(&mut rules, "R2", &seeds.payment_processor);

        // R3: hex-encoded view key.
        rules.push(RedactionRule::Regex {
            id: "R3",
            pattern: r3_pattern().clone(),
            reason: "Hex-encoded view key.",
        });
        // R4: hex-encoded spend key.
        rules.push(RedactionRule::Regex {
            id: "R4",
            pattern: r4_pattern().clone(),
            reason: "Hex-encoded spend key.",
        });

        // R5: wallet passphrase (substring).
        rules.push(RedactionRule::Substring {
            id: "R5",
            value: std::env::var(&seeds.wallet_password).unwrap_or_default(),
            reason: "Wallet passphrase.",
        });

        // R6: long hex blob — raw signed-tx body threshold.
        rules.push(RedactionRule::Regex {
            id: "R6",
            pattern: r6_pattern().clone(),
            reason: "Long hex blob — raw signed-tx body threshold.",
        });
        // R7: long base64 blob — alt-encoding raw tx threshold.
        rules.push(RedactionRule::Regex {
            id: "R7",
            pattern: r7_pattern().clone(),
            reason: "Long base64 blob — alt-encoding raw tx threshold.",
        });
        // R8: gRPC/HTTP bearer tokens.
        rules.push(RedactionRule::Regex {
            id: "R8",
            pattern: r8_pattern().clone(),
            reason: "gRPC/HTTP bearer tokens.",
        });

        // R9: $USER / $LOGNAME username leakage.
        let user = current_username();
        rules.push(RedactionRule::Substring {
            id: "R9",
            value: user.clone(),
            reason: "$USER / $LOGNAME username leakage.",
        });

        // R10: /Users/<user>/ and /home/<user>/ path leakage.
        if !user.is_empty() {
            rules.push(RedactionRule::Substring {
                id: "R10",
                value: format!("/Users/{user}/"),
                reason: "macOS user-path leakage.",
            });
            rules.push(RedactionRule::Substring {
                id: "R10",
                value: format!("/home/{user}/"),
                reason: "Linux user-path leakage.",
            });
        }

        Self { rules }
    }

    /// Read-only view of the rules. Tests use this to assert R1..R10 are all
    /// populated.
    pub fn rules(&self) -> &[RedactionRule] {
        &self.rules
    }

    /// Thin test-only constructor — builds against a freshly-defaulted
    /// [`Seeds`] with no env vars set. Used by scenario unit tests that
    /// need a `&RedactionDenylist` to populate
    /// [`crate::scenarios::ScenarioCtx`] without touching env vars. The
    /// returned denylist still applies the static regex rules (R1, R3,
    /// R4, R6, R7, R8) — only the env-derived substring rules degrade
    /// to never-firing per [`RedactionRule::matches`].
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::init_from_env(&Seeds::default())
    }

    /// Serialise `profile` to JSON and check every rule against the dump.
    ///
    /// Returns `Ok(())` if no rule matches. On match, bails with the
    /// matching rule's `id` and the byte offset of the first match — the
    /// matched substring itself is never echoed into the error, so a panic
    /// or log surface won't accidentally re-leak the secret the rule
    /// caught.
    pub fn check<T: Serialize>(&self, profile: &T) -> anyhow::Result<()> {
        let serialised =
            serde_json::to_string(profile).context("serialising profile for redaction check")?;
        for rule in &self.rules {
            if rule.matches(&serialised) {
                let position = match rule {
                    RedactionRule::Regex { pattern, .. } => pattern
                        .find(&serialised)
                        .map(|m| m.start())
                        .unwrap_or(usize::MAX),
                    RedactionRule::Substring { value, .. } => {
                        serialised.find(value.as_str()).unwrap_or(usize::MAX)
                    }
                };
                anyhow::bail!(
                    "redaction denylist tripped: rule {} ({}) matched serialised profile at byte offset {}",
                    rule.id(),
                    rule.reason(),
                    position,
                );
            }
        }
        Ok(())
    }
}

impl Serialize for RedactionDenylist {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Delegate to the slice serialiser — each `RedactionRule` already
        // elides its env-derived value through its own Serialize impl.
        self.rules.serialize(serializer)
    }
}

impl std::fmt::Debug for RedactionDenylist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedactionDenylist")
            .field("rules", &self.rules)
            .finish()
    }
}

/// Push the whole-mnemonic value AND each word as separate substring rules
/// under the same rule ID. Schema §6 R2 names both whole-string and per-word
/// matching explicitly.
fn push_seed_substrings(rules: &mut Vec<RedactionRule>, id: &'static str, env_name: &str) {
    let raw = std::env::var(env_name).unwrap_or_default();
    if raw.is_empty() {
        return;
    }
    rules.push(RedactionRule::Substring {
        id,
        value: raw.clone(),
        reason: "Captured seed phrase (whole-string match).",
    });
    for word in raw.split_whitespace() {
        if word.len() >= 3 {
            // Skip 1-2 letter tokens — they would over-match the JSON.
            rules.push(RedactionRule::Substring {
                id,
                value: word.to_string(),
                reason: "Captured seed phrase (per-word match).",
            });
        }
    }
}

/// $USER falls back to $LOGNAME, falls back to empty.
fn current_username() -> String {
    std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok())
        .unwrap_or_default()
}

// --- compiled regex patterns, lazily initialised on first access. ---

fn r1_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\b(?:[a-z]{3,8}\s+){11,23}[a-z]{3,8}\b").expect("R1 regex compiles")
    })
}

fn r3_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)\bview[-_ ]?key\b\s*[:=]\s*[0-9a-f]{32,}").expect("R3 regex compiles")
    })
}

fn r4_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)\bspend[-_ ]?key\b\s*[:=]\s*[0-9a-f]{32,}").expect("R4 regex compiles")
    })
}

fn r6_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#""[0-9a-fA-F]{2048,}""#).expect("R6 regex compiles"))
}

fn r7_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#""[A-Za-z0-9+/=]{1024,}""#).expect("R7 regex compiles"))
}

fn r8_pattern() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._\-]{20,}").expect("R8 regex compiles"))
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use serde_json::json;

    use super::*;
    use crate::gen_seed;

    fn unique_seeds(suffix: &str) -> Seeds {
        Seeds {
            old: format!("WALLET_BENCHMARKS_TEST_REDACT_OLD_{suffix}"),
            new: format!("WALLET_BENCHMARKS_TEST_REDACT_NEW_{suffix}"),
            payment_processor: format!("WALLET_BENCHMARKS_TEST_REDACT_PP_{suffix}"),
            wallet_password: format!("WALLET_BENCHMARKS_TEST_REDACT_PW_{suffix}"),
        }
    }

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

    #[derive(Serialize)]
    struct FakeProfile {
        scenario: String,
        notes: String,
    }

    #[test]
    fn denylist_catches_realistic_seed_phrase() {
        let seeds = unique_seeds("SEED_PHRASE");
        let mnemonic = gen_seed().expect("gen_seed");
        set_env(&seeds.old, &mnemonic);
        set_env(&seeds.new, "");
        set_env(&seeds.payment_processor, "");
        set_env(&seeds.wallet_password, "");
        let denylist = RedactionDenylist::init_from_env(&seeds);
        let profile = FakeProfile {
            scenario: "S0".to_string(),
            notes: format!("operator note containing the seed: {mnemonic}"),
        };
        let err = denylist
            .check(&profile)
            .expect_err("denylist must catch the embedded seed");
        unset_env(&seeds.old);
        unset_env(&seeds.new);
        unset_env(&seeds.payment_processor);
        unset_env(&seeds.wallet_password);
        let msg = format!("{err:#}");
        assert!(
            msg.contains("R1") || msg.contains("R2"),
            "matched rule id should be present in error: {msg}",
        );
        assert!(
            !msg.contains(&mnemonic),
            "error must NEVER echo the matched secret: {msg}",
        );
    }

    #[test]
    fn denylist_catches_username_path() {
        // R10 reads `$USER` from env — exercise it by setting a deliberately
        // unusual username for the duration of the test.
        const USER_FOR_TEST: &str = "wallet-benchmarks-redaction-canary";
        set_env("USER", USER_FOR_TEST);
        let seeds = unique_seeds("USERNAME_PATH");
        set_env(&seeds.wallet_password, "");
        let denylist = RedactionDenylist::init_from_env(&seeds);
        let profile = json!({
            "config": { "data_dir": format!("/Users/{USER_FOR_TEST}/.tari") },
        });
        let err = denylist
            .check(&profile)
            .expect_err("denylist must catch the username path");
        unset_env(&seeds.wallet_password);
        let msg = format!("{err:#}");
        assert!(
            msg.contains("R9") || msg.contains("R10"),
            "username-leak rule should fire: {msg}",
        );
        assert!(
            !msg.contains(USER_FOR_TEST),
            "error must NEVER echo the matched username: {msg}",
        );
    }

    #[test]
    fn denylist_serialization_elides_secret_values() {
        let seeds = unique_seeds("SERIAL_ELIDE");
        let mnemonic = gen_seed().expect("gen_seed");
        let passphrase = "test-secret-passphrase-do-not-log";
        set_env(&seeds.old, &mnemonic);
        set_env(&seeds.new, "");
        set_env(&seeds.payment_processor, "");
        set_env(&seeds.wallet_password, passphrase);
        let denylist = RedactionDenylist::init_from_env(&seeds);
        let dumped = serde_json::to_string(&denylist).expect("denylist serialises");
        unset_env(&seeds.old);
        unset_env(&seeds.new);
        unset_env(&seeds.payment_processor);
        unset_env(&seeds.wallet_password);
        assert!(
            !dumped.contains(&mnemonic),
            "denylist JSON must NOT contain the env-derived mnemonic value: {dumped}",
        );
        assert!(
            !dumped.contains(passphrase),
            "denylist JSON must NOT contain the env-derived passphrase: {dumped}",
        );
        // Sanity: it DOES contain the rule IDs and reasons.
        assert!(dumped.contains("R1"));
        assert!(dumped.contains("R2"));
        assert!(dumped.contains("R5"));
    }

    #[test]
    fn denylist_returns_ok_when_profile_is_clean() {
        let seeds = unique_seeds("CLEAN_PROFILE");
        set_env(&seeds.old, "");
        set_env(&seeds.new, "");
        set_env(&seeds.payment_processor, "");
        set_env(&seeds.wallet_password, "");
        let denylist = RedactionDenylist::init_from_env(&seeds);
        let profile = json!({
            "scenario": "S0",
            "duration_ms": 1234,
        });
        let r = denylist.check(&profile);
        unset_env(&seeds.old);
        unset_env(&seeds.new);
        unset_env(&seeds.payment_processor);
        unset_env(&seeds.wallet_password);
        r.expect("clean profile must not trip any rule");
    }

    #[test]
    fn denylist_catches_long_hex_blob() {
        let seeds = unique_seeds("LONG_HEX");
        set_env(&seeds.wallet_password, "");
        let denylist = RedactionDenylist::init_from_env(&seeds);
        let long_hex: String = "a".repeat(4096);
        let profile = json!({
            "raw_tx": long_hex,
        });
        let err = denylist
            .check(&profile)
            .expect_err("R6 should catch a 4096-char hex string");
        unset_env(&seeds.wallet_password);
        let msg = format!("{err:#}");
        assert!(msg.contains("R6"), "R6 should match: {msg}");
    }
}
