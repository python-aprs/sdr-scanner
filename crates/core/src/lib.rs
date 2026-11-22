//! Core building blocks for sdr-scanner: configuration (band profiles),
//! sample-counter timing, and DSP utilities.
//!
//! See `docs/PLAN.md` for the overall design.

pub mod clock;
pub mod config;
pub mod psd;
pub mod units;

pub use num_complex::Complex32;
