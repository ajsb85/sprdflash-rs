// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! A tiny, dependency-free Prometheus exporter for the line.
//!
//! Stations feed [`Metrics`] (lock-free atomics) as units complete; a small HTTP
//! server serves the counters at `/metrics` in the Prometheus text format so a
//! scraper (or Grafana) can watch yield/throughput live during a run. Most
//! useful with a long-running/continuous line; a batch run also works if the
//! scrape interval is short.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::record::{Outcome, UnitRecord};

/// Lock-free line metrics.
#[derive(Debug)]
pub struct Metrics {
    passed: AtomicU64,
    failed: AtomicU64,
    retries: AtomicU64,
    bytes: AtomicU64,
    flash_millis: AtomicU64,
    started: Instant,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            passed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            flash_millis: AtomicU64::new(0),
            started: Instant::now(),
        }
    }
}

impl Metrics {
    /// Fold one completed unit into the counters.
    pub fn record(&self, rec: &UnitRecord) {
        match rec.result {
            Outcome::Pass => self.passed.fetch_add(1, Ordering::Relaxed),
            Outcome::Fail => self.failed.fetch_add(1, Ordering::Relaxed),
        };
        self.retries
            .fetch_add(u64::from(rec.attempts.saturating_sub(1)), Ordering::Relaxed);
        self.bytes.fetch_add(rec.bytes, Ordering::Relaxed);
        self.flash_millis
            .fetch_add((rec.flash_seconds * 1000.0) as u64, Ordering::Relaxed);
    }

    /// Current `(passed, failed)` unit counts.
    #[must_use]
    pub fn totals(&self) -> (u64, u64) {
        (
            self.passed.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }

    /// Render the current counters in Prometheus text format.
    #[must_use]
    pub fn render(&self) -> String {
        let p = self.passed.load(Ordering::Relaxed);
        let f = self.failed.load(Ordering::Relaxed);
        let flash_s = self.flash_millis.load(Ordering::Relaxed) as f64 / 1000.0;
        let up = self.started.elapsed().as_secs_f64();
        format!(
            "# HELP sprdflash_units_total Units flashed, by result.\n\
             # TYPE sprdflash_units_total counter\n\
             sprdflash_units_total{{result=\"pass\"}} {p}\n\
             sprdflash_units_total{{result=\"fail\"}} {f}\n\
             # HELP sprdflash_retries_total Retry attempts beyond the first.\n\
             # TYPE sprdflash_retries_total counter\n\
             sprdflash_retries_total {}\n\
             # HELP sprdflash_bytes_total Payload bytes written.\n\
             # TYPE sprdflash_bytes_total counter\n\
             sprdflash_bytes_total {}\n\
             # HELP sprdflash_flash_seconds_total Sum of flash wall-clock seconds.\n\
             # TYPE sprdflash_flash_seconds_total counter\n\
             sprdflash_flash_seconds_total {flash_s}\n\
             # HELP sprdflash_uptime_seconds Exporter uptime.\n\
             # TYPE sprdflash_uptime_seconds gauge\n\
             sprdflash_uptime_seconds {up}\n",
            self.retries.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

/// Serve `/metrics` on `addr` until `shutdown` is set. Any request gets the
/// metrics text; errors are logged, not fatal.
pub fn serve(addr: &str, metrics: Arc<Metrics>, shutdown: Arc<AtomicBool>) {
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("metrics: cannot bind {addr}: {e}");
            return;
        }
    };
    tracing::info!("metrics: serving http://{addr}/metrics");
    serve_listener(&listener, &metrics, &shutdown);
}

/// Serve on an already-bound `listener` (used by tests to pick an ephemeral port).
pub fn serve_listener(listener: &TcpListener, metrics: &Metrics, shutdown: &AtomicBool) {
    if listener.set_nonblocking(true).is_err() {
        tracing::error!("metrics: cannot set non-blocking");
        return;
    }
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // drain the request line
                let body = metrics.render();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => tracing::warn!("metrics: accept error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::PhaseTiming;

    fn rec(result: Outcome, attempts: u32) -> UnitRecord {
        UnitRecord {
            ts_ms: 0,
            station: "s".into(),
            work_order: None,
            operator: None,
            port: String::new(),
            product: String::new(),
            pac: String::new(),
            result,
            attempts,
            bytes: 1000,
            flash_seconds: 2.5,
            total_seconds: 3.0,
            phases: Vec::<PhaseTiming>::new(),
            firmware: None,
            imei: None,
            error: None,
        }
    }

    #[test]
    fn counts_and_renders() {
        let m = Metrics::default();
        m.record(&rec(Outcome::Pass, 1));
        m.record(&rec(Outcome::Pass, 2)); // 1 retry
        m.record(&rec(Outcome::Fail, 1));
        let text = m.render();
        assert!(text.contains("sprdflash_units_total{result=\"pass\"} 2"));
        assert!(text.contains("sprdflash_units_total{result=\"fail\"} 1"));
        assert!(text.contains("sprdflash_retries_total 1"));
        assert!(text.contains("sprdflash_bytes_total 3000"));
        assert_eq!(m.totals(), (2, 1), "two pass, one fail");
    }

    #[test]
    fn serves_valid_http_response() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let m = Arc::new(Metrics::default());
        m.record(&rec(Outcome::Pass, 1));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (m2, s2) = (m.clone(), shutdown.clone());
        let h = std::thread::spawn(move || serve_listener(&listener, &m2, &s2));

        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        shutdown.store(true, Ordering::Relaxed);
        h.join().unwrap();

        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("text/plain"));
        assert!(resp.contains("sprdflash_units_total{result=\"pass\"} 1"));
    }
}
