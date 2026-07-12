//! Manufacturing-line orchestrator for `sprdflash-rs`.
//!
//! One thread per fixture ([`station`]), each taking a unit through
//! flash → boot-verify ([`verify`]) → record ([`record`]), aggregated by
//! [`line`] into throughput/yield metrics. Independent stations mean one bad
//! unit never stalls the line; per-unit JSON-lines records feed the MES.

pub mod line;
pub mod record;
pub mod station;
pub mod verify;

pub use line::{run, LineConfig, LineSummary};
pub use record::{Outcome, UnitRecord};
pub use station::StationConfig;
pub use verify::{verify_boot, BootInfo};
