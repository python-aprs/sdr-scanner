//! A DMR transmitter for tests and synthetic captures: builds the bursts of
//! a voice call (header, superframes with embedded link control and talker
//! alias, terminator) and modulates them to IQ.

use super::burst::{self, DataType, Emb, Source, Sync};
use super::fec;
use super::lc::{self, Lc, TalkerAlias};
use super::rx::{self, SPS};
use crate::dsp::Resampler;
use num_complex::Complex32;

#[derive(Debug, Clone)]
pub struct TxCall {
    pub lc: Lc,
    pub color_code: u8,
    /// Voice superframes (360 ms each).
    pub superframes: usize,
    pub alias: Option<String>,
    /// Voice LC headers before the voice.
    pub headers: usize,
    pub terminator: bool,
}

impl TxCall {
    pub fn group(tg: u32, src: u32, superframes: usize) -> Self {
        TxCall {
            lc: Lc::group(tg, src),
            color_code: 1,
            superframes,
            alias: None,
            headers: 2,
            terminator: true,
        }
    }
}

/// Test voice parameters for frame `n`: varying, never all zero.
pub fn test_frame(n: usize) -> [u8; 49] {
    std::array::from_fn(|i| (((n * 31 + i * 7) ^ (i * i)) % 3 == 0) as u8)
}

fn data_burst(cc: u8, dt: DataType, bits96: &[u8; 96], sync: Sync) -> [u8; 264] {
    let mut b = [0u8; 264];
    burst::set_info_bits(&mut b, &fec::bptc_encode(bits96));
    burst::set_slot_type(&mut b, cc, dt);
    burst::set_sync(&mut b, burst::sync_pattern(sync));
    b
}

/// An idle burst, as a repeater sends on a slot with nothing to say.
pub fn idle_burst(cc: u8) -> [u8; 264] {
    let bits: [u8; 96] = std::array::from_fn(|i| (i % 3 == 1) as u8);
    data_burst(
        cc,
        DataType::Idle,
        &bits,
        Sync {
            source: Source::Base,
            voice: false,
        },
    )
}

/// The bursts of one call, in order, all on one timeslot.
pub fn call_bursts(c: &TxCall, source: Source) -> Vec<[u8; 264]> {
    let data = Sync {
        source,
        voice: false,
    };
    let voice = Sync {
        source,
        voice: true,
    };
    let mut out = Vec::new();
    let header = lc::full_lc_bits(&c.lc, lc::MASK_VOICE_HEADER);
    for _ in 0..c.headers {
        out.push(data_burst(
            c.color_code,
            DataType::VoiceLcHeader,
            &header,
            data,
        ));
    }
    let alias = c
        .alias
        .as_deref()
        .map(TalkerAlias::encode)
        .unwrap_or_default();
    let mut frame = 0;
    for sf in 0..c.superframes {
        // Odd superframes carry the talker alias, a block at a time.
        let emb_lc = if sf % 2 == 1 && !alias.is_empty() {
            alias[(sf / 2) % alias.len()]
        } else {
            c.lc.to_bytes()
        };
        let embedded = fec::embedded_encode(&emb_lc);
        for k in 0..6 {
            let mut b = [0u8; 264];
            let frames = [
                test_frame(frame),
                test_frame(frame + 1),
                test_frame(frame + 2),
            ];
            frame += 3;
            burst::set_ambe_frames(&mut b, &frames);
            if k == 0 {
                burst::set_sync(&mut b, burst::sync_pattern(voice));
            } else {
                let (lcss, bits) = match k {
                    1 => (1, &embedded[0..32]),
                    2 | 3 => (3, &embedded[(k - 1) * 32..k * 32]),
                    4 => (2, &embedded[96..128]),
                    _ => (0, &[0u8; 32][..]),
                };
                let e = Emb {
                    cc: c.color_code,
                    pi: false,
                    lcss,
                };
                burst::set_emb(&mut b, e, bits.try_into().unwrap());
            }
            out.push(b);
        }
    }
    if c.terminator {
        let t = lc::full_lc_bits(&c.lc, lc::MASK_TERMINATOR);
        out.push(data_burst(
            c.color_code,
            DataType::TerminatorWithLc,
            &t,
            data,
        ));
    }
    out
}

