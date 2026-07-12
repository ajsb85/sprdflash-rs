//! Post-flash boot verification: confirm the module boots and read back its
//! firmware banner and IMEI over the AT port. This closes the loop on the line —
//! a unit is only "pass" once it has actually come up reporting the right build.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// What a booted module reports.
#[derive(Debug, Clone)]
pub struct BootInfo {
    /// `ATI` banner (e.g. `LuatOS-Air_V4035_...` / `CSDK_V302340_...`).
    pub firmware: String,
    /// `AT+CGSN` IMEI.
    pub imei: String,
}

/// Boot-verify errors.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// The AT port could not be opened.
    #[error("open AT port {0}: {1}")]
    Open(String, serialport::Error),
    /// The module did not answer `ATI` within the timeout.
    #[error("no ATI response from {0}")]
    NoResponse(String),
}

/// Send one AT command and collect the response until it goes quiet.
fn at_query(port: &mut Box<dyn serialport::SerialPort>, cmd: &str) -> String {
    let _ = port.clear(serialport::ClearBuffer::Input);
    let _ = port.write_all(format!("{cmd}\r\n").as_bytes());
    let _ = port.flush();
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    let deadline = Instant::now() + Duration::from_millis(700);
    while Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(n) if n > 0 => out.extend_from_slice(&buf[..n]),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Extract the meaningful line from an AT response (skip echo, `OK`, blanks).
fn meaningful(resp: &str, cmd: &str) -> Option<String> {
    resp.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && *l != "OK" && *l != cmd && !l.starts_with("AT"))
        .map(str::to_string)
}

/// Open `at_port` and read the firmware banner + IMEI, retrying until `timeout`.
pub fn verify_boot(at_port: &str, timeout: Duration) -> Result<BootInfo, VerifyError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(mut port) = serialport::new(at_port, 115_200)
            .timeout(Duration::from_millis(200))
            .open()
        {
            let ati = at_query(&mut port, "ATI");
            if let Some(firmware) = meaningful(&ati, "ATI") {
                let cgsn = at_query(&mut port, "AT+CGSN");
                let imei = meaningful(&cgsn, "AT+CGSN").unwrap_or_default();
                return Ok(BootInfo { firmware, imei });
            }
        }
        if Instant::now() >= deadline {
            return Err(VerifyError::NoResponse(at_port.to_string()));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}
