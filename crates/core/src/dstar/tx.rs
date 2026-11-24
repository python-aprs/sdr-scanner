//! A D-STAR transmitter for tests and synthetic captures: preamble, frame
//! sync, header, voice frames with slow data, end pattern, GMSK-modulated.

use super::frame::{self, FRAME_BITS, FRAME_SYNC, SUPERFRAME, SlowData, pattern_bits};
use super::header::{self, Header};
use super::rx::{RATE, SPS};
use crate::dmr::tx::test_frame;
use crate::dsp::Resampler;
use num_complex::Complex32;

/// Peak deviation, Hz (modulation index 0.5 at 4800 bit/s).
pub const DEVIATION: f64 = 1200.0;

#[derive(Debug, Clone)]
pub struct TxCall {
    pub header: Header,
    pub message: String,
    /// Voice frames (20 ms each).
    pub frames: usize,
    /// Send the header (a late entry misses it).
    pub send_header: bool,
    /// Repeat the header in the slow data.
    pub slow_header: bool,
}

impl TxCall {
    pub fn new(my: &str, frames: usize) -> Self {
        TxCall {
            header: Header {
                flags: [0; 3],
                rpt2: Some("WB7DZG G".into()),
                rpt1: Some("WB7DZG B".into()),
                your: Some("CQCQCQ".into()),
                my: Some(my.into()),
                suffix: Some("ID51".into()),
            },
            message: "Hello from Rainier".into(),
            frames,
            send_header: true,
            slow_header: true,
        }
    }
}

/// The call's bits on air, from the preamble to the end pattern. Voice
/// frame `n` carries [`test_frame`] `n`.
pub fn call_bits(c: &TxCall) -> Vec<u8> {
    let mut out = Vec::new();
    let hb = c.header.to_bytes();
    if c.send_header {
        for _ in 0..32 {
            out.extend([1, 0]);
        }
        out.extend(pattern_bits(FRAME_SYNC, 24));
        out.extend(header::encode(&hb));
    }
    let blocks = SlowData::blocks(&c.message, if c.slow_header { &hb[..] } else { &[][..] });
    let mut halves = blocks
        .iter()
        .flat_map(|b| [[b[0], b[1], b[2]], [b[3], b[4], b[5]]])
        .cycle();
    for n in 0..c.frames {
        let mut f = [0u8; FRAME_BITS];
        frame::set_voice(&mut f, &test_frame(n));
        if n % SUPERFRAME == 0 {
            frame::set_data_sync(&mut f);
        } else {
            // Twenty halves between syncs, so blocks stay paired.
            frame::set_slow_data(&mut f, halves.next().unwrap());
        }
        out.extend_from_slice(&f);
    }
    out.extend(pattern_bits(frame::END, 48));
    out
}

/// GMSK-modulate (BT 0.5) at `rate`, carrier on only for the bits, with
/// `pad` bit times of silence either side.
pub fn modulate(bits: &[u8], rate: f64, amp: f32, pad: usize) -> Vec<Complex32> {
    // Gaussian pulse over three bits.
    let bt = 0.5;
    let span = 3 * SPS;
    let sigma = (2f64.ln()).sqrt() / (2.0 * std::f64::consts::PI * bt) * SPS as f64;
    let mut g: Vec<f64> = (0..=span)
        .map(|i| {
            let t = i as f64 - span as f64 / 2.0;
            (-t * t / (2.0 * sigma * sigma)).exp()
        })
        .collect();
    let s: f64 = g.iter().sum();
    g.iter_mut().for_each(|v| *v /= s);
    let n = (bits.len() + 2 * pad) * SPS + g.len();
    let mut nrz = vec![0f64; n];
    let mut gate = vec![0f32; n];
    for (i, &b) in bits.iter().enumerate() {
        let v = if b == 1 { -1.0 } else { 1.0 };
        for k in 0..SPS {
            nrz[(pad + i) * SPS + k] = v;
            gate[(pad + i) * SPS + k + g.len() / 2] = 1.0;
        }
    }
    let freq: Vec<f32> = (0..n)
        .map(|i| {
            let acc: f64 = g
                .iter()
                .enumerate()
                .map(|(k, w)| if i >= k { nrz[i - k] * w } else { 0.0 })
                .sum();
            (acc * DEVIATION) as f32
        })
        .collect();
    let (freq, gate) = if (rate - RATE).abs() > 1e-6 {
        let mut f = Vec::new();
        let mut gg = Vec::new();
        Resampler::new(RATE, rate, 6000.0, 3000.0).process(&freq, &mut f);
        Resampler::new(RATE, rate, 6000.0, 3000.0).process(&gate, &mut gg);
        (f, gg)
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
