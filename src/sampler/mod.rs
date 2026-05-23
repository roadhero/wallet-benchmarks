//! Per-PID resource sampler — drives `peak_rss_bytes` / `peak_cpu_pct` on
//! every scenario `*Outcome` struct (B0/S0/S1/S2/S3/S4/S5/S6/S7).
//!
//! Per `analysis/RESULT_PROFILE_SCHEMA.md` lines 126-127 (and the mirror
//! lines under §S2/§S3/§S5/§S6/§S7), every scenario records two
//! resource-usage peaks for its run:
//!
//!   * `peak_rss_bytes` — resident set size in bytes, max across samples.
//!     `null` only when no sample was successfully collected (e.g. the
//!     sampled PID exited before the first tick).
//!   * `peak_cpu_pct` — 0..100 across all cores; max across consecutive
//!     sample deltas. `null` when fewer than 2 samples were collected
//!     (CPU% requires a delta).
//!
//! Module structure mirrors [`crate::env_capture`]: a public
//! [`ResourceSampler`] + [`Sampler`] trait here, with the per-OS sampling
//! code gated behind `#[cfg(target_os = "linux")]` / `#[cfg(target_os =
//! "macos")]` modules. The trait split keeps unit tests honest — they
//! inject [`FakeSampler`] (canned values, no FS / FFI) so deterministic
//! assertions on peak detection are possible.
//!
//! The sampling task runs on the tokio runtime at the configured interval
//! (default 1 Hz per `Config::sampler_interval_ms` — see step 3j's wiring
//! commit). It tracks running peaks via task-local state and returns them
//! atomically when [`ResourceSampler::stop`] is awaited. Drop semantics
//! abort the task so a scenario that errors out doesn't leak the
//! background loop.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("wallet-benchmarks supports Linux and macOS only");

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const LOG_TARGET: &str = "c::sampler";

/// Process ID wrapper. Unix-only — POSIX `pid_t` is `i32` on the platforms
/// we target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pid(pub i32);

impl Pid {
    /// Returns the harness's own PID (i.e. the wallet-benchmarks process
    /// itself). Used by [`crate::modes::Mode::target_pid_for_sampling`]'s
    /// default impl — measures harness-side overhead per the brief's
    /// option (a) per-mode sampling choice.
    pub fn self_pid() -> Self {
        Self(std::process::id() as i32)
    }
}

/// Per-PID resource sampling primitive. Trait abstraction over the live
/// platform-gated implementation so unit tests can inject canned values
/// via [`FakeSampler`].
pub trait Sampler: Send + Sync {
    /// Sample resident set size in bytes for `pid`. Returns `None` when
    /// the PID is dead, the sample syscall failed, or parsing failed —
    /// every "I couldn't read it" is collapsed to `None` so the sampler
    /// loop can keep ticking past transient failures.
    fn sample_rss(&self, pid: Pid) -> Option<u64>;
    /// Sample `(utime_ticks, stime_ticks)` for `pid`. Same `None`
    /// semantics as [`Sampler::sample_rss`]. Ticks are in
    /// `_SC_CLK_TCK` units on Linux (typically 100/sec) and CPU ticks on
    /// macOS (proc_pidinfo's `pti_total_user` + `pti_total_system`,
    /// nanoseconds — see [`macos::sample_cpu_ticks`] for the conversion).
    fn sample_cpu_ticks(&self, pid: Pid) -> Option<(u64, u64)>;
}

/// Production sampler. Delegates to the platform-gated module's free
/// functions for both RSS and CPU-ticks.
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveSampler;

impl Sampler for LiveSampler {
    fn sample_rss(&self, pid: Pid) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            linux::sample_rss(pid.0)
        }
        #[cfg(target_os = "macos")]
        {
            macos::sample_rss(pid.0)
        }
    }

    fn sample_cpu_ticks(&self, pid: Pid) -> Option<(u64, u64)> {
        #[cfg(target_os = "linux")]
        {
            linux::sample_cpu_ticks(pid.0)
        }
        #[cfg(target_os = "macos")]
        {
            macos::sample_cpu_ticks(pid.0)
        }
    }
}

