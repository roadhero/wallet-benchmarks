//! S4 anti-dispatch-serialization guard — enforces the concurrent dispatch
//! pattern in `src/scenarios/s4_concurrent.rs`.
//!
//! Per AC-17, AC-30, AC-31, AC-32 and `analysis/DESIGN.md §S4 state machine`
//! plus the 3i.1.f brief: S4 dispatches N ∈ {8,16,32,64,128} concurrent
//! construction tasks per sub-block via `tokio::JoinSet` and races
//! `joinset.join_next()` against `clock.sleep(budget)` inside a
//! `tokio::select!` with a two-arm shape. Any deviation — Semaphore,
//! Mutex<*Dispatcher>, dispatch-time for-loop with `.await`, retry, backoff,
//! throttle — defeats the measurement.
//!
//! Scope: `src/scenarios/s4_concurrent.rs` only (file-narrow grep). Broader
//! AC-30/31/32 patterns are enforced by `tests/c_no_retry_backoff_throttle.rs`
//! across all of `src/scenarios/*.rs` and `src/modes/*.rs`.
//!
//! When `src/scenarios/s4_concurrent.rs` does not yet exist (commit-2-only
//! state per the 3i.1.f brief), this test passes trivially — same convention
//! as `tests/c_no_utxo_pre_partition_in_s1.rs`. Enforcement begins the
//! moment the file appears (commit 3).
//!
//! `#[cfg(test)] mod tests { ... }` blocks are stripped before grep so
//! scenario-side unit tests can use fixture patterns the production-side
//! shape forbids (the same convention as `c_no_retry_backoff_throttle.rs`).

use std::path::PathBuf;

use regex::Regex;

/// Resolve the absolute path of `src/scenarios/s4_concurrent.rs`.
fn s4_concurrent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/scenarios/s4_concurrent.rs")
}

/// Strip `#[cfg(test)] mod NAME { ... }` blocks from the byte-level source
/// via a balanced-brace parser. Mirrors the helper in
/// `tests/c_no_retry_backoff_throttle.rs` — kept inline here so the two
/// test files are independent and a refactor to one does not silently
/// break the other.
fn strip_cfg_test_mods(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out_bytes = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = find_cfg_test_mod_end(bytes, i) {
            i = end;
        } else {
            out_bytes.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out_bytes).expect("byte-balanced strip preserves UTF-8")
}

fn starts_with_at(bytes: &[u8], start: usize, needle: &[u8]) -> bool {
    bytes.len() >= start + needle.len() && &bytes[start..start + needle.len()] == needle
}

fn find_cfg_test_mod_end(bytes: &[u8], start: usize) -> Option<usize> {
    let head = b"#[cfg(test)]";
    if !starts_with_at(bytes, start, head) {
        return None;
    }
    let mut cursor = start + head.len();
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if !starts_with_at(bytes, cursor, b"mod") {
        let visibilities: &[&[u8]] = &[b"pub(crate)", b"pub(super)", b"pub(in", b"pub"];
        let mut matched = false;
        for v in visibilities {
            if starts_with_at(bytes, cursor, v) {
                cursor += v.len();
                while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                if v == b"pub(in" {
                    while cursor < bytes.len() && bytes[cursor] != b')' {
                        cursor += 1;
                    }
                    if cursor < bytes.len() {
                        cursor += 1;
                    }
                    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                        cursor += 1;
                    }
                }
                matched = true;
                break;
            }
        }
        if !matched {
            return None;
        }
    }
    if !starts_with_at(bytes, cursor, b"mod") {
        return None;
    }
    cursor += b"mod".len();
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    while cursor < bytes.len() {
        let b = bytes[cursor];
        if b.is_ascii_alphanumeric() || b == b'_' {
            cursor += 1;
        } else {
            break;
        }
    }
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if cursor >= bytes.len() || bytes[cursor] != b'{' {
        return None;
    }
    Some(find_matching_brace(bytes, cursor) + 1)
}

fn find_matching_brace(bytes: &[u8], open_idx: usize) -> usize {
    debug_assert_eq!(bytes[open_idx], b'{');
    let mut depth: usize = 0;
    let mut i = open_idx;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'"' {
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                if c == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if c == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if b == b'\'' {
            if i + 2 < bytes.len() && bytes[i + 2] == b'\'' {
                i += 3;
                continue;
            }
            if i + 3 < bytes.len() && bytes[i + 1] == b'\\' && bytes[i + 3] == b'\'' {
                i += 4;
                continue;
            }
            i += 1;
            continue;
        }
        if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
        i += 1;
    }
    bytes.len().saturating_sub(1)
}

