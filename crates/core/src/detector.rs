//! Slot detector and per-slot state machine, PLAN.md 3.3.
//!
//! Every forward-FFT block updates a smoothed power spectrum. Each slot of the
//! channel plan owns a window of its bandwidth; the integrated power in that
//! window, relative to the local noise floor times the window width, is the
//! slot's SNR. A per-slot state machine (idle → attack → active → hang → idle)
//! turns that into transmission open/close events.

use crate::channelizer::{BankParams, Block};
use crate::config::{Slot, SlotOrigin};
use num_complex::Complex32;

#[derive(Debug, Clone, PartialEq)]
pub struct DetectorParams {
    /// SNR (dB, slot power over noise in the slot bandwidth) to open.
    pub open_db: f32,
    /// SNR to stay open (hysteresis).
    pub close_db: f32,
    /// Time above `open_db` before a slot opens.
    pub attack: f64,
    /// Time below `close_db` before an active slot closes.
    pub hang: f64,
    /// Audio before detection to include.
    pub preroll: f64,
    /// Power spectrum smoothing for the noise floor estimate (EMA time
    /// constant, seconds).
    pub smoothing: f64,
    /// Noise floor smoothing (EMA time constant, seconds).
    pub floor_smoothing: f64,
    /// Number of noise-floor segments across the usable band.
    pub floor_segments: usize,
    /// Fraction of the sample rate that is usable (anti-alias filter edges).
    pub usable: f64,
    /// Most slots active at once (priority slots may exceed it).
    pub max_active: usize,
    /// Interpolate over the bins around 0 Hz (DC spike of zero-IF tuners).
    pub dc_notch_hz: f64,
    /// Detection window for fallback grid slots (usually the grid spacing),
    /// so the grid tiles the band with no gaps for off-raster signals to
    /// fall through. 0 uses the slot bandwidth.
    pub grid_window_hz: f64,
}

