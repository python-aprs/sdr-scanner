//! GMSK bit recovery and framing.
//!
//! Channel IQ through the FM discriminator, resampled to 48 kHz (10 samples
//! a bit) and averaged over a bit. Syncs are found by correlating 24 bits
//! against their pattern, which also gives the timing, the slicing level
//! (any frequency offset) and the polarity.
//!
//! A frame sync must be followed by a header whose CRC checks. A data sync
//! found on its own (late entry) must be followed by another 21 frames on,
//! and the frames between are held until it is.

use super::frame::{self, DATA_SYNC, END, FRAME_BITS, FRAME_SYNC, SUPERFRAME, pattern_bits};
use super::header::{self, HEADER_BITS, HEADER_BYTES};
use crate::dsp::{FmDiscriminator, Resampler};
use num_complex::Complex32;

pub const RATE: f64 = 48_000.0;
pub const SPS: usize = 10;
pub const FRAME_SAMPLES: u64 = (FRAME_BITS * SPS) as u64;
const SEARCH_R: f32 = 0.85;
const TRACK_R: f32 = 0.6;
/// Missed data syncs in a row before the receiver gives up.
const LOST_AFTER: u32 = 2;

#[derive(Debug, Clone, PartialEq)]
pub enum RxEvent {
    /// A header that checked; `at` is its first bit (48 kHz sample).
    Header {
        at: u64,
        lock: u32,
        bytes: [u8; HEADER_BYTES],
    },
    /// A voice frame; `index` counts from the frame after the header
    /// (multiples of 21 carry the data sync).
    Frame {
        at: u64,
        lock: u32,
        index: u64,
        bits: [u8; FRAME_BITS],
    },
    /// The end pattern, or the syncs stopped: `at` is just past the last
    /// frame.
    End { at: u64, lock: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Search,
    /// Frame `index` starts at `next`.
    Locked {
        next: u64,
        index: u64,
        misses: u32,
        /// Late entry, waiting for the second data sync.
        tentative: bool,
    },
}

pub struct Receiver {
    disc: FmDiscriminator,
    resamp: Resampler,
    avg: Vec<f32>,
    tmp: Vec<f32>,
    tmp2: Vec<f32>,
    buf: Vec<f32>,
    base: u64,
    state: State,
    scan: u64,
    /// Slicing: bit = 1 when (x - level) * polarity < 0.
    level: f32,
    polarity: f32,
    lock: u32,
    held: Vec<RxEvent>,
    held_from: u64,
    fs: [f32; 24],
    ds: [f32; 24],
    delay: f64,
    pub syncs: u64,
}

/// A pattern as signs: bit 1 is the lower tone.
fn signs(p: u64) -> [f32; 24] {
    let b = pattern_bits(p, 24);
    std::array::from_fn(|i| if b[i] == 1 { -1.0 } else { 1.0 })
}

impl Receiver {
    pub fn new(rate: f64) -> Self {
        let resamp = Resampler::new(rate, RATE, 3000.0, 2000.0);
        let delay = resamp.delay_out() + (SPS as f64 - 1.0) / 2.0;
        Receiver {
            disc: FmDiscriminator::new(rate, 1.0),
            resamp,
            avg: vec![0.0; SPS - 1],
            tmp: Vec::new(),
            tmp2: Vec::new(),
            buf: Vec::new(),
            base: 0,
            state: State::Search,
            scan: 0,
            level: 0.0,
            polarity: 1.0,
            lock: 0,
            held: Vec::new(),
            held_from: 0,
            fs: signs(FRAME_SYNC),
            ds: signs(DATA_SYNC),
            delay,
            syncs: 0,
        }
    }

    pub fn delay(&self) -> f64 {
        self.delay
    }

    pub fn push(&mut self, iq: &[Complex32], out: &mut Vec<RxEvent>) {
        self.tmp.clear();
        self.disc.process(iq, &mut self.tmp);
        self.tmp2.clear();
        self.resamp.process(&self.tmp, &mut self.tmp2);
        self.avg.extend_from_slice(&self.tmp2);
        for i in 0..self.tmp2.len() {
            let s: f32 = self.avg[i..i + SPS].iter().sum();
            self.buf.push(s / SPS as f32);
        }
        let drop = self.avg.len() - (SPS - 1);
        self.avg.drain(..drop);
        self.run(out);
    }

    fn at(&self, i: u64) -> f32 {
        self.buf[(i - self.base) as usize]
    }

