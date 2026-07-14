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
        UnsupportedOperation,
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
    // 1. Load config + run mainnet-protection guard + cross-field validation.
    let config_owned: Config = wallet_benchmarks::config::load::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    guards::enforce_esmeralda(&config_owned).context("enforce_esmeralda pre-flight")?;
    // Mode 3 binary paths + bench-account env vars are validated here so a
    // misconfiguration surfaces before the per-mode loop spawns anything.
    // See analysis/specs/MODE_3_REWORK_SPEC.md §12 startup-validation list.
    config_owned.validate().context("Config::validate")?;
    let config = Arc::new(config_owned);

    // 2. Build SeedHandle + assert the active seeds are distinct. The PP
    // seed is exempt when Mode 3 is disabled — same rationale as the
    // funding pre-flight exemption in guards.rs.
    let seeds = Arc::new(SeedHandle::new(&config.seeds));
    seeds
        .assert_distinct(config.mode_3.is_some())
        .context("assert_distinct pre-flight")?;

    // 3. Funding pre-flight (unless explicitly skipped).
    if !skip_funding_preflight {
        log::info!(
            target: LOG_TARGET,
            "running funding pre-flight (3 transient console_wallet spawns; ~90s)",
        );
        let bq = WalletGrpcBalanceQuery::new(Arc::clone(&config), Arc::clone(&seeds));
        // Mode 3 (swe-review C4): when configured, route the SeedRole::Pp
        // arm through the PR daemon's HTTP balance endpoint instead of
        // the mnemonic-derived console_wallet path. The PR daemon isn't
        // yet spawned at pre-flight time — the call will return a
        // connection error and the Pp arm will be logged + skipped per
        // the warn-path inside `enforce_funding`.
        let pr_bq = config.mode_3.as_ref().map(|m| {
            wallet_benchmarks::wallet_lifecycle::pr_balance_query::PrBalanceQuery::new(
                format!("http://127.0.0.1:{}", m.pr_port),
                "default",
            )
        });
        guards::enforce_funding(&config, &seeds, &bq, pr_bq.as_ref())
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
    // Write #0: an all-null skeleton with run_complete=false, so from this
    // point on the operator always has a valid profile on disk no matter
    // where the run dies. Best-effort like the per-mode checkpoints; a
    // failure here (e.g. unwritable output dir) is worth failing fast on,
    // since the final write would fail the same way hours later.
    result_profile::write(
        &matrix,
        &config,
        &env,
        &versions,
        &redaction,
        &output_path,
        false,
    )
    .context("initial result-profile skeleton write (is the output path writable?)")?;
    for mode_role in [SeedRole::Old, SeedRole::New, SeedRole::Pp] {
        log::info!(target: LOG_TARGET, "running scenarios for mode {mode_role:?}");
        // Mode 3 is optional: an absent [mode_3] block disables the mode.
        // Record its nine cells as skipped (null in the profile) and move
        // on, instead of failing the whole run at mode construction, which
        // used to discard the completed Mode 1/2 results (observed live:
        // a ~7 h operator run lost to exactly this path).
        if mode_role == SeedRole::Pp && config.mode_3.is_none() {
            log::info!(
                target: LOG_TARGET,
                "Mode 3 (payment_processor) disabled: no [mode_3] block in the config; \
                 recording all nine cells as skipped",
            );
            record_mode3_skipped(&mut matrix);
            continue;
        }
        // Build the mode. Mode 3 returns a concretely-typed PaymentProcessor
        // (carried inside a generic ModeHandle) so the run loop can call
        // start_external_services + shutdown without downcasting through
        // the Mode trait object. Construction failure must not abort the
        // run (that used to skip the writer and discard completed modes):
        // record this mode's cells as errors and continue.
        let mut mode_handle = match construct_mode(mode_role, &config, &seeds) {
            Ok(h) => h,
            Err(e) => {
                record_mode_error(&mut matrix, mode_role, "mode construction failed", &e);
                continue;
            }
        };
        // Mode 3 needs the PR + PP child processes spawned before the
        // scenario loop runs. PaymentProcessor::start_external_services
        // boots both lifecycles and waits for their HTTP readiness probes
        // per analysis/specs/MODE_3_REWORK_SPEC.md §2 step 4.
        if let ModeHandle::Mode3(pp) = &mut mode_handle {
            if let Err(e) = pp
                .start_external_services()
                .await
                .context("Mode 3 start_external_services")
            {
                // A mode whose external services cannot start must not
                // destroy the other modes' results: record every cell of
                // this mode as an error carrying the startup failure and
                // move on to the summary + writer. Observed live: a PP
                // daemon that exits during its readiness probe used to
                // abort the whole run here, discarding the completed
                // old_wallet and new_wallet matrices.
                record_mode_error(
                    &mut matrix,
                    mode_role,
                    "mode external services failed to start",
                    &e,
                );
                continue;
            }
        }
        // The S3/S7 rescan birthday is derived from wall-clock alone (the
        // funding lands the same day the run starts), so it is populated up
        // front rather than threaded through S0's outcome: an S0 error must
        // not cascade into S3/S7 "requires ScenarioInput::h_birth_s3" bails
        // (observed live: the maintainer's s0+s3+s7 err trio, 2026-07-14).
        let run_birthday = scenarios::s0_funding_birthday(chrono::Utc::now().timestamp());
        let mut scenario_input = ScenarioInput {
            s5_seed_role_for_mode: Some(mode_role),
            h_birth_s3: Some(run_birthday),
            s7_h_birth: Some(run_birthday),
            ..ScenarioInput::default()
        };

        for scenario_id in ScenarioId::all() {
            log::info!(
                target: LOG_TARGET,
                "running {scenario_id} for mode {mode_role:?}",
            );
            // Terminal feedback (stdout, independent of RUST_LOG). A
            // canonical baseline takes 3-5 hours; per-scenario start/done
            // lines let the operator see the harness is still alive
            // without paging through structured log output.
            println!(
                "[{}] mode={} scenario={}  start",
                chrono::Local::now().format("%H:%M:%S"),
                mode_name(mode_role),
                scenario_id,
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
            let outcome = scenarios::run_scenario(
                scenario_id,
                &ctx,
                mode_handle.as_mode_mut(),
                &scenario_input,
            )
            .await;
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
                    scenarios::update_scenario_input(
                        &o,
                        &mut scenario_input,
                        chrono::Utc::now().timestamp(),
                    );
                    let fees = compute_fees_paid(&o, &config);
                    (CellResult::Outcome(Box::new(o)), fees)
                }
                Err(e) => {
                    // Mode 3's scan-shaped methods return UnsupportedOperation
                    // (see analysis/specs/MODE_3_REWORK_SPEC.md §7); the
                    // runner records the cell as NotRun rather than Error so
                    // the result-profile emits null (not a failure) for the
                    // skipped scenario. Mode 1's S5-batch-arm uses the same
                    // precedent via arms.batch.applies = false.
                    if let Some(uo) = e.downcast_ref::<UnsupportedOperation>() {
                        log::info!(
                            target: LOG_TARGET,
                            "{scenario_id} for {mode_role:?} skipped: {} (op={}, mode={})",
                            uo.reason, uo.op, uo.mode,
                        );
                        (CellResult::NotRun, 0)
                    } else {
                        log::warn!(
                            target: LOG_TARGET,
                            "{scenario_id} for {mode_role:?} returned Err: {e:#}",
                        );
                        (CellResult::Error(e), 0)
                    }
                }
            };
            // Terminal feedback at scenario completion. Peek at
            // `cell_result` before it's moved into matrix.record.
            let (status, tx_count) = match &cell_result {
                CellResult::Outcome(o) => ("ok", count_txs(o.as_ref())),
                CellResult::NotRun => ("skipped", 0),
                CellResult::Error(_) => ("err", 0),
            };
            println!(
                "[{}] mode={} scenario={}  done   tx_count={tx_count} elapsed={:.1}s status={status}",
                chrono::Local::now().format("%H:%M:%S"),
                mode_name(mode_role),
                scenario_id,
                wall_clock_ms as f64 / 1000.0,
            );
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
        // Mode 3 shutdown: poll terminal state, SIGTERM the children, then
        // proceed. Best-effort — see analysis/specs/MODE_3_REWORK_SPEC.md §9.
        if let ModeHandle::Mode3(mut pp) = mode_handle {
            if let Err(e) = pp.shutdown().await {
                log::warn!(
                    target: LOG_TARGET,
                    "Mode 3 shutdown returned an error: {e:#} (proceeding)",
                );
            }
        }
        // Partial checkpoint: persist everything recorded so far so a
        // failure or kill in a later mode cannot discard this mode's
        // results. Best-effort: a failed partial write must not abort the
        // run (the final write below is the authoritative one and does
        // propagate its error).
        if let Err(e) = result_profile::write(
            &matrix,
            &config,
            &env,
            &versions,
            &redaction,
            &output_path,
            false,
        ) {
            log::warn!(
                target: LOG_TARGET,
                "partial result-profile write after mode {mode_role:?} failed: {e:#} (continuing)",
            );
        }
    }

    // 5b. Print the operator-facing summary table to stdout before
    //     serializing the JSON. The table assembles from `matrix`,
    //     which is the same source the writer reads from.
    print_summary_table(&matrix);

    // 6. Serialize via the result-profile writer.
    //    Confirmation-poll backfill for S4/S5 (t_confirm_ms population +
    //    derived stall_count adjustment) is deferred to Phase 4 — needs a
    //    WalletClient transaction-info query method that's a follow-up
    //    after the canonical baseline run validates the matrix shape. See
    //    analysis/PR_BODY_PLAN.md §Phase 4 confirmation-backfill gap.
    result_profile::write(
        &matrix,
        &config,
        &env,
        &versions,
        &redaction,
        &output_path,
        true,
    )
    .context("writing result profile")?;

    println!("wrote {}", output_path.display());
    Ok(())
}

