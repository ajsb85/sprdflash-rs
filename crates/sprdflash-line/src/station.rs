// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! A single station: take one unit through flash → boot-verify → record, with
//! bounded retries and recovery. Stations are independent and run on their own
//! thread, so one bad unit never stalls its neighbours.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sprdflash_core::pac::PacInfo;
use sprdflash_flash::{FlashOptions, Flasher};
use sprdflash_transport::{Serial, discovery, recovery};

use crate::record::{Outcome, PhaseTiming, UnitRecord};
use crate::verify::verify_boot;

/// Configuration for one physical station/fixture.
#[derive(Debug, Clone)]
pub struct StationConfig {
    /// Human label (e.g. `fixture-3`).
    pub label: String,
    /// The module's AT port (used to switch to download mode and to verify boot).
    pub at_port: Option<String>,
    /// The download-mode port; auto-detected if `None`.
    pub download_port: Option<String>,
}

/// The work one unit needs done, shared read-only across stations.
pub struct UnitJob<'a> {
    /// Parsed PAC.
    pub info: &'a PacInfo,
    /// PAC bytes (mmap).
    pub pac: &'a [u8],
    /// PAC file name (for the record).
    pub pac_name: String,
    /// Flash options.
    pub opts: FlashOptions,
    /// Verify boot after flashing.
    pub verify: bool,
    /// Extra attempts after the first (with recovery between).
    pub retries: u32,
    /// ERP work order to stamp on each record.
    pub work_order: Option<String>,
    /// Operator to stamp on each record.
    pub operator: Option<String>,
}

/// Run one unit on `station`, returning its record. Never panics.
#[must_use]
pub fn run_unit(station: &StationConfig, job: &UnitJob) -> UnitRecord {
    let ts_ms = UnitRecord::now_ms();
    let started = Instant::now();
    let mut attempts = 0u32;
    let mut last_err = String::new();

    while attempts <= job.retries {
        attempts += 1;
        match try_once(station, job) {
            Ok((outcome, port)) => {
                return UnitRecord {
                    ts_ms,
                    station: station.label.clone(),
                    work_order: job.work_order.clone(),
                    operator: job.operator.clone(),
                    port,
                    product: job.info.product_name.clone(),
                    pac: job.pac_name.clone(),
                    result: Outcome::Pass,
                    attempts,
                    bytes: outcome.bytes,
                    flash_seconds: outcome.flash_seconds,
                    total_seconds: started.elapsed().as_secs_f64(),
                    phases: outcome.phases,
                    firmware: outcome.firmware,
                    imei: outcome.imei,
                    error: None,
                };
            }
            Err(e) => {
                last_err = e;
                tracing::warn!(
                    station = station.label,
                    attempt = attempts,
                    "unit failed: {last_err}"
                );
                if attempts <= job.retries {
                    recover(station);
                }
            }
        }
    }

    UnitRecord {
        ts_ms,
        station: station.label.clone(),
        work_order: job.work_order.clone(),
        operator: job.operator.clone(),
        port: station.download_port.clone().unwrap_or_default(),
        product: job.info.product_name.clone(),
        pac: job.pac_name.clone(),
        result: Outcome::Fail,
        attempts,
        bytes: 0,
        flash_seconds: 0.0,
        total_seconds: started.elapsed().as_secs_f64(),
        phases: Vec::new(),
        firmware: None,
        imei: None,
        error: Some(last_err),
    }
}

struct Success {
    bytes: u64,
    flash_seconds: f64,
    phases: Vec<crate::record::PhaseTiming>,
    firmware: Option<String>,
    imei: Option<String>,
}

fn try_once(station: &StationConfig, job: &UnitJob) -> Result<(Success, String), String> {
    let port = ensure_download(station).map_err(|e| format!("enter download: {e}"))?;
    let mut serial = Serial::open(&port, 115_200).map_err(|e| format!("open {port}: {e}"))?;

    let mut noop = |_: &str, _: u64, _: u64| {};
    let outcome = Flasher::new(job.opts.clone())
        .run(&mut serial, job.info, job.pac, &mut noop)
        .map_err(|e| format!("flash: {e}"))?;
    drop(serial); // release the port so the module can re-enumerate

    let verify_start = Instant::now();
    let verified = job.verify && job.opts.reset;
    let (firmware, imei) = if verified {
        let mods = discovery::wait_for_module(Duration::from_secs(60));
        if mods.is_empty() {
            return Err("module did not boot after flash".into());
        }
        let at = pick_at_port(station, &mods);
        match verify_boot(&at, Duration::from_secs(20)) {
            Ok(b) => (Some(b.firmware), Some(b.imei)),
            Err(e) => return Err(format!("verify: {e}")),
        }
    } else {
        (None, None)
    };

    let mut phases: Vec<PhaseTiming> = outcome
        .phases
        .into_iter()
        .map(|(phase, seconds)| PhaseTiming { phase, seconds })
        .collect();
    if verified {
        phases.push(PhaseTiming {
            phase: "verify".into(),
            seconds: verify_start.elapsed().as_secs_f64(),
        });
    }

    Ok((
        Success {
            bytes: outcome.bytes_written,
            flash_seconds: outcome.seconds,
            phases,
            firmware,
            imei,
        },
        port,
    ))
}

