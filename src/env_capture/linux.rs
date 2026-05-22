//! Linux-specific host capture for [`super::Environment`].
//!
//! Source commands (per `analysis/DESIGN.md §Environment-disclosure` table):
//!   * `cpu_model` — first `model name` line in `/proc/cpuinfo`.
//!   * `ram_bytes` — `MemTotal:` line in `/proc/meminfo`, value in kB × 1024.
//!   * `disk_type` — `lsblk -d -o name,rota`; `rota=0` → ssd (refined to
//!     `nvme-ssd` when the device name contains `nvme`), `rota=1` → hdd.
//!   * `os` — `uname -srm`.
//!
//! Per-field failures degrade gracefully to `"unknown"` (or zero for
//! `ram_bytes`) — the schema requires every field to be non-null, never the
//! whole-block failure mode.

use std::process::Command;

use super::Environment;

const LOG_TARGET: &str = "c::env_capture::linux";

/// Capture the four host-dependent fields. `network_path_to_base_node` is
/// computed by the caller in [`super::LiveEnvCapture::capture`].
pub(super) fn capture() -> anyhow::Result<Environment> {
    log::debug!(target: LOG_TARGET, "capturing linux host environment");
    Ok(Environment {
        cpu_model: cpu_model().unwrap_or_else(|| "unknown".to_string()),
        ram_bytes: ram_bytes().unwrap_or(0),
        disk_type: disk_type().unwrap_or_else(|| "unknown".to_string()),
        os: os_uname().unwrap_or_else(|| "unknown".to_string()),
        // Filled by the caller once it has the base-node URL.
        network_path_to_base_node: String::new(),
    })
}

fn cpu_model() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("model name") {
            if let Some(value) = rest.split_once(':').map(|(_, v)| v.trim()) {
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn ram_bytes() -> Option<u64> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            // Format: `MemTotal:       65927828 kB`
            let trimmed = rest.trim();
            let kb_str = trimmed.split_whitespace().next()?;
            let kb: u64 = kb_str.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

fn disk_type() -> Option<String> {
    let output = Command::new("lsblk")
        .args(["-d", "-o", "name,rota"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut saw_ssd = false;
    let mut saw_nvme = false;
    let mut saw_hdd = false;
    for line in stdout.lines().skip(1) {
        let mut cols = line.split_whitespace();
        let name = match cols.next() {
            Some(n) => n,
            None => continue,
        };
        let rota = match cols.next() {
            Some(r) => r,
            None => continue,
        };
        match rota {
            "0" => {
                saw_ssd = true;
                if name.to_ascii_lowercase().contains("nvme") {
                    saw_nvme = true;
                }
            }
            "1" => saw_hdd = true,
            _ => {}
        }
    }
    if saw_nvme {
        Some("nvme-ssd".to_string())
    } else if saw_ssd {
        Some("ssd".to_string())
    } else if saw_hdd {
        Some("hdd".to_string())
    } else {
        None
    }
}

fn os_uname() -> Option<String> {
    let output = Command::new("uname").arg("-srm").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
