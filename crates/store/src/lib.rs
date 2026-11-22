//! Recording output: streaming WAV files and their Ogg Opus web copies, the
//! SQLite transmission index and the recorder thread that ties them to the
//! scanner (PLAN.md 3.5), and retention by disk budget.

pub mod index;
pub mod migrate;
pub mod ogg;
pub mod recorder;
pub mod retention;
pub mod settings;
pub mod wav;

pub use index::{Index, Query, Row};
pub use recorder::{Recorder, RecorderStats};
