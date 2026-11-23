//! Core building blocks for sdr-scanner: configuration (band profiles),
//! sample-counter timing, and DSP utilities.
//!
//! See `docs/PLAN.md` for the overall design.

pub mod channelizer;
pub mod clock;
pub mod config;
pub mod detector;
pub mod dsp;
pub mod psd;
pub mod scanner;
pub mod tone;
pub mod units;

pub use num_complex::Complex32;
