//! IQ sample sources.
//!
//! Every source presents the same pull interface: the DSP thread calls
//! [`Source::read`] to fill a buffer of normalised complex samples. File
//! sources read on demand. Hardware sources (Phase 2) copy driver callbacks
//! into a lock-free ring and serve `read` from the consumer side of that ring,
//! so the driver callback never blocks (PLAN.md 3.4).

pub mod file;
pub mod sigmf;

#[cfg(feature = "airspy")]
pub mod airspy;
#[cfg(feature = "sdrplay")]
pub mod sdrplay;

pub use file::FileSource;

use num_complex::Complex32;
use std::time::SystemTime;

/// What a source delivers.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInfo {
    /// Complex samples per second.
    pub sample_rate: f64,
    /// RF frequency at baseband 0 Hz.
    pub center_freq: f64,
    /// Wall-clock time of sample 0, if known.
    pub start_time: Option<SystemTime>,
    /// Total samples, for finite sources.
    pub total_samples: Option<u64>,
    /// Human-readable description (device name, file path).
    pub description: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SigMF metadata: {0}")]
    Meta(String),
    #[error("unsupported sample format {0:?}")]
    UnsupportedFormat(String),
}

/// Counters every source keeps so drops are visible, never silent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceStats {
    /// Samples delivered to the caller.
    pub samples_read: u64,
    /// Samples lost to overruns (always 0 for files).
    pub samples_dropped: u64,
}

pub trait Source: Send {
    fn info(&self) -> &SourceInfo;

    /// Fill `out` with up to `out.len()` samples. Returns the number written;
    /// `Ok(0)` means the stream has ended. The index of `out[0]` in the stream
    /// is `stats().samples_read + stats().samples_dropped` before the call.
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize, SourceError>;

    fn stats(&self) -> SourceStats;
}
