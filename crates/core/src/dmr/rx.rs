//! 4FSK symbol recovery and burst framing.
//!
//! Channel IQ goes through the FM discriminator (in Hz), is resampled to
//! 48 kHz (10 samples a symbol) and matched-filtered. Bursts are found by
//! their 24-symbol sync: once one is found the receiver stays on the 30 ms
//! TDMA frame grid, re-aligning on every sync it sees, and slices each
//! frame's CACH and burst with the symbol levels the last sync showed.
//! Symbol clocks are good to a few ppm, so timing holds between syncs
//! (360 ms apart in a voice superframe) without a tracking loop.

use super::burst::{
    self, BURST_BITS, BURST_SYMBOLS, CACH_SYMBOLS, FRAME_SYMBOLS, SYMBOL_RATE, SYNC_START,
    SYNC_SYMBOLS, Source, Sync, VOICE_SYNCS,
};
use crate::dsp::{FmDiscriminator, Resampler};
use num_complex::Complex32;

pub const RATE: f64 = 48_000.0;
pub const SPS: usize = 10;
/// Separation of the sync's two symbol groups over their spread needed to
/// call it a sync.
const SYNC_SCORE: f32 = 3.5;
/// Frames on the grid without a sync before the receiver goes back to
/// searching (600 ms: past a voice superframe on one slot).
const LOST_AFTER: u32 = 20;
const ROLL_OFF: f64 = 0.2;
const RRC_SPAN: usize = 8;

/// One burst as received.
#[derive(Debug, Clone)]
pub struct RxBurst {
    /// 48 kHz sample index of the burst's first symbol.
    pub start: u64,
    /// Frames since the receiver locked (the lock's burst is frame 0).
    pub frame: u64,
    /// Counts locks, so a new one can reset what depended on the old.
    pub lock: u32,
    pub sync: Option<Sync>,
    /// Source of the last sync seen on this lock (base station bursts carry
    /// a CACH).
    pub source: Source,
    pub bits: [u8; BURST_BITS],
    pub cach: [u8; 24],
    /// Sync score when there was one.
    pub score: f32,
}

/// Root raised cosine taps, unity DC gain.
pub fn rrc(sps: usize, span: usize, beta: f64) -> Vec<f32> {
    let n = sps * span + 1;
    let mid = (n / 2) as f64;
    let mut h: Vec<f64> = (0..n)
        .map(|i| {
            let t = (i as f64 - mid) / sps as f64;
            if t.abs() < 1e-9 {
                1.0 - beta + 4.0 * beta / std::f64::consts::PI
            } else if (t.abs() - 1.0 / (4.0 * beta)).abs() < 1e-9 {
                let pi4b = std::f64::consts::PI / (4.0 * beta);
                beta / 2f64.sqrt()
                    * ((1.0 + 2.0 / std::f64::consts::PI) * pi4b.sin()
                        + (1.0 - 2.0 / std::f64::consts::PI) * pi4b.cos())
            } else {
                let pt = std::f64::consts::PI * t;
                ((pt * (1.0 - beta)).sin() + 4.0 * beta * t * (pt * (1.0 + beta)).cos())
                    / (pt * (1.0 - (4.0 * beta * t).powi(2)))
            }
        })
        .collect();
    let s: f64 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= s);
    h.into_iter().map(|v| v as f32).collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Search,
    /// Next frame's burst starts at this 48 kHz index.
    Locked {
        next: u64,
        since_sync: u32,
    },
}

pub struct Receiver {
    disc: FmDiscriminator,
    resamp: Resampler,
    rrc: Vec<f32>,
    /// Matched filter input history.
    hist: Vec<f32>,
    tmp: Vec<f32>,
    tmp2: Vec<f32>,
    /// Filtered 48 kHz signal; `buf[0]` is sample `base`.
    buf: Vec<f32>,
    base: u64,
    state: State,
    /// Next sync position to try while searching.
    scan: u64,
    /// Symbol levels (+3, -3) from the last sync.
    levels: (f32, f32),
    frame: u64,
    lock: u32,
    source: Source,
    signs: Vec<(Source, [f32; SYNC_SYMBOLS])>,
    /// Delay from channel IQ to the filtered signal, 48 kHz samples.
    delay: f64,
    pub syncs: u64,
}