/// Background per-PID sampler. Started via [`ResourceSampler::start`],
/// stopped via [`ResourceSampler::stop`] which awaits the background task
/// and returns the observed peaks.
///
/// Drop semantics abort the task so a scenario that errors out doesn't
/// leak the loop. Explicit `stop()` is the normal exit path because it
/// returns the peaks.
pub struct ResourceSampler {
    handle: Option<JoinHandle<(Option<u64>, Option<f64>)>>,
    stop_tx: Option<oneshot::Sender<()>>,
}

impl ResourceSampler {
    /// Start sampling `pid` at the given interval. Returns immediately
    /// after spawning the background task. Sampling continues until
    /// [`Self::stop`] is awaited (normal path) or `Self` is dropped
    /// (cleanup path).
    ///
    /// `interval_ms` is clamped to a minimum of 1 ms so a misconfigured
    /// 0 doesn't spin-loop the runtime. Production callers pass
    /// `Config::sampler_interval_ms` (default 1000 = 1 Hz per
    /// `DESIGN.md §Sampling`).
    ///
    /// `sampler` is `Arc<dyn Sampler>` so the background task can take
    /// shared ownership of either [`LiveSampler`] or [`FakeSampler`]
    /// without lifetime gymnastics.
    pub fn start(sampler: Arc<dyn Sampler>, pid: Pid, interval_ms: u64) -> Self {
        let interval = Duration::from_millis(interval_ms.max(1));
        let ticks_per_sec = clock_ticks_per_sec();
        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();

        log::debug!(
            target: LOG_TARGET,
            "starting sampler: pid={} interval_ms={} ticks_per_sec={} num_cpus={}",
            pid.0,
            interval_ms,
            ticks_per_sec,
            num_cpus,
        );

        let handle = tokio::spawn(async move {
            let mut peak_rss: Option<u64> = None;
            let mut peak_cpu_pct: Option<f64> = None;
            // Previous (utime, stime, instant) sample for the CPU-pct delta.
            // None until the first successful sample_cpu_ticks call.
            let mut prev: Option<(u64, u64, Instant)> = None;

            loop {
                // Sample RSS — running peak.
                if let Some(rss) = sampler.sample_rss(pid) {
                    peak_rss = Some(peak_rss.map_or(rss, |p| p.max(rss)));
                }

                // Sample CPU ticks — compute pct as a delta vs `prev`.
                if let Some((utime, stime)) = sampler.sample_cpu_ticks(pid) {
                    let now = Instant::now();
                    if let Some((prev_u, prev_s, prev_t)) = prev {
                        let delta_ticks = (utime + stime).saturating_sub(prev_u + prev_s);
                        let elapsed_secs = now.duration_since(prev_t).as_secs_f64();
                        if let Some(pct) =
                            compute_cpu_pct(delta_ticks, elapsed_secs, ticks_per_sec, num_cpus)
                        {
                            peak_cpu_pct = Some(peak_cpu_pct.map_or(pct, |p| p.max(pct)));
                        }
                    }
                    prev = Some((utime, stime, now));
                }

                // Wait one interval OR stop signal — whichever first.
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => break,
                    _ = tokio::time::sleep(interval) => {}
                }
            }

            (peak_rss, peak_cpu_pct)
        });

        Self {
            handle: Some(handle),
            stop_tx: Some(stop_tx),
        }
    }

    /// Signal the background task to stop and await its observed peaks.
    /// Consumes `self` — callers are expected to call this once per
    /// sampler.
    pub async fn stop(mut self) -> (Option<u64>, Option<f64>) {
        if let Some(tx) = self.stop_tx.take() {
            // Send may fail if the receiver was already dropped (e.g.
            // task panicked); the JoinHandle::await below surfaces that.
            let _ = tx.send(());
        }
        match self.handle.take() {
            Some(h) => h.await.unwrap_or((None, None)),
            None => (None, None),
        }
    }
}

