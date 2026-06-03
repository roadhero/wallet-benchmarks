//! Shared `minotari` wallet-management subprocess orchestration for Modes 2/3.
//!
//! Per `analysis/DESIGN_AMENDMENT.md §8.3` and `analysis/DESIGN.md §Mode 2 step 7`,
//! Modes 2 and 3 share the same wipe / re-import / scan / balance pipeline on the
//! new `minotari` CLI (from `tari-project/minotari-cli`, pinned at commit
//! `52a7287a3fe1e7831855649c530534af9f2d4830`). The send-side helper lives in
//! [`crate::modes::minotari_subprocess`]; this module is the read-side
//! counterpart.
//!
//! Mode 1 (`OldWalletMode`) is gRPC-based and **does not** route through this
//! module — it owns its own wipe/scan logic in [`crate::modes::old_wallet`].
//!
//! ## Subprocess shapes (verified against `cli.rs` at the pinned commit)
//!
//! All subcommands share top-level flags `--config <harness.toml>` and
//! `--network esmeralda` (the `--config` default of `config/config.toml`
//! is relative to CWD; the harness spawns from a controlled tempdir where
//! that path is absent — same plumbing as
//! [`crate::modes::minotari_subprocess::write_harness_toml`]).
//!
//! * **Create** (restores from seed words):
//!   `--password <pw> --database-path <db> --account-name default --seed-words "<24 words>"`.
//!   Successful exit means the wallet DB exists at `<db>`.
//!
//! * **Scan** (one-shot scan up to `--max-blocks-to-scan` blocks):
//!   `--password <pw> --database-path <db> --account-name default --max-blocks-to-scan <N>`.
//!   The subprocess emits human-readable progress on stderr; the harness
//!   ignores it. Output-discovery counts are read from the wallet sqlite3
//!   DB post-scan via [`crate::wallet_db`] (see PR #6 review threads
//!   4.1 + 4.3 and `analysis/specs/THREADS_4_1_4_3_SPEC.md`).
//!
//! * **Balance** (reads the wallet DB; **takes no `--password`**):
//!   `--database-path <db> --account-name default`. Stdout format is the literal
//!   `Balance at height {height}({date}): {total}` where `{total}` is
//!   [`MicroMinotari`]'s `Display` impl — `"{n} µT"` for amounts < 1 T,
//!   `"{n.nnnnnn} T"` for amounts ≥ 1 T. The parser matches both shapes
//!   (anchor strategy `(a)`: structural anchor on the unit sentinel, robust
//!   to label renames).
//!
//! [`MicroMinotari`]: tari_transaction_components::tari_amount::MicroMinotari

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::OnceLock,
};

use anyhow::Context;
use regex::Regex;
use tari_common_types::seeds::{
    cipher_seed::CipherSeed,
    mnemonic::{Mnemonic, MnemonicLanguage},
    seed_words::SeedWords,
};
use tari_transaction_components::tari_amount::MicroMinotari;
use tokio::process::Command;

use crate::{config::Config, wallet_lifecycle::HarnessDataDir};

const LOG_TARGET: &str = "c::modes::minotari_wallet_ops";

/// Default binary name resolved via `$PATH` when [`Config::minotari_path`] is
/// `None`. Matches [`crate::modes::minotari_subprocess::DEFAULT_BINARY`].
const DEFAULT_BINARY: &str = "minotari";

/// Top-level `--network` value passed on every subprocess invocation.
const NETWORK_FLAG_VALUE: &str = "esmeralda";

/// Account name set on every subprocess `--account-name` flag. Mirrors
/// [`crate::modes::minotari_subprocess::ACCOUNT_NAME`].
const ACCOUNT_NAME: &str = "default";

/// `--max-blocks-to-scan` value passed to `minotari Scan` when the caller does
/// not specify one. The cli.rs default is 50 (per `cli.rs` line 165 at the
/// pinned commit); the harness's B0/S2/S3/S6/S7 scenarios need a much higher
/// ceiling because they walk from genesis to tip. `u64::MAX` lets the
/// subprocess consume blocks until the wallet's underlying scan logic stops
/// (typically when it catches up to the live tip).
///
/// Justified in commit body; logged in `analysis/API_DRIFT.md` Step 3i.
const DEFAULT_MAX_BLOCKS_TO_SCAN: u64 = u64::MAX;

