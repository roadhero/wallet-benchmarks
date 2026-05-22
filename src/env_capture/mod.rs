//! Host environment capture for the result profile's `environment` block.
//!
//! Per `analysis/RESULT_PROFILE_SCHEMA.md §2` and `analysis/DESIGN.md
//! §Environment-disclosure`, every harness run records five host facts:
//! `cpu_model`, `ram_bytes`, `disk_type`, `os`, and `network_path_to_base_node`.
//!
//! The first four are OS-dependent and live in [`linux`] / [`macos`]; the fifth
//! is URL-derived and so lives here. The [`EnvCapture`] trait abstracts the
//! capture site so unit tests inject deterministic values via [`FakeEnvCapture`]
//! without depending on the test host's actual hardware.
//!
//! `wallet-benchmarks` ships first-class support for Linux and macOS only — any
//! other target triggers a compile-time error rather than a runtime fallback,
//! mirroring the bounty's stated scope.
//!
//! See `analysis/API_DRIFT.md` for the rationale behind the trait split.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("wallet-benchmarks supports Linux and macOS only");

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use serde::{Deserialize, Serialize};
use url::Url;

const LOG_TARGET: &str = "c::env_capture";

/// Loopback hosts treated as `network_path_to_base_node = "local"`. The
/// bracketed IPv6 form (`[::1]`) is what `url::Url::host_str` returns for an
/// IPv6 URL, so the bracketed variant has to appear here verbatim.
const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "::1", "[::1]", "localhost"];

/// Captured host environment — the five fields of
/// `RESULT_PROFILE_SCHEMA.md §2`. All fields are non-null by contract; the
/// per-OS modules substitute `"unknown"` if a capture command fails so this
/// stays true even on degraded hosts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Environment {
    /// Schema: `cpu_model`. Example: `"AMD Ryzen 9 5950X 16-Core Processor"`.
    pub cpu_model: String,
    /// Schema: `ram_bytes`. Total physical RAM in bytes.
    pub ram_bytes: u64,
    /// Schema: `disk_type`. One of `"nvme-ssd"`, `"sata-ssd"`, `"ssd"`,
    /// `"hdd"`, `"unknown"`.
    pub disk_type: String,
    /// Schema: `os`. Output of `uname -srm`.
    pub os: String,
    /// Schema: `network_path_to_base_node`. `"local"` when the base-node host
    /// is a loopback address, `"remote"` otherwise.
    pub network_path_to_base_node: String,
}

/// Captures the host environment for the result profile.
///
/// Implementations are deliberately small — the OS-specific work lives in the
/// gated [`linux`] / [`macos`] modules, the trait only exists to let tests
/// substitute [`FakeEnvCapture`] without reaching for the real `/proc` paths.
pub trait EnvCapture {
    /// Capture the five `environment` fields. `base_node_url` is required so
    /// the URL-derived `network_path_to_base_node` field can be computed
    /// without the trait having to thread the harness `Config` through.
    fn capture(&self, base_node_url: &Url) -> anyhow::Result<Environment>;
}

/// Production [`EnvCapture`] — delegates to the OS-gated module for the four
/// host-dependent fields and computes `network_path_to_base_node` from the
/// supplied URL.
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveEnvCapture;

impl EnvCapture for LiveEnvCapture {
    fn capture(&self, base_node_url: &Url) -> anyhow::Result<Environment> {
        log::debug!(target: LOG_TARGET, "capturing host environment");
        #[cfg(target_os = "linux")]
        let mut env = linux::capture()?;
        #[cfg(target_os = "macos")]
        let mut env = macos::capture()?;
        env.network_path_to_base_node = classify_network_path(base_node_url);
        Ok(env)
    }
}

/// Returns `"local"` when `url`'s host is a loopback address, `"remote"`
/// otherwise. Hosts that fail to parse (e.g. unix-socket URLs) are treated as
/// remote — the harness is documented as targeting an HTTP base node, so a
/// missing host string indicates an exotic configuration that the schema
/// treats as remote-by-default.
fn classify_network_path(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    if LOOPBACK_HOSTS.iter().any(|&h| h.eq_ignore_ascii_case(host)) {
        "local".to_string()
    } else {
        "remote".to_string()
    }
}

