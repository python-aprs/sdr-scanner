//! Fast-convolution (overlap-save) filter bank, PLAN.md 3.2.
//!
//! One forward FFT of size `n` per block of `hop` new input samples feeds both
//! the detector (power spectrum) and every active channel. A channel takes the
//! `m` bins around its centre, multiplies them by the channel filter's
//! frequency response, runs an `m`-point inverse FFT and keeps the last
//! `hop / d` samples (`d = n / m`), which is the decimated, filtered,
//! frequency-shifted baseband for that block.
//!
//! Overlap-save requires the channel filter (at the input rate) to be no
//! longer than `n - hop + 1` taps; [`FilterBank`] designs filters to fit.

use crate::dsp;
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::f64::consts::PI;
use std::sync::Arc;

/// Shape of the filter bank.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BankParams {
    pub sample_rate: f64,
    /// Forward FFT size (power of two).
    pub n: usize,
    /// New input samples per block. `n - hop` samples of overlap.
    pub hop: usize,
}

impl BankParams {
    /// Pick sizes for a sample rate: FFT bins of roughly 50–100 Hz and 25 %
    /// overlap.
    pub fn for_rate(sample_rate: f64) -> Self {
        // Aim for ~76 Hz bins at 10 MS/s (2^17), keeping a power of two.
        let target = sample_rate / 76.0;
        let n = (target.log2().round() as u32).clamp(10, 20);
        let n = 1usize << n;
        BankParams {
            sample_rate,
            n,
            hop: n / 4 * 3,
        }
    }

    pub fn bin_hz(&self) -> f64 {
        self.sample_rate / self.n as f64
    }

    pub fn overlap(&self) -> usize {
        self.n - self.hop
    }

    /// Duration of one block of new input.
    pub fn block_secs(&self) -> f64 {
        self.hop as f64 / self.sample_rate
    }

    /// Smallest power-of-two channel IFFT size whose output rate is at least
    /// `min_rate`, also keeping `hop` divisible by the decimation.
    pub fn channel_size(&self, min_rate: f64) -> usize {
        let mut m = 16usize;
        while (m as f64) * self.bin_hz() < min_rate || self.hop % (self.n / m) != 0 {
            m *= 2;
            assert!(m <= self.n, "no channel size fits rate {min_rate}");
        }
        m
    }
}

/// One forward-FFT block, in natural FFT order (bin 0 = DC). Shared read-only
/// by the detector and all channel extractors.
#[derive(Debug, Clone)]
pub struct Block {
    /// Index of this block (0, 1, 2, ...).
    pub index: u64,
    /// Stream sample index of the first *new* sample in this block (the
    /// sample at offset `overlap` within the FFT window).
    pub first_sample: u64,
    pub bins: Vec<Complex32>,
}

/// Produces forward-FFT blocks from a stream of samples.
pub struct Analyzer {
    params: BankParams,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<Complex32>,
    /// Samples currently in `window` (fills to `n`).
    fill: usize,
    scratch: Vec<Complex32>,
    next_index: u64,
    /// Stream index of `window[0]`.
    window_start: i64,
}

impl Analyzer {
    pub fn new(params: BankParams) -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(params.n);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        // Start with `overlap` zeros so the first block's new samples begin at
        // stream index 0.
        Analyzer {
            params,
            fft,
            window: vec![Complex32::default(); params.n],
            fill: params.overlap(),
            scratch,
            next_index: 0,
            window_start: -(params.overlap() as i64),
        }
    }

    pub fn params(&self) -> &BankParams {
        &self.params
    }

    /// Feed samples; calls `emit` with each completed block. `recycle` returns
    /// a spare bins buffer if the caller has one (to avoid allocation).
    pub fn push(
        &mut self,
        mut samples: &[Complex32],
        mut recycle: impl FnMut() -> Option<Vec<Complex32>>,
        mut emit: impl FnMut(Block),
    ) {
        let n = self.params.n;
        while !samples.is_empty() {
            let take = (n - self.fill).min(samples.len());
            self.window[self.fill..self.fill + take].copy_from_slice(&samples[..take]);
            self.fill += take;
            samples = &samples[take..];
            if self.fill == n {
                let mut bins = recycle().unwrap_or_default();
                bins.clear();
                bins.extend_from_slice(&self.window);
                self.fft.process_with_scratch(&mut bins, &mut self.scratch);
                let first = self.window_start + self.params.overlap() as i64;
                emit(Block {
                    index: self.next_index,
                    first_sample: first as u64,
                    bins,
                });
                self.next_index += 1;
                self.window.copy_within(self.params.hop.., 0);
                self.fill = self.params.overlap();
                self.window_start += self.params.hop as i64;
            }
        }
    }
}

