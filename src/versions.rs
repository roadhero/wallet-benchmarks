//! Version discovery for the result profile's `versions` block.
//!
//! Per `analysis/RESULT_PROFILE_SCHEMA.md §3` the harness records, for each
//! tracked component, a `BinaryVersion { tag, commit }` pair. At least one of
//! `tag` / `commit` must be populated; absent both, the schema requires a
//! `null` pair which here surfaces as `(None, None)`.
//!
//! Probing strategy (decided per `analysis/DESIGN_ADDENDUM.md §3` and recorded
//! in `analysis/API_DRIFT.md`):
//!
//! * `harness.commit`: runtime `git rev-parse HEAD` in the harness source dir.
//!   No `build.rs` / `vergen`: the published tari crates inspected for
//!   precedent (`tari_common`, `tari_common_types`, `minotari_node_wallet_client`)
//!   use neither — they use `tari_features::resolver::build_features` for
//!   feature gating and nothing else. A runtime subprocess call is the
//!   smallest possible mirror.
//! * `minotari_console_wallet.{tag,commit}` and `minotari_cli.{tag,commit}`:
//!   subprocess `<binary> --version`, parsed via a `name <semver>` /
//!   `name <semver>+<commit>` matcher. Best-effort: a missing binary or a
//!   non-zero exit yields `BinaryVersion { tag: None, commit: None }` — the
//!   result profile's `errors.details` block surfaces the absence later.
//! * `base_node.{tag,commit}`: the `BaseNodeWalletClient` trait on
//!   `minotari_node_wallet_client = "5.3.1"` does NOT expose a version
//!   endpoint (verified by reading the trait at the published source). The
//!   probe falls back to the canonical pinned version `v5.3.1` documented in
//!   DESIGN_ADDENDUM §3 — that's the contract for what the operator pointed
//!   the harness at, not a runtime check. Live verification will move into
//!   step 3e when wallet_lifecycle/broadcast lands.

use std::{path::Path, process::Command};

use serde::{Deserialize, Serialize};

const LOG_TARGET: &str = "c::versions";

/// Pinned base-node version recorded when no live endpoint is queried.
/// Sourced from `DESIGN_ADDENDUM.md §3 AC-27 recording`.
const PINNED_BASE_NODE_TAG: &str = "v5.3.1";
const PINNED_BASE_NODE_COMMIT: &str = "5d6ef11bb89caa34fe9ee676d608f273db90038d";

/// A single component's recorded version. Per `RESULT_PROFILE_SCHEMA.md §3`
/// at least one of `tag` / `commit` MUST be populated; both `None` is
/// reserved for the documented graceful-failure case (binary absent).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryVersion {
    /// Release tag (e.g. `"v5.3.1"`) or `None` if the binary reports only a
    /// commit-based version.
    pub tag: Option<String>,
    /// Forty-character lowercase hex commit hash, or `None` if unavailable.
    pub commit: Option<String>,
}

/// All five components tracked by `RESULT_PROFILE_SCHEMA.md §3`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versions {
    pub minotari_console_wallet: BinaryVersion,
    pub minotari_cli: BinaryVersion,
    pub base_node: BinaryVersion,
    pub harness: BinaryVersion,
}

/// Probe a `<binary> --version` style command, mapping its first line into a
/// [`BinaryVersion`]. Missing binaries or non-zero exits yield
/// `BinaryVersion::default()` — never a panic, never an error: the result
/// profile records absence rather than aborting the run.
pub fn probe_binary(binary_path: &Path) -> anyhow::Result<BinaryVersion> {
    log::debug!(
        target: LOG_TARGET,
        "probing --version of {}",
        binary_path.display(),
    );
    let output = match Command::new(binary_path).arg("--version").output() {
        Ok(out) => out,
        Err(err) => {
            log::warn!(
                target: LOG_TARGET,
                "binary {} not invocable: {err}",
                binary_path.display(),
            );
            return Ok(BinaryVersion::default());
        }
    };
    if !output.status.success() {
        log::warn!(
            target: LOG_TARGET,
            "binary {} returned non-zero exit",
            binary_path.display(),
        );
        return Ok(BinaryVersion::default());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().unwrap_or("").trim();
    Ok(parse_version_line(first_line))
}

/// Parse a `<name> <semver>` or `<name> <semver>+<commit>` style version
/// line. Returns `BinaryVersion::default()` on shapes we cannot recognise —
/// the harness records absence rather than mis-recording.
///
/// Examples (per `--version` output captured from cargo/rustc on the dev
/// host and the documented tari binary layout):
///
/// * `"cargo 1.95.0 (f2d3ce0bd 2026-03-21)"` → tag=`"1.95.0"`, commit=None.
/// * `"minotari-console-wallet 5.3.1+5d6ef11"` → tag=`"5.3.1"`, commit=
///   `"5d6ef11"`.
/// * `""` (empty) → `(None, None)`.
fn parse_version_line(line: &str) -> BinaryVersion {
    let mut tokens = line.split_whitespace();
    let _name = tokens.next();
    let version = match tokens.next() {
        Some(v) => v,
        None => return BinaryVersion::default(),
    };
    // The version token may be `1.2.3` or `1.2.3+commitsha`. Split once on `+`
    // — anything before is the tag candidate, anything after is the commit.
    let (tag_candidate, commit) = match version.split_once('+') {
        Some((t, c)) => (t, Some(c.to_string())),
        None => (version, None),
    };
    // A bare digit-string like `1.95.0` is a tag; preserve as-is so the
    // operator can compare against the canonical release tag form.
    let tag = if tag_candidate.is_empty() {
        None
    } else {
        Some(tag_candidate.to_string())
    };
    BinaryVersion { tag, commit }
}

/// Capture the harness's own git HEAD at runtime. Runs `git rev-parse HEAD`
/// from the source dir; if git is absent or the directory is not a git tree,
/// returns `None` rather than failing the harness — recorded as `commit:
/// None` in the profile.
pub fn harness_commit() -> Option<String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        log::warn!(target: LOG_TARGET, "git rev-parse HEAD returned non-zero");
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase();
    if raw.len() == 40 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(raw)
    } else {
        log::warn!(target: LOG_TARGET, "git rev-parse HEAD output is not a 40-hex sha: {raw}");
        None
    }
}

