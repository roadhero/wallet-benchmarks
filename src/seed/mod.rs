//! Three-seed loading and address derivation for the harness.
//!
//! Per `analysis/DESIGN.md §Secret handling` and `analysis/DESIGN_ADDENDUM.md
//! §S1`, the harness keeps mnemonic and passphrase material in environment
//! variables — never on disk, never inside `Config`. [`SeedHandle`] is the
//! single runtime accessor: it carries the *names* of the env vars (from
//! [`crate::config::Seeds`]) and re-reads the values at every call so an
//! operator can rotate a seed mid-run by exporting a new value.
//!
//! Values come back as [`RedactedString`] — `tari_utilities::Hidden<String>`
//! is the maintainer-idiom equivalent, but it stringifies as
//! `"Hidden<alloc::string::String>"` rather than `"[REDACTED]"`. The harness
//! prefers a uniform `[REDACTED]` placeholder for human-readable logs while
//! keeping the same zeroize-on-drop semantics.
//!
//! Address derivation (`address_old` / `address_new` / `address_payment_processor`)
//! shares its code path with [`crate::print_address`] — both call
//! [`derive_address`] below — so the bounty's three address-touching surfaces
//! (`print-address` subcommand, `enforce_funding` pre-flight, Mode 2/3
//! recipient computation) all read from the same site.

pub mod redact;

use std::str::FromStr;

use anyhow::Context;
use tari_common::configuration::Network;
use tari_common_types::{
    seeds::{cipher_seed::CipherSeed, mnemonic::Mnemonic, seed_words::SeedWords},
    tari_address::{TariAddress, TariAddressFeatures},
};
use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
use tari_utilities::hidden::Hidden;

const LOG_TARGET: &str = "c::seed";

/// Per-call zeroize-on-drop string wrapper. Mirrors `tari_utilities::Hidden`
/// but with a fixed `[REDACTED]` Debug / Display form — uniformity matters
/// more than the type-name embedded in `Hidden<T>`'s formatter.
pub struct RedactedString {
    inner: Hidden<String>,
}

impl RedactedString {
    /// Wrap a string. The plaintext lives in a zeroize-on-drop allocation.
    pub fn new(s: String) -> Self {
        Self {
            inner: Hidden::hide(s),
        }
    }

    /// Reveal the underlying plaintext. Callers are responsible for keeping
    /// the borrow scoped — never assign `let s = handle.reveal().clone();`
    /// into a long-lived binding.
    pub fn reveal(&self) -> &str {
        self.inner.reveal().as_str()
    }
}

impl std::fmt::Debug for RedactedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[REDACTED]")
    }
}

impl std::fmt::Display for RedactedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[REDACTED]")
    }
}

// Note: zeroize-on-drop is handled transitively by the inner `Hidden<String>`,
// whose own `Drop` impl zeros the boxed string. Adding an explicit `Zeroize`
// impl here would require pulling `zeroize` as a direct dep, which isn't
// listed in `DESIGN.md §Dependency strategy`. The transitive path is the
// same code that all other tari ecosystem crates rely on.

/// Per-mode seed slot identifier.
///
/// Names the three benchmarked wallets' seed slots — `Old` / `New` / `Pp`
/// — so callers that need to address-derive against a specific mode (e.g.
/// [`crate::scenarios::RecipientStrategy::SelfAddress`] for the send-to-self
/// scenarios S4/S6/S7) can name the slot uniformly. Mirrors the wording the
/// modes layer already uses in its module docs (`SeedRole::New`,
/// `SeedRole::Pp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeedRole {
    /// Mode 1 — `old_wallet` seed slot.
    Old,
    /// Mode 2 — `new_wallet` seed slot.
    New,
    /// Mode 3 — `payment_processor` seed slot.
    Pp,
}

