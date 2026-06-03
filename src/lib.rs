//! Library entry point for the `wallet-benchmarks` harness.
//!
//! See `analysis/DESIGN.md §Workspace layout` for the module roster. Modules are
//! added in the order specified in `analysis/DESIGN_ADDENDUM.md §S4 Pre-flight
//! execution order`.

pub mod broadcast;
pub mod cli;
pub mod clock;
pub mod config;
pub mod env_capture;
pub mod guards;
pub mod modes;
pub mod pp_http_client;
pub mod pp_migrations;
pub mod result_profile;
pub mod sampler;
pub mod scenarios;
pub mod seed;
pub mod versions;
pub mod wallet_db;
pub mod wallet_lifecycle;

use anyhow::Context;
use tari_common_types::seeds::{
    cipher_seed::CipherSeed,
    mnemonic::{Mnemonic, MnemonicLanguage},
};

const LOG_TARGET: &str = "c::lib";

/// Generates a fresh 24-word Tari mnemonic.
///
/// Uses [`CipherSeed::random`] — Tari's canonical seed generator — and emits its
/// English mnemonic via the [`Mnemonic`] trait. The result is the operator-visible
/// mnemonic for `wallet-benchmarks gen-seed`; per DESIGN_ADDENDUM.md §S1 this
/// output bypasses the result-profile redaction denylist because the operator
/// explicitly asked for the phrase. Callers are responsible for not echoing it
/// into structured logs or the result profile.
pub fn gen_seed() -> anyhow::Result<String> {
    log::debug!(target: LOG_TARGET, "generating fresh CipherSeed mnemonic");
    let cipher_seed = CipherSeed::random();
    let seed_words = cipher_seed
        .to_mnemonic(MnemonicLanguage::English, None)
        .context("encoding CipherSeed as English mnemonic")?;
    Ok(seed_words.join(" ").reveal().to_string())
}

/// Derives the Esmeralda wallet address from the seed mnemonic held in the named
/// environment variable and returns it as a base58 string.
///
/// Delegates to [`seed::derive_address`] — the single shared site reused by
/// [`seed::SeedHandle::address_old`] / `_new` / `_payment_processor` and the
/// funding pre-flight in [`guards`]. Output uses base58 per DESIGN_ADDENDUM.md
/// §S2 so it can be piped straight into `minotari --network esmeralda
/// create-unsigned-transaction --recipient ...`.
pub fn print_address(seed_env_name: &str) -> anyhow::Result<String> {
    log::debug!(
        target: LOG_TARGET,
        "deriving address from seed env var {seed_env_name}",
    );
    let mnemonic = std::env::var(seed_env_name).with_context(|| {
        format!("reading seed mnemonic from env var ${seed_env_name} (set it to the 24-word Tari mnemonic)")
    })?;
    let address = seed::derive_address(&mnemonic)
        .with_context(|| format!("deriving address from ${seed_env_name}"))?;
    Ok(address.to_base58())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tari_common_types::{seeds::seed_words::SeedWords, tari_address::TariAddress};

    use super::*;

    /// Each test that exercises the env-var path uses a unique env var name to
    /// avoid cross-test mutation races under cargo's default parallel runner.
    const ROUNDTRIP_ENV: &str = "WALLET_BENCHMARKS_TEST_SEED_ROUNDTRIP";
    const DETERMINISM_ENV: &str = "WALLET_BENCHMARKS_TEST_SEED_DETERMINISM";

    #[test]
    fn gen_seed_emits_24_english_words() {
        let mnemonic = gen_seed().expect("gen_seed succeeds");
        let words: Vec<&str> = mnemonic.split(' ').collect();
        assert_eq!(words.len(), 24, "Tari mnemonic must be 24 words");
        for word in &words {
            assert!(!word.is_empty(), "no empty words in mnemonic");
            assert!(
                word.chars().all(|c| c.is_ascii_lowercase()),
                "english mnemonic words are lowercase ASCII: {word:?}",
            );
        }
    }

    #[test]
    fn gen_seed_parses_back_through_cipher_seed() {
        let mnemonic = gen_seed().expect("gen_seed succeeds");
        let seed_words = SeedWords::from_str(&mnemonic).expect("mnemonic splits into SeedWords");
        let reparsed = <CipherSeed as Mnemonic<CipherSeed>>::from_mnemonic(&seed_words, None)
            .expect("CipherSeed decodes its own mnemonic output");
        let reemitted = reparsed
            .to_mnemonic(MnemonicLanguage::English, None)
            .expect("re-encode succeeds");
        assert_eq!(reemitted.join(" ").reveal().as_str(), mnemonic);
    }

    #[test]
    fn gen_seed_is_random_across_invocations() {
        let a = gen_seed().expect("first gen_seed");
        let b = gen_seed().expect("second gen_seed");
        assert_ne!(a, b, "two CipherSeed::random invocations must differ");
    }

    #[test]
    fn print_address_round_trips_through_base58() {
        let mnemonic = gen_seed().expect("gen_seed succeeds");
        // SAFETY: each test uses a unique env-var name to avoid cross-test races
        // under cargo's default parallel runner. `set_var`/`remove_var` are
        // `unsafe` on Rust 1.84+; the `unused_unsafe` lint is allowed for older
        // toolchains.
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(ROUNDTRIP_ENV, &mnemonic);
        }
        let base58 = print_address(ROUNDTRIP_ENV).expect("print_address succeeds");
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(ROUNDTRIP_ENV);
        }
        let decoded = TariAddress::from_base58(&base58).expect("base58 decodes");
        assert_eq!(
            decoded.to_base58(),
            base58,
            "base58 round-trip must be stable",
        );
    }

    #[test]
    fn print_address_is_deterministic_for_a_given_seed() {
        let mnemonic = gen_seed().expect("gen_seed succeeds");
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(DETERMINISM_ENV, &mnemonic);
        }
        let first = print_address(DETERMINISM_ENV).expect("first print_address");
        let second = print_address(DETERMINISM_ENV).expect("second print_address");
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(DETERMINISM_ENV);
        }
        assert_eq!(
            first, second,
            "two derivations of the same seed must produce the same address",
        );
    }

    #[test]
    fn print_address_errors_when_env_var_missing() {
        const MISSING: &str = "WALLET_BENCHMARKS_NO_SUCH_SEED_ENV_FOR_TEST";
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(MISSING);
        }
        let err = print_address(MISSING).expect_err("missing env must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(MISSING),
            "error should name the missing env var: {msg}",
        );
    }

    #[test]
    fn print_address_errors_on_invalid_mnemonic() {
        const BAD: &str = "WALLET_BENCHMARKS_BAD_SEED_ENV_FOR_TEST";
        // Twenty-four real-looking but unchecksummed words: parses past the
        // SeedWords splitter but fails CipherSeed decoding.
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(
                BAD,
                "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon",
            );
        }
        let err = print_address(BAD).expect_err("bad mnemonic must error");
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(BAD);
        }
        let msg = format!("{err:#}");
        assert!(
            msg.contains("CipherSeed") || msg.contains("mnemonic"),
            "error should describe the decoding failure: {msg}",
        );
    }
}
