// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! BSL — the Spreadtrum/UNISOC download protocol over HDLC framing.
//!
//! Used for FDL2 and the partition writes (after PDL hands over). One message:
//!
//! ```text
//! 0x7e | escape( type[2 BE] size[2 BE] data[size] checksum[2 BE] ) | 0x7e
//! ```
//!
//! The checksum is either the Spreadtrum sum (RDA8910/UIS8910, the default) or
//! CRC-16 (classic BootROM); the check-baud handshake detects which.

use crate::checksum::{crc16_ccitt, sprd_sum};
use crate::error::{Error, Result};

/// HDLC frame flag.
pub const FLAG: u8 = 0x7E;
/// HDLC escape byte.
pub const ESCAPE: u8 = 0x7D;

/// Host → device command opcodes.
pub mod cmd {
    #![allow(missing_docs)]
    pub const CONNECT: u16 = 0x00;
    pub const START_DATA: u16 = 0x01;
    pub const MIDST_DATA: u16 = 0x02;
    pub const END_DATA: u16 = 0x03;
    pub const EXEC_DATA: u16 = 0x04;
    pub const NORMAL_RESET: u16 = 0x05;
    pub const READ_FLASH: u16 = 0x06;
    pub const READ_CHIP_TYPE: u16 = 0x07;
    pub const CHANGE_BAUD: u16 = 0x09;
    pub const ERASE_FLASH: u16 = 0x0A;
    pub const DISABLE_TRANSCODE: u16 = 0x21;
    pub const READ_CHIP_UID: u16 = 0x1A;
    pub const END_PROCESS: u16 = 0x7F;
}

/// Device → host reply opcodes.
pub mod rep {
    #![allow(missing_docs)]
    pub const ACK: u16 = 0x80;
    pub const VER: u16 = 0x81;
    pub const INVALID_CMD: u16 = 0x82;
    pub const UNKNOWN_CMD: u16 = 0x83;
    pub const OPERATION_FAILED: u16 = 0x84;
    pub const READ_FLASH: u16 = 0x93;
    pub const READ_CHIP_TYPE: u16 = 0x94;
    pub const READ_CHIP_UID: u16 = 0xAB;
    pub const UNSUPPORTED_COMMAND: u16 = 0xFE;
}

/// Human-readable name for a reply opcode (for diagnostics).
#[must_use]
pub fn rep_name(code: u16) -> &'static str {
    match code {
        rep::ACK => "ACK",
        rep::VER => "VER",
        rep::INVALID_CMD => "INVALID_CMD",
        rep::UNKNOWN_CMD => "UNKNOWN_CMD",
        rep::OPERATION_FAILED => "OPERATION_FAILED",
        rep::READ_FLASH => "READ_FLASH",
        rep::READ_CHIP_TYPE => "READ_CHIP_TYPE",
        rep::READ_CHIP_UID => "READ_CHIP_UID",
        rep::UNSUPPORTED_COMMAND => "UNSUPPORTED_COMMAND",
        _ => "?",
    }
}

/// Which checksum a BSL session uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checksum {
    /// Spreadtrum ones-complement sum (RDA8910/UIS8910).
    Sprd,
    /// CRC-16 (classic BootROM).
    Crc,
}

impl Checksum {
    #[must_use]
    fn compute(self, body: &[u8]) -> u16 {
        match self {
            Checksum::Sprd => sprd_sum(body),
            Checksum::Crc => crc16_ccitt(body),
        }
    }
}

/// HDLC-escape a frame body (never touches the outer 0x7e flags).
#[must_use]
pub fn escape(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len());
    for &b in frame {
        if b == FLAG || b == ESCAPE {
            out.push(ESCAPE);
            out.push(b ^ 0x20);
        } else {
            out.push(b);
        }
    }
    out
}

/// Reverse [`escape`].
#[must_use]
pub fn unescape(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len());
    let mut it = frame.iter();
    while let Some(&b) = it.next() {
        if b == ESCAPE {
            if let Some(&nxt) = it.next() {
                out.push(nxt ^ 0x20);
            }
        } else {
            out.push(b);
        }
    }
    out
}

/// Frame one BSL message for the wire (including the outer 0x7e flags).
#[must_use]
pub fn build_message(cmd: u16, data: &[u8], checksum: Checksum) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + data.len() + 2);
    body.extend_from_slice(&cmd.to_be_bytes());
    body.extend_from_slice(&(data.len() as u16).to_be_bytes());
    body.extend_from_slice(data);
    let ck = checksum.compute(&body);
    body.extend_from_slice(&ck.to_be_bytes());

    let mut msg = Vec::with_capacity(body.len() + 2);
    msg.push(FLAG);
    msg.extend_from_slice(&escape(&body));
    msg.push(FLAG);
    msg
}

/// Decode an already-unescaped message body (no 0x7e flags): returns
/// `(type, data)`.
pub fn parse_message(body: &[u8]) -> Result<(u16, &[u8])> {
    if body.len() < 6 {
        return Err(Error::FrameTooShort(body.len()));
    }
    let cmd = u16::from_be_bytes([body[0], body[1]]);
    let size = usize::from(u16::from_be_bytes([body[2], body[3]]));
    let data = &body[4..];
    // data + 2-byte checksum must fit
    if data.len() < size + 2 {
        return Err(Error::FrameTruncated {
            declared: size,
            got: data.len().saturating_sub(2),
        });
    }
    Ok((cmd, &data[..size]))
}

/// Detect which checksum validates an unescaped frame body, if any.
#[must_use]
pub fn detect_checksum(body: &[u8]) -> Option<Checksum> {
    if body.len() < 6 {
        return None;
    }
    let (payload, chk) = body.split_at(body.len() - 2);
    let chk = u16::from_be_bytes([chk[0], chk[1]]);
    if sprd_sum(payload) == chk {
        Some(Checksum::Sprd)
    } else if crc16_ccitt(payload) == chk {
        Some(Checksum::Crc)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_frame_matches_capture() {
        // Ground truth: CONNECT = 7e 00000000 ffff 7e
        let m = build_message(cmd::CONNECT, &[], Checksum::Sprd);
        assert_eq!(hex(&m), "7e00000000ffff7e");
    }

    #[test]
    fn change_baud_frame_matches_capture() {
        // Ground truth: CHANGE_BAUD to 115200 (0x0001c200) = 7e 0009 0004 0001c200 3df1 7e
        let m = build_message(
            cmd::CHANGE_BAUD,
            &0x0001_c200u32.to_be_bytes(),
            Checksum::Sprd,
        );
        assert_eq!(hex(&m), "7e000900040001c2003df17e");
    }

    #[test]
    fn escape_roundtrip_including_flags() {
        let raw = [0x00, 0x7e, 0x11, 0x7d, 0x22];
        assert_eq!(unescape(&escape(&raw)), raw);
    }

    #[test]
    fn parse_rejects_short() {
        assert!(matches!(
            parse_message(&[0, 0]),
            Err(Error::FrameTooShort(2))
        ));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
