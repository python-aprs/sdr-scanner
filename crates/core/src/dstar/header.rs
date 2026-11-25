//! The D-STAR radio header: 41 bytes (flags, four callsigns, a suffix and a
//! CRC) sent once at the start of a transmission, protected by a rate 1/2,
//! K=3 convolutional code, interleaved and scrambled into 660 bits.
//!
//! Bits on air are each byte's least significant first.

use super::frame::callsign;

pub const HEADER_BYTES: usize = 41;
pub const HEADER_BITS: usize = 660;

/// What the header says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    pub flags: [u8; 3],
    /// Destination repeater (the gateway, or the other module) and the
    /// repeater the radio talks into.
    pub rpt2: Option<String>,
    pub rpt1: Option<String>,
    /// Who the call is for (`CQCQCQ`, a callsign, or a linking command).
    pub your: Option<String>,
    pub my: Option<String>,
    /// Four characters after MY, often the radio model.
    pub suffix: Option<String>,
}

impl Header {
    pub fn from_bytes(b: &[u8]) -> Self {
        Header {
            flags: [b[0], b[1], b[2]],
            rpt2: callsign(&b[3..11]),
            rpt1: callsign(&b[11..19]),
            your: callsign(&b[19..27]),
            my: callsign(&b[27..35]),
            suffix: callsign(&b[35..39]),
        }
    }

