// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! Reconstruct a **proposed** partition layout from a full NOR flash dump.
//!
//! The RDA8910/UIS8910 FDL2 can't report its partition table (see
//! `sprdflash-flash`), and the device stores no partition-name strings. But the
//! flash *content* is self-describing enough to recover the physical geometry:
//!
//! - **Code images** carry a U-Boot legacy `uImage` header (magic `0x27051956`,
//!   64 bytes, big-endian) with an exact payload size and two CRC-32s — so
//!   BOOTLOADER/AP are recovered byte-exact and integrity-checked.
//! - **LuatOS `luadb`** script packs carry magic `0x5AA55AA5` (at offset +2).
//! - The **filesystem / NV** back half is a wear-levelled NOR FS whose 64 KiB
//!   blocks start with a small little-endian *generation* counter; the longest
//!   single-generation run is the live modem FS (PS), and the top block-family
//!   change marks the factory/NV region.
//!
//! What is **derived** (high confidence): every region base + reserved size, the
//! code payload sizes, and each region's **content type**. What the dump can't
//! tell you is a region's *role name*: no name strings are stored on flash, and
//! roles are firmware-specific (e.g. a LuatOS PAC calls its luadb region `LUA`,
//! but a Logicrom/CSDK module has no such partition). So this module does **not**
//! guess PAC IDs — it assigns generic, content-type default names
//! (`uimage_0`, `luadb_0`, `filesystem_0`, `nv_0`) and reports what each region
//! *is*. Reconstructed byte-exact (bases + reserved sizes) against a real
//! Air724UG V4018 dump.

use crate::checksum::crc32;

/// NOR flash is memory-mapped here on the RDA8910/UIS8910.
pub const NOR_BASE: u32 = 0x6000_0000;
/// Erase-block / partition alignment.
const BLOCK: usize = 0x1_0000;
/// U-Boot legacy image header magic (big-endian on disk).
const UIMAGE_MAGIC: u32 = 0x2705_1956;
const UIMAGE_HDR: usize = 0x40;
/// LuatOS `luadb` script-pack magic, found at offset +2.
const LUADB_MAGIC: [u8; 4] = [0x5A, 0xA5, 0x5A, 0xA5];

/// How trustworthy a reconstructed field is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Cross-checked (e.g. a CRC-verified header).
    Verified,
    /// Derived from an unambiguous on-flash marker.
    Derived,
    /// Best-effort inference (filesystem boundaries; all names).
    Heuristic,
}

impl Confidence {
    /// Short tag for display.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Confidence::Verified => "verified",
            Confidence::Derived => "derived",
            Confidence::Heuristic => "heuristic",
        }
    }
}

/// One reconstructed partition/region.
#[derive(Debug, Clone)]
pub struct Region {
    /// Conventional ID (guessed by position + type).
    pub id: String,
    /// Physical flash address.
    pub phys_addr: u32,
    /// Reserved window = next base − this base (last = to end of flash).
    pub reserved_size: u32,
    /// Bytes of actual content before the trailing erase padding.
    pub used_size: u32,
    /// `code` | `luadb` | `filesystem` | `nv`.
    pub kind: &'static str,
    /// Confidence in this region's boundaries.
    pub confidence: Confidence,
    /// Human detail (build tag, CRC status, …).
    pub detail: String,
}

