//! NBFM voice: discriminator, noise squelch, squelch gate, de-emphasis,
//! resampling to the audio rate, CTCSS/DCS tagging, high-pass, levelling.

use super::{Demod, Levels, SquelchGate, Tags};
use crate::config::Slot;
use crate::dsp::{Agc, Deemphasis, FmDiscriminator, HighPass, NoiseSquelch, Resampler, soft_limit};
use crate::scanner::ScannerConfig;
use crate::tone::ToneDetector;
use num_complex::Complex32;

pub struct NbfmVoice {
    disc: FmDiscriminator,
    nsq: Option<NoiseSquelch>,
    gate: SquelchGate,
    deemph: Option<Deemphasis>,
    resampler: Resampler,
    /// Audio samples still to drop for the resampler's group delay.
    skip: usize,
    tone: Option<ToneDetector>,
    hp: Option<HighPass>,
    levels: Levels,
    spur_spread_db: f32,
    agc: Option<Agc>,
    gain: f32,
    /// Discriminator samples released unmuted so far, and their rate.
    played: u64,
    rate: f64,
    /// The block being released (reused for the next block's output).
    demod: Vec<f32>,
}

impl NbfmVoice {
    pub fn new(slot: &Slot, rate: f64, cfg: &ScannerConfig) -> Self {
        let audio_rate = cfg.audio_rate as f64;
        let cutoff = (audio_rate / 2.0 * 0.85).min(slot.bandwidth / 2.0);
        let resampler = Resampler::new(rate, audio_rate, cutoff, audio_rate / 2.0 - cutoff);
        NbfmVoice {
            disc: FmDiscriminator::new(rate, slot.deviation),
            nsq: (cfg.noise_squelch_db > 0.0).then(|| {
                NoiseSquelch::new(rate, slot.deviation, slot.bandwidth, cfg.noise_squelch_db)
            }),
            gate: SquelchGate::new(rate),
            deemph: cfg
                .deemphasis
                .then(|| Deemphasis::new(rate, cfg.deemphasis_tau)),
            skip: resampler.delay_out().round() as usize,
            resampler,
            tone: cfg.tones.then(|| ToneDetector::new(audio_rate)),
            hp: (cfg.highpass_hz > 0.0).then(|| HighPass::new(audio_rate, cfg.highpass_hz)),
            levels: Levels::new(cfg.audio_rate, cfg.spur_secs),
            spur_spread_db: cfg.spur_spread_db,
            agc: cfg.agc.then(|| Agc::new(audio_rate)),
            gain: cfg.audio_gain,
            played: 0,
            rate,
            demod: Vec::new(),
        }
    }

    /// Gate one block's discriminator output (in `demod`) and run it
    /// through the audio filters into `audio`.
    ///
    /// The squelch acts on the discriminator output, before de-emphasis,
    /// resampling and the high-pass, so those filters never ring on loud
    /// FM noise.
    fn release(&mut self, play: bool, audio: &mut Vec<f32>) {
        if play {
            self.played += self.demod.len() as u64;
        }
        self.gate.apply(&mut self.demod, play);
        if let Some(d) = &mut self.deemph {
            d.process(&mut self.demod);
        }
        let start = audio.len();
        self.resampler.process(&self.demod, audio);
        if self.skip > 0 {
            let k = self.skip.min(audio.len() - start);
            audio.drain(start..start + k);
            self.skip -= k;
        }
        let new = &mut audio[start..];
        if play && let Some(t) = &mut self.tone {
            t.push(new);
        }
        if let Some(h) = &mut self.hp {
            h.process(new);
        }
        self.levels.push(new, play);
        if let Some(agc) = &mut self.agc {
            agc.process(new, play);
        } else {
            for v in new {
                *v = soft_limit(*v * self.gain);
            }
        }
    }
}

impl Demod for NbfmVoice {
    /// Blocks where the slot is below the close threshold (pre-roll noise,
    /// the hang time, fades, zero-filled drops) are muted, and so are
    /// blocks the noise squelch calls noise. A block is gated once its
    /// neighbours are known (see [`SquelchGate`]), which keeps the
    /// discriminator's settling at key-up and its breakup at key-down out
    /// of the recording.
    fn process(&mut self, iq: &[Complex32], open: bool, audio: &mut Vec<f32>) {
        let mut d = std::mem::take(&mut self.demod);
        d.clear();
        self.disc.process(iq, &mut d);
        // The noise squelch measures every block, so its hysteresis follows
        // the signal even while the power squelch has it muted.
        let quiet = self.nsq.as_mut().is_none_or(|n| n.update(&d));
        if let Some((d, play)) = self.gate.push(d, open && quiet) {
            self.demod = d;
            self.release(play, audio);
        }
    }

    fn flush(&mut self, audio: &mut Vec<f32>) {
        while let Some(d) = self.gate.pop_held() {
            self.demod = d;
            self.release(false, audio);
        }
        self.demod.clear();
    }

    fn played_secs(&self) -> f64 {
        self.played as f64 / self.rate
    }

    fn tags(&self) -> Tags {
        Tags {
            tone: self.tone.as_ref().and_then(|t| t.detect()),
        }
    }

    fn is_spur(&mut self) -> bool {
        self.levels.is_steady(self.spur_spread_db)
    }
}
