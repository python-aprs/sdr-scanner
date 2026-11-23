//! 1200-baud AFSK (Bell 202) packet decoding: AX.25 frames from an FM
//! discriminator's output (PLAN.md 3.8, Phase 5).
//!
//! Mark (1200 Hz) and space (2200 Hz) are measured by correlating the
//! audio against both tones over one bit. Transmitters' pre-emphasis and
//! receivers' filters tilt one tone against the other by up to ~10 dB, so,
//! as Direwolf does, several slicers compare mark with space at different
//! weights, each with its own clock recovery and HDLC deframer. A frame
//! any of them decodes with a good FCS is kept once.
//!
//! This only gets the frames' text out (source, destination, path, info),
//! not APRS fields.

use std::collections::VecDeque;
use std::f64::consts::PI;

pub const BAUD: f64 = 1200.0;
pub const MARK_HZ: f64 = 1200.0;
pub const SPACE_HZ: f64 = 2200.0;

/// Weights on the space tone, one slicer each (-6 to +6 dB).
const SPACE_GAINS: [f32; 5] = [0.5, 0.7, 1.0, 1.4, 2.0];
/// Longest frame accepted (AX.25 allows 256 info bytes plus headers).
const MAX_FRAME: usize = 340;
/// Shortest: two addresses, control and FCS.
const MIN_FRAME: usize = 17;

/// CRC-16/X.25 (the AX.25 FCS).
pub fn fcs(data: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x8408
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// One decoded AX.25 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub dest: String,
    pub source: String,
    /// Digipeaters, with `*` on those that have repeated it.
    pub path: Vec<String>,
    pub control: u8,
    pub pid: Option<u8>,
    pub info: Vec<u8>,
    /// The frame as received, without the FCS (what KISS carries).
    pub raw: Vec<u8>,
}

fn address(b: &[u8]) -> String {
    let call: String = b[..6]
        .iter()
        .map(|&c| (c >> 1) as char)
        .collect::<String>()
        .trim_end()
        .to_string();
    let ssid = (b[6] >> 1) & 0x0f;
    if ssid == 0 {
        call
    } else {
        format!("{call}-{ssid}")
    }
}

impl Frame {
    /// Parse a frame (FCS already checked and removed).
    pub fn parse(raw: &[u8]) -> Option<Frame> {
        let mut n = 0;
        // Addresses: 7 bytes each until one has the end bit.
        loop {
            let a = raw.get(n..n + 7)?;
            n += 7;
            if a[6] & 1 == 1 {
                break;
            }
            if n >= 7 * 10 {
                return None;
            }
        }
        if n < 14 {
            return None;
        }
        let dest = address(&raw[0..7]);
        let source = address(&raw[7..14]);
        let path = raw[14..n]
            .chunks(7)
            .map(|a| {
                let s = address(a);
                if a[6] & 0x80 != 0 { s + "*" } else { s }
            })
            .collect();
        let control = *raw.get(n)?;
        n += 1;
        // I frames and UI frames carry a PID.
        let pid = if control & 1 == 0 || control & 0xef == 0x03 {
            let p = *raw.get(n)?;
            n += 1;
            Some(p)
        } else {
            None
        };
        Some(Frame {
            dest,
            source,
            path,
            control,
            pid,
            info: raw[n..].to_vec(),
            raw: raw.to_vec(),
        })
    }

    /// The usual monitor format: `SRC>DEST,PATH:info`, with bytes outside
    /// printable ASCII shown as `<0xNN>`.
    pub fn tnc2(&self) -> String {
        let mut s = format!("{}>{}", self.source, self.dest);
        for p in &self.path {
            s.push(',');
            s.push_str(p);
        }
        s.push(':');
        for &b in &self.info {
            if (0x20..0x7f).contains(&b) {
                s.push(b as char);
            } else {
                s.push_str(&format!("<0x{b:02x}>"));
            }
        }
        s
    }
}

