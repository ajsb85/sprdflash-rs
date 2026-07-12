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

    /// Write all of `data` to the wire. PDL requires the header and payload to be
    /// *separate* `write_all` calls, which this preserves (each maps to its own
    /// `WriteFile`). `flush` is best-effort: the `sprd_rdavcom` virtual COM driver
    /// rejects `FlushFileBuffers` with ERROR_INVALID_FUNCTION, and the bytes are
    /// already handed to the driver by `write_all`.
    pub fn write_all(&mut self, data: &[u8]) -> Result<(), TransportError> {
        self.port.write_all(data)?;
        let _ = self.port.flush();
        Ok(())
    }

    /// Read whatever bytes are available into `buf`, returning the count. A
    /// timeout yields `Ok(0)` rather than an error, so callers can poll against
    /// their own deadline.
    pub fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    /// Read until `pred(accumulated)` returns `Some(value)` or `deadline` passes.
    /// Accumulated bytes are kept in `acc` so partial frames survive across calls.
    pub fn read_until<T>(
        &mut self,
        acc: &mut Vec<u8>,
        deadline: Instant,
        mut pred: impl FnMut(&[u8]) -> Option<T>,
    ) -> Result<T, TransportError> {
        if let Some(v) = pred(acc) {
            return Ok(v);
        }
        let mut tmp = [0u8; 4096];
        while Instant::now() < deadline {
            let n = self.read_some(&mut tmp)?;
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

    /// Set DTR and RTS together.
    pub fn set_dtr_rts(&mut self, on: bool) -> Result<(), TransportError> {
        self.port.write_data_terminal_ready(on)?;
        self.port.write_request_to_send(on)?;
        Ok(())
    }

    /// Change the line baud rate (for `CHANGE_BAUD`).
    pub fn set_baud(&mut self, baud: u32) -> Result<(), TransportError> {
        self.port.set_baud_rate(baud)?;
        Ok(())
    }

    /// Discard any buffered input (before a fresh handshake).
    pub fn purge_input(&mut self) {
        let _ = self.port.clear(serialport::ClearBuffer::Input);
    }

    /// Reset teardown: flush the last frame and hold the line briefly before the
    /// port is dropped. Closing immediately cancels an in-flight `NORMAL_RESET`
    /// (Windows `CloseHandle` aborts pending I/O), leaving the module in download
    /// mode instead of booting.
    pub fn reset_teardown(&mut self, hold: Duration) {
        let _ = self.port.flush();
        std::thread::sleep(hold);
    }
}
