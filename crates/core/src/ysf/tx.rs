//! A System Fusion transmitter for tests and synthetic captures: header,
//! voice frames with the callsigns spread over their data channel, and
//! terminator, modulated to IQ.

use super::frame::{self, DataType, FRAME_BITS, Fi, Fich, callsign_field};
use crate::dmr::burst::dibit_symbol;
use crate::dmr::tx::{Symbols, modulate_4fsk, test_frame};
use num_complex::Complex32;

/// Deviation of the outer symbols used for tests, Hz.
pub const OUTER_DEV: f64 = 2700.0;

#[derive(Debug, Clone)]
pub struct TxCall {
    pub src: String,
    pub dst: String,
    pub downlink: String,
    pub uplink: String,
    pub dg_id: u8,
    pub dt: DataType,
    /// Voice frames (100 ms each).
    pub frames: usize,
    pub header: bool,
    pub terminator: bool,
}

impl TxCall {
    pub fn dn(src: &str, frames: usize) -> Self {
        TxCall {
            src: src.into(),
            dst: "ALL".into(),
            downlink: String::new(),
            uplink: String::new(),
            dg_id: 0,
            dt: DataType::VdMode2,
            frames,
            header: true,
            terminator: true,
        }
    }
}

const FT: u8 = 6;

fn fich(c: &TxCall, fi: Fi, fn_: u8) -> Fich {
    Fich {
        fi,
        cm: 0,
        fn_,
        ft: FT,
        dt: c.dt,
        mr: 0,
        dg_id: c.dg_id,
    }
}

fn header_like(c: &TxCall, fi: Fi) -> [u8; FRAME_BITS] {
    let mut b = [0u8; FRAME_BITS];
    frame::set_sync(&mut b);
    frame::set_fich(&mut b, fich(c, fi, 0));
    let csd1 = [callsign_field(&c.dst), callsign_field(&c.src)].concat();
    let csd2 = [callsign_field(&c.downlink), callsign_field(&c.uplink)].concat();
    frame::set_dch(&mut b, &csd1, true, false);
    frame::set_dch(&mut b, &csd2, true, true);
    b
}

/// The frames of a call. Voice frame `n` carries [`test_frame`] 5n..5n+5.
pub fn call_frames(c: &TxCall) -> Vec<[u8; FRAME_BITS]> {
    let mut out = Vec::new();
    if c.header {
        out.push(header_like(c, Fi::Header));
    }
    for n in 0..c.frames {
        let fn_ = (n % (FT as usize + 1)) as u8;
        let mut b = [0u8; FRAME_BITS];
        frame::set_sync(&mut b);
        frame::set_fich(&mut b, fich(c, Fi::Communications, fn_));
        match c.dt {
            DataType::VdMode1 => {
                let d = match fn_ {
                    0 => [callsign_field(&c.dst), callsign_field(&c.src)].concat(),
                    1 => [callsign_field(&c.downlink), callsign_field(&c.uplink)].concat(),
                    _ => vec![b' '; 20],
                };
                frame::set_dch(&mut b, &d, true, false);
                for k in 0..5 {
                    frame::set_vch_vd1(&mut b, k, &test_frame(n * 5 + k));
                }
            }
            _ => {
                let d = match fn_ {
                    0 => callsign_field(&c.dst),
                    1 => callsign_field(&c.src),
                    2 => callsign_field(&c.downlink),
                    3 => callsign_field(&c.uplink),
                    _ => [b' '; 10],
                };
                frame::set_dch(&mut b, &d, false, false);
                for k in 0..5 {
                    frame::set_vch_dn(&mut b, k, &test_frame(n * 5 + k));
                }
            }
        }
        out.push(b);
    }
    if c.terminator {
        out.push(header_like(c, Fi::Terminator));
    }
    out
}

/// Frames back to back, as a radio sends them.
pub fn symbols(frames: &[[u8; FRAME_BITS]]) -> Symbols {
    frames
        .iter()
        .flat_map(|f| f.chunks(2).map(|d| Some(dibit_symbol((d[0] << 1) | d[1]))))
        .collect()
}

/// FM-modulate at `rate` with the outer symbols at [`OUTER_DEV`].
pub fn modulate(symbols: &Symbols, rate: f64, amp: f32) -> Vec<Complex32> {
    modulate_4fsk(symbols, rate, amp, OUTER_DEV)
}
