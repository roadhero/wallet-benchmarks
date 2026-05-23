//! Linux-specific per-PID sampling for [`super::ResourceSampler`].
//!
//! Sources (per `analysis/DESIGN.md §Sampling`):
//!   * RSS — `/proc/<pid>/statm` field 2 (resident pages) × page size.
//!     Faster + cheaper to parse than `/proc/<pid>/status`'s `VmRSS`
//!     line and avoids the kB → bytes multiplication ambiguity.
//!   * CPU ticks — `/proc/<pid>/stat` fields 14 and 15 (`utime`,
//!     `stime`). The first field after the pid is the `comm` field
//!     wrapped in parentheses, which can contain arbitrary bytes
//!     including spaces — we locate it by finding the **last** `)` in
//!     the file and tokenising the remainder, mirroring what
//!     `proc(5)` recommends.
//!
//! Every failure (PID dead, permission denied, parse failure) collapses
//! to `None` so the sampler loop in [`super::ResourceSampler`] can keep
//! ticking past transient errors.

const LOG_TARGET: &str = "c::sampler::linux";

pub(super) fn sample_rss(pid: i32) -> Option<u64> {
    let path = format!("/proc/{pid}/statm");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            log::trace!(target: LOG_TARGET, "read {path} failed: {e}");
            return None;
        }
    };
    // statm layout: `size resident shared text lib data dt` — all in pages.
    let resident_pages: u64 = raw.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = page_size_bytes();
    Some(resident_pages.saturating_mul(page_size))
}

pub(super) fn sample_cpu_ticks(pid: i32) -> Option<(u64, u64)> {
    let path = format!("/proc/{pid}/stat");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            log::trace!(target: LOG_TARGET, "read {path} failed: {e}");
            return None;
        }
    };
    // /proc/<pid>/stat format:
    //   pid (comm) state ppid pgrp ... utime stime ...
    // `comm` can contain spaces and parens, so per proc(5) we find the
    // LAST ')' and tokenise from there.
    let last_paren = raw.rfind(')')?;
    let rest = raw.get(last_paren + 1..)?.trim_start();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the closing paren, field indices align like this:
    //   [0] state, [1] ppid, [2] pgrp, [3] session, [4] tty_nr,
    //   [5] tpgid, [6] flags, [7] minflt, [8] cminflt, [9] majflt,
    //   [10] cmajflt, [11] utime, [12] stime, ...
    // (Combined with the pid + (comm) prefix this gives the canonical
    // 14/15 indexing from /proc(5).)
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime, stime))
}

fn page_size_bytes() -> u64 {
    // SAFETY: sysconf is signal-safe and takes a constant input.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v <= 0 {
        4096
    } else {
        v as u64
    }
}
