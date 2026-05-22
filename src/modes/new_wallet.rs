//! Mode 2 — `new_wallet` (the new `minotari` CLI from `tari-project/minotari-cli`).
//!
//! Per `analysis/DESIGN.md §Mode 2 — concrete wiring` and
//! `analysis/DESIGN_ADDENDUM.md §Mode 3 CLI shape — proven`, Mode 2 builds and
//! broadcasts transactions through the shared
//! [`crate::modes::minotari_subprocess::create_sign_and_submit`] pipeline:
//!
//! 1. Subprocess `minotari create-unsigned-transaction ...`.
//! 2. Parse the unsigned-tx JSON.
//! 3. Sign in-process via `sign_locked_transaction`.
//! 4. Broadcast via [`crate::broadcast::Broadcaster`].
//!
//! Mode 2 does **NOT** spawn `minotari_console_wallet` — that's Mode 1's
//! binary. AC-6 polices this; `tests/mode2_no_console_wallet.rs` is the
//! file-level grep check, and the unit test
//! [`crate::modes::minotari_subprocess::tests::helper_source_does_not_reference_console_wallet`]
//! is the per-helper guard.
//!
//! Submit-side retry is forbidden by AC-30/31/32. Concurrent dispatch in S4
//! happens at the scenario level via `tokio::JoinSet`; each task calls
//! `send_single` once and the helper makes one subprocess + one HTTP submit
//! per call — no internal loops, semaphores, or sleeps.
//!
//! **Subprocess-backed read/scan flows (`scan_from_birthday`, `get_balance`,
//! `get_utxo_count`, `wipe_and_reimport`)**: per `analysis/DESIGN_AMENDMENT.md`
//! §8 (filed in Step 3g), the new `minotari` CLI surface at the pinned commit
//! has no machine-parseable `get-balance` / `list-utxos` subcommand and uses
//! `Create --seed-words` (not `import-seed`) for restoration. These flows
//! are wired in step 3i once the scenarios layer knows the stdout-parsing
//! contract; today they bail with a structured pointer at the amendment. The
//! send_* paths above are fully implemented and are the AC-critical surface
//! for S0/S1/S4/S5.

use anyhow::Context;
use tari_common_types::tari_address::TariAddress;

use crate::{
    broadcast::Broadcaster,
    config::Config,
    modes::{
        minotari_subprocess::{create_sign_and_submit, SeedRole},
        minotari_wallet_ops::{rewrite_birthday, wipe_and_reimport_via_create},
        Mode, ScanOutcome, TxRecord,
    },
    seed::SeedHandle,
    wallet_lifecycle::HarnessDataDir,
};

const LOG_TARGET: &str = "c::modes::new_wallet";

/// Mode 2's name as it appears in `results.<name>` of the result profile.
pub const MODE_NAME: &str = "new_wallet";

/// Mode 2 — `new_wallet`. Owns a per-mode [`HarnessDataDir`], a [`Broadcaster`]
/// for HTTP submission, and a clone of the harness [`Config`] / [`SeedHandle`]
/// references it needs for each subprocess call.
///
/// Construction is cheap and side-effect-free — the subprocess is only spawned
/// when [`Mode::send_single`] / [`Mode::send_batch_one_to_many`] runs.
pub struct NewWallet {
    cfg: Config,
    seeds: SeedHandle,
    broadcaster: Broadcaster,
    data_dir: HarnessDataDir,
    tx_idx: u64,
}

impl NewWallet {
    /// Construct from the harness-shared config + seeds, plus a freshly
    /// allocated per-mode data dir.
    ///
    /// The [`Broadcaster`] is built once here from `cfg.base_node_url` so the
    /// underlying `reqwest` connection pool is reused across calls.
    pub fn new(cfg: Config, seeds: SeedHandle, data_dir: HarnessDataDir) -> Self {
        let broadcaster = Broadcaster::new(&cfg.base_node_url);
        Self {
            cfg,
            seeds,
            broadcaster,
            data_dir,
            tx_idx: 0,
        }
    }

    fn next_tx_idx(&mut self) -> u64 {
        let idx = self.tx_idx;
        self.tx_idx = self.tx_idx.saturating_add(1);
        idx
    }
}

