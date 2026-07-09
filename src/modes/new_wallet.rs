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
//! `get_utxo_count`, `wipe_and_reimport`)**: wired in step 3i.0 against the
//! pinned `minotari-cli` commit per `analysis/DESIGN_AMENDMENT.md §8.3`. The
//! shared orchestration lives in
//! [`crate::modes::minotari_wallet_ops`] — Mode 2 and Mode 3 both route
//! through it, differing only in seed slot (`mnemonic_new` here,
//! `mnemonic_payment_processor` in Mode 3). UTXO discovery counts come from
//! the wallet's sqlite3 DB via [`crate::wallet_db`] (PR #6 review threads
//! 4.1 + 4.3, `analysis/specs/THREADS_4_1_4_3_SPEC.md`).

use anyhow::Context;
use tari_common_types::tari_address::TariAddress;

use std::{path::PathBuf, sync::Arc, time::Instant};

use crate::{
    broadcast::Broadcaster,
    config::Config,
    modes::{
        minotari_subprocess::{create_sign_and_submit, MinotariSubprocessDispatcher, SeedRole},
        minotari_wallet_ops::{
            rewrite_birthday, run_balance_subprocess, run_scan_subprocess,
            wait_for_balance_positive, wait_for_confirmed_spendable, wipe_and_reimport_via_create,
        },
        Mode, S4Dispatcher, ScanOutcome, TxRecord,
    },
    seed::SeedHandle,
    wallet_db::{LiveWalletDb, WalletDbArc},
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
    /// Read-only sqlite3 query surface for `outputs_found` /
    /// `utxo_count`. Production wires [`LiveWalletDb`]; tests inject a
    /// `FakeWalletDb` via [`NewWallet::new_with_wallet_db`].
    wallet_db: WalletDbArc,
}

impl NewWallet {
    /// Construct from the harness-shared config + seeds, plus a freshly
    /// allocated per-mode data dir.
    ///
    /// The [`Broadcaster`] is built once here from `cfg.base_node_url` so the
    /// underlying `reqwest` connection pool is reused across calls.
    pub fn new(cfg: Config, seeds: SeedHandle, data_dir: HarnessDataDir) -> Self {
        Self::new_with_wallet_db(cfg, seeds, data_dir, Arc::new(LiveWalletDb))
    }

    /// Test-only constructor — accepts an arbitrary [`WalletDbArc`]. Used by
    /// the per-mode unit tests to swap in a `FakeWalletDb` that returns
    /// canned counts without touching the filesystem.
    pub(crate) fn new_with_wallet_db(
        cfg: Config,
        seeds: SeedHandle,
        data_dir: HarnessDataDir,
        wallet_db: WalletDbArc,
    ) -> Self {
        let broadcaster = Broadcaster::new(&cfg.base_node_url);
        Self {
            cfg,
            seeds,
            broadcaster,
            data_dir,
            tx_idx: 0,
            wallet_db,
        }
    }