/// Deterministic [`EnvCapture`] for unit tests. Returns a fixed [`Environment`]
/// regardless of host — lets scenario tests assert against schema-shape
/// without depending on the test machine's hardware.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct FakeEnvCapture {
    inner: Environment,
}

#[cfg(test)]
impl FakeEnvCapture {
    /// Builds a `FakeEnvCapture` that returns the canonical example
    /// environment from `RESULT_PROFILE_SCHEMA.md §2`.
    pub fn new() -> Self {
        Self {
            inner: Environment {
                cpu_model: "AMD Ryzen 9 5950X 16-Core Processor".to_string(),
                ram_bytes: 68_719_476_736,
                disk_type: "nvme-ssd".to_string(),
                os: "Linux 6.5.0-21-generic x86_64".to_string(),
                network_path_to_base_node: "remote".to_string(),
            },
        }
    }
}

#[cfg(test)]
impl Default for FakeEnvCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl EnvCapture for FakeEnvCapture {
    fn capture(&self, base_node_url: &Url) -> anyhow::Result<Environment> {
        let mut env = self.inner.clone();
        env.network_path_to_base_node = classify_network_path(base_node_url);
        Ok(env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("test url parses")
    }

    #[test]
    fn fake_env_capture_returns_fixed_values() {
        let fake = FakeEnvCapture::new();
        let env = fake
            .capture(&url("https://rpc.esmeralda.tari.com"))
            .expect("fake capture succeeds");
        assert_eq!(env.cpu_model, "AMD Ryzen 9 5950X 16-Core Processor");
        assert_eq!(env.ram_bytes, 68_719_476_736);
        assert_eq!(env.disk_type, "nvme-ssd");
        assert_eq!(env.os, "Linux 6.5.0-21-generic x86_64");
        assert!(!env.cpu_model.is_empty());
        assert!(!env.disk_type.is_empty());
        assert!(!env.os.is_empty());
        assert!(!env.network_path_to_base_node.is_empty());
    }

    #[test]
    fn network_path_local_for_loopback() {
        assert_eq!(
            classify_network_path(&url("http://127.0.0.1:18142")),
            "local"
        );
        assert_eq!(classify_network_path(&url("http://[::1]:18142")), "local");
        assert_eq!(
            classify_network_path(&url("http://localhost:18142")),
            "local"
        );
    }

    #[test]
    fn network_path_remote_for_public_host() {
        assert_eq!(
            classify_network_path(&url("https://rpc.esmeralda.tari.com")),
            "remote",
        );
    }

    #[test]
    fn fake_env_capture_recomputes_network_path_from_url() {
        let fake = FakeEnvCapture::new();
        let local = fake
            .capture(&url("http://127.0.0.1:18142"))
            .expect("fake capture local");
        assert_eq!(local.network_path_to_base_node, "local");
        let remote = fake
            .capture(&url("https://rpc.esmeralda.tari.com"))
            .expect("fake capture remote");
        assert_eq!(remote.network_path_to_base_node, "remote");
    }

    /// Live-capture sanity for the macOS branch (the dev host is darwin per
    /// the workspace's CLAUDE.md note). On Linux this test compiles out so a
    /// CI on either platform sees a sanity check tailored to it.
    #[cfg(target_os = "macos")]
    #[test]
    fn live_capture_returns_non_empty_fields_on_macos() {
        let env = LiveEnvCapture
            .capture(&url("https://rpc.esmeralda.tari.com"))
            .expect("live capture succeeds on macos");
        assert!(!env.cpu_model.is_empty(), "cpu_model must be non-empty");
        assert!(env.ram_bytes > 0, "ram_bytes must be > 0");
        assert!(!env.os.is_empty(), "os must be non-empty");
        assert_eq!(env.network_path_to_base_node, "remote");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_capture_returns_non_empty_fields_on_linux() {
        let env = LiveEnvCapture
            .capture(&url("https://rpc.esmeralda.tari.com"))
            .expect("live capture succeeds on linux");
        assert!(!env.cpu_model.is_empty(), "cpu_model must be non-empty");
        assert!(env.ram_bytes > 0, "ram_bytes must be > 0");
        assert!(!env.os.is_empty(), "os must be non-empty");
        assert_eq!(env.network_path_to_base_node, "remote");
    }
}
