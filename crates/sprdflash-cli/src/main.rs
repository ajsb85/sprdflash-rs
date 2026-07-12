//! `sprdflash` — native flasher CLI for SPRD/UNISOC `.pac` firmware.
//!
//! Commands: `info`, `list-ports`, and `flash` (PDL → BSL → partitions →
//! optional cross-SDK `--format` → reset), all hardware-verified on RDA8910.

use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use memmap2::Mmap;
use sprdflash_core::pac;
use sprdflash_core::plan::{self, Role};
use sprdflash_flash::{FlashOptions, Flasher};
use sprdflash_transport::{discovery, recovery, Serial};

/// BootROM / download-mode USB identity (SPRD download gadget).
const DOWNLOAD_VID: u16 = 0x0525;
const DOWNLOAD_PID: u16 = 0xA4A7;
/// Normal-mode composite device (Spreadtrum/RDA).
const MODULE_VID: u16 = 0x1782;

#[derive(Parser)]
#[command(
    name = "sprdflash",
    version,
    about = "Native SPRD/UNISOC .pac flasher (no vendor tool)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse and validate a .pac file, printing its table and flash plan.
    Info {
        /// Path to the .pac file.
        pac: PathBuf,
        /// Skip the (streaming) payload CRC check.
        #[arg(long)]
        no_verify: bool,
    },
    /// List serial ports, flagging download-mode and module ports.
    ListPorts,
    /// Flash a .pac to the module natively (PDL + BSL, no vendor tool).
    Flash {
        /// Path to the .pac file.
        pac: PathBuf,
        /// Download-mode COM port; auto-detected (0525:a4a7) if omitted.
        #[arg(long)]
        port: Option<String>,
        /// Send AT*DOWNLOAD=1 on the module's AT port first (auto mode-switch).
        #[arg(long)]
        enter_download: bool,
        /// Also format the filesystem + refresh NV/prepack (firmware-TYPE change).
        #[arg(long)]
        format: bool,
        /// MIDST chunk size (bytes). Larger = fewer round trips = faster.
        #[arg(long, default_value_t = 2048)]
        chunk: usize,
        /// Issue CHANGE_BAUD to this rate after FDL2 (experimental speed lever).
        #[arg(long)]
        baud: Option<u32>,
        /// Do not reset the module after flashing.
        #[arg(long)]
        no_reset: bool,
        /// Skip the PAC payload CRC check before flashing.
        #[arg(long)]
        no_verify: bool,
    },
    /// Run a manufacturing line: flash + boot-verify across stations in parallel.
    Line {
        /// Path to the .pac file.
        pac: PathBuf,
        /// A station, repeatable: `label[:at_port[:download_port]]`. If omitted,
        /// one auto-discovered station is used.
        #[arg(long = "station")]
        stations: Vec<String>,
        /// Cross-SDK format (erase + NV + prepack).
        #[arg(long)]
        format: bool,
        /// MIDST chunk size for partition writes.
        #[arg(long, default_value_t = 2048)]
        chunk: usize,
        /// Extra attempts per unit after the first.
        #[arg(long, default_value_t = 1)]
        retries: u32,
        /// Do not boot-verify (ATI/IMEI) each unit.
        #[arg(long)]
        no_verify: bool,
        /// Append per-unit JSON-lines records here (MES / audit log).
        #[arg(long)]
        records: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::Info { pac, no_verify } => cmd_info(&pac, !no_verify),
        Command::ListPorts => cmd_list_ports(),
        Command::Flash {
            pac,
            port,
            enter_download,
            format,
            chunk,
            baud,
            no_reset,
            no_verify,
        } => cmd_flash(FlashArgs {
            pac,
            port,
            enter_download,
            format,
            chunk,
            baud,
            no_reset,
            verify: !no_verify,
        }),
        Command::Line {
            pac,
            stations,
            format,
            chunk,
            retries,
            no_verify,
            records,
        } => cmd_line(LineArgs {
            pac,
            stations,
            format,
            chunk,
            retries,
            verify: !no_verify,
            records,
        }),
    }
}

struct LineArgs {
    pac: PathBuf,
    stations: Vec<String>,
    format: bool,
    chunk: usize,
    retries: u32,
    verify: bool,
    records: Option<PathBuf>,
}

