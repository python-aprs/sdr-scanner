//! IQ sample sources.
//!
//! Every source presents the same pull interface: the DSP thread calls
//! [`Source::read`] to fill a buffer of normalised complex samples. File
//! sources read on demand. Hardware sources copy driver callbacks
//! into a lock-free ring and serve `read` from the consumer side of that ring,
//! so the driver callback never blocks (PLAN.md 3.4).

pub mod file;
pub mod ring;
pub mod sigmf;

pub mod sdrplay;

pub use file::FileSource;
pub use ring::{RingProducer, RingSource, ring};

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
    #[error("radio: {0}")]
    Device(String),
}

/// Counters every source keeps so drops are visible, never silent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceStats {
    /// Samples delivered to the caller.
    pub samples_read: u64,
    /// Samples lost to overruns and replaced by zeros in the delivered
    /// stream (always 0 for files). Included in `samples_read`.
    pub samples_dropped: u64,
}

pub trait Source: Send {
    fn info(&self) -> &SourceInfo;

    /// Fill `out` with up to `out.len()` samples. Returns the number written;
    /// `Ok(0)` means the stream has ended. The index of `out[0]` in the stream
    /// is `stats().samples_read` before the call: spans lost to overruns are
    /// delivered as zeros, so positions (and timestamps) never slip.
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize, SourceError>;

    fn stats(&self) -> SourceStats;
}
