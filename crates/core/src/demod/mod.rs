//! Per-mode demodulators (PLAN.md 3.8).
//!
//! A scanner channel owns what every mode shares: the extractor that turns
//! FFT blocks into channel IQ, the transmission id and timing, pre-roll
//! catch-up, the minimum-length check, hang-time trimming and live audio.
//! What depends on the mode sits behind [`Demod`]: NBFM voice turns the IQ
//! into audio, AFSK1200 into AX.25 frames. Further modes (DMR, SSB, CW)
//! are further implementations, built by [`build`].

mod afsk;
mod audio;
mod nbfm;

pub use afsk::AfskPacket;
pub use audio::{Levels, SquelchGate};
pub use nbfm::NbfmVoice;

use crate::afsk::Decoded;
use crate::config::{Mode, Slot};
use crate::scanner::ScannerConfig;
use crate::tone::Tone;
use num_complex::Complex32;

/// What a demodulator found out about a transmission, for its close record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tags {
    /// CTCSS tone or DCS code heard, if any.
    pub tone: Option<Tone>,
}

/// One mode's signal chain for one transmission, fed channel IQ at the
/// extractor's output rate.
pub trait Demod: Send {
    /// Demodulate one block. `open` is the power squelch for it (the slot
    /// over its close threshold). Audio is appended to `audio`, at the
    /// scanner's audio rate, sample-aligned with the channel: muted spans
    /// are kept as silence so timing never slips.
    fn process(&mut self, iq: &[Complex32], open: bool, audio: &mut Vec<f32>);

    /// End of transmission: release anything held back, muted.
    fn flush(&mut self, audio: &mut Vec<f32>);

    /// Whether this mode records audio. A mode that doesn't (packet) gets no
    /// recording or live audio, only what [`Demod::take_frames`] returns.
    fn records_audio(&self) -> bool {
        true
    }

    /// Seconds of audio released unmuted so far.
    fn played_secs(&self) -> f64 {
        0.0
    }

    /// Tags for the close record.
    fn tags(&self) -> Tags {
        Tags::default()
    }

    /// Whether the transmission so far looks like a steady carrier (a spur)
    /// rather than traffic. Called once per block.
    fn is_spur(&mut self) -> bool {
        false
    }

    /// Frames decoded since the last call, timed in channel IQ samples
    /// from the first block.
    fn take_frames(&mut self) -> Vec<Decoded> {
        Vec::new()
    }
}

/// Whether [`build`] has a demodulator for this mode.
pub fn supports(mode: Mode) -> bool {
    matches!(mode, Mode::Nbfm | Mode::Afsk1200)
}

/// The demodulator for `slot`'s mode, fed channel IQ at `rate`, or `None`
/// if the mode isn't written yet.
pub fn build(slot: &Slot, rate: f64, cfg: &ScannerConfig) -> Option<Box<dyn Demod>> {
    match slot.mode {
        Mode::Nbfm => Some(Box::new(NbfmVoice::new(slot, rate, cfg))),
        Mode::Afsk1200 => Some(Box::new(AfskPacket::new(slot, rate))),
        _ => None,
    }
}
