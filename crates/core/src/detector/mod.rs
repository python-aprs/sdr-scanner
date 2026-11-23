//! Slot detector and per-slot state machine, PLAN.md 3.3.
//!
//! Every forward-FFT block updates a smoothed power spectrum. Each slot of the
//! channel plan owns a window of its bandwidth; the integrated power in that
//! window, relative to the local noise floor times the window width, is the
//! slot's SNR. A per-slot state machine (idle → attack → active → hang → idle)
//! turns that into transmission open/close events.

mod background;
mod floor;
mod guards;
mod spectrum;
mod state;

pub use state::{Event, SlotState};

use crate::channelizer::{BankParams, Block};
use crate::config::{Slot, SlotOrigin, same_freq};
use background::{PulsedComb, SteadyBackground, comb_pattern};
use floor::NoiseFloor;
use guards::{BurstGuard, InterferenceGuard};
use spectrum::Spectrum;
use state::SlotStats;

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
    /// Broadband-burst guard: when at least this many idle slots read over
    /// the close threshold in the same block, it is front-end overload or
    /// intermod (a strong nearby transmitter), not that many new
    /// transmissions. Then only slots within `burst_margin_db` of the
    /// strongest signal (open or not) may open. 0 disables.
    pub burst_slots: usize,
    pub burst_margin_db: f32,
    /// Per-slot background, for steady interference such as the comb of
    /// lines a switching power supply puts across the band: each slot
    /// learns the level it sits at most of the time (the `background_pct`
    /// quantile of its 1 s mean SNR over the last `background_secs`), and a
    /// background of `background_min_db` to `background_max_db` raises
    /// that slot's open and close thresholds by as much. A real signal on
    /// such a line must then clear the line, not just the noise. 0
    /// disables.
    pub background_secs: f64,
    pub background_pct: f32,
    pub background_min_db: f32,
    /// A steady level above this is a real carrier (a beacon, a weather
    /// broadcast), not a comb line; it is left alone.
    pub background_max_db: f32,
    /// Pulsed comb guard. A switching supply that turns on and off puts
    /// its whole comb up at once: when at least `comb_teeth` slots start an
    /// attack within `comb_window` seconds, all no stronger than
    /// `background_max_db`, and that many of them sit on one evenly spaced
    /// grid, those slots are comb lines. For `comb_hold` seconds after a
    /// slot was last seen as one, it must clear the level it rose to then
    /// by `comb_margin_db` to open (its close threshold is raised by as
    /// much). Linked repeaters keying up together are not evenly spaced
    /// and are usually stronger, so they are left alone. 0 teeth disables.
    pub comb_teeth: usize,
    pub comb_window: f64,
    pub comb_hold: f64,
    pub comb_margin_db: f32,
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
            background_secs: 300.0,
            background_pct: 0.1,
            background_min_db: 1.5,
            background_max_db: 15.0,
            comb_teeth: 4,
            comb_window: 1.0,
            comb_hold: 1800.0,
            comb_margin_db: 6.0,
        }
    }
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

