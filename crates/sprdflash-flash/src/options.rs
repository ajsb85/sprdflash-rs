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
    /// After writing each partition, read it back and compare (high-assurance;
    /// roughly doubles the flash time).
    pub verify_readback: bool,
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
            verify_readback: false,
            reset: true,
            reset_hold: Duration::from_millis(1000),
            // 5 s absorbs USB jitter (notably usbipd → WSL latency spikes) while
            // still failing fast on a genuinely dead agent. ACKs normally arrive
            // in milliseconds, so this only bites when something is wrong.
            timeout: Duration::from_secs(5),
            exec_timeout: Duration::from_secs(15),
        }
    }
}