/// Build the full [`Versions`] block.
///
/// `minotari_binary_path` and `minotari_console_wallet_binary_path` are
/// caller-supplied (from the harness config); the function calls each
/// `<binary> --version` and falls back to `BinaryVersion::default()` if
/// either is unreachable. `base_node` is recorded from the pinned constants
/// per `DESIGN_ADDENDUM.md §3`. `harness` uses [`harness_commit`].
pub fn capture(
    minotari_console_wallet_binary: &Path,
    minotari_cli_binary: &Path,
) -> anyhow::Result<Versions> {
    log::debug!(target: LOG_TARGET, "capturing versions block");
    Ok(Versions {
        minotari_console_wallet: probe_binary(minotari_console_wallet_binary)?,
        minotari_cli: probe_binary(minotari_cli_binary)?,
        base_node: BinaryVersion {
            tag: Some(PINNED_BASE_NODE_TAG.to_string()),
            commit: Some(PINNED_BASE_NODE_COMMIT.to_string()),
        },
        harness: BinaryVersion {
            tag: None,
            commit: harness_commit(),
        },
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn binary_version_parses_known_format() {
        let v = parse_version_line("cargo 1.95.0 (f2d3ce0bd 2026-03-21)");
        assert_eq!(v.tag.as_deref(), Some("1.95.0"));
        assert_eq!(v.commit, None);
    }

    #[test]
    fn binary_version_parses_commit_suffix() {
        let v = parse_version_line("minotari-console-wallet 5.3.1+5d6ef11");
        assert_eq!(v.tag.as_deref(), Some("5.3.1"));
        assert_eq!(v.commit.as_deref(), Some("5d6ef11"));
    }

    #[test]
    fn binary_version_handles_empty_line() {
        let v = parse_version_line("");
        assert_eq!(v, BinaryVersion::default());
    }

    #[test]
    fn binary_version_handles_only_name() {
        let v = parse_version_line("just-a-name");
        assert_eq!(v, BinaryVersion::default());
    }

    #[test]
    fn probe_binary_handles_missing_binary() {
        let v = probe_binary(&PathBuf::from("/nonexistent/wallet-benchmarks-test-binary"))
            .expect("probe_binary never errors on a missing binary");
        assert_eq!(v, BinaryVersion::default());
    }

    #[test]
    fn capture_records_pinned_base_node_version() {
        let v = capture(
            &PathBuf::from("/nonexistent/console-wallet"),
            &PathBuf::from("/nonexistent/minotari"),
        )
        .expect("capture never errors");
        assert_eq!(v.base_node.tag.as_deref(), Some(PINNED_BASE_NODE_TAG));
        assert_eq!(v.base_node.commit.as_deref(), Some(PINNED_BASE_NODE_COMMIT));
        // Unreachable binaries record absence, not an error.
        assert_eq!(v.minotari_console_wallet, BinaryVersion::default());
        assert_eq!(v.minotari_cli, BinaryVersion::default());
    }

    #[test]
    fn harness_commit_returns_a_40_hex_sha_in_this_repo() {
        // The workspace IS a git tree, so this must succeed in any environment
        // where the harness builds. If git is genuinely absent, the test
        // can't run and that's a build-environment issue not a code defect.
        let commit = harness_commit();
        if let Some(sha) = commit {
            assert_eq!(
                sha.len(),
                40,
                "git rev-parse HEAD returned non-40 sha: {sha}"
            );
            assert!(
                sha.chars().all(|c| c.is_ascii_hexdigit()),
                "non-hex sha: {sha}"
            );
        } else {
            // Acceptable iff git isn't on PATH in the test environment.
            let git_present = Command::new("git")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(
                !git_present,
                "git is present but harness_commit returned None — wallet-benchmarks repo state",
            );
        }
    }
}
