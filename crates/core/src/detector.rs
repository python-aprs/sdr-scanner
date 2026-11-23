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
    /// Broadband-burst guard: when at least this many idle slots cross the
    /// open threshold in the same block, it is front-end overload or
    /// intermod (a strong nearby transmitter), not that many new
    /// transmissions. Then only slots within `burst_margin_db` of the
    /// strongest signal (open or not) may open. 0 disables.
    pub burst_slots: usize,
    pub burst_margin_db: f32,
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
            // Zero-IF tuners leave a DC residual; the RSP1A's skirt reaches
            // ~5 kHz either side (live test, Nov 2026).
            dc_notch_hz: 6000.0,
            grid_window_hz: 0.0,
            burst_slots: 6,
            burst_margin_db: 10.0,
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
    /// Per slot, SNR (linear) smoothed over ~50 ms; see [`Detector::squelch_db`].
    smooth: Vec<f32>,
    smooth_alpha: f32,
    /// Smoothed power per bin, shifted order.
    psd: Vec<f32>,
    /// Scratch: raw power per bin, shifted order.
    raw: Vec<f32>,
    cumsum: Vec<f64>,
    seg_bounds: Vec<(usize, usize)>,
    seg_floor: Vec<f32>,
    seg_scratch: Vec<f32>,
    /// Each segment's level this block (from its median bin).
    seg_level: Vec<f32>,
    /// Band-wide noise level this block over the smoothed floor (at least
    /// 1). See [`Detector::common_ratio`].
    common: f32,
    alpha: f32,
    floor_alpha: f32,
    blocks_seen: u64,
    /// Blocks in which the burst guard held slots back.
    burst_blocks: u64,
    /// Blocks in which the noise floor rose across much of the band at once.
    step_blocks: u64,
    /// Scratch: per slot, whether it may start or continue an attack.
    opens: Vec<bool>,
    /// Per grid slot: sum and count of the signal centre over the attack,
    /// so a retune is not thrown off by one block of lopsided FM speech.
    attack_centre: Vec<(f64, u32)>,
    attack_blocks: u64,
    hang_blocks: u64,
    preroll_blocks: u64,
    skipped: Vec<String>,
}

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
/// Time constant (s) of the SNR average that must also clear the close
/// threshold to hold a transmission open. An FM carrier's power is steady,
/// so averaging costs it nothing; noise alone pokes one block over the
/// threshold now and then, and on a real RSP1A that was often enough to
/// hold empty recordings open for minutes.
const SQUELCH_SMOOTHING: f64 = 0.05;

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
            smooth: vec![0.0; count],
            smooth_alpha: (block / SQUELCH_SMOOTHING.max(block)).min(1.0) as f32,
            psd: vec![0.0; n],
            raw: vec![0.0; n],
            cumsum: vec![0.0; n + 1],
            seg_bounds,
            seg_floor: vec![0.0; nseg],
            seg_scratch: Vec::new(),
            seg_level: vec![0.0; nseg],
            common: 1.0,
            blocks_seen: 0,
            burst_blocks: 0,
            step_blocks: 0,
            opens: Vec::new(),
            attack_centre: Vec::new(),
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

    /// The SNR that holds a transmission open and unmutes its audio: the
    /// lower of this block's SNR and its average over the last ~50 ms. A
    /// key-up drops it at once; a one-block noise spike does not raise it.
    pub fn squelch_db(&self, i: usize) -> f32 {
        let avg = 10.0 * self.smooth[i].max(1e-12).log10();
        self.stats[i].snr_db.min(avg)
    }

    /// Power-weighted centre (offset from the capture centre, Hz) of the
    /// excess power over the noise floor in a slot's window, from the latest
    /// block. Used to tune fallback grid slots onto off-raster signals.
    pub fn centroid_hz(&self, i: usize) -> f64 {
        match self.attack_centre.get(i) {
            Some(&(sum, n)) if n > 0 => sum / n as f64,
            _ => self.block_centroid_hz(i),
        }
    }

    fn block_centroid_hz(&self, i: usize) -> f64 {
        let s = &self.slots[i];
        let floor = self.seg_floor[s.segment];
        // Look half a channel beyond the window on each side, so a signal
        // straddling the window edge is measured whole.
        let pad = (s.slot.bandwidth / 2.0 / self.bank.bin_hz()) as usize;
        let lo = s.lo.saturating_sub(pad);
        let hi = (s.hi + pad).min(self.raw.len());
        self.excess_centroid(lo, hi, floor).unwrap_or(s.offset_hz)
    }

    /// Power-weighted centre (offset Hz) of the power above `floor` in
    /// shifted bins `lo..hi`, or None if nothing is above it.
    fn excess_centroid(&self, lo: usize, hi: usize, floor: f32) -> Option<f64> {
        let (mut w, mut acc) = (0.0f64, 0.0f64);
        for (k, &p) in self.raw[lo..hi].iter().enumerate() {
            let excess = (p - floor).max(0.0) as f64;
            w += excess;
            acc += excess * (lo + k) as f64;
        }
        (w > 0.0).then(|| (acc / w - (self.bank.n / 2) as f64) * self.bank.bin_hz())
    }

    /// Whether the excess power in an explicit slot's own window is centred
    /// within a quarter of its bandwidth.
    fn is_centred(&self, i: usize) -> bool {
        let s = &self.slots[i];
        let floor = self.seg_floor[s.segment];
        let Some(off) = self.excess_centroid(s.lo, s.hi, floor) else {
            return false;
        };
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

    /// The smoothed spectrum between `lo_hz` and `hi_hz` (offsets from the
    /// capture centre), reduced to `bins` values in dB (same scale as
    /// [`Self::floor_db`]). Each value is the strongest bin it covers, so a
    /// narrow carrier stays visible at low display resolution.
    pub fn spectrum_db(&self, lo_hz: f64, hi_hz: f64, bins: usize) -> Vec<f32> {
        let n = self.bank.n;
        let bin_hz = self.bank.bin_hz();
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
                self.raw[h - w..=h + w].fill(0.5 * (a + b));
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
        let mut risen = 0usize;
        for (i, &(a, b)) in self.seg_bounds.iter().enumerate() {
            // The whole segment's level this block, from its median bin:
            // signals occupy too few bins to move it.
            self.seg_scratch.clear();
            let stride = ((b - a) / 512).max(1);
            self.seg_scratch
                .extend(self.raw[a..b].iter().step_by(stride).copied());
            let mid = self.seg_scratch.len() / 2;
            let (_, m, _) = self.seg_scratch.select_nth_unstable_by(mid, f32::total_cmp);
            let level = *m * RAW_MEDIAN_TO_MEAN;
            self.seg_level[i] = level;
            {
                let f = &mut self.seg_floor[i];
                if *f > 0.0 && level > *f * STEP_RATIO {
                    // Follow a broadband rise at once instead of over the
                    // slow growth time, or every slot in the segment looks
                    // busy until the floor catches up.
                    risen += 1;
                    *f = level;
                    continue;
                }
            }
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
            // A block of zeros (a zero-filled overrun gap) says nothing about
            // the noise floor; following it down would make every slot look
            // busy once samples return.
            if *f > 0.0 && est < *f * 1e-3 {
                continue;
            }
            if *f == 0.0 || est < *f {
                *f += (falpha * 4.0).min(1.0) * (est - *f);
            } else {
                *f += falpha * (est - *f);
            }
        }
        self.blocks_seen += 1;
        // A broadband step across much of the band this block: nothing new
        // may start (see the guard below).
        let step = risen * 4 >= self.seg_bounds.len() && self.blocks_seen > 1;
        if step {
            self.step_blocks += 1;
        }
        self.common = self.common_ratio();

        // SNR per slot, against the floor raised by any band-wide rise this
        // block (see `common_ratio`).
        let common = self.common as f64;
        for (i, s) in self.slots.iter().enumerate() {
            let p = self.cumsum[s.hi] - self.cumsum[s.lo];
            let noise = self.seg_floor[s.segment] as f64 * common * (s.hi - s.lo) as f64;
            let snr = if noise > 0.0 {
                10.0 * (p / noise).max(1e-12).log10()
            } else {
                0.0
            };
            self.stats[i].snr_db = snr as f32;
            let lin = 10f32.powf(snr as f32 / 10.0);
            let a = if self.blocks_seen <= 1 {
                1.0
            } else {
                self.smooth_alpha
            };
            self.smooth[i] += a * (lin - self.smooth[i]);
        }

        let b = block.index;
        let mut active = self
            .states
            .iter()
            .filter(|s| matches!(s, SlotState::Active { .. } | SlotState::Hang { .. }))
            .count();
        self.opens.clear();
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
            self.opens.push(snr >= self.params.open_db && wins);
        }
        if step {
            // Hold every waiting slot back for this block; the floor has
            // already moved up, so real signals still clear it next block.
            for i in 0..self.slots.len() {
                if matches!(self.states[i], SlotState::Idle | SlotState::Attack { .. }) {
                    self.opens[i] = false;
                }
            }
        }
        // Broadband-burst guard (see DetectorParams::burst_slots).
        if self.params.burst_slots > 0 {
            let waiting = |i: usize| {
                matches!(self.states[i], SlotState::Idle | SlotState::Attack { .. })
                    && self.opens[i]
            };
            let hot = (0..self.slots.len()).filter(|&i| waiting(i)).count();
            if hot >= self.params.burst_slots {
                // The reference is the strongest signal anywhere, including
                // one already open: the transmitter causing the overload is
                // usually open before its products cross the threshold.
                let top = (0..self.slots.len())
                    .filter(|&i| waiting(i) || !matches!(self.states[i], SlotState::Idle))
                    .map(|i| self.stats[i].snr_db)
                    .fold(f32::MIN, f32::max);
                let keep = top - self.params.burst_margin_db;
                let drop: Vec<usize> = (0..self.slots.len())
                    .filter(|&i| waiting(i) && self.stats[i].snr_db < keep)
                    .collect();
                for i in drop {
                    self.opens[i] = false;
                }
                self.burst_blocks += 1;
            }
        }
        self.attack_centre.resize(self.slots.len(), (0.0, 0));
        for i in 0..self.slots.len() {
            let snr = self.stats[i].snr_db;
            let open = self.opens[i];
            // An explicit slot starts its attack only for a signal centred on
            // it, not for the skirt of a neighbour 10 kHz away. (Grid slots
            // retune to the signal instead.) Checked once, at the start of the
            // attack, because FM speech is lopsided from block to block.
            let start =
                open && (self.slots[i].slot.origin == SlotOrigin::Grid || self.is_centred(i));
            let stay = self.squelch_db(i) >= self.params.close_db;
            let st = &mut self.stats[i];
            let next = match self.states[i] {
                SlotState::Idle if start => SlotState::Attack { since: b },
                SlotState::Idle => SlotState::Idle,
                SlotState::Attack { since } if open => {
                    if b + 1 - since >= self.attack_blocks
                        && (active < self.params.max_active || self.slots[i].slot.priority)
                    {
                        // If the open was held back (max_active), only the
                        // last attack's worth of blocks is still in the
                        // consumer's history; start from there so signal
                        // time is never counted from before the audio.
                        let since = since.max(b + 1 - self.attack_blocks);
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
            let grid = self.slots[i].slot.origin == SlotOrigin::Grid;
            match (self.states[i], next) {
                (_, SlotState::Attack { .. }) if grid => {
                    let c = self.block_centroid_hz(i);
                    let a = &mut self.attack_centre[i];
                    if matches!(self.states[i], SlotState::Idle) {
                        *a = (0.0, 0);
                    }
                    a.0 += c;
                    a.1 += 1;
                }
                (SlotState::Attack { .. }, SlotState::Active { .. }) if grid => {
                    let c = self.block_centroid_hz(i);
                    let a = &mut self.attack_centre[i];
                    a.0 += c;
                    a.1 += 1;
                }
                (_, SlotState::Idle) => self.attack_centre[i] = (0.0, 0),
                _ => {}
            }
            let st = &mut self.stats[i];
            if matches!(next, SlotState::Active { .. }) {
                st.peak = st.peak.max(snr);
                st.sum += snr as f64;
                st.count += 1;
            }
            self.states[i] = next;
        }
    }

    /// How far the whole band's noise sits above the smoothed floor this
    /// block: the median over segments of this block's level over the
    /// floor, at least 1.
    ///
    /// The floor follows the noise over seconds, but the noise itself can
    /// swell by several dB for a few hundred ms at a time across the whole
    /// band (seen live on an RSP1A at LNA 3 / GR 40, Nov 2026: every slot
    /// rose 2-6 dB together, several times a second). Against the slow
    /// floor alone that reads as a weak signal in every slot, which opened
    /// dozens of empty recordings and then held them open for minutes. A
    /// rise shared by most segments is noise, not signals: a signal
    /// occupies a few segments at most, so the median ignores it.
    fn common_ratio(&mut self) -> f32 {
        self.seg_scratch.clear();
        self.seg_scratch.extend(
            self.seg_level
                .iter()
                .zip(&self.seg_floor)
                .filter(|&(&l, &f)| f > 0.0 && l > 0.0)
                .map(|(&l, &f)| l / f),
        );
        if self.seg_scratch.is_empty() {
            return 1.0;
        }
        let mid = self.seg_scratch.len() / 2;
        let (_, m, _) = self.seg_scratch.select_nth_unstable_by(mid, f32::total_cmp);
        m.max(1.0)
    }

    /// Blocks in which the broadband-burst guard held slots back.
    pub fn burst_blocks(&self) -> u64 {
        self.burst_blocks
    }

    /// Blocks in which the noise floor stepped up across much of the band
    /// (gain change, overload, a radio stream restart).
    pub fn step_blocks(&self) -> u64 {
        self.step_blocks
    }

    /// End of stream: close everything that is open.
    pub fn finish(&mut self, last_block: u64, events: &mut Vec<Event>) {
        for i in 0..self.slots.len() {
            self.close(i, last_block, events);
        }
    }

    /// End slot `i`'s transmission now (if it has one), as if the signal had
    /// stopped at `last_block`. The slot can open again on a later block.
    pub fn close(&mut self, i: usize, last_block: u64, events: &mut Vec<Event>) {
        let (start, end) = match self.states[i] {
            SlotState::Active { start } => (start, last_block),
            SlotState::Hang { start, since } => (start, since.saturating_sub(1)),
            SlotState::Attack { .. } => {
                self.states[i] = SlotState::Idle;
                return;
            }
            SlotState::Idle => return,
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

    /// Change whether slot `i` is recorded and whether it is a priority
    /// slot (lockout and priority edits while running).
    pub fn set_flags(&mut self, i: usize, record: bool, priority: bool) {
        let s = &mut self.slots[i].slot;
        s.record = record;
        s.priority = priority;
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
            pair: None,
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
    fn spectrum_shows_a_carrier_where_it_is() {
        // 20 dB FM signal 250 kHz above centre, over 0.5 s.
        let x = scene(0.5, &[(250e3, 20.0, 0.0, 1.0)], 3);
        let (det, _, _) = run(&x, &[slot(0.0, 16e3)], DetectorParams::default());
        // 500 kHz either side in 100 bins of 10 kHz: the carrier is bin 75.
        let s = det.spectrum_db(-500e3, 500e3, 100);
        assert_eq!(s.len(), 100);
        let peak = (0..100).max_by(|&a, &b| s[a].total_cmp(&s[b])).unwrap();
        assert!((74..=75).contains(&peak), "peak at {peak}");
        let mut sorted = s.clone();
        sorted.sort_by(f32::total_cmp);
        assert!(
            s[peak] - sorted[50] > 15.0,
            "{} over {}",
            s[peak],
            sorted[50]
        );
        assert!(det.spectrum_db(1.0, 1.0, 10).is_empty());
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

    /// Swell the noise of `x` by up to `rise_db` once a second, as the
    /// whole band rises and falls back together: a 0.1 s ramp up, 0.4 s
    /// held, a 0.1 s ramp down. Under 3 dB it never trips the step guard.
    fn swell(x: &mut [Complex32], rise_db: f64) {
        let fs = params().sample_rate;
        for (i, v) in x.iter_mut().enumerate() {
            let t = (i as f64 / fs).fract();
            let shape = if t < 0.1 {
                t / 0.1
            } else if t < 0.5 {
                1.0
            } else if t < 0.6 {
                (0.6 - t) / 0.1
            } else {
                0.0
            };
            *v *= 10f64.powf(shape * rise_db / 20.0) as f32;
        }
    }

    #[test]
    fn band_wide_noise_swells_open_nothing() {
        // Live RSP1A at LNA 3 / GR 40: the whole band's noise rose 2-6 dB
        // together for a few hundred ms at a time; every slot read as a
        // weak signal against the slow floor.
        let mut x = scene(6.0, &[], 5);
        swell(&mut x, 2.9);
        let plan: Vec<Slot> = (-30..=30).map(|k| slot(k as f64 * 25e3, 16e3)).collect();
        let (det, events, snrs) = run(&x, &plan, DetectorParams::default());
        let opens = events
            .iter()
            .filter(|e| matches!(e, Event::Open { .. }))
            .count();
        // Blocks reading above the close threshold would hold a
        // transmission open (and play noise into it).
        let tail: Vec<f32> = snrs[snrs.len() / 3..].iter().flatten().copied().collect();
        let over = tail.iter().filter(|&&v| v >= 3.0).count() as f64 / tail.len() as f64;
        eprintln!(
            "swell: {opens} opens, {:.3}% of slot-blocks over 3 dB, {} step blocks",
            over * 100.0,
            det.step_blocks()
        );
        assert_eq!(opens, 0, "{events:?}");
        assert!(over < 1e-3, "{over}");
    }

    #[test]
    fn signal_survives_band_wide_noise_swells() {
        let mut x = scene(6.0, &[(100e3, 15.0, 1.0, 5.0)], 9);
        swell(&mut x, 2.9);
        let plan: Vec<Slot> = (-20..=20).map(|k| slot(k as f64 * 25e3, 16e3)).collect();
        let (_, events, _) = run(&x, &plan, DetectorParams::default());
        let opens: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::Open { .. }))
            .collect();
        assert_eq!(opens.len(), 1, "{events:?}");
        // It ends when the signal does, not whenever the swells let up.
        let block = params().block_secs();
        let end = events
            .iter()
            .find_map(|e| match e {
                Event::Close { end_block, .. } => Some(*end_block as f64 * block),
                _ => None,
            })
            .expect("closed");
        assert!((end - 5.0).abs() < 0.1, "ended at {end}");
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
    fn zero_filled_gap_does_not_cause_false_opens() {
        let mut x = scene(3.0, &[], 9);
        let fs = params().sample_rate;
        for v in &mut x[(1.0 * fs) as usize..(1.6 * fs) as usize] {
            *v = Complex32::default();
        }
        let plan: Vec<Slot> = (-20..=20).map(|k| slot(k as f64 * 25e3, 16e3)).collect();
        let (_, events, _) = run(&x, &plan, DetectorParams::default());
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn broadband_burst_opens_only_the_strong_signal() {
        // A strong carrier plus eight weak "intermod products" that all key
        // up in the same instant, as when a nearby handheld overloads the
        // front end.
        let mut sigs = vec![(100e3, 45.0, 0.5, 2.0)];
        for k in 0..8 {
            sigs.push((-300e3 + k as f64 * 50e3, 12.0, 0.5, 2.0));
        }
        let x = scene(3.0, &sigs, 11);
        let plan: Vec<Slot> = (-20..=20).map(|k| slot(k as f64 * 25e3, 16e3)).collect();
        let (det, events, _) = run(&x, &plan, DetectorParams::default());
        let opened: Vec<f64> = events
            .iter()
            .filter_map(|e| match e {
                Event::Open { slot, .. } => Some(det.slots()[*slot].slot.freq),
                _ => None,
            })
            .collect();
        assert_eq!(opened, vec![100e3], "{opened:?}");
        assert!(det.burst_blocks() > 0);
        // With the guard off, the products open too.
        let (_, events, _) = run(
            &x,
            &plan,
            DetectorParams {
                burst_slots: 0,
                ..Default::default()
            },
        );
        let n = events
            .iter()
            .filter(|e| matches!(e, Event::Open { .. }))
            .count();
        assert!(n >= 7, "{n}");
    }

    #[test]
    fn broadband_noise_step_does_not_open_slots() {
        // The noise level jumps 6 dB across the whole band (a gain step or a
        // radio stream restart), with one real signal starting after it.
        let mut x = scene(4.0, &[(100e3, 25.0, 2.5, 3.5)], 13);
        let fs = params().sample_rate;
        for v in &mut x[(1.5 * fs) as usize..] {
            *v *= 2.0;
        }
        let plan: Vec<Slot> = (-20..=20).map(|k| slot(k as f64 * 25e3, 16e3)).collect();
        let (det, events, _) = run(&x, &plan, DetectorParams::default());
        let opened: Vec<f64> = events
            .iter()
            .filter_map(|e| match e {
                Event::Open { slot, .. } => Some(det.slots()[*slot].slot.freq),
                _ => None,
            })
            .collect();
        assert_eq!(opened, vec![100e3], "{opened:?}");
        assert!(det.step_blocks() > 0);
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
