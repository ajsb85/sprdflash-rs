// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! Concurrency soak: hammer the line's shared aggregation (atomic metrics + the
//! records sink) from many parallel "stations" and assert there is no data race
//! and no cross-talk — every record is accounted for and attributed correctly.
//! This proves the orchestration primitives scale before real fixtures do.

use std::sync::Mutex;

use sprdflash_line::Metrics;
use sprdflash_line::record::{Outcome, PhaseTiming, UnitRecord};

fn synth(station: usize, unit: usize) -> UnitRecord {
    // Every third unit "fails" on its second attempt (exercises retries + fails).
    let fail = unit % 3 == 0;
    UnitRecord {
        ts_ms: unit as u128,
        station: format!("fixture-{station}"),
        work_order: Some("WO-SOAK".into()),
        operator: Some("robot".into()),
        port: String::new(),
        product: "UIX8910_MODEM".into(),
        pac: "fw.pac".into(),
        result: if fail { Outcome::Fail } else { Outcome::Pass },
        attempts: if fail { 2 } else { 1 },
        bytes: 1000,
        flash_seconds: 1.0,
        total_seconds: 1.0,
        phases: vec![PhaseTiming {
            phase: "partitions".into(),
            seconds: 1.0,
        }],
        firmware: None,
        imei: None,
        error: fail.then(|| "flash: simulated".to_string()),
    }
}

#[test]
fn stations_aggregate_without_races_or_crosstalk() {
    const STATIONS: usize = 16;
    const UNITS: usize = 2000;

    let metrics = Metrics::default();
    let records: Mutex<Vec<UnitRecord>> = Mutex::new(Vec::with_capacity(STATIONS * UNITS));

    std::thread::scope(|scope| {
        for station in 0..STATIONS {
            let metrics = &metrics;
            let records = &records;
            scope.spawn(move || {
                for unit in 0..UNITS {
                    let rec = synth(station, unit);
                    metrics.record(&rec);
                    records.lock().expect("records mutex").push(rec);
                }
            });
        }
    });

    let recs = records.into_inner().expect("records mutex");
    let total = STATIONS * UNITS;

    // Nothing dropped, nothing duplicated.
    assert_eq!(recs.len(), total, "every record must land exactly once");

    // No cross-talk: each station contributed exactly UNITS records, and every
    // record's fields are internally consistent (pass <=> no error).
    for station in 0..STATIONS {
        let label = format!("fixture-{station}");
        let n = recs.iter().filter(|r| r.station == label).count();
        assert_eq!(n, UNITS, "station {label} lost/gained records");
    }
    for r in &recs {
        match r.result {
            Outcome::Pass => assert!(r.error.is_none()),
            Outcome::Fail => assert!(r.error.is_some() && r.attempts == 2),
        }
    }

    // Metrics match a serial recount of the same records (atomics were sound).
    let passed = recs.iter().filter(|r| r.result == Outcome::Pass).count();
    let failed = total - passed;
    let retries: u64 = recs.iter().map(|r| u64::from(r.attempts - 1)).sum();
    let text = metrics.render();
    assert!(text.contains(&format!(
        "sprdflash_units_total{{result=\"pass\"}} {passed}"
    )));
    assert!(text.contains(&format!(
        "sprdflash_units_total{{result=\"fail\"}} {failed}"
    )));
    assert!(text.contains(&format!("sprdflash_retries_total {retries}")));
    assert!(text.contains(&format!("sprdflash_bytes_total {}", total as u64 * 1000)));
}