/// Ensure the station is in download mode and return its download port.
fn ensure_download(station: &StationConfig) -> Result<String, String> {
    if let Some(p) = &station.download_port {
        if discovery::find_download_port().is_some() || station.at_port.is_none() {
            return Ok(p.clone());
        }
    }
    if let Some(p) = discovery::find_download_port() {
        return Ok(p.name);
    }
    if let Some(at) = &station.at_port {
        recovery::enter_download_mode(at).map_err(|e| e.to_string())?;
        return discovery::wait_for_download_port(Duration::from_secs(30))
            .map(|p| p.name)
            .ok_or_else(|| "download port did not appear".to_string());
    }
    station
        .download_port
        .clone()
        .ok_or_else(|| "no download port and no AT port to switch".to_string())
}

/// Choose the AT port to verify on: the configured one, else a discovered " AT".
fn pick_at_port(station: &StationConfig, mods: &[discovery::PortInfo]) -> String {
    if let Some(at) = &station.at_port {
        return at.clone();
    }
    mods.iter()
        .find(|p| {
            let d = p.product.as_deref().unwrap_or("");
            d.ends_with(" AT") || d.contains(" AT ") || d.contains("AT (")
        })
        .or_else(|| mods.first())
        .map(|p| p.name.clone())
        .unwrap_or_default()
}

const PRESENCE_POLL: Duration = Duration::from_millis(250);

/// Wait for this station's *next* unit: block until the current unit is removed
/// **and** a fresh one is inserted — an absent→present transition on the
/// station's own port — so a continuous run never re-flashes the unit it just
/// booted, and neighbouring fixtures don't cross-trigger each other. Returns
/// early if `stop` is set. A station with no configured port (a single
/// auto-detected fixture) falls back to a global "any device gone" check.
pub fn wait_for_next_unit(station: &StationConfig, stop: &AtomicBool) {
    if station.at_port.is_some() || station.download_port.is_some() {
        wait_cycle(stop, PRESENCE_POLL, || station_present(station));
    } else {
        // No per-fixture identity to track; wait for the bench to go empty.
        while !stop.load(Ordering::Relaxed) {
            if discovery::find_download_port().is_none()
                && discovery::find_module_ports().is_empty()
            {
                return;
            }
            std::thread::sleep(PRESENCE_POLL);
        }
    }
}

/// True if this station's own device is currently enumerated — its module port
/// after boot, or its download port. Keyed on the configured port names, which
/// pin each fixture to its USB slot.
fn station_present(station: &StationConfig) -> bool {
    station
        .at_port
        .as_deref()
        .is_some_and(discovery::is_present)
        || station
            .download_port
            .as_deref()
            .is_some_and(discovery::is_present)
}

/// Block until `present` transitions true→false (current unit removed) and then
/// false→true (next unit inserted), or `stop` is set. Split out from discovery
/// so the transition logic is unit-testable.
fn wait_cycle(stop: &AtomicBool, poll: Duration, present: impl Fn() -> bool) {
    while !stop.load(Ordering::Relaxed) && present() {
        std::thread::sleep(poll); // phase 1: wait for removal
    }
    while !stop.load(Ordering::Relaxed) && !present() {
        std::thread::sleep(poll); // phase 2: wait for the next insertion
    }
}

/// Best-effort recovery between attempts.
fn recover(station: &StationConfig) {
    if recovery::restart_download_device() {
        std::thread::sleep(Duration::from_secs(6));
    } else if let Some(at) = &station.at_port {
        let _ = recovery::enter_download_mode(at);
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn wait_cycle_waits_for_remove_then_insert() {
        let stop = AtomicBool::new(false);
        let calls = AtomicUsize::new(0);
        // present, present, absent, absent, present -> returns after the insert.
        let seq = [true, true, false, false, true];
        let present = || {
            let n = calls.fetch_add(1, Ordering::Relaxed);
            seq.get(n).copied().unwrap_or(true)
        };
        wait_cycle(&stop, Duration::from_millis(1), present);
        assert!(
            calls.load(Ordering::Relaxed) >= seq.len(),
            "should poll through the whole remove→insert cycle"
        );
    }

    #[test]
    fn wait_cycle_returns_immediately_when_stopped() {
        let stop = AtomicBool::new(true);
        // Device is "present" forever, but the stop flag must short-circuit both
        // phases so a shutdown never hangs.
        wait_cycle(&stop, Duration::from_millis(10), || true);
    }
}
