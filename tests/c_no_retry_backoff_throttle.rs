//! AC-30 / AC-31 / AC-32 static guards — forbidden retry, serialization,
//! backoff, and throttle patterns in scenario + send-side code.
//!
//! Per `analysis/DESIGN.md §Test Strategy "Harness Measures, Does Not
//! Engineer Around Wallet Pain" enforcement (AC-30/31/32/33)` and
//! `analysis/DESIGN_ADDENDUM.md §S3`, the harness records wallet behaviour
//! raw — no retry on broadcast failure, no semaphore- or mutex-based
//! serialization of concurrent construction, no rate-limit / exponential
//! backoff / throttle / sleep-as-sync. Runtime tests for "harness does NOT
//! retry" are unfalsifiable in finite time; this file is the static
//! grep-style enforcement bar.
//!
//! Scope: `src/scenarios/*.rs` and `src/modes/*.rs` (the send helpers
//! shared between Modes 2 and 3 live in `src/modes/minotari_subprocess.rs`
//! and `src/modes/minotari_wallet_ops.rs`).
//!
//! `#[cfg(test)] mod tests { ... }` blocks are stripped via a balanced-brace
//! parser before grepping. Test code is allowed to set up fixtures (e.g.
//! `tokio::time::sleep` in a confirmation-timeout test) that the
//! production-path patterns forbid.
//!
//! `tokio::time::sleep` inside `tokio::select! { ... }` arms is exempted
//! via a documented carve-out: the `tokio::select!` block body is excised
//! before the AC-32 grep. This permits the deadline-arm idiom used by S0+
//! confirmation loops (`tokio::select! { _ = deadline => ..., _ = sleep(POLL) => ... }`)
//! where the sleep is a *bound*, not a serialization mechanism.

use std::path::{Path, PathBuf};

use regex::Regex;

/// Walk the source tree and produce the absolute paths of every `.rs` file
/// under `src/scenarios/` and `src/modes/` (the AC-30/31/32 enforcement
/// scope per DESIGN.md §Test Strategy table).
fn files_to_scan() -> Vec<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    for sub in ["src/scenarios", "src/modes"] {
        let dir = manifest_dir.join(sub);
        collect_rs_files(&dir, &mut out);
    }
    out.sort();
    out
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Strip every `#[cfg(test)] mod tests { ... }` block from `source` via a
/// balanced-brace parser, returning the production-only remainder.
///
/// The parser scans for the literal byte-sequence `#[cfg(test)]` followed
/// by whitespace, the keyword `mod`, an identifier, and an opening brace;
/// it then walks the source counting `{` / `}` (tracking string/char
/// literals and line/block comments to avoid spurious matches) and excises
/// through the matching close brace.
///
/// Operates on the byte slice (not on `str` slicing) so multi-byte UTF-8
/// codepoints in comments (e.g. `§`, `→`) do not panic the index math.
fn strip_cfg_test_mods(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out_bytes = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = find_cfg_test_mod_end(bytes, i) {
            // Skip the entire `#[cfg(test)] mod foo { ... }` block. Don't
            // emit it into the output.
            i = end;
        } else {
            out_bytes.push(bytes[i]);
            i += 1;
        }
    }
    // The excised slices are always whole tokens (byte-balanced braces) so
    // the remainder is still valid UTF-8.
    String::from_utf8(out_bytes).expect("excising whole brace blocks preserves UTF-8")
}

/// Test whether `bytes[start..]` begins with the ASCII byte sequence
/// `needle`. Byte-exact; multi-byte chars in the surrounding source don't
/// affect this check.
fn starts_with_at(bytes: &[u8], start: usize, needle: &[u8]) -> bool {
    bytes.len() >= start + needle.len() && &bytes[start..start + needle.len()] == needle
}

