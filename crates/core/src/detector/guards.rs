//! Guards that hold back opens for a block: band-wide interference bursts
//! and broadband overload. (The noise-step guard lives with the floor.)

use super::state::{SlotState, SlotStats};

/// A block whose band-wide power (the median over segments of their mean
/// power) is this many times its usual value is an interference burst. Live on an RSP1A at LNA 3 /
/// GR 40 (Nov 2026), a comb with lines every 256 kHz, each ~10 kHz wide,
/// pulsed on for ~100 ms every ~0.6 s: it lifted the band's mean ~6 dB but
/// the channels under its lines 10-14 dB, so taking the band-wide rise out
/// (`common_ratio`) still left them over the open threshold. Nothing may
/// start in such a block, and it cannot hold a transmission open: a
/// transmission in its hang time keeps counting down. Front-end overload
/// from a nearby handheld keyed up lifted the band's mean ~1.5x. The
/// usual value is tracked apart from the floor: between bursts the band's
/// mean sat at 0.3-0.6 of a floor the bursts themselves held up, and a
/// burst that lifts the band 3 dB snaps the floor up (the step guard).
const INTERFERENCE_RATIO: f32 = 2.0;
/// Blocks after an interference burst that are still treated as one.
const INTERFERENCE_HOLD: u32 = 2;

/// Flags interference bursts against the band's usual power.
#[derive(Default)]
pub(super) struct InterferenceGuard {
    /// Band power followed over seconds (grows slowly, falls fast, like
    /// the floor): the reference interference is judged against.
    base: f32,
    /// Blocks still to treat as interference after a flagged one, which
    /// covers the burst's tail spilling into the next blocks.
    hold: u32,
    /// Blocks flagged.
    pub blocks: u64,
}

impl InterferenceGuard {
    /// Judge this block's band power, then follow it (`alpha` as for the
    /// floor). Returns whether the block counts as interference.
    pub fn update(&mut self, band_power: f32, alpha: f32, first: bool) -> bool {
        let burst = !first && self.base > 0.0 && band_power >= INTERFERENCE_RATIO * self.base;
        if burst {
            self.hold = INTERFERENCE_HOLD;
        }
        let interfered = burst || self.hold > 0;
        if !burst {
            self.hold = self.hold.saturating_sub(1);
        }
        if interfered {
            self.blocks += 1;
        }
        // Followed through the bursts too (slowly, as they push it up), so
        // a lasting rise such as a gain change is absorbed within seconds
        // instead of being treated as interference for good. A block of
        // zeros (a dropped stretch) says nothing about it.
        if band_power > 0.0 {
            let m = band_power;
            let base = &mut self.base;
            if *base == 0.0 || m < *base {
                *base += (alpha * 4.0).min(1.0) * (m - *base);
            } else {
                *base += alpha * (m - *base);
            }
        }
        interfered
    }
}

/// Broadband-burst guard (see `DetectorParams::burst_slots`).
pub(super) struct BurstGuard {
    slots: usize,
    margin_db: f32,
    /// Blocks in which it held slots back.
    pub blocks: u64,
}

impl BurstGuard {
    pub fn new(slots: usize, margin_db: f32) -> Self {
        BurstGuard {
            slots,
            margin_db,
            blocks: 0,
        }
    }

    /// When many idle slots read over their close threshold (`close[i]`) at
    /// once, keep only the opens within the margin of the strongest signal.
    /// Only slots at least as strong as their neighbours (`peak[i]`) count,
    /// so one signal across several narrow grid windows counts once.
    pub fn apply(
        &mut self,
        states: &[SlotState],
        stats: &[SlotStats],
        close: &[f32],
        peak: &[bool],
        opens: &mut [bool],
    ) {
        if self.slots == 0 {
            return;
        }
        let idle = |i: usize| matches!(states[i], SlotState::Idle | SlotState::Attack { .. });
        let waiting = |i: usize, opens: &[bool]| idle(i) && opens[i];
        // Counted against the close threshold, not the open one:
        // overload products sit a few dB lower once band-wide rises are
        // taken out of the SNR (`common_ratio`), and an RSP1A at LNA 3
        // keyed up against by a nearby handheld then put only four or
        // five of them over the open threshold at once, too few to arm
        // the guard, so they opened one by one.
        let hot = (0..states.len())
            .filter(|&i| peak[i] && idle(i) && stats[i].snr_db >= close[i])
            .count();
        if hot < self.slots {
            return;
        }
        // The reference is the strongest signal anywhere, including
        // one already open: the transmitter causing the overload is
        // usually open before its products cross the threshold.
        let top = (0..states.len())
            .filter(|&i| waiting(i, opens) || !matches!(states[i], SlotState::Idle))
            .map(|i| stats[i].snr_db)
            .fold(f32::MIN, f32::max);
        let keep = top - self.margin_db;
        for i in 0..states.len() {
            if waiting(i, opens) && stats[i].snr_db < keep {
                opens[i] = false;
            }
        }
        self.blocks += 1;
    }
}
