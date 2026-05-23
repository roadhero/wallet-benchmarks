//! macOS-specific per-PID sampling for [`super::ResourceSampler`].
//!
//! Sources (per `analysis/DESIGN.md §Sampling`):
//!   * RSS — `libc::proc_pidinfo` with `PROC_PIDTASKINFO`, returning
//!     `proc_taskinfo::pti_resident_size` directly in bytes.
//!   * CPU times — same FFI call returning `pti_total_user` +
//!     `pti_total_system`, both in **nanoseconds** (Mach absolute time
//!     already converted by the kernel). The [`super::compute_cpu_pct`]
//!     formula expects ticks, so we encode 1 ns = 1 "tick" and pass
//!     `ticks_per_sec = 1_000_000_000` from [`super::clock_ticks_per_sec`]
//!     — keeps the shared formula uniform.
//!
//! Every FFI failure (PID dead, EPERM, EINVAL) collapses to `None` so
//! the sampler loop in [`super::ResourceSampler`] can keep ticking past
//! transient errors.

use std::mem::{size_of, MaybeUninit};

use libc::{c_int, c_void, proc_pidinfo, proc_taskinfo, PROC_PIDTASKINFO};

const LOG_TARGET: &str = "c::sampler::macos";

pub(super) fn sample_rss(pid: i32) -> Option<u64> {
    let ti = fetch_task_info(pid)?;
    Some(ti.pti_resident_size)
}

pub(super) fn sample_cpu_ticks(pid: i32) -> Option<(u64, u64)> {
    let ti = fetch_task_info(pid)?;
    // pti_total_user / pti_total_system are already in nanoseconds — see
    // <https://opensource.apple.com/source/xnu/xnu-7195.121.3/bsd/sys/proc_info.h>.
    // Encode ns directly as our "ticks" unit (see module-level doc).
    Some((ti.pti_total_user, ti.pti_total_system))
}

fn fetch_task_info(pid: i32) -> Option<proc_taskinfo> {
    let mut ti: MaybeUninit<proc_taskinfo> = MaybeUninit::uninit();
    let size = size_of::<proc_taskinfo>() as c_int;
    // SAFETY:
    //   * pid is an integer (no pointer indirection).
    //   * PROC_PIDTASKINFO is the documented flavour for proc_taskinfo.
    //   * Buffer is sized exactly to proc_taskinfo (see size_of above).
    //   * proc_pidinfo writes into the buffer when it returns size; we
    //     only call assume_init on that success path.
    let written = unsafe {
        proc_pidinfo(
            pid as c_int,
            PROC_PIDTASKINFO,
            0,
            ti.as_mut_ptr() as *mut c_void,
            size,
        )
    };
    if written <= 0 {
        log::trace!(
            target: LOG_TARGET,
            "proc_pidinfo(pid={pid}, PROC_PIDTASKINFO) returned {written}",
        );
        return None;
    }
    if written != size {
        log::trace!(
            target: LOG_TARGET,
            "proc_pidinfo wrote {written} bytes, expected {size} — short read",
        );
        return None;
    }
    // SAFETY: proc_pidinfo wrote exactly size_of::<proc_taskinfo>() bytes
    // into the buffer per the size check above; the struct is fully
    // initialised.
    Some(unsafe { ti.assume_init() })
}
