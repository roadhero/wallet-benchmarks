//! Entry point for the `wallet-benchmarks` harness.
//!
//! Dispatches the three subcommands per `analysis/DESIGN_ADDENDUM.md §S1`:
//!
//! * `run` — full benchmark harness end-to-end (Esmeralda only, enforced by
//!   `guards::enforce_esmeralda`).
//! * `gen-seed` — emit a fresh 24-word Tari mnemonic.
//! * `print-address` — derive the wallet address from the seed mnemonic in
//!   the named env var.
//!
//! See `analysis/DESIGN.md §Workflow` for the run-loop shape and
//! `analysis/RESULT_PROFILE_SCHEMA.md §1-§5` for the output JSON shape.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use clap::Parser;
use minotari_node_wallet_client::{http::Client as BaseNodeHttpClient, BaseNodeWalletClient};
use url::Url;
use wallet_benchmarks::{
    cli::{Cli, Commands},
    clock::RealClock,
    config::Config,
    env_capture::{EnvCapture, LiveEnvCapture},
    gen_seed, guards,
    modes::{
        new_wallet::NewWallet, old_wallet::OldWallet, payment_processor::PaymentProcessor, Mode,
    },
    print_address,
    result_profile::{self, CellResult, Matrix},
    sampler::LiveSamplerFactory,
    scenarios::{self, RecipientStrategy, ScenarioCtx, ScenarioId, ScenarioInput, ScenarioOutcome},
    seed::{redact::RedactionDenylist, SeedHandle, SeedRole},
    versions::Versions,
    wallet_lifecycle::{
        balance_query::WalletGrpcBalanceQuery, console_wallet::ConsoleWalletLifecycle,
        HarnessDataDir,
    },
};

const LOG_TARGET: &str = "c::main";

fn main() -> ExitCode {
    env_logger::init();

    let cli = Cli::parse();
    let result = match cli.resolved_command() {
        Commands::Run {
            config,
            output,
            skip_funding_preflight,
        } => dispatch_run(config, output, skip_funding_preflight),
        Commands::GenSeed => dispatch_gen_seed(),
        Commands::PrintAddress { seed_env } => dispatch_print_address(&seed_env),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!(target: LOG_TARGET, "{e:#}");
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch_gen_seed() -> anyhow::Result<()> {
    let m = gen_seed().context("generating mnemonic")?;
    println!("{m}");
    Ok(())
}

fn dispatch_print_address(seed_env: &str) -> anyhow::Result<()> {
    let addr_b58 = print_address(seed_env).context("deriving address")?;
    println!("{addr_b58}");
    Ok(())
}

fn dispatch_run(
    config_path: PathBuf,
    output_path: PathBuf,
    skip_funding_preflight: bool,
) -> anyhow::Result<()> {
    // The harness body is fully async (mode dispatch, sampler tasks, gRPC
    // calls); main.rs constructs the tokio runtime and blocks on the entry
    // point so the rest of the codebase can stay async-first.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    runtime.block_on(run_harness_async(
        config_path,
        output_path,
        skip_funding_preflight,
    ))
}