/// Build the argv for `minotari Create --seed-words "<mnemonic>"`.
///
/// Pure function — caller spawns the subprocess. Order is significant per
/// clap's parsing: top-level `--config` / `--network` BEFORE the subcommand,
/// subcommand flags AFTER. The mnemonic is passed as a SINGLE argv argument
/// (space-separated 24 words inside one string) — clap's `Option<String>`
/// declaration consumes the next value as a whole.
pub(super) fn build_create_argv(
    harness_toml_path: &Path,
    database_path: &Path,
    password: &str,
    mnemonic: &str,
) -> Vec<String> {
    vec![
        "--config".to_string(),
        harness_toml_path.display().to_string(),
        "--network".to_string(),
        NETWORK_FLAG_VALUE.to_string(),
        "create".to_string(),
        "--password".to_string(),
        password.to_string(),
        "--database-path".to_string(),
        database_path.display().to_string(),
        "--account-name".to_string(),
        ACCOUNT_NAME.to_string(),
        "--seed-words".to_string(),
        mnemonic.to_string(),
    ]
}

/// Build the argv for `minotari Scan`.
///
/// `max_blocks_to_scan` is the only knob the CLI exposes for bounding the
/// scan (per `cli.rs` lines 154-167 at the pinned commit). The harness uses
/// [`DEFAULT_MAX_BLOCKS_TO_SCAN`] when the caller passes `None`.
pub(super) fn build_scan_argv(
    harness_toml_path: &Path,
    database_path: &Path,
    password: &str,
    max_blocks_to_scan: u64,
) -> Vec<String> {
    vec![
        "--config".to_string(),
        harness_toml_path.display().to_string(),
        "--network".to_string(),
        NETWORK_FLAG_VALUE.to_string(),
        "scan".to_string(),
        "--password".to_string(),
        password.to_string(),
        "--database-path".to_string(),
        database_path.display().to_string(),
        "--account-name".to_string(),
        ACCOUNT_NAME.to_string(),
        "--max-blocks-to-scan".to_string(),
        max_blocks_to_scan.to_string(),
    ]
}

/// Build the argv for `minotari Balance`.
///
/// **`Balance` takes NO `--password` flag** at the pinned `cli.rs` (lines
/// 247-252) — it reads the DB without unlocking the master cipher seed.
/// Logged in `analysis/API_DRIFT.md` Step 3i.
pub(super) fn build_balance_argv(harness_toml_path: &Path, database_path: &Path) -> Vec<String> {
    vec![
        "--config".to_string(),
        harness_toml_path.display().to_string(),
        "--network".to_string(),
        NETWORK_FLAG_VALUE.to_string(),
        "balance".to_string(),
        "--database-path".to_string(),
        database_path.display().to_string(),
        "--account-name".to_string(),
        ACCOUNT_NAME.to_string(),
    ]
}

/// Decode `mnemonic` to a [`CipherSeed`], rewrite its birthday to
/// `new_birthday`, and re-encode to a fresh mnemonic string. Pure-Rust,
/// no IO. Mirrors [`crate::modes::old_wallet::rewrite_birthday`] for symmetry —
/// both Mode 1 (gRPC re-spawn) and Modes 2/3 (subprocess re-create) need the
/// same rewrite at the start of `wipe_and_reimport`.
pub(super) fn rewrite_birthday(mnemonic: &str, new_birthday: u16) -> anyhow::Result<String> {
    let seed_words = SeedWords::from_str(mnemonic)
        .map_err(|e| anyhow::Error::msg(format!("parsing mnemonic words: {e}")))?;
    let mut cipher_seed = <CipherSeed as Mnemonic<CipherSeed>>::from_mnemonic(&seed_words, None)
        .map_err(|e| anyhow::Error::msg(format!("decoding CipherSeed from mnemonic: {e}")))?;
    cipher_seed.change_birthday(new_birthday);
    let new_words = cipher_seed
        .to_mnemonic(MnemonicLanguage::English, None)
        .map_err(|e| anyhow::Error::msg(format!("re-encoding CipherSeed mnemonic: {e}")))?;
    Ok(new_words.join(" ").reveal().to_string())
}

