// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! `sprdflash` — native flasher CLI for SPRD/UNISOC `.pac` firmware.
//!
//! Commands: `info`, `list-ports`, and `flash` (PDL → BSL → partitions →
//! optional cross-SDK `--format` → reset), all hardware-verified on RDA8910.

use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use memmap2::Mmap;
use sprdflash_core::checksum::crc16_arc;
use sprdflash_core::pac;
use sprdflash_core::plan::{self, Role};
use sprdflash_flash::{FlashOptions, Flasher};
use sprdflash_transport::{Serial, discovery, recovery};

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
    /// Reboot a device stuck in FDL2/download mode (BSL `NORMAL_RESET`). Useful
    /// after a flash aborts mid-way (e.g. a genuine read-back verify failure).
    Reset {
        /// Download-mode COM port; auto-detected (0525:a4a7) if omitted.
        #[arg(long)]
        port: Option<String>,
    },
    /// Read the device's partitions back off the flash (read-only) into files.
    /// The .pac supplies this device's FDL1/FDL2 stages and the partition layout
    /// (addresses + sizes); its partition payloads are not used.
    Dump {
        /// PAC providing the FDL stages (and, without --region, the partition
        /// layout). Any PAC for this chip works for the FDLs.
        #[arg(long)]
        pac: PathBuf,
        /// Output directory; one <file_id>.bin (or region_<addr>.bin) is written here.
        #[arg(long)]
        out: PathBuf,
        /// Explicit region(s) to read as ADDR:SIZE (hex `0x..` or decimal),
        /// repeatable. If given, overrides the PAC's partition layout — the PAC
        /// is then used only for the FDL stages.
        #[arg(long = "region")]
        regions: Vec<String>,
        /// Dump the WHOLE flash to <out>/flash.bin, auto-discovering its size (no
        /// partition layout needed). The PAC supplies only the FDL stages.
        #[arg(long)]
        full: bool,
        /// Download-mode COM port; auto-detected (0525:a4a7) if omitted.
        #[arg(long)]
        port: Option<String>,
        /// Send AT*DOWNLOAD=1 on the module's AT port first (auto mode-switch).
        #[arg(long)]
        enter_download: bool,
    },
    /// Clone a device into a flashable golden .pac: read its partitions off the
    /// flash and splice them into a copy of the reference PAC (same FDL stages,
    /// layout, and markers), refreshing the CRCs. Captures a configured
    /// reference unit's firmware so it can be flashed to others.
    Clone {
        /// Reference PAC (FDL stages + partition layout to capture).
        #[arg(long)]
        pac: PathBuf,
        /// Output path for the golden .pac.
        #[arg(long)]
        out: PathBuf,
        /// Download-mode COM port; auto-detected (0525:a4a7) if omitted.
        #[arg(long)]
        port: Option<String>,
        /// Send AT*DOWNLOAD=1 on the module's AT port first (auto mode-switch).
        #[arg(long)]
        enter_download: bool,
    },
    /// Probe the device for an on-flash partition table (BSL READ_PARTITION).
    /// Experimental: works only if this chip's FDL2 implements it.
    Parts {
        /// PAC providing the FDL stages for this device.
        #[arg(long)]
        pac: PathBuf,
        /// Download-mode COM port; auto-detected (0525:a4a7) if omitted.
        #[arg(long)]
        port: Option<String>,
        /// Send AT*DOWNLOAD=1 on the module's AT port first (auto mode-switch).
        #[arg(long)]
        enter_download: bool,
    },
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
        /// MIDST chunk size (bytes). Larger = fewer round trips = faster on a
        /// direct connection; use a smaller value (e.g. 512) over usbipd -> WSL,
        /// where cdc_acm stalls on 2 KB frames at the tail of a big transfer.
        #[arg(long, default_value_t = 2048)]
        chunk: usize,
        /// Issue CHANGE_BAUD to this rate after FDL2 (experimental speed lever).
        #[arg(long)]
        baud: Option<u32>,
        /// Per-command response timeout, seconds (raise for high-latency links
        /// such as usbipd → WSL).
        #[arg(long, default_value_t = 5.0)]
        timeout: f64,
        /// Read each partition back and compare after writing (high-assurance;
        /// roughly doubles the flash time).
        #[arg(long)]
        verify_readback: bool,
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
        /// ERP work order to stamp on every record.
        #[arg(long)]
        work_order: Option<String>,
        /// Operator to stamp on every record.
        #[arg(long)]
        operator: Option<String>,
        /// Append per-unit JSON-lines records here (MES / audit log).
        #[arg(long)]
        records: Option<PathBuf>,
        /// Serve live Prometheus metrics at http://ADDR/metrics (e.g. 0.0.0.0:9184).
        #[arg(long)]
        metrics_addr: Option<String>,
        /// Run unattended: keep each station flashing units in a loop until Ctrl-C.
        #[arg(long = "loop")]
        continuous: bool,
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
        Command::Reset { port } => cmd_reset(port),
        Command::Dump {
            pac,
            out,
            regions,
            full,
            port,
            enter_download,
        } => cmd_dump(&pac, &out, &regions, full, port, enter_download),
        Command::Clone {
            pac,
            out,
            port,
            enter_download,
        } => cmd_clone(&pac, &out, port, enter_download),
        Command::Parts {
            pac,
            port,
            enter_download,
        } => cmd_parts(&pac, port, enter_download),
        Command::Flash {
            pac,
            port,
            enter_download,
            format,
            chunk,
            baud,
            timeout,
            verify_readback,
            no_reset,
            no_verify,
        } => cmd_flash(FlashArgs {
            pac,
            port,
            enter_download,
            format,
            chunk,
            baud,
            timeout,
            verify_readback,
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
            work_order,
            operator,
            records,
            metrics_addr,
            continuous,
        } => cmd_line(LineArgs {
            pac,
            stations,
            format,
            chunk,
            retries,
            verify: !no_verify,
            work_order,
            operator,
            records,
            metrics_addr,
            continuous,
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
    work_order: Option<String>,
    operator: Option<String>,
    records: Option<PathBuf>,
    metrics_addr: Option<String>,
    continuous: bool,
}

fn cmd_line(a: LineArgs) -> Result<()> {
    use sprdflash_line::{LineConfig, run as run_line};

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
        "Line: {} ({}), {} station(s), {}{}{}",
        info.product_name,
        pac_name,
        stations.len(),
        if a.format { "format, " } else { "" },
        if a.verify { "boot-verify" } else { "no verify" },
        if a.continuous { ", continuous" } else { "" },
    );

    // Ctrl-C ends a continuous run cleanly after the in-flight units finish.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if a.continuous {
        let stop = stop.clone();
        ctrlc::set_handler(move || {
            eprintln!("\nstopping after in-flight units…");
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        })
        .context("installing Ctrl-C handler")?;
        println!("continuous mode — press Ctrl-C to stop");
    }

    let cfg = LineConfig {
        stations,
        format: a.format,
        chunk: a.chunk,
        verify: a.verify,
        retries: a.retries,
        work_order: a.work_order,
        operator: a.operator,
        records_path: a.records,
        metrics_addr: a.metrics_addr,
        continuous: a.continuous,
        stop,
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
    timeout: f64,
    verify_readback: bool,
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

    let opts = FlashOptions {
        format: a.format,
        chunk: a.chunk,
        baud: a.baud,
        verify_readback: a.verify_readback,
        reset: !a.no_reset,
        timeout: std::time::Duration::from_secs_f64(a.timeout.max(0.1)),
        ..Default::default()
    };

    let outcome = flash_with_recovery(&opts, a.port, a.enter_download, &info, &mmap)?;
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

/// Resolve the port, open it, and flash — with one automatic reset-and-retry if
/// the PDL handshake fails. That is the signature of a module left in FDL2 by a
/// prior aborted flash; a `NORMAL_RESET` reboots it back to the boot ROM so the
/// retry's PDL connect succeeds, without the operator running `reset` by hand.
fn flash_with_recovery(
    opts: &FlashOptions,
    explicit_port: Option<String>,
    enter_download: bool,
    info: &pac::PacInfo,
    mmap: &[u8],
) -> Result<sprdflash_flash::FlashOutcome> {
    for attempt in 0u8..2 {
        let port = resolve_download_port(explicit_port.clone(), enter_download)?;
        println!("Download port: {port}");
        let mut serial = Serial::open(&port, 115_200).context("opening download port")?;

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

        match Flasher::new(opts.clone()).run(&mut serial, info, mmap, &mut progress) {
            Ok(outcome) => return Ok(outcome),
            Err(sprdflash_flash::FlashError::Pdl(e)) if attempt == 0 => {
                println!();
                eprintln!(
                    "PDL handshake failed ({e}); the module may be stuck in FDL2 from a prior \
                     aborted flash — sending reset and retrying once"
                );
                send_normal_reset(&mut serial);
                drop(serial);
                // Wait for the module to re-enumerate: back in download (an
                // unbootable image) or booted to normal mode.
                let mut booted = false;
                for _ in 0..40 {
                    if discovery::find_download_port().is_some() {
                        break;
                    }
                    if !discovery::find_module_ports().is_empty() {
                        booted = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                // A port appearing mid-boot is not yet ready for AT*DOWNLOAD;
                // let the firmware finish coming up before the retry switches it.
                if booted {
                    std::thread::sleep(Duration::from_secs(10));
                }
            }
            Err(e) => return Err(anyhow::anyhow!("{e}")),
        }
    }
    unreachable!("the retry loop returns within two attempts")
}

/// Send a BSL `NORMAL_RESET` to reboot a module out of FDL2/download mode.
fn send_normal_reset(serial: &mut Serial) {
    use sprdflash_core::bsl::{self, Checksum};
    use sprdflash_transport::Transport;
    let msg = bsl::build_message(bsl::cmd::NORMAL_RESET, &[], Checksum::Sprd);
    let _ = serial.write_all(&msg);
    serial.reset_teardown(Duration::from_millis(1000));
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

/// Read a device's partitions and repackage them into a flashable golden PAC by
/// splicing them into a copy of the reference PAC and refreshing the CRCs.
fn cmd_clone(
    pac_path: &PathBuf,
    out_path: &PathBuf,
    port: Option<String>,
    enter_download: bool,
) -> Result<()> {
    let file = File::open(pac_path).with_context(|| format!("opening {}", pac_path.display()))?;
    let mmap =
        unsafe { Mmap::map(&file) }.with_context(|| format!("mmap {}", pac_path.display()))?;
    let info = pac::parse(&mmap, false).context("parsing reference PAC")?;
    let flashplan = plan::build(&info).map_err(|e| anyhow::anyhow!("building plan: {e}"))?;
    // (file_id, flash address, payload offset in the PAC, size)
    let parts: Vec<(String, u32, u32, u32)> = flashplan
        .partitions
        .iter()
        .map(|e| (e.file_id.clone(), e.address, e.offset, e.size))
        .collect();
    if parts.is_empty() {
        bail!("no partitions in {}", pac_path.display());
    }

    let dl = resolve_download_port(port, enter_download)?;
    println!("Download port: {dl}");
    let mut serial = Serial::open(&dl, 115_200).context("opening download port")?;
    let regions: Vec<(u32, u32)> = parts.iter().map(|(_, a, _, s)| (*a, *s)).collect();

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
    let dumps = Flasher::new(FlashOptions::default())
        .dump(&mut serial, &info, &mmap, &regions, &mut progress)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!();

    // Splice the dumped partitions into a copy of the reference PAC.
    let mut out = mmap.to_vec();
    for ((fid, _, offset, size), data) in parts.iter().zip(&dumps) {
        if data.len() != *size as usize {
            bail!(
                "{fid}: short read ({} of {} bytes) — cannot clone",
                data.len(),
                size
            );
        }
        let o = *offset as usize;
        out[o..o + data.len()].copy_from_slice(data);
        println!("  {fid:<12} {} bytes captured", data.len());
    }

    // Refresh the CRC-16-ARC fields (stored little-endian): payload over the body
    // past the header, then the header itself. Neither covers the CRC bytes.
    const OFF_CRC1: usize = 2120; // header crc
    const OFF_CRC2: usize = 2122; // payload crc
    let crc2 = crc16_arc(&out[pac::HEADER_SIZE..]);
    out[OFF_CRC2..OFF_CRC2 + 2].copy_from_slice(&crc2.to_le_bytes());
    let crc1 = crc16_arc(&out[..OFF_CRC1]);
    out[OFF_CRC1..OFF_CRC1 + 2].copy_from_slice(&crc1.to_le_bytes());

    // Validate before writing — the golden PAC must pass its own CRCs.
    let check = pac::parse(&out, true).context("re-parsing the golden PAC")?;
    if !check.crc_ok() {
        bail!("reconstructed PAC failed its own CRC — refusing to write");
    }
    std::fs::write(out_path, &out).with_context(|| format!("writing {}", out_path.display()))?;
    println!(
        "Wrote golden PAC {} ({} bytes) — {} partition(s) captured from the device",
        out_path.display(),
        out.len(),
        dumps.len()
    );
    Ok(())
}

/// Probe the device for an on-flash partition table via BSL `READ_PARTITION`.
fn cmd_parts(pac_path: &PathBuf, port: Option<String>, enter_download: bool) -> Result<()> {
    use sprdflash_core::bsl;

    let file = File::open(pac_path).with_context(|| format!("opening {}", pac_path.display()))?;
    let mmap =
        unsafe { Mmap::map(&file) }.with_context(|| format!("mmap {}", pac_path.display()))?;
    let info = pac::parse(&mmap, false).context("parsing PAC")?;

    let dl = resolve_download_port(port, enter_download)?;
    println!("Download port: {dl}");
    let mut serial = Serial::open(&dl, 115_200).context("opening download port")?;

    // Info commands (no payload), then READ_FLASH at rising addresses to bound
    // the flash extent — the first address that errors marks the end. Info first
    // so a READ_FLASH hang past the end can't mask them.
    const NOR_BASE: u32 = 0x6000_0000;
    let mut names: Vec<String> = Vec::new();
    let mut cmds: Vec<(u16, Vec<u8>)> = Vec::new();
    for (c, n) in [
        (bsl::cmd::READ_CHIP_TYPE, "READ_CHIP_TYPE"),
        (bsl::cmd::READ_CHIP_UID, "READ_CHIP_UID"),
        (0x0C, "READ_FLASH_TYPE"),
        (0x0D, "READ_FLASH_INFO"),
        (0x29, "READ_NAND_BLOCK_INFO"),
        (bsl::cmd::READ_PARTITION, "READ_PARTITION"),
    ] {
        names.push(n.to_string());
        cmds.push((c, Vec::new()));
    }
    for mb in [4u32, 8, 12, 16, 24, 32, 48, 64] {
        let addr = NOR_BASE + mb * 1024 * 1024;
        names.push(format!("READ_FLASH @{addr:#010x} ({mb}MB)"));
        cmds.push((bsl::cmd::READ_FLASH, read_flash_req(addr, 16)));
    }

    let mut noop = |_: &str, _: u64, _: u64| {};
    let replies = Flasher::new(FlashOptions::default())
        .probe_commands(&mut serial, &info, &mmap, &cmds, &mut noop)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("\n{:<30} {:<20} data", "command", "reply");
    for (name, (ty, data)) in names.iter().zip(&replies) {
        let head: String = data
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let reply = if *ty == 0 {
            "(no reply/timeout)".to_string()
        } else {
            format!("{} 0x{ty:02x}", bsl::rep_name(*ty))
        };
        println!("{name:<30} {reply:<20} [{}] {head}", data.len());
    }
    Ok(())
}

/// Build a `READ_FLASH` request payload: `addr | size | offset(=0)`, big-endian.
fn read_flash_req(addr: u32, size: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(12);
    p.extend_from_slice(&addr.to_be_bytes());
    p.extend_from_slice(&size.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    p
}

/// Parse an `ADDR:SIZE` region spec (each hex `0x..` or decimal) into `(u32, u32)`.
fn parse_region(s: &str) -> Result<(u32, u32)> {
    let (a, b) = s
        .split_once(':')
        .with_context(|| format!("region '{s}' must be ADDR:SIZE"))?;
    let num = |x: &str| -> Result<u32> {
        let x = x.trim();
        match x.strip_prefix("0x").or_else(|| x.strip_prefix("0X")) {
            Some(hex) => u32::from_str_radix(hex, 16),
            None => x.parse(),
        }
        .with_context(|| format!("bad number '{x}' in region '{s}'"))
    };
    Ok((num(a)?, num(b)?))
}

/// Read the device's partitions (or explicit `--region`s) off flash into files.
fn cmd_dump(
    pac_path: &PathBuf,
    out_dir: &PathBuf,
    region_specs: &[String],
    full: bool,
    port: Option<String>,
    enter_download: bool,
) -> Result<()> {
    let file = File::open(pac_path).with_context(|| format!("opening {}", pac_path.display()))?;
    let mmap =
        unsafe { Mmap::map(&file) }.with_context(|| format!("mmap {}", pac_path.display()))?;
    let info = pac::parse(&mmap, false).context("parsing PAC")?;
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    let dl = resolve_download_port(port, enter_download)?;
    println!("Download port: {dl}");
    let mut serial = Serial::open(&dl, 115_200).context("opening download port")?;

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
            print!("\r  {stage:<16} {pct:3}%");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    };

    // Whole-flash backup: auto-discover the size, no partition layout needed.
    if full {
        const NOR_BASE: u32 = 0x6000_0000;
        let (image, size) = Flasher::new(FlashOptions::default())
            .dump_full(&mut serial, &info, &mmap, NOR_BASE, &mut progress)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        println!();
        let path = out_dir.join("flash.bin");
        std::fs::write(&path, &image).with_context(|| format!("writing {}", path.display()))?;
        println!(
            "Dumped whole flash: {size:#x} bytes ({} MiB) -> {}",
            size >> 20,
            path.display()
        );
        return Ok(());
    }

    // (name, address, size): explicit --region overrides the PAC's partitions.
    let targets: Vec<(String, u32, u32)> = if region_specs.is_empty() {
        let flashplan = plan::build(&info).map_err(|e| anyhow::anyhow!("building plan: {e}"))?;
        let parts: Vec<(String, u32, u32)> = flashplan
            .partitions
            .iter()
            .map(|e| (e.file_id.clone(), e.address, e.size))
            .collect();
        if parts.is_empty() {
            bail!("no partitions in {}", pac_path.display());
        }
        parts
    } else {
        region_specs
            .iter()
            .map(|s| {
                let (addr, size) = parse_region(s)?;
                Ok((format!("region_{addr:#010x}"), addr, size))
            })
            .collect::<Result<Vec<_>>>()?
    };

    let regions: Vec<(u32, u32)> = targets.iter().map(|(_, a, s)| (*a, *s)).collect();
    let dumps = Flasher::new(FlashOptions::default())
        .dump(&mut serial, &info, &mmap, &regions, &mut progress)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!();

    for ((name, _, declared), data) in targets.iter().zip(&dumps) {
        let path = out_dir.join(format!("{name}.bin"));
        std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
        let note = if data.len() as u32 == *declared {
            ""
        } else {
            " (short read)"
        };
        println!(
            "  {name:<18} {} bytes{note} -> {}",
            data.len(),
            path.display()
        );
    }
    println!("Dumped {} region(s) to {}", dumps.len(), out_dir.display());
    Ok(())
}

/// Reboot a device out of FDL2/download mode with a BSL `NORMAL_RESET`.
fn cmd_reset(port: Option<String>) -> Result<()> {
    let name = match port {
        Some(p) => p,
        None => discovery::find_download_port()
            .map(|p| p.name)
            .context("no download port (0525:a4a7) found; pass --port")?,
    };
    let mut serial = Serial::open(&name, 115_200).context("opening download port")?;
    send_normal_reset(&mut serial);
    println!("Sent NORMAL_RESET to {name}; the module should reboot into its firmware.");
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::parse_region;

    #[test]
    fn parses_hex_and_decimal_regions() {
        assert_eq!(
            parse_region("0x60000000:42112").unwrap(),
            (0x6000_0000, 42112)
        );
        assert_eq!(
            parse_region("1610612736:1024").unwrap(),
            (1_610_612_736, 1024)
        );
        assert_eq!(parse_region(" 0x10 : 0x20 ").unwrap(), (0x10, 0x20));
    }

    #[test]
    fn rejects_malformed_regions() {
        assert!(parse_region("0x60000000").is_err()); // no size
        assert!(parse_region("nope:123").is_err()); // bad address
        assert!(parse_region("0x10:xyz").is_err()); // bad size
    }
}
