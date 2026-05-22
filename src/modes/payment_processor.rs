//! Mode 3 — `payment_processor`.
//!
//! Per `analysis/DESIGN.md §Mode 3 — concrete wiring` and
//! `analysis/DESIGN_ADDENDUM.md §Mode 3 CLI shape — proven`, Mode 3 IS Mode 2
//! with the batch 1-to-many shape: identical four-step pipeline (subprocess
//! `minotari create-unsigned-transaction` with K repeated `--recipient` flags,
//! parse, sign in-process via `sign_locked_transaction`, broadcast via
//! [`crate::broadcast::Broadcaster`]). The only operational difference is
//! **which seed slot** the helper reads — Mode 3 uses `SeedRole::Pp`
//! ([`crate::seed::SeedHandle::mnemonic_payment_processor`]) where Mode 2 uses
//! `SeedRole::New`.
//!
//! For S5 (per AC-19 / AC-20):
//! * **Batch arm** (Mode 3 primary): `send_batch_one_to_many(K=10 recipients)`
//!   invoked 10 times → 100 total recipients across 10 batch txs.
//! * **Individual arm** (Mode 3 context-only per ambiguity #3):
//!   `send_single` invoked 100 times against the same recipient list.
//!
//! Mode 3 itself doesn't know which scenario arm calls it; scenarios in 3i
//! orchestrate.
//!
//! See [`crate::modes::new_wallet`] (Mode 2) for the read-side placeholder
//! convention — Mode 3 inherits the same placeholder for
//! `scan_from_birthday` / `get_balance` / `get_utxo_count` / `wipe_and_reimport`,
//! pointing at the same `DESIGN_AMENDMENT.md §8` follow-up.

use anyhow::Context;
use tari_common_types::tari_address::TariAddress;

use std::time::Instant;

use crate::{
    broadcast::Broadcaster,
    config::Config,
    modes::{
        minotari_subprocess::{create_sign_and_submit, SeedRole},
        minotari_wallet_ops::{
            rewrite_birthday, run_scan_subprocess, wipe_and_reimport_via_create,
        },
        Mode, ScanOutcome, TxRecord,
    },
    seed::SeedHandle,
    wallet_lifecycle::HarnessDataDir,
};

const LOG_TARGET: &str = "c::modes::payment_processor";

/// Mode 3's name as it appears in `results.<name>` of the result profile.
pub const MODE_NAME: &str = "payment_processor";

/// Mode 3 — `payment_processor`. Owns the same shape as
/// [`crate::modes::new_wallet::NewWallet`] (per-mode [`HarnessDataDir`],
/// [`Broadcaster`], cloned config + seeds) but routes the helper through
/// `SeedRole::Pp` so the payment-processor wallet's seed is used for
/// in-process signing.
pub struct PaymentProcessor {
    cfg: Config,
    seeds: SeedHandle,
    broadcaster: Broadcaster,
    data_dir: HarnessDataDir,
    tx_idx: u64,
    /// Cached [`ScanOutcome`] from the most recent successful
    /// [`Mode::scan_from_birthday`] call. [`Mode::get_utxo_count`] reads
    /// `outputs_found` from here per `analysis/DESIGN_AMENDMENT.md §8.3`
    /// step 4 — same convention as [`crate::modes::new_wallet::NewWallet`].
    last_scan: Option<ScanOutcome>,
}

impl PaymentProcessor {
    /// Construct from the harness-shared config + seeds, plus a freshly
    /// allocated per-mode data dir.
    pub fn new(cfg: Config, seeds: SeedHandle, data_dir: HarnessDataDir) -> Self {
        let broadcaster = Broadcaster::new(&cfg.base_node_url);
        Self {
            cfg,
            seeds,
            broadcaster,
            data_dir,
            tx_idx: 0,
            last_scan: None,
        }
    }

    fn next_tx_idx(&mut self) -> u64 {
        let idx = self.tx_idx;
        self.tx_idx = self.tx_idx.saturating_add(1);
        idx
    }
}