/// Clock recovery, NRZI and HDLC for one slicer.
#[derive(Debug, Clone)]
struct Slicer {
    gain: f32,
    /// Bit clock phase (0..1, the bit boundary at 0, sampled at 0.5).
    phase: f32,
    step: f32,
    last_raw: bool,
    last_bit: bool,
    sr: u8,
    ones: u32,
    collecting: bool,
    cur: u8,
    nbits: u32,
    bytes: Vec<u8>,
}

impl Slicer {
    fn new(gain: f32, samples_per_bit: f64) -> Self {
        Slicer {
            gain,
            phase: 0.0,
            step: (1.0 / samples_per_bit) as f32,
            last_raw: false,
            last_bit: false,
            sr: 0,
            ones: 0,
            collecting: false,
            cur: 0,
            nbits: 0,
            bytes: Vec::new(),
        }
    }

    /// One demodulated sample; returns a frame (with FCS) when one ends.
    fn sample(&mut self, mark: f32, space: f32) -> Option<Vec<u8>> {
        let raw = mark > self.gain * space;
        let before = self.phase;
        self.phase += self.step;
        if raw != self.last_raw {
            // Pull the clock so transitions land on bit boundaries.
            let mut err = self.phase;
            if err >= 0.5 {
                err -= 1.0;
            }
            self.phase -= 0.3 * err;
            self.last_raw = raw;
        }
        let mut out = None;
        if before < 0.5 && self.phase >= 0.5 {
            // NRZI: no change is a 1.
            let bit = raw == self.last_bit;
            self.last_bit = raw;
            out = self.bit(bit);
        }
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        } else if self.phase < 0.0 {
            self.phase += 1.0;
        }
        out
    }

    fn bit(&mut self, b: bool) -> Option<Vec<u8>> {
        self.sr = (self.sr >> 1) | ((b as u8) << 7);
        if self.sr == 0x7e {
            // The flag's first seven bits went into `cur`: a frame that
            // ended on a byte boundary has exactly seven left over.
            let frame = (self.collecting
                && self.nbits == 7
                && self.bytes.len() >= MIN_FRAME
                && self.bytes.len() <= MAX_FRAME)
                .then(|| std::mem::take(&mut self.bytes));
            self.collecting = true;
            self.bytes.clear();
            self.cur = 0;
            self.nbits = 0;
            self.ones = 0;
            return frame;
        }
        if b {
            self.ones += 1;
            if self.ones >= 7 {
                // Abort, or idle ones between frames.
                self.collecting = false;
            }
        } else {
            let stuffed = self.ones == 5;
            self.ones = 0;
            if stuffed {
                return None;
            }
        }
        if self.collecting {
            self.cur |= (b as u8) << self.nbits;
            self.nbits += 1;
            if self.nbits == 8 {
                self.bytes.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
                if self.bytes.len() > MAX_FRAME {
                    self.collecting = false;
                }
            }
        }
        None
    }
}

/// Tone correlator over a sliding one-bit window.
#[derive(Debug, Clone)]
struct Tone {
    /// Per-sample rotation and the current phasor.
    rot: (f64, f64),
    ph: (f64, f64),
    hist: VecDeque<(f32, f32)>,
    sum: (f32, f32),
    len: usize,
}

impl Tone {
    fn new(rate: f64, f: f64, len: usize) -> Self {
        let w = 2.0 * PI * f / rate;
        Tone {
            rot: (w.cos(), w.sin()),
            ph: (1.0, 0.0),
            hist: VecDeque::with_capacity(len + 1),
            sum: (0.0, 0.0),
            len,
        }
    }