/// Turns FFT blocks into open/close events, in stages each block:
///
/// 1. [`Spectrum`]: per-bin power, DC notch, smoothed spectrum.
/// 2. [`NoiseFloor`]: per-segment floor, the noise-step guard, and the
///    band-wide rise taken out of every slot's SNR.
/// 3. [`InterferenceGuard`]: band-wide bursts block opens.
/// 4. Slot SNRs, and the thresholds raised over steady interference
///    ([`SteadyBackground`]) and pulsed comb lines ([`PulsedComb`]).
/// 5. Which slots may open: stronger than overlapping neighbours, over
///    their raised threshold, not held back by a guard ([`BurstGuard`]).
/// 6. The per-slot state machine (idle → attack → active → hang → idle).
pub struct Detector {
    params: DetectorParams,
    bank: BankParams,
    slots: Vec<WatchedSlot>,
    states: Vec<SlotState>,
    stats: Vec<SlotStats>,
    /// Per slot, SNR (linear) smoothed over ~50 ms; see [`Detector::squelch_db`].
    smooth: Vec<f32>,
    smooth_alpha: f32,
    spectrum: Spectrum,
    floor: NoiseFloor,
    floor_alpha: f32,
    interference: InterferenceGuard,
    burst: BurstGuard,
    background: SteadyBackground,
    comb: PulsedComb,
    blocks_seen: u64,
    /// Scratch: per slot, whether it may start or continue an attack.
    opens: Vec<bool>,
    /// Scratch: per slot, the close threshold this block (raised over
    /// interference).
    close_db: Vec<f32>,
    /// Per grid slot: sum and count of the signal centre over the attack,
    /// so a retune is not thrown off by one block of lopsided FM speech.
    attack_centre: Vec<(f64, u32)>,
    /// Per slot: the last block it came back from its hang time (an open
    /// slot rising again), for the pulsed comb guard.
    back_at: Vec<Option<u64>>,
    attack_blocks: u64,
    hang_blocks: u64,
    preroll_blocks: u64,
    skipped: Vec<String>,
}

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
        let floor = NoiseFloor::new(bank, params.usable, params.floor_segments);
        for s in &mut slots {
            s.segment = floor.segment_of((s.lo + s.hi) / 2);
        }
        let block = bank.block_secs();
        let blocks = |secs: f64| (secs / block).ceil() as u64;
        let count = slots.len();
        let attack_blocks = blocks(params.attack).max(1);
        Detector {
            spectrum: Spectrum::new(bank, params.smoothing, params.dc_notch_hz),
            floor,
            floor_alpha: (block / params.floor_smoothing.max(block)).min(1.0) as f32,
            interference: InterferenceGuard::default(),
            burst: BurstGuard::new(params.burst_slots, params.burst_margin_db),
            background: SteadyBackground::new(
                count,
                params.background_secs,
                params.background_pct,
                block,
            ),
            comb: PulsedComb::new(
                params.comb_teeth,
                params.comb_window,
                params.comb_hold,
                params.background_max_db,
                attack_blocks,
                block,
                slots.iter().map(|s| s.offset_hz).collect(),
            ),
            attack_blocks,
            hang_blocks: blocks(params.hang).max(1),
            preroll_blocks: blocks(params.preroll),
            params,
            bank,
            slots,
            states: vec![SlotState::Idle; count],
            stats: vec![SlotStats::default(); count],
            smooth: vec![0.0; count],
            smooth_alpha: (block / SQUELCH_SMOOTHING.max(block)).min(1.0) as f32,
            blocks_seen: 0,
            opens: vec![false; count],
            close_db: vec![0.0; count],
            attack_centre: vec![(0.0, 0); count],
            back_at: vec![None; count],
            skipped,
        }
    }

    pub fn slots(&self) -> &[WatchedSlot] {
        &self.slots
    }

    /// The watched slot at `freq` (Hz, see [`same_freq`]).
    pub fn slot_at(&self, freq: f64) -> Option<usize> {
        self.slots.iter().position(|w| same_freq(w.slot.freq, freq))
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
        let floor = self.floor.floor[s.segment];
        // Look half a channel beyond the window on each side, so a signal
        // straddling the window edge is measured whole.
        let pad = (s.slot.bandwidth / 2.0 / self.bank.bin_hz()) as usize;
        let lo = s.lo.saturating_sub(pad);
        let hi = (s.hi + pad).min(self.spectrum.raw.len());
        self.spectrum
            .excess_centroid(lo, hi, floor)
            .unwrap_or(s.offset_hz)
    }

    /// Whether the excess power in an explicit slot's own window is centred
    /// within a quarter of its bandwidth.
    fn is_centred(&self, i: usize) -> bool {
        let s = &self.slots[i];
        let floor = self.floor.floor[s.segment];
        let Some(off) = self.spectrum.excess_centroid(s.lo, s.hi, floor) else {
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
        self.floor.floor_db(self.bank.n)
    }

    /// The smoothed spectrum between `lo_hz` and `hi_hz` (offsets from the
    /// capture centre), reduced to `bins` values in dB (same scale as
    /// [`Self::floor_db`]). Each value is the strongest bin it covers, so a
    /// narrow carrier stays visible at low display resolution.
    pub fn spectrum_db(&self, lo_hz: f64, hi_hz: f64, bins: usize) -> Vec<f32> {
        self.spectrum.smoothed_db(lo_hz, hi_hz, bins)
    }

    /// Update with one block; appends open/close events.
    pub fn update(&mut self, block: &Block, events: &mut Vec<Event>) {
        let first = self.blocks_seen == 0;
        self.spectrum.update(block, first);
        let falpha = if first { 1.0 } else { self.floor_alpha };
        let step = self.floor.update(&self.spectrum, falpha, first);
        self.blocks_seen += 1;
        let interfered = self
            .interference
            .update(self.floor.band_power, falpha, first);
        let held = step || interfered;

        // SNR per slot, against the floor raised by any band-wide rise this
        // block (see `NoiseFloor::common_ratio`).
        let common = self.floor.common as f64;
        let a = if first { 1.0 } else { self.smooth_alpha };
        for (i, s) in self.slots.iter().enumerate() {
            let p = self.spectrum.power(s.lo, s.hi);
            let noise = self.floor.floor[s.segment] as f64 * common * (s.hi - s.lo) as f64;
            let snr = if noise > 0.0 {
                10.0 * (p / noise).max(1e-12).log10()
            } else {
                0.0
            };
            self.stats[i].snr_db = snr as f32;
            if !held && noise > 0.0 {
                self.background.add(i, p / noise);
            }
            let lin = 10f32.powf(snr as f32 / 10.0);
            self.smooth[i] += a * (lin - self.smooth[i]);
        }
        if !held {
            self.background.end_block();
        }

        let b = block.index;
        self.find_opens(b, held);
        if held {
            // Hold every waiting slot back for this block; after a step the
            // floor has already moved up, so real signals still clear it
            // next block.
            for (open, st) in self.opens.iter_mut().zip(&self.states) {
                if st.is_waiting() {
                    *open = false;
                }
            }
        }
        let open_on = |i: usize| {
            let snr = self.stats[i].snr_db;
            let back = self.back_at[i]?;
            (self.states[i].is_open() && snr >= self.params.open_db).then_some((snr, back))
        };
        for i in self.comb.learn(b, open_on) {
            if self.states[i].is_waiting() {
                self.states[i] = SlotState::Idle;
                self.attack_centre[i] = (0.0, 0);
                self.opens[i] = false;
            }
        }
        // Comb lines just learned raise their thresholds from here on.
        for i in 0..self.slots.len() {
            self.close_db[i] = self.params.close_db + self.background_db(i);
        }
        self.burst
            .apply(&self.states, &self.stats, &self.close_db, &mut self.opens);
        self.step_states(b, interfered, events);
    }

    /// Which slots may start or continue an attack this block (`opens`),
    /// and the rises the pulsed comb guard watches.
    fn find_opens(&mut self, b: u64, held: bool) {
        for i in 0..self.slots.len() {
            let snr = self.stats[i].snr_db;
            let dens = |j: usize| self.stats[j].snr_db;
            let wins = self.slots[i].neighbors.iter().all(|&j| {
                // A busy neighbour that is stronger takes this signal, unless
                // this slot already owns it.
                let other_owns = self.states[j].is_open();
                !(dens(j) > snr || (other_owns && dens(j) >= snr - 3.0))
            });
            self.opens[i] = snr >= self.params.open_db + self.background_db(i) && wins;
            let over = self.states[i].is_waiting() && snr >= self.params.open_db && wins && !held;
            self.comb.observe(i, b, snr, over);
        }
    }

    /// Advance every slot's state machine by one block.
    fn step_states(&mut self, b: u64, interfered: bool, events: &mut Vec<Event>) {
        let mut active = self.states.iter().filter(|s| s.is_open()).count();
        for i in 0..self.slots.len() {
            let open = self.opens[i];
            // An explicit slot starts its attack only for a signal centred on
            // it, not for the skirt of a neighbour 10 kHz away. (Grid slots
            // retune to the signal instead.) Checked once, at the start of the
            // attack, because FM speech is lopsided from block to block.
            let start =
                open && (self.slots[i].slot.origin == SlotOrigin::Grid || self.is_centred(i));
            let stay = self.squelch_db(i) >= self.close_db[i];
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
                        st.open();
                        self.back_at[i] = None;
                        SlotState::Active { start }
                    } else {
                        SlotState::Attack { since }
                    }
                }
                SlotState::Attack { .. } => SlotState::Idle,
                SlotState::Active { start } if stay => SlotState::Active { start },
                SlotState::Active { start } => SlotState::Hang { start, since: b },
                SlotState::Hang { start, .. } if stay && !interfered => {
                    self.back_at[i] = Some(b);
                    SlotState::Active { start }
                }
                SlotState::Hang { start, since } => {
                    if b - since >= self.hang_blocks {
                        events.push(st.close(i, start, since.saturating_sub(1)));
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
            if matches!(next, SlotState::Active { .. }) {
                self.stats[i].add();
            }
            self.states[i] = next;
        }
    }

    /// Pulsed comb events seen (see `DetectorParams::comb_teeth`).
    pub fn comb_events(&self) -> u64 {
        self.comb.events
    }

    /// How far slot `i`'s open and close thresholds are raised for steady
    /// interference under it (dB; see `DetectorParams::background_secs`).
    pub fn background_db(&self, i: usize) -> f32 {
        let bg = self.background.db[i];
        let steady = if bg >= self.params.background_min_db && bg <= self.params.background_max_db {
            bg
        } else {
            0.0
        };
        let level = self.comb.level[i];
        let comb = if level > 0.0 {
            (level + self.params.comb_margin_db - self.params.open_db).max(0.0)
        } else {
            0.0
        };
        steady.max(comb)
    }

    /// Slots whose thresholds are raised for steady or pulsed comb
    /// interference.
    pub fn background_slots(&self) -> usize {
        (0..self.slots.len())
            .filter(|&i| self.background_db(i) > 0.0)
            .count()
    }

    /// The comb pattern in the steady interference, if there is one: the
    /// commonest spacing (Hz, to 5 kHz) between neighbouring raised slots,
    /// and how many times it occurs. A switching power supply puts lines
    /// at multiples of its switching frequency, so evenly spaced raised
    /// slots point at one. Diagnostic only: the thresholds are raised per
    /// slot either way.
    pub fn comb(&self) -> Option<(f64, usize)> {
        comb_pattern(
            (0..self.slots.len())
                .filter(|&i| self.background_db(i) >= 3.0)
                .map(|i| self.slots[i].offset_hz)
                .collect(),
        )
    }

    /// Blocks in which the broadband-burst guard held slots back.
    pub fn burst_blocks(&self) -> u64 {
        self.burst.blocks
    }

    /// Blocks in which the noise floor stepped up across much of the band
    /// (gain change, overload, a radio stream restart).
    pub fn step_blocks(&self) -> u64 {
        self.floor.step_blocks
    }

    /// Blocks flagged as interference bursts.
    pub fn interference_blocks(&self) -> u64 {
        self.interference.blocks
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
        events.push(self.stats[i].close(i, start, end));
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
    use background::comb_members;
    use num_complex::Complex32;
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

    /// Add a burst of clicks (a pulse every ms, so a 1 kHz comb across the
    /// band) lasting `len` seconds every `period` seconds.
    fn clicks(x: &mut [Complex32], period: f64, len: f64, amp: f32) {
        let fs = params().sample_rate;
        let every = (fs / 1000.0) as usize;
        for (i, v) in x.iter_mut().enumerate() {
            let t = i as f64 / fs;
            if t % period < len && i % every == 0 {
                *v += Complex32::new(amp, 0.0);
            }
        }
    }

    #[test]
    fn periodic_click_bursts_open_nothing() {
        // Live RSP1A at LNA 3 / GR 40: a 20-50 ms burst every 1.2 s read
        // 5-11 dB in every slot without moving the segments' median bins;
        // it opened recordings and, coming sooner than the hang time,
        // never let them close. These clicks add ~9 dB to the band's mean
        // power while they last.
        let mut x = scene(8.0, &[], 13);
        clicks(&mut x, 1.2, 0.04, 1.0);
        let plan: Vec<Slot> = (-30..=30).map(|k| slot(k as f64 * 25e3, 16e3)).collect();
        let (_, events, snrs) = run(&x, &plan, DetectorParams::default());
        let opens = events
            .iter()
            .filter(|e| matches!(e, Event::Open { .. }))
            .count();
        // The slots' mean SNR in the loudest block.
        let worst = snrs[snrs.len() / 8..]
            .iter()
            .map(|row| row.iter().sum::<f32>() / row.len() as f32)
            .fold(f32::MIN, f32::max);
        eprintln!("clicks: {opens} opens, worst block mean {worst} dB");
        assert_eq!(opens, 0, "{events:?}");
        assert!(worst < 1.5, "worst {worst}");
    }

    /// The interference in a live RSP1A capture (LNA 3 / GR 40, Nov 2026):
    /// for 0.1 s every 0.6 s the band's noise rises ~4 dB and a comb of
    /// ~10 kHz-wide lines every 256 kHz comes up ~14 dB, right on channels.
    fn comb_bursts(secs: f64, extra: &[(f64, f64, f64, f64)], seed: u64) -> Vec<Complex32> {
        let mut sigs: Vec<(f64, f64, f64, f64)> = extra.to_vec();
        let mut t = 0.3;
        while t < secs {
            for k in [-3.0, -2.0, -1.0, 1.0, 2.0, 3.0] {
                sigs.push((k * 256e3, 14.0, t, t + 0.1));
            }
            t += 0.6;
        }
        let mut x = scene(secs, &sigs, seed);
        let fs = params().sample_rate;
        let g = 10f32.powf(4.0 / 20.0);
        for (i, v) in x.iter_mut().enumerate() {
            let t = i as f64 / fs;
            if t >= 0.3 && (t - 0.3) % 0.6 < 0.1 {
                *v *= g;
            }
        }
        x
    }

    #[test]
    fn pulsed_comb_bursts_open_nothing() {
        let x = comb_bursts(6.0, &[], 21);
        let plan: Vec<Slot> = (-40..=40).map(|k| slot(k as f64 * 20e3, 16e3)).collect();
        let (det, events, _) = run(&x, &plan, DetectorParams::default());
        let opens = events
            .iter()
            .filter(|e| matches!(e, Event::Open { .. }))
            .count();
        eprintln!(
            "comb: {opens} opens, {} interference blocks",
            det.interference_blocks()
        );
        assert_eq!(opens, 0, "{events:?}");
        assert!(det.interference_blocks() > 0);
    }

    #[test]
    fn signals_get_through_pulsed_comb_bursts() {
        // One on a comb line, one between lines, both ~10 dB.
        let sigs = [(256e3, 10.0, 1.0, 4.0), (380e3, 10.0, 1.5, 5.0)];
        let x = comb_bursts(6.0, &sigs, 23);
        let plan: Vec<Slot> = (-40..=40).map(|k| slot(k as f64 * 20e3, 16e3)).collect();
        let (det, events, _) = run(&x, &plan, DetectorParams::default());
        let block = params().block_secs();
        let mut got: Vec<(f64, f64, f64)> = events
            .iter()
            .filter_map(|e| match e {
                Event::Close {
                    slot,
                    start_block,
                    end_block,
                    ..
                } => Some((
                    det.slots()[*slot].offset_hz,
                    (*start_block + det.signal_offset_blocks()) as f64 * block,
                    *end_block as f64 * block,
                )),
                _ => None,
            })
            .collect();
        got.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(got.len(), 2, "{got:?}");
        for (g, (off, _, on, end)) in got.iter().zip(sigs) {
            assert!((g.0 - off).abs() < 15e3, "{got:?}");
            // May start a burst late (nothing opens during one), and ends
            // with the signal.
            assert!(g.1 >= on - 0.05 && g.1 < on + 0.25, "{got:?}");
            assert!((g.2 - end).abs() < 0.1, "{got:?}");
        }
    }

    #[test]
    fn steady_comb_lines_stop_opening_but_signals_on_them_still_do() {
        // A switching-supply comb: steady lines every 125 kHz, ~9 dB in a
        // slot, and a real 20 dB signal right next to one of them later.
        let mut sigs: Vec<(f64, f64, f64, f64)> = [-3.0, -2.0, -1.0, 1.0, 2.0, 3.0]
            .iter()
            .map(|k| (k * 125e3, 11.0, 0.0, 9.0))
            .collect();
        sigs.push((255e3, 20.0, 5.0, 7.0));
        let x = scene(8.0, &sigs, 31);
        let plan: Vec<Slot> = (-16..=16).map(|k| slot(k as f64 * 25e3, 16e3)).collect();
        let dp = DetectorParams {
            grid_window_hz: 25e3,
            background_secs: 4.0,
            ..DetectorParams::default()
        };
        let (det, events, _) = run(&x, &plan, dp);
        let block = params().block_secs();
        let t = |b: u64| b as f64 * block;
        let mut real = Vec::new();
        for e in &events {
            match *e {
                Event::Open { slot, block, .. } => {
                    let off = det.slots()[slot].offset_hz;
                    if t(block) > 4.0 {
                        // Only the real signal may open once the lines are
                        // learned.
                        assert!((off - 250e3).abs() < 1.0, "{off} at {}", t(block));
                        real.push(t(block));
                    }
                }
                Event::Close {
                    slot, end_block, ..
                } => {
                    let off = det.slots()[slot].offset_hz;
                    if (off - 250e3).abs() > 1.0 {
                        // The lines close once learned instead of holding
                        // their slots open for good.
                        assert!(t(end_block) < 4.0, "{off} held to {}", t(end_block));
                    }
                }
            }
        }
        assert_eq!(real.len(), 1, "{events:?}");
        assert!((real[0] - 5.0).abs() < 0.2, "{real:?}");
        let raised = det.background_slots();
        assert!((6..=12).contains(&raised), "{raised} slots raised");
        assert_eq!(det.comb(), Some((125e3, 6)));
    }

    #[test]
    fn pulsed_comb_lines_are_learned_and_signals_still_open() {
        // A supply that cycles: lines every 125 kHz, ~9 dB, on for 2 s and
        // off for 2 s; a real 20 dB signal next to one starts while the
        // comb is up.
        let mut sigs: Vec<(f64, f64, f64, f64)> = Vec::new();
        for on in [0.5, 4.5] {
            for k in [-3.0, -2.0, -1.0, 1.0, 2.0, 3.0] {
                sigs.push((k * 125e3, 11.0, on, on + 2.0));
            }
        }
        sigs.push((255e3, 20.0, 5.0, 7.0));
        let x = scene(8.0, &sigs, 37);
        let plan: Vec<Slot> = (-16..=16).map(|k| slot(k as f64 * 25e3, 16e3)).collect();
        let dp = DetectorParams {
            grid_window_hz: 25e3,
            ..DetectorParams::default()
        };
        let (det, events, _) = run(&x, &plan, dp);
        let block = params().block_secs();
        let opens: Vec<(f64, f64)> = events
            .iter()
            .filter_map(|e| match *e {
                Event::Open { slot, block: b, .. } => {
                    Some((det.slots()[slot].offset_hz, b as f64 * block))
                }
                _ => None,
            })
            .collect();
        assert_eq!(opens.len(), 1, "{opens:?}");
        assert!((opens[0].0 - 250e3).abs() < 1.0, "{opens:?}");
        assert!((opens[0].1 - 5.0).abs() < 0.2, "{opens:?}");
        assert!(det.comb_events() >= 2, "{}", det.comb_events());
        assert_eq!(det.comb(), Some((125e3, 6)));
    }

    #[test]
    fn comb_line_open_before_the_comb_is_learned_closes_once_it_is() {
        // Startup: one line of a pulsed comb (0.1 s every 0.6 s, under the
        // 1.5 s hang) opens a slot before the guard knows the comb. Its own
        // pulses then hold it open. Once the whole comb pulses and the
        // guard learns it, that slot must close too, not record until the
        // comb stops (minutes of blips and silence, live on the RSP1A).
        let mut sigs: Vec<(f64, f64, f64, f64)> = Vec::new();
        let mut t = 0.3;
        while t < 10.0 {
            let ks: &[f64] = if t < 2.0 {
                &[1.0]
            } else {
                &[-3.0, -2.0, -1.0, 1.0, 2.0, 3.0]
            };
            for &k in ks {
                sigs.push((k * 260e3, 12.0, t, t + 0.1));
            }
            t += 0.6;
        }
        let x = scene(10.0, &sigs, 41);
        let plan: Vec<Slot> = (-40..=40).map(|k| slot(k as f64 * 20e3, 16e3)).collect();
        let (det, events, _) = run(&x, &plan, DetectorParams::default());
        let block = params().block_secs();
        let dbg: Vec<(f64, f64)> = events
            .iter()
            .map(|e| match *e {
                Event::Open { slot, block: b, .. } => {
                    (det.slots()[slot].offset_hz, b as f64 * block)
                }
                Event::Close {
                    slot, end_block, ..
                } => (-det.slots()[slot].offset_hz, end_block as f64 * block),
            })
            .collect();
        assert!(det.comb_events() >= 1, "comb never learned {dbg:?}");
        let mut opened = 0;
        for e in &events {
            match *e {
                Event::Open { block: b, .. } => {
                    // Only the line that came up alone, before the comb.
                    assert!(b as f64 * block < 2.0, "opened at {}", b as f64 * block);
                    opened += 1;
                }
                Event::Close { end_block, .. } => {
                    let end = end_block as f64 * block;
                    // Learned at the first full pulse (2.1 s); held at most
                    // until that pulse, not to the end of the capture.
                    assert!(end < 3.0, "held open to {end:.2} s");
                }
            }
        }
        assert_eq!(opened, 1, "{events:?}");
    }

    #[test]
    fn comb_members_needs_an_even_grid() {
        let khz = |v: &[f64]| v.iter().map(|f| f * 1e3).collect::<Vec<f64>>();
        // Masen's comb: every ~262 kHz, with a line missing.
        let got = comb_members(&khz(&[-520.0, -258.0, 4.0, 528.0, 790.0]), 4).unwrap();
        assert_eq!(got.len(), 5);
        // Four repeaters keyed up together, not evenly spaced.
        assert!(comb_members(&khz(&[-610.0, -130.0, 270.0, 875.0]), 4).is_none());
        // Too few lines.
        assert!(comb_members(&khz(&[0.0, 260.0, 520.0]), 4).is_none());
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
        // With the guard off, the products open too. (Evenly spaced as
        // they are here, the comb guard would also catch them.)
        let (_, events, _) = run(
            &x,
            &plan,
            DetectorParams {
                burst_slots: 0,
                comb_teeth: 0,
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