/// Runtime accessor for the three seed mnemonics and the wallet passphrase.
///
/// Holds env-var *names*, not values. Every accessor re-reads the env var
/// via [`std::env::var`] so a mid-run rotation is visible without
/// reconstructing the handle. The trade-off is documented per-accessor:
/// callers MUST NOT cache the returned [`RedactedString`] across long-lived
/// boundaries; the runtime contract is "fresh value per call".
///
/// `Clone` is derived because S4's [`crate::modes::S4Dispatcher`] handles
/// (per `analysis/DESIGN_AMENDMENT.md §9.6` Option B) capture this handle
/// inside an `Arc` shared across N concurrent dispatch tasks. The handle
/// holds only env-var names, so cloning is a pointer-and-string copy with
/// no secret material involved.
#[derive(Clone)]
pub struct SeedHandle {
    seeds_config: crate::config::Seeds,
}

impl SeedHandle {
    /// Construct a `SeedHandle` against the env-var names recorded in
    /// `seeds_config`. The values themselves are not touched here — every
    /// accessor reads them on demand.
    pub fn new(seeds_config: &crate::config::Seeds) -> Self {
        Self {
            seeds_config: seeds_config.clone(),
        }
    }

    /// Old-wallet mode seed mnemonic. Re-read from env on every call.
    pub fn mnemonic_old(&self) -> anyhow::Result<RedactedString> {
        read_env_redacted(&self.seeds_config.old)
    }

    /// New-wallet mode seed mnemonic. Re-read from env on every call.
    pub fn mnemonic_new(&self) -> anyhow::Result<RedactedString> {
        read_env_redacted(&self.seeds_config.new)
    }

    /// Payment-processor mode seed mnemonic. Re-read from env on every call.
    pub fn mnemonic_payment_processor(&self) -> anyhow::Result<RedactedString> {
        read_env_redacted(&self.seeds_config.payment_processor)
    }

    /// Wallet passphrase (shared across the three modes per
    /// `DESIGN.md §Secret handling`). Re-read from env on every call.
    pub fn wallet_password(&self) -> anyhow::Result<RedactedString> {
        read_env_redacted(&self.seeds_config.wallet_password)
    }

    /// Assert that the three seed mnemonics are mutually distinct.
    ///
    /// Per `DESIGN.md §Secret handling` (AC-35), the harness uses three
    /// separate funded wallets. A duplicate mnemonic anywhere across the
    /// three slots collapses two modes onto one wallet and silently mixes
    /// their UTXO sets — a measurement bug, not a configuration nicety.
    pub fn assert_distinct(&self) -> anyhow::Result<()> {
        let old = self.mnemonic_old()?;
        let new = self.mnemonic_new()?;
        let pp = self.mnemonic_payment_processor()?;
        // Compare under the same scope so all three `RedactedString`s drop
        // (and zero) together when the function returns.
        if old.reveal() == new.reveal() {
            anyhow::bail!(
                "${} and ${} resolve to the same mnemonic — the three harness seeds must be distinct",
                self.seeds_config.old,
                self.seeds_config.new,
            );
        }
        if old.reveal() == pp.reveal() {
            anyhow::bail!(
                "${} and ${} resolve to the same mnemonic — the three harness seeds must be distinct",
                self.seeds_config.old,
                self.seeds_config.payment_processor,
            );
        }
        if new.reveal() == pp.reveal() {
            anyhow::bail!(
                "${} and ${} resolve to the same mnemonic — the three harness seeds must be distinct",
                self.seeds_config.new,
                self.seeds_config.payment_processor,
            );
        }
        Ok(())
    }

    /// Wallet address for the old-wallet mode seed.
    pub fn address_old(&self) -> anyhow::Result<TariAddress> {
        let m = self.mnemonic_old()?;
        derive_address(m.reveal())
    }

    /// Wallet address for the new-wallet mode seed.
    pub fn address_new(&self) -> anyhow::Result<TariAddress> {
        let m = self.mnemonic_new()?;
        derive_address(m.reveal())
    }

    /// Wallet address for the payment-processor mode seed.
    pub fn address_payment_processor(&self) -> anyhow::Result<TariAddress> {
        let m = self.mnemonic_payment_processor()?;
        derive_address(m.reveal())
    }

