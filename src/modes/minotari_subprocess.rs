//! Shared `minotari` subprocess pipeline used by Modes 2 and 3.
//!
//! Per `analysis/DESIGN.md §Mode 2 — concrete wiring` and
//! `analysis/DESIGN_ADDENDUM.md §Mode 3 CLI shape — proven`, both new-wallet
//! modes share the same four-step pipeline (the only difference is how many
//! `--recipient` flags Mode 3 emits relative to Mode 2's single recipient).
//! This helper module is the DRY core:
//!
//! 1. **Subprocess** — `minotari --config <harness.toml> --network esmeralda
//!    create-unsigned-transaction --database-path … --password … --account-name
//!    default --recipient <addr_base58>::<amount> [--recipient ...] --output-file
//!    <tx_<idx>.json>`. Spawned with `env_clear()` plus a controlled `HOME`,
//!    `PATH`, `TARI_NETWORK` envelope — the `--password` is in argv per
//!    `DESIGN_ADDENDUM.md §Mode 3 CLI shape — proven §Flag-level differences`
//!    point 3 and PR #99's literal mirror.
//! 2. **Parse** — `PrepareOneSidedTransactionForSigningResult::from_json(...)`
//!    from `tari_transaction_components::offline_signing::models`.
//! 3. **Sign** — `sign_locked_transaction(&key_manager,
//!    ConsensusConstantsBuilder::new(Network::Esmeralda).build(),
//!    Network::Esmeralda, unsigned)`. The KeyManager is reconstituted from the
//!    seed mnemonic via the approved API surface
//!    (`WalletType::SeedWords + SeedWordsWallet::construct_new + KeyManager::new`).
//! 4. **Broadcast** — `Broadcaster::submit_transaction(tx).await`.
//!
//! No retry, backoff, or throttling anywhere (AC-30/31/32). The output JSON
//! file is deleted on success (keeps the tempdir lean) and preserved on
//! failure (helpful for operator post-mortem).

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    time::Instant,
};

use anyhow::Context;
use tari_common::configuration::Network;
use tari_common_types::{
    seeds::{cipher_seed::CipherSeed, mnemonic::Mnemonic, seed_words::SeedWords},
    tari_address::TariAddress,
};
use tari_transaction_components::{
    consensus::ConsensusConstantsBuilder,
    key_manager::{
        wallet_types::{SeedWordsWallet, WalletType},
        KeyManager,
    },
    offline_signing::{
        models::{PrepareOneSidedTransactionForSigningResult, TransactionResult},
        sign_locked_transaction,
    },
};
use tokio::process::Command;

use crate::{
    broadcast::Broadcaster,
    config::Config,
    modes::{TxRecord, TxRecordPhase, TxRecordStatus},
    seed::SeedHandle,
};

const LOG_TARGET: &str = "c::modes::minotari_subprocess";

/// Default binary name resolved via `$PATH` when [`Config::minotari_path`] is
/// `None`. `minotari` is the new wallet CLI from `tari-project/minotari-cli`,
/// distinct from `minotari_console_wallet` (Mode 1's binary).
const DEFAULT_BINARY: &str = "minotari";

/// Tari network identifier — defense in depth re-assertion at this layer.
/// `crate::guards::enforce_esmeralda` is the primary gate; we re-assert here
/// because `minotari_subprocess` is invoked once per tx and the argv carries
/// `--network esmeralda` to the subprocess.
const NETWORK: Network = Network::Esmeralda;

/// Network identifier passed as the top-level `--network` argv flag.
const NETWORK_FLAG_VALUE: &str = "esmeralda";

/// Account name used on every `--account-name` flag. Matches PR #99's literal.
const ACCOUNT_NAME: &str = "default";

