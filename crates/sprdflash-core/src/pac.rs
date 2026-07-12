//! Parser and validator for SPRD/UNISOC `.pac` firmware packages.
//!
//! Layout: a fixed 2124-byte UTF-16LE header, a table of 2580-byte per-file
//! headers, then the raw payloads. Two CRC-16-ARC values protect the header and
//! the payload area. Everything operates on an in-memory slice (mmap the file
//! and pass `&mmap`), so parsing is zero-copy and allocation-light.

use crate::checksum::{crc16_arc, crc16_arc_from};
use crate::error::{Error, Result};

/// Size of the fixed PAC header, in bytes.
pub const HEADER_SIZE: usize = 2124;
/// Size of one per-file table entry, in bytes.
pub const FILE_HEADER_SIZE: usize = 2580;
/// Magic value expected in the header.
pub const MAGIC: u32 = 0xFFFA_FFFA;

// Header field offsets.
const OFF_PAC_SIZE: usize = 48;
const OFF_PRD_NAME: usize = 52;
const OFF_PRD_VERSION: usize = 564;
const OFF_FILE_COUNT: usize = 1076;
const OFF_FILE_OFFSET: usize = 1080;
const OFF_MODE: usize = 1084;
const OFF_FLASH_TYPE: usize = 1088;
const OFF_MAGIC: usize = 2116;
const OFF_CRC1: usize = 2120; // header crc
const OFF_CRC2: usize = 2122; // payload crc

// File-header field offsets.
const FOFF_FILE_ID: usize = 4;
const FOFF_FILE_NAME: usize = 516;
const FOFF_DATA_SIZE: usize = 1540;
const FOFF_FLAG: usize = 1544;
const FOFF_DATA_OFFSET: usize = 1552;
const FOFF_OMIT: usize = 1556;
const FOFF_ADDRESS: usize = 1564;

/// One entry in the PAC file table.
#[derive(Debug, Clone)]
pub struct PacEntry {
    /// Short identifier, e.g. `HOST_FDL`, `AP`, `NV`, `FMT_FSSYS`.
    pub file_id: String,
    /// Original file name (may be empty for markers).
    pub file_name: String,
    /// Payload size in bytes (0 for markers).
    pub size: u32,
    /// Byte offset of the payload within the PAC.
    pub offset: u32,
    /// Logical/physical load address (`>= LOGICAL_ADDRESS_BASE` ⇒ marker).
    pub address: u32,
    /// Raw flag field.
    pub flag: u32,
    /// Raw "omit" field.
    pub omit: u32,
}

impl PacEntry {
    /// True for pseudo-entries (erase/format/phase-check markers) with no payload.
    #[must_use]
    pub fn is_marker(&self) -> bool {
        self.size == 0
    }

    /// The payload bytes for this entry within `pac` (the full file buffer).
    #[must_use]
    pub fn payload<'a>(&self, pac: &'a [u8]) -> Option<&'a [u8]> {
        let start = self.offset as usize;
        let end = start.checked_add(self.size as usize)?;
        pac.get(start..end)
    }
}

/// Parsed and validated PAC metadata plus its file table.
#[derive(Debug, Clone)]
pub struct PacInfo {
    /// PAC format version string.
    pub version: String,
    /// Product name (e.g. `LuatOS-Air_V4035_...`).
    pub product_name: String,
    /// Product version string.
    pub product_version: String,
    /// Total size recorded in the header (== file length).
    pub size: u32,
    /// Download mode field.
    pub mode: u32,
    /// Flash type field.
    pub flash_type: u32,
    /// Magic value from the header.
    pub magic: u32,
    /// Whether the header CRC matched.
    pub header_crc_ok: bool,
    /// Whether the payload CRC matched (`None` if not checked).
    pub payload_crc_ok: Option<bool>,
    /// The file table, in file order.
    pub entries: Vec<PacEntry>,
}

impl PacInfo {
    /// True when the header CRC matches and the payload CRC did not fail.
    #[must_use]
    pub fn crc_ok(&self) -> bool {
        self.header_crc_ok && self.payload_crc_ok != Some(false)
    }
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&buf[off..off + 4]);
    u32::from_le_bytes(b)
}