/// A channel filter's response on the `m` bins around the channel centre,
/// in FFT order (bin 0 = channel centre), with the `1/n`-style scaling folded
/// in so the extractor output has the same amplitude as the input.
#[derive(Debug, Clone)]
pub struct ChannelFilter {
    pub m: usize,
    pub bandwidth: f64,
    pub response: Vec<f32>,
}

impl ChannelFilter {
    /// Design a low-pass with passband edge `bandwidth / 2` and stopband at
    /// most `stop_hz` (and within the channel output Nyquist), at the input
    /// rate, then sample its response on the bank's bin grid.
    pub fn design(params: &BankParams, m: usize, bandwidth: f64) -> Self {
        let fs = params.sample_rate;
        let out_rate = m as f64 * params.bin_hz();
        let pass = bandwidth / 2.0;
        // Stopband edge: as wide as the output Nyquist allows, so the filter
        // stays short, but never past it (aliasing).
        let stop = (out_rate / 2.0)
            .min(pass * 1.6)
            .max(pass + 2.0 * params.bin_hz());
        let max_taps = params.overlap() + 1;
        let mut transition = (stop - pass) / fs;
        let atten = 70.0;
        let mut ntaps = dsp::kaiser_taps(transition, atten);
        while ntaps > max_taps {
            transition *= 1.25;
            ntaps = dsp::kaiser_taps(transition, atten);
        }
        let cutoff = (pass + transition * fs / 2.0) / fs;
        let h = dsp::lowpass(cutoff, ntaps, atten);
        // Zero-phase response sampled at bin k: sum h[i] e^{-j 2pi k (i-mid)/n}.
        // Symmetric taps make it real. Only bins within +-m/2 are needed.
        let n = params.n;
        let mid = (ntaps - 1) as f64 / 2.0;
        let half = m / 2;
        let mut response = vec![0.0f32; m];
        // Forward FFT (n points) and inverse FFT (m points) are both
        // unnormalised: a full-scale tone on a bin reads n, so scale by 1/n.
        let scale = 1.0 / n as f64;
        for (j, r) in response.iter_mut().enumerate() {
            let k = if j < half {
                j as f64
            } else {
                j as f64 - m as f64
            };
            let mut acc = 0.0f64;
            for (i, &v) in h.iter().enumerate() {
                acc += v as f64 * (2.0 * PI * k * (i as f64 - mid) / n as f64).cos();
            }
            *r = (acc * scale) as f32;
        }
        // The linear-phase delay is handled by the extractor (it keeps the
        // samples the filter has fully settled on), so a zero-phase response is
        // right here: overlap-save with a zero-phase filter of half-length
        // `mid` needs `mid` samples of history on *both* sides; the extractor
        // keeps the centre of the block. See `Extractor::keep_range`.
        ChannelFilter {
            m,
            bandwidth,
            response,
        }
    }
}

/// Pulls one channel out of successive [`Block`]s.
pub struct Extractor {
    params: BankParams,
    filter: Arc<ChannelFilter>,
    ifft: Arc<dyn Fft<f32>>,
    /// Channel centre as an FFT bin (may be negative), and the residual
    /// offset (Hz) that the bin grid can't represent.
    center_bin: i64,
    residual_hz: f64,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    /// NCO phase (cycles) for the residual offset, continuous across blocks.
    nco_phase: f64,
}

impl Extractor {
    /// `offset_hz` is the channel centre relative to the capture centre.
    pub fn new(
        params: BankParams,
        filter: Arc<ChannelFilter>,
        ifft: Arc<dyn Fft<f32>>,
        offset_hz: f64,
    ) -> Self {
        let bin = (offset_hz / params.bin_hz()).round() as i64;
        let residual = offset_hz - bin as f64 * params.bin_hz();
        let m = filter.m;
        Extractor {
            params,
            filter,
            scratch: vec![Complex32::default(); ifft.get_inplace_scratch_len()],
            ifft,
            center_bin: bin,
            residual_hz: residual,
            buf: vec![Complex32::default(); m],
            nco_phase: 0.0,
        }
    }

    pub fn decimation(&self) -> usize {
        self.params.n / self.filter.m
    }

    pub fn output_rate(&self) -> f64 {
        self.params.sample_rate / self.decimation() as f64
    }