#[async_trait::async_trait]
impl Mode for NewWallet {
    fn name(&self) -> &'static str {
        MODE_NAME
    }

    async fn send_single(
        &mut self,
        recipient: &TariAddress,
        amount_microtari: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxRecord> {
        let idx = self.next_tx_idx();
        log::debug!(
            target: LOG_TARGET,
            "Mode 2 send_single idx={idx} amount={amount_microtari} fee_rate={fee_rate}",
        );
        let recipients = [(recipient.clone(), amount_microtari)];
        create_sign_and_submit(
            &self.cfg,
            &self.seeds,
            SeedRole::New,
            &recipients,
            fee_rate,
            &self.broadcaster,
            self.data_dir.path(),
            idx,
        )
        .await
        .context("Mode 2 create_sign_and_submit (single-recipient)")
    }

    async fn send_batch_one_to_many(
        &mut self,
        recipients: &[(TariAddress, u64)],
        fee_rate: u64,
    ) -> anyhow::Result<TxRecord> {
        let idx = self.next_tx_idx();
        log::debug!(
            target: LOG_TARGET,
            "Mode 2 send_batch_one_to_many idx={idx} K={} fee_rate={fee_rate}",
            recipients.len(),
        );
        create_sign_and_submit(
            &self.cfg,
            &self.seeds,
            SeedRole::New,
            recipients,
            fee_rate,
            &self.broadcaster,
            self.data_dir.path(),
            idx,
        )
        .await
        .context("Mode 2 create_sign_and_submit (batch 1-to-many)")
    }

    async fn scan_from_birthday(&mut self, _birthday: u16) -> anyhow::Result<ScanOutcome> {
        anyhow::bail!(read_side_placeholder("scan_from_birthday"));
    }

    async fn get_balance(&mut self) -> anyhow::Result<u64> {
        anyhow::bail!(read_side_placeholder("get_balance"));
    }

    async fn get_utxo_count(&mut self) -> anyhow::Result<u64> {
        anyhow::bail!(read_side_placeholder("get_utxo_count"));
    }

    async fn wipe_and_reimport(&mut self, birthday: u16) -> anyhow::Result<()> {
        // Per `analysis/DESIGN_AMENDMENT.md §8.3` step 4: rewrite the mnemonic's
        // birthday to `birthday` then re-create the wallet DB from the new
        // mnemonic via `minotari Create --seed-words`. The shared helper owns
        // the wipe + create + harness.toml plumbing; Mode 2's job is just to
        // produce the right mnemonic + password for it.
        let mnemonic_handle = self
            .seeds
            .mnemonic_new()
            .context("reading SeedRole::New mnemonic for wipe_and_reimport")?;
        let password_handle = self
            .seeds
            .wallet_password()
            .context("reading wallet password for wipe_and_reimport")?;
        let rewritten = rewrite_birthday(mnemonic_handle.reveal(), birthday)
            .context("rewriting CipherSeed birthday for Mode 2 re-import")?;
        wipe_and_reimport_via_create(
            &self.cfg,
            &mut self.data_dir,
            &rewritten,
            password_handle.reveal(),
        )
        .await
        .context("Mode 2 wipe_and_reimport via minotari Create --seed-words")?;
        log::info!(
            target: LOG_TARGET,
            "Mode 2 wipe_and_reimport complete (birthday={birthday})",
        );
        Ok(())
    }
}