impl Default for DetectorParams {
    fn default() -> Self {
        DetectorParams {
            open_db: 6.0,
            close_db: 3.0,
            attack: 0.06,
            hang: 1.5,
            preroll: 0.2,
            smoothing: 0.1,
            floor_smoothing: 2.0,
            floor_segments: 64,
            usable: 0.9,
            max_active: 32,
            dc_notch_hz: 500.0,
            grid_window_hz: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Idle,
    /// Above the open threshold since this block.
    Attack {
        since: u64,
    },
    /// Open; transmission started at this block (pre-roll included).
    Active {
        start: u64,
    },
    /// Below the close threshold since `since`.
    Hang {
        start: u64,
        since: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A slot opened. `start_block` includes pre-roll; the consumer should
    /// demodulate blocks `start_block..=block` now.
    Open {
        slot: usize,
        start_block: u64,
        block: u64,
    },
    /// A slot closed. `end_block` is the last block the signal was present.
    Close {
        slot: usize,
        start_block: u64,
        end_block: u64,
        peak_snr_db: f32,
        mean_snr_db: f32,
    },
}

/// A slot as the detector sees it.
#[derive(Debug, Clone)]
pub struct WatchedSlot {
    pub slot: Slot,
    /// Offset from the capture centre (Hz).
    pub offset_hz: f64,
    /// Window in shifted-spectrum bin indices (0 = most negative frequency).
    lo: usize,
    hi: usize,
    neighbors: Vec<usize>,
    segment: usize,
}

#[derive(Debug, Clone, Default)]
struct SlotStats {
    snr_db: f32,
    peak: f32,
    sum: f64,
    count: u64,
}

pub struct Detector {
    params: DetectorParams,
    bank: BankParams,
    slots: Vec<WatchedSlot>,
    states: Vec<SlotState>,
    stats: Vec<SlotStats>,
    /// Smoothed power per bin, shifted order.
    psd: Vec<f32>,
    /// Scratch: raw power per bin, shifted order.
    raw: Vec<f32>,
    cumsum: Vec<f64>,
    seg_bounds: Vec<(usize, usize)>,
    seg_floor: Vec<f32>,
    seg_scratch: Vec<f32>,
    alpha: f32,
    floor_alpha: f32,
    blocks_seen: u64,
    attack_blocks: u64,
    hang_blocks: u64,
    preroll_blocks: u64,
    skipped: Vec<String>,
}

/// Median of an exponential-ish smoothed noise PSD sits below its mean; this
/// converts the per-segment median to a mean estimate. Measured by
/// `idle_slots_read_zero_db` for the default smoothing at typical block rates.
const MEDIAN_TO_MEAN: f32 = 1.2;

impl Detector {
    /// Watch the slots of a channel plan that fall inside the capture.
    pub fn new(params: DetectorParams, bank: BankParams, center_freq: f64, plan: &[Slot]) -> Self {
        let n = bank.n;
        let bin_hz = bank.bin_hz();
        let half_usable = bank.sample_rate * params.usable / 2.0;
        let mut slots = Vec::new();
        let mut skipped = Vec::new();
        for s in plan {
            let off = s.freq - center_freq;
            if off.abs() + s.bandwidth / 2.0 > half_usable {
                skipped.push(format!("{:.4} MHz outside the capture", s.freq / 1e6));
                continue;
            }
            let width = if s.origin == SlotOrigin::Grid {
                s.bandwidth.max(params.grid_window_hz)
            } else {
                s.bandwidth
            };
            let lo = ((off - width / 2.0) / bin_hz).round() as i64 + (n / 2) as i64;
            let hi = ((off + width / 2.0) / bin_hz).round() as i64 + (n / 2) as i64;
            slots.push(WatchedSlot {
                slot: s.clone(),
                offset_hz: off,
                lo: lo as usize,
                hi: (hi as usize).max(lo as usize + 1),
                neighbors: Vec::new(),
                segment: 0,
            });
        }
        // Grid windows (one spacing wide) tile the band between themselves.
        // Next to an explicit slot, whose window is only its bandwidth, a gap
        // can open; grow the grid window to meet the explicit window's edge
        // (by at most one grid window) so no signal falls between them.
        slots.sort_by(|a, b| a.offset_hz.total_cmp(&b.offset_hz));
        let cap = (params.grid_window_hz / bin_hz).round() as usize;
        if cap > 0 {
            let snapshot: Vec<(usize, usize, bool)> = slots
                .iter()
                .map(|s| (s.lo, s.hi, s.slot.origin == SlotOrigin::Grid))
                .collect();
            for i in 0..slots.len() {
                let (lo, hi, grid) = snapshot[i];
                if !grid {
                    continue;
                }
                if i > 0 && !snapshot[i - 1].2 && snapshot[i - 1].1 < lo {
                    slots[i].lo = snapshot[i - 1].1.max(lo.saturating_sub(cap));
                }
                if i + 1 < slots.len() && !snapshot[i + 1].2 && snapshot[i + 1].0 > hi {
                    slots[i].hi = snapshot[i + 1].0.min(hi + cap);
                }
            }
        }
        // Overlapping windows compete: only the strongest may open.
        for i in 0..slots.len() {
            for j in 0..slots.len() {
                if i != j && slots[i].lo < slots[j].hi && slots[j].lo < slots[i].hi {
                    slots[i].neighbors.push(j);
                }
            }
        }
        // Noise floor segments over the usable band.
        let first = ((n as f64 / 2.0) - half_usable / bin_hz).max(0.0) as usize;
        let last = ((n as f64 / 2.0) + half_usable / bin_hz).min(n as f64) as usize;
        let segs = params.floor_segments.max(1);
        let seg_len = (last - first).div_ceil(segs);
        let seg_bounds: Vec<(usize, usize)> = (0..segs)
            .map(|k| (first + k * seg_len, (first + (k + 1) * seg_len).min(last)))
            .filter(|(a, b)| b > a)
            .collect();
        for s in &mut slots {
            let mid = (s.lo + s.hi) / 2;
            s.segment = seg_bounds
                .iter()
                .position(|&(a, b)| mid >= a && mid < b)
                .unwrap_or(0);
        }
        let block = bank.block_secs();
        let blocks = |secs: f64| (secs / block).ceil() as u64;
        let count = slots.len();
        let nseg = seg_bounds.len();
        Detector {
            alpha: (block / params.smoothing.max(block)).min(1.0) as f32,
            floor_alpha: (block / params.floor_smoothing.max(block)).min(1.0) as f32,
            attack_blocks: blocks(params.attack).max(1),
            hang_blocks: blocks(params.hang).max(1),
            preroll_blocks: blocks(params.preroll),
            params,
            bank,
            slots,
            states: vec![SlotState::Idle; count],
            stats: vec![SlotStats::default(); count],
            psd: vec![0.0; n],
            raw: vec![0.0; n],
            cumsum: vec![0.0; n + 1],
            seg_bounds,
            seg_floor: vec![0.0; nseg],
            seg_scratch: Vec::new(),
            blocks_seen: 0,
            skipped,
        }
    }

    pub fn slots(&self) -> &[WatchedSlot] {
        &self.slots
    }

    pub fn state(&self, i: usize) -> SlotState {
        self.states[i]
    }

    pub fn snr_db(&self, i: usize) -> f32 {
        self.stats[i].snr_db
    }

    /// Power-weighted centre (offset from the capture centre, Hz) of the
    /// excess power over the noise floor in a slot's window, from the latest
    /// block. Used to tune fallback grid slots onto off-raster signals.
    pub fn centroid_hz(&self, i: usize) -> f64 {
        let s = &self.slots[i];
        let floor = self.seg_floor[s.segment];
        // Look half a channel beyond the window on each side, so a signal
        // straddling the window edge is measured whole.
        let pad = (s.slot.bandwidth / 2.0 / self.bank.bin_hz()) as usize;
        let lo = s.lo.saturating_sub(pad);
        let hi = (s.hi + pad).min(self.raw.len());
        let (mut w, mut acc) = (0.0f64, 0.0f64);
        for k in lo..hi {
            let excess = (self.raw[k] - floor).max(0.0) as f64;
            w += excess;
            acc += excess * k as f64;
        }
        if w <= 0.0 {
            return s.offset_hz;
        }
        let bin = acc / w - (self.bank.n / 2) as f64;
        bin * self.bank.bin_hz()
    }

    /// Whether the excess power in an explicit slot's own window is centred
    /// within a quarter of its bandwidth.
    fn is_centred(&self, i: usize) -> bool {
        let s = &self.slots[i];
        let floor = self.seg_floor[s.segment];
        let (mut w, mut acc) = (0.0f64, 0.0f64);
        for k in s.lo..s.hi {
            let excess = (self.raw[k] - floor).max(0.0) as f64;
            w += excess;
            acc += excess * k as f64;
        }
        if w <= 0.0 {
            return false;
        }
        let off = (acc / w - (self.bank.n / 2) as f64) * self.bank.bin_hz();
        (off - s.offset_hz).abs() < s.slot.bandwidth / 4.0
    }

    /// Plan slots that were not watched, and why.
    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    /// Blocks of pre-roll the consumer must keep.
    pub fn preroll_blocks(&self) -> u64 {
        self.preroll_blocks + self.attack_blocks
    }

    /// Blocks between a transmission's `start_block` and the block where
    /// the signal was first seen (the pre-roll alone).
    pub fn signal_offset_blocks(&self) -> u64 {
        self.preroll_blocks
    }

    pub fn hang_blocks(&self) -> u64 {
        self.hang_blocks
    }

    /// Noise floor (dB per bin, relative to a full-scale tone reading 0 dB)
    /// at each segment.
    pub fn floor_db(&self) -> Vec<f32> {
        let n2 = (self.bank.n as f32).powi(2);
        self.seg_floor
            .iter()
            .map(|&f| 10.0 * (f / n2).max(1e-30).log10())
            .collect()
    }

    /// Update with one block; appends open/close events.
    pub fn update(&mut self, block: &Block, events: &mut Vec<Event>) {
        let n = self.bank.n;
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
        if self.params.dc_notch_hz > 0.0 {
            let w = (self.params.dc_notch_hz / self.bank.bin_hz()).ceil() as usize + 1;
            if h > w + 1 {
                let a = self.raw[h - w - 1];
                let b = self.raw[h + w + 1];
                for k in h - w..=h + w {
                    self.raw[k] = 0.5 * (a + b);
                }
            }
        }
        let alpha = if self.blocks_seen == 0 {
            1.0
        } else {
            self.alpha
        };
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
        // Per-segment median floor (subsampled for speed).
        let falpha = if self.blocks_seen == 0 {
            1.0
        } else {
            self.floor_alpha
        };
        for (i, &(a, b)) in self.seg_bounds.iter().enumerate() {
            self.seg_scratch.clear();
            let stride = ((b - a) / 512).max(1);
            self.seg_scratch
                .extend(self.psd[a..b].iter().step_by(stride).copied());
            let mid = self.seg_scratch.len() / 2;
            let (_, m, _) = self.seg_scratch.select_nth_unstable_by(mid, f32::total_cmp);
            let est = *m * MEDIAN_TO_MEAN;
            // Grow slowly, fall fast: a burst of signals in a segment must not
            // drag the floor up, but a gain drop should be followed quickly.
            let f = &mut self.seg_floor[i];
            if *f == 0.0 || est < *f {
                *f += (falpha * 4.0).min(1.0) * (est - *f);
            } else {
                *f += falpha * (est - *f);
            }
        }
        self.blocks_seen += 1;

        // SNR per slot.
        for (i, s) in self.slots.iter().enumerate() {
            let p = self.cumsum[s.hi] - self.cumsum[s.lo];
            let noise = self.seg_floor[s.segment] as f64 * (s.hi - s.lo) as f64;
            let snr = if noise > 0.0 {
                10.0 * (p / noise).max(1e-12).log10()
            } else {
                0.0
            };
            self.stats[i].snr_db = snr as f32;
        }

        let b = block.index;
        let mut active = self
            .states
            .iter()
            .filter(|s| matches!(s, SlotState::Active { .. } | SlotState::Hang { .. }))
            .count();
        for i in 0..self.slots.len() {
            let snr = self.stats[i].snr_db;
            let dens = |j: usize| self.stats[j].snr_db;
            let wins = self.slots[i].neighbors.iter().all(|&j| {
                // A busy neighbour that is stronger takes this signal, unless
                // this slot already owns it.
                let other_owns = matches!(
                    self.states[j],
                    SlotState::Active { .. } | SlotState::Hang { .. }
                );
                !(dens(j) > snr || (other_owns && dens(j) >= snr - 3.0))
            });
            let open = snr >= self.params.open_db && wins;
            // An explicit slot starts its attack only for a signal centred on
            // it, not for the skirt of a neighbour 10 kHz away. (Grid slots
            // retune to the signal instead.) Checked once, at the start of the
            // attack, because FM speech is lopsided from block to block.
            let start =
                open && (self.slots[i].slot.origin == SlotOrigin::Grid || self.is_centred(i));
            let stay = snr >= self.params.close_db;
            let st = &mut self.stats[i];
            let next = match self.states[i] {
                SlotState::Idle if start => SlotState::Attack { since: b },
                SlotState::Idle => SlotState::Idle,
                SlotState::Attack { since } if open => {
                    if b + 1 - since >= self.attack_blocks
                        && (active < self.params.max_active || self.slots[i].slot.priority)
                    {
                        let start = since.saturating_sub(self.preroll_blocks);
                        events.push(Event::Open {
                            slot: i,
                            start_block: start,
                            block: b,
                        });
                        active += 1;
                        *st = SlotStats {
                            snr_db: snr,
                            peak: snr,
                            sum: 0.0,
                            count: 0,
                        };
                        SlotState::Active { start }
                    } else {
                        SlotState::Attack { since }
                    }
                }
                SlotState::Attack { .. } => SlotState::Idle,
                SlotState::Active { start } if stay => SlotState::Active { start },
                SlotState::Active { start } => SlotState::Hang { start, since: b },
                SlotState::Hang { start, .. } if stay => SlotState::Active { start },
                SlotState::Hang { start, since } => {
                    if b - since >= self.hang_blocks {
                        events.push(Event::Close {
                            slot: i,
                            start_block: start,
                            end_block: since.saturating_sub(1),
                            peak_snr_db: st.peak,
                            mean_snr_db: if st.count > 0 {
                                (st.sum / st.count as f64) as f32
                            } else {
                                snr
                            },
                        });
                        active -= 1;
                        SlotState::Idle
                    } else {
                        SlotState::Hang { start, since }
                    }
                }
            };
            if matches!(next, SlotState::Active { .. }) {
                st.peak = st.peak.max(snr);
                st.sum += snr as f64;
                st.count += 1;
            }
            self.states[i] = next;
        }
    }

    /// End of stream: close everything that is open.
    pub fn finish(&mut self, last_block: u64, events: &mut Vec<Event>) {
        for i in 0..self.slots.len() {
            let (start, end) = match self.states[i] {
                SlotState::Active { start } => (start, last_block),
                SlotState::Hang { start, since } => (start, since.saturating_sub(1)),
                _ => continue,
            };
            let st = &self.stats[i];
            events.push(Event::Close {
                slot: i,
                start_block: start,
                end_block: end,
                peak_snr_db: st.peak,
                mean_snr_db: if st.count > 0 {
                    (st.sum / st.count as f64) as f32
                } else {
                    st.snr_db
                },
            });
            self.states[i] = SlotState::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channelizer::Analyzer;
    use crate::config::Mode;
    use std::f64::consts::PI;

    fn slot(freq: f64, bw: f64) -> Slot {
        Slot {
            freq,
            bandwidth: bw,
            deviation: 5e3,
            mode: Mode::Nbfm,
            priority: false,
            record: true,
            name: None,
            origin: SlotOrigin::Grid,
        }
    }

    /// Deterministic Gaussian noise (xorshift + Box-Muller).
    struct Noise(u64);
    impl Noise {
        fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn complex(&mut self, sigma: f64) -> Complex32 {
            let u1 = self.uniform().max(1e-300);
            let u2 = self.uniform();
            let r = (-2.0 * u1.ln()).sqrt() * sigma / std::f64::consts::SQRT_2;
            Complex32::new(
                (r * (2.0 * PI * u2).cos()) as f32,
                (r * (2.0 * PI * u2).sin()) as f32,
            )
        }
    }

    fn params() -> BankParams {
        BankParams {
            sample_rate: 2_000_000.0,
            n: 1 << 15,
            hop: (1 << 15) / 4 * 3,
        }
    }

    /// FM carrier with a 1 kHz tone, on for [on, off) samples, with the
    /// given SNR in a 16 kHz bandwidth over noise of power `npow`.
    fn scene(secs: f64, sigs: &[(f64, f64, f64, f64)], seed: u64) -> Vec<Complex32> {
        let p = params();
        let fs = p.sample_rate;
        let total = (secs * fs) as usize;
        let npow: f64 = 1e-4;
        let mut noise = Noise(seed);
        let mut x: Vec<Complex32> = (0..total).map(|_| noise.complex(npow.sqrt())).collect();
        for &(off, snr_db, on, off_t) in sigs {
            // Noise power in 16 kHz = npow * 16e3 / fs.
            let amp = (npow * 16e3 / fs * 10f64.powf(snr_db / 10.0)).sqrt();
            let mut ph = 0.0f64;
            for (i, v) in x.iter_mut().enumerate() {
                let t = i as f64 / fs;
                if t >= on && t < off_t {
                    let f = off + 3000.0 * (2.0 * PI * 1000.0 * t).sin();
                    ph = (ph + 2.0 * PI * f / fs).rem_euclid(2.0 * PI);
                    *v += Complex32::from_polar(amp as f32, ph as f32);
                }
            }
        }
        x
    }

    fn run(
        x: &[Complex32],
        plan: &[Slot],
        dp: DetectorParams,
    ) -> (Detector, Vec<Event>, Vec<Vec<f32>>) {
        let p = params();
        let mut det = Detector::new(dp, p, 0.0, plan);
        let mut an = Analyzer::new(p);
        let mut events = Vec::new();
        let mut snrs = Vec::new();
        let mut last = 0;
        an.push(
            x,
            || None,
            |b| {
                det.update(&b, &mut events);
                snrs.push((0..det.slots().len()).map(|i| det.snr_db(i)).collect());
                last = b.index;
            },
        );
        det.finish(last, &mut events);
        (det, events, snrs)
    }

    #[test]
    fn idle_slots_read_zero_db() {
        let x = scene(2.0, &[], 7);
        let plan: Vec<Slot> = (-30..=30).map(|k| slot(k as f64 * 25e3, 16e3)).collect();
        let (_, events, snrs) = run(&x, &plan, DetectorParams::default());
        assert!(events.is_empty(), "{events:?}");
        let tail = &snrs[snrs.len() / 2..];
        let all: Vec<f32> = tail.iter().flatten().copied().collect();
        let mean = all.iter().sum::<f32>() / all.len() as f32;
        let max = all.iter().copied().fold(f32::MIN, f32::max);
        eprintln!("idle mean {mean} max {max}");
        assert!(mean.abs() < 0.5, "mean {mean}");
        assert!(max < 3.0, "max {max}");
    }

    #[test]
    fn detects_transmission_with_correct_timing() {
        let x = scene(4.0, &[(100e3, 15.0, 0.5, 2.0)], 11);
        let plan: Vec<Slot> = (-20..=20).map(|k| slot(k as f64 * 12.5e3, 11e3)).collect();
        let dp = DetectorParams {
            hang: 0.5,
            ..Default::default()
        };
        let (det, events, _) = run(&x, &plan, dp);
        let block = params().block_secs();
        let opens: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::Open { .. }))
            .collect();
        assert_eq!(opens.len(), 1, "{events:?}");
        let Event::Close {
            slot,
            start_block,
            end_block,
            peak_snr_db,
            ..
        } = events
            .iter()
            .find(|e| matches!(e, Event::Close { .. }))
            .unwrap()
            .clone()
        else {
            unreachable!()
        };
        assert_eq!(det.slots()[slot].slot.freq, 100e3);
        let start = start_block as f64 * block;
        let end = (end_block + 1) as f64 * block;
        // Start includes ~0.2 s pre-roll before the 0.5 s key-up.
        assert!(start > 0.2 && start < 0.5, "start {start}");
        assert!((end - 2.0).abs() < 0.08, "end {end}");
        assert!(peak_snr_db > 12.0, "{peak_snr_db}");
    }

    #[test]
    fn strong_signal_does_not_open_neighbours() {
        // 50 dB signal on a 12.5 kHz grid: only its own slot opens.
        let x = scene(2.0, &[(-50e3, 50.0, 0.2, 1.5)], 3);
        let plan: Vec<Slot> = (-20..=20).map(|k| slot(k as f64 * 12.5e3, 11e3)).collect();
        let (det, events, _) = run(&x, &plan, DetectorParams::default());
        let opened: Vec<f64> = events
            .iter()
            .filter_map(|e| match e {
                Event::Open { slot, .. } => Some(det.slots()[*slot].slot.freq),
                _ => None,
            })
            .collect();
        assert_eq!(opened, vec![-50e3]);
    }

    #[test]
    fn short_blip_below_attack_is_ignored() {
        let x = scene(1.5, &[(0.2e6, 20.0, 0.5, 0.52)], 5);
        let plan = vec![slot(0.2e6, 16e3)];
        let (_, events, _) = run(&x, &plan, DetectorParams::default());
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn slots_outside_capture_are_skipped() {
        let p = params();
        let plan = vec![slot(0.0, 16e3), slot(0.95e6, 16e3)];
        let det = Detector::new(DetectorParams::default(), p, 0.0, &plan);
        assert_eq!(det.slots().len(), 1);
        assert_eq!(det.skipped().len(), 1);
    }
}