fn u16_at(buf: &[u8], off: usize) -> u16 {
    let mut b = [0u8; 2];
    b.copy_from_slice(&buf[off..off + 2]);
    u16::from_le_bytes(b)
}

/// Decode a NUL-terminated UTF-16LE fixed field.
fn utf16z(raw: &[u8]) -> String {
    let mut s = String::new();
    let mut i = 0;
    while i + 1 < raw.len() {
        let u = u16::from_le_bytes([raw[i], raw[i + 1]]);
        if u == 0 {
            break;
        }
        // decode one UTF-16 unit (surrogate-tolerant via replacement)
        if let Some(c) = char::from_u32(u32::from(u)) {
            s.push(c);
        } else {
            s.push('\u{FFFD}');
        }
        i += 2;
    }
    s
}

/// Parse a `.pac` from an in-memory buffer, validating the CRCs.
///
/// `verify_payload` streams the (potentially large) payload CRC; skip it only
/// when speed matters more than integrity (it never should on the line).
pub fn parse(pac: &[u8], verify_payload: bool) -> Result<PacInfo> {
    let file_size = pac.len() as u64;
    if pac.len() < HEADER_SIZE {
        return Err(Error::PacTooSmall(file_size));
    }
    let header = &pac[..HEADER_SIZE];

    let pac_size = u32_at(header, OFF_PAC_SIZE);
    if u64::from(pac_size) != file_size {
        return Err(Error::PacSizeMismatch {
            declared: u64::from(pac_size),
            actual: file_size,
        });
    }

    let crc1 = u16_at(header, OFF_CRC1);
    let crc2 = u16_at(header, OFF_CRC2);
    let header_crc_ok = crc16_arc(&header[..OFF_CRC1]) == crc1;

    let file_count = u32_at(header, OFF_FILE_COUNT);
    let file_offset = u32_at(header, OFF_FILE_OFFSET) as usize;

    let mut entries = Vec::with_capacity(file_count as usize);
    for idx in 0..file_count {
        let base = file_offset + (idx as usize) * FILE_HEADER_SIZE;
        let fh = pac
            .get(base..base + FILE_HEADER_SIZE)
            .ok_or(Error::PacTruncatedTable(idx))?;
        let entry = PacEntry {
            file_id: utf16z(&fh[FOFF_FILE_ID..FOFF_FILE_ID + 512]),
            file_name: utf16z(&fh[FOFF_FILE_NAME..FOFF_FILE_NAME + 512]),
            size: u32_at(fh, FOFF_DATA_SIZE),
            offset: u32_at(fh, FOFF_DATA_OFFSET),
            address: u32_at(fh, FOFF_ADDRESS),
            flag: u32_at(fh, FOFF_FLAG),
            omit: u32_at(fh, FOFF_OMIT),
        };
        // A non-marker entry must point inside the buffer.
        if entry.size != 0 && entry.payload(pac).is_none() {
            return Err(Error::PacEntryOutOfRange {
                file_id: entry.file_id,
                offset: u64::from(entry.offset),
                size: u64::from(entry.size),
            });
        }
        entries.push(entry);
    }

    let payload_crc_ok = if verify_payload {
        // CRC over everything after the fixed header, streamed in 1 MiB chunks.
        let mut crc = 0u16;
        for chunk in pac[HEADER_SIZE..].chunks(1 << 20) {
            crc = crc16_arc_from(crc, chunk);
        }
        Some(crc == crc2)
    } else {
        None
    };

    Ok(PacInfo {
        version: utf16z(&header[..48]),
        product_name: utf16z(&header[OFF_PRD_NAME..OFF_PRD_NAME + 512]),
        product_version: utf16z(&header[OFF_PRD_VERSION..OFF_PRD_VERSION + 512]),
        size: pac_size,
        mode: u32_at(header, OFF_MODE),
        flash_type: u32_at(header, OFF_FLASH_TYPE),
        magic: u32_at(header, OFF_MAGIC),
        header_crc_ok,
        payload_crc_ok,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_too_small() {
        assert!(matches!(
            parse(&[0u8; 16], false),
            Err(Error::PacTooSmall(16))
        ));
    }

    #[test]
    fn utf16z_stops_at_nul() {
        let raw = b"A\x00B\x00\x00\x00C\x00";
        assert_eq!(utf16z(raw), "AB");
    }
}
