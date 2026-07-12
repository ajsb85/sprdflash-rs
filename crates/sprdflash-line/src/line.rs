// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! The line orchestrator: run every station in parallel, stream per-unit
//! records, and aggregate throughput/yield metrics.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use sprdflash_core::pac::PacInfo;
use sprdflash_flash::FlashOptions;

use crate::record::{Outcome, UnitRecord};
use crate::station::{StationConfig, UnitJob, run_unit};

/// Line-wide configuration.
#[derive(Debug, Clone)]
pub struct LineConfig {
    /// One entry per physical fixture; all run concurrently.
    pub stations: Vec<StationConfig>,
    /// Cross-SDK format (erase + NV + PREPACK).
    pub format: bool,
    /// MIDST chunk size for partition writes.
    pub chunk: usize,
    /// Verify each unit boots the expected build (ATI/IMEI).
    pub verify: bool,
    /// Extra attempts per unit after the first.
    pub retries: u32,
    /// Append per-unit JSON-lines records here (for the MES / audit log).
    pub records_path: Option<PathBuf>,
}

/// Aggregate result of a line run.
#[derive(Debug, Clone)]
pub struct LineSummary {
    /// All unit records, in completion order.
    pub records: Vec<UnitRecord>,
    /// Units attempted.
    pub total: usize,
    /// Units that passed.
    pub passed: usize,
    /// Units that failed.
    pub failed: usize,
    /// Wall-clock seconds for the whole batch.
    pub wall_seconds: f64,
    /// Extrapolated good units per hour at this concurrency.
    pub units_per_hour: f64,
}

/// Run one unit on each configured station, concurrently. Blocks until all
/// stations finish. The PAC is shared read-only (`pac`, `info`), so no copies.
#[must_use]
pub fn run(info: &PacInfo, pac: &[u8], pac_name: &str, cfg: &LineConfig) -> LineSummary {
    let opts = FlashOptions {
        format: cfg.format,
        chunk: cfg.chunk,
        reset: true,
        ..Default::default()
    };

    let records = Mutex::new(Vec::<UnitRecord>::new());
    let sink = cfg
        .records_path
        .as_ref()
        .and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map_err(|e| tracing::error!("cannot open records file {}: {e}", p.display()))
                .ok()
        })
        .map(Mutex::new);

    let wall = Instant::now();
    std::thread::scope(|scope| {
        for station in &cfg.stations {
            let job = UnitJob {
                info,
                pac,
                pac_name: pac_name.to_string(),
                opts: opts.clone(),
                verify: cfg.verify,
                retries: cfg.retries,
            };
            let records = &records;
            let sink = &sink;
            std::thread::Builder::new()
                .name(station.label.clone())
                .spawn_scoped(scope, move || {
                    tracing::info!(station = station.label, "start");
                    let rec = run_unit(station, &job);
                    tracing::info!(
                        station = station.label,
                        result = ?rec.result,
                        secs = rec.total_seconds,
                        "done"
                    );
                    if let Some(m) = sink {
                        if let Ok(mut f) = m.lock() {
                            let _ = writeln!(f, "{}", rec.to_json_line());
                        }
                    }
                    records.lock().expect("records mutex").push(rec);
                })
                .expect("spawn station thread");
        }
    });

    let records = records.into_inner().expect("records mutex");
    let wall_seconds = wall.elapsed().as_secs_f64();
    let passed = records.iter().filter(|r| r.result == Outcome::Pass).count();
    let failed = records.len() - passed;
    let units_per_hour = if wall_seconds > 0.0 {
        (passed as f64) * 3600.0 / wall_seconds
    } else {
        0.0
    };

    LineSummary {
        total: records.len(),
        passed,
        failed,
        wall_seconds,
        units_per_hour,
        records,
    }
}