    /// Output samples per block.
    pub fn per_block(&self) -> usize {
        self.params.hop / self.decimation()
    }

    /// Which IFFT outputs are valid for a zero-phase filter: the window holds
    /// stream samples `[first - overlap, first + hop)`; outputs at decimated
    /// positions covering `[first - overlap/2, first + hop - overlap/2)` are
    /// clear of circular wrap from both ends when the filter half-length is
    /// at most `overlap / 2`. Their stream time is therefore delayed by
    /// `overlap / 2` input samples relative to the block's new samples.
    fn keep_range(&self) -> (usize, usize) {
        let d = self.decimation();
        let start = self.params.overlap() / 2 / d;
        (start, start + self.per_block())
    }

    /// Stream sample index (input rate) of the first output of a block.
    pub fn output_start_sample(&self, block: &Block) -> u64 {
        (block.first_sample as i64 - (self.params.overlap() / 2) as i64).max(0) as u64
    }

    /// Append this block's channel samples to `out`.
    pub fn process(&mut self, block: &Block, out: &mut Vec<Complex32>) {
        let n = self.params.n as i64;
        let m = self.filter.m;
        let half = (m / 2) as i64;
        let resp = &self.filter.response;
        for (j, (b, &r)) in self.buf.iter_mut().zip(resp).enumerate() {
            let k = if (j as i64) < half {
                j as i64
            } else {
                j as i64 - m as i64
            };
            let src = (self.center_bin + k).rem_euclid(n) as usize;
            *b = block.bins[src] * r;
        }
        self.ifft
            .process_with_scratch(&mut self.buf, &mut self.scratch);

        // The IFFT output is the channel at baseband sampled at window offsets
        // 0, d, 2d, ..., carrying a phase term from the bin shift:
        // z[t] = (x*h)[t] e^{-j 2pi bin t / n}, with t measured from the window
        // start. Rotate by the window start so the phase is continuous in
        // stream time (computed in exact integer turns mod n).
        let win_start = block.first_sample as i64 - self.params.overlap() as i64;
        let (a, b) = self.keep_range();
        let block_turns = (self.center_bin * win_start).rem_euclid(n) as f64 / n as f64;
        let rot = Complex32::from_polar(1.0, (-2.0 * PI * block_turns) as f32);
        // Residual offset the bin grid can't represent, removed by a small NCO
        // running at the output rate.
        let step = self.residual_hz / self.output_rate();
        out.reserve(b - a);
        for v in &self.buf[a..b] {
            let nco = Complex32::from_polar(1.0, (-2.0 * PI * self.nco_phase) as f32);
            out.push(v * rot * nco);
            self.nco_phase = (self.nco_phase + step).rem_euclid(1.0);
        }
    }
}

/// Designs and caches channel filters (one per distinct bandwidth) and the
/// inverse FFT plans.
pub struct FilterBank {
    params: BankParams,
    min_rate: f64,
    planner: FftPlanner<f32>,
    filters: Vec<Arc<ChannelFilter>>,
}

impl FilterBank {
    /// `min_rate` is the lowest acceptable channel output rate.
    pub fn new(params: BankParams, min_rate: f64) -> Self {
        FilterBank {
            params,
            min_rate,
            planner: FftPlanner::new(),
            filters: Vec::new(),
        }
    }

    pub fn params(&self) -> &BankParams {
        &self.params
    }

