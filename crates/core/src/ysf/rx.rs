//! C4FM symbol recovery and frame framing.
//!
//! As for DMR: channel IQ through the FM discriminator, resampled to
//! 48 kHz (10 samples a symbol) and matched-filtered with a root raised
//! cosine. Every 100 ms frame opens with a 20-symbol sync, so once a frame
//! is found the receiver steps frame by frame, re-aligning on each sync and
//! taking the symbol levels from it (a straight-line fit of the received
//! sync against the pattern: no fixed deviation, and an inverted receiver
//! just fits a negative slope).
//!
//! A sync alone is weak evidence in noise, so a search only locks when the
//! frame's FICH also decodes.

use super::frame::{self, FRAME_BITS, FRAME_SYMBOLS, SYNC, SYNC_SYMBOLS};
use crate::dmr::burst::dibit_symbol;
use crate::dmr::rx::rrc;
use crate::dsp::{FmDiscriminator, Resampler};
use num_complex::Complex32;

pub const RATE: f64 = 48_000.0;
pub const SPS: usize = 10;
pub const FRAME_SAMPLES: u64 = (FRAME_SYMBOLS * SPS) as u64;
/// Correlation of the received sync with the pattern needed to try a
/// frame while searching, and to re-align on while locked.
const SEARCH_R: f32 = 0.8;
const TRACK_R: f32 = 0.6;
/// Frames in a row without a FICH before the receiver goes back to
/// searching.
const LOST_AFTER: u32 = 3;

/// One frame as received.
#[derive(Debug, Clone)]
pub struct RxFrame {
    /// 48 kHz sample index of the frame's first symbol.
    pub start: u64,
    /// Counts locks, so a new one can reset what depended on the old.
    pub lock: u32,
    pub fich: Option<frame::Fich>,
    pub bits: Box<[u8; FRAME_BITS]>,
    /// Correlation of the sync with the pattern (0 when there was none to
    /// re-align on).
    pub r: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Search,
    Locked { next: u64, misses: u32 },
}

/// The sync's symbols (+3, +1, -1, -3).
fn sync_symbols() -> [f32; SYNC_SYMBOLS] {
    std::array::from_fn(|i| dibit_symbol(((SYNC >> (38 - 2 * i)) & 3) as u8) as f32)
}

pub struct Receiver {
    disc: FmDiscriminator,
    resamp: Resampler,
    rrc: Vec<f32>,
    hist: Vec<f32>,
    tmp: Vec<f32>,
    tmp2: Vec<f32>,
    /// Filtered 48 kHz signal; `buf[0]` is sample `base`.
    buf: Vec<f32>,
    base: u64,
    state: State,
    scan: u64,
    /// Received value = `levels.0` * symbol + `levels.1`.
    levels: (f32, f32),
    lock: u32,
    sync: [f32; SYNC_SYMBOLS],
    delay: f64,
    pub syncs: u64,
}

