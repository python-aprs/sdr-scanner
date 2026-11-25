//! What sits where in a 100 ms System Fusion frame.
//!
//! ```text
//! | sync 40 | FICH 200 | 5 x ( DCH | VCH ) = 720 |        960 bits
//! V/D mode 2 (DN):          DCH 40, VCH 104 (one AMBE+2 frame, repeated bits)
//! V/D mode 1, headers:      DCH 72, VCH 72 (headers: a second data channel)
//! ```
//!
//! FICH (frame information) says what the frame is: header, voice or
//! terminator, the data type (DN, VW, ...), frame number and DG-ID. The data
//! channel carries callsigns, ten characters at a time: headers and
//! terminators carry them all, voice frames a piece per frame number.

use super::fec;
use crate::dmr::burst::{self, AmbeFrame};
use crate::dmr::fec::{bits_to_bytes, bytes_to_bits, golay24_decode, golay24_encode};

pub const FRAME_BITS: usize = 960;
pub const FRAME_SYMBOLS: usize = 480;
pub const SYNC_SYMBOLS: usize = 20;
pub const SYNC: u64 = 0xD4_71C9_634D;
const FICH_AT: usize = 40;
const PAYLOAD_AT: usize = 240;
const SECTION: usize = 144;
pub const CALLSIGN_LEN: usize = 10;

/// Frame type (FICH FI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fi {
    Header,
    Communications,
    Terminator,
    Test,
}

/// What the payload carries (FICH DT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// Half voice, half data, AMBE+2 voice with its usual FEC.
    VdMode1,
    /// Data only (pictures, Wires-X).
    DataFr,
    /// "DN": AMBE+2 voice with repeated bits, a little data.
    VdMode2,
    /// "VW": full rate IMBE voice.
    VoiceFr,
}

impl DataType {
    pub fn as_str(self) -> &'static str {
        match self {
            DataType::VdMode1 => "vd1",
            DataType::DataFr => "data",
            DataType::VdMode2 => "dn",
            DataType::VoiceFr => "vw",
        }
    }
}

/// Frame information channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fich {
    pub fi: Fi,
    /// Call mode: 0 group (CQ), 1 group (radio id), 3 individual.
    pub cm: u8,
    /// Frame number in the data channel's cycle, and the cycle's last.
    pub fn_: u8,
    pub ft: u8,
    pub dt: DataType,
    /// Message route: 0 direct, 1 repeater not busy, 2 busy.
    pub mr: u8,
    pub dg_id: u8,
}

impl Fich {
    pub fn to_bytes(self) -> [u8; 4] {
        let fi = match self.fi {
            Fi::Header => 0,
            Fi::Communications => 1,
            Fi::Terminator => 2,
            Fi::Test => 3,
        };
        let dt = match self.dt {
            DataType::VdMode1 => 0,
            DataType::DataFr => 1,
            DataType::VdMode2 => 2,
            DataType::VoiceFr => 3,
        };
        [
            (fi << 6) | ((self.cm & 3) << 2),
            ((self.fn_ & 7) << 3) | (self.ft & 7),
            ((self.mr & 7) << 3) | dt,
            self.dg_id & 0x7F,
        ]
    }

    pub fn from_bytes(b: &[u8]) -> Self {
        Fich {
            fi: [Fi::Header, Fi::Communications, Fi::Terminator, Fi::Test][(b[0] >> 6) as usize],
            cm: (b[0] >> 2) & 3,
            fn_: (b[1] >> 3) & 7,
            ft: b[1] & 7,
            dt: [
                DataType::VdMode1,
                DataType::DataFr,
                DataType::VdMode2,
                DataType::VoiceFr,
            ][(b[2] & 3) as usize],
            mr: (b[2] >> 3) & 3,
            dg_id: b[3] & 0x7F,
        }
    }
}

/// The FICH of a frame (`bits`: the whole frame), if it decodes: four
/// Golay (24,12) words under the convolutional code, CRC checked.
pub fn fich(bits: &[u8]) -> Option<Fich> {
    let (dec, _) = fec::deconv(&bits[FICH_AT..PAYLOAD_AT], 5);
    let mut data = [0u8; 48];
    for w in 0..4 {
        let (d, _, bad) = golay24_decode(crate::dmr::fec::to_u32(&dec[w * 24..w * 24 + 24]));
        if bad {
            return None;
        }
        crate::dmr::fec::from_u32(d, 12, &mut data[w * 12..]);
    }
    let bytes = bits_to_bytes(&data);
    fec::crc_ok(&bytes).then(|| Fich::from_bytes(&bytes))
}