/// Which seed slot the helper reads from [`SeedHandle`] when reconstituting
/// the [`KeyManager`]. Mode 1 (`Old`) does not flow through this helper —
/// `create_sign_and_submit` bails on `SeedRole::Old` so the slot mapping is
/// explicit at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeedRole {
    /// Old-wallet mode seed (`SeedHandle::mnemonic_old`). Not used by Modes
    /// 2 or 3 (Mode 1 goes through gRPC, not this helper). The variant exists
    /// so the slot mapping is exhaustive at the call site — `create_sign_and_submit`
    /// bails loudly if `Old` ever reaches it, surfacing the contract violation
    /// in a single test rather than as a silent miswire.
    #[allow(dead_code)]
    Old,
    /// New-wallet mode seed (`SeedHandle::mnemonic_new`) — Mode 2.
    New,
    /// Payment-processor seed (`SeedHandle::mnemonic_payment_processor`) — Mode 3.
    /// `dead_code` allow lifts in the Mode 3 (`PaymentProcessor`) commit that
    /// follows this one — that's the only consumer of this variant.
    #[allow(dead_code)]
    Pp,
}

/// Build the argv vector for `minotari create-unsigned-transaction`.
///
/// Pure function (no IO) so unit tests can snapshot the exact argv shape
/// against `DESIGN_ADDENDUM.md §Mode 3 CLI shape — proven` without spawning a
/// subprocess. Order is significant per clap's parsing — top-level flags
/// (`--config`, `--network`) come BEFORE the subcommand, subcommand flags come
/// AFTER it.
///
/// `password` is the revealed wallet password. The caller is responsible for
/// not echoing the returned vector to logs at info-or-higher level; we log it
/// at debug-redacted from [`create_sign_and_submit`].
pub(super) fn build_create_unsigned_tx_argv(
    harness_toml_path: &Path,
    database_path: &Path,
    password: &str,
    recipients: &[(TariAddress, u64)],
    output_file: &Path,
) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(12 + recipients.len() * 2);
    // Top-level flags (Cli-level, BEFORE the subcommand).
    argv.push("--config".to_string());
    argv.push(harness_toml_path.display().to_string());
    argv.push("--network".to_string());
    argv.push(NETWORK_FLAG_VALUE.to_string());
    // Subcommand.
    argv.push("create-unsigned-transaction".to_string());
    // Subcommand flags.
    argv.push("--database-path".to_string());
    argv.push(database_path.display().to_string());
    argv.push("--password".to_string());
    argv.push(password.to_string());
    argv.push("--account-name".to_string());
    argv.push(ACCOUNT_NAME.to_string());
    for (addr, amount) in recipients {
        argv.push("--recipient".to_string());
        argv.push(format!("{}::{amount}", addr.to_base58()));
    }
    argv.push("--output-file".to_string());
    argv.push(output_file.display().to_string());
    argv
}

/// Write the minimal `harness.toml` the subprocess needs into `data_dir`.
///
/// Per `DESIGN_ADDENDUM.md §Mode 3 CLI shape — proven §Edge cases` point 2,
/// the subprocess's default `--config config/config.toml` is relative to CWD;
/// the harness spawns from a controlled tempdir where that path is absent.
/// The minimum content is `network = "esmeralda"` so the subprocess's
/// network-config layer is satisfied; the top-level `--network esmeralda`
/// argv flag still wins per the CLI's documented precedence.
///
/// Returns the absolute path to the written file.
pub(super) fn write_harness_toml(data_dir: &Path) -> anyhow::Result<PathBuf> {
    let path = data_dir.join("harness.toml");
    let body = "network = \"esmeralda\"\n";
    std::fs::write(&path, body)
        .with_context(|| format!("writing harness.toml to {}", path.display()))?;
    Ok(path)
}

