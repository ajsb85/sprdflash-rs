// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! PDL link-layer I/O over a [`Serial`] stream (first stage: loads FDL1).

use std::time::{Duration, Instant};

use sprdflash_core::pdl;
use sprdflash_transport::{Transport, TransportError, read_until};

/// PDL I/O errors.
#[derive(Debug, thiserror::Error)]
pub enum PdlError {
    /// Transport failure.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// No BSL VER frame after EXEC.
    #[error("no BSL VER after PDL EXEC")]
    NoVersion,
}

/// PDL transport bound to a byte stream.
pub struct PdlIo<'a> {
    port: &'a mut dyn Transport,
    timeout: Duration,
}

impl<'a> PdlIo<'a> {
    /// Wrap a transport with a default per-command `timeout`.
    pub fn new(port: &'a mut dyn Transport, timeout: Duration) -> Self {
        Self { port, timeout }
    }

    /// Send one PDL message: header write, then payload write (must be separate).
    fn send(&mut self, payload: &[u8]) -> Result<(), PdlError> {
        let header = pdl::header(payload.len() as u16);
        self.port.write_all(&header)?;
        self.port.write_all(payload)?;
        Ok(())
    }

    /// Send a message and wait for its `ae`-framed response.
    fn command(&mut self, payload: &[u8], timeout: Duration) -> Result<pdl::Response, PdlError> {
        self.send(payload)?;
        let deadline = Instant::now() + timeout;
        let mut acc = Vec::new();
        let resp = read_until(self.port, &mut acc, deadline, pdl::try_parse_response)?;
        Ok(resp)
    }

    /// PDL CONNECT handshake.
    pub fn connect(&mut self) -> Result<(), PdlError> {
        self.command(&pdl::params(pdl::Cmd::Connect, 0, 0, &[]), self.timeout)?;
        Ok(())
    }

    /// Load an image (FDL1) at `addr`: START → MIDST* → END.
    ///
    /// The END image checksum is unverified by the agent (a full flash with
    /// checksum 0 boots correctly), so 0 is sent.
    pub fn send_image(
        &mut self,
        addr: u32,
        data: &[u8],
        mut progress: impl FnMut(u64, u64),
    ) -> Result<(), PdlError> {
        self.command(
            &pdl::params(pdl::Cmd::StartData, addr, data.len() as u32, b"PDL1\x00"),
            self.timeout,
        )?;
        let total = data.len() as u64;
        for (i, chunk) in data.chunks(pdl::CHUNK).enumerate() {
            self.command(
                &pdl::params(pdl::Cmd::MidstData, i as u32, chunk.len() as u32, chunk),
                self.timeout,
            )?;
            progress(((i as u64) * pdl::CHUNK as u64) + chunk.len() as u64, total);
        }
        self.command(
            &pdl::params(pdl::Cmd::EndData, 0, 0, &0u32.to_le_bytes()),
            self.timeout,
        )?;
        Ok(())
    }

    /// EXEC the loaded FDL1, then drive the BSL check-baud (lone `0x7e`) until the
    /// device replies with its `0x7e`-framed VER. Returns the raw VER frame.
    pub fn exec_and_get_ver(&mut self, timeout: Duration) -> Result<Vec<u8>, PdlError> {
        // EXEC = the PDL cmd=7 frame with no extra byte...
        self.send(&pdl::params(pdl::Cmd::Exec, 0, 0, &[]))?;
        // ...immediately followed by BSL check-baud kicks until VER arrives.
        let deadline = Instant::now() + timeout;
        let mut acc = Vec::new();
        let mut next_kick = Instant::now();
        loop {
            if Instant::now() >= deadline {
                return Err(PdlError::NoVersion);
            }
            if Instant::now() >= next_kick {
                self.port.write_all(&[0x7e])?;
                next_kick = Instant::now() + Duration::from_millis(50);
            }
            let mut tmp = [0u8; 256];
            let n = self.port.read_some(&mut tmp)?;
            if n > 0 {
                acc.extend_from_slice(&tmp[..n]);
                if let Some(frame) = extract_flag_frame(&acc) {
                    return Ok(frame);
                }
            }
        }
    }
}

/// Extract the first complete `0x7e … 0x7e` frame (inclusive of flags).
fn extract_flag_frame(buf: &[u8]) -> Option<Vec<u8>> {
    let first = buf.iter().position(|&b| b == 0x7e)?;
    let rest = &buf[first + 1..];
    let end = rest.iter().position(|&b| b == 0x7e)?;
    Some(buf[first..first + 1 + end + 1].to_vec())
}