    fn wallet_db_path(&self) -> PathBuf {
        self.data_dir.path().join("wallet.sqlite3")
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

    async fn settle_after_send(&mut self) -> anyhow::Result<bool> {
        // A send only locks its inputs and writes a pending_transactions
        // row; the outputs table gains rows exclusively via `minotari
        // scan`, and the input selector only picks outputs whose
        // confirmed_height is set. Scan-and-wait until the confirmed
        // spendable count rises above the post-send baseline so the next
        // chained send can lock an input instead of failing with "Funds
        // are pending". See `wait_for_confirmed_spendable` for the
        // predicate caveat (any new confirmed output satisfies the gate).
        let db_path = self.wallet_db_path();
        let baseline = self
            .wallet_db
            .count_confirmed_spendable_utxos(&db_path)
            .context("Mode 2 settle_after_send baseline count")?;
        let password = self
            .seeds
            .wallet_password()
            .context("reading wallet password for Mode 2 settle_after_send")?;
        let settled = wait_for_confirmed_spendable(
            &self.cfg,
            self.data_dir.path(),
            password.reveal(),
            self.wallet_db.as_ref(),
            baseline,
            None,
        )
        .await
        .context("Mode 2 settle_after_send")?;
        if !settled {
            log::warn!(
                target: LOG_TARGET,
                "Mode 2 settle_after_send: no new confirmed output within the settle \
                 deadline (baseline {baseline}); the caller decides whether that fails \
                 its scenario (see Mode::settle_after_send)",
            );
        }
        Ok(settled)
    }

    async fn refresh_wallet_view(&mut self) -> anyhow::Result<()> {
        // One bounded catch-up scan so the sqlite view tracks the chain
        // during S1's confirmation polling. Without it the DB is frozen
        // between sends (outputs rows are created only by scans) and
        // `wait_for_state_change` could never observe the self-send.
        let password = self
            .seeds
            .wallet_password()
            .context("reading wallet password for Mode 2 refresh_wallet_view")?;
        run_scan_subprocess(&self.cfg, self.data_dir.path(), password.reveal(), None)
            .await
            .context("Mode 2 refresh_wallet_view scan")?;
        Ok(())
    }

    async fn scan_from_birthday(&mut self, birthday: u16) -> anyhow::Result<ScanOutcome> {
        // Per PR #6 review threads 4.1 + 4.3 (analysis/specs/THREADS_4_1_4_3_SPEC.md):
        //   wipe_and_reimport(birthday) -> run minotari Scan -> read counts
        //   from the wallet's sqlite3 DB (NOT the rejected stderr parse).
        // `outputs_found` is the total non-burn, non-deleted output count;
        // `utxo_count` is the spendable subset (status = 'UNSPENT').
        let started = Instant::now();
        self.wipe_and_reimport(birthday)
            .await
            .context("Mode 2 wipe_and_reimport prerequisite to scan")?;
        let _parsed = run_scan_subprocess(
            &self.cfg,
            self.data_dir.path(),
            self.seeds
                .wallet_password()
                .context("reading wallet password for Mode 2 scan")?
                .reveal(),
            None,
        )
        .await
        .context("Mode 2 run_scan_subprocess")?;
        // Bug 3 from the canonical-baseline runbook: even after Scan exits,
        // the wallet finalizes per-output commitment + state-write work
        // asynchronously and `create-unsigned-transaction` then hits
        // insufficient_funds. Poll Balance until the wallet reports a
        // positive total (or the default 5-minute deadline elapses) before
        // returning. The Mode 1 equivalent is `has_done_initial_validation`
        // gate in `wallet_lifecycle::console_wallet::wait_ready`.
        let _seen_balance = wait_for_balance_positive(&self.cfg, self.data_dir.path(), None)
            .await
            .context("Mode 2 wait_for_balance_positive after scan")?;
        let t_scan_ms = started.elapsed().as_millis() as u64;
        let db_path = self.wallet_db_path();
        let outputs_found = self
            .wallet_db
            .count_outputs(&db_path)
            .context("Mode 2 post-scan count_outputs from wallet DB")?;
        let utxo_count = self
            .wallet_db
            .count_spendable_utxos(&db_path)
            .context("Mode 2 post-scan count_spendable_utxos from wallet DB")?;
        let outcome = ScanOutcome {
            t_scan_ms,
            // h_tip_start / h_tip_end: scenarios layer fills these from
            // base-node tip queries (Mode 1's old_wallet.rs uses the same
            // "0 here means scenario fills it in" convention).
            h_tip_start: 0,
            h_tip_end: 0,
            outputs_found,
            utxo_count,
            // balance_microtari: scenarios layer composes a `get_balance` call
            // after `scan_from_birthday`.
            balance_microtari: 0,
        };
        log::info!(
            target: LOG_TARGET,
            "Mode 2 scan_from_birthday complete (birthday={birthday}, outputs_found={outputs_found}, utxo_count={utxo_count}, t_scan_ms={t_scan_ms})",
        );
        Ok(outcome)
    }

    async fn get_balance(&mut self) -> anyhow::Result<u64> {
        // Direct subprocess invocation — `minotari Balance` reads the DB at
        // its current state and returns the parsed microTari total.
        run_balance_subprocess(&self.cfg, self.data_dir.path())
            .await
            .context("Mode 2 get_balance via minotari Balance")
    }

    async fn get_utxo_count(&mut self) -> anyhow::Result<u64> {
        // Per PR #6 review threads 4.1 + 4.3: the canonical source is a
        // sqlite3 read of `outputs WHERE deleted_at IS NULL AND is_burn = 0
        // AND status = 'UNSPENT'` — no subprocess and no cached state.
        let db_path = self.wallet_db_path();
        self.wallet_db
            .count_spendable_utxos(&db_path)
            .context("Mode 2 get_utxo_count via wallet DB")
    }

    fn dispatcher(&self) -> Arc<dyn S4Dispatcher> {
        // Wrap Mode 2's already-held state in Arcs for cross-task sharing.
        // Constructing fresh Arcs every call is the simplest contract — the
        // caller (S4 scenario, scenarios/s4_concurrent.rs) acquires the
        // dispatcher once per sub-block and clones the inner Arc inside each
        // spawned task. Per `analysis/DESIGN_AMENDMENT.md §9.6` Option B.
        Arc::new(MinotariSubprocessDispatcher::new(
            Arc::new(self.cfg.clone()),
            Arc::new(self.seeds.clone()),
            Arc::new(Broadcaster::new(&self.cfg.base_node_url)),
            Arc::new(PathBuf::from(self.data_dir.path())),
            SeedRole::New,
            self.tx_idx,
        ))
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
    async fn mode2_scan_from_birthday_attempts_subprocess() {
        // Scan first wipes (which spawns `minotari Create`) and then scans
        // (`minotari Scan`). With a non-existent binary the failure surfaces
        // at the first spawn (inside wipe_and_reimport) with Mode 2's scan
        // context wrapped over the inner wipe context.
        let (mut m, seeds) = build_mode2("SCAN");
        m.cfg.minotari_path = Some(std::path::PathBuf::from(
            "/wallet-benchmarks-test-nonexistent-minotari-binary",
        ));
        let err = m
            .scan_from_birthday(0)
            .await
            .expect_err("missing binary must surface as a spawn error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Mode 2") && msg.contains("scan"),
            "error must name Mode 2's scan path: {msg}",
        );
        teardown_seeds(&seeds);
    }

    #[tokio::test]
    async fn mode2_get_balance_attempts_subprocess() {
        let (mut m, seeds) = build_mode2("BAL");
        m.cfg.minotari_path = Some(std::path::PathBuf::from(
            "/wallet-benchmarks-test-nonexistent-minotari-binary",
        ));
        let err = m
            .get_balance()
            .await
            .expect_err("missing binary must surface as a spawn error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Mode 2 get_balance"),
            "error must name Mode 2's get_balance: {msg}",
        );
        teardown_seeds(&seeds);
    }

