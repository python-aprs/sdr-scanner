//! AFSK1200 packet channels: discriminator output into the AX.25 decoder.

use super::Demod;
use crate::afsk::{AfskDecoder, Decoded};
use crate::config::{Mode, Slot};
use crate::dsp::FmDiscriminator;
use num_complex::Complex32;

/// Makes frames, never audio. The FCS decides what is a packet, so it
/// needs no squelch.
pub struct AfskPacket {
    disc: FmDiscriminator,
    decoder: AfskDecoder,
    demod: Vec<f32>,
    /// Frames decoded (each passed its FCS).
    frames: u32,
}

impl AfskPacket {
    pub fn new(slot: &Slot, rate: f64) -> Self {
        AfskPacket {
            disc: FmDiscriminator::new(rate, slot.deviation),
            decoder: AfskDecoder::new(rate),
            demod: Vec::new(),
            frames: 0,
        }
    }
}

impl Demod for AfskPacket {
    fn process(&mut self, iq: &[Complex32], _open: bool, _audio: &mut Vec<f32>) {
        self.demod.clear();
        self.disc.process(iq, &mut self.demod);
        self.decoder.push(&self.demod);
    }

    fn flush(&mut self, _audio: &mut Vec<f32>) {}

    fn records_audio(&self) -> bool {
        false
    }

    fn preambles(&self) -> u64 {
        self.decoder.preambles
    }

    fn detected(&self) -> Option<Mode> {
        (self.frames > 0).then_some(Mode::Afsk1200)
    }

    fn take_frames(&mut self) -> Vec<Decoded> {
        let f = self.decoder.take();
        self.frames += f.len() as u32;
        f
    }
}
