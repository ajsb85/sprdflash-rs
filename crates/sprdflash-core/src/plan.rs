//! Turn a parsed PAC into an ordered flash plan.
//!
//! This module encodes the cross-SDK format behaviour that was reverse
//! engineered byte-for-byte from the vendor tool and verified on real hardware
//! (see the `sprdflash` Python project). The sequence, after the FDLs run:
//!
//! 1. write every payload partition (physical addresses);
//! 2. **with `--format` only**, replay the logical markers in PAC order:
//!    - `FMT_FSSYS` → `ERASE_FLASH "SYSF"`, `FLASH` → `ERASE_FLASH 0`
//!      (deduped by address; `FMT_FSEXT` shares `FMT_FSSYS`'s address and is not
//!      a second erase);
//!    - `NV` → the PAC's nvitem template with a refreshed CRC-16-ARC, written
//!      with a sum32-checked `START_DATA`;
//!    - `PREPACK` → the prepack blob, plain.

use crate::checksum::{crc16_arc, sum32};
use crate::pac::{PacEntry, PacInfo};
use crate::LOGICAL_ADDRESS_BASE;

/// Role of a PAC entry in the flash flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// First-stage loader (`HOST_FDL`/`FDL1`).
    Fdl1,
    /// Second-stage loader (`FDL2`).
    Fdl2,
    /// Logical marker (erase/format/phase-check).
    Marker,
    /// Real payload partition.
    Flash,
}

/// Classify a PAC entry. Marker wins first, then FDL2 before FDL1 (so `FDL2` is
/// not swallowed by the `FDL` prefix used for FDL1).
#[must_use]
pub fn classify(e: &PacEntry) -> Role {
    if e.size == 0 || e.address == 0 || e.address >= LOGICAL_ADDRESS_BASE {
        return Role::Marker;
    }
    let fid = e.file_id.to_ascii_uppercase();
    if fid == "FDL2" || fid.starts_with("FDL2") {
        return Role::Fdl2;
    }
    if ["HOST_FDL", "FDL", "FDL1"]
        .iter()
        .any(|x| fid == *x || fid.starts_with(x))
    {
        return Role::Fdl1;
    }
    Role::Flash
}

/// A single filesystem-format erase: `ERASE_FLASH(address, param)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EraseOp {
    /// The marker's file id (for logging).
    pub file_id: String,
    /// The marker's logical address (big-endian on the wire).
    pub address: u32,
    /// The 4-byte parameter the vendor sends.
    pub param: [u8; 4],
}

/// Map a zero-size format marker to its verified `ERASE_FLASH` parameter.
fn erase_param(file_id: &str) -> Option<[u8; 4]> {
    match file_id.to_ascii_uppercase().as_str() {
        "FMT_FSSYS" => Some(*b"SYSF"),
        "FLASH" => Some([0, 0, 0, 0]),
        _ => None,
    }
}

/// The erases to issue (zero-size markers with a known param), in PAC order,
/// deduped by address.
#[must_use]
pub fn erase_ops(info: &PacInfo) -> Vec<EraseOp> {
    let mut ops = Vec::new();
    let mut seen = Vec::new();
    for e in &info.entries {
        if e.size != 0 || e.address < LOGICAL_ADDRESS_BASE {
            continue;
        }
        if let Some(param) = erase_param(&e.file_id) {
            if !seen.contains(&e.address) {
                seen.push(e.address);
                ops.push(EraseOp {
                    file_id: e.file_id.clone(),
                    address: e.address,
                    param,
                });
            }
        }
    }
    ops
}

/// Data-bearing logical markers (`address >= LOGICAL_ADDRESS_BASE`, `size > 0`),
/// in PAC order: `NV` (0xFE000003) and `PREPACK` (0xFE000004).
#[must_use]
pub fn data_markers(info: &PacInfo) -> Vec<&PacEntry> {
    info.entries
        .iter()
        .filter(|e| e.size != 0 && e.address >= LOGICAL_ADDRESS_BASE)
        .collect()
}

/// True if `file_id` names the NV region (its write needs the sum32-checked
/// START and a refreshed CRC).
#[must_use]
pub fn is_nv(file_id: &str) -> bool {
    file_id.eq_ignore_ascii_case("NV")
}

/// Refresh an NV image's leading big-endian CRC-16-ARC slot to match its
/// content.
///
/// The PAC's nvitem template ships with a stale placeholder CRC; the device's
/// FDL2 validates this field at `END_DATA`, so it must be recomputed over the
/// whole region (bytes `2..`) or the write fails with `OPERATION_FAILED` and the
/// module will not boot on a soft reset.
#[must_use]
pub fn nv_fix_crc(data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    if out.len() >= 2 {
        let crc = crc16_arc(&out[2..]);
        out[0] = (crc >> 8) as u8; // big-endian
        out[1] = (crc & 0xFF) as u8;
    }
    out
}

