//! Recording output: streaming WAV files, the SQLite transmission index and
//! the recorder thread that ties them to the scanner (PLAN.md 3.5).
//!
//! Opus copies and disk-budget retention arrive with the web service
//! (Phase 3).

pub mod index;
pub mod recorder;
pub mod wav;

pub use index::Index;
pub use recorder::{Recorder, RecorderStats};
