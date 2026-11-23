//! Interference that raises a slot's thresholds instead of blocking it:
//! steady lines a slot sits on most of the time, and comb lines that pulse
//! on together.

use std::collections::VecDeque;

/// Per-slot learned background (see `DetectorParams::background_secs`).
pub(super) struct SteadyBackground {
    secs: f64,
    pct: f32,
    /// Per slot: 1 s means of the linear SNR, newest last.
    hist: Vec<VecDeque<f32>>,
    /// Per slot: the linear SNR summed over the current second.
    acc: Vec<f64>,
    /// Blocks summed into `acc` so far, and per second.
    n: u32,
    blocks: u32,
    /// Per slot: the learned background (dB), 0 until enough is held.
    pub db: Vec<f32>,
    scratch: Vec<f32>,
}

impl SteadyBackground {
    pub fn new(slots: usize, secs: f64, pct: f32, block_secs: f64) -> Self {
        SteadyBackground {
            secs,
            pct,
            hist: vec![VecDeque::new(); slots],
            acc: vec![0.0; slots],
            n: 0,
            blocks: (1.0 / block_secs).round().max(1.0) as u32,
            db: vec![0.0; slots],
            scratch: Vec::new(),
        }
    }

    /// Add slot `i`'s linear SNR this block.
    pub fn add(&mut self, i: usize, snr: f64) {
        self.acc[i] += snr;
    }

    /// A block's SNRs are all in; learn once a second's worth are.
    pub fn end_block(&mut self) {
        if self.secs > 0.0 {
            self.n += 1;
            if self.n >= self.blocks {
                self.learn();
            }
        }
    }

    /// Close out a second: push each slot's mean, drop what is older than
    /// `secs`, and take the quantile once enough is held.
    fn learn(&mut self) {
        let n = self.n as f64;
        let cap = self.secs.ceil().max(1.0) as usize;
        let need = cap.div_ceil(2).min(10);
        let pct = self.pct.clamp(0.0, 1.0);
        for i in 0..self.hist.len() {
            let h = &mut self.hist[i];
            h.push_back((self.acc[i] / n) as f32);
            while h.len() > cap {
                h.pop_front();
            }
            self.acc[i] = 0.0;
            self.db[i] = if h.len() >= need {
                self.scratch.clear();
                self.scratch.extend(h.iter().copied());
                let k = ((self.scratch.len() as f32 * pct) as usize).min(self.scratch.len() - 1);
                let (_, q, _) = self.scratch.select_nth_unstable_by(k, f32::total_cmp);
                (10.0 * q.max(1e-12).log10()).max(0.0)
            } else {
                0.0
            };
        }
        self.n = 0;
    }
}

/// Pulsed comb guard (see `DetectorParams::comb_teeth`).
pub(super) struct PulsedComb {
    teeth: usize,
    /// Rises in this many blocks count as together.
    window: u64,
    /// Blocks a comb line stays marked after it was last seen.
    hold: u64,
    /// Rises stronger than this (dB) are not comb lines.
    max_db: f32,
    /// Blocks over the open threshold that make a rise.
    attack_blocks: u64,
    /// Per slot: offset from the capture centre (Hz).
    offsets: Vec<f64>,
    /// Recent rises: (slot, block, SNR).
    rises: VecDeque<(usize, u64, f32)>,
    /// Per slot: the level (dB) it rose to as a comb line, and the block it
    /// was last seen as one.
    pub level: Vec<f32>,
    seen: Vec<u64>,
    /// Per slot: blocks in a row over the plain open threshold while idle.
    run: Vec<u64>,
    /// Comb events seen.
    pub events: u64,
}

impl PulsedComb {
    pub fn new(
        teeth: usize,
        window_secs: f64,
        hold_secs: f64,
        max_db: f32,
        attack_blocks: u64,
        block_secs: f64,
        offsets: Vec<f64>,
    ) -> Self {
        let slots = offsets.len();
        PulsedComb {
            teeth,
            window: (window_secs / block_secs).ceil() as u64,
            hold: (hold_secs / block_secs) as u64,
            max_db,
            attack_blocks,
            offsets,
            rises: VecDeque::new(),
            level: vec![0.0; slots],
            seen: vec![0; slots],
            run: vec![0; slots],
            events: 0,
        }
    }