/// Strip line + block comments from byte-level source. Doc-comments that
/// legitimately mention forbidden tokens (`backoff`, `Semaphore`, etc.) in
/// "we do NOT use X" prose must not trip the grep.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out_bytes = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'"' {
            out_bytes.push(bytes[i]);
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                out_bytes.push(c);
                i += 1;
                if c == b'\\' && i < bytes.len() {
                    out_bytes.push(bytes[i]);
                    i += 1;
                    continue;
                }
                if c == b'"' {
                    break;
                }
            }
            continue;
        }
        out_bytes.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out_bytes).expect("byte-balanced comment strip preserves UTF-8")
}

/// Load `src/scenarios/s4_concurrent.rs`, strip `#[cfg(test)]` mod blocks
/// AND line/block comments. Returns `None` if the file does not yet exist
/// (commit-2-only state per the 3i.1.f brief).
fn load_production_source() -> Option<String> {
    let path = s4_concurrent_path();
    if !path.exists() {
        return None;
    }
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let cfg_stripped = strip_cfg_test_mods(&raw);
    Some(strip_comments(&cfg_stripped))
}

/// Positive assertion 1 — `JoinSet` is the spawn primitive S4 uses for the
/// N concurrent construction tasks.
#[test]
fn s4_concurrent_uses_joinset() {
    let Some(src) = load_production_source() else {
        return;
    };
    let re = Regex::new(r"\bJoinSet\b").expect("regex compiles");
    assert!(
        re.is_match(&src),
        "src/scenarios/s4_concurrent.rs must use tokio::task::JoinSet \
         for N concurrent dispatch tasks (AC-17). Token `JoinSet` not \
         found anywhere in production source.",
    );
}

/// Positive assertion 2 — `tokio::select!` with a two-arm body
/// (`join_next` + clock sleep) drives the budget-vs-completion race.
#[test]
fn s4_concurrent_uses_tokio_select_with_join_next_and_sleep_arms() {
    let Some(src) = load_production_source() else {
        return;
    };
    let select_present = Regex::new(r"tokio::select!\s*\{")
        .expect("regex compiles")
        .is_match(&src);
    assert!(
        select_present,
        "src/scenarios/s4_concurrent.rs must contain a tokio::select! block \
         racing JoinSet drain against the budget deadline (AC-17).",
    );
    let join_next_arm = Regex::new(r"join_next\s*\(")
        .expect("regex compiles")
        .is_match(&src);
    assert!(
        join_next_arm,
        "tokio::select! block must include a `joinset.join_next()` arm \
         to drain completed dispatches (AC-17 / AC-18).",
    );
    let sleep_arm = Regex::new(r"\.sleep\s*\(")
        .expect("regex compiles")
        .is_match(&src);
    assert!(
        sleep_arm,
        "tokio::select! block must include a `clock.sleep(budget)` arm \
         to enforce s4_t_budget_ms_per_sub_block (AC-17).",
    );
}

/// Negative assertion 3 — no `Semaphore` of any kind in S4 dispatch
/// (per the brief's "no Mutex on the dispatcher surface" rule and AC-31).
#[test]
fn s4_concurrent_has_no_semaphore() {
    let Some(src) = load_production_source() else {
        return;
    };
    let re = Regex::new(r"\bSemaphore\b").expect("regex compiles");
    let mut hits = Vec::new();
    for (line_idx, line) in src.lines().enumerate() {
        if re.is_match(line) {
            hits.push(format!("line {}: {}", line_idx + 1, line.trim()));
        }
    }
    assert!(
        hits.is_empty(),
        "src/scenarios/s4_concurrent.rs must not use a Semaphore to \
         serialize concurrent dispatch (AC-31). Hits:\n  {}",
        hits.join("\n  "),
    );
}

