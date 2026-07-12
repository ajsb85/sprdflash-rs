//! Error types for the protocol core.

/// Result alias for the core crate.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors produced while parsing or framing the download protocol.
///
/// These are all *pure* errors (bad data, malformed frames); transport/timeout
/// errors live in the transport crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The `.pac` file is smaller than its fixed header.
    #[error("pac too small: {0} bytes")]
    PacTooSmall(u64),

    /// The header's size field disagrees with the actual file length.
    #[error("pac size field {declared} != actual file size {actual} (truncated download?)")]
    PacSizeMismatch {
        /// Size recorded in the PAC header.
        declared: u64,
        /// Actual byte length of the file.
        actual: u64,
    },

    /// The file table is truncated.
    #[error("pac file table truncated at entry {0}")]
    PacTruncatedTable(u32),

    /// A parsed entry points past the end of the buffer.
    #[error("pac entry {file_id:?} data out of range (offset {offset}, size {size})")]
    PacEntryOutOfRange {
        /// The entry's file id.
        file_id: String,
        /// Declared payload offset.
        offset: u64,
        /// Declared payload size.
        size: u64,
    },

    /// A received BSL frame was shorter than the minimum header + checksum.
    #[error("bsl frame too short: {0} bytes")]
    FrameTooShort(usize),

    /// A received BSL frame's declared length did not match its payload.
    #[error("bsl frame truncated: declared {declared}, got {got}")]
    FrameTruncated {
        /// Length declared in the frame header.
        declared: usize,
        /// Bytes actually present.
        got: usize,
    },
}
