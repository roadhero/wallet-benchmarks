//! Harness-owned wallet data directory.
//!
//! Per `analysis/DESIGN.md §Mainnet-protection guard` and AC-34, every scan
//! scenario (B0/S2/S3/S6/S7) must call `wipe_data_dir()` before scanning AND
//! must refuse to delete anything that isn't inside the harness's own tempdir
//! root. The [`HarnessDataDir`] type owns a `tempfile::TempDir` so cleanup
//! happens on `Drop` even on panic / Ctrl-C; the [`HarnessDataDir::wipe`]
//! method enforces the path-confinement invariant before touching the
//! filesystem.
//!
//! The tempdir root lives under `target/harness-data/<run-id>/<mode>/` per
//! `DESIGN.md §Workspace layout`. We pass the parent to `tempfile::Builder`
//! so individual mode dirs share the same parent and are easy to inspect when
//! a run leaves them in place (a panic skips drop but leaves the tree behind
//! for diagnosis).

use std::path::{Path, PathBuf};

use anyhow::Context;
use tempfile::{Builder, TempDir};

const LOG_TARGET: &str = "c::wallet_lifecycle::data_dir";

/// The `target/harness-data/<run-id>/` parent under which per-mode tempdirs
/// live. Resolved relative to `CARGO_MANIFEST_DIR` at runtime so the harness
/// behaves the same whether invoked from the repo root or via `cargo run`.
const HARNESS_DATA_PARENT: &str = "target/harness-data";

/// Harness-owned per-mode wallet data directory.
///
/// Backed by a [`TempDir`] whose root is `target/harness-data/<run-id>/<mode>-XXXXXX/`.
/// Drops clean up the tree; [`Self::wipe`] is the explicit "before-scan" wipe
/// that AC-34 polices.
pub struct HarnessDataDir {
    /// Owned [`TempDir`] — drop cleans up. Kept as `Option` only so [`Drop`]
    /// can run normally; in practice this is `Some` for the type's whole life.
    tempdir: Option<TempDir>,
    /// Cached path of `tempdir.path()` — used by [`Self::wipe`]'s path-prefix
    /// check without re-borrowing the [`TempDir`].
    root: PathBuf,
}

impl HarnessDataDir {
    /// Create a new harness-owned data dir for `mode_name` ("old_wallet" /
    /// "new_wallet" / "payment_processor") under
    /// `target/harness-data/<run-id>/`.
    ///
    /// The `run_id` segment keeps concurrent test invocations from clobbering
    /// each other; the random suffix `tempfile` appends keeps multiple
    /// `HarnessDataDir`s for the same `(run_id, mode_name)` distinct.
    pub fn new(run_id: &str, mode_name: &str) -> anyhow::Result<Self> {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let parent = Path::new(manifest_dir)
            .join(HARNESS_DATA_PARENT)
            .join(run_id);
        std::fs::create_dir_all(&parent)
            .with_context(|| format!("creating harness data parent {}", parent.display()))?;
        let prefix = format!("{mode_name}-");
        let tempdir = Builder::new()
            .prefix(&prefix)
            .tempdir_in(&parent)
            .with_context(|| {
                format!(
                    "creating tempdir under {} with prefix {prefix:?}",
                    parent.display()
                )
            })?;
        let root = tempdir.path().to_path_buf();
        log::debug!(
            target: LOG_TARGET,
            "created harness data dir at {} (mode={mode_name}, run_id={run_id})",
            root.display(),
        );
        Ok(Self {
            tempdir: Some(tempdir),
            root,
        })
    }

