//! `sprdflash` — native flasher CLI for SPRD/UNISOC `.pac` firmware.
//!
//! This turn ships the hardware-independent commands (`info`, `list-ports`);
//! `flash`/`identify` land with the transport crate.

use std::fs::File;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use memmap2::Mmap;
use sprdflash_core::pac;
use sprdflash_core::plan::{self, Role};

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
    }
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
