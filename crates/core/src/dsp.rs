//! Small DSP building blocks shared by the channel chains: FIR design,
//! a rational polyphase resampler, de-emphasis, a DC-blocking high-pass,
//! the FM discriminator and an audio AGC.

use num_complex::Complex32;
use std::f64::consts::PI;

/// Zeroth-order modified Bessel function of the first kind (series form).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..64 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Kaiser window beta for a stopband attenuation in dB.
pub fn kaiser_beta(atten_db: f64) -> f64 {
    if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db >= 21.0 {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0)
    } else {
        0.0
    }
}

/// Odd tap count for a Kaiser-windowed design with the given transition
/// width (as a fraction of the sample rate).
pub fn kaiser_taps(transition: f64, atten_db: f64) -> usize {
    let n = ((atten_db - 7.95) / (14.36 * transition)).ceil().max(3.0) as usize;
    n | 1
}

/// Kaiser-windowed sinc low-pass. `cutoff` is the -6 dB point as a fraction
/// of the sample rate (0..0.5). Taps are normalised to unity DC gain.
pub fn lowpass(cutoff: f64, ntaps: usize, atten_db: f64) -> Vec<f32> {
    assert!(ntaps >= 1);
    let beta = kaiser_beta(atten_db);
    let mid = (ntaps - 1) as f64 / 2.0;
    let i0b = bessel_i0(beta);
    let mut h: Vec<f64> = (0..ntaps)
        .map(|i| {
            let t = i as f64 - mid;
            let sinc = if t == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * PI * cutoff * t).sin() / (PI * t)
            };
            let r = if mid > 0.0 { t / mid } else { 0.0 };
            let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
            sinc * w
        })
        .collect();
    let s: f64 = h.iter().sum();
    for v in &mut h {
        *v /= s;
    }
    h.into_iter().map(|v| v as f32).collect()
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Express `out_rate / in_rate` as a reduced fraction `up / down`. Rates are
/// rounded to the millihertz, which is exact for every rate this program uses
/// (power-of-two divisions of sample rates given in whole hertz).
pub fn rational_ratio(in_rate: f64, out_rate: f64) -> (usize, usize) {
    let a = (in_rate * 1000.0).round() as u64;
    let b = (out_rate * 1000.0).round() as u64;
    let g = gcd(a, b);
    ((b / g) as usize, (a / g) as usize)
}

/// Rational polyphase resampler for real samples: up-sample by `up`, low-pass,
/// down-sample by `down`, computing only the outputs that are kept.
#[derive(Debug, Clone)]
pub struct Resampler {
    up: usize,
    down: usize,
    /// `phases[p][k]` = prototype tap `k*up + p`, times `up`.
    phases: Vec<Vec<f32>>,
    taps_per_phase: usize,
    /// Input history, newest last; length `taps_per_phase - 1` between calls.
    hist: Vec<f32>,
    /// Position of the next output on the up-sampled grid, relative to the
    /// first sample of the current input chunk (in up-sampled units).
    phase: usize,
}

impl Resampler {
    /// `cutoff_hz` is the low-pass -6 dB point and `transition_hz` the full
    /// transition width (stopband edge = cutoff + transition / 2). Keep the
    /// stopband edge below the output Nyquist.
    pub fn new(in_rate: f64, out_rate: f64, cutoff_hz: f64, transition_hz: f64) -> Self {
        let (up, down) = rational_ratio(in_rate, out_rate);
        let proto_rate = in_rate * up as f64;
        let atten = 70.0;
        let taps_per_phase = kaiser_taps(transition_hz / proto_rate, atten)
            .div_ceil(up)
            .max(2);
        let ntaps = up * taps_per_phase;
        let h = lowpass(cutoff_hz / proto_rate, ntaps, atten);
        let mut phases = vec![vec![0.0f32; taps_per_phase]; up];
        for (i, &v) in h.iter().enumerate() {
            phases[i % up][i / up] = v * up as f32;
        }
        // Store each phase reversed so the inner loop is a forward dot product
        // over the oldest-to-newest history window.
        for p in &mut phases {
            p.reverse();
        }
        Resampler {
            up,
            down,
            phases,
            taps_per_phase,
            hist: vec![0.0; taps_per_phase - 1],
            phase: 0,
        }
    }