    #[tokio::test]
    async fn mode2_get_utxo_count_reads_from_wallet_db() {
        // Per PR #6 threads 4.1 + 4.3 (analysis/specs/THREADS_4_1_4_3_SPEC.md):
        // `get_utxo_count` queries the wallet sqlite3 DB directly via the
        // injected `WalletDb`. With a fake returning 7, the method must
        // return 7 — and crucially must NOT spawn any subprocess.
        let seeds_cfg = unique_seeds("UTXO_DB");
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
        let data_dir = HarnessDataDir::new("test-mode2-UTXO_DB", MODE_NAME).expect("data dir");
        let fake = Arc::new(crate::wallet_db::FakeWalletDb::ok(99, 7));
        let mut m = NewWallet::new_with_wallet_db(cfg, seeds, data_dir, fake);
        let n = m
            .get_utxo_count()
            .await
            .expect("DB-backed get_utxo_count returns Ok");
        assert_eq!(n, 7);
        teardown_seeds(&seeds_cfg);
    }

    #[tokio::test]
    async fn mode2_get_utxo_count_surfaces_db_error() {
        // A fake WalletDb that errors on count_spendable_utxos must propagate
        // through Mode 2's context wrapper (PR #6 threads 4.1 + 4.3 require
        // DB errors to surface, not be silently masked into 0).
        let seeds_cfg = unique_seeds("UTXO_DB_ERR");
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
        let data_dir = HarnessDataDir::new("test-mode2-UTXO_DB_ERR", MODE_NAME).expect("data dir");
        let fake = Arc::new(crate::wallet_db::FakeWalletDb {
            canned_count_outputs: Ok(0),
            canned_count_spendable: Err("simulated DB failure".to_string()),
            canned_count_confirmed_spendable: Ok(0),
        });
        let mut m = NewWallet::new_with_wallet_db(cfg, seeds, data_dir, fake);
        let err = m
            .get_utxo_count()
            .await
            .expect_err("DB error must propagate");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Mode 2 get_utxo_count") && msg.contains("simulated DB failure"),
            "error must include Mode 2 context + the inner cause: {msg}",
        );
        teardown_seeds(&seeds_cfg);
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