impl Drop for ResourceSampler {
    fn drop(&mut self) {
        // Drop path (scenario errored out / Self was dropped without
        // stop()). Signal the task to exit gracefully; if the receiver
        // was already taken by stop() this is a no-op.
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        // Abort the JoinHandle if it's still around — the task may not
        // have observed the signal yet, and we don't want to leak it.
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

/// Compute CPU% from a tick-delta over a wall-clock interval, normalised
/// to "all cores at 100%" per `RESULT_PROFILE_SCHEMA.md` lines 126-127:
///
/// ```text
/// pct = (delta_ticks / ticks_per_sec) / (elapsed_secs * num_cpus) * 100
/// ```
///
/// Returns `None` when any divisor is zero. Extracted as a pure free
/// function so unit tests can assert the formula directly without going
/// through the async sampler loop.
pub(crate) fn compute_cpu_pct(
    delta_ticks: u64,
    elapsed_secs: f64,
    ticks_per_sec: u64,
    num_cpus: usize,
) -> Option<f64> {
    if elapsed_secs <= 0.0 || ticks_per_sec == 0 || num_cpus == 0 {
        return None;
    }
    let cpu_secs = delta_ticks as f64 / ticks_per_sec as f64;
    Some(cpu_secs / (elapsed_secs * num_cpus as f64) * 100.0)
}

/// Returns `_SC_CLK_TCK` for converting CPU ticks → seconds on Linux.
/// macOS doesn't use ticks (proc_pidinfo returns nanoseconds), but the
/// shared compute_cpu_pct path uses ticks-per-sec uniformly — macOS's
/// [`macos::sample_cpu_ticks`] converts its nanoseconds into a synthetic
/// 1_000_000_000 ticks-per-sec scale so the same formula works.
fn clock_ticks_per_sec() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: sysconf is signal-safe and takes a constant input.
        let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if v <= 0 {
            100
        } else {
            v as u64
        }
    }
    #[cfg(target_os = "macos")]
    {
        // macOS uses nanosecond-resolution CPU times — see
        // macos::sample_cpu_ticks. Encode 1 ns = 1 "tick" so the shared
        // compute_cpu_pct formula stays uniform.
        1_000_000_000
    }
}

/// Deterministic [`Sampler`] for unit tests. Returns canned values popped
/// from per-call queues; `None` once a queue is exhausted (so tests can
/// assert "after K samples the loop sees no more data").
#[cfg(test)]
#[derive(Default)]
pub struct FakeSampler {
    rss: std::sync::Mutex<std::collections::VecDeque<u64>>,
    cpu: std::sync::Mutex<std::collections::VecDeque<(u64, u64)>>,
}

#[cfg(test)]
impl FakeSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a single RSS value to be returned by the next
    /// `sample_rss` call.
    pub fn push_rss(&self, v: u64) {
        self.rss.lock().unwrap().push_back(v);
    }

    /// Queue a single `(utime, stime)` pair for the next
    /// `sample_cpu_ticks` call.
    pub fn push_cpu(&self, utime: u64, stime: u64) {
        self.cpu.lock().unwrap().push_back((utime, stime));
    }
}