impl Receiver {
    /// Fed channel IQ at `rate`.
    pub fn new(rate: f64) -> Self {
        let resamp = Resampler::new(rate, RATE, 4000.0, 2000.0);
        let rrc = rrc(SPS, RRC_SPAN, ROLL_OFF);
        let delay = resamp.delay_out() + (rrc.len() / 2) as f64;
        Receiver {
            disc: FmDiscriminator::new(rate, 1.0),
            resamp,
            hist: vec![0.0; rrc.len() - 1],
            rrc,
            tmp: Vec::new(),
            tmp2: Vec::new(),
            buf: Vec::new(),
            base: 0,
            state: State::Search,
            scan: 0,
            levels: (burst::OUTER_DEV as f32, -burst::OUTER_DEV as f32),
            frame: 0,
            lock: 0,
            source: Source::Base,
            signs: VOICE_SYNCS
                .iter()
                .map(|&(s, p)| (s, burst::sync_signs(p).map(|v| v as f32)))
                .collect(),
            delay,
            syncs: 0,
        }
    }

    /// Delay from channel IQ to burst positions, in 48 kHz samples.
    pub fn delay(&self) -> f64 {
        self.delay
    }

    pub fn locked(&self) -> bool {
        matches!(self.state, State::Locked { .. })
    }

    pub fn push(&mut self, iq: &[Complex32], out: &mut Vec<RxBurst>) {
        self.tmp.clear();
        self.disc.process(iq, &mut self.tmp);
        self.tmp2.clear();
        self.resamp.process(&self.tmp, &mut self.tmp2);
        // Matched filter.
        let n = self.rrc.len();
        self.hist.extend_from_slice(&self.tmp2);
        for i in 0..self.tmp2.len() {
            let w = &self.hist[i..i + n];
            let acc: f32 = w.iter().zip(&self.rrc).map(|(a, b)| a * b).sum();
            self.buf.push(acc);
        }
        let drop = self.hist.len() - (n - 1);
        self.hist.drain(..drop);
        self.run(out);
    }

    fn at(&self, i: u64) -> f32 {
        self.buf[(i - self.base) as usize]
    }

