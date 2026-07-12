// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! BSL link-layer I/O over a [`Serial`] stream (FDL2 + partitions).

use std::time::{Duration, Instant};

use sprdflash_core::bsl::{self, Checksum};
use sprdflash_transport::{Transport, TransportError, read_until};

/// BSL I/O errors.
#[derive(Debug, thiserror::Error)]
pub enum BslError {
    /// Transport failure.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// A frame could not be decoded.
    #[error("bad BSL frame: {0}")]
    Frame(#[from] sprdflash_core::Error),
    /// The device replied with an unexpected type.
    #[error("{what}: expected {expected}, got {got}")]
    Unexpected {
        /// What we were doing.
        what: String,
        /// Expected reply name.
        expected: &'static str,
        /// Actual reply name.
        got: &'static str,
    },
}

impl BslError {
    /// True if this is a response timeout — worth retrying the stage at a smaller
    /// chunk (the tail-stall the generic `cdc_acm` driver hits over usbipd).
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, BslError::Transport(TransportError::Timeout(_)))
    }
}

/// BSL transport bound to a byte stream.
pub struct BslIo<'a> {
    port: &'a mut dyn Transport,
    /// Session checksum (Spreadtrum sum on RDA8910/UIS8910).
    pub checksum: Checksum,
    timeout: Duration,
}

impl<'a> BslIo<'a> {
    /// Wrap a transport; RDA8910/UIS8910 speaks the Spreadtrum sum.
    pub fn new(port: &'a mut dyn Transport, timeout: Duration) -> Self {
        Self {
            port,
            checksum: Checksum::Sprd,
            timeout,
        }
    }

    /// Frame and send one BSL command (no reply wait).
    pub fn send(&mut self, cmd: u16, data: &[u8]) -> Result<(), BslError> {
        let msg = bsl::build_message(cmd, data, self.checksum);
        self.port.write_all(&msg)?;
        Ok(())
    }

    /// Read one framed reply, returning `(type, data)`.
    pub fn recv(&mut self, timeout: Duration) -> Result<(u16, Vec<u8>), BslError> {
        let deadline = Instant::now() + timeout;
        let mut acc = Vec::new();
        let body = read_until(self.port, &mut acc, deadline, find_frame_body)?;
        let (t, data) = bsl::parse_message(&body)?;
        Ok((t, data.to_vec()))
    }

    /// Send a command and require `expect` as the reply type.
    pub fn command(
        &mut self,
        cmd: u16,
        data: &[u8],
        expect: u16,
        timeout: Duration,
        what: &str,
    ) -> Result<Vec<u8>, BslError> {
        self.send(cmd, data)?;
        let (t, d) = self.recv(timeout)?;
        if t != expect {
            return Err(BslError::Unexpected {
                what: what.to_string(),
                expected: bsl::rep_name(expect),
                got: bsl::rep_name(t),
            });
        }
        Ok(d)
    }

    /// CONNECT handshake (expects ACK).
    pub fn connect(&mut self) -> Result<(), BslError> {
        self.command(
            bsl::cmd::CONNECT,
            &[],
            bsl::rep::ACK,
            self.timeout,
            "CONNECT",
        )?;
        Ok(())
    }

    /// Write a staged image: `START_DATA(addr,size)` → `MIDST_DATA*` → `END_DATA`.
    pub fn send_stage(
        &mut self,
        addr: u32,
        data: &[u8],
        chunk: usize,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<(), BslError> {
        let mut start = Vec::with_capacity(8);
        start.extend_from_slice(&addr.to_be_bytes());
        start.extend_from_slice(&(data.len() as u32).to_be_bytes());
        self.command(
            bsl::cmd::START_DATA,
            &start,
            bsl::rep::ACK,
            self.timeout,
            "START_DATA",
        )?;
        let total = data.len() as u64;
        let mut sent = 0u64;
        for piece in data.chunks(chunk.max(1)) {
            self.command(
                bsl::cmd::MIDST_DATA,
                piece,
                bsl::rep::ACK,
                self.timeout,
                "MIDST_DATA",
            )?;
            sent += piece.len() as u64;
            progress(sent, total);
        }
        self.command(
            bsl::cmd::END_DATA,
            &[],
            bsl::rep::ACK,
            self.timeout,
            "END_DATA",
        )?;
        Ok(())
    }

    /// Read `total` bytes from logical `addr` via BSL READ_FLASH, in `chunk`-byte
    /// requests (payload = addr | size | offset, all big-endian).
    pub fn read_flash(
        &mut self,
        addr: u32,
        total: usize,
        chunk: usize,
    ) -> Result<Vec<u8>, BslError> {
        let mut out = Vec::with_capacity(total);
        while out.len() < total {
            let off = out.len();
            let n = (total - off).min(chunk.max(1));
            let mut p = Vec::with_capacity(12);
            // Advance the address per chunk. The RDA8910/UIS8910 FDL2 reads from
            // `addr` and does not honour a separate offset field, so we fold the
            // running offset into `addr` and send a zero offset — correct whether
            // the device reads `addr` or `addr + offset`.
            p.extend_from_slice(&addr.wrapping_add(off as u32).to_be_bytes());
            p.extend_from_slice(&(n as u32).to_be_bytes());
            p.extend_from_slice(&0u32.to_be_bytes());
            let data = self.command(
                bsl::cmd::READ_FLASH,
                &p,
                bsl::rep::READ_FLASH,
                self.timeout,
                "READ_FLASH",
            )?;
            if data.is_empty() {
                break; // device returned nothing; let the caller's length check fail
            }
            out.extend_from_slice(&data);
        }
        Ok(out)
    }

    /// Access the underlying transport (for baud changes / teardown).
    pub fn port(&mut self) -> &mut dyn Transport {
        self.port
    }
}

/// Find the first complete BSL frame in `buf` and return its unescaped body.
fn find_frame_body(buf: &[u8]) -> Option<Vec<u8>> {
    // skip to the opening flag, then over any run of flags
    let mut i = buf.iter().position(|&b| b == bsl::FLAG)?;
    while i < buf.len() && buf[i] == bsl::FLAG {
        i += 1;
    }
    if i >= buf.len() {
        return None;
    }
    let rel = buf[i..].iter().position(|&b| b == bsl::FLAG)?;
    Some(bsl::unescape(&buf[i..i + rel]))
}