    /// Wallet address for the named [`SeedRole`] slot.
    ///
    /// Thin dispatcher over [`Self::address_old`] / [`Self::address_new`] /
    /// [`Self::address_payment_processor`] so callers that want the address
    /// for a *named* slot — notably
    /// [`crate::scenarios::RecipientStrategy::SelfAddress`] — don't need a
    /// hand-rolled match at every call site.
    pub fn address_for(&self, role: SeedRole) -> anyhow::Result<TariAddress> {
        match role {
            SeedRole::Old => self.address_old(),
            SeedRole::New => self.address_new(),
            SeedRole::Pp => self.address_payment_processor(),
        }
    }

    /// Resolve the mnemonic for a role at call time, mirroring
    /// [`Self::address_for`]. Used by [`crate::wallet_lifecycle::balance_query::WalletGrpcBalanceQuery`]
    /// to spawn a transient `console_wallet` keyed by role rather than by a
    /// hardcoded slot.
    pub fn mnemonic_for(&self, role: SeedRole) -> anyhow::Result<RedactedString> {
        match role {
            SeedRole::Old => self.mnemonic_old(),
            SeedRole::New => self.mnemonic_new(),
            SeedRole::Pp => self.mnemonic_payment_processor(),
        }
    }

    /// Thin test-only constructor — wraps a freshly-defaulted [`Seeds`].
    /// Used by scenario unit tests that need a `&SeedHandle` to populate
    /// [`crate::scenarios::ScenarioCtx`] without touching env vars. The
    /// resulting handle's accessors will return env-var-missing errors if
    /// called; tests that need a real value set the env explicitly.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            seeds_config: crate::config::Seeds::default(),
        }
    }
}

/// Read an env var by name into a [`RedactedString`], with a context-rich
/// error if the var is unset.
fn read_env_redacted(env_name: &str) -> anyhow::Result<RedactedString> {
    log::debug!(target: LOG_TARGET, "reading env var ${env_name} (value redacted)");
    let value = std::env::var(env_name).with_context(|| {
        format!(
            "reading seed/passphrase from env var ${env_name} (set it before invoking the harness)"
        )
    })?;
    Ok(RedactedString::new(value))
}

/// Derive the Esmeralda wallet address from a 24-word Tari mnemonic.
///
/// This is the single shared site referenced by [`crate::print_address`],
/// [`SeedHandle::address_old`] / `_new` / `_payment_processor`, and (via
/// `enforce_funding`) the harness's funding pre-flight. Per
/// `analysis/API_DRIFT.md §Step 3b`, `WalletType::tari_address()` does not
/// exist at the published v5.3.1 surface, so we assemble the dual address
/// from the wallet's public view/spend keys and the one-sided features flag,
/// matching PR #99's recipient-construction shape.
pub fn derive_address(mnemonic: &str) -> anyhow::Result<TariAddress> {
    let seed_words = SeedWords::from_str(mnemonic)
        .map_err(|e| anyhow::Error::msg(format!("parsing mnemonic words: {e}")))?;
    let cipher_seed = <CipherSeed as Mnemonic<CipherSeed>>::from_mnemonic(&seed_words, None)
        .map_err(|e| anyhow::Error::msg(format!("decoding CipherSeed from mnemonic: {e}")))?;
    let seed_words_wallet =
        SeedWordsWallet::construct_new(cipher_seed).map_err(anyhow::Error::msg)?;
    let wallet = WalletType::SeedWords(seed_words_wallet);
    let view_pub = wallet.get_public_view_key();
    let spend_pub = wallet.get_public_spend_key();
    TariAddress::new_dual_address(
        view_pub,
        spend_pub,
        Network::Esmeralda,
        TariAddressFeatures::create_one_sided_only(),
        None,
    )
    .map_err(|e| anyhow::Error::msg(format!("assembling TariAddress: {e}")))
}