pub fn set_fich(bits: &mut [u8], f: Fich) {
    let mut bytes = f.to_bytes().to_vec();
    bytes.extend(fec::crc16(&bytes).to_be_bytes());
    let data = bytes_to_bits(&bytes);
    let mut code = vec![0u8; 96];
    for w in 0..4 {
        let c = golay24_encode(crate::dmr::fec::to_u32(&data[w * 12..w * 12 + 12]));
        crate::dmr::fec::from_u32(c, 24, &mut code[w * 24..]);
    }
    bits[FICH_AT..PAYLOAD_AT].copy_from_slice(&fec::conv(&code, 5));
}

pub fn set_sync(bits: &mut [u8]) {
    crate::dmr::fec::from_u32((SYNC >> 8) as u32, 32, &mut bits[..32]);
    crate::dmr::fec::from_u32((SYNC & 0xFF) as u32, 8, &mut bits[32..40]);
}

/// Where the data channel sits in each section: its offset and length.
/// `second` picks the half a header uses for its second data channel.
fn dch_span(wide: bool, second: bool) -> (usize, usize) {
    match (wide, second) {
        (true, false) => (0, 72),
        (true, true) => (72, 72),
        (false, _) => (0, 40),
    }
}

fn gather(bits: &[u8], wide: bool, second: bool) -> Vec<u8> {
    let (off, len) = dch_span(wide, second);
    (0..5)
        .flat_map(|k| {
            let s = PAYLOAD_AT + k * SECTION + off;
            bits[s..s + len].iter().copied()
        })
        .collect()
}

fn scatter(bits: &mut [u8], raw: &[u8], wide: bool, second: bool) {
    let (off, len) = dch_span(wide, second);
    for k in 0..5 {
        let s = PAYLOAD_AT + k * SECTION + off;
        bits[s..s + len].copy_from_slice(&raw[k * len..k * len + len]);
    }
}

/// The data channel's bytes, if its CRC checks: 20 bytes for headers,
/// terminators and V/D mode 1 (`wide`), 10 for V/D mode 2. A header's
/// second channel (`second`) sits where the voice would.
pub fn dch(bits: &[u8], wide: bool, second: bool) -> Option<Vec<u8>> {
    let cols = if wide { 9 } else { 5 };
    let (dec, _) = fec::deconv(&gather(bits, wide, second), cols);
    let mut bytes = bits_to_bytes(&dec);
    if !fec::crc_ok(&bytes) {
        return None;
    }
    bytes.truncate(bytes.len() - 2);
    for (b, w) in bytes.iter_mut().zip(fec::WHITENING) {
        *b ^= w;
    }
    Some(bytes)
}

/// Writes a data channel ([`dch`]'s inverse; `data` is 20 or 10 bytes).
pub fn set_dch(bits: &mut [u8], data: &[u8], wide: bool, second: bool) {
    let cols = if wide { 9 } else { 5 };
    let mut bytes: Vec<u8> = data
        .iter()
        .zip(fec::WHITENING)
        .map(|(b, w)| b ^ w)
        .collect();
    bytes.extend(fec::crc16(&bytes).to_be_bytes());
    scatter(bits, &fec::conv(&bytes_to_bits(&bytes), cols), wide, second);
}

/// Start of section `k`'s voice channel.
fn vch_at(k: usize, wide: bool) -> usize {
    PAYLOAD_AT + k * SECTION + if wide { 72 } else { 40 }
}

/// V/D mode 2 voice frame `k` (0..5): 104 bits, interleaved and whitened,
/// holding the first 27 parameter bits three times each, then the other
/// 22 and a spare. `errs` counts the repeated bits that disagreed.
pub fn vch_dn(bits: &[u8], k: usize) -> AmbeFrame {
    let at = vch_at(k, false);
    let v: Vec<u8> = (0..104)
        .map(|i| bits[at + fec::vch_interleave(i)] ^ fec::whitening_bit(i))
        .collect();
    let mut f = [0u8; 49];
    let mut errs = 0u8;
    for i in 0..27 {
        let n = v[3 * i] + v[3 * i + 1] + v[3 * i + 2];
        f[i] = (n >= 2) as u8;
        errs += (n == 1 || n == 2) as u8;
    }
    f[27..].copy_from_slice(&v[81..103]);
    AmbeFrame { bits: f, errs }
}