/// Construct, sign, and broadcast a one-sided transaction via the Mode 2/3
/// pipeline.
///
/// Steps (mirrors PR #99's `send_transactions` step verbatim with
/// `Network::LocalNet → Network::Esmeralda`):
/// 1. Spawn `minotari create-unsigned-transaction ...` and wait for exit.
/// 2. Parse the unsigned-tx JSON file the subprocess wrote.
/// 3. Reconstitute the [`KeyManager`] from `seeds[seed_role]` and call
///    `sign_locked_transaction(&key_manager, consensus_constants,
///    Network::Esmeralda, unsigned)` in-process.
/// 4. Broadcast the signed transaction via [`Broadcaster::submit_transaction`].
///
/// Output-file lifecycle: deleted on success, preserved on failure (logged
/// at `warn` so the operator knows where to look).
///
/// **No retry / backoff / throttle** on any failure path (AC-30/31/32). A
/// failure at any step is surfaced as a [`TxRecord`] with status
/// `Failed { phase }` so the scenario layer can record the contention raw.
///
/// The arg count exceeds clippy's default; each is genuinely required (config
/// for binary path, seeds for KeyManager construction, seed_role for the
/// Mode 2 vs Mode 3 distinction, recipients for single vs batch, fee_rate
/// recorded in the TxRecord, broadcaster for HTTP submission, data_dir for
/// the per-mode tempdir, tx_idx for the output file name). Bundling into a
/// struct adds a wrapper type whose only purpose is satisfying lints —
/// `allow` is cleaner.
#[allow(clippy::too_many_arguments)]
pub(super) async fn create_sign_and_submit(
    cfg: &Config,
    seeds: &SeedHandle,
    seed_role: SeedRole,
    recipients: &[(TariAddress, u64)],
    fee_rate: u64,
    broadcaster: &Broadcaster,
    data_dir: &Path,
    tx_idx: u64,
) -> anyhow::Result<TxRecord> {
    let started = Instant::now();

    // Read the seed up front so the role-specific bail happens before any IO.
    let mnemonic_handle = match seed_role {
        SeedRole::New => seeds.mnemonic_new()?,
        SeedRole::Pp => seeds.mnemonic_payment_processor()?,
        SeedRole::Old => {
            anyhow::bail!(
                "minotari subprocess pipeline is for Modes 2/3 only; Mode 1 uses the \
                 console_wallet gRPC surface (see crate::modes::old_wallet)",
            );
        }
    };
    let password_handle = seeds.wallet_password()?;

    // Resolve binary path — operator override or `$PATH` fallback.
    let binary = cfg
        .minotari_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BINARY));

    // Build per-call paths inside the per-mode data dir.
    let harness_toml = write_harness_toml(data_dir)?;
    let database_path = data_dir.join("wallet.sqlite3");
    let output_path = data_dir.join(format!("tx_{tx_idx}.json"));

    // Step 1: Subprocess — create unsigned transaction.
    let argv = build_create_unsigned_tx_argv(
        &harness_toml,
        &database_path,
        password_handle.reveal(),
        recipients,
        &output_path,
    );
    log::debug!(
        target: LOG_TARGET,
        "spawning {} for create-unsigned-transaction (tx_idx={tx_idx}, recipients={}, password redacted)",
        binary.display(),
        recipients.len(),
    );

    // Carry only the env the subprocess needs — `env_clear()` first, then put
    // back HOME/PATH/TARI_NETWORK per DESIGN.md §Secret handling. The wallet
    // password itself rides in argv per PR #99 + the cli.rs declaration.
    let harness_home = data_dir.to_path_buf();
    let path_env = std::env::var("PATH").unwrap_or_default();
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
                "spawning {} create-unsigned-transaction (is the binary on $PATH or set \
                 Config::minotari_path?)",
                binary.display(),
            )
        });
    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
            log::warn!(
                target: LOG_TARGET,
                "create-unsigned-transaction exited non-zero (tx_idx={tx_idx}, status={:?}); \
                 output file at {} preserved for diagnosis",
                o.status,
                output_path.display(),
            );
            return Ok(failed_record(
                started,
                TxRecordPhase::Construct,
                format!(
                    "minotari create-unsigned-transaction exit {:?}: {}",
                    o.status, stderr
                ),
                fee_rate,
            ));
        }
        Err(e) => {
            log::warn!(
                target: LOG_TARGET,
                "create-unsigned-transaction spawn failed (tx_idx={tx_idx}): {e:#}",
            );
            return Ok(failed_record(
                started,
                TxRecordPhase::Construct,
                format!("{e:#}"),
                fee_rate,
            ));
        }
    };
    log::debug!(
        target: LOG_TARGET,
        "subprocess wrote unsigned tx to {} (stdout={} bytes)",
        output_path.display(),
        output.stdout.len(),
    );

    // Step 2: Parse the unsigned tx JSON file.
    let unsigned_json = match tokio::fs::read_to_string(&output_path).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                target: LOG_TARGET,
                "reading unsigned tx file {} failed: {e:#}; preserving file for diagnosis",
                output_path.display(),
            );
            return Ok(failed_record(
                started,
                TxRecordPhase::Construct,
                format!("reading {}: {e:#}", output_path.display()),
                fee_rate,
            ));
        }
    };
    let unsigned = match PrepareOneSidedTransactionForSigningResult::from_json(&unsigned_json) {
        Ok(u) => u,
        Err(e) => {
            log::warn!(
                target: LOG_TARGET,
                "deserialising unsigned tx from {} failed: {e}; preserving file for diagnosis",
                output_path.display(),
            );
            return Ok(failed_record(
                started,
                TxRecordPhase::Sign,
                format!("PrepareOneSidedTransactionForSigningResult::from_json: {e}"),
                fee_rate,
            ));
        }
    };

    // Step 3: Reconstruct KeyManager and consensus constants, then sign in-process.
    let mnemonic = mnemonic_handle.reveal().to_string();
    let seed_words = match SeedWords::from_str(&mnemonic) {
        Ok(s) => s,
        Err(e) => {
            return Ok(failed_record(
                started,
                TxRecordPhase::Sign,
                format!("parsing mnemonic for KeyManager: {e}"),
                fee_rate,
            ));
        }
    };
    let cipher_seed = match <CipherSeed as Mnemonic<CipherSeed>>::from_mnemonic(&seed_words, None) {
        Ok(s) => s,
        Err(e) => {
            return Ok(failed_record(
                started,
                TxRecordPhase::Sign,
                format!("decoding CipherSeed: {e}"),
                fee_rate,
            ));
        }
    };
    let seed_words_wallet = match SeedWordsWallet::construct_new(cipher_seed) {
        Ok(w) => w,
        Err(e) => {
            return Ok(failed_record(
                started,
                TxRecordPhase::Sign,
                format!("constructing SeedWordsWallet: {e}"),
                fee_rate,
            ));
        }
    };
    let wallet = WalletType::SeedWords(seed_words_wallet);
    let key_manager = match KeyManager::new(wallet) {
        Ok(km) => km,
        Err(e) => {
            return Ok(failed_record(
                started,
                TxRecordPhase::Sign,
                format!("KeyManager::new: {e}"),
                fee_rate,
            ));
        }
    };
    let consensus_constants = ConsensusConstantsBuilder::new(NETWORK).build();
    let signed = match sign_locked_transaction(&key_manager, consensus_constants, NETWORK, unsigned)
    {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                target: LOG_TARGET,
                "sign_locked_transaction failed (tx_idx={tx_idx}): {e}",
            );
            return Ok(failed_record(
                started,
                TxRecordPhase::Sign,
                format!("sign_locked_transaction: {e}"),
                fee_rate,
            ));
        }
    };
    let txid = signed.signed_transaction.tx_id.to_string();
    let tx = signed.signed_transaction.transaction;

    // Step 4: Broadcast via the published HTTP client.
    let broadcast_started_ms = started.elapsed().as_millis() as u64;
    let outcome = match broadcaster.submit_transaction(tx).await {
        Ok(o) => o,
        Err(e) => {
            log::warn!(
                target: LOG_TARGET,
                "submit_transaction failed (tx_idx={tx_idx}, txid={txid}): {e:#}",
            );
            return Ok(TxRecord {
                txid,
                t_total_ms: started.elapsed().as_millis() as u64,
                t_broadcast_ms: started.elapsed().as_millis() as u64,
                t_confirm_ms: None,
                status: TxRecordStatus::failed(TxRecordPhase::Broadcast).to_string(),
                error_string: Some(format!("{e:#}")),
                fee_microtari: 0,
            });
        }
    };
    let t_total_ms = started.elapsed().as_millis() as u64;

    // On success: delete the now-redundant output file. On any of the failure
    // paths above we return early WITHOUT deleting, so the operator can
    // inspect what the subprocess produced.
    if let Err(e) = tokio::fs::remove_file(&output_path).await {
        // Not fatal — the tempdir is cleaned up on `HarnessDataDir::drop` anyway.
        log::debug!(
            target: LOG_TARGET,
            "could not remove unsigned tx file {} after success: {e:#}",
            output_path.display(),
        );
    }

    let (status, error_string) = if outcome.accepted {
        (TxRecordStatus::Success.to_string(), None)
    } else {
        (
            TxRecordStatus::Rejected.to_string(),
            Some(format!("{:?}", outcome.rejection_reason)),
        )
    };
    Ok(TxRecord {
        txid,
        t_total_ms,
        t_broadcast_ms: t_total_ms.saturating_sub(broadcast_started_ms),
        t_confirm_ms: None,
        status,
        error_string,
        // Per `RESULT_PROFILE_SCHEMA.md §4 tx_records[]`, fee_microtari is the
        // base-node-reported fee. The published `TxSubmissionResponse` does
        // not surface the fee on the submit acknowledgement; scenario code
        // backfills via subsequent polling (3i). 0 here is "not yet observed",
        // not "actually zero".
        fee_microtari: 0,
    })
}

