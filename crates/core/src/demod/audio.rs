//! Pieces any audio mode can use: the squelch gate with look-ahead, and the
//! level history the spur check reads.

use std::collections::VecDeque;

/// Blocks of squelch look-ahead. A block plays only if it, the block before
/// and the `LOOKAHEAD` blocks after it are all open. The detector's FFT
/// window reaches a few ms past a block's audio, so the block after a
/// key-down can still read as open; two blocks of look-ahead mute the
/// discriminator's key-down breakup anyway.
const LOOKAHEAD: usize = 2;

/// Holds demodulated blocks until the squelch state of the blocks around
/// them is known, then ramps them in or out (5 ms) so muting never clicks.
/// Muting keeps the samples, so timing is unchanged.
pub struct SquelchGate {
    /// Blocks held until the gates of the blocks after them are known.
    held: VecDeque<Vec<f32>>,
    /// Gate of the block before the oldest held block, then one per held
    /// block.
    gates: VecDeque<bool>,
    /// Gain now (0 = muted, 1 = open), ramped between blocks.
    gain: f32,
    /// Ramp step per sample.
    step: f32,
}

impl SquelchGate {
    /// For blocks at `rate` samples per second.
    pub fn new(rate: f64) -> Self {
        SquelchGate {
            held: VecDeque::new(),
            gates: VecDeque::from([false]),
            gain: 0.0,
            step: (1.0 / (0.005 * rate)) as f32,
        }
    }

    /// Hold block `d` with its gate. Returns the oldest held block once it
    /// is decided, with whether it plays (it and both neighbours open).
    pub fn push(&mut self, d: Vec<f32>, open: bool) -> Option<(Vec<f32>, bool)> {
        self.held.push_back(d);
        self.gates.push_back(open);
        if self.held.len() > LOOKAHEAD {
            // gates: [before, oldest held, ..., newest]
            let play = self.gates.iter().all(|&g| g);
            self.gates.pop_front();
            return self.held.pop_front().map(|d| (d, play));
        }
        None
    }

    /// The next held block, for releasing (muted) at the end.
    pub fn pop_held(&mut self) -> Option<Vec<f32>> {
        self.held.pop_front()
    }

    /// Ramp `buf` towards open (`play`) or muted.
    pub fn apply(&mut self, buf: &mut [f32], play: bool) {
        let target = if play { 1.0 } else { 0.0 };
        let step = self.step;
        for v in buf {
            if self.gain < target {
                self.gain = (self.gain + step).min(1.0);
            } else if self.gain > target {
                self.gain = (self.gain - step).max(0.0);
            }
            *v *= self.gain;
        }
    }
}

/// Audio level (dB) in 100 ms frames, `None` for a frame with any muted
/// audio, newest last. Speech swings 10 dB and more from frame to frame; a
/// carrier, plain or with a steady tone, barely moves.
pub struct Levels {
    levels: VecDeque<Option<f32>>,
    /// The frame being summed: sum of squares, samples, any muted.
    acc: (f64, usize, bool),
    /// Set when a new level is in.
    new: bool,
    /// Samples per level, and levels kept.
    len: usize,
    max: usize,
}

impl Levels {
    /// 100 ms frames at `audio_rate`, `secs` of them kept (0 keeps none).
    pub fn new(audio_rate: u32, secs: f64) -> Self {
        Levels {
            levels: VecDeque::new(),
            acc: (0.0, 0, false),
            new: false,
            len: audio_rate as usize / 10,
            max: (secs * 10.0).ceil() as usize,
        }
    }

    /// Add audio; `play` is false if any of it was muted.
    pub fn push(&mut self, audio: &[f32], play: bool) {
        let (acc, n, muted) = &mut self.acc;
        *muted |= !play;
        for &v in audio {
            *acc += v as f64 * v as f64;
            *n += 1;
            if *n == self.len {
                let db = 10.0 * (*acc / *n as f64).max(1e-20).log10();
                self.levels.push_back((!*muted).then_some(db as f32));
                if self.levels.len() > self.max {
                    self.levels.pop_front();
                }
                self.new = true;
                (*acc, *n, *muted) = (0.0, 0, !play);
            }
        }
    }

    /// Whether the levels say this is a steady carrier: a full window of
    /// frames, none muted, spread (standard deviation) under `spread_db`.
    /// Checked once per new level; false in between.
    pub fn is_steady(&mut self, spread_db: f32) -> bool {
        if !std::mem::take(&mut self.new) || self.max == 0 || self.levels.len() < self.max {
            return false;
        }
        let Some(v) = self.levels.iter().copied().collect::<Option<Vec<f32>>>() else {
            return false;
        };
        let mean = v.iter().sum::<f32>() / v.len() as f32;
        let var = v.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / v.len() as f32;
        var.sqrt() < spread_db
    }
}
