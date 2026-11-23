//! AFSK1200 packet channels: discriminator output into the AX.25 decoder.

use super::Demod;
use crate::afsk::{AfskDecoder, Decoded};
use crate::config::Slot;
use crate::dsp::FmDiscriminator;
use num_complex::Complex32;

/// Makes frames, never audio. The FCS decides what is a packet, so it
/// needs no squelch.
pub struct AfskPacket {
    disc: FmDiscriminator,
    decoder: AfskDecoder,
    demod: Vec<f32>,
}

impl AfskPacket {
    pub fn new(slot: &Slot, rate: f64) -> Self {
        AfskPacket {
            disc: FmDiscriminator::new(rate, slot.deviation),
            decoder: AfskDecoder::new(rate),
            demod: Vec::new(),
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

    fn take_frames(&mut self) -> Vec<Decoded> {
        self.decoder.take()
    }
}