/// Symbols of a stream: `None` while the carrier is off.
pub type Symbols = Vec<Option<i8>>;

fn push_bits(out: &mut Symbols, bits: &[u8]) {
    for d in bits.chunks(2) {
        out.push(Some(burst::dibit_symbol((d[0] << 1) | d[1])));
    }
}

/// A radio talking without a repeater: each burst in its own 60 ms, the
/// carrier off between them.
pub fn mobile_symbols(bursts: &[[u8; 264]]) -> Symbols {
    let mut out = Symbols::new();
    for b in bursts {
        out.extend(std::iter::repeat_n(None, burst::CACH_SYMBOLS));
        push_bits(&mut out, b);
        out.extend(std::iter::repeat_n(None, burst::FRAME_SYMBOLS));
    }
    out
}

/// A repeater's output: CACH + burst every 30 ms, `slots[ts]` alternating,
/// idle bursts where a slot has run out.
pub fn base_symbols(slots: [&[[u8; 264]]; 2], cc: u8, extra_idle: usize) -> Symbols {
    let n = slots[0].len().max(slots[1].len()) + extra_idle;
    let idle = idle_burst(cc);
    let mut out = Symbols::new();
    for i in 0..n {
        for (ts, s) in slots.iter().enumerate() {
            push_bits(&mut out, &burst::cach(ts as u8));
            push_bits(&mut out, s.get(i).unwrap_or(&idle));
        }
    }
    out
}

/// FM-modulate symbols with root raised cosine shaping, at `rate`.
pub fn modulate(symbols: &Symbols, rate: f64, amp: f32) -> Vec<Complex32> {
    modulate_4fsk(symbols, rate, amp, burst::OUTER_DEV)
}

/// [`modulate`] with the outer symbols at `outer_dev` Hz (4FSK at 4800
/// symbols/s: DMR, System Fusion).
pub fn modulate_4fsk(symbols: &Symbols, rate: f64, amp: f32, outer_dev: f64) -> Vec<Complex32> {
    let h = rx::rrc(SPS, 8, 0.2);
    let n = symbols.len() * SPS + h.len();
    let mut freq = vec![0f32; n];
    let mut on = vec![false; n];
    for (i, s) in symbols.iter().enumerate() {
        if let Some(v) = s {
            let a = *v as f32 / 3.0 * outer_dev as f32 * SPS as f32;
            for (k, t) in h.iter().enumerate() {
                freq[i * SPS + k] += a * t;
            }
            // The carrier comes up a little before the burst's first
            // symbol and stays a little past its last, as a radio's does.
            let c = i * SPS + h.len() / 2;
            for o in &mut on[c.saturating_sub(3 * SPS)..(c + 4 * SPS).min(n)] {
                *o = true;
            }
        }
    }
    let gate: Vec<f32> = on.iter().map(|&o| o as u8 as f32).collect();
    let (freq, gate) = if (rate - rx::RATE).abs() > 1e-6 {
        let mut f = Vec::new();
        let mut g = Vec::new();
        Resampler::new(rx::RATE, rate, 6000.0, 3000.0).process(&freq, &mut f);
        Resampler::new(rx::RATE, rate, 6000.0, 3000.0).process(&gate, &mut g);
        (f, g)
    } else {
        (freq, gate)
    };
    let mut ph = 0f64;
    freq.iter()
        .zip(gate)
        .map(|(&f, g)| {
            ph = (ph + 2.0 * std::f64::consts::PI * f as f64 / rate)
                .rem_euclid(2.0 * std::f64::consts::PI);
            Complex32::from_polar(amp * g.clamp(0.0, 1.0), ph as f32)
        })
        .collect()
}
