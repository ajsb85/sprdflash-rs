// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! Pure, sans-I/O protocol core for flashing SPRD/UNISOC `.pac` firmware.
//!
//! This crate contains **no I/O**: every function either transforms bytes or
//! parses them. That makes the whole download protocol deterministic and unit
//! testable without hardware, and lets the transport/orchestration layers stay
//! thin. It is a faithful port of the hardware-verified Python `sprdflash`
//! implementation (RDA8910 / Air724UG), including the cross-SDK format path.
//!
//! Layers:
//! - [`checksum`] — the four checksums the protocol uses.
//! - [`pac`] — parse + validate a `.pac` firmware package.
//! - [`pdl`] — the first-stage PDL link layer (loads FDL1).
//! - [`bsl`] — the BSL/HDLC link layer (FDL2 + partitions).
//! - [`plan`] — turn a parsed PAC into an ordered flash plan.

pub mod bsl;
pub mod checksum;
pub mod error;
pub mod pac;
pub mod pdl;
pub mod plan;

pub use error::{Error, Result};

/// Logical PAC "address" values at or above this are pseudo-entries (erase /
/// format / phase-check markers), not real load addresses.
pub const LOGICAL_ADDRESS_BASE: u32 = 0xFE00_0000;
