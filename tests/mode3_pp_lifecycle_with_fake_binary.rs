//! Mode 3 PpLifecycle end-to-end against a fake binary.
//!
//! Per `analysis/specs/MODE_3_REWORK_SPEC.md §13` integration list: spawns
//! `PpLifecycle` against `tests/fixtures/fake_pp.sh`, verifies the spawn
//! produces a PID, the captured log file under the data dir contains the
//! fake's startup banner, and the teardown SIGTERM-grace-then-SIGKILL
//! cascade returns cleanly.
//!
//! The fake never binds a TCP port, so `PpLifecycle::spawn` (which includes
//! the readiness probe) will time out — this is by design. The test calls
//! `spawn().await.expect_err()` and then validates the post-spawn
//! observable state (Child PID, stdio capture, teardown idempotency).

use std::path::PathBuf;
use std::time::Duration;

use tokio::process::{Child, Command};

use wallet_benchmarks::{
    config::{
        Config, Mode3Account, Mode3Accounts, Mode3Config, Seeds, WorkerSleepOverrides,
    },
    gen_seed,
    seed::SeedHandle,
    wallet_lifecycle::{pp_lifecycle::PpLifecycle, HarnessDataDir},
};

fn fake_pp_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_pp.sh")
}

fn fake_minotari_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_minotari.sh")
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

/// Allocate a localhost port via ephemeral OS allocation. Same pattern
/// as the in-tree `ConsoleWalletLifecycle::allocate_port`; kept inline
/// here so the integration test doesn't pull in private helpers.
fn allocate_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("local_addr").port()
}

/// Build a `PpLifecycle` against the fake PP binary. Returns the lifecycle
/// plus the list of env-var names the caller must `unset_env` to clean up.
fn build_pp_lifecycle(suffix: &str) -> (PpLifecycle, Vec<String>) {
    let seeds_cfg = Seeds {
        old: format!("WB_PPL_OLD_{suffix}"),
        new: format!("WB_PPL_NEW_{suffix}"),
        payment_processor: format!("WB_PPL_PP_{suffix}"),
        wallet_password: format!("WB_PPL_PW_{suffix}"),
    };
    let view_env = format!("WB_PPL_VIEW_{suffix}");
    let spend_env = format!("WB_PPL_SPEND_{suffix}");
    let m_old = gen_seed().expect("m_old");
    let m_new = gen_seed().expect("m_new");
    let m_pp = gen_seed().expect("m_pp");
    set_env(&seeds_cfg.old, &m_old);
    set_env(&seeds_cfg.new, &m_new);
    set_env(&seeds_cfg.payment_processor, &m_pp);
    set_env(&seeds_cfg.wallet_password, "test-password");
    set_env(
        &view_env,
        "572a5fb63972da84aeec33071d13074e244d80c52be842ab5b0859ef4b4db00a",
    );
    set_env(
        &spend_env,
        "40e65c9bbf4592bc995c421108c01a5d7c9f9b2239569757895134549cef371f",
    );
    let port = allocate_port();
    let cfg = Config {
        seeds: seeds_cfg.clone(),
        mode_3: Some(Mode3Config {
            pp_binary_path: fake_pp_path(),
            minotari_binary_path: fake_minotari_path(),
            api_port: port,
            pr_port: allocate_port(),
            terminal_state_poll_timeout_secs: 1,
            worker_sleep_overrides: WorkerSleepOverrides::default(),
            accounts: Mode3Accounts {
                bench: Mode3Account {
                    view_key_env: view_env.clone(),
                    public_spend_key_env: spend_env.clone(),
                },
            },
        }),
        ..Config::default()
    };
    let seeds = SeedHandle::new(&seeds_cfg);
    let dir =
        HarnessDataDir::new(&format!("int-pp-{suffix}"), "payment_processor").expect("data dir");
    let life = PpLifecycle::new(&cfg, &seeds, dir).expect("lifecycle ctor");
    (
        life,
        vec![
            seeds_cfg.old,
            seeds_cfg.new,
            seeds_cfg.payment_processor,
            seeds_cfg.wallet_password,
            view_env,
            spend_env,
        ],
    )
}

