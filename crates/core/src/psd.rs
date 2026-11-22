//! Averaged power spectral density (Welch) and simple peak finding.
//!
//! This is a diagnostic tool for Phase 0 (`scanner peaks`) and a reference for
//! the detector tests. The real-time path computes its PSD from the shared
//! channelizer FFT instead (PLAN.md 3.2).

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Accumulates Hann-windowed, 50 %-overlapped FFT power spectra.
pub struct Welch {
    n: usize,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    pending: Vec<Complex32>,
    scratch: Vec<Complex32>,
    acc: Vec<f64>,
    frames: u64,
}

impl Welch {
    pub fn new(n: usize) -> Self {
        assert!(
            n >= 16 && n.is_power_of_two(),
            "FFT size must be a power of two >= 16"
        );
        let fft = FftPlanner::new().plan_fft_forward(n);
        let window: Vec<f32> = (0..n)
            .map(|i| {
                let x = std::f32::consts::PI * 2.0 * i as f32 / n as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        Self {
            n,
            fft,
            window,
            pending: Vec::with_capacity(2 * n),
            scratch: vec![Complex32::default(); n],
            acc: vec![0.0; n],
            frames: 0,
        }
    }

    pub fn push(&mut self, samples: &[Complex32]) {
        self.pending.extend_from_slice(samples);
        let hop = self.n / 2;
        let mut start = 0;
        while self.pending.len() - start >= self.n {
            for (i, s) in self.scratch.iter_mut().enumerate() {
                *s = self.pending[start + i] * self.window[i];
            }
            self.fft.process(&mut self.scratch);
            for (a, s) in self.acc.iter_mut().zip(&self.scratch) {
                *a += s.norm_sqr() as f64;
            }
            self.frames += 1;
            start += hop;
        }
        self.pending.drain(..start);
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Mean power per bin in dBFS, scaled so a full-scale complex tone centred
    /// on a bin reads 0 dB. Natural frequency order: index 0 is -fs/2, index
    /// n/2 is DC.
    pub fn spectrum_db(&self) -> Vec<f32> {
        let frames = self.frames.max(1) as f64;
        let coherent_gain: f64 = self.window.iter().map(|&w| w as f64).sum();
        let scale = 1.0 / (frames * coherent_gain * coherent_gain);
        let half = self.n / 2;
        let mut out = vec![0f32; self.n];
        for (k, a) in self.acc.iter().enumerate() {
            out[(k + half) % self.n] = (10.0 * (a * scale).max(1e-30).log10()) as f32;
        }
        out
    }
}

/// A spectral peak found by [`find_peaks`].
#[derive(Debug, Clone, PartialEq)]
pub struct Peak {
    /// Offset from the tuner centre in Hz (power-weighted centroid).
    pub offset_hz: f64,
    /// Peak bin power in dBFS.
    pub power_db: f32,
    /// Peak power above the estimated noise floor in dB.
    pub snr_db: f32,
    /// Width of the region above threshold in Hz.
    pub width_hz: f64,
}

/// Estimate the noise floor as a low percentile of the spectrum (dB).
pub fn noise_floor_db(spectrum_db: &[f32], percentile: f32) -> f32 {
    let mut v: Vec<f32> = spectrum_db.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let idx = ((v.len() - 1) as f32 * percentile.clamp(0.0, 1.0)).round() as usize;
    v[idx]
}

/// Find contiguous regions more than `threshold_db` above the noise floor,
/// merging regions closer than `merge_hz`. Returns peaks sorted by power.
pub fn find_peaks(
    spectrum_db: &[f32],
    sample_rate: f64,
    threshold_db: f32,
    merge_hz: f64,
) -> Vec<Peak> {
    let n = spectrum_db.len();
    let bin_hz = sample_rate / n as f64;
    let floor = noise_floor_db(spectrum_db, 0.2);
    let thr = floor + threshold_db;
    let merge_bins = (merge_hz / bin_hz).ceil() as usize;

    let mut regions: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if spectrum_db[i] > thr {
            let start = i;
            while i < n && spectrum_db[i] > thr {
                i += 1;
            }
            match regions.last_mut() {
                Some(last) if start - last.1 <= merge_bins => last.1 = i,
                _ => regions.push((start, i)),
            }
        } else {
            i += 1;
        }
    }

    let mut peaks: Vec<Peak> = regions
        .into_iter()
        .map(|(a, b)| {
            let mut wsum = 0.0;
            let mut psum = 0.0;
            let mut max = f32::MIN;
            for (k, &db) in spectrum_db.iter().enumerate().take(b).skip(a) {
                let p = 10f64.powf(db as f64 / 10.0);
                wsum += p * k as f64;
                psum += p;
                max = max.max(db);
            }
            let centroid = wsum / psum;
            Peak {
                offset_hz: (centroid - (n / 2) as f64) * bin_hz,
                power_db: max,
                snr_db: max - floor,
                width_hz: (b - a) as f64 * bin_hz,
            }
        })
        .collect();
    peaks.sort_by(|a, b| b.power_db.total_cmp(&a.power_db));
    peaks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(fs: f64, f: f64, amp: f32, n: usize) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let ph = 2.0 * std::f64::consts::PI * f * i as f64 / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32) * amp
            })
            .collect()
    }

    #[test]
    fn full_scale_tone_reads_near_zero_db() {
        let fs = 1e6;
        let n = 1024;
        let bin = fs / n as f64;
        let mut w = Welch::new(n);
        w.push(&tone(fs, 37.0 * bin, 1.0, 16 * n));
        let s = w.spectrum_db();
        let peak = s.iter().cloned().fold(f32::MIN, f32::max);
        assert!((peak - 0.0).abs() < 0.1, "peak {peak}");
        let idx = s.iter().position(|&v| v == peak).unwrap();
        assert_eq!(idx, n / 2 + 37);
    }

    #[test]
    fn finds_tones_at_right_offsets() {
        let fs = 2.5e6;
        let n = 4096;
        let bin = fs / n as f64;
        // Tones on bin centres so Hann scalloping does not skew the levels.
        let (f1, f2) = (491.0 * bin, -852.0 * bin);
        let mut sig = tone(fs, f1, 0.1, 64 * n);
        for (s, t) in sig.iter_mut().zip(tone(fs, f2, 0.01, 64 * n)) {
            *s += t;
        }
        // small deterministic "noise" floor
        let mut x: u32 = 1;
        for s in sig.iter_mut() {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let a = (x >> 8) as f32 / (1 << 24) as f32 - 0.5;
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let b = (x >> 8) as f32 / (1 << 24) as f32 - 0.5;
            *s += Complex32::new(a, b) * 1e-3;
        }
        let mut w = Welch::new(n);
        w.push(&sig);
        let peaks = find_peaks(&w.spectrum_db(), fs, 20.0, 5e3);
        assert_eq!(peaks.len(), 2, "{peaks:?}");
        assert!((peaks[0].offset_hz - f1).abs() < bin);
        assert!((peaks[1].offset_hz - f2).abs() < bin);
        assert!((peaks[0].power_db - peaks[1].power_db - 20.0).abs() < 1.0);
    }

    #[test]
    fn push_in_odd_chunks_matches_one_push() {
        let fs = 1e6;
        let sig = tone(fs, 12_345.0, 0.5, 10_000);
        let mut a = Welch::new(256);
        a.push(&sig);
        let mut b = Welch::new(256);
        for c in sig.chunks(77) {
            b.push(c);
        }
        assert_eq!(a.frames(), b.frames());
        assert_eq!(a.spectrum_db(), b.spectrum_db());
    }
}
