//! The detector's view of each block: per-bin power with a frequency-domain
//! Hann window, the DC notch, the smoothed spectrum and a running sum for
//! window power.

use crate::channelizer::{BankParams, Block};
use num_complex::Complex32;

pub(super) struct Spectrum {
    n: usize,
    bin_hz: f64,
    /// Bins either side of 0 Hz to interpolate over (0 for none).
    notch: usize,
    /// Smoothing factor for `psd`.
    alpha: f32,
    /// Raw power per bin this block, shifted order (0 = most negative
    /// frequency).
    pub raw: Vec<f32>,
    /// Smoothed power per bin, shifted order.
    pub psd: Vec<f32>,
    /// Running sum of `raw`: `cumsum[k]` is the sum of bins `..k`.
    pub cumsum: Vec<f64>,
}

impl Spectrum {
    pub fn new(bank: BankParams, smoothing: f64, dc_notch_hz: f64) -> Self {
        let n = bank.n;
        let block = bank.block_secs();
        Spectrum {
            n,
            bin_hz: bank.bin_hz(),
            notch: if dc_notch_hz > 0.0 {
                (dc_notch_hz / bank.bin_hz()).ceil() as usize + 1
            } else {
                0
            },
            alpha: (block / smoothing.max(block)).min(1.0) as f32,
            raw: vec![0.0; n],
            psd: vec![0.0; n],
            cumsum: vec![0.0; n + 1],
        }
    }

    /// Take in one block (`first`: the stream's first, which seeds the
    /// smoothed spectrum).
    pub fn update(&mut self, block: &Block, first: bool) {
        let n = self.n;
        let h = n / 2;
        let x = &block.bins;
        // Power with a frequency-domain Hann window (X[k] - (X[k-1]+X[k+1])/2,
        // times 1/2), which tames rectangular-window leakage from strong
        // signals into neighbouring slots. Written in shifted order.
        let hann = |prev: Complex32, cur: Complex32, next: Complex32| {
            ((cur - (prev + next) * 0.5) * 0.5).norm_sqr()
        };
        let (neg, pos) = self.raw.split_at_mut(h);
        // Bins 0..h (DC and positive) go to the upper half, h..n to the lower.
        pos[0] = hann(x[n - 1], x[0], x[1]);
        for (k, w) in x.windows(3).enumerate() {
            let k = k + 1;
            let v = hann(w[0], w[1], w[2]);
            if k < h {
                pos[k] = v;
            } else {
                neg[k - h] = v;
            }
        }
        neg[h - 1] = hann(x[n - 2], x[n - 1], x[0]);
        let w = self.notch;
        if w > 0 && h > w + 1 {
            let a = self.raw[h - w - 1];
            let b = self.raw[h + w + 1];
            self.raw[h - w..=h + w].fill(0.5 * (a + b));
        }
        let alpha = if first { 1.0 } else { self.alpha };
        for (s, &r) in self.psd.iter_mut().zip(&self.raw) {
            *s += alpha * (r - *s);
        }
        let mut acc = 0.0f64;
        self.cumsum[0] = 0.0;
        // Slot power uses this block's raw spectrum: integrating over the
        // slot's ~100+ bins already makes it a low-variance estimate, and
        // temporal smoothing would stretch short bursts past the attack time.
        for (k, &v) in self.raw.iter().enumerate() {
            acc += v as f64;
            self.cumsum[k + 1] = acc;
        }
    }

    /// Power this block in shifted bins `lo..hi`.
    pub fn power(&self, lo: usize, hi: usize) -> f64 {
        self.cumsum[hi] - self.cumsum[lo]
    }

    /// Power-weighted centre (offset Hz) of the power above `floor` in
    /// shifted bins `lo..hi` this block, or None if nothing is above it.
    pub fn excess_centroid(&self, lo: usize, hi: usize, floor: f32) -> Option<f64> {
        let (mut w, mut acc) = (0.0f64, 0.0f64);
        for (k, &p) in self.raw[lo..hi].iter().enumerate() {
            let excess = (p - floor).max(0.0) as f64;
            w += excess;
            acc += excess * (lo + k) as f64;
        }
        (w > 0.0).then(|| (acc / w - (self.n / 2) as f64) * self.bin_hz)
    }

    /// The smoothed spectrum between `lo_hz` and `hi_hz` (offsets from the
    /// capture centre), reduced to `bins` values in dB. Each value is the
    /// strongest bin it covers, so a narrow carrier stays visible at low
    /// display resolution.
    pub fn smoothed_db(&self, lo_hz: f64, hi_hz: f64, bins: usize) -> Vec<f32> {
        let n = self.n;
        let bin_hz = self.bin_hz;
        let to_bin =
            |hz: f64| ((hz / bin_hz).round() as i64 + (n / 2) as i64).clamp(0, n as i64) as usize;
        let (a, b) = (to_bin(lo_hz.min(hi_hz)), to_bin(lo_hz.max(hi_hz)));
        let n2 = (n as f32).powi(2);
        if b <= a || bins == 0 {
            return Vec::new();
        }
        let width = (b - a) as f64 / bins as f64;
        (0..bins)
            .map(|i| {
                let s = a + (i as f64 * width) as usize;
                let e = (a + ((i + 1) as f64 * width) as usize).max(s + 1).min(b);
                let p = self.psd[s..e].iter().copied().fold(0.0f32, f32::max);
                10.0 * (p / n2).max(1e-30).log10()
            })
            .collect()
    }
}
