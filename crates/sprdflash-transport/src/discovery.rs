// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! USB serial-port discovery by VID/PID, and the beacon-window wait.

use std::time::{Duration, Instant};

use crate::{DOWNLOAD_PID, DOWNLOAD_VID, MODULE_VID};

/// A discovered serial port with its USB identity.
#[derive(Debug, Clone)]
pub struct PortInfo {
    /// OS port name (e.g. `COM34`, `/dev/ttyUSB0`).
    pub name: String,
    /// USB vendor id, if a USB port.
    pub vid: Option<u16>,
    /// USB product id, if a USB port.
    pub pid: Option<u16>,
    /// Product string, if known.
    pub product: Option<String>,
}

fn ports() -> Vec<PortInfo> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let (vid, pid, product) = match p.port_type {
                serialport::SerialPortType::UsbPort(u) => (Some(u.vid), Some(u.pid), u.product),
                _ => (None, None, None),
            };
            PortInfo {
                name: p.port_name,
                vid,
                pid,
                product,
            }
        })
        .collect()
}

/// True if a serial port with this exact OS name is currently enumerated. Used
/// to track one fixture's device across insert/remove without confusing it with
/// a neighbouring station's port.
#[must_use]
pub fn is_present(name: &str) -> bool {
    ports().iter().any(|p| p.name == name)
}

/// The download-mode port (`0525:a4a7`), if present.
#[must_use]
pub fn find_download_port() -> Option<PortInfo> {
    ports()
        .into_iter()
        .find(|p| p.vid == Some(DOWNLOAD_VID) && p.pid == Some(DOWNLOAD_PID))
}

/// All normal-mode module ports (`1782:xxxx`).
#[must_use]
pub fn find_module_ports() -> Vec<PortInfo> {
    ports()
        .into_iter()
        .filter(|p| p.vid == Some(MODULE_VID))
        .collect()
}

/// Wait up to `timeout` for the download port to appear, polling tightly so we
/// catch the brief beacon window right after `AT*DOWNLOAD`.
#[must_use]
pub fn wait_for_download_port(timeout: Duration) -> Option<PortInfo> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(p) = find_download_port() {
            return Some(p);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Wait up to `timeout` for the module to re-enumerate in normal mode (post-flash
/// boot check).
#[must_use]
pub fn wait_for_module(timeout: Duration) -> Vec<PortInfo> {
    let deadline = Instant::now() + timeout;
    loop {
        let m = find_module_ports();
        if !m.is_empty() || Instant::now() >= deadline {
            return m;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
