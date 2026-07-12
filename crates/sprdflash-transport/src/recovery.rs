// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! Getting the module into download mode, and recovering a stuck agent.
//!
//! Happy path: [`enter_download_mode`] sends `AT*DOWNLOAD=1` on the module's AT
//! port. Edge case (a wedged download agent): [`restart_download_device`]
//! re-enumerates the USB device so a fresh agent comes up. On a real line the
//! most reliable recovery is a fixture-controlled USB power cycle (e.g.
//! `uhubctl`); [`RecoveryStrategy`] documents the options for the orchestrator.

use std::io::Write;
use std::time::Duration;

/// How a station recovers a module that will not connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStrategy {
    /// Re-send `AT*DOWNLOAD=1` on the AT port.
    AtRedownload,
    /// Re-enumerate the USB device (Windows `pnputil`, best effort).
    UsbRestart,
    /// Ask a fixture to power-cycle the USB port (most reliable on a line).
    PowerCycle,
}

/// Send `AT*DOWNLOAD=1` on `at_port` to reboot the module into download mode.
pub fn enter_download_mode(at_port: &str) -> Result<(), serialport::Error> {
    let mut p = serialport::new(at_port, 115_200)
        .timeout(Duration::from_millis(500))
        .open()?;
    let _ = p.write_all(b"AT*DOWNLOAD=1\r\n");
    let _ = p.flush();
    Ok(())
}

/// Best-effort re-enumeration of the download device on Windows via `pnputil`.
///
/// Requires elevation; returns the command's success. On non-Windows this is a
/// no-op returning `false` (use `usbip`/`uhubctl` at the fixture instead).
#[cfg(windows)]
#[must_use]
pub fn restart_download_device() -> bool {
    use std::process::Command;
    // Find the device instance id under the USB enum key.
    let out = Command::new("reg")
        .args([
            "query",
            r"HKLM\SYSTEM\CurrentControlSet\Enum\USB\VID_0525&PID_A4A7",
        ])
        .output();
    let Ok(out) = out else { return false };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(instance) = text
        .lines()
        .filter_map(|l| l.trim().rsplit('\\').next())
        .find(|s| !s.is_empty())
    else {
        return false;
    };
    let path = format!(r"USB\VID_0525&PID_A4A7\{instance}");
    Command::new("pnputil")
        .args(["/restart-device", &path])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Non-Windows stub (use `usbip`/`uhubctl` at the fixture).
#[cfg(not(windows))]
#[must_use]
pub fn restart_download_device() -> bool {
    false
}
