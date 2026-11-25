//! CTCSS and DCS detection, to tag recordings with the tone or code the
//! transmitting station used (PLAN.md 3.3).
//!
//! A channel feeds [`ToneDetector`] its unmuted audio before the voice
//! high-pass removes the sub-audible band. The detector low-passes it at
//! 270 Hz, keeps a 2 kHz copy (up to [`ToneDetector::MAX_SECS`]) and works
//! out the tone when the transmission ends:
//!
//! - CTCSS: Goertzel filters at the 51 standard tones over 1 s Hann
//!   windows, powers summed across windows. A tone is reported when it holds
//!   most of the sub-audible power and is at least ~150 Hz of deviation on a
//!   5 kHz channel.
//! - DCS: 134.4 bit/s NRZ, a 23-bit Golay (23,12) word repeated with no
//!   sync. Bits are sliced at 8 clock phases and every 23-bit window is
//!   checked, as sent and inverted, against the standard codes.
//!
//! A DCS waveform is identical to its inverted partner code's (023 normal is
//! 047 inverted), so the two can't be told apart; the normal-polarity name
//! wins, with a 1 bit read as positive deviation.

use crate::dsp::LowPass;
use std::f64::consts::PI;

/// The standard CTCSS tones (Hz).
pub const CTCSS_TONES: [f32; 51] = [
    67.0, 69.3, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5, 107.2,
    110.9, 114.8, 118.8, 123.0, 127.3, 131.8, 136.5, 141.3, 146.2, 150.0, 151.4, 156.7, 159.8,
    162.2, 165.5, 167.9, 171.3, 173.8, 177.3, 179.9, 183.5, 186.2, 189.9, 192.8, 196.6, 199.5,
    203.5, 206.5, 210.7, 218.1, 225.7, 229.1, 233.6, 241.8, 250.3, 254.1,
];

/// The standard DCS codes, as octal digits written in decimal-coded octal
/// (023 is `0o023`).
pub const DCS_CODES: [u16; 104] = [
    0o023, 0o025, 0o026, 0o031, 0o032, 0o036, 0o043, 0o047, 0o051, 0o053, 0o054, 0o065, 0o071,
    0o072, 0o073, 0o074, 0o114, 0o115, 0o116, 0o122, 0o125, 0o131, 0o132, 0o134, 0o143, 0o145,
    0o152, 0o155, 0o156, 0o162, 0o165, 0o172, 0o174, 0o205, 0o212, 0o223, 0o225, 0o226, 0o243,
    0o244, 0o245, 0o246, 0o251, 0o252, 0o255, 0o261, 0o263, 0o265, 0o266, 0o271, 0o274, 0o306,
    0o311, 0o315, 0o325, 0o331, 0o332, 0o343, 0o346, 0o351, 0o356, 0o364, 0o365, 0o371, 0o411,
    0o412, 0o413, 0o423, 0o431, 0o432, 0o445, 0o446, 0o452, 0o454, 0o455, 0o462, 0o464, 0o465,
    0o466, 0o503, 0o506, 0o516, 0o523, 0o526, 0o532, 0o546, 0o565, 0o606, 0o612, 0o624, 0o627,
    0o631, 0o632, 0o654, 0o662, 0o664, 0o703, 0o712, 0o723, 0o731, 0o732, 0o734, 0o743, 0o754,
];

pub const DCS_BAUD: f64 = 134.4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tone {
    /// A CTCSS tone (Hz, from [`CTCSS_TONES`]).
    Ctcss(f32),
    /// A DCS code (9 bits, shown in octal).
    Dcs { code: u16, inverted: bool },
}

impl Tone {
    /// `100.0` for CTCSS, `D023N` / `D023I` for DCS.
    pub fn label(&self) -> String {
        match *self {
            Tone::Ctcss(f) => format!("{f:.1}"),
            Tone::Dcs { code, inverted } => {
                format!("D{code:03o}{}", if inverted { 'I' } else { 'N' })
            }
        }
    }

    /// The reverse of [`Tone::label`] (as the recordings index stores it).
    pub fn parse(label: &str) -> Option<Tone> {
        let l = label.trim();
        if let Some(rest) = l.strip_prefix('D') {
            let (digits, inverted) = match rest.as_bytes().last()? {
                b'N' => (&rest[..rest.len() - 1], false),
                b'I' => (&rest[..rest.len() - 1], true),
                _ => (rest, false),
            };
            let code = u16::from_str_radix(digits, 8)
                .ok()
                .filter(|&c| c < 0o1000)?;
            return Some(Tone::Dcs { code, inverted });
        }
        l.parse::<f32>()
            .ok()
            .filter(|f| f.is_finite() && *f > 0.0)
            .map(Tone::Ctcss)
    }
}