    fn end(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    /// Best sync at `pos` (first sync symbol): (score, source, voice, hi, lo).
    fn score(&self, pos: u64) -> (f32, Source, bool, f32, f32) {
        let mut x = [0f32; SYNC_SYMBOLS];
        for (i, v) in x.iter_mut().enumerate() {
            *v = self.at(pos + (i * SPS) as u64);
        }
        let sum: f32 = x.iter().sum();
        let sq: f32 = x.iter().map(|v| v * v).sum();
        let mut best = (0.0f32, Source::Base, true, 0.0, 0.0);
        for (src, s) in &self.signs {
            let (mut sp, mut qp, mut np) = (0.0f32, 0.0f32, 0.0f32);
            for (v, sg) in x.iter().zip(s) {
                if *sg > 0.0 {
                    sp += v;
                    qp += v * v;
                    np += 1.0;
                }
            }
            let nm = SYNC_SYMBOLS as f32 - np;
            let (sm, qm) = (sum - sp, sq - qp);
            let (mp, mm) = (sp / np, sm / nm);
            let var =
                ((qp - np * mp * mp) + (qm - nm * mm * mm)).max(0.0) / (SYNC_SYMBOLS - 2) as f32;
            let sep = mp - mm;
            let floor = 1e-3 * (mp.abs() + mm.abs()) + 1e-3;
            let score = sep / (var.sqrt() + floor);
            if score.abs() > best.0.abs() {
                // A data sync is the voice sync negated.
                let (hi, lo) = if score > 0.0 { (mp, mm) } else { (mm, mp) };
                best = (score, *src, score > 0.0, hi, lo);
            }
        }
        best
    }

    fn slice(&self, pos: u64) -> u8 {
        let v = self.at(pos);
        let (hi, lo) = self.levels;
        let mid = (hi + lo) / 2.0;
        let d = (hi - lo) / 3.0;
        if v > mid + d {
            0b01
        } else if v > mid {
            0b00
        } else if v > mid - d {
            0b10
        } else {
            0b11
        }
    }

    fn burst(&self, start: u64, sync: Option<Sync>, score: f32) -> RxBurst {
        let mut bits = [0u8; BURST_BITS];
        for s in 0..BURST_SYMBOLS {
            let d = self.slice(start + (s * SPS) as u64);
            bits[2 * s] = d >> 1;
            bits[2 * s + 1] = d & 1;
        }
        let mut cach = [0u8; 24];
        let c0 = start.saturating_sub((CACH_SYMBOLS * SPS) as u64);
        if c0 >= self.base {
            for s in 0..CACH_SYMBOLS {
                let d = self.slice(c0 + (s * SPS) as u64);
                cach[2 * s] = d >> 1;
                cach[2 * s + 1] = d & 1;
            }
        }
        RxBurst {
            start,
            frame: self.frame,
            lock: self.lock,
            sync,
            source: self.source,
            bits,
            cach,
            score,
        }
    }

    /// Samples needed after a burst start to slice the whole burst.
    const AFTER: u64 = ((BURST_SYMBOLS - 1) * SPS) as u64 + 1;
    const SYNC_OFF: u64 = (SYNC_START * SPS) as u64;

    fn run(&mut self, out: &mut Vec<RxBurst>) {
        loop {
            match self.state {
                State::Search => {
                    let first = self.base + Self::SYNC_OFF;
                    if self.scan < first {
                        self.scan = first;
                    }
                    // Look a few samples past a hit for its peak.
                    let need = Self::AFTER - Self::SYNC_OFF + 8;
                    let mut found = None;
                    while self.scan + need <= self.end() {
                        let s = self.score(self.scan);
                        if s.0.abs() >= SYNC_SCORE {
                            let mut best = (self.scan, s);
                            for p in self.scan + 1..self.scan + 8 {
                                let t = self.score(p);
                                if t.0.abs() > best.1.0.abs() {
                                    best = (p, t);
                                }
                            }
                            found = Some(best);
                            break;
                        }
                        self.scan += 1;
                    }
                    let Some((pos, (score, source, voice, hi, lo))) = found else {
                        break;
                    };
                    self.lock += 1;
                    self.frame = 0;
                    self.source = source;
                    self.levels = (hi, lo);
                    self.syncs += 1;
                    let start = pos - Self::SYNC_OFF;
                    out.push(self.burst(start, Some(Sync { source, voice }), score.abs()));
                    self.state = State::Locked {
                        next: start + (FRAME_SYMBOLS * SPS) as u64,
                        since_sync: 0,
                    };
                }
                State::Locked { next, since_sync } => {
                    if next + Self::AFTER + 4 > self.end() {
                        break;
                    }
                    self.frame += 1;
                    let mut best = (0u64, (0f32, Source::Base, true, 0f32, 0f32));
                    let centre = next + Self::SYNC_OFF;
                    for p in centre - 4..=centre + 4 {
                        let t = self.score(p);
                        if t.0.abs() > best.1.0.abs() {
                            best = (p, t);
                        }
                    }
                    let (pos, (score, source, voice, hi, lo)) = best;
                    let (start, sync, since) = if score.abs() >= SYNC_SCORE {
                        self.source = source;
                        self.levels = (hi, lo);
                        self.syncs += 1;
                        (pos - Self::SYNC_OFF, Some(Sync { source, voice }), 0)
                    } else {
                        (next, None, since_sync + 1)
                    };
                    out.push(self.burst(start, sync, score.abs()));
                    self.state = if since > LOST_AFTER {
                        self.scan = start + Self::SYNC_OFF + 1;
                        State::Search
                    } else {
                        State::Locked {
                            next: start + (FRAME_SYMBOLS * SPS) as u64,
                            since_sync: since,
                        }
                    };
                }
            }
        }
        // Keep what the next burst (and its CACH) can still need.
        let keep_from = match self.state {
            State::Search => self.scan.saturating_sub(Self::SYNC_OFF),
            State::Locked { next, .. } => next,
        }
        .saturating_sub((CACH_SYMBOLS * SPS + 8) as u64);
        if keep_from > self.base {
            let n = ((keep_from - self.base) as usize).min(self.buf.len());
            self.buf.drain(..n);
            self.base += n as u64;
        }
    }

    /// Symbol rate in 48 kHz samples, for converting burst positions.
    pub fn samples_per_frame() -> u64 {
        (FRAME_SYMBOLS * SPS) as u64
    }
}

/// Seconds of one TDMA frame.
pub const FRAME_SECS: f64 = FRAME_SYMBOLS as f64 / SYMBOL_RATE;