fn teardown(envs: &[String]) {
    for e in envs {
        unset_env(e);
    }
}

/// Spawn the fake PP directly via `tokio::process::Command`, mirroring
/// PpLifecycle's spawn shape minus the readiness probe. Used by the
/// log-capture and teardown tests so they don't pay the 30s readiness
/// deadline cost.
async fn spawn_fake_pp_direct(data_dir: &std::path::Path) -> Child {
    let logs = data_dir.join("logs");
    std::fs::create_dir_all(&logs).expect("logs dir");
    let log_path = logs.join("pp.log");
    let stdout = std::fs::File::create(&log_path).expect("create pp.log");
    let stderr = stdout.try_clone().expect("clone log fd");
    Command::new(fake_pp_path())
        .current_dir(data_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr))
        .kill_on_drop(true)
        .spawn()
        .expect("spawn fake_pp.sh")
}

#[tokio::test]
async fn fake_pp_logs_startup_banner_to_per_run_log_file() {
    // Spawn the fake directly so we don't pay the 30s readiness deadline.
    // Verify the captured log file contains the script's stderr banner.
    let dir = HarnessDataDir::new("int-pp-banner", "payment_processor").expect("data dir");
    let log_path = dir.path().join("logs").join("pp.log");
    let mut child = spawn_fake_pp_direct(dir.path()).await;
    // Wait briefly for the banner to flush. Poll up to 2s.
    let mut banner_seen = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if log_path.exists() {
            let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
            if contents.contains("fake_pp.sh: spawned") {
                banner_seen = true;
                break;
            }
        }
    }
    assert!(
        banner_seen,
        "fake_pp.sh stderr banner must land in {} within 2s",
        log_path.display(),
    );
    // Cleanup.
    child.start_kill().ok();
    let _ = child.wait().await;
}

#[tokio::test]
async fn pp_lifecycle_spawn_against_fake_times_out_then_teardown_is_clean() {
    // End-to-end: PpLifecycle::spawn -> readiness probe times out
    // (fake never binds) -> teardown SIGTERMs the still-running fake,
    // which traps and exits 0. The teardown must return Ok without
    // escalating to SIGKILL.
    let (mut life, envs) = build_pp_lifecycle("e2e_timeout");
    let err = life
        .spawn()
        .await
        .expect_err("readiness probe must time out");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("did not become ready"),
        "spawn error must surface the readiness deadline: {msg}",
    );
    // Lifecycle still owns the child; teardown must reap it gracefully.
    let start = std::time::Instant::now();
    life.teardown().await.expect("teardown ok after timeout");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "teardown must succeed inside the 5s SIGTERM grace; took {:?}",
        start.elapsed(),
    );
    assert!(!life.is_alive(), "lifecycle must not be alive after teardown");
    // Idempotency: second teardown is a no-op.
    life.teardown().await.expect("idempotent teardown");
    teardown(&envs);
}

#[tokio::test]
async fn pp_lifecycle_drop_without_teardown_relies_on_kill_on_drop() {
    // Spawn a real fake child directly into a PpLifecycle-shaped layout
    // (data dir + logs/pp.log) and let Drop reap it. We don't go through
    // PpLifecycle::spawn here because the readiness deadline (30s) would
    // dominate the test; instead the test exercises the "operator forgot
    // to call teardown" path documented at failure mode #12 in spec §14.
    let dir = HarnessDataDir::new("int-pp-drop", "payment_processor").expect("data dir");
    let child = spawn_fake_pp_direct(dir.path()).await;
    let pid = child.id().expect("PID");
    drop(child);
    // Wait for kill_on_drop's SIGKILL to reap the child. Up to 2s.
    let mut alive = true;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        #[cfg(unix)]
        {
            use nix::sys::signal::kill as nix_kill;
            use nix::unistd::Pid;
            let pid_i32 = pid as i32;
            if nix_kill(Pid::from_raw(pid_i32), None).is_err() {
                alive = false;
                break;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            alive = false;
            break;
        }
    }
    assert!(
        !alive,
        "child PID {pid} must be reaped after Drop (kill_on_drop)",
    );
}