    pub fn ratio(&self) -> (usize, usize) {
        (self.up, self.down)
    }

    /// Group delay in output samples (approximate, for alignment).
    pub fn delay_out(&self) -> f64 {
        (self.up * self.taps_per_phase - 1) as f64 / 2.0 / self.down as f64
    }

    /// Resample `input`, appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let t = self.taps_per_phase;
        let h = t - 1;
        let mut buf = std::mem::take(&mut self.hist);
        buf.extend_from_slice(input);
        // Output n uses up-sampled index m = phase (relative to input[0]);
        // input index i = m / up, polyphase branch p = m % up. The newest
        // input it needs is input[i], which is buf[h + i].
        let n_in = input.len();
        let mut m = self.phase;
        while m / self.up < n_in {
            let i = m / self.up;
            let p = m % self.up;
            let window = &buf[i..i + t];
            let taps = &self.phases[p];
            let mut acc = 0.0f32;
            for k in 0..t {
                acc += window[k] * taps[k];
            }
            out.push(acc);
            m += self.down;
        }
        self.phase = m - n_in * self.up;
        let keep_from = buf.len() - h;
        buf.drain(..keep_from);
        self.hist = buf;
    }
}

/// One-pole de-emphasis low-pass with time constant `tau` (bilinear form,
/// unity gain at DC).
#[derive(Debug, Clone)]
pub struct Deemphasis {
    b0: f32,
    a1: f32,
    x1: f32,
    y1: f32,
}

impl Deemphasis {
    pub fn new(rate: f64, tau: f64) -> Self {
        // H(s) = 1 / (1 + s*tau), prewarped bilinear transform.
        let wc = 1.0 / tau;
        let k = wc / (2.0 * rate);
        let k = (k).tan();
        let b0 = k / (1.0 + k);
        let a1 = (k - 1.0) / (k + 1.0);
        Deemphasis {
            b0: b0 as f32,
            a1: a1 as f32,
            x1: 0.0,
            y1: 0.0,
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for v in buf {
            let x = *v;
            let y = self.b0 * (x + self.x1) - self.a1 * self.y1;
            self.x1 = x;
            self.y1 = y;
            *v = y;
        }
    }
}

#[derive(Debug, Clone)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Biquad {
    fn highpass(rate: f64, cutoff: f64, q: f64) -> Self {
        let w0 = 2.0 * PI * cutoff / rate;
        let alpha = w0.sin() / (2.0 * q);
        let cw = w0.cos();
        let a0 = 1.0 + alpha;
        let b0 = (1.0 + cw) / 2.0 / a0;
        Biquad {
            b: [b0 as f32, (-(1.0 + cw) / a0) as f32, b0 as f32],
            a: [(-2.0 * cw / a0) as f32, ((1.0 - alpha) / a0) as f32],
            z: [0.0; 2],
        }
    }

    fn process(&mut self, buf: &mut [f32]) {
        let [b0, b1, b2] = self.b;
        let [a1, a2] = self.a;
        for v in buf {
            let x = *v;
            let y = b0 * x + self.z[0];
            self.z[0] = b1 * x - a1 * y + self.z[1];
            self.z[1] = b2 * x - a2 * y;
            *v = y;
        }
    }
}

/// Fourth-order Butterworth high-pass (two biquads), used to remove CTCSS
/// tones (67–254 Hz) and DC from recorded audio.
#[derive(Debug, Clone)]
pub struct HighPass {
    stages: [Biquad; 2],
}