/// Resolve the `minotari` binary path — explicit config override or `$PATH`.
fn resolve_binary(cfg: &Config) -> PathBuf {
    cfg.minotari_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BINARY))
}

/// Write (or rewrite) the minimal `harness.toml` the subprocess needs.
///
/// Mirrors [`crate::modes::minotari_subprocess::write_harness_toml`] — both
/// helpers want the same `network = "esmeralda"` floor for the subprocess's
/// config layer. Kept duplicated here so the read-side helper does not have
/// to reach into the send-side module's private surface.
fn write_harness_toml(data_dir: &Path) -> anyhow::Result<PathBuf> {
    let path = data_dir.join("harness.toml");
    let body = "network = \"esmeralda\"\n";
    std::fs::write(&path, body)
        .with_context(|| format!("writing harness.toml to {}", path.display()))?;
    Ok(path)
}

/// Standard subprocess env envelope: clear then add HOME / PATH / TARI_NETWORK.
/// Matches [`crate::modes::minotari_subprocess`]'s pattern verbatim so the
/// secret-handling surface is identical across send-side and read-side helpers.
fn subprocess_env(data_dir: &Path) -> (PathBuf, String) {
    let harness_home = data_dir.to_path_buf();
    let path_env = std::env::var("PATH").unwrap_or_default();
    (harness_home, path_env)
}