async fn run_harness_async(
    config_path: PathBuf,
    output_path: PathBuf,
    skip_funding_preflight: bool,
) -> anyhow::Result<()> {
    // 1. Load config + run mainnet-protection guard.
    let config_owned: Config = wallet_benchmarks::config::load::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    guards::enforce_esmeralda(&config_owned).context("enforce_esmeralda pre-flight")?;
    let config = Arc::new(config_owned);

    // 2. Build SeedHandle + assert the three seeds are distinct.
    let seeds = Arc::new(SeedHandle::new(&config.seeds));
    seeds
        .assert_distinct()
        .context("assert_distinct pre-flight")?;

    // 3. Funding pre-flight (unless explicitly skipped).
    if !skip_funding_preflight {
        log::info!(
            target: LOG_TARGET,
            "running funding pre-flight (3 transient console_wallet spawns; ~90s)",
        );
        let bq = WalletGrpcBalanceQuery::new(Arc::clone(&config), Arc::clone(&seeds));
        guards::enforce_funding(&config, &seeds, &bq)
            .await
            .context("enforce_funding pre-flight")?;
    } else {
        log::warn!(
            target: LOG_TARGET,
            "--skip-funding-preflight is set; production runs MUST use the live pre-flight \
             (see analysis/PR_BODY_PLAN.md §Operator Setup)",
        );
    }

    // 4. Capture environment + probe versions + build sampler / redaction
    //    / clock once for the whole run.
    let env = LiveEnvCapture
        .capture(&config.base_node_url)
        .context("env_capture")?;
    let versions = probe_versions(&config);
    let sampler_factory = LiveSamplerFactory;
    let redaction = RedactionDenylist::init_from_env(&config.seeds);
    let clock = RealClock;

    // 5. Per-mode × per-scenario loop.
    let mut matrix = Matrix::new();
    for mode_role in [SeedRole::Old, SeedRole::New, SeedRole::Pp] {
        log::info!(target: LOG_TARGET, "running scenarios for mode {mode_role:?}");
        let mut mode = construct_mode(mode_role, &config, &seeds)?;
        let mut scenario_input = ScenarioInput {
            s5_seed_role_for_mode: Some(mode_role),
            ..ScenarioInput::default()
        };

        for scenario_id in ScenarioId::all() {
            log::info!(
                target: LOG_TARGET,
                "running {scenario_id} for mode {mode_role:?}",
            );

            // Tip queries only happen for send scenarios. Scans
            // (B0/S2/S3/S6/S7) supply intrinsic `h_tip_*` on their
            // Outcome — the writer prefers intrinsic over caller-
            // supplied, so the outer query would be pure overhead
            // (15 cells × 2 queries = 30 redundant RPC calls).
            let needs_outer_tip = matches!(
                scenario_id,
                ScenarioId::S0 | ScenarioId::S1 | ScenarioId::S4 | ScenarioId::S5,
            );

            // Pre-scenario tip query + wall-clock measurement bracket.
            // Tip query failures don't bail the scenario — emit None and
            // attach a per-cell note so the writer surfaces the gap
            // observably.
            let (tip_start, tip_query_note_start) = if needs_outer_tip {
                fetch_tip_height_observably(&config.base_node_url).await
            } else {
                (None, None)
            };

            let ctx = ScenarioCtx {
                config: &config,
                seeds: &seeds,
                redaction: &redaction,
                clock: &clock,
                recipients: RecipientStrategy::SelfAddress(mode_role),
                sampler_factory: Some(&sampler_factory),
            };
            let t0 = Instant::now();
            let outcome =
                scenarios::run_scenario(scenario_id, &ctx, mode.as_mut(), &scenario_input).await;
            let wall_clock_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);

            let (tip_end, tip_query_note_end) = if needs_outer_tip {
                fetch_tip_height_observably(&config.base_node_url).await
            } else {
                (None, None)
            };
            // Combine the two notes so any tip-query failure is surfaced
            // without dropping the other half's value.
            let tip_query_note = match (tip_query_note_start, tip_query_note_end) {
                (None, None) => None,
                (Some(a), None) => Some(format!("pre-scenario tip query: {a}")),
                (None, Some(b)) => Some(format!("post-scenario tip query: {b}")),
                (Some(a), Some(b)) => Some(format!(
                    "pre-scenario tip query: {a} | post-scenario tip query: {b}"
                )),
            };

            let (cell_result, fees) = match outcome {
                Ok(o) => {
                    update_scenario_input(&o, &mut scenario_input);
                    let fees = compute_fees_paid(&o, &config);
                    (CellResult::Outcome(Box::new(o)), fees)
                }
                Err(e) => {
                    log::warn!(
                        target: LOG_TARGET,
                        "{scenario_id} for {mode_role:?} returned Err: {e:#}",
                    );
                    (CellResult::Error(e), 0)
                }
            };
            matrix.record(
                mode_role,
                scenario_id,
                cell_result,
                wall_clock_ms,
                tip_start,
                tip_end,
                tip_query_note,
                fees,
            );
        }
    }

    // 6. Serialize via the result-profile writer.
    //    Confirmation-poll backfill for S4/S5 (t_confirm_ms population +
    //    derived stall_count adjustment) is deferred to Phase 4 — needs a
    //    WalletClient transaction-info query method that's a follow-up
    //    after the canonical baseline run validates the matrix shape. See
    //    analysis/PR_BODY_PLAN.md §Phase 4 confirmation-backfill gap.
    result_profile::write(&matrix, &config, &env, &versions, &redaction, &output_path)
        .context("writing result profile")?;

    println!("wrote {}", output_path.display());
    Ok(())
}

