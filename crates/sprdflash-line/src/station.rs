//! A single station: take one unit through flash → boot-verify → record, with
//! bounded retries and recovery. Stations are independent and run on their own
//! thread, so one bad unit never stalls its neighbours.

use std::time::{Duration, Instant};

use sprdflash_core::pac::PacInfo;
use sprdflash_flash::{FlashOptions, Flasher};
use sprdflash_transport::{discovery, recovery, Serial};

use crate::record::{Outcome, UnitRecord};
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
                    port,
                    product: job.info.product_name.clone(),
                    pac: job.pac_name.clone(),
                    result: Outcome::Pass,
                    attempts,
                    bytes: outcome.bytes,
                    flash_seconds: outcome.flash_seconds,
                    total_seconds: started.elapsed().as_secs_f64(),
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
        port: station.download_port.clone().unwrap_or_default(),
        product: job.info.product_name.clone(),
        pac: job.pac_name.clone(),
        result: Outcome::Fail,
        attempts,
        bytes: 0,
        flash_seconds: 0.0,
        total_seconds: started.elapsed().as_secs_f64(),
        firmware: None,
        imei: None,
        error: Some(last_err),
    }
}

struct Success {
    bytes: u64,
    flash_seconds: f64,
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

    let (firmware, imei) = if job.verify && job.opts.reset {
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

    Ok((
        Success {
            bytes: outcome.bytes_written,
            flash_seconds: outcome.seconds,
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

/// Best-effort recovery between attempts.
fn recover(station: &StationConfig) {
    if recovery::restart_download_device() {
        std::thread::sleep(Duration::from_secs(6));
    } else if let Some(at) = &station.at_port {
        let _ = recovery::enter_download_mode(at);
        std::thread::sleep(Duration::from_secs(2));
    }
}
