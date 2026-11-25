//! `scanner calibrate`: measure the band at each gain step and suggest gain
//! and detection thresholds (PLAN.md Phase 2).
//!
//! At each step [`StepMeter`] records the ADC level and clipping, the noise
//! floor, how many narrow peaks stand out, and the SNR idle slots read on
//! noise alone. [`suggest`] then picks a gain:
//!
//! - Steps that clip, that the radio flags as overloaded, or that grow
//!   peaks the lower steps didn't have (intermod: its products rise 3 dB per
//!   dB of gain, so they surface suddenly) are out.
//! - Among the rest, the floor's rise per dB of gain shows which noise
//!   dominates. While the radio's own noise does, the floor barely moves and
//!   more gain still helps weak signals. Once the antenna's noise does, the
//!   floor rises dB for dB and more gain only uses up headroom. The pick is
//!   the first step where the floor tracks the gain, plus `MARGIN_DB` so the
//!   antenna noise sits clearly above the radio's.
//! - Thresholds come from what idle slots read at that gain: the open
//!   threshold clears the 99.9th percentile of noise-only SNR.
//!
//! Nothing here assumes a particular antenna or site: a quiet rural site
//! with a good antenna ends up at more gain than a noisy one with a rubber
//! duck, because the floor starts tracking the gain later.

use crate::channelizer::{Analyzer, BankParams};
use crate::config::Slot;
use crate::detector::{Detector, DetectorParams, SlotState};
use crate::psd::{Welch, find_peaks, noise_floor_db};
use num_complex::Complex32;
use serde::Serialize;

/// RSP1A LNA attenuation (dB) by LNA state, 60-420 MHz (SDRplay API
/// specification, gain reduction tables).
pub const RSP1A_LNA_DB: [f32; 10] = [0.0, 6.0, 12.0, 18.0, 20.0, 26.0, 32.0, 38.0, 57.0, 62.0];

/// Gain relative to the most the RSP1A can give (0 dB at LNA 0, GR 20).
pub fn rsp1a_gain_db(lna: u8, gr: u8) -> f32 {
    let lna_db = RSP1A_LNA_DB
        .get(lna as usize)
        .copied()
        .unwrap_or(RSP1A_LNA_DB[9]);
    -(lna_db + (gr as f32 - 20.0))
}

/// What one gain step measured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    pub lna: u8,
    pub gr: u8,
    /// Relative gain (dB, 0 = the most).
    pub gain_db: f32,
    /// Mean power (dBFS) and the largest |I| or |Q| (dBFS).
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
    /// Samples at full scale, per million.
    pub clip_ppm: f64,
    /// The radio reported an ADC overload during the step.
    pub overload: bool,
    /// Noise floor (dB per FFT bin, 20th percentile).
    pub floor_db: f32,
    /// Narrow peaks over floor + 10 dB.
    pub peaks: usize,
    /// SNR idle slots read: median and 99.9th percentile (dB).
    pub idle_snr_median_db: f32,
    pub idle_snr_p999_db: f32,
}

/// Samples at or above this magnitude count as clipped.
const CLIP: f32 = 0.98;

/// Measures one gain step from samples fed in any chunk size.
pub struct StepMeter {
    sample_rate: f64,
    detector: DetectorParams,
    sum: f64,
    n: u64,
    peak: f32,
    clipped: u64,
    welch: Welch,
    analyzer: Analyzer,
    det: Detector,
    block_secs: f64,
    blocks: usize,
    idle: Vec<f32>,
    events: Vec<crate::detector::Event>,
}

