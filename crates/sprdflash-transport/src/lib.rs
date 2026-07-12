// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! Serial transport, USB port discovery, and device recovery for `sprdflash-rs`.
//!
//! Cross-platform (Windows + Linux/WSL) via the `serialport` crate. The
//! [`Serial`] byte stream is the single I/O primitive the flash driver builds
//! on; [`discovery`] finds the download/module ports by USB VID/PID; and
//! [`recovery`] re-enumerates a stuck download agent.

pub mod discovery;
pub mod recovery;
pub mod serial;

pub use serial::{Serial, TransportError};

/// BootROM / download-mode USB identity (SPRD download gadget).
pub const DOWNLOAD_VID: u16 = 0x0525;
/// BootROM / download-mode USB product id.
pub const DOWNLOAD_PID: u16 = 0xA4A7;
/// Normal-mode composite device vendor id (Spreadtrum/RDA).
pub const MODULE_VID: u16 = 0x1782;