    /// Whether slot `i` was over the plain open threshold this block while
    /// waiting to open (whether or not its threshold is raised, so a known
    /// comb keeps being seen). An attack's worth of blocks in a row is a
    /// rise at `snr`.
    pub fn observe(&mut self, i: usize, block: u64, snr: f32, over: bool) {
        if over {
            self.run[i] += 1;
            if self.run[i] == self.attack_blocks {
                self.rises.push_back((i, block, snr));
            }
        } else {
            self.run[i] = 0;
        }
    }

    /// Expire old lines and rises, then look for a comb among the slots
    /// that rose within the window. Returns the slots marked as comb lines
    /// this block, whose attacks the caller cancels.
    ///
    /// `open_on(i)` is, for an open (recording) slot over the plain open
    /// threshold, its SNR (dB) this block and the last block it came back
    /// from its hang time; `None` for any other slot. An open slot is never
    /// seen rising, so a comb line that opened before the comb was learned
    /// (at startup, say) would be held open by its own pulses for as long
    /// as the comb lasts. An open slot on the learned grid that came back
    /// within the window, so rose with the comb, is marked too: that
    /// raises its close threshold, and it closes.
    pub fn learn(&mut self, b: u64, open_on: impl Fn(usize) -> Option<(f32, u64)>) -> Vec<usize> {
        for i in 0..self.level.len() {
            if self.level[i] > 0.0 && b.saturating_sub(self.seen[i]) > self.hold {
                self.level[i] = 0.0;
            }
        }
        while self.rises.front().is_some_and(|r| r.1 + self.window < b) {
            self.rises.pop_front();
        }
        let teeth = self.teeth;
        if teeth == 0 || !self.rises.back().is_some_and(|r| r.1 == b) {
            return Vec::new();
        }
        // Per slot, the strongest rise in the window; strong ones are not
        // comb lines.
        let mut rose: Vec<(usize, f32)> = Vec::new();
        for &(i, _, snr) in &self.rises {
            match rose.iter_mut().find(|r| r.0 == i) {
                Some(r) => r.1 = r.1.max(snr),
                None => rose.push((i, snr)),
            }
        }
        rose.retain(|r| r.1 <= self.max_db);
        if rose.len() < teeth {
            return Vec::new();
        }
        let offs: Vec<f64> = rose.iter().map(|r| self.offsets[r.0]).collect();
        let Some((members, f0, spacing)) = comb_grid(&offs, teeth) else {
            return Vec::new();
        };
        self.events += 1;
        let mut marked = Vec::with_capacity(members.len());
        for k in members {
            let (i, snr) = rose[k];
            self.level[i] = self.level[i].max(snr);
            self.seen[i] = b;
            marked.push(i);
        }
        for i in 0..self.offsets.len() {
            let Some((snr, back)) = open_on(i) else {
                continue;
            };
            if back + self.window < b || snr > self.max_db || !on_grid(self.offsets[i], f0, spacing)
            {
                continue;
            }
            self.level[i] = self.level[i].max(snr);
            self.seen[i] = b;
        }
        self.rises.retain(|r| self.seen[r.0] != b);
        marked
    }
}

/// Which of these offsets (Hz) lie on one evenly spaced grid of at least
/// `teeth` lines, if any: the grid (spacing 50 kHz or more, from any pair
/// and its first few divisions) that holds the most of them, each within
/// 10 kHz of a line. Offsets within 30 kHz of each other count as one line.
#[cfg(test)]
pub(super) fn comb_members(offs: &[f64], teeth: usize) -> Option<Vec<usize>> {
    comb_grid(offs, teeth).map(|g| g.0)
}