impl HighPass {
    pub fn new(rate: f64, cutoff: f64) -> Self {
        HighPass {
            stages: [
                Biquad::highpass(rate, cutoff, 0.541_196_1),
                Biquad::highpass(rate, cutoff, 1.306_563),
            ],
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for s in &mut self.stages {
            s.process(buf);
        }
    }
}

/// Automatic gain control for recorded audio: a speech-level normalizer.
///
/// NBFM transmitters differ a lot in deviation: a handheld's voice can sit
/// 15-20 dB below a repeater's, and one repeater recording often holds
/// several people at different levels. This measures the audio in 20 ms
/// frames, keeps only the frames that are speech (well above the quietest
/// recent frames, which are the channel's own noise), and averages their
/// level over about half a second of speech. The gain steers that speech
/// level to `target` RMS (-18 dBFS) and moves smoothly (0.2 s), so it
/// follows a new speaker within a second without riding individual words.
///
/// Pauses, the repeater's carrier between overs and anything muted by the
/// squelch never change the gain, so silence is not pumped up. The gain is
/// set straight away from the first speech frame of a transmission and is
/// held between `min` and `max`. A soft limiter catches peaks.
#[derive(Debug, Clone)]
pub struct Agc {
    gain: f32,
    /// Gain the smoothing heads for.
    want: f32,
    /// Speech level (RMS, linear), or 0 before the first speech frame.
    level: f32,
    /// Level of the quietest recent frames (the noise between words).
    floor: f32,
    target: f32,
    min: f32,
    max: f32,
    /// Samples per frame, and the current frame's sum of squares and count.
    frame: usize,
    acc: f64,
    n: usize,
    /// Per-frame smoothing of the speech level (in dB terms), per-frame
    /// rise of the noise floor, and per-sample gain smoothing.
    level_k: f32,
    floor_rise: f32,
    gain_k: f32,
    limit_k: f32,
    /// Frames still to skip after the squelch opens: the first ones hold
    /// the unmute ramp and filter settling, which would read too quiet.
    skip: u32,
}

impl Agc {
    /// Speech RMS lands here (-20 dBFS).
    pub const TARGET: f32 = 0.1;
    /// Most gain, relative to full deviation reading 1.0 (30 dB over that):
    /// enough to bring a light-deviation handheld (~250 Hz rms) up to the
    /// target.
    pub const MAX_GAIN: f32 = 32.0;
    pub const MIN_GAIN: f32 = 0.25;
    /// Gain until the first speech frame is measured (~60 ms): the least,
    /// so a transmission never starts louder than it goes on.
    pub const START_GAIN: f32 = Self::MIN_GAIN;
    /// A frame is speech when it is this far over the noise floor (12 dB).
    const SPEECH_OVER_FLOOR: f32 = 2.8;
    const SKIP_FRAMES: u32 = 2;