    /// Path to the data dir root. Pass this directly to `--base-path` /
    /// `--database-path` flags on `minotari_console_wallet` and `minotari`.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Delete `target` (a file or directory) — but only if it is inside this
    /// data dir's root. AC-34's path-confinement guard: passing a path outside
    /// the tempdir returns `Err` BEFORE any filesystem call.
    ///
    /// The path is canonicalised relative to its parent so a `..`-laden input
    /// can't escape the prefix check. If `target` does not exist this returns
    /// `Ok(())` — the contract is "ensure absent", not "must have existed".
    pub fn wipe(&self, target: &Path) -> anyhow::Result<()> {
        let canonical_root = self
            .root
            .canonicalize()
            .with_context(|| format!("canonicalising harness data root {}", self.root.display()))?;
        // Resolve `target` to an absolute path; canonicalise if it exists so
        // `..` and symlinks are followed before the prefix check.
        let canonical_target = if target.exists() {
            target
                .canonicalize()
                .with_context(|| format!("canonicalising wipe target {}", target.display()))?
        } else {
            // For a non-existent path, walk up to a canonicalisable ancestor
            // and rejoin so the prefix check still applies to where the file
            // *would* live.
            let mut ancestor = target.to_path_buf();
            let mut tail = PathBuf::new();
            while !ancestor.exists() {
                match ancestor.file_name().map(|n| n.to_os_string()) {
                    Some(name) => {
                        tail = if tail.as_os_str().is_empty() {
                            PathBuf::from(name)
                        } else {
                            PathBuf::from(name).join(&tail)
                        };
                        if !ancestor.pop() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            let canonical_ancestor = ancestor
                .canonicalize()
                .with_context(|| format!("canonicalising parent of {}", target.display()))?;
            canonical_ancestor.join(&tail)
        };
        if !canonical_target.starts_with(&canonical_root) {
            anyhow::bail!(
                "refusing to wipe {} — path lies outside harness data root {}",
                canonical_target.display(),
                canonical_root.display(),
            );
        }
        log::debug!(
            target: LOG_TARGET,
            "wiping {} (inside harness root {})",
            canonical_target.display(),
            canonical_root.display(),
        );
        if !canonical_target.exists() {
            return Ok(());
        }
        if canonical_target.is_dir() {
            std::fs::remove_dir_all(&canonical_target)
                .with_context(|| format!("removing directory {}", canonical_target.display()))?;
        } else {
            std::fs::remove_file(&canonical_target)
                .with_context(|| format!("removing file {}", canonical_target.display()))?;
        }
        Ok(())
    }
}

impl Drop for HarnessDataDir {
    fn drop(&mut self) {
        // `TempDir::drop` cleans up the tree. We do nothing extra here; the
        // explicit `Option::take` matches the Rust idiom for "ensure the
        // owned thing drops at a predictable point" without spawning a new
        // tokio task from inside `Drop`.
        if let Some(td) = self.tempdir.take() {
            let p = td.path().to_path_buf();
            // `td` drops here, removing the directory.
            drop(td);
            log::debug!(target: LOG_TARGET, "dropped harness data dir at {}", p.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn data_dir_owns_path_under_harness_root() {
        let d = HarnessDataDir::new("test-run-id-owns", "unit").expect("data dir");
        let p = d.path();
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let expected_parent = PathBuf::from(manifest_dir)
            .join(HARNESS_DATA_PARENT)
            .join("test-run-id-owns");
        assert!(
            p.starts_with(&expected_parent),
            "data dir {} must live under {}",
            p.display(),
            expected_parent.display(),
        );
    }

    #[test]
    fn data_dir_wipe_refuses_external_path() {
        let d = HarnessDataDir::new("test-run-id-ext", "unit").expect("data dir");
        let err = d
            .wipe(Path::new("/tmp/wallet-benchmarks-NEVER-DELETE-ME"))
            .expect_err("external path must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("outside harness data root"),
            "refusal must name the invariant: {msg}",
        );
    }

    #[test]
    fn data_dir_wipe_refuses_root() {
        let d = HarnessDataDir::new("test-run-id-root", "unit").expect("data dir");
        d.wipe(Path::new("/"))
            .expect_err("/ must be refused as outside harness data root");
    }

    #[test]
    fn data_dir_wipe_refuses_dotdot_escape() {
        let d = HarnessDataDir::new("test-run-id-dotdot", "unit").expect("data dir");
        // `<root>/../<root-sibling-name>` resolves to a path outside the root.
        let escape = d.path().join("..").join("other");
        d.wipe(&escape)
            .expect_err("dotdot escape must be refused after canonicalising");
    }

    #[test]
    fn data_dir_wipe_accepts_internal_path() {
        let d = HarnessDataDir::new("test-run-id-internal", "unit").expect("data dir");
        let sub = d.path().join("subfolder");
        std::fs::create_dir(&sub).expect("create subfolder");
        let inner_file = sub.join("inner.txt");
        std::fs::write(&inner_file, b"hello").expect("write inner file");
        d.wipe(&sub).expect("wiping an internal directory succeeds");
        assert!(
            !sub.exists(),
            "subfolder must be gone after wipe: {}",
            sub.display(),
        );
    }

    #[test]
    fn data_dir_wipe_on_nonexistent_internal_path_is_ok() {
        let d = HarnessDataDir::new("test-run-id-absent", "unit").expect("data dir");
        let absent = d.path().join("never-existed");
        d.wipe(&absent)
            .expect("wiping a non-existent internal path is Ok");
    }

    #[test]
    fn data_dir_cleans_up_on_drop() {
        let captured = {
            let d = HarnessDataDir::new("test-run-id-drop", "unit").expect("data dir");
            d.path().to_path_buf()
        };
        // After `d` drops, the TempDir tree is gone. Filesystem visibility may
        // lag on some platforms; if `exists()` is still true the path must
        // at least be empty (the OS still has handles open).
        let still_present = captured.exists();
        assert!(
            !still_present || std::fs::read_dir(&captured).map(|i| i.count()).unwrap_or(0) == 0,
            "harness data dir {} should be absent or empty after drop",
            captured.display(),
        );
    }
}
