//! One transmission being demodulated: what every mode shares, around the
//! mode's [`Demod`].

use super::Output;
use crate::channelizer::{Block, Extractor};
use crate::config::Slot;
use crate::demod::Demod;
use num_complex::Complex32;

pub(super) struct Channel {
    pub id: u64,
    /// Index of the detector slot that opened it.
    pub slot: usize,
    /// The slot as recorded (frequency retuned for grid slots).
    pub tuned: Slot,
    pub extractor: Extractor,
    pub demod: Box<dyn Demod>,
    /// Channel IQ sample rate (the extractor's output rate).
    pub rate: f64,
    /// Stream sample (input rate) of audio sample 0, may be negative at
    /// stream start.
    pub audio_t0: i64,
    /// Sent its `Output::Open` (passed the minimum length).
    pub confirmed: bool,
    /// Audio not yet emitted.
    pub pending: Vec<f32>,
    /// Audio samples emitted so far.
    pub emitted: usize,
    /// Audio samples sent to live listeners so far (counted like `emitted`),
    /// or `None` until the channel is announced to them.
    pub live_sent: Option<usize>,
    /// Digital calls being recorded, by the demodulator's call key.
    pub calls: [Option<CallRec>; 2],
    iq: Vec<Complex32>,
}

/// A digital call's recording.
pub(super) struct CallRec {
    pub id: u64,
    /// Stream sample of its start.
    pub start: u64,
    pub peak_snr_db: f32,
    pub snr_sum: f64,
    pub blocks: u32,
    /// Announced to live listeners.
    pub live: bool,
    pub codec: crate::dmr::vocoder::Codec,
}

impl Channel {
    pub fn new(
        id: u64,
        slot: usize,
        tuned: Slot,
        extractor: Extractor,
        demod: Box<dyn Demod>,
        audio_t0: i64,
    ) -> Self {
        Channel {
            id,
            slot,
            tuned,
            rate: extractor.output_rate(),
            extractor,
            demod,
            audio_t0,
            confirmed: false,
            pending: Vec::new(),
            emitted: 0,
            live_sent: None,
            calls: [None, None],
            iq: Vec::new(),
        }
    }

    /// Demodulate one block; `open` is the power squelch for it.
    pub fn process(&mut self, block: &Block, open: bool) {
        self.iq.clear();
        self.extractor.process(block, &mut self.iq);
        self.demod.process(&self.iq, open, &mut self.pending);
    }

    pub fn records_audio(&self) -> bool {
        self.demod.records_audio()
    }

    /// End of transmission: release held audio, muted.
    pub fn flush(&mut self) {
        self.demod.flush(&mut self.pending);
    }

    /// Emit everything except the last `holdback` samples.
    pub fn drain_ready(&mut self, holdback: usize, out: &mut Vec<Output>) {
        let n = self.pending.len().saturating_sub(holdback);
        if n > 0 {
            let samples: Vec<f32> = self.pending.drain(..n).collect();
            self.emitted += samples.len();
            out.push(Output::Audio {
                id: self.id,
                samples,
            });
        }
    }
}
