// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! The [`Flasher`]: PDL → BSL → partitions → format → reset.

use std::time::{Duration, Instant};

use sprdflash_core::pac::PacInfo;
use sprdflash_core::{bsl, plan};
use sprdflash_transport::Transport;

use crate::FlashOptions;
use crate::bsl_io::{BslError, BslIo};
use crate::pdl_io::{PdlError, PdlIo};

/// Progress callback: `(stage, done_bytes, total_bytes)`.
pub type Progress<'a> = &'a mut dyn FnMut(&str, u64, u64);

/// Chunk size for loading the FDL stages, which run under the BootROM/FDL1 whose
/// receive buffer is small. Kept conservative; the large `opts.chunk` is used
/// only for partition/NV writes under FDL2 (which has a much bigger buffer).
const FDL_LOAD_CHUNK: usize = 2048;

/// Floor for the adaptive chunk back-off. 512 B is proven reliable over the
/// generic `cdc_acm` driver behind usbipd, where larger frames stall at the tail
/// of a big transfer.
const MIN_ADAPTIVE_CHUNK: usize = 512;

/// Read-back request size for `--verify-readback` (conservative for usbipd).
const READBACK_CHUNK: usize = 1024;

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
    /// A written partition read back different bytes (`--verify-readback`).
    #[error("read-back verify failed for {0}")]
    VerifyMismatch(String),
    /// The device rejected even the base flash address, so its size is unknown.
    #[error("could not read the flash base to size it")]
    FlashSize,
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
    /// Per-phase wall-clock seconds (`fdl1`, `fdl2`, `partitions`, `format`),
    /// for line balancing and bottleneck analysis.
    pub phases: Vec<(String, f64)>,
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
        port: &mut dyn Transport,
        info: &PacInfo,
        pac: &[u8],
        progress: Progress,
    ) -> Result<FlashOutcome, FlashError> {
        let start = Instant::now();
        let mut phases: Vec<(String, f64)> = Vec::new();
        let plan = plan::build(info).map_err(FlashError::Plan)?;
        let mut written = 0u64;

        // ---- Phases 1-2: PDL → FDL1 → BSL → FDL2 (shared with `dump`) -------
        let (mut bsl, version, fdl1_secs, fdl2_secs) = self.bring_up(port, pac, &plan, progress)?;
        phases.push(("fdl1".into(), fdl1_secs));
        phases.push(("fdl2".into(), fdl2_secs));
        let mut mark = Instant::now();

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
            self.send_stage_adaptive(&mut bsl, e.address, data, fid, progress)?;
            written += data.len() as u64;

            if self.opts.verify_readback {
                tracing::info!("verify {} ({} bytes)", e.file_id, data.len());
                let back = bsl.read_flash(e.address, data.len(), READBACK_CHUNK)?;
                if back != data {
                    tracing::error!("verify {}: {}", e.file_id, describe_mismatch(data, &back));
                    return Err(FlashError::VerifyMismatch(e.file_id.clone()));
                }
                progress(fid, data.len() as u64, data.len() as u64);
            }
        }

        phases.push(("partitions".into(), mark.elapsed().as_secs_f64()));
        mark = Instant::now();

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
                    self.send_stage_adaptive(&mut bsl, e.address, data, fid, progress)?;
                    written += data.len() as u64;
                }
            }
        }

        if self.opts.format {
            phases.push(("format".into(), mark.elapsed().as_secs_f64()));
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
            phases,
        })
    }

    /// Read `regions` (address, length) back from the device **without writing
    /// anything**: bring up FDL2, `READ_FLASH` each region, then reset. Returns
    /// one byte-vector per region, in order. Needs a `pac` only for this device's
    /// FDL1/FDL2 stages — the partition payloads are ignored.
    pub fn dump(
        &self,
        port: &mut dyn Transport,
        info: &PacInfo,
        pac: &[u8],
        regions: &[(u32, u32)],
        progress: Progress,
    ) -> Result<Vec<Vec<u8>>, FlashError> {
        let plan = plan::build(info).map_err(FlashError::Plan)?;
        let (mut bsl, version, _f1, _f2) = self.bring_up(port, pac, &plan, progress)?;
        tracing::info!("dumping via {version}");
        let mut out = Vec::with_capacity(regions.len());
        for (addr, len) in regions {
            tracing::info!("read {addr:#x} ({len} bytes)");
            let data = bsl.read_flash(*addr, *len as usize, READBACK_CHUNK)?;
            progress("read", data.len() as u64, u64::from(*len));
            out.push(data);
        }
        if self.opts.reset {
            bsl.send(bsl::cmd::NORMAL_RESET, &[])?;
            bsl.port().reset_teardown(self.opts.reset_hold);
        }
        Ok(out)
    }

    /// Dump the whole flash from `base` into one image, **discovering the flash
    /// size** by probing `READ_FLASH` (this minimal FDL2 has no geometry command:
    /// an in-range read replies `READ_FLASH`, an out-of-range one replies
    /// `INVALID_CMD`). Returns `(image, size)`. A full standalone backup — no
    /// partition layout needed.
    pub fn dump_full(
        &self,
        port: &mut dyn Transport,
        info: &PacInfo,
        pac: &[u8],
        base: u32,
        progress: Progress,
    ) -> Result<(Vec<u8>, u32), FlashError> {
        let plan = plan::build(info).map_err(FlashError::Plan)?;
        let (mut bsl, version, _f1, _f2) = self.bring_up(port, pac, &plan, progress)?;
        tracing::info!("discovering flash size via {version}");
        let size = discover_flash_size(&mut bsl, base, self.opts.timeout)?;
        tracing::info!("flash size: {size:#x} ({} MiB)", size >> 20);
        progress("size", u64::from(size), u64::from(size));

        let mut out = Vec::with_capacity(size as usize);
        while (out.len() as u32) < size {
            let off = out.len() as u32;
            let n = (size - off).min(READBACK_CHUNK as u32);
            let chunk = bsl.read_flash(base.wrapping_add(off), n as usize, READBACK_CHUNK)?;
            if chunk.is_empty() {
                break;
            }
            out.extend_from_slice(&chunk);
            progress("read", out.len() as u64, u64::from(size));
        }
        if self.opts.reset {
            bsl.send(bsl::cmd::NORMAL_RESET, &[])?;
            bsl.port().reset_teardown(self.opts.reset_hold);
        }
        Ok((out, size))
    }

    /// Bring up FDL2, send each `(cmd, payload)` in one session, and return the
    /// `(reply_type, data)` for each — for probing device capabilities (e.g.
    /// `READ_PARTITION`, flash geometry). Stops early if a command errors or
    /// times out (which desyncs the link). Resets afterwards.
    pub fn probe_commands(
        &self,
        port: &mut dyn Transport,
        info: &PacInfo,
        pac: &[u8],
        cmds: &[(u16, Vec<u8>)],
        progress: Progress,
    ) -> Result<Vec<(u16, Vec<u8>)>, FlashError> {
        let plan = plan::build(info).map_err(FlashError::Plan)?;
        let (mut bsl, _v, _f1, _f2) = self.bring_up(port, pac, &plan, progress)?;
        let mut replies = Vec::with_capacity(cmds.len());
        for (cmd, payload) in cmds {
            bsl.send(*cmd, payload)?;
            match bsl.recv(self.opts.timeout) {
                Ok(reply) => replies.push(reply),
                Err(e) if e.is_timeout() => {
                    replies.push((0, Vec::new())); // 0 = no reply / timeout
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }
        if self.opts.reset {
            bsl.send(bsl::cmd::NORMAL_RESET, &[])?;
            bsl.port().reset_teardown(self.opts.reset_hold);
        }
        Ok(replies)
    }

    /// Bring the device up to a connected FDL2 (PDL → FDL1 → BSL → FDL2 →
    /// optional `CHANGE_BAUD`). Returns the live BSL session, the FDL1 version
    /// banner, and the `(fdl1, fdl2)` phase durations. Shared by [`Self::run`]
    /// and [`Self::dump`].
    fn bring_up<'p>(
        &self,
        port: &'p mut dyn Transport,
        pac: &[u8],
        plan: &plan::FlashPlan<'_>,
        progress: Progress,
    ) -> Result<(BslIo<'p>, String, f64, f64), FlashError> {
        let fdl1 = plan
            .fdl1
            .payload(pac)
            .ok_or_else(|| FlashError::Payload(plan.fdl1.file_id.clone()))?;

        // ---- Phase 1: PDL loads + execs FDL1 -------------------------------
        let mut mark = Instant::now();
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
        let fdl1_secs = mark.elapsed().as_secs_f64();
        mark = Instant::now();

        // ---- Phase 2: BSL loads + execs FDL2 -------------------------------
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
        let fdl2_secs = mark.elapsed().as_secs_f64();

        Ok((bsl, version, fdl1_secs, fdl2_secs))
    }

    /// Write a staged image with an adaptive chunk: on a response timeout (the
    /// `cdc_acm`/usbipd tail-stall), re-`CONNECT` to reset the FDL2 write session
    /// and retry the whole stage at half the chunk, down to [`MIN_ADAPTIVE_CHUNK`].
    /// `START_DATA` re-initialises the write pointer, so re-sending is safe.
    fn send_stage_adaptive(
        &self,
        bsl: &mut BslIo,
        addr: u32,
        data: &[u8],
        label: &str,
        progress: Progress,
    ) -> Result<(), FlashError> {
        let mut chunk = self.opts.chunk.max(1);
        loop {
            match bsl.send_stage(addr, data, chunk, |d, t| progress(label, d, t)) {
                Ok(()) => return Ok(()),
                Err(e) if e.is_timeout() && chunk > MIN_ADAPTIVE_CHUNK => {
                    let next = (chunk / 2).max(MIN_ADAPTIVE_CHUNK);
                    tracing::warn!(
                        "{label}: response timeout at chunk {chunk}; re-connect and retry at {next}"
                    );
                    chunk = next;
                    bsl.port().purge_input();
                    bsl.connect()?; // reset the FDL2 session before restarting the stage
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Write the NV region (CRC-16-ARC refreshed, sum32-checked START), with the
    /// same adaptive-chunk retry as [`Self::send_stage_adaptive`].
    fn write_nv(
        &self,
        bsl: &mut BslIo,
        addr: u32,
        data: &[u8],
        progress: Progress,
    ) -> Result<u64, FlashError> {
        let fixed = plan::nv_fix_crc(data);
        let mut chunk = self.opts.chunk.max(1);
        loop {
            match self.write_nv_once(bsl, addr, &fixed, chunk, progress) {
                Ok(n) => return Ok(n),
                Err(e) if e.is_timeout() && chunk > MIN_ADAPTIVE_CHUNK => {
                    let next = (chunk / 2).max(MIN_ADAPTIVE_CHUNK);
                    tracing::warn!(
                        "NV: response timeout at chunk {chunk}; re-connect and retry at {next}"
                    );
                    chunk = next;
                    bsl.port().purge_input();
                    bsl.connect()?;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn write_nv_once(
        &self,
        bsl: &mut BslIo,
        addr: u32,
        fixed: &[u8],
        chunk: usize,
        progress: Progress,
    ) -> Result<u64, BslError> {
        let start = plan::nv_start_payload(addr, fixed);
        bsl.command(
            bsl::cmd::START_DATA,
            &start,
            bsl::rep::ACK,
            self.opts.timeout,
            "NV START",
        )?;
        let total = fixed.len() as u64;
        let mut sent = 0u64;
        for piece in fixed.chunks(chunk.max(1)) {
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
        // With the refreshed CRC this ACKs (historically OPERATION_FAILED).
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

/// Discover the flash size by probing `READ_FLASH`: exponentially find an
/// out-of-range offset, then binary-search the boundary down to 64 KiB. An
/// in-range read replies `READ_FLASH`; an out-of-range one replies a non-data
/// type (`INVALID_CMD` on RDA8910). Offsets are relative to `base`.
fn discover_flash_size(bsl: &mut BslIo, base: u32, timeout: Duration) -> Result<u32, FlashError> {
    const GRAN: u32 = 0x1_0000; // 64 KiB
    const CAP: u32 = 128 << 20; // never probe past 128 MiB

    fn probe(bsl: &mut BslIo, addr: u32, timeout: Duration) -> Result<bool, FlashError> {
        let mut p = Vec::with_capacity(12);
        p.extend_from_slice(&addr.to_be_bytes());
        p.extend_from_slice(&16u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        bsl.send(bsl::cmd::READ_FLASH, &p)?;
        let (ty, _) = bsl.recv(timeout)?;
        Ok(ty == bsl::rep::READ_FLASH)
    }

    if !probe(bsl, base, timeout)? {
        return Err(FlashError::FlashSize);
    }
    // Exponential search for the first out-of-range offset.
    let mut lo = 0u32; // base + lo is readable
    let mut s = 1u32 << 20; // 1 MiB
    let mut hi = loop {
        if s >= CAP {
            break CAP;
        }
        if probe(bsl, base.wrapping_add(s), timeout)? {
            lo = s;
            s <<= 1;
        } else {
            break s;
        }
    };
    // Binary search the boundary down to GRAN.
    while hi - lo > GRAN {
        let mid = lo + (((hi - lo) / 2) & !(GRAN - 1));
        if mid == lo {
            break;
        }
        if probe(bsl, base.wrapping_add(mid), timeout)? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(hi)
}

/// Summarize how a read-back differs from the written image, for diagnostics.
fn describe_mismatch(expected: &[u8], actual: &[u8]) -> String {
    if expected.len() != actual.len() {
        return format!(
            "read-back length {} != written {} bytes",
            actual.len(),
            expected.len()
        );
    }
    let differ = expected.iter().zip(actual).filter(|(a, b)| a != b).count();
    let first = expected
        .iter()
        .zip(actual)
        .position(|(a, b)| a != b)
        .unwrap_or(0);
    let end = (first + 16).min(expected.len());
    format!(
        "{differ}/{} bytes differ; first at offset {first}; expected {:02x?} got {:02x?}",
        expected.len(),
        &expected[first..end],
        &actual[first..end],
    )
}

#[cfg(test)]
mod mismatch_tests {
    use super::describe_mismatch;

    #[test]
    fn reports_first_diff_and_count() {
        let exp = [1u8, 2, 3, 4, 5];
        let act = [1u8, 2, 9, 4, 8];
        let s = describe_mismatch(&exp, &act);
        assert!(s.contains("2/5 bytes differ"), "{s}");
        assert!(s.contains("offset 2"), "{s}");
    }

    #[test]
    fn reports_length_difference() {
        let s = describe_mismatch(&[1, 2, 3], &[1, 2]);
        assert!(s.contains("length 2 != written 3"), "{s}");
    }
}