/// Build the 12-byte NV `START_DATA` payload: `addr | size | sum32` (all BE).
///
/// `data` must already have its CRC refreshed via [`nv_fix_crc`].
#[must_use]
pub fn nv_start_payload(addr: u32, data: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12);
    p.extend_from_slice(&addr.to_be_bytes());
    p.extend_from_slice(&(data.len() as u32).to_be_bytes());
    p.extend_from_slice(&sum32(data).to_be_bytes());
    p
}

/// An ordered flash plan derived from a PAC. Payload bytes are fetched on demand
/// via [`PacEntry::payload`], so this borrows the parsed info.
#[derive(Debug)]
pub struct FlashPlan<'a> {
    /// The first-stage loader (required).
    pub fdl1: &'a PacEntry,
    /// The second-stage loader (usually present).
    pub fdl2: Option<&'a PacEntry>,
    /// Payload partitions to write, in PAC order.
    pub partitions: Vec<&'a PacEntry>,
    /// Format erases (only used when `format` is requested).
    pub erases: Vec<EraseOp>,
    /// Data-bearing markers `NV`/`PREPACK` (only used when `format` is requested).
    pub data_markers: Vec<&'a PacEntry>,
}

/// Build a [`FlashPlan`] from parsed PAC info.
///
/// Fails if the PAC has no FDL1 stage.
pub fn build(info: &PacInfo) -> Result<FlashPlan<'_>, &'static str> {
    let fdl1 = info
        .entries
        .iter()
        .find(|e| classify(e) == Role::Fdl1)
        .ok_or("no FDL1 (HOST_FDL) stage in the PAC")?;
    let fdl2 = info.entries.iter().find(|e| classify(e) == Role::Fdl2);
    let partitions = info
        .entries
        .iter()
        .filter(|e| classify(e) == Role::Flash)
        .collect();
    Ok(FlashPlan {
        fdl1,
        fdl2,
        partitions,
        erases: erase_ops(info),
        data_markers: data_markers(info),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pac::PacEntry;

    fn entry(id: &str, addr: u32, size: u32) -> PacEntry {
        PacEntry {
            file_id: id.into(),
            file_name: String::new(),
            size,
            offset: 0,
            address: addr,
            flag: 0,
            omit: 0,
        }
    }

    fn v4035_markers() -> PacInfo {
        PacInfo {
            version: String::new(),
            product_name: String::new(),
            product_version: String::new(),
            size: 0,
            mode: 0,
            flash_type: 0,
            magic: 0,
            header_crc_ok: true,
            payload_crc_ok: Some(true),
            entries: vec![
                entry("PhaseCheck", 0xFE00_0002, 0),
                entry("FMT_FSSYS", 0xFE00_0006, 0),
                entry("FMT_FSEXT", 0xFE00_0006, 0),
                entry("FLASH", 0xFE00_0001, 0),
                entry("NV", 0xFE00_0003, 131072),
                entry("PREPACK", 0xFE00_0004, 92),
            ],
        }
    }

    #[test]
    fn erase_ops_match_vendor() {
        let ops = erase_ops(&v4035_markers());
        assert_eq!(
            ops,
            vec![
                EraseOp {
                    file_id: "FMT_FSSYS".into(),
                    address: 0xFE00_0006,
                    param: *b"SYSF"
                },
                EraseOp {
                    file_id: "FLASH".into(),
                    address: 0xFE00_0001,
                    param: [0, 0, 0, 0]
                },
            ]
        );
    }

    #[test]
    fn data_markers_are_nv_then_prepack() {
        let info = v4035_markers();
        let dm: Vec<_> = data_markers(&info)
            .iter()
            .map(|e| e.file_id.clone())
            .collect();
        assert_eq!(dm, vec!["NV".to_string(), "PREPACK".to_string()]);
    }

    #[test]
    fn nv_fix_crc_patches_be_slot_only() {
        let blob = {
            let mut v = vec![0u8, 0u8];
            v.extend_from_slice(b"hello world padding data...");
            v
        };
        let fixed = nv_fix_crc(&blob);
        assert_eq!(&fixed[2..], &blob[2..]);
        let stored = (u16::from(fixed[0]) << 8) | u16::from(fixed[1]);
        assert_eq!(stored, crc16_arc(&blob[2..]));
    }

    #[test]
    fn classify_prefers_fdl2_over_fdl_prefix() {
        assert_eq!(classify(&entry("FDL2", 0x0081_0000, 100)), Role::Fdl2);
        assert_eq!(classify(&entry("HOST_FDL", 0x0083_8000, 100)), Role::Fdl1);
        assert_eq!(classify(&entry("AP", 0x6001_0000, 100)), Role::Flash);
        assert_eq!(classify(&entry("NV", 0xFE00_0003, 131072)), Role::Marker);
    }

    #[test]
    fn nv_start_payload_shape() {
        let data = nv_fix_crc(&vec![0u8; 1024]);
        let p = nv_start_payload(0xFE00_0003, &data);
        assert_eq!(p.len(), 12);
        assert_eq!(&p[0..8], &hex_bytes("fe00000300000400"));
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