impl StepMeter {
    pub fn new(
        sample_rate: f64,
        center_freq: f64,
        plan: &[Slot],
        detector: &DetectorParams,
    ) -> Self {
        let params = BankParams::for_rate(sample_rate);
        StepMeter {
            sample_rate,
            detector: detector.clone(),
            sum: 0.0,
            n: 0,
            peak: 0.0,
            clipped: 0,
            welch: Welch::new(8192),
            analyzer: Analyzer::new(params),
            det: Detector::new(detector.clone(), params, center_freq, plan),
            block_secs: params.block_secs(),
            blocks: 0,
            idle: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn push(&mut self, samples: &[Complex32]) {
        for s in samples {
            self.sum += s.norm_sqr() as f64;
            let m = s.re.abs().max(s.im.abs());
            self.peak = self.peak.max(m);
            self.clipped += (m >= CLIP) as u64;
        }
        self.n += samples.len() as u64;
        self.welch.push(samples);
        let mut blocks = Vec::new();
        self.analyzer.push(samples, || None, |b| blocks.push(b));
        // Skip the first half second while the floor estimate settles.
        let settle = (0.5 / self.block_secs) as usize;
        for b in blocks {
            self.events.clear();
            self.det.update(&b, &mut self.events);
            self.blocks += 1;
            if self.blocks <= settle {
                continue;
            }
            for i in 0..self.det.slots().len() {
                if self.det.state(i) == SlotState::Idle {
                    let v = self.det.snr_db(i);
                    if v.is_finite() {
                        self.idle.push(v);
                    }
                }
            }
        }
    }

    pub fn finish(mut self, lna: u8, gr: u8, gain_db: f32, overload: bool) -> Step {
        let spec = self.welch.spectrum_db();
        // Only the middle of the band: anti-alias roll-off and the DC
        // residual aren't the noise floor. `spec` is in natural FFT order.
        let usable = (spec.len() as f64 * self.detector.usable / 2.0) as usize;
        let notch = ((self.detector.dc_notch_hz / (self.sample_rate / spec.len() as f64)).ceil()
            as usize)
            .clamp(2, usable);
        let mut mid: Vec<f32> = Vec::with_capacity(spec.len());
        mid.extend_from_slice(&spec[spec.len() - usable..spec.len() - notch]);
        mid.extend_from_slice(&spec[notch..usable]);
        let (floor_db, peaks) = if mid.is_empty() {
            (f32::NAN, 0)
        } else {
            (
                noise_floor_db(&mid, 0.2),
                find_peaks(&mid, self.sample_rate, 10.0, 5e3).len(),
            )
        };
        self.idle.sort_by(f32::total_cmp);
        let idle = &self.idle;
        let pct = |p: f64| -> f32 {
            if idle.is_empty() {
                f32::NAN
            } else {
                idle[((idle.len() - 1) as f64 * p).round() as usize]
            }
        };
        let n = self.n.max(1);
        Step {
            lna,
            gr,
            gain_db,
            rms_dbfs: (10.0 * (self.sum / n as f64).max(1e-20).log10()) as f32,
            peak_dbfs: 20.0 * self.peak.max(1e-10).log10(),
            clip_ppm: self.clipped as f64 * 1e6 / n as f64,
            overload,
            floor_db,
            peaks,
            idle_snr_median_db: pct(0.5),
            idle_snr_p999_db: pct(0.999),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    pub lna: u8,
    pub gr: u8,
    pub open_db: f32,
    pub close_db: f32,
    /// Why, in a sentence or two.
    pub reason: String,
}

/// Most clipping allowed (per million samples).
pub const MAX_CLIP_PPM: f64 = 1.0;
/// Headroom kept between the mean level and full scale (dB): room for
/// a strong nearby station.
pub const MIN_HEADROOM_DB: f32 = 30.0;
/// The floor "tracks" the gain when it rises at least this many dB per dB.
pub const TRACKING_SLOPE: f32 = 0.6;
/// Gain kept above the point where the floor starts tracking.
pub const MARGIN_DB: f32 = 5.0;

/// Pick a gain and thresholds from a sweep (any order).
pub fn suggest(steps: &[Step]) -> Option<Suggestion> {
    let mut s: Vec<&Step> = steps.iter().filter(|s| s.floor_db.is_finite()).collect();
    s.sort_by(|a, b| a.gain_db.total_cmp(&b.gain_db));
    if s.len() < 2 {
        return None;
    }
    // Peaks at the quieter half of the sweep: real signals and birdies.
    // A step with many more is making its own (intermod).
    let base_peaks = {
        let mut p: Vec<usize> = s[..s.len().div_ceil(2)].iter().map(|x| x.peaks).collect();
        p.sort_unstable();
        p[p.len() / 2]
    };
    let max_peaks = base_peaks + (base_peaks / 2).max(3);
    let clean = |x: &Step| {
        x.clip_ppm <= MAX_CLIP_PPM
            && !x.overload
            && x.rms_dbfs <= -MIN_HEADROOM_DB
            && x.peaks <= max_peaks
    };
    // Highest gain before anything goes wrong: the ceiling.
    let ceiling = s.iter().take_while(|x| clean(x)).last().copied()?;
    // Where the floor starts tracking the gain.
    let mut knee = None;
    for w in s.windows(2) {
        let dg = w[1].gain_db - w[0].gain_db;
        if dg <= 0.5 {
            continue;
        }
        if (w[1].floor_db - w[0].floor_db) / dg >= TRACKING_SLOPE {
            knee = Some(w[0].gain_db);
            break;
        }
    }
    let (want, why) = match knee {
        Some(k) => (
            k + MARGIN_DB,
            format!(
                "the noise floor starts rising with the gain at {k:.0} dB (antenna noise taking over), \
                 so this is {MARGIN_DB:.0} dB above that"
            ),
        ),
        None => (
            ceiling.gain_db,
            "the floor never rose with the gain (the radio's own noise dominates throughout), \
             so this is the most gain that stays clean"
                .to_string(),
        ),
    };
    let target = want.min(ceiling.gain_db);
    // The clean step nearest the target, preferring more LNA gain (lower
    // state) for the same total: a lower noise figure.
    let pick = s
        .iter()
        .filter(|x| clean(x) && x.gain_db <= ceiling.gain_db)
        .min_by(|a, b| {
            let da = (a.gain_db - target).abs();
            let db = (b.gain_db - target).abs();
            da.total_cmp(&db).then(a.lna.cmp(&b.lna))
        })?;
    let mut reason = format!("LNA {} / GR {}: {why}", pick.lna, pick.gr);
    if target < want {
        reason.push_str(&format!(
            "; held to {:.0} dB, the most before clipping, overload or intermod",
            ceiling.gain_db
        ));
    }
    let p999 = if pick.idle_snr_p999_db.is_finite() {
        pick.idle_snr_p999_db
    } else {
        3.0
    };
    // Noise alone should hardly ever reach the open threshold.
    let open_db = (p999 + 2.0).max(4.0).ceil();
    let close_db = (open_db - 3.0).max(2.0);
    reason.push_str(&format!(
        ". Idle slots reach {p999:.1} dB SNR on noise 0.1 % of the time, so open at {open_db:.0} dB."
    ));
    Some(Suggestion {
        lna: pick.lna,
        gr: pick.gr,
        open_db,
        close_db,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(lna: u8, gr: u8, floor: f32, rms: f32, peaks: usize, clip: f64) -> Step {
        Step {
            lna,
            gr,
            gain_db: rsp1a_gain_db(lna, gr),
            rms_dbfs: rms,
            peak_dbfs: rms + 12.0,
            clip_ppm: clip,
            overload: false,
            floor_db: floor,
            peaks,
            idle_snr_median_db: 0.5,
            idle_snr_p999_db: 3.4,
        }
    }

    /// A sweep with the radio's noise at `rx` dB and the antenna's at
    /// `ant` dB below it at the most gain; strong signals clip past
    /// `clip_at` dB of gain and intermod shows past `im_at`.
    fn sweep(ant_db: f32, clip_at: f32, im_at: f32) -> Vec<Step> {
        let mut v = Vec::new();
        for lna in 0..10u8 {
            for gr in [59u8, 50, 40, 30, 20] {
                let g = rsp1a_gain_db(lna, gr);
                // Floor: radio noise fixed at -100 (in dBFS-ish units);
                // antenna noise -100 + ant + g (rises with gain).
                let p = |db: f32| 10f32.powf(db / 10.0);
                let floor = 10.0 * (p(-100.0) + p(-100.0 + ant_db + g)).log10();
                let rms = -60.0 + g;
                let peaks = if g > im_at { 12 } else { 4 };
                let clip = if g > clip_at { 50.0 } else { 0.0 };
                v.push(step(lna, gr, floor, rms, peaks, clip));
            }
        }
        v
    }

    #[test]
    fn picks_gain_past_the_knee() {
        // Antenna noise reaches the radio's at 30 dB below the most gain.
        let s = suggest(&sweep(30.0, 100.0, 100.0)).unwrap();
        let g = rsp1a_gain_db(s.lna, s.gr);
        assert!(g > -32.0 && g < -15.0, "{s:?} at {g}");
        assert_eq!((s.open_db, s.close_db), (6.0, 3.0), "{s:?}");
        // A quieter antenna (or noisier site) shifts the pick with it.
        let quiet = suggest(&sweep(10.0, 100.0, 100.0)).unwrap();
        assert!(rsp1a_gain_db(quiet.lna, quiet.gr) > g, "{quiet:?}");
    }

    #[test]
    fn stays_under_clipping_and_intermod() {
        let s = suggest(&sweep(10.0, -20.0, 100.0)).unwrap();
        assert!(rsp1a_gain_db(s.lna, s.gr) <= -20.0, "{s:?}");
        assert!(s.reason.contains("held"), "{}", s.reason);
        let s = suggest(&sweep(10.0, 100.0, -25.0)).unwrap();
        assert!(rsp1a_gain_db(s.lna, s.gr) <= -25.0, "{s:?}");
    }

    #[test]
    fn measures_level_floor_and_clipping() {
        use crate::config::{Mode, SlotOrigin};
        let fs = 2e6;
        let mut seed = 1u64;
        let mut noise = |a: f32| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let u = |x: u64| (x as f32 / (1u64 << 24) as f32 - 0.5) * 2.0 * a;
            Complex32::new(u(seed >> 40), u(seed & 0xff_ffff))
        };
        let quiet: Vec<Complex32> = (0..2_000_000).map(|_| noise(1e-3)).collect();
        let loud: Vec<Complex32> = (0..2_000_000)
            .map(|i| noise(1e-2) + Complex32::from_polar(1.2, i as f32 * 0.3))
            .collect();
        let plan: Vec<Slot> = (-8..=8)
            .map(|k| Slot {
                freq: 146e6 + k as f64 * 25e3,
                bandwidth: 16e3,
                deviation: 5e3,
                mode: Mode::Nbfm,
                priority: false,
                record: true,
                name: None,
                origin: SlotOrigin::Grid,
                pair: None,
                sites: Vec::new(),
            })
            .collect();
        let d = DetectorParams::default();
        let measure = |x: &[Complex32], lna, gr, g| {
            let mut m = StepMeter::new(fs, 146e6, &plan, &d);
            for c in x.chunks(30_000) {
                m.push(c);
            }
            m.finish(lna, gr, g, false)
        };
        let a = measure(&quiet, 3, 40, -38.0);
        let b = measure(&loud, 0, 20, 0.0);
        assert!(a.clip_ppm == 0.0 && b.clip_ppm > 1e5, "{a:?} {b:?}");
        assert!((a.rms_dbfs - -61.8).abs() < 0.5, "{}", a.rms_dbfs);
        assert!(b.floor_db > a.floor_db + 15.0, "{a:?} {b:?}");
        // Noise alone: idle slots read a few dB at most.
        assert!(
            a.idle_snr_p999_db > 0.0 && a.idle_snr_p999_db < 6.0,
            "{a:?}"
        );
    }
}