#[cfg(test)]
impl Sampler for FakeSampler {
    fn sample_rss(&self, _pid: Pid) -> Option<u64> {
        self.rss.lock().unwrap().pop_front()
    }
    fn sample_cpu_ticks(&self, _pid: Pid) -> Option<(u64, u64)> {
        self.cpu.lock().unwrap().pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_cpu_pct_known_inputs() {
        // 100 ticks over 1.0s with ticks_per_sec=100 and num_cpus=1 →
        // (100/100) / (1.0 * 1) * 100 = 100.0%.
        assert_eq!(compute_cpu_pct(100, 1.0, 100, 1), Some(100.0));
        // Same ticks/wallclock but 4 cores → 25%.
        assert_eq!(compute_cpu_pct(100, 1.0, 100, 4), Some(25.0));
        // 50 ticks over 0.5s, 1 core, ticks_per_sec=100 →
        // (50/100) / (0.5 * 1) * 100 = 100%.
        assert_eq!(compute_cpu_pct(50, 0.5, 100, 1), Some(100.0));
        // Zero divisors → None (no panic).
        assert_eq!(compute_cpu_pct(100, 0.0, 100, 1), None);
        assert_eq!(compute_cpu_pct(100, 1.0, 0, 1), None);
        assert_eq!(compute_cpu_pct(100, 1.0, 100, 0), None);
    }

    #[tokio::test]
    async fn sampler_returns_some_rss_with_at_least_one_sample() {
        let fake = Arc::new(FakeSampler::new());
        fake.push_rss(4096);
        let sampler = ResourceSampler::start(
            fake.clone() as Arc<dyn Sampler>,
            Pid(std::process::id() as i32),
            5,
        );
        // Give the task one tick + a small buffer.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (peak_rss, peak_cpu) = sampler.stop().await;
        assert_eq!(peak_rss, Some(4096));
        assert!(peak_cpu.is_none(), "no CPU samples pushed → None");
    }

    #[tokio::test]
    async fn sampler_returns_none_cpu_with_fewer_than_two_samples() {
        let fake = Arc::new(FakeSampler::new());
        fake.push_cpu(100, 50);
        let sampler = ResourceSampler::start(fake.clone() as Arc<dyn Sampler>, Pid::self_pid(), 5);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (_, peak_cpu) = sampler.stop().await;
        assert!(
            peak_cpu.is_none(),
            "one CPU sample isn't enough for a delta → None",
        );
    }

    #[tokio::test]
    async fn sampler_returns_max_rss_across_samples() {
        let fake = Arc::new(FakeSampler::new());
        for v in [100u64, 200, 150, 300, 200] {
            fake.push_rss(v);
        }
        let sampler = ResourceSampler::start(fake.clone() as Arc<dyn Sampler>, Pid::self_pid(), 5);
        // 5ms interval × 5 ticks = ~25ms; wait long enough for all 5
        // queued values to be consumed.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let (peak_rss, _) = sampler.stop().await;
        assert_eq!(
            peak_rss,
            Some(300),
            "peak RSS = max across consumed samples"
        );
    }

    #[tokio::test]
    async fn sampler_drop_aborts_task_without_panic() {
        let fake = Arc::new(FakeSampler::new());
        fake.push_rss(1024);
        let sampler = ResourceSampler::start(fake.clone() as Arc<dyn Sampler>, Pid::self_pid(), 5);
        // Drop without explicit stop — Drop impl must abort cleanly.
        drop(sampler);
        // Give the runtime a moment to process the abort.
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Reaching here means no panic on drop.
    }

    /// LiveSampler smoke test — sample the harness's own PID and assert
    /// the RSS is positive. Gated to the supported platforms only.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn live_sampler_smoke_on_self_pid() {
        let live = Arc::new(LiveSampler) as Arc<dyn Sampler>;
        let sampler = ResourceSampler::start(live, Pid::self_pid(), 10);
        // Wait long enough for at least 2 ticks so CPU-pct gets a delta.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let (peak_rss, peak_cpu) = sampler.stop().await;
        assert!(
            peak_rss.map(|r| r > 0).unwrap_or(false),
            "self-pid RSS must be > 0, got {peak_rss:?}",
        );
        // CPU% may legitimately round to 0.0 for an idle test task — only
        // assert that we got SOME value, not its magnitude.
        assert!(
            peak_cpu.is_some(),
            "two ticks must produce a CPU-pct delta, got None",
        );
    }
}
