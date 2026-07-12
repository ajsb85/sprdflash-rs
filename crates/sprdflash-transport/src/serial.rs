// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! The [`Serial`] byte stream — a thin, robust wrapper over `serialport`.
//!
//! Everything the download protocol needs from the wire is here: separate
//! header/payload writes (PDL requires them), a deadline-based reader for the
//! frame parsers, DTR/RTS control, live baud changes (`CHANGE_BAUD`), and the
//! flush-and-hold reset teardown that lets `NORMAL_RESET` actually boot the
//! module instead of dropping it back into download mode.

use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use serialport::SerialPort;

/// Transport-layer errors (I/O and timeouts).
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The port could not be opened within the retry budget.
    #[error("could not open {port}: {source}")]
    Open {
        /// Port name.
        port: String,
        /// Underlying error.
        source: serialport::Error,
    },
    /// A read or write failed.
    #[error("serial I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A line-control operation (baud/DTR/RTS) failed.
    #[error("serial control error: {0}")]
    Control(#[from] serialport::Error),
    /// A read did not complete before its deadline.
    #[error("serial read timed out after {0:?}")]
    Timeout(Duration),
}

/// The byte-stream operations the download protocol needs. Abstracting the wire
/// behind this trait lets the flash driver run over a real serial port
/// ([`Serial`]) or a mock end-to-end in tests, with no hardware.
pub trait Transport {
    /// Write all of `data` as one wire write (PDL needs the header and payload
    /// to be *separate* `write_all` calls).
    fn write_all(&mut self, data: &[u8]) -> Result<(), TransportError>;
    /// Read whatever bytes are available; a timeout yields `Ok(0)`, not an error.
    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
    /// Change the line baud rate (for `CHANGE_BAUD`).
    fn set_baud(&mut self, baud: u32) -> Result<(), TransportError>;
    /// Discard any buffered input (before a fresh handshake).
    fn purge_input(&mut self);
    /// Flush and hold the line briefly before the port is dropped, so an
    /// in-flight `NORMAL_RESET` lands instead of being cancelled.
    fn reset_teardown(&mut self, hold: Duration);
}

/// Read until `pred(accumulated)` returns `Some(value)` or `deadline` passes.
/// Accumulated bytes are kept in `acc` so partial frames survive across calls.
pub fn read_until<R>(
    port: &mut dyn Transport,
    acc: &mut Vec<u8>,
    deadline: Instant,
    mut pred: impl FnMut(&[u8]) -> Option<R>,
) -> Result<R, TransportError> {
    if let Some(v) = pred(acc) {
        return Ok(v);
    }
    let mut tmp = [0u8; 4096];
    while Instant::now() < deadline {
        let n = port.read_some(&mut tmp)?;
        if n > 0 {
            acc.extend_from_slice(&tmp[..n]);
            if let Some(v) = pred(acc) {
                return Ok(v);
            }
        }
    }
    Err(TransportError::Timeout(
        deadline.saturating_duration_since(Instant::now()),
    ))
}

/// A robust serial byte stream for the download protocol.
pub struct Serial {
    port: Box<dyn SerialPort>,
    name: String,
}

impl Serial {
    /// Open `port` at `baud`, retrying briefly (Windows often needs a moment
    /// between a port appearing and it being openable).
    pub fn open(port: &str, baud: u32) -> Result<Self, TransportError> {
        let mut last = None;
        for attempt in 0..200 {
            match serialport::new(port, baud)
                // Short per-read timeout: the frame readers manage their own
                // overall deadline and poll in small slices.
                .timeout(Duration::from_millis(20))
                .open()
            {
                Ok(mut p) => {
                    // Assert carrier so a CDC-ACM gadget starts responding.
                    let _ = p.write_data_terminal_ready(true);
                    let _ = p.write_request_to_send(true);
                    let _ = p.clear(serialport::ClearBuffer::Input);
                    return Ok(Self {
                        port: p,
                        name: port.to_string(),
                    });
                }
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(30));
                    let _ = attempt;
                }
            }
        }
        Err(TransportError::Open {
            port: port.to_string(),
            source: last.expect("at least one attempt"),
        })
    }

    /// Port name (e.g. `COM34` or `/dev/ttyUSB0`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Set DTR and RTS together.
    pub fn set_dtr_rts(&mut self, on: bool) -> Result<(), TransportError> {
        self.port.write_data_terminal_ready(on)?;
        self.port.write_request_to_send(on)?;
        Ok(())
    }
}

impl Transport for Serial {
    /// `flush` is best-effort: the `sprd_rdavcom` virtual COM driver rejects
    /// `FlushFileBuffers` with ERROR_INVALID_FUNCTION, and the bytes are already
    /// handed to the driver by `write_all`.
    fn write_all(&mut self, data: &[u8]) -> Result<(), TransportError> {
        self.port.write_all(data)?;
        let _ = self.port.flush();
        Ok(())
    }

    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    fn set_baud(&mut self, baud: u32) -> Result<(), TransportError> {
        self.port.set_baud_rate(baud)?;
        Ok(())
    }

    fn purge_input(&mut self) {
        let _ = self.port.clear(serialport::ClearBuffer::Input);
    }

    fn reset_teardown(&mut self, hold: Duration) {
        let _ = self.port.flush();
        std::thread::sleep(hold);
    }
}
