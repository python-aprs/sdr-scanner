//! Noise floor per segment of the band, the noise-step guard, and how far
//! the whole band sits above its floor this block.

use super::spectrum::Spectrum;
use crate::channelizer::BankParams;

/// Median of an exponential-ish smoothed noise PSD sits below its mean; this
/// converts the per-segment median to a mean estimate. Measured by
/// `idle_slots_read_zero_db` for the default smoothing at typical block rates.
const MEDIAN_TO_MEAN: f32 = 1.2;
/// Mean over median of one block's (unsmoothed, exponentially distributed)
/// bin powers: 1 / ln 2.
const RAW_MEDIAN_TO_MEAN: f32 = std::f32::consts::LOG2_E;
/// A segment whose median power this block is this far above its floor has
/// risen as a whole (a gain step or front-end overload), not because of a
/// few signals in it.
const STEP_RATIO: f32 = 2.0;

pub(super) struct NoiseFloor {
    pub bounds: Vec<(usize, usize)>,
    /// Per segment: the floor (mean power per bin).
    pub floor: Vec<f32>,
    /// Per segment: its level this block (from its median bin).
    level: Vec<f32>,
    scratch: Vec<f32>,
    /// Scratch: each segment's mean power this block.
    power: Vec<f32>,
    /// Band-wide noise this block over the floor, at least 1 (see
    /// [`NoiseFloor::common_ratio`]).
    pub common: f32,
    /// Band-wide power this block: the median over segments of their mean
    /// power (absolute, not over the floor, which a burst can snap up).
    pub band_power: f32,
    /// Blocks in which the floor rose across much of the band at once.
    pub step_blocks: u64,
}

impl NoiseFloor {
    /// `segments` segments over the middle `usable` fraction of the band.
    pub fn new(bank: BankParams, usable: f64, segments: usize) -> Self {
        let n = bank.n;
        let bin_hz = bank.bin_hz();
        let half_usable = bank.sample_rate * usable / 2.0;
        let first = ((n as f64 / 2.0) - half_usable / bin_hz).max(0.0) as usize;
        let last = ((n as f64 / 2.0) + half_usable / bin_hz).min(n as f64) as usize;
        let segs = segments.max(1);
        let seg_len = (last - first).div_ceil(segs);
        let bounds: Vec<(usize, usize)> = (0..segs)
            .map(|k| (first + k * seg_len, (first + (k + 1) * seg_len).min(last)))
            .filter(|(a, b)| b > a)
            .collect();
        let nseg = bounds.len();
        NoiseFloor {
            bounds,
            floor: vec![0.0; nseg],
            level: vec![0.0; nseg],
            scratch: Vec::new(),
            power: Vec::with_capacity(nseg),
            common: 1.0,
            band_power: 0.0,
            step_blocks: 0,
        }
    }

    /// The segment holding shifted bin `bin` (0 if none does).
    pub fn segment_of(&self, bin: usize) -> usize {
        self.bounds
            .iter()
            .position(|&(a, b)| bin >= a && bin < b)
            .unwrap_or(0)
    }

