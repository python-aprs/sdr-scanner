//! D-STAR voice frames and slow data.
//!
//! After the header come 20 ms frames of 96 bits: 72 bits of AMBE voice,
//! then 24 bits of slow data. Every 21st frame (from the first) carries the
//! data sync in place of slow data; the frames between pair up into
//! six-byte blocks, scrambled, that carry a 20-character message, a copy of
//! the header (for late entry), GPS and so on. A transmission ends with a
//! 48-bit end pattern.

use crate::dmr::burst::{self, Ambe72Layout, AmbeFrame};

pub const FRAME_BITS: usize = 96;
pub const VOICE_BITS: usize = 72;
/// Frames from one data sync to the next.
pub const SUPERFRAME: usize = 21;

/// Bits on air (oldest first) of the frame sync that ends the preamble
/// before a header, the data sync, and the end pattern.
pub const FRAME_SYNC: u64 = 0x55_7650;
pub const DATA_SYNC: u64 = 0xAA_B468;
pub const END: u64 = 0xAAAA_AAAA_135E;

/// The pattern's `n` bits, oldest first.
pub fn pattern_bits(p: u64, n: usize) -> Vec<u8> {
    (0..n).map(|i| ((p >> (n - 1 - i)) & 1) as u8).collect()
}

/// Where D-STAR's 72-bit AMBE frame keeps its codes (the same Golay codes
/// and whitening as AMBE+2, in another order).
pub const LAYOUT: Ambe72Layout = Ambe72Layout {
    c0: [
        7, 1, 11, 21, 31, 25, 35, 45, 55, 49, 59, 69, 6, 0, 10, 20, 30, 24, 34, 44, 54, 48, 58, 68,
    ],
    c1: [
        5, 15, 9, 19, 29, 39, 33, 43, 53, 63, 57, 67, 4, 14, 8, 18, 28, 38, 32, 42, 52, 62, 56,
    ],
    c23: [
        66, 3, 13, 23, 17, 27, 37, 47, 41, 51, 61, 71, 65, 2, 12, 22, 16, 26, 36, 46, 40, 50, 60,
        70, 64,
    ],
};

pub fn voice(bits: &[u8]) -> AmbeFrame {
    burst::ambe72_decode_with(&LAYOUT, |j| bits[j])
}

pub fn set_voice(bits: &mut [u8], f: &[u8; 49]) {
    burst::ambe72_encode_with(&LAYOUT, f, |j, v| bits[j] = v);
}

const SCRAMBLER: [u8; 3] = [0x70, 0x4F, 0x93];

/// A frame's three slow data bytes, descrambled.
pub fn slow_data(bits: &[u8]) -> [u8; 3] {
    std::array::from_fn(|k| {
        let b = (0..8).fold(0u8, |a, i| a | (bits[VOICE_BITS + k * 8 + i] << i));
        b ^ SCRAMBLER[k]
    })
}

pub fn set_slow_data(bits: &mut [u8], d: [u8; 3]) {
    for (k, b) in d.iter().enumerate() {
        let b = b ^ SCRAMBLER[k];
        for i in 0..8 {
            bits[VOICE_BITS + k * 8 + i] = (b >> i) & 1;
        }
    }
}

pub fn set_data_sync(bits: &mut [u8]) {
    bits[VOICE_BITS..].copy_from_slice(&pattern_bits(DATA_SYNC, 24));
}

/// Printable text of a callsign field, blanks trimmed.
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

/// Collects slow data blocks: the message, and the header copy.
#[derive(Debug, Default, Clone)]
pub struct SlowData {
    half: Option<[u8; 3]>,
    text: [u8; 20],
    text_have: u8,
    header: Vec<u8>,
}

const TYPE_TEXT: u8 = 0x40;
const TYPE_HEADER: u8 = 0x50;

impl SlowData {
    /// A data sync: blocks start again on the next frame.
    pub fn sync(&mut self) {
        self.half = None;
    }

    /// One frame's slow data bytes ([`slow_data`]).
    pub fn push(&mut self, d: [u8; 3]) {
        let Some(first) = self.half.take() else {
            self.half = Some(d);
            return;
        };
        let block = [first[0], first[1], first[2], d[0], d[1], d[2]];
        match block[0] & 0xF0 {
            TYPE_TEXT => {
                let n = (block[0] & 0x03) as usize;
                for (i, &c) in block[1..].iter().enumerate() {
                    self.text[n * 5 + i] = c & 0x7F;
                }
                self.text_have |= 1 << n;
            }
            TYPE_HEADER => {
                let len = (block[0] & 0x0F).min(5) as usize;
                if self.header.len() < super::header::HEADER_BYTES {
                    self.header.extend_from_slice(&block[1..1 + len]);
                }
            }
            _ => {}
        }
    }

    /// The message once all four of its blocks are in.
    pub fn message(&self) -> Option<String> {
        (self.text_have == 0xF)
            .then(|| callsign(&self.text))
            .flatten()
    }

    /// The header the slow data repeats, once it's all in and checks.
    pub fn header(&self) -> Option<super::header::Header> {
        let n = super::header::HEADER_BYTES;
        if self.header.len() < n {
            return None;
        }
        let mut b = self.header[..n].to_vec();
        for c in &mut b[3..39] {
            *c &= 0x7F;
        }
        super::header::crc_ok(&b).then(|| super::header::Header::from_bytes(&b))
    }

    /// The blocks that carry `message` and `header` (the bytes after the
    /// type byte, five at a time), for the transmitter.
    pub fn blocks(message: &str, header: &[u8]) -> Vec<[u8; 6]> {
        let mut out = Vec::new();
        let text: Vec<u8> = message
            .bytes()
            .chain(std::iter::repeat(b' '))
            .take(20)
            .collect();
        for n in 0..4 {
            let mut b = [TYPE_TEXT | n as u8, 0, 0, 0, 0, 0];
            b[1..].copy_from_slice(&text[n * 5..n * 5 + 5]);
            out.push(b);
        }
        for c in header.chunks(5) {
            let mut b = [TYPE_HEADER | c.len() as u8, 0x66, 0x66, 0x66, 0x66, 0x66];
            b[1..1 + c.len()].copy_from_slice(c);
            out.push(b);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_sync_is_published_bytes() {
        // 0x55 0x2D 0x16, least significant bit first.
        let mut bits = [0u8; FRAME_BITS];
        set_data_sync(&mut bits);
        let bytes: Vec<u8> = bits[VOICE_BITS..]
            .chunks(8)
            .map(|c| c.iter().enumerate().fold(0, |a, (i, &b)| a | (b << i)))
            .collect();
        assert_eq!(bytes, [0x55, 0x2D, 0x16]);
    }

    #[test]
    fn null_voice_frame_decodes_cleanly() {
        // D-STAR's silence frame, as sent between transmissions.
        let null = [0x9E, 0x8D, 0x32, 0x88, 0x26, 0x1A, 0x3F, 0x61, 0xE8];
        let bits: Vec<u8> = null
            .iter()
            .flat_map(|b| (0..8).map(move |i| (b >> i) & 1))
            .collect();
        let f = voice(&bits);
        assert_eq!(f.errs, 0);
        let mut again = vec![0u8; 72];
        set_voice(&mut again, &f.bits);
        assert_eq!(again, bits);
    }
}
