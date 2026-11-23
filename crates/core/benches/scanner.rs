//! End-to-end timing of the whole [`Scanner`] at 10 MS/s over the 2 m band:
//! forward FFT, detector with all its guards, and live channels with noise
//! squelch, de-emphasis, tone tagging, AGC and spur levels, plus one AFSK
//! packet channel.
//!
//!     cargo bench -p scanner-core --bench scanner
//!
//! Prints the wall-clock share of one core each scene needs at 10 MS/s (best of
//! five runs) and a fingerprint of every [`Output`] the scanner produced. A
//! refactor that is meant to change nothing must leave the fingerprints
//! alone; compare them before and after.

use num_complex::Complex32;
use scanner_core::afsk;
use scanner_core::config::{Mode, Slot, SlotOrigin};
use scanner_core::scanner::{Output, Scanner, ScannerConfig, ScannerStats};
use std::f64::consts::TAU;
use std::time::{Instant, SystemTime};

const FS: f64 = 10e6;
const CENTER: f64 = 146e6;
const SECS: f64 = 3.0;
const CHUNK: usize = 1 << 16;
const RUNS: usize = 5;

/// One keyed-up transmitter: RF frequency, key-up and key-down times (s),
/// a CTCSS tone (Hz, 0 for none), and whether it sends 1200-baud AFSK
/// instead of voice.
struct Tx {
    freq: f64,
    on: f64,
    off: f64,
    ctcss: f64,
    afsk: bool,
}

/// Deterministic xorshift noise.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
}

/// Band noise plus the transmitters, FM at 3 kHz peak deviation. Voice is
/// two tones under a 4 Hz syllable envelope, so levels move the way speech
/// does (AGC and the spur check see something realistic).
fn scene(txs: &[Tx]) -> Vec<Complex32> {
    let n = (SECS * FS) as usize;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut x: Vec<Complex32> = (0..n)
        .map(|_| Complex32::new(rng.next(), rng.next()) * 2e-3)
        .collect();
    let amp = 0.02f32;
    for (k, tx) in txs.iter().enumerate() {
        let off = tx.freq - CENTER;
        let (a, b) = ((tx.on * FS) as usize, ((tx.off * FS) as usize).min(n));
        let mut ph = 0.0f64;
        let tones = afsk_tones(k);
        let mut tph = 0.0f64;
        let (f1, f2) = (420.0 + 37.0 * k as f64, 1130.0 + 53.0 * k as f64);
        for (i, v) in x[a..b].iter_mut().enumerate() {
            let t = i as f64 / FS;
            // Instantaneous deviation (Hz).
            let dev = if tx.afsk {
                let mark = tones.get((t * 1200.0) as usize).copied().unwrap_or(true);
                tph = (tph + TAU * if mark { 1200.0 } else { 2200.0 } / FS).rem_euclid(TAU);
                3000.0 * tph.sin()
            } else {
                let env = 0.55 + 0.45 * (TAU * 4.0 * t).sin();
                let voice = env * (0.6 * (TAU * f1 * t).sin() + 0.4 * (TAU * f2 * t).sin());
                let ctcss = 0.2 * (TAU * tx.ctcss * t).sin();
                3000.0 * (0.8 * voice + ctcss)
            };
            let f = off + dev;
            ph = (ph + TAU * f / FS).rem_euclid(TAU);
            *v += Complex32::from_polar(amp, ph as f32);
        }
    }
    x
}

/// Tones (true = mark) for a few AX.25 UI frames sent back to back:
/// flags, the frame with its FCS, bit stuffing, NRZI.
fn afsk_tones(seed: usize) -> Vec<bool> {
    let addr = |call: &str, ssid: u8, last: bool| -> Vec<u8> {
        let mut v: Vec<u8> = format!("{call:<6}").bytes().map(|c| c << 1).collect();
        v.push(0x60 | (ssid << 1) | last as u8);
        v
    };
    let mut bits = Vec::new();
    let flag = |bits: &mut Vec<bool>| (0..8).for_each(|i| bits.push((0x7e >> i) & 1 == 1));
    for n in 0..3 {
        let mut frame = addr("APRS", 0, false);
        frame.extend(addr("KF7HVM", 9, true));
        frame.extend([0x03, 0xf0]);
        frame.extend(format!(">bench frame {seed}-{n}").bytes());
        frame.extend(afsk::fcs(&frame).to_le_bytes());
        for _ in 0..20 {
            flag(&mut bits);
        }
        let mut ones = 0;
        for b in frame {
            for i in 0..8 {
                let bit = (b >> i) & 1 == 1;
                bits.push(bit);
                ones = if bit { ones + 1 } else { 0 };
                if ones == 5 {
                    bits.push(false);
                    ones = 0;
                }
            }
        }
        flag(&mut bits);
    }
    let mut mark = true;
    bits.iter()
        .map(|&b| {
            if !b {
                mark = !mark;
            }
            mark
        })
        .collect()
}