    pub fn new(rate: f64) -> Self {
        let frame = ((0.02 * rate).round() as usize).max(1);
        let frames_per_s = rate / frame as f64;
        Agc {
            gain: Self::START_GAIN,
            want: Self::START_GAIN,
            level: 0.0,
            floor: 0.0,
            target: Self::TARGET,
            min: Self::MIN_GAIN,
            max: Self::MAX_GAIN,
            frame,
            acc: 0.0,
            n: 0,
            level_k: (1.0 - (-1.0 / (0.3 * frames_per_s)).exp()) as f32,
            // 3 dB a second.
            floor_rise: 10f32.powf(3.0 / 20.0 / frames_per_s as f32),
            gain_k: (1.0 - (-1.0 / (0.1 * rate)).exp()) as f32,
            limit_k: (1.0 - (-1.0 / (0.005 * rate)).exp()) as f32,
            skip: Self::SKIP_FRAMES,
        }
    }

    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Apply the gain to `buf` in place. With `adapt` false (squelch
    /// muted) the gain heads for where it was going but nothing new is
    /// measured.
    pub fn process(&mut self, buf: &mut [f32], adapt: bool) {
        for v in buf {
            if adapt {
                self.acc += (*v as f64) * (*v as f64);
                self.n += 1;
                if self.n == self.frame {
                    let rms = (self.acc / self.n as f64).sqrt() as f32;
                    if self.skip > 0 {
                        self.skip -= 1;
                    } else {
                        self.end_frame(rms);
                    }
                    self.acc = 0.0;
                    self.n = 0;
                }
            } else {
                self.acc = 0.0;
                self.n = 0;
                self.skip = Self::SKIP_FRAMES;
            }
            // Smoothed in log terms, so the gain moves at the same rate in dB
            // whichever way it goes.
            if self.gain != self.want {
                self.gain *= (self.want / self.gain).powf(self.gain_k);
            }
            // A peak that would reach the limiter pulls the gain down within
            // ~5 ms, and the aim with it, so it does not climb straight back.
            let out = (*v * self.gain).abs();
            if out > SOFT_KNEE {
                self.gain *= (SOFT_KNEE / out).powf(self.limit_k);
                self.want = self.want.min(self.gain);
            }
            *v = soft_limit(*v * self.gain);
        }
    }

    fn end_frame(&mut self, rms: f32) {
        if rms <= 0.0 {
            return;
        }
        // The noise floor follows the quietest frames down at once and
        // creeps up otherwise. The first frame puts it 24 dB down, taking
        // the first sound to be speech.
        if self.floor == 0.0 {
            self.floor = rms / 16.0;
        } else if rms < self.floor {
            self.floor = rms;
        } else {
            self.floor *= self.floor_rise;
        }
        if rms < self.floor * Self::SPEECH_OVER_FLOOR {
            return;
        }
        let first = self.level == 0.0;
        self.level = if first {
            rms
        } else {
            // Averaged in power: loud syllables weigh more than quiet ones,
            // so the peaks of speech, not its pauses, set the level.
            (self.level * self.level + self.level_k * (rms * rms - self.level * self.level)).sqrt()
        };
        self.want = (self.target / self.level).clamp(self.min, self.max);
        if first {
            self.gain = self.want;
        }
    }
}

const SOFT_KNEE: f32 = 0.8;

/// Linear up to 0.8, then a tanh knee that never exceeds 1.0.
pub fn soft_limit(x: f32) -> f32 {
    const KNEE: f32 = SOFT_KNEE;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        let y = KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh();
        y.copysign(x)
    }
}

/// Polar FM discriminator: the phase step between successive samples,
/// scaled so a deviation of `deviation_hz` gives an output of 1.0.
#[derive(Debug, Clone)]
pub struct FmDiscriminator {
    prev: Complex32,
    gain: f32,
}

impl FmDiscriminator {
    pub fn new(rate: f64, deviation_hz: f64) -> Self {
        FmDiscriminator {
            prev: Complex32::new(1.0, 0.0),
            gain: (rate / (2.0 * PI * deviation_hz)) as f32,
        }
    }

    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<f32>) {
        let mut prev = self.prev;
        out.reserve(input.len());
        for &x in input {
            let d = x * prev.conj();
            out.push(d.im.atan2(d.re) * self.gain);
            prev = x;
        }
        self.prev = prev;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: f64, f: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * f * i as f64 / rate).sin() as f32)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn lowpass_has_unity_dc_and_rejects_stopband() {
        let h = lowpass(0.1, kaiser_taps(0.05, 60.0), 60.0);
        assert!((h.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        // Response at 0.2 fs (well into the stopband).
        let (re, im) = h
            .iter()
            .enumerate()
            .fold((0.0f64, 0.0f64), |(r, i), (k, &v)| {
                let w = 2.0 * PI * 0.2 * k as f64;
                (r + v as f64 * w.cos(), i - v as f64 * w.sin())
            });
        let mag_db = 10.0 * (re * re + im * im).log10();
        assert!(mag_db < -55.0, "{mag_db}");
    }

    #[test]
    fn ratio_is_reduced() {
        assert_eq!(rational_ratio(39062.5, 8000.0), (128, 625));
        assert_eq!(rational_ratio(31250.0, 8000.0), (32, 125));
        assert_eq!(rational_ratio(48000.0, 8000.0), (1, 6));
    }

    #[test]
    fn resampler_preserves_tone_and_length_across_chunks() {
        let fin = 39062.5;
        let x = tone(fin, 1000.0, 39062);
        let mut whole = Resampler::new(fin, 8000.0, 3400.0, 1000.0);
        let mut a = Vec::new();
        whole.process(&x, &mut a);
        let mut chunked = Resampler::new(fin, 8000.0, 3400.0, 1000.0);
        let mut b = Vec::new();
        for c in x.chunks(777) {
            chunked.process(c, &mut b);
        }
        assert_eq!(a.len(), b.len());
        assert!((a.len() as i64 - 8000).abs() <= 1, "{}", a.len());
        for (u, v) in a.iter().zip(&b) {
            assert!((u - v).abs() < 1e-5);
        }
        let steady = &a[400..7600];
        assert!((rms(steady) - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01);
        // Check the frequency by counting zero crossings over 0.9 s.
        let zc = steady
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        assert!((zc as i64 - 900).abs() <= 2, "{zc}");
    }

    #[test]
    fn resampler_rejects_alias() {
        // 6 kHz tone at 39 kHz input must not appear at 8 kHz output (it
        // would alias to 2 kHz).
        let fin = 39062.5;
        let x = tone(fin, 6000.0, 39062);
        let mut r = Resampler::new(fin, 8000.0, 3400.0, 1000.0);
        let mut y = Vec::new();
        r.process(&x, &mut y);
        assert!(rms(&y[400..7600]) < 0.003, "{}", rms(&y[400..]));
    }

    #[test]
    fn deemphasis_slopes_down() {
        let rate = 8000.0;
        let mut lo = Deemphasis::new(rate, 530e-6);
        let mut hi = Deemphasis::new(rate, 530e-6);
        let mut a = tone(rate, 300.0, 8000);
        let mut b = tone(rate, 2400.0, 8000);
        lo.process(&mut a);
        hi.process(&mut b);
        let drop = 20.0 * (rms(&b[1000..]) / rms(&a[1000..])).log10();
        // 3 octaves above ~300 Hz: roughly -15 to -18 dB.
        assert!(drop < -13.0 && drop > -20.0, "{drop}");
    }

    #[test]
    fn highpass_blocks_ctcss() {
        let rate = 8000.0;
        let mut f = HighPass::new(rate, 300.0);
        let mut x = tone(rate, 100.0, 8000);
        f.process(&mut x);
        assert!(rms(&x[2000..]) < 0.1 * std::f32::consts::FRAC_1_SQRT_2);
        let mut f = HighPass::new(rate, 300.0);
        let mut y = tone(rate, 1000.0, 8000);
        f.process(&mut y);
        assert!(rms(&y[2000..]) > 0.65);
    }

    #[test]
    fn agc_levels_quiet_and_loud_audio_alike() {
        let rate = 8000.0;
        let mut levels = Vec::new();
        for amp in [0.02f32, 0.1, 0.6] {
            let mut agc = Agc::new(rate);
            let mut x: Vec<f32> = tone(rate, 1000.0, 24000).iter().map(|v| v * amp).collect();
            agc.process(&mut x, true);
            // Settled over the last second.
            levels.push(rms(&x[16000..]));
            assert!(x.iter().all(|v| v.abs() <= 1.0));
        }
        // Inputs 30 dB apart come out within 1 dB of the target.
        for p in &levels {
            let db = 20.0 * (p / Agc::TARGET).log10();
            assert!(db.abs() < 1.0, "{levels:?}");
        }
    }

    #[test]
    fn agc_gain_is_bounded_and_holds_while_muted() {
        let rate = 8000.0;
        let mut agc = Agc::new(rate);
        // Near-silence cannot drive the gain past the maximum.
        let mut x: Vec<f32> = tone(rate, 1000.0, 40000).iter().map(|v| v * 1e-4).collect();
        agc.process(&mut x, true);
        assert!((agc.gain() - Agc::MAX_GAIN).abs() < 0.1, "{}", agc.gain());
        // Muted: the gain stays where it was, however long the silence.
        let mut agc = Agc::new(rate);
        let mut loud: Vec<f32> = tone(rate, 1000.0, 8000).iter().map(|v| v * 0.2).collect();
        agc.process(&mut loud, true);
        let g = agc.gain();
        let mut silence = vec![0.0f32; 40000];
        agc.process(&mut silence, false);
        assert_eq!(agc.gain(), g);
        assert!(silence.iter().all(|&v| v == 0.0));
    }

    /// "Speech": 1 kHz bursts of `amp` (0.3 s on, 0.1 s off) over noise
    /// of `noise` RMS, for `secs`.
    fn talk(rate: f64, secs: f64, amp: f32, noise: f32, seed: &mut u32) -> Vec<f32> {
        let n = (secs * rate) as usize;
        (0..n)
            .map(|i| {
                *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let r = ((*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 3.46 * noise;
                let t = i as f64 / rate;
                let on = t % 0.4 < 0.3;
                r + if on {
                    amp * (2.0 * PI * 1000.0 * t).sin() as f32
                } else {
                    0.0
                }
            })
            .collect()
    }

    #[test]
    fn agc_follows_a_new_speaker_and_ignores_pauses() {
        // A repeater recording: one speaker, a 1.5 s pause on the carrier,
        // then a second speaker 20 dB quieter.
        let rate = 8000.0;
        let mut seed = 7;
        let mut x = talk(rate, 4.0, 0.3, 0.003, &mut seed);
        let pause_at = x.len();
        x.extend(talk(rate, 1.5, 0.0, 0.003, &mut seed));
        let second_at = x.len();
        x.extend(talk(rate, 4.0, 0.03, 0.003, &mut seed));
        let mut agc = Agc::new(rate);
        let mut gains = Vec::new();
        for c in x.chunks_mut(160) {
            agc.process(c, true);
            gains.push(agc.gain());
        }
        let db = |a: &[f32]| 20.0 * (rms(a) / Agc::TARGET).log10();
        // Each speaker's last 2 s within 1.5 dB of the target (the RMS
        // includes the short gaps between bursts, ~1.2 dB below a burst).
        let first = db(&x[pause_at - 16000..pause_at]);
        let second = db(&x[x.len() - 16000..]);
        assert!((first + 1.2).abs() < 1.5, "first {first}");
        assert!((second + 1.2).abs() < 1.5, "second {second}");
        // The pause does not wind the gain up.
        let g_pause_start = gains[pause_at / 160];
        let g_pause_end = gains[second_at / 160 - 1];
        let drift = 20.0 * (g_pause_end / g_pause_start).log10();
        assert!(drift.abs() < 1.0, "gain moved {drift} dB in the pause");
        // Within a speaker the gain holds steady (no riding each word).
        let steady = &gains[(pause_at - 16000) / 160..pause_at / 160];
        let (lo, hi) = steady
            .iter()
            .fold((f32::MAX, 0.0f32), |(l, h), &g| (l.min(g), h.max(g)));
        assert!(20.0 * (hi / lo).log10() < 1.0, "gain range {lo}..{hi}");
    }

    #[test]
    fn soft_limit_is_bounded_and_monotonic() {
        let mut last = -2.0f32;
        for i in -400..=400 {
            let x = i as f32 / 100.0;
            let y = soft_limit(x);
            assert!(y.abs() <= 1.0 && y >= last, "{x} -> {y}");
            last = y;
        }
        assert_eq!(soft_limit(0.5), 0.5);
    }

    #[test]
    fn discriminator_scales_to_deviation() {
        let rate = 39062.5;
        let dev = 5000.0;
        let x: Vec<Complex32> = (0..1000)
            .map(|i| {
                let ph = 2.0 * PI * dev * i as f64 / rate;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let mut d = FmDiscriminator::new(rate, dev);
        let mut out = Vec::new();
        d.process(&x, &mut out);
        for v in &out[1..] {
            assert!((v - 1.0).abs() < 1e-3);
        }
    }
}