/// Derive a pool of `size` distinct dual addresses from the seed mnemonic at
/// `role`. Each slot `i` carries a distinct `payment_id_user_data = [i as bytes]`
/// — per `tari_common_types::tari_address::DualAddress`, supplying a non-`None`
/// payment-id sets the `PAYMENT_ID` features bit and encodes the bytes into
/// the address payload, so every slot serialises (`to_base58()`) to a distinct
/// string while sharing the same underlying view/spend keypair.
///
/// Used by S5 (AC-19) to materialise a 100-recipient pool that survives both
/// the individual arm (100 single-recipient sends) and the batch arm (10
/// 10-recipient batch sends) against a single seed — no per-slot mnemonic
/// derivation needed. Deterministic in `size` and `role`: the same call
/// reproduces the same list, so re-runs against the same seed produce the
/// same `recipient_list_hash` (schema §S5 line 206).
pub fn derive_recipient_pool(
    seeds: &SeedHandle,
    role: SeedRole,
    size: usize,
) -> anyhow::Result<Vec<TariAddress>> {
    let mnemonic = match role {
        SeedRole::Old => seeds.mnemonic_old()?,
        SeedRole::New => seeds.mnemonic_new()?,
        SeedRole::Pp => seeds.mnemonic_payment_processor()?,
    };
    let seed_words = SeedWords::from_str(mnemonic.reveal())
        .map_err(|e| anyhow::Error::msg(format!("parsing mnemonic words: {e}")))?;
    let cipher_seed = <CipherSeed as Mnemonic<CipherSeed>>::from_mnemonic(&seed_words, None)
        .map_err(|e| anyhow::Error::msg(format!("decoding CipherSeed from mnemonic: {e}")))?;
    let seed_words_wallet =
        SeedWordsWallet::construct_new(cipher_seed).map_err(anyhow::Error::msg)?;
    let wallet = WalletType::SeedWords(seed_words_wallet);
    let view_pub = wallet.get_public_view_key();
    let spend_pub = wallet.get_public_spend_key();
    let mut pool = Vec::with_capacity(size);
    for i in 0..size {
        // Little-endian u64 of the slot index — 8 bytes, well under
        // `MAX_ENCRYPTED_DATA_SIZE`. Distinct per slot, so each address
        // serialises to a distinct base58.
        let payment_id = (i as u64).to_le_bytes().to_vec();
        let addr = TariAddress::new_dual_address(
            view_pub.clone(),
            spend_pub.clone(),
            Network::Esmeralda,
            TariAddressFeatures::create_one_sided_only(),
            Some(payment_id),
        )
        .map_err(|e| anyhow::Error::msg(format!("assembling pool address {i}: {e}")))?;
        pool.push(addr);
    }
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Seeds, gen_seed};

    /// Builds a `Seeds` whose four env-var names are unique per-test to avoid
    /// cross-test mutation races under cargo's default parallel runner.
    fn unique_seeds(suffix: &str) -> Seeds {
        Seeds {
            old: format!("WALLET_BENCHMARKS_TEST_SEED_OLD_{suffix}"),
            new: format!("WALLET_BENCHMARKS_TEST_SEED_NEW_{suffix}"),
            payment_processor: format!("WALLET_BENCHMARKS_TEST_SEED_PP_{suffix}"),
            wallet_password: format!("WALLET_BENCHMARKS_TEST_SEED_PW_{suffix}"),
        }
    }

    /// Mutate env via `set_var` / `remove_var`. Both calls are `unsafe` on
    /// Rust 1.84+; allow `unused_unsafe` for older toolchains.
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

    #[test]
    fn redacted_string_debug_and_display_are_redacted() {
        let s = RedactedString::new("super secret".to_string());
        assert_eq!(format!("{s:?}"), "[REDACTED]");
        assert_eq!(format!("{s}"), "[REDACTED]");
        assert_eq!(s.reveal(), "super secret");
    }

    #[test]
    fn seed_handle_reads_env_at_call_time() {
        let seeds = unique_seeds("CALL_TIME");
        let m1 = gen_seed().expect("first mnemonic");
        let m2 = gen_seed().expect("second mnemonic");
        set_env(&seeds.old, &m1);
        let handle = SeedHandle::new(&seeds);
        assert_eq!(handle.mnemonic_old().expect("read1").reveal(), m1);
        set_env(&seeds.old, &m2);
        assert_eq!(handle.mnemonic_old().expect("read2").reveal(), m2);
        unset_env(&seeds.old);
    }

    #[test]
    fn seed_handle_bails_on_missing_env_var() {
        let seeds = unique_seeds("MISSING");
        unset_env(&seeds.old);
        let handle = SeedHandle::new(&seeds);
        let err = handle.mnemonic_old().expect_err("missing env must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&seeds.old),
            "error should name the missing env var: {msg}",
        );
    }

    #[test]
    fn seed_handle_assert_distinct_passes_for_three_unique_seeds() {
        let seeds = unique_seeds("DISTINCT_OK");
        let m_old = gen_seed().expect("old");
        let m_new = gen_seed().expect("new");
        let m_pp = gen_seed().expect("pp");
        set_env(&seeds.old, &m_old);
        set_env(&seeds.new, &m_new);
        set_env(&seeds.payment_processor, &m_pp);
        let handle = SeedHandle::new(&seeds);
        handle
            .assert_distinct()
            .expect("three unique seeds should pass");
        unset_env(&seeds.old);
        unset_env(&seeds.new);
        unset_env(&seeds.payment_processor);
    }

    #[test]
    fn seed_handle_assert_distinct_bails_on_duplicate() {
        let seeds = unique_seeds("DISTINCT_DUP");
        let shared = gen_seed().expect("shared mnemonic");
        let other = gen_seed().expect("other mnemonic");
        set_env(&seeds.old, &shared);
        set_env(&seeds.new, &shared);
        set_env(&seeds.payment_processor, &other);
        let handle = SeedHandle::new(&seeds);
        let err = handle
            .assert_distinct()
            .expect_err("duplicate old/new must be rejected");
        let msg = format!("{err:#}");
        unset_env(&seeds.old);
        unset_env(&seeds.new);
        unset_env(&seeds.payment_processor);
        assert!(
            msg.contains("distinct"),
            "error should mention distinctness: {msg}",
        );
    }

    #[test]
    fn seed_handle_address_round_trips_through_base58() {
        let seeds = unique_seeds("ADDRESS");
        let m = gen_seed().expect("mnemonic");
        set_env(&seeds.old, &m);
        let handle = SeedHandle::new(&seeds);
        let addr = handle.address_old().expect("derive address_old");
        unset_env(&seeds.old);
        let base58 = addr.to_base58();
        let decoded = TariAddress::from_base58(&base58).expect("base58 round-trip");
        assert_eq!(decoded.to_base58(), base58);
    }

    #[test]
    fn seed_handle_address_is_deterministic_for_same_seed() {
        let seeds = unique_seeds("ADDRESS_DET");
        let m = gen_seed().expect("mnemonic");
        set_env(&seeds.old, &m);
        let handle = SeedHandle::new(&seeds);
        let a1 = handle.address_old().expect("first derive");
        let a2 = handle.address_old().expect("second derive");
        unset_env(&seeds.old);
        assert_eq!(a1.to_base58(), a2.to_base58());
    }

    #[test]
    fn derive_address_matches_print_address_for_same_seed() {
        // The shared-derivation contract: print_address (from lib.rs) and
        // SeedHandle::address_* must agree on the same input mnemonic.
        let m = gen_seed().expect("mnemonic");
        let direct = derive_address(&m).expect("derive_address direct");
        const ENV: &str = "WALLET_BENCHMARKS_TEST_SHARED_DERIVE";
        set_env(ENV, &m);
        let from_print = crate::print_address(ENV).expect("print_address path");
        unset_env(ENV);
        assert_eq!(direct.to_base58(), from_print);
    }
}