#[async_trait::async_trait]
impl Mode for PaymentProcessor {
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
            "Mode 3 send_single idx={idx} amount={amount_microtari} fee_rate={fee_rate}",
        );
        let recipients = [(recipient.clone(), amount_microtari)];
        create_sign_and_submit(
            &self.cfg,
            &self.seeds,
            SeedRole::Pp,
            &recipients,
            fee_rate,
            &self.broadcaster,
            self.data_dir.path(),
            idx,
        )
        .await
        .context("Mode 3 create_sign_and_submit (single-recipient)")
    }

    async fn send_batch_one_to_many(
        &mut self,
        recipients: &[(TariAddress, u64)],
        fee_rate: u64,
    ) -> anyhow::Result<TxRecord> {
        let idx = self.next_tx_idx();
        log::debug!(
            target: LOG_TARGET,
            "Mode 3 send_batch_one_to_many idx={idx} K={} fee_rate={fee_rate}",
            recipients.len(),
        );
        create_sign_and_submit(
            &self.cfg,
            &self.seeds,
            SeedRole::Pp,
            recipients,
            fee_rate,
            &self.broadcaster,
            self.data_dir.path(),
            idx,
        )
        .await
        .context("Mode 3 create_sign_and_submit (batch 1-to-many)")
    }

    async fn scan_from_birthday(&mut self, birthday: u16) -> anyhow::Result<ScanOutcome> {
        // Mirror of [`crate::modes::new_wallet::NewWallet::scan_from_birthday`];
        // see that impl's comment for the design trace and field-by-field
        // rationale. The only difference is the seed slot consumed by the
        // prerequisite `wipe_and_reimport` (`mnemonic_payment_processor` here).
        let started = Instant::now();
        self.wipe_and_reimport(birthday)
            .await
            .context("Mode 3 wipe_and_reimport prerequisite to scan")?;
        let parsed = run_scan_subprocess(
            &self.cfg,
            self.data_dir.path(),
            self.seeds
                .wallet_password()
                .context("reading wallet password for Mode 3 scan")?
                .reveal(),
            None,
        )
        .await
        .context("Mode 3 run_scan_subprocess")?;
        let t_scan_ms = started.elapsed().as_millis() as u64;
        let outputs_found = parsed.outputs_found.unwrap_or(0);
        let outcome = ScanOutcome {
            t_scan_ms,
            h_tip_start: 0,
            h_tip_end: 0,
            outputs_found,
            utxo_count: outputs_found,
            balance_microtari: 0,
        };
        self.last_scan = Some(outcome.clone());
        log::info!(
            target: LOG_TARGET,
            "Mode 3 scan_from_birthday complete (birthday={birthday}, outputs_found={outputs_found}, t_scan_ms={t_scan_ms})",
        );
        Ok(outcome)
    }

    async fn get_balance(&mut self) -> anyhow::Result<u64> {
        anyhow::bail!(read_side_placeholder("get_balance"));
    }

    async fn get_utxo_count(&mut self) -> anyhow::Result<u64> {
        anyhow::bail!(read_side_placeholder("get_utxo_count"));
    }

    async fn wipe_and_reimport(&mut self, birthday: u16) -> anyhow::Result<()> {
        // Mirror of `crate::modes::new_wallet::NewWallet::wipe_and_reimport`,
        // differing only in seed slot (`mnemonic_payment_processor` vs
        // `mnemonic_new`). See `analysis/DESIGN_AMENDMENT.md §8.3`.
        let mnemonic_handle = self
            .seeds
            .mnemonic_payment_processor()
            .context("reading SeedRole::Pp mnemonic for wipe_and_reimport")?;
        let password_handle = self
            .seeds
            .wallet_password()
            .context("reading wallet password for wipe_and_reimport")?;
        let rewritten = rewrite_birthday(mnemonic_handle.reveal(), birthday)
            .context("rewriting CipherSeed birthday for Mode 3 re-import")?;
        wipe_and_reimport_via_create(
            &self.cfg,
            &mut self.data_dir,
            &rewritten,
            password_handle.reveal(),
        )
        .await
        .context("Mode 3 wipe_and_reimport via minotari Create --seed-words")?;
        log::info!(
            target: LOG_TARGET,
            "Mode 3 wipe_and_reimport complete (birthday={birthday})",
        );
        Ok(())
    }
}

