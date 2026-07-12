// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! End-to-end flash of a synthetic PAC against the [`MockTransport`] — the whole
//! PDL → BSL → partitions → format → reset flow, with no hardware.

use std::time::Duration;

use sprdflash_core::checksum::crc16_arc;
use sprdflash_core::pac::{self, FILE_HEADER_SIZE, HEADER_SIZE, MAGIC};
use sprdflash_flash::mock::MockTransport;
use sprdflash_flash::{FlashOptions, Flasher};

fn put_u32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn put_u16(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn put_utf16(buf: &mut [u8], off: usize, s: &str) {
    for (i, u) in s.encode_utf16().enumerate() {
        buf[off + i * 2..off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
    }
}

/// Build a minimal but valid PAC: two FDL stages, one payload partition, the
/// two format markers, and an NV marker with data. Both CRCs are correct.
fn build_pac() -> Vec<u8> {
    // (file_id, address, payload size)
    let entries: [(&str, u32, usize); 6] = [
        ("HOST_FDL", 0x0083_8000, 16),
        ("FDL2", 0x0081_0000, 16),
        ("AP", 0x6001_0000, 32),
        ("FMT_FSSYS", 0xFE00_0006, 0),
        ("FLASH", 0xFE00_0001, 0),
        ("NV", 0xFE00_0003, 64),
    ];
    let file_offset = HEADER_SIZE;
    let payload_start = file_offset + entries.len() * FILE_HEADER_SIZE;

    let mut offsets = [0u32; 6];
    let mut cursor = payload_start;
    for (i, (_, _, size)) in entries.iter().enumerate() {
        if *size > 0 {
            offsets[i] = cursor as u32;
            cursor += size;
        }
    }
    let total = cursor;
    let mut buf = vec![0u8; total];

    // Header.
    put_utf16(&mut buf, 0, "BP_R1.0.0");
    put_u32(&mut buf, 48, total as u32);
    put_utf16(&mut buf, 52, "UIX8910_MODEM");
    put_utf16(&mut buf, 564, "8910 MODULE");
    put_u32(&mut buf, 1076, entries.len() as u32);
    put_u32(&mut buf, 1080, file_offset as u32);
    put_u32(&mut buf, 2116, MAGIC);

    // File table.
    for (i, (id, addr, size)) in entries.iter().enumerate() {
        let base = file_offset + i * FILE_HEADER_SIZE;
        put_utf16(&mut buf, base + 4, id);
        put_utf16(&mut buf, base + 516, &format!("{id}.img"));
        put_u32(&mut buf, base + 1540, *size as u32);
        put_u32(&mut buf, base + 1552, offsets[i]);
        put_u32(&mut buf, base + 1564, *addr);
    }

    // Payloads (deterministic bytes).
    for (i, (_, _, size)) in entries.iter().enumerate() {
        let o = offsets[i] as usize;
        for j in 0..*size {
            buf[o + j] = (i as u8).wrapping_mul(31).wrapping_add(j as u8);
        }
    }

    // CRC-16-ARC: payload crc over everything past the header, then header crc.
    let crc2 = crc16_arc(&buf[HEADER_SIZE..]);
    put_u16(&mut buf, 2122, crc2);
    let crc1 = crc16_arc(&buf[..2120]);
    put_u16(&mut buf, 2120, crc1);
    buf
}

#[test]
fn flashes_a_synthetic_pac_end_to_end() {
    let pac = build_pac();
    let info = pac::parse(&pac, true).expect("valid PAC");
    assert!(info.crc_ok(), "synthetic PAC must pass its own CRCs");

    let mut mock = MockTransport::default();
    let opts = FlashOptions {
        format: true,
        ..Default::default()
    };
    let mut noop = |_: &str, _: u64, _: u64| {};
    let outcome = Flasher::new(opts)
        .run(&mut mock, &info, &pac, &mut noop)
        .expect("mock flash succeeds");

    assert!(outcome.version.contains("Spreadtrum Boot Block"));
    assert_eq!(outcome.bytes_written, 32 + 64, "AP + NV payloads");
    let phases: Vec<&str> = outcome.phases.iter().map(|(p, _)| p.as_str()).collect();
    for p in ["fdl1", "fdl2", "partitions", "format"] {
        assert!(phases.contains(&p), "missing phase {p}: {phases:?}");
    }
}

#[test]
fn adaptive_retry_recovers_from_a_dropped_ack() {
    let pac = build_pac();
    let info = pac::parse(&pac, true).expect("valid PAC");

    // Frame 8 is the AP partition's MIDST; withholding its ack forces one
    // timeout, and the adaptive retry must re-connect and finish the flash.
    let mut mock = MockTransport::dropping_bsl_acks(&[8]);
    let opts = FlashOptions {
        format: true,
        timeout: Duration::from_millis(60), // keep the induced timeout short
        ..Default::default()
    };
    let mut noop = |_: &str, _: u64, _: u64| {};
    let outcome = Flasher::new(opts)
        .run(&mut mock, &info, &pac, &mut noop)
        .expect("adaptive retry recovers the flash");
    assert_eq!(outcome.bytes_written, 32 + 64);
}

#[test]
fn read_back_verify_passes_on_a_good_flash() {
    let pac = build_pac();
    let info = pac::parse(&pac, true).expect("valid PAC");
    let mut mock = MockTransport::default(); // serves back exactly what was written
    let opts = FlashOptions {
        format: true,
        verify_readback: true,
        ..Default::default()
    };
    let mut noop = |_: &str, _: u64, _: u64| {};
    let outcome = Flasher::new(opts)
        .run(&mut mock, &info, &pac, &mut noop)
        .expect("read-back matches the written bytes");
    assert_eq!(outcome.bytes_written, 32 + 64);
}

#[test]
fn repacking_a_partition_keeps_the_pac_valid() {
    // Models `clone`: overwrite a partition's payload in a copy of the PAC and
    // refresh the CRC-16-ARC fields, then confirm it re-parses and passes CRCs.
    let mut pac = build_pac();
    let info = pac::parse(&pac, true).expect("valid PAC");
    let ap = info
        .entries
        .iter()
        .find(|e| e.file_id == "AP")
        .expect("AP entry");
    let (off, size) = (ap.offset as usize, ap.size as usize);
    for b in &mut pac[off..off + size] {
        *b = 0xAB;
    }
    // Refresh CRCs exactly as `clone` does: payload over [HEADER_SIZE..] @2122,
    // header over [..2120] @2120, both little-endian.
    let crc2 = crc16_arc(&pac[HEADER_SIZE..]);
    pac[2122..2124].copy_from_slice(&crc2.to_le_bytes());
    let crc1 = crc16_arc(&pac[..2120]);
    pac[2120..2122].copy_from_slice(&crc1.to_le_bytes());

    let info2 = pac::parse(&pac, true).expect("repacked PAC parses");
    assert!(info2.crc_ok(), "repacked PAC must pass its own CRCs");
    let ap2 = info2.entries.iter().find(|e| e.file_id == "AP").unwrap();
    assert!(
        pac[ap2.offset as usize..][..ap2.size as usize]
            .iter()
            .all(|&b| b == 0xAB),
        "repacked AP payload carries the new bytes"
    );
}

#[test]
fn dump_reads_partitions_off_the_device() {
    let pac = build_pac();
    let info = pac::parse(&pac, true).expect("valid PAC");
    // Seed the device's "flash" at the AP partition address, then dump it back
    // without any prior write this session (models extracting an existing image).
    let ap: Vec<u8> = (0..32u8).collect();
    let mut mock = MockTransport::seeded(&[(0x6001_0000, ap.clone())]);
    let mut noop = |_: &str, _: u64, _: u64| {};
    let dumps = Flasher::new(FlashOptions::default())
        .dump(&mut mock, &info, &pac, &[(0x6001_0000, 32)], &mut noop)
        .expect("dump succeeds");
    assert_eq!(dumps.len(), 1);
    assert_eq!(dumps[0], ap, "dump returns the persistent flash bytes");
}

#[test]
fn dump_full_discovers_size_and_reads_the_whole_flash() {
    let pac = build_pac();
    let info = pac::parse(&pac, true).expect("valid PAC");
    // A 128 KiB "flash" at the NOR base; out-of-range reads reply INVALID_CMD,
    // so size discovery must land on exactly 0x20000.
    let image: Vec<u8> = (0..0x20000u32).map(|i| (i & 0xff) as u8).collect();
    let mut mock = MockTransport::seeded(&[(0x6000_0000, image.clone())]);
    let mut noop = |_: &str, _: u64, _: u64| {};
    let (got, size) = Flasher::new(FlashOptions::default())
        .dump_full(&mut mock, &info, &pac, 0x6000_0000, &mut noop)
        .expect("dump_full succeeds");
    assert_eq!(size, 0x20000, "auto-discovered flash size");
    assert_eq!(got, image, "read the whole flash");
}

#[test]
fn read_back_verify_catches_corruption() {
    use sprdflash_flash::FlashError;
    let pac = build_pac();
    let info = pac::parse(&pac, true).expect("valid PAC");
    let mut mock = MockTransport::corrupting_readback(); // flips a byte on read-back
    let opts = FlashOptions {
        format: true,
        verify_readback: true,
        ..Default::default()
    };
    let mut noop = |_: &str, _: u64, _: u64| {};
    let err = Flasher::new(opts)
        .run(&mut mock, &info, &pac, &mut noop)
        .expect_err("corrupted read-back must fail the flash");
    assert!(matches!(err, FlashError::VerifyMismatch(_)), "got {err:?}");
}