/// Owning wrapper for the per-mode handle the run loop holds.
///
/// Mode 1/2 are pure trait-object wrappers — the per-mode lifecycle is
/// either internal to the boxed Mode (Mode 1's `ConsoleWalletLifecycle`)
/// or absent (Mode 2 is stateless). Mode 3 owns two child processes that
/// must be explicitly spawned before the scenario loop and torn down
/// after; the run loop calls
/// [`PaymentProcessor::start_external_services`] +
/// [`PaymentProcessor::shutdown`] through this enum's `Mode3` arm without
/// downcasting through the `Mode` trait object.
enum ModeHandle {
    Mode1(Box<OldWallet>),
    Mode2(Box<NewWallet>),
    Mode3(Box<PaymentProcessor>),
}

impl ModeHandle {
    /// Borrow as a `&mut dyn Mode` for `scenarios::run_scenario`.
    fn as_mode_mut(&mut self) -> &mut dyn Mode {
        match self {
            Self::Mode1(m) => m.as_mut(),
            Self::Mode2(m) => m.as_mut(),
            Self::Mode3(m) => m.as_mut(),
        }
    }
}

/// Construct the per-mode handle for a given role.
///
/// Mode 1 (Old) wraps a [`ConsoleWalletLifecycle`] — spawn happens lazily
/// inside the lifecycle's `WalletLifecycle::spawn` trait method, invoked
/// by the scenarios as needed. Drop on the returned trait object tears
/// the lifecycle down (SIGTERM grace + SIGKILL).
///
/// Mode 2 (New) is a stateless subprocess invoker — each
/// `send_*` / `scan_*` call spawns its own `minotari` subprocess via
/// `create_sign_and_submit`.
///
/// Mode 3 (Pp) spawns two child processes (PR + PP) that the run loop
/// brings up via `PaymentProcessor::start_external_services` after this
/// function returns and tears down via `PaymentProcessor::shutdown` after
/// the scenario loop completes.
/// Human-readable mode name for stdout progress lines. The
/// `seed_label_for_metric` already exists for result-profile JSON keys,
/// but stays internal; this is the stdout-facing label.
fn mode_name(role: SeedRole) -> &'static str {
    match role {
        SeedRole::Old => "old_wallet",
        SeedRole::New => "new_wallet",
        SeedRole::Pp => "payment_processor",
    }
}