/// Mirrors [`crate::modes::new_wallet::read_side_placeholder`] — Mode 3
/// shares the same `minotari` CLI surface gap. See
/// `analysis/DESIGN_AMENDMENT.md §8`.
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

    fn unique_seeds(suffix: &str) -> Seeds {
        Seeds {
            old: format!("WALLET_BENCHMARKS_TEST_MODE3_OLD_{suffix}"),
            new: format!("WALLET_BENCHMARKS_TEST_MODE3_NEW_{suffix}"),
            payment_processor: format!("WALLET_BENCHMARKS_TEST_MODE3_PP_{suffix}"),
            wallet_password: format!("WALLET_BENCHMARKS_TEST_MODE3_PW_{suffix}"),
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

    fn build_mode3(suffix: &str) -> (PaymentProcessor, Seeds) {
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
            HarnessDataDir::new(&format!("test-mode3-{suffix}"), MODE_NAME).expect("data dir");
        (PaymentProcessor::new(cfg, seeds, data_dir), seeds_cfg)
    }

    fn teardown_seeds(seeds_cfg: &Seeds) {
        unset_env(&seeds_cfg.old);
        unset_env(&seeds_cfg.new);
        unset_env(&seeds_cfg.payment_processor);
        unset_env(&seeds_cfg.wallet_password);
    }

    #[test]
    fn mode3_name_is_payment_processor() {
        assert_eq!(MODE_NAME, "payment_processor");
        let (m, seeds) = build_mode3("NAME");
        assert_eq!(m.name(), "payment_processor");
        teardown_seeds(&seeds);
    }

    #[test]
    fn mode3_tx_idx_increments_per_send() {
        let (mut m, seeds) = build_mode3("TX_IDX");
        let a = m.next_tx_idx();
        let b = m.next_tx_idx();
        let c = m.next_tx_idx();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 2);
        teardown_seeds(&seeds);
    }

    #[tokio::test]
    async fn mode3_scan_from_birthday_attempts_subprocess() {
        // Mirror of the Mode 2 test — first spawn (inside wipe_and_reimport)
        // fails because `minotari_path` points at a non-existent binary; the
        // failure surfaces with Mode 3's scan context.
        let (mut m, seeds) = build_mode3("SCAN");
        m.cfg.minotari_path = Some(std::path::PathBuf::from(
            "/wallet-benchmarks-test-nonexistent-minotari-binary",
        ));
        let err = m
            .scan_from_birthday(0)
            .await
            .expect_err("missing binary must surface as a spawn error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Mode 3") && msg.contains("scan"),
            "error must name Mode 3's scan path: {msg}",
        );
        teardown_seeds(&seeds);
    }

    #[tokio::test]
    async fn mode3_get_balance_bails_with_amendment_pointer() {
        let (mut m, seeds) = build_mode3("BAL");
        let err = m
            .get_balance()
            .await
            .expect_err("get_balance placeholder must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("DESIGN_AMENDMENT.md §8"), "{msg}");
        teardown_seeds(&seeds);
    }

    #[tokio::test]
    async fn mode3_wipe_and_reimport_attempts_subprocess() {
        // Mirror of `mode2_wipe_and_reimport_attempts_subprocess` — confirms
        // Mode 3 routes through the shared helper. Point `minotari_path` at a
        // non-existent binary so the spawn deterministically surfaces the
        // Mode 3 wipe context.
        let (mut m, seeds) = build_mode3("WIPE");
        m.cfg.minotari_path = Some(std::path::PathBuf::from(
            "/wallet-benchmarks-test-nonexistent-minotari-binary",
        ));
        let err = m
            .wipe_and_reimport(0)
            .await
            .expect_err("missing binary must surface as a spawn error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Mode 3 wipe_and_reimport"),
            "error context must name Mode 3's wipe step: {msg}",
        );
        teardown_seeds(&seeds);
    }
}
