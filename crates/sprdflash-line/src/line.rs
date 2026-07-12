// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! The line orchestrator: run every station in parallel, stream per-unit
//! records, and aggregate throughput/yield metrics.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sprdflash_core::pac::PacInfo;
use sprdflash_flash::FlashOptions;

use crate::metrics::{self, Metrics};
use crate::record::UnitRecord;
use crate::station::{StationConfig, UnitJob, run_unit};

/// In continuous mode, how many recent records to retain in memory for the
/// summary (the JSONL sink keeps the full history).
const RECENT_KEEP: usize = 500;

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
    /// ERP work order stamped on every record.
    pub work_order: Option<String>,
    /// Operator stamped on every record.
    pub operator: Option<String>,
    /// Append per-unit JSON-lines records here (for the MES / audit log).
    pub records_path: Option<PathBuf>,
    /// If set, serve live Prometheus metrics at `http://<addr>/metrics`.
    pub metrics_addr: Option<String>,
    /// Keep each station flashing units in a loop (unattended line) until `stop`.
    pub continuous: bool,
    /// Cooperative stop flag (e.g. set on Ctrl-C) — ends a continuous run.
    pub stop: Arc<AtomicBool>,
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

    // Optional live Prometheus exporter (runs alongside the stations).
    let metrics = Arc::new(Metrics::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let server = cfg.metrics_addr.as_ref().map(|addr| {
        let (m, s, a) = (metrics.clone(), shutdown.clone(), addr.clone());
        std::thread::spawn(move || metrics::serve(&a, m, s))
    });

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
                work_order: cfg.work_order.clone(),
                operator: cfg.operator.clone(),
            };
            let records = &records;
            let sink = &sink;
            let metrics = &metrics;
            let stop = &cfg.stop;
            let continuous = cfg.continuous;
            std::thread::Builder::new()
                .name(station.label.clone())
                .spawn_scoped(scope, move || {
                    tracing::info!(station = station.label, "start");
                    loop {
                        let rec = run_unit(station, &job);
                        tracing::info!(
                            station = station.label,
                            result = ?rec.result,
                            secs = rec.total_seconds,
                            "done"
                        );
                        metrics.record(&rec);
                        if let Some(m) = sink {
                            if let Ok(mut f) = m.lock() {
                                let _ = writeln!(f, "{}", rec.to_json_line());
                            }
                        }
                        {
                            // Keep only a recent tail in memory for the summary;
                            // the sink + metrics hold the full continuous history.
                            let mut r = records.lock().expect("records mutex");
                            r.push(rec);
                            if continuous && r.len() > RECENT_KEEP {
                                let drop = r.len() - RECENT_KEEP;
                                r.drain(..drop);
                            }
                        }
                        if !continuous || stop.load(Ordering::Relaxed) {
                            break;
                        }
                        // A small floor so a misconfigured/empty line (units that
                        // fast-fail with no device present) can't busy-spin.
                        std::thread::sleep(Duration::from_millis(500));
                        // Wait for the operator to swap in the next unit.
                        crate::station::wait_for_next_unit(station, stop);
                    }
                })
                .expect("spawn station thread");
        }
    });

    // Hold the exporter open briefly so a scraper can catch the final numbers.
    if let Some(server) = server {
        std::thread::sleep(Duration::from_secs(2));
        shutdown.store(true, Ordering::Relaxed);
        let _ = server.join();
    }

    let records = records.into_inner().expect("records mutex");
    let wall_seconds = wall.elapsed().as_secs_f64();
    // Totals come from the metrics counters, which see every unit — `records`
    // only holds a recent tail in continuous mode.
    let (passed64, failed64) = metrics.totals();
    let (passed, failed) = (passed64 as usize, failed64 as usize);
    let units_per_hour = if wall_seconds > 0.0 {
        (passed as f64) * 3600.0 / wall_seconds
    } else {
        0.0
    };

    LineSummary {
        total: passed + failed,
        passed,
        failed,
        wall_seconds,
        units_per_hour,
        records,
    }
}
