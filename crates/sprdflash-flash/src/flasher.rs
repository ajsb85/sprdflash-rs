// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! The [`Flasher`]: PDL → BSL → partitions → format → reset.

use std::time::Instant;

use sprdflash_core::pac::PacInfo;
use sprdflash_core::{bsl, plan};
use sprdflash_transport::Serial;

use crate::FlashOptions;
use crate::bsl_io::{BslError, BslIo};
use crate::pdl_io::{PdlError, PdlIo};

/// Progress callback: `(stage, done_bytes, total_bytes)`.
pub type Progress<'a> = &'a mut dyn FnMut(&str, u64, u64);

/// Chunk size for loading the FDL stages, which run under the BootROM/FDL1 whose
/// receive buffer is small. Kept conservative; the large `opts.chunk` is used
/// only for partition/NV writes under FDL2 (which has a much bigger buffer).
const FDL_LOAD_CHUNK: usize = 2048;

/// A flash failure.
#[derive(Debug, thiserror::Error)]
pub enum FlashError {
    /// PDL phase failure.
    #[error("PDL: {0}")]
    Pdl(#[from] PdlError),
    /// BSL phase failure.
    #[error("BSL: {0}")]
    Bsl(#[from] BslError),
    /// The PAC had no usable flash plan.
    #[error("plan: {0}")]
    Plan(&'static str),
    /// A referenced payload was missing/out of range.
    #[error("payload for {0:?} missing from PAC")]
    Payload(String),
    /// The device did not answer with a VER frame.
    #[error("no/invalid VER frame after FDL1")]
    Version,
}

/// Result of a successful flash.
#[derive(Debug, Clone)]
pub struct FlashOutcome {
    /// FDL1 version banner (e.g. `Spreadtrum Boot Block version 1.2`).
    pub version: String,
    /// Total payload bytes written (partitions + markers).
    pub bytes_written: u64,
    /// Wall-clock seconds for the whole run.
    pub seconds: f64,
}

/// Drives a single device through a full flash.
pub struct Flasher {
    opts: FlashOptions,
}

impl Flasher {
    /// Create a flasher with the given options.
    #[must_use]
    pub fn new(opts: FlashOptions) -> Self {
        Self { opts }
    }

    /// Flash `pac` (its parsed `info`) to the module on `port`.
    pub fn run(
        &self,
        port: &mut Serial,
        info: &PacInfo,
        pac: &[u8],
        progress: Progress,
    ) -> Result<FlashOutcome, FlashError> {
        let start = Instant::now();
        let plan = plan::build(info).map_err(FlashError::Plan)?;
        let mut written = 0u64;

        let fdl1 = plan
            .fdl1
            .payload(pac)
            .ok_or_else(|| FlashError::Payload(plan.fdl1.file_id.clone()))?;

        // ---- Phase 1: PDL loads + execs FDL1 -------------------------------
        let version;
        {
            let mut pdl = PdlIo::new(port, self.opts.timeout);
            tracing::info!("PDL connect");
            pdl.connect()?;
            tracing::info!(
                "PDL load FDL1 ({} bytes @ {:#x})",
                fdl1.len(),
                plan.fdl1.address
            );
            pdl.send_image(plan.fdl1.address, fdl1, |d, t| progress("FDL1", d, t))?;
            let raw = pdl.exec_and_get_ver(self.opts.exec_timeout)?;
            // strip 0x7e flags, unescape, parse the VER frame
            let body = bsl::unescape(&raw[1..raw.len().saturating_sub(1)]);
            let (t, vdata) = bsl::parse_message(&body).map_err(|_| FlashError::Version)?;
            if t != bsl::rep::VER {
                return Err(FlashError::Version);
            }
            version = String::from_utf8_lossy(vdata).trim().to_string();
            tracing::info!("FDL1 running: {version}");
        }

        // ---- Phase 2: BSL loads FDL2, then writes partitions ---------------
        let mut bsl = BslIo::new(port, self.opts.timeout);
        bsl.connect()?;

        if let Some(fdl2e) = plan.fdl2 {
            let fdl2 = fdl2e
                .payload(pac)
                .ok_or_else(|| FlashError::Payload(fdl2e.file_id.clone()))?;
            tracing::info!(
                "BSL load FDL2 ({} bytes @ {:#x})",
                fdl2.len(),
                fdl2e.address
            );
            bsl.send_stage(fdl2e.address, fdl2, FDL_LOAD_CHUNK, |d, t| {
                progress("FDL2", d, t)
            })?;
            bsl.command(
                bsl::cmd::EXEC_DATA,
                &[],
                bsl::rep::ACK,
                self.opts.exec_timeout,
                "EXEC FDL2",
            )?;
            bsl.connect()?; // re-handshake under FDL2
        }

        // ---- Speed lever: CHANGE_BAUD (measured, off by default) -----------
        if let Some(baud) = self.opts.baud {
            tracing::info!("CHANGE_BAUD -> {baud}");
            bsl.command(
                bsl::cmd::CHANGE_BAUD,
                &baud.to_be_bytes(),
                bsl::rep::ACK,
                self.opts.timeout,
                "CHANGE_BAUD",
            )?;
            bsl.port().set_baud(baud).map_err(BslError::from)?;
            bsl.connect()?;
        }

        // ---- Partitions ----------------------------------------------------
        for e in &plan.partitions {
            let data = e
                .payload(pac)
                .ok_or_else(|| FlashError::Payload(e.file_id.clone()))?;
            tracing::info!(
                "flash {} ({} bytes @ {:#x})",
                e.file_id,
                data.len(),
                e.address
            );
            let fid = e.file_id.as_str();
            bsl.send_stage(e.address, data, self.opts.chunk, |d, t| progress(fid, d, t))?;
            written += data.len() as u64;
        }

        // ---- Cross-SDK format: erases + NV template + PREPACK --------------
        if self.opts.format {
            for op in &plan.erases {
                tracing::info!("erase {} ({:#x})", op.file_id, op.address);
                let mut payload = op.address.to_be_bytes().to_vec();
                payload.extend_from_slice(&op.param);
                bsl.command(
                    bsl::cmd::ERASE_FLASH,
                    &payload,
                    bsl::rep::ACK,
                    self.opts.timeout,
                    "ERASE",
                )?;
            }
            for e in &plan.data_markers {
                let data = e
                    .payload(pac)
                    .ok_or_else(|| FlashError::Payload(e.file_id.clone()))?;
                if plan::is_nv(&e.file_id) {
                    tracing::info!("write NV ({} bytes @ {:#x})", data.len(), e.address);
                    written += self.write_nv(&mut bsl, e.address, data, progress)?;
                } else {
                    tracing::info!(
                        "write {} ({} bytes @ {:#x})",
                        e.file_id,
                        data.len(),
                        e.address
                    );
                    let fid = e.file_id.as_str();
                    bsl.send_stage(e.address, data, self.opts.chunk, |d, t| progress(fid, d, t))?;
                    written += data.len() as u64;
                }
            }
        }

        // ---- Reset (with the flush-and-hold teardown) ----------------------
        if self.opts.reset {
            tracing::info!("reset; module reboots into the new firmware");
            bsl.send(bsl::cmd::NORMAL_RESET, &[])?;
            bsl.port().reset_teardown(self.opts.reset_hold);
        }

        Ok(FlashOutcome {
            version,
            bytes_written: written,
            seconds: start.elapsed().as_secs_f64(),
        })
    }

    /// Write the NV region: refresh its CRC-16-ARC, then a sum32-checked START,
    /// MIDST chunks, and END.
    fn write_nv(
        &self,
        bsl: &mut BslIo,
        addr: u32,
        data: &[u8],
        progress: Progress,
    ) -> Result<u64, FlashError> {
        let fixed = plan::nv_fix_crc(data);
        let start = plan::nv_start_payload(addr, &fixed);
        bsl.command(
            bsl::cmd::START_DATA,
            &start,
            bsl::rep::ACK,
            self.opts.timeout,
            "NV START",
        )?;
        let total = fixed.len() as u64;
        let mut sent = 0u64;
        for piece in fixed.chunks(self.opts.chunk.max(1)) {
            bsl.command(
                bsl::cmd::MIDST_DATA,
                piece,
                bsl::rep::ACK,
                self.opts.timeout,
                "NV MIDST",
            )?;
            sent += piece.len() as u64;
            progress("NV", sent, total);
        }
        // With the refreshed CRC this now ACKs (historically OPERATION_FAILED).
        bsl.command(
            bsl::cmd::END_DATA,
            &[],
            bsl::rep::ACK,
            self.opts.timeout,
            "NV END",
        )?;
        Ok(total)
    }
}