/// Construct the [`Mode`] for a given role.
///
/// Mode 1 (Old) wraps a [`ConsoleWalletLifecycle`] — spawn happens lazily
/// inside the lifecycle's `WalletLifecycle::spawn` trait method, invoked
/// by the scenarios as needed. Drop on the returned trait object tears
/// the lifecycle down (SIGTERM grace + SIGKILL).
///
/// Mode 2/3 (New / Pp) are stateless subprocess invokers — each
/// `send_*` / `scan_*` call spawns its own `minotari` subprocess via
/// `create_sign_and_submit`.
fn construct_mode(
    role: SeedRole,
    config: &Arc<Config>,
    seeds: &Arc<SeedHandle>,
) -> anyhow::Result<Box<dyn Mode>> {
    let run_id = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    let mode_name = match role {
        SeedRole::Old => "old_wallet",
        SeedRole::New => "new_wallet",
        SeedRole::Pp => "payment_processor",
    };
    let data_dir = HarnessDataDir::new(&run_id, mode_name)?;

    let mode: Box<dyn Mode> = match role {
        SeedRole::Old => {
            let lifecycle = ConsoleWalletLifecycle::new(config, seeds, data_dir)?;
            Box::new(OldWallet::new(lifecycle))
        }
        SeedRole::New => Box::new(NewWallet::new(
            (**config).clone(),
            (**seeds).clone(),
            data_dir,
        )),
        SeedRole::Pp => Box::new(PaymentProcessor::new(
            (**config).clone(),
            (**seeds).clone(),
            data_dir,
        )),
    };
    Ok(mode)
}

/// Best-effort version probe. Failures degrade to
/// `Versions::default()` rather than aborting the run — the
/// result-profile records absence rather than failing the whole
/// matrix per `analysis/DESIGN_ADDENDUM.md §3`.
fn probe_versions(config: &Config) -> Versions {
    let mut v = Versions::default();
    if let Some(p) = config.minotari_console_wallet_path.as_ref() {
        v.minotari_console_wallet =
            wallet_benchmarks::versions::probe_binary(p).unwrap_or_default();
    }
    if let Some(p) = config.minotari_path.as_ref() {
        v.minotari_cli = wallet_benchmarks::versions::probe_binary(p).unwrap_or_default();
    }
    // base_node falls back to the pinned tag/commit in Versions::default
    // when no live endpoint is queried — see versions::Versions doc.
    v
}

/// Fetch the base-node tip height observably: on success returns
/// `(Some(h), None)`; on failure returns `(None, Some(diagnostic))` so
/// the writer can attach a per-cell `tip_query_note` rather than
/// silently emitting `null`. A failed tip query never aborts a
/// scenario — the wallet itself records its own `h_tip_*` for scan
/// scenarios; this helper is the fallback for send scenarios that
/// don't carry tip data on their outcome.
async fn fetch_tip_height_observably(base_node_url: &Url) -> (Option<u64>, Option<String>) {
    let client = BaseNodeHttpClient::new(base_node_url.clone(), base_node_url.clone());
    match client.get_tip_info().await {
        Ok(resp) => match resp.metadata {
            Some(m) => (Some(m.best_block_height()), None),
            None => (None, Some("get_tip_info returned no metadata".to_string())),
        },
        Err(e) => (None, Some(format!("get_tip_info failed: {e:#}"))),
    }
}