    fn end(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    /// Correlation of the 24 bits from `pos` with `s`, and the mean level.
    fn fit(&self, pos: u64, s: &[f32; 24]) -> (f32, f32) {
        let n = 24.0;
        let (mut sx, mut ss, mut sxx, mut sss, mut sxs) = (0f32, 0f32, 0f32, 0f32, 0f32);
        for (i, &v) in s.iter().enumerate() {
            let x = self.at(pos + (i * SPS) as u64);
            sx += x;
            ss += v;
            sxx += x * x;
            sss += v * v;
            sxs += x * v;
        }
        let cxs = sxs - sx * ss / n;
        let cxx = sxx - sx * sx / n;
        let css = sss - ss * ss / n;
        if cxx <= 0.0 {
            return (0.0, 0.0);
        }
        let r = cxs / (cxx * css).sqrt();
        let a = cxs / css;
        (r, (sx - a * ss) / n)
    }

    fn bits<const N: usize>(&self, pos: u64) -> [u8; N] {
        std::array::from_fn(|i| {
            ((self.at(pos + (i * SPS) as u64) - self.level) * self.polarity < 0.0) as u8
        })
    }

    /// Bits that differ from the end pattern at `pos`.
    fn end_errs(&self, pos: u64) -> u32 {
        let b: [u8; 48] = self.bits(pos);
        b.iter()
            .zip(pattern_bits(END, 48))
            .filter(|(a, e)| *a != e)
            .count() as u32
    }

    fn best(&self, centre: u64, s: &[f32; 24]) -> (u64, f32, f32) {
        let mut best = (centre, 0f32, 0f32);
        for p in centre.saturating_sub(4).max(self.base)..=centre + 4 {
            let (r, l) = self.fit(p, s);
            if r.abs() > best.1.abs() {
                best = (p, r, l);
            }
        }
        best
    }

    const SYNC: u64 = (24 * SPS) as u64;
    /// Offset of a frame's data sync from its start.
    const SYNC_AT: u64 = (frame::VOICE_BITS * SPS) as u64;

    fn lose(&mut self, at: u64, out: &mut Vec<RxEvent>, scan_from: u64) {
        if let State::Locked { tentative, .. } = self.state {
            if tentative {
                self.held.clear();
                self.scan = self.held_from + 1;
            } else {
                out.push(RxEvent::End {
                    at,
                    lock: self.lock,
                });
                self.scan = scan_from;
            }
        }
        self.state = State::Search;
    }

    fn run(&mut self, out: &mut Vec<RxEvent>) {
        loop {
            match self.state {
                State::Search => {
                    if self.scan < self.base {
                        self.scan = self.base;
                    }
                    let need = Self::SYNC + (HEADER_BITS * SPS) as u64 + 16;
                    let mut found = None;
                    while self.scan + need <= self.end() {
                        let (rf, _) = self.fit(self.scan, &self.fs);
                        let (rd, _) = self.fit(self.scan, &self.ds);
                        if rf.abs() >= SEARCH_R || rd.abs() >= SEARCH_R {
                            let header = rf.abs() >= rd.abs();
                            let s = if header { self.fs } else { self.ds };
                            let (p, r, l) = self.best(self.scan + 4, &s);
                            found = Some((header, p, r, l));
                            break;
                        }
                        self.scan += 1;
                    }
                    let Some((header, pos, r, level)) = found else {
                        break;
                    };
                    self.level = level;
                    self.polarity = r.signum();
                    let after = pos + Self::SYNC;
                    if header {
                        let bits: [u8; HEADER_BITS] = self.bits(after);
                        match header::decode(&bits) {
                            Some(bytes) => {
                                self.lock += 1;
                                self.syncs += 1;
                                out.push(RxEvent::Header {
                                    at: after,
                                    lock: self.lock,
                                    bytes,
                                });
                                self.state = State::Locked {
                                    next: after + (HEADER_BITS * SPS) as u64,
                                    index: 0,
                                    misses: 0,
                                    tentative: false,
                                };
                            }
                            None => self.scan = pos + 1,
                        }
                    } else {
                        // The sync closes frame 0 of a superframe; voice
                        // picks up with frame 1.
                        self.lock += 1;
                        self.syncs += 1;
                        self.held.clear();
                        self.held_from = pos;
                        self.state = State::Locked {
                            next: after,
                            index: 1,
                            misses: 0,
                            tentative: true,
                        };
                    }
                }
                State::Locked {
                    next,
                    index,
                    misses,
                    tentative,
                } => {
                    if next + FRAME_SAMPLES + 8 > self.end() {
                        break;
                    }
                    if !tentative && self.end_errs(next) <= 3 {
                        out.push(RxEvent::End {
                            at: next,
                            lock: self.lock,
                        });
                        self.state = State::Search;
                        self.scan = next + 1;
                        continue;
                    }
                    let bits: [u8; FRAME_BITS] = self.bits(next);
                    let mut start = next;
                    let mut misses = misses;
                    let mut tentative = tentative;
                    if index % SUPERFRAME as u64 == 0 {
                        let (p, r, l) = self.best(next + Self::SYNC_AT, &self.ds);
                        if r.abs() >= TRACK_R && r.signum() == self.polarity {
                            self.level = l;
                            self.syncs += 1;
                            start = p - Self::SYNC_AT;
                            misses = 0;
                            if tentative {
                                tentative = false;
                                out.append(&mut self.held);
                            }
                        } else {
                            misses += 1;
                            if tentative || misses >= LOST_AFTER {
                                self.lose(next, out, next + 1);
                                continue;
                            }
                        }
                    }
                    let ev = RxEvent::Frame {
                        at: next,
                        lock: self.lock,
                        index,
                        bits,
                    };
                    if tentative {
                        self.held.push(ev);
                    } else {
                        out.push(ev);
                    }
                    self.state = State::Locked {
                        next: start + FRAME_SAMPLES,
                        index: index + 1,
                        misses,
                        tentative,
                    };
                }
            }
        }
        let keep_from = match self.state {
            State::Search => self.scan,
            State::Locked {
                next, tentative, ..
            } => {
                if tentative {
                    self.held_from.min(next)
                } else {
                    next
                }
            }
        }
        .saturating_sub(16);
        if keep_from > self.base {
            let n = ((keep_from - self.base) as usize).min(self.buf.len());
            self.buf.drain(..n);
            self.base += n as u64;
        }
    }
}
