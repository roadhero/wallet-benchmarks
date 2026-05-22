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
//!   Emits the result as `info!(event_count = events.len(); "Scan complete")` —
//!   `log` crate, lands on stderr under env_logger. There is no
//!   machine-readable `--output-format json` flag (DESIGN_AMENDMENT.md §8.1).
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

/// Result of [`run_scan_subprocess`] — the parsed shape of `minotari Scan`'s
/// observable output. `outputs_found` comes from the stderr `event_count=N`
/// emission (log-crate's key-value formatting); `blocks_scanned` is taken from
/// `max_blocks_to_scan` because the CLI does not emit the actual processed
/// count separately (DESIGN_AMENDMENT.md §8 / API_DRIFT.md Step 3i).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScanStdoutParsed {
    /// Wallet outputs discovered by the scan. Parsed from
    /// `info!(event_count = events.len(); "Scan complete")` on the subprocess's
    /// stderr. `None` when the line was absent (subprocess exited successfully
    /// but emitted no `event_count` token — e.g. log level filtered it out).
    pub outputs_found: Option<u64>,
    /// Upper bound on the number of blocks scanned this invocation. The CLI
    /// does not emit a precise post-hoc count; the harness records what it
    /// asked for so scenarios can compare against base-node tip deltas.
    #[allow(dead_code)]
    pub max_blocks_to_scan: u64,
}

/// Spawn `minotari Scan` and capture its parsed result.
///
/// `RUST_LOG=info` is forced into the subprocess environment so the
/// `info!(event_count = ...)` line is emitted at all. The parser is anchored
/// on `event_count=N` (regex `event_count=(\d+)`); if the subprocess exits
/// successfully but the line is absent, `outputs_found` is `None` and the
/// caller decides whether that is a soft signal or a failure.
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
        // The CLI emits its scan-complete summary via the `log` crate at
        // `info`. Without RUST_LOG=info the env_logger default ("error") drops
        // the only token the harness can parse — force it explicitly.
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    let outputs_found = parse_event_count(&stderr);
    log::info!(
        target: LOG_TARGET,
        "minotari scan succeeded (outputs_found={outputs_found:?}, max_blocks_to_scan={max_blocks_to_scan})",
    );
    Ok(ScanStdoutParsed {
        outputs_found,
        max_blocks_to_scan,
    })
}

/// Parse the `event_count=N` token from `Scan`'s stderr.
///
/// Anchor is structural (the literal key-value name emitted by the `log`
/// crate's `info!(event_count = events.len(); ...)` macro form). Returns
/// `None` if the token is absent (e.g. log level filtered it out).
fn parse_event_count(stderr: &str) -> Option<u64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"event_count=(\d+)").expect("event_count regex compiles"));
    let cap = re.captures(stderr)?;
    cap.get(1).and_then(|m| m.as_str().parse::<u64>().ok())
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
/// The parser tries µT first (lossless u64), then falls back to T (parse
/// decimal, multiply by 1_000_000). Anchor strategy (a) per
/// `analysis/DESIGN_AMENDMENT.md §8.3` — robust to label renames as long as
/// the unit sentinel survives.
fn parse_balance_microtari(stdout: &str) -> anyhow::Result<u64> {
    static MICROTARI_RE: OnceLock<Regex> = OnceLock::new();
    static TARI_RE: OnceLock<Regex> = OnceLock::new();
    let micro_re = MICROTARI_RE.get_or_init(|| {
        // Match `{n} µT` — the unit sentinel for MicroMinotari < 1 T.
        Regex::new(r"(\d+)\s*µT").expect("microtari regex compiles")
    });
    let tari_re = TARI_RE.get_or_init(|| {
        // Match `{n.nnnnnn} T` — the unit sentinel for Minotari ≥ 1 T.
        // The decimal portion is optional so future formatter precision changes
        // (e.g. zero precision printing `12 T`) still parse.
        Regex::new(r"(\d+(?:\.\d+)?)\s*T(?:\b|$)").expect("tari regex compiles")
    });
    if let Some(cap) = micro_re.captures(stdout) {
        if let Some(m) = cap.get(1) {
            return m
                .as_str()
                .parse::<u64>()
                .with_context(|| format!("parsing µT number from {:?}", m.as_str()));
        }
    }
    if let Some(cap) = tari_re.captures(stdout) {
        if let Some(m) = cap.get(1) {
            let tari: f64 = m
                .as_str()
                .parse::<f64>()
                .with_context(|| format!("parsing T decimal from {:?}", m.as_str()))?;
            let microtari = (tari * 1_000_000.0).round() as u64;
            return Ok(microtari);
        }
    }
    anyhow::bail!(
        "no balance amount matched (looked for `{{n}} µT` and `{{n.nnnnnn}} T`); \
         stdout was {stdout:?}",
    )
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
    fn parse_event_count_extracts_number() {
        let stderr = "[2026-05-22T12:00:00 INFO minotari] event_count=42 Scan complete\n";
        assert_eq!(parse_event_count(stderr), Some(42));
    }

    #[test]
    fn parse_event_count_returns_none_when_absent() {
        assert_eq!(parse_event_count("Scan complete\n"), None);
        assert_eq!(parse_event_count(""), None);
    }

    #[test]
    fn parse_event_count_extracts_zero() {
        let stderr = "info: event_count=0 Scan complete";
        assert_eq!(parse_event_count(stderr), Some(0));
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
}