/// The 2 m plan: 161 grid slots on 25 kHz, with 144.400 a listed AFSK slot.
fn plan() -> Vec<Slot> {
    (0..=160)
        .map(|k| {
            let freq = 144e6 + k as f64 * 25e3;
            let afsk = k == 16;
            Slot {
                freq,
                bandwidth: 16e3,
                deviation: 5e3,
                mode: if afsk { Mode::Afsk1200 } else { Mode::Nbfm },
                priority: false,
                record: true,
                name: None,
                origin: if afsk {
                    SlotOrigin::Listed
                } else {
                    SlotOrigin::Grid
                },
                pair: None,
            }
        })
        .collect()
}

/// FNV-1a over everything the scanner output, audio by its exact bits.
fn fingerprint(out: &[Output]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for o in out {
        match o {
            Output::Audio { id, samples } => {
                eat(&id.to_le_bytes());
                for s in samples {
                    eat(&s.to_bits().to_le_bytes());
                }
            }
            other => eat(format!("{other:?}").as_bytes()),
        }
    }
    h
}

fn run(name: &str, txs: &[Tx]) {
    let x = scene(txs);
    let plan = plan();
    let mut best = f64::MAX;
    let mut result: Option<(u64, ScannerStats, usize)> = None;
    for _ in 0..RUNS {
        let mut s = Scanner::new(
            ScannerConfig::default(),
            FS,
            CENTER,
            SystemTime::UNIX_EPOCH,
            &plan,
        );
        let mut out = Vec::new();
        let mut all = Vec::new();
        let t = Instant::now();
        for c in x.chunks(CHUNK) {
            s.push(c, &mut out);
            all.append(&mut out);
        }
        s.finish(&mut out);
        all.append(&mut out);
        best = best.min(t.elapsed().as_secs_f64());
        let fp = fingerprint(&all);
        if let Some((prev, _, _)) = &result {
            assert_eq!(*prev, fp, "runs disagree: the scanner is not deterministic");
        }
        result = Some((fp, s.stats(), all.len()));
    }
    let (fp, st, n) = result.unwrap();
    println!(
        "{name:<26} {:>6.1} % of a core at 10 MS/s  ({} recorded, {} packets, up to {} channels, {} outputs, fingerprint {fp:016x})",
        best / SECS * 100.0,
        st.recorded,
        st.packets,
        st.max_concurrent,
        n
    );
}

fn main() {
    let voice = |k: usize, on: f64, off: f64| Tx {
        // Spread across the band, off the DC notch at 146.000.
        freq: 144.525e6 + k as f64 * 375e3,
        on,
        off,
        ctcss: if k % 2 == 0 { 100.0 } else { 0.0 },
        afsk: false,
    };
    let afsk = Tx {
        freq: 144.4e6,
        on: 0.4,
        off: 2.2,
        ctcss: 0.0,
        afsk: true,
    };

    // Idle band: FFT, detector and guards only.
    run("idle band", &[]);

    // Eight voice channels keyed at staggered times, plus packet.
    let mut txs: Vec<Tx> = (0..8)
        .map(|k| voice(k, 0.2 + 0.1 * k as f64, 2.4 + 0.05 * k as f64))
        .collect();
    txs.push(afsk);
    run("8 voice + 1 AFSK", &txs);

    // Sixteen at once: takes the parallel channel path (12 or more).
    let txs: Vec<Tx> = (0..16)
        .map(|k| Tx {
            freq: 144.225e6 + k as f64 * 225e3,
            ..voice(k, 0.2 + 0.03 * k as f64, 2.6)
        })
        .collect();
    run("16 voice (parallel)", &txs);
}
