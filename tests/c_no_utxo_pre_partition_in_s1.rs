//! S1 anti-pre-partition guard — forbids chunking / partitioning / splitting
//! the UTXO set before submission inside `src/scenarios/s1_volume.rs`.
//!
//! Per AC-30 ("Harness measures, does not engineer around wallet pain") and
//! `analysis/DESIGN.md §Scenario state machine §S1`, S1's volume loop
//! measures the wallet's own UTXO-selection logic across 127 transactions
//! in 7 doubling rounds. Pre-partitioning the input set — `chunk(N)`,
//! `partition(...)`, `split_at(N)`, `split(N)` — defeats the measurement:
//! the harness would be doing the wallet's job and the recorded
//! selection-rejection rate would be 0% by construction.
//!
//! This grep test enforces the constraint statically. If
//! `src/scenarios/s1_volume.rs` does not yet exist (the file lands in a
//! subsequent commit of the same batch), the test passes trivially —
//! enforcement begins the moment the file appears.
//!
//! `#[cfg(test)] mod tests { ... }` blocks are stripped before grep
//! (mirrors `tests/c_no_retry_backoff_throttle.rs`'s carve-out). Test code
//! is allowed to use `chunk` / `partition` for fixture setup.

use std::path::PathBuf;

use regex::Regex;

/// Resolve the absolute path of `src/scenarios/s1_volume.rs`.
fn s1_volume_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/scenarios/s1_volume.rs")
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
/// mention `chunk` / `partition` / `split` legitimately (e.g. "we do NOT
/// chunk the input set") must not trip the test.
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

#[test]
fn s1_volume_does_not_pre_partition_utxos() {
    let path = s1_volume_path();
    if !path.exists() {
        // File not yet introduced — passes trivially. Enforcement begins
        // the moment `src/scenarios/s1_volume.rs` appears in the tree.
        return;
    }
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let production = strip_comments(&strip_cfg_test_mods(&raw));

    let patterns = [
        r"\.chunks?\s*\(",
        r"\.chunks_exact\s*\(",
        r"\.partition\s*\(",
        r"\.split_at\s*\(",
        r"\.split\s*\(\s*\d",
    ];
    let mut hits = Vec::new();
    for p in patterns {
        let re = Regex::new(p).expect("regex compiles");
        for (line_idx, line) in production.lines().enumerate() {
            if re.is_match(line) {
                hits.push(format!(
                    "{}:{}: matched `{p}`: {}",
                    path.display(),
                    line_idx + 1,
                    line.trim(),
                ));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "S1 anti-pre-partition guard tripped: pre-partitioning the input set \
         defeats the wallet's own selection-logic measurement (AC-30, S1 \
         send-loop discipline).\n  {}",
        hits.join("\n  "),
    );
}
