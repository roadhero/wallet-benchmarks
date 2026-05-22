//! Dep-graph spike — wallet-benchmarks#1, M1.
//!
//! Proves the five hot Tari ecosystem dependencies resolve and compile against the
//! crates.io `5.3.1` publication of the Tari workspace (commit
//! `5d6ef11bb89caa34fe9ee676d608f273db90038d`, tag `v5.3.1`). See
//! `analysis/DESIGN_ADDENDUM.md §Dependency strategy — resolved` for the pin rationale
//! and `§M1` for the spike contract. This file is replaced by the real `main.rs` in the
//! module-implementation phase per `§S4`.

use minotari_node_wallet_client::http::Client as BaseNodeClient;
use tari_common::configuration::Network;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_transaction_components::{
    consensus::{ConsensusConstants, ConsensusConstantsBuilder},
    key_manager::{
        wallet_types::{SeedWordsWallet, WalletType},
        KeyManager,
    },
    offline_signing::{
        models::{PrepareOneSidedTransactionForSigningResult, SignedOneSidedTransactionResult},
        sign_locked_transaction,
    },
    TransactionBuilderError,
};
use url::Url;

fn main() -> anyhow::Result<()> {
    // Proof 1: Network::Esmeralda constructs.
    let network = Network::Esmeralda;
    println!("ok 1/5  tari_common::Network::Esmeralda = {:?}", network);

    // Proof 2: ConsensusConstantsBuilder::new(Esmeralda).build() returns ConsensusConstants.
    let _consensus_constants: ConsensusConstants = ConsensusConstantsBuilder::new(network).build();
    println!("ok 2/5  ConsensusConstantsBuilder::new(Esmeralda).build() -> ConsensusConstants");

    // Proof 3: KeyManager constructs from a WalletType::SeedWords reconstituted via the
    // CipherSeed::random() constructor — Tari's canonical seed generator. The seed is
    // ephemeral and never persisted; the goal is to prove the constructor chain links and
    // executes, not to exercise mnemonic parsing (which is module-implementation phase).
    let cipher_seed = CipherSeed::random();
    let seed_words_wallet =
        SeedWordsWallet::construct_new(cipher_seed).map_err(anyhow::Error::msg)?;
    let wallet_type = WalletType::SeedWords(seed_words_wallet);
    let _key_manager = KeyManager::new(wallet_type)?;
    println!("ok 3/5  KeyManager::new(WalletType::SeedWords(..)) constructed from CipherSeed::random()");

    // Proof 4: sign_locked_transaction resolves as a symbol. Per §M1 we do NOT call it
    // end-to-end (no real unsigned tx available); coercing it to a typed function pointer
    // forces the linker to resolve the symbol and the type checker to verify its signature
    // against DESIGN.md §Mode 2.
    let _sign_locked_fn: fn(
        &KeyManager,
        ConsensusConstants,
        Network,
        PrepareOneSidedTransactionForSigningResult,
    ) -> Result<SignedOneSidedTransactionResult, TransactionBuilderError> =
        sign_locked_transaction::<KeyManager>;
    println!("ok 4/5  sign_locked_transaction symbol resolves (signature checked, not invoked)");

    // Proof 5: minotari_node_wallet_client::http::Client::new constructs against Esmeralda RPC.
    let base_node_url = Url::parse("https://rpc.esmeralda.tari.com")?;
    let _client = BaseNodeClient::new(base_node_url.clone(), base_node_url);
    println!("ok 5/5  minotari_node_wallet_client::http::Client::new(esmeralda RPC) constructed");

    Ok(())
}
