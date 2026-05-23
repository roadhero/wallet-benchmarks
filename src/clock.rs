//! Time abstraction for scenario / send-loop timing.
//!
//! Production uses [`RealClock`] which delegates to [`std::time::Instant::now`]
//! and [`tokio::time::sleep`]. Tests use [`FakeClock`] (under `#[cfg(test)]`)
//! that also delegates to tokio's timer — integrates with
//! [`tokio::time::pause`] so scenario timeouts can be advanced
//! deterministically without real wall-clock waits.
//!
//! Per `analysis/DESIGN.md §Scenario state machine`, scenarios that poll for
//! confirmation route their sleeps through this trait so the test suite can
//! exercise the AC-33 timeout branch without burning the configured
//! `per_tx_confirmation_timeout_ms` on every run. The trait is intentionally
//! minimal — only the two primitives every existing sleep site needs.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// Time source for scenario timing. Production wires `RealClock`; tests
/// wire `FakeClock`. Both delegate to tokio's timer so behaviour under
/// `tokio::time::pause()` is identical.
pub trait Clock: Send + Sync {
    /// Wall-clock instant. Backed by [`Instant::now`] in both impls — the
    /// trait exists so callers can swap the sleep implementation without
    /// also swapping the time source.
    fn now(&self) -> Instant;

    /// Sleep for `dur`. Returns a boxed future so the trait is
    /// object-safe; the lifetime `'static` matches the parent borrow of
    /// `&self` (tokio's sleep future does not retain `&self`).
    fn sleep(&self, dur: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}

/// Production clock — backed by tokio's timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, dur: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(tokio::time::sleep(dur))
    }
}

/// Test clock — also backed by tokio's timer, so `tokio::time::pause()` +
/// `tokio::time::advance()` drive scenario timeouts deterministically.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeClock;

#[cfg(test)]
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, dur: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(tokio::time::sleep(dur))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clock_now_advances() {
        let clock = RealClock;
        let t0 = clock.now();
        // Yield + sleep one tick so the kernel monotonic clock advances.
        tokio::time::sleep(Duration::from_millis(1)).await;
        let t1 = clock.now();
        assert!(
            t1 >= t0,
            "RealClock::now must be monotonic; t0={t0:?}, t1={t1:?}",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fake_clock_sleep_completes_under_paused_time() {
        // Under `start_paused`, tokio's timer does not advance unless the
        // test explicitly calls `tokio::time::advance()`. The sleep future
        // therefore only resolves when we advance the timer past the
        // requested duration — proving the impl routes through tokio's
        // timer and not a real-time blocker.
        let clock = FakeClock;
        let sleep_fut = clock.sleep(Duration::from_secs(60));
        tokio::pin!(sleep_fut);
        // Poll once; should not be ready.
        let poll_once = futures_poll_once(&mut sleep_fut).await;
        assert!(
            !poll_once,
            "sleep must not resolve before timer advances under paused time",
        );
        tokio::time::advance(Duration::from_secs(61)).await;
        sleep_fut.await;
    }

    /// Best-effort one-shot poll: returns true if the future is `Ready`,
    /// false if `Pending`. Avoids pulling `futures` as a dep — uses the
    /// stdlib `noop_waker` shim equivalent via a hand-rolled waker.
    async fn futures_poll_once<F: Future + Unpin>(fut: &mut F) -> bool {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        unsafe fn no_op(_: *const ()) {}
        unsafe fn no_op_clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(no_op_clone, no_op, no_op, no_op);
        let raw = RawWaker::new(std::ptr::null(), &VTABLE);
        // SAFETY: the VTABLE entries are all no-ops and the waker carries
        // no data; the resulting Waker is valid for the duration of the
        // single poll call below.
        let waker = unsafe { Waker::from_raw(raw) };
        let mut cx = Context::from_waker(&waker);
        matches!(std::pin::Pin::new(fut).poll(&mut cx), Poll::Ready(_))
    }
}