    /// Magnitude of the tone over the last bit.
    fn push(&mut self, x: f32) -> f32 {
        let (c, s) = self.ph;
        self.ph = (
            c * self.rot.0 - s * self.rot.1,
            c * self.rot.1 + s * self.rot.0,
        );
        let p = (x * c as f32, x * s as f32);
        self.sum.0 += p.0;
        self.sum.1 += p.1;
        self.hist.push_back(p);
        if self.hist.len() > self.len {
            let o = self.hist.pop_front().unwrap();
            self.sum.0 -= o.0;
            self.sum.1 -= o.1;
        }
        (self.sum.0 * self.sum.0 + self.sum.1 * self.sum.1).sqrt()
    }

    /// Keep the phasor on the unit circle and the sums from drifting.
    fn renormalize(&mut self) {
        let m = (self.ph.0 * self.ph.0 + self.ph.1 * self.ph.1).sqrt();
        self.ph = (self.ph.0 / m, self.ph.1 / m);
        self.sum = self
            .hist
            .iter()
            .fold((0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1));
    }
}

/// A decoded frame and the sample (at the decoder's rate) where it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub frame: Frame,
    pub at_sample: u64,
}

/// AFSK1200 demodulator and AX.25 deframer, fed discriminator output
/// (no de-emphasis) at any rate of at least ~8 kHz.
#[derive(Debug, Clone)]
pub struct AfskDecoder {
    mark: Tone,
    space: Tone,
    slicers: Vec<Slicer>,
    n: u64,
    /// Recent frames, to drop the same frame from other slicers.
    recent: VecDeque<(u64, Vec<u8>)>,
    dedup_samples: u64,
    out: Vec<Decoded>,
    /// Frames that failed the FCS (for stats).
    pub bad_fcs: u64,
}

impl AfskDecoder {
    pub fn new(rate: f64) -> Self {
        let spb = rate / BAUD;
        let len = spb.round() as usize;
        AfskDecoder {
            mark: Tone::new(rate, MARK_HZ, len),
            space: Tone::new(rate, SPACE_HZ, len),
            slicers: SPACE_GAINS.iter().map(|&g| Slicer::new(g, spb)).collect(),
            n: 0,
            recent: VecDeque::new(),
            dedup_samples: rate as u64,
            out: Vec::new(),
            bad_fcs: 0,
        }
    }

    pub fn push(&mut self, x: &[f32]) {
        for &v in x {
            let m = self.mark.push(v);
            let s = self.space.push(v);
            self.n += 1;
            if self.n % 4096 == 0 {
                self.mark.renormalize();
                self.space.renormalize();
            }
            for k in 0..self.slicers.len() {
                if let Some(bytes) = self.slicers[k].sample(m, s) {
                    self.frame(bytes);
                }
            }
        }
    }

    fn frame(&mut self, bytes: Vec<u8>) {
        let n = bytes.len();
        let got = u16::from_le_bytes([bytes[n - 2], bytes[n - 1]]);
        let body = &bytes[..n - 2];
        if fcs(body) != got {
            self.bad_fcs += 1;
            return;
        }
        while self
            .recent
            .front()
            .is_some_and(|(t, _)| self.n - t > self.dedup_samples)
        {
            self.recent.pop_front();
        }
        if self.recent.iter().any(|(_, b)| b == body) {
            return;
        }
        self.recent.push_back((self.n, body.to_vec()));
        if let Some(frame) = Frame::parse(body) {
            self.out.push(Decoded {
                frame,
                at_sample: self.n,
            });
        }
    }