/// Compute `fees_paid_microtari` for the cell envelope. Read from
/// `tx_record.fee_microtari` for scenarios that carry tx records
/// (S0/S1/S5); S4's `TaskOutcome` doesn't track per-task fees, so use
/// the documented formula `config.fee_rate × 35 × success_count` where
/// 35 is the standard 1-input-1-output transaction kernel weight per
/// DESIGN.md. Scan scenarios always return 0.
fn compute_fees_paid(outcome: &ScenarioOutcome, config: &Config) -> u64 {
    use wallet_benchmarks::scenarios::ScenarioOutcome as S;
    const S4_KERNEL_WEIGHT: u64 = 35;
    match outcome {
        S::B0(_) | S::S2(_) | S::S3(_) | S::S6(_) | S::S7(_) => 0,
        S::S0(s0) => s0.tx_record.fee_microtari,
        S::S1(s1) => s1
            .rounds
            .iter()
            .flat_map(|r| r.tx_records.iter())
            .map(|t| t.fee_microtari)
            .sum(),
        S::S4(s4) => {
            // S4's TaskOutcome doesn't carry a fee field; derive from the
            // documented kernel-weight formula: each Accepted task pays
            // `kernel_weight × config.fee_rate` microTari.
            let successes: u64 = s4
                .sub_blocks
                .iter()
                .flat_map(|sb| sb.tx_records.iter())
                .filter(|t| {
                    matches!(
                        t.broadcast_outcome,
                        wallet_benchmarks::scenarios::BroadcastOutcome::Accepted
                    )
                })
                .count() as u64;
            successes
                .saturating_mul(S4_KERNEL_WEIGHT)
                .saturating_mul(config.fee_rate)
        }
        S::S5(s5) => {
            let ind: u64 = s5
                .arms
                .individual
                .tx_records
                .iter()
                .map(|t| t.fee_microtari)
                .sum();
            let bat: u64 = s5
                .arms
                .batch
                .tx_records
                .iter()
                .map(|t| t.fee_microtari)
                .sum();
            ind + bat
        }
    }
}

/// Thread the just-completed scenario's results into `ScenarioInput` so
/// downstream scenarios in the same mode's loop pick up derived values
/// (S0's `h_birth` → S3/S7; S1's `outputs_found` → S2; S5's
/// `outputs_found` → S6/S7).
fn update_scenario_input(outcome: &ScenarioOutcome, input: &mut ScenarioInput) {
    match outcome {
        ScenarioOutcome::S0(_s0) => {
            // S0 records a tx_record; the harness doesn't yet derive an
            // h_birth value from S0 because the pinned client doesn't
            // expose a "block height of last accepted tx" lookup.
            // h_birth_s3 / s7_h_birth stay at the operator-supplied (or
            // default-zero) value for now. Tracked in
            // analysis/PR_BODY_PLAN.md §Phase 4 h_birth derivation.
        }
        ScenarioOutcome::S1(s1) => {
            // The chain's final UTXO count after S1's 7 rounds is the
            // expected scan target for S2 / S3. Derive from the success
            // count (the schema reads expected_outputs as "scan should
            // rediscover this many").
            input.expected_outputs_s2 = Some(s1.success_count);
        }
        ScenarioOutcome::S5(s5) => {
            // After S5's send-volume, the wallet's net output set is S1's
            // net + S5's per-arm tx_records (only successful sends mature
            // into spendable UTXOs).
            let s5_successes = s5.success_count;
            input.s6_expected_outputs = input.expected_outputs_s2.map(|p| p + s5_successes);
            input.s7_expected_outputs = input.s6_expected_outputs;
        }
        _ => {}
    }
}
