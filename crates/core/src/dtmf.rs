//! DTMF (touch-tone) decoding, to tag recordings with the digits sent in
//! them: repeater and link control codes, autopatch, paging.
//!
//! Each digit is one tone from the low (row) group plus one from the high
//! (column) group:
//!
//! ```text
//!            1209  1336  1477  1633 Hz
//!    697 Hz   1     2     3     A
//!    770 Hz   4     5     6     B
//!    852 Hz   7     8     9     C
//!    941 Hz   *     0     #     D
//! ```
//!
//! [`DtmfDecoder`] runs Goertzel filters at the eight tones over 25.6 ms
//! windows, stepped every 6.4 ms. A window counts as a digit when:
//!
//! - the two strongest tones (one per group) hold at least
//!   [`DtmfDecoder::MIN_SHARE`] of the window's power, which voice (many
//!   harmonics), noise and squelch tails (broadband) don't reach;
//! - each is [`DtmfDecoder::MIN_GROUP_RATIO_DB`] above the next tone of its
//!   group;
//! - they are within [`DtmfDecoder::MAX_TWIST_DB`] of each other;
//! - the audio is above [`DtmfDecoder::MIN_POWER`].
//!
//! - the power is steady across the window, so only windows wholly inside
//!   a tone count.
//!
//! A digit starts after [`DtmfDecoder::ON_FRAMES`] matching windows in a
//! row (a tone of about 40 ms) and ends after [`DtmfDecoder::OFF_FRAMES`]
//! windows without it (a gap of about 20 ms), so fast dialling (40 ms
//! gaps) keeps repeated digits apart while a short dropout doesn't split
//! one.
//!
//! The decoder works on any audio rate of 4 kHz or more; the recorder feeds
//! it the recorded audio (8 kHz by default).

use std::f64::consts::PI;

/// Row (low group) tones, Hz.
pub const ROWS: [f64; 4] = [697.0, 770.0, 852.0, 941.0];
/// Column (high group) tones, Hz.
pub const COLS: [f64; 4] = [1209.0, 1336.0, 1477.0, 1633.0];

/// Digits by `[row][col]`.
pub const KEYS: [[char; 4]; 4] = [
    ['1', '2', '3', 'A'],
    ['4', '5', '6', 'B'],
    ['7', '8', '9', 'C'],
    ['*', '0', '#', 'D'],
];

/// The (row, column) tone pair for a digit (`0-9`, `*`, `#`, `A-D`,
/// case-insensitive).
pub fn tones(digit: char) -> Option<(f64, f64)> {
    let d = digit.to_ascii_uppercase();
    for (r, row) in KEYS.iter().enumerate() {
        if let Some(c) = row.iter().position(|&k| k == d) {
            return Some((ROWS[r], COLS[c]));
        }
    }
    None
}

/// One decoded digit, timed in samples from the first sample pushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Digit {
    pub key: char,
    /// First sample of the first window it was heard in.
    pub start: u64,
    /// One past the last sample of the last window it was heard in.
    pub end: u64,
}

/// Streaming DTMF decoder; push audio in any chunk sizes, read the digits
/// with [`DtmfDecoder::digits`] or [`DtmfDecoder::text`].
pub struct DtmfDecoder {
    rate: f64,
    /// Window and step, in samples.
    win: usize,
    hop: usize,
    /// 2cos(w) per tone, rows then columns.
    coeff: [f64; 8],
    /// Samples not yet analysed; the front is window `frame`'s start.
    buf: Vec<f32>,
    /// Sample index of `buf[0]`.
    buf_start: u64,
    state: State,
    digits: Vec<Digit>,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Idle,
    /// `key` heard in `n` windows in a row, from `start`.
    Rising {
        key: char,
        n: u32,
        start: u64,
    },
    /// Digit `digits.last()` is on; `misses` windows in a row without it.
    On {
        misses: u32,
    },
}

