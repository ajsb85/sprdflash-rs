// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Alexander Salas Bastidas <ajsb85@firechip.dev>

//! Manufacturing-line orchestrator for `sprdflash-rs`.
//!
//! One thread per fixture ([`station`]), each taking a unit through
//! flash → boot-verify ([`verify`]) → record ([`record`]), aggregated by
//! [`line`](mod@line) into throughput/yield metrics. Independent stations mean
//! one bad unit never stalls the line; per-unit JSON-lines records feed the MES.

pub mod line;
pub mod metrics;
pub mod record;
pub mod station;
pub mod verify;

pub use line::{LineConfig, LineSummary, run};
pub use metrics::Metrics;
pub use record::{Outcome, UnitRecord};
pub use station::StationConfig;
pub use verify::{BootInfo, verify_boot};