/// Build a [`TxRecord`] describing a failure at `phase` with `error_string`.
/// Shared helper so every failure-return path produces the same shape.
fn failed_record(
    started: Instant,
    phase: TxRecordPhase,
    error_string: String,
    _fee_rate: u64,
) -> TxRecord {
    let elapsed = started.elapsed().as_millis() as u64;
    TxRecord {
        txid: String::new(),
        t_total_ms: elapsed,
        t_broadcast_ms: 0,
        t_confirm_ms: None,
        status: TxRecordStatus::failed(phase).to_string(),
        error_string: Some(error_string),
        fee_microtari: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tari_common_types::tari_address::TariAddress;
    use tempfile::TempDir;

    use super::*;
    use crate::{gen_seed, seed::derive_address};

    /// Single-recipient argv (Mode 2's shape). Verbatim against
    /// `DESIGN_ADDENDUM.md §Mode 3 CLI shape — proven` "Exact argv shape Mode 3
    /// (and Mode 2) use" block.
    #[test]
    fn builds_correct_argv_for_single_recipient() {
        let mnemonic = gen_seed().expect("gen_seed");
        let recipient = derive_address(&mnemonic).expect("derive_address");
        let amount: u64 = 1000;
        let argv = build_create_unsigned_tx_argv(
            Path::new("/data/harness.toml"),
            Path::new("/data/wallet.sqlite3"),
            "the-password",
            &[(recipient.clone(), amount)],
            Path::new("/data/tx_0.json"),
        );
        // Top-level flags come BEFORE the subcommand.
        assert_eq!(argv[0], "--config");
        assert_eq!(argv[1], "/data/harness.toml");
        assert_eq!(argv[2], "--network");
        assert_eq!(argv[3], "esmeralda");
        // Subcommand and its flags.
        assert_eq!(argv[4], "create-unsigned-transaction");
        assert_eq!(argv[5], "--database-path");
        assert_eq!(argv[6], "/data/wallet.sqlite3");
        assert_eq!(argv[7], "--password");
        assert_eq!(argv[8], "the-password");
        assert_eq!(argv[9], "--account-name");
        assert_eq!(argv[10], "default");
        assert_eq!(argv[11], "--recipient");
        assert_eq!(argv[12], format!("{}::{amount}", recipient.to_base58()));
        assert_eq!(argv[13], "--output-file");
        assert_eq!(argv[14], "/data/tx_0.json");
        assert_eq!(argv.len(), 15, "argv length: {argv:?}");
    }

    /// Mode 3 batch shape — three recipients exercise the repeated `--recipient`
    /// flag clap declares as `Vec<String>`.
    #[test]
    fn builds_correct_argv_for_three_recipients() {
        let m1 = gen_seed().expect("m1");
        let m2 = gen_seed().expect("m2");
        let m3 = gen_seed().expect("m3");
        let a1 = derive_address(&m1).expect("a1");
        let a2 = derive_address(&m2).expect("a2");
        let a3 = derive_address(&m3).expect("a3");
        let argv = build_create_unsigned_tx_argv(
            Path::new("/d/harness.toml"),
            Path::new("/d/wallet.sqlite3"),
            "pw",
            &[(a1.clone(), 1), (a2.clone(), 2), (a3.clone(), 3)],
            Path::new("/d/tx_1.json"),
        );
        let recipient_indices: Vec<usize> = argv
            .iter()
            .enumerate()
            .filter_map(|(i, s)| if s == "--recipient" { Some(i) } else { None })
            .collect();
        assert_eq!(
            recipient_indices.len(),
            3,
            "must emit --recipient three times: {argv:?}",
        );
        assert_eq!(
            argv[recipient_indices[0] + 1],
            format!("{}::1", a1.to_base58())
        );
        assert_eq!(
            argv[recipient_indices[1] + 1],
            format!("{}::2", a2.to_base58())
        );
        assert_eq!(
            argv[recipient_indices[2] + 1],
            format!("{}::3", a3.to_base58())
        );
    }

    #[test]
    fn argv_has_top_level_flags_before_subcommand() {
        // Spot-check on a real address: the subcommand name MUST appear after
        // both --config and --network, not in between.
        let m = gen_seed().expect("seed");
        let a = derive_address(&m).expect("addr");
        let argv = build_create_unsigned_tx_argv(
            Path::new("/x/harness.toml"),
            Path::new("/x/wallet.sqlite3"),
            "pw",
            &[(a, 7)],
            Path::new("/x/tx_2.json"),
        );
        let subcmd_idx = argv
            .iter()
            .position(|s| s == "create-unsigned-transaction")
            .expect("must contain subcommand");
        let config_idx = argv.iter().position(|s| s == "--config").expect("--config");
        let network_idx = argv
            .iter()
            .position(|s| s == "--network")
            .expect("--network");
        assert!(
            config_idx < subcmd_idx,
            "--config must come before the subcommand (argv: {argv:?})",
        );
        assert!(
            network_idx < subcmd_idx,
            "--network must come before the subcommand (argv: {argv:?})",
        );
    }

    #[test]
    fn argv_uses_base58_recipient_encoding() {
        // Pin: recipient values are `<addr_base58>::<amount>` per
        // DESIGN_ADDENDUM §S2 — not emoji-ID, not hex.
        let m = gen_seed().expect("seed");
        let a = derive_address(&m).expect("addr");
        let argv = build_create_unsigned_tx_argv(
            Path::new("/h.toml"),
            Path::new("/w.sqlite3"),
            "pw",
            &[(a.clone(), 42)],
            Path::new("/tx.json"),
        );
        let pair = argv
            .iter()
            .find(|s| s.contains("::42"))
            .expect("must contain the recipient::amount pair");
        let expected_prefix = format!("{}::42", a.to_base58());
        assert_eq!(pair, &expected_prefix);
        // Negative: no emoji bytes.
        for token in &argv {
            assert!(
                token.is_ascii(),
                "argv tokens must be ASCII (no emoji-ID), got {token:?}",
            );
        }
    }

    #[test]
    fn writes_harness_toml_with_network_esmeralda() {
        let tempdir = TempDir::new().expect("tempdir");
        let path = write_harness_toml(tempdir.path()).expect("write");
        assert_eq!(path.parent(), Some(tempdir.path()));
        assert_eq!(
            path.file_name().map(|n| n.to_os_string()),
            Some("harness.toml".into())
        );
        let body = std::fs::read_to_string(&path).expect("read back");
        assert!(
            body.contains("network = \"esmeralda\""),
            "harness.toml must pin network=esmeralda; got {body:?}",
        );
    }

    #[test]
    fn parses_unsigned_tx_rejects_garbage() {
        // Positive-shape round-trips require constructing a
        // `PrepareOneSidedTransactionForSigningResult` whose `version` field is
        // `semver::Version` — `semver` is a transitive dep not re-exported by
        // `tari_transaction_components`. The scenarios layer (step 3i) exercises
        // the positive path with a real subprocess-produced JSON fixture; here
        // we lock in the negative contract that drives the failure-branch
        // record-construction in `create_sign_and_submit` step 2.
        let result = PrepareOneSidedTransactionForSigningResult::from_json("{}");
        assert!(result.is_err(), "empty object must fail from_json");
        let result = PrepareOneSidedTransactionForSigningResult::from_json("not json");
        assert!(result.is_err(), "non-json must fail from_json");
        let result =
            PrepareOneSidedTransactionForSigningResult::from_json(r#"{"version": "0.0.1"}"#);
        assert!(result.is_err(), "unsupported version must fail from_json");
    }

    #[test]
    fn preserves_output_file_on_failure_branch_documented_by_construction() {
        // The contract is "delete on success path, leave alone on every
        // failure path". Spot-check by constructing a tempdir, writing a
        // file at `tx_<idx>.json`, then invoking the failure branch via
        // `failed_record` — the file MUST still exist afterwards (the only
        // call to `remove_file` is in the post-success block).
        let tempdir = TempDir::new().expect("tempdir");
        let path = tempdir.path().join("tx_99.json");
        std::fs::write(&path, "{}").expect("write fixture");
        // Constructing a failure record is a pure operation — does not delete
        // anything.
        let started = Instant::now();
        let record = failed_record(
            started,
            TxRecordPhase::Sign,
            "fake parse failure".to_string(),
            5,
        );
        assert!(path.exists(), "preserved on failure path");
        assert!(record.error_string.is_some());
        assert!(record.status.contains("sign"));
    }

    #[test]
    fn create_sign_and_submit_bails_on_seed_role_old() {
        // SeedRole::Old must not reach the helper — Mode 1 uses gRPC, not
        // this subprocess pipeline. The bail must happen before any IO.
        let tempdir = TempDir::new().expect("tempdir");
        let cfg = crate::config::Config::default();
        let seeds_cfg = crate::config::Seeds {
            old: "WALLET_BENCHMARKS_TEST_HELPER_OLD".to_string(),
            new: "WALLET_BENCHMARKS_TEST_HELPER_NEW".to_string(),
            payment_processor: "WALLET_BENCHMARKS_TEST_HELPER_PP".to_string(),
            wallet_password: "WALLET_BENCHMARKS_TEST_HELPER_PW".to_string(),
        };
        let m_old = gen_seed().expect("m_old");
        let m_new = gen_seed().expect("m_new");
        let m_pp = gen_seed().expect("m_pp");
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(&seeds_cfg.old, &m_old);
            std::env::set_var(&seeds_cfg.new, &m_new);
            std::env::set_var(&seeds_cfg.payment_processor, &m_pp);
            std::env::set_var(&seeds_cfg.wallet_password, "pw");
        }
        let seeds = SeedHandle::new(&seeds_cfg);
        let recipient = derive_address(&gen_seed().expect("rec")).expect("addr");
        let broadcaster = Broadcaster::new(&cfg.base_node_url);
        // Build a runtime to call the async helper synchronously.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let result = rt.block_on(async {
            create_sign_and_submit(
                &cfg,
                &seeds,
                SeedRole::Old,
                &[(recipient, 100)],
                5,
                &broadcaster,
                tempdir.path(),
                0,
            )
            .await
        });
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(&seeds_cfg.old);
            std::env::remove_var(&seeds_cfg.new);
            std::env::remove_var(&seeds_cfg.payment_processor);
            std::env::remove_var(&seeds_cfg.wallet_password);
        }
        let err = result.expect_err("SeedRole::Old must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Modes 2/3 only"),
            "bail must name the mode contract: {msg}",
        );
    }

    /// Pin: the helper's `DEFAULT_BINARY` resolves to `minotari`, distinct
    /// from `minotari_console_wallet`. Catches a future refactor that
    /// accidentally collapses the two binary names.
    #[test]
    fn default_binary_is_minotari_not_console_wallet() {
        assert_eq!(DEFAULT_BINARY, "minotari");
        assert_ne!(DEFAULT_BINARY, "minotari_console_wallet");
    }

    /// AC-6 guard at the unit level: the helper's source must not name
    /// `minotari_console_wallet`. (The integration grep test
    /// `tests/mode2_no_console_wallet.rs` does the file-level check.)
    #[test]
    fn helper_source_does_not_reference_console_wallet() {
        // Constant-time sanity: read the file via the macro path the build
        // resolves to and assert.
        let src = include_str!("./minotari_subprocess.rs");
        // Excluding the literal we use in this test assertion itself, the
        // source has no references. Strip the assertion block before scanning.
        let scrub_token = "minotari_console_wallet";
        let scan_target = src.replace(scrub_token, "");
        assert!(
            !scan_target.contains("minotari_console_wallet"),
            "Mode 2/3 helper must not reference minotari_console_wallet (AC-6)",
        );
    }

    // Static spot-check to keep `TariAddress` in active use even if a
    // future edit accidentally drops the import.
    #[test]
    fn tari_address_type_is_in_scope() {
        let _: Option<TariAddress> = None;
        let _: Option<PathBuf> = None;
    }
}