    pub fn extractor(&mut self, offset_hz: f64, bandwidth: f64) -> Extractor {
        let min_rate = self.min_rate.max(bandwidth * 1.5);
        let m = self.params.channel_size(min_rate);
        let filter = match self
            .filters
            .iter()
            .find(|f| f.m == m && f.bandwidth == bandwidth)
        {
            Some(f) => f.clone(),
            None => {
                let f = Arc::new(ChannelFilter::design(&self.params, m, bandwidth));
                self.filters.push(f.clone());
                f
            }
        };
        let ifft = self.planner.plan_fft_inverse(m);
        Extractor::new(self.params, filter, ifft, offset_hz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cw(fs: f64, f: f64, n: usize, amp: f32) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let ph = 2.0 * PI * f * i as f64 / fs;
                Complex32::from_polar(amp, ph as f32)
            })
            .collect()
    }

    fn run(params: BankParams, x: &[Complex32], offset: f64, bw: f64) -> (Vec<Complex32>, f64) {
        let mut bank = FilterBank::new(params, 25_000.0);
        let mut ex = bank.extractor(offset, bw);
        let rate = ex.output_rate();
        let mut an = Analyzer::new(params);
        let mut out = Vec::new();
        for chunk in x.chunks(10_000) {
            an.push(chunk, || None, |b| ex.process(&b, &mut out));
        }
        (out, rate)
    }

    fn small() -> BankParams {
        BankParams {
            sample_rate: 2_000_000.0,
            n: 1 << 15,
            hop: (1 << 15) / 4 * 3,
        }
    }

    #[test]
    fn params_for_common_rates() {
        let p = BankParams::for_rate(10e6);
        assert_eq!(p.n, 1 << 17);
        assert_eq!(p.channel_size(25_000.0), 512);
        let p = BankParams::for_rate(8e6);
        assert_eq!(p.n, 1 << 17);
        let p = BankParams::for_rate(2.5e6);
        assert_eq!(p.n, 1 << 15);
    }

    #[test]
    fn off_bin_tone_comes_out_at_baseband_offset_with_unit_amplitude() {
        let p = small();
        let fs = p.sample_rate;
        // Channel at +312.5 kHz plus an off-grid offset; tone 1.2 kHz above.
        let ch = 312_517.0;
        let x = cw(fs, ch + 1200.0, 400_000, 0.5);
        let (y, rate) = run(p, &x, ch, 16_000.0);
        let settled = &y[200..y.len() - 10];
        for v in settled {
            assert!((v.norm() - 0.5).abs() < 0.01, "{}", v.norm());
        }
        // Instantaneous frequency should be +1200 Hz everywhere (continuous
        // phase across block boundaries).
        // Per-sample error is bounded by the 70 dB stopband (leakage of the
        // rectangular block edges); the mean must be exact.
        let mut sum = 0.0;
        for w in settled.windows(2) {
            let f = (w[1] * w[0].conj()).arg() as f64 * rate / (2.0 * PI);
            assert!((f - 1200.0).abs() < 20.0, "{f}");
            sum += f;
        }
        let mean = sum / (settled.len() - 1) as f64;
        assert!((mean - 1200.0).abs() < 0.05, "{mean}");
    }

    #[test]
    fn adjacent_channel_is_rejected() {
        let p = small();
        let fs = p.sample_rate;
        let ch = -150_000.0;
        // Strong tone 25 kHz away.
        let x = cw(fs, ch + 25_000.0, 300_000, 1.0);
        let (y, _) = run(p, &x, ch, 16_000.0);
        let pwr: f32 = y[100..].iter().map(|v| v.norm_sqr()).sum::<f32>() / (y.len() - 100) as f32;
        let db = 10.0 * pwr.log10();
        assert!(db < -60.0, "{db}");
    }

    #[test]
    fn output_length_tracks_input() {
        let p = small();
        let x = cw(p.sample_rate, 0.0, p.hop * 10, 1.0);
        let (y, rate) = run(p, &x, 0.0, 16_000.0);
        let d = p.sample_rate / rate;
        assert_eq!(y.len() as f64, (p.hop * 10) as f64 / d);
    }

    #[test]
    fn timing_matches_stream_position() {
        // A tone that switches on at a known sample shows up at the right
        // output index once the overlap/2 delay is accounted for.
        let p = small();
        let fs = p.sample_rate;
        let on = 100_000usize;
        let mut x = vec![Complex32::default(); 300_000];
        for (i, v) in x.iter_mut().enumerate().skip(on) {
            *v = Complex32::from_polar(1.0, (2.0 * PI * 1000.0 * i as f64 / fs) as f32);
        }
        let mut bank = FilterBank::new(p, 25_000.0);
        let mut ex = bank.extractor(0.0, 16_000.0);
        let d = ex.decimation();
        let mut an = Analyzer::new(p);
        let mut first_out: Option<u64> = None;
        let mut out = Vec::new();
        an.push(
            &x,
            || None,
            |b| {
                if first_out.is_none() {
                    first_out = Some(ex.output_start_sample(&b));
                }
                ex.process(&b, &mut out)
            },
        );
        // Output index where amplitude crosses 0.5.
        let idx = out.iter().position(|v| v.norm() > 0.5).unwrap();
        // first_out is 0 clamped; the first block's output starts overlap/2
        // before stream 0.
        let t0 = -((p.overlap() / 2) as i64);
        let t = t0 + (idx * d) as i64;
        assert!((t - on as i64).abs() <= 2 * d as i64, "t={t} on={on}");
    }
}