/// Negative assertion 4 — no Mutex wrapping any dispatcher-shaped type on
/// the production dispatch path (per the brief: "no Mutex on the production
/// dispatcher surface" — Option B from `DESIGN_AMENDMENT.md §9.6`).
#[test]
fn s4_concurrent_has_no_mutex_around_dispatcher_or_client() {
    let Some(src) = load_production_source() else {
        return;
    };
    // Mutex<Client>, Mutex<Subprocess>, Mutex<Spawner>, Mutex<Dispatcher>
    // and their qualified equivalents (`tokio::sync::Mutex<...>`).
    let patterns = [
        r"Mutex<\s*\w*Client\b",
        r"Mutex<\s*\w*Subprocess\b",
        r"Mutex<\s*\w*Spawner\b",
        r"Mutex<\s*\w*Dispatcher\b",
    ];
    let mut hits = Vec::new();
    for p in patterns {
        let re = Regex::new(p).expect("regex compiles");
        for (line_idx, line) in src.lines().enumerate() {
            if re.is_match(line) {
                hits.push(format!(
                    "matched `{p}` at line {}: {}",
                    line_idx + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "src/scenarios/s4_concurrent.rs must not wrap a dispatch surface \
         in Mutex (defeats Option B's lock-free design; AC-31). Hits:\n  {}",
        hits.join("\n  "),
    );
}

/// Negative assertion 5 — no dispatch-time `for … in … { … .await … }`
/// loop that would serialize the N tasks. The only allowed `.await` inside
/// a for-loop is the drain shape `while let Some(joined) = joinset.join_next().await`
/// (a `while let` is not a `for`-loop and is the canonical JoinSet drain).
#[test]
fn s4_concurrent_has_no_dispatch_serializing_for_loop_with_await() {
    let Some(src) = load_production_source() else {
        return;
    };
    // Find every `for <binding> in <expr> { ... }` block and assert none of
    // their bodies contain `.await`. The byte-balanced brace walker excises
    // each body for inspection.
    let bytes = src.as_bytes();
    let for_re = Regex::new(r"^\s*for\s+\w+\s+in\b").expect("regex compiles");
    let mut hits = Vec::new();
    for (line_idx, line) in src.lines().enumerate() {
        if for_re.is_match(line) {
            // Locate the corresponding brace in the byte stream — `for ... { ... }`.
            // Find the byte offset of this line's start.
            let line_start = locate_line_start(&src, line_idx);
            // Scan forward for the opening brace on this or subsequent lines.
            if let Some(open) = find_open_brace_after(bytes, line_start) {
                let close = find_matching_brace(bytes, open);
                let body = String::from_utf8_lossy(&bytes[open + 1..close]).into_owned();
                if body.contains(".await") {
                    hits.push(format!(
                        "line {}: `for` loop body contains `.await` — \
                         dispatch must spawn into JoinSet, not serialize:\n    {}",
                        line_idx + 1,
                        line.trim(),
                    ));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "src/scenarios/s4_concurrent.rs must spawn dispatch tasks into a \
         JoinSet (each call non-blocking), not iterate-and-await \
         (AC-17 / AC-31). The only legitimate `.await` inside a loop is \
         `while let Some(joined) = joinset.join_next().await` (a `while let`, \
         not a `for` — the regex `^\\s*for\\s+\\w+\\s+in\\b` does not match \
         that shape). Hits:\n  {}",
        hits.join("\n  "),
    );
}

/// Negative assertion 6 — no retry / backoff / throttle / rate-limit /
/// exponential-* tokens anywhere in the file (AC-30 / AC-32). Echoes
/// `c_no_retry_backoff_throttle.rs` patterns but scoped to S4 only so a
/// regression localised to S4 is reported with a clearer file-named error.
#[test]
fn s4_concurrent_has_no_retry_backoff_throttle_tokens() {
    let Some(src) = load_production_source() else {
        return;
    };
    let patterns = [
        r"\bthrottle\w*\s*[(:]",
        r"\bbackoff\w*\s*[(:]",
        r"\brate_limit\w*\s*[(:]",
        r"\bexponential\w*\s*[(:]",
        r"\bretry\w*\s*[(:]",
    ];
    let mut hits = Vec::new();
    for p in patterns {
        let re = Regex::new(p).expect("regex compiles");
        for (line_idx, line) in src.lines().enumerate() {
            if re.is_match(line) {
                hits.push(format!(
                    "matched `{p}` at line {}: {}",
                    line_idx + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "src/scenarios/s4_concurrent.rs must not contain retry / backoff / \
         throttle / rate-limit tokens (AC-30 / AC-32). Hits:\n  {}",
        hits.join("\n  "),
    );
}

/// Find the byte offset corresponding to the start of `line_idx` (0-based)
/// in `src`. Used to root the for-loop body inspection at the right byte.
fn locate_line_start(src: &str, line_idx: usize) -> usize {
    let mut byte = 0;
    let bytes = src.as_bytes();
    let mut current_line = 0;
    while byte < bytes.len() && current_line < line_idx {
        if bytes[byte] == b'\n' {
            current_line += 1;
        }
        byte += 1;
    }
    byte
}

/// Find the first `{` at byte position `start` or later in `bytes`. Scans
/// linearly; comments / strings are intentionally NOT excluded — this is
/// invoked only on for-loop heads where the opening brace of the body is
/// the very next non-whitespace token after the loop expression, and the
/// scan stops at the first `{` it sees.
fn find_open_brace_after(bytes: &[u8], start: usize) -> Option<usize> {
    (start..bytes.len()).find(|&i| bytes[i] == b'{')
}