/// Per-scenario transaction count for stdout progress lines. Maps the
/// `ScenarioOutcome` variant to "what the operator naturally calls the
/// tx count for this scenario": for send scenarios the success count;
/// for scan-only scenarios 0 (no txs sent). Best-effort summary, not a
/// load-bearing value — the canonical numbers live in the result
/// profile.
/// Record every cell of `mode_role` as an error carrying `why` + the source
/// error, print the per-scenario `status=err` progress lines, and log once.
/// Shared by the two mode-level failure classes (construction failure,
/// external-services startup failure) so neither aborts the run: aborting
/// used to skip the writer and discard the other modes' completed results.
fn record_mode_error(matrix: &mut Matrix, mode_role: SeedRole, why: &str, e: &anyhow::Error) {
    log::error!(
        target: LOG_TARGET,
        "mode {mode_role:?}: {why}; recording all cells for this mode as errors and \
         continuing: {e:#}",
    );
    for scenario_id in ScenarioId::all() {
        println!(
            "[{}] mode={} scenario={}  done   tx_count=0 elapsed=0.0s status=err",
            chrono::Local::now().format("%H:%M:%S"),
            mode_name(mode_role),
            scenario_id,
        );
        matrix.record(
            mode_role,
            scenario_id,
            CellResult::Error(anyhow::anyhow!("{why}: {e:#}")),
            0,
            None,
            None,
            Some(format!("mode skipped: {why}")),
            0,
        );
    }
}