const POLY: u32 = 0xC75;

/// The 11 Golay check bits of a 12-bit data word.
fn golay_check(data: u32) -> u32 {
    let mut cw = data & 0xfff;
    for _ in 0..12 {
        if cw & 1 != 0 {
            cw ^= POLY;
        }
        cw >>= 1;
    }
    cw
}

/// The 23-bit DCS word for `code`, bit 0 sent first: 9 code bits, then
/// `001` (in sending order), then 11 check bits.
pub fn dcs_word(code: u16) -> u32 {
    let data = 0x800 | (code as u32 & 0x1ff);
    (golay_check(data) << 12) | data
}

/// The standard code a 23-bit word carries, if any.
fn dcs_code(w: u32) -> Option<u16> {
    let data = w & 0xfff;
    if data >> 9 != 0b100 || golay_check(data) != w >> 12 {
        return None;
    }
    let code = (data & 0x1ff) as u16;
    DCS_CODES.contains(&code).then_some(code)
}

/// The standard code whose normal-polarity word is a rotation of `code`
/// sent inverted (023 inverted is 047), if any.
pub fn dcs_partner(code: u16) -> Option<u16> {
    let inv = !dcs_word(code) & 0x7f_ffff;
    (0..23)
        .map(|r| ((inv >> r) | (inv << (23 - r))) & 0x7f_ffff)
        .find_map(dcs_code)
}

pub struct ToneDetector {
    lp: LowPass,
    decim: usize,
    /// Input samples until the next kept one.
    countdown: usize,
    /// Rate of `buf`.
    rate: f64,
    buf: Vec<f32>,
    cap: usize,
    scratch: Vec<f32>,
}

impl ToneDetector {
    /// Audio kept for detection; a tone is settled long before this.
    pub const MAX_SECS: f64 = 20.0;
    /// Shortest audio worth looking at.
    pub const MIN_SECS: f64 = 0.5;
    /// Least CTCSS amplitude, as a share of the channel's full deviation
    /// (150 Hz on a 5 kHz channel; tones are usually 500-750 Hz).
    pub const MIN_AMPLITUDE: f64 = 0.03;
    /// Share of the sub-audible power the tone must hold.
    pub const MIN_SHARE: f64 = 0.5;
    /// Valid DCS words needed (3 words take half a second).
    pub const MIN_WORDS: u32 = 3;

    /// `rate` is the audio rate. Audio is in deviation units (1.0 = the
    /// channel's full deviation).
    pub fn new(rate: f64) -> Self {
        let decim = ((rate / 2000.0).round() as usize).max(1);
        let out_rate = rate / decim as f64;
        ToneDetector {
            lp: LowPass::new(rate, 270.0),
            decim,
            countdown: 0,
            rate: out_rate,
            buf: Vec::new(),
            cap: (Self::MAX_SECS * out_rate) as usize,
            scratch: Vec::new(),
        }
    }

    /// Feed unmuted audio.
    pub fn push(&mut self, audio: &[f32]) {
        if self.buf.len() >= self.cap {
            return;
        }
        self.scratch.clear();
        self.scratch.extend_from_slice(audio);
        self.lp.process(&mut self.scratch);
        for &v in &self.scratch {
            if self.countdown == 0 {
                self.buf.push(v);
                self.countdown = self.decim;
            }
            self.countdown -= 1;
        }
        self.buf.truncate(self.cap);
    }

    /// The tone or code heard, if any.
    pub fn detect(&self) -> Option<Tone> {
        if (self.buf.len() as f64) < Self::MIN_SECS * self.rate {
            return None;
        }
        self.dcs().or_else(|| self.ctcss())
    }