pub fn set_vch_dn(bits: &mut [u8], k: usize, f: &[u8; 49]) {
    let at = vch_at(k, false);
    let mut v = [0u8; 104];
    for i in 0..27 {
        v[3 * i..3 * i + 3].fill(f[i]);
    }
    v[81..103].copy_from_slice(&f[27..]);
    for (i, b) in v.iter().enumerate() {
        bits[at + fec::vch_interleave(i)] = b ^ fec::whitening_bit(i);
    }
}

/// V/D mode 1 voice frame `k` (0..5): a 72-bit AMBE+2 frame laid out as in
/// a DMR voice burst.
pub fn vch_vd1(bits: &[u8], k: usize) -> AmbeFrame {
    let at = vch_at(k, true);
    burst::ambe72_decode(|j| bits[at + j])
}

pub fn set_vch_vd1(bits: &mut [u8], k: usize, f: &[u8; 49]) {
    let at = vch_at(k, true);
    burst::ambe72_encode(f, |j, v| bits[at + j] = v);
}

/// A callsign field as text: printable ASCII, trailing blanks dropped.
/// `None` when it's empty.
pub fn callsign(b: &[u8]) -> Option<String> {
    let s: String = b
        .iter()
        .map(|&c| {
            if (0x20..0x7F).contains(&c) {
                c as char
            } else {
                ' '
            }
        })
        .collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// A callsign padded to the field's ten characters.
pub fn callsign_field(s: &str) -> [u8; CALLSIGN_LEN] {
    let mut f = [b' '; CALLSIGN_LEN];
    for (o, c) in f.iter_mut().zip(s.bytes()) {
        *o = c;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fich_round_trips() {
        let f = Fich {
            fi: Fi::Communications,
            cm: 0,
            fn_: 3,
            ft: 6,
            dt: DataType::VdMode2,
            mr: 2,
            dg_id: 42,
        };
        let mut bits = [0u8; FRAME_BITS];
        set_fich(&mut bits, f);
        // A few channel errors.
        for i in [45, 90, 133, 201] {
            bits[i] ^= 1;
        }
        assert_eq!(fich(&bits), Some(f));
    }

    #[test]
    fn data_channels_round_trip() {
        let mut bits = [0u8; FRAME_BITS];
        let csd1: Vec<u8> = [callsign_field("ALL"), callsign_field("KF7HVM")].concat();
        let csd2: Vec<u8> = [callsign_field("W7ABC"), callsign_field("N0CALL")].concat();
        set_dch(&mut bits, &csd1, true, false);
        set_dch(&mut bits, &csd2, true, true);
        assert_eq!(dch(&bits, true, false), Some(csd1));
        assert_eq!(dch(&bits, true, true), Some(csd2));
        let mut bits = [0u8; FRAME_BITS];
        set_dch(&mut bits, &callsign_field("KF7HVM"), false, false);
        bits[PAYLOAD_AT + 5] ^= 1;
        assert_eq!(
            dch(&bits, false, false).as_deref(),
            Some(&callsign_field("KF7HVM")[..])
        );
        assert_eq!(
            callsign(&callsign_field("KF7HVM")).as_deref(),
            Some("KF7HVM")
        );
    }

    #[test]
    fn voice_frames_round_trip() {
        let mut bits = [0u8; FRAME_BITS];
        let frames: Vec<[u8; 49]> = (0..5).map(crate::dmr::tx::test_frame).collect();
        for (k, f) in frames.iter().enumerate() {
            set_vch_dn(&mut bits, k, f);
        }
        bits[vch_at(2, false) + fec::vch_interleave(4)] ^= 1;
        for (k, f) in frames.iter().enumerate() {
            let got = vch_dn(&bits, k);
            assert_eq!(&got.bits, f, "frame {k}");
            assert_eq!(got.errs, (k == 2) as u8);
        }
        let mut bits = [0u8; FRAME_BITS];
        for (k, f) in frames.iter().enumerate() {
            set_vch_vd1(&mut bits, k, f);
        }
        for (k, f) in frames.iter().enumerate() {
            assert_eq!(&vch_vd1(&bits, k).bits, f);
        }
    }
}