/// Record all nine Mode 3 cells as `NotRun` (the profile's skipped/null
/// semantics) when the operator runs without a `[mode_3]` block. Prints the
/// same per-scenario progress lines the run loop emits so the operator sees
/// the skips inline. Mirrors the per-scenario `UnsupportedOperation` skip
/// path's recording shape (zero wall clock, no tip queries, zero fees).
fn record_mode3_skipped(matrix: &mut Matrix) {
    for scenario_id in ScenarioId::all() {
        println!(
            "[{}] mode={} scenario={}  done   tx_count=0 elapsed=0.0s status=skipped",
            chrono::Local::now().format("%H:%M:%S"),
            mode_name(SeedRole::Pp),
            scenario_id,
        );
        matrix.record(
            SeedRole::Pp,
            scenario_id,
            CellResult::NotRun,
            0,
            None,
            None,
            None,
            0,
        );
    }
}

fn count_txs(outcome: &ScenarioOutcome) -> u64 {
    use wallet_benchmarks::scenarios::ScenarioOutcome as S;
    match outcome {
        S::B0(_) | S::S2(_) | S::S3(_) | S::S6(_) | S::S7(_) => 0,
        S::S0(_) => 1,
        S::S1(s1) => s1.success_count,
        S::S4(s4) => {
            // SubBlockOutcome carries `n_concurrent` (u32) and
            // `success_rate` (f64 in [0,1]); reconstruct the success count
            // per sub-block and sum.
            s4.sub_blocks
                .iter()
                .map(|b| (f64::from(b.n_concurrent) * b.success_rate).round() as u64)
                .sum()
        }
        S::S5(s5) => s5.success_count,
    }
}

/// Final summary table printed to stdout after the per-mode × per-scenario
/// loop completes, before the result-profile JSON is written. Rows = mode,
/// columns = scenario. Each cell shows status + tx_count.
fn print_summary_table(matrix: &Matrix) {
    println!();
    println!("=== run summary ===");
    let scenarios = ScenarioId::all();
    let modes = [SeedRole::Old, SeedRole::New, SeedRole::Pp];
    // Header.
    print!("{:<20}", "mode\\scenario");
    for sid in &scenarios {
        print!("  {:<12}", format!("{sid}"));
    }
    println!();
    // Body.
    for role in modes {
        print!("{:<20}", mode_name(role));
        for sid in &scenarios {
            let cell = match matrix.cells.get(&(role, *sid)) {
                Some(c) => match &c.result {
                    CellResult::Outcome(o) => format!("ok ({})", count_txs(o.as_ref())),
                    CellResult::NotRun => "skipped".to_string(),
                    CellResult::Error(_) => "err".to_string(),
                },
                None => "n/a".to_string(),
            };
            print!("  {:<12}", cell);
        }
        println!();
    }
    println!();
}

fn construct_mode(
    role: SeedRole,
    config: &Arc<Config>,
    seeds: &Arc<SeedHandle>,
) -> anyhow::Result<ModeHandle> {
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

    let handle = match role {
        SeedRole::Old => {
            let lifecycle = ConsoleWalletLifecycle::new(config, seeds, data_dir)?;
            ModeHandle::Mode1(Box::new(OldWallet::new(lifecycle)))
        }
        SeedRole::New => ModeHandle::Mode2(Box::new(NewWallet::new(
            (**config).clone(),
            (**seeds).clone(),
            data_dir,
        ))),
        SeedRole::Pp => {
            // Mode 3 needs a second data dir for the PR daemon's view-key
            // wallet — distinct from PP's so sqlite locks cannot collide
            // (spec §14 failure mode #6).
            let pr_data_dir = HarnessDataDir::new(&run_id, "payment_processor_pr")?;
            ModeHandle::Mode3(Box::new(PaymentProcessor::new(
                (**config).clone(),
                (**seeds).clone(),
                data_dir,
                pr_data_dir,
            )?))
        }
    };
    Ok(handle)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_mode3_records_nine_skipped_cells() {
        // Defect C acceptance: the mode-level skip must cover every
        // scenario cell with NotRun so the writer emits null (the same
        // shape the per-scenario skip path produces), never an error and
        // never a missing cell.
        let mut matrix = Matrix::new();
        record_mode3_skipped(&mut matrix);
        let ids = ScenarioId::all();
        assert_eq!(ids.len(), 9, "scenario roster is the 9-cell matrix");
        for sid in ids {
            let entry = matrix
                .get(SeedRole::Pp, sid)
                .unwrap_or_else(|| panic!("cell {sid} must be recorded"));
            assert!(
                matches!(entry.result, CellResult::NotRun),
                "cell {sid} must be NotRun",
            );
            assert_eq!(entry.wall_clock_ms, 0);
            assert_eq!(entry.fees_paid_microtari, 0);
        }
    }
}
