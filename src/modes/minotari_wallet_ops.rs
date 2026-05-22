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
//! Additional subcommand wiring (Scan, Balance) lands in subsequent commits.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
};

use anyhow::Context;
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
}
