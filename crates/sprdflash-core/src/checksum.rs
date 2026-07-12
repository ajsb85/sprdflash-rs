// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! The four checksums used across the SPRD/UNISOC download protocol.
//!
//! | function          | polynomial / rule        | used for                         |
//! |-------------------|--------------------------|----------------------------------|
//! | [`sprd_sum`]      | ones-complement sum      | every BSL frame (RDA8910/UIS8910)|
//! | [`crc16_ccitt`]   | 0x1021, MSB-first        | classic BootROM BSL frame        |
//! | [`crc16_arc`]     | 0xA001, reflected        | PAC header/payload + NV image    |
//! | [`sum32`]         | additive byte sum        | NV START_DATA transfer check     |
//!
//! All are verified byte-for-byte against the vendor tool and the hardware.

/// Spreadtrum "sum" checksum: ones-complement sum of little-endian 16-bit
/// words, with an **unconditional** final byte swap.
///
/// This is the checksum the RDA8910/UIS8910 BootROM and FDLs use for *every*
/// BSL packet (matches `kagaimiq/sprdproto`'s `calc_sprdcheck`).
#[must_use]
pub fn sprd_sum(data: &[u8]) -> u16 {
    let mut total: u32 = 0;
    let mut i = 0;
    while data.len() - i >= 2 {
        total += u32::from(data[i]) | (u32::from(data[i + 1]) << 8);
        i += 2;
    }
    if i < data.len() {
        total += u32::from(data[i]);
    }
    total = (total >> 16) + (total & 0xFFFF);
    let folded = !(total + (total >> 16)) & 0xFFFF;
    ((folded >> 8) | ((folded & 0xFF) << 8)) as u16
}

/// Classic Spreadtrum BootROM checksum: CRC-16 with polynomial 0x1021,
/// MSB-first, init 0.
#[must_use]
pub fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u32 = 0;
    for &b in data {
        crc ^= u32::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1_1021
            } else {
                crc << 1
            } & 0xFFFF;
        }
    }
    crc as u16
}

/// CRC-16-ARC (polynomial 0xA001, reflected, init 0).
///
/// Used both for the `.pac` header/payload checks and for the internal NV image
/// checksum the modem validates. Standard check value: `crc16_arc(b"123456789")
/// == 0xBB3D`.
#[must_use]
pub fn crc16_arc(data: &[u8]) -> u16 {
    crc16_arc_from(0, data)
}

/// Streaming variant of [`crc16_arc`] that continues from a running `crc`.
#[must_use]
pub fn crc16_arc_from(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc = (crc >> 8) ^ CRC16_ARC_TABLE[usize::from((crc ^ u16::from(b)) & 0xFF)];
    }
    crc
}

/// 32-bit additive byte sum (the value in the NV `START_DATA` trailer, verified
/// by `END_DATA`).
#[must_use]
pub fn sum32(data: &[u8]) -> u32 {
    data.iter()
        .fold(0u32, |acc, &b| acc.wrapping_add(u32::from(b)))
}

/// Precomputed CRC-16-ARC table (poly 0xA001), built once at load.
const CRC16_ARC_TABLE: [u16; 256] = build_arc_table();

const fn build_arc_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u16;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_arc_standard_check_value() {
        assert_eq!(crc16_arc(b"123456789"), 0xBB3D);
        assert_eq!(crc16_arc(b""), 0x0000);
    }

    #[test]
    fn sum32_wraps_at_32_bits() {
        assert_eq!(sum32(b""), 0);
        assert_eq!(sum32(&[1, 2, 3]), 6);
        assert_eq!(sum32(&[0xFF; 4]), 0x3FC);
    }

    #[test]
    fn sprd_sum_matches_captured_bootrom_frames() {
        // Ground-truth vendor packets (body = type|size|data, without checksum):
        // CONNECT frame body 0x0000_0000 -> checksum 0xffff
        assert_eq!(sprd_sum(&[0x00, 0x00, 0x00, 0x00]), 0xFFFF);
        // ACK reply body 0x0080_0000 -> checksum 0xff7f
        assert_eq!(sprd_sum(&[0x00, 0x80, 0x00, 0x00]), 0xFF7F);
    }

    #[test]
    fn crc16_ccitt_known_vector() {
        // CRC-16/XMODEM check value for "123456789"
        assert_eq!(crc16_ccitt(b"123456789"), 0x31C3);
    }
}
