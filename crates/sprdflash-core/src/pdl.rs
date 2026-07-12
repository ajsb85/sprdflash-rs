//! PDL (Packet Download Loader) — the first-stage link layer.
//!
//! The RDA8910/UIS8910 download agent (USB `0525:a4a7`, reached via
//! `AT*DOWNLOAD`) speaks this proprietary protocol *before* any BSL, to load and
//! execute the first-stage loader (`HOST_FDL`/`PDL1`). Each message is an 8-byte
//! header **written separately** from its payload — the `sprd_rdavcom` driver
//! reads the header first to learn the payload length, so a combined write is
//! silently dropped.
//!
//! ```text
//! header : ae | len(u16 LE) | 00 00 | ff | 00 00            (8 bytes)
//! payload: cmd(u32 LE) | arg1(u32 LE) | arg2(u32 LE) | extra...
//! reply  : ae | rlen(u16 LE) | 00 00 | .. | 00 00 | status(u32 LE)
//! ```

/// PDL header magic byte.
pub const MAGIC: u8 = 0xAE;
/// Header length, in bytes.
pub const HEADER_LEN: usize = 8;
/// MIDST payload chunk size the vendor uses.
pub const CHUNK: usize = 2048;

/// PDL command opcodes (the `cmd` field of a payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Cmd {
    /// Handshake.
    Connect = 0,
    /// Begin an image transfer: `arg1=load_addr`, `arg2=size`, extra = name.
    StartData = 4,
    /// Image chunk: `arg1=block_index`, `arg2=len`, extra = chunk bytes.
    MidstData = 5,
    /// Finish transfer: extra = 4-byte image checksum.
    EndData = 6,
    /// Execute the loaded image (hands over to BSL).
    Exec = 7,
}

/// Build the 8-byte header describing a `payload_len`-byte payload.
#[must_use]
pub fn header(payload_len: u16) -> [u8; HEADER_LEN] {
    let l = payload_len.to_le_bytes();
    [MAGIC, l[0], l[1], 0x00, 0x00, 0xFF, 0x00, 0x00]
}

/// Build a command payload: `cmd | arg1 | arg2` (all u32 LE) followed by `extra`.
#[must_use]
pub fn params(cmd: Cmd, arg1: u32, arg2: u32, extra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + extra.len());
    out.extend_from_slice(&(cmd as u32).to_le_bytes());
    out.extend_from_slice(&arg1.to_le_bytes());
    out.extend_from_slice(&arg2.to_le_bytes());
    out.extend_from_slice(extra);
    out
}

/// A decoded PDL response: `status == 0` means OK for this agent.
#[derive(Debug, Clone)]
pub struct Response {
    /// Status word (first 4 bytes of the response payload).
    pub status: u32,
    /// Full response payload (including the status word).
    pub payload: Vec<u8>,
}

/// Try to decode a PDL response from `buf`.
///
/// Returns `Some(response)` once a full `ae`-framed reply is present, `None`
/// if more bytes are needed. Resyncs to the magic byte if `buf` has leading
/// garbage.
#[must_use]
pub fn try_parse_response(buf: &[u8]) -> Option<Response> {
    let start = buf.iter().position(|&b| b == MAGIC)?;
    let buf = &buf[start..];
    if buf.len() < HEADER_LEN {
        return None;
    }
    let rlen = usize::from(u16::from_le_bytes([buf[1], buf[2]]));
    let end = HEADER_LEN + rlen;
    if buf.len() < end {
        return None;
    }
    let payload = buf[HEADER_LEN..end].to_vec();
    let status = if payload.len() >= 4 {
        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]])
    } else {
        0
    };
    Some(Response { status, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_capture() {
        // 12-byte payload (CONNECT) -> header ae0c000000ff0000
        assert_eq!(header(12), [0xAE, 0x0C, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00]);
    }

    #[test]
    fn connect_payload_matches_capture() {
        assert_eq!(params(Cmd::Connect, 0, 0, &[]), vec![0u8; 12]);
    }

    #[test]
    fn start_payload_layout() {
        // START: cmd=4, addr=0x00838000, size=0x33a0, name "PDL1\0"
        let p = params(Cmd::StartData, 0x0083_8000, 0x33a0, b"PDL1\x00");
        assert_eq!(&p[0..4], &4u32.to_le_bytes());
        assert_eq!(&p[4..8], &0x0083_8000u32.to_le_bytes());
        assert_eq!(&p[8..12], &0x33a0u32.to_le_bytes());
        assert_eq!(&p[12..], b"PDL1\x00");
    }

    #[test]
    fn parse_response_resyncs_and_reads_status() {
        // leading garbage, then a well-formed ae-header (len=4) + status=0x12345678
        let mut frame = vec![0x99u8];
        frame.extend_from_slice(&header(4));
        frame.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        let r = try_parse_response(&frame).unwrap();
        assert_eq!(r.status, 0x1234_5678);
        assert_eq!(r.payload.len(), 4);
    }

    #[test]
    fn parse_response_needs_more() {
        assert!(try_parse_response(&[0xAE, 0x08, 0x00]).is_none());
    }
}
