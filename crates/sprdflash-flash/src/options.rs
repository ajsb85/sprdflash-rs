// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! Tunables for a flash run.

use std::time::Duration;

/// Options controlling a flash run — including the speed levers.
#[derive(Debug, Clone)]
pub struct FlashOptions {
    /// Replay the PAC's format markers (erase + NV template + PREPACK). Needed
    /// only for a firmware-TYPE change (e.g. LuatOS ⇄ CSDK).
    pub format: bool,
    /// MIDST data chunk size. Larger = fewer round trips (the dominant cost on
    /// the USB gadget). 528 matches the reference; 2048–4096 is much faster.
    pub chunk: usize,
    /// If set, issue `CHANGE_BAUD` to this rate after FDL2 comes up.
    pub baud: Option<u32>,
    /// Issue `DISABLE_TRANSCODE` so the bulk data phase skips HDLC escaping.
    pub disable_transcode: bool,
    /// Send `NORMAL_RESET` when done (so the module boots the new firmware).
    pub reset: bool,
    /// How long to hold the port open after `NORMAL_RESET` before closing.
    pub reset_hold: Duration,
    /// Per-command response timeout.
    pub timeout: Duration,
    /// Timeout for the slower `EXEC` of FDL2.
    pub exec_timeout: Duration,
}

impl Default for FlashOptions {
    fn default() -> Self {
        Self {
            format: false,
            chunk: 2048,
            baud: None,
            disable_transcode: false,
            reset: true,
            reset_hold: Duration::from_millis(1000),
            timeout: Duration::from_secs(2),
            exec_timeout: Duration::from_secs(15),
        }
    }
}
