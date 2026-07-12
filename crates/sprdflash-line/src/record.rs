// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! Per-unit result records — the append-only, MES-ready trace of every flash.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Pass/fail outcome of a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Flashed and verified booting the expected build.
    Pass,
    /// Failed at some phase (see `error`).
    Fail,
}

/// One unit's traceable record, emitted as a JSON line for the MES / audit log.
#[derive(Debug, Clone, Serialize)]
pub struct UnitRecord {
    /// Unix epoch milliseconds when the unit started.
    pub ts_ms: u128,
    /// Station label (fixture).
    pub station: String,
    /// Download port used.
    pub port: String,
    /// PAC product name.
    pub product: String,
    /// PAC file name.
    pub pac: String,
    /// Outcome.
    pub result: Outcome,
    /// Attempts taken (1 = first-pass).
    pub attempts: u32,
    /// Payload bytes written.
    pub bytes: u64,
    /// Flash wall-clock seconds.
    pub flash_seconds: f64,
    /// Total unit wall-clock seconds (flash + verify + recovery).
    pub total_seconds: f64,
    /// Firmware banner read back after boot (if verified).
    pub firmware: Option<String>,
    /// IMEI read back after boot (if verified).
    pub imei: Option<String>,
    /// Error message on failure.
    pub error: Option<String>,
}

impl UnitRecord {
    /// Current Unix time in milliseconds (for `ts_ms`).
    #[must_use]
    pub fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    /// Serialize as a single JSON line.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| format!("{{\"serialize_error\":\"{e}\"}}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_line_is_single_line_and_has_fields() {
        let r = UnitRecord {
            ts_ms: 1_700_000_000_000,
            station: "fixture-3".into(),
            port: "COM34".into(),
            product: "UIX8910_MODEM".into(),
            pac: "fw.pac".into(),
            result: Outcome::Pass,
            attempts: 1,
            bytes: 6_071_296,
            flash_seconds: 33.2,
            total_seconds: 41.0,
            firmware: Some("LuatOS-Air_V4035".into()),
            imei: Some("863488050987562".into()),
            error: None,
        };
        let line = r.to_json_line();
        assert!(!line.contains('\n'));
        assert!(line.contains("\"result\":\"pass\""));
        assert!(line.contains("\"imei\":\"863488050987562\""));
        assert!(line.contains("\"station\":\"fixture-3\""));
    }
}