    /// Follow this block. `alpha` is the floor's smoothing factor (1 for
    /// the first block). Returns whether the noise stepped up across much
    /// of the band at once (gain change, overload, a stream restart):
    /// nothing new may start in such a block.
    pub fn update(&mut self, sp: &Spectrum, alpha: f32, first: bool) -> bool {
        let mut risen = 0usize;
        for (i, &(a, b)) in self.bounds.iter().enumerate() {
            // The whole segment's level this block, from its median bin:
            // signals occupy too few bins to move it.
            self.scratch.clear();
            let stride = ((b - a) / 512).max(1);
            self.scratch
                .extend(sp.raw[a..b].iter().step_by(stride).copied());
            let mid = self.scratch.len() / 2;
            let (_, m, _) = self.scratch.select_nth_unstable_by(mid, f32::total_cmp);
            let level = *m * RAW_MEDIAN_TO_MEAN;
            self.level[i] = level;
            {
                let f = &mut self.floor[i];
                if *f > 0.0 && level > *f * STEP_RATIO {
                    // Follow a broadband rise at once instead of over the
                    // slow growth time, or every slot in the segment looks
                    // busy until the floor catches up.
                    risen += 1;
                    *f = level;
                    continue;
                }
            }
            self.scratch.clear();
            let stride = ((b - a) / 512).max(1);
            self.scratch
                .extend(sp.psd[a..b].iter().step_by(stride).copied());
            let mid = self.scratch.len() / 2;
            let (_, m, _) = self.scratch.select_nth_unstable_by(mid, f32::total_cmp);
            let est = *m * MEDIAN_TO_MEAN;
            // Grow slowly, fall fast: a burst of signals in a segment must not
            // drag the floor up, but a gain drop should be followed quickly.
            let f = &mut self.floor[i];
            // A block of zeros (a zero-filled overrun gap) says nothing about
            // the noise floor; following it down would make every slot look
            // busy once samples return.
            if *f > 0.0 && est < *f * 1e-3 {
                continue;
            }
            if *f == 0.0 || est < *f {
                *f += (alpha * 4.0).min(1.0) * (est - *f);
            } else {
                *f += alpha * (est - *f);
            }
        }
        let step = risen * 4 >= self.bounds.len() && !first;
        if step {
            self.step_blocks += 1;
        }
        self.common = self.common_ratio(sp);
        step
    }

    /// How far the whole band's noise sits above the smoothed floor this
    /// block, at least 1: the median over segments of this block's level
    /// over the floor, measured two ways (below), whichever is higher.
    ///
    /// The floor follows the noise over seconds, but interference can lift
    /// the whole band for a block or a few hundred ms at a time. Seen live
    /// on an RSP1A at LNA 3 / GR 40 (Nov 2026): swells of 2-6 dB several
    /// times a second, and a 20-50 ms burst every 1.2 s that read 5-11 dB
    /// in every slot. Against the slow floor alone that reads as a weak
    /// signal in every slot, which opened dozens of empty recordings and
    /// held them open for minutes. A rise shared by most segments is
    /// interference, not signals: a signal occupies a few segments at most,
    /// so the median over segments ignores it.
    ///
    /// Each segment's level is measured from its median bin and from its
    /// mean power. The median bin follows broadband noise, including the
    /// non-Gaussian kind from front-end overload, without being pulled up
    /// by the signals in the segment. The mean (the same statistic as slot
    /// power) also follows interference that is spiky across frequency,
    /// like the comb from a pulse train, which lifts a band's power
    /// without moving its median bin.
    ///
    /// Also sets `band_power`.
    fn common_ratio(&mut self, sp: &Spectrum) -> f32 {
        let median = |v: &mut Vec<f32>| -> f32 {
            if v.is_empty() {
                return 1.0;
            }
            let mid = v.len() / 2;
            *v.select_nth_unstable_by(mid, f32::total_cmp).1
        };
        self.scratch.clear();
        self.scratch.extend(
            self.level
                .iter()
                .zip(&self.floor)
                .filter(|&(&l, &f)| f > 0.0 && l > 0.0)
                .map(|(&l, &f)| l / f),
        );
        let by_median = median(&mut self.scratch);
        self.scratch.clear();
        self.power.clear();
        for (&(a, b), &f) in self.bounds.iter().zip(&self.floor) {
            let mean = (sp.power(a, b) / (b - a) as f64) as f32;
            if f > 0.0 && mean > 0.0 {
                self.scratch.push(mean / f);
                self.power.push(mean);
            }
        }
        let by_mean = median(&mut self.scratch);
        self.band_power = if self.power.is_empty() {
            0.0
        } else {
            median(&mut self.power)
        };
        by_median.max(by_mean).max(1.0)
    }

    /// Noise floor (dB per bin, relative to a full-scale tone reading 0 dB)
    /// at each segment, for an FFT of `n`.
    pub fn floor_db(&self, n: usize) -> Vec<f32> {
        let n2 = (n as f32).powi(2);
        self.floor
            .iter()
            .map(|&f| 10.0 * (f / n2).max(1e-30).log10())
            .collect()
    }
}
