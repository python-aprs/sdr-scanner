//! Core building blocks for sdr-scanner: configuration (band profiles),
//! sample-counter timing, and DSP utilities.
//!
//! See `docs/PLAN.md` for the overall design.

pub mod afsk;
pub mod calibrate;
pub mod channelizer;
pub mod clock;
pub mod config;
pub mod demod;
pub mod detector;
pub mod dmr;
pub mod dsp;
pub mod dstar;
pub mod dtmf;
pub mod psd;
pub mod scanner;
pub mod tone;
pub mod units;
pub mod ysf;

pub use num_complex::Complex32;