fn be32(d: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

fn round_up(v: usize, to: usize) -> usize {
    v.div_ceil(to) * to
}

/// Bytes of real content in `[start, limit)` — everything up to the last byte
/// that isn't erase padding (`0xFF`).
fn used_len(image: &[u8], start: usize, limit: usize) -> usize {
    let end = limit.min(image.len());
    let mut i = end;
    while i > start && image[i - 1] == 0xFF {
        i -= 1;
    }
    i - start
}

struct Anchor {
    off: usize,
    kind: &'static str,
    used: usize,
    conf: Confidence,
    detail: String,
}

/// Detect a U-Boot `uImage` at `off` and return its anchor.
fn detect_uimage(image: &[u8], off: usize) -> Option<Anchor> {
    if off + UIMAGE_HDR > image.len() || be32(image, off) != UIMAGE_MAGIC {
        return None;
    }
    let payload = be32(image, off + 0x0C) as usize;
    let used = UIMAGE_HDR + payload;
    if off + used > image.len() {
        return None; // truncated / false magic
    }
    // Header CRC-32 is computed with the hcrc field (0x04..0x08) zeroed.
    let mut hdr = image[off..off + UIMAGE_HDR].to_vec();
    hdr[4..8].fill(0);
    let hcrc_ok = crc32(&hdr) == be32(image, off + 0x04);
    let dcrc_ok = crc32(&image[off + UIMAGE_HDR..off + used]) == be32(image, off + 0x18);
    let name = {
        let raw = &image[off + 0x20..off + UIMAGE_HDR];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        String::from_utf8_lossy(&raw[..end]).into_owned()
    };
    Some(Anchor {
        off,
        kind: "uimage",
        used,
        conf: if hcrc_ok && dcrc_ok {
            Confidence::Verified
        } else {
            Confidence::Derived
        },
        detail: format!(
            "uImage{}{} hcrc={} dcrc={}",
            if name.is_empty() { "" } else { " '" },
            if name.is_empty() {
                String::new()
            } else {
                format!("{name}'")
            },
            if hcrc_ok { "ok" } else { "BAD" },
            if dcrc_ok { "ok" } else { "BAD" },
        ),
    })
}

/// Detect a LuatOS `luadb` pack at `off`.
fn detect_luadb(image: &[u8], off: usize) -> Option<Anchor> {
    if image.get(off + 2..off + 6) == Some(&LUADB_MAGIC[..]) {
        Some(Anchor {
            off,
            kind: "luadb",
            used: 0, // filled by the caller (0xFF scan to the next anchor)
            conf: Confidence::Derived,
            detail: "LuatOS luadb script pack (the app/script region; a PAC may \
                     call it LUA/APP)"
                .into(),
        })
    } else {
        None
    }
}

/// True if a 64 KiB block starts with a small NOR-FS generation counter
/// (little-endian u32, high 16 bits zero, non-empty, not erased).
fn fs_generation(image: &[u8], off: usize) -> Option<u16> {
    if off + 4 > image.len() {
        return None;
    }
    if image[off + 2] == 0 && image[off + 3] == 0 {
        let g = u16::from_le_bytes([image[off], image[off + 1]]);
        if g != 0 {
            return Some(g);
        }
    }
    None
}

/// Reconstruct a proposed layout from a full flash `image` mapped at `phys_base`.
///
/// Phase 1 walks the block-aligned front, anchoring code (`uImage`, exact length)
/// and `luadb` regions, and stops at the first filesystem block. Phase 2 splits
/// the wear-levelled filesystem back half by its longest single-generation run
/// (the live modem FS), with any mixed-generation staging before it and the
/// factory/NV region at the top.
#[must_use]
pub fn reconstruct(image: &[u8], phys_base: u32) -> Vec<Region> {
    let n = image.len();
    let mut segs: Vec<Anchor> = Vec::new();

    // Phase 1: firmware (code + luadb) at the front.
    let mut off = 0usize;
    while off + BLOCK <= n {
        if let Some(a) = detect_uimage(image, off) {
            let end = round_up(off + a.used, BLOCK);
            segs.push(a);
            off = end;
        } else if let Some(a) = detect_luadb(image, off) {
            segs.push(a);
            off += BLOCK;
        } else if fs_generation(image, off).is_some() {
            break; // the filesystem back half starts here
        } else {
            off += BLOCK; // erase padding or a firmware body block
        }
    }

    // Phase 2: the filesystem / NV back half.
    for a in segment_backhalf(image, off.min(n), n) {
        segs.push(a);
    }

    // Assign reserved sizes (to the next base / end of flash), used sizes, and
    // generic content-type default names (`<type>_<index>`). We deliberately do
    // NOT guess PAC role IDs — those are firmware-specific, not on the flash.
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut out = Vec::with_capacity(segs.len());
    for i in 0..segs.len() {
        let a = &segs[i];
        let next = segs.get(i + 1).map_or(n, |b| b.off);
        let reserved = (next - a.off) as u32;
        let used = if a.kind == "uimage" {
            a.used as u32
        } else {
            used_len(image, a.off, next) as u32
        };
        // The final filesystem segment (reaching the top of flash) is
        // conventionally the factory/NV region.
        let kind = if a.kind == "filesystem" && i + 1 == segs.len() {
            "nv"
        } else {
            a.kind
        };
        let idx = counts.entry(kind).or_insert(0);
        let id = format!("{kind}_{idx}");
        *idx += 1;
        out.push(Region {
            id,
            phys_addr: phys_base + a.off as u32,
            reserved_size: reserved,
            used_size: used,
            kind,
            confidence: a.conf,
            detail: a.detail.clone(),
        });
    }
    out
}

/// Split the wear-levelled filesystem back half `[start, n)`: the longest run of
/// 64 KiB blocks sharing one generation counter is the live filesystem; anything
/// before it is mixed-generation staging, and the region above it is factory/NV.
/// All boundaries are heuristic.
fn segment_backhalf(image: &[u8], start: usize, n: usize) -> Vec<Anchor> {
    if start >= n {
        return Vec::new();
    }
    let blocks: Vec<(usize, Option<u16>)> = (start..n)
        .step_by(BLOCK)
        .filter(|&o| o + BLOCK <= n)
        .map(|o| (o, fs_generation(image, o)))
        .collect();

    // Longest run of consecutive blocks with the same generation value.
    let (mut best_i, mut best_len) = (0usize, 0usize);
    let mut i = 0;
    while i < blocks.len() {
        if let Some(g) = blocks[i].1 {
            let mut j = i + 1;
            while j < blocks.len() && blocks[j].1 == Some(g) {
                j += 1;
            }
            if j - i > best_len {
                (best_i, best_len) = (i, j - i);
            }
            i = j;
        } else {
            i += 1;
        }
    }

    if best_len == 0 {
        return vec![Anchor {
            off: start,
            kind: "filesystem",
            used: 0,
            conf: Confidence::Heuristic,
            detail: "unclassified back-half region".into(),
        }];
    }

    let fs_start = blocks[best_i].0;
    let fs_end = fs_start + best_len * BLOCK;
    let generation = blocks[best_i].1.unwrap_or(0);
    let mut out = Vec::new();
    if start < fs_start {
        out.push(Anchor {
            off: start,
            kind: "filesystem",
            used: 0,
            conf: Confidence::Heuristic,
            detail: "NOR-FS staging (mixed generations)".into(),
        });
    }
    out.push(Anchor {
        off: fs_start,
        kind: "filesystem",
        used: 0,
        conf: Confidence::Heuristic,
        detail: format!(
            "NOR-FS, longest single-generation run (gen {generation:#x}, {best_len} blocks)"
        ),
    });
    if fs_end < n {
        out.push(Anchor {
            off: fs_end,
            kind: "filesystem",
            used: 0,
            conf: Confidence::Heuristic,
            detail: "NOR-FS top region (factory/NV)".into(),
        });
    }
    out
}

/// Render reconstructed `regions` as a `<BMAConfig>` XML (same shape as a `.pac`
/// scheme). Names are conventional; a comment records that it was reconstructed.
#[must_use]
pub fn to_bmaconfig_xml(regions: &[Region]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<BMAConfig>\n  <SchemeList>\n    \
         <Scheme name=\"RECONSTRUCTED_FROM_DUMP\">\n      <!-- Physical geometry \
         derived from a full flash dump. Bases/sizes are derived; names are \
         conventional guesses; FDL/erase/control entries are not recoverable. -->\n",
    );
    for r in regions {
        s.push_str(&format!(
            "      <File>\n        <ID>{}</ID>\n        <Type>{}</Type>\n        \
             <Block>\n          <Base>{:#010x}</Base>\n          <Size>{:#x}</Size>\n        \
             </Block>\n        <Description>{} | used {:#x} | {}</Description>\n      </File>\n",
            r.id,
            r.kind,
            r.phys_addr,
            r.reserved_size,
            r.confidence.tag(),
            r.used_size,
            r.detail,
        ));
    }
    s.push_str("    </Scheme>\n  </SchemeList>\n</BMAConfig>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid U-Boot uImage (64-byte header + body) with correct CRCs.
    fn uimage(name: &str, load: u32, body: &[u8]) -> Vec<u8> {
        let mut hdr = vec![0u8; UIMAGE_HDR];
        hdr[0..4].copy_from_slice(&UIMAGE_MAGIC.to_be_bytes());
        hdr[8..12].copy_from_slice(&0u32.to_be_bytes()); // time
        hdr[12..16].copy_from_slice(&(body.len() as u32).to_be_bytes());
        hdr[16..20].copy_from_slice(&load.to_be_bytes());
        hdr[20..24].copy_from_slice(&load.to_be_bytes());
        hdr[24..28].copy_from_slice(&crc32(body).to_be_bytes());
        let nb = name.as_bytes();
        hdr[32..32 + nb.len()].copy_from_slice(nb);
        let hcrc = crc32(&hdr); // hcrc field already zero
        hdr[4..8].copy_from_slice(&hcrc.to_be_bytes());
        hdr.extend_from_slice(body);
        hdr
    }

    fn pad_to(v: &mut Vec<u8>, len: usize) {
        v.resize(len.max(v.len()), 0xFF);
    }

    #[test]
    fn recovers_code_and_luadb_partitions() {
        // BOOTLOADER @0, AP @0x10000, LUA (luadb) @0x30000, in a 0x50000 image.
        let mut img = Vec::new();
        img.extend_from_slice(&uimage("boot", 0x0080_0100, &[0xAA; 0x400]));
        pad_to(&mut img, 0x10000);
        img.extend_from_slice(&uimage("ap", 0x6001_0040, &[0xBB; 0x900]));
        pad_to(&mut img, 0x30000);
        img.extend_from_slice(&[0x01, 0x04]);
        img.extend_from_slice(&LUADB_MAGIC);
        img.extend_from_slice(&[0xCC; 0x200]);
        pad_to(&mut img, 0x50000);

        let regs = reconstruct(&img, NOR_BASE);
        // Generic content-type names — no PAC role guessing.
        let ids: Vec<&str> = regs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(&ids[..3], &["uimage_0", "uimage_1", "luadb_0"]);
        let kinds: Vec<&str> = regs.iter().map(|r| r.kind).collect();
        assert_eq!(&kinds[..3], &["uimage", "uimage", "luadb"]);

        let boot = &regs[0];
        assert_eq!(boot.phys_addr, 0x6000_0000);
        assert_eq!(boot.reserved_size, 0x10000);
        assert_eq!(boot.used_size, (UIMAGE_HDR + 0x400) as u32);
        assert_eq!(boot.confidence, Confidence::Verified); // CRCs check out

        let ap = &regs[1];
        assert_eq!(ap.phys_addr, 0x6001_0000);
        assert_eq!(ap.reserved_size, 0x20000);
        assert_eq!(ap.used_size, (UIMAGE_HDR + 0x900) as u32);

        // The proposed BMAConfig is well-formed and carries the derived bases.
        let xml = to_bmaconfig_xml(&regs);
        assert!(xml.contains("<Base>0x60000000</Base>"));
        assert!(xml.contains("<ID>uimage_1</ID>"));
    }

    #[test]
    fn segments_backhalf_by_longest_generation_run() {
        // uImage @0, then a filesystem back half: staging (gen 5 ×2), a longer
        // live run (gen 9 ×4), and a top region (gen 0x100 ×1) -> NV.
        let mut img = uimage("fw", 0x6000_0040, &[0xAA; 0x400]);
        pad_to(&mut img, BLOCK);
        let genblk = |g: u16| {
            let mut b = vec![0u8; BLOCK];
            b[0..2].copy_from_slice(&g.to_le_bytes()); // gen counter; b[2..4] stay 0
            b[8] = 0x11;
            b
        };
        for g in [5u16, 5] {
            img.extend_from_slice(&genblk(g));
        }
        for _ in 0..4 {
            img.extend_from_slice(&genblk(9));
        }
        img.extend_from_slice(&genblk(0x100));

        let regs = reconstruct(&img, NOR_BASE);
        let ids: Vec<&str> = regs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["uimage_0", "filesystem_0", "filesystem_1", "nv_0"]);
        assert_eq!(regs[1].phys_addr, 0x6001_0000); // staging
        assert_eq!(regs[1].reserved_size, 0x20000);
        assert_eq!(regs[2].phys_addr, 0x6003_0000); // longest run
        assert_eq!(regs[2].reserved_size, 0x40000);
        assert_eq!(regs[3].phys_addr, 0x6007_0000); // top -> nv
        assert_eq!(regs[3].kind, "nv");
    }

    #[test]
    fn corrupt_payload_drops_confidence() {
        let mut img = uimage("x", 0x6000_0040, &[0x11; 0x100]);
        img[UIMAGE_HDR + 10] ^= 0xFF; // corrupt the body -> dcrc fails
        pad_to(&mut img, 0x10000);
        let regs = reconstruct(&img, NOR_BASE);
        assert_eq!(regs[0].confidence, Confidence::Derived); // magic+size ok, CRC not
        assert!(regs[0].detail.contains("dcrc=BAD"));
    }
}