/// Wipe the harness data dir, then run `minotari Create --seed-words ...` to
/// initialise a fresh wallet DB from the supplied mnemonic.
///
/// Steps:
/// 1. `data_dir.wipe(data_dir.path())` — confined by AC-34's path check.
/// 2. Recreate the directory so subsequent steps can write into it.
/// 3. Write `harness.toml` (subprocess's default config path is relative to
///    CWD; the controlled spawn cwd has none).
/// 4. Spawn `minotari Create --seed-words "<mnemonic>"`.
///
/// On any failure, the partially-wiped data dir is left in place for operator
/// diagnosis (matches the send-side helper's "preserve on failure" convention).
pub(super) async fn wipe_and_reimport_via_create(
    cfg: &Config,
    data_dir: &mut HarnessDataDir,
    mnemonic_with_birthday: &str,
    password: &str,
) -> anyhow::Result<()> {
    let data_dir_path = data_dir.path().to_path_buf();
    data_dir
        .wipe(&data_dir_path)
        .context("wiping data dir before re-import")?;
    std::fs::create_dir_all(&data_dir_path)
        .with_context(|| format!("recreating wiped data dir {}", data_dir_path.display()))?;

    let harness_toml = write_harness_toml(&data_dir_path)?;
    let database_path = data_dir_path.join("wallet.sqlite3");
    let argv = build_create_argv(
        &harness_toml,
        &database_path,
        password,
        mnemonic_with_birthday,
    );
    let binary = resolve_binary(cfg);
    let (harness_home, path_env) = subprocess_env(&data_dir_path);

    log::debug!(
        target: LOG_TARGET,
        "spawning {} for Create --seed-words (data_dir={}, password redacted, mnemonic redacted)",
        binary.display(),
        data_dir_path.display(),
    );

    let output = Command::new(&binary)
        .args(&argv)
        .env_clear()
        .env("HOME", &harness_home)
        .env("PATH", &path_env)
        .env("TARI_NETWORK", NETWORK_FLAG_VALUE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| {
            format!(
                "spawning {} create (is the binary on $PATH or set Config::minotari_path?)",
                binary.display(),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        anyhow::bail!(
            "minotari create --seed-words exit {:?}; stderr={}",
            output.status,
            stderr,
        );
    }
    log::info!(
        target: LOG_TARGET,
        "minotari create --seed-words succeeded (db={})",
        database_path.display(),
    );
    Ok(())
}

/// Result of [`run_scan_subprocess`]. The scan subprocess only signals
/// success-or-failure; UTXO discovery counts are read post-scan from the
/// wallet sqlite3 DB via [`crate::wallet_db`] (PR #6 review threads
/// 4.1 + 4.3, `analysis/specs/THREADS_4_1_4_3_SPEC.md`). `max_blocks_to_scan`
/// is preserved on the result purely for log/telemetry symmetry — scenarios
/// compare against base-node tip deltas, not against this value directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScanStdoutParsed {
    /// Upper bound on the number of blocks scanned this invocation. The CLI
    /// does not emit a precise post-hoc count; the harness records what it
    /// asked for so scenarios can compare against base-node tip deltas.
    #[allow(dead_code)]
    pub max_blocks_to_scan: u64,
}

/// Spawn `minotari Scan` and wait for it to complete.
///
/// `RUST_LOG=info` is forced into the subprocess environment for visibility
/// in captured logs — the harness ignores stderr beyond using it to enrich
/// error messages on non-zero exit. UTXO counts are queried separately from
/// the wallet DB (see [`crate::wallet_db`]).
pub(super) async fn run_scan_subprocess(
    cfg: &Config,
    data_dir: &Path,
    password: &str,
    max_blocks: Option<u64>,
) -> anyhow::Result<ScanStdoutParsed> {
    let harness_toml = write_harness_toml(data_dir)?;
    let database_path = data_dir.join("wallet.sqlite3");
    let max_blocks_to_scan = max_blocks.unwrap_or(DEFAULT_MAX_BLOCKS_TO_SCAN);
    let argv = build_scan_argv(&harness_toml, &database_path, password, max_blocks_to_scan);
    let binary = resolve_binary(cfg);
    let (harness_home, path_env) = subprocess_env(data_dir);

    log::debug!(
        target: LOG_TARGET,
        "spawning {} for Scan (data_dir={}, max_blocks_to_scan={max_blocks_to_scan}, password redacted)",
        binary.display(),
        data_dir.display(),
    );

    let output = Command::new(&binary)
        .args(&argv)
        .env_clear()
        .env("HOME", &harness_home)
        .env("PATH", &path_env)
        .env("TARI_NETWORK", NETWORK_FLAG_VALUE)
        // Force `info` so captured stderr is useful for diagnosing failures;
        // the harness no longer parses any stderr token.
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| {
            format!(
                "spawning {} scan (is the binary on $PATH or set Config::minotari_path?)",
                binary.display(),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        anyhow::bail!("minotari scan exit {:?}; stderr={}", output.status, stderr,);
    }
    log::info!(
        target: LOG_TARGET,
        "minotari scan succeeded (max_blocks_to_scan={max_blocks_to_scan})",
    );
    Ok(ScanStdoutParsed { max_blocks_to_scan })
}

/// Spawn `minotari Balance` and return the parsed microTari u64 total.
///
/// Stdout format is exactly `Balance at height {h}({d}): {total}` per
/// `main.rs::handle_balance` at the pinned commit (lines 680-694). `{total}`
/// is [`MicroMinotari`]'s Display: `"{n} µT"` or `"{n.nnnnnn} T"`.
///
/// [`MicroMinotari`]: tari_transaction_components::tari_amount::MicroMinotari
pub(super) async fn run_balance_subprocess(cfg: &Config, data_dir: &Path) -> anyhow::Result<u64> {
    let harness_toml = write_harness_toml(data_dir)?;
    let database_path = data_dir.join("wallet.sqlite3");
    let argv = build_balance_argv(&harness_toml, &database_path);
    let binary = resolve_binary(cfg);
    let (harness_home, path_env) = subprocess_env(data_dir);

    log::debug!(
        target: LOG_TARGET,
        "spawning {} for Balance (data_dir={})",
        binary.display(),
        data_dir.display(),
    );

    let output = Command::new(&binary)
        .args(&argv)
        .env_clear()
        .env("HOME", &harness_home)
        .env("PATH", &path_env)
        .env("TARI_NETWORK", NETWORK_FLAG_VALUE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| {
            format!(
                "spawning {} balance (is the binary on $PATH or set Config::minotari_path?)",
                binary.display(),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        anyhow::bail!(
            "minotari balance exit {:?}; stderr={}",
            output.status,
            stderr,
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_balance_microtari(&stdout).with_context(|| {
        format!("parsing microTari total from minotari balance stdout (stdout was: {stdout:?})")
    })
}

/// Parse the microTari u64 from `Balance`'s stdout.
///
/// The stdout format is `Balance at height {h}({d}): {total}` where `{total}`
/// is one of:
/// * `"{n} µT"` — `MicroMinotari::Display` when total < 1 T.
/// * `"{n.nnnnnn} T"` — `Minotari::Display` (delegated by MicroMinotari when
///   total ≥ 1 T), 6 decimal places.
///
/// Both forms parse cleanly through the upstream
/// [`MicroMinotari::from_str`] impl in `tari_transaction_components` 5.3.1
/// (see `tari_amount.rs:184-210`): the impl strips whitespace + lowercases,
/// then dispatches to either a lossless `u64` parse (µT branch) or a
/// `Decimal::from_str → Minotari::try_from` chain (T branch, which
/// inherently rejects fractional precision > 6 digits). Per @SWvheerden's
/// 2026-05-29 12:56 review on PR #6: "it should be on the type and from
/// string." Anchor strategy (a) preserved per `analysis/DESIGN_AMENDMENT.md
/// §8.3` — the regex extracts the unit-sentinel substring; from_str handles
/// the rest.
fn parse_balance_microtari(stdout: &str) -> anyhow::Result<u64> {
    static BALANCE_RE: OnceLock<Regex> = OnceLock::new();
    // µT MUST come before bare T in the alternation — otherwise
    // `"500000 µT"` matches the T branch and is parsed as
    // `"500000 T" = 500_000_000_000 µT`.
    let re = BALANCE_RE.get_or_init(|| {
        Regex::new(r"(\d+(?:\.\d+)?\s*(?:µT|T))\b").expect("balance regex compiles")
    });
    let cap = re.captures(stdout).ok_or_else(|| {
        anyhow::anyhow!(
            "no balance amount matched (looked for `{{n}} µT` or `{{n.nnnnnn}} T`); \
             stdout was {stdout:?}",
        )
    })?;
    let amount_str = cap
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("balance regex matched but group 1 missing"))?
        .as_str();
    let amount = MicroMinotari::from_str(amount_str)
        .map_err(|e| anyhow::anyhow!("MicroMinotari::from_str({amount_str:?}): {e}"))?;
    Ok(amount.as_u64())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::gen_seed;

    #[test]
    fn build_create_argv_has_top_level_flags_before_subcommand() {
        let argv = build_create_argv(
            Path::new("/data/harness.toml"),
            Path::new("/data/wallet.sqlite3"),
            "the-password",
            "word1 word2 word3 word4 word5 word6 word7 word8 word9 word10 word11 word12 \
             word13 word14 word15 word16 word17 word18 word19 word20 word21 word22 word23 word24",
        );
        assert_eq!(argv[0], "--config");
        assert_eq!(argv[1], "/data/harness.toml");
        assert_eq!(argv[2], "--network");
        assert_eq!(argv[3], "esmeralda");
        assert_eq!(argv[4], "create");
        assert_eq!(argv[5], "--password");
        assert_eq!(argv[6], "the-password");
        assert_eq!(argv[7], "--database-path");
        assert_eq!(argv[8], "/data/wallet.sqlite3");
        assert_eq!(argv[9], "--account-name");
        assert_eq!(argv[10], "default");
        assert_eq!(argv[11], "--seed-words");
        // The mnemonic must be a SINGLE argv arg (space-separated inside one
        // string) — clap's `Option<String>` declaration consumes the next
        // value as a whole.
        assert!(
            argv[12].split(' ').count() == 24,
            "mnemonic in one argv slot: {:?}",
            argv[12],
        );
        assert_eq!(argv.len(), 13, "argv length: {argv:?}");
    }

    #[test]
    fn rewrite_birthday_round_trips_through_cipher_seed() {
        let original = gen_seed().expect("gen_seed");
        let rewritten = rewrite_birthday(&original, 7_000).expect("rewrite");
        let seed_words = SeedWords::from_str(&rewritten).expect("rewritten parses");
        let cipher =
            <CipherSeed as Mnemonic<CipherSeed>>::from_mnemonic(&seed_words, None).expect("decode");
        assert_eq!(cipher.birthday(), 7_000);
    }

    #[test]
    fn rewrite_birthday_zero_is_genesis() {
        let original = gen_seed().expect("gen_seed");
        let rewritten = rewrite_birthday(&original, 0).expect("rewrite to zero");
        let seed_words = SeedWords::from_str(&rewritten).expect("rewritten parses");
        let cipher =
            <CipherSeed as Mnemonic<CipherSeed>>::from_mnemonic(&seed_words, None).expect("decode");
        assert_eq!(cipher.birthday(), 0);
    }

    #[test]
    fn default_binary_is_minotari() {
        assert_eq!(DEFAULT_BINARY, "minotari");
        assert_ne!(DEFAULT_BINARY, "minotari_console_wallet");
    }

    #[test]
    fn build_scan_argv_has_top_level_flags_before_subcommand() {
        let argv = build_scan_argv(
            Path::new("/data/harness.toml"),
            Path::new("/data/wallet.sqlite3"),
            "pw",
            12345,
        );
        assert_eq!(argv[0], "--config");
        assert_eq!(argv[1], "/data/harness.toml");
        assert_eq!(argv[2], "--network");
        assert_eq!(argv[3], "esmeralda");
        assert_eq!(argv[4], "scan");
        assert_eq!(argv[5], "--password");
        assert_eq!(argv[6], "pw");
        assert_eq!(argv[7], "--database-path");
        assert_eq!(argv[8], "/data/wallet.sqlite3");
        assert_eq!(argv[9], "--account-name");
        assert_eq!(argv[10], "default");
        assert_eq!(argv[11], "--max-blocks-to-scan");
        assert_eq!(argv[12], "12345");
        assert_eq!(argv.len(), 13);
    }

    #[test]
    fn build_balance_argv_has_no_password_flag() {
        // cli.rs lines 247-252: Balance takes only DatabaseArgs + AccountArgs.
        // No SecurityArgs. The argv must NOT contain --password.
        let argv = build_balance_argv(
            Path::new("/data/harness.toml"),
            Path::new("/data/wallet.sqlite3"),
        );
        assert!(
            !argv.iter().any(|s| s == "--password"),
            "Balance subcommand takes no --password at the pinned cli.rs (lines 247-252): {argv:?}",
        );
        assert_eq!(argv[0], "--config");
        assert_eq!(argv[2], "--network");
        assert_eq!(argv[4], "balance");
        assert!(argv.iter().any(|s| s == "--database-path"));
        assert!(argv.iter().any(|s| s == "--account-name"));
    }

    #[test]
    fn parse_balance_microtari_empty_wallet() {
        // Fixture: empty wallet — total < 1 T, MicroMinotari Display = "{n} µT".
        let stdout = include_str!("../../fixtures/minotari_balance_empty.txt");
        assert_eq!(parse_balance_microtari(stdout).expect("parse"), 0);
    }

    #[test]
    fn parse_balance_microtari_partial() {
        // Fixture: partial wallet — total < 1 T, microtari format.
        let stdout = include_str!("../../fixtures/minotari_balance_partial.txt");
        // 500_000 µT.
        assert_eq!(parse_balance_microtari(stdout).expect("parse"), 500_000);
    }

    #[test]
    fn parse_balance_microtari_full() {
        // Fixture: large wallet — total ≥ 1 T, Tari format with 6 decimals.
        let stdout = include_str!("../../fixtures/minotari_balance_full.txt");
        // 10.000000 T = 10_000_000 µT.
        assert_eq!(parse_balance_microtari(stdout).expect("parse"), 10_000_000);
    }

    #[test]
    fn parse_balance_microtari_rejects_garbage() {
        let err = parse_balance_microtari("nothing useful here").expect_err("must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no balance amount matched"),
            "error must name the parsing failure: {msg}",
        );
    }

    #[test]
    fn parse_balance_microtari_integer_tari() {
        // Future-proof: if the formatter ever drops the decimal portion ("5 T"
        // instead of "5.000000 T"), the parser should still work.
        let stdout = "Balance at height 100(2026-05-01T00:00:00): 5 T\n";
        assert_eq!(parse_balance_microtari(stdout).expect("parse"), 5_000_000);
    }

    /// Precision-sensitive edge: `10.000001 T` MUST round-trip to exactly
    /// `10_000_001 µT`, NOT `10_000_000.999...` from f64 mantissa loss.
    /// Per gemini-code-assist review on PR #6.
    #[test]
    fn parse_balance_microtari_no_precision_loss_for_smallest_fractional() {
        let stdout = "Balance at height 100(...): 10.000001 T\n";
        assert_eq!(parse_balance_microtari(stdout).expect("parse"), 10_000_001);
    }

    /// Large-value edge: `10_000_000 T` (10 million XTM) is past f64's
    /// safe integer range when multiplied by 1_000_000 (= 10^13, above
    /// 2^53 ≈ 9.007×10^15 — actually within safe range, but combined
    /// with any fractional portion the f64 path could drift). Integer
    /// parser must be exact.
    #[test]
    fn parse_balance_microtari_no_precision_loss_for_large_amounts() {
        let stdout = "Balance at height 100(...): 10000000 T\n";
        assert_eq!(
            parse_balance_microtari(stdout).expect("parse"),
            10_000_000_000_000,
        );
    }

    #[test]
    fn parse_balance_microtari_half_tari_is_500_000_microtari() {
        let stdout = "Balance at height 100(...): 0.5 T\n";
        assert_eq!(parse_balance_microtari(stdout).expect("parse"), 500_000);
    }

    /// Round-trip: format 100 deterministic-but-varied microtari values as
    /// either µT or T, parse back, assert exact equality. No f64
    /// involved in the parser path.
    #[test]
    fn parse_balance_microtari_round_trip_exact() {
        // Mix of small (<1T), medium (1T-100T), and large (≥1MT) amounts.
        let cases: Vec<u64> = (0..100)
            .map(|i| {
                // Pseudo-random spread without rand dep: combine i with
                // bit-twiddling so we hit a variety of bit patterns.
                let base = (i as u64).wrapping_mul(0x9E3779B97F4A7C15);
                base % 12_000_000_000_000 // cap at 12 MT
            })
            .collect();
        for &microtari in &cases {
            let formatted = if microtari < 1_000_000 {
                format!("{} µT", microtari)
            } else {
                // Format as "{int}.{frac:06} T", trim trailing zeros if any.
                let int_part = microtari / 1_000_000;
                let frac_part = microtari % 1_000_000;
                if frac_part == 0 {
                    format!("{} T", int_part)
                } else {
                    format!("{}.{:06} T", int_part, frac_part)
                }
            };
            let stdout = format!("Balance at height 1(2026-05-01): {formatted}\n");
            let parsed = parse_balance_microtari(&stdout)
                .unwrap_or_else(|e| panic!("parse {formatted:?}: {e:#}"));
            assert_eq!(parsed, microtari, "round-trip failed for {formatted:?}");
        }
    }

    #[test]
    fn parse_balance_microtari_rejects_too_much_precision() {
        // 7 fractional digits → reject (below 1 µT resolution).
        let stdout = "Balance at height 100(...): 10.0000001 T\n";
        assert!(parse_balance_microtari(stdout).is_err());
    }
}