impl DtmfDecoder {
    /// Window length; 39 Hz resolution, enough to part tones ~70 Hz apart.
    pub const WINDOW_SECS: f64 = 0.0256;
    /// Windows per window length (6.4 ms steps).
    pub const OVERLAP: usize = 4;
    /// Windows in a row that start a digit (~40 ms of tone).
    pub const ON_FRAMES: u32 = 2;
    /// Windows in a row without the digit that end it. Any window that
    /// overlaps a gap misses, so this is a gap of about 20 ms (a 40 ms gap
    /// misses ~10 windows, an 8 ms dropout ~5).
    pub const OFF_FRAMES: u32 = 7;
    /// Least power of each quarter of a window, as a share of the
    /// window's mean (-6 dB).
    pub const MIN_STEADY: f64 = 0.25;
    /// Share of a window's power the tone pair must hold.
    pub const MIN_SHARE: f64 = 0.7;
    /// How far the strongest tone of each group must clear the next one.
    pub const MIN_GROUP_RATIO_DB: f64 = 8.0;
    /// Most the row and column tone levels may differ (FM de-emphasis that
    /// doesn't match the transmitter's pre-emphasis tilts the pair).
    pub const MAX_TWIST_DB: f64 = 10.0;
    /// Least window power (mean square; -50 dBFS, the recorder's AGC aims
    /// for about -20 dBFS).
    pub const MIN_POWER: f64 = 1e-5;
    /// A gap longer than this between digits starts a new group (shown
    /// with a space).
    pub const GROUP_GAP_SECS: f64 = 1.0;