fn cmd_line(a: LineArgs) -> Result<()> {
    use sprdflash_line::{run as run_line, LineConfig};

    let file = File::open(&a.pac).with_context(|| format!("opening {}", a.pac.display()))?;
    let mmap = unsafe { Mmap::map(&file) }.with_context(|| format!("mmap {}", a.pac.display()))?;
    let info = pac::parse(&mmap, true).context("parsing PAC")?;
    if !info.crc_ok() {
        bail!("PAC checksum mismatch - refusing to flash");
    }
    let pac_name = a
        .pac
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let stations = build_stations(&a.stations)?;
    println!(
        "Line: {} ({}), {} station(s), {}{}",
        info.product_name,
        pac_name,
        stations.len(),
        if a.format { "format, " } else { "" },
        if a.verify { "boot-verify" } else { "no verify" },
    );

    let cfg = LineConfig {
        stations,
        format: a.format,
        chunk: a.chunk,
        verify: a.verify,
        retries: a.retries,
        records_path: a.records,
    };
    let summary = run_line(&info, &mmap, &pac_name, &cfg);

    println!("\n── results ──");
    for r in &summary.records {
        let tag = match r.result {
            sprdflash_line::Outcome::Pass => "PASS",
            sprdflash_line::Outcome::Fail => "FAIL",
        };
        println!(
            "  [{tag}] {:<12} {:>5.1}s  {}{}",
            r.station,
            r.total_seconds,
            r.firmware.as_deref().unwrap_or("-"),
            r.imei
                .as_deref()
                .map(|i| format!("  IMEI {i}"))
                .unwrap_or_default(),
        );
        if let Some(e) = &r.error {
            println!("             error: {e}");
        }
    }
    println!(
        "\n{}/{} passed in {:.1}s  →  ~{:.0} good units/hour at this concurrency",
        summary.passed, summary.total, summary.wall_seconds, summary.units_per_hour
    );
    if summary.failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Build station configs from `label[:at_port[:download_port]]` specs, or one
/// auto-discovered station if none were given.
fn build_stations(specs: &[String]) -> Result<Vec<sprdflash_line::StationConfig>> {
    use sprdflash_line::StationConfig;
    if specs.is_empty() {
        // Auto: one station from whatever module/download port is present.
        let at = discovery::find_module_ports()
            .into_iter()
            .find(|p| {
                let d = p.product.as_deref().unwrap_or("");
                d.ends_with(" AT") || d.contains(" AT ") || d.contains("AT (")
            })
            .map(|p| p.name);
        let dl = discovery::find_download_port().map(|p| p.name);
        if at.is_none() && dl.is_none() {
            bail!("no module or download port found; connect a device or pass --station");
        }
        return Ok(vec![StationConfig {
            label: "station-1".into(),
            at_port: at,
            download_port: dl,
        }]);
    }
    Ok(specs
        .iter()
        .map(|s| {
            let mut parts = s.splitn(3, ':');
            let label = parts.next().unwrap_or("station").to_string();
            let at_port = parts.next().filter(|s| !s.is_empty()).map(String::from);
            let download_port = parts.next().filter(|s| !s.is_empty()).map(String::from);
            StationConfig {
                label,
                at_port,
                download_port,
            }
        })
        .collect())
}

struct FlashArgs {
    pac: PathBuf,
    port: Option<String>,
    enter_download: bool,
    format: bool,
    chunk: usize,
    baud: Option<u32>,
    no_reset: bool,
    verify: bool,
}

fn cmd_flash(a: FlashArgs) -> Result<()> {
    let file = File::open(&a.pac).with_context(|| format!("opening {}", a.pac.display()))?;
    let mmap = unsafe { Mmap::map(&file) }.with_context(|| format!("mmap {}", a.pac.display()))?;
    let info = pac::parse(&mmap, a.verify).context("parsing PAC")?;
    if !info.crc_ok() {
        bail!("PAC checksum mismatch - refusing to flash");
    }
    println!("Flashing {} ({} bytes)", info.product_name, info.size);

    let port = resolve_download_port(a.port, a.enter_download)?;
    println!("Download port: {port}");
    let mut serial = Serial::open(&port, 115_200).context("opening download port")?;

    let opts = FlashOptions {
        format: a.format,
        chunk: a.chunk,
        baud: a.baud,
        reset: !a.no_reset,
        ..Default::default()
    };

    let mut state: (String, i64) = (String::new(), -1);
    let mut progress = |stage: &str, done: u64, total: u64| {
        let pct = done.saturating_mul(100).checked_div(total).unwrap_or(100) as i64;
        if stage != state.0 {
            if !state.0.is_empty() {
                println!();
            }
            state = (stage.to_string(), -1);
        }
        if pct != state.1 {
            state.1 = pct;
            print!("\r  {stage:<14} {pct:3}%");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    };

    let outcome = Flasher::new(opts)
        .run(&mut serial, &info, &mmap, &mut progress)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!();
    let mib = outcome.bytes_written as f64 / (1024.0 * 1024.0);
    println!(
        "Done: {:.2} MiB in {:.1}s ({:.0} KiB/s) — FDL1: {}",
        mib,
        outcome.seconds,
        (outcome.bytes_written as f64 / 1024.0) / outcome.seconds.max(0.001),
        outcome.version,
    );

    if a.verify && !a.no_reset {
        print!("Waiting for the module to boot... ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let ports = discovery::wait_for_module(Duration::from_secs(60));
        if ports.is_empty() {
            println!("not seen in 60s (may need a USB re-enumeration)");
        } else {
            println!("up ({} ports)", ports.len());
        }
    }
    Ok(())
}

/// Resolve the download-mode port, optionally switching the module first.
fn resolve_download_port(explicit: Option<String>, enter_download: bool) -> Result<String> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    if let Some(p) = discovery::find_download_port() {
        return Ok(p.name);
    }
    if enter_download {
        // The AT port's product ends in " AT" (avoid matching "LUAT"/"AP Diag").
        let mods = discovery::find_module_ports();
        let at = mods
            .iter()
            .find(|p| {
                let d = p.product.as_deref().unwrap_or("");
                d.ends_with(" AT") || d.contains(" AT ") || d.contains("AT (")
            })
            .or_else(|| mods.first())
            .context("no module AT port found to send AT*DOWNLOAD")?;
        println!("AT*DOWNLOAD=1 -> {}", at.name);
        recovery::enter_download_mode(&at.name).context("sending AT*DOWNLOAD")?;
        return discovery::wait_for_download_port(Duration::from_secs(30))
            .map(|p| p.name)
            .context("download port did not appear after AT*DOWNLOAD");
    }
    bail!("no download port (0525:a4a7) found; pass --port or --enter-download")
}

fn cmd_info(path: &PathBuf, verify: bool) -> Result<()> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // SAFETY note: mmap of a local firmware file for read-only parsing.
    let mmap = unsafe { Mmap::map(&file) }.with_context(|| format!("mmap {}", path.display()))?;
    let info = pac::parse(&mmap, verify).context("parsing PAC")?;

    println!("Product : {} ({})", info.product_name, info.product_version);
    println!("Size    : {} bytes", info.size);
    println!(
        "CRC     : header {}, payload {}",
        if info.header_crc_ok { "ok" } else { "BAD" },
        match info.payload_crc_ok {
            Some(true) => "ok",
            Some(false) => "BAD",
            None => "n/a",
        }
    );
    println!("Entries :");
    for e in &info.entries {
        let role = match plan::classify(e) {
            Role::Fdl1 => "fdl1",
            Role::Fdl2 => "fdl2",
            Role::Marker => "marker",
            Role::Flash => "flash",
        };
        let addr = if e.address != 0 {
            format!("0x{:08X}", e.address)
        } else {
            "          ".into()
        };
        let size = if e.is_marker() {
            "      marker".to_string()
        } else {
            format!("{:>10} B", e.size)
        };
        println!(
            "  [{role:<6}] {:<12} {addr}  {size}  {}",
            e.file_id, e.file_name
        );
    }

    match plan::build(&info) {
        Ok(p) => {
            println!(
                "\nPlan    : FDL1={}, FDL2={}, {} partition(s), {} erase(s), {} data-marker(s)",
                p.fdl1.file_id,
                p.fdl2.map_or("-", |e| e.file_id.as_str()),
                p.partitions.len(),
                p.erases.len(),
                p.data_markers.len(),
            );
            if !p.erases.is_empty() {
                let e: Vec<String> = p.erases.iter().map(|o| o.file_id.clone()).collect();
                println!("          erases (--format): {}", e.join(", "));
            }
        }
        Err(e) => println!("\nPlan    : unavailable ({e})"),
    }

    if !info.crc_ok() {
        anyhow::bail!("PAC checksum mismatch");
    }
    Ok(())
}

fn cmd_list_ports() -> Result<()> {
    let ports = serialport::available_ports().context("enumerating serial ports")?;
    if ports.is_empty() {
        println!("(no serial ports found)");
        return Ok(());
    }
    for p in ports {
        let (tag, ids) = match &p.port_type {
            serialport::SerialPortType::UsbPort(u) => {
                let ids = format!("{:04x}:{:04x}", u.vid, u.pid);
                let tag = if u.vid == DOWNLOAD_VID && u.pid == DOWNLOAD_PID {
                    "  <- BootROM download port"
                } else if u.vid == MODULE_VID {
                    "  <- module (normal mode)"
                } else {
                    ""
                };
                (tag, ids)
            }
            _ => ("", "-".into()),
        };
        println!(
            "{:<8} {:<11} {}{}",
            p.port_name,
            ids,
            describe(&p.port_type),
            tag
        );
    }
    Ok(())
}

fn describe(t: &serialport::SerialPortType) -> String {
    match t {
        serialport::SerialPortType::UsbPort(u) => {
            u.product.clone().unwrap_or_else(|| "USB serial".into())
        }
        serialport::SerialPortType::BluetoothPort => "Bluetooth".into(),
        serialport::SerialPortType::PciPort => "PCI".into(),
        serialport::SerialPortType::Unknown => "Unknown".into(),
    }
}
