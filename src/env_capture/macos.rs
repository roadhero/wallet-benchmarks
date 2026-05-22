//! macOS-specific host capture for [`super::Environment`].
//!
//! Source commands (per `analysis/DESIGN.md §Environment-disclosure` table):
//!   * `cpu_model` — `sysctl -n machdep.cpu.brand_string`.
//!   * `ram_bytes` — `sysctl -n hw.memsize` (already bytes; no multiplier).
//!   * `disk_type` — `diskutil info /`; `Solid State: Yes` → ssd, refined to
//!     `nvme-ssd` when `Protocol: PCI-Express`.
//!   * `os` — `uname -srm`.
//!
//! Per-field failures degrade gracefully to `"unknown"` (or zero for
//! `ram_bytes`) — the schema requires every field to be non-null.

use std::process::Command;

use super::Environment;

const LOG_TARGET: &str = "c::env_capture::macos";

/// Capture the four host-dependent fields. `network_path_to_base_node` is
/// computed by the caller in [`super::LiveEnvCapture::capture`].
pub(super) fn capture() -> anyhow::Result<Environment> {
    log::debug!(target: LOG_TARGET, "capturing macos host environment");
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
    let output = Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()?;
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

fn ram_bytes() -> Option<u64> {
    let output = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    raw.trim().parse::<u64>().ok()
}

fn disk_type() -> Option<String> {
    let output = Command::new("diskutil").args(["info", "/"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut solid_state = false;
    let mut pci_express = false;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Solid State:") {
            if rest.trim().eq_ignore_ascii_case("Yes") {
                solid_state = true;
            }
        } else if let Some(rest) = line.strip_prefix("Protocol:") {
            if rest.trim().eq_ignore_ascii_case("PCI-Express") {
                pci_express = true;
            }
        }
    }
    if pci_express && solid_state {
        Some("nvme-ssd".to_string())
    } else if solid_state {
        Some("ssd".to_string())
    } else {
        // `diskutil info /` ran but reported neither solid-state nor PCI-Express;
        // most likely a rotating drive on macOS (vanishingly rare in 2026), or a
        // bind-mounted volume whose info we can't classify. Surface as hdd.
        Some("hdd".to_string())
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