    pub fn new(rate: f64) -> Self {
        assert!(rate >= 4000.0, "DTMF needs audio at 4 kHz or more");
        let win = (rate * Self::WINDOW_SECS).round() as usize;
        let mut coeff = [0.0; 8];
        for (c, f) in coeff.iter_mut().zip(ROWS.iter().chain(COLS.iter())) {
            *c = 2.0 * (2.0 * PI * f / rate).cos();
        }
        DtmfDecoder {
            rate,
            win,
            hop: (win / Self::OVERLAP).max(1),
            coeff,
            buf: Vec::with_capacity(win * 2),
            buf_start: 0,
            state: State::Idle,
            digits: Vec::new(),
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// Feed audio (muted spans as silence, so timing holds).
    pub fn push(&mut self, audio: &[f32]) {
        let mut rest = audio;
        while !rest.is_empty() {
            let take = (self.win - self.buf.len().min(self.win)).min(rest.len());
            self.buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.buf.len() >= self.win {
                let key = self.analyse();
                self.step(key);
                self.buf.drain(..self.hop);
                self.buf_start += self.hop as u64;
            }
        }
    }

    /// Digits heard so far (a digit still sounding is included).
    pub fn digits(&self) -> &[Digit] {
        &self.digits
    }

    /// The digits as text, groups split by a space: `*73 1234#`.
    pub fn text(&self) -> String {
        text(&self.digits, self.rate)
    }

    /// The digit in the window at the front of `buf`, if any.
    fn analyse(&self) -> Option<char> {
        let x = &self.buf[..self.win];
        let n = self.win as f64;
        let energy: f64 = x.iter().map(|&v| v as f64 * v as f64).sum();
        let mean_power = energy / n;
        if mean_power < Self::MIN_POWER {
            return None;
        }
        // The tone must fill the window: a window only part-way into a
        // tone (or a gap) would otherwise pass on the tone alone, and
        // tones far shorter than a window would count.
        let quarter = energy / Self::OVERLAP as f64 * Self::MIN_STEADY;
        if x.chunks_exact(self.hop)
            .any(|q| q.iter().map(|&v| v as f64 * v as f64).sum::<f64>() < quarter)
        {
            return None;
        }
        // Tone power (A²/2 for amplitude A) at each of the eight tones.
        let mut p = [0.0f64; 8];
        for (pk, &c) in p.iter_mut().zip(&self.coeff) {
            let (mut s1, mut s2) = (0.0f64, 0.0f64);
            for &v in x {
                let s0 = v as f64 + c * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            let mag2 = s1 * s1 + s2 * s2 - c * s1 * s2;
            *pk = 2.0 * mag2 / (n * n);
        }
        let (r, pr, pr2) = best_two(&p[..4]);
        let (c, pc, pc2) = best_two(&p[4..]);
        let ratio = 10f64.powf(Self::MIN_GROUP_RATIO_DB / 10.0);
        let twist = 10f64.powf(Self::MAX_TWIST_DB / 10.0);
        let ok = (pr + pc) >= Self::MIN_SHARE * mean_power
            && pr >= ratio * pr2
            && pc >= ratio * pc2
            && pr <= twist * pc
            && pc <= twist * pr;
        ok.then(|| KEYS[r][c])
    }

    fn step(&mut self, key: Option<char>) {
        let start = self.buf_start;
        let end = start + self.win as u64;
        self.state = match (self.state, key) {
            (State::On { misses }, k) => {
                let last = self.digits.last_mut().expect("a digit is on");
                if k == Some(last.key) {
                    last.end = end;
                    State::On { misses: 0 }
                } else if misses + 1 >= Self::OFF_FRAMES || k.is_some() {
                    // A different digit cuts straight in; fall through to
                    // counting it.
                    match k {
                        Some(k) => self.rise(k, 1, start),
                        None => State::Idle,
                    }
                } else {
                    State::On { misses: misses + 1 }
                }
            }
            (
                State::Rising {
                    key: rk,
                    n,
                    start: s,
                },
                Some(k),
            ) if k == rk => self.rise(k, n + 1, s),
            (_, Some(k)) => self.rise(k, 1, start),
            (_, None) => State::Idle,
        };
    }

    fn rise(&mut self, key: char, n: u32, start: u64) -> State {
        if n >= Self::ON_FRAMES {
            self.digits.push(Digit {
                key,
                start,
                end: self.buf_start + self.win as u64,
            });
            State::On { misses: 0 }
        } else {
            State::Rising { key, n, start }
        }
    }
}

/// Index and value of the largest entry, and the second-largest value.
fn best_two(p: &[f64]) -> (usize, f64, f64) {
    let mut best = 0;
    for i in 1..p.len() {
        if p[i] > p[best] {
            best = i;
        }
    }
    let second = p
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != best)
        .map(|(_, &v)| v)
        .fold(0.0, f64::max);
    (best, p[best], second)
}

/// Digits as text, with a space where the gap between two is longer than
/// [`DtmfDecoder::GROUP_GAP_SECS`].
pub fn text(digits: &[Digit], rate: f64) -> String {
    let gap = (DtmfDecoder::GROUP_GAP_SECS * rate) as u64;
    let mut s = String::with_capacity(digits.len() + 4);
    for (i, d) in digits.iter().enumerate() {
        if i > 0 && d.start.saturating_sub(digits[i - 1].end) > gap {
            s.push(' ');
        }
        s.push(d.key);
    }
    s
}

/// Synthesise DTMF: each `(digit, on_secs, off_secs)` as a tone pair of
/// `amplitude` per tone, then silence. For tests and benches.
pub fn synth(rate: f64, seq: &[(char, f64, f64)], amplitude: f32) -> Vec<f32> {
    let mut out = Vec::new();
    for &(d, on, off) in seq {
        let (fr, fc) = tones(d).expect("a DTMF digit");
        let n = (on * rate).round() as usize;
        out.extend((0..n).map(|i| {
            let t = i as f64 / rate;
            amplitude * ((2.0 * PI * fr * t).sin() + (2.0 * PI * fc * t).sin()) as f32
        }));
        out.extend(std::iter::repeat_n(0.0, (off * rate).round() as usize));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 8000.0;

    fn decode(x: &[f32]) -> String {
        let mut d = DtmfDecoder::new(RATE);
        // Uneven chunks, as the recorder gets them.
        for c in x.chunks(333) {
            d.push(c);
        }
        d.text()
    }

    /// Deterministic noise in -1..1.
    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((s >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn tone_table_round_trips() {
        for row in KEYS {
            for k in row {
                let (r, c) = tones(k).unwrap();
                assert!(ROWS.contains(&r) && COLS.contains(&c));
            }
        }
        assert_eq!(tones('d'), Some((941.0, 1633.0)));
        assert_eq!(tones('x'), None);
    }

    #[test]
    fn decodes_all_sixteen_digits() {
        let keys = "123A456B789C*0#D";
        let seq: Vec<_> = keys.chars().map(|k| (k, 0.08, 0.06)).collect();
        assert_eq!(decode(&synth(RATE, &seq, 0.1)), keys);
    }

    #[test]
    fn timing_is_reported_in_samples() {
        let mut x = vec![0.0; 4000];
        x.extend(synth(RATE, &[('5', 0.1, 0.1)], 0.1));
        let mut d = DtmfDecoder::new(RATE);
        d.push(&x);
        let g = d.digits();
        assert_eq!(g.len(), 1);
        let (s, e) = (g[0].start as i64, g[0].end as i64);
        // Within a window of the true edges (0.5 s and 0.6 s).
        assert!((s - 4000).abs() <= 205, "start {s}");
        assert!((e - 4800).abs() <= 205, "end {e}");
    }

    #[test]
    fn repeated_digit_needs_its_gap() {
        // Fast dialling: 45 ms on, 45 ms off.
        let seq: Vec<_> = "1100#".chars().map(|k| (k, 0.045, 0.045)).collect();
        assert_eq!(decode(&synth(RATE, &seq, 0.1)), "1100#");
    }

    #[test]
    fn decodes_the_shortest_standard_timing() {
        // 40 ms on, 40 ms off, at a few start offsets against the windows.
        for lead in [0, 13, 27, 40] {
            let mut x = vec![0.0; lead];
            let seq: Vec<_> = "5#5*9".chars().map(|k| (k, 0.04, 0.04)).collect();
            x.extend(synth(RATE, &seq, 0.1));
            assert_eq!(decode(&x), "5#5*9", "lead {lead}");
        }
    }

    #[test]
    fn too_short_tones_are_ignored() {
        let seq: Vec<_> = "123".chars().map(|k| (k, 0.02, 0.1)).collect();
        assert_eq!(decode(&synth(RATE, &seq, 0.1)), "");
    }

    #[test]
    fn long_gaps_split_groups() {
        let x = synth(
            RATE,
            &[
                ('*', 0.1, 0.1),
                ('7', 0.1, 0.1),
                ('3', 0.1, 1.5),
                ('#', 0.1, 0.1),
            ],
            0.1,
        );
        assert_eq!(decode(&x), "*73 #");
    }

    #[test]
    fn a_single_dropout_does_not_split_a_digit() {
        let mut x = synth(RATE, &[('9', 0.3, 0.1)], 0.1);
        // 8 ms of silence in the middle (a fade or a dropped block).
        for v in &mut x[1200..1264] {
            *v = 0.0;
        }
        assert_eq!(decode(&x), "9");
    }

    #[test]
    fn survives_noise_twist_and_rate() {
        // 20 dB SNR per tone, 6 dB twist, at 16 kHz.
        let rate = 16000.0;
        let mut x = Vec::new();
        for k in "0123456789".chars() {
            let (fr, fc) = tones(k).unwrap();
            let n = (0.07 * rate) as usize;
            x.extend((0..n).map(|i| {
                let t = i as f64 / rate;
                (0.1 * (2.0 * PI * fr * t).sin() + 0.05 * (2.0 * PI * fc * t).sin()) as f32
            }));
            x.extend(std::iter::repeat_n(0.0, (0.06 * rate) as usize));
        }
        // Noise at -20 dB relative to the low tone's power, across 0-8 kHz.
        let nz = noise(x.len(), 3);
        let scale = (0.1f32 * 0.1 / 2.0 * 0.01 * 3.0).sqrt();
        for (v, n) in x.iter_mut().zip(nz) {
            *v += n * scale;
        }
        let mut d = DtmfDecoder::new(rate);
        d.push(&x);
        assert_eq!(d.text(), "0123456789");
    }

    #[test]
    fn rejects_excess_twist() {
        let (fr, fc) = tones('5').unwrap();
        let x: Vec<f32> = (0..2000)
            .map(|i| {
                let t = i as f64 / RATE;
                (0.1 * (2.0 * PI * fr * t).sin() + 0.02 * (2.0 * PI * fc * t).sin()) as f32
            })
            .collect();
        assert_eq!(decode(&x), "");
    }

    #[test]
    fn single_tones_and_quiet_tones_are_ignored() {
        let one: Vec<f32> = (0..4000)
            .map(|i| (0.2 * (2.0 * PI * 1000.0 * i as f64 / RATE).sin()) as f32)
            .collect();
        assert_eq!(decode(&one), "");
        assert_eq!(decode(&synth(RATE, &[('1', 0.2, 0.1)], 0.001)), "");
    }

    #[test]
    fn noise_and_squelch_tails_are_ignored() {
        assert_eq!(decode(&noise(80000, 1)), "");
        // A squelch tail: a burst of loud noise after silence.
        let mut x = vec![0.0; 4000];
        x.extend(noise(1600, 2).iter().map(|v| v * 0.8));
        assert_eq!(decode(&x), "");
    }

    #[test]
    fn voice_like_audio_is_ignored() {
        // Ten seconds of vowel-like sound: a gliding pitch with harmonics
        // shaped by two moving formants, under a syllable envelope.
        let n = (10.0 * RATE) as usize;
        let mut phase = 0.0f64;
        let mut x = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / RATE;
            let f0 = 140.0 + 60.0 * (2.0 * PI * 0.7 * t).sin();
            phase += 2.0 * PI * f0 / RATE;
            let f1 = 650.0 + 250.0 * (2.0 * PI * 0.9 * t).sin();
            let f2 = 1300.0 + 400.0 * (2.0 * PI * 0.6 * t).cos();
            let mut v = 0.0;
            for h in 1..=25 {
                let fh = f0 * h as f64;
                let g = (-((fh - f1) / 150.0).powi(2)).exp()
                    + 0.7 * (-((fh - f2) / 200.0).powi(2)).exp()
                    + 0.05;
                v += g * (phase * h as f64).sin();
            }
            let env = 0.5 + 0.5 * (2.0 * PI * 4.0 * t).sin().abs();
            x.push((0.05 * env * v) as f32);
        }
        assert_eq!(decode(&x), "");
    }
}