    fn ctcss(&self) -> Option<Tone> {
        let win = (self.rate as usize).min(self.buf.len());
        let hann: Vec<f64> = (0..win)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / win as f64).cos())
            .collect();
        let mut power = [0.0f64; CTCSS_TONES.len()];
        let mut total = 0.0f64;
        let mut x = vec![0.0f64; win];
        let mut start = 0;
        // Whole windows, plus a last part-window if it is long enough.
        while start + win / 2 <= self.buf.len() {
            let end = (start + win).min(self.buf.len());
            let n = end - start;
            let seg = &self.buf[start..end];
            let mean = seg.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
            for (i, &v) in seg.iter().enumerate() {
                x[i] = v as f64 - mean;
            }
            total += x[..n].iter().map(|v| v * v).sum::<f64>() / n as f64;
            // A shorter last window gets its own Hann.
            let w: Vec<f64> = if n == win {
                hann.clone()
            } else {
                (0..n)
                    .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos())
                    .collect()
            };
            for (k, &f) in CTCSS_TONES.iter().enumerate() {
                let c = 2.0 * (2.0 * PI * f as f64 / self.rate).cos();
                let (mut s1, mut s2) = (0.0f64, 0.0f64);
                for i in 0..n {
                    let s0 = x[i] * w[i] + c * s1 - s2;
                    s2 = s1;
                    s1 = s0;
                }
                let mag2 = s1 * s1 + s2 * s2 - c * s1 * s2;
                // Hann sums to n/2: a tone of amplitude A gives |X| = A n/4.
                let amp = 4.0 * mag2.sqrt() / n as f64;
                power[k] += amp * amp / 2.0;
            }
            start += win;
        }
        let (best, &p) = power.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
        let windows = (self.buf.len() as f64 / win as f64).round().max(1.0);
        let amp = (2.0 * p / windows).sqrt();
        (p >= Self::MIN_SHARE * total && amp >= Self::MIN_AMPLITUDE)
            .then_some(Tone::Ctcss(CTCSS_TONES[best]))
    }

    fn dcs(&self) -> Option<Tone> {
        let spb = self.rate / DCS_BAUD;
        let n = self.buf.len();
        // Remove the offset (and the code's own average) with a moving
        // average over one 23-bit period.
        let period = (23.0 * spb).round() as usize;
        if n < 2 * period {
            return None;
        }
        let mut prefix = Vec::with_capacity(n + 1);
        prefix.push(0.0f64);
        for &v in &self.buf {
            prefix.push(prefix.last().unwrap() + v as f64);
        }
        let level = |i: usize| -> f64 {
            let lo = i.saturating_sub(period / 2);
            let hi = (lo + period).min(n);
            let lo = hi - period;
            self.buf[i] as f64 - (prefix[hi] - prefix[lo]) / period as f64
        };
        let mut hits = vec![0u32; 1024];
        const PHASES: usize = 8;
        for p in 0..PHASES {
            let mut t = (p as f64 + 0.5) / PHASES as f64 * spb;
            let mut w = 0u32;
            let mut have = 0;
            while (t as usize) < n {
                let bit = (level(t as usize) > 0.0) as u32;
                w = (w >> 1) | (bit << 22);
                have += 1;
                if have >= 23 {
                    if let Some(c) = dcs_code(w) {
                        hits[c as usize] += 1;
                    }
                    if let Some(c) = dcs_code(!w & 0x7f_ffff) {
                        hits[512 + c as usize] += 1;
                    }
                }
                t += spb;
            }
        }
        let (i, &h) = hits
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(&a.0)))?;
        if h < Self::MIN_WORDS {
            return None;
        }
        let code = (i % 512) as u16;
        // The same waveform reads as a code and as its inverted partner:
        // name it by normal polarity where there is one.
        Some(match (i >= 512, dcs_partner(code)) {
            (true, Some(p)) => Tone::Dcs {
                code: p,
                inverted: false,
            },
            (inverted, _) => Tone::Dcs { code, inverted },
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// NRZ DCS at `amp` (1 bit = +amp), at `rate`, for `secs`, starting
    /// part-way into a word.
    pub(crate) fn dcs_wave(code: u16, inverted: bool, amp: f32, rate: f64, secs: f64) -> Vec<f32> {
        let w = dcs_word(code);
        let n = (secs * rate) as usize;
        let mut lp = LowPass::new(rate, 300.0);
        let mut x: Vec<f32> = (0..n)
            .map(|i| {
                let bit = ((i as f64 / rate * DCS_BAUD) as usize + 7) % 23;
                let one = (w >> bit) & 1 == 1;
                if one != inverted { amp } else { -amp }
            })
            .collect();
        lp.process(&mut x);
        x
    }

    fn noise(n: usize, rms: f32, seed: &mut u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 3.46 * rms
            })
            .collect()
    }

    /// Voice-ish audio: a few tones over 300 Hz, switching every 0.2 s.
    fn voice(n: usize, rate: f64) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f64 / rate;
                let f = [420.0, 870.0, 1300.0, 650.0][(t / 0.2) as usize % 4];
                (0.4 * (2.0 * PI * f * t).sin()) as f32
            })
            .collect()
    }

    fn detect(x: &[f32], rate: f64) -> Option<Tone> {
        let mut d = ToneDetector::new(rate);
        for c in x.chunks(333) {
            d.push(c);
        }
        d.detect()
    }

    #[test]
    fn tables_and_words() {
        assert_eq!(DCS_CODES.len(), 104);
        // Every code word is a codeword, and so is its complement (the
        // all-ones word is in the Golay code).
        for &c in &DCS_CODES {
            let w = dcs_word(c);
            assert_eq!(dcs_code(w), Some(c));
            let inv = !w & 0x7f_ffff;
            assert_eq!(golay_check(inv & 0xfff), inv >> 12);
        }
        assert_eq!(
            Tone::Dcs {
                code: 0o23,
                inverted: false
            }
            .label(),
            "D023N"
        );
        // Partners from the published normal/inverse table.
        for (a, b) in [
            (0o023, 0o047),
            (0o025, 0o244),
            (0o026, 0o464),
            (0o031, 0o627),
            (0o032, 0o051),
        ] {
            assert_eq!(dcs_partner(a), Some(b), "{a:o}");
            assert_eq!(dcs_partner(b), Some(a), "{b:o}");
        }
        assert_eq!(Tone::Ctcss(100.0).label(), "100.0");
    }

    #[test]
    fn labels_parse_back() {
        for t in [
            Tone::Ctcss(103.5),
            Tone::Ctcss(67.0),
            Tone::Dcs {
                code: 0o023,
                inverted: false,
            },
            Tone::Dcs {
                code: 0o754,
                inverted: true,
            },
        ] {
            assert_eq!(Tone::parse(&t.label()), Some(t));
        }
        for bad in ["", "D", "D9", "D1234", "abc", "-1"] {
            assert_eq!(Tone::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn finds_every_ctcss_tone_under_voice() {
        let rate = 8000.0;
        let mut seed = 1;
        for &f in &CTCSS_TONES {
            let n = (2.5 * rate) as usize;
            let v = voice(n, rate);
            let z = noise(n, 0.02, &mut seed);
            let x: Vec<f32> = (0..n)
                .map(|i| {
                    let t = i as f64 / rate;
                    // 600 Hz of a 5 kHz channel, plus a frequency offset.
                    0.12 * (2.0 * PI * f as f64 * t).sin() as f32 + v[i] + z[i] + 0.05
                })
                .collect();
            assert_eq!(detect(&x, rate), Some(Tone::Ctcss(f)), "{f}");
        }
    }

    #[test]
    fn no_tone_on_plain_voice_or_noise() {
        let rate = 8000.0;
        let n = (3.0 * rate) as usize;
        let mut seed = 5;
        let z = noise(n, 0.05, &mut seed);
        let x: Vec<f32> = voice(n, rate).iter().zip(&z).map(|(a, b)| a + b).collect();
        assert_eq!(detect(&x, rate), None);
        assert_eq!(detect(&z, rate), None);
        // Too short.
        let x: Vec<f32> = (0..2000)
            .map(|i| 0.12 * (2.0 * PI * 100.0 * i as f64 / rate).sin() as f32)
            .collect();
        assert_eq!(detect(&x, rate), None);
    }

    #[test]
    fn finds_dcs_codes_and_their_polarity() {
        let rate = 8000.0;
        let mut seed = 9;
        for &(code, inv) in &[
            (0o023, false),
            (0o754, false),
            (0o411, true),
            (0o125, false),
        ] {
            let n = (1.5 * rate) as usize;
            let d = dcs_wave(code, inv, 0.15, rate, 1.5);
            let v = voice(n, rate);
            let z = noise(n, 0.02, &mut seed);
            let x: Vec<f32> = (0..n).map(|i| d[i] + v[i] + z[i] - 0.1).collect();
            // An inverted code reads as its normal-polarity partner.
            let want = match (inv, dcs_partner(code)) {
                (true, Some(p)) => Tone::Dcs {
                    code: p,
                    inverted: false,
                },
                _ => Tone::Dcs {
                    code,
                    inverted: inv,
                },
            };
            assert_eq!(detect(&x, rate), Some(want), "{code:o} {inv}");
        }
    }
}