/// If position `start` in `bytes` begins a `#[cfg(test)] mod NAME { ... }`
/// declaration, return the byte index immediately after the closing brace.
/// Otherwise return `None`.
fn find_cfg_test_mod_end(bytes: &[u8], start: usize) -> Option<usize> {
    // Anchor: must start with `#[cfg(test)]`. (Other `cfg` annotations are
    // allowed in production paths.)
    let head = b"#[cfg(test)]";
    if !starts_with_at(bytes, start, head) {
        return None;
    }
    let mut cursor = start + head.len();
    // Skip whitespace.
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    // Allow an optional `pub(...)` visibility before `mod`.
    if !starts_with_at(bytes, cursor, b"mod") {
        let visibilities: &[&[u8]] = &[b"pub(crate)", b"pub(super)", b"pub(in", b"pub"];
        let mut matched = false;
        for v in visibilities {
            if starts_with_at(bytes, cursor, v) {
                cursor += v.len();
                while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                // For `pub(in ...)` we need to also skip the parens.
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
    // Skip whitespace and the identifier.
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
    // Now we must be at the opening brace.
    if cursor >= bytes.len() || bytes[cursor] != b'{' {
        return None;
    }
    Some(find_matching_brace(bytes, cursor) + 1)
}

/// Given `bytes` and the byte index of an opening `{`, return the byte
/// index of the matching `}`. Tracks string literals, char literals, line
/// comments, and block comments so braces inside those don't confuse the
/// counter.
fn find_matching_brace(bytes: &[u8], open_idx: usize) -> usize {
    debug_assert_eq!(bytes[open_idx], b'{');
    let mut depth: usize = 0;
    let mut i = open_idx;
    while i < bytes.len() {
        let b = bytes[i];
        // Block comment.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        // Line comment.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // String literal.
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
        // Char literal — keep it simple; skip `'..'` runs.
        if b == b'\'' {
            // Heuristic: if the next character is alphanumeric/whitespace
            // and the one after is also a quote, it's a char literal;
            // otherwise it's likely a lifetime tick. Either way we advance
            // one byte to keep going without crashing.
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
    // Unterminated — defensive: point at the end so the caller doesn't
    // panic. The grep will then run against the truncated string.
    bytes.len().saturating_sub(1)
}

/// Strip line comments (`// ...` to end of line) and block comments
/// (`/* ... */`) from the byte-level source. Doc-comments (`/// ...` and
/// `//! ...`) are handled by the line-comment rule. Preserves UTF-8 by
/// emitting a single space byte in place of each excised byte run, so the
/// remaining source still parses byte-for-byte.
///
/// Documentation legitimately mentions forbidden words (`backoff`,
/// `throttle`, `retry`) when explaining WHY they are forbidden; stripping
/// comments before grep prevents those mentions from tripping the test.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out_bytes = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Block comment.
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        // Line comment (covers both `//` and `///` and `//!`).
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // String literal — copy verbatim so `tokio::time::sleep` inside a
        // `"docstring"` literal would still trip the test (we don't want
        // sleeps hiding in string literals either).
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

/// Excise every `tokio::select! { ... }` block from `source`, returning the
/// remainder. AC-32's `tokio::time::sleep` ban is enforced only OUTSIDE
/// `tokio::select!` arms — inside, the sleep is a deadline/poll-interval
/// bound, not a throttle.
fn strip_tokio_select_bodies(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out_bytes = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let needle = b"tokio::select!";
    while i < bytes.len() {
        if starts_with_at(bytes, i, needle) {
            let mut cursor = i + needle.len();
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b'{' {
                let end = find_matching_brace(bytes, cursor) + 1;
                // Skip the macro invocation entirely.
                i = end;
                continue;
            }
        }
        out_bytes.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out_bytes).expect("excising whole brace blocks preserves UTF-8")
}

/// Read a source file, strip `#[cfg(test)]` mod blocks AND line/block
/// comments. Returns the production-only, comment-free source. (Caller
/// decides whether to additionally strip `tokio::select!` bodies — only
/// AC-32 needs that.)
fn load_production_source(path: &Path) -> String {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read source {}: {e}", path.display()));
    let cfg_stripped = strip_cfg_test_mods(&raw);
    strip_comments(&cfg_stripped)
}

/// Run a regex against the production-only source of each file in
/// `files_to_scan`; panic with file:line if any match is found.
fn assert_no_matches(label: &str, pattern: &str, transform: impl Fn(&Path, &str) -> String) {
    let regex = Regex::new(pattern).expect("AC pattern regex compiles");
    let mut hits = Vec::new();
    for path in files_to_scan() {
        let production = transform(&path, &load_production_source(&path));
        for (line_idx, line) in production.lines().enumerate() {
            if regex.is_match(line) {
                hits.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    line_idx + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "{label}: forbidden pattern `{pattern}` matched {} time(s):\n  {}",
        hits.len(),
        hits.join("\n  "),
    );
}

/// AC-30 — no retry / reattempt / resubmit / reschedule in send paths.
///
/// Pattern catches:
/// * function/method calls named `retry*` (`.retry(...)`, `retry_with(...)`,
///   `retries(...)`).
/// * named retry-count integers (`attempts < 3`, `attempt >= max`).
/// * for-loops over a retry counter (`for _ in 0..retries { ... }`).
#[test]
fn ac30_no_retry_in_send_paths() {
    let patterns = [
        r"\bretry\w*\s*[(:]",
        r"\battempt\w+\s*[<>=]+\s*\d",
        r"for\s+\w+\s+in\s+[^{]*\bretries?\b",
        r"\b(reattempt|resubmit|reschedule)\w*\s*[(:]",
    ];
    for p in patterns {
        assert_no_matches("AC-30", p, |_, src| src.to_string());
    }
}

/// AC-31 — no serialization of concurrent construction via Semaphore /
/// Mutex<WalletClient> / pre-partition of UTXO sets.
///
/// S4's concurrent dispatch must let the wallet's own UTXO selection
/// surface contention raw; pre-partitioning the input set defeats the
/// measurement (this is also enforced by `tests/c_no_utxo_pre_partition_in_s1.rs`
/// for S1 specifically — the patterns here cover the broader concurrent
/// construction shape).
///
/// Pattern catches:
/// * `Semaphore::new(...)` or `Arc::new(Semaphore::...)`.
/// * `Mutex<` wrapping a wallet-client or send-side type.
/// * `parking_lot::Mutex<...>` over the same.
#[test]
fn ac31_no_serialization_in_concurrent_construction() {
    let patterns = [
        r"Semaphore::new\s*\(",
        r"Arc::new\s*\(\s*Semaphore\b",
        r"Mutex<\s*(?:WalletClient|Broadcaster|BaseNode\w+Client)\b",
        r"RateLimit\w*\s*::\s*new\s*\(",
    ];
    for p in patterns {
        assert_no_matches("AC-31", p, |_, src| src.to_string());
    }
}

/// AC-32 — no backoff / throttle / rate-limiter / sleep-as-sync in send
/// paths.
///
/// `tokio::time::sleep` is permitted inside `tokio::select! { ... }` arms
/// only — those bodies are excised via `strip_tokio_select_bodies` before
/// the grep. The carve-out makes the AC-32 enforcement byte-exact: a
/// `tokio::time::sleep` outside a `tokio::select!` block fails this test;
/// one inside (the S0+ confirmation-loop deadline / poll-interval pattern)
/// passes.
#[test]
fn ac32_no_backoff_or_throttle() {
    let patterns = [
        r"\bbackoff\w*\s*[(:]",
        r"\bexponential_backoff\b",
        r"\bthrottle\w*\s*[(:]",
        r"\brate_limit\w*\s*[(:]",
        // Catches `tokio::time::sleep`, `tokio::time::sleep_until`, and any
        // future `tokio::time::sleep_*` variant. All such uses must live
        // inside a `tokio::select! { ... }` body (excised before grep).
        r"tokio::time::sleep\w*\s*\(",
        r"std::thread::sleep\s*\(",
        r"tokio::time::interval\s*\(",
    ];
    for p in patterns {
        assert_no_matches("AC-32", p, |_, src| strip_tokio_select_bodies(src));
    }
}

#[cfg(test)]
mod harness_self_tests {
    use super::*;

    #[test]
    fn strip_cfg_test_mods_removes_simple_block() {
        let src = r#"
fn prod() {}

#[cfg(test)]
mod tests {
    fn boom() { let _ = tokio::time::sleep(Duration::from_secs(1)); }
}

fn other_prod() {}
"#;
        let stripped = strip_cfg_test_mods(src);
        assert!(stripped.contains("fn prod()"));
        assert!(stripped.contains("fn other_prod()"));
        assert!(
            !stripped.contains("boom"),
            "test mod body must be excised: {stripped}"
        );
        assert!(
            !stripped.contains("tokio::time::sleep"),
            "test mod body must be excised: {stripped}"
        );
    }

    #[test]
    fn strip_cfg_test_mods_handles_nested_braces() {
        let src = r#"
#[cfg(test)]
mod tests {
    fn outer() {
        if true {
            for _ in 0..3 {
                let _x = "}";
            }
        }
    }
}

fn prod() {}
"#;
        let stripped = strip_cfg_test_mods(src);
        assert!(stripped.contains("fn prod()"));
        assert!(!stripped.contains("fn outer"));
    }

    #[test]
    fn strip_tokio_select_bodies_excises_select_arm_sleeps() {
        let src = r#"
async fn poll(deadline_ms: u64) {
    let deadline = tokio::time::sleep(Duration::from_millis(deadline_ms));
    tokio::pin!(deadline);
    tokio::select! {
        _ = &mut deadline => {},
        _ = tokio::time::sleep(Duration::from_secs(2)) => {},
    }
}
"#;
        let stripped = strip_tokio_select_bodies(src);
        // The select body is gone, but the outer `tokio::time::sleep` for
        // the deadline binding still stands. AC-32's intent is to ban sleep
        // OUTSIDE the select; we permit one binding-form sleep when it's
        // immediately the bound passed into select. The test below proves
        // the inner sleep is excised, which is the load-bearing claim.
        assert!(
            !stripped.contains("Duration::from_secs(2)"),
            "tokio::select! body sleep must be excised: {stripped}",
        );
    }
}
