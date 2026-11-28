//! Per-mode demodulators (PLAN.md 3.8).
//!
//! A scanner channel owns what every mode shares: the extractor that turns
//! FFT blocks into channel IQ, the transmission id and timing, pre-roll
//! catch-up, the minimum-length check, hang-time trimming and live audio.
//! What depends on the mode sits behind [`Demod`]: NBFM voice turns the IQ
//! into audio, AFSK1200 into AX.25 frames, DMR into calls. Further modes
//! (SSB, CW) are further implementations, built by [`build`].
//!
//! NBFM slots run [`AutoDetect`] unless `auto_detect` is off: FM voice plus
//! every digital and packet decoder, so a digital repeater or a packet
//! burst on a channel nobody listed is decoded rather than recorded as
//! noise.

mod afsk;
mod audio;
mod auto;
mod dmr;
mod dstar;
mod nbfm;
mod ysf;

pub use afsk::AfskPacket;
pub use audio::{Levels, SquelchGate};
pub use auto::AutoDetect;
pub use dmr::DmrVoice;
pub use dstar::DstarVoice;
pub use nbfm::NbfmVoice;
pub use ysf::YsfVoice;

use crate::afsk::Decoded;
use crate::config::{Mode, Slot};
use crate::dmr::AmbeFrame;
use crate::scanner::ScannerConfig;
use crate::tone::Tone;
use num_complex::Complex32;

/// What a demodulator found out about a transmission, for its close record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tags {
    /// CTCSS tone or DCS code heard, if any.
    pub tone: Option<Tone>,
}

/// What a digital voice call said about itself, by mode.
#[derive(Debug, Clone, PartialEq)]
pub enum CallInfo {
    Dmr(crate::dmr::CallInfo),
    Ysf(crate::ysf::CallInfo),
    Dstar(crate::dstar::CallInfo),
}

impl CallInfo {
    pub fn mode(&self) -> Mode {
        match self {
            CallInfo::Dmr(_) => Mode::Dmr,
            CallInfo::Ysf(_) => Mode::Ysf,
            CallInfo::Dstar(_) => Mode::Dstar,
        }
    }

    /// The codec of the call's voice frames.
    pub fn codec(&self) -> crate::dmr::vocoder::Codec {
        match self {
            CallInfo::Dstar(_) => crate::dmr::vocoder::Codec::Ambe,
            _ => crate::dmr::vocoder::Codec::AmbePlus2,
        }
    }
}

/// A digital voice call on a channel (a channel can carry more than one at
/// a time: `key` tells them apart). Sample positions are channel IQ
/// samples from the first block, like [`Decoded::at_sample`].
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Start {
        key: u8,
        at_sample: u64,
        info: CallInfo,
    },
    /// Voice frames (20 ms each) and their audio (silence without a
    /// vocoder) at [`crate::dmr::vocoder::AUDIO_RATE`].
    Voice {
        key: u8,
        frames: Vec<AmbeFrame>,
        audio: Vec<f32>,
    },
    End {
        key: u8,
        at_sample: u64,
        info: CallInfo,
    },
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

    /// Digital voice call events since the last call.
    fn take_calls(&mut self) -> Vec<Call> {
        Vec::new()
    }

    /// Packet preambles heard so far (counts up at least once per packet):
    /// data on the air, decoded or not.
    fn preambles(&self) -> u64 {
        0
    }

    /// The digital or packet mode this transmission has shown itself to be
    /// (syncs, a checked header or a frame that passed its FCS), if any.
    fn detected(&self) -> Option<Mode> {
        None
    }
}

/// Whether [`build`] has a demodulator for this mode.
pub fn supports(mode: Mode) -> bool {
    matches!(
        mode,
        Mode::Nbfm | Mode::Afsk1200 | Mode::Dmr | Mode::Ysf | Mode::Dstar
    )
}

/// The demodulator for `slot`'s mode, fed channel IQ at `rate`, or `None`
/// if the mode isn't written yet.
pub fn build(slot: &Slot, rate: f64, cfg: &ScannerConfig) -> Option<Box<dyn Demod>> {
    match slot.mode {
        Mode::Nbfm if cfg.auto_detect => Some(Box::new(AutoDetect::new(slot, rate, cfg))),
        Mode::Nbfm => Some(Box::new(NbfmVoice::new(slot, rate, cfg))),
        Mode::Afsk1200 => Some(Box::new(AfskPacket::new(slot, rate))),
        Mode::Dmr => Some(Box::new(DmrVoice::new(rate))),
        Mode::Ysf => Some(Box::new(YsfVoice::new(rate))),
        Mode::Dstar => Some(Box::new(DstarVoice::new(rate))),
        _ => None,
    }
}