/// Whether `f` (Hz) lies within 10 kHz of a line of the grid through `f0`
/// spaced `spacing` apart.
fn on_grid(f: f64, f0: f64, spacing: f64) -> bool {
    let k = (f - f0) / spacing;
    (k - k.round()).abs() * spacing <= 10e3
}

/// The comb members (see `comb_members`), with the grid they lie on: (members, a line's
/// offset, spacing), all Hz.
fn comb_grid(offs: &[f64], teeth: usize) -> Option<(Vec<usize>, f64, f64)> {
    let mut order: Vec<usize> = (0..offs.len()).collect();
    order.sort_by(|&a, &b| offs[a].total_cmp(&offs[b]));
    // Lines: (centre, members).
    let mut lines: Vec<(f64, Vec<usize>)> = Vec::new();
    for k in order {
        let f = offs[k];
        match lines.last_mut() {
            Some((c, m)) if f - *c <= 30e3 => {
                *c = (*c * m.len() as f64 + f) / (m.len() + 1) as f64;
                m.push(k);
            }
            _ => lines.push((f, vec![k])),
        }
    }
    if lines.len() < teeth {
        return None;
    }
    let on = |f0: f64, s: f64| -> Vec<usize> {
        (0..lines.len())
            .filter(|&j| on_grid(lines[j].0, f0, s))
            .collect()
    };
    let mut best: Vec<usize> = Vec::new();
    let mut grid = (0.0, 0.0);
    for a in 0..lines.len() {
        for b in a + 1..lines.len() {
            let d = lines[b].0 - lines[a].0;
            for div in 1..=4 {
                let s = d / div as f64;
                if s < 50e3 {
                    break;
                }
                let got = on(lines[a].0, s);
                // A grid must be filled in, not just two lines far apart
                // with the rest landing on a fine spacing by chance: at
                // least half the lines between the outermost members.
                let span = (lines[*got.last().unwrap()].0 - lines[got[0]].0) / s;
                if got.len() > best.len() && got.len() as f64 >= (span.round() + 1.0) / 2.0 {
                    best = got;
                    grid = (lines[a].0, s);
                }
            }
        }
    }
    (best.len() >= teeth).then(|| {
        let members = best.iter().flat_map(|&j| lines[j].1.clone()).collect();
        (members, grid.0, grid.1)
    })
}

/// The comb pattern among these raised slots' offsets (Hz), if there is
/// one: the commonest spacing (Hz, to 5 kHz) between neighbouring raised
/// lines, and how many lines sit on it. See `Detector::comb`.
pub(super) fn comb_pattern(mut raised: Vec<f64>) -> Option<(f64, usize)> {
    // Centres of raised runs (neighbouring raised slots within 30 kHz
    // are one line).
    raised.sort_by(f64::total_cmp);
    let mut lines: Vec<(f64, usize)> = Vec::new();
    for f in raised {
        match lines.last_mut() {
            Some((sum, n)) if f - *sum / *n as f64 <= 30e3 => {
                *sum += f;
                *n += 1;
            }
            _ => lines.push((f, 1)),
        }
    }
    let centres: Vec<f64> = lines.iter().map(|&(s, n)| s / n as f64).collect();
    let mut counts: Vec<(i64, usize)> = Vec::new();
    for w in centres.windows(2) {
        let d = ((w[1] - w[0]) / 5e3).round() as i64;
        match counts.iter_mut().find(|(k, _)| (*k - d).abs() <= 1) {
            Some(c) => c.1 += 1,
            None => counts.push((d, 1)),
        }
    }
    let (d, c) = counts.into_iter().max_by_key(|&(_, c)| c)?;
    if c < 3 {
        return None;
    }
    let spacing = d as f64 * 5e3;
    // Lines on that comb (within 10 kHz of a multiple of the spacing
    // from the first line on it).
    let teeth = centres
        .iter()
        .map(|&f| {
            centres
                .iter()
                .filter(|&&g| {
                    let k = (g - f) / spacing;
                    (k - k.round()).abs() * spacing <= 10e3
                })
                .count()
        })
        .max()
        .unwrap_or(0);
    Some((spacing, teeth))
}