/// Structured placeholder message for Mode 2/3 read-side flows whose CLI shape
/// at the pinned `minotari-cli` commit differs substantially from
/// `DESIGN.md §Mode 2 step 7`. See `analysis/DESIGN_AMENDMENT.md §8`.
fn read_side_placeholder(op: &'static str) -> String {
    format!(
        "Mode 2/3 {op} placeholder: the new minotari CLI's read-side subcommands \
         (Scan / Balance / Create --seed-words) at minotari-cli pinned commit \
         52a7287a3fe1e7831855649c530534af9f2d4830 differ from DESIGN.md §Mode 2 \
         step 7 (no list-utxos, Balance emits human stdout, Create not import-seed). \
         Real impl lands in step 3i once the scenarios layer knows the stdout-\
         parsing contract. See analysis/DESIGN_AMENDMENT.md §8.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, Seeds},
        gen_seed,
    };

    /// Builds a `Seeds` whose env-var names are unique per-test to avoid
    /// cross-test mutation races under cargo's default parallel runner.
    fn unique_seeds(suffix: &str) -> Seeds {
        Seeds {
            old: format!("WALLET_BENCHMARKS_TEST_MODE2_OLD_{suffix}"),
            new: format!("WALLET_BENCHMARKS_TEST_MODE2_NEW_{suffix}"),
            payment_processor: format!("WALLET_BENCHMARKS_TEST_MODE2_PP_{suffix}"),
            wallet_password: format!("WALLET_BENCHMARKS_TEST_MODE2_PW_{suffix}"),
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

    fn build_mode2(suffix: &str) -> (NewWallet, Seeds) {
        let seeds_cfg = unique_seeds(suffix);
        let m_old = gen_seed().expect("m_old");
        let m_new = gen_seed().expect("m_new");
        let m_pp = gen_seed().expect("m_pp");
        set_env(&seeds_cfg.old, &m_old);
        set_env(&seeds_cfg.new, &m_new);
        set_env(&seeds_cfg.payment_processor, &m_pp);
        set_env(&seeds_cfg.wallet_password, "pw");
        let cfg = Config {
            seeds: seeds_cfg.clone(),
            ..Config::default()
        };
        let seeds = SeedHandle::new(&seeds_cfg);
        let data_dir =
            HarnessDataDir::new(&format!("test-mode2-{suffix}"), MODE_NAME).expect("data dir");
        (NewWallet::new(cfg, seeds, data_dir), seeds_cfg)
    }

    fn teardown_seeds(seeds_cfg: &Seeds) {
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
        unset_env(&seeds_cfg.wallet_password);
    }

    #[test]
    fn mode2_name_is_new_wallet() {
        assert_eq!(MODE_NAME, "new_wallet");
        let (m, seeds) = build_mode2("NAME");
        assert_eq!(m.name(), "new_wallet");
        teardown_seeds(&seeds);
    }

    #[test]
    fn mode2_tx_idx_increments_per_send() {
        let (mut m, seeds) = build_mode2("TX_IDX");
        let a = m.next_tx_idx();
        let b = m.next_tx_idx();
        let c = m.next_tx_idx();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 2);
        teardown_seeds(&seeds);
    }

    #[tokio::test]
    async fn mode2_scan_from_birthday_bails_with_amendment_pointer() {
        let (mut m, seeds) = build_mode2("SCAN");
        let err = m
            .scan_from_birthday(0)
            .await
            .expect_err("scan placeholder must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("DESIGN_AMENDMENT.md §8"),
            "bail must point at the amendment: {msg}",
        );
        teardown_seeds(&seeds);
    }

    #[tokio::test]
    async fn mode2_get_balance_bails_with_amendment_pointer() {
        let (mut m, seeds) = build_mode2("BAL");
        let err = m
            .get_balance()
            .await
            .expect_err("get_balance placeholder must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("DESIGN_AMENDMENT.md §8"), "{msg}");
        teardown_seeds(&seeds);
    }

    #[tokio::test]
    async fn mode2_get_utxo_count_bails_with_amendment_pointer() {
        let (mut m, seeds) = build_mode2("UTXO");
        let err = m
            .get_utxo_count()
            .await
            .expect_err("get_utxo_count placeholder must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("DESIGN_AMENDMENT.md §8"), "{msg}");
        teardown_seeds(&seeds);
    }

    #[tokio::test]
    async fn mode2_wipe_and_reimport_attempts_subprocess() {
        // The real impl from 3i.0 spawns `minotari Create --seed-words ...`.
        // Point `minotari_path` at a non-existent binary so the spawn fails
        // deterministically with a "No such file or directory" — the test
        // does not depend on the host's $PATH state, but does prove the
        // method routes through the shared helper (the error context
        // includes "Mode 2 wipe_and_reimport").
        let (mut m, seeds) = build_mode2("WIPE");
        m.cfg.minotari_path = Some(std::path::PathBuf::from(
            "/wallet-benchmarks-test-nonexistent-minotari-binary",
        ));
        let err = m
            .wipe_and_reimport(0)
            .await
            .expect_err("missing binary must surface as a spawn error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Mode 2 wipe_and_reimport"),
            "error context must name Mode 2's wipe step: {msg}",
        );
        teardown_seeds(&seeds);
    }
}