    /// Frames decoded since the last call.
    pub fn take(&mut self) -> Vec<Decoded> {
        std::mem::take(&mut self.out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn addr(call: &str, ssid: u8, last: bool, h: bool) -> Vec<u8> {
        let mut v: Vec<u8> = format!("{call:<6}").bytes().map(|c| c << 1).collect();
        v.push(0x60 | (ssid << 1) | last as u8 | if h { 0x80 } else { 0 });
        v
    }

    /// A UI frame (no FCS).
    pub(crate) fn ui_frame(
        src: &str,
        dst: &str,
        path: &[(&str, u8, bool)],
        info: &[u8],
    ) -> Vec<u8> {
        let mut f = addr(dst, 0, false, false);
        f.extend(addr(src, 9, path.is_empty(), false));
        for (i, &(c, s, h)) in path.iter().enumerate() {
            f.extend(addr(c, s, i + 1 == path.len(), h));
        }
        f.extend([0x03, 0xf0]);
        f.extend_from_slice(info);
        f
    }

    /// The frame as AFSK audio at `rate`: flags, stuffed bits with FCS,
    /// NRZI, phase-continuous tones of amplitude `mark_amp` / `space_amp`.
    pub(crate) fn afsk_audio(frame: &[u8], rate: f64, mark_amp: f32, space_amp: f32) -> Vec<f32> {
        let mut bits = Vec::new();
        let flag = |bits: &mut Vec<bool>| (0..8).for_each(|i| bits.push((0x7e >> i) & 1 == 1));
        for _ in 0..25 {
            flag(&mut bits);
        }
        let mut data = frame.to_vec();
        data.extend(fcs(frame).to_le_bytes());
        let mut ones = 0;
        for b in data {
            for i in 0..8 {
                let bit = (b >> i) & 1 == 1;
                bits.push(bit);
                if bit {
                    ones += 1;
                    if ones == 5 {
                        bits.push(false);
                        ones = 0;
                    }
                } else {
                    ones = 0;
                }
            }
        }
        for _ in 0..3 {
            flag(&mut bits);
        }
        let mut tone = true; // mark
        let mut out = Vec::new();
        let mut ph = 0.0f64;
        let spb = rate / BAUD;
        for (k, &b) in bits.iter().enumerate() {
            if !b {
                tone = !tone;
            }
            let end = ((k + 1) as f64 * spb).round() as usize;
            while out.len() < end {
                let f = if tone { MARK_HZ } else { SPACE_HZ };
                ph += 2.0 * PI * f / rate;
                let a = if tone { mark_amp } else { space_amp };
                out.push(a * ph.sin() as f32);
            }
        }
        out
    }

    #[test]
    fn fcs_check_value() {
        assert_eq!(fcs(b"123456789"), 0x906e);
    }

    #[test]
    fn decodes_a_frame_with_tilt_and_noise() {
        let rate = 31250.0;
        let f = ui_frame(
            "KF7HVM",
            "APRS",
            &[("WIDE1", 1, true), ("WIDE2", 1, false)],
            b"!4559.61N/12302.48W-Test \xb0 packet",
        );
        for (ma, sa) in [(0.6f32, 0.6f32), (0.3, 0.9), (0.9, 0.3)] {
            let mut x = vec![0.0f32; 3000];
            x.extend(afsk_audio(&f, rate, ma, sa));
            x.extend(vec![0.0f32; 3000]);
            let mut seed = 3u32;
            for v in &mut x {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *v += ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.4;
            }
            let mut d = AfskDecoder::new(rate);
            for c in x.chunks(500) {
                d.push(c);
            }
            let got = d.take();
            assert_eq!(got.len(), 1, "tilt {ma}/{sa}: {got:?}");
            let fr = &got[0].frame;
            assert_eq!(
                fr.tnc2(),
                "KF7HVM-9>APRS,WIDE1-1*,WIDE2-1:!4559.61N/12302.48W-Test <0xb0> packet"
            );
            assert_eq!(fr.raw, f);
        }
    }

    #[test]
    fn rejects_corruption() {
        let rate = 31250.0;
        let f = ui_frame("N0CALL", "APRS", &[], b">status");
        let mut x = afsk_audio(&f, rate, 0.5, 0.5);
        // Flip the tone for a few bits in the middle of the frame.
        let mid = x.len() * 2 / 3;
        for v in &mut x[mid..mid + 80] {
            *v = 0.0;
        }
        let mut d = AfskDecoder::new(rate);
        d.push(&x);
        assert!(d.take().is_empty());
    }
}
