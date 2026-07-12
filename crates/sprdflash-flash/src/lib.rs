//! Device flash driver: ties the sans-I/O protocol core to a serial transport.
//!
//! Flow (verified on real RDA8910 hardware):
//! `PDL connect → load FDL1 → exec → BSL VER → connect → load FDL2 → exec →
//! [CHANGE_BAUD] → write partitions → [format: erase + NV + PREPACK] → reset`.

mod bsl_io;
mod flasher;
mod options;
mod pdl_io;

pub use flasher::{FlashError, FlashOutcome, Flasher, Progress};
pub use options::FlashOptions;
