// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! A protocol-simulating [`Transport`] for hardware-free end-to-end flash tests.
//!
//! [`MockTransport`] answers the PDL and BSL protocols like a healthy device —
//! PDL command acks, the BSL `VER` banner after `EXEC`, and an `ACK` for every
//! BSL frame — so the entire [`crate::Flasher`] flow can be exercised in CI with
//! no serial port. Optional fault injection drops selected BSL acks to exercise
//! the timeout / adaptive-retry paths.

use std::collections::VecDeque;
use std::time::Duration;

use sprdflash_core::bsl::{self, Checksum};
use sprdflash_core::pdl;
use sprdflash_transport::{Transport, TransportError};

const VER_BANNER: &[u8] = b"Spreadtrum Boot Block version 1.2\0";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Pdl,
    Bsl,
}

/// A mock device that speaks just enough of the download protocol to drive a
/// full flash. Construct with [`MockTransport::default`], or
/// [`MockTransport::dropping_bsl_acks`] to inject timeouts.
pub struct MockTransport {
    mode: Mode,
    exec_seen: bool,
    in_buf: Vec<u8>,
    out: VecDeque<u8>,
    bsl_frames: usize,
    drop_acks: Vec<usize>,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self {
            mode: Mode::Pdl,
            exec_seen: false,
            in_buf: Vec::new(),
            out: VecDeque::new(),
            bsl_frames: 0,
            drop_acks: Vec::new(),
        }
    }
}

impl MockTransport {
    /// A mock that withholds the ack for the given 1-based BSL frame indices
    /// (each triggers one host-side timeout, exercising the adaptive retry).
    #[must_use]
    pub fn dropping_bsl_acks(indices: &[usize]) -> Self {
        Self {
            drop_acks: indices.to_vec(),
            ..Self::default()
        }
    }

    fn process(&mut self) {
        loop {
            match self.mode {
                Mode::Pdl if !self.exec_seen => {
                    if self.in_buf.len() < pdl::HEADER_LEN || self.in_buf[0] != pdl::MAGIC {
                        break;
                    }
                    let len = usize::from(u16::from_le_bytes([self.in_buf[1], self.in_buf[2]]));
                    let total = pdl::HEADER_LEN + len;
                    if self.in_buf.len() < total || len < 4 {
                        break;
                    }
                    let cmd = u32::from_le_bytes([
                        self.in_buf[8],
                        self.in_buf[9],
                        self.in_buf[10],
                        self.in_buf[11],
                    ]);
                    self.in_buf.drain(..total);
                    if cmd == pdl::Cmd::Exec as u32 {
                        self.exec_seen = true; // VER follows the next checkbaud 0x7e
                    } else {
                        self.out.extend(pdl::header(4)); // ae response, rlen=4
                        self.out.extend([0u8; 4]); // status = OK
                    }
                }
                Mode::Pdl => {
                    if self.in_buf.iter().any(|&b| b == bsl::FLAG) {
                        self.in_buf.clear();
                        self.out.extend(bsl::build_message(
                            bsl::rep::VER,
                            VER_BANNER,
                            Checksum::Sprd,
                        ));
                        self.mode = Mode::Bsl;
                    } else {
                        break;
                    }
                }
                Mode::Bsl => {
                    let Some(i0) = self.in_buf.iter().position(|&b| b == bsl::FLAG) else {
                        break;
                    };
                    let Some(rel) = self.in_buf[i0 + 1..].iter().position(|&b| b == bsl::FLAG)
                    else {
                        break;
                    };
                    let i1 = i0 + 1 + rel;
                    let body = bsl::unescape(&self.in_buf[i0 + 1..i1]);
                    self.in_buf.drain(..=i1);
                    self.bsl_frames += 1;
                    let ty = if body.len() >= 2 {
                        u16::from_be_bytes([body[0], body[1]])
                    } else {
                        0
                    };
                    // NORMAL_RESET gets no reply; a dropped ack simulates a stall.
                    if ty != bsl::cmd::NORMAL_RESET && !self.drop_acks.contains(&self.bsl_frames) {
                        self.out
                            .extend(bsl::build_message(bsl::rep::ACK, &[], Checksum::Sprd));
                    }
                }
            }
        }
    }
}

impl Transport for MockTransport {
    fn write_all(&mut self, data: &[u8]) -> Result<(), TransportError> {
        self.in_buf.extend_from_slice(data);
        self.process();
        Ok(())
    }

    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let n = buf.len().min(self.out.len());
        for slot in buf.iter_mut().take(n) {
            *slot = self.out.pop_front().expect("out has n bytes");
        }
        Ok(n)
    }

    fn set_baud(&mut self, _baud: u32) -> Result<(), TransportError> {
        Ok(())
    }

    fn purge_input(&mut self) {
        self.in_buf.clear();
    }

    fn reset_teardown(&mut self, _hold: Duration) {}
}
