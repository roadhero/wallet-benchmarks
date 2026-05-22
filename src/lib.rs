//! Library entry point for the `wallet-benchmarks` harness.
//!
//! See `analysis/DESIGN.md §Workspace layout` for the module roster. Modules are
//! added in the order specified in `analysis/DESIGN_ADDENDUM.md §S4 Pre-flight
//! execution order`.

pub mod cli;
pub mod config;
pub mod guards;

use anyhow::{bail, Context};
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

/// Stub for the `print-address` subcommand. The real implementation lands in a
/// later commit; this placeholder lets the CLI shape parse and dispatch through
/// the same code path.
pub fn print_address(_seed_env_name: &str) -> anyhow::Result<String> {
    bail!("print-address not yet implemented");
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tari_common_types::seeds::seed_words::SeedWords;

    use super::*;

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
}