impl Receiver {
    /// Fed channel IQ at `rate`.
    pub fn new(rate: f64) -> Self {
        let resamp = Resampler::new(rate, RATE, 4000.0, 2000.0);
        let rrc = rrc(SPS, 8, 0.2);
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
            levels: (1.0, 0.0),
            lock: 0,
            sync: sync_symbols(),
            delay,
            syncs: 0,
        }
    }

    /// Delay from channel IQ to frame positions, in 48 kHz samples.
    pub fn delay(&self) -> f64 {
        self.delay
    }

    pub fn locked(&self) -> bool {
        matches!(self.state, State::Locked { .. })
    }

    pub fn push(&mut self, iq: &[Complex32], out: &mut Vec<RxFrame>) {
        self.tmp.clear();
        self.disc.process(iq, &mut self.tmp);
        self.tmp2.clear();
        self.resamp.process(&self.tmp, &mut self.tmp2);
        let n = self.rrc.len();
        self.hist.extend_from_slice(&self.tmp2);
        for i in 0..self.tmp2.len() {
            let w = &self.hist[i..i + n];
            self.buf
                .push(w.iter().zip(&self.rrc).map(|(a, b)| a * b).sum());
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

    /// Fit of the sync at `pos` (first symbol): correlation, slope, offset.
    fn fit(&self, pos: u64) -> (f32, f32, f32) {
        let n = SYNC_SYMBOLS as f32;
        let (mut sx, mut ss, mut sxx, mut sss, mut sxs) = (0f32, 0f32, 0f32, 0f32, 0f32);
        for (i, &s) in self.sync.iter().enumerate() {
            let x = self.at(pos + (i * SPS) as u64);
            sx += x;
            ss += s;
            sxx += x * x;
            sss += s * s;
            sxs += x * s;
        }
        let cxs = sxs - sx * ss / n;
        let cxx = sxx - sx * sx / n;
        let css = sss - ss * ss / n;
        if cxx <= 0.0 {
            return (0.0, 1.0, 0.0);
        }
        let r = cxs / (cxx * css).sqrt();
        let a = cxs / css;
        let b = (sx - a * ss) / n;
        (r, a, b)
    }

    fn frame(&self, start: u64, r: f32) -> RxFrame {
        let (a, b) = self.levels;
        let mut bits = Box::new([0u8; FRAME_BITS]);
        for s in 0..FRAME_SYMBOLS {
            let v = (self.at(start + (s * SPS) as u64) - b) / a;
            let d = if v > 2.0 {
                0b01
            } else if v > 0.0 {
                0b00
            } else if v > -2.0 {
                0b10
            } else {
                0b11
            };
            bits[2 * s] = d >> 1;
            bits[2 * s + 1] = d & 1;
        }
        let fich = frame::fich(&bits[..]);
        RxFrame {
            start,
            lock: self.lock,
            fich,
            bits,
            r,
        }
    }

    const AFTER: u64 = ((FRAME_SYMBOLS - 1) * SPS) as u64 + 1;

    fn run(&mut self, out: &mut Vec<RxFrame>) {
        loop {
            match self.state {
                State::Search => {
                    if self.scan < self.base {
                        self.scan = self.base;
                    }
                    let need = Self::AFTER + 8;
                    let mut found = None;
                    while self.scan + need <= self.end() {
                        let f = self.fit(self.scan);
                        if f.0.abs() >= SEARCH_R {
                            let mut best = (self.scan, f);
                            for p in self.scan + 1..self.scan + 8 {
                                let t = self.fit(p);
                                if t.0.abs() > best.1.0.abs() {
                                    best = (p, t);
                                }
                            }
                            found = Some(best);
                            break;
                        }
                        self.scan += 1;
                    }
                    let Some((pos, (r, a, b))) = found else {
                        break;
                    };
                    let old = self.levels;
                    self.levels = (a, b);
                    let mut f = self.frame(pos, r.abs());
                    if f.fich.is_none() {
                        self.levels = old;
                        self.scan = pos + 1;
                        continue;
                    }
                    self.lock += 1;
                    self.syncs += 1;
                    f.lock = self.lock;
                    out.push(f);
                    self.state = State::Locked {
                        next: pos + FRAME_SAMPLES,
                        misses: 0,
                    };
                }
                State::Locked { next, misses } => {
                    if next + Self::AFTER + 4 > self.end() {
                        break;
                    }
                    let mut best = (next, (0f32, 0f32, 0f32));
                    for p in next - 4..=next + 4 {
                        let t = self.fit(p);
                        if t.0.abs() > best.1.0.abs() {
                            best = (p, t);
                        }
                    }
                    let (pos, (r, a, b)) = best;
                    // A sync of the same polarity as the lock's.
                    let start = if r.abs() >= TRACK_R && (a > 0.0) == (self.levels.0 > 0.0) {
                        self.levels = (a, b);
                        self.syncs += 1;
                        pos
                    } else {
                        next
                    };
                    let f = self.frame(start, if start == pos { r.abs() } else { 0.0 });
                    let misses = if f.fich.is_some() { 0 } else { misses + 1 };
                    out.push(f);
                    self.state = if misses >= LOST_AFTER {
                        self.scan = start + 1;
                        State::Search
                    } else {
                        State::Locked {
                            next: start + FRAME_SAMPLES,
                            misses,
                        }
                    };
                }
            }
        }
        let keep_from = match self.state {
            State::Search => self.scan,
            State::Locked { next, .. } => next.saturating_sub(8),
        };
        if keep_from > self.base {
            let n = ((keep_from - self.base) as usize).min(self.buf.len());
            self.buf.drain(..n);
            self.base += n as u64;
        }
    }
}