    /// The 41 bytes, CRC included.
    pub fn to_bytes(&self) -> [u8; HEADER_BYTES] {
        let mut b = [b' '; HEADER_BYTES];
        b[..3].copy_from_slice(&self.flags);
        for (s, at, len) in [
            (&self.rpt2, 3, 8),
            (&self.rpt1, 11, 8),
            (&self.your, 19, 8),
            (&self.my, 27, 8),
            (&self.suffix, 35, 4),
        ] {
            for (o, c) in b[at..at + len]
                .iter_mut()
                .zip(s.as_deref().unwrap_or("").bytes())
            {
                *o = c;
            }
        }
        let crc = crc(&b[..39]);
        b[39..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Sent by a repeater (rather than a radio).
    pub fn repeater(&self) -> bool {
        self.flags[0] & 0x40 != 0
    }
}

/// CRC-16/X.25 (CCITT polynomial, reflected, initial all ones, inverted),
/// sent low byte first.
pub fn crc(bytes: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in bytes {
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

pub fn crc_ok(b: &[u8]) -> bool {
    let n = b.len();
    crc(&b[..n - 2]).to_le_bytes() == b[n - 2..]
}

/// The scrambler: x^7 + x^4 + 1 from all ones, one bit per bit on air.
pub fn scrambler(n: usize) -> Vec<u8> {
    let mut st = 0x7Fu8;
    (0..n)
        .map(|_| {
            let b = ((st >> 3) ^ (st >> 6)) & 1;
            st = ((st << 1) | b) & 0x7F;
            b
        })
        .collect()
}

/// Air position of code bit `i`: written down 24-bit columns, read out
/// along rows (the first 12 rows 28 long, the rest 27).
fn interleave(i: usize) -> usize {
    let (c, r) = (i / 24, i % 24);
    if r < 12 {
        r * 28 + c
    } else {
        336 + (r - 12) * 27 + c
    }
}

/// The two code bits for input `d` from `state` (last two inputs, newest in
/// bit 0), in the order they're sent: 1 + D + D^2, then 1 + D^2.
fn branch(state: u8, d: u8) -> (u8, u8) {
    let (d1, d2) = (state & 1, (state >> 1) & 1);
    (d ^ d1 ^ d2, d ^ d2)
}

/// The header's 660 bits on air.
pub fn encode(bytes: &[u8; HEADER_BYTES]) -> [u8; HEADER_BITS] {
    let mut code = Vec::with_capacity(HEADER_BITS);
    let mut state = 0u8;
    for i in 0..330 {
        let d = if i < 328 {
            (bytes[i / 8] >> (i % 8)) & 1
        } else {
            0
        };
        let (a, b) = branch(state, d);
        code.push(a);
        code.push(b);
        state = ((state << 1) | d) & 3;
    }
    let pn = scrambler(HEADER_BITS);
    let mut out = [0u8; HEADER_BITS];
    for (i, &c) in code.iter().enumerate() {
        let p = interleave(i);
        out[p] = c ^ pn[p];
    }
    out
}

/// The header's bytes from its 660 bits on air, if the CRC checks.
pub fn decode(bits: &[u8]) -> Option<[u8; HEADER_BYTES]> {
    let pn = scrambler(HEADER_BITS);
    let code: Vec<u8> = (0..HEADER_BITS)
        .map(|i| {
            let p = interleave(i);
            bits[p] ^ pn[p]
        })
        .collect();
    // Viterbi over four states, starting and ending in zero.
    const INF: u32 = u32::MAX / 2;
    let mut metric = [0, INF, INF, INF];
    let mut from = vec![[0u8; 4]; 330];
    for (t, pair) in code.chunks_exact(2).enumerate() {
        let mut next = [INF; 4];
        for (s, &m) in metric.iter().enumerate() {
            if m >= INF {
                continue;
            }
            for d in 0..2u8 {
                let (a, b) = branch(s as u8, d);
                let cost = m + (a != pair[0]) as u32 + (b != pair[1]) as u32;
                let ns = (((s as u8) << 1) | d) as usize & 3;
                if cost < next[ns] {
                    next[ns] = cost;
                    from[t][ns] = s as u8;
                }
            }
        }
        metric = next;
    }
    let mut bits = [0u8; 330];
    let mut s = 0u8;
    for t in (0..330).rev() {
        bits[t] = s & 1;
        s = from[t][s as usize];
    }
    let mut out = [0u8; HEADER_BYTES];
    for (i, &b) in bits[..328].iter().enumerate() {
        out[i / 8] |= b << (i % 8);
    }
    crc_ok(&out).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrambler_matches_published_bytes() {
        let pn = scrambler(24);
        let bytes: Vec<u8> = pn
            .chunks(8)
            .map(|c| c.iter().enumerate().fold(0, |a, (i, &b)| a | (b << i)))
            .collect();
        assert_eq!(bytes, [0x70, 0x4F, 0x93]);
    }

    #[test]
    fn crc_is_x25() {
        assert_eq!(crc(b"123456789"), 0x906E);
    }

    #[test]
    fn interleave_is_a_permutation() {
        let mut seen = [false; HEADER_BITS];
        for i in 0..HEADER_BITS {
            seen[interleave(i)] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    /// A header as the MMDVM modem firmware's encoder (its interleave and
    /// scramble tables) lays it out; its first four bits belong to the
    /// sync before.
    const REF_AIR: &str = "402bfcdda0cb9b0be34b1466a2231a2834a3026e2f2cd02c623d3c33f8d3bc81d331cc4595839a2928c0336fba14fe852bcd7bd6cff5ca7d22ecce0aadbd0adbea222b7a18399a1c61f3440aec60b7f111640a";
    const REF_HEADER: &str =
        "000000574237445a472047574237445a47204243514351435120204b463748564d202049443531b111";

    #[test]
    fn matches_reference_header() {
        let hex = |h: &str| -> Vec<u8> {
            (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
                .collect()
        };
        let air: Vec<u8> = hex(REF_AIR)
            .iter()
            .flat_map(|b| (0..8).map(move |i| (b >> i) & 1))
            .collect();
        let want = hex(REF_HEADER);
        assert_eq!(decode(&air[4..664]).map(|b| b.to_vec()), Some(want.clone()));
        assert_eq!(&encode(&want.try_into().unwrap())[..], &air[4..664]);
    }

    #[test]
    fn header_round_trips_through_errors() {
        let h = Header {
            flags: [0, 0, 0],
            rpt2: Some("WB7DZG G".into()),
            rpt1: Some("WB7DZG B".into()),
            your: Some("CQCQCQ".into()),
            my: Some("KF7HVM".into()),
            suffix: Some("ID51".into()),
        };
        let b = h.to_bytes();
        let mut bits = encode(&b);
        for i in [3, 100, 101, 250, 400, 600] {
            bits[i] ^= 1;
        }
        let got = decode(&bits).expect("crc");
        assert_eq!(Header::from_bytes(&got), h);
    }
}
