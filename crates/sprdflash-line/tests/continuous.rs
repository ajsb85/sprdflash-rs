// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! Continuous-mode plumbing, exercised without hardware: with the stop flag set
//! up front, each station runs exactly one (failing) unit and the loop exits.
//! The stations point at bogus ports with no AT fallback, so `run_unit` fast-fails
//! at `Serial::open` — no device required.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use sprdflash_core::pac::PacInfo;
use sprdflash_line::{LineConfig, StationConfig, run};

fn empty_pac_info() -> PacInfo {
    PacInfo {
        version: "BP_R1.0.0".into(),
        product_name: "TEST_PRODUCT".into(),
        product_version: String::new(),
        size: 0,
        mode: 0,
        flash_type: 0,
        magic: 0,
        header_crc_ok: true,
        payload_crc_ok: Some(true),
        entries: Vec::new(),
    }
}

#[test]
fn continuous_loop_honors_the_stop_flag() {
    let info = empty_pac_info();
    let stations = vec![
        StationConfig {
            label: "fixture-a".into(),
            at_port: None,
            download_port: Some("BOGUS-A".into()),
        },
        StationConfig {
            label: "fixture-b".into(),
            at_port: None,
            download_port: Some("BOGUS-B".into()),
        },
    ];

    // Pre-stopped: every station should run one unit, then break out of the loop.
    let cfg = LineConfig {
        stations,
        format: false,
        chunk: 2048,
        verify: false,
        retries: 0,
        work_order: None,
        operator: None,
        records_path: None,
        metrics_addr: None,
        continuous: true,
        stop: Arc::new(AtomicBool::new(true)),
    };

    let summary = run(&info, &[], "test.pac", &cfg);
    assert_eq!(summary.total, 2, "one unit per station");
    assert_eq!(summary.passed, 0, "bogus ports cannot flash");
    assert_eq!(summary.failed, 2);
    assert_eq!(summary.records.len(), 2, "both records retained");
}
